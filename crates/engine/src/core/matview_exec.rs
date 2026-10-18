//! `CREATE MATERIALIZED VIEW` の実行本体（TABLE-42 (2)(3)・Issue #1664。親: #1662）。
//!
//! 責務境界: 構文検証は `sql::allowlist`（`ValidatedCreateMaterializedView`）、カタログと文脈単位の
//! 実体ストアは `catalog::materialized`（Issue #1663）、列の確定とセル変換は `sql::matview` が担う。
//! 本モジュールは `core.rs` の `execute_parsed_in_session_inner` の `CreateMaterializedView` 腕
//! （DDL 権限ゲート通過後）から呼ばれ、それらを **1 つの write txn** の中で束ねる。
//!
//! 原子性: 書き込みゲート（単一ライタ）を取った write txn を読み取り源（`ReadSource`）としても使い、
//! (1) 本文の出力列の確定、(2) 全文脈の本文評価、(3) 定義と全文脈の実体の書き込み、を同一の
//! スナップショット上で行う。評価で失敗（行数超過・変換失敗・文脈数超過）しても commit 前に txn を
//! 破棄するため、定義も実体も残らない。クラッシュ後は「作成前」か「作成後」のどちらかだけが見える。
//!
//! テナント境界（P0）: 各文脈の実体は、その文脈の `PolicyContext` で既存の実行経路（RLS の暗黙適用）を
//! 通して評価する。実体化対象の文脈は認証構成から導出した集合（未設定なら作成セッションの文脈のみ）
//! で、クライアントの申告値は使わない。成功応答の行数は作成セッション自身の文脈ぶんだけで、他文脈の
//! 件数は合算しない。評価中は書き込みゲートを保持するため他の writer を待たせるが、文脈ごとの行数上限
//! （`--max-result-rows`）と文脈数上限（`MAX_MATVIEW_CONTEXTS`）で有界にする。

use super::EngineCore;
use crate::catalog::materialized::{
    begin_streamed_create_in_txn, finish_streamed_create_in_txn, write_context_entity_in_txn,
    ContextKey, MaterializedViewDef, MAX_MATVIEW_CONTEXTS,
};
use crate::catalog::CatalogError;
use crate::policy::PolicyContext;
use crate::result_limits::{ResultLimits, MAX_RESULT_ROWS_HARD_CAP};
use crate::row_codec::Value;
use crate::sql::allowlist::{
    classify_view_body, validate_sql_tokens, SqlSurfaceError, Statement,
    ValidatedCreateMaterializedView, ViewBodyCheck, ViewBodyKind,
};
use crate::sql::ddl::{map_create_matview_error, CreateMaterializedViewOutcome};
use crate::sql::lexer::{Keyword, Token};
use crate::sql::mode::SessionState;

/// 実行可能な本文 1 件と、その評価に渡す結果行数上限。
struct EvalPlan {
    stmt: Statement,
    limits: ResultLimits,
}

