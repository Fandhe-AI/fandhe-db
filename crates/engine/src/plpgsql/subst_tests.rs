//! `plpgsql/subst.rs` の単体テスト（Issue #2226・SQL-68。ADR D3・D6）。
//!
//! 値から組み立てたトークン列が、同じリテラルを字句解析器で読んだ結果と一致すること（値が SQL
//! テキストを経由しないことの根拠）、役割別の置換、`$n` の過不足、置換後サイズの上限を固定する。

use super::ast::{VarId, VarRef, VarRole, VarSite};
use super::host::{PlScalar, VarLookup};
use super::subst::{scalar_tokens, scalar_tokens_styled, substitute, substitute_params, NumStyle};
use crate::catalog::ColumnType;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::lexer::{self, Token};

struct Env(Vec<PlScalar>);

impl VarLookup for Env {
    fn scalar(&self, r: &VarRef) -> Result<&PlScalar, SqlSurfaceError> {
        match r {
            VarRef::Var(id) => self.0.get(id.0 as usize).ok_or(SqlSurfaceError::Internal {
                detail: "missing".to_string(),
            }),
            VarRef::Field(..) => Err(SqlSurfaceError::Internal {
                detail: "field".to_string(),
            }),
        }
    }
}

fn int(i: i64) -> PlScalar {
    PlScalar::new(Cell::SignedInteger(i), ColumnType::Integer)
}

fn text(s: &str) -> PlScalar {
    PlScalar::new(Cell::Text(s.to_string()), ColumnType::Text)
}

fn lex(sql: &str) -> Vec<Token> {
    lexer::tokenize_with_params(sql).expect("lex")
}

fn site(pos: usize, role: VarRole, id: u32) -> VarSite {
    VarSite {
        pos,
        role,
        var: VarRef::Var(VarId(id)),
    }
}

#[test]
fn bare_value_tokens_equal_the_lexer_output_for_the_same_literal() {
    let cases: Vec<(PlScalar, &str)> = vec![
        (int(5), "5"),
        (int(0), "0"),
        (int(i64::MAX), "9223372036854775807"),
        (int(-5), "-5"),
        (int(i64::MIN), "-9223372036854775808"),
        (text("abc"), "'abc'"),
        (text("it's"), "'it''s'"),
        (text("'; DROP TABLE t; --"), "'''; DROP TABLE t; --'"),
        (PlScalar::new(Cell::Bool(true), ColumnType::Boolean), "true"),
        (
            PlScalar::new(Cell::Bool(false), ColumnType::Boolean),
            "false",
        ),
        (PlScalar::new(Cell::Float(1.5), ColumnType::Double), "1.5"),
        (PlScalar::new(Cell::Float(-2.0), ColumnType::Double), "-2.0"),
    ];
    for (v, lit) in cases {
        let got = scalar_tokens_styled(&v, VarRole::Value, NumStyle::Bare, false).expect("tokens");
        assert_eq!(got, lex(lit), "{lit}");
    }
}

#[test]
fn non_finite_floats_and_unsupported_cells_are_rejected() {
    for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let e = scalar_tokens(
            &PlScalar::new(Cell::Float(f), ColumnType::Double),
            VarRole::Value,
        )
        .expect_err("non finite");
        assert_eq!(e.wire_code(), "0A000");
    }
    let e = scalar_tokens(
        &PlScalar::new(Cell::Bytes(vec![1]), ColumnType::Bytea),
        VarRole::Value,
    )
    .expect_err("bytes");
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn null_is_typed_in_value_positions_and_bare_elsewhere() {
    let n = PlScalar::null(ColumnType::Integer);
    assert_eq!(
        scalar_tokens(&n, VarRole::Value).expect("value"),
        lex("CAST(NULL AS integer)")
    );
    assert_eq!(
        scalar_tokens(&n, VarRole::LiteralSlot).expect("slot"),
        lex("NULL")
    );
    assert_eq!(
        scalar_tokens(&n, VarRole::WhereHead).expect("head"),
        lex("NULL")
    );
}

