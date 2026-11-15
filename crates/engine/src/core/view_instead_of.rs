//! ビューへの `INSERT`／`UPDATE`／`DELETE` を INSTEAD OF ROW トリガへ振り替える実行本体
//! （TABLE-45・TABLE-43・Issue #3193。設計記録: `docs/design/trigger-instead-of.md`）。
//!
//! 役割: `core.rs` のビュー書き込み腕（`ParsedSql::ViewInsert`／`ViewUpdate`／`ViewDelete`。autocommit・
//! 明示トランザクション）が、書き込みテナント自身の scope に該当イベントの INSTEAD OF トリガがあるときだけ
//! 本モジュールへ振り分ける（トリガが無ければ従来の自動書き換え経路。更新可能ビューでもトリガがあれば
//! トリガを優先する。PostgreSQL と同じ）。本モジュールは次を担う。
//!
//! - ビューの公開列から行型（[`TableSchema`]）を合成する（`view_output_columns`・`matview` と同じ導出器を
//!   再利用し、第 2 の導出器は作らない。疑似列 `id` を持たない採番方式の形）
//! - INSERT: `VALUES` を既存の束縛器（`bind_insert_form`）で合成スキーマへ束縛し、行ごとに発火する
//! - UPDATE／DELETE: OLD 行を、ビューを読む `SELECT`（文の `WHERE` 以降のトークンを再利用。SQL 文字列は組み立てない）を
//!   外側の write txn を読み取り源にして実行して得る。UPDATE の NEW は OLD に `SET` を適用して作る
//! - 行ごとに `trigger::fire::fire_instead_of_row` を呼び、`NULL` 返却でない行だけを影響行数に数える
//! - 文全体は単一 write txn・単一 commit（トリガ内の例外は文全体の取り消し）
//!
//! テナント境界（P0）: トリガ本体は書き込みを発行したテナントの `PolicyContext` で、同じ write txn の中でだけ
//! 実行される（`TriggerWriteHandle`。文脈は差し替えられない）。OLD 行はビュー本文を呼び出しセッション自身の
//! `PolicyContext` で評価して得るため、他テナントの行・作成者の可視性は材料にならない（RLS-10 (b)）。
//! トリガの有無の判定は書き込みテナントの scope のカタログだけで行い、他テナントの定義に依存しない。
//! 未対応の形（`RETURNING`・`ON CONFLICT`・`PARTITIONED`・`USING OPERATION_ID` 等）は固定文言の `0A000` で
//! 拒否する（fail-closed）。

