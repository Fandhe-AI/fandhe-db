//! 表ごとの行セキュリティ有効化状態（`ALTER TABLE ... ENABLE／DISABLE／FORCE／NO FORCE ROW LEVEL
//! SECURITY`）のカタログと Storage 水準の読み書き（RLS-13 のポインタ、Issue #2275。合成規則は
//! `docs/design/row-security-policy-composition.md`）。
//!
//! 役割: `sql::ddl` の `ALTER TABLE` 実行本体が書く「利用者ポリシーを適用するか」の状態だけを永続化する。
//! ポリシー定義の評価・各読み取り経路への組み込み（#2276）、所有者・`BYPASSRLS`・`row_security` の実体
//! （#2278）は本ファイルの担当外で、後続はここの [`RowSecurityState::user_policies_apply`] と
//! `row_security_state_in_txn` を引く想定。テナント境界（`PolicyContext::is_visible`／`ImplicitRlsHook`）は
//! この状態と独立に常に適用され、本状態が外す・付けるのは利用者ポリシーだけ（設計 doc の判断 A。
//! `PolicyContext` には有効化状態を持たせない）。
//!
//! キー設計: `(table_key, scope)`。`scope` は検証済みの `tenant_id`。表キーを先頭に置くのは、表の DROP・
//! RENAME が全テナント分の状態を前方一致の範囲走査だけで処理するため（共有 `public` 表ではテナントごとに
//! 状態が独立する）。テナント境界は全 API が `scope` を必須引数に取り、範囲走査でも突き合わせて守る。
//! 両ビットとも false の行は持たない（行が無い＝既定）。
//!
//! 既存データの不変: [`ROW_SECURITY_TABLE`] は最初の書き込みまで作らない（遅延作成。読み取りは
//! `TableDoesNotExist` を既定状態として扱う）。
//!
//! fail-closed: 未知の版・予約ビット・長さ違いは固定文言の `CorruptSchema` とし、blob の断片・tenant_id を
//! エラーに含めない。

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;
use crate::storage::MAX_TENANT_ID_LEN;

use super::{
    bump_table_generation_in_txn, index_name_exists_in_txn, materialized_view_exists_in_txn,
    validate_relation_key, AlterApply, CatalogError, Result, Storage, CATALOG_TABLE, VIEWS_TABLE,
};

/// 状態の専用カタログ表。キーは `(table_key, scope)`、値は 2 バイトの状態 blob。
pub(crate) const ROW_SECURITY_TABLE: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("table_row_security");

/// 状態 blob の版。
const STATE_VERSION: u8 = 1;
/// flags の bit0: `ENABLE`（PostgreSQL の `relrowsecurity` 相当）。
const FLAG_ENABLED: u8 = 0b01;
/// flags の bit1: `FORCE`（PostgreSQL の `relforcerowsecurity` 相当）。
const FLAG_FORCED: u8 = 0b10;
/// 1 バッチの件数（表の DROP・RENAME の範囲走査。無制限確保をしない）。
const SWEEP_BATCH: usize = 256;

/// `ALTER TABLE` の行セキュリティ切り替えアクション（RLS-13）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSecurityAction {
    /// `ENABLE ROW LEVEL SECURITY`: `enabled = true`（`forced` は変えない）。
    Enable,
    /// `DISABLE ROW LEVEL SECURITY`: `enabled = false`（`forced` は変えない）。
    Disable,
    /// `FORCE ROW LEVEL SECURITY`: `forced = true`（`enabled` は変えない）。
    Force,
    /// `NO FORCE ROW LEVEL SECURITY`: `forced = false`（`enabled` は変えない）。
    NoForce,
}

/// 表 x テナントの有効化状態。PostgreSQL と同じく独立した 2 ビットで持つ（4 値の enum にしない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowSecurityState {
    /// `ENABLE` 済みか。
    pub enabled: bool,
    /// `FORCE` 済みか（`enabled` でなければ誰にも適用されない）。
    pub forced: bool,
}

impl RowSecurityState {
    /// アクションを適用した後の状態。各アクションは自分のビットだけを変える。
    pub fn apply(self, action: RowSecurityAction) -> Self {
        match action {
            RowSecurityAction::Enable => Self {
                enabled: true,
                ..self
            },
            RowSecurityAction::Disable => Self {
                enabled: false,
                ..self
            },
            RowSecurityAction::Force => Self {
                forced: true,
                ..self
            },
            RowSecurityAction::NoForce => Self {
                forced: false,
                ..self
            },
        }
    }

