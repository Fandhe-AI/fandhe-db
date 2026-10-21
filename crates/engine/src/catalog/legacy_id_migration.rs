//! 既存表の `id` 移行ドライバ（TABLE-23・Issue #2200。設計記録:
//! `docs/design/rowid-primary-key-separation.md` §12）。
//!
//! 役割: データディレクトリを開く時点（`Storage::open_with_durability`）で、従来方式
//! （[`RowIdentity::LegacyId`]）の永続表を、移行済みの印（[`RowIdentity::MigratedId`]、
//! カタログ v15 の `rowmode:migrated`）が付いた表へ一方向に書き換える。
//!
//! 方式: 行・物理キー・索引・台帳は一切書き換えない（`id` は rowid の別名のまま）。1 段目で書き換えるのは
//! カタログ値 1 件だけで、表ごとに 1 つの write トランザクションで原子的に行う。続く 2 段目（Issue #1699）で、
//! 移行済みなのに `id` を所有する SEQUENCE（`<table>_id_seq`）を持たない表へ IDENTITY 相当の SEQUENCE を
//! 割り当てる（`catalog::sequence::assign_migrated_id_identity_in_txn`。SQL の `id` 省略 INSERT の採番先。
//! 設計記録: `docs/design/migrated-id-identity.md` D4）。2 段目は 1 段目で移した表だけでなく、すでに
//! 移行済みで SEQUENCE が無い表も補完する（1 段目と 2 段目の間のクラッシュ後も次回の open で収束する）。
//! 各トランザクションは
//! 先頭でスキーマを読み直して従来方式のままかを再確認するため、再実行・並行プロセスでも収束する
//! （途中でクラッシュしても、各表は「移行済み」か「未移行」のどちらかで、次回の open が残りを移す）。
//!
//! 不変条件:
//! - テナント境界（P0）: テナントの行・索引は読まず、全テナント共有のカタログ定義だけを書く。
//! - fail-closed: デコードできない項目は移行せずそのまま残す（アクセス時に従来どおり
//!   `CorruptSchema` で拒否される。1 表の破損で DB 全体を開けなくしない）。redb の I/O・
//!   トランザクションのエラーは open を失敗させる。
//! - 非露出: エラー文面に表名・テナント名・値を含めない（固定文言）。
//! - DoS: 1 段目の作業量はカタログの件数に比例し行数に依存しない。2 段目は割り当て対象の表ごとに
//!   テナント単位の最大 `id` を物理キーの飛ばし読みで求める（テナント数 × log n。`MAX_CURSORS` で
//!   打ち切る）。どちらも open 時に一度だけ走り外部入力では起動しない。対象が無いときは write
//!   トランザクションを開かない（新規 DB に catalog 表を作らない）。
//! - 2 段目の失敗: redb の I/O エラーは open を失敗させる。前提の不一致・上限超過など
//!   それ以外の失敗は、その表の txn を破棄して見送る（SEQUENCE の無い移行済み表のまま残り、
//!   `id` 省略の INSERT は固定文言で拒否される。fail-closed）。
//! - 世代表: 書き換えた表の世代を同じ txn で進める（open 中でキャッシュは未構築だが、カタログ変更と世代 bump を
//!   常に組にする既存規約に従う）。

use redb::{ReadableDatabase, ReadableTable};

use super::{
    decode_schema_with_resolver, encode_schema, get_enum_type_in_read_txn, validate_identifier,
    CatalogError, RowIdentity, CATALOG_TABLE,
};
use crate::storage::{Result, Storage, StorageError};

/// 故障注入の位置（テスト専用の `hook` が参照する。本番は常に中断しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// 1 段目・2 段目とも「commit の前後」を表すため、変種名の接尾辞が揃うのは意図どおり。
#[allow(clippy::enum_variant_names)]
pub(crate) enum MigrationStage {
    /// 表の書き換え後・commit 前。`true` を返すと commit せずトランザクションを破棄する。
    BeforeCommit,
    /// 表の commit 後。`true` を返すとそこで open を失敗させる。
    AfterCommit,
    /// 2 段目（SEQUENCE の割り当て）の表の書き込み後・commit 前。
    IdentityBeforeCommit,
    /// 2 段目の表の commit 後。
    IdentityAfterCommit,
}

