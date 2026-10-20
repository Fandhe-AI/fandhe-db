//! `schema.table` 形のテーブル参照（`QualifiedIdent`）の名前解決（TABLE-27・Issue #1714。
//! ADR `docs/design/schema-tenant-namespace.md` の D2・D4 のポインタ）。
//!
//! 責務境界: 字句解析（`sql::lexer`）が `a.b` を 1 トークンにした結果のうち、**テーブル参照の
//! 位置**にあるものだけを、構文解析（`sql::allowlist::Parser`）へ渡す前に解決して
//! `Token::Ident(<名前>)` へ置き換える。置き換え後のトークン列は未修飾の `<名前>` と完全に
//! 同一になるため、DDL・DML・SELECT・FK・索引・ビュー本文（保存時は解決後の名前）が
//! 修飾の有無で同じ経路を通る（暗黙 RLS・DDL 権限ゲート・一時表の振り分けも同じ）。
//!
//! 呼び出し元: `core::EngineCore` の `parse_tokens_in`・拡張クエリ Parse と、
//! `sql::allowlist` の文字列入口（`validate_sql` 等）。列修飾（`t.col`）の位置は対象外で、
//! 従来どおり `sql::qualified_column` が扱う。
//!
//! 解決規則（fail-closed）:
//! - `public`（ASCII 大小無視）: 裸の名前へ（ADR D2）。`CREATE TEMP TABLE public.t` は `42P16`
//! - `pg_catalog`／`information_schema`／`pg_temp`: `0A000`（ADR D4。`pg_temp` の結線は別 Issue）
//! - それ以外: テナントが分からない入口では `3F000`。分かる入口（[`SchemaExists`] を渡された
//!   場合）は `(tenant, schema)` の 1 回の照会で、無ければ `3F000`（他テナントの SCHEMA も
//!   同じ応答）、あれば参照系は `42P01`・作成系は `0A000`（テナント SCHEMA への配置は #1717）
//!
//! 解決エラーはその位置で返し、置き換え用の名前で解析を続けない（別オブジェクトへの誤解決を
//! 構造的に防ぐ）。ただし選択リスト等に列修飾（`x.id`）が残る文は、従来の `42601` を保つため
//! 解決エラーを保留して書き換えずに返す（`SELECT x.id FROM s.docs` の既存契約）。

use crate::catalog::namespace::is_builtin_schema_name;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::{Keyword, Token};

/// 呼び出し元テナントの SCHEMA 存在照会。照会失敗は `XX000` 系（fail-closed）の
/// [`SqlSurfaceError`] で返す。
pub(crate) type SchemaExists<'a> = &'a dyn Fn(&str) -> Result<bool, SqlSurfaceError>;

/// 括弧のネスト段数の上限（超えた `(` は最内側の段へ畳む。走査状態の確保を抑える）。
const MAX_LEVELS: usize = 256;

/// 括弧 1 段分の走査状態。
#[derive(Default, Clone, Copy)]
struct Level {
    /// `SELECT`／`UPDATE`／`DELETE` を見た（この段の `FROM` が表参照の `FROM`）。
    from_ok: bool,
    /// カンマの直後が表参照になるリスト（`FROM a, b`・`TRUNCATE a, b`）の中。
    list: bool,
}

