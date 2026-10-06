//! 比較述語に残っていた文字列リテラル形の結合テスト（Issue #1431。親 #1408・#1420。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-24・
//! `docs/spec/04-behavior/errors.md` ERR-2・ERR-6）。
//!
//! (a) `<>`／`!=` × 文字列リテラル（`NOT col = 'x'` と同じ結果）、(b) 疑似列 `id` ×
//! 文字列リテラル（数値リテラル形と同じ結果）、(c) BOOLEAN 列 × 文字列の範囲比較・`IN`・
//! `BETWEEN`（PostgreSQL の順序 `false < true`）を、実 `Storage`＋`CpuScalarProvider` の
//! production 経路（`EngineCore::execute_sql`）で固定する。解釈できない文字列は `22P02`。
//! テナント境界（他テナント行が結果に現れないこと、エラーがリテラル本文を反射しないこと）も確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("big", ColumnType::BigInt, true),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new("day", ColumnType::Date, true),
            ColumnDef::new("ext_id", ColumnType::Uuid, true),
            ColumnDef::new("blob", ColumnType::Bytea, true),
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql24-literal-kind-coercion");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed, got {e:?}"));
}

/// 行を 1 件挿入する。`extra` は `(列名, SQL リテラル)` の組（省略した nullable 列は NULL）。
fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, extra: &[(&str, &str)], op: &str) {
    let mut columns = vec!["id".to_string(), "embedding".into(), "lang".into()];
    let mut values = vec![id.to_string(), "'[0.1,0.2]'".into(), "'ja'".into()];
    for (c, v) in extra {
        columns.push((*c).to_string());
        values.push((*v).to_string());
    }
    exec(
        core,
        ctx,
        &format!(
            "INSERT INTO {TABLE} ({}) VALUES ({}) USING OPERATION_ID '{op}'",
            columns.join(", "),
            values.join(", ")
        ),
    );
}

fn seed(core: &EngineCore, alice: &PolicyContext, bob: &PolicyContext) {
    insert(
        core,
        alice,
        1,
        &[
            ("qty", "1"),
            ("big", "1"),
            ("r", "1.5"),
            ("d", "1.5"),
            ("flag", "true"),
        ],
        "op-1",
    );
    insert(
        core,
        alice,
        2,
        &[
            ("qty", "2"),
            ("big", "2"),
            ("r", "2.5"),
            ("d", "2.5"),
            ("flag", "false"),
        ],
        "op-2",
    );
    // 数値・真偽値列がすべて NULL の行（三値論理による fail-open の検出用）。
    insert(core, alice, 3, &[], "op-3");
    insert(
        core,
        alice,
        4,
        &[("qty", "10"), ("big", "10"), ("r", "10.5"), ("d", "10.5")],
        "op-4",
    );
    // 他テナントの行（alice の結果・件数に現れてはならない）。
    insert(
        core,
        bob,
        9,
        &[("qty", "1"), ("big", "1"), ("flag", "true")],
        "op-9",
    );
}

fn ids(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("query {where_clause:?} should succeed, got {e:?}"));
    let mut out: Vec<u64> = result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => panic!("expected Cell::Integer for id, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn code(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> String {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
    )
    .expect_err(&format!("query {where_clause:?} must be rejected"))
    .wire_code()
    .to_string()
}

#[test]
fn not_equal_forms_match_negated_equality_for_every_column_type() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for (col, lit) in [
        ("lang", "ja"),
        ("qty", "2"),
        ("big", "2"),
        ("r", "2.5"),
        ("d", "2.5"),
        ("flag", "t"),
        ("id", "2"),
    ] {
        let oracle = ids(&core, &alice, &format!("NOT {col} = '{lit}'"));
        assert_eq!(
            ids(&core, &alice, &format!("{col} <> '{lit}'")),
            oracle,
            "{col} <>"
        );
        assert_eq!(
            ids(&core, &alice, &format!("{col} != '{lit}'")),
            oracle,
            "{col} !="
        );
    }
    // NULL 行（id 3）は結果に含まれない。
    assert_eq!(ids(&core, &alice, "qty <> '1'"), vec![2, 4]);
    assert_eq!(ids(&core, &alice, "flag <> 't'"), vec![2]);
    assert_eq!(ids(&core, &alice, "qty != '1' AND qty <> '2'"), vec![4]);
    assert_eq!(
        ids(&core, &alice, "qty <> '1' OR qty <> '2'"),
        vec![1, 2, 4]
    );
    assert_eq!(ids(&core, &alice, "NOT (qty <> '1')"), vec![1]);
    // エラーコードは `=` 形と一致する。
    assert_eq!(code(&core, &alice, "qty <> '1.5'"), "22P02");
    assert_eq!(code(&core, &alice, "qty != '3000000000'"), "22003");
    assert_eq!(code(&core, &alice, "flag <> 'x'"), "22P02");
    assert_eq!(code(&core, &alice, "qty = '1.5'"), "22P02");
}

