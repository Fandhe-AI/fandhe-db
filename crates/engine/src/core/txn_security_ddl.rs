//! 明示・暗黙トランザクション内の関数・ポリシー・行セキュリティ・権限・所有者・ロール・トリガの DDL
//! （`CREATE`／`DROP FUNCTION`・`CREATE`／`ALTER`／`DROP POLICY`・`ALTER TABLE ... ROW LEVEL SECURITY`・
//! `GRANT`／`REVOKE`・`ALTER DEFAULT PRIVILEGES`・`ALTER ... OWNER TO`・`CREATE`／`DROP`／`ALTER ROLE`・
//! `CREATE`／`DROP TRIGGER`）の実行入口（SQL-80・SQL-31・SQL-23・SQL-52・TABLE-22・TABLE-36・TABLE-44・
//! TASK-349 のポインタ、Issue #3316。設計記録: `docs/design/explicit-transaction.md`）。
//!
//! 責務境界: `core.rs` の `execute_in_active_txn_inner`（明示 Tx と複数文メッセージの暗黙 Tx が共有する入口）が
//! 該当の腕から [`EngineCore::execute_security_ddl_in_txn`] を呼ぶ。表・索引の DDL を担う `core/txn_ddl.rs` とは
//! 別モジュールにし、実行本体は autocommit と同じ `sql/*_ddl.rs` の `execute_*_in`（`target` に共有 write txn を
//! 渡す）で、第 2 の実行器を作らない。commit は Tx の COMMIT が 1 回だけ行う。
//!
//! 判定順（autocommit の腕と同じ前段を同じ順で通す。権限の無い主体へ対象の有無を返さない）:
//! 権限ゲート（`42501`。writer を取る前）→ 未対応の形の拒否（`0A000`。writer を取る前・カタログを読む前）→
//! writer の取得（`55P03`・`40001`）→ DDL を積んだ印（本体が途中で失敗しても残す）→ 本体。本体が失敗した Tx は
//! `Failed` になり、COMMIT できない。付与者の権限（grant option）・所有者・依存の判定は本体が write txn の上で
//! 行うため、writer を取った後の再判定は構造的に満たされる（`BEGIN` の後に他セッションが commit した変更も見る）。
//! セッションだけで決まるゲート（`require_ddl` 等）には再判定の意味が無い。
//!
//! 後続文の扱い（P0・暫定。受け皿は #3311）: 権限・ポリシー・ロール・トリガ・関数削除の DDL を積んだ Tx は
//! `SessionTransaction::mark_ddl_guarded` の印を持ち、`core/txn_ddl.rs::check_post_ddl_statement` が DDL・Tx 制御・
//! セッション変数以外の文を `0A000` にする。権限判定・ポリシー適用・トリガの発火・関数の解決は確定済みの
//! カタログを読むため、同じ Tx で変更した内容を見ずに古い判定で通す fail-open を作らない。`CREATE [OR REPLACE]
//! FUNCTION` だけは従来（暗黙 Tx。Issue #2882）どおり印を立てず、セッションのレジストリ経由で後続の呼び出しが
//! 新定義を見る。関数の世代は `mark_function_written` が commit 成功後にだけ進める。
//!
//! 受理しない形（`0A000`）: PL/pgSQL 本体・`DO`・動的 `EXECUTE` の中の本種別（`plpgsql_host::classify` が DML と
//! SELECT 系以外を拒否する）、共有 public 表への `CREATE TRIGGER`（本体の判定。Issue #3382）、一時表への
//! `OWNER TO`。
//!
//! SCHEMA 配下の関係（内部キー `#<id>.<name>`）が対象の文は、Tx がまだ表・索引・関数の DDL を積んでいない間は
//! 受理する（共有 public 表にはトリガを持てない〔Issue #3361〕ため、トリガの対象は SCHEMA 配下の表に限られる）。
//! 名前解決は確定済みのカタログで行われ、同じ Tx で作った public の表・関数は `search_path` で見える SCHEMA 配下の
//! 同名の関係へ誤って解決されうる（別の関係の権限・ポリシー・トリガを変える fail-open）。そのため表・索引・関数の
//! DDL を積んだ後は、内部キーに解決された対象を固定の `0A000` で拒否する
//! （[`reject_schema_keys_after_shape_change`]。受け皿は #3315）。SCHEMA の作成・削除・改名と SCHEMA 配下の表の DDL は
//! Tx 内で受理しない（`core/txn_ddl.rs`）ので、積んでいない間に解決された内部キーはすべて確定済みの関係を指す。
//! #3315 が SCHEMA の DDL を受理するときは、この前提を見直す。

