//! `GRANT`／`REVOKE` の権限カタログ更新（SQL-40、Issue #1965。設計記録: `docs/design/role-privilege-model.md`
//! §8）。
//!
//! 役割: `sql::grant_ddl` が構造検証済みの文から作る [`AclRequest`] を、単一の write txn で ACL 表
//! （[`super::ACL_TABLE`]）へ原子的に反映する。付与者が持つ GRANT OPTION の範囲だけを付与でき、REVOKE は
//! 付与者自身が付けた行だけを対象にする（PostgreSQL 17 の `aclchk.c` と同じ意味論）。ロール・表・列の存在
//! 検査と権限の判定は、書き込みと同じ txn のスナップショットで行い TOCTOU を作らない。
//!
//! 判定順（決定的・fail-closed。すべて commit 前に返るため副作用ゼロ）: 付与者（束縛ロール）の現存確認 →
//! 対象 relation の解決（`42P01`・種別違い `42809`）→ 付与先ロールの解決（`42704`。他テナントにだけ在る
//! ロールも不在と同じ）→ 列の存在（`42703`）→ 対象ごとの付与権限の判定（`42501`・警告）→ 書き込み。
//!
//! 所有者: ロール所有の機構（#2236）が入るまで、呼び出し元が `owner_equivalent`（DDL 権限ゲートを通過した
//! 主体）を真にした場合に限り、対象の全権限を GRANT OPTION 付きで持つ「所有者相当」として扱う。記録される
//! 付与者は常に現在ロール ID。したがって別の所有者相当の主体が付けた付与は REVOKE できない（所有者の概念が
//! 入るまでの既知の制約）。継承（メンバーシップ経由の権限）は #2232 の担当で、本ファイルは付与者本人の直接の
//! 行と PUBLIC の行だけを見る。
//!
//! 有界性: 対象の総数・CASCADE の反復回数に上限を置き、超過は固定の `LimitExceeded`（`54000`）にする。

use std::collections::HashMap;

use redb::ReadableTable;

use crate::storage::read_source::ReadSource;

use super::super::{
    bump_table_generation_in_txn, index_name_exists_in_txn, materialized_view_exists_in_txn,
    require_table_schema_write_any_identity, CatalogError, Storage, CATALOG_TABLE, VIEWS_TABLE,
};
use super::{
    list_acl_for_object_in_txn, upsert_acl_in_txn, AclEntry, PrivilegeObject, PUBLIC_GRANTEE_ID,
};
use crate::catalog::role::{get_role_by_id_in_txn, get_role_in_txn, validate_tenant};

/// 1 回の GRANT／REVOKE が走査する対象（表・列）の総数の上限（実装既定値）。
const MAX_TARGET_VISITS: usize = 20_000;

/// CASCADE の連鎖で処理する付与者の最大数（実装既定値）。
const MAX_CASCADE_STEPS: usize = 10_000;

/// 付与先の指定（解決は書き込みと同じ txn の中で行う）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GranteeSpec {
    /// `PUBLIC`。
    Public,
    /// 解決済みのロール ID（`CURRENT_USER`・`SESSION_USER`。現存は txn 内で確かめる）。
    Id(u64),
    /// ロール名（自テナント内だけを引く）。
    Name(String),
}

/// 権限 1 項目。`columns` が空なら表単位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AclItem {
    pub(crate) bits: u16,
    pub(crate) columns: Vec<String>,
}

/// [`Storage::apply_acl_changes`] への要求。
#[derive(Debug, Clone)]
pub(crate) struct AclRequest<'a> {
    pub(crate) is_grant: bool,
    /// 付与者（現在ロール）の ID。0 は不可。
    pub(crate) grantor_id: u64,
    /// 所有者相当（DDL 権限ゲート通過済み）か。
    pub(crate) owner_equivalent: bool,
    pub(crate) tables: &'a [String],
    pub(crate) items: &'a [AclItem],
    pub(crate) grantees: &'a [GranteeSpec],
    /// `WITH GRANT OPTION`（GRANT）／`GRANT OPTION FOR`（REVOKE）。
    pub(crate) grant_option: bool,
    pub(crate) cascade: bool,
}

