//! DB ファイルを読み取り専用で開く経路（RECOVER-13 のポインタ。Issue #3074。親 #2154）。
//!
//! # 役割と責務境界
//!
//! [`ReadOnlyStorage`] は `redb::ReadOnlyDatabase` を包み、DB ファイルへ一切書き込まずに整合検査
//! （[`ReadOnlyStorage::verify_integrity`]）だけを提供する。運用者向け CLI `wire-server verify`
//! （`restore_opt`）が、バックアップ等の保管中ファイルを変更せずに検査するために呼ぶ。
//!
//! 書き込み可能な [`Storage`](super::Storage) とは**別の型**にしてある。`Storage` は書き込みトランザクションの
//! 取得口（`begin_write_txn` 等）を多数持つため、フラグで書き込みを拒否する方式だと取りこぼしが
//! 起きうる。本型は書き込み API を一切持たず、公開面は open と検査だけで、書き込みは型の上で呼べない。
//!
//! # 書き込みを伴う処理を行わない根拠
//!
//! - `Storage::open` が行う旧形式 ID の移行・変更ログの再開・commit 記録の起動は、本型では実行しない。
//!   整合検査の結果は従来方式（LegacyId）と移行済み（MigratedId）の間で変わらない（採番方式 `Allocated` の
//!   表では分岐するが、`Allocated` は新規作成でしか生じず移行前の旧形式 DB には現れない）ため、移行前のファイルも
//!   そのまま検査できる（移行の要否で拒否もフォールバックもしない）。
//! - redb の読み取り専用 open（`ReadOnlyDatabase`）は、ファイルを書き込み権限なしで開き、共有ロック
//!   （`try_lock_shared`）を取る。ヘッダ・アロケータ状態を書き換える修復と、形式の更新は行わず、
//!   必要な状態なら [`ReadOnlyOpenError::NeedsWrite`] で拒否する（fail-closed）。
//!   書き込み経路は redb 内部で到達しない分岐になっており、仮に呼ばれても書き込まずに中断する。
//! - 閉じるとき（drop）も `Database` と違い、アロケータ状態やヘッダを書かない。
//!
//! # ロック
//!
//! 共有ロックなので、読み取り専用の open 同士は並行できる。稼働中のサーバー（排他ロック保持）とは
//! 競合し、[`ReadOnlyOpenError::InUse`] で拒否する。

use std::path::Path;

use redb::ReadableDatabase as _;

use super::integrity::{verify_in_txn, IntegrityError, IntegrityOptions, IntegrityReport};

/// [`ReadOnlyStorage::open`] の失敗。`Display` は固定の英語文言で、パス・内容を含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadOnlyOpenError {
    /// 別のプロセスが書き込み用に開いている（稼働中のサーバー等。ロックが取れない）。
    InUse,
    /// 開くには書き込みが要る（異常終了後の修復、または redb ファイル形式の更新）。
    /// 書かずに拒否した。呼び出し元は、コピーを作ってそちらを開く等の別経路を選べる。
    NeedsWrite,
    /// その他の失敗（存在しない・空・DB ファイルではない・破損・入出力エラー）。
    /// 空のファイルや存在しないパスを新規作成することはない。
    Failed,
}

impl std::fmt::Display for ReadOnlyOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InUse => "database is open for writing by another process",
            Self::NeedsWrite => "database needs repair or upgrade, which requires writing",
            Self::Failed => "failed to open database read-only",
        })
    }
}

impl std::error::Error for ReadOnlyOpenError {}

/// 読み取り専用で開いた DB ファイル。書き込み API を持たない（モジュール doc）。
pub struct ReadOnlyStorage {
    db: redb::ReadOnlyDatabase,
}