use super::{EngineCore, ParsedSql};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{SqlSurfaceError, Statement};
use crate::sql::mode::SessionState;
use crate::sql::privilege_gate::{require_ddl, DdlStatementKind};
use crate::sql::transaction::SessionTransaction;
use crate::sql::SqlOutcome;

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// 表・索引・関数の DDL を積んだ Tx で、SCHEMA 配下の内部キー（`#<id>.<name>`）に解決された対象を拒否する
/// （`0A000`。P0・fail-closed。受け皿は #3315）。
///
/// 名前解決は文ごとに確定済みのカタログで行われる。同じ Tx で作った public の表・関数は確定済みカタログには
/// まだ無いため、`search_path` で見える SCHEMA 配下の同名の関係へ解決されてしまい、本種別の DDL が別の表・関数へ
/// 当たって権限・ポリシー・トリガを変える。Tx の積んだ内容と食い違いうる状況（関係・関数の集合が変わった）では、
/// 内部キーに解決された対象を受け付けない。何も積んでいなければ解決は確定済みの状態と一致するため受理する。
/// 文言は固定で、名前・存在を含まない。`shape_changed` は対象の種類に対応する集合が変わったか（関係なら
/// `has_relation_ddl`、関数なら `has_function_ddl`。種類を混ぜない。トリガ関数を同じ Tx で作ってトリガを張る形を通すため）。
fn reject_schema_keys_after_shape_change<'a>(
    shape_changed: bool,
    keys: impl IntoIterator<Item = &'a str>,
) -> Result<(), SqlSurfaceError> {
    if !shape_changed {
        return Ok(());
    }
    if keys.into_iter().any(|k| k.starts_with('#')) {
        return Err(SqlSurfaceError::transaction_feature_not_supported(
            "DDL on relations inside a schema is not supported after table or function DDL inside a transaction block yet",
        ));
    }
    Ok(())
}

