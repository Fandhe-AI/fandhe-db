//! 明示・暗黙トランザクション内の表・索引の DDL（`CREATE TABLE`・`DROP TABLE`・`ALTER TABLE` の一部・
//! `CREATE INDEX`・`DROP INDEX`）の実行入口と、DDL を積んだ後の文の許可リスト（SQL-80・SQL-31・SQL-23・
//! TABLE-22・TASK-349 のポインタ、Issue #3313。設計記録: `docs/design/explicit-transaction.md`
//! 「明示トランザクション内の DDL の方式」）。
//!
//! VIEW・SEQUENCE・SCHEMA・ENUM 型の DDL（Issue #3315）は同じ入口を通り、本体は `core/txn_ddl_objects.rs` が担う。
//!
//! 責務境界: `core.rs` の `execute_in_active_txn_inner`（明示 Tx と複数文メッセージの暗黙 Tx が共有する入口）が
//! DDL の腕から [`EngineCore::execute_table_ddl_in_txn`] を呼ぶ。実行本体は autocommit と同じ `sql::ddl` の
//! `*_in_txn`（共有 write txn を受け取る。カタログの書き込みは redb の txn に載るため ROLLBACK で消える）で、
//! 第 2 の DDL 実行器を作らない。commit は Tx の COMMIT が 1 回だけ行い、メモリ上の状態（採番器の現在値）は
//! `SessionTransaction::mark_ddl_done` が積んだ commit 後フックだけが変える。
//!
//! 判定順（autocommit の腕と同じ前段を同じ順で通す。権限の無い主体へ対象の有無を返さない）:
//! DDL 権限（`42501`）→ 作成先の検査 → 未対応の形の拒否（`0A000`。writer を取る前・カタログを読む前）→
//! writer の取得（`55P03`・`40001`）→ DDL 本体。本体が失敗した Tx は `Failed` になり、COMMIT できない
//! （`SessionTransaction::mark_ddl_started`）ため、部分的な DDL を COMMIT する経路は無い。前の savepoint が
//! あれば `ROLLBACK TO` で、その時点（DDL 状態の印・commit 後の採番器の後始末の列を含む）へ戻れる（Issue #3317）。
//!
//! 暫定の規則（受け皿は #3311。V1・V5・V7）: DDL を積んだ後に同じ Tx で実行できる文は、結線を済ませた最小の
//! 種類だけにする（[`check_post_ddl_statement`]）。`search_path` の解決・ビュー展開・権限ゲートが読む確定済み
//! カタログと、write txn 上の内容が食い違う経路を作らないため、それ以外は `0A000` で拒否する（fail-closed）。
//! ビュー経由の読みは `catalog.rs` の `TableLookup::view_definition` が重ね見の区間で `0A000` にする。
//! 既存行の走査を伴う DDL（一意・式・述語つき・GIN の `CREATE INDEX`・`SET NOT NULL`・生成列の `ADD COLUMN`）は、
//! Tx の持続時間上限の期限（`SessionTransaction::duration_deadline`）を走査ループの確認点が見て打ち切る
//! （Issue #3314。超過は復帰不可の `Failed` と `54000`）。
//! 確定済みカタログで行う解析の表スキーマの引き当ては、Tx が変更した表の write txn 上のスキーマを重ねて
//! 見る（`sql::txn_overlay`）。

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

/// 内部キー（`#<id>.<name>`）で表す SCHEMA 配下の関係か。SCHEMA 配下の DDL は、後続の文の `search_path` の
/// 解決が確定済みカタログを読むため DDL 後に別の表へ解決されうる。結線（#3311）までは `0A000`。
fn is_schema_key(name: &str) -> bool {
    name.starts_with('#')
}

/// 本体が write txn だけを読む、権限・ポリシー・ロール・トリガ・関数削除の DDL か（SQL-80・Issue #3316）。
/// `CREATE [OR REPLACE] FUNCTION` は関数本体の束縛が確定済みカタログを読むため含めない。
fn is_security_ddl(parsed: &ParsedSql) -> bool {
    matches!(
        parsed,
        ParsedSql::DropFunction(_)
            | ParsedSql::CreatePolicy(_)
            | ParsedSql::AlterPolicy(_)
            | ParsedSql::DropPolicy(_)
            | ParsedSql::GrantRevoke(_)
            | ParsedSql::AlterDefaultPrivileges(_)
            | ParsedSql::AlterOwner(_)
            | ParsedSql::CreateRole(_)
            | ParsedSql::DropRole(_)
            | ParsedSql::AlterRole(_)
            | ParsedSql::CreateTrigger(_)
            | ParsedSql::DropTrigger(_)
    )
}

