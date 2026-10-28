//! 列の読み出し時補完値（`fill_value`）と INSERT 既定値（`default_expr`）のカタログ上の
//! 分離（TABLE-39 の前提・TABLE-5・TABLE-16、Issue #1627）の結合テスト。ポインタ表記のみ。
//!
//! 本 Issue の時点では DDL 経路は常に 2 つの値を同じにする（ミラー）。食い違う列は
//! Rust API（`ColumnDef::with_fill_value`）でのみ作れ、カタログ v14 として永続化される。
//! ここでは (1) 食い違う列の reopen をまたぐ往復、(2) ミラー状態の表が変わらないこと、
//! (3) `alter_table_add_column` が食い違う列を拒否すること、(4) 時刻 DEFAULT の補完値を
//! 拒否すること、(5) 型変更が補完値にも範囲検査をかけることを固定する。

use engine::catalog::{ColumnDef, ColumnDefault, ColumnType, TableSchema, TimeDefault};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn text(s: &str) -> ColumnDefault {
    ColumnDefault::Text(s.to_string())
}

fn diverging_table() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, true)
                .with_default(text("ja"))
                .with_fill_value(Some(text("en"))),
            ColumnDef::new("same", ColumnType::Text, true).with_default(text("x")),
        ],
    )
}

#[test]
fn diverging_columns_roundtrip_across_reopen() {
    let path = unique_db_path("fill-value-diverge-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage
            .create_table(&diverging_table())
            .expect("create diverging table");
    }
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("docs").expect("schema");
    let lang = schema
        .columns
        .iter()
        .find(|c| c.name == "lang")
        .expect("lang");
    assert_eq!(lang.default_expr, Some(text("ja")));
    assert_eq!(lang.fill_value, Some(text("en")));
    let same = schema
        .columns
        .iter()
        .find(|c| c.name == "same")
        .expect("same");
    assert_eq!(same.default_expr, same.fill_value);
}

#[test]
fn mirrored_table_is_unchanged_across_reopen() {
    let path = unique_db_path("fill-value-mirror-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(4), false),
                    ColumnDef::new("lang", ColumnType::Text, false).with_default(text("ja")),
                ],
            ))
            .expect("create");
    }
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("docs").expect("schema");
    for column in &schema.columns {
        assert_eq!(column.fill_value, column.default_expr, "{}", column.name);
    }
}

#[test]
fn add_column_rejects_diverging_fill_value() {
    let path = unique_db_path("fill-value-add-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        ))
        .expect("create");
    let diverging = ColumnDef::new("lang", ColumnType::Text, true)
        .with_default(text("ja"))
        .with_fill_value(Some(text("en")));
    assert!(storage.alter_table_add_column("docs", diverging).is_err());
    let mirrored = ColumnDef::new("lang", ColumnType::Text, true).with_default(text("ja"));
    storage
        .alter_table_add_column("docs", mirrored)
        .expect("mirrored add column");
    let schema = storage.get_table_schema("docs").expect("schema");
    let lang = schema
        .columns
        .iter()
        .find(|c| c.name == "lang")
        .expect("lang");
    assert_eq!(lang.fill_value, Some(text("ja")));
}

#[test]
fn time_default_fill_value_is_rejected_unless_mirrored() {
    let path = unique_db_path("fill-value-time");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    let bad = TableSchema::new(
        "events",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("at", ColumnType::Timestamp, true)
                .with_fill_value(Some(ColumnDefault::CurrentTime(TimeDefault::Now))),
        ],
    );
    assert!(storage.create_table(&bad).is_err());
    let mirrored = TableSchema::new(
        "events",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("at", ColumnType::Timestamp, true)
                .with_default(ColumnDefault::CurrentTime(TimeDefault::Now)),
        ],
    );
    storage
        .create_table(&mirrored)
        .expect("mirrored time default");
}

/// 補完値だけが範囲外になる SMALLINT への縮小は拒否され、表は変わらない。
#[test]
fn smallint_narrowing_checks_fill_value_too() {
    let path = unique_db_path("fill-value-smallint");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage
            .create_table(&TableSchema::new(
                "d",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(4), false),
                    ColumnDef::new("y", ColumnType::Integer, true)
                        .with_default(ColumnDefault::Number("7".to_string()))
                        .with_fill_value(Some(ColumnDefault::Number("40000".to_string()))),
                ],
            ))
            .expect("create");
    }
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    let mut s = SessionState::default();
    s.allow_ddl();
    let err = core
        .execute_sql_in_session(&ctx(), &mut s, "ALTER TABLE d ALTER COLUMN y TYPE SMALLINT")
        .expect_err("fill value out of SMALLINT range");
    assert_eq!(err.wire_code(), "22003");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("d").expect("schema");
    assert_eq!(schema.columns[1].ty, ColumnType::Integer);
}
