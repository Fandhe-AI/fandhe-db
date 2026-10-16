//! Parse で宣言された型（`DeclaredParamType`）を `$n` のスロット型と値の検証に用いる
//! 契約（Issue #2436）の結合テスト。
//!
//! ポインタ: WIRE-24・WIRE-12・SQL-45 (6)。wire-server が型 OID から写した宣言型を
//! `parse_sql_prepared_with_declared` へ渡す経路の engine 側を、実 `Storage` 越しに固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::params::{DeclaredParamType as D, PreparedParamType};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::with_visibilities("sys", [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE docs (n INTEGER, d DOUBLE PRECISION, flag BOOLEAN, t TEXT)",
    )
    .expect("create table");
    (core, CleanupGuard(path))
}

fn some(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

fn bind_code(
    core: &EngineCore,
    sql: &str,
    declared: &[Option<D>],
    values: &[&str],
) -> Result<(), String> {
    let prepared = core
        .parse_sql_prepared_with_declared(sql, declared)
        .map_err(|e| e.wire_code().to_string())?;
    let binds: Vec<Option<Vec<u8>>> = values.iter().map(|v| some(v)).collect();
    core.bind_prepared(&prepared, &binds)
        .map(|_| ())
        .map_err(|e: SqlSurfaceError| e.wire_code().to_string())
}

#[test]
fn indeterminate_expression_uses_declared_type() {
    let (core, _g) = setup("decl-expr");
    let sql = "UPDATE docs SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(sql, &[Some(D::Int4), Some(D::Int4)])
        .expect("declared types resolve the expression");
    assert_eq!(
        prepared.param_types(),
        &[
            PreparedParamType::Declared(D::Int4),
            PreparedParamType::Declared(D::Int4)
        ]
    );
    let bound = core
        .bind_prepared(&prepared, &[some("2"), some("3")])
        .expect("bind");
    let literal = core
        .parse_sql("UPDATE docs SET n = 2 + 3 WHERE id = 1 USING OPERATION_ID 'op-1'")
        .expect("literal form");
    assert_eq!(bound, literal);
}

#[test]
fn partially_declared_expression_keeps_42p18() {
    let (core, _g) = setup("decl-partial");
    let sql = "UPDATE docs SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int4), None], &["1", "2"]),
        Err("42P18".to_string())
    );
    assert_eq!(
        bind_code(&core, sql, &[], &["1", "2"]),
        Err("42P18".to_string())
    );
}

#[test]
fn unsupported_declared_type_inside_expression_is_0a000() {
    let (core, _g) = setup("decl-expr-unsupported");
    let sql = "UPDATE docs SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    for ty in [D::Bool, D::Date, D::Timestamp, D::Numeric, D::Uuid] {
        assert_eq!(
            bind_code(&core, sql, &[Some(ty), Some(ty)], &["1", "2"]),
            Err("0A000".to_string()),
            "{ty:?}"
        );
    }
}

#[test]
fn unreferenced_number_takes_declared_type() {
    let (core, _g) = setup("decl-unref");
    let prepared = core
        .parse_sql_prepared_with_declared(
            "SELECT id FROM docs WHERE n = $2 ORDER BY id LIMIT 5",
            &[Some(D::Date), None],
        )
        .expect("parse");
    assert_eq!(
        prepared.param_types().first(),
        Some(&PreparedParamType::Declared(D::Date))
    );
}

#[test]
fn integer_declared_type_validates_range_and_syntax() {
    let (core, _g) = setup("decl-int");
    let sql = "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5";
    let d = [Some(D::Int2)];
    assert_eq!(bind_code(&core, sql, &d, &["40000"]), Err("22003".into()));
    assert_eq!(bind_code(&core, sql, &d, &["-32769"]), Err("22003".into()));
    assert_eq!(bind_code(&core, sql, &d, &["x"]), Err("22P02".into()));
    assert_eq!(bind_code(&core, sql, &d, &["7"]), Ok(()));
    assert_eq!(bind_code(&core, sql, &d, &["-32768"]), Ok(()));
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int4)], &["2147483648"]),
        Err("22003".into())
    );
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int8)], &["9223372036854775808"]),
        Err("22003".into())
    );
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int8)], &["9223372036854775807"]),
        Ok(())
    );
    // 巨大な桁数でも panic せず範囲外になる。
    let huge = "9".repeat(200);
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int8)], &[huge.as_str()]),
        Err("22003".into())
    );
}