/// セッション変数だけを変える・読む文か（DDL 後でも読み取り源に影響しない）。
fn is_session_variable_statement(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::SetSearchMode { .. }
            | Statement::SetTimeZone { .. }
            | Statement::ShowTimeZone
            | Statement::SetSearchPath { .. }
            | Statement::ShowSearchPath
            | Statement::SetRowSecurity { .. }
            | Statement::ShowRowSecurity
            | Statement::ShowRestoreGeneration
            | Statement::SetStatementTimeout { .. }
            | Statement::ShowStatementTimeout
            | Statement::SetHnswEfSearch { .. }
            | Statement::ShowHnswEfSearch
            | Statement::SetHnswScanSetting { .. }
            | Statement::ShowHnswScanSetting(_)
    )
}

fn after_guarded_ddl_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::transaction_feature_not_supported(
        "this statement is not supported after privilege, policy, role or trigger DDL inside a transaction block yet",
    )
}

fn schema_scoped_ddl_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::transaction_feature_not_supported(
        "DDL on relations inside a schema is not supported inside a transaction block yet",
    )
}

/// DDL を積んだ Tx に対し、`parsed` を実行してよいか判定する（暫定の許可リスト。受け皿は #3311。V1・V5・V7）。
///
/// 許可するのは、本モジュールが受理する DDL・Tx 制御・セッション変数の `SET`/`SHOW`・結線を済ませた
/// `INSERT ... VALUES`・読み取り（`INSERT ... SELECT` は結線まで拒否）（`USING PLAN`・`EXPLAIN`・ルーチン呼び出し・集合返却関数・ビュー展開を伴う形は `view_definition` 側で拒否）。
/// 表・索引を DROP した後は、確定済みカタログでの解析・ビュー展開が古い名前を見るため DDL 以外を拒否する。
/// DDL を積んでいない Tx では何もしない。文言は文の種類だけで、対象の名前・存在を含めない。
pub(super) fn check_post_ddl_statement(
    session: &SessionState,
    txn: &SessionTransaction<'_>,
    parsed: &ParsedSql,
) -> Result<(), SqlSurfaceError> {
    if !txn.has_ddl() {
        return Ok(());
    }
    let removed = txn.ddl_removed();
    // Issue #3315: SCHEMA の DDL を積んだ後は、後続文の `search_path` の解決が確定済みカタログを読むため、
    // SCHEMA の DDL・`SET`/`SHOW` 以外を拒否する（暫定。受け皿は #3311）。
    let schema_changed = txn.ddl_schema_changed();
    // 一時カタログ・`search_path` の振り分けの包みは中身で判定する（包んだままだと許可対象の文も拒否される）。
    let parsed = parsed.peel_session_route();
    // SQL-80・Issue #3316: 権限・ポリシー・ロール・トリガ・関数削除の DDL を積んだ Tx は、権限ゲート・ポリシー
    // 適用・トリガの発火・関数の解決が確定済みカタログを読むため、古い判定で後続文を通さない（fail-closed）。
    // DDL・Tx 制御・セッション変数の `SET`/`SHOW` だけを許す。文言は文の種類だけで、名前・存在を含めない。
    // Issue #3315: SCHEMA の DDL を積んだ後は、権限・ポリシー等の DDL や表・索引の DDL を含め、名前解決が
    // 確定済みカタログの旧スキーマを見るため、SCHEMA の DDL・Tx 制御・セッション変数の `SET`/`SHOW` 以外を
    // 拒否する（fail-closed。保護対象 DDL の分岐より先に適用する）。
    if schema_changed {
        let allowed = match parsed {
            ParsedSql::CreateSchema(_)
            | ParsedSql::DropSchema(_)
            | ParsedSql::AlterSchema(_)
            | ParsedSql::Transaction(_)
            | ParsedSql::SetTransaction(_) => true,
            ParsedSql::Statement(stmt) => is_session_variable_statement(stmt),
            _ => false,
        };
        return if allowed {
            Ok(())
        } else {
            Err(SqlSurfaceError::transaction_feature_not_supported(
                "this statement is not supported after SCHEMA DDL inside a transaction block yet",
            ))
        };
    }
    if txn.has_guarded_ddl() {
        let allowed = match parsed {
            ParsedSql::CreateTable(_)
            | ParsedSql::DropTable(_)
            | ParsedSql::AlterTable(_)
            | ParsedSql::CreateIndex(_)
            | ParsedSql::DropIndex(_)
            | ParsedSql::Transaction(_)
            | ParsedSql::SetTransaction(_) => true,
            // Issue #3315: SCHEMA の DDL は常に、VIEW・SEQUENCE・ENUM 型の DDL は SCHEMA の DDL を積む前なら、
            // 保護対象 DDL の後も受理する（拡張 Execute 経路と同じ結果にそろえる）。
            ParsedSql::CreateSchema(_) | ParsedSql::DropSchema(_) | ParsedSql::AlterSchema(_) => {
                true
            }
            ParsedSql::CreateView(_)
            | ParsedSql::DropView(_)
            | ParsedSql::CreateSequence(_)
            | ParsedSql::DropSequence(_)
            | ParsedSql::AlterSequence(_)
            | ParsedSql::CreateType(_)
            | ParsedSql::DropType(_) => !schema_changed,
            ParsedSql::Statement(stmt) => is_session_variable_statement(stmt),
            other => is_security_ddl(other),
        };
        return if allowed {
            Ok(())
        } else {
            Err(after_guarded_ddl_unsupported())
        };
    }
    let allowed = match parsed {
        ParsedSql::CreateTable(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_) => !schema_changed,
        // Issue #3315: SCHEMA の DDL は SCHEMA の DDL を積んだ後も受理する。VIEW・SEQUENCE・ENUM 型の DDL は
        // SCHEMA の DDL を積んだ後は未修飾名の作成先が確定済みカタログの `search_path` で決まるため拒否する。
        ParsedSql::CreateSchema(_) | ParsedSql::DropSchema(_) | ParsedSql::AlterSchema(_) => true,
        ParsedSql::CreateView(_)
        | ParsedSql::DropView(_)
        | ParsedSql::CreateSequence(_)
        | ParsedSql::DropSequence(_)
        | ParsedSql::AlterSequence(_)
        | ParsedSql::CreateType(_)
        | ParsedSql::DropType(_) => !schema_changed,
        other if is_security_ddl(other) => true,
        // `INSERT ... SELECT` は挿入先スキーマを確定済みカタログから取得する（束縛・実行とも）ため、同じ Tx で
        // 作った表・追加した列を解決できない。overlay 結線までは fail-closed で拒否する（受け皿は #3311）。
        ParsedSql::Insert(_) => !removed && !schema_changed,
        ParsedSql::Statement(stmt) => match stmt {
            Statement::SetSearchMode { .. }
            | Statement::SetTimeZone { .. }
            | Statement::ShowTimeZone
            | Statement::SetSearchPath { .. }
            | Statement::ShowSearchPath
            | Statement::SetRowSecurity { .. }
            | Statement::ShowRowSecurity
            | Statement::ShowRestoreGeneration
            | Statement::SetStatementTimeout { .. }
            | Statement::ShowStatementTimeout
            | Statement::SetHnswEfSearch { .. }
            | Statement::ShowHnswEfSearch
            | Statement::SetHnswScanSetting { .. }
            | Statement::ShowHnswScanSetting(_) => true,
            Statement::Select(v) => !removed && !schema_changed && v.using_plan().is_none(),
            Statement::Aggregate(_)
            | Statement::Scan(_)
            | Statement::Join(_)
            | Statement::SetOperation(_) => !removed && !schema_changed,
            Statement::ConstSelect(v) => {
                !removed
                    && !schema_changed
                    && !crate::sql::const_select::calls_routine(v, session.udfs())
            }
            _ => false,
        },
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(SqlSurfaceError::transaction_feature_not_supported(
            "this statement is not supported after DDL inside a transaction block yet",
        ))
    }
}

