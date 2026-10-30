//! `colref_sink`（列参照の位置・役割の記録）のテスト。
//!
//! 1. ガード無し／ガードあり（スコープ空）で構文解析結果が不変であること
//! 2. 代表的な文で記録される位置と役割のスナップショット
//! 3. 網羅性: 受理済みの文の各識別子をリテラルに置き換え、記録漏れ（リテラルが通るのに未記録）
//!    と列専用位置（記録済みでリテラルが通らない）を検出する。変数側（リテラル位置への
//!    スコープ内変数）の受理漏れも逆向きの置換で検出する。

use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::{self, SqlSurfaceError, StructuralOnlyLookup};
use crate::sql::colref_sink::{self, ColrefReport, Role};
use crate::sql::lexer::Token;

/// 文頭で振り分けて既存の検証入口へ渡す（`core::parse_tokens_in` の主要な振り分けの縮約）。
fn parse_tokens(tokens: &[Token]) -> Result<String, SqlSurfaceError> {
    let head = match tokens.first() {
        Some(Token::Ident(w)) => w.to_ascii_uppercase(),
        _ => String::new(),
    };
    let limits = crate::nesting_limits::NestingLimits::default();
    match head.as_str() {
        "INSERT" => {
            allowlist::parse_insert_any_shape_tokens_with_limit(tokens, LedgerMode::default(), None)
                .map(|s| format!("{s:?}"))
        }
        "UPDATE" if allowlist::looks_like_update_from(tokens) => {
            allowlist::validate_update_from_tokens_with_limits(
                tokens,
                LedgerMode::default(),
                false,
                limits,
                &StructuralOnlyLookup,
            )
            .map(|s| format!("{s:?}"))
        }
        "UPDATE" => allowlist::parse_update_form_shape_tokens_with_limits(
            tokens,
            LedgerMode::default(),
            limits,
        )
        .map(|s| format!("{s:?}")),
        "DELETE" if allowlist::looks_like_delete_using(tokens) => {
            allowlist::validate_delete_using_tokens_with_limits(
                tokens,
                LedgerMode::default(),
                false,
                limits,
                &StructuralOnlyLookup,
            )
            .map(|s| format!("{s:?}"))
        }
        "DELETE" => allowlist::parse_delete_statement_shape_tokens_with_limits(
            tokens,
            LedgerMode::default(),
            limits,
        )
        .map(|s| format!("{s:?}")),
        _ => allowlist::validate_sql_tokens_with_subquery_ctx(tokens, &StructuralOnlyLookup, 0)
            .map(|s| format!("{s:?}")),
    }
}

fn lex(sql: &str) -> Vec<Token> {
    crate::sql::relation_name::tokenize_resolved(sql, true)
        .unwrap_or_else(|e| panic!("lex failed: {sql}: {e:?}"))
}

/// ガード無しの結果（構造検証専用の遅延モード下）。
fn plain(tokens: &[Token]) -> Result<String, SqlSurfaceError> {
    let _defer = crate::sql::const_expr::defer_all_non_syntax_errors();
    parse_tokens(tokens)
}

/// ガード下の結果と記録。
fn recorded(tokens: &[Token], scope: &[&str]) -> (Result<String, SqlSurfaceError>, ColrefReport) {
    let _defer = crate::sql::const_expr::defer_all_non_syntax_errors();
    let guard = colref_sink::enter(tokens, scope.iter().map(|s| s.to_string()));
    let r = parse_tokens(tokens);
    (r, guard.finish())
}

