//! 書き込み系の文（INSERT／UPDATE／DELETE／UPSERT／TRUNCATE とそのビュー・データ変更 CTE・
//! `EXPLAIN ... PARTITIONED`）の表単位権限検査（SQL-40・RLS-12・SQL-19・SQL-22 ポインタ。Issue #1968。
//! 設計は `docs/design/role-privilege-model.md` §13、経路一覧は `docs/design/privilege-check-paths.md`）。
//!
//! 役割: 検証済みの文から「書き込み先 relation とその必要ビット」を分類し（[`classify_write`]）、文が参照する
//! 読み取り側の relation（INSERT ... SELECT の元・UPDATE ... FROM・サブクエリ等）に SELECT を課して、
//! 共通ゲート [`super::check`] へ渡す。判定そのものは `privilege_gate.rs`、ACL 照会は `read.rs` の
//! [`super::read::require_relations_bits`] に一本化し、本ファイルは「何を・どの権限で」だけを決める。
//!
//! 呼び出し元: `core::EngineCore::parse_tokens_gated`（構文解析成功後。簡易クエリ・明示 Tx・PL/pgSQL の
//! 埋め込み SQL）、`core/temp_route` の拡張クエリ Parse・Bind・Describe、分割実行 DML のチャンクごとの再判定
//! （[`RelationPrivilegeRecheck`]）と TRUNCATE の CASCADE 閉包の再判定。
//!
//! 契約:
//! - 現在ロール未束縛、または運用者権限のセッションは、トークン走査も ACL の照会もしない（従来と同一）。
//! - 判定は表単位。列単位の付与だけでは表単位の要求を満たさない（暫定の fail-closed。#2081）。
//! - 判別できない式（SET 右辺の式・ON CONFLICT 句）は SELECT を要求する側（厳しい側）に倒す。
//! - ビューへの DML はビュー自身の権限だけを判定する（基底表はビュー所有者の権限で書く扱い）。
//! - `SHOW`／`CANCEL PARTITIONED DML` はジョブ表だけを触り利用者の行に触れないので対象外。一時表だけを
//!   対象にする文はセッションの所有物なので対象外。`COPY` は `copy.rs`（#1969）、`LOCK` は対象外。
//! - エラーは固定文言の `42501`。表名・ロール ID・テナントを含めない。

use crate::catalog::privilege::{
    relation_grant_bits_in_txn, PRIV_DELETE, PRIV_INSERT, PRIV_SELECT, PRIV_TRUNCATE, PRIV_UPDATE,
};
use crate::core::ParsedSql;
use crate::sql::allowlist::{
    ConflictTargetSpec, DeleteStatement, OnConflictAction, OnConflictClause, SetValue,
    SqlSurfaceError, ValidatedInsert, ValidatedUpdateForm,
};
use crate::sql::cte::DmlStatement;
use crate::sql::lexer::Token;
use crate::sql::mode::SessionState;
use crate::sql::relation_name::SchemaResolver;
use crate::storage::read_source::ReadSource;
use crate::storage::Storage;

use super::read::require_relations_bits;
use super::{check, Principal, PrivilegeTarget};

/// 文の書き込み側の要求。`targets` は書き込み先 relation（キー）と必要ビット。文が参照する他の relation
/// には SELECT を課す（トークン走査で得た参照から `targets` を 1 回ずつ差し引いた残り）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct WriteRequirement {
    pub(crate) targets: Vec<(String, u16)>,
}

impl WriteRequirement {
    fn one(table: &str, bits: u16) -> Self {
        Self {
            targets: vec![(table.to_string(), bits)],
        }
    }

    /// 本ゲートの対象外（書き込み先を持たない）か。
    pub(crate) fn is_not_applicable(&self) -> bool {
        self.targets.is_empty()
    }

    /// 全 target のビットの和（ACL 非依存の主体の一括判定に使う）。
    fn all_bits(&self) -> u16 {
        self.targets.iter().fold(0, |a, (_, b)| a | b)
    }
}

/// `ON CONFLICT` 句が足す必要ビット。`DO UPDATE` は UPDATE と、衝突対象・SET 式・条件が読む列のための
/// SELECT。`DO NOTHING` は衝突対象を明示したときだけ推論に使う列のため SELECT を足す（厳しい側）。
fn on_conflict_bits(oc: Option<&OnConflictClause>) -> u16 {
    match oc {
        None => 0,
        Some(c) => match &c.action {
            OnConflictAction::DoUpdate { .. } => PRIV_UPDATE | PRIV_SELECT,
            OnConflictAction::DoNothing => match c.target {
                ConflictTargetSpec::Omitted => 0,
                ConflictTargetSpec::Columns(_) | ConflictTargetSpec::Constraint(_) => PRIV_SELECT,
            },
        },
    }
}

