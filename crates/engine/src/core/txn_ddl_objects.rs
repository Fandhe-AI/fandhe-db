//! 明示・暗黙トランザクション内の VIEW・SEQUENCE・SCHEMA・ENUM 型の DDL（`CREATE VIEW`・`DROP VIEW`・
//! `CREATE`/`DROP`/`ALTER SEQUENCE`・`CREATE`/`DROP`/`ALTER SCHEMA`・`CREATE`/`DROP TYPE`）の実行本体
//! （SQL-80・SQL-31・SQL-23・TABLE-22・TABLE-24・TABLE-27・TABLE-14・TASK-349 のポインタ、Issue #3315。
//! 設計記録: `docs/design/explicit-transaction.md`「明示トランザクション内の DDL の方式」）。
//!
//! 責務境界: `core/txn_ddl.rs` の `execute_table_ddl_in_txn_inner`（Tx の持続時間の期限を張った区間）が、
//! 表・索引以外の DDL をここへ委ねる。実行本体は autocommit と同じ `sql::ddl` の `*_in_txn`（共有 write txn
//! を受け取る）で、第 2 の DDL 実行器を作らない。commit は Tx の COMMIT が 1 回だけ行い、メモリ上の状態は
//! `SessionTransaction::mark_ddl_done` が積んだ commit 後フックだけが変える。
//!
//! 判定順（`txn_ddl.rs` の表・索引の DDL と同じ。権限の無い主体へ対象の有無を返さない）:
//! DDL 権限（`42501`）→ 作成先の検査 → 未対応の形の拒否（`0A000`。writer を取る前・カタログを読む前）→
//! writer の取得（`55P03`・`40001`）→ 作成先の再判定 → DDL 本体。本体が失敗した Tx は `Failed` になる。
//!
//! 未対応の形（fail-closed の `0A000`。受け皿は #3311）: SCHEMA 配下の関係（内部キー `#<id>.<name>`）の作成・
//! 削除・改名、要素つき `CREATE SCHEMA`、`DROP SCHEMA ... CASCADE`、`CREATE OR REPLACE VIEW`、
//! `ALTER SEQUENCE ... RESTART`、DDL を積んだ後の列別名・`WITH CHECK OPTION` つき `CREATE VIEW`
//! （確定済みカタログで行う事前検査が write txn の内容と食い違いうるため）。
//! マテリアライズドビューは `core.rs` の入口が既定の腕（`0A000`）のまま拒否する。
//!
//! SEQUENCE の採番状態は非トランザクション性を保つ（ROLLBACK しても採番済みの値は戻らない）。カタログの
//! 変更だけを Tx に積み、内部 ID の再利用による古いカーソルの引き継ぎは、払い出し時にその ID のカーソルを
//! 捨てることで防ぐ（`catalog::sequence::create_sequence_in_txn_evicting`）。

use super::{EngineCore, ParsedSql};
use crate::catalog::ViewCheckOption;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::privilege_gate::{require_ddl, require_ddl_or_owner, DdlStatementKind};
use crate::sql::transaction::SessionTransaction;
use crate::sql::SqlOutcome;

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// 内部キー（`#<id>.<name>`）で表す SCHEMA 配下の関係か（`txn_ddl.rs` の同名の判定と同じ規則）。
fn is_schema_key(name: &str) -> bool {
    name.starts_with('#')
}

fn schema_scoped_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::transaction_feature_not_supported(
        "DDL on relations inside a schema is not supported inside a transaction block yet",
    )
}