/// 中断時の固定文言。
const INTERRUPTED_MESSAGE: &str = "legacy id migration interrupted";
/// カタログ値の異常時の固定文言。
const CATALOG_MESSAGE: &str = "legacy id migration failed to update the catalog";

fn catalog_to_storage(e: CatalogError) -> StorageError {
    match e {
        CatalogError::Backend(err) => StorageError::Backend(err),
        _ => StorageError::Codec(CATALOG_MESSAGE.to_string()),
    }
}

/// `Storage::open_with_durability` から呼ぶ入口。移行した表の件数を返す。
pub(crate) fn migrate_legacy_ids(storage: &Storage) -> Result<usize> {
    migrate_legacy_ids_with(storage, &mut |_, _| false)
}

/// 故障注入つきの本体。`hook(stage, 0 始まりの移行済み件数)` が `true` を返すと中断する。
pub(crate) fn migrate_legacy_ids_with(
    storage: &Storage,
    hook: &mut dyn FnMut(MigrationStage, usize) -> bool,
) -> Result<usize> {
    let migrated = migrate_marks_with(storage, hook)?;
    assign_missing_identities(storage, hook)?;
    Ok(migrated)
}

/// 1 段目だけ（`rowmode:migrated` の印の書き換え）。SEQUENCE の割り当てを単独で試すテストが、
/// 2 段目を挟まない移行済み表を用意するために使う。
#[cfg(test)]
pub(crate) fn migrate_marks_only(storage: &Storage) -> Result<usize> {
    migrate_marks_with(storage, &mut |_, _| false)
}

fn migrate_marks_with(
    storage: &Storage,
    hook: &mut dyn FnMut(MigrationStage, usize) -> bool,
) -> Result<usize> {
    let candidates = candidates_with(storage, RowIdentity::LegacyId)?;
    let mut migrated = 0usize;
    for name in candidates {
        let txn = storage.begin_write_txn()?;
        let schema = {
            let read = &*txn;
            let bytes = {
                let table = read
                    .open_table(CATALOG_TABLE)
                    .map_err(|e| catalog_to_storage(e.into()))?;
                let guard = table
                    .get(name.as_str())
                    .map_err(|e| catalog_to_storage(e.into()))?;
                match guard {
                    Some(g) => g.value().to_vec(),
                    None => continue,
                }
            };
            let mut resolve = |n: &str| get_enum_type_in_read_txn(read, n);
            match decode_schema_with_resolver(&name, &bytes, &mut resolve) {
                Ok(s) => s,
                // 壊れた項目は移行せず残す（アクセス時に fail-closed で拒否される）。
                Err(CatalogError::Backend(err)) => return Err(StorageError::Backend(err)),
                Err(_) => continue,
            }
        };
        // 同一 txn 内で従来方式のままかを再確認する（再実行・並行プロセスでの冪等性）。
        if schema.row_identity() != RowIdentity::LegacyId {
            continue;
        }
        let bytes = match encode_schema(&schema.with_row_identity(RowIdentity::MigratedId)) {
            Ok(b) => b,
            Err(CatalogError::Backend(err)) => return Err(StorageError::Backend(err)),
            Err(_) => continue,
        };
        {
            let mut table = txn
                .open_table(CATALOG_TABLE)
                .map_err(|e| catalog_to_storage(e.into()))?;
            table
                .insert(name.as_str(), bytes.as_slice())
                .map_err(|e| catalog_to_storage(e.into()))?;
        }
        if hook(MigrationStage::BeforeCommit, migrated) {
            drop(txn);
            return Err(StorageError::Codec(INTERRUPTED_MESSAGE.to_string()));
        }
        // スキーマ値が変わるため世代を進める（世代照合の網羅検査 table_generation_bump_coverage に従う。
        // open 中でキャッシュは未構築だが、カタログ変更は常に世代 bump と組にして不変条件を単純に保つ）。
        super::bump_table_generation_in_txn(&txn, &name).map_err(catalog_to_storage)?;
        crate::recovery::commit_boundary::commit(txn)?;
        migrated += 1;
        if hook(MigrationStage::AfterCommit, migrated) {
            return Err(StorageError::Codec(INTERRUPTED_MESSAGE.to_string()));
        }
    }
    Ok(migrated)
}

