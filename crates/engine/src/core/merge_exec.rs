//! `MERGE` の束縛入口（SQL-79・Issue #3352。親 #3349、設計記録は `docs/design/sql-merge.md`）。
//!
//! 責務境界: 構文検証済みの [`ValidatedMerge`] に対し、対象・元のスキーマを**同一の読み取りスナップショット**
//! から集めて `sql::merge_bind::bind_merge` へ渡すだけを担う。行・台帳・世代・writer・暗黙 Tx には一切触れず、
//! 束縛エラーは書き込みより前に返る（Execute・明示 Tx・Describe の各腕が同じ入口を通るため、
//! SQLSTATE がそろう）。実行は #3353・#3354、`RETURNING` は #3356、台帳は #3357 の担当で、
//! 対象外の形は固定文言の `0A000` で止まる。
//!
//! 元が部分クエリの形は、内側の検証済み `Statement` を Describe して列記述だけを取り（**実行しない**）、
//! 疑似列 `id` を持たない採番方式の合成スキーマへ写す。内側の表の RLS は実行時（#3353 以降）に書き込む
//! セッションの `PolicyContext` で読み取り経路が適用するため、本モジュールはテナント情報を持たず、
//! 他テナントの存在・件数を観測させる経路を作らない。
//!
//! 実行（#3353・#3354）: `WHEN MATCHED`・`WHEN NOT MATCHED BY SOURCE`・`WHEN NOT MATCHED [BY TARGET]` の MERGE は
//! [`EngineCore::execute_merge_in_txn`] が 3 段で実行する。第 1 段で全組の操作を決めて操作列を作り（行は書かない。
//! `tenant::enumerate_merge_candidates_readonly`）、第 2 段で `21000` と各上限を判定し、第 3 段で操作列を既存の
//! 書き込み経路（`tenant::update_rows_joined_unchecked`・`delete_rows_where_with_triggers`・
//! `sql::exec::execute_insert_batch_with_schema_in`）へ渡す。第 3 段より前の失敗は write txn を commit しないため
//! 副作用ゼロ（台帳も消費しない）。結合・一意化・上限は `sql::join::dml` の `UPDATE ... FROM` と同じ実行器を使い、
//! 第 2 の結合実行器・第 2 の書き込み経路は作らない。
//!
//! 外部保持の列挙（#3354）: 第 1 段の対象候補の列挙は、元の各行に「どれかの対象行に ON が真で一致したか」の
//! フラグを立てる（句の選択結果や DO NOTHING に関係なく、一致の定義は ON だけ）。一致 0 件の対象行は
//! `WHEN NOT MATCHED BY SOURCE` の句で、一致があるのに MATCHED の句がどれも発火しない対象行は何も受けない。
//! 列挙の後、フラグの立っていない元の行（結合キーが NULL の行を含む）を走査順に `WHEN NOT MATCHED [BY TARGET]`
//! の句で評価する。BY SOURCE は対象表の全候補を走査し得るため、走査行数の上限（`54000`）は第 1 段の列挙で数える。
//! 第 3 段の適用順は UPDATE の句ごと → DELETE → INSERT の句ごと（PostgreSQL の元の行順の逐次適用との差は
//! `docs/design/sql-merge.md` §10）。
//!
//! 規則の結線（#3355）: 利用者ポリシー（RLS-13）は、第 1 段の候補列挙が SELECT 側の `USING` で対象を絞り、UPDATE・
//! DELETE を選んだ行（MATCHED・BY SOURCE の両方）に各 `USING` を評価して違反を `42501` で拒否する（新しい行の
//! `WITH CHECK` は第 3 段の既存の検査点）。文トリガは `tenant::merge_triggers` が WHEN 句に現れた操作の種類ごとに
//! 入口（BEFORE）と出口（AFTER）で 1 回発火し、第 3 段の間は既存経路の文トリガを抑止する。ビュー対象は基底表へ
//! 書き換えて実行し（ビュー述語は第 1 段の候補の絞り込み、`WITH CHECK OPTION` は `ViewWriteScope` 経由の既存の
//! 検査点）、束縛時の指紋と INSTEAD OF の有無を writer 取得後に write txn で照合する。UPDATE 句と INSERT 句を
//! またぐ子側 FK 検査は先送りして全区間の後に 1 回検査する。
//!
//! `RETURNING`（#3356）: 束縛は両側スコープ（`sql::merge_bind`）、計画は元の列を走査投影へ載せ（`plan_merge`）、
//! 第 1 段が選んだ元の走査位置を記録し、第 3 段の既存の書き込み経路の capturing 版（`project` 引数）が書いた最終行を
//! `sql::dml_from_returning::MergeReturningSink` へ積む（UPDATE は更新後・DELETE は削除前・INSERT は書いた行）。
//! 結果行は対象候補の走査順 → 元の走査順の INSERT の順で返す（設計記録 §12）。該当トリガを持つ表との併用は
//! BEFORE STATEMENT より前に固定文言の `0A000` で拒否する。
//!
//! 対象外の形（台帳記録・INSTEAD OF トリガを持つビュー）は固定文言の `0A000` で文全体を拒否する
//! （受け皿は #3357。INSTEAD OF は対応しない）。
//!
//! 呼び出し元: `core.rs` の `ParsedSql::Merge` の腕（autocommit は暗黙 Tx 経由で明示 Tx と同じ腕・Describe）。

use super::EngineCore;
use crate::catalog::{RowIdentity, TableSchema};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{
    MergeMatchKind, MergeSource, SqlSurfaceError, Statement, ValidatedMerge,
};
use crate::sql::exec::{MergeOutcome, QueryResult};
use crate::sql::join::dml::JoinedProbe;
use crate::sql::join::merge_plan::{plan_merge, Choice, MergeEval, MergeSourceInput};
use crate::sql::merge_bind::{bind_merge, BoundMerge, BoundMergeAction, MergeBindInput};
use crate::sql::mode::SessionState;
use crate::sql::parser::BoundInsert;
use crate::sql::transaction::SessionTransaction;
use crate::sql::udf_call::UdfRegistry;
use crate::sql::view::{resolve_from, Resolved};
use crate::sql::SqlOutcome;
use crate::tenant::{DmlCandidate, PredicateDmlError, PredicateDmlOutcome, WriteTarget};
use std::collections::{BTreeMap, HashMap, HashSet};

/// 束縛に使った対象・元のスキーマ（実行が同じスナップショットの値で計画を組むために返す）。
pub(super) struct MergeBindSchemas {
    /// 対象の（基底）表スキーマ。
    pub(super) target_schema: TableSchema,
    /// 元のスキーマ（実表・単純形ビューの基底表・部分クエリの合成スキーマ）。
    pub(super) source_schema: TableSchema,
    /// 元が単純形ビューのときの、基底表の名前とビュー定義の述語。
    pub(super) source_view: Option<(String, Vec<crate::sql::allowlist::WherePredicate>)>,
    /// 対象がビューのときの、書き込み先の基底表・述語・`WITH CHECK OPTION` の検査述語・束縛時の指紋
    /// （Issue #3355。実行は基底表の経路へ流し、指紋は writer 取得後に write txn で照合する）。
    pub(super) target_view: Option<MergeTargetViewBinding>,
}