use super::{EngineCore, ParsedSql};
use crate::catalog::trigger::{list_triggers_in_txn, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::catalog::{RowIdentity, TableSchema};
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::{
    SqlSurfaceError, ValidatedUpdateForm, ValidatedViewDelete, ValidatedViewInsert,
    ValidatedViewUpdate,
};
use crate::sql::exec::{ColumnMeta, DeleteOutcome, InsertOutcome, UpdateOutcome};
use crate::sql::lexer::{Keyword, Token};
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::trigger::call::TriggerOp;
use crate::trigger::write_handle::TriggerWriteHandle;

/// 実行モード。autocommit か、明示トランザクションの共有 write txn か。
pub(super) enum InsteadOfMode<'a> {
    Autocommit,
    InTxn(&'a redb::WriteTransaction),
}

/// 振り分け対象のビュー書き込み文。
pub(super) enum InsteadOfStmt<'a> {
    Insert(&'a ValidatedViewInsert),
    Update(&'a ValidatedViewUpdate),
    Delete(&'a ValidatedViewDelete),
}

impl InsteadOfStmt<'_> {
    fn view_name(&self) -> &str {
        match self {
            Self::Insert(s) => s.view_name(),
            Self::Update(s) => s.view_name(),
            Self::Delete(s) => s.view_name(),
        }
    }

    fn event(&self) -> u8 {
        match self {
            Self::Insert(_) => TriggerEvents::INSERT,
            Self::Update(_) => TriggerEvents::UPDATE,
            Self::Delete(_) => TriggerEvents::DELETE,
        }
    }
}

/// 未対応の形の固定文言（値・ビュー名・他テナントの情報を含めない）。
const FORM_UNSUPPORTED: &str =
    "this statement form is not supported for views with INSTEAD OF triggers";

fn unsupported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

/// 書き込みゲートの待機超過・commit の失敗をクライアント向けの契約へ写す。待機超過は `55P03`
/// （文の期限が先に来ていたなら `57014`。他の書き込み経路と同じ）、他は固定文言の分類。
fn instead_of_catalog_error(e: crate::catalog::CatalogError) -> SqlSurfaceError {
    match e {
        crate::catalog::CatalogError::WriteLockTimeout => crate::interrupt::lock_wait_error(),
        other => crate::catalog::table_lookup_error(other),
    }
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// 結果列のメタデータから列名を取る（合成スキーマとの突き合わせ用）。
fn meta_name(m: &ColumnMeta) -> &str {
    match m {
        ColumnMeta::Id => "id",
        ColumnMeta::AliasedId { name }
        | ColumnMeta::Scalar { name, .. }
        | ColumnMeta::Computed { name, .. }
        | ColumnMeta::Record { name, .. } => name.as_str(),
    }
}

impl EngineCore {
    /// 書き込みテナント自身の scope に、`view_key` の `event` に該当する INSTEAD OF ROW トリガがあるか。
    /// `in_txn` が `Some` なら明示 Tx の write txn から、`None`（autocommit）なら確定済みカタログから読む。
    /// 他テナントの定義は構造上引けず、有無は応答に出ない。カタログを読めなければ固定文言の内部エラー
    /// （「無い」側へは倒さない。fail-closed）。
    pub(super) fn view_has_instead_of(
        &self,
        ctx: &PolicyContext,
        view_key: &str,
        event: u8,
        in_txn: Option<&redb::WriteTransaction>,
    ) -> Result<bool, SqlSurfaceError> {
        let defs = match in_txn {
            Some(w) => list_triggers_in_txn(w, ctx.tenant_id(), view_key),
            None => self.storage.list_triggers(ctx.tenant_id(), view_key),
        }
        .map_err(|_| internal("trigger catalog operation failed"))?;
        Ok(defs.iter().any(|t| {
            t.timing == TriggerTiming::InsteadOf
                && t.level == TriggerLevel::Row
                && t.events.contains(event)
        }))
    }

    /// autocommit 用: 書き込みゲート（単一ライタ）を取得してから、その同じカタログで [`Self::view_has_instead_of`]
    /// と同じ判定を行う（取得前の確定済みカタログで判定すると、判定後に commit した並行 `CREATE TRIGGER` を
    /// 見落として基表へ直接書く。D3・D5）。判定後は書き込み txn を abort で閉じる。残る隙間は、基表書き込み側の
    /// 書き込み txn 冒頭の再照合（[`crate::sql::view_write_scope::InsteadOfGuard`]）が塞ぐ。
    pub(super) fn autocommit_view_has_instead_of(
        &self,
        ctx: &PolicyContext,
        view_key: &str,
        event: u8,
    ) -> Result<bool, SqlSurfaceError> {
        let write_txn = self
            .storage
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)
            .map_err(instead_of_catalog_error)?;
        let hit = self.view_has_instead_of(ctx, view_key, event, Some(&write_txn))?;
        drop(write_txn);
        Ok(hit)
    }

    /// Describe（拡張クエリの Bind・Describe）用: セッション自身のテナントの scope に該当イベントの INSTEAD OF
    /// トリガがあるビューなら、`RETURNING` なしの文として列メタ無しで受理してよい（`true`）。`RETURNING` 付きは
    /// 未対応形として固定文言の `0A000`。トリガが無い、またはテナントが未束縛なら `false`（従来どおり自動書き換えの
    /// 判定へ進み、自動更新不可は `55000`）。判定は呼び出しセッション自身の scope だけで行い、他テナントの定義に
    /// 依存しない。
    pub(super) fn describe_view_instead_of(
        &self,
        session: &SessionState,
        stmt: &InsteadOfStmt<'_>,
    ) -> Result<bool, SqlSurfaceError> {
        let Some(tenant) = session.resolution_tenant() else {
            return Ok(false);
        };
        let defs = self
            .storage
            .list_triggers(tenant, stmt.view_name())
            .map_err(|_| internal("trigger catalog operation failed"))?;
        let hit = defs.iter().any(|t| {
            t.timing == TriggerTiming::InsteadOf
                && t.level == TriggerLevel::Row
                && t.events.contains(stmt.event())
        });
        if hit {
            Self::reject_unsupported_instead_of_form(stmt)?;
        }
        Ok(hit)
    }

    /// 振り分け先の入口。`stmt` が `stmt.event()` の INSTEAD OF トリガを持つビューへの書き込みであることは
    /// 呼び出し元が確認済み。戻り値の第 2 要素はトリガ本体が書いた表（明示 Tx の `mark_written` 用）。
    pub(super) fn execute_view_instead_of(
        &self,
        mode: InsteadOfMode<'_>,
        ctx: &PolicyContext,
        session: &mut SessionState,
        stmt: InsteadOfStmt<'_>,
    ) -> Result<(SqlOutcome, Vec<String>), SqlSurfaceError> {
        Self::reject_unsupported_instead_of_form(&stmt)?;
        let view_key = stmt.view_name().to_string();
        let recheck = crate::sql::privilege_gate::write::RelationPrivilegeRecheck::for_session(
            session,
            ctx.tenant_id(),
            match &stmt {
                InsteadOfStmt::Insert(_) => crate::catalog::privilege::PRIV_INSERT,
                InsteadOfStmt::Update(_) => crate::catalog::privilege::PRIV_UPDATE,
                InsteadOfStmt::Delete(_) => crate::catalog::privilege::PRIV_DELETE,
            },
        );
        match mode {
            InsteadOfMode::Autocommit => {
                // 書き込みゲート（単一ライタ）を取ってからビューの行型を導く（カタログが書き込み中に変わらない。
                // matview の作成と同じ順序）。
                let write_txn = self
                    .storage
                    .begin_write_txn()
                    .map_err(crate::catalog::convert_storage_error)
                    .map_err(instead_of_catalog_error)?;
                // Parse 後・書き込みゲート取得前の REVOKE を書き込みの直前に検出する（他の DML 経路と同じ。
                // 照会失敗も拒否側。fail-closed）。要求ビットは書き込み権限のみ（OLD 行の内部取得は SELECT を要求しない）。
                Self::recheck_view_write_privilege(recheck.as_ref(), &write_txn, &view_key)?;
                let schema = self.instead_of_view_schema(session, &view_key)?;
                let (outcome, tables) = self
                    .run_instead_of(&write_txn, false, ctx, session, &stmt, &view_key, &schema)?;
                if tables.is_empty() {
                    // 何も書いていない（全行が `NULL` 返却・0 行）。commit せず abort で閉じる。
                    drop(write_txn);
                } else {
                    crate::interrupt::check()?;
                    crate::recovery::commit_boundary::commit(write_txn)
                        .map_err(crate::catalog::convert_storage_error)
                        .map_err(instead_of_catalog_error)?;
                }
                Ok((outcome, tables))
            }
            InsteadOfMode::InTxn(write_txn) => {
                Self::recheck_view_write_privilege(recheck.as_ref(), write_txn, &view_key)?;
                let schema = self.instead_of_view_schema(session, &view_key)?;
                self.run_instead_of(write_txn, true, ctx, session, &stmt, &view_key, &schema)
            }
        }
    }

    /// 書き込み txn の内側で、ビューへの書き込み権限（INSERT／UPDATE／DELETE のいずれか 1 ビット）を再判定する。
    /// `recheck` が `None`（未束縛・運用者権限）なら従来どおり何もしない。権限不足・照会失敗は `42501` 系の
    /// 固定拒否（他テナントの情報を含めない）。
    fn recheck_view_write_privilege(
        recheck: Option<&crate::sql::privilege_gate::write::RelationPrivilegeRecheck>,
        write_txn: &redb::WriteTransaction,
        view_key: &str,
    ) -> Result<(), SqlSurfaceError> {
        if let Some(r) = recheck {
            r.check_in_txn(write_txn, &[view_key.to_string()])
                .map_err(|()| SqlSurfaceError::RelationPermissionDenied)?;
        }
        Ok(())
    }

    /// D4: 初版で対応しない形を固定文言の `0A000` で拒否する（`RETURNING`・`ON CONFLICT`・`PARTITIONED`・
    /// `USING OPERATION_ID`）。外側の台帳エントリの付け先が無く、黙って落とす fail-open を作らない。
    fn reject_unsupported_instead_of_form(stmt: &InsteadOfStmt<'_>) -> Result<(), SqlSurfaceError> {
        let bad = match stmt {
            InsteadOfStmt::Insert(s) => {
                s.shape.returning.is_some()
                    || s.shape.on_conflict.is_some()
                    || s.operation_id().is_some()
            }
            InsteadOfStmt::Update(s) => match &s.shape {
                ValidatedUpdateForm::Single(v) => v.returning.is_some() || v.operation_id.is_some(),
                ValidatedUpdateForm::Predicate(v) => {
                    v.returning().is_some()
                        || v.operation_id().is_some()
                        || v.partitioned().is_some()
                }
            },
            InsteadOfStmt::Delete(s) => match &s.shape {
                crate::sql::allowlist::DeleteStatement::SingleRow(v) => {
                    v.returning.is_some() || v.operation_id.is_some()
                }
                crate::sql::allowlist::DeleteStatement::Predicate(v) => {
                    v.returning().is_some()
                        || v.operation_id().is_some()
                        || v.partitioned().is_some()
                }
            },
        };
        if bad {
            return Err(unsupported(FORM_UNSUPPORTED));
        }
        Ok(())
    }

    /// D6: ビューの公開列（列別名適用後）・型から行型を合成する。`view_output_columns`（`CREATE OR REPLACE VIEW`・
    /// matview と同じ導出器）と `column_defs_from_view_columns` を再利用する。型を静的に持てない列などで
    /// 合成できなければ固定文言の `0A000`（fail-closed）。疑似列 `id` を持たない採番方式の形にする。
    fn instead_of_view_schema(
        &self,
        session: &SessionState,
        view_key: &str,
    ) -> Result<TableSchema, SqlSurfaceError> {
        let def = self
            .storage
            .view_definition(view_key)
            .map_err(crate::catalog::table_lookup_error)?
            .ok_or_else(|| SqlSurfaceError::UndefinedTable {
                name: view_key.to_string(),
            })?;
        let columns = self
            .view_output_columns(session, &def.body_sql, &def.column_aliases)
            .and_then(|cols| crate::sql::matview::column_defs_from_view_columns(&cols))
            .map_err(|_| {
                unsupported("INSTEAD OF triggers are not supported for this view's columns")
            })?;
        Ok(TableSchema::new(view_key, columns).with_row_identity(RowIdentity::Allocated))
    }

    /// 単一 write txn の中で、計画 → 行ごとの発火 → 後始末までを行う。`in_txn` は明示 Tx か（遅延 FK の
    /// 扱いを決める）。トリガ本体が書いた表を返す。
    #[allow(clippy::too_many_arguments)]
    fn run_instead_of(
        &self,
        write_txn: &redb::WriteTransaction,
        in_txn: bool,
        ctx: &PolicyContext,
        session: &mut SessionState,
        stmt: &InsteadOfStmt<'_>,
        view_key: &str,
        schema: &TableSchema,
    ) -> Result<(SqlOutcome, Vec<String>), SqlSurfaceError> {
        let runtime = self.trigger_runtime.bind(self);
        // 計画は書き込み txn の中で 1 回だけ読む（文の開始時に同じ txn から決める。D5）。ここで空なら、
        // 振り分けの判定後にトリガが DROP された競合なので、再試行を促す（`55P03`）。
        let plan = crate::trigger::plan::plan_instead_of(
            write_txn,
            ctx.tenant_id(),
            view_key,
            stmt.event(),
            Some(&runtime),
        )
        .map_err(crate::sql::exec::map_insert_write_error)?
        .ok_or(SqlSurfaceError::LockNotAvailable)?;
        let writes = TriggerWriteHandle::new(write_txn, ctx, Some(&runtime));
        let _clock_scope = self.trigger_firing_clock_scope()?;
        let _limits_scope = self.sparsevec_limits_scope();
        // 入れ子のトリガ（INSTEAD OF 本体の書いた表の AFTER ROW トリガが更に別表へ書く場合）が書いた表も
        // 遅延 FK の記録区間へ収集する。区間が無いと内側の記録が失敗し、遅延 FK をその場で検査して
        // しまう（明示 Tx は COMMIT 時検査という契約に反する）。
        let nested_scope = crate::tenant::trigger_writes::TriggerWrittenTablesScope::enter();

        let outcome = match stmt {
            InsteadOfStmt::Insert(v) => {
                // 既存の時刻（INSERT 入口が張った明示 Tx の開始時刻・外側の式スコープの時刻）を引き継ぐ。
                // 現在時刻で張り直すと、トリガ先の DEFAULT now() が Tx 開始時刻とずれる。
                // 時刻の文脈が無いときだけ、ここで 1 度確定する。
                let clock = match crate::sql::time_fn::current_default_clock()
                    .or_else(crate::sql::time_fn::current_clock)
                {
                    Some(c) => c,
                    None => {
                        let now = crate::sql::time_fn::now_micros()?;
                        crate::sql::time_fn::StatementClock {
                            txn_start_micros: now,
                            stmt_start_micros: now,
                        }
                    }
                };
                let _default_clock = crate::sql::time_fn::DefaultClockScope::enter(clock);
                let rows = self.instead_of_bind_insert(v, schema)?;
                let mut affected: u64 = 0;
                for values in rows {
                    let fired = crate::trigger::fire::fire_instead_of_row(
                        plan.runtime,
                        &writes,
                        view_key,
                        schema,
                        &plan.triggers,
                        TriggerOp::Insert,
                        None,
                        values,
                    )?;
                    if fired.is_some() {
                        affected = affected.saturating_add(1);
                    }
                }
                SqlOutcome::Insert(InsertOutcome {
                    rows_affected: affected,
                    incremental: None,
                })
            }
            InsteadOfStmt::Update(v) => {
                let assignments = match &v.shape {
                    ValidatedUpdateForm::Single(u) => &u.assignments,
                    ValidatedUpdateForm::Predicate(p) => &p.assignments,
                };
                let (literals, exprs) =
                    crate::sql::parser::bind_set_assignments(assignments, schema)?;
                let old_rows = self.instead_of_read_old_rows(
                    &writes,
                    ctx,
                    session,
                    view_key,
                    &v.where_tail,
                    schema,
                )?;
                let mut affected: u64 = 0;
                for old in old_rows {
                    let new_row = Self::instead_of_apply_set(schema, &old, &literals, &exprs)?;
                    let fired = crate::trigger::fire::fire_instead_of_row(
                        plan.runtime,
                        &writes,
                        view_key,
                        schema,
                        &plan.triggers,
                        TriggerOp::Update,
                        Some(&old),
                        new_row,
                    )?;
                    if fired.is_some() {
                        affected = affected.saturating_add(1);
                    }
                }
                SqlOutcome::Update(UpdateOutcome {
                    rows_affected: affected,
                })
            }
            InsteadOfStmt::Delete(v) => {
                let old_rows = self.instead_of_read_old_rows(
                    &writes,
                    ctx,
                    session,
                    view_key,
                    &v.where_tail,
                    schema,
                )?;
                let mut affected: u64 = 0;
                for old in old_rows {
                    let fired = crate::trigger::fire::fire_instead_of_row(
                        plan.runtime,
                        &writes,
                        view_key,
                        schema,
                        &plan.triggers,
                        TriggerOp::Delete,
                        Some(&old),
                        Vec::new(),
                    )?;
                    if fired.is_some() {
                        affected = affected.saturating_add(1);
                    }
                }
                SqlOutcome::Delete(DeleteOutcome {
                    rows_affected: affected,
                })
            }
        };

        // トリガ本体が書いた表の遅延 FK を確定する（fail-open の防止）。明示 Tx は COMMIT 時の検査対象
        // （`mark_written`）にし、autocommit は文の終わりに即時検査する。
        let mut written = writes.written_tables();
        for table in nested_scope.take() {
            if !written.iter().any(|w| w == &table) {
                written.push(table);
            }
        }
        if !in_txn {
            for table in &written {
                crate::constraint::enforce_deferred_foreign_keys_in_txn(
                    write_txn,
                    ctx.tenant_id(),
                    table,
                )
                .map_err(crate::sql::exec::map_insert_write_error)?;
            }
        }
        Ok((outcome, written))
    }

    /// INSERT の `VALUES` を合成スキーマへ束縛し、行値（`schema.columns` と同じ幅・順）の列にする。行数は
    /// `--max-insert-rows` で上限化する。時刻関数・`nextval` 等の揮発的な値の遅延適用が要る形は `0A000`。
    fn instead_of_bind_insert(
        &self,
        stmt: &ValidatedViewInsert,
        schema: &TableSchema,
    ) -> Result<Vec<Vec<Value>>, SqlSurfaceError> {
        self.check_insert_row_count_limit(stmt.shape.rows.len())?;
        let bounds = match crate::sql::parser::bind_insert_form(&stmt.shape, schema)? {
            crate::sql::parser::BoundInsertForm::Row(b) => vec![b],
            crate::sql::parser::BoundInsertForm::RowBatch(bs) => {
                self.validate_insert_row_batch_limits(&bs)?;
                bs
            }
            _ => return Err(unsupported(FORM_UNSUPPORTED)),
        };
        let mut rows: Vec<Vec<Value>> = Vec::new();
        rows.try_reserve_exact(bounds.len())
            .map_err(|_| internal("failed to reserve insert batch"))?;
        for b in bounds {
            if !b.volatile_default_columns.is_empty() {
                return Err(unsupported(FORM_UNSUPPORTED));
            }
            rows.push(b.values);
        }
        Ok(rows)
    }

    /// UPDATE／DELETE の OLD 行を、ビューを読む `SELECT * FROM <view> [WHERE ...]` で得る（D7）。トークン列は
    /// 文の `WHERE` 以降の写しから組み、SQL 文字列は組み立てない。外側の write txn を読み取り源にするため
    /// 同じ Tx の先の書き込みが見え、評価は呼び出しセッション自身の `PolicyContext` で行う（作成者の可視性を
    /// 引き継がない。RLS-10 (b)）。件数は結果行数の上限と影響行数の上限（`--max-dml-affected-rows`）で
    /// 確保前に頭打ちにする。
    fn instead_of_read_old_rows(
        &self,
        writes: &TriggerWriteHandle<'_>,
        ctx: &PolicyContext,
        session: &mut SessionState,
        view_key: &str,
        where_tail: &[Token],
        schema: &TableSchema,
    ) -> Result<Vec<Vec<Value>>, SqlSurfaceError> {
        let mut tokens: Vec<Token> = vec![
            Token::Keyword(Keyword::Select),
            Token::Punct('*'),
            Token::Keyword(Keyword::From),
            Token::Ident(view_key.to_string()),
        ];
        if !where_tail.is_empty() {
            tokens.push(Token::Keyword(Keyword::Where));
            tokens.extend(where_tail.iter().cloned());
        }
        // OLD 行の取得は内部処理であり、利用者の権限検査は元の DML 文の入口（書き込み先ビューの
        // INSERT／UPDATE／DELETE ビットと `WHERE` 内サブクエリ参照先の SELECT）で済んでいる。ここで通常の
        // SELECT ゲート（ビューへの SELECT 権限・全列要求）を通すと、DELETE／UPDATE 権限だけの主体や列単位付与が
        // 元の文が読まない列まで要求されて `42501` になるため、ゲートなしの解析入口を使う。SCHEMA 解決器・一時カタログ
        // の扱いは通常入口と同じ。ビュー本文は呼び出しセッション自身の `PolicyContext` で評価する（RLS-10 (b)）。
        let parsed = {
            let resolver = self.schema_resolver_for(ctx, session);
            let hits = crate::sql::temp_catalog::RelationHits::default();
            self.parse_tokens_gated_grant_aware(tokens, session, Some(&resolver), None, &hits)?
        };
        let stmt = match parsed {
            ParsedSql::Statement(stmt) => stmt,
            _ => return Err(unsupported(FORM_UNSUPPORTED)),
        };
        let _scopes = (
            crate::sql::jsonpath_fn::JsonPathLimitsScope::enter(self.jsonpath_limits),
            crate::sql::regex_op::RegexLimitsScope::enter(self.regex_limits),
        );
        let outcome =
            writes.with_read_source(|src| self.execute_read_statement(ctx, session, stmt, src))?;
        let SqlOutcome::Query(result) = outcome else {
            return Err(internal("unexpected result for view read"));
        };
        // 合成した行型と読み取り結果の列が一致することを確かめる（食い違いは黙って別の列へ詰めない）。
        if result.columns.len() != schema.columns.len()
            || result
                .columns
                .iter()
                .zip(&schema.columns)
                .any(|(m, c)| meta_name(m) != c.name)
        {
            return Err(internal("view columns changed during the statement"));
        }
        crate::sql::parser::check_dml_affected_rows_with_limit(
            result.rows.len(),
            self.dml_limits.max_affected_rows,
        )?;
        let mut out: Vec<Vec<Value>> = Vec::new();
        out.try_reserve_exact(result.rows.len())
            .map_err(|_| internal("failed to reserve view rows"))?;
        for row in result.rows {
            if row.cells.len() != schema.columns.len() {
                return Err(internal("view row width mismatch"));
            }
            let mut values: Vec<Value> = Vec::with_capacity(row.cells.len());
            for (cell, col) in row.cells.into_iter().zip(&schema.columns) {
                values.push(crate::sql::matview::cell_to_value(cell, &col.ty)?);
            }
            out.push(values);
        }
        Ok(out)
    }

    /// OLD 行に `SET` を適用して NEW の候補を作る。式は更新前の行（OLD）に対して評価し、他の `SET` の
    /// 結果は見えない（`SET a = 1, b = a` の `b` は更新前の `a`。PostgreSQL と同じ）。
    fn instead_of_apply_set(
        schema: &TableSchema,
        old: &[Value],
        literals: &[(usize, Value)],
        exprs: &[(usize, crate::sql::set_expr::BoundSetExpr)],
    ) -> Result<Vec<Value>, SqlSurfaceError> {
        let mut resolved: Vec<(usize, Value)> = literals.to_vec();
        if !exprs.is_empty() {
            let embedding: &[f32] = match schema
                .columns
                .iter()
                .position(|c| matches!(c.ty, crate::catalog::ColumnType::Vector(_)))
                .and_then(|i| old.get(i))
            {
                Some(Value::Vector(v)) => v.as_slice(),
                _ => &[],
            };
            let metadata = crate::row_codec::encode_scalar_columns(schema, old)
                .map_err(|_| internal("failed to encode view row"))?;
            let scanned = crate::row_codec::scan_scalar_columns(schema, &metadata)
                .map_err(|_| internal("failed to decode view row"))?;
            for (idx, set) in exprs {
                resolved.push((
                    *idx,
                    crate::sql::set_expr::eval_set_expr_to_value(set, 0, embedding, &scanned)?,
                ));
            }
        }
        let mut new_row = old.to_vec();
        for (idx, value) in resolved {
            match new_row.get_mut(idx) {
                Some(slot) => *slot = value,
                None => return Err(internal("SET column out of range")),
            }
        }
        Ok(new_row)
    }
}