#[test]
fn substitution_replaces_sites_in_place_and_keeps_other_tokens() {
    // SELECT a FROM t WHERE n = v AND m > w
    let tokens = lex("SELECT a FROM t WHERE n = v AND m > w");
    let sites = vec![site(7, VarRole::Value, 0), site(11, VarRole::Value, 1)];
    let env = Env(vec![int(-3), text("x")]);
    let out = substitute(&tokens, &sites, &env, true, NumStyle::Typed).expect("substitute");
    assert_eq!(
        out,
        lex("SELECT a FROM t WHERE n = CAST(-3 AS integer) AND m > 'x'")
    );
}

#[test]
fn bare_select_item_gets_an_alias_and_order_key_is_wrapped() {
    let tokens = lex("SELECT v FROM t ORDER BY w");
    let sites = vec![
        site(1, VarRole::SelectItemBare, 0),
        site(6, VarRole::OrderKey, 1),
    ];
    let env = Env(vec![int(7), int(2)]);
    let out = substitute(&tokens, &sites, &env, true, NumStyle::Typed).expect("substitute");
    assert_eq!(
        out,
        lex("SELECT CAST(7 AS integer) AS v FROM t ORDER BY CAST(2 AS integer)")
    );
    // 別名付与を止めると（`WHERE` 形の式）素の値になる。
    let out = substitute(&tokens, &sites[..1], &env, false, NumStyle::Typed).expect("no alias");
    assert_eq!(out, lex("SELECT CAST(7 AS integer) FROM t ORDER BY w"));
}

#[test]
fn text_select_item_is_cast_so_that_the_projection_accepts_it() {
    let tokens = lex("SELECT v FROM t");
    let sites = vec![site(1, VarRole::SelectItemBare, 0)];
    let env = Env(vec![text("a")]);
    let out = substitute(&tokens, &sites, &env, true, NumStyle::Typed).expect("substitute");
    assert_eq!(out, lex("SELECT CAST('a' AS text) AS v FROM t"));
}

#[test]
fn negative_values_before_a_type_cast_are_parenthesized() {
    let tokens = lex("SELECT v::int");
    let sites = vec![site(1, VarRole::Value, 0)];
    let env = Env(vec![int(-5)]);
    let out = substitute(&tokens, &sites, &env, false, NumStyle::Bare).expect("substitute");
    assert_eq!(out, lex("SELECT (-5)::int"));
    // 型付きの置換は CAST が負値を包むため、`::` に先に結合しない。
    let out = substitute(&tokens, &sites, &env, false, NumStyle::Typed).expect("typed");
    assert_eq!(out, lex("SELECT CAST(-5 AS integer)::int"));
}

#[test]
fn negative_typed_const_values_after_minus_keep_the_sign_semantics() {
    // ConstExpr の負の整数は 3 トークン（`-` + `$typed:<型>` + 数値）。単項の `-` の直後は二重否定を
    // 畳み、二項の `-` の直後は括弧で包む（どちらも `--` を作らない）。
    let env = Env(vec![typed(ColumnType::BigInt, Cell::SignedInteger(-5))]);
    let neg = [vec![Token::Punct('-')], tv("int8", "5")].concat();
    let unary = lex("VALUES (-v)");
    let out = substitute(
        &unary,
        &[site(3, VarRole::ConstExpr, 0)],
        &env,
        false,
        NumStyle::Typed,
    )
    .expect("unary");
    assert_eq!(out, [lex("VALUES ("), tv("int8", "5"), lex(")")].concat());
    let binary = lex("VALUES (1 - v)");
    let out = substitute(
        &binary,
        &[site(4, VarRole::ConstExpr, 0)],
        &env,
        false,
        NumStyle::Typed,
    )
    .expect("binary");
    let want = [lex("VALUES (1 - ("), neg, lex(")"), lex(")")].concat();
    assert_eq!(out, want);
}