/// 2 段目: 移行済みで `id` の SEQUENCE を持たない表へ IDENTITY 相当の SEQUENCE を割り当てる
/// （Issue #1699）。表ごとに 1 つの write txn（SEQUENCE 作成・状態行・世代 bump を同じ txn で
/// 原子的に commit）。部品自体が冪等なので、再実行・並行プロセスでも収束する。
fn assign_missing_identities(
    storage: &Storage,
    hook: &mut dyn FnMut(MigrationStage, usize) -> bool,
) -> Result<()> {
    let missing = identity_candidates(storage)?;
    let mut assigned = 0usize;
    for name in missing {
        let txn = storage.begin_write_txn()?;
        match super::sequence::assign_migrated_id_identity_in_txn(&txn, &name) {
            Ok(super::sequence::MigratedIdIdentityOutcome::AlreadyAssigned { .. }) => continue,
            Ok(super::sequence::MigratedIdIdentityOutcome::Assigned { .. }) => {}
            Err(CatalogError::Backend(err)) => return Err(StorageError::Backend(err)),
            // 前提の不一致・破損・上限超過は、その表を見送る（txn は破棄され何も残らない）。
            Err(_) => continue,
        }
        if hook(MigrationStage::IdentityBeforeCommit, assigned) {
            drop(txn);
            return Err(StorageError::Codec(INTERRUPTED_MESSAGE.to_string()));
        }
        super::bump_table_generation_in_txn(&txn, &name).map_err(catalog_to_storage)?;
        crate::recovery::commit_boundary::commit(txn)?;
        assigned += 1;
        if hook(MigrationStage::IdentityAfterCommit, assigned) {
            return Err(StorageError::Codec(INTERRUPTED_MESSAGE.to_string()));
        }
    }
    Ok(())
}

/// 2 段目の候補: 移行済みで `id` を所有する SEQUENCE が無い表（読み取り txn で検出する。
/// 対象が無ければ write トランザクションを開かない）。
fn identity_candidates(storage: &Storage) -> Result<Vec<String>> {
    let migrated = candidates_with(storage, RowIdentity::MigratedId)?;
    if migrated.is_empty() {
        return Ok(migrated);
    }
    let read = storage.db().begin_read().map_err(StorageError::from)?;
    super::sequence::tables_missing_id_sequence(&read, &migrated).map_err(catalog_to_storage)
}