/// 対象ビューの束縛結果のうち、実行段が使う分（束縛と同じ読み取りスナップショット由来）。
pub(super) struct MergeTargetViewBinding {
    /// 対象に書かれたビュー名（INSTEAD OF の再照合・指紋の照合に使う）。
    pub(super) name: String,
    /// 連鎖を辿った先の実表名（実行はこの表へ書く）。
    pub(super) base_table: String,
    /// ビュー定義由来の述語（基底表の物理列名）。第 1 段の対象候補の絞り込みに使う。
    pub(super) view_predicates: Vec<crate::sql::allowlist::WherePredicate>,
    /// `WITH CHECK OPTION` の検査対象の述語（内側のビューが先頭）。
    pub(super) check_predicates: Vec<Vec<crate::sql::allowlist::WherePredicate>>,
    /// 束縛に使った読み取りスナップショットから計算した、ビュー連鎖と基底表スキーマの指紋。
    pub(super) fingerprint: Vec<u8>,
}

/// `MERGE` の WHEN 句に現れた操作の種類に対応する、トリガのイベントビット（INSTEAD OF の判定に使う）。
fn merge_trigger_events(stmt: &ValidatedMerge) -> Vec<u8> {
    use crate::catalog::trigger::TriggerEvents;
    let mut events = Vec::new();
    for clause in stmt.when_clauses() {
        let bit = match clause.action() {
            crate::sql::allowlist::MergeAction::Update { .. } => TriggerEvents::UPDATE,
            crate::sql::allowlist::MergeAction::Delete => TriggerEvents::DELETE,
            crate::sql::allowlist::MergeAction::Insert { .. } => TriggerEvents::INSERT,
            _ => continue,
        };
        if !events.contains(&bit) {
            events.push(bit);
        }
    }
    events
}

/// INSTEAD OF トリガを持つビューを対象にした `MERGE` を拒否する固定文言（対応しない。設計記録 D14・§11）。
const INSTEAD_OF_TARGET_UNSUPPORTED: &str =
    "MERGE into a view with INSTEAD OF triggers is not supported";

impl EngineCore {
    /// `MERGE` を束縛する（Execute・Describe 共通）。`dummy_flags` は Describe の `$n` ダミーフラグ
    /// （Execute は空）。権限ゲート（`42501`）は呼び出し元が済ませている前提で、本関数は読み取りスナップショット
    /// だけを使う。
    pub(super) fn bind_merge_stmt(
        &self,
        session: &SessionState,
        stmt: &ValidatedMerge,
        udfs: &UdfRegistry,
        dummy_flags: &[bool],
        snap: Option<&redb::ReadTransaction>,
    ) -> Result<BoundMerge, SqlSurfaceError> {
        self.bind_merge_stmt_with_schemas(session, stmt, udfs, dummy_flags, snap)
            .map(|(bound, _)| bound)
    }