    /// 利用者定義ポリシーを適用するか（`enabled && (!is_owner || forced)`）。
    ///
    /// 後続 Issue（#2276 の評価組み込み）が引く判定。`BYPASSRLS`・`row_security` 設定は #2278 が前段に
    /// 足す。**テナント境界とは独立**で、この関数が false でもテナント境界は常に適用される。
    /// 所有者の根拠は #2278 が決めるため、呼び出し元が `is_owner` を引数で渡す。
    pub fn user_policies_apply(self, is_owner: bool) -> bool {
        self.enabled && (!is_owner || self.forced)
    }

    fn encode(self) -> [u8; 2] {
        let mut flags = 0;
        if self.enabled {
            flags |= FLAG_ENABLED;
        }
        if self.forced {
            flags |= FLAG_FORCED;
        }
        [STATE_VERSION, flags]
    }

    /// untrusted な保存 blob の復号。長さ・版・予約ビットを検証し、不正は固定文言の `CorruptSchema`。
    fn decode(bytes: &[u8]) -> Result<Self> {
        let [version, flags] = <[u8; 2]>::try_from(bytes).map_err(|_| corrupt())?;
        if version != STATE_VERSION || flags & !(FLAG_ENABLED | FLAG_FORCED) != 0 {
            return Err(corrupt());
        }
        Ok(Self {
            enabled: flags & FLAG_ENABLED != 0,
            forced: flags & FLAG_FORCED != 0,
        })
    }

    fn is_default(self) -> bool {
        !self.enabled && !self.forced
    }
}

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid row security catalog entry".to_string())
}

/// `scope`（`tenant_id`）の形状検査。
fn validate_scope(scope: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > usize::from(MAX_TENANT_ID_LEN) {
        return Err(CatalogError::Invalid(
            "invalid row security scope".to_string(),
        ));
    }
    Ok(())
}

/// `(table_key, scope)` の状態を引く（行が無い・表が無ければ既定）。他テナントの状態は構造上引けない。
fn get_in_txn<R: ReadSource>(src: &R, table_key: &str, scope: &str) -> Result<RowSecurityState> {
    let table = match src.open_table(ROW_SECURITY_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(RowSecurityState::default()),
        Err(e) => return Err(e.into()),
    };
    let found = table.get((table_key, scope))?;
    match found {
        Some(v) => RowSecurityState::decode(v.value()),
        None => Ok(RowSecurityState::default()),
    }
}

/// 書き込み・読み取り txn の中で `scope` の表 `table_key` の状態を引く（#2276 以降が同じスナップショットで
/// 引く入口。存在しないカタログ表は作成しない）。
pub(crate) fn row_security_state_in_txn<R: ReadSource>(
    src: &R,
    scope: &str,
    table_key: &str,
) -> Result<RowSecurityState> {
    get_in_txn(src, table_key, scope)
}

