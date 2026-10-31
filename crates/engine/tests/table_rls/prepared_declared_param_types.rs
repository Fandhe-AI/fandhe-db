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

use crate::temp_db;
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
            &[Some(D::Int8)],
        )
        .expect("parse");
    assert!(matches!(
        prepared.param_types().first(),
        Some(PreparedParamType::Column(_))
    ));
    assert_eq!(prepared.declared_param_types(), &[Some(D::Int8)]);
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

/// float8 宣言の整数値 `1`・`2` は浮動小数リテラル（`1.0`・`2.0`）で置換され、`$1 / $2` が
/// 整数除算にならない。NaN／Infinity は float4 と同じく `22P02`、f64 範囲外は `22003`。
#[test]
fn float8_canonical_text_keeps_float_literal_type() {
    let (core, _g) = setup("decl-f8-float-literal");
    let sql = "UPDATE docs SET d = $1 / $2 WHERE id = 1 USING OPERATION_ID 'op-1'";
    let prepared = core
        .parse_sql_prepared_with_declared(sql, &[Some(D::Float8), Some(D::Float8)])
        .expect("parse");
    let bound = core
        .bind_prepared(&prepared, &[some("1"), some("2")])
        .expect("bind");
    let expected = core
        .parse_sql("UPDATE docs SET d = 1.0 / 2.0 WHERE id = 1 USING OPERATION_ID 'op-1'")
        .expect("literal parse");
    assert_eq!(bound, expected);
    // 整数リテラルのままの `1 / 2` とは別の構文木になる。
    let integer_form = core
        .parse_sql("UPDATE docs SET d = 1 / 2 WHERE id = 1 USING OPERATION_ID 'op-1'")
        .expect("literal parse");
    assert_ne!(bound, integer_form);

    for (v, code) in [("NaN", "22P02"), ("Infinity", "22P02"), ("1e999", "22003")] {
        assert_eq!(
            bind_code(&core, sql, &[Some(D::Float8), Some(D::Float8)], &[v, "2"]),
            Err(code.to_string()),
            "{v}"
        );
    }
}
// ---- Issue #2797: `INSERT ... VALUES` の式内 `$n` を宣言型どおりに評価する ----
// ポインタ: WIRE-12・SQL-24（併せて WIRE-24・SQL-45 (6)）。

fn setup_typed(label: &str) -> (EngineCore, CleanupGuard, PolicyContext, SessionState) {
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
        "CREATE TABLE tv (id BIGINT PRIMARY KEY, b BIGINT, d DOUBLE PRECISION, r REAL)",
    )
    .expect("create table");
    (core, CleanupGuard(path), ctx, session)
}

/// 宣言型つきで Parse → Bind → Execute し、結果コード（成功は `Ok`）を返す。
fn exec_typed(
    core: &EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    sql: &str,
    declared: &[Option<D>],
    values: &[&str],
) -> Result<(), String> {
    let prepared = core
        .parse_sql_prepared_with_declared(sql, declared)
        .map_err(|e| e.wire_code().to_string())?;
    let binds: Vec<Option<Vec<u8>>> = values.iter().map(|v| some(v)).collect();
    let parsed = core
        .bind_prepared(&prepared, &binds)
        .map_err(|e| e.wire_code().to_string())?;
    core.execute_parsed_in_session(ctx, session, &parsed)
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string())
}

fn cell_text(core: &EngineCore, ctx: &PolicyContext, s: &mut SessionState, q: &str) -> String {
    match core.execute_sql_in_session(ctx, s, q).expect("select") {
        engine::sql::SqlOutcome::Query(r) => format!(
            "{:?}",
            r.rows.into_iter().map(|row| row.cells).collect::<Vec<_>>()
        ),
        _ => panic!("expected Query outcome"),
    }
}

#[test]
fn bigint_declared_expression_is_evaluated_as_bigint() {
    let (core, _g, ctx, mut s) = setup_typed("typed-i8");
    let sql = "INSERT INTO tv (id, b) VALUES (1, $1 + 1) USING OPERATION_ID 'op-1'";
    // int4 の上限 + 1 は bigint の算術として成功する（従来は int4 の算術で `22003`）。
    exec_typed(&core, &ctx, &mut s, sql, &[Some(D::Int8)], &["2147483647"]).expect("bigint");
    let out = cell_text(&core, &ctx, &mut s, "SELECT b FROM tv LIMIT 10");
    assert!(out.contains("2147483648"), "{out}");
}