    /// [`Self::bind_merge_stmt`] が束縛に使ったスキーマも返す版（実行の計画変換が使う。#3353）。
    pub(super) fn bind_merge_stmt_with_schemas(
        &self,
        session: &SessionState,
        stmt: &ValidatedMerge,
        udfs: &UdfRegistry,
        dummy_flags: &[bool],
        snap: Option<&redb::ReadTransaction>,
    ) -> Result<(BoundMerge, MergeBindSchemas), SqlSurfaceError> {
        // 呼び出し元が読み取り源（明示 Tx の `BEGIN` 時点のスナップショット）を渡したときは、対象・元・
        // 部分クエリのすべてでそれを使う（最新の確定カタログで束縛すると、BEGIN 後の別セッションの
        // 列変更で Describe と Execute の見る世界がずれる）。無ければ最新の確定カタログ。
        let owned;
        let read_txn = match snap {
            Some(s) => s,
            None => {
                owned = self.begin_read_txn()?;
                &owned
            }
        };
        let snap = crate::catalog::ViewWriteSnapshot::over(read_txn);
        let schema_of = |name: &str| -> Result<TableSchema, SqlSurfaceError> {
            snap.table_schema(name)
                .map_err(crate::catalog::table_lookup_error)
        };

        // 対象: 実表またはビュー（自動更新できなければ単独のビュー書き込みと同じ `55000`）。
        let target_name = stmt.target.table();
        let (target_schema, target_view, target_binding) = match resolve_from(&snap, target_name)? {
            Resolved::Table => (schema_of(target_name)?, None, None),
            Resolved::View { .. } | Resolved::Buffered { .. } => {
                // INSTEAD OF トリガを持つビューは自動更新可能性（`55000`）の判定より前に固定文言で拒否する
                // （対応しない。書き込む側の write txn でも同じ判定を再照合する）。テナントが未束縛なら判定できないので
                // 実行段の再照合に任せる。
                if let Some(tenant) = session.resolution_tenant() {
                    let defs = crate::catalog::trigger::list_triggers_in_source(
                        read_txn,
                        tenant,
                        target_name,
                    )
                    .map_err(|_| SqlSurfaceError::Internal {
                        detail: "trigger catalog operation failed".to_string(),
                    })?;
                    let events = merge_trigger_events(stmt);
                    if defs.iter().any(|t| {
                        t.timing == crate::catalog::trigger::TriggerTiming::InsteadOf
                            && t.level == crate::catalog::trigger::TriggerLevel::Row
                            && events.iter().any(|e| t.events.contains(*e))
                    }) {
                        return Err(SqlSurfaceError::feature_not_supported(
                            INSTEAD_OF_TARGET_UNSUPPORTED,
                        ));
                    }
                }
                let view = crate::sql::view_dml::resolve_merge_target(&snap, target_name)?;
                let fingerprint =
                    crate::catalog::view_write_fingerprint_in_read_txn(read_txn, target_name)
                        .map_err(crate::catalog::table_lookup_error)?;
                let schema = schema_of(&view.base_table)?;
                let binding = MergeTargetViewBinding {
                    name: target_name.to_string(),
                    base_table: view.base_table.clone(),
                    view_predicates: view.view_predicates.clone(),
                    check_predicates: view.check_predicates.clone(),
                    fingerprint,
                };
                (schema, Some(view), Some(binding))
            }
        };

        // 元: 実表・単純形ビューは基底表のスキーマ（ビューは公開列だけを見せる）、部分クエリは Describe から合成。
        let mut source_view = None;
        let (source_schema, source_visible) = match &stmt.source {
            MergeSource::Table(rel) => match resolve_from(&snap, rel.table())? {
                Resolved::Table => (schema_of(rel.table())?, None),
                Resolved::View {
                    base_table,
                    view_columns,
                    view_predicates,
                    column_map: None,
                    ..
                } => {
                    let schema = schema_of(&base_table)?;
                    source_view = Some((base_table, view_predicates));
                    (schema, view_columns)
                }
                Resolved::View { .. } | Resolved::Buffered { .. } => {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "MERGE supports only simple views without column aliases as the USING source",
                    ));
                }
            },
            MergeSource::Subquery {
                query,
                alias,
                column_aliases,
                ..
            } => (
                self.merge_subquery_schema(
                    session,
                    read_txn,
                    query,
                    alias,
                    column_aliases.as_deref(),
                )?,
                None,
            ),
        };

        let bound = bind_merge(
            stmt,
            &MergeBindInput {
                target_schema: &target_schema,
                target_view: target_view.as_ref(),
                source_schema: &source_schema,
                source_visible,
                dummy_flags,
            },
            udfs,
        )?;
        Ok((
            bound,
            MergeBindSchemas {
                target_schema,
                source_schema,
                source_view,
                target_view: target_binding,
            },
        ))
    }

    /// 部分クエリの元の合成スキーマ（疑似列 `id` なしの採番方式）。内側は Describe するだけで実行しない。
    /// 静的な型を持てない列・不正な列名は固定文言の `0A000`、重複する出力名は `42701`、列別名が出力列より
    /// 多ければ `DerivedTableColumnListTooLong`。
    fn merge_subquery_schema(
        &self,
        session: &SessionState,
        read_txn: &redb::ReadTransaction,
        query: &crate::sql::allowlist::Statement,
        alias: &str,
        column_aliases: Option<&[String]>,
    ) -> Result<TableSchema, SqlSurfaceError> {
        let metas = self
            .describe_parsed_in_session_snap(
                session,
                &super::ParsedSql::Statement(query.clone()),
                &crate::sql::parser::LiteralSkip::NONE,
                &[],
                &crate::result_limits::ResultLimits::default(),
                Some(read_txn),
            )?
            .ok_or_else(|| {
                SqlSurfaceError::feature_not_supported(
                    "MERGE source subquery has no output columns",
                )
            })?;
        let mut columns = crate::sql::ctas::output_columns_from_metas(metas);
        if let Some(aliases) = column_aliases {
            if aliases.len() > columns.len() {
                return Err(SqlSurfaceError::DerivedTableColumnListTooLong {
                    alias: alias.to_string(),
                    available: columns.len(),
                    specified: aliases.len(),
                });
            }
            for (col, a) in columns.iter_mut().zip(aliases.iter()) {
                col.name = a.clone();
            }
        }
        let defs = crate::sql::matview::column_defs_from_view_columns(&columns).map_err(|e| {
            match e.wire_code() {
                // 重複する出力名はそのまま（`42701`）。それ以外は固定文言で拒否する（列名を示さない）。
                "42701" => e,
                _ => SqlSurfaceError::feature_not_supported(
                    "MERGE source subquery columns need valid names and static types",
                ),
            }
        })?;
        Ok(TableSchema::new(alias, defs).with_row_identity(RowIdentity::Allocated))
    }

    /// `MERGE`（`WHEN MATCHED`・`NOT MATCHED BY SOURCE`・`NOT MATCHED [BY TARGET]`）を共有 write txn 上で実行する（autocommit は暗黙 Tx 経由で同じ腕に入る）。
    /// 手順は先頭の `//!` を参照。失敗時は呼び出し元が明示 Tx を `Failed` へ遷移させる（他の失敗文と同じ契約）。
    /// 書いた表（対象表・トリガが書いた表）へ `mark_written` する（漏れると COMMIT が write txn を破棄し、
    /// 遅延 FK 検査も取りこぼす。P0）。
    pub(super) fn execute_merge_in_txn(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'_>,
        stmt: &ValidatedMerge,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        // 1. 束縛。writer 取得前は `BEGIN` 時点のスナップショットで束縛し、Describe と読み取り源をそろえる。
        let (bound, schemas) = {
            let snap = Self::describe_snapshot(txn)?;
            self.bind_merge_stmt_with_schemas(session, stmt, session.udfs(), &[], snap)?
        };

        // 2. `RETURNING`（Issue #3356）は束縛済み（`bound.returning()`）。束縛エラーは上で返っている。
        let want_returning = bound.returning().is_some();
        // 台帳を記録する構成（`USING OPERATION_ID` つき）は、操作種別と文ハッシュが入る #3357 まで拒否する
        // （記録漏れで再送の冪等性が無いまま受理する経路を作らない）。省略時は台帳へ記録しない（`Omitted`）。
        let ledger_write = self
            .ledger_mode
            .resolve(stmt.operation_id())
            .map_err(|_| SqlSurfaceError::MissingOperationId)?;
        if ledger_write.op_id().is_some() {
            return Err(SqlSurfaceError::feature_not_supported(
                "MERGE with a ledger-recorded operation_id is not supported yet",
            ));
        }

        // 3. writer 取得（待機超過は `55P03`。`READ ONLY` Tx は呼び出し元のゲートが `25006`）。
        txn.ensure_writer(ctx)?;
        let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        })?;
        // 対象がビューなら、以降の実行（計画・事前判定・候補列挙・書き込み・FK 検査・`mark_written`）は
        // すべて基底表名で行う（単独のビュー書き込みと同じ。ビュー名は指紋・INSTEAD OF の照合にだけ使う）。
        let target_table: &str = match &schemas.target_view {
            Some(v) => v.base_table.as_str(),
            None => stmt.target_table(),
        };
        let target_schema = &schemas.target_schema;
        if let Some(v) = &schemas.target_view {
            // 束縛時のスナップショットで取った指紋を、writer 取得後の最新カタログで照合する。第 1 段より前に
            // 行うので、0 行の MERGE（書き込み経路に到達しない）でも並行 DDL を検出する。不一致は書き込まない。
            let live = crate::catalog::view_write_fingerprint_in_txn(write_txn, &v.name).map_err(
                |_| SqlSurfaceError::Internal {
                    detail: "failed to load view definition".to_string(),
                },
            )?;
            if live != v.fingerprint {
                return Err(SqlSurfaceError::invalid_input(
                    "view definition changed after the statement was bound",
                ));
            }
            // INSTEAD OF の再照合（指紋はトリガを含まない）。書き込み txn の中なので、判定と書き込みの間に
            // 並行 `CREATE TRIGGER` は入らない。
            for event in merge_trigger_events(stmt) {
                if self.view_has_instead_of(ctx, &v.name, event, Some(write_txn))? {
                    return Err(SqlSurfaceError::feature_not_supported(
                        INSTEAD_OF_TARGET_UNSUPPORTED,
                    ));
                }
            }
        }

        // 4. 計画への変換（純関数）。
        let source_input = match (&stmt.source, &schemas.source_view) {
            (MergeSource::Table(_), Some((base, preds))) => MergeSourceInput {
                table: base.clone(),
                schema: schemas.source_schema.clone(),
                view_predicates: preds.clone(),
            },
            (MergeSource::Table(rel), None) => MergeSourceInput {
                table: rel.table().to_string(),
                schema: schemas.source_schema.clone(),
                view_predicates: Vec::new(),
            },
            (MergeSource::Subquery { .. }, _) => MergeSourceInput {
                table: String::new(),
                schema: schemas.source_schema.clone(),
                view_predicates: Vec::new(),
            },
        };
        let plan = plan_merge(&bound, target_table, target_schema, source_input)?;

        // 5. 対象表の事前判定（行は読まず書かない）と、元の実表のスキーマ照合。
        let has_insert = bound
            .clauses()
            .iter()
            .any(|c| matches!(c.action(), BoundMergeAction::Insert(_)));
        crate::tenant::merge_target_preflight(write_txn, ctx, target_table, target_schema)
            .map_err(|e| crate::sql::exec::map_write_error(e, "update"))?;
        if matches!(stmt.source, MergeSource::Table(_)) {
            let live = crate::catalog::require_table_schema_write_any_identity(
                write_txn,
                &plan.joined.source.table,
            )
            .map_err(|e| match e {
                crate::catalog::CatalogError::TableNotFound(name) => {
                    SqlSurfaceError::UndefinedTable { name }
                }
                _ => SqlSurfaceError::Internal {
                    detail: "failed to load table schema".to_string(),
                },
            })?;
            if live != schemas.source_schema {
                return Err(SqlSurfaceError::invalid_input(
                    "table schema changed after the statement was bound",
                ));
            }
        }

        // 5b. 文トリガ（TABLE-45・D12）。WHEN 句に現れた操作の種類ごとに 1 回、入口（BEFORE）と出口（AFTER）で
        // 発火する。`TriggerWrittenTablesScope` は文トリガ本体が書いた表も `mark_written` に含めるため、BEFORE の
        // 発火より前に張る（漏れると COMMIT が write txn を破棄し、遅延 FK も取りこぼす。P0）。発火は redb の
        // 表ハンドルを開く前（元の走査・候補列挙より前）に行う。
        let trigger_scope = crate::tenant::trigger_writes::TriggerWrittenTablesScope::enter();
        let trigger_runtime = self.trigger_runtime.bind(self);
        let stmt_triggers = crate::tenant::merge_triggers::MergeStatementTriggers::plan(
            write_txn,
            ctx,
            target_table,
            target_schema,
            &plan.action_kinds(),
            &trigger_runtime,
        )
        .map_err(|e| crate::sql::exec::map_write_error(e, "update"))?;
        // `RETURNING` つきの MERGE は、結合形 DML の `RETURNING` とトリガの併用と同じく、該当するトリガ（行・文・
        // 参照アクション先の子表）が 1 つでもあれば BEFORE STATEMENT の発火より前に固定文言で拒否する
        // （書いた後に拒否する経路を作らない。設計記録 D14）。
        if want_returning && stmt_triggers.has_any() {
            return Err(SqlSurfaceError::feature_not_supported(
                "MERGE with RETURNING on a table with triggers is not supported",
            ));
        }
        let stmt_writes =
            crate::tenant::merge_triggers::statement_writes(write_txn, ctx, &trigger_runtime);
        stmt_triggers
            .fire_before(&stmt_writes, target_table, target_schema)
            .and_then(|()| {
                crate::tenant::merge_triggers::settle_statement_writes(write_txn, ctx, &stmt_writes)
            })
            .map_err(|e| crate::sql::exec::map_write_error(e, "update"))?;

        // 6. 元の実体化と probe の構築。書き込むセッションの `ctx`（RLS 暗黙適用）で、同じ write txn から読む。
        let limits = self.joined_dml_limits();
        let max_affected_rows = self.dml_limits.max_affected_rows;
        let max_scanned_rows = self.scan_limits.max_scanned_rows();
        let mut probe = match &stmt.source {
            MergeSource::Subquery { query, .. } => {
                let rows = self.materialize_merge_subquery(
                    ctx,
                    session,
                    write_txn,
                    query,
                    &plan.joined,
                    &limits,
                )?;
                JoinedProbe::new_with_rows(&plan.joined, &limits, rows)?
            }
            MergeSource::Table(_) => {
                JoinedProbe::new(write_txn, ctx, &plan.joined, session.udfs(), &limits)?
            }
        };

        // 7. 第 1 段: 全組の操作を決める（行は書かない）。1 つの対象行について UPDATE か DELETE を選んだ組が
        // 2 つ目になった時点で `21000`（DO NOTHING と、どの句も真にならなかった組は数えない。D5）。
        // 時刻は文の先頭で 1 度だけ確定し、SET・VALUES の `now()` と省略列の `DEFAULT now()` で共有する
        // （`INSERT ... SELECT` と同じ。SQL-37）。外側の式スコープが確定済みならそれを使う。
        // 文レベルの default clock（明示 Tx では実トランザクション開始時刻を持つ）があれば継承し、
        // 無いときに限り新規に確定する（開始時刻を現在時刻で捏造して Tx 内の他の INSERT と食い違わせない）。
        let outer_clock = crate::sql::time_fn::current_clock();
        let clock = match outer_clock.or_else(crate::sql::time_fn::current_default_clock) {
            Some(c) => c,
            None => {
                let now = crate::sql::time_fn::now_micros()?;
                crate::sql::time_fn::StatementClock {
                    txn_start_micros: now,
                    stmt_start_micros: now,
                }
            }
        };
        let _expr_clock = if has_insert && outer_clock.is_none() {
            Some(crate::sql::time_fn::ClockScope::enter(clock))
        } else {
            None
        };
        let _default_clock = if has_insert {
            Some(crate::sql::time_fn::DefaultClockScope::enter(clock))
        } else {
            None
        };
        let mut eval = MergeEval::new(&plan)?;
        // 句の種類ごとの数（条件評価の比較回数の計上に使う）。
        let count_of =
            |kind: MergeMatchKind| bound.clauses().iter().filter(|c| c.kind() == kind).count();
        let matched_clauses = count_of(MergeMatchKind::Matched);
        let by_source_clauses = count_of(MergeMatchKind::NotMatchedBySource);
        let by_target_clauses = count_of(MergeMatchKind::NotMatchedByTarget);
        let index = probe.index_handle();
        // 元の各行が、どれかの対象行に `ON` が真で一致したか（BY TARGET の句があるときだけ確保する。
        // 確保前に共有バイト予算へ計上する。D7）。
        let mut matched_flags: Vec<bool> = Vec::new();
        if by_target_clauses > 0 {
            probe.charge_bytes(index.rows.len())?;
            matched_flags
                .try_reserve_exact(index.rows.len())
                .map_err(|_| {
                    SqlSurfaceError::payload_too_large("MERGE source exceeds the row limit")
                })?;
            matched_flags.resize(index.rows.len(), false);
        }
        let no_source_row = crate::sql::exec::ResultRow {
            id: 0,
            score: 0.0,
            cells: Vec::new(),
        };
        let mut positions: Vec<u32> = Vec::new();
        // 利用者ポリシー（RLS-13）: UPDATE・DELETE を選んだ対象行（MATCHED・BY SOURCE の両方）に、それぞれの
        // `USING` を評価する。満たさなければ行を黙って飛ばさず文全体を `42501` で拒否する（行を書く前の
        // 第 1 段なので副作用ゼロ。`ON CONFLICT DO UPDATE` の既存行検査と同じ規則）。SELECT 側の `USING` は
        // 候補列挙が先に適用済み（隠れた行は一致にも BY SOURCE にも入らない）。
        let kinds = plan.action_kinds();
        let using_update = if kinds.update {
            crate::sql::policy_eval::compile_in_txn(
                write_txn,
                ctx.tenant_id(),
                target_table,
                target_schema,
                crate::sql::policy_eval::PolicyCheckKind::ConflictUpdateUsing,
            )
            .map_err(|e| crate::sql::exec::map_write_error(e, "update"))?
        } else {
            None
        };
        let using_delete = if kinds.delete {
            crate::sql::policy_eval::compile_in_txn(
                write_txn,
                ctx.tenant_id(),
                target_table,
                target_schema,
                crate::sql::policy_eval::PolicyCheckKind::ExistingDeleteUsing,
            )
            .map_err(|e| crate::sql::exec::map_write_error(e, "delete"))?
        } else {
            None
        };
        // ビュー対象: ビュー述語は第 1 段の対象候補の絞り込みとして評価する（`ON` の残余には入れない。入れると
        // BY SOURCE の DELETE・UPDATE がビューの外の基底行に及ぶ）。偽・NULL の行は一致にも BY SOURCE にも入らない。
        let check_using =
            |c: &DmlCandidate<'_>, chosen: &Option<Planned>| -> Result<(), SqlSurfaceError> {
                match chosen {
                    Some(Planned::Update(..)) => {
                        if let Some(p) = &using_update {
                            p.enforce(target_schema, c.id, c.embedding, c.metadata)
                                .map_err(|e| crate::sql::exec::map_write_error(e, "update"))?;
                        }
                    }
                    Some(Planned::Delete(_)) => {
                        if let Some(p) = &using_delete {
                            p.enforce(target_schema, c.id, c.embedding, c.metadata)
                                .map_err(|e| crate::sql::exec::map_write_error(e, "delete"))?;
                        }
                    }
                    None => {}
                }
                Ok(())
            };
        let view_predicates: &[crate::sql::allowlist::WherePredicate] = match &schemas.target_view {
            Some(v) => &v.view_predicates,
            None => &[],
        };
        let use_view_predicate = schemas.target_view.is_some();
        let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        let (view_md, view_expr, _view_rls, view_or) = crate::sql::parser::bind_where_predicates(
            view_predicates,
            target_schema,
            session.udfs(),
            &mut node_budget,
            &[],
        )?;
        let planned = crate::sql::exec::run_where_predicate(
            target_schema,
            &view_md,
            &view_expr,
            &view_or,
            |view_needs_embedding, view_pred| {
                crate::tenant::enumerate_merge_candidates_readonly(
                    write_txn,
                    target_table,
                    ctx,
                    target_schema,
                    plan.needs_embedding
                        || view_needs_embedding
                        || using_update.is_some()
                        || using_delete.is_some(),
                    max_scanned_rows,
                    &mut |c: &DmlCandidate<'_>| -> Result<Option<Planned>, SqlSurfaceError> {
                        if use_view_predicate && !view_pred(c)? {
                            return Ok(None);
                        }
                        let scanned =
                            crate::row_codec::scan_scalar_columns(target_schema, c.metadata)?;
                        positions.clear();
                        probe.matches_of(c, &scanned, &mut positions)?;
                        let mut chosen: Option<Planned> = None;
                        if positions.is_empty() {
                            // 元に一致する行が無い対象行（結合キーが NULL の行を含む）: `WHEN NOT MATCHED BY SOURCE`。
                            // 一致 0 件なので `21000` は起きない。
                            if by_source_clauses > 0 {
                                probe.charge_comparisons(by_source_clauses)?;
                                match eval.choose_by_source(&scanned, c)? {
                                    Choice::None | Choice::DoNothing => {}
                                    Choice::Delete => {
                                        charge_planned_row(&mut probe)?;
                                        chosen = Some(Planned::Delete(None));
                                    }
                                    Choice::Update(clause) => {
                                        let values = eval.eval_update(
                                            clause,
                                            &scanned,
                                            c,
                                            &no_source_row,
                                            target_schema,
                                            &mut |n| probe.charge_bytes(n),
                                        )?;
                                        chosen = Some(Planned::Update(clause, values, None));
                                    }
                                    Choice::Insert(_) => return Err(merge_plan_mismatch()),
                                }
                            }
                            check_using(c, &chosen)?;
                            return Ok(chosen);
                        }
                        // 一致の記録は、どの句が選ばれたか（DO NOTHING・どの句も真にならない場合を含む）に関係なく
                        // `ON` が真であることだけで立てる。一致があるのに MATCHED の句がどれも発火しない対象行は
                        // BY SOURCE の句を受けない。
                        if by_target_clauses > 0 {
                            for &pos in &positions {
                                *matched_flags.get_mut(pos as usize).ok_or_else(|| {
                                    SqlSurfaceError::Internal {
                                        detail: "matched source row out of range".to_string(),
                                    }
                                })? = true;
                            }
                        }
                        // 句の条件評価（`WHEN ... AND`）は組ごとのネストループ。`ON` の残余と同じ比較回数の上限へ
                        // 計上し（超過は `54000`）、一定間隔で中断要求（`57014`）を確認する。
                        probe
                            .charge_comparisons(positions.len().saturating_mul(matched_clauses))?;
                        for (n, &pos) in positions.iter().enumerate() {
                            if n % 1024 == 0 {
                                crate::interrupt::check()?;
                            }
                            let row = index.rows.get(pos as usize).ok_or_else(|| {
                                SqlSurfaceError::Internal {
                                    detail: "matched source row out of range".to_string(),
                                }
                            })?;
                            let choice = eval.choose_matched(&scanned, c, row)?;
                            if matches!(choice, Choice::Delete | Choice::Update(_))
                                && chosen.is_some()
                            {
                                return Err(SqlSurfaceError::merge_cardinality_violation());
                            }
                            match choice {
                                Choice::None | Choice::DoNothing => {}
                                Choice::Delete => {
                                    charge_planned_row(&mut probe)?;
                                    chosen = Some(Planned::Delete(Some(pos)));
                                }
                                Choice::Update(clause) => {
                                    let values = eval.eval_update(
                                        clause,
                                        &scanned,
                                        c,
                                        row,
                                        target_schema,
                                        &mut |n| probe.charge_bytes(n),
                                    )?;
                                    chosen = Some(Planned::Update(clause, values, Some(pos)));
                                }
                                Choice::Insert(_) => return Err(merge_plan_mismatch()),
                            }
                        }
                        check_using(c, &chosen)?;
                        Ok(chosen)
                    },
                )
            },
        )
        .map_err(|e| match e {
            PredicateDmlError::Predicate(e) => e,
            PredicateDmlError::Write(e) => crate::sql::exec::map_write_error(e, "update"),
        })?;

        // 7b. 元だけにある行（どの対象行にも一致しなかった行。結合キーが NULL の行を含む）を走査順に
        // `WHEN NOT MATCHED [BY TARGET]` の句で評価し、INSERT の行を句ごとに積む（行は書かない）。
        let mut insert_rows: BTreeMap<usize, Vec<InsertRow>> = BTreeMap::new();
        let mut insert_total: usize = 0;
        if by_target_clauses > 0 {
            // 条件評価は一致しなかった行だけが受けるため、その行数だけを計上する（一致した行は評価しない）。
            let unmatched = matched_flags.iter().filter(|m| !**m).count();
            probe.charge_comparisons(unmatched.saturating_mul(by_target_clauses))?;
            for (n, row) in index.rows.iter().enumerate() {
                if n % 1024 == 0 {
                    crate::interrupt::check()?;
                }
                let matched =
                    matched_flags
                        .get(n)
                        .copied()
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "source row flag out of range".to_string(),
                        })?;
                if matched {
                    continue;
                }
                match eval.choose_by_target(row)? {
                    Choice::None | Choice::DoNothing => {}
                    Choice::Insert(clause) => {
                        let (id, values, provided) =
                            eval.eval_insert(clause, row, target_schema, &mut |n| {
                                probe.charge_bytes(n)
                            })?;
                        probe.charge_bytes(std::mem::size_of::<InsertRow>())?;
                        let rows = insert_rows.entry(clause).or_default();
                        rows.try_reserve(1).map_err(|_| {
                            SqlSurfaceError::payload_too_large("MERGE INSERT rows exceed the limit")
                        })?;
                        rows.push((id, values, provided, n));
                        insert_total = insert_total.saturating_add(1);
                    }
                    Choice::Delete | Choice::Update(_) => return Err(merge_plan_mismatch()),
                }
            }
        }

        // 8. 第 2 段: 上限の判定（一致ペア・比較回数・予算は第 1 段の中で判定済み）。
        // UPDATE は選んだ句ごとに分ける（`UPDATE OF` トリガの発火判定は 1 回の適用の SET 列の和集合で
        // 行われるため、句の SET 列が異なる行を同じ適用に混ぜると、別の句の列で発火してしまう）。
        let mut update_plan: BTreeMap<usize, HashMap<u64, Vec<(usize, crate::row_codec::Value)>>> =
            BTreeMap::new();
        let mut delete_plan: Vec<u64> = Vec::new();
        let mut planned_total: usize = 0;
        // `RETURNING` つきのときだけ、対象行 id → (元の走査位置, 結果の並び順) を作る（並び順は候補列挙順）。
        // 確保前に共有バイト予算へ計上する（D7）。
        let mut target_info: HashMap<u64, (Option<u32>, usize)> = HashMap::new();
        for (ord, (id, p)) in planned.into_iter().enumerate() {
            planned_total = planned_total.saturating_add(1);
            let src = match &p {
                Planned::Update(_, _, s) | Planned::Delete(s) => *s,
            };
            if want_returning {
                probe.charge_bytes(
                    std::mem::size_of::<(u64, (Option<u32>, usize))>().saturating_mul(2),
                )?;
                target_info.try_reserve(1).map_err(|_| {
                    SqlSurfaceError::payload_too_large("MERGE RETURNING rows exceed the limit")
                })?;
                target_info.insert(id, (src, ord));
            }
            match p {
                Planned::Update(clause, values, _) => {
                    update_plan.entry(clause).or_default().insert(id, values);
                }
                Planned::Delete(_) => delete_plan.push(id),
            }
        }
        // 影響行数の上限は UPDATE・DELETE・INSERT の計画件数の合計で判定する（DO NOTHING は数えない）。
        crate::sql::parser::check_dml_affected_rows_with_limit(
            planned_total.saturating_add(insert_total),
            max_affected_rows,
        )?;
        // INSERT の行数・バッチ上限（INDEX-4）も書き込みの前に判定する。
        let mut insert_bounds: Vec<Vec<BoundInsert>> = Vec::new();
        // 句ごとの INSERT 行が読む元の走査位置（`insert_bounds` と同じ並び。`RETURNING` 用）。
        let mut insert_srcs: Vec<Vec<usize>> = Vec::new();
        if insert_total > 0 {
            self.check_insert_row_count_limit(insert_total)?;
            for (clause, rows) in insert_rows {
                let mut bounds: Vec<BoundInsert> = Vec::new();
                bounds.try_reserve_exact(rows.len()).map_err(|_| {
                    SqlSurfaceError::payload_too_large("MERGE INSERT rows exceed the limit")
                })?;
                let id_omitted = bound_id_omitted(&bound, &clause);
                // 省略列の DEFAULT 値は行ごとに複製されるため、列ごとの 1 値のサイズを 1 度だけ束縛して測り、
                // 確保前（`finish_bound_insert` の前）に行数分を共有バイト予算へ計上する（超過は `54000`）。
                let mut default_bytes: Vec<usize> = Vec::new();
                for column in &target_schema.columns {
                    let n = match &column.default_expr {
                        Some(
                            d @ (crate::catalog::ColumnDefault::Text(_)
                            | crate::catalog::ColumnDefault::Number(_)
                            | crate::catalog::ColumnDefault::Bool(_)),
                        ) if column.generated.is_none() => {
                            crate::sql::parser::bind_column_default(column, d)
                                .map(|v| crate::sql::join::dml::value_bytes(&v))
                                .unwrap_or(0)
                        }
                        _ => 0,
                    };
                    default_bytes.push(n);
                }
                let mut srcs: Vec<usize> = Vec::new();
                srcs.try_reserve_exact(rows.len()).map_err(|_| {
                    SqlSurfaceError::payload_too_large("MERGE INSERT rows exceed the limit")
                })?;
                for (id, values, provided, src) in rows {
                    srcs.push(src);
                    let mut row_default: usize = 0;
                    for (i, n) in default_bytes.iter().enumerate() {
                        if !provided.get(i).copied().unwrap_or(false) {
                            row_default = row_default.saturating_add(*n);
                        }
                    }
                    if row_default > 0 {
                        probe.charge_bytes(row_default)?;
                    }
                    bounds.push(crate::sql::insert_select::finish_bound_insert(
                        target_table,
                        target_schema,
                        id,
                        values,
                        &provided,
                        id_omitted,
                        None,
                        clock,
                    )?);
                }
                insert_bounds.push(bounds);
                insert_srcs.push(srcs);
            }
            // 上限は全 INSERT 句の累計で 1 回判定する（句ごとにリセットされない）。
            self.validate_merge_insert_groups_limits(&insert_bounds)?;
        }
        drop(eval);
        drop(probe);

        // 9. 第 3 段: 既存の書き込み経路へ適用する。台帳は記録しない（`Omitted`／`Disabled`）ため、文ハッシュは
        // 記録に使われない（#3357 が MERGE 用の `for_merge` に置き換える）。適用は UPDATE の句ごと → DELETE →
        // INSERT の句ごとの順（走査し直す回数は UPDATE 句数 + 1 で、句数は構文段の上限で抑えられ行数に比例
        // しない。UPDATE と DELETE の対象行は `21000` の判定と一致 0 件の振り分けにより互いに素）。INSERT は
        // 対象を走査し直さず、句ごとに `id` 省略の一様性（バッチの制約）を保つ。
        debug_assert!(ledger_write.op_id().is_none());
        let hash = crate::recovery::content_hash::for_delete(0);
        let generated_scope =
            crate::sql::generated_column::GeneratedComputeScope::enter(target_table);
        // 文トリガは入口・出口で種類ごとに 1 回だけ発火するため、第 3 段の既存経路が呼ぶ分類器には対象表の
        // 文トリガ群を空で返させる（行トリガは発火する）。
        let stmt_trigger_scope =
            crate::tenant::merge_triggers::MergeStatementTriggerScope::enter(target_table);
        // ビュー対象: 基底表への書き込みが「ビュー経由」であること（指紋の照合・`WITH CHECK OPTION` の検査）を
        // 既存の書き込み経路へ搬送する（単独のビュー書き込みと同じ型）。
        let view_scope = schemas.target_view.as_ref().map(|v| {
            crate::sql::view_write_scope::ViewWriteScope::enter_with_checks(
                &v.name,
                v.fingerprint.clone(),
                &v.base_table,
                v.check_predicates.clone(),
            )
        });
        // UPDATE 句と INSERT 句をまたぐ参照（自己参照を含む）を文単位の検査と同じ意味にするため、複数区間が
        // 書くときは子側の FK 検査を先送りし、全区間の書き込み後に 1 回検査する（Issue #3354・#3355）。
        let fk_defer = ((!update_plan.is_empty() && !insert_bounds.is_empty())
            || insert_bounds.len() > 1)
            .then(|| crate::tenant::MergeFkDeferScope::enter(target_table));
        let mut rows_affected: u64 = 0;
        // `RETURNING`（Issue #3356）の収集器。3 つの書き込み区間が、既存の書き込み経路の capturing 版
        // （`project` 引数）を通して書き込み txn の中・commit 前に行ごとに積む。第 2 の書き込み経路は作らない。
        let mut sink = plan.returning.as_ref().map(|ret| {
            crate::sql::dml_from_returning::MergeReturningSink::new(
                ctx,
                ret,
                target_schema,
                target_schema.is_allocated(),
            )
        });
        // 適用結果は実際に変更した件数を使う。BEFORE ROW トリガが行を飛ばす（`RETURN NULL`）・別の行を
        // 消すなどの正常な動作で計画より少なくなっても、文は失敗させない（他の書き込み経路と同じ意味論）。
        for (_clause, rows) in update_plan {
            let mut by_id = rows;
            let mut update_project = |row: &crate::tenant::CapturedRow| match sink.as_mut() {
                Some(s) => merge_push_existing(s, &target_info, &index, row, "UPDATE"),
                None => Ok(()),
            };
            let outcome = crate::tenant::update_rows_joined_unchecked(
                WriteTarget::InTxn(write_txn),
                target_table,
                ctx,
                ledger_write,
                &hash,
                Some(target_schema),
                &[],
                false,
                max_affected_rows,
                max_scanned_rows,
                |c: &DmlCandidate<'_>| Ok::<_, SqlSurfaceError>(by_id.remove(&c.id)),
                want_returning
                    .then_some(&mut update_project as &mut crate::tenant::ReturningProjectFn<'_>),
                Some(&self.trigger_runtime.bind(self)),
            )
            .map_err(|e| match e {
                PredicateDmlError::Predicate(e) => e,
                PredicateDmlError::Write(e) => crate::sql::exec::map_write_error(e, "update"),
            })?;
            match outcome {
                PredicateDmlOutcome::Applied { rows_affected: n } => {
                    rows_affected = rows_affected.saturating_add(n as u64);
                }
                _ => return Err(merge_plan_mismatch()),
            }
        }
        if !delete_plan.is_empty() {
            let ids: HashSet<u64> = delete_plan.into_iter().collect();
            let outcome = if want_returning {
                // `RETURNING` つきは削除前の値を捕捉する経路へ切り替える（トリガとの併用は上で拒否済みなので
                // 通過証は渡さない。該当トリガが現れたら書き込み側が `0A000` で止める。多層防御）。
                let mut delete_project = |row: &crate::tenant::CapturedRow| match sink.as_mut() {
                    Some(s) => merge_push_existing(s, &target_info, &index, row, "DELETE"),
                    None => Ok(()),
                };
                crate::tenant::delete_rows_where_capturing_unchecked(
                    WriteTarget::InTxn(write_txn),
                    target_table,
                    ctx,
                    ledger_write,
                    &hash,
                    Some(target_schema),
                    false,
                    max_affected_rows,
                    max_scanned_rows,
                    |c: &DmlCandidate<'_>| Ok::<bool, SqlSurfaceError>(ids.contains(&c.id)),
                    &mut delete_project,
                    true,
                    None,
                )
            } else {
                crate::tenant::delete_rows_where_with_triggers(
                    WriteTarget::InTxn(write_txn),
                    target_table,
                    ctx,
                    ledger_write,
                    &hash,
                    Some(target_schema),
                    false,
                    max_affected_rows,
                    max_scanned_rows,
                    |c: &DmlCandidate<'_>| Ok::<bool, SqlSurfaceError>(ids.contains(&c.id)),
                    Some(&self.trigger_runtime.bind(self)),
                    // 一致の判定で既存行を読む（RLS-13）。
                    true,
                )
            }
            .map_err(|e| match e {
                PredicateDmlError::Predicate(e) => e,
                PredicateDmlError::Write(e) => crate::sql::exec::map_write_error(e, "delete"),
            })?;
            match outcome {
                PredicateDmlOutcome::Applied { rows_affected: n } => {
                    rows_affected = rows_affected.saturating_add(n as u64);
                }
                _ => return Err(merge_plan_mismatch()),
            }
        }
        for (bounds, srcs) in insert_bounds.iter().zip(insert_srcs.iter()) {
            if !want_returning {
                let outcome = crate::sql::exec::execute_insert_batch_with_schema_in(
                    WriteTarget::InTxn(write_txn),
                    ctx,
                    bounds,
                    self.ledger_mode,
                    Some(target_schema),
                    Some(&self.trigger_runtime.bind(self)),
                )?;
                rows_affected = rows_affected.saturating_add(outcome.rows_affected);
                continue;
            }
            // `RETURNING` つき: 既存の挿入経路が書いた最終行（DEFAULT・採番・生成列を反映した値）を、束縛行の
            // 順に commit 前に受け取る。呼び出し回数が束縛行数と食い違えば `XX000`（fail-closed）。
            let mut called: usize = 0;
            let mut insert_project = |row: &crate::tenant::CapturedRow| match sink.as_mut() {
                Some(s) => {
                    let src = srcs.get(called).copied();
                    called = called.saturating_add(1);
                    merge_push_inserted(s, &index, row, src)
                }
                None => Ok(()),
            };
            let (outcome, _trigger_path) =
                crate::sql::exec::execute_insert_batch_with_schema_in_projecting(
                    WriteTarget::InTxn(write_txn),
                    ctx,
                    bounds,
                    self.ledger_mode,
                    Some(target_schema),
                    None,
                    Some(&mut insert_project as &mut crate::tenant::ReturningProjectFn<'_>),
                )?;
            if called != bounds.len() {
                return Err(merge_plan_mismatch());
            }
            rows_affected = rows_affected.saturating_add(outcome.rows_affected);
        }
        if fk_defer.is_some() {
            drop(fk_defer);
            crate::constraint::enforce_immediate_foreign_keys_by_scan_in_txn(
                write_txn,
                ctx.tenant_id(),
                target_table,
            )
            .map_err(crate::sql::exec::map_insert_write_error)?;
        }
        if view_scope.as_ref().is_some_and(|s| s.mismatched()) {
            return Err(SqlSurfaceError::invalid_input(
                "view definition changed after the statement was bound",
            ));
        }
        drop(view_scope);
        drop(stmt_trigger_scope);
        drop(generated_scope);

        // 9b. AFTER STATEMENT（DELETE → UPDATE → INSERT の順。種類ごとに 1 回。書いた行が 0 件でも発火する）。
        stmt_triggers
            .fire_after(&stmt_writes, target_table, target_schema)
            .and_then(|()| {
                crate::tenant::merge_triggers::settle_statement_writes(write_txn, ctx, &stmt_writes)
            })
            .map_err(|e| crate::sql::exec::map_write_error(e, "update"))?;
        let written = trigger_scope.take();
        drop(trigger_scope);

        // 10. 後始末（`write_txn` の借用はここまで）。
        txn.mark_written(ctx.tenant_id(), target_table);
        for table in written {
            txn.mark_written(ctx.tenant_id(), &table);
        }
        match (sink, plan.returning.as_ref()) {
            (Some(sink), Some(ret)) => {
                Ok(SqlOutcome::Returning(crate::sql::exec::ReturningOutcome {
                    command: crate::sql::returning::DmlCommand::Merge,
                    rows_affected,
                    result: QueryResult {
                        columns: crate::sql::dml_from_returning::column_meta(ret),
                        rows: sink.finish(),
                    },
                }))
            }
            _ => Ok(SqlOutcome::Merge(MergeOutcome { rows_affected })),
        }
    }

    /// 部分クエリの元を、書き込むセッションの `PolicyContext` で既存の読み取り経路に通して実体化し、
    /// 走査投影（`JoinedDmlPlan::scan_columns`）の並びへ射影する（`core::insert_select_exec` の前例）。
    /// 取得行数の上限は結果行数上限（超過は `54000`。黙って切り詰めない）と結合の可視行数上限。
    fn materialize_merge_subquery(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        write_txn: &redb::WriteTransaction,
        query: &Statement,
        joined: &crate::sql::join::dml::JoinedDmlPlan,
        limits: &crate::sql::join::JoinLimits,
    ) -> Result<QueryResult, SqlSurfaceError> {
        let cap = self.write_input_cap()?;
        let outcome = if matches!(query, Statement::ConstSelect(_)) {
            // `FROM` なしの定数式はテーブルを読まないため、読み取り経路ではなく定数式の実行本体へ渡す。
            self.execute_validated_in_session(ctx, session, query.clone())?
        } else {
            self.execute_read_statement_with_limits(
                ctx,
                session,
                query.clone(),
                write_txn,
                cap.eval_limits,
            )?
        };
        let SqlOutcome::Query(QueryResult { columns, rows }) = outcome else {
            return Err(SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            });
        };
        if columns.len() != joined.source.schema.columns.len() {
            return Err(SqlSurfaceError::invalid_input(
                "MERGE source subquery shape changed during the statement",
            ));
        }
        if rows.len() > cap.cap || rows.len() > limits.max_input_rows {
            return Err(SqlSurfaceError::payload_too_large(
                "MERGE source subquery exceeds the row limit",
            ));
        }
        let wanted = joined.scan_columns();
        let mut projected: Vec<crate::sql::exec::ResultRow> = Vec::new();
        projected.try_reserve_exact(rows.len()).map_err(|_| {
            SqlSurfaceError::payload_too_large("MERGE source subquery exceeds the row limit")
        })?;
        for row in rows {
            let mut cells = Vec::with_capacity(wanted.len());
            for &c in &wanted {
                cells.push(
                    row.cells
                        .get(c)
                        .cloned()
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "source column out of range".to_string(),
                        })?,
                );
            }
            projected.push(crate::sql::exec::ResultRow { cells, ..row });
        }
        let projected_columns = wanted
            .iter()
            .filter_map(|&c| columns.get(c).cloned())
            .collect();
        Ok(QueryResult {
            columns: projected_columns,
            rows: projected,
        })
    }
}

