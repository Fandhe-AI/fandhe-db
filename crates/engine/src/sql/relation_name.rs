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
//! - それ以外: テナントが分からない入口では `3F000`。分かる入口（[`SchemaResolver`] を渡された
//!   場合）は `(tenant, schema)` の 1 回の照会で、無ければ `3F000`（他テナントの SCHEMA も
//!   同じ応答）、あれば内部キー `#<schema_id>.<name>`（ADR D2。[`RelationKey`]）の
//!   `Token::Ident` へ置き換える（Issue #1717。表・ビュー・索引・SEQUENCE の名前位置）。
//!   MATERIALIZED VIEW の SCHEMA 配置は未対応で `0A000`
//!
//! 内部キーを作れるのはこの解決器だけである（字句解析器は `#` を含む識別子を作らない。
//! 保存済みビュー本文だけは `lexer::tokenize_stored_view_body` が引用形で受理する）。
//!
//! 解決エラーはその位置で返し、置き換え用の名前で解析を続けない（別オブジェクトへの誤解決を
//! 構造的に防ぐ）。ただし選択リスト等に列修飾（`x.id`）が残る文は、従来の `42601` を保つため
//! 解決エラーを保留して書き換えずに返す（`SELECT x.id FROM s.docs` の既存契約）。

use crate::catalog::namespace::{is_builtin_schema_name, RelationKey};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::{Keyword, Token};

/// 呼び出し元テナントの SCHEMA 解決（`(tenant, schema)` の 1 回照会で内部 ID を返す。無ければ
/// `None`）。照会失敗は `XX000` 系（fail-closed）の [`SqlSurfaceError`] で返す。`core::EngineCore`
/// の ctx を持つ入口だけが `Storage::schema_id` を包んで渡す（テナントは ctx から。SQL 文からは
/// 変えられない）。
pub(crate) type SchemaResolver<'a> = &'a dyn Fn(&str) -> Result<Option<u64>, SqlSurfaceError>;

/// 名前位置の対象オブジェクト種別（置き換え後の可否判定に使う）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ObjKind {
    /// 表・ビュー・索引・SEQUENCE（SCHEMA 配下に置ける）。
    Plain,
    /// MATERIALIZED VIEW（SCHEMA 配下への配置は未対応。`0A000`）。
    Matview,
}

/// [`rewrite_relation_names`] の結果。
#[derive(Debug, Default)]
pub(crate) struct Rewritten {
    /// `public.` で修飾されていた名前（一時表との衝突検査用）。
    pub(crate) public_names: Vec<String>,
}

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
    /// 作成対象名（`CREATE TABLE`／`CREATE [OR REPLACE] VIEW`／`CREATE SEQUENCE` の名前）の位置か。
    pub create_name: bool,
    /// MATERIALIZED VIEW の名前位置か（SCHEMA 配下への配置は未対応で `0A000`。Issue #1717）。
    pub matview: bool,
}