fn insert_bits(v: &ValidatedInsert) -> u16 {
    let mut b = PRIV_INSERT | on_conflict_bits(v.on_conflict.as_ref());
    if v.returning.is_some() {
        b |= PRIV_SELECT;
    }
    b
}

/// SET 右辺が行の列を読まない（リテラル・DEFAULT）か。式は列を読みうるので SELECT 側に倒す。
fn sets_are_literal(assignments: &[(String, SetValue)]) -> bool {
    assignments
        .iter()
        .all(|(_, v)| matches!(v, SetValue::Literal(_) | SetValue::Default))
}

fn update_form_bits(u: &ValidatedUpdateForm) -> (String, u16) {
    match u {
        // `WHERE id = ...` は列を読む。
        ValidatedUpdateForm::Single(v) => (v.table_name.clone(), PRIV_UPDATE | PRIV_SELECT),
        ValidatedUpdateForm::Predicate(v) => {
            let mut b = PRIV_UPDATE;
            if !v.where_predicates.is_empty()
                || v.returning.is_some()
                || !sets_are_literal(&v.assignments)
            {
                b |= PRIV_SELECT;
            }
            (v.table_name.clone(), b)
        }
    }
}

fn delete_form_bits(d: &DeleteStatement) -> (String, u16) {
    match d {
        DeleteStatement::SingleRow(v) => (v.table_name.clone(), PRIV_DELETE | PRIV_SELECT),
        DeleteStatement::Predicate(v) => {
            let mut b = PRIV_DELETE;
            if !v.where_predicates.is_empty() || v.returning.is_some() {
                b |= PRIV_SELECT;
            }
            (v.table_name.clone(), b)
        }
    }
}

fn dml_statement_bits(s: &DmlStatement) -> (String, u16) {
    match s {
        DmlStatement::Insert(v) => (v.table_name.clone(), insert_bits(v)),
        DmlStatement::Update(u) => update_form_bits(u),
        DmlStatement::Delete(d) => delete_form_bits(d),
    }
}

/// 述語形 `UPDATE ... PARTITIONED` の必要ビット（チャンクごとの再判定が使う。文の入口の判定と同じ値）。
pub(crate) fn partitioned_update_bits(u: &crate::sql::allowlist::ValidatedPredicateUpdate) -> u16 {
    update_form_bits(&ValidatedUpdateForm::Predicate(u.clone())).1
}

/// 述語形 `DELETE ... PARTITIONED` の必要ビット。[`partitioned_update_bits`] と同じ位置付け。
pub(crate) fn partitioned_delete_bits(d: &crate::sql::allowlist::ValidatedPredicateDelete) -> u16 {
    delete_form_bits(&DeleteStatement::Predicate(d.clone())).1
}