#[test]
fn substituted_size_is_bounded() {
    // 同じ変数を多数参照して巨大な値を複製しても、置換後の総バイト数で拒否する。
    let big = text(&"x".repeat(200_000));
    let n = 8;
    let mut tokens = vec![Token::Ident("SELECT".to_string())];
    let mut sites = Vec::new();
    for i in 0..n {
        if i > 0 {
            tokens.push(Token::Punct(','));
        }
        sites.push(site(tokens.len(), VarRole::Value, 0));
        tokens.push(Token::Ident("v".to_string()));
    }
    let e =
        substitute(&tokens, &sites, &Env(vec![big]), false, NumStyle::Typed).expect_err("too big");
    assert_eq!(e.wire_code(), "54000");
}

#[test]
fn params_must_match_using_values_exactly() {
    let tokens = lex("SELECT $1, $2");
    let ok = substitute_params(&tokens, &[int(1), text("a")], NumStyle::Typed).expect("match");
    assert_eq!(ok, lex("SELECT CAST(1 AS integer), CAST('a' AS text)"));
    for using in [vec![int(1)], vec![int(1), int(2), int(3)], vec![]] {
        let e = substitute_params(&tokens, &using, NumStyle::Typed).expect_err("mismatch");
        assert_eq!(e.wire_code(), "42601");
    }
    // `$n` を含まない文に USING があれば過剰。
    let e = substitute_params(&lex("SELECT 1"), &[int(1)], NumStyle::Typed).expect_err("excess");
    assert_eq!(e.wire_code(), "42601");
    assert!(substitute_params(&lex("SELECT 1"), &[], NumStyle::Typed).is_ok());
}

#[test]
fn param_values_cannot_inject_sql() {
    let tokens = lex("SELECT $1");
    let evil = text("1; DROP TABLE t");
    let out = substitute_params(&tokens, &[evil], NumStyle::Typed).expect("substitute");
    assert_eq!(
        out,
        vec![
            Token::Keyword(lexer::Keyword::Select),
            Token::Ident("CAST".to_string()),
            Token::Punct('('),
            Token::StringLiteral("1; DROP TABLE t".to_string()),
            Token::Ident("AS".to_string()),
            Token::Ident("text".to_string()),
            Token::Punct(')'),
        ]
    );
}

#[test]
fn order_and_group_keys_keep_the_integer_type_of_the_variable() {
    let big = PlScalar::new(Cell::SignedInteger(2_147_483_648), ColumnType::BigInt);
    let small = PlScalar::new(Cell::SignedInteger(3), ColumnType::SmallInt);
    for role in [VarRole::OrderKey, VarRole::GroupKey] {
        let tokens = lex("SELECT 1 FROM t ORDER BY w");
        let sites = vec![site(6, role, 0)];
        let out = substitute(
            &tokens,
            &sites,
            &Env(vec![big.clone()]),
            false,
            NumStyle::Typed,
        )
        .expect("big");
        assert_eq!(
            out,
            lex("SELECT 1 FROM t ORDER BY CAST(2147483648 AS bigint)")
        );
        let out = substitute(
            &tokens,
            &sites,
            &Env(vec![small.clone()]),
            false,
            NumStyle::Typed,
        )
        .expect("small");
        assert_eq!(out, lex("SELECT 1 FROM t ORDER BY CAST(3 AS smallint)"));
    }
}

#[test]
fn dynamic_params_get_role_based_protection() {
    // 投影の裸の文字列・真偽値は CAST で包む（静的 SQL の SelectItemBare と同じ）。
    let out = substitute_params(&lex("SELECT $1"), &[text("abc")], NumStyle::Typed).expect("proj");
    assert_eq!(out, lex("SELECT CAST('abc' AS text)"));
    let b = PlScalar::new(Cell::Bool(true), ColumnType::Boolean);
    let out = substitute_params(&lex("SELECT 1, $1 FROM t"), &[b], NumStyle::Typed).expect("bool");
    assert_eq!(out, lex("SELECT 1, CAST('true' AS boolean) FROM t"));
    // ORDER BY／GROUP BY の整数は序数参照にならない。
    let out = substitute_params(
        &lex("SELECT a FROM t ORDER BY $1"),
        &[int(1)],
        NumStyle::Typed,
    )
    .expect("order");
    assert_eq!(out, lex("SELECT a FROM t ORDER BY CAST(1 AS integer)"));
    let out = substitute_params(
        &lex("SELECT a FROM t GROUP BY a, $1"),
        &[int(2)],
        NumStyle::Typed,
    )
    .expect("group");
    assert_eq!(out, lex("SELECT a FROM t GROUP BY a, CAST(2 AS integer)"));
    // WHERE 内の値は従来どおり。
    let out = substitute_params(
        &lex("SELECT a FROM t WHERE a = $1"),
        &[int(1)],
        NumStyle::Typed,
    )
    .expect("where");
    assert_eq!(out, lex("SELECT a FROM t WHERE a = CAST(1 AS integer)"));
}

