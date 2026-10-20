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
    /// `SELECT` の射影リスト内（`SELECT` 後〜この段の `FROM` 前）。射影の識別子（列 `"update"` と
    /// その別名 `AS x` 等）は構文語に見えても表参照を導かない。
    in_proj: bool,
}

fn ident_is(tok: Option<&Token>, word: &str) -> bool {
    matches!(tok, Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
}

/// 識別子として表名になりうるトークンか（修飾あり・なし・二重引用符付きの `NULL`／`true`／`false` 等。
/// 引用付きは構文解析が識別子位置で `Ident` へ戻して受理するため、参照ゲートも同じ範囲を見る）。
pub(crate) fn is_ident_like(tok: &Token) -> bool {
    matches!(
        tok,
        Token::QualifiedIdent { .. }
            | Token::Ident(_)
            | Token::QuotedNull(_)
            | Token::QuotedBoolWord(_)
    )
}

/// 未修飾の識別子トークン（`Ident`・引用付き）の名前。`QualifiedIdent` は `None`。
pub(crate) fn unqualified_ident_name(tok: &Token) -> Option<&str> {
    match tok {
        Token::Ident(s) | Token::QuotedNull(s) | Token::QuotedBoolWord(s) => Some(s.as_str()),
        _ => None,
    }
}

/// テーブル参照位置にあるトークン 1 件（[`relation_positions`] の戻り値の要素）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RelPos {
    /// `tokens` 内の添字。
    pub index: usize,
    /// `CREATE TABLE <名前>` の作成対象名の位置か。
    pub create_name: bool,
}

/// [`relation_positions`] の結果。
pub(crate) struct RelationWalk {
    /// テーブル参照位置にある `Token::Ident`／`Token::QualifiedIdent`（出現順）。
    pub positions: Vec<RelPos>,
    /// テーブル参照位置ではない `Token::QualifiedIdent`（列修飾 `x.id` 等）が残っているか。
    pub residual_column_qualifier: bool,
}