impl EngineCore {
    /// 明示・暗黙 Tx の関数・ポリシー・権限・ロール・トリガの DDL を、Tx の共有 write txn へ積む。
    /// 判定順・失敗時の Tx の遷移はモジュールドキュメントのとおり。呼び出し元が `Err` で Tx を `Failed` にする。
    ///
    /// Tx の持続時間上限の期限を張り（writer の待機が打ち切る。`statement_timeout` との早い方が勝つ）、
    /// Tx の期限で打ち切られた場合は復帰不可の `Failed` と `54000` にする（表・索引の DDL と同じ契約）。
    pub(super) fn execute_security_ddl_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let deadline = match txn.duration_deadline() {
            Ok(d) => d,
            Err(e) => {
                txn.fail_unrecoverable();
                return Err(e);
            }
        };
        let guard = deadline.map(crate::interrupt::DeadlineScope::install);
        let result = self.execute_security_ddl_in_txn_inner(ctx, session, txn, parsed);
        drop(guard);
        match result {
            Err(SqlSurfaceError::QueryCanceled {
                reason: crate::interrupt::InterruptReason::StatementTimeout,
            }) if txn.duration_exceeded() => Err(txn.fail_duration_exceeded()),
            other => other,
        }
    }

    fn execute_security_ddl_in_txn_inner<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let tenant = ctx.tenant_id();
        match parsed {
            ParsedSql::Statement(stmt @ Statement::CreateFunction { .. }) => {
                require_ddl(session, DdlStatementKind::CreateFunction)?;
                txn.ensure_writer(ctx)?;
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = self.execute_create_function_stmt(
                    ctx,
                    session,
                    stmt.clone(),
                    crate::sql::udf_ddl::CatalogWrite::InTxn(write_txn),
                )?;
                // 全 scope に印を付ける（漏れると commit 後の世代 bump が落ち、他セッションが新定義を読めない）。
                txn.mark_function_written(tenant);
                Ok(outcome)
            }
            ParsedSql::DropFunction(stmt) => {
                // 所有者（束縛ロール）の検査は本体が write txn の上で行う（未束縛は `42501`）。
                crate::sql::privilege_gate::require_ddl_or_owner(
                    session,
                    DdlStatementKind::DropFunction,
                )?;
                reject_schema_keys_after_shape_change(
                    txn.has_function_ddl(),
                    [stmt.name.as_str()],
                )?;
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::udf_ddl::execute_drop_function_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    session,
                    stmt,
                )?;
                if outcome.skipped() {
                    txn.restore_ddl_started(snap);
                } else {
                    txn.mark_function_written(tenant);
                }
                Ok(SqlOutcome::DropFunction(outcome))
            }
            ParsedSql::CreatePolicy(stmt) => {
                require_ddl(session, DdlStatementKind::CreatePolicy)?;
                reject_schema_keys_after_shape_change(
                    txn.has_relation_ddl(),
                    [stmt.table.as_str()],
                )?;
                txn.ensure_writer(ctx)?;
                txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::policy_ddl::execute_create_policy_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    stmt,
                )?;
                Ok(SqlOutcome::CreatePolicy(outcome))
            }
            ParsedSql::AlterPolicy(stmt) => {
                require_ddl(session, DdlStatementKind::AlterPolicy)?;
                reject_schema_keys_after_shape_change(
                    txn.has_relation_ddl(),
                    [stmt.table.as_str()],
                )?;
                txn.ensure_writer(ctx)?;
                txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::policy_ddl::execute_alter_policy_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    stmt,
                )?;
                Ok(SqlOutcome::AlterPolicy(outcome))
            }
            ParsedSql::DropPolicy(stmt) => {
                require_ddl(session, DdlStatementKind::DropPolicy)?;
                reject_schema_keys_after_shape_change(
                    txn.has_relation_ddl(),
                    [stmt.table.as_str()],
                )?;
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::policy_ddl::execute_drop_policy_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    stmt,
                )?;
                if outcome.skipped() {
                    // `IF EXISTS` で対象が無い no-op は印を巻き戻す。
                    txn.restore_ddl_started(snap);
                }
                Ok(SqlOutcome::DropPolicy(outcome))
            }
            ParsedSql::GrantRevoke(stmt) => {
                crate::sql::grant_ddl::require_grant_principal(session)?;
                if let crate::sql::allowlist::GrantTarget::Objects(o) = &stmt.target {
                    reject_schema_keys_after_shape_change(
                        txn.has_relation_ddl(),
                        o.tables
                            .iter()
                            .map(String::as_str)
                            .chain(o.sequences.iter().map(String::as_str)),
                    )?;
                    reject_schema_keys_after_shape_change(
                        txn.has_function_ddl(),
                        o.functions.iter().map(|f| f.name.as_str()),
                    )?;
                }
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, changed) = crate::sql::grant_ddl::execute_grant_revoke_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    session,
                    stmt,
                )?;
                if !changed {
                    // 既に反映済みの再実行（何も書かない no-op）は印を巻き戻す。
                    txn.restore_ddl_started(snap);
                }
                Ok(SqlOutcome::GrantRevoke(outcome))
            }
            ParsedSql::AlterDefaultPrivileges(stmt) => {
                crate::sql::grant_ddl::require_grant_principal(session)?;
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, changed) =
                    crate::sql::default_privileges_ddl::execute_alter_default_privileges_in(
                        &self.storage,
                        Some(write_txn),
                        tenant,
                        session,
                        stmt,
                    )?;
                if !changed {
                    txn.restore_ddl_started(snap);
                }
                Ok(SqlOutcome::AlterDefaultPrivileges(outcome))
            }
            ParsedSql::AlterOwner(stmt) => {
                // 自動コミットと同じく、運用者権限があっても現在ロールが未束縛なら `42501`（O8）。
                crate::sql::privilege_gate::bound_principal(session)?;
                require_ddl_or_owner_for_owner(session)?;
                if stmt
                    .relation_name()
                    .is_some_and(|name| session.temp_tables().contains(name))
                {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "OWNER TO is not supported for temporary tables",
                    ));
                }
                reject_schema_keys_after_shape_change(
                    txn.has_relation_ddl(),
                    stmt.relation_name(),
                )?;
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, changed) = crate::sql::owner_ddl::execute_alter_owner_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    session,
                    stmt,
                )?;
                if !changed {
                    txn.restore_ddl_started(snap);
                }
                Ok(SqlOutcome::AlterOwner(outcome))
            }
            ParsedSql::CreateRole(stmt) => {
                require_ddl(session, DdlStatementKind::CreateRole)?;
                txn.ensure_writer(ctx)?;
                txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::role_ddl::execute_create_role_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    session,
                    stmt,
                )?;
                Ok(SqlOutcome::CreateRole(outcome))
            }
            ParsedSql::DropRole(stmt) => {
                require_ddl(session, DdlStatementKind::DropRole)?;
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, removed) = crate::sql::role_ddl::execute_drop_role_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    stmt,
                )?;
                if !removed {
                    // 何も削除しなかった `IF EXISTS` の no-op は印を巻き戻す（一部でも消えたら残す）。
                    txn.restore_ddl_started(snap);
                }
                Ok(SqlOutcome::DropRole(outcome))
            }
            ParsedSql::AlterRole(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, wrote) = crate::sql::role_ddl::execute_alter_role_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    session,
                    stmt,
                )?;
                if !wrote {
                    // オプション指定なしの存在確認だけ（何も書かない）は印を巻き戻す。
                    txn.restore_ddl_started(snap);
                }
                Ok(SqlOutcome::AlterRole(outcome))
            }
            ParsedSql::CreateTrigger(stmt) => {
                require_ddl(session, DdlStatementKind::CreateTrigger)?;
                // WHEN 条件の定義時の意味検査（Issue #3190。D7）。権限ゲートの後・カタログ書き込みの前。
                self.validate_trigger_when(stmt)?;
                reject_schema_keys_after_shape_change(
                    txn.has_relation_ddl(),
                    [stmt.table.as_str()],
                )?;
                reject_schema_keys_after_shape_change(
                    txn.has_function_ddl(),
                    [stmt.function.as_str()],
                )?;
                txn.ensure_writer(ctx)?;
                txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::trigger_ddl::execute_create_trigger_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    stmt,
                )?;
                Ok(SqlOutcome::CreateTrigger(outcome))
            }
            ParsedSql::DropTrigger(stmt) => {
                require_ddl(session, DdlStatementKind::DropTrigger)?;
                reject_schema_keys_after_shape_change(
                    txn.has_relation_ddl(),
                    [stmt.table.as_str()],
                )?;
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_guarded();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::trigger_ddl::execute_drop_trigger_in(
                    &self.storage,
                    Some(write_txn),
                    tenant,
                    stmt,
                )?;
                if outcome.skipped() {
                    txn.restore_ddl_started(snap);
                }
                Ok(SqlOutcome::DropTrigger(outcome))
            }
            // 呼び出し元の腕が上の種類だけを渡す。到達したら fail-closed。
            _ => Err(internal()),
        }
    }

    /// `ALTER TABLE ... [NO] FORCE／ENABLE／DISABLE ROW LEVEL SECURITY` を共有 write txn へ積む
    /// （`core/txn_ddl.rs` の `AlterTable` の腕から、DDL 権限ゲートの後に呼ばれる。期限のスコープは呼び出し元が
    /// 既に張っている）。再指定で状態が変わらない no-op は印を巻き戻す。
    pub(super) fn execute_row_security_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        txn: &mut SessionTransaction<'e>,
        stmt: &crate::sql::allowlist::ValidatedAlterTableRowSecurity,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        reject_schema_keys_after_shape_change(txn.has_relation_ddl(), [stmt.table_name.as_str()])?;
        txn.ensure_writer(ctx)?;
        let snap = txn.mark_ddl_guarded();
        let write_txn = txn.write_txn().ok_or_else(internal)?;
        let (outcome, changed) = crate::sql::ddl::execute_alter_table_row_security_in(
            &self.storage,
            Some(write_txn),
            ctx.tenant_id(),
            stmt,
        )?;
        if !changed {
            txn.restore_ddl_started(snap);
        }
        Ok(SqlOutcome::AlterTable(outcome))
    }
}

/// `ALTER ... OWNER TO` の権限ゲート（運用者権限、または束縛ロールによる所有者検査つき。本体が write txn で
/// 所有者を判定する）。
fn require_ddl_or_owner_for_owner(session: &SessionState) -> Result<(), SqlSurfaceError> {
    crate::sql::privilege_gate::require_ddl_or_owner(session, DdlStatementKind::AlterOwner)
        .map(|_| ())
}
