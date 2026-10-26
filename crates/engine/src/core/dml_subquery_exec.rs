//! 述語形 `UPDATE`／`DELETE` の WHERE 内サブクエリの実行（SQL-53・Issue #1751。親: #1749）。
//!
//! 責務境界: 構文段での受理（#1750）と台帳ハッシュの正規化（#1752）は別 Issue の担当で、
//! 本モジュールは「束縛より前に、内側 SELECT を書き込みトランザクションのスナップショットで評価し、
//! 具体的な [`WherePredicate`] へ書き換える」ことだけを担う。書き換え後の述語は既存の束縛・実行経路
//! （SQL-19。`sql::exec::execute_predicate_*_in`）へそのまま渡り、第 2 の実行器は作らない。
//!
//! 呼び出し元: `core.rs` の述語形 4 form（`execute_predicate_delete_form`・
//! `execute_predicate_delete_returning_form`・`execute_predicate_update_form`・
//! `execute_update_returning_form`）が、スキーマ取得の後・束縛の前に呼ぶ。共通本体
//! （NoSQL 表層と共有する `run_predicate_*`）は解決済みの述語だけを受け取る契約を保つ。
//!
//! テナント境界（P0）: 内側は `sql::subquery::resolve_where_predicates` 経由で、書き込む
//! セッションの `PolicyContext`（RLS 暗黙適用）・同じ予算・上限（SQL-29。実行回数・内側の可視行数・
//! IN 値数）で評価する（独自の走査を持たない）。外側の変更対象も既存の `ctx` 所有行の列挙のまま。
//! 他テナントの行は IN／EXISTS の真偽・スカラー値・件数・RETURNING のいずれにも影響しない。
//!
//! スナップショット: 評価は `WriteTarget::InTxn` の共有 write txn で行う（自トランザクションの
//! 未 commit の変更が見え、対象行の変更より前に全部解決するため自己参照でも文の開始時点を読む）。
//! autocommit の文は [`EngineCore::execute_subquery_dml_autocommit`] が文単位の暗黙トランザクションで
//! 包んで同じ経路へ流す（評価と書き込みの間に他 writer の commit が挟まらない）。
//!
//! 台帳ハッシュ（RECOVER-11・SQL-53 (5)）: ハッシュは常に解決**前**の文から計算し、解決結果の値は
//! 入れない。Issue #2650 で #1752 の正規化ハッシュ（`content_hash` のサブクエリ葉）へ結線済みで、
//! 解決前の述語でハッシュを出した直後・内側の評価の前に台帳を事前照合する
//! （[`EngineCore::precheck_subquery_dml_ledger`]）。再送は内側の結果に左右されず `23505`、
//! 別文の再利用は `22023` になる。
//!
//! fail-closed: autocommit の `WriteTarget` でサブクエリつき述語が来た場合（暗黙トランザクションを
//! 通らない未知の経路）は解決せず、固定文言の `0A000` で拒否する。`PARTITIONED` との組合せは
//! 束縛段の `42601` を維持する（解決せずに既存の拒否経路へ落とす）。

use super::EngineCore;
use crate::policy::PolicyContext;
use crate::recovery::content_hash::ContentHash;
use crate::sql::allowlist::{
    SqlSurfaceError, ValidatedPredicateDelete, ValidatedPredicateUpdate, ValidatedUpdateForm,
    WherePredicate,
};
use crate::sql::mode::SessionState;
use crate::tenant::WriteTarget;

/// 述語列（`Or` の分岐・`Not` の中身を含む）にサブクエリの葉が 1 つでもあるか。
pub(super) fn where_has_subquery(preds: &[WherePredicate]) -> bool {
    preds.iter().any(|p| match p {
        WherePredicate::InSubquery { .. }
        | WherePredicate::Exists { .. }
        | WherePredicate::ScalarSubqueryCompare { .. }
        | WherePredicate::QuantifiedSubqueryCompare { .. } => true,
        WherePredicate::Not(inner) => where_has_subquery(std::slice::from_ref(inner.as_ref())),
        WherePredicate::Or(branches) => branches.iter().any(|b| where_has_subquery(b)),
        _ => false,
    })
}

fn not_supported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    }
}