/// 拡張クエリの Execute（Parse・Bind 済みの [`ParsedSql`] を実行する入口）で、DDL を積んだ Tx に実行してよいか
/// 判定する（SQL-80・Issue #3313。V7。受け皿は #3311）。Parse・Bind は確定済みカタログで文を束縛・検証して
/// おり、同じ Tx で変更した表のスキーマと食い違いうるため、DDL・Tx 制御・`SET`/`SHOW` だけを許す。
/// DDL を積んでいない Tx では何もしない。永続側を包んだ文（`SessionRouted`）は中身で判定する。
pub(super) fn check_extended_execute_after_ddl(
    txn: &SessionTransaction<'_>,
    parsed: &ParsedSql,
) -> Result<(), SqlSurfaceError> {
    if !txn.has_ddl() {
        return Ok(());
    }
    // Issue #3314: Tx 内で禁じる文（分割実行。SQL-32）の `25001` は、DDL 後の `0A000` より先に返す。
    // ここでは素通りさせ、`execute_parsed_in_txn_inner` の既存の `25001` 判定へ到達させる。
    if super::is_partitioned_write(parsed) {
        return Ok(());
    }
    let schema_changed = txn.ddl_schema_changed();
    let inner = match parsed {
        ParsedSql::SessionRouted(routed) if routed.is_persistent() => routed.inner(),
        other => other,
    };
    if schema_changed {
        let allowed = match inner {
            ParsedSql::CreateSchema(_)
            | ParsedSql::DropSchema(_)
            | ParsedSql::AlterSchema(_)
            | ParsedSql::Transaction(_)
            | ParsedSql::SetTransaction(_) => true,
            ParsedSql::Statement(stmt) => is_session_variable_statement(stmt),
            _ => false,
        };
        return if allowed {
            Ok(())
        } else {
            Err(SqlSurfaceError::transaction_feature_not_supported(
                "this statement is not supported after SCHEMA DDL inside a transaction block yet",
            ))
        };
    }
    let allowed = match inner {
        ParsedSql::CreateTable(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_) => !schema_changed,
        ParsedSql::CreateSchema(_)
        | ParsedSql::DropSchema(_)
        | ParsedSql::AlterSchema(_)
        | ParsedSql::Transaction(_)
        | ParsedSql::SetTransaction(_) => true,
        // Issue #3315: VIEW・SEQUENCE・ENUM 型の DDL（SCHEMA の DDL を積んだ後は拒否）。
        ParsedSql::CreateView(_)
        | ParsedSql::DropView(_)
        | ParsedSql::CreateSequence(_)
        | ParsedSql::DropSequence(_)
        | ParsedSql::AlterSequence(_)
        | ParsedSql::CreateType(_)
        | ParsedSql::DropType(_) => !schema_changed,
        ParsedSql::Statement(stmt) => is_session_variable_statement(stmt),
        other => is_security_ddl(other),
    };
    if allowed {
        Ok(())
    } else {
        Err(SqlSurfaceError::transaction_feature_not_supported(
            "this statement is not supported after DDL inside a transaction block yet",
        ))
    }
}