impl ReadOnlyStorage {
    /// `path` の DB ファイルを読み取り専用で開く。ファイルは書き込み権限なしで開き、存在しなければ
    /// 作らず、書き込みを要する状態（修復・形式更新）なら書かずに [`ReadOnlyOpenError::NeedsWrite`]
    /// で拒否する。`Storage::open` の挙動は変えない。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReadOnlyOpenError> {
        match redb::ReadOnlyDatabase::open(path) {
            Ok(db) => Ok(Self { db }),
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => Err(ReadOnlyOpenError::InUse),
            Err(redb::DatabaseError::RepairAborted | redb::DatabaseError::UpgradeRequired(_)) => {
                Err(ReadOnlyOpenError::NeedsWrite)
            }
            Err(_) => Err(ReadOnlyOpenError::Failed),
        }
    }

    /// DB ファイル全体の整合を読み取りだけで検査する（[`Storage::verify_integrity`](super::Storage::verify_integrity)
    /// と同じ検査項目・報告。契約は [`super::integrity`] を参照）。読み取りトランザクションを開始
    /// できなければ [`IntegrityError::Backend`]。
    pub fn verify_integrity(
        &self,
        options: &IntegrityOptions,
    ) -> Result<IntegrityReport, IntegrityError> {
        let read = self.db.begin_read().map_err(|_| IntegrityError::Backend)?;
        Ok(verify_in_txn(&read, options))
    }

    /// 変更ログのアーカイブ先の連鎖を、この DB のメタ情報（DB 識別子・timeline・有効化時の世代・水位）と
    /// 突き合わせて検証する（[`Storage::verify_change_log_archive`](super::Storage::verify_change_log_archive)
    /// と同じ検査。RECOVER-14 のポインタ・Issue #2167）。読み取りトランザクションだけを使い、DB へ書かない。
    pub fn verify_change_log_archive(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<super::ArchiveSummary, super::ArchiveError> {
        let read = self.db.begin_read()?;
        super::change_log::archive::verify_against_read(&read, dir.as_ref())
    }

    /// [`Storage::verify_change_log_archive_from`](super::Storage::verify_change_log_archive_from) と同じ検査を、
    /// 読み取り専用で開いた DB から行う（保持管理で古いセグメントを消したアーカイブ向け。Issue #3092）。
    pub fn verify_change_log_archive_from(
        &self,
        dir: impl AsRef<Path>,
        retained_from: u64,
    ) -> Result<super::ArchiveSummary, super::ArchiveError> {
        let read = self.db.begin_read()?;
        super::change_log::archive::verify_against_read_from(
            &read,
            dir.as_ref(),
            Some(retained_from),
        )
    }
}

impl ReadOnlyStorage {
    /// [`Storage::export_incremental`](super::Storage::export_incremental) と同じ増分バックアップの書き出しを、
    /// 読み取り専用で開いた停止中の DB から行う（RECOVER-13・RECOVER-14・Issue #2168）。DB へ書かない。
    pub fn export_incremental(
        &self,
        base: &super::ChainPoint,
        dest: impl AsRef<Path>,
    ) -> Result<super::IncrementalSummary, super::IncrementalError> {
        super::change_log::incremental::export_from_read(
            &self.db.begin_read()?,
            base,
            dest.as_ref(),
        )
    }
}