/// `tokens[i]` が `DELETE FROM <表> [[AS] <別名>] USING <表の並び>` の `USING`（表参照リストを導く語）か。
/// 文頭が `DELETE FROM <表>` で、`USING` が対象表と別名の直後の構文位置にあり、直後が
/// `OPERATION_ID`（operation_id 句）でないときだけ真（SQL-53 (b)、Issue #1754）。`USING` は予約語では
/// ないため、WHERE 内の列名 `using` を USING 句と誤判定しないよう位置で限定する
/// （`allowlist::looks_like_delete_using` と同じ規則。こちらは修飾表名も許す）。
fn delete_using_opener(tokens: &[Token], i: usize) -> bool {
    if !ident_is(tokens.first(), "DELETE")
        || !matches!(tokens.get(1), Some(Token::Keyword(Keyword::From)))
        || !matches!(
            tokens.get(2),
            Some(Token::Ident(_) | Token::QualifiedIdent { .. })
        )
    {
        return false;
    }
    let mut pos: usize = 3;
    if ident_is(tokens.get(pos), "AS") {
        pos = pos.saturating_add(2);
    } else if matches!(tokens.get(pos), Some(Token::Ident(w))
        if !crate::sql::allowlist::is_dml_target_reserved_word(w))
    {
        pos = pos.saturating_add(1);
    }
    pos == i && ident_is(tokens.get(i), "USING") && !ident_is(tokens.get(i + 1), "OPERATION_ID")
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
    let head_drop = ident_is(tokens.first(), "DROP");
    // 文頭付近（先頭 6 トークン）に現れる最初のオブジェクト種別語（`CREATE UNIQUE INDEX`・
    // `CREATE OR REPLACE VIEW`・`DROP SEQUENCE` 等）。`MATERIALIZED VIEW` は種別が VIEW になるため
    // `head_matview` で区別する。
    let head6 = tokens.get(..6).unwrap_or(tokens);
    let ddl_obj: Option<&'static str> = head6.iter().find_map(|t| {
        ["TABLE", "VIEW", "INDEX", "SEQUENCE"]
            .into_iter()
            .find(|k| ident_is(Some(t), k))
    });
    let head_matview = head6.iter().any(|t| ident_is(Some(t), "MATERIALIZED"));
    let obj_ddl_head = ["CREATE", "DROP", "ALTER", "REFRESH"]
        .iter()
        .any(|k| ident_is(tokens.first(), k));
    let create_table = head_create && head.iter().any(|t| ident_is(Some(t), "TABLE"));
    let table_head = head.iter().any(|t| ident_is(Some(t), "TABLE"));
    // `TABLE` が表参照を導くのは DDL 文の先頭付近だけ（`SELECT table, t.id` の識別子 `table` と
    // 区別する）。
    let ddl_head = ["CREATE", "DROP", "ALTER", "TRUNCATE", "LOCK", "COMMENT"]
        .iter()
        .any(|k| ident_is(tokens.first(), k));
    let mut index_on_pending = head_create && head.iter().any(|t| ident_is(Some(t), "INDEX"));
    // `CREATE [OR REPLACE] TRIGGER <名前> ... ON <表> ...`・`DROP TRIGGER [IF EXISTS] <名前> ON <表>`
    // （TABLE-45・Issue #1722）。関係名の位置は最初の `ON` の直後だけ。それ以外の語（`UPDATE OF`・
    // `TRUNCATE ON` 等）を関係位置と誤検出して `search_path` ゲートへ渡さないよう、この文では下で
    // 位置判定を上書きする。
    let trigger_head = ((head_create || head_drop) && ident_is(tokens.get(1), "TRIGGER"))
        || (head_create && ident_is(tokens.get(1), "OR") && ident_is(tokens.get(3), "TRIGGER"));
    // `CREATE|ALTER|DROP POLICY <名前> ON <表> ...`（RLS-13・Issue #2274）も同じ規則で、関係名の位置は
    // 最初の `ON` の直後だけ。`USING (...)`・`WITH CHECK (...)` の式中の語（列名・`USING` 等）を関係位置と
    // 誤検出して `search_path` ゲートや解決へ渡さない。
    let policy_head = (head_create || head_drop || ident_is(tokens.first(), "ALTER"))
        && ident_is(tokens.get(1), "POLICY");
    let trigger_head = trigger_head || policy_head;
    let mut trigger_on_pending = trigger_head;
    // `GRANT|REVOKE <権限> [(列, ...)] ON [TABLE] <表>[, ...] TO|FROM <ロール>`（SQL-40・Issue #1965）。関係名の
    // 位置は `ON [TABLE]` の直後のカンマ区切りだけで、列リスト・`TO`／`FROM` の後のロール名・
    // `ON SCHEMA|SEQUENCE|FUNCTION|ALL ...`（範囲外の対象種別）は関係位置にしない（ロール名を表として解決させない）。
    let grant_head = ident_is(tokens.first(), "GRANT") || ident_is(tokens.first(), "REVOKE");
    // 0: `ON` の前、1: `ON` の直後、2: 名前待ち、3: 名前の直後（`,` で 2 へ）、4: 終了。
    let mut grant_phase: u8 = 0;

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
        let mut kind = ObjKind::Plain;
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
                } else if delete_using_opener(tokens, i) {
                    // `DELETE FROM t USING a, b`（SQL-53 (b)、Issue #1754）。以降のカンマは表参照リストの区切り。
                    next.list = true;
                } else if ((w.eq_ignore_ascii_case("TRUNCATE") || w.eq_ignore_ascii_case("LOCK"))
                    && i == 0)
                    || (w.eq_ignore_ascii_case("TABLE") && ddl_head && i < 4)
                    || (head_drop
                        && i < 4
                        && ["VIEW", "SEQUENCE", "INDEX"]
                            .iter()
                            .any(|k| w.eq_ignore_ascii_case(k)))
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
        // 直前の識別子（`"update"` 等の引用付き列名は構文語と同じ `Ident` になる）が式の位置にある
        // ときは、表参照を導く語ではなく式の一部なので表参照位置にしない。
        let prev3 = i.checked_sub(3).and_then(|p| tokens.get(p));
        let expr_ctx = match prev2 {
            Some(Token::Keyword(
                Keyword::Where
                | Keyword::And
                | Keyword::Select
                | Keyword::Order
                | Keyword::By
                | Keyword::Limit,
            )) => true,
            Some(Token::Punct(c)) => match c {
                '(' => {
                    !(ident_is(prev, "UPDATE")
                        && (ident_is(prev3, "AS") || ident_is(prev3, "MATERIALIZED")))
                }
                ',' | '=' | '+' | '-' | '/' | '>' | '<' | '%' | '*' | '#' | '[' | ':' => true,
                _ => false,
            },
            Some(Token::Le | Token::Ge | Token::Concat | Token::TypeCast | Token::DistanceOp) => {
                true
            }
            // `IF NOT EXISTS t`・`COMMENT ON TABLE t` の `NOT`／`ON` は表参照の文脈。
            Some(Token::Ident(w))
                if (w.eq_ignore_ascii_case("NOT") && ident_is(prev, "EXISTS"))
                    || (w.eq_ignore_ascii_case("ON") && ident_is(prev, "TABLE")) =>
            {
                false
            }
            Some(Token::Ident(w)) => [
                "AND", "OR", "NOT", "ON", "HAVING", "WHEN", "THEN", "ELSE", "IS", "IN", "LIKE",
                "ILIKE", "BETWEEN", "CASE", "BY",
            ]
            .iter()
            .any(|k| w.eq_ignore_ascii_case(k)),
            _ => false,
        };
        let prev_is_word = matches!(prev, Some(Token::Ident(_)));
        // 関係参照を導入する語（`ONLY`・`JOIN`・`INTO`・`REFERENCES`・索引 DDL の `ON`）の直後は、
        // 式の文脈判定（`expr_ctx`）で落とさない。直前の `prev2` が式の語に見えても、その語の
        // 直後にこれらの導入語が構文上現れる形は無く、`prev2` の側が引用符付きの識別子（表名・
        // 別名・索引名）か、導入語が本物の構文語である。判定に迷う位置は関係参照として扱う
        // （fail-closed。`search_path` の参照ゲートを素通りさせない。Issue #1715）。
        //  - `TRUNCATE a, ONLY b`・`FROM a, ONLY b`: カンマの直後でも、表参照リストの中なら `ONLY` は構文語
        //  - `FROM "in" JOIN b`・`CREATE INDEX is ON t`: 直前が式の語に見える引用付き識別子
        let introducer_keeps_relation = match prev {
            Some(Token::Ident(w)) => {
                let is = |k: &str| w.eq_ignore_ascii_case(k);
                let after_comma = matches!(prev2, Some(Token::Punct(',')));
                let after_word = matches!(prev2, Some(Token::Ident(_)));
                (is("ONLY") && after_comma && level.list)
                    || (after_word
                        && ((is("ON") && index_on_pending)
                            || is("INTO")
                            || is("REFERENCES")
                            // `DELETE FROM t AS case USING u`: 別名が式の語でも USING 直後は表参照
                            || (is("USING") && delete_using_opener(tokens, i.saturating_sub(1)))
                            || ((is("JOIN") || is("ONLY")) && level.list)))
            }
            _ => false,
        };
        if is_ident_like(cur)
            && !level.in_proj
            && !(prev_is_word && expr_ctx && !introducer_keeps_relation)
        {
            relation_pos = match prev {
                Some(Token::Keyword(Keyword::From)) => from_rel,
                Some(Token::Ident(w)) => {
                    let is = |k: &str| w.eq_ignore_ascii_case(k);
                    if is("TABLE") {
                        is_create_name = create_table;
                        ddl_head && i <= 4
                    } else if is("VIEW") {
                        // `CREATE [OR REPLACE] VIEW`・`DROP VIEW`・`ALTER VIEW`・
                        // `{CREATE|DROP|ALTER|REFRESH} MATERIALIZED VIEW` の名前位置。
                        if head_matview {
                            kind = ObjKind::Matview;
                        }
                        is_create_name = head_create;
                        obj_ddl_head && ddl_obj == Some("VIEW") && i <= 5
                    } else if is("SEQUENCE") {
                        is_create_name = head_create;
                        obj_ddl_head && ddl_obj == Some("SEQUENCE") && i <= 3
                    } else if is("INDEX") {
                        // 索引名の修飾は `DROP INDEX` だけ受け付ける（`CREATE INDEX s.i` は PG と同じく
                        // 構文エラー。索引は対象表の SCHEMA に置かれる）。
                        head_drop && ddl_obj == Some("INDEX") && i <= 2
                    } else if is("CONCURRENTLY") {
                        head_drop && ddl_obj == Some("INDEX") && i <= 3
                    } else if is("EXISTS") {
                        let if_prefixed = ident_is(prev2, "IF") || ident_is(prev2, "NOT");
                        if if_prefixed && head_matview && ddl_obj == Some("VIEW") {
                            kind = ObjKind::Matview;
                        }
                        let ok = if_prefixed
                            && match ddl_obj {
                                Some("TABLE") => table_head,
                                Some("VIEW" | "SEQUENCE") => obj_ddl_head,
                                Some("INDEX") => head_drop,
                                _ => false,
                            };
                        // 作成対象名（`CREATE TABLE|VIEW|SEQUENCE IF NOT EXISTS <名前>`）。
                        is_create_name = ok && head_create && ddl_obj != Some("INDEX");
                        ok
                    } else if is("COPY") {
                        i == 1
                    } else if is("USING") {
                        // `DELETE ... USING <表>`（SQL-53 (b)、Issue #1754）。
                        delete_using_opener(tokens, i.saturating_sub(1))
                    } else if is("ON") {
                        index_on_pending
                    } else {
                        is("JOIN")
                            || is("INTO")
                            || is("TRUNCATE")
                            // `LOCK <表>`（`TABLE` 省略形。Issue #1958）。文頭の `LOCK` の直後だけ。
                            // 直後が省略可能な `TABLE` 構文語（次が名前）のときは関係ではない。
                            || (is("LOCK")
                                && i == 1
                                && !(ident_is(Some(cur), "TABLE")
                                    && tokens.get(i.saturating_add(1)).is_some_and(is_ident_like)))
                            || is("UPDATE")
                            || is("ONLY")
                            || is("REFERENCES")
                    }
                }
                Some(Token::Punct(',')) => level.list,
                _ => false,
            };
        }
        if trigger_head {
            relation_pos = trigger_on_pending && ident_is(prev, "ON") && is_ident_like(cur);
            is_create_name = false;
            if ident_is(prev, "ON") {
                trigger_on_pending = false;
            }
        }
        if grant_head {
            relation_pos = false;
            is_create_name = false;
            let next_word = tokens.get(i.saturating_add(1)).is_some_and(|t| {
                is_ident_like(t) && !ident_is(Some(t), "TO") && !ident_is(Some(t), "FROM")
            });
            // 括弧内（権限の列リスト `SELECT ("on", "to")` 等）の語は構文語として扱わない。
            // 状態遷移は括弧深さ 0 の最上位の `ON`・名前・`,`・`TO`／`FROM` だけで起こす。
            let grant_top = levels.len() == 1;
            match grant_phase {
                _ if !grant_top => {}
                0 => {
                    if ident_is(Some(cur), "TO") || matches!(cur, Token::Keyword(Keyword::From)) {
                        grant_phase = 4;
                    } else if ident_is(Some(cur), "ON") {
                        grant_phase = 1;
                    }
                }
                1 => {
                    let kind_word = [
                        "SCHEMA",
                        "SEQUENCE",
                        "FUNCTION",
                        "PROCEDURE",
                        "ROUTINE",
                        "DATABASE",
                        "DOMAIN",
                        "TYPE",
                        "LANGUAGE",
                        "TABLESPACE",
                        "FOREIGN",
                        "LARGE",
                        "PARAMETER",
                        "ALL",
                    ]
                    .iter()
                    .any(|k| ident_is(Some(cur), k));
                    if ident_is(Some(cur), "TABLE") && next_word {
                        grant_phase = 2;
                    } else if kind_word && next_word {
                        grant_phase = 4;
                    } else {
                        relation_pos = is_ident_like(cur);
                        grant_phase = 3;
                    }
                }
                2 => {
                    relation_pos = is_ident_like(cur);
                    grant_phase = 3;
                }
                3 => {
                    grant_phase = if matches!(cur, Token::Punct(',')) {
                        2
                    } else {
                        4
                    };
                }
                _ => {}
            }
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
                    matview: kind == ObjKind::Matview,
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
    /// 修飾を外した（`public.` を裸の名前へ、テナントの SCHEMA を内部キーへ置き換えた）トークンの添字。
    /// 準備済み文が未修飾名の `search_path` ゲートから除外するために使う（置換は 1 対 1 なので位置は
    /// 保たれる。TABLE-27・Issue #1715）。
    pub positions: Vec<usize>,
    /// `CREATE TABLE` の作成対象名が `public.` で明示修飾されていたか。
    pub create_public: bool,
}