/// BY TARGET の `INSERT` が積んだ 1 行（疑似列 id・列ごとの値・列ごとの「値が指定されたか」）。
/// 末尾は元の走査位置（`RETURNING` の結果行が読む元の行と並び順に使う。Issue #3356）。
type InsertRow = (u64, Vec<crate::row_codec::Value>, Vec<bool>, usize);

/// 採った対象行の計画（`(id, Planned)` の蓄積・`delete_plan`・`HashSet` の 3 か所）を確保前に共有バイト予算へ
/// 計上する（超過は `54000`。D7）。
fn charge_planned_row(probe: &mut JoinedProbe<'_>) -> Result<(), SqlSurfaceError> {
    probe.charge_bytes(
        std::mem::size_of::<(u64, Planned)>()
            .saturating_add(std::mem::size_of::<u64>().saturating_mul(3)),
    )
}

/// 句 `clause` の INSERT が疑似列 `id` を省略するか（移行済み表の採番。`BoundInsert::id_omitted`）。
fn bound_id_omitted(bound: &BoundMerge, clause: &usize) -> bool {
    matches!(
        bound.clauses().get(*clause).map(|c| c.action()),
        Some(BoundMergeAction::Insert(ins)) if ins.id_omitted()
    )
}

/// 第 1 段が 1 つの対象行について決めた操作。
enum Planned {
    /// 選んだ句の添字・評価済みの SET 値・採った元の走査位置（BY SOURCE は `None`）。
    Update(usize, Vec<(usize, crate::row_codec::Value)>, Option<u32>),
    /// 採った元の走査位置（BY SOURCE は `None`）。位置は `RETURNING` の結果行だけが使う（Issue #3356）。
    Delete(Option<u32>),
}