impl EngineCore {
    /// 述語列のサブクエリを `target`（`InTxn` の共有 write txn）で解決して返す。
    /// 予算は 1 文につき 1 組を新しく初期化する（SELECT 腕と同じ。SQL-29）。
    fn resolve_dml_where(
        &self,
        target: &WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &SessionState,
        schema: &crate::catalog::TableSchema,
        preds: &[WherePredicate],
    ) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
        let WriteTarget::InTxn(write_txn) = target else {
            // 暗黙トランザクションを通らない経路。解決せず固定文言で拒否する（fail-closed）。
            return Err(not_supported(
                "subquery in WHERE is not supported for this statement",
            ));
        };
        let mut budget = self.nesting_limits.max_subquery_executions();
        let mut in_budget = crate::sql::subquery::MAX_SUBQUERY_IN_VALUES;
        crate::sql::subquery::resolve_where_predicates(
            preds.to_vec(),
            &[schema],
            *write_txn,
            ctx,
            &self.storage,
            session.udfs(),
            self.group_by_limits,
            self.nesting_limits,
            &mut budget,
            &mut in_budget,
        )
    }

    /// 台帳の事前照合（Issue #2650・RECOVER-11・SQL-53 (5)）。解決**前**の文から出した正規化ハッシュで、
    /// 内側の評価より前に再送（`23505`）・別文の `operation_id` 再利用（`22023`）を判定する。
    /// 読み取り専用で台帳表を作らない。確定記録は実行本体の `record_in_txn` が同じ txn で行い、
    /// そのハッシュは `prepare_predicate_*` が同じ入力・同じ関数で計算するため一致する。
    /// `InTxn` 以外は解決しない target と同じく `0A000` で fail-closed（照合より先に確かめる）。
    fn precheck_subquery_dml_ledger(
        &self,
        target: &WriteTarget<'_>,
        ctx: &PolicyContext,
        table: &str,
        op_id: Option<&crate::recovery::required_op_id::OperationId>,
        hash: &ContentHash,
        op: &'static str,
    ) -> Result<(), SqlSurfaceError> {
        let WriteTarget::InTxn(write_txn) = target else {
            return Err(not_supported(
                "subquery in WHERE is not supported for this statement",
            ));
        };
        let ledger_write = self
            .ledger_mode
            .resolve(op_id)
            .map_err(|_| SqlSurfaceError::MissingOperationId)?;
        crate::recovery::ledger::precheck_in_write_txn(
            write_txn,
            ctx.tenant_id(),
            table,
            ledger_write,
            hash,
        )
        .map_err(|e| {
            crate::sql::exec::map_write_error(crate::tenant::TenantWriteError::from(e), op)
        })
    }

    /// 述語形 `DELETE` のサブクエリを解決した文を返す。サブクエリが無い・`PARTITIONED` 付き
    /// （既存の拒否経路へ落とす）なら `None`（呼び出し元は元の文をそのまま束縛する）。
    pub(super) fn resolve_predicate_delete_subqueries(
        &self,
        target: &WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &SessionState,
        stmt: &ValidatedPredicateDelete,
        schema: &crate::catalog::TableSchema,
    ) -> Result<Option<ValidatedPredicateDelete>, SqlSurfaceError> {
        if stmt.partitioned().is_some() || !where_has_subquery(stmt.where_predicates()) {
            return Ok(None);
        }
        let hash = crate::recovery::content_hash::for_delete_where(
            stmt.table_name(),
            stmt.where_predicates(),
            session.udfs(),
        )?;
        self.precheck_subquery_dml_ledger(
            target,
            ctx,
            stmt.table_name(),
            stmt.operation_id(),
            &hash,
            "delete",
        )?;
        let preds =
            self.resolve_dml_where(target, ctx, session, schema, stmt.where_predicates())?;
        let mut resolved = stmt.clone();
        resolved.where_predicates = preds;
        Ok(Some(resolved))
    }

    /// 述語形 `UPDATE` 版（[`Self::resolve_predicate_delete_subqueries`] と同じ契約）。
    /// 単一行形（`Single`）は対象外で常に `None`。
    pub(super) fn resolve_predicate_update_subqueries(
        &self,
        target: &WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &SessionState,
        stmt: &ValidatedUpdateForm,
        schema: &crate::catalog::TableSchema,
    ) -> Result<Option<ValidatedPredicateUpdate>, SqlSurfaceError> {
        let ValidatedUpdateForm::Predicate(v) = stmt else {
            return Ok(None);
        };
        if v.partitioned().is_some() || !where_has_subquery(v.where_predicates()) {
            return Ok(None);
        }
        let refs: Vec<(&str, &crate::sql::allowlist::SetValue)> = v
            .assignments()
            .iter()
            .map(|(n, val)| (n.as_str(), val))
            .collect();
        let hash = crate::recovery::content_hash::for_update_where(
            v.table_name(),
            &refs,
            v.where_predicates(),
            session.udfs(),
            schema,
        )?;
        self.precheck_subquery_dml_ledger(
            target,
            ctx,
            v.table_name(),
            v.operation_id(),
            &hash,
            "update",
        )?;
        let preds = self.resolve_dml_where(target, ctx, session, schema, v.where_predicates())?;
        let mut resolved = v.clone();
        resolved.where_predicates = preds;
        Ok(Some(resolved))
    }

    /// サブクエリつき述語形 `UPDATE`／`DELETE`（autocommit。`PARTITIONED` なし）かどうか。
    pub(super) fn is_subquery_predicate_dml(parsed: &super::ParsedSql) -> bool {
        use crate::sql::allowlist::DeleteStatement;
        match parsed {
            super::ParsedSql::Delete(DeleteStatement::Predicate(v)) => {
                v.partitioned().is_none() && where_has_subquery(v.where_predicates())
            }
            super::ParsedSql::Update(ValidatedUpdateForm::Predicate(v)) => {
                v.partitioned().is_none() && where_has_subquery(v.where_predicates())
            }
            _ => false,
        }
    }

    /// autocommit のサブクエリつき述語形 DML を、文単位の暗黙トランザクションで実行する。
    /// 評価と書き込みが同じ write txn に乗り、commit 時に遅延 FK の検査と commit 境界を
    /// 通る（`SessionTransaction::commit_implicit`）。失敗時は txn を drop して abort する
    /// （部分書き込みなし。暗黙トランザクションの `ON COMMIT` 適用も commit 側が担う）。
    pub(super) fn execute_subquery_dml_autocommit(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        parsed: &super::ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let mut txn = self.new_session_transaction();
        txn.begin_implicit(&self.storage, session)?;
        // 粗い一時表ゲートは呼び出し元（`execute_parsed_in_session_inner` の振り分け展開後）で通過済み。
        // 振り分け済みの永続側の文へ再適用すると一時表を持つセッションで `0A000` になるため inner へ委譲する。
        let outcome = self.execute_parsed_in_txn_inner(ctx, session, &mut txn, parsed)?;
        txn.commit_implicit(session)?;
        Ok(outcome)
    }
}