impl EngineCore {
    /// 明示・暗黙 Tx の表・索引の DDL を、Tx の共有 write txn へ積む（`execute_in_active_txn_inner` の
    /// `CreateTable`・`DropTable`・`AlterTable`・`CreateIndex`・`DropIndex` の腕から呼ばれる）。
    /// 判定順・失敗時の Tx の遷移はモジュールドキュメントのとおり。呼び出し元が `Err` で Tx を `Failed` にする。
    ///
    /// Issue #3314: 本体の前に Tx の持続時間上限の期限を張り（writer の待機と既存行の走査ループの確認点が
    /// 打ち切る。`statement_timeout` との早い方が勝つ）、Tx の期限で打ち切られた場合は復帰不可の `Failed` と
    /// `54000` にする（文の開始時点の上限超過と同じ契約）。確認点はすべて commit 前にあるため副作用は残らない。
    pub(super) fn execute_table_ddl_in_txn<'e>(
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
        let result = self.execute_table_ddl_in_txn_inner(ctx, session, txn, parsed);
        drop(guard);
        match result {
            Err(SqlSurfaceError::QueryCanceled {
                reason: crate::interrupt::InterruptReason::StatementTimeout,
            }) if txn.duration_exceeded() => Err(txn.fail_duration_exceeded()),
            other => other,
        }
    }

    /// [`Self::execute_table_ddl_in_txn`] の本体（期限を張った区間の内側で実行される）。
    fn execute_table_ddl_in_txn_inner<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        match parsed {
            ParsedSql::CreateTable(stmt) => {
                require_ddl(session, DdlStatementKind::CreateTable)?;
                self.require_creation_target(
                    ctx,
                    session,
                    stmt.public_qualified,
                    &stmt.table_name,
                )?;
                if is_schema_key(&stmt.table_name) {
                    return Err(schema_scoped_ddl_unsupported());
                }
                txn.ensure_writer(ctx)?;
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                // 永続表の FOREIGN KEY がこのセッションの一時表の名前に解決される場合は `42P16`
                // （autocommit の腕と同じ。`IF NOT EXISTS` で同名が既にあるときは no-op 成功が先）。
                let existing_noop = stmt.if_not_exists
                    && crate::catalog::relation_name_exists_in_txn(write_txn, &stmt.table_name)
                        .map_err(|_| internal())?;
                if !existing_noop {
                    // 何も書かない拒否（`42P16`・SERIAL/IDENTITY の `0A000` 等）は、DDL を積んだ印を立てる前に
                    // 返す（`ROLLBACK TO` で戻せるままにする）。
                    if stmt
                        .foreign_keys
                        .iter()
                        .any(|fk| session.temp_tables().contains(fk.parent_table()))
                    {
                        return Err(SqlSurfaceError::invalid_table_definition(
                            stmt.table_name.clone(),
                        ));
                    }
                    crate::sql::ddl::precheck_create_table_in_txn(
                        &self.storage,
                        stmt,
                        &self.schema_limits,
                    )?;
                }
                // 既存名への `IF NOT EXISTS` は何も変えない成功のため、DDL を積んだ印を立てない。
                if !existing_noop {
                    txn.mark_ddl_started();
                }
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let _owner_guard = self.arm_created_relation_owner(ctx, session, &stmt.table_name);
                let outcome = crate::sql::ddl::execute_create_table_in_txn(
                    &self.storage,
                    write_txn,
                    stmt,
                    &self.schema_limits,
                )?;
                txn.mark_ddl_done(false, Vec::new());
                Ok(SqlOutcome::CreateTable(outcome))
            }
            ParsedSql::DropTable(stmt) => {
                // 一時表だけ・混在の `DROP TABLE` は、一時側と永続側の 2 つのストアにまたがるため本入口では
                // 扱わない（`0A000`。判定は自セッションの一時カタログだけで決まり、永続表の存否は観測できない）。
                match crate::sql::ddl::classify_drop_targets(session, &stmt.table_names) {
                    Some(false) => {}
                    _ => {
                        return Err(SqlSurfaceError::transaction_feature_not_supported(
                            "DROP TABLE of temporary tables is not supported inside a transaction block yet",
                        ));
                    }
                }
                require_ddl(session, DdlStatementKind::DropTable)?;
                if stmt.table_names.iter().any(|n| is_schema_key(n)) {
                    return Err(schema_scoped_ddl_unsupported());
                }
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, evict) =
                    crate::sql::ddl::execute_drop_table_in_txn(&self.storage, write_txn, stmt)?;
                if outcome.skipped {
                    // `IF EXISTS` で対象が無い no-op は印を巻き戻す。
                    txn.restore_ddl_started(snap);
                }
                txn.mark_ddl_done(!outcome.skipped, evict);
                Ok(SqlOutcome::DropTable(outcome))
            }
            ParsedSql::AlterTable(stmt) => {
                require_ddl(session, DdlStatementKind::AlterTable)?;
                crate::sql::schema_dependency::check_alter_table(stmt)?;
                // SQL-80・Issue #3316: 行セキュリティだけのアクションは権限・ポリシーの DDL と同じ入口へ回す
                // （他のアクションと混ぜた形は下の `0A000`）。
                if let crate::sql::allowlist::ValidatedAlterTable::RowSecurity(rs) = stmt {
                    // RLS-13（Issue #2275）: 自動コミット側と同じく、同名の一時表が永続表を隠しているときは
                    // 永続表の有効化状態を黙って変えない（fail-closed の `0A000`）。
                    if session.temp_tables().contains(&rs.table_name) {
                        return Err(SqlSurfaceError::feature_not_supported(
                            "ROW LEVEL SECURITY is not supported for temporary tables",
                        ));
                    }
                    return self.execute_row_security_in_txn(ctx, txn, rs);
                }
                let Some(table_name) = crate::sql::ddl::alter_table_in_txn_target(stmt) else {
                    return Err(SqlSurfaceError::transaction_feature_not_supported(
                        "this ALTER TABLE action is not supported inside a transaction block yet",
                    ));
                };
                if is_schema_key(table_name) {
                    return Err(schema_scoped_ddl_unsupported());
                }
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let (outcome, evict, changed) = crate::sql::ddl::execute_alter_table_in_txn(
                    &self.storage,
                    write_txn,
                    stmt,
                    &self.schema_limits,
                )?;
                if !changed {
                    // 全アクションが冪等な no-op（`ADD COLUMN IF NOT EXISTS` の既存列等）は印を巻き戻す。
                    txn.restore_ddl_started(snap);
                }
                txn.mark_ddl_done(false, evict);
                Ok(SqlOutcome::AlterTable(outcome))
            }
            ParsedSql::CreateIndex(stmt) => {
                // 権限の無いセッションにはカタログを引かず、従来どおりの `0A000`（構文段で組み込み以外の
                // 関数に当たったときの拒否。権限検査より前）を返す。存在情報を出さない（autocommit の腕と同じ）。
                if let Some(pending) = stmt.pending_user_function.as_ref() {
                    if require_ddl(session, DdlStatementKind::CreateIndex).is_err() {
                        return Err(crate::sql::index_expr::udf_unsupported(pending.site));
                    }
                }
                require_ddl(session, DdlStatementKind::CreateIndex)?;
                if let Some(pending) = stmt.pending_user_function.as_ref() {
                    return Err(crate::sql::index_udf::index_user_function_verdict(
                        &self.storage,
                        ctx.tenant_id(),
                        session,
                        pending,
                    ));
                }
                if is_schema_key(stmt.table()) || is_schema_key(stmt.name()) {
                    return Err(schema_scoped_ddl_unsupported());
                }
                txn.ensure_writer(ctx)?;
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                // 同名の relation が既にある場合は構築（走査）に入る前に結果が決まる（`IF NOT EXISTS` の
                // no-op 成功・名前衝突のエラー）。既存行の走査（一意・式・述語つき・GIN）は走査ループの
                // 確認点が Tx の残り時間で打ち切る（Issue #3314）。
                let name_taken = crate::catalog::namespace::sibling_key(stmt.table(), stmt.name())
                    .ok()
                    .and_then(|key| {
                        crate::catalog::relation_name_exists_in_txn(write_txn, &key).ok()
                    })
                    .unwrap_or(false);
                // 同名の relation があり `IF NOT EXISTS` なら何も変えない成功のため、印を立てない。
                if !(name_taken && stmt.if_not_exists()) {
                    txn.mark_ddl_started();
                }
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::ddl::execute_create_index_in_txn(write_txn, stmt)?;
                txn.mark_ddl_done(false, Vec::new());
                Ok(SqlOutcome::CreateIndex(outcome))
            }
            ParsedSql::DropIndex(stmt) => {
                require_ddl(session, DdlStatementKind::DropIndex)?;
                if is_schema_key(stmt.name()) {
                    return Err(schema_scoped_ddl_unsupported());
                }
                txn.ensure_writer(ctx)?;
                let snap = txn.mark_ddl_started();
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                let outcome = crate::sql::ddl::execute_drop_index_in_txn(write_txn, stmt)?;
                if outcome.skipped() {
                    txn.restore_ddl_started(snap);
                }
                txn.mark_ddl_done(!outcome.skipped(), Vec::new());
                Ok(SqlOutcome::DropIndex(outcome))
            }
            // VIEW・SEQUENCE・SCHEMA・ENUM 型の DDL は `txn_ddl_objects.rs`（Issue #3315）。
            ParsedSql::CreateView(_)
            | ParsedSql::DropView(_)
            | ParsedSql::CreateSequence(_)
            | ParsedSql::DropSequence(_)
            | ParsedSql::AlterSequence(_)
            | ParsedSql::CreateSchema(_)
            | ParsedSql::DropSchema(_)
            | ParsedSql::AlterSchema(_)
            | ParsedSql::CreateType(_)
            | ParsedSql::DropType(_) => {
                self.execute_object_ddl_in_txn_inner(ctx, session, txn, parsed)
            }
            // 呼び出し元の腕が上の種類だけを渡す。到達したら fail-closed。
            _ => Err(internal()),
        }
    }
}
