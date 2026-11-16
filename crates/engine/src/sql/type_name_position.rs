//! 型名の位置の走査（TABLE-27・TABLE-44・Issue #3289。ADR `docs/design/schema-tenant-namespace.md`
//! の「追記（Issue #3289）」のポインタ）。
//!
//! 責務境界: トークン列の中で「型名が書かれる位置」（列型・キャストの型）にある識別子の添字を集めるだけで、
//! トークン列は書き換えない。関係位置（`sql::relation_name::relation_positions`）とは別の走査器にしている。
//! 参照ゲート・読み取り権限の参照名集め・RLS の名前集めが型名を relation と取り違えないため
//! （Issue #3288 の K2 と同じ理由）。
//!
//! 呼び出し元: `sql::relation_name::rewrite_relation_names_full`（修飾名 `s.mood` の解決）と
//! `sql::search_path`（未修飾の列型名の `search_path` 解決・参照ゲート）。
//!
//! 対象の位置（どれも直後のトークンが識別子系のときだけ）:
//! - `::` の直後（キャスト）
//! - `CAST ( <式> AS <型>` の `AS` の直後（キャスト）
//! - `CREATE [..] TABLE [IF NOT EXISTS] <名> ( <列名> <型> , ..` の各列定義の型
//! - `ALTER TABLE .. ADD [COLUMN] [IF NOT EXISTS] <列> <型>`・`ALTER [COLUMN] <列> [SET DATA] TYPE <型>`
//!
//! 取りこぼしは構文段の `42601` のまま（拒否側）。誤検出した位置の修飾名は型名として解決できなければ
//! 必ずエラーで返る（別オブジェクトへ化ける経路は無い）。括弧の段数には [`MAX_LEVELS`] の上限を設ける。

use crate::sql::lexer::Token;

/// 括弧のネスト段数の上限（超えた `(` は畳む。走査状態の確保を抑える）。
const MAX_LEVELS: usize = 256;

/// 型名の位置 1 件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TypePos {
    /// `tokens` 内の添字。
    pub index: usize,
    /// 列型の位置（`CREATE TABLE`・`ADD COLUMN`・`ALTER COLUMN .. TYPE`）か。偽ならキャストの位置。
    /// 未修飾の型名の `search_path` 解決は列型の位置だけが対象（キャストは従来どおりカタログを引かない）。
    pub column_type: bool,
}