/// 受理される文（ガード無しで構文エラーにならない）の集合。網羅性・不変性・スナップショットの共通資料。
const CORPUS: &[&str] = &[
    "SELECT a FROM t",
    "SELECT a, b FROM t WHERE a = 'x'",
    "SELECT a AS z FROM t",
    "SELECT * FROM t WHERE a = 'x' AND b <> 'y'",
    "SELECT * FROM t WHERE a IS NULL",
    "SELECT * FROM t WHERE a IS NOT NULL",
    "SELECT * FROM t WHERE a IN ('x', 'y')",
    "SELECT * FROM t WHERE a NOT IN ('x', 'y')",
    "SELECT * FROM t WHERE n BETWEEN 1 AND 5",
    "SELECT * FROM t WHERE a LIKE 'x%'",
    "SELECT * FROM t WHERE a NOT LIKE 'x%'",
    "SELECT * FROM t WHERE n > 5",
    "SELECT * FROM t WHERE n + 1 > 5",
    "SELECT * FROM t WHERE n > m",
    "SELECT * FROM t WHERE n >= 1 OR m < 2",
    "SELECT * FROM t WHERE NOT n > 3",
    "SELECT * FROM t WHERE n = 5",
    "SELECT * FROM t WHERE flag = true",
    "SELECT * FROM t WHERE (n > 1 AND m > 2) OR k = 3",
    "SELECT n + 1 FROM t",
    "SELECT n + 1 AS x FROM t",
    "SELECT abs(n) FROM t",
    "SELECT coalesce(n, m) FROM t",
    "SELECT CASE WHEN n > 1 THEN a ELSE b END FROM t",
    "SELECT * FROM t ORDER BY n",
    "SELECT * FROM t ORDER BY n DESC, m ASC",
    "SELECT * FROM t ORDER BY 1",
    "SELECT * FROM t LIMIT 5",
    "SELECT * FROM t LIMIT 5 OFFSET 2",
    "SELECT count(*) FROM t",
    "SELECT sum(n) FROM t",
    "SELECT a, sum(n) FROM t GROUP BY a",
    "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1",
    "SELECT a, sum(n) FROM t GROUP BY a ORDER BY a",
    "SELECT a, sum(n + 1) FROM t GROUP BY a",
    "SELECT n / 2, count(*) FROM t GROUP BY n / 2",
    "SELECT DISTINCT a FROM t",
    "SELECT row_number() OVER (ORDER BY n) FROM t",
    "SELECT sum(n) OVER (PARTITION BY a ORDER BY m) FROM t",
    "SELECT * FROM t WHERE a IN (SELECT a FROM u)",
    "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.a = 'x')",
    "SELECT * FROM t WHERE n > (SELECT max(n) FROM u)",
    "SELECT (SELECT max(n) FROM u) FROM t",
    "SELECT a FROM t UNION SELECT a FROM u",
    "WITH c AS (SELECT a FROM t) SELECT a FROM c",
    "SELECT t.a FROM t WHERE t.n > 1",
    "SELECT x.a FROM t AS x WHERE x.n > 1 ORDER BY x.a",
    "SELECT t.a, u.b FROM t JOIN u ON t.k = u.k",
    "SELECT t.a, u.b FROM t LEFT JOIN u ON t.k = u.k AND u.z = 'x'",
    "SELECT t.a, u.b FROM t JOIN u ON t.k = u.k WHERE t.n = 'x'",
    "SELECT t.a, u.b FROM t JOIN u ON t.k = u.k WHERE t.n = 'x' ORDER BY t.a LIMIT 5",
    "SELECT a FROM u",
    "SELECT max(n) FROM u",
    "SELECT 1 FROM u WHERE u.a = 'x'",
    "SELECT * FROM t WHERE a IS DISTINCT FROM b",
    "SELECT * FROM t WHERE a IS NOT DISTINCT FROM 'x'",
    "SELECT * FROM t WHERE lower(a) = 'x'",
    "SELECT * FROM t WHERE lower(a) LIKE 'x%' ESCAPE '!'",
    "SELECT * FROM t WHERE a ~ 'x.*'",
    "SELECT * FROM t WHERE n::text = 'x'",
    "SELECT * FROM t WHERE n = ANY (SELECT n FROM u)",
    "SELECT * FROM t WHERE docb -> 'k' = 'v'",
    "SELECT * FROM t WHERE docb @> 'x'",
    "SELECT * FROM t WHERE n IN (1, 2, 3)",
    "SELECT * FROM t WHERE n NOT BETWEEN 1 AND 5",
    "SELECT * FROM t WHERE ts > TIMESTAMP '2020-01-01 00:00:00'",
    "SELECT * FROM t WHERE visible()",
    "SELECT docb -> 'k' AS x FROM t",
    "SELECT n::text FROM t",
    "SELECT lower(a) AS l, upper(b) FROM t",
    "SELECT (n) FROM t",
    "SELECT DISTINCT a, b FROM t ORDER BY a",
    "SELECT * FROM t ORDER BY lower(a) DESC",
    "SELECT * FROM t ORDER BY a NULLS FIRST LIMIT 3",
    "SELECT * FROM t ORDER BY embedding <=> '[1,2]' LIMIT 5",
    "SELECT * FROM t FETCH FIRST 5 ROWS ONLY",
    "SELECT * FROM t LIMIT ALL",
    "SELECT a, count(*) AS c FROM t GROUP BY a ORDER BY c DESC LIMIT 3",
    "SELECT a, b, sum(n) FROM t GROUP BY a, b HAVING sum(n) > 5",
    "SELECT a, sum(n) FROM t GROUP BY ROLLUP (a)",
    "SELECT sum(n) FILTER (WHERE m > 1) FROM t",
    "SELECT string_agg(a, ',' ORDER BY a) FROM t",
    "SELECT count(DISTINCT a) FROM t",
    "SELECT a, row_number() OVER (PARTITION BY b ORDER BY n DESC) AS rn FROM t",
    "SELECT a, lag(n) OVER (ORDER BY m) FROM t",
    "SELECT a FROM t WHERE n > 1 UNION ALL SELECT a FROM u WHERE n < 5",
    "SELECT a FROM t INTERSECT SELECT a FROM u",
    "SELECT t.a, u.b FROM t JOIN u ON t.k = u.k AND t.z = 'x'",
    "SELECT 1",
    "SELECT n + 1",
    "SELECT 'x' AS a",
    "INSERT INTO t (a, n) VALUES ('x', abs(1))",
    "INSERT INTO t (a, n) VALUES ('x', 1) ON CONFLICT (a) DO NOTHING",
    "INSERT INTO t (a, n) VALUES ('x', 1) ON CONFLICT (a) DO UPDATE SET n = n + EXCLUDED.n",
    "INSERT INTO t (a, n) VALUES ('x', 1) ON CONFLICT (a) DO UPDATE SET n = t.n * 2 WHERE t.n > 1",
    "INSERT INTO t (a, n) SELECT a, n + 1 FROM u WHERE n > 1 ORDER BY n",
    "UPDATE t SET n = n * 2, a = 'z' WHERE n > 1 AND a <> 'q'",
    "UPDATE t SET n = abs(m) WHERE a IN ('x', 'y')",
    "UPDATE t SET n = n + 1 WHERE n BETWEEN 1 AND 5 RETURNING a, n",
    "DELETE FROM t WHERE a IN (SELECT a FROM u)",
    "DELETE FROM t WHERE n > (SELECT max(n) FROM u)",
    "DELETE FROM t WHERE lower(a) = 'x' OR n IS NULL",
    "DELETE FROM t USING u WHERE t.k = u.k AND u.z = 'x'",
    "DELETE FROM t WHERE a = 'x' AND n BETWEEN 1 AND 5 RETURNING a, n",
    "SELECT a, sum(n) AS s FROM t GROUP BY a HAVING sum(n) > 1 ORDER BY s",
    "SELECT * FROM t WHERE n = (SELECT max(n) FROM u)",
    "SELECT * FROM t WHERE (a = 'x' OR b = 'y') AND n > 1",
    "SELECT * FROM t WHERE coalesce(n, 0) > 1",
    "SELECT * FROM t WHERE a ILIKE 'x%'",
    "SELECT * FROM t WHERE a SIMILAR TO 'x%'",
    "SELECT * FROM t WHERE a !~ 'x'",
    "SELECT * FROM t WHERE n > 1 AND NOT m > 2",
    "SELECT t.a FROM t JOIN u USING (k)",
    "INSERT INTO t (a, n) VALUES ('x', 1 + 2 * 3)",
    "UPDATE t SET n = CASE WHEN n > 1 THEN 1 ELSE 2 END WHERE a = 'x'",
    "UPDATE t SET a = lower(b) WHERE n = 1",
    "UPDATE t SET n = 1 WHERE n = 1 AND a ILIKE 'x%'",
    "DELETE FROM t WHERE a = 'x' OR a = 'y'",
    "DELETE FROM t WHERE n = (SELECT max(n) FROM u) RETURNING n",
    "INSERT INTO t (a, n) VALUES ('x', 1)",
    "INSERT INTO t (a, n) VALUES ('x', 1 + 2)",
    "INSERT INTO t (a, n) VALUES ('x', 1), ('y', 2)",
    "INSERT INTO t (a) VALUES ('x') RETURNING a",
    "INSERT INTO t (a, n) VALUES ('x', 1) ON CONFLICT (a) DO UPDATE SET n = n + 1",
    "INSERT INTO t (a, n) VALUES ('x', 1) ON CONFLICT (a) DO UPDATE SET n = EXCLUDED.n",
    "INSERT INTO t (a, n) SELECT a, n FROM u WHERE n > 1",
    "UPDATE t SET n = 5 WHERE a = 'x'",
    "UPDATE t SET n = n + 1 WHERE a = 'x'",
    "UPDATE t SET a = 'y', n = m WHERE n > 3",
    "UPDATE t SET n = 5 WHERE a = 'x' RETURNING n",
    "UPDATE t SET n = u.n FROM u WHERE t.k = u.k",
    "DELETE FROM t WHERE a = 'x'",
    "DELETE FROM t WHERE n > 5 AND m < 3",
    "DELETE FROM t WHERE a = 'x' RETURNING a",
    "DELETE FROM t USING u WHERE t.k = u.k",
];

