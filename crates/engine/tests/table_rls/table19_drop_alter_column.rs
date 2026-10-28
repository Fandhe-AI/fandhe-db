//! `ALTER TABLE ... DROP COLUMN`／`ALTER COLUMN ... TYPE`（TABLE-19・TASK-203、
//! Issue #901）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-19・TABLE-14（ENUM 型の依存検査）。
//!
//! `crates/engine/src/catalog.rs` の `#[cfg(test)] mod tests` に、行バイト列
//! 不変性・保護列/存在検査・同名再追加・DB 再オープン後の永続性・
//! `merge_encode_scalar_columns` の NULL 書き込みを固定する単体テストを
//! 既に追加済み（詳細は `docs/design/alter-table-drop-modify-column.md`
//! 参照）。本ファイルは、それらとは別のクレート横断（結合）観点、すなわち
//! (1) ENUM 列の削除が `DROP TYPE` の依存結合を断つこと、(2) 物理スロット
//! 上限が墓標も含めて消費されること、を固定する。

use engine::catalog::{CatalogError, ColumnDef, ColumnType, TableSchema};
use engine::storage::Storage;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// ENUM 列を削除すると、その列が参照していた型はもはや「依存テーブルあり」
/// と判定されなくなり、`DROP TYPE` が成功する（TABLE-19 D1: 墓標はフレーム
/// 等価型〔`TEXT`〕へ正規化するため、`ENUM` 型名への参照が残らない）。
#[test]
fn dropping_enum_column_unblocks_drop_type() {
    let path = unique_db_path("table19-enum-drop-unblocks-type");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let mood = storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("create enum type");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("mood", ColumnType::Enum(mood), true),
            ],
        ))
        .expect("create table");

    // 削除前は依存列が残っているため DROP TYPE は拒否される。
    assert!(matches!(
        storage.drop_enum_type("mood"),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));

    storage
        .alter_table_drop_column("docs", "mood")
        .expect("drop enum column");

    let schema = storage.get_table_schema("docs").expect("get schema");
    assert_eq!(schema.columns.len(), 1);

    // 削除後は依存が無くなり DROP TYPE が成功する。
    storage
        .drop_enum_type("mood")
        .expect("drop type should succeed once no live column references it");
}

/// 列数上限（256）は物理スロット総数（生存列 + 墓標）に適用され、DROP
/// COLUMN で生存列を減らしても墓標が容量を消費し続けるため、上限に達した
/// 後は ADD COLUMN が引き続き拒否される（TABLE-19 D1）。
#[test]
fn column_count_limit_applies_to_physical_slots_including_tombstones() {
    let path = unique_db_path("table19-physical-slot-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        ))
        .expect("create table");

    // 255 列を追加し、物理スロット総数を上限（256）ちょうどまで埋める。
    for i in 0..255 {
        storage
            .alter_table_add_column(
                "docs",
                ColumnDef::new(format!("col{i}"), ColumnType::Text, true),
            )
            .unwrap_or_else(|e| panic!("alter_table_add_column(col{i}) should succeed: {e}"));
    }
    let schema = storage.get_table_schema("docs").expect("get schema");
    assert_eq!(schema.columns.len(), 256);

    // 上限ちょうどのため、これ以上の追加は拒否される（Issue #900 以降は `Invalid`
    // と区別した `TooManyColumns`。SQL 表層は `54000` へ写像する）。
    assert!(matches!(
        storage.alter_table_add_column("docs", ColumnDef::new("overflow", ColumnType::Text, true)),
        Err(CatalogError::TooManyColumns { count: 257 })
    ));

    // 1 列削除しても、墓標が物理容量を消費し続けるため、追加は依然として
    // 拒否される（生存列数だけを見る誤った実装ならここで成功してしまう）。
    storage
        .alter_table_drop_column("docs", "col0")
        .expect("drop one column to free a logical slot");
    let schema = storage
        .get_table_schema("docs")
        .expect("get schema after drop");
    assert_eq!(schema.columns.len(), 255);

    assert!(
        matches!(
            storage.alter_table_add_column(
                "docs",
                ColumnDef::new("still_over_limit", ColumnType::Text, true)
            ),
            Err(CatalogError::TooManyColumns { count: 257 })
        ),
        "a tombstoned physical slot must still count against MAX_COLUMN_COUNT"
    );
}
