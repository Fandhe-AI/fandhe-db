//! SEQUENCE 単位の ACL（`object_kind = 4`）の読み取りと掃除（SQL-40・RLS-12 のポインタ、Issue #2235。
//! 設計記録: `docs/design/role-privilege-model.md`「Issue #2235 実装記録」S2・S6・S12）。
//!
//! 役割: 書き込み（`GRANT`／`REVOKE`）は `apply.rs`、`nextval`／`currval`／`setval`／`lastval` の検査は
//! `tenant.rs` の採番・`sql::sequence_fn` が [`sequence_privileges_in_txn`] を引く。SEQUENCE の定義は
//! 全テナント共有（名前キー）だが ACL 行は `tenant_id` を先頭キーにした範囲だけで読み書きする（他テナントの
//! 行は構造上引けない）。SEQUENCE の削除（`DROP SEQUENCE`・所有表の道連れ・`DROP SCHEMA ... CASCADE`）は同じ
//! write txn で [`remove_sequence_acl_in_txn`] を呼び、残った行が `DROP ROLE` の依存検査
//! （`acl_references_role_in_txn`）とテナントあたりの件数上限を消費し続けないようにする。
//!
//! 暗黙の既定は持たせない（行が無ければ 0。所有者相当・未束縛の主体は呼び出し側が検査を免除する）。
//! 破損行は読み飛ばさず `Err`（fail-closed）。

use crate::storage::read_source::ReadSource;

use super::super::{Result, Storage};
use super::{object_privileges_in_txn, remove_matching, PrivilegeObject, KIND_SEQUENCE};

/// SEQUENCE `sequence_id` について、`role_ids` のいずれか（と PUBLIC）が持つ権限ビットの和を返す
/// （USAGE・SELECT・UPDATE のビット。行が無ければ 0）。
pub(crate) fn sequence_privileges_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    sequence_id: u64,
    role_ids: &[u64],
) -> Result<u16> {
    object_privileges_in_txn(
        src,
        tenant,
        &PrivilegeObject::Sequence(sequence_id),
        role_ids,
    )
}

/// SEQUENCE `sequence_id` の ACL 行を全テナント分削除する（SEQUENCE の定義を消す同一 write txn から呼ぶ。
/// 内部 ID は再利用されないため、他の SEQUENCE の行には触れない）。ACL 表が無ければ何もしない。
/// `RESTART` が呼ぶ状態行の削除には結線しない（定義が残るため ACL も残す）。
pub(crate) fn remove_sequence_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    sequence_id: u64,
) -> Result<()> {
    let key = sequence_id.to_string();
    remove_matching(write_txn, &|kind, k, _| kind == KIND_SEQUENCE && k == key)
}

impl Storage {
    /// 読み取り専用スナップショットで [`sequence_privileges_in_txn`] を引く（`sql::sequence_fn` の `lastval`
    /// の検査が使う）。
    pub(crate) fn sequence_privileges(
        &self,
        tenant: &str,
        sequence_id: u64,
        role_ids: &[u64],
    ) -> Result<u16> {
        use redb::ReadableDatabase;
        let rt = self.db().begin_read()?;
        sequence_privileges_in_txn(&rt, tenant, sequence_id, role_ids)
    }
}

#[cfg(test)]
mod tests;