#[cfg(test)]
mod tests {
    //! RECOVER-13・Issue #3074: 読み取り専用 open の前後でファイルのバイト列が変わらないこと、
    //! 書き込みを要する状態を書かずに拒否すること、稼働中を拒否すること。
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, RowIdentity, TableSchema};
    use crate::row_codec::Value as RowCodecValue;
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::TempDir;

    fn put_rows(s: &Storage) {
        for id in 1..=3u64 {
            s.put(
                id,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0],
                    metadata: b"m",
                },
            )
            .unwrap();
        }
    }

    /// 閉じた（整合した）DB を作る。
    fn clean_db(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.db_path();
        let s = Storage::open(&path).unwrap();
        put_rows(&s);
        drop(s);
        path
    }

    fn legacy_schema() -> TableSchema {
        TableSchema::new(
            "plain",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        )
    }

    #[test]
    fn read_only_verify_leaves_bytes_identical_and_matches_writable_report() {
        let dir = TempDir::new("ro-clean");
        let path = clean_db(&dir);
        let before = std::fs::read(&path).unwrap();
        let ro_report = {
            let ro = ReadOnlyStorage::open(&path).unwrap();
            ro.verify_integrity(&IntegrityOptions::default()).unwrap()
        };
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(ro_report.is_consistent());
        assert!(ro_report.rows_scanned >= 3);

        // 書き込み可能な open でも同じ報告になる（検査項目・結果が同一）。open は書くのでコピーで比べる。
        let copy = dir.path().join("copy.redb");
        std::fs::copy(&path, &copy).unwrap();
        let rw = Storage::open(&copy).unwrap();
        assert_eq!(
            rw.verify_integrity(&IntegrityOptions::default()).unwrap(),
            ro_report
        );
    }

    #[test]
    fn read_only_verify_with_small_memory_budget_leaves_bytes_identical() {
        let dir = TempDir::new("ro-budget");
        let path = clean_db(&dir);
        let before = std::fs::read(&path).unwrap();
        let options = IntegrityOptions {
            memory_budget_bytes: 1,
            ..IntegrityOptions::default()
        };
        let ro = ReadOnlyStorage::open(&path).unwrap();
        // 予算が極小でも読み取りだけで完了する（結果が整合かは問わない。書かないことを確かめる）。
        let _ = ro.verify_integrity(&options).unwrap();
        drop(ro);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn legacy_format_table_is_verified_without_migration() {
        let dir = TempDir::new("ro-legacy");
        let path = dir.db_path();
        {
            let s = Storage::open(&path).unwrap();
            s.create_table(&legacy_schema()).unwrap();
            for (t, id) in [("tenant-a", 1u64), ("tenant-b", 1)] {
                s.insert_typed_row(
                    "plain",
                    id,
                    t,
                    Visibility::Public,
                    &[RowCodecValue::Text(format!("{t}-{id}"))],
                )
                .unwrap();
            }
        }
        let before = std::fs::read(&path).unwrap();
        let report = {
            let ro = ReadOnlyStorage::open(&path).unwrap();
            ro.verify_integrity(&IntegrityOptions::default()).unwrap()
        };
        assert!(report.is_consistent());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // 移行は書き込み可能な open だけが行う（読み取り専用の後でも従来方式のまま）。
        let s = Storage::open(&path).unwrap();
        assert_eq!(
            s.get_table_schema("plain").unwrap().row_identity(),
            RowIdentity::MigratedId
        );
    }

    #[test]
    fn file_copied_while_live_needs_repair_and_is_rejected_unchanged() {
        let dir = TempDir::new("ro-crash");
        let live = dir.db_path();
        let s = Storage::open(&live).unwrap();
        put_rows(&s);
        // 開いたまま複製すると、正常に閉じていない（異常終了後と同じ）状態のファイルになる。
        let crashed = dir.path().join("crashed.redb");
        std::fs::copy(&live, &crashed).unwrap();
        let before = std::fs::read(&crashed).unwrap();
        assert_eq!(
            ReadOnlyStorage::open(&crashed).err(),
            Some(ReadOnlyOpenError::NeedsWrite)
        );
        assert_eq!(std::fs::read(&crashed).unwrap(), before);
        drop(s);
    }

    #[test]
    fn writer_open_database_is_rejected_as_in_use_and_shared_opens_coexist() {
        let dir = TempDir::new("ro-lock");
        let path = clean_db(&dir);
        {
            let _rw = Storage::open(&path).unwrap();
            assert_eq!(
                ReadOnlyStorage::open(&path).err(),
                Some(ReadOnlyOpenError::InUse)
            );
        }
        let a = ReadOnlyStorage::open(&path).unwrap();
        let b = ReadOnlyStorage::open(&path).unwrap();
        assert!(a.verify_integrity(&IntegrityOptions::default()).is_ok());
        assert!(b.verify_integrity(&IntegrityOptions::default()).is_ok());
        // 読み取り専用が残っている間は書き込み用の open も拒否される。
        assert!(Storage::open(&path).is_err());
    }

    #[test]
    fn missing_empty_and_non_database_files_fail_without_creating_or_writing() {
        let dir = TempDir::new("ro-bad");
        let missing = dir.path().join("missing.redb");
        assert_eq!(
            ReadOnlyStorage::open(&missing).err(),
            Some(ReadOnlyOpenError::Failed)
        );
        assert!(!missing.exists());

        let empty = dir.path().join("empty.redb");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            ReadOnlyStorage::open(&empty).err(),
            Some(ReadOnlyOpenError::Failed)
        );
        assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0);

        let junk = dir.path().join("junk.redb");
        std::fs::write(&junk, vec![0xAB; 8192]).unwrap();
        assert_eq!(
            ReadOnlyStorage::open(&junk).err(),
            Some(ReadOnlyOpenError::Failed)
        );
        assert_eq!(std::fs::read(&junk).unwrap(), vec![0xAB; 8192]);
    }
}