#[test]
fn float_declared_on_double_column_rejects_fraction_for_int4() {
    let (core, _g) = setup("decl-int-on-double");
    let sql = "SELECT id FROM docs WHERE d = $1 ORDER BY id LIMIT 5";
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int4)], &["1.5"]),
        Err("22P02".into())
    );
    assert_eq!(bind_code(&core, sql, &[Some(D::Int4)], &["2"]), Ok(()));
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Float4)], &["1e40"]),
        Err("22003".into())
    );
    assert_eq!(bind_code(&core, sql, &[Some(D::Float8)], &["1.5"]), Ok(()));
}

#[test]
fn bool_declared_type_uses_pg_bool_grammar() {
    let (core, _g) = setup("decl-bool");
    let sql = "SELECT id FROM docs WHERE flag = $1 ORDER BY id LIMIT 5";
    assert_eq!(bind_code(&core, sql, &[Some(D::Bool)], &["yes"]), Ok(()));
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Bool)], &["maybe"]),
        Err("22P02".into())
    );
}

#[test]
fn error_message_does_not_contain_value() {
    let (core, _g) = setup("decl-msg");
    let prepared = core
        .parse_sql_prepared_with_declared(
            "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5",
            &[Some(D::Int2)],
        )
        .expect("parse");
    let err = core
        .bind_prepared(&prepared, &[some("987654")])
        .expect_err("out of range");
    let text = format!("{err}");
    assert!(!text.contains("987654"), "{text}");
    assert!(text.contains("$1"), "{text}");
}

#[test]
fn no_declaration_is_identical_to_legacy_parse() {
    let (core, _g) = setup("decl-regression");
    let sql = "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5";
    let legacy = core.parse_sql_prepared(sql).expect("legacy");
    let none = core
        .parse_sql_prepared_with_declared(sql, &[None])
        .expect("none");
    assert_eq!(legacy.param_types(), none.param_types());
    assert!(legacy.declared_param_types().iter().all(Option::is_none));
    let a = core.bind_prepared(&legacy, &[some("5")]).expect("a");
    let b = core.bind_prepared(&none, &[some("5")]).expect("b");
    assert_eq!(a, b);
}

#[test]
fn surplus_declarations_are_ignored_by_engine() {
    let (core, _g) = setup("decl-surplus");
    let prepared = core
        .parse_sql_prepared_with_declared(
            "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5",
            &[Some(D::Int4), Some(D::Int4), Some(D::Int4)],
        )
        .expect("surplus is wire's concern");
    assert_eq!(prepared.declared_param_types().len(), 1);
}

#[test]
fn inferred_slot_type_is_not_overridden_by_declaration() {
    let (core, _g) = setup("decl-keep-inferred");
    let prepared = core
        .parse_sql_prepared_with_declared(
            "SELECT id FROM docs WHERE n = $1 ORDER BY id LIMIT 5",
            &[Some(D::Varchar)],
        )
        .expect("parse");
    assert!(matches!(
        prepared.param_types().first(),
        Some(PreparedParamType::Column(_))
    ));
    assert_eq!(prepared.declared_param_types(), &[Some(D::Varchar)]);
}

/// 未参照番号（空文字列で残る位置）に宣言型付きで実値を Bind しても、生の Bind 値で
/// 検証されるため `22P02` にならない（PR #2470 指摘）。
#[test]
fn unreferenced_number_with_declared_numeric_types_binds_real_value() {
    let (core, _g) = setup("decl-unref-bind");
    let sql = "SELECT id FROM docs WHERE n = $2 ORDER BY id LIMIT 5";
    let cases: [(D, &str); 6] = [
        (D::Int2, "1"),
        (D::Int4, "1"),
        (D::Int8, "1"),
        (D::Float4, "1.5"),
        (D::Float8, "1.5"),
        (D::Bool, "true"),
    ];
    for (ty, v) in cases {
        assert_eq!(
            bind_code(&core, sql, &[Some(ty), Some(D::Int4)], &[v, "2"]),
            Ok(()),
            "{ty:?}"
        );
    }
    // 未参照でも実値は宣言型で検証される（不正値は 22P02）。
    assert_eq!(
        bind_code(&core, sql, &[Some(D::Int4), Some(D::Int4)], &["x", "2"]),
        Err("22P02".to_string())
    );
}