/// 検証済みの文を書き込みの要求に分類する。ワイルドカード腕を置かない網羅 `match` で、`ParsedSql` に
/// variant が足されたときに分類の更新（書き込みか否か）をコンパイラが強制する。
pub(crate) fn classify_write(parsed: &ParsedSql) -> WriteRequirement {
    use crate::sql::partitioned::PartitionedControl;
    let na = WriteRequirement::default;
    match parsed {
        ParsedSql::Insert(v) => WriteRequirement::one(&v.table_name, insert_bits(v)),
        ParsedSql::InsertSelect(v) => {
            let mut b = PRIV_INSERT | on_conflict_bits(v.on_conflict.as_ref());
            if v.returning.is_some() {
                b |= PRIV_SELECT;
            }
            WriteRequirement::one(&v.table_name, b)
        }
        ParsedSql::Update(u) => {
            let (t, b) = update_form_bits(u);
            WriteRequirement::one(&t, b)
        }
        ParsedSql::Delete(d) => {
            let (t, b) = delete_form_bits(d);
            WriteRequirement::one(&t, b)
        }
        ParsedSql::UpdateFrom(v) => {
            WriteRequirement::one(v.target.table(), PRIV_UPDATE | PRIV_SELECT)
        }
        ParsedSql::DeleteUsing(v) => {
            WriteRequirement::one(v.target.table(), PRIV_DELETE | PRIV_SELECT)
        }
        ParsedSql::Truncate(v) => WriteRequirement {
            targets: v
                .table_names
                .iter()
                .map(|t| (t.clone(), PRIV_TRUNCATE))
                .collect(),
        },
        ParsedSql::ViewInsert(v) => WriteRequirement::one(v.view_name(), insert_bits(&v.shape)),
        ParsedSql::ViewUpdate(v) => {
            let (t, b) = update_form_bits(&v.shape);
            WriteRequirement::one(&t, b)
        }
        ParsedSql::ViewDelete(v) => {
            let (t, b) = delete_form_bits(&v.shape);
            WriteRequirement::one(&t, b)
        }
        ParsedSql::DataModifyingCte(v) => WriteRequirement {
            targets: v
                .dm_ctes
                .iter()
                .map(|d| dml_statement_bits(&d.statement))
                .collect(),
        },
        ParsedSql::Partitioned(c) => match c {
            PartitionedControl::ExplainDelete(d) => {
                let (t, b) = delete_form_bits(&DeleteStatement::Predicate(d.clone()));
                WriteRequirement::one(&t, b)
            }
            PartitionedControl::ExplainUpdate(u) => {
                let (t, b) = update_form_bits(&ValidatedUpdateForm::Predicate(u.clone()));
                WriteRequirement::one(&t, b)
            }
            // ジョブ表だけを読み書きし利用者の行に触れない（ジョブのロール単位の所有はスコープ外）。
            PartitionedControl::Show(_) | PartitionedControl::Cancel(_) => na(),
        },
        // 一時表だけを対象にする文はセッションの所有物。永続側はセッション経由でも同じ検査を受ける。
        ParsedSql::SessionRouted(r) => {
            if r.is_persistent() {
                classify_write(r.inner())
            } else {
                na()
            }
        }
        // 読み取り・カーソル・行ロック（read ゲートが担う）。COPY は `copy.rs`（#1969。begin_copy* が直接呼ぶ）、LOCK TABLE は対象外。
        ParsedSql::Statement(_)
        | ParsedSql::LockingSelect(_)
        | ParsedSql::Cursor(_)
        | ParsedSql::Copy(_)
        | ParsedSql::Lock(_)
        // トランザクション・セッション制御・DDL・関数呼び出し（DDL は運用者権限ゲートが担う）。
        | ParsedSql::Transaction(_)
        | ParsedSql::SetTransaction(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::CreateTable(_)
        | ParsedSql::CreateTempTable(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateView(_)
        | ParsedSql::DropView(_)
        | ParsedSql::DropMaterializedView(_)
        | ParsedSql::CreateMaterializedView(_)
        | ParsedSql::CreateTableAs(_)
        | ParsedSql::RefreshMaterializedView(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_)
        | ParsedSql::CreateType(_)
        | ParsedSql::DropType(_)
        | ParsedSql::CreateSequence(_)
        | ParsedSql::DropSequence(_)
        | ParsedSql::DropFunction(_)
        | ParsedSql::CreateTrigger(_)
        | ParsedSql::DropTrigger(_)
        | ParsedSql::CreatePolicy(_)
        | ParsedSql::AlterPolicy(_)
        | ParsedSql::DropPolicy(_)
        | ParsedSql::GrantRevoke(_)
        | ParsedSql::CreateRole(_)
        | ParsedSql::DropRole(_)
        | ParsedSql::AlterRole(_)
        | ParsedSql::SetRole(_)
        | ParsedSql::Do(_)
        | ParsedSql::CreateSchema(_)
        | ParsedSql::DropSchema(_)
        | ParsedSql::AlterSchema(_)
        | ParsedSql::AlterSequence(_)
        | ParsedSql::SequenceFunction(_) => na(),
    }
}

/// `req` と参照トークンから relation ごとの必要ビットを組み立てる。参照から target を 1 回ずつ差し引いた
/// 残りには SELECT を課す。target が参照に余分に現れる（自己参照）ときは、その分が残りに入り、同名の
/// target に SELECT が OR で足される。
fn build_wants(req: &WriteRequirement, candidates: &[String]) -> Vec<(String, u16)> {
    let mut remaining: Vec<&String> = candidates.iter().collect();
    let mut wants: Vec<(String, u16)> = req.targets.clone();
    for (t, _) in &req.targets {
        if let Some(i) = remaining.iter().position(|c| *c == t) {
            remaining.remove(i);
        }
    }
    for c in remaining {
        wants.push((c.clone(), PRIV_SELECT));
    }
    wants
}

fn check_unlooked(principal: &Principal, req: &WriteRequirement) -> Result<(), SqlSurfaceError> {
    check(
        principal,
        PrivilegeTarget::Relation {
            required_bits: req.all_bits(),
            granted_bits: 0,
        },
    )
}

/// 文の書き込み先と参照先に必要な権限があるかを判定する（権限不足は `42501`）。`tokens` は構文解析へ
/// 渡した字句列（名前解決前。`resolver` で修飾を解決してから走査する）。呼び出し元は構文解析の成功後に
/// 呼ぶ。現在ロール未束縛・運用者権限のセッションは何もしない。
pub(crate) fn require_write_privileges(
    storage: &Storage,
    tenant: &str,
    session: &SessionState,
    tokens: &[Token],
    resolver: Option<SchemaResolver<'_>>,
    parsed: &ParsedSql,
) -> Result<(), SqlSurfaceError> {
    let req = classify_write(parsed);
    if req.is_not_applicable() {
        return Ok(());
    }
    let principal = Principal::from_session(session);
    if !principal.needs_acl_lookup() {
        return check_unlooked(&principal, &req);
    }
    let mut resolved = tokens.to_vec();
    crate::sql::relation_name::rewrite_relation_names_full(&mut resolved, resolver)?;
    let candidates = crate::sql::search_path::resolved_relation_ref_names(&resolved);
    require_relations_bits(storage, tenant, &principal, &build_wants(&req, &candidates))
}

/// [`require_write_privileges`] の、名前解決済みのトークン列を受け取る版（拡張クエリの Parse・Bind・
/// Describe）。
pub(crate) fn require_write_privileges_resolved(
    storage: &Storage,
    tenant: &str,
    session: &SessionState,
    resolved_tokens: &[Token],
    parsed: &ParsedSql,
) -> Result<(), SqlSurfaceError> {
    let req = classify_write(parsed);
    if req.is_not_applicable() {
        return Ok(());
    }
    let principal = Principal::from_session(session);
    if !principal.needs_acl_lookup() {
        return check_unlooked(&principal, &req);
    }
    let candidates = crate::sql::search_path::resolved_relation_ref_names(resolved_tokens);
    require_relations_bits(storage, tenant, &principal, &build_wants(&req, &candidates))
}

/// 表名を直接受け取る入口（HTTP 表層の bound 書き込み API。Issue #1969）用。`table` は解決済みの relation
/// キーで、`required_bits` は SQL 表層の同等の文と同じ関数（[`partitioned_update_bits`] 等）が決めた値。
/// 現在ロール未束縛・運用者権限のセッションは何もしない。権限不足は固定文言の `42501`。
pub(crate) fn require_table_write(
    storage: &Storage,
    tenant: &str,
    session: &SessionState,
    table: &str,
    required_bits: u16,
) -> Result<(), SqlSurfaceError> {
    let principal = Principal::from_session(session);
    if !principal.needs_acl_lookup() {
        return check(
            &principal,
            PrivilegeTarget::Relation {
                required_bits,
                granted_bits: 0,
            },
        );
    }
    require_relations_bits(
        storage,
        tenant,
        &principal,
        &[(table.to_string(), required_bits)],
    )
}

/// 書き込み txn の内側で権限を再判定するための値（分割実行 DML のチャンクごとの再検査と、TRUNCATE の
/// CASCADE 閉包の再判定）。主体が束縛済みかつ運用者権限なしのときだけ作られる（それ以外は `None` を渡し、
/// 従来と同一）。ACL 行は呼び出し側の txn から読むので、同じ txn の GRANT／REVOKE の結果を見る。
#[derive(Debug, Clone)]
pub(crate) struct RelationPrivilegeRecheck {
    tenant: String,
    role_id: u64,
    required_bits: u16,
}

impl RelationPrivilegeRecheck {
    /// 検査が要る主体のときだけ `Some`。
    pub(crate) fn for_session(
        session: &SessionState,
        tenant: &str,
        required_bits: u16,
    ) -> Option<Self> {
        let principal = Principal::from_session(session);
        if !principal.needs_acl_lookup() {
            return None;
        }
        Some(Self {
            tenant: tenant.to_string(),
            role_id: principal.current_role_id().unwrap_or(0),
            required_bits,
        })
    }

    /// `tables` のすべてに必要ビットがあるか。`Err(())` は権限不足、または照会失敗（fail-closed）。
    pub(crate) fn check_in_txn<R: ReadSource>(&self, src: &R, tables: &[String]) -> Result<(), ()> {
        let bits =
            relation_grant_bits_in_txn(src, &self.tenant, tables, self.role_id).map_err(|_| ())?;
        for granted in bits.into_iter().flatten() {
            if self.required_bits == 0 || granted & self.required_bits != self.required_bits {
                return Err(());
            }
        }
        Ok(())
    }
}