/// `tokens` のテーブル参照位置にある `schema.name` を解決して `Token::Ident(name)` へ置き換える。
/// `public.` は裸の名前へ、テナントの SCHEMA は内部キー（`#<id>.<name>`）へ置き換える。戻り値の
/// [`Rewritten::public_names`] は `public.` で修飾されていた名前（一時表との衝突検査用）。
/// 修飾が無ければ何もしない。`resolver` が `None`（テナント不明の入口）なら、public・組み込み
/// SCHEMA 以外は `3F000`。
pub(crate) fn rewrite_relation_names(
    tokens: &mut [Token],
    resolver: Option<SchemaResolver<'_>>,
) -> Result<Rewritten, SqlSurfaceError> {
    Ok(Rewritten {
        public_names: rewrite_relation_names_full(tokens, resolver)?.names,
    })
}

/// [`rewrite_relation_names`] の位置情報つき版。
pub(crate) fn rewrite_relation_names_full(
    tokens: &mut [Token],
    resolver: Option<SchemaResolver<'_>>,
) -> Result<RewriteInfo, SqlSurfaceError> {
    rewrite_sequence_name_literals(tokens, resolver)?;
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
    let mut residual_column_qualifier = false;
    let mut deferred: Option<SqlSurfaceError> = None;

    // 解決の対象: 表参照位置に加え、関数名の位置（`CREATE FUNCTION s.f(`・式中の `s.f(x)`）にある
    // テナントの SCHEMA 修飾（UDF の登録キー・呼び出しキー。Issue #1717）。`public.`・組み込み
    // SCHEMA・`pg_temp` の修飾は、裸の名前へ化けて組み込み関数に当たる経路を作らないよう従来どおり
    // 解決しない（構文エラーのまま）。出現順に処理して、最初の解決エラーを保留する。
    struct Target {
        index: usize,
        create_name: bool,
        matview: bool,
        function_pos: bool,
        /// `DROP FUNCTION s.f` の関数名位置（存在しない SCHEMA は `3F000` で fail-closed にする）。
        drop_name: bool,
    }
    let mut targets: Vec<Target> = walk
        .positions
        .iter()
        .map(|p| Target {
            index: p.index,
            create_name: p.create_name,
            matview: p.matview,
            function_pos: false,
            drop_name: false,
        })
        .collect();
    let mut public_drop_rewrites: Vec<(usize, String)> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let Token::QualifiedIdent { qualifier, .. } = t else {
            continue;
        };
        if walk.positions.iter().any(|p| p.index == i) {
            continue;
        }
        // `DROP FUNCTION s.f`（#1920）は型の並びの括弧を省略できるため、先頭が `DROP FUNCTION` の文では
        // 3 語目の修飾名を（括弧が無くても）関数位置として扱う。
        // `DROP FUNCTION IF EXISTS s.f`（Issue #1917）は名前が 5 語目にある。
        let drop_function_name = ident_is(tokens.first(), "DROP")
            && ident_is(tokens.get(1), "FUNCTION")
            && (i == 2
                || (i == 4 && ident_is(tokens.get(2), "IF") && ident_is(tokens.get(3), "EXISTS")));
        // `DROP FUNCTION public.f` は `DROP TABLE public.t` と同様に裸の名前へ正規化する
        // （DROP は UDF カタログの削除だけで、組み込み関数へ化ける経路にならない）。
        if drop_function_name && qualifier.eq_ignore_ascii_case("public") {
            if let Token::QualifiedIdent { name, .. } = t {
                public_drop_rewrites.push((i, name.clone()));
            }
            continue;
        }
        // `CREATE TRIGGER ... EXECUTE FUNCTION s.f(...)` の関数名（Issue #1722）も、`ON s.t` と同じく
        // 存在しない SCHEMA は `3F000` で fail-closed にする（式中の関数呼び出しではないため）。
        let trigger_function_name = i >= 2
            && matches!(tokens.get(i + 1), Some(Token::Punct('(')))
            && (ident_is(tokens.get(i - 1), "FUNCTION")
                || ident_is(tokens.get(i - 1), "PROCEDURE"))
            && ident_is(tokens.get(i - 2), "EXECUTE")
            && crate::sql::allowlist::is_create_trigger_statement(tokens);
        let strict_function_name = drop_function_name || trigger_function_name;
        let function_pos = resolver.is_some()
            && (strict_function_name || matches!(tokens.get(i + 1), Some(Token::Punct('('))))
            && !qualifier.eq_ignore_ascii_case("public")
            && !is_builtin_schema_name(qualifier)
            && !qualifier.eq_ignore_ascii_case("pg_temp");
        if function_pos {
            targets.push(Target {
                index: i,
                create_name: false,
                matview: false,
                function_pos: true,
                drop_name: strict_function_name,
            });
        } else {
            residual_column_qualifier = true;
        }
    }
    targets.sort_by_key(|t| t.index);
    for (i, name) in public_drop_rewrites {
        if let Some(slot) = tokens.get_mut(i) {
            *slot = Token::Ident(name);
        }
    }

    for pos in &targets {
        let i = pos.index;
        let Some(Token::QualifiedIdent { qualifier, name }) = tokens.get(i) else {
            continue;
        };
        let (qualifier, name) = (qualifier.clone(), name.clone());
        let kind = if pos.matview {
            ObjKind::Matview
        } else {
            ObjKind::Plain
        };
        match resolve(
            &qualifier,
            &name,
            // 一時表の作成対象名だけが対象（`REFERENCES public.u` 等の参照側は許可する）。
            create_temp_table && pos.create_name,
            kind,
            resolver,
        ) {
            Ok(Resolved::Public) => {
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
            Ok(Resolved::InSchema(key)) => {
                // 内部キーは CTE 名・一時表名と衝突しない（`#` を含む）。名前部だけが構文語に
                // 化けないか確認する。
                if collides_with_syntax_word(&name) {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified relation name collides with a syntax word",
                    ));
                }
                info.positions.push(i);
                if let Some(slot) = tokens.get_mut(i) {
                    *slot = Token::Ident(key);
                }
            }
            // 関数名の位置で SCHEMA が見つからない場合は、`x.lower(col)` のような関数呼び出し形の
            // 修飾を従来どおり構文エラー（`42601`。SQL-44 の範囲外の形）のままにする。
            Err(SqlSurfaceError::InvalidSchemaName { .. })
                if pos.function_pos && !pos.drop_name =>
            {
                residual_column_qualifier = true;
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
        Some(_) if residual_column_qualifier => Ok(info),
        Some(e) => Err(e),
        None => Ok(info),
    }
}