/// float4 宣言は f32 へ丸めた値を置換する。text の "16777217" はバイナリ形式（f32 復号）
/// の 16777216 と同じ結果になる。
#[test]
fn float4_declared_value_is_rounded_like_binary_form() {
    let (core, _g) = setup("decl-f4-round");
    let sql = "UPDATE docs SET d = $1 + $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(sql, &[Some(D::Float4), Some(D::Float4)])
        .expect("parse");
    let bound = core
        .bind_prepared(&prepared, &[some("16777217"), some("0")])
        .expect("bind");
    let rounded = core
        .bind_prepared(&prepared, &[some("16777216"), some("0")])
        .expect("bind rounded");
    assert_eq!(bound, rounded);
}

/// 置換後総バイト数の上限検査は float4 の正規化後の値で行う。生の Bind 値のままなら
/// 合計 2MB 超で `54000` になる構成でも、正規値（`0.0`）なら上限内で受理される。
#[test]
fn float4_size_limit_is_checked_on_canonical_text() {
    let (core, _g) = setup("decl-f4-size");
    let sql = "UPDATE docs SET d = $1 + $1 + $1 + $1 + $1 + $1 + $1 + $1 + $1 + $1 \
               WHERE id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(sql, &[Some(D::Float4)])
        .expect("parse");
    let long = format!("0.{}1", "0".repeat(200_000));
    core.bind_prepared(&prepared, &[some(&long)])
        .expect("canonical text is short");
}

/// float4 の正規テキストは小数点または指数を保ち、再解析で整数リテラルへ型が変わらない
/// （`$1 / 2` が整数除算にならない）。`1.0`・`1e-40` を含む。
#[test]
fn float4_canonical_text_keeps_float_literal_type() {
    let (core, _g) = setup("decl-f4-float-literal");
    let sql = "UPDATE docs SET d = $1 + $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(sql, &[Some(D::Float4), Some(D::Float4)])
        .expect("parse");
    // 期待値は f32 へ丸めた値を f64 へ拡張した最短往復表記（`1e-40` の f32 値は 9.99994610111476e-41）。
    for (text, literal) in [
        ("1.0", "1.0"),
        ("1", "1.0"),
        ("1e-40", "9.99994610111476e-41"),
    ] {
        let bound = core
            .bind_prepared(&prepared, &[some(text), some("0")])
            .expect("bind");
        let expected = core
            .parse_sql(&format!(
                "UPDATE docs SET d = {literal} + 0.0 WHERE id = 1 USING OPERATION_ID 'op-1'"
            ))
            .expect("literal parse");
        assert_eq!(bound, expected, "{text}");
    }
}

/// float4 宣言で `0.1` を渡すと、DOUBLE 列スロットへは f32 を f64 へ拡張した値
/// （0.10000000149011612）が置換される（f32 の最短表記 `0.1` のままにしない）。
#[test]
fn float4_value_for_double_slot_is_widened_f32() {
    let (core, _g) = setup("decl-f4-widen");
    let sql = "UPDATE docs SET d = $1 WHERE id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(sql, &[Some(D::Float4)])
        .expect("parse");
    let bound = core.bind_prepared(&prepared, &[some("0.1")]).expect("bind");
    let expected = core
        .parse_sql("UPDATE docs SET d = 0.10000000149011612 WHERE id = 1 USING OPERATION_ID 'op-1'")
        .expect("literal parse");
    assert_eq!(bound, expected);
}

/// float4 宣言でも、推論済みの INTEGER・TEXT・OPERATION_ID スロットでは値を据え置く
/// （`'2'` が `'2.0'` に変わって 22P02 になる・TEXT へ `2.0` が保存される退行を防ぐ）。
#[test]
fn float4_declared_value_is_kept_for_non_float_slots() {
    let (core, _g) = setup("decl-f4-keep");
    let int_sql = "UPDATE docs SET d = 1.5 WHERE n = $1 AND id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(int_sql, &[Some(D::Float4)])
        .expect("parse");
    let bound = core
        .bind_prepared(&prepared, &[some("2")])
        .expect("bind int");
    let expected = core
        .parse_sql("UPDATE docs SET d = 1.5 WHERE n = 2 AND id = 1 USING OPERATION_ID 'op-1'")
        .expect("literal parse");
    assert_eq!(bound, expected);

    let text_sql = "UPDATE docs SET t = $1 WHERE id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(text_sql, &[Some(D::Float4)])
        .expect("parse");
    let bound = core
        .bind_prepared(&prepared, &[some("2")])
        .expect("bind text");
    let expected = core
        .parse_sql("UPDATE docs SET t = '2' WHERE id = 1 USING OPERATION_ID 'op-1'")
        .expect("literal parse");
    assert_eq!(bound, expected);
}