#[test]
fn corpus_is_accepted_without_guard() {
    let mut bad = Vec::new();
    for sql in CORPUS {
        let toks = lex(sql);
        if let Err(e) = plain(&toks) {
            bad.push(format!("{sql}: {e:?}"));
        }
    }
    assert!(bad.is_empty(), "rejected:\n{}", bad.join("\n"));
}

/// ガード無し／ガードあり（スコープ空）で解析結果が完全一致する（不変条件）。
#[test]
fn guard_does_not_change_parse_result() {
    for sql in CORPUS {
        let toks = lex(sql);
        let a = plain(&toks);
        let (b, _rep) = recorded(&toks, &[]);
        assert_eq!(
            a.as_ref().map_err(|e| e.wire_code()),
            b.as_ref().map_err(|e| e.wire_code()),
            "{sql}"
        );
        if let (Ok(a), Ok(b)) = (&a, &b) {
            assert_eq!(a, b, "{sql}");
        }
    }
}

/// 識別子（または修飾識別子）を `Number("1")` と `StringLiteral("x")` へ置換して受理されるか。
fn literal_accepted(toks: &[Token], i: usize) -> [bool; 2] {
    [
        Token::Number("1".to_string()),
        Token::StringLiteral("x".to_string()),
    ]
    .map(|lit| {
        let mut t = toks.to_vec();
        if let Some(slot) = t.get_mut(i) {
            *slot = lit;
        }
        plain(&t).is_ok()
    })
}