/// UPDATE・DELETE 区間が書いた対象行 1 行を `RETURNING` の収集器へ積む（Issue #3356）。`target_info` は
/// 第 1 段が記録した対象行 id → (元の走査位置, 並び順)。引けない行は結合と書き込みの食い違いで `XX000`。
fn merge_push_existing(
    sink: &mut crate::sql::dml_from_returning::MergeReturningSink<'_>,
    target_info: &HashMap<u64, (Option<u32>, usize)>,
    index: &crate::sql::join::dml::SourceIndex,
    row: &crate::tenant::CapturedRow,
    action: &str,
) -> Result<(), crate::tenant::TenantWriteError> {
    let (src, ord) = target_info.get(&row.id).copied().ok_or_else(|| {
        crate::tenant::TenantWriteError::ReturningProjectionFailed(
            "MERGE target row not found in the planned operations".to_string(),
        )
    })?;
    let source = match src {
        Some(pos) => Some(source_row_ref(index, pos as usize)?),
        None => None,
    };
    sink.push(row, action, (0, ord), source)
}

/// INSERT 区間が書いた行 1 行を `RETURNING` の収集器へ積む。`src` は束縛行に対応する元の走査位置。
fn merge_push_inserted(
    sink: &mut crate::sql::dml_from_returning::MergeReturningSink<'_>,
    index: &crate::sql::join::dml::SourceIndex,
    row: &crate::tenant::CapturedRow,
    src: Option<usize>,
) -> Result<(), crate::tenant::TenantWriteError> {
    let pos = src.ok_or_else(|| {
        crate::tenant::TenantWriteError::ReturningProjectionFailed(
            "MERGE inserted row count mismatch".to_string(),
        )
    })?;
    sink.push(row, "INSERT", (1, pos), Some(source_row_ref(index, pos)?))
}

/// 元の走査位置から `RETURNING` 用の行参照を引く（範囲外は `XX000`）。
fn source_row_ref(
    index: &crate::sql::join::dml::SourceIndex,
    pos: usize,
) -> Result<crate::sql::dml_from_returning::FromRowRef<'_>, crate::tenant::TenantWriteError> {
    index
        .rows
        .get(pos)
        .map(|r| crate::sql::dml_from_returning::FromRowRef { cells: &r.cells })
        .ok_or_else(|| {
            crate::tenant::TenantWriteError::ReturningProjectionFailed(
                "MERGE source row out of range".to_string(),
            )
        })
}

/// 第 3 段の適用結果が想定外の形（`Applied` 以外）だった。write txn を commit
/// させず `XX000` で fail-closed にする（行・件数・テナントを含めない）。
fn merge_plan_mismatch() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "MERGE apply stage did not match the planned operations".to_string(),
    }
}