fn ident_is(tok: Option<&Token>, word: &str) -> bool {
    matches!(tok, Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
}

/// `tokens` のテーブル参照位置にある `schema.name` を解決して `Token::Ident(name)` へ置き換える。
/// 戻り値は `public.` で修飾されていた表名（一時表との衝突検査用）。修飾が無ければ何もしない。
pub(crate) fn rewrite_relation_names(
    tokens: &mut [Token],
    schema_exists: Option<SchemaExists<'_>>,
) -> Result<Vec<String>, SqlSurfaceError> {
    if !tokens
        .iter()
        .any(|t| matches!(t, Token::QualifiedIdent { .. }))
    {
        return Ok(Vec::new());
    }
    let head = tokens.get(..4).unwrap_or(&*tokens);
    let head_create = ident_is(tokens.first(), "CREATE");
    let create_table = head_create && head.iter().any(|t| ident_is(Some(t), "TABLE"));
    // `CREATE [GLOBAL|LOCAL] {TEMP|TEMPORARY} TABLE`（一時表 DDL として受理される全形）。
    let create_temp_table = crate::sql::allowlist::is_create_temp_table_statement(tokens);
    let table_head = head.iter().any(|t| ident_is(Some(t), "TABLE"));
    let mut index_on_pending = head_create && head.iter().any(|t| ident_is(Some(t), "INDEX"));

    let mut levels: Vec<Level> = vec![Level::default()];
    let mut public_names: Vec<String> = Vec::new();
    let mut deferred: Option<SqlSurfaceError> = None;
    let mut residual_column_qualifier = false;

    for i in 0..tokens.len() {
        let prev = i.checked_sub(1).and_then(|p| tokens.get(p));
        let prev2 = i.checked_sub(2).and_then(|p| tokens.get(p));
        let Some(cur) = tokens.get(i) else { break };
        let Some(level) = levels.last().copied() else {
            break;
        };
        // 表参照の `FROM`（`IS DISTINCT FROM`・`EXTRACT(.. FROM x)` を除く）の直後から、
        // 以降のカンマは表参照リストの区切りになる。
        let from_rel = matches!(prev, Some(Token::Keyword(Keyword::From)))
            && level.from_ok
            && !ident_is(prev2, "DISTINCT");
        let mut next = level;
        if from_rel {
            next.list = true;
        }
        let mut relation_pos = false;
        let mut is_create_name = false;
        match cur {
            Token::Punct('(') => {
                if let Some(l) = levels.last_mut() {
                    *l = next;
                }
                if levels.len() < MAX_LEVELS {
                    levels.push(Level::default());
                }
                continue;
            }
            Token::Punct(')') => {
                if levels.len() > 1 {
                    levels.pop();
                }
                continue;
            }
            Token::Keyword(Keyword::Select) => next.from_ok = true,
            Token::Keyword(Keyword::Where | Keyword::Order | Keyword::Limit) => next.list = false,
            Token::Ident(w) => {
                if w.eq_ignore_ascii_case("UPDATE") || w.eq_ignore_ascii_case("DELETE") {
                    next.from_ok = true;
                } else if w.eq_ignore_ascii_case("TRUNCATE") || w.eq_ignore_ascii_case("TABLE") {
                    next.list = true;
                } else if [
                    "GROUP",
                    "HAVING",
                    "UNION",
                    "INTERSECT",
                    "EXCEPT",
                    "WINDOW",
                    "OFFSET",
                    "FETCH",
                    "RETURNING",
                    "CONFLICT",
                ]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
                {
                    next.list = false;
                }
            }
            Token::QualifiedIdent { .. } => {
                relation_pos = match prev {
                    Some(Token::Keyword(Keyword::From)) => from_rel,
                    Some(Token::Ident(w)) => {
                        let is = |k: &str| w.eq_ignore_ascii_case(k);
                        if is("TABLE") {
                            is_create_name = create_table;
                            true
                        } else if is("EXISTS") {
                            let ok =
                                table_head && (ident_is(prev2, "IF") || ident_is(prev2, "NOT"));
                            is_create_name = ok && create_table;
                            ok
                        } else if is("COPY") {
                            i == 1
                        } else if is("ON") {
                            index_on_pending
                        } else {
                            is("JOIN")
                                || is("INTO")
                                || is("TRUNCATE")
                                || is("UPDATE")
                                || is("ONLY")
                                || is("REFERENCES")
                        }
                    }
                    Some(Token::Punct(',')) => level.list,
                    _ => false,
                };
            }
            _ => {}
        }
        // 索引 DDL の最初の `ON` だけが表参照を導く（JOIN の `ON <条件>` と区別する）。
        if ident_is(prev, "ON") {
            index_on_pending = false;
        }
        if let Some(l) = levels.last_mut() {
            *l = next;
        }
        let Token::QualifiedIdent { qualifier, name } = cur else {
            continue;
        };
        if !relation_pos {
            residual_column_qualifier = true;
            continue;
        }
        let (qualifier, name) = (qualifier.clone(), name.clone());
        match resolve(
            &qualifier,
            &name,
            is_create_name,
            create_temp_table,
            schema_exists,
        ) {
            Ok(()) => {
                public_names.push(name.clone());
                if let Some(slot) = tokens.get_mut(i) {
                    *slot = Token::Ident(name);
                }
            }
            Err(e) => {
                if deferred.is_none() {
                    deferred = Some(e);
                }
            }
        }
    }
    match deferred {
        // 列修飾が残る文は従来の `42601`（パーサー判定）を保つため、保留した解決エラーは返さない。
        Some(_) if residual_column_qualifier => Ok(public_names),
        Some(e) => Err(e),
        None => Ok(public_names),
    }
}

/// `qualifier.name` 1 件の解決。成功は「`public` で裸の名前へ置き換えてよい」を意味する。
fn resolve(
    qualifier: &str,
    name: &str,
    create: bool,
    create_temp: bool,
    schema_exists: Option<SchemaExists<'_>>,
) -> Result<(), SqlSurfaceError> {
    if qualifier.eq_ignore_ascii_case("public") {
        if create_temp {
            // 一時ではない SCHEMA への一時表の作成（黙って一時表 `name` を作らない）。
            return Err(SqlSurfaceError::invalid_table_definition(name));
        }
        return Ok(());
    }
    if is_builtin_schema_name(qualifier) || qualifier.eq_ignore_ascii_case("pg_temp") {
        return Err(SqlSurfaceError::feature_not_supported(
            "relations in this schema are not supported",
        ));
    }
    let exists = match schema_exists {
        Some(probe) => probe(qualifier)?,
        None => false,
    };
    if !exists {
        return Err(SqlSurfaceError::invalid_schema_name(qualifier));
    }
    if create {
        return Err(SqlSurfaceError::feature_not_supported(
            "creating relations inside a schema is not supported",
        ));
    }
    Err(SqlSurfaceError::undefined_table(format!(
        "{qualifier}.{name}"
    )))
}

/// [`rewrite_relation_names`] を適用した字句解析（`sql::allowlist` の文字列入口用。テナント不明）。
pub(crate) fn tokenize_resolved(sql: &str, marked: bool) -> Result<Vec<Token>, SqlSurfaceError> {
    let mut tokens = if marked {
        crate::sql::lexer::tokenize_marked(sql)?
    } else {
        crate::sql::lexer::tokenize(sql)?
    };
    rewrite_relation_names(&mut tokens, None)?;
    Ok(tokens)
}

/// ctx を持つ入口（`core::EngineCore::execute_sql*`・`begin_copy*`）で、テナント不明の解決が
/// 返した `3F000` を、テナントの SCHEMA 照会つきの解決で `42P01`／`0A000`／`3F000` に精密化する。
/// `e` が `3F000` でなければ素通しで、通常の文は追加コストを払わない。
pub(crate) fn refine_schema_error(
    sql: &str,
    schema_exists: SchemaExists<'_>,
    e: SqlSurfaceError,
) -> SqlSurfaceError {
    if !matches!(e, SqlSurfaceError::InvalidSchemaName { .. }) {
        return e;
    }
    let Ok(mut tokens) = crate::sql::lexer::tokenize_marked(sql) else {
        return e;
    };
    match rewrite_relation_names(&mut tokens, Some(schema_exists)) {
        Err(refined) => refined,
        Ok(_) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn rewrite(sql: &str) -> Result<Vec<Token>, SqlSurfaceError> {
        let mut t = tokenize(sql).expect("lex");
        rewrite_relation_names(&mut t, None)?;
        Ok(t)
    }

    fn same(qualified: &str, plain: &str) {
        assert_eq!(
            rewrite(qualified).expect(qualified),
            tokenize(plain).expect("lex"),
            "{qualified}"
        );
    }

    #[test]
    fn public_is_rewritten_in_relation_positions() {
        same("SELECT * FROM public.t", "SELECT * FROM t");
        same("SELECT * FROM PUBLIC.t LIMIT 1", "SELECT * FROM t LIMIT 1");
        same(
            "SELECT * FROM public.a JOIN public.b ON a.id = b.id",
            "SELECT * FROM a JOIN b ON a.id = b.id",
        );
        same("SELECT * FROM a, public.b", "SELECT * FROM a, b");
        same(
            "INSERT INTO public.t (a) VALUES (1)",
            "INSERT INTO t (a) VALUES (1)",
        );
        same("UPDATE public.t SET a = 1", "UPDATE t SET a = 1");
        same("DELETE FROM public.t", "DELETE FROM t");
        same("TRUNCATE public.a, public.b", "TRUNCATE a, b");
        same("DROP TABLE IF EXISTS public.t", "DROP TABLE IF EXISTS t");
        same(
            "CREATE TABLE public.t (id TEXT)",
            "CREATE TABLE t (id TEXT)",
        );
        same(
            "ALTER TABLE public.t ADD COLUMN c TEXT",
            "ALTER TABLE t ADD COLUMN c TEXT",
        );
        same("CREATE INDEX i ON public.t (a)", "CREATE INDEX i ON t (a)");
        same("COPY public.t FROM STDIN", "COPY t FROM STDIN");
        same(
            "CREATE TABLE c (p TEXT REFERENCES public.t (id))",
            "CREATE TABLE c (p TEXT REFERENCES t (id))",
        );
        same(
            "SELECT * FROM t WHERE id IN (SELECT id FROM public.u)",
            "SELECT * FROM t WHERE id IN (SELECT id FROM u)",
        );
    }

    #[test]
    fn column_qualifiers_are_left_alone() {
        for sql in [
            "SELECT t.id FROM t WHERE t.id = 1",
            "SELECT a.id FROM a JOIN b ON a.id = b.id, c",
            "SELECT EXTRACT(YEAR FROM t.ts) FROM t",
            "SELECT a FROM t WHERE a IS DISTINCT FROM b.x",
        ] {
            assert_eq!(
                rewrite(sql).expect(sql),
                tokenize(sql).expect("lex"),
                "{sql}"
            );
        }
    }

    #[test]
    fn unknown_schema_is_3f000_and_system_is_0a000() {
        for sql in [
            "SELECT * FROM s.t",
            "INSERT INTO s.t (a) VALUES (1)",
            "DROP TABLE s.t",
        ] {
            assert_eq!(rewrite(sql).expect_err(sql).wire_code(), "3F000", "{sql}");
        }
        for sql in [
            "SELECT * FROM pg_catalog.t",
            "SELECT * FROM information_schema.t",
            "SELECT * FROM pg_temp.t",
        ] {
            assert_eq!(rewrite(sql).expect_err(sql).wire_code(), "0A000", "{sql}");
        }
        assert_eq!(
            rewrite("CREATE TEMP TABLE public.t (id TEXT)")
                .expect_err("temp")
                .wire_code(),
            "42P16"
        );
    }

    #[test]
    fn existing_tenant_schema_is_42p01_or_0a000_on_create() {
        let probe = |s: &str| Ok(s == "s");
        for (sql, code) in [
            ("SELECT * FROM s.t", "42P01"),
            ("CREATE TABLE s.t (id TEXT)", "0A000"),
            ("SELECT * FROM z.t", "3F000"),
        ] {
            let mut t = tokenize(sql).expect("lex");
            let e = rewrite_relation_names(&mut t, Some(&probe)).expect_err(sql);
            assert_eq!(e.wire_code(), code, "{sql}");
        }
    }

    #[test]
    fn residual_column_qualifier_keeps_legacy_syntax_error_path() {
        let mut t = tokenize("SELECT x.id FROM s.docs LIMIT 1").expect("lex");
        let before = t.clone();
        assert!(rewrite_relation_names(&mut t, None).is_ok());
        assert_eq!(t, before);
    }
}
