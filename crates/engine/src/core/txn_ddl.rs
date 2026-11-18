//! 明示・暗黙トランザクション内の表・索引の DDL（`CREATE TABLE`・`DROP TABLE`・`ALTER TABLE` の一部・
//! `CREATE INDEX`・`DROP INDEX`）の実行入口と、DDL を積んだ後の文の許可リスト（SQL-80・SQL-31・SQL-23・
//! TABLE-22・TASK-349 のポインタ、Issue #3313。設計記録: `docs/design/explicit-transaction.md`
//! 「明示トランザクション内の DDL の方式」）。
//!
//! 責務境界: `core.rs` の `execute_in_active_txn_inner`（明示 Tx と複数文メッセージの暗黙 Tx が共有する入口）が
//! DDL の腕から [`EngineCore::execute_table_ddl_in_txn`] を呼ぶ。実行本体は autocommit と同じ `sql::ddl` の
//! `*_in_txn`（共有 write txn を受け取る。カタログの書き込みは redb の txn に載るため ROLLBACK で消える）で、
//! 第 2 の DDL 実行器を作らない。commit は Tx の COMMIT が 1 回だけ行い、メモリ上の状態（採番器の現在値）は
//! `SessionTransaction::mark_ddl_done` が積んだ commit 後フックだけが変える。
//!
//! 判定順（autocommit の腕と同じ前段を同じ順で通す。権限の無い主体へ対象の有無を返さない）:
//! DDL 権限（`42501`）→ 作成先の検査 → 未対応の形の拒否（`0A000`。writer を取る前・カタログを読む前）→
//! writer の取得（`55P03`・`40001`）→ DDL 本体。本体が失敗した Tx は復帰不可の `Failed` になる
//! （`SessionTransaction::mark_ddl_started`）ため、部分的な DDL を COMMIT する経路は無い。
//!
//! 暫定の規則（受け皿は #3314。V1・V5・V7）: DDL を積んだ後に同じ Tx で実行できる文は、結線を済ませた最小の
//! 種類だけにする（[`check_post_ddl_statement`]）。`search_path` の解決・ビュー展開・権限ゲートが読む確定済み
//! カタログと、write txn 上の内容が食い違う経路を作らないため、それ以外は `0A000` で拒否する（fail-closed）。
//! ビュー経由の読みは `catalog.rs` の `TableLookup::view_definition` が重ね見の区間で `0A000` にする。
//! 既存行の走査を伴う `CREATE INDEX`（一意・式・述語つき・GIN）・`SET NOT NULL` は、期限の結線まで対象表に行がある間 `0A000` にする。
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
/// 解決が確定済みカタログを読むため DDL 後に別の表へ解決されうる。結線（#3314）までは `0A000`。
fn is_schema_key(name: &str) -> bool {
    name.starts_with('#')
}

fn schema_scoped_ddl_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::transaction_feature_not_supported(
        "DDL on relations inside a schema is not supported inside a transaction block yet",
    )
}

/// 既存行の走査を伴う DDL（`CREATE INDEX`・`SET NOT NULL`・生成列の `ADD COLUMN`）は、走査ループに Tx の残り時間
/// （`remaining_duration`）の期限が結線されていないため、対象表に行がある間は `0A000` にする
/// （共有 writer を持続時間上限を越えて保持しない。fail-closed。受け皿は #3314）。
fn reject_scan_ddl_on_nonempty_table(
    write_txn: &redb::WriteTransaction,
    table: &str,
) -> Result<(), SqlSurfaceError> {
    let has_rows = crate::catalog::table_has_rows_in_txn(write_txn, table)
        .map_err(|_| internal())?
        // 実体化ビューの行は別の行表にあるため、そちらも見る（既存行の走査が期限なしで走るのを防ぐ）。
        || crate::catalog::materialized::matview_has_rows_in_txn(write_txn, table)
            .map_err(|_| internal())?;
    if has_rows {
        return Err(SqlSurfaceError::transaction_feature_not_supported(
            "this DDL scans existing rows and is not supported on a non-empty table inside a transaction block yet",
        ));
    }
    Ok(())
}