fn typed(ty: ColumnType, cell: Cell) -> PlScalar {
    PlScalar::new(cell, ty)
}

#[test]
fn numbers_keep_their_declared_type_in_value_positions() {
    // 宣言型を保つ（裸の整数にすると SQL 側が値から型を推定し直す）。境界値・小数・smallint を固定する。
    let cases: Vec<(PlScalar, &str)> = vec![
        (
            typed(ColumnType::BigInt, Cell::SignedInteger(2_147_483_647)),
            "CAST(2147483647 AS bigint)",
        ),
        (
            typed(ColumnType::BigInt, Cell::SignedInteger(i64::MIN)),
            "CAST(-9223372036854775808 AS bigint)",
        ),
        (
            typed(ColumnType::SmallInt, Cell::SignedInteger(7)),
            "CAST(7 AS smallint)",
        ),
        (
            typed(ColumnType::Double, Cell::Float(1.5)),
            "CAST(1.5 AS double precision)",
        ),
        (
            typed(ColumnType::Real, Cell::Float(2.0)),
            "CAST(2.0 AS real)",
        ),
        (
            typed(ColumnType::Double, Cell::Float(1e300)),
            "CAST(1e300 AS double precision)",
        ),
    ];
    for role in [
        VarRole::Value,
        VarRole::SelectItemBare,
        VarRole::OrderKey,
        VarRole::GroupKey,
    ] {
        for (v, want) in &cases {
            assert_eq!(scalar_tokens(v, role).expect("typed"), lex(want), "{want}");
        }
    }
}