impl EngineCore {
    /// 実体化の対象文脈を解決する（`CREATE`〔本 Issue〕と後続の `REFRESH`〔#1665〕が共有する）。
    /// 注入済みの集合があればそれを、無ければ作成/更新したセッションの文脈だけを返す。
    /// 文脈数が [`MAX_MATVIEW_CONTEXTS`] を超えたら `54000`（確保・評価の前に検査する）。
    pub(super) fn materialization_contexts_for(
        &self,
        creator: &PolicyContext,
    ) -> Result<Vec<PolicyContext>, SqlSurfaceError> {
        let contexts: Vec<PolicyContext> = match &self.materialization_contexts {
            Some(list) => {
                if list.len() > MAX_MATVIEW_CONTEXTS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many materialized view contexts",
                    ));
                }
                list.as_ref().clone()
            }
            None => vec![creator.clone()],
        };
        Ok(contexts)
    }

    /// 1 文脈あたりの行数上限。`--max-result-rows` を上限とし、`+1` で超過を検出するため
    /// ハード上限の 1 つ手前に丸める（設定がハード上限のときは 99,999 行まで。fail-closed）。
    fn matview_row_cap(&self) -> usize {
        self.result_limits
            .max_result_rows()
            .min(MAX_RESULT_ROWS_HARD_CAP.saturating_sub(1))
    }

    /// 本文（格納形）を、文脈ごとに実行できる 1 つの読み取り文へ変える。単純形は `LIMIT cap+1` を
    /// 付けて超過を検出できるようにし、評価後射影形（集計・LIMIT・JOIN 等）は検証済みの文をそのまま使う。
    fn matview_eval_plan(&self, body_sql: &str, cap: usize) -> Result<EvalPlan, SqlSurfaceError> {
        let invalid_body = || SqlSurfaceError::Internal {
            detail: "invalid materialized view definition".to_string(),
        };
        let tokens =
            crate::sql::lexer::tokenize_stored_view_body(body_sql).map_err(|_| invalid_body())?;
        match classify_view_body(&tokens, &self.storage, ViewBodyCheck::Resolved)? {
            ViewBodyKind::Simple(_) => {
                let fetch = cap.checked_add(1).ok_or_else(invalid_body)?;
                let mut limited = tokens;
                limited.push(Token::Keyword(Keyword::Limit));
                limited.push(Token::Number(fetch.to_string()));
                let stmt = validate_sql_tokens(&limited, &self.storage)?;
                Ok(EvalPlan {
                    stmt,
                    limits: ResultLimits::new(fetch),
                })
            }
            ViewBodyKind::Buffered { stmt, .. } => Ok(EvalPlan {
                stmt: *stmt,
                // 評価後射影形の本文は外側の設定値と連動させない（`BufferedView` の本文と同じ）。
                limits: ResultLimits::default(),
            }),
        }
    }

    /// `CREATE MATERIALIZED VIEW` の実行本体（DDL 権限ゲートは呼び出し元が通過済み）。
    pub(super) fn execute_create_materialized_view(
        &self,
        creator: &PolicyContext,
        session: &SessionState,
        stmt: &ValidatedCreateMaterializedView,
    ) -> Result<CreateMaterializedViewOutcome, SqlSurfaceError> {
        let skipped = CreateMaterializedViewOutcome {
            skipped: true,
            populated: false,
            own_rows: None,
        };
        // 早期の名前衝突判定（高価な評価の前に抜ける）。最終判定は下の write txn が担う。
        let exists = self
            .storage
            .relation_name_exists(stmt.name())
            .map_err(map_create_matview_error)?;
        if exists {
            return if stmt.if_not_exists() {
                Ok(skipped)
            } else {
                Err(SqlSurfaceError::DuplicateTable {
                    name: stmt.name().to_string(),
                })
            };
        }

        // 書き込みゲート（単一ライタ）を取る。以降の読み取りは、このゲートを保持している間は
        // 他の writer が commit できないため、write txn 開始時点のスナップショットと一致する。
        let write_txn = self
            .storage
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)
            .map_err(map_create_matview_error)?;

        let cap = self.matview_row_cap();
        let columns = {
            let view_columns =
                self.view_output_columns(session, stmt.body_sql(), stmt.column_aliases())?;
            crate::sql::matview::column_defs_from_view_columns(&view_columns)?
        };
        let def = MaterializedViewDef {
            name: stmt.name().to_string(),
            base_relations: stmt.base_relations().to_vec(),
            body_sql: stmt.body_sql().to_string(),
            column_aliases: stmt.column_aliases().to_vec(),
            columns: columns.clone(),
            populated: stmt.with_data(),
        };

        // 本文を実行可能な文へ束縛し、実体は列照合を運ばないため非バイト順照合の列を持つ表を読む本文は
        // 拒否する（後続の ORDER BY・比較がバイト順で黙って行われる意味論のずれを作らない）。
        // `WITH NO DATA` でも同じ本文が後で `REFRESH` されるため、定義時に弾いておく。
        let plan = self.matview_eval_plan(stmt.body_sql(), cap)?;
        self.check_matview_body_collations(&*write_txn, &plan.stmt)?;

        // 定義の登録。`WITH DATA` では文脈ごとに評価した直後に同じ write txn へ保存し行値を解放する
        // （全文脈の行をメモリへ蓄積するとピークが文脈数に比例するため）。途中失敗は txn 破棄で
        // 原子性を保つ。
        let mut own_rows: Option<u64> = None;
        match begin_streamed_create_in_txn(&write_txn, &def) {
            Ok(()) => {}
            Err(CatalogError::TableAlreadyExists(_)) if stmt.if_not_exists() => return Ok(skipped),
            Err(e) => return Err(map_create_matview_error(e)),
        }
        if stmt.with_data() {
            let contexts = self.materialization_contexts_for(creator)?;
            let creator_key = ContextKey::from_context(creator);
            let mut seen: Vec<ContextKey> = Vec::new();
            for ctx in contexts {
                let is_creator = ContextKey::from_context(&ctx) == creator_key;
                let rows = match self.eval_matview_body_for(
                    &*write_txn,
                    &ctx,
                    session,
                    &plan,
                    &columns,
                    cap,
                ) {
                    Ok(rows) => rows,
                    // 作成者以外の文脈の評価失敗（行数超過・実行時エラー等）は他テナントのデータに
                    // 依存するため作成者へ返さない（成否の差で不可視な一致件数を探れてしまう。
                    // テナント境界・fail-closed）。その文脈の実体メタは保存せず、読み取りは
                    // 「未実体化」と同じ 55000 で拒否される（空の正常結果へは変換しない）。
                    Err(_) if !is_creator => continue,
                    Err(e) => return Err(e),
                };
                if is_creator {
                    own_rows = Some(rows.len() as u64);
                }
                write_context_entity_in_txn(&write_txn, &def, &ctx, &rows, &mut seen, cap)
                    .map_err(map_create_matview_error)?;
                drop(rows);
            }
        }
        finish_streamed_create_in_txn(&write_txn, &def.name).map_err(map_create_matview_error)?;
        // 表単位の世代の bump は `create_materialized_view_in_txn` が同じ txn 内で
        // `bump_table_generation_in_txn` により済ませている（commit は唯一のこの 1 回）。
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(crate::catalog::convert_storage_error)
            .map_err(map_create_matview_error)?;
        Ok(CreateMaterializedViewOutcome {
            skipped: false,
            populated: stmt.with_data(),
            own_rows,
        })
    }

    /// 非バイト順照合の列を持つ表を本文が読むなら `0A000`（`check_buffered_view_collations` と同じ判定）。
    fn check_matview_body_collations(
        &self,
        read_txn: &impl crate::storage::read_source::ReadSource,
        stmt: &Statement,
    ) -> Result<(), SqlSurfaceError> {
        let mut names = Vec::new();
        super::buffered_body_tables(stmt, &mut names);
        let schemas = super::table_schemas_in(read_txn, &names)?;
        let collated = schemas.values().any(|schema| {
            schema
                .columns
                .iter()
                .any(|c| c.collation.is_some_and(|coll| !coll.is_byte_order()))
        });
        if collated {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "this collation is not supported in this expression".to_string(),
            });
        }
        Ok(())
    }

    /// 本文を文脈 `ctx` の `PolicyContext` で評価し、実体の行値へ変換する。行数が `cap` を超えたら
    /// `54000`（文脈ごとに数え、合計では数えない）。
    fn eval_matview_body_for(
        &self,
        read_txn: &impl crate::storage::read_source::ReadSource,
        ctx: &PolicyContext,
        creator_session: &SessionState,
        plan: &EvalPlan,
        columns: &[crate::catalog::ColumnDef],
        cap: usize,
    ) -> Result<Vec<Vec<Value>>, SqlSurfaceError> {
        // 評価用セッションは UDF・検索モードを引き継がず（本文は UDF に依存できない）、
        // 作成者の TimeZone だけを写す。
        let mut eval_session = SessionState::default();
        eval_session.set_time_zone(creator_session.time_zone().clone());
        let outcome = self.execute_read_statement_with_limits(
            ctx,
            &mut eval_session,
            plan.stmt.clone(),
            read_txn,
            plan.limits,
        )?;
        let crate::sql::SqlOutcome::Query(result) = outcome else {
            return Err(SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            });
        };
        if result.columns.len() != columns.len() {
            return Err(SqlSurfaceError::Internal {
                detail: "materialized view body changed during the statement".to_string(),
            });
        }
        if result.rows.len() > cap {
            return Err(SqlSurfaceError::payload_too_large(
                "materialized view result exceeds the row limit",
            ));
        }
        let mut rows: Vec<Vec<Value>> = Vec::new();
        rows.try_reserve_exact(result.rows.len())
            .map_err(|_| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        for row in result.rows {
            if row.cells.len() != columns.len() {
                return Err(SqlSurfaceError::Internal {
                    detail: "materialized view body changed during the statement".to_string(),
                });
            }
            let mut values = Vec::with_capacity(columns.len());
            for (cell, col) in row.cells.into_iter().zip(columns) {
                values.push(crate::sql::matview::cell_to_value(cell, &col.ty)?);
            }
            rows.push(values);
        }
        Ok(rows)
    }
}