/// 読み取り txn で、行識別方式が `identity` の表名を集める（辞書順）。対象が無ければ空を返し、
/// write トランザクションを開かない。デコード不能な項目は候補に含めない。
fn candidates_with(storage: &Storage, identity: RowIdentity) -> Result<Vec<String>> {
    let read = storage.db().begin_read().map_err(StorageError::from)?;
    let table = match read.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(catalog_to_storage(e.into())),
    };
    let mut names = Vec::new();
    for entry in table.iter().map_err(|e| catalog_to_storage(e.into()))? {
        let (key, value) = entry.map_err(|e| catalog_to_storage(e.into()))?;
        let name = key.value();
        if validate_identifier(name).is_err() {
            continue;
        }
        let bytes = value.value();
        let mut resolve = |n: &str| get_enum_type_in_read_txn(&read, n);
        match decode_schema_with_resolver(name, bytes, &mut resolve) {
            Ok(s) if s.row_identity() == identity => names.push(name.to_string()),
            Err(CatalogError::Backend(err)) => return Err(StorageError::Backend(err)),
            _ => {}
        }
    }
    Ok(names)
}
#[cfg(test)]
mod tests {
    //! TABLE-23・Issue #2200: 移行の収束・冪等・故障注入・壊れた項目・ALTER 後の印の維持。
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, MigratedIdRole, TableSchema};
    use crate::row_codec::Value as RowCodecValue;
    use crate::storage::Visibility;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn legacy(name: &str) -> TableSchema {
        TableSchema::new(name, vec![ColumnDef::new("body", ColumnType::Text, false)])
    }

    fn with_rows(storage: &Storage, name: &str) {
        for (t, id) in [("tenant-a", 1u64), ("tenant-a", 7), ("tenant-b", 1)] {
            storage
                .insert_typed_row(
                    name,
                    id,
                    t,
                    Visibility::Public,
                    &[RowCodecValue::Text(format!("{t}-{id}"))],
                )
                .expect("insert");
        }
    }

    fn identity(storage: &Storage, name: &str) -> RowIdentity {
        storage
            .get_table_schema(name)
            .expect("schema")
            .row_identity()
    }

    fn raw_catalog_value(storage: &Storage, name: &str) -> Vec<u8> {
        let read = storage.db().begin_read().expect("read");
        let table = read.open_table(CATALOG_TABLE).expect("catalog");
        let v = table
            .get(name)
            .expect("get")
            .expect("entry")
            .value()
            .to_vec();
        v
    }

    /// 開き直すと全表が移行済みになり、行 API・`id` の役割は従来どおり、3 回目の open は書き込まない。
    #[test]
    fn open_migrates_all_legacy_tables_and_is_idempotent() {
        let path = unique_db_path("legacy-id-migrate");
        let _g = CleanupGuard(path.clone());
        {
            let s = Storage::open(&path).expect("open");
            s.create_table(&legacy("plain")).expect("create");
            s.create_table(&legacy("keyed").with_primary_key(vec!["body".into()]))
                .expect("create");
            with_rows(&s, "plain");
            assert_eq!(identity(&s, "plain"), RowIdentity::LegacyId);
        }
        let before = {
            let s = Storage::open(&path).expect("reopen");
            assert_eq!(identity(&s, "plain"), RowIdentity::MigratedId);
            assert_eq!(identity(&s, "keyed"), RowIdentity::MigratedId);
            let plain = s.get_table_schema("plain").expect("schema");
            assert_eq!(plain.migrated_id_role(), Some(MigratedIdRole::PrimaryKey));
            let keyed = s.get_table_schema("keyed").expect("schema");
            assert_eq!(keyed.migrated_id_role(), Some(MigratedIdRole::Unique));
            // 行・キーは書き換えない（rowid == id のまま読める）。
            let row = s.get_row_from_table("plain", "tenant-a", 7).expect("row");
            assert_eq!(row.id, 7);
            assert!(raw_catalog_value(&s, "plain").starts_with(b"v15\n"));
            assert!(s.write_txn_creations() >= 2);
            raw_catalog_value(&s, "plain")
        };
        let s = Storage::open(&path).expect("third open");
        // 移行済みのみのカタログでは write txn を開かない。
        assert_eq!(s.write_txn_creations(), 0);
        assert_eq!(raw_catalog_value(&s, "plain"), before);
    }

    /// 新規 DB を開いても catalog 表を作らず write txn も開かない。
    #[test]
    fn fresh_database_open_creates_nothing() {
        let path = unique_db_path("legacy-id-fresh");
        let _g = CleanupGuard(path.clone());
        let s = Storage::open(&path).expect("open");
        assert_eq!(s.write_txn_creations(), 0);
        let read = s.db().begin_read().expect("read");
        assert!(matches!(
            read.open_table(CATALOG_TABLE),
            Err(redb::TableError::TableDoesNotExist(_))
        ));
    }

    /// 故障注入（受け入れ基準 2）: commit 後の中断・未 commit の破棄のどちらでも、各表は
    /// 移行済みか未移行のどちらかで、再実行で収束する。
    #[test]
    fn interrupted_migration_converges_without_partial_state() {
        let path = unique_db_path("legacy-id-crash");
        let _g = CleanupGuard(path.clone());
        let s = Storage::open(&path).expect("open");
        for n in ["a", "b", "c"] {
            s.create_table(&legacy(n)).expect("create");
        }
        // 未 commit のまま破棄 → 何も移行されない。
        let r = migrate_legacy_ids_with(&s, &mut |st, _| st == MigrationStage::BeforeCommit);
        assert!(r.is_err());
        for n in ["a", "b", "c"] {
            assert_eq!(identity(&s, n), RowIdentity::LegacyId);
        }
        // 1 表 commit 後に中断 → 先頭だけ移行済み。
        let r =
            migrate_legacy_ids_with(&s, &mut |st, k| st == MigrationStage::AfterCommit && k == 1);
        assert!(r.is_err());
        assert_eq!(identity(&s, "a"), RowIdentity::MigratedId);
        assert_eq!(identity(&s, "b"), RowIdentity::LegacyId);
        assert_eq!(identity(&s, "c"), RowIdentity::LegacyId);
        // 再実行で残りが移行され、さらに再実行しても 0 件。
        assert_eq!(migrate_legacy_ids(&s).expect("rerun"), 2);
        assert_eq!(migrate_legacy_ids(&s).expect("again"), 0);
        for n in ["a", "b", "c"] {
            assert_eq!(identity(&s, n), RowIdentity::MigratedId);
        }
    }

    fn id_sequence_owner(
        storage: &Storage,
        table: &str,
    ) -> Option<crate::catalog::sequence::SequenceOwner> {
        storage
            .sequence_owner(&format!("{table}_id_seq"))
            .ok()
            .flatten()
    }

    /// Issue #1699: 開くと 1 段目で移した表へ `<table>_id_seq` が割り当てられ（所有者 `(table, id)`）、
    /// 3 回目の open は何も書かない。
    #[test]
    fn open_assigns_id_sequence_and_is_idempotent() {
        let path = unique_db_path("legacy-id-assign");
        let _g = CleanupGuard(path.clone());
        {
            let s = Storage::open(&path).expect("open");
            s.create_table(&legacy("plain")).expect("create");
            with_rows(&s, "plain");
            assert!(id_sequence_owner(&s, "plain").is_none());
        }
        {
            let s = Storage::open(&path).expect("reopen");
            let owner = id_sequence_owner(&s, "plain").expect("owner");
            assert_eq!(
                (owner.table.as_str(), owner.column.as_str()),
                ("plain", "id")
            );
        }
        let s = Storage::open(&path).expect("third open");
        assert_eq!(s.write_txn_creations(), 0);
        assert!(id_sequence_owner(&s, "plain").is_some());
    }

    /// Issue #1699: 1 段目だけ済んで SEQUENCE が無い移行済み表（#2589 以降の DB）も、次の open が補完する。
    #[test]
    fn open_backfills_sequence_for_already_migrated_tables() {
        let path = unique_db_path("legacy-id-backfill");
        let _g = CleanupGuard(path.clone());
        {
            let s = Storage::open(&path).expect("open");
            s.create_table(&legacy("old")).expect("create");
            assert_eq!(migrate_marks_only(&s).expect("marks"), 1);
            assert_eq!(identity(&s, "old"), RowIdentity::MigratedId);
            assert!(id_sequence_owner(&s, "old").is_none());
        }
        let s = Storage::open(&path).expect("reopen");
        assert!(id_sequence_owner(&s, "old").is_some());
    }

    /// Issue #1699: 無関係な SEQUENCE が 1 件壊れていても open は成功し、他の表の割り当ては続く。
    /// 壊れた名前が `id` 用の暗黙名に当たる表は所有関係を判断できないため見送る（fail-closed）。
    #[test]
    fn corrupt_sequence_does_not_block_open_or_other_assignments() {
        let path = unique_db_path("legacy-id-corrupt-seq");
        let _g = CleanupGuard(path.clone());
        {
            let s = Storage::open(&path).expect("open");
            s.create_table(&legacy("good")).expect("create");
            s.create_table(&legacy("risky")).expect("create");
            let txn = s.begin_write_txn().expect("txn");
            {
                let mut t = txn
                    .open_table(super::super::sequence::SEQUENCES_TABLE)
                    .expect("sequences");
                t.insert(("public", "unrelated_seq"), b"garbage".as_slice())
                    .expect("insert");
                t.insert(("public", "risky_id_seq"), b"garbage".as_slice())
                    .expect("insert");
            }
            txn.commit_raw_for_test().expect("commit");
        }
        let s = Storage::open(&path).expect("open despite corrupt sequence");
        assert!(id_sequence_owner(&s, "good").is_some());
        assert!(id_sequence_owner(&s, "risky").is_none());
    }

    /// Issue #1699: 2 段目を commit 前・commit 後で中断しても、再実行で収束し二重に作らない。
    #[test]
    fn interrupted_identity_assignment_converges() {
        let path = unique_db_path("legacy-id-assign-crash");
        let _g = CleanupGuard(path.clone());
        let s = Storage::open(&path).expect("open");
        for n in ["a", "b"] {
            s.create_table(&legacy(n)).expect("create");
        }
        let r =
            migrate_legacy_ids_with(&s, &mut |st, _| st == MigrationStage::IdentityBeforeCommit);
        assert!(r.is_err());
        // 1 段目は commit 済み、2 段目は未 commit のため SEQUENCE は無い。
        assert_eq!(identity(&s, "a"), RowIdentity::MigratedId);
        assert!(id_sequence_owner(&s, "a").is_none());
        let r = migrate_legacy_ids_with(&s, &mut |st, k| {
            st == MigrationStage::IdentityAfterCommit && k == 1
        });
        assert!(r.is_err());
        assert!(id_sequence_owner(&s, "a").is_some());
        assert!(id_sequence_owner(&s, "b").is_none());
        migrate_legacy_ids(&s).expect("rerun");
        assert!(id_sequence_owner(&s, "b").is_some());
        // 収束後は write txn を開かない。
        let before = s.write_txn_creations();
        migrate_legacy_ids(&s).expect("again");
        assert_eq!(s.write_txn_creations(), before);
    }

    /// Issue #1699 (J5): 移行済みなのに所有 SEQUENCE が無い表（2 段目が見送った表）への `id` 省略 INSERT は、
    /// 値も行も消費せず `23502` で拒否される。明示 `id` の INSERT は従来どおり通る。
    #[test]
    fn omitted_id_without_sequence_is_rejected_with_23502() {
        let path = unique_db_path("legacy-id-noseq");
        let _g = CleanupGuard(path.clone());
        let s = Storage::open(&path).expect("open");
        s.create_table(&legacy("t")).expect("create");
        assert_eq!(migrate_marks_only(&s).expect("marks"), 1);
        let core =
            crate::core::EngineCore::from_storage(s, Box::new(crate::kernel::CpuScalarProvider));
        let ctx = crate::policy::PolicyContext::with_visibilities(
            "tenant-a",
            [Visibility::Public, Visibility::Private],
        )
        .expect("ctx");
        let mut session = crate::sql::mode::SessionState::default();
        let err = core
            .execute_sql_in_session(&ctx, &mut session, "INSERT INTO t (body) VALUES ('x')")
            .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "23502", "{err}");
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "INSERT INTO t (id, body) VALUES (1, 'x')",
        )
        .expect("explicit id still works");
        let rows = core
            .execute_sql(&ctx, "SELECT id FROM t LIMIT 10")
            .expect("select");
        assert_eq!(rows.rows.len(), 1);
    }

    /// 採番方式の表は移行しない。壊れた項目は残したまま他の表を移行し、open は成功する。
    #[test]
    fn allocated_is_kept_and_corrupt_entry_does_not_block_open() {
        let path = unique_db_path("legacy-id-corrupt");
        let _g = CleanupGuard(path.clone());
        {
            let s = Storage::open(&path).expect("open");
            s.create_table(&legacy("old")).expect("create");
            s.create_table(&legacy("alloc").with_row_identity(RowIdentity::Allocated))
                .expect("create");
            let txn = s.begin_write_txn().expect("txn");
            {
                let mut t = txn.open_table(CATALOG_TABLE).expect("catalog");
                t.insert("broken", b"garbage".as_slice()).expect("insert");
            }
            txn.commit_raw_for_test().expect("commit");
        }
        let s = Storage::open(&path).expect("reopen despite corrupt entry");
        assert_eq!(identity(&s, "old"), RowIdentity::MigratedId);
        assert_eq!(identity(&s, "alloc"), RowIdentity::Allocated);
        assert!(matches!(
            s.get_table_schema("broken"),
            Err(CatalogError::CorruptSchema(_))
        ));
        assert_eq!(raw_catalog_value(&s, "broken"), b"garbage");
    }

    /// ALTER 後も移行済みの印が残る（次の open で再移行されない）。
    #[test]
    fn alter_keeps_migrated_mark() {
        let path = unique_db_path("legacy-id-alter");
        let _g = CleanupGuard(path.clone());
        {
            let s = Storage::open(&path).expect("open");
            s.create_table(&legacy("t")).expect("create");
        }
        let s = Storage::open(&path).expect("reopen");
        assert_eq!(identity(&s, "t"), RowIdentity::MigratedId);
        s.alter_table_add_column("t", ColumnDef::new("extra", ColumnType::Text, true))
            .expect("add column");
        assert_eq!(identity(&s, "t"), RowIdentity::MigratedId);
        s.alter_table_rename("t", "t2").expect("rename");
        assert_eq!(identity(&s, "t2"), RowIdentity::MigratedId);
        drop(s);
        let s = Storage::open(&path).expect("reopen again");
        assert_eq!(s.write_txn_creations(), 0);
        assert!(raw_catalog_value(&s, "t2")
            .windows(16)
            .any(|w| w == b"rowmode:migrated"));
    }

    /// v15 の往復・`rowmode:legacy` の拒否・従来方式のバイト列不変。
    #[test]
    fn v15_migrated_roundtrip_and_strict_decode() {
        let schema = legacy("t").with_row_identity(RowIdentity::MigratedId);
        let bytes = encode_schema(&schema).expect("encode");
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        assert!(text.contains("\nrowmode:migrated\n"), "{text}");
        let back = super::super::decode_schema("t", &bytes).expect("decode");
        assert_eq!(back, schema);
        let bad = text.replace("rowmode:migrated", "rowmode:legacy");
        assert!(super::super::decode_schema("t", bad.as_bytes()).is_err());
        let legacy_bytes = encode_schema(&legacy("t")).expect("encode");
        assert!(!String::from_utf8(legacy_bytes)
            .expect("utf8")
            .contains("rowmode:"));
    }

    /// 旧バイナリ（物理キーを rowid へ切り替える前のコミット）が書いたデータディレクトリの
    /// フィクスチャ（Issue #2202。生成手順は `tests/table23_legacy_fixture_migration.rs`）を開く前に
    /// 直接読み、カタログ値の版が v2〜v14 の幅を持つこと（非空虚ゲート）を確かめる。開いた後は全表が
    /// 移行済みで、スキーマ本体（移行の印を除く）が移行前と同値であること。
    #[test]
    fn pre_rowid_fixture_catalog_versions_and_schemas_survive_migration() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/table23_legacy_pre_rowid.redb"
        );
        let path = unique_db_path("legacy-id-fixture-unit");
        let _g = CleanupGuard(path.clone());
        std::fs::copy(fixture, &path).expect("copy fixture");
        // 開く前の生のカタログ値（版プレフィックス）と、移行前のスキーマ（版を問わない復号）。
        // 同一ファイルを二重に開けないため、読み終えたら redb のハンドルを手放してから `Storage` で開く。
        let mut versions = std::collections::BTreeSet::new();
        let mut before: Vec<(String, TableSchema)> = Vec::new();
        {
            let db = redb::Database::open(&path).expect("open raw");
            let read = db.begin_read().expect("read");
            let table = read.open_table(CATALOG_TABLE).expect("catalog");
            for entry in table.iter().expect("iter") {
                let (k, v) = entry.expect("entry");
                let bytes = v.value().to_vec();
                let head = bytes.split(|b| *b == b'\n').next().unwrap_or(&[]).to_vec();
                versions.insert(String::from_utf8(head).expect("utf8 version"));
                let name = k.value().to_string();
                let mut resolve = |n: &str| get_enum_type_in_read_txn(&read, n);
                let old = decode_schema_with_resolver(&name, &bytes, &mut resolve)
                    .expect("decode old value");
                before.push((name, old));
            }
        }
        // 旧バイナリの表はいずれも従来方式（v15 の印を持たない）。
        assert!(!versions.contains("v15"), "{versions:?}");
        for needed in [
            "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10", "v11", "v12", "v13", "v14",
        ] {
            assert!(
                versions.contains(needed),
                "{needed} missing in {versions:?}"
            );
        }
        eprintln!("fixture catalog versions: {versions:?}");
        assert!(
            before.len() >= 15,
            "fixture has too few tables: {}",
            before.len()
        );
        let s = Storage::open(&path).expect("open migrates");
        for (name, old) in &before {
            let migrated = s.get_table_schema(name).expect("schema after migration");
            assert_eq!(
                migrated.row_identity(),
                RowIdentity::MigratedId,
                "{name} must be migrated"
            );
            // 移行前の値を印だけ差し替えて符号化した結果が、移行後の値と一致する（本体は不変）。
            assert_eq!(old.row_identity(), RowIdentity::LegacyId, "{name}");
            let expected = encode_schema(&old.clone().with_row_identity(RowIdentity::MigratedId))
                .expect("encode expected");
            assert_eq!(raw_catalog_value(&s, name), expected, "{name}");
        }
    }
}
