//! `CREATE MATERIALIZED VIEW` の実行本体（TABLE-42 (2)(3)・Issue #1664。親: #1662）。
//!
//! 本モジュールは `REFRESH MATERIALIZED VIEW`（TABLE-42 (1)(3)(4)・Issue #1665）の実行本体も担う。
//! `CREATE` と同じ骨格（writer gate を保持した 1 つの write txn で、旧実体の消去・全文脈の再評価・
//! 書き込みを行い、commit は 1 回だけ）で、失敗時は txn 破棄により旧実体と定義が丸ごと残る。
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
    begin_streamed_create_in_txn, begin_streamed_refresh_in_txn, finish_streamed_create_in_txn,
    finish_streamed_refresh_in_txn, is_context_local_failure, write_context_entity_in_txn,
    ContextKey, MaterializedViewDef, MAX_MATVIEW_CONTEXTS,
};
use crate::catalog::CatalogError;
use crate::policy::PolicyContext;
use crate::result_limits::ResultLimits;
use crate::row_codec::Value;
use crate::sql::allowlist::{
    classify_view_body, validate_sql_tokens, SqlSurfaceError, Statement,
    ValidatedCreateMaterializedView, ValidatedRefreshMaterializedView, ViewBodyCheck, ViewBodyKind,
};
use crate::sql::ddl::{
    map_create_matview_error, map_refresh_matview_error, CreateMaterializedViewOutcome,
    RefreshMaterializedViewOutcome,
};
use crate::sql::lexer::{Keyword, Token};
use crate::sql::mode::SessionState;

/// 実行可能な本文 1 件と、その評価に渡す結果行数上限。
struct EvalPlan {
    stmt: Statement,
    limits: ResultLimits,
    /// 1 文脈あたりの行数上限（超過は `54000`）。
    cap: usize,
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