/// `ALTER TABLE` が実際に既存行を走査しうるか。生成列の `ADD COLUMN`（既存列への `IF NOT EXISTS` を除く）と、`SET NOT NULL` のうち、対象列が現在すでに NOT NULL で、
/// かつ同じ文の他アクション（`DROP NOT NULL`・`DROP/ADD COLUMN`）が同列を変えないものは no-op のため走査しない。
/// 表・列が見つからない場合は実行側がエラーで返す（走査前）ので、ここでは走査なし扱いにする。
fn alter_needs_row_scan(
    write_txn: &redb::WriteTransaction,
    table: &str,
    stmt: &crate::sql::allowlist::ValidatedAlterTable,
) -> bool {
    use crate::sql::allowlist::ValidatedAlterTable as V;
    if !alter_scans_rows(stmt) {
        return false;
    }
    let Ok(schema) = crate::catalog::get_table_schema_in_txn_impl(write_txn, table, false) else {
        return false;
    };
    let actions: Vec<&V> = match stmt {
        V::Multiple(m) => m.actions.iter().collect(),
        other => vec![other],
    };
    actions.iter().any(|a| {
        if let V::AddColumn(x) = a {
            if x.generated.is_none() {
                return false;
            }
            // `ADD COLUMN IF NOT EXISTS` で既存列がある no-op は走査しない（同じ文で先に外す場合は走査あり）。
            let exists = schema.columns.iter().any(|c| c.name == x.column_name);
            let dropped_in_stmt = actions
                .iter()
                .any(|o| matches!(o, V::DropColumn(d) if d.column_name == x.column_name));
            return !(x.if_not_exists && exists && !dropped_in_stmt);
        }
        let V::AlterColumnSetNotNull(s) = a else {
            return false;
        };
        let Some(col) = schema.columns.iter().find(|c| c.name == s.column_name) else {
            // 現スキーマに無い列は、同じ文の ADD COLUMN で現れうるため走査ありとして扱う。
            return actions
                .iter()
                .any(|o| matches!(o, V::AddColumn(x) if x.column_name == s.column_name));
        };
        if col.nullable {
            return true;
        }
        actions.iter().any(|o| match o {
            V::AlterColumnDropNotNull(x) => x.column_name == s.column_name,
            V::DropColumn(x) => x.column_name == s.column_name,
            V::AddColumn(x) => x.column_name == s.column_name,
            _ => false,
        })
    })
}

/// `ALTER TABLE` のアクションに既存行を走査する `SET NOT NULL`・生成列の `ADD COLUMN` が含まれるか。
fn alter_scans_rows(stmt: &crate::sql::allowlist::ValidatedAlterTable) -> bool {
    use crate::sql::allowlist::ValidatedAlterTable as V;
    match stmt {
        V::AlterColumnSetNotNull(_) => true,
        // 生成列の追加は全テナントの既存行を計算して書き直す（`backfill_generated_column_in_txn`）。
        V::AddColumn(a) => a.generated.is_some(),
        V::Multiple(m) => m.actions.iter().any(alter_scans_rows),
        _ => false,
    }
}

