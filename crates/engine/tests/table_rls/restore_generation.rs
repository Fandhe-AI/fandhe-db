//! 復元の世代番号の永続化と進行の結合テスト（RECOVER-17・RECOVER-13。Issue #2448・親 #2446）。
//!
//! 物理の復元は「バックアップを複製 → 開く → 世代を進める」の順なので、ここでは書き出し（スナップショット）
//! を復元元に見立て、(1) 世代が復元元より大きくなる、(2) 同じ元から繰り返し復元しても、通常の運用条件では前回より大きい（復元間で共有する採番状態は
//! 持たないので、保証は復元元より大きいことに限り、繰り返しの増加は実行環境が通常どおり進むことが前提）、
//! (3) 世代を持つ DB が書き出し・整合検査・論理ダンプの表分類で拒否されない（未分類の表で失敗する不具合の
//! 再発防止）、(4) 壊れた値を黙って受け入れないことを検査する。

use engine::catalog::ddl_export::SchemaDdlScope;
use engine::storage::{IntegrityOptions, Storage};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 復元元から復元先を作って世代を進める（`wire-server restore` と同じ手順の engine 側）。
fn restore_from(src: &std::path::Path, label: &str) -> (u64, std::path::PathBuf, CleanupGuard) {
    let dest = unique_db_path(label);
    let guard = CleanupGuard(dest.clone());
    std::fs::copy(src, &dest).expect("copy backup");
    let mut restored = Storage::open(&dest).expect("open restored");
    let next = restored.advance_restore_generation().expect("advance");
    drop(restored);
    (next, dest, guard)
}

#[test]
fn recover17_a_restore_is_larger_than_its_source_and_repeats_are_monotonic() {
    let src_path = unique_db_path("rg-src");
    let _g = CleanupGuard(src_path.clone());
    {
        let src = Storage::open(&src_path).expect("open src");
        assert_eq!(src.restore_generation().expect("fresh"), 0);
    }
    // 出力先の親は共有でない（0700）ディレクトリにする（Issue #3135）。
    let backup_dir = temp_db::TempDir::new("rg-backup");
    let backup = backup_dir.path().join("backup.redb");
    Storage::open(&src_path)
        .expect("open")
        .export_snapshot(&backup)
        .expect("export");

    let (first, first_path, _g1) = restore_from(&backup, "rg-restored-1");
    assert!(first > 0, "larger than the source generation 0");
    let (second, _p2, _g2) = restore_from(&backup, "rg-restored-2");
    assert!(second > first, "a repeat from the same source is larger");

    // 復元した DB を元にした次の復元は、その世代より大きい。
    let (third, third_path, _g3) = restore_from(&first_path, "rg-restored-3");
    assert!(third > first);
    assert_eq!(
        Storage::open(&third_path)
            .expect("reopen")
            .restore_generation()
            .expect("read"),
        third,
        "persisted across reopen"
    );
}

#[test]
fn recover13_a_database_with_a_generation_passes_export_verify_and_dump() {
    let path = unique_db_path("rg-classified");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    let generation = storage.advance_restore_generation().expect("advance");

    // 書き出し（表の型分類）と再度の復元（世代を持つバックアップの複製）。
    let backup_dir = temp_db::TempDir::new("rg-classified-backup");
    let backup = backup_dir.path().join("backup.redb");
    storage
        .export_snapshot(&backup)
        .expect("export with generation");
    let copy = Storage::open(&backup).expect("open backup");
    assert_eq!(copy.restore_generation().expect("carried"), generation);
    // 整合検査は未知の型の表を矛盾として報告する。世代の表は報告されない。
    let report = copy
        .verify_integrity(&IntegrityOptions::default())
        .expect("verify");
    assert!(report.is_consistent(), "{report:?}");
    // 論理ダンプの表分類で未分類として失敗しない。
    storage
        .export_schema_ddl(&SchemaDdlScope::shared_only(), |_| {
            Ok::<(), std::convert::Infallible>(())
        })
        .expect("schema dump classifies the generation table");
}

#[test]
fn recover17_a_corrupt_value_is_rejected_on_read_and_on_advance() {
    let path = unique_db_path("rg-corrupt");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    storage.advance_restore_generation().expect("advance");
    drop(storage);
    {
        let db = redb::Database::open(&path).expect("open redb");
        let w = db.begin_write().expect("begin");
        {
            #[allow(clippy::disallowed_methods)]
            let mut t = w
                .open_table(redb::TableDefinition::<&str, &[u8]>::new(
                    "restore_generation",
                ))
                .expect("open");
            t.insert("generation", [1u8, 2, 3].as_slice()).expect("put");
        }
        w.commit().expect("commit");
    }
    let mut storage = Storage::open(&path).expect("reopen");
    assert!(storage.restore_generation().is_err());
    assert!(storage.advance_restore_generation().is_err());
}