/// `tokens` を走査して、テーブル参照位置にある識別子（修飾あり・なし）の添字を集める。位置判定は
/// 修飾名の解決（[`rewrite_relation_names`]）と `search_path` の未修飾名ゲート
/// （`sql::search_path`。TABLE-27・Issue #1715）が共有する唯一の実装で、トークン列は書き換えない。
/// 未修飾の `Token::Ident` には構文語・表関数名などの偽陽性があり得るため、呼び出し側が除外する。
pub(crate) fn relation_positions(tokens: &[Token]) -> RelationWalk {
    let head = tokens.get(..4).unwrap_or(tokens);
    let head_create = ident_is(tokens.first(), "CREATE");
    let create_table = head_create && head.iter().any(|t| ident_is(Some(t), "TABLE"));
    let table_head = head.iter().any(|t| ident_is(Some(t), "TABLE"));
    // `TABLE` が表参照を導くのは DDL 文の先頭付近だけ（`SELECT table, t.id` の識別子 `table` と
    // 区別する）。
    let ddl_head = ["CREATE", "DROP", "ALTER", "TRUNCATE", "LOCK", "COMMENT"]
        .iter()
        .any(|k| ident_is(tokens.first(), k));
    let mut index_on_pending = head_create && head.iter().any(|t| ident_is(Some(t), "INDEX"));

    let mut levels: Vec<Level> = vec![Level::default()];
    let mut out = RelationWalk {
        positions: Vec::new(),
        residual_column_qualifier: false,
    };

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
            Token::Keyword(Keyword::Select) => {
                next.from_ok = true;
                next.in_proj = true;
            }
            Token::Keyword(Keyword::From) if !ident_is(prev, "DISTINCT") => next.in_proj = false,
            Token::Keyword(Keyword::Where | Keyword::Order | Keyword::Limit) => next.list = false,
            Token::Ident(w) => {
                if w.eq_ignore_ascii_case("UPDATE") || w.eq_ignore_ascii_case("DELETE") {
                    next.from_ok = true;
                } else if (w.eq_ignore_ascii_case("TRUNCATE") && i == 0)
                    || (w.eq_ignore_ascii_case("TABLE") && ddl_head && i < 4)
                {
                    // `TRUNCATE` は文頭のときだけ表参照リストを開く（`SELECT 1 AS truncate, id`
                    // の別名を構文語と取り違えない）。
                    next.list = true;
                } else if !ident_is(prev, "AS")
                    && [
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
            _ => {}
        }
        if is_ident_like(cur) && !level.in_proj {
            relation_pos = match prev {
                Some(Token::Keyword(Keyword::From)) => from_rel,
                Some(Token::Ident(w)) => {
                    let is = |k: &str| w.eq_ignore_ascii_case(k);
                    if is("TABLE") {
                        is_create_name = create_table;
                        ddl_head && i <= 4
                    } else if is("EXISTS") {
                        let ok = table_head && (ident_is(prev2, "IF") || ident_is(prev2, "NOT"));
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
        // 索引 DDL の最初の `ON` だけが表参照を導く（JOIN の `ON <条件>` と区別する）。
        if ident_is(prev, "ON") {
            index_on_pending = false;
        }
        if let Some(l) = levels.last_mut() {
            *l = next;
        }
        match cur {
            Token::QualifiedIdent { .. } if !relation_pos => out.residual_column_qualifier = true,
            t if relation_pos && is_ident_like(t) => {
                out.positions.push(RelPos {
                    index: i,
                    create_name: is_create_name,
                });
            }
            _ => {}
        }
    }
    out
}

/// [`rewrite_relation_names_full`] の結果。
pub(crate) struct RewriteInfo {
    /// `public.` で修飾されていた表名（一時表との衝突検査用）。
    pub names: Vec<String>,
    /// `public.` 修飾を外したトークンの添字（準備済み文が未修飾名ゲートから除外するため。
    /// 置換は 1 対 1 なので位置は保たれる。TABLE-27・Issue #1715）。
    pub positions: Vec<usize>,
    /// `CREATE TABLE` の作成対象名が `public.` で明示修飾されていたか。
    pub create_public: bool,
}

/// `tokens` のテーブル参照位置にある `schema.name` を解決して `Token::Ident(name)` へ置き換える。
/// 戻り値は `public.` で修飾されていた表名（一時表との衝突検査用）。修飾が無ければ何もしない。
pub(crate) fn rewrite_relation_names(
    tokens: &mut [Token],
    schema_exists: Option<SchemaExists<'_>>,
) -> Result<Vec<String>, SqlSurfaceError> {
    Ok(rewrite_relation_names_full(tokens, schema_exists)?.names)
}

/// [`rewrite_relation_names`] の位置情報つき版。
pub(crate) fn rewrite_relation_names_full(
    tokens: &mut [Token],
    schema_exists: Option<SchemaExists<'_>>,
) -> Result<RewriteInfo, SqlSurfaceError> {
    let mut info = RewriteInfo {
        names: Vec::new(),
        positions: Vec::new(),
        create_public: false,
    };
    if !tokens
        .iter()
        .any(|t| matches!(t, Token::QualifiedIdent { .. }))
    {
        return Ok(info);
    }
    // 一時表 DDL として受理される全形（`CREATE [GLOBAL|LOCAL] {TEMP|TEMPORARY} TABLE`）。
    let create_temp_table = crate::sql::allowlist::is_create_temp_table_statement(tokens);
    let cte_names = collect_cte_names(tokens);
    let walk = relation_positions(tokens);
    let mut deferred: Option<SqlSurfaceError> = None;

    for pos in &walk.positions {
        let i = pos.index;
        let Some(Token::QualifiedIdent { qualifier, name }) = tokens.get(i) else {
            continue;
        };
        let (qualifier, name) = (qualifier.clone(), name.clone());
        match resolve(
            &qualifier,
            &name,
            pos.create_name,
            // 一時表の作成対象名だけが対象（`REFERENCES public.u` 等の参照側は許可する）。
            create_temp_table && pos.create_name,
            schema_exists,
        ) {
            Ok(()) => {
                // 同名の CTE があると、置換後の裸の名前が CTE に解決され永続表を指さなくなる。
                // `public.` が永続側を指す契約を守れないため拒否する（fail-closed）。
                if cte_names.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "public-qualified relation conflicts with a CTE name",
                    ));
                }
                if collides_with_syntax_word(&name) {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified relation name collides with a syntax word",
                    ));
                }
                info.names.push(name.clone());
                info.positions.push(i);
                info.create_public |= pos.create_name;
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
        Some(_) if walk.residual_column_qualifier => Ok(info),
        Some(e) => Err(e),
        None => Ok(info),
    }
}

/// 文中の CTE 定義名を集める。`WITH [RECURSIVE] name [(cols)] AS [[NOT] MATERIALIZED] (..)
/// [, name ..]*` の構造に沿って定義名だけを拾う（表別名・列別名・`CAST(.. AS ..)` の `AS` は
/// 対象外）。`WITH` を含まない文では空。構造が想定外の `WITH` はそこで打ち切る（その文は後段の
/// パーサーが拒否する）。
pub(crate) fn collect_cte_names(tokens: &[Token]) -> Vec<String> {
    let mut out = Vec::new();
    for (start, t) in tokens.iter().enumerate() {
        if !ident_is(Some(t), "WITH") {
            continue;
        }
        let mut j = start + 1;
        if ident_is(tokens.get(j), "RECURSIVE") {
            j += 1;
        }
        while let Some(Token::Ident(name)) = tokens.get(j) {
            j += 1;
            if matches!(tokens.get(j), Some(Token::Punct('('))) {
                // 列名リスト `(a, b)` を対応する `)` の直後まで読み飛ばす。
                match skip_group(tokens, j) {
                    Some(next) => j = next,
                    None => break,
                }
            }
            if !ident_is(tokens.get(j), "AS") {
                break;
            }
            j += 1;
            if ident_is(tokens.get(j), "NOT") {
                j += 1;
            }
            if ident_is(tokens.get(j), "MATERIALIZED") {
                j += 1;
            }
            if !matches!(tokens.get(j), Some(Token::Punct('('))) {
                break;
            }
            out.push(name.clone());
            match skip_group(tokens, j) {
                Some(next) => j = next,
                None => break,
            }
            if matches!(tokens.get(j), Some(Token::Punct(','))) {
                j += 1;
            } else {
                break;
            }
        }
    }
    out
}

/// CTE 1 件の可視範囲（[`collect_cte_scopes`] の要素）。
#[derive(Debug, Clone)]
pub(crate) struct CteScope {
    /// CTE 名（利用者が書いたとおり）。
    pub name: String,
    /// 可視範囲の開始添字（自身の本文の `)` の直後。自身の本文と先行する CTE の本文は含まない）。
    pub visible_from: usize,
    /// 可視範囲の終了添字（排他。`WITH` を含む括弧の `)`、無ければ末尾）。
    pub visible_to: usize,
}

impl CteScope {
    /// 添字 `index` の位置から、この CTE の名前が見えるか。
    pub(crate) fn covers(&self, index: usize) -> bool {
        self.visible_from <= index && index < self.visible_to
    }
}

/// [`collect_cte_names`] と同じ構造で CTE 定義を拾い、各 CTE の可視範囲を返す。非再帰 CTE は自身の
/// 本文からは見えず（同名の実リレーションへ解決される。`sql::cte` の契約）、後続の CTE の本文と主
/// クエリから見える。`search_path` の参照ゲートが「参照位置ごとに実際に可視な CTE だけ」を除外するために使う。
pub(crate) fn collect_cte_scopes(tokens: &[Token]) -> Vec<CteScope> {
    let mut out = Vec::new();
    for (start, t) in tokens.iter().enumerate() {
        if !ident_is(Some(t), "WITH") {
            continue;
        }
        let scope_end = enclosing_scope_end(tokens, start);
        let mut j = start + 1;
        if ident_is(tokens.get(j), "RECURSIVE") {
            j += 1;
        }
        while let Some(name) = tokens.get(j).and_then(unqualified_ident_name) {
            j += 1;
            if matches!(tokens.get(j), Some(Token::Punct('('))) {
                match skip_group(tokens, j) {
                    Some(next) => j = next,
                    None => break,
                }
            }
            if !ident_is(tokens.get(j), "AS") {
                break;
            }
            j += 1;
            if ident_is(tokens.get(j), "NOT") {
                j += 1;
            }
            if ident_is(tokens.get(j), "MATERIALIZED") {
                j += 1;
            }
            if !matches!(tokens.get(j), Some(Token::Punct('('))) {
                break;
            }
            let Some(after) = skip_group(tokens, j) else {
                break;
            };
            j = after;
            out.push(CteScope {
                name: name.to_string(),
                visible_from: after,
                visible_to: scope_end,
            });
            if matches!(tokens.get(j), Some(Token::Punct(','))) {
                j += 1;
            } else {
                break;
            }
        }
    }
    out
}

/// `tokens[start]` を含む括弧の閉じ `)` の添字（無ければ末尾）。
fn enclosing_scope_end(tokens: &[Token], start: usize) -> usize {
    let mut depth = 0usize;
    for k in start..tokens.len() {
        match tokens.get(k) {
            Some(Token::Punct('(')) => depth += 1,
            Some(Token::Punct(')')) => {
                if depth == 0 {
                    return k;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    tokens.len()
}

/// `tokens[open]` の `(` に対応する `)` の次の添字を返す（対応が無ければ `None`）。
fn skip_group(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for k in open..tokens.len() {
        match tokens.get(k) {
            Some(Token::Punct('(')) => depth += 1,
            Some(Token::Punct(')')) => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return k.checked_add(1);
                }
            }
            _ => {}
        }
    }
    None
}

/// 解決後の名前を裸の `Token::Ident` に置き換えると、後段の構文解析が構文語として
/// 再解釈しうるか（`DROP TABLE public.if EXISTS x` の `if` 等）。予約語トークンになる語、
/// または文脈依存の構文語に一致する名前は置き換えず拒否する（fail-closed）。
pub(crate) fn collides_with_syntax_word(name: &str) -> bool {
    const SYNTAX_WORDS: &[&str] = &[
        "IF",
        "NOT",
        "EXISTS",
        "ONLY",
        "TABLE",
        "TEMP",
        "TEMPORARY",
        "GLOBAL",
        "LOCAL",
        "UNLOGGED",
        "VIEW",
        "INDEX",
        "UNIQUE",
        "CONCURRENTLY",
        "RECURSIVE",
        "LATERAL",
        "NATURAL",
        "INNER",
        "LEFT",
        "RIGHT",
        "FULL",
        "CROSS",
        "OUTER",
        "JOIN",
        "ON",
        "USING",
        "SET",
        "VALUES",
        "DEFAULT",
        "AS",
        "CASCADE",
        "RESTRICT",
        "RESTART",
        "CONTINUE",
        "IDENTITY",
        "INTO",
        "REFERENCES",
        "WITH",
        "MATERIALIZED",
        "DISTINCT",
        "OR",
        "REPLACE",
        "IN",
        "IS",
        "NULL",
        "LIKE",
        "BETWEEN",
        "FOR",
        "SHARE",
        "MODE",
        "NOWAIT",
        "DELETE",
        "UPDATE",
        "INSERT",
        "TRUNCATE",
        "DROP",
        "ALTER",
        "CREATE",
        "COPY",
        "COMMENT",
        "LOCK",
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
    ];
    if SYNTAX_WORDS.iter().any(|w| name.eq_ignore_ascii_case(w)) {
        return true;
    }
    !matches!(
        crate::sql::lexer::tokenize(name).as_deref(),
        Ok([Token::Ident(n)]) if n == name
    )
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
        same("DROP TABLE public.a, public.b", "DROP TABLE a, b");
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
            "SELECT table, t.id FROM t LIMIT 10",
            "SELECT table, public.id FROM t",
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
    fn public_relation_with_same_name_cte_is_rejected() {
        for sql in [
            "WITH t AS (SELECT id FROM u) SELECT id FROM public.t LIMIT 10",
            "WITH t(id) AS (SELECT id FROM u) SELECT id FROM public.t",
            "WITH x AS (SELECT 1), T AS MATERIALIZED (SELECT 2) SELECT * FROM public.t",
        ] {
            assert_eq!(rewrite(sql).expect_err(sql).wire_code(), "0A000", "{sql}");
        }
        // 別名の CTE とは衝突しない。
        same(
            "WITH c AS (SELECT id FROM public.t) SELECT id FROM c",
            "WITH c AS (SELECT id FROM t) SELECT id FROM c",
        );
    }

    #[test]
    fn qualified_name_colliding_with_syntax_word_is_rejected() {
        for sql in [
            "DROP TABLE public.if EXISTS victim",
            "DROP TABLE public.IF EXISTS victim",
            "CREATE TABLE public.if NOT EXISTS victim (id TEXT)",
            "CREATE TABLE public.if not EXISTS victim (id TEXT)",
            "TRUNCATE public.only victim",
            "TRUNCATE public.table victim",
            "TRUNCATE TABLE public.only victim",
            "SELECT * FROM public.from",
        ] {
            assert_eq!(rewrite(sql).expect_err(sql).wire_code(), "42601", "{sql}");
        }
    }

    #[test]
    fn cte_collection_ignores_aliases_and_casts() {
        assert_eq!(
            collect_cte_names(
                &tokenize(
                    "WITH c AS (SELECT id FROM t AS x LIMIT 10) SELECT id FROM public.t LIMIT 10"
                )
                .expect("lex")
            ),
            vec!["c".to_string()]
        );
        assert_eq!(
            collect_cte_names(&tokenize("WITH a(x, y) AS (SELECT CAST(q AS TEXT) FROM u), b AS NOT MATERIALIZED (SELECT 1) SELECT 1").expect("lex")),
            vec!["a".to_string(), "b".to_string()]
        );
        for sql in [
            "WITH c AS (SELECT id FROM t AS x LIMIT 10) SELECT id FROM public.t LIMIT 10",
            "WITH c AS (SELECT CAST(id AS TEXT) FROM u) SELECT id FROM public.id",
            "WITH c AS (SELECT id FROM u AS t) SELECT id FROM public.t",
        ] {
            assert!(rewrite(sql).is_ok(), "{sql}");
        }
        let e = rewrite("WITH RECURSIVE x AS (SELECT 1), t AS (SELECT 2) SELECT * FROM public.t")
            .expect_err("cte");
        assert_eq!(e.wire_code(), "0A000");
    }

    #[test]
    fn residual_column_qualifier_keeps_legacy_syntax_error_path() {
        let mut t = tokenize("SELECT x.id FROM s.docs LIMIT 1").expect("lex");
        let before = t.clone();
        assert!(rewrite_relation_names(&mut t, None).is_ok());
        assert_eq!(t, before);
    }
}