/// PostgreSQL の警告相当（wire には出さない。#2836 のポリシー DDL と同じ扱い）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AclWarning {
    /// 付与・取り消しできる権限が 1 つも無かった（`01007` 相当）。
    NoPrivileges,
    /// 一部の権限だけが付与・取り消された（`01006` 相当）。
    NotAllPrivileges,
}

/// [`Storage::apply_acl_changes`] の成功結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct AclApplyOutcome {
    pub(crate) warning: Option<AclWarning>,
    /// 書き込んだ（追加・更新・削除した）ACL 行の数。
    pub(crate) rows_changed: usize,
}

/// 失敗の分類。`sql::grant_ddl` が SQLSTATE へ写す。文言に他テナントの情報・内部キーを含めない。
#[derive(Debug)]
pub(crate) enum AclChangeError {
    Catalog(CatalogError),
    /// 付与先ロールが自テナントに無い（他テナントにだけ在る場合を含む）。利用者が書いた名前。
    GranteeNotFound(String),
    /// 列が無い（利用者が書いた名前）。
    ColumnNotFound(String),
    /// 付与者が未束縛・不在、または対象に何の権限も持たない（`42501`）。
    PermissionDenied,
    /// RESTRICT で依存する付与が残る（`2BP01`）。
    DependentPrivileges,
    /// 不正な付与（`0LP01`）。固定文言。
    InvalidGrant(&'static str),
    /// 未対応の形（`0A000`）。固定文言。
    Unsupported(&'static str),
    /// 対象数・連鎖数の上限超過（`54000`）。
    LimitExceeded,
}

impl From<CatalogError> for AclChangeError {
    fn from(e: CatalogError) -> Self {
        Self::Catalog(e)
    }
}

type Result<T> = std::result::Result<T, AclChangeError>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RelKind {
    Table,
    View,
    Matview,
}

/// relation の種別を引く。無ければ `TableNotFound`、索引・SEQUENCE は `WrongObjectKind`。
fn relation_kind(write_txn: &redb::WriteTransaction, tenant: &str, key: &str) -> Result<RelKind> {
    crate::catalog::namespace::ensure_tenant_key_schema_alive_in_txn(write_txn, tenant, key)?;
    let in_views = match ReadSource::open_table(write_txn, VIEWS_TABLE) {
        Ok(t) => t.get(key).map_err(CatalogError::from)?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(CatalogError::from(e).into()),
    };
    if in_views {
        return Ok(RelKind::View);
    }
    if materialized_view_exists_in_txn(write_txn, key)? {
        return Ok(RelKind::Matview);
    }
    if index_name_exists_in_txn(write_txn, key)?
        || crate::catalog::sequence::sequence_exists_in_txn(write_txn, key)?
    {
        return Err(CatalogError::WrongObjectKind(key.to_string()).into());
    }
    let exists = match ReadSource::open_table(write_txn, CATALOG_TABLE) {
        Ok(t) => t.get(key).map_err(CatalogError::from)?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(CatalogError::from(e).into()),
    };
    if exists {
        Ok(RelKind::Table)
    } else {
        Err(CatalogError::TableNotFound(key.to_string()).into())
    }
}

/// 更新対象の 1 オブジェクトと、その要求ビット。
struct Target {
    object: PrivilegeObject,
    bits: u16,
    /// 表単位 REVOKE が列の行へ波及させたもの（付与権限の判定・警告の対象外）。
    propagated: bool,
}

fn target_key(object: &PrivilegeObject) -> String {
    match object {
        PrivilegeObject::Table(t) => format!("T\0{t}"),
        PrivilegeObject::Column { table, column } => format!("C\0{table}\0{column}"),
        _ => String::new(),
    }
}

fn add_target(
    targets: &mut Vec<Target>,
    index: &mut HashMap<String, usize>,
    object: PrivilegeObject,
    bits: u16,
    propagated: bool,
) -> Result<()> {
    let key = target_key(&object);
    if let Some(&i) = index.get(&key) {
        if let Some(t) = targets.get_mut(i) {
            t.bits |= bits;
            t.propagated &= propagated;
        }
        return Ok(());
    }
    if targets.len() >= MAX_TARGET_VISITS {
        return Err(AclChangeError::LimitExceeded);
    }
    index.insert(key, targets.len());
    targets.push(Target {
        object,
        bits,
        propagated,
    });
    Ok(())
}

/// 付与者が対象に対して持つ（直接の行の）GRANT OPTION の集合と、何らかの権限（直接・PUBLIC）の有無。
fn grantor_rights(entries: &[AclEntry], grantor: u64) -> (u16, u16) {
    let mut go = 0u16;
    let mut any = 0u16;
    for e in entries {
        if e.grantee_id == grantor {
            go |= e.grant_options;
            any |= e.privileges;
        } else if e.grantee_id == PUBLIC_GRANTEE_ID {
            any |= e.privileges;
        }
    }
    (go, any)
}

/// `grantor` が権限ビット `bits` について持つ GRANT OPTION の出所（上流の付与者）の集合。循環の検出に使う
/// （有界・訪問済み集合付き）。付与する権限ビットと重なる GRANT OPTION の経路だけを辿り、別の権限の
/// 経路を同じ上流とみなさない。
fn ancestors_of(entries: &[AclEntry], grantor: u64, bits: u16) -> Result<Vec<u64>> {
    let mut seen: Vec<u64> = Vec::new();
    let mut stack = vec![grantor];
    let mut steps = 0usize;
    while let Some(g) = stack.pop() {
        steps += 1;
        if steps > MAX_CASCADE_STEPS {
            return Err(AclChangeError::LimitExceeded);
        }
        for e in entries {
            if e.grantee_id == g && e.grant_options & bits != 0 && !seen.contains(&e.grantor_id) {
                seen.push(e.grantor_id);
                stack.push(e.grantor_id);
            }
        }
    }
    Ok(seen)
}

/// 1 オブジェクトの ACL 行の作業状態（`before` は txn スナップショット、`work` は更新後）。
struct AclState {
    object: PrivilegeObject,
    before: Vec<AclEntry>,
    work: Vec<AclEntry>,
}

/// 作業状態の集合（挿入順を保つ。表と列の依存解決で相互参照するため、1 箇所で持つ）。
#[derive(Default)]
struct AclStates {
    order: Vec<AclState>,
    index: HashMap<String, usize>,
}

impl AclStates {
    /// `object` の作業状態の位置を返す（無ければ txn から読み込む）。
    fn idx(
        &mut self,
        write_txn: &redb::WriteTransaction,
        tenant: &str,
        object: &PrivilegeObject,
    ) -> Result<usize> {
        let key = target_key(object);
        if let Some(&i) = self.index.get(&key) {
            return Ok(i);
        }
        if self.order.len() >= MAX_TARGET_VISITS {
            return Err(AclChangeError::LimitExceeded);
        }
        let before = list_acl_for_object_in_txn(write_txn, tenant, object)?;
        self.index.insert(key, self.order.len());
        self.order.push(AclState {
            object: object.clone(),
            work: before.clone(),
            before,
        });
        Ok(self.order.len() - 1)
    }

    fn work_of(&self, i: usize) -> &[AclEntry] {
        self.order.get(i).map_or(&[][..], |s| s.work.as_slice())
    }
}

/// `x` が `entries` で持つ GRANT OPTION ビットの和。
fn go_of(entries: &[AclEntry], x: u64) -> u16 {
    entries
        .iter()
        // 自己付与の行（付与者 == 付与先）は独立した出所として数えない（循環による REVOKE 回避の防止）。
        .filter(|e| e.grantee_id == x && e.grantor_id != x)
        .fold(0u16, |a, e| a | e.grant_options)
}

/// 表の列名（表でない・スキーマが無ければ空）。
fn table_column_names(write_txn: &redb::WriteTransaction, table: &str) -> Result<Vec<String>> {
    match require_table_schema_write_any_identity(write_txn, table) {
        Ok(s) => Ok(s.columns.iter().map(|c| c.name.clone()).collect()),
        Err(CatalogError::TableNotFound(_)) => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// REVOKE の後始末。GRANT OPTION を失った `(付与先 x, 範囲)` について、`x` が付けた付与のうち `x` の残りの
/// GRANT OPTION で賄えない権限は依存する付与として、RESTRICT なら `2BP01`、CASCADE なら連鎖して取り消す。
/// 範囲が表なら表の行と全列の行を、列ならその列の行を見る。列の付与は列自身の GRANT OPTION に加えて
/// 表単位の GRANT OPTION（更新後の表の行）でも賄える。
fn resolve_dependents(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    states: &mut AclStates,
    start: Vec<(u64, PrivilegeObject)>,
    cascade: bool,
) -> Result<()> {
    let mut queue = start;
    let mut steps = 0usize;
    while let Some((x, scope)) = queue.pop() {
        steps += 1;
        if steps > MAX_CASCADE_STEPS {
            return Err(AclChangeError::LimitExceeded);
        }
        let mut objs: Vec<PrivilegeObject> = Vec::new();
        match &scope {
            PrivilegeObject::Table(t) => {
                objs.push(scope.clone());
                for c in table_column_names(write_txn, t)? {
                    objs.push(PrivilegeObject::Column {
                        table: t.clone(),
                        column: c,
                    });
                }
            }
            PrivilegeObject::Column { .. } => objs.push(scope.clone()),
            _ => continue,
        }
        for obj in objs {
            let i = states.idx(write_txn, tenant, &obj)?;
            let mut covered = go_of(states.work_of(i), x);
            if let PrivilegeObject::Column { table, .. } = &obj {
                let ti = states.idx(write_txn, tenant, &PrivilegeObject::Table(table.clone()))?;
                covered |= go_of(states.work_of(ti), x);
            }
            let Some(state) = states.order.get_mut(i) else {
                continue;
            };
            for e in state.work.iter_mut() {
                if e.grantor_id != x {
                    continue;
                }
                let uncovered = e.privileges & !covered;
                if uncovered == 0 {
                    continue;
                }
                if !cascade {
                    return Err(AclChangeError::DependentPrivileges);
                }
                let old_go = e.grant_options;
                e.privileges &= !uncovered;
                e.grant_options &= e.privileges;
                if e.grant_options != old_go {
                    queue.push((e.grantee_id, obj.clone()));
                }
            }
        }
    }
    Ok(())
}

fn merge_warning(cur: &mut Option<AclWarning>, new: AclWarning) {
    *cur = match (*cur, new) {
        (Some(AclWarning::NoPrivileges), _) | (_, AclWarning::NoPrivileges) => {
            Some(AclWarning::NoPrivileges)
        }
        _ => Some(AclWarning::NotAllPrivileges),
    };
}

/// 書き込みの計画 1 件（`privileges` が 0 なら行の削除）。
struct Change {
    object: PrivilegeObject,
    entry: AclEntry,
}

/// 更新後の行を、更新前（`before`）との差分だけ `changes` へ積む。
fn diff_into(
    changes: &mut Vec<Change>,
    object: &PrivilegeObject,
    before: &[AclEntry],
    after: &[AclEntry],
) {
    for a in after {
        let same = before
            .iter()
            .any(|b| b.grantee_id == a.grantee_id && b.grantor_id == a.grantor_id && b == a);
        if !same {
            changes.push(Change {
                object: object.clone(),
                entry: *a,
            });
        }
    }
}

/// 表単位の付与権限の判定で、表の列の行に何らかの権限があるか（PG は列の権限も「何らかの権限」に数える）。
fn has_any_column_privilege(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    grantor: u64,
) -> Result<bool> {
    // ビュー・matview には列の ACL を作れない（`0A000` で拒否済み）ため、スキーマが無ければ「無い」。
    let schema = match require_table_schema_write_any_identity(write_txn, table) {
        Ok(s) => s,
        Err(CatalogError::TableNotFound(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for c in &schema.columns {
        let obj = PrivilegeObject::Column {
            table: table.to_string(),
            column: c.name.clone(),
        };
        let entries = list_acl_for_object_in_txn(write_txn, tenant, &obj)?;
        if grantor_rights(&entries, grantor).1 != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Storage {
    /// `tenant` の ACL へ GRANT／REVOKE を単一 write txn・1 回の commit で反映する。
    ///
    /// 呼び出し元は `sql::grant_ddl::execute_grant_revoke`（DDL 権限・現在ロール束縛のゲートの後）。成功時、
    /// 実際に行を書いた relation の世代を進め（`USING PLAN` の世代照合・準備済み文の失効）、DDL として
    /// commit 記録へ残す。書き込みが 0 件ならそのまま破棄する。エラーはすべて commit 前に返る。
    pub(crate) fn apply_acl_changes(
        &self,
        tenant: &str,
        req: &AclRequest<'_>,
    ) -> Result<AclApplyOutcome> {
        validate_tenant(tenant)?;
        if req.grantor_id == 0 {
            return Err(AclChangeError::PermissionDenied);
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)?;
        // 束縛ロールの現存（DROP 済みの古い束縛で付与しない）。
        if get_role_by_id_in_txn(&*write_txn, tenant, req.grantor_id)?.is_none() {
            return Err(AclChangeError::PermissionDenied);
        }
        // 1. 対象 relation。
        let mut kinds: Vec<(String, RelKind)> = Vec::new();
        for t in req.tables {
            let k = relation_kind(&write_txn, tenant, t)?;
            kinds.push((t.clone(), k));
        }
        // 2. 付与先。
        let mut grantee_ids: Vec<u64> = Vec::new();
        for g in req.grantees {
            let id = match g {
                GranteeSpec::Public => PUBLIC_GRANTEE_ID,
                GranteeSpec::Id(id) => {
                    if get_role_by_id_in_txn(&*write_txn, tenant, *id)?.is_none() {
                        return Err(AclChangeError::PermissionDenied);
                    }
                    *id
                }
                GranteeSpec::Name(n) => get_role_in_txn(&*write_txn, tenant, n)?
                    .map(|r| r.id)
                    .ok_or_else(|| AclChangeError::GranteeNotFound(n.clone()))?,
            };
            if !grantee_ids.contains(&id) {
                grantee_ids.push(id);
            }
        }
        if req.is_grant && req.grant_option && grantee_ids.contains(&PUBLIC_GRANTEE_ID) {
            return Err(AclChangeError::InvalidGrant(
                "grant options can only be granted to roles",
            ));
        }
        // 3. 列の存在と対象の組み立て。
        let mut targets: Vec<Target> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (table, kind) in &kinds {
            let has_columns = req.items.iter().any(|i| !i.columns.is_empty());
            let schema = if *kind == RelKind::Table && (has_columns || !req.is_grant) {
                Some(require_table_schema_write_any_identity(&write_txn, table)?)
            } else {
                None
            };
            if has_columns && *kind != RelKind::Table {
                return Err(AclChangeError::Unsupported(
                    "column privileges on views are not supported yet",
                ));
            }
            if let Some(schema) = &schema {
                for item in req.items {
                    for c in &item.columns {
                        if !schema.columns.iter().any(|d| d.name == *c) {
                            return Err(AclChangeError::ColumnNotFound(c.clone()));
                        }
                    }
                }
            }
            for item in req.items {
                if item.columns.is_empty() {
                    add_target(
                        &mut targets,
                        &mut index,
                        PrivilegeObject::Table(table.clone()),
                        item.bits,
                        false,
                    )?;
                    // 表単位の REVOKE は同じ権限の列単位の行からも落とす（PostgreSQL と同じ）。
                    if !req.is_grant {
                        if let Some(schema) = &schema {
                            for c in &schema.columns {
                                add_target(
                                    &mut targets,
                                    &mut index,
                                    PrivilegeObject::Column {
                                        table: table.clone(),
                                        column: c.name.clone(),
                                    },
                                    item.bits,
                                    true,
                                )?;
                            }
                        }
                    }
                } else {
                    for c in &item.columns {
                        add_target(
                            &mut targets,
                            &mut index,
                            PrivilegeObject::Column {
                                table: table.clone(),
                                column: c.clone(),
                            },
                            item.bits,
                            false,
                        )?;
                    }
                }
            }
        }
        // 4. 対象ごとの判定と計画。
        let mut warning: Option<AclWarning> = None;
        let mut states = AclStates::default();
        let mut affected: Vec<(u64, PrivilegeObject)> = Vec::new();
        for t in &targets {
            let allowed = t.object.allowed_privileges();
            let want = t.bits & allowed;
            if want == 0 && t.propagated {
                continue;
            }
            let si = states.idx(&write_txn, tenant, &t.object)?;
            let entries: Vec<AclEntry> = states.work_of(si).to_vec();
            // 付与者が持つ GRANT OPTION。列は表単位の行も合わせて見る。
            let eff = if req.owner_equivalent {
                want
            } else if t.propagated {
                // 表単位 REVOKE から派生した列ターゲット。表単位の対象と同じく、付与者の表の
                // GRANT OPTION で絞る（0 なら表側が警告のみで無変更のため列も変更しない）。
                if let PrivilegeObject::Column { table, .. } = &t.object {
                    let table_entries = list_acl_for_object_in_txn(
                        &*write_txn,
                        tenant,
                        &PrivilegeObject::Table(table.clone()),
                    )?;
                    let (tgo, _) = grantor_rights(&table_entries, req.grantor_id);
                    want & tgo
                } else {
                    want
                }
            } else {
                let (mut go, mut any) = grantor_rights(&entries, req.grantor_id);
                if let PrivilegeObject::Column { table, .. } = &t.object {
                    let table_entries = list_acl_for_object_in_txn(
                        &*write_txn,
                        tenant,
                        &PrivilegeObject::Table(table.clone()),
                    )?;
                    let (tgo, tany) = grantor_rights(&table_entries, req.grantor_id);
                    go |= tgo & allowed;
                    any |= tany & allowed;
                }
                if go == 0 {
                    let mut has_any = any != 0;
                    if !has_any {
                        if let PrivilegeObject::Table(table) = &t.object {
                            has_any = has_any_column_privilege(
                                &write_txn,
                                tenant,
                                table,
                                req.grantor_id,
                            )?;
                        }
                    }
                    if !has_any {
                        return Err(AclChangeError::PermissionDenied);
                    }
                }
                want & go
            };
            if !t.propagated {
                if eff == 0 {
                    merge_warning(&mut warning, AclWarning::NoPrivileges);
                } else if eff != want && want != allowed {
                    merge_warning(&mut warning, AclWarning::NotAllPrivileges);
                }
            }
            if eff == 0 {
                continue;
            }
            let mut work: Vec<AclEntry> = entries.clone();
            if req.is_grant {
                if req.grant_option && !req.owner_equivalent {
                    // 自己への GRANT OPTION 付与は循環として拒否する。
                    if grantee_ids.contains(&req.grantor_id) {
                        return Err(AclChangeError::InvalidGrant(
                            "grant options cannot be granted back to your own grantor",
                        ));
                    }
                    // 列の付与は、同じ権限ビットの表単位の GRANT OPTION の経路（上流の付与者）も辿る
                    // （列 ACL が空でも表単位の経路で上流へ戻る付与を循環として拒否する）。
                    let mut path_entries: Vec<AclEntry> = entries.clone();
                    if let PrivilegeObject::Column { table, .. } = &t.object {
                        path_entries.extend(list_acl_for_object_in_txn(
                            &*write_txn,
                            tenant,
                            &PrivilegeObject::Table(table.clone()),
                        )?);
                    }
                    // 権限ビットごとに経路を辿る（別の権限の辺をつないで架空の循環を作らない）。
                    for bit in (0..16).map(|i| 1u16 << i).filter(|b| eff & b != 0) {
                        let anc = ancestors_of(&path_entries, req.grantor_id, bit)?;
                        if grantee_ids.iter().any(|g| anc.contains(g)) {
                            return Err(AclChangeError::InvalidGrant(
                                "grant options cannot be granted back to your own grantor",
                            ));
                        }
                    }
                }
                for g in &grantee_ids {
                    if let Some(e) = work
                        .iter_mut()
                        .find(|e| e.grantee_id == *g && e.grantor_id == req.grantor_id)
                    {
                        e.privileges |= eff;
                        if req.grant_option {
                            e.grant_options |= eff;
                        }
                    } else {
                        work.push(AclEntry {
                            grantee_id: *g,
                            grantor_id: req.grantor_id,
                            privileges: eff,
                            grant_options: if req.grant_option { eff } else { 0 },
                        });
                    }
                }
            } else {
                for g in &grantee_ids {
                    if let Some(e) = work
                        .iter_mut()
                        .find(|e| e.grantee_id == *g && e.grantor_id == req.grantor_id)
                    {
                        let old_go = e.grant_options;
                        if req.grant_option {
                            e.grant_options &= !eff;
                        } else {
                            e.privileges &= !eff;
                            e.grant_options &= e.privileges;
                        }
                        if e.grant_options != old_go
                            && !affected.iter().any(|(a, o)| a == g && *o == t.object)
                        {
                            affected.push((*g, t.object.clone()));
                        }
                    }
                }
            }
            if let Some(st) = states.order.get_mut(si) {
                st.work = work;
            }
        }
        // 失った GRANT OPTION に依存する付与を、表・列をまたいで解決する（全対象の直接更新の後）。
        if !req.is_grant {
            resolve_dependents(&write_txn, tenant, &mut states, affected, req.cascade)?;
        }
        let mut changes: Vec<Change> = Vec::new();
        for st in &states.order {
            diff_into(&mut changes, &st.object, &st.before, &st.work);
        }
        // 5. 書き込み。
        if changes.is_empty() {
            return Ok(AclApplyOutcome {
                warning,
                rows_changed: 0,
            });
        }
        for c in &changes {
            upsert_acl_in_txn(
                &write_txn,
                tenant,
                &c.object,
                c.entry.grantee_id,
                c.entry.grantor_id,
                c.entry.privileges,
                c.entry.grant_options,
            )?;
        }
        let mut touched: Vec<String> = Vec::new();
        for c in &changes {
            let table = match &c.object {
                PrivilegeObject::Table(t) => t,
                PrivilegeObject::Column { table, .. } => table,
                _ => continue,
            };
            if !touched.contains(table) {
                touched.push(table.clone());
            }
        }
        let rows_changed = changes.len();
        commit_with_bumps(write_txn, &touched)?;
        Ok(AclApplyOutcome {
            warning,
            rows_changed,
        })
    }
}

/// 触れた relation の世代を進めて DDL として記録し、commit する（`USING PLAN` の世代照合・commit 記録の契約。
/// `tests/table_rls/table_generation_bump_coverage.rs` が commit 直前の bump を検査する）。
fn commit_with_bumps(write_txn: crate::storage::GatedWriteTxn, tables: &[String]) -> Result<()> {
    for t in tables {
        bump_table_generation_in_txn(&write_txn, t)?;
        crate::storage::commit_log::note_ddl(&write_txn, t);
    }
    crate::recovery::commit_boundary::commit(write_txn)
        .map_err(crate::catalog::convert_storage_error)?;
    Ok(())
}