#[test]
fn smallint_declared_expression_overflows_as_smallint() {
    let (core, _g, ctx, mut s) = setup_typed("typed-i2");
    let sql = "INSERT INTO tv (id, b) VALUES (1, $1 + $2) USING OPERATION_ID 'op-1'";
    let both = [Some(D::Int2), Some(D::Int2)];
    // smallint 同士の算術は smallint の範囲で `22003` になる。
    assert_eq!(
        exec_typed(&core, &ctx, &mut s, sql, &both, &["32767", "1"]),
        Err("22003".to_string())
    );
    exec_typed(&core, &ctx, &mut s, sql, &both, &["32766", "1"]).expect("in range");
    // smallint + integer リテラルは integer の算術になり成功する（PostgreSQL と同じ）。
    let mixed = "INSERT INTO tv (id, b) VALUES (2, $1 + 1) USING OPERATION_ID 'op-2'";
    exec_typed(&core, &ctx, &mut s, mixed, &[Some(D::Int2)], &["32767"]).expect("int4 arithmetic");
}

#[test]
fn float8_declared_expression_is_evaluated_as_float() {
    let (core, _g, ctx, mut s) = setup_typed("typed-f8");
    let div = "INSERT INTO tv (id, d) VALUES (1, $1 / 2) USING OPERATION_ID 'op-1'";
    exec_typed(&core, &ctx, &mut s, div, &[Some(D::Float8)], &["5"]).expect("float division");
    let out = cell_text(&core, &ctx, &mut s, "SELECT d FROM tv LIMIT 10");
    assert!(out.contains("2.5"), "{out}");
    let add = "INSERT INTO tv (id, d) VALUES (2, $1 + 1) USING OPERATION_ID 'op-2'";
    exec_typed(&core, &ctx, &mut s, add, &[Some(D::Float8)], &["1.5"]).expect("fraction");
    let out = cell_text(
        &core,
        &ctx,
        &mut s,
        "SELECT d FROM tv WHERE id = 2 LIMIT 10",
    );
    assert!(out.contains("2.5"), "{out}");
}

#[test]
fn negative_values_in_typed_expressions() {
    let (core, _g, ctx, mut s) = setup_typed("typed-neg");
    let unary = "INSERT INTO tv (id, b) VALUES (1, -$1) USING OPERATION_ID 'op-1'";
    exec_typed(&core, &ctx, &mut s, unary, &[Some(D::Int8)], &["-5"]).expect("unary minus");
    let binary = "INSERT INTO tv (id, b) VALUES (2, 1 - $1) USING OPERATION_ID 'op-2'";
    exec_typed(&core, &ctx, &mut s, binary, &[Some(D::Int8)], &["-5"]).expect("binary minus");
    let plain = "INSERT INTO tv (id, b) VALUES (3, $1 + 0) USING OPERATION_ID 'op-3'";
    exec_typed(&core, &ctx, &mut s, plain, &[Some(D::Int8)], &["-5"]).expect("negative");
    let out = cell_text(
        &core,
        &ctx,
        &mut s,
        "SELECT id, b FROM tv ORDER BY id LIMIT 10",
    );
    assert!(
        out.contains('5') && out.contains('6') && out.contains("-5"),
        "{out}"
    );
}

#[test]
fn typed_substitution_keeps_fail_closed_shapes() {
    let (core, _g, ctx, mut s) = setup_typed("typed-closed");
    // `$n::T` は従来どおり定数式で拒否される。
    let cast = "INSERT INTO tv (id, b) VALUES (1, $1::bigint + 1) USING OPERATION_ID 'op-1'";
    assert_eq!(
        exec_typed(&core, &ctx, &mut s, cast, &[Some(D::Int8)], &["1"]),
        Err("0A000".to_string())
    );
    // 宣言 int4 の範囲外は宣言型の検証で `22003`。
    let sql = "INSERT INTO tv (id, b) VALUES (2, $1 + 1) USING OPERATION_ID 'op-2'";
    assert_eq!(
        exec_typed(&core, &ctx, &mut s, sql, &[Some(D::Int4)], &["3000000000"]),
        Err("22003".to_string())
    );
    // Parse 段の値依存エラー（0 除算）は Bind まで遅延され、Bind で `22012` になる。
    let div = "INSERT INTO tv (id, b) VALUES (3, 10 / $1) USING OPERATION_ID 'op-3'";
    assert_eq!(
        exec_typed(&core, &ctx, &mut s, div, &[Some(D::Int8)], &["0"]),
        Err("22012".to_string())
    );
    exec_typed(&core, &ctx, &mut s, div, &[Some(D::Int8)], &["5"]).expect("division ok");
}