fn word(tok: Option<&Token>) -> Option<&str> {
    match tok {
        Some(Token::Ident(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn is_word(tok: Option<&Token>, w: &str) -> bool {
    word(tok).is_some_and(|s| s.eq_ignore_ascii_case(w))
}

/// 識別子系（修飾あり・なし・引用付き）か。
fn is_name(tok: Option<&Token>) -> bool {
    tok.is_some_and(crate::sql::relation_name::is_ident_like)
}

/// 列定義ではなく表制約・`LIKE` を始める語。
fn is_constraint_word(tok: Option<&Token>) -> bool {
    [
        "CONSTRAINT",
        "PRIMARY",
        "UNIQUE",
        "FOREIGN",
        "CHECK",
        "EXCLUDE",
        "LIKE",
    ]
    .iter()
    .any(|k| is_word(tok, k))
}

/// `tokens` から型名の位置を集める（出現順・重複なし）。
pub(crate) fn type_name_positions(tokens: &[Token]) -> Vec<TypePos> {
    let mut out: Vec<TypePos> = Vec::new();
    cast_positions(tokens, &mut out);
    create_table_positions(tokens, &mut out);
    alter_table_positions(tokens, &mut out);
    out.sort_by_key(|p| p.index);
    out.dedup_by_key(|p| p.index);
    out
}

/// `::<型>` と `CAST ( .. AS <型>`。
fn cast_positions(tokens: &[Token], out: &mut Vec<TypePos>) {
    // 括弧の段ごとの「`CAST (` の括弧か」。上限を超えた段は畳む（`overflow` で対応する `)` を数える）。
    let mut stack: Vec<bool> = Vec::new();
    let mut overflow: usize = 0;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::TypeCast => {
                if is_name(tokens.get(i + 1)) {
                    out.push(TypePos {
                        index: i + 1,
                        column_type: false,
                    });
                }
            }
            Token::Punct('(') => {
                let is_cast = i
                    .checked_sub(1)
                    .is_some_and(|p| is_word(tokens.get(p), "CAST"));
                if stack.len() < MAX_LEVELS {
                    stack.push(is_cast);
                } else {
                    overflow += 1;
                }
            }
            Token::Punct(')') => {
                if overflow > 0 {
                    overflow -= 1;
                } else {
                    stack.pop();
                }
            }
            Token::Ident(w)
                if w.eq_ignore_ascii_case("AS")
                    && overflow == 0
                    && stack.last().copied().unwrap_or(false)
                    && is_name(tokens.get(i + 1)) =>
            {
                out.push(TypePos {
                    index: i + 1,
                    column_type: false,
                });
            }
            _ => {}
        }
    }
}

/// `CREATE [GLOBAL|LOCAL] [TEMP|TEMPORARY|UNLOGGED] TABLE [IF NOT EXISTS] <名> (` の列定義の型。
fn create_table_positions(tokens: &[Token], out: &mut Vec<TypePos>) {
    if !is_word(tokens.first(), "CREATE") {
        return;
    }
    let mut i = 1;
    while ["GLOBAL", "LOCAL", "TEMP", "TEMPORARY", "UNLOGGED"]
        .iter()
        .any(|k| is_word(tokens.get(i), k))
    {
        i += 1;
    }
    if !is_word(tokens.get(i), "TABLE") {
        return;
    }
    i += 1;
    if is_word(tokens.get(i), "IF")
        && is_word(tokens.get(i + 1), "NOT")
        && is_word(tokens.get(i + 2), "EXISTS")
    {
        i += 3;
    }
    if !is_name(tokens.get(i)) || !matches!(tokens.get(i + 1), Some(Token::Punct('('))) {
        return;
    }
    // 第 1 段（`(` の直後）の要素の先頭だけを見る。入れ子の括弧は段数で追う。
    let mut depth: usize = 1;
    let mut j = i + 2;
    let mut at_start = true;
    while let Some(t) = tokens.get(j) {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            Token::Punct(',') if depth == 1 => {
                at_start = true;
                j += 1;
                continue;
            }
            _ => {}
        }
        if at_start && depth == 1 {
            at_start = false;
            if matches!(t, Token::Ident(_))
                && !is_constraint_word(Some(t))
                && is_name(tokens.get(j + 1))
            {
                out.push(TypePos {
                    index: j + 1,
                    column_type: true,
                });
            }
        }
        j += 1;
    }
}

/// `ALTER TABLE [IF EXISTS] [ONLY] <表> <アクション>[, ..]` の列の追加・型の変更。
fn alter_table_positions(tokens: &[Token], out: &mut Vec<TypePos>) {
    if !(is_word(tokens.first(), "ALTER") && is_word(tokens.get(1), "TABLE")) {
        return;
    }
    let mut i = 2;
    if is_word(tokens.get(i), "IF") && is_word(tokens.get(i + 1), "EXISTS") {
        i += 2;
    }
    if is_word(tokens.get(i), "ONLY") {
        i += 1;
    }
    if !is_name(tokens.get(i)) {
        return;
    }
    i += 1;
    // 最上位のカンマで区切った各アクションの先頭添字。
    let mut starts: Vec<usize> = vec![i];
    let mut depth: usize = 0;
    for (j, t) in tokens.iter().enumerate().skip(i) {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Punct(',') if depth == 0 => starts.push(j + 1),
            _ => {}
        }
    }
    for s in starts {
        let mut k = s;
        if is_word(tokens.get(k), "ADD") {
            k += 1;
            if is_word(tokens.get(k), "COLUMN") {
                k += 1;
            } else if is_constraint_word(tokens.get(k)) {
                continue;
            }
            if is_word(tokens.get(k), "IF")
                && is_word(tokens.get(k + 1), "NOT")
                && is_word(tokens.get(k + 2), "EXISTS")
            {
                k += 3;
            }
            // `<列> <型>`
            if is_name(tokens.get(k)) && is_name(tokens.get(k + 1)) {
                out.push(TypePos {
                    index: k + 1,
                    column_type: true,
                });
            }
        } else if is_word(tokens.get(k), "ALTER") {
            k += 1;
            if is_word(tokens.get(k), "COLUMN") {
                k += 1;
            }
            if !is_name(tokens.get(k)) {
                continue;
            }
            k += 1;
            if is_word(tokens.get(k), "SET") && is_word(tokens.get(k + 1), "DATA") {
                k += 2;
            }
            if is_word(tokens.get(k), "TYPE") && is_name(tokens.get(k + 1)) {
                out.push(TypePos {
                    index: k + 1,
                    column_type: true,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positions(sql: &str) -> Vec<(String, bool)> {
        let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
        type_name_positions(&tokens)
            .into_iter()
            .map(|p| {
                let name = match tokens.get(p.index) {
                    Some(Token::Ident(n)) => n.clone(),
                    Some(Token::QualifiedIdent { qualifier, name }) => {
                        format!("{qualifier}.{name}")
                    }
                    other => format!("{other:?}"),
                };
                (name, p.column_type)
            })
            .collect()
    }

    fn names(sql: &str) -> Vec<String> {
        positions(sql).into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn cast_positions_cover_double_colon_and_cast_as() {
        assert_eq!(names("SELECT 'a'::s.mood"), ["s.mood"]);
        assert_eq!(names("SELECT CAST('a' AS s.mood)"), ["s.mood"]);
        assert_eq!(names("SELECT CAST((1 + 2) AS s.t), x::int"), ["s.t", "int"]);
        // 入れ子の括弧の内側の `AS` は別名ではなく、外側の CAST の `AS` だけを見る。
        assert_eq!(
            names("SELECT CAST(CAST(1 AS int) AS s.mood)"),
            ["int", "s.mood"]
        );
        // CAST ではない括弧の中の `AS`（別名）は型名の位置ではない。
        assert!(names("SELECT (a AS b) FROM t").is_empty());
        assert!(positions("SELECT 'a'::s.mood")
            .iter()
            .all(|(_, column_type)| !column_type));
    }

    #[test]
    fn create_table_column_types_are_positions_but_constraints_are_not() {
        let got = positions(
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.mood, ms mood[], \
             CONSTRAINT c CHECK (m = 'a'::s.mood), PRIMARY KEY (id), UNIQUE (m), \
             d TEXT DEFAULT (lower('A')))",
        );
        let col: Vec<&str> = got
            .iter()
            .filter(|(_, c)| *c)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(col, ["BIGINT", "s.mood", "mood", "TEXT"]);
        assert!(got.contains(&("s.mood".to_string(), false)));
        assert!(names("CREATE TABLE t (LIKE u)").is_empty());
        // CTAS の列名リストは型名の位置ではない。
        assert!(names("CREATE TABLE t (a, b) AS SELECT 1, 2").is_empty());
        assert_eq!(
            names("CREATE TEMP TABLE IF NOT EXISTS t (m s.mood)"),
            ["s.mood"]
        );
    }

    #[test]
    fn alter_table_actions_are_positions() {
        assert_eq!(names("ALTER TABLE t ADD COLUMN m s.mood"), ["s.mood"]);
        assert_eq!(names("ALTER TABLE t ADD m s.mood"), ["s.mood"]);
        assert_eq!(
            names("ALTER TABLE IF EXISTS ONLY t ADD COLUMN IF NOT EXISTS m s.mood"),
            ["s.mood"]
        );
        assert_eq!(
            names("ALTER TABLE t ALTER COLUMN m TYPE s.mood"),
            ["s.mood"]
        );
        assert_eq!(
            names("ALTER TABLE t ALTER m SET DATA TYPE s.mood"),
            ["s.mood"]
        );
        assert_eq!(
            names("ALTER TABLE t ADD COLUMN a s.m1, ADD COLUMN b s.m2, ALTER COLUMN c TYPE s.m3"),
            ["s.m1", "s.m2", "s.m3"]
        );
        // 制約の追加・既定値の設定・列の削除は型名の位置を持たない。
        assert!(names("ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0)").is_empty());
        assert!(names("ALTER TABLE t ADD PRIMARY KEY (id)").is_empty());
        assert!(names("ALTER TABLE t ALTER COLUMN m SET DEFAULT 'a'").is_empty());
        assert!(names("ALTER TABLE t DROP COLUMN m").is_empty());
    }

    #[test]
    fn non_type_positions_are_not_collected() {
        assert!(names("SELECT x.id FROM s.t").is_empty());
        assert!(names("SELECT s.f(1)").is_empty());
        assert!(names("INSERT INTO s.t (a, b) VALUES (1, 2)").is_empty());
        assert!(names("CREATE VIEW v AS SELECT 1").is_empty());
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let sql = format!("SELECT {}1{}::s.mood", "(".repeat(1000), ")".repeat(1000));
        assert_eq!(names(&sql), ["s.mood"]);
    }
}