/// DDL を積んだ Tx に対し、`parsed` を実行してよいか判定する（暫定の許可リスト。受け皿は #3314。V1・V5・V7）。
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
    // 一時カタログ・`search_path` の振り分けの包みは中身で判定する（包んだままだと許可対象の文も拒否される）。
    let parsed = parsed.peel_session_route();
    let allowed = match parsed {
        ParsedSql::CreateTable(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_) => true,
        // `INSERT ... SELECT` は挿入先スキーマを確定済みカタログから取得する（束縛・実行とも）ため、同じ Tx で
        // 作った表・追加した列を解決できない。overlay 結線までは fail-closed で拒否する（受け皿は #3314）。
        ParsedSql::Insert(_) => !removed,
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
            Statement::Select(v) => !removed && v.using_plan().is_none(),
            Statement::Aggregate(_)
            | Statement::Scan(_)
            | Statement::Join(_)
            | Statement::SetOperation(_) => !removed,
            Statement::ConstSelect(v) => {
                !removed && !crate::sql::const_select::calls_routine(v, session.udfs())
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
/// 判定する（SQL-80・Issue #3313。V7。受け皿は #3314）。Parse・Bind は確定済みカタログで文を束縛・検証して
/// おり、同じ Tx で変更した表のスキーマと食い違いうるため、DDL・Tx 制御・`SET`/`SHOW` だけを許す。
/// DDL を積んでいない Tx では何もしない。永続側を包んだ文（`SessionRouted`）は中身で判定する。
pub(super) fn check_extended_execute_after_ddl(
    txn: &SessionTransaction<'_>,
    parsed: &ParsedSql,
) -> Result<(), SqlSurfaceError> {
    if !txn.has_ddl() {
        return Ok(());
    }
    let inner = match parsed {
        ParsedSql::SessionRouted(routed) if routed.is_persistent() => routed.inner(),
        other => other,
    };
    let allowed = match inner {
        ParsedSql::CreateTable(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_)
        | ParsedSql::Transaction(_)
        | ParsedSql::SetTransaction(_) => true,
        ParsedSql::Statement(stmt) => matches!(
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
        ),
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

impl EngineCore {
    /// 明示・暗黙 Tx の表・索引の DDL を、Tx の共有 write txn へ積む（`execute_in_active_txn_inner` の
    /// `CreateTable`・`DropTable`・`AlterTable`・`CreateIndex`・`DropIndex` の腕から呼ばれる）。
    /// 判定順・失敗時の Tx の遷移はモジュールドキュメントのとおり。呼び出し元が `Err` で Tx を `Failed` にする。
    pub(super) fn execute_table_ddl_in_txn<'e>(
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
                let Some(table_name) = crate::sql::ddl::alter_table_in_txn_target(stmt) else {
                    return Err(SqlSurfaceError::transaction_feature_not_supported(
                        "this ALTER TABLE action is not supported inside a transaction block yet",
                    ));
                };
                if is_schema_key(table_name) {
                    return Err(schema_scoped_ddl_unsupported());
                }
                txn.ensure_writer(ctx)?;
                let write_txn = txn.write_txn().ok_or_else(internal)?;
                // 既に NOT NULL の列への `SET NOT NULL`・既存列への `ADD COLUMN IF NOT EXISTS` は走査しない
                // no-op のため、ゲートの対象外にする。何も書かない拒否は、DDL を積んだ印を立てる前に返す。
                if alter_needs_row_scan(write_txn, table_name, stmt) {
                    reject_scan_ddl_on_nonempty_table(write_txn, table_name)?;
                }
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
                // 既存行を走査する形（一意・式・述語つき・GIN）だけが対象。列の宣言だけの索引は走査しない。
                // 同名の relation が既にある場合は構築（走査）に入る前に結果が決まる（`IF NOT EXISTS` の
                // no-op 成功・名前衝突のエラー）ため、ゲートの対象外にする。
                let name_taken = crate::catalog::namespace::sibling_key(stmt.table(), stmt.name())
                    .ok()
                    .and_then(|key| {
                        crate::catalog::relation_name_exists_in_txn(write_txn, &key).ok()
                    })
                    .unwrap_or(false);
                if !name_taken
                    && (stmt.is_unique()
                        || stmt.predicate().is_some()
                        || stmt.gin_opclass().is_some()
                        || stmt.expression_keys().iter().any(Option::is_some))
                {
                    reject_scan_ddl_on_nonempty_table(write_txn, stmt.table())?;
                }
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
            // 呼び出し元の腕が上の 5 種類だけを渡す。到達したら fail-closed。
            _ => Err(internal()),
        }
    }
}