/// 列参照として読まれうる識別子トークンか（真偽・NULL・`ALL` の語はリテラル・構文語として除く）。
fn is_column_candidate(t: &Token) -> bool {
    match t {
        Token::Ident(w) => !matches!(
            w.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "all"
        ),
        Token::QualifiedIdent { .. } => true,
        _ => false,
    }
}

/// 網羅性（記録漏れ）: 識別子をリテラルへ置き換えても受理される位置は、列参照として記録済みでなければ
/// ならない（未記録だと同名の列を黙って読む意味上の誤りになる。取り込み済みサブクエリ範囲の内側は
/// 範囲ごとの再帰分類の対象なのでここでは除く）。あわせて、どの文も範囲外にならないこと。
#[test]
fn every_literal_accepting_identifier_position_is_recorded() {
    let mut leaks = Vec::new();
    for sql in CORPUS {
        let toks = lex(sql);
        let (r, rep) = recorded(&toks, &[]);
        assert!(r.is_ok(), "{sql}");
        assert!(!rep.out_of_range, "out of range: {sql}");
        for (i, t) in toks.iter().enumerate() {
            if !is_column_candidate(t)
                || rep.subquery_ranges.iter().any(|r| r.contains(&i))
                || rep.records.iter().any(|r| r.pos == i)
            {
                continue;
            }
            let acc = literal_accepted(&toks, i);
            if acc[0] || acc[1] {
                leaks.push(format!("{sql}  @{i} {t:?} num={} str={}", acc[0], acc[1]));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "unrecorded column positions:\n{}",
        leaks.join("\n")
    );
}

/// 列専用位置（記録済みなのにリテラルへ置き換えると受理されない位置）の既知一覧。
/// 先頭の 5 件は左辺が列の `IS [NOT] NULL`・`[NOT] IN (list)`・`[NOT] BETWEEN`（左辺が式の形は
/// SQL-24 の拡張〔別コミット〕で受理され、この一覧から消える）。それ以外は列名がそのまま
/// 出力名・構造になる位置（集合演算の枝・DISTINCT・窓の PARTITION BY／ORDER BY・サブクエリ連動の
/// 左辺・RETURNING など）で、変数を置くと置換後に構文エラー（42601）になる。
const COLUMN_ONLY: &[&str] = &[
    "SELECT * FROM t WHERE a IS NULL @5 WhereHead",
    "SELECT * FROM t WHERE a IS NOT NULL @5 WhereHead",
    "SELECT * FROM t WHERE a IN ('x', 'y') @5 WhereHead",
    "SELECT * FROM t WHERE a NOT IN ('x', 'y') @5 WhereHead",
    "SELECT * FROM t WHERE n BETWEEN 1 AND 5 @5 WhereHead",
    "SELECT DISTINCT a FROM t @2 SelectItemBare",
    "SELECT row_number() OVER (ORDER BY n) FROM t @8 Value",
    "SELECT sum(n) OVER (PARTITION BY a ORDER BY m) FROM t @9 Value",
    "SELECT sum(n) OVER (PARTITION BY a ORDER BY m) FROM t @12 Value",
    "SELECT * FROM t WHERE a IN (SELECT a FROM u) @5 WhereHead",
    "SELECT a FROM t UNION SELECT a FROM u @1 SelectItemBare",
    "SELECT a FROM t UNION SELECT a FROM u @6 SelectItemBare",
    "WITH c AS (SELECT a FROM t) SELECT a FROM c @5 SelectItemBare",
    "SELECT * FROM t WHERE n = ANY (SELECT n FROM u) @5 WhereHead",
    "SELECT * FROM t WHERE n IN (1, 2, 3) @5 WhereHead",
    "SELECT * FROM t WHERE n NOT BETWEEN 1 AND 5 @5 WhereHead",
    "SELECT DISTINCT a, b FROM t ORDER BY a @2 SelectItemBare",
    "SELECT DISTINCT a, b FROM t ORDER BY a @4 SelectItemBare",
    "SELECT * FROM t ORDER BY embedding <=> '[1,2]' LIMIT 5 @6 OrderKey",
    "SELECT count(DISTINCT a) FROM t @4 Value",
    "SELECT a, row_number() OVER (PARTITION BY b ORDER BY n DESC) AS rn FROM t @10 Value",
    "SELECT a, row_number() OVER (PARTITION BY b ORDER BY n DESC) AS rn FROM t @13 Value",
    "SELECT a, lag(n) OVER (ORDER BY m) FROM t @11 Value",
    "SELECT a FROM t WHERE n > 1 UNION ALL SELECT a FROM u WHERE n < 5 @1 SelectItemBare",
    "SELECT a FROM t WHERE n > 1 UNION ALL SELECT a FROM u WHERE n < 5 @11 SelectItemBare",
    "SELECT a FROM t INTERSECT SELECT a FROM u @1 SelectItemBare",
    "SELECT a FROM t INTERSECT SELECT a FROM u @6 SelectItemBare",
    "UPDATE t SET n = abs(m) WHERE a IN ('x', 'y') @10 WhereHead",
    "UPDATE t SET n = n + 1 WHERE n BETWEEN 1 AND 5 RETURNING a, n @9 WhereHead",
    "UPDATE t SET n = n + 1 WHERE n BETWEEN 1 AND 5 RETURNING a, n @15 SelectItemBare",
    "UPDATE t SET n = n + 1 WHERE n BETWEEN 1 AND 5 RETURNING a, n @17 SelectItemBare",
    "DELETE FROM t WHERE a IN (SELECT a FROM u) @4 WhereHead",
    "DELETE FROM t WHERE n > (SELECT max(n) FROM u) @4 WhereHead",
    "DELETE FROM t WHERE lower(a) = 'x' OR n IS NULL @11 WhereHead",
    "DELETE FROM t WHERE a = 'x' AND n BETWEEN 1 AND 5 RETURNING a, n @8 WhereHead",
    "DELETE FROM t WHERE a = 'x' AND n BETWEEN 1 AND 5 RETURNING a, n @14 SelectItemBare",
    "DELETE FROM t WHERE a = 'x' AND n BETWEEN 1 AND 5 RETURNING a, n @16 SelectItemBare",
    "DELETE FROM t WHERE n = (SELECT max(n) FROM u) RETURNING n @4 WhereHead",
    "DELETE FROM t WHERE n = (SELECT max(n) FROM u) RETURNING n @16 SelectItemBare",
    "INSERT INTO t (a) VALUES ('x') RETURNING a @11 SelectItemBare",
    "UPDATE t SET n = 5 WHERE a = 'x' RETURNING n @11 SelectItemBare",
    "DELETE FROM t WHERE a = 'x' RETURNING a @8 SelectItemBare",
];

#[test]
fn column_only_positions_match_known_list() {
    let mut found = Vec::new();
    for sql in CORPUS {
        let toks = lex(sql);
        let (r, rep) = recorded(&toks, &[]);
        assert!(r.is_ok(), "{sql}");
        for rec in &rep.records {
            if rep.subquery_ranges.iter().any(|r| r.contains(&rec.pos))
                || !matches!(toks.get(rec.pos), Some(Token::Ident(_)))
            {
                continue;
            }
            let acc = literal_accepted(&toks, rec.pos);
            if !acc[0] && !acc[1] {
                found.push(format!("{sql} @{} {:?}", rec.pos, rec.role));
            }
        }
    }
    let expected: Vec<String> = COLUMN_ONLY.iter().map(|s| s.to_string()).collect();
    assert_eq!(found, expected);
}

/// 記録されるのは識別子系トークンの位置だけである（`Number`・`StringLiteral`・記号を指さない）。
#[test]
fn records_point_at_identifier_tokens_only() {
    for sql in CORPUS {
        let toks = lex(sql);
        let (_r, rep) = recorded(&toks, &[]);
        for rec in &rep.records {
            let t = toks.get(rec.pos);
            assert!(
                matches!(
                    t,
                    Some(
                        Token::Ident(_)
                            | Token::QuotedBoolWord(_)
                            | Token::QuotedNull(_)
                            | Token::QualifiedIdent { .. }
                    )
                ),
                "{sql} @{} {t:?}",
                rec.pos
            );
        }
    }
}

/// 変数側の網羅性: リテラルをスコープ内変数（識別子）へ置き換えて受理されるなら、その位置は
/// 記録されていなければならない（受理されたのに未記録だと変数が置換されず黙って列として読まれる）。
/// 受理されない位置は既知一覧（`VARIABLE_REJECTED`）と一致すること。
const VARIABLE_REJECTED: &[&str] = &[
    r#"SELECT * FROM t WHERE lower(a) LIKE 'x%' ESCAPE '!' @12 StringLiteral("!") 42601"#,
    r#"SELECT * FROM t WHERE ts > TIMESTAMP '2020-01-01 00:00:00' @8 StringLiteral("2020-01-01 00:00:00") 42601"#,
    r#"SELECT * FROM t WHERE a SIMILAR TO 'x%' @8 StringLiteral("x%") 42601"#,
];

#[test]
fn variable_in_literal_position_is_recorded_or_rejected() {
    let mut unrecorded = Vec::new();
    let mut rejected = Vec::new();
    for sql in CORPUS {
        let toks = lex(sql);
        for (i, t) in toks.iter().enumerate() {
            if !matches!(t, Token::Number(_) | Token::StringLiteral(_)) {
                continue;
            }
            let mut t2 = toks.clone();
            if let Some(slot) = t2.get_mut(i) {
                *slot = Token::Ident("v".to_string());
            }
            let (r, rep) = recorded(&t2, &["v"]);
            if rep.subquery_ranges.iter().any(|r| r.contains(&i)) {
                continue;
            }
            match r {
                Ok(_) => {
                    if !rep.records.iter().any(|r| r.pos == i) {
                        unrecorded.push(format!("{sql} @{i} {t:?}"));
                    }
                    if rep.out_of_range {
                        unrecorded.push(format!("OUT_OF_RANGE {sql} @{i} {t:?}"));
                    }
                }
                Err(e) => rejected.push(format!("{sql} @{i} {t:?} {}", e.wire_code())),
            }
        }
    }
    assert!(unrecorded.is_empty(), "{}", unrecorded.join("\n"));
    let expected: Vec<String> = VARIABLE_REJECTED.iter().map(|s| s.to_string()).collect();
    assert_eq!(rejected, expected);
}

fn roles(sql: &str, scope: &[&str]) -> (Vec<(usize, Role)>, ColrefReport) {
    let toks = lex(sql);
    let (r, rep) = recorded(&toks, scope);
    assert!(r.is_ok(), "{sql}: {r:?}");
    assert!(!rep.out_of_range, "{sql}");
    (rep.records.iter().map(|r| (r.pos, r.role)).collect(), rep)
}

/// 役割別のスナップショット。
#[test]
fn snapshot_roles_per_statement() {
    use Role::{GroupKey, OrderKey, SelectItemBare, Value, WhereHead};
    // 投影の裸識別子・WHERE の構造葉の先頭・ORDER BY キー。
    assert_eq!(
        roles("SELECT a, b FROM t WHERE a = 'x' ORDER BY b", &[]).0,
        vec![
            (1, SelectItemBare),
            (3, SelectItemBare),
            (7, WhereHead),
            (12, OrderKey)
        ]
    );
    // 式・関数引数・式述語は Value（別名付き投影の式、括弧付き裸識別子は列項目へ戻るので裸の役割）。
    assert_eq!(
        roles("SELECT n + 1 AS x, abs(m), (k) FROM t WHERE n + 1 > m", &[]).0,
        vec![
            (1, Value),
            (9, Value),
            (13, SelectItemBare),
            (18, Value),
            (22, Value)
        ]
    );
    // 集計: 集計引数は Value、GROUP BY キーは GroupKey、投影の群キー列は裸の役割。
    assert_eq!(
        roles("SELECT a, sum(n) FROM t GROUP BY a ORDER BY a", &[]).0,
        vec![
            (1, SelectItemBare),
            (5, Value),
            (11, GroupKey),
            (14, OrderKey)
        ]
    );
}

/// リテラル専用位置（LIKE パターン・IN 要素・BETWEEN 境界・LIMIT/OFFSET・距離演算の右辺）に置かれた
/// スコープ内変数は `LiteralSlot` として受理される。ガードが無い（スコープ外の名前）ときは従来どおり
/// 構文エラー。
#[test]
fn scope_variable_in_literal_only_positions_is_literal_slot() {
    use Role::LiteralSlot;
    for sql in [
        "SELECT * FROM t WHERE a LIKE v",
        "SELECT * FROM t WHERE b IN ('x', v)",
        "SELECT * FROM t WHERE c BETWEEN v AND v",
        "SELECT * FROM t ORDER BY embedding <=> v LIMIT v",
        "SELECT * FROM t LIMIT v OFFSET v",
    ] {
        let toks = lex(sql);
        let (r, rep) = recorded(&toks, &["v"]);
        assert!(r.is_ok(), "{sql}: {r:?}");
        assert!(!rep.out_of_range, "{sql}");
        let slots: Vec<usize> = rep
            .records
            .iter()
            .filter(|r| r.role == LiteralSlot)
            .map(|r| r.pos)
            .collect();
        let v_positions: Vec<usize> = toks
            .iter()
            .enumerate()
            .filter(|(_, t)| matches!(t, Token::Ident(w) if w == "v"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(slots, v_positions, "{sql}");
        // スコープ外の名前、およびガード無しでは受理しない（fail-closed。既存の挙動）。
        let (r, _) = recorded(&toks, &["w"]);
        assert_eq!(r.err().map(|e| e.wire_code()), Some("42601"), "{sql}");
        assert_eq!(
            plain(&toks).err().map(|e| e.wire_code()),
            Some("42601"),
            "{sql}"
        );
    }
}

/// 修飾子がスコープ内レコード変数の `rec.field` は、表の公開名に一致しないときだけ変数参照として
/// 残り、`Value` で記録される（表の公開名が常に優先。ガード無しでは従来どおり `42P01`）。
#[test]
fn record_qualifier_is_kept_as_value_reference() {
    let sql = "SELECT rec.f FROM t WHERE t.n > rec.g";
    let toks = lex(sql);
    let (r, rep) = recorded(&toks, &["rec"]);
    assert!(r.is_ok(), "{r:?}");
    assert!(!rep.out_of_range);
    let rec_positions: Vec<usize> = toks
        .iter()
        .enumerate()
        .filter(|(_, t)| matches!(t, Token::QualifiedIdent { qualifier, .. } if qualifier == "rec"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(rec_positions.len(), 2);
    for p in rec_positions {
        assert!(
            rep.records
                .iter()
                .any(|r| r.pos == p && matches!(r.role, Role::Value | Role::SelectItemBare)),
            "rec.* @{p} not recorded: {:?}",
            rep.records
        );
    }
    assert_eq!(plain(&toks).err().map(|e| e.wire_code()), Some("42P01"));
    // WHERE 葉の先頭・UPDATE/DELETE の WHERE・INSERT の値・ORDER BY・関数引数の中でも同じ。
    for sql in [
        "SELECT a FROM t WHERE rec.g > 1",
        "SELECT a FROM t WHERE rec.g = 'x'",
        "SELECT a FROM t WHERE n > 1 AND rec.g = 'x'",
        "SELECT a FROM t ORDER BY rec.f",
        "SELECT a FROM t WHERE lower(rec.g) = 'x'",
        "UPDATE t SET n = rec.n WHERE rec.g = 1",
        "DELETE FROM t WHERE rec.g = 1",
        "INSERT INTO t (a, n) VALUES (rec.a, rec.n)",
    ] {
        let toks = lex(sql);
        let (r, rep) = recorded(&toks, &["rec"]);
        assert!(r.is_ok(), "{sql}: {r:?}");
        assert!(!rep.out_of_range, "{sql}");
        for (i, t) in toks.iter().enumerate() {
            if matches!(t, Token::QualifiedIdent { qualifier, .. } if qualifier == "rec") {
                assert!(rep.records.iter().any(|r| r.pos == i), "{sql} @{i}");
            }
        }
        assert!(plain(&toks).is_err(), "{sql}");
    }
    // 表の公開名と同名のレコード変数は表が優先（`t.n` は列参照のまま）。
    let toks = lex("SELECT t.a FROM t WHERE t.n > 1");
    let (r, rep) = recorded(&toks, &["t"]);
    assert!(r.is_ok(), "{r:?}");
    assert!(!rep.out_of_range);
}

/// `Parser::new` が引用付き `NULL` を降格した複製で解析しても、記録は元のトークン列上の位置になる
/// （1 対 1 の別名登録）。
#[test]
fn quoted_null_demotion_copy_is_translated_to_original_positions() {
    let toks = lex(r#"SELECT "null" FROM t WHERE n > "null""#);
    assert!(toks.iter().any(|t| matches!(t, Token::QuotedNull(_))));
    let (r, rep) = recorded(&toks, &[]);
    assert!(r.is_ok(), "{r:?}");
    assert!(!rep.out_of_range);
    let quoted: Vec<usize> = toks
        .iter()
        .enumerate()
        .filter(|(_, t)| matches!(t, Token::QuotedNull(_)))
        .map(|(i, _)| i)
        .collect();
    for p in quoted {
        assert!(
            rep.records.iter().any(|r| r.pos == p),
            "@{p}: {:?}",
            rep.records
        );
    }
}

/// ルート（ガードを張った入口のトークン列）の外の複製で列参照が確定したら範囲外フラグが立つ
/// （記録漏れを黙って許さない fail-closed）。
#[test]
fn identifier_outside_root_sets_out_of_range() {
    let toks = lex("SELECT a FROM t WHERE n > 1");
    let other = lex("SELECT a FROM t WHERE n > 1");
    let guard = colref_sink::enter(&other, Vec::<String>::new());
    let _ = parse_tokens(&toks);
    let rep = guard.finish();
    assert!(rep.out_of_range);
    // 同じ内容でも、ルートの外のアドレスは記録されない。
    assert!(rep.records.is_empty());
}

/// 取り込まれたサブクエリ本体の範囲はルート上の位置で登録される。
#[test]
fn subquery_bodies_are_registered_as_root_ranges() {
    let sql = "SELECT * FROM t WHERE a IN (SELECT a FROM u) AND EXISTS (SELECT 1 FROM u)";
    let toks = lex(sql);
    let (r, rep) = recorded(&toks, &[]);
    assert!(r.is_ok(), "{r:?}");
    assert!(!rep.out_of_range);
    assert_eq!(rep.subquery_ranges.len(), 2);
    for range in &rep.subquery_ranges {
        assert!(matches!(
            toks.get(range.start),
            Some(Token::Keyword(crate::sql::lexer::Keyword::Select))
        ));
        assert!(matches!(toks.get(range.end), Some(Token::Punct(')'))));
    }
}

/// ガードのネスト・Drop で前の状態へ戻り、ガード無しではスコープ照会が常に偽になる。
#[test]
fn guard_restores_previous_state_on_drop() {
    assert!(!colref_sink::is_active());
    assert!(!colref_sink::is_scope_var("v"));
    let toks = lex("SELECT a FROM t");
    {
        let outer = colref_sink::enter(&toks, ["v"]);
        assert!(colref_sink::is_scope_var("v"));
        {
            let inner = colref_sink::enter(&toks, ["w"]);
            assert!(colref_sink::is_scope_var("w"));
            assert!(!colref_sink::is_scope_var("v"));
            drop(inner);
        }
        assert!(colref_sink::is_scope_var("v"));
        drop(outer);
    }
    assert!(!colref_sink::is_active());
    assert!(!colref_sink::is_scope_var("v"));
}