impl EngineCore {
    /// 表・索引以外の DDL（本モジュールの冒頭の種別）を、Tx の共有 write txn へ積む。
    /// `execute_table_ddl_in_txn_inner` から呼ばれる。判定順・失敗時の Tx の遷移はモジュールドキュメントのとおり。
    pub(super) fn execute_object_ddl_in_txn_inner<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        match parsed {
            ParsedSql::CreateView(stmt) => {
                require_ddl(session, DdlStatementKind::CreateView)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                crate::sql::schema_dependency::check_view_body(stmt.name(), stmt.body_sql())?;
                if is_schema_key(stmt.name()) || is_schema_key(stmt.base_relation()) {
                    return Err(schema_scoped_unsupported());
                }
                if stmt.or_replace() {
                    return Err(SqlSurfaceError::transaction_feature_not_supported(
                        "CREATE OR REPLACE VIEW is not supported inside a transaction block yet",
                    ));
                }
                let needs_catalog_precheck = !stmt.column_aliases().is_empty()
                    || !matches!(stmt.check_option(), ViewCheckOption::None);
                // 列別名の突合・`WITH CHECK OPTION` の宣言検査は確定済みカタログを読む。DDL を積む前なら
                // writer 保持中の write txn の起点と一致するが、積んだ後は食い違いうるため拒否する。
                if needs_catalog_precheck && txn.has_ddl() {
                    return Err(SqlSurfaceError::transaction_feature_not_supported(
                        "CREATE VIEW with column aliases or CHECK OPTION is not supported after DDL inside a transaction block yet",
                    ));
                }
                txn.ensure_writer(ctx)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                if needs_catalog_precheck {
                    self.check_create_view_column_aliases(session, stmt)?;
                    crate::sql::view_check::check_declaration(
                        &self.storage,
                        stmt.name(),
                        crate::catalog::ViewDef {
                            base_relation: stmt.base_relation().to_string(),
                            body_sql: stmt.body_sql().to_string(),
                            column_aliases: stmt.column_aliases().to_vec(),
                            check_option: stmt.check_option(),
                        },
                    )?;
                }
                txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let _owner_guard = self.arm_created_relation_owner(ctx, session, stmt.name());
                let outcome = crate::sql::ddl::execute_create_view_in_txn(write_txn, stmt)?;
                txn.mark_ddl_done(false, Vec::new());
                Ok(SqlOutcome::CreateView(outcome))
            }
            ParsedSql::DropView(stmt) => {
                require_ddl(session, DdlStatementKind::DropView)?;
                if is_schema_key(stmt.name()) {
                    return Err(schema_scoped_unsupported());
                }
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::ddl::execute_drop_view_in_txn(write_txn, stmt)?;
                if outcome.skipped {
                    // `IF EXISTS` で対象が無い no-op は印を巻き戻す。
                    txn.restore_ddl_started(snap);
                }
                txn.mark_ddl_done(!outcome.skipped, Vec::new());
                Ok(SqlOutcome::DropView(outcome))
            }
            ParsedSql::CreateSequence(stmt) => {
                require_ddl(session, DdlStatementKind::CreateSequence)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                if is_schema_key(stmt.name()) {
                    return Err(schema_scoped_unsupported());
                }
                txn.ensure_writer(ctx)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                // 作成者の `SEQUENCES` の既定権限を同じ write txn の初期 ACL へ適用するため、作成の直前に
                // 作成者を予約する（autocommit の腕と同じ）。
                let _owner_guard = self.arm_created_relation_owner(ctx, session, stmt.name());
                let outcome = crate::sql::ddl::execute_create_sequence_in_txn(
                    &self.storage,
                    write_txn,
                    stmt,
                )?;
                txn.mark_ddl_done(false, Vec::new());
                Ok(SqlOutcome::CreateSequence(outcome))
            }
            ParsedSql::DropSequence(stmt) => {
                require_ddl(session, DdlStatementKind::DropSequence)?;
                if is_schema_key(stmt.name()) {
                    return Err(schema_scoped_unsupported());
                }
                txn.ensure_writer(ctx)?;
                txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, id) = crate::sql::ddl::execute_drop_sequence_in_txn(write_txn, stmt)?;
                // 採番器の現在値は commit 成功後にだけ捨てる。DROP の後の `nextval` が確定済みカタログの
                // 古い定義を引かないよう、後続の読み取り・INSERT を拒否する（`removed`）。
                txn.mark_ddl_done(true, vec![id]);
                Ok(SqlOutcome::DropSequence(outcome))
            }
            ParsedSql::AlterSequence(stmt) => {
                require_ddl(session, DdlStatementKind::AlterSequence)?;
                if is_schema_key(stmt.name()) {
                    return Err(schema_scoped_unsupported());
                }
                // RESTART は commit 後にしか採番器の現在値を捨てられず、同じ Tx の後続の `nextval` が古い現在値を
                // 払い出すため未対応（writer を取る前に拒否する）。
                if crate::sql::ddl::alter_sequence_restarts(stmt) {
                    return Err(SqlSurfaceError::transaction_feature_not_supported(
                        "ALTER SEQUENCE RESTART is not supported inside a transaction block yet",
                    ));
                }
                txn.ensure_writer(ctx)?;
                txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::ddl::execute_alter_sequence_in_txn(
                    &self.storage,
                    write_txn,
                    ctx.tenant_id(),
                    stmt,
                )?;
                // 後続の `nextval` が確定済みカタログの古い定義・名前を引かないよう、読み取り・INSERT を拒否する。
                txn.mark_ddl_done(true, Vec::new());
                Ok(SqlOutcome::AlterSequence(outcome))
            }
            ParsedSql::CreateSchema(stmt) => {
                require_ddl(session, DdlStatementKind::CreateSchema)?;
                if !stmt.elements.is_empty() {
                    return Err(SqlSurfaceError::transaction_feature_not_supported(
                        "CREATE SCHEMA with schema elements is not supported inside a transaction block yet",
                    ));
                }
                crate::sql::ddl::precheck_create_schema_in_txn(ctx.tenant_id(), stmt)?;
                txn.ensure_writer(ctx)?;
                // Issue #3315: 名前付きロールは共有 write txn 上で解決する（同じ Tx の CREATE/ALTER ROLE を反映）。
                let owner = {
                    let write_txn = txn.write_txn().ok_or_else(internal)?;
                    crate::sql::owner_ddl::resolve_schema_owner_in_txn(
                        write_txn,
                        ctx.tenant_id(),
                        session,
                        stmt.authorization.as_ref(),
                    )?
                };
                let snap = txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, created) = crate::sql::ddl::execute_create_schema_in_txn(
                    write_txn,
                    ctx.tenant_id(),
                    stmt,
                    owner,
                )?;
                if created {
                    txn.mark_schema_changed();
                } else {
                    // `IF NOT EXISTS` で既存の no-op は印を巻き戻す。
                    txn.restore_ddl_started(snap);
                }
                txn.mark_ddl_done(false, Vec::new());
                Ok(SqlOutcome::CreateSchema(outcome))
            }
            ParsedSql::DropSchema(stmt) => {
                let decision = require_ddl_or_owner(session, DdlStatementKind::DropSchema)?;
                if stmt.cascade {
                    return Err(SqlSurfaceError::transaction_feature_not_supported(
                        "DROP SCHEMA CASCADE is not supported inside a transaction block yet",
                    ));
                }
                crate::sql::ddl::precheck_drop_schema_in_txn(ctx.tenant_id(), stmt)?;
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, dropped) = crate::sql::ddl::execute_drop_schema_in_txn(
                    write_txn,
                    ctx.tenant_id(),
                    stmt,
                    decision.owner_gate(),
                )?;
                if dropped {
                    txn.mark_schema_changed();
                } else {
                    // `IF EXISTS` で不在の no-op は印を巻き戻す。
                    txn.restore_ddl_started(snap);
                }
                txn.mark_ddl_done(false, Vec::new());
                Ok(SqlOutcome::DropSchema(outcome))
            }
            ParsedSql::AlterSchema(stmt) => {
                let decision = require_ddl_or_owner(session, DdlStatementKind::AlterSchema)?;
                crate::sql::ddl::precheck_alter_schema_in_txn(ctx.tenant_id(), stmt)?;
                txn.ensure_writer(ctx)?;
                txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::ddl::execute_alter_schema_in_txn(
                    write_txn,
                    ctx.tenant_id(),
                    stmt,
                    decision.owner_gate(),
                )?;
                txn.mark_schema_changed();
                txn.mark_ddl_done(false, Vec::new());
                Ok(SqlOutcome::AlterSchema(outcome))
            }
            ParsedSql::CreateType(stmt) => {
                require_ddl(session, DdlStatementKind::CreateType)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                if is_schema_key(stmt.name()) {
                    return Err(schema_scoped_unsupported());
                }
                // 予約名・ラベル数の上限は writer 取得・印の前に検査する（失敗しても Tx を継続できる）。
                crate::sql::ddl::precheck_create_type_in_txn(stmt, &self.schema_limits)?;
                txn.ensure_writer(ctx)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::ddl::execute_create_type_in_txn(
                    write_txn,
                    stmt,
                    &self.schema_limits,
                )?;
                txn.mark_ddl_done(false, Vec::new());
                Ok(SqlOutcome::CreateType(outcome))
            }
            ParsedSql::DropType(stmt) => {
                require_ddl(session, DdlStatementKind::DropType)?;
                if is_schema_key(stmt.name()) {
                    return Err(schema_scoped_unsupported());
                }
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::ddl::execute_drop_type_in_txn(write_txn, stmt)?;
                if outcome.skipped() {
                    txn.restore_ddl_started(snap);
                }
                txn.mark_ddl_done(!outcome.skipped(), Vec::new());
                Ok(SqlOutcome::DropType(outcome))
            }
            // 呼び出し元が上の種別だけを渡す。到達したら fail-closed。
            _ => Err(internal()),
        }
    }
}
