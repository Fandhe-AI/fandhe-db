//! `plpgsql/subst.rs` の単体テスト（Issue #2226・SQL-68。ADR D3・D6）。
//!
//! 値から組み立てたトークン列が、同じリテラルを字句解析器で読んだ結果と一致すること（値が SQL
//! テキストを経由しないことの根拠）、役割別の置換、`$n` の過不足、置換後サイズの上限を固定する。

use super::ast::{VarId, VarRef, VarRole, VarSite};
use super::host::{PlScalar, VarLookup};
use super::subst::{scalar_tokens, substitute, substitute_params};
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
fn value_tokens_equal_the_lexer_output_for_the_same_literal() {
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
        let got = scalar_tokens(&v, VarRole::Value).expect("tokens");
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
    let out = substitute(&tokens, &sites, &env, true).expect("substitute");
    assert_eq!(out, lex("SELECT a FROM t WHERE n = -3 AND m > 'x'"));
}

#[test]
fn bare_select_item_gets_an_alias_and_order_key_is_wrapped() {
    let tokens = lex("SELECT v FROM t ORDER BY w");
    let sites = vec![
        site(1, VarRole::SelectItemBare, 0),
        site(6, VarRole::OrderKey, 1),
    ];
    let env = Env(vec![int(7), int(2)]);
    let out = substitute(&tokens, &sites, &env, true).expect("substitute");
    assert_eq!(out, lex("SELECT 7 AS v FROM t ORDER BY CAST(2 AS integer)"));
    // 別名付与を止めると（`WHERE` 形の式）素の値になる。
    let out = substitute(&tokens, &sites[..1], &env, false).expect("no alias");
    assert_eq!(out, lex("SELECT 7 FROM t ORDER BY w"));
}

#[test]
fn text_select_item_is_cast_so_that_the_projection_accepts_it() {
    let tokens = lex("SELECT v FROM t");
    let sites = vec![site(1, VarRole::SelectItemBare, 0)];
    let env = Env(vec![text("a")]);
    let out = substitute(&tokens, &sites, &env, true).expect("substitute");
    assert_eq!(out, lex("SELECT CAST('a' AS text) AS v FROM t"));
}

#[test]
fn negative_values_before_a_type_cast_are_parenthesized() {
    let tokens = lex("SELECT v::int");
    let sites = vec![site(1, VarRole::Value, 0)];
    let env = Env(vec![int(-5)]);
    let out = substitute(&tokens, &sites, &env, false).expect("substitute");
    assert_eq!(out, lex("SELECT (-5)::int"));
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
    let e = substitute(&tokens, &sites, &Env(vec![big]), false).expect_err("too big");
    assert_eq!(e.wire_code(), "54000");
}

#[test]
fn params_must_match_using_values_exactly() {
    let tokens = lex("SELECT $1, $2");
    let ok = substitute_params(&tokens, &[int(1), text("a")]).expect("match");
    assert_eq!(ok, lex("SELECT 1, 'a'"));
    for using in [vec![int(1)], vec![int(1), int(2), int(3)], vec![]] {
        let e = substitute_params(&tokens, &using).expect_err("mismatch");
        assert_eq!(e.wire_code(), "42601");
    }
    // `$n` を含まない文に USING があれば過剰。
    let e = substitute_params(&lex("SELECT 1"), &[int(1)]).expect_err("excess");
    assert_eq!(e.wire_code(), "42601");
    assert!(substitute_params(&lex("SELECT 1"), &[]).is_ok());
}

#[test]
fn param_values_cannot_inject_sql() {
    let tokens = lex("SELECT $1");
    let evil = text("1; DROP TABLE t");
    let out = substitute_params(&tokens, &[evil]).expect("substitute");
    assert_eq!(
        out,
        vec![
            Token::Keyword(lexer::Keyword::Select),
            Token::StringLiteral("1; DROP TABLE t".to_string()),
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
        let out = substitute(&tokens, &sites, &Env(vec![big.clone()]), false).expect("big");
        assert_eq!(
            out,
            lex("SELECT 1 FROM t ORDER BY CAST(2147483648 AS bigint)")
        );
        let out = substitute(&tokens, &sites, &Env(vec![small.clone()]), false).expect("small");
        assert_eq!(out, lex("SELECT 1 FROM t ORDER BY CAST(3 AS smallint)"));
    }
}