#[test]
fn literal_slots_take_a_bare_literal_and_refuse_untyped_forms() {
    // LIMIT／IN／BETWEEN は CAST を受理せず、値の型が結果へ伝わらないため裸のリテラル。
    let big = typed(ColumnType::BigInt, Cell::SignedInteger(3_000_000_000));
    assert_eq!(
        scalar_tokens(&big, VarRole::LiteralSlot).expect("slot"),
        lex("3000000000")
    );
    let d = typed(ColumnType::Double, Cell::Float(1.5));
    assert_eq!(
        scalar_tokens(&d, VarRole::LiteralSlot).expect("slot"),
        lex("1.5")
    );
    // 指数表記・scale 0 の numeric も裸のリテラル（リテラル専用位置は値で比較される）。
    let e = typed(ColumnType::Double, Cell::Float(1e300));
    assert_eq!(
        scalar_tokens(&e, VarRole::LiteralSlot).expect("exponent"),
        lex("1e300")
    );
    let n0 = typed(
        ColumnType::Numeric {
            precision: 5,
            scale: 0,
        },
        Cell::Numeric(crate::numeric::Decimal::from_parts(3, 0).expect("decimal")),
    );
    assert_eq!(
        scalar_tokens(&n0, VarRole::LiteralSlot).expect("numeric"),
        lex("3")
    );
    // NaN は拒否する。
    let e = scalar_tokens(
        &typed(ColumnType::Double, Cell::Float(f64::NAN)),
        VarRole::LiteralSlot,
    )
    .expect_err("nan");
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn null_order_and_group_keys_are_typed_never_a_column_name() {
    let n = PlScalar::null(ColumnType::Integer);
    for role in [VarRole::OrderKey, VarRole::GroupKey] {
        assert_eq!(
            scalar_tokens(&n, role).expect("typed null"),
            lex("CAST(NULL AS integer)")
        );
        // 型語の無い型は裸の NULL（列名）へ倒さず拒否する。
        let e = scalar_tokens(&PlScalar::null(ColumnType::Bytea), role).expect_err("untyped");
        assert_eq!(e.wire_code(), "0A000");
    }
}

/// 非 NULL のとき型付き形（`CAST`／型付きリテラル）で置換する型（日付・時刻・数値・真偽値・文字列）は、
/// NULL でも列名として読まれない型付き形（`CAST(NULL AS 型)`）になる（`time` の漏れの回帰）。
#[test]
fn null_of_every_substitutable_type_is_typed_in_column_like_positions() {
    let cases: [(ColumnType, &str); 5] = [
        (ColumnType::Time, "time"),
        (ColumnType::Date, "date"),
        (ColumnType::Timestamp, "timestamp"),
        (ColumnType::Boolean, "boolean"),
        (ColumnType::Text, "text"),
    ];
    for (ty, word) in cases {
        let n = PlScalar::null(ty);
        for role in [
            VarRole::Value,
            VarRole::SelectItemBare,
            VarRole::OrderKey,
            VarRole::GroupKey,
        ] {
            assert_eq!(
                scalar_tokens(&n, role).expect("typed null"),
                lex(&format!("CAST(NULL AS {word})")),
                "{word} {role:?}"
            );
        }
    }
}

#[test]
fn dynamic_param_roles_survive_and_inside_case_and_literal_slots() {
    // CASE 内の AND で ORDER BY の役割判定が止まらない（後続キーの NULL が型付きになる）。
    let n = PlScalar::null(ColumnType::Integer);
    let sql = "SELECT a FROM t ORDER BY CASE WHEN a > 1 AND a < 5 THEN 1 ELSE 2 END, $1";
    let out =
        substitute_params(&lex(sql), std::slice::from_ref(&n), NumStyle::Typed).expect("order");
    assert_eq!(
        out,
        lex("SELECT a FROM t ORDER BY CASE WHEN a > 1 AND a < 5 THEN 1 ELSE 2 END, CAST(NULL AS integer)")
    );
    // リテラル専用位置（LIMIT・IN リスト・BETWEEN の上側境界）は裸のリテラル。
    let v = typed(ColumnType::BigInt, Cell::SignedInteger(3));
    for (sql, want) in [
        ("SELECT a FROM t LIMIT $1", "SELECT a FROM t LIMIT 3"),
        (
            "SELECT a FROM t WHERE a IN ($1, $1)",
            "SELECT a FROM t WHERE a IN (3, 3)",
        ),
        (
            "SELECT a FROM t WHERE a BETWEEN $1 AND $1",
            "SELECT a FROM t WHERE a BETWEEN 3 AND 3",
        ),
    ] {
        let out =
            substitute_params(&lex(sql), std::slice::from_ref(&v), NumStyle::Typed).expect(sql);
        assert_eq!(out, lex(want), "{sql}");
    }
    // BETWEEN の外側の AND の後ろは値の位置（型付き）。
    let out = substitute_params(
        &lex("SELECT a FROM t WHERE a BETWEEN 1 AND 2 AND a < $1"),
        std::slice::from_ref(&v),
        NumStyle::Typed,
    )
    .expect("and");
    assert_eq!(
        out,
        lex("SELECT a FROM t WHERE a BETWEEN 1 AND 2 AND a < CAST(3 AS bigint)")
    );
}

/// 内部専用の型付き値（合成識別子 + 数値）のトークン列。
fn tv(kind: &str, raw: &str) -> Vec<Token> {
    vec![
        Token::Ident(format!("$typed:{kind}")),
        Token::Number(raw.to_string()),
    ]
}

#[test]
fn const_expr_elements_use_internal_typed_values() {
    // 整数・浮動小数は内部専用の型付き値トークン（宣言型を畳み込みへ持ち込む）。
    let cases: Vec<(PlScalar, Vec<Token>)> = vec![
        (
            typed(ColumnType::BigInt, Cell::SignedInteger(2_147_483_647)),
            tv("int8", "2147483647"),
        ),
        (
            typed(ColumnType::BigInt, Cell::SignedInteger(-5)),
            [vec![Token::Punct('-')], tv("int8", "5")].concat(),
        ),
        (
            typed(ColumnType::SmallInt, Cell::SignedInteger(7)),
            tv("int2", "7"),
        ),
        (
            typed(ColumnType::Integer, Cell::SignedInteger(7)),
            tv("int4", "7"),
        ),
        (
            typed(ColumnType::Double, Cell::Float(1.5)),
            tv("float8", "1.5"),
        ),
        (
            typed(ColumnType::Real, Cell::Float(2.0)),
            tv("float4", "2.0"),
        ),
    ];
    for (v, want) in cases {
        assert_eq!(scalar_tokens(&v, VarRole::ConstExpr).expect("typed"), want);
    }
    // 日付は型付きリテラル、NULL は裸（列型が決める）。
    let d = typed(ColumnType::Date, Cell::Date(0));
    assert_eq!(
        scalar_tokens(&d, VarRole::ConstExpr).expect("date"),
        lex("DATE '1970-01-01'")
    );
    assert_eq!(
        scalar_tokens(&PlScalar::null(ColumnType::Integer), VarRole::ConstExpr).expect("null"),
        lex("NULL")
    );
}

#[test]
fn const_expr_numeric_is_bare_only_as_a_lone_element() {
    let n0 = typed(
        ColumnType::Numeric {
            precision: 5,
            scale: 0,
        },
        Cell::Numeric(crate::numeric::Decimal::from_parts(3, 0).expect("decimal")),
    );
    let env = Env(vec![n0]);
    // 単独（符号付きを含む）は裸の数値。
    for (sql, pos, want) in [
        ("VALUES (v)", 2, "VALUES (3)"),
        ("VALUES (1, v)", 4, "VALUES (1, 3)"),
        ("VALUES (-v)", 3, "VALUES (-3)"),
    ] {
        let sites = vec![site(pos, VarRole::ConstExpr, 0)];
        let out = substitute(&lex(sql), &sites, &env, false, NumStyle::Typed).expect(sql);
        assert_eq!(out, lex(want), "{sql}");
    }
    // 式の中・関数の引数は 0A000（numeric の算術は定数式で扱えない）。
    for (sql, pos) in [
        ("VALUES (v / 2)", 2),
        ("VALUES (abs(v))", 4),
        ("VALUES (1 + v)", 4),
    ] {
        let sites = vec![site(pos, VarRole::ConstExpr, 0)];
        let e = substitute(&lex(sql), &sites, &env, false, NumStyle::Typed).expect_err(sql);
        assert_eq!(e.wire_code(), "0A000", "{sql}");
    }
}

#[test]
fn internal_typed_value_cannot_be_written_by_users() {
    // 利用者の SQL からは作れない（`$` で始まる識別子は字句化できない）。
    assert!(lexer::tokenize("SELECT $typed:int8 1").is_err());
    assert!(lexer::tokenize_with_params("SELECT $typed:int8 1").is_err());
}

#[test]
fn execute_values_role_survives_and_inside_case() {
    let v = typed(ColumnType::BigInt, Cell::SignedInteger(5));
    let sql = "INSERT INTO t (a) VALUES (CASE WHEN 1 > 0 AND 2 > 1 THEN $1 END)";
    let out = substitute_params(&lex(sql), std::slice::from_ref(&v), NumStyle::Typed).expect(sql);
    let mut want = lex("INSERT INTO t (a) VALUES (CASE WHEN 1 > 0 AND 2 > 1 THEN 5 END)");
    // `5` を内部専用の型付き値トークンへ差し替えたものと一致する。
    let pos = want
        .iter()
        .position(|t| matches!(t, Token::Number(n) if n == "5"))
        .expect("five");
    want.splice(pos..=pos, tv("int8", "5"));
    assert_eq!(out, want);
}