    /// 本文（格納形）を、文脈ごとに実行できる 1 つの読み取り文へ変える。1 文脈あたりの行数上限
    /// （`--max-result-rows`。算出は共有の `write_input_cap`・SQL-34・Issue #2212）も併せて決める。
    /// 単純形は、まず `LIMIT` を足さずに検証し、読み取り経路の暗黙上限（#1796）が受ける形なら設定値
    /// そのものを上限とする。受理されない形だけ `LIMIT cap+1` を注入して超過を検出する
    /// （ハード上限の 1 つ手前へ丸める）。評価後射影形（集計・LIMIT・JOIN 等）は検証済みの文をそのまま使い、
    /// 評価上限は既定値と `cap+1` の大きい方（設定値を上げれば本文の暗黙上限も上がる）。
    fn matview_eval_plan(&self, body_sql: &str) -> Result<EvalPlan, SqlSurfaceError> {
        let invalid_body = || SqlSurfaceError::Internal {
            detail: "invalid materialized view definition".to_string(),
        };
        let tokens =
            crate::sql::lexer::tokenize_stored_view_body(body_sql).map_err(|_| invalid_body())?;
        match classify_view_body(&tokens, &self.storage, ViewBodyCheck::Resolved)? {
            ViewBodyKind::Simple(_) => {
                if let Ok(stmt) = validate_sql_tokens(&tokens, &self.storage) {
                    let c = self.write_input_cap()?;
                    return Ok(EvalPlan {
                        stmt,
                        limits: c.eval_limits,
                        cap: c.cap,
                    });
                }
                let c = self.write_input_cap_injected()?;
                let mut limited = tokens;
                limited.push(Token::Keyword(Keyword::Limit));
                limited.push(Token::Number(c.fetch.to_string()));
                let stmt = validate_sql_tokens(&limited, &self.storage)?;
                Ok(EvalPlan {
                    stmt,
                    limits: c.eval_limits,
                    cap: c.cap,
                })
            }
            ViewBodyKind::Buffered { stmt, .. } => {
                let c = self.write_input_cap()?;
                Ok(EvalPlan {
                    stmt: *stmt,
                    limits: ResultLimits::new(
                        crate::result_limits::DEFAULT_MAX_RESULT_ROWS.max(c.fetch),
                    ),
                    cap: c.cap,
                })
            }
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
        let plan = self.matview_eval_plan(stmt.body_sql())?;
        let cap = plan.cap;
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
                // 他文脈の符号化・書き込み失敗（テナント ID 長等で結果の有無により成否が分かれうる）も
                // 評価失敗と同様に作成者へ返さない。write_context_entity_in_txn は失敗時に行・メタを
                // 残さないため、その文脈は未実体化（読み取りは 55000）になる。
                // ストレージエラー等（部分書き込みが残りうる失敗）は隠さず txn ごと破棄して返す。
                match write_context_entity_in_txn(&write_txn, &def, &ctx, &rows, &mut seen, cap) {
                    Ok(()) => {}
                    Err(ref e) if !is_creator && is_context_local_failure(e) => continue,
                    Err(e) => return Err(map_create_matview_error(e)),
                }
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

    /// `REFRESH MATERIALIZED VIEW` の実行本体（DDL 権限ゲートは呼び出し元が通過済み。
    /// TABLE-42 (1)(3)(4)・Issue #1665）。
    ///
    /// 判定順: 書き込みゲート → 定義のロード（無ければ `42P01`、表・ビュー・索引の名前なら
    /// `42809`）→ （`CONCURRENTLY` なら）未実体化 `0A000` → 適格な UNIQUE 索引なし `55000`
    /// → 列形状の再検証 → 照合順序 → 文脈ごとの評価と逐次書き込み
    /// → commit（1 回だけ）。評価の読み取り源と書き込みは同一 write txn で、読み手は MVCC
    /// スナップショットで旧実体を読み続けブロックされない。
    ///
    /// `CONCURRENTLY` は前提検査を通れば非 CONCURRENTLY と同じ経路・同じ結果（差分適用は採らない。
    /// 読み手は MVCC で旧実体を読み続け元からブロックされず、実体の行キーは連番で差分でも振り直しが
    /// 要るため）。
    ///
    /// 失敗方針（fail-closed・テナント境界）: 実行者自身の文脈の評価失敗はそのまま返し、txn 破棄で
    /// 全文脈の旧実体を残す。他の文脈の評価失敗は実行者へ返さず（成否の差から他テナントの件数・内容を
    /// 探れないため）、その文脈の実体を書かない＝以後の読み取りは `55000`。「その文脈だけ旧実体を残す」
    /// 案は、1 回の REFRESH で新旧スナップショットの実体が混在するため採らない（TABLE-42 (2)）。
    pub(super) fn execute_refresh_materialized_view(
        &self,
        refresher: &PolicyContext,
        session: &SessionState,
        stmt: &ValidatedRefreshMaterializedView,
    ) -> Result<RefreshMaterializedViewOutcome, SqlSurfaceError> {
        let write_txn = self
            .storage
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)
            .map_err(map_refresh_matview_error)?;
        let def = match begin_streamed_refresh_in_txn(&write_txn, stmt.name()) {
            Ok(def) => def,
            Err(CatalogError::ViewNotFound(name)) => {
                // 表・ビュー・索引と名前空間を共有する。存在すれば種別違い（42809）。
                return match self.storage.relation_name_exists(&name) {
                    Ok(true) => Err(SqlSurfaceError::WrongObjectType { name }),
                    Ok(false) => Err(SqlSurfaceError::UndefinedTable { name }),
                    Err(e) => Err(map_refresh_matview_error(e)),
                };
            }
            Err(e) => return Err(map_refresh_matview_error(e)),
        };

        // CONCURRENTLY の前提検査（TABLE-42 (4)・Issue #1666）。判定はカタログ構造だけで決まり
        // テナントのデータに依存しないため、成否から他テナントの情報は漏れない。ここでの Err は
        // txn ごと破棄され、begin_streamed_refresh_in_txn が消した旧実体・定義はそのまま残る。
        // 未実体化（0A000）を索引の有無（55000）より先に判定する（PostgreSQL と同順）。
        if stmt.concurrently() {
            if !def.populated {
                return Err(SqlSurfaceError::feature_not_supported(
                    "REFRESH MATERIALIZED VIEW CONCURRENTLY cannot be used when the materialized view is not populated",
                ));
            }
            if !crate::catalog::materialized::has_concurrent_refresh_unique_index_in_txn(
                &write_txn, &def.name,
            )
            .map_err(map_refresh_matview_error)?
            {
                return Err(SqlSurfaceError::MaterializedViewNotRefreshableConcurrently);
            }
        }

        if !stmt.with_data() {
            finish_streamed_refresh_in_txn(&write_txn, &def, false)
                .map_err(map_refresh_matview_error)?;
            // 表単位の世代の bump は finish_streamed_refresh_in_txn が同じ txn 内で
            // bump_table_generation_in_txn により済ませている（commit は唯一のこの 1 回）。
            crate::recovery::commit_boundary::commit(write_txn)
                .map_err(crate::catalog::convert_storage_error)
                .map_err(map_refresh_matview_error)?;
            return Ok(RefreshMaterializedViewOutcome { populated: false });
        }

        // 列形状の再検証。単純形本文の `*` は展開されず保存されるため、基表への `ADD COLUMN` で
        // 出力列が保存済みの列とずれうる。カタログ構造だけに依存しテナントのデータに依存しないため、
        // 実行者へ返しても他テナントの情報は漏れない。ずれたら旧実体を残して拒否する。
        let current = {
            let view_columns =
                self.view_output_columns(session, &def.body_sql, &def.column_aliases)?;
            crate::sql::matview::column_defs_from_view_columns(&view_columns)?
        };
        if current != def.columns {
            return Err(SqlSurfaceError::feature_not_supported(
                "materialized view definition no longer matches its source columns",
            ));
        }

        let plan = self.matview_eval_plan(&def.body_sql)?;
        let cap = plan.cap;
        self.check_matview_body_collations(&*write_txn, &plan.stmt)?;

        let contexts = self.materialization_contexts_for(refresher)?;
        let refresher_key = ContextKey::from_context(refresher);
        let mut seen: Vec<ContextKey> = Vec::new();
        for ctx in contexts {
            let is_refresher = ContextKey::from_context(&ctx) == refresher_key;
            let rows = match self.eval_matview_body_for(
                &*write_txn,
                &ctx,
                session,
                &plan,
                &def.columns,
                cap,
            ) {
                Ok(rows) => rows,
                Err(_) if !is_refresher => continue,
                Err(e) => return Err(e),
            };
            // 他文脈の符号化・書き込み失敗も評価失敗と同様に実行者へ返さない（失敗時は行・メタを
            // 残さず、その文脈は未実体化＝読み取りは 55000）。
            // ストレージエラー等（部分書き込みが残りうる失敗）は隠さず txn ごと破棄して返す。
            match write_context_entity_in_txn(&write_txn, &def, &ctx, &rows, &mut seen, cap) {
                Ok(()) => {}
                Err(ref e) if !is_refresher && is_context_local_failure(e) => continue,
                Err(e) => return Err(map_refresh_matview_error(e)),
            }
            drop(rows);
        }
        finish_streamed_refresh_in_txn(&write_txn, &def, true)
            .map_err(map_refresh_matview_error)?;
        // 表単位の世代の bump は finish_streamed_refresh_in_txn が同じ txn 内で
        // bump_table_generation_in_txn により済ませている（commit は唯一のこの 1 回）。
        #[cfg(feature = "test-sync-points")]
        self.storage
            .sync_point(crate::test_sync::SyncPoint::RefreshMaterializedViewBeforeCommit);
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(crate::catalog::convert_storage_error)
            .map_err(map_refresh_matview_error)?;
        Ok(RefreshMaterializedViewOutcome { populated: true })
    }

    /// 非バイト順照合の列を持つ表を本文が読むなら `0A000`（`check_buffered_view_collations` と同じ判定）。
    pub(super) fn check_matview_body_collations(
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