/// 全テナントの `table_key` 配下のキーを最大 `limit` 件集める（バッチ走査）。値は検証せず複製だけ行う。
fn collect_table_keys<R: ReadSource>(
    src: &R,
    table_key: &str,
    limit: usize,
) -> Result<Vec<(String, Vec<u8>)>> {
    let table = match src.open_table(ROW_SECURITY_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((table_key, "")..)? {
        let (k, v) = entry?;
        let (t, s) = k.value();
        if t != table_key || out.len() >= limit {
            break;
        }
        out.push((s.to_string(), v.value().to_vec()));
    }
    Ok(out)
}

/// 表 `table_key` の全テナント分の状態を削除する（`DROP TABLE`・`DROP SCHEMA ... CASCADE` の表削除から
/// 同じ write txn で呼ぶ。同名の表を再作成しても旧状態が復活しないようにする）。表が無ければ何もしない。
pub(crate) fn remove_row_security_for_table_in_txn(
    write_txn: &redb::WriteTransaction,
    table_key: &str,
) -> Result<()> {
    loop {
        let batch = collect_table_keys(write_txn, table_key, SWEEP_BATCH)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(ROW_SECURITY_TABLE)?;
        for (scope, _) in &batch {
            table.remove((table_key, scope.as_str()))?;
        }
    }
}

/// 表の改名に追随して全テナント分の状態を新しい表キーへ移す（`ALTER TABLE RENAME` から同じ write txn で
/// 呼ぶ）。改名先に残っていた状態（通常は無い孤児）は先に掃除する。`SET SCHEMA` は表キーが変わらない
/// （共有 `public` から別 SCHEMA への実移動は未実装で拒否される）ため追随不要。
pub(crate) fn rename_row_security_table_in_txn(
    write_txn: &redb::WriteTransaction,
    old_key: &str,
    new_key: &str,
) -> Result<()> {
    remove_row_security_for_table_in_txn(write_txn, new_key)?;
    loop {
        let batch = collect_table_keys(write_txn, old_key, SWEEP_BATCH)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(ROW_SECURITY_TABLE)?;
        for (scope, blob) in &batch {
            table.remove((old_key, scope.as_str()))?;
            table.insert((new_key, scope.as_str()), blob.as_slice())?;
        }
    }
}

impl Storage {
    /// `scope` のテナントとして表 `table_key` の行セキュリティ有効化状態を切り替える。単一 write txn・
    /// 1 回の commit で行い、判定順は次のとおり（すべて commit 前に返るため副作用ゼロ）。
    ///
    /// scope 形状 → SCHEMA の現存 → 対象の種別（ビュー・索引・SEQUENCE・マテリアライズドビューは
    /// `WrongObjectKind`）→ 表の存在（`TableNotFound`）→ 遷移の計算。状態が変わらなければ commit も世代更新も
    /// せず [`AlterApply::Unchanged`]（PostgreSQL も再指定は成功する）。変わったら書き込み、対象表の世代を進めて
    /// [`AlterApply::Applied`]（後続のキャッシュが失効できるようにする）。呼び出し元は `sql::ddl`。
    pub(crate) fn alter_table_row_security(
        &self,
        scope: &str,
        table_key: &str,
        action: RowSecurityAction,
    ) -> Result<AlterApply> {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        // 名前解決後に `DROP SCHEMA` が完了しても、削除済み ID 配下の表へ状態を残さない。
        super::namespace::ensure_tenant_key_schema_alive_in_txn(&*write_txn, scope, table_key)?;
        let in_views = match ReadSource::open_table(&*write_txn, VIEWS_TABLE) {
            Ok(t) => t.get(table_key)?.is_some(),
            Err(redb::TableError::TableDoesNotExist(_)) => false,
            Err(e) => return Err(e.into()),
        };
        if in_views
            || index_name_exists_in_txn(&write_txn, table_key)?
            || materialized_view_exists_in_txn(&write_txn, table_key)?
            || super::sequence::sequence_exists_in_txn(&write_txn, table_key)?
        {
            return Err(CatalogError::WrongObjectKind(table_key.to_string()));
        }
        let table_exists = match ReadSource::open_table(&*write_txn, CATALOG_TABLE) {
            Ok(t) => t.get(table_key)?.is_some(),
            Err(redb::TableError::TableDoesNotExist(_)) => false,
            Err(e) => return Err(e.into()),
        };
        if !table_exists {
            return Err(CatalogError::TableNotFound(table_key.to_string()));
        }
        let current = get_in_txn(&*write_txn, table_key, scope)?;
        let next = current.apply(action);
        if next == current {
            return Ok(AlterApply::Unchanged);
        }
        {
            let mut table = write_txn.open_table(ROW_SECURITY_TABLE)?;
            if next.is_default() {
                table.remove((table_key, scope))?;
            } else {
                table.insert((table_key, scope), next.encode().as_slice())?;
            }
        }
        // commit 記録（SQL-38・Issue #2404）: スキーマ・カタログを変える書き込みは DDL として記録する。
        crate::storage::commit_log::note_ddl(&write_txn, table_key);
        bump_table_generation_in_txn(&write_txn, table_key)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(AlterApply::Applied)
    }

    /// `scope` の表 `table_key` の行セキュリティ有効化状態を返す（読み取り専用。無ければ既定の両方 false）。
    /// 他テナントの状態は構造上引けない。
    pub fn table_row_security(&self, scope: &str, table_key: &str) -> Result<RowSecurityState> {
        validate_scope(scope)?;
        validate_relation_key(table_key)?;
        let rt = self.db().begin_read()?;
        get_in_txn(&rt, table_key, scope)
    }
}

#[cfg(test)]
mod tests;