#[test]
fn bang_equal_alone_is_still_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    assert_eq!(code(&core, &alice, "qty ! 1"), "42601");
}

#[test]
fn pseudo_id_column_matches_numeric_literal_form() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    let pairs = [
        ("id = '2'", "id = 2"),
        ("id > '2'", "id > 2"),
        ("id <= '3'", "id <= 3"),
        ("id IN ('1','3')", "id IN (1, 3)"),
        ("id NOT IN ('1','3')", "id NOT IN (1, 3)"),
        ("id BETWEEN '2' AND '4'", "id BETWEEN 2 AND 4"),
        ("id NOT BETWEEN '2' AND '3'", "id NOT BETWEEN 2 AND 3"),
        ("NOT id = '2'", "NOT id = 2"),
        ("id <> '2'", "NOT id = 2"),
    ];
    for (quoted, numeric) in pairs {
        assert_eq!(
            ids(&core, &alice, quoted),
            ids(&core, &alice, numeric),
            "{quoted}"
        );
    }
    assert_eq!(ids(&core, &alice, "id IN ('1','3')"), vec![1, 3]);
    // 他テナントの id=9 は現れない。
    assert!(ids(&core, &alice, "id > '0'").iter().all(|i| *i != 9));
    assert_eq!(code(&core, &alice, "id = 'abc'"), "22P02");
    assert_eq!(
        code(&core, &alice, "id = '9007199254740993'"),
        code(&core, &alice, "id = 9007199254740993"),
    );
}

#[test]
fn boolean_column_string_range_in_between_follow_false_lt_true() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    // flag: id1=true, id2=false, id3/id4=NULL（NULL は常に除外）。
    let cases: [(&str, Vec<u64>); 20] = [
        ("flag > 'f'", vec![1]),
        ("flag >= 'f'", vec![1, 2]),
        ("flag < 't'", vec![2]),
        ("flag <= 't'", vec![1, 2]),
        ("flag > 't'", vec![]),
        ("flag < 'f'", vec![]),
        ("flag > 'off'", vec![1]),
        ("flag < '1'", vec![2]),
        ("NOT flag > 'f'", vec![2]),
        ("NOT flag < 'f'", vec![1, 2]),
        ("flag IN ('t','no')", vec![1, 2]),
        ("flag IN ('t')", vec![1]),
        ("flag NOT IN ('t')", vec![2]),
        ("flag NOT IN ('t','f')", vec![]),
        ("flag BETWEEN 'f' AND 't'", vec![1, 2]),
        ("flag BETWEEN 't' AND 'f'", vec![]),
        ("flag NOT BETWEEN 't' AND 'f'", vec![1, 2]),
        ("flag NOT BETWEEN 'f' AND 'f'", vec![1]),
        ("flag BETWEEN 't' AND 't'", vec![1]),
        ("flag > 'f' OR flag < 'f'", vec![1]),
    ];
    for (clause, expected) in cases {
        assert_eq!(ids(&core, &alice, clause), expected, "{clause}");
    }
    for bad in [
        "flag > 'x'",
        "flag IN ('t','x')",
        "flag BETWEEN 'f' AND 'x'",
        "NOT flag < 'x'",
    ] {
        assert_eq!(code(&core, &alice, bad), "22P02", "{bad}");
    }
    // 数値リテラルとの比較は従来どおり演算子なし。
    assert_eq!(code(&core, &alice, "flag > 1"), "42883");
    // エラーはリテラル本文を反射しない。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE flag > 'secret-literal' LIMIT 1"),
        )
        .unwrap_err();
    assert!(!format!("{err:?}").contains("secret-literal"));
}

#[test]
fn not_equal_in_update_delete_matches_negated_equality() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    exec(
        &core,
        &alice,
        "DELETE FROM docs WHERE qty <> '1' USING OPERATION_ID 'del-1'",
    );
    // qty=2,10 が消え、qty=1 と NULL 行が残る。
    assert_eq!(ids(&core, &alice, "id > 0"), vec![1, 3]);
}