/// `nextval('s.q')`／`currval('s.q')`／`setval('s.q', ..)` の文字列引数（regclass 相当）のうち、
/// テナントの SCHEMA で修飾されたものを解決して、内部キーの `Token::Ident` へ置き換える
/// （TABLE-27・Issue #1717、ADR D2）。文字列リテラルの中身は利用者が自由に書けるため、解決後は
/// 文字列ではなく `#` を含む `Ident`（解決器だけが作れる）にして、利用者が内部キーを文字列で
/// 直接指定できないようにする（受理側は `sql::allowlist` の `try_parse_sequence_call` と
/// `validate_sequence_function_tokens`）。
///
/// `resolver` が `None`、修飾が `public`・組み込み SCHEMA・`pg_temp`、`.` が 2 つ以上、引用符を含む、
/// 各部が識別子規則に反する場合は何もせず、従来の拒否（`0A000`／`42601`）に任せる。SCHEMA が
/// 存在しない（他テナントの SCHEMA を含む）場合は `3F000`。
fn rewrite_sequence_name_literals(
    tokens: &mut [Token],
    resolver: Option<SchemaResolver<'_>>,
) -> Result<(), SqlSurfaceError> {
    let Some(probe) = resolver else {
        return Ok(());
    };
    for i in 0..tokens.len() {
        let is_call = matches!(tokens.get(i), Some(Token::Ident(w))
            if ["nextval", "currval", "setval"].iter().any(|k| w.eq_ignore_ascii_case(k)))
            && matches!(tokens.get(i + 1), Some(Token::Punct('(')));
        if !is_call {
            continue;
        }
        let Some(Token::StringLiteral(lit)) = tokens.get(i + 2) else {
            continue;
        };
        let Some((qualifier, name)) = lit.split_once('.') else {
            continue;
        };
        if qualifier.eq_ignore_ascii_case("public")
            || is_builtin_schema_name(qualifier)
            || qualifier.eq_ignore_ascii_case("pg_temp")
            || crate::catalog::validate_identifier(qualifier).is_err()
            || crate::catalog::validate_identifier(name).is_err()
        {
            continue;
        }
        let Some(schema_id) = probe(qualifier)? else {
            return Err(SqlSurfaceError::invalid_schema_name(qualifier));
        };
        let key = RelationKey::InSchema {
            schema_id,
            name: name.to_string(),
        }
        .encode();
        if let Some(slot) = tokens.get_mut(i + 2) {
            *slot = Token::Ident(key);
        }
    }
    Ok(())
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
        "ILIKE",
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

/// [`resolve`] の結果。
enum Resolved {
    /// `public`（裸の名前へ置き換える）。
    Public,
    /// テナントの SCHEMA 配下（内部キー `#<id>.<name>`）。
    InSchema(String),
}

/// `qualifier.name` 1 件の解決。`public` は裸の名前、テナントの SCHEMA は内部キーになる。
fn resolve(
    qualifier: &str,
    name: &str,
    create_temp: bool,
    kind: ObjKind,
    resolver: Option<SchemaResolver<'_>>,
) -> Result<Resolved, SqlSurfaceError> {
    if qualifier.eq_ignore_ascii_case("public") {
        if create_temp {
            // 一時ではない SCHEMA への一時表の作成（黙って一時表 `name` を作らない）。
            return Err(SqlSurfaceError::invalid_table_definition(name));
        }
        return Ok(Resolved::Public);
    }
    if is_builtin_schema_name(qualifier) || qualifier.eq_ignore_ascii_case("pg_temp") {
        return Err(SqlSurfaceError::feature_not_supported(
            "relations in this schema are not supported",
        ));
    }
    if create_temp {
        // 一時表は `public` 相当（一時ではない SCHEMA）に置けない。SCHEMA の存否で結果を
        // 変えないよう、照会の前に固定のエラーで拒否する（`public.t` と同じ経路）。
        return Err(SqlSurfaceError::invalid_table_definition(name));
    }
    // `(tenant, schema)` の 1 回の照会。他テナントにしか無い SCHEMA と存在しない SCHEMA は
    // 同じ `3F000`（存在オラクルにしない）。
    let schema_id = match resolver {
        Some(probe) => probe(qualifier)?,
        None => None,
    };
    let Some(schema_id) = schema_id else {
        return Err(SqlSurfaceError::invalid_schema_name(qualifier));
    };
    if kind == ObjKind::Matview {
        return Err(SqlSurfaceError::feature_not_supported(
            "materialized views inside a schema are not supported",
        ));
    }
    Ok(Resolved::InSchema(
        RelationKey::InSchema {
            schema_id,
            name: name.to_string(),
        }
        .encode(),
    ))
}

/// 利用者向けの文字列（エラー文言・EXPLAIN・Describe）に現れる内部キー `#<schema_id>.<name>` を
/// `<schema>.<name>` へ書き換える（TABLE-27・Issue #1717、ADR D2。内部 ID を利用者へ見せない）。
/// `names` はそのテナントの `内部 ID → SCHEMA 名`。逆引きできない ID（理論上は起きない）は
/// `#<id>.` を落として名前部だけを出す（`#` を出さない。fail-closed）。呼び出し元は
/// `core::EngineCore` の ctx を持つ入口の出口（エラーの `map_text`・EXPLAIN の行）。
pub(crate) fn display_relation_keys(
    text: &str,
    names: &std::collections::BTreeMap<u64, String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while let Some(rest) = text.get(i..) {
        let Some(ch) = rest.chars().next() else { break };
        if ch == '#' {
            let digits_end = bytes
                .iter()
                .skip(i + 1)
                .take_while(|b| b.is_ascii_digit())
                .count();
            if digits_end > 0 && bytes.get(i + 1 + digits_end) == Some(&b'.') {
                let id = text
                    .get(i + 1..i + 1 + digits_end)
                    .and_then(|d| d.parse::<u64>().ok());
                if let Some(id) = id {
                    if let Some(schema) = names.get(&id) {
                        out.push_str(schema);
                        out.push('.');
                    }
                    i += digits_end + 2;
                    continue;
                }
            }
        }
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// [`rewrite_relation_names`] を適用した字句解析（`sql::allowlist` の文字列入口用。テナント不明）。
pub(crate) fn tokenize_resolved(sql: &str, marked: bool) -> Result<Vec<Token>, SqlSurfaceError> {
    tokenize_resolved_with(sql, marked, None)
}

/// [`tokenize_resolved`] のテナント解決つき版。ctx を持つ入口（`core::EngineCore::execute_sql`・
/// `begin_copy*`）が、SQL 文字列を再字句化する検証器へ渡す前に使う。
pub(crate) fn tokenize_resolved_with(
    sql: &str,
    marked: bool,
    resolver: Option<SchemaResolver<'_>>,
) -> Result<Vec<Token>, SqlSurfaceError> {
    let tokens = if marked {
        crate::sql::lexer::tokenize_marked(sql)?
    } else {
        crate::sql::lexer::tokenize(sql)?
    };
    // 標準 SQL の行数制限句（`FETCH FIRST n ROWS ONLY` 等）を正規形へ書き換える（Issue #1793）。
    let mut tokens = crate::sql::fetch_clause::normalize_statement_tokens(tokens)?;
    rewrite_relation_names(&mut tokens, resolver)?;
    Ok(tokens)
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

    /// SQL-53 (b)・Issue #1754: `DELETE ... USING` の表参照リストと `UPDATE ... FROM` の各表が
    /// 表参照位置になる（`search_path` ゲートの素通りを防ぐ）。`USING OPERATION_ID` は参照位置にしない。
    #[test]
    fn delete_using_and_update_from_lists_are_relation_positions() {
        same(
            "DELETE FROM public.a USING public.b, public.c WHERE a.id = b.id",
            "DELETE FROM a USING b, c WHERE a.id = b.id",
        );
        same(
            "UPDATE public.a SET x = b.x FROM public.b, public.c WHERE a.id = b.id",
            "UPDATE a SET x = b.x FROM b, c WHERE a.id = b.id",
        );
        // 後続の `USING OPERATION_ID` は表参照ではない（従来どおり位置は DELETE FROM の表のみ）。
        let t = tokenize("DELETE FROM public.a USING OPERATION_ID 'x'").expect("lex");
        assert_eq!(relation_positions(&t).positions.len(), 1);
        let t = tokenize("DELETE FROM a USING b WHERE a.id = b.id USING OPERATION_ID 'x'")
            .expect("lex");
        assert_eq!(relation_positions(&t).positions.len(), 2);
        // `DELETE` で始まらない文の `USING`（`ALTER ... USING <式>`・JOIN の USING）は影響を受けない。
        let t = tokenize("ALTER TABLE t ALTER COLUMN c TYPE int USING c").expect("lex");
        assert_eq!(relation_positions(&t).positions.len(), 1);
    }

    #[test]
    fn quoted_keyword_column_in_expression_is_not_a_relation_position() {
        let t = tokenize("SELECT id FROM public.t WHERE \"update\" IS NULL LIMIT 5").expect("lex");
        let walk = relation_positions(&t);
        assert_eq!(walk.positions.len(), 1);
        assert!(matches!(
            t.get(walk.positions[0].index),
            Some(Token::QualifiedIdent { .. })
        ));
    }

    /// #1920: `DROP FUNCTION public.f` は裸の名前へ正規化され、存在しない SCHEMA は `3F000`。
    #[test]
    fn drop_function_public_is_bare_and_unknown_schema_is_3f000() {
        same("DROP FUNCTION public.f", "DROP FUNCTION f");
        same(
            "DROP FUNCTION PUBLIC.f(integer)",
            "DROP FUNCTION f(integer)",
        );
        let none = |_: &str| -> Result<Option<u64>, SqlSurfaceError> { Ok(None) };
        let mut t = tokenize("DROP FUNCTION nope.f").expect("lex");
        let err = rewrite_relation_names(&mut t, Some(&none)).expect_err("unknown schema");
        assert_eq!(err.wire_code(), "3F000");
        let some = |_: &str| -> Result<Option<u64>, SqlSurfaceError> { Ok(Some(7)) };
        let mut t = tokenize("DROP FUNCTION s.f(integer)").expect("lex");
        rewrite_relation_names(&mut t, Some(&some)).expect("resolved");
        assert!(matches!(t.get(2), Some(Token::Ident(k)) if k.contains('#')));
    }

    /// Issue #1917: `DROP FUNCTION IF EXISTS s.f` の名前位置も判定し、`public.` は裸の名前へ正規化、
    /// 存在しない SCHEMA は `3F000` のまま fail-closed にする。`CREATE OR REPLACE FUNCTION s.f(` も
    /// 関数名位置として解決される。
    #[test]
    fn drop_function_if_exists_and_or_replace_resolve_schema() {
        same(
            "DROP FUNCTION IF EXISTS public.f CASCADE",
            "DROP FUNCTION IF EXISTS f CASCADE",
        );
        let none = |_: &str| -> Result<Option<u64>, SqlSurfaceError> { Ok(None) };
        let mut t = tokenize("DROP FUNCTION IF EXISTS nope.f").expect("lex");
        let err = rewrite_relation_names(&mut t, Some(&none)).expect_err("unknown schema");
        assert_eq!(err.wire_code(), "3F000");
        let some = |_: &str| -> Result<Option<u64>, SqlSurfaceError> { Ok(Some(7)) };
        let mut t = tokenize("DROP FUNCTION IF EXISTS s.f(integer)").expect("lex");
        rewrite_relation_names(&mut t, Some(&some)).expect("resolved");
        assert!(matches!(t.get(4), Some(Token::Ident(k)) if k.contains('#')));
        let mut t = tokenize("CREATE OR REPLACE FUNCTION s.f(x) AS x").expect("lex");
        rewrite_relation_names(&mut t, Some(&some)).expect("resolved");
        assert!(matches!(t.get(4), Some(Token::Ident(k)) if k.contains('#')));
        // 存在しない SCHEMA への作成は、`CREATE FUNCTION` と同じく書き換えでは拒否せず、
        // 実行側（SCHEMA の現存確認）が拒否する。`OR REPLACE` の有無で挙動を変えない。
        let mut plain = tokenize("CREATE FUNCTION nope.f(x) AS x").expect("lex");
        let mut replace = tokenize("CREATE OR REPLACE FUNCTION nope.f(x) AS x").expect("lex");
        assert_eq!(
            rewrite_relation_names(&mut plain, Some(&none)).is_ok(),
            rewrite_relation_names(&mut replace, Some(&none)).is_ok()
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
        // カンマに続く `ONLY` の対象表（式の文脈と取り違えない）。
        same("TRUNCATE public.a, ONLY public.b", "TRUNCATE a, ONLY b");
        // `LOCK`（`TABLE` 省略形・複数表。Issue #1958）。
        same("LOCK public.t", "LOCK t");
        same(
            "LOCK TABLE public.t IN SHARE MODE",
            "LOCK TABLE t IN SHARE MODE",
        );
        same(
            "LOCK TABLE ONLY public.a, public.b NOWAIT",
            "LOCK TABLE ONLY a, b NOWAIT",
        );
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

    /// SCHEMA `s` を内部 ID 12 へ解決する照会（`z` は存在しない）。
    fn probe(s: &str) -> Result<Option<u64>, SqlSurfaceError> {
        Ok((s == "s").then_some(12))
    }

    fn rewrite_in_tenant(sql: &str) -> Result<Vec<Token>, SqlSurfaceError> {
        let mut t = tokenize(sql).expect("lex");
        rewrite_relation_names(&mut t, Some(&probe))?;
        Ok(t)
    }

    /// TABLE-27・Issue #1717: テナントの SCHEMA 配下の名前位置は内部キーの `Ident` へ置き換わる
    /// （表・ビュー・索引・SEQUENCE の作成・参照・削除）。未解決の SCHEMA は `3F000`。
    #[test]
    fn tenant_schema_names_are_rewritten_to_internal_keys() {
        for (qualified, plain) in [
            ("SELECT * FROM s.t", "SELECT * FROM \"#12.t\""),
            (
                "CREATE TABLE s.t (id TEXT)",
                "CREATE TABLE \"#12.t\" (id TEXT)",
            ),
            ("DROP TABLE IF EXISTS s.t", "DROP TABLE IF EXISTS \"#12.t\""),
            (
                "CREATE VIEW s.v AS SELECT id FROM s.t",
                "CREATE VIEW \"#12.v\" AS SELECT id FROM \"#12.t\"",
            ),
            ("DROP VIEW s.v", "DROP VIEW \"#12.v\""),
            ("DROP INDEX IF EXISTS s.i", "DROP INDEX IF EXISTS \"#12.i\""),
            ("CREATE SEQUENCE s.q", "CREATE SEQUENCE \"#12.q\""),
            ("DROP SEQUENCE s.q", "DROP SEQUENCE \"#12.q\""),
            (
                "CREATE INDEX i ON s.t (a)",
                "CREATE INDEX i ON \"#12.t\" (a)",
            ),
        ] {
            let got = rewrite_in_tenant(qualified).expect(qualified);
            // 期待側は引用形の内部キーを字句解析できないため、トークン列を直接組み立てて比較する。
            let want: Vec<Token> = tokenize(&plain.replace("\"#12.", "\"internal_"))
                .expect("lex")
                .into_iter()
                .map(|t| match t {
                    Token::Ident(n) if n.starts_with("internal_") => {
                        Token::Ident(format!("#12.{}", &n["internal_".len()..]))
                    }
                    other => other,
                })
                .collect();
            assert_eq!(got, want, "{qualified}");
        }
        assert_eq!(
            rewrite_in_tenant("SELECT * FROM z.t")
                .expect_err("unknown")
                .wire_code(),
            "3F000"
        );
    }

    /// CREATE INDEX の索引名の修飾は PG と同じく構文エラー（索引は対象表の SCHEMA に置かれる）。
    #[test]
    fn index_name_qualifier_on_create_is_left_to_the_parser() {
        let mut t = tokenize("CREATE INDEX s.i ON s.t (a)").expect("lex");
        let before = t.clone();
        assert!(rewrite_relation_names(&mut t, Some(&probe)).is_ok());
        assert_ne!(t, before);
        assert!(t.iter().any(|x| matches!(x, Token::QualifiedIdent { .. })));
    }

    /// RLS-13・Issue #2274: `CREATE|ALTER|DROP POLICY ... ON <表>` の表名だけが解決され、式中の語
    /// （`USING`・`WITH CHECK`・列名）は関係位置にならない。未知の SCHEMA は `3F000`。
    #[test]
    fn policy_ddl_names_are_resolved() {
        same(
            "CREATE POLICY p ON public.t FOR SELECT USING (owner = 'a')",
            "CREATE POLICY p ON t FOR SELECT USING (owner = 'a')",
        );
        same(
            "ALTER POLICY p ON public.t RENAME TO q",
            "ALTER POLICY p ON t RENAME TO q",
        );
        same(
            "DROP POLICY IF EXISTS p ON public.t",
            "DROP POLICY IF EXISTS p ON t",
        );
        let got = rewrite_in_tenant("CREATE POLICY p ON s.t USING (a > 1)").expect("resolved");
        let keys: Vec<&str> = got
            .iter()
            .filter_map(|t| match t {
                Token::Ident(n) if n.starts_with('#') => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["#12.t"]);
        assert_eq!(
            rewrite_in_tenant("DROP POLICY p ON z.t")
                .expect_err("unknown")
                .wire_code(),
            "3F000"
        );
        let t = tokenize("CREATE POLICY p ON t USING (a > 1) WITH CHECK (b > 1)").expect("lex");
        assert_eq!(relation_positions(&t).positions.len(), 1);
    }

    /// SQL-40・Issue #1965: `GRANT`／`REVOKE` は `ON [TABLE]` の直後のカンマ区切りだけが関係位置で、列リスト・
    /// `TO`／`FROM` の後のロール名・範囲外の対象種別（`ON SCHEMA s` 等）は関係位置にならない。
    #[test]
    fn grant_revoke_names_are_resolved() {
        same(
            "GRANT SELECT (a, b) ON TABLE public.t, public.u TO alice WITH GRANT OPTION",
            "GRANT SELECT (a, b) ON TABLE t, u TO alice WITH GRANT OPTION",
        );
        same(
            "REVOKE GRANT OPTION FOR ALL ON public.t FROM alice, bob CASCADE",
            "REVOKE GRANT OPTION FOR ALL ON t FROM alice, bob CASCADE",
        );
        let got = rewrite_in_tenant("GRANT SELECT ON s.t TO alice").expect("resolved");
        assert!(got.contains(&Token::Ident("#12.t".to_string())));
        assert_eq!(
            rewrite_in_tenant("GRANT SELECT ON z.t TO alice")
                .expect_err("unknown")
                .wire_code(),
            "3F000"
        );
        let count = |sql: &str| {
            relation_positions(&tokenize(sql).expect("lex"))
                .positions
                .len()
        };
        // ロール名・列名・種別語は関係位置ではない。
        assert_eq!(count("REVOKE SELECT ON t FROM alice"), 1);
        assert_eq!(count("GRANT SELECT (a, b) ON t, u TO a, b"), 2);
        assert_eq!(count("GRANT SELECT ON TABLE t TO a"), 1);
        assert_eq!(count("GRANT r1, r2 TO u"), 0);
        assert_eq!(count("REVOKE ADMIN OPTION FOR r FROM u"), 0);
        assert_eq!(count("GRANT USAGE ON SCHEMA s TO a"), 0);
        assert_eq!(count("GRANT SELECT ON ALL TABLES IN SCHEMA s TO a"), 0);
        assert_eq!(count("GRANT SELECT ON \"schema\" TO a"), 1);
    }

    /// TABLE-45・Issue #1722: `CREATE TRIGGER ... ON <表>`／`DROP TRIGGER ... ON <表>` の表名と
    /// `EXECUTE FUNCTION <関数>(...)` の関数名は解決される。`UPDATE OF <列>` の語は関係位置にならない。
    #[test]
    fn trigger_ddl_names_are_resolved() {
        same(
            "CREATE TRIGGER g AFTER UPDATE OF a ON public.t EXECUTE FUNCTION f()",
            "CREATE TRIGGER g AFTER UPDATE OF a ON t EXECUTE FUNCTION f()",
        );
        same(
            "DROP TRIGGER IF EXISTS g ON public.t",
            "DROP TRIGGER IF EXISTS g ON t",
        );
        let got = rewrite_in_tenant(
            "CREATE OR REPLACE TRIGGER g BEFORE INSERT ON s.t EXECUTE FUNCTION s.f()",
        )
        .expect("resolved");
        let keys: Vec<&str> = got
            .iter()
            .filter_map(|t| match t {
                Token::Ident(n) if n.starts_with('#') => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["#12.t", "#12.f"]);
        let got = rewrite_in_tenant("DROP TRIGGER g ON s.t").expect("resolved");
        assert!(got
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n == "#12.t")));
        assert_eq!(
            rewrite_in_tenant("DROP TRIGGER g ON z.t")
                .expect_err("unknown")
                .wire_code(),
            "3F000"
        );
        // 位置走査は `ON` の直後の 1 つだけ（`UPDATE OF`・`TRUNCATE ON` を関係位置にしない）。
        let t = tokenize("CREATE TRIGGER g AFTER TRUNCATE ON t EXECUTE FUNCTION f()").expect("lex");
        assert_eq!(relation_positions(&t).positions.len(), 1);
        let t = tokenize("CREATE TRIGGER g AFTER UPDATE OF a, b ON t EXECUTE FUNCTION f()")
            .expect("lex");
        assert_eq!(relation_positions(&t).positions.len(), 1);
    }

    /// MATERIALIZED VIEW の SCHEMA 配置は未対応（`0A000`）。存在しない SCHEMA は `3F000`。
    #[test]
    fn matview_in_tenant_schema_is_unsupported() {
        assert_eq!(
            rewrite_in_tenant("CREATE MATERIALIZED VIEW s.m AS SELECT id FROM t")
                .expect_err("matview")
                .wire_code(),
            "0A000"
        );
        assert_eq!(
            rewrite_in_tenant("CREATE MATERIALIZED VIEW z.m AS SELECT id FROM t")
                .expect_err("matview")
                .wire_code(),
            "3F000"
        );
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

    /// TABLE-27・Issue #1717: 利用者向けの文字列から内部キーを除き、SCHEMA 名へ戻す（逆引きできない ID は
    /// `#<id>.` を落とす。`#` を残さない）。
    #[test]
    fn display_relation_keys_restores_schema_names_and_never_leaves_hash() {
        let names: std::collections::BTreeMap<u64, String> =
            [(12, "s".to_string()), (7, "other".to_string())].into();
        for (input, want) in [
            ("undefined table: #12.t", "undefined table: s.t"),
            ("a #12.x and #7.y", "a s.x and other.y"),
            ("unknown #99.z here", "unknown z here"),
            ("no keys here", "no keys here"),
            ("#", "#"),
            ("#12", "#12"),
            ("#x.y", "#x.y"),
            ("日本語 #12.表", "日本語 s.表"),
        ] {
            assert_eq!(display_relation_keys(input, &names), want, "{input}");
        }
        // 数字の桁あふれは変換せずそのまま（`#` は残るが、内部キーとして生成されない形）。
        let huge = "#99999999999999999999999.t";
        assert_eq!(display_relation_keys(huge, &names), huge);
    }

    /// `nextval('s.q')` の文字列引数は、テナントの SCHEMA 修飾のときだけ内部キーの `Ident` へ置き換わる。
    /// Issue #1722: `CREATE TRIGGER ... EXECUTE FUNCTION z.f()` の未知 SCHEMA は `ON z.t` と同じ `3F000`。
    #[test]
    fn trigger_function_unknown_schema_is_3f000() {
        for sql in [
            "CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION z.f()",
            "CREATE TRIGGER g AFTER INSERT ON t EXECUTE PROCEDURE z.f()",
            "CREATE TRIGGER g AFTER INSERT ON z.t EXECUTE FUNCTION f()",
        ] {
            assert_eq!(
                rewrite_in_tenant(sql).expect_err(sql).wire_code(),
                "3F000",
                "{sql}"
            );
        }
        let ok = rewrite_in_tenant("CREATE TRIGGER g AFTER INSERT ON t EXECUTE FUNCTION s.f()")
            .expect("known schema");
        assert!(ok.contains(&Token::Ident("#12.f".to_string())));
    }

    /// `public.`・組み込み SCHEMA・引用符・複数の `.`・不正な識別子は何もしない（従来の拒否に任せる）。
    #[test]
    fn sequence_name_literals_are_resolved_only_for_tenant_schemas() {
        let resolved = rewrite_in_tenant("INSERT INTO t (n) VALUES (nextval('s.q'))").expect("ok");
        assert!(resolved.contains(&Token::Ident("#12.q".to_string())));
        assert!(!resolved.contains(&Token::StringLiteral("s.q".to_string())));
        for sql in [
            "INSERT INTO t (n) VALUES (nextval('public.q'))",
            "INSERT INTO t (n) VALUES (nextval('pg_catalog.q'))",
            "INSERT INTO t (n) VALUES (nextval('s.q.r'))",
            "INSERT INTO t (n) VALUES (nextval('s.\"q\"'))",
            "INSERT INTO t (n) VALUES (nextval('s.1q'))",
            "INSERT INTO t (n) VALUES (nextval('#12.q'))",
        ] {
            let before = tokenize(sql).expect("lex");
            let after = rewrite_in_tenant(sql).expect(sql);
            assert_eq!(before, after, "{sql}");
        }
        // 存在しない SCHEMA は 3F000。テナント不明の入口（resolver なし）は何もしない。
        assert_eq!(
            rewrite_in_tenant("INSERT INTO t (n) VALUES (nextval('z.q'))")
                .expect_err("unknown")
                .wire_code(),
            "3F000"
        );
        let mut t = tokenize("INSERT INTO t (n) VALUES (nextval('s.q'))").expect("lex");
        let before = t.clone();
        rewrite_relation_names(&mut t, None).expect("ok");
        assert_eq!(t, before);
    }

    /// 関数名の位置の修飾は、テナントの SCHEMA だけが内部キーになる。未解決の SCHEMA・`public.`・組み込み
    /// SCHEMA は解決せず（構文エラーのまま）、列修飾の保留経路を保つ。
    #[test]
    fn qualified_function_names_resolve_only_in_tenant_schemas() {
        let t = rewrite_in_tenant("SELECT s.f(x) FROM t LIMIT 1").expect("ok");
        assert!(t.contains(&Token::Ident("#12.f".to_string())));
        let t = rewrite_in_tenant("CREATE FUNCTION s.f(x) AS x").expect("ok");
        assert!(t.contains(&Token::Ident("#12.f".to_string())));
        for sql in [
            "SELECT z.f(x) FROM t LIMIT 1",
            "SELECT public.f(x) FROM t LIMIT 1",
            "SELECT pg_catalog.f(x) FROM t LIMIT 1",
        ] {
            let after = rewrite_in_tenant(sql).expect(sql);
            assert_eq!(after, tokenize(sql).expect("lex"), "{sql}");
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
