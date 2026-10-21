//! `CREATE TABLE ... AS`（CTAS。SQL-52、Issue #1677・#1678）の `EngineCore` 側の実行本体。
//!
//! 責務境界: 構文検証は `sql::allowlist`（`ValidatedCreateTableAs`。カタログを照会しない）、出力列から
//! 列定義を導く純粋関数は `sql::ctas`（#1677）が担い、本モジュールは `core.rs` の
//! `execute_parsed_in_session_inner` の `CreateTableAs` 腕（DDL 権限ゲート・作成先判定の通過後）から呼ばれ、
//! それらを **1 つの write txn** の中で束ねる（表の作成と行の投入を原子的に行う）。`operation_id` の台帳・
//! 冪等な再送・入力側上限の判定点の集約は #1679 の担当で、本 Issue では `USING OPERATION_ID` を `0A000`、
//! 台帳必須モード（`Ledgered`）の `WITH DATA` を `23502` で拒否する（fail-closed）。
//!
//! 原子性: 書き込みゲート（単一ライタ）を取った write txn を読み取り源にも書き込み先にも使い、
//! (1) 出力列の確定、(2) 表の作成、(3) 内側の問い合わせの評価、(4) 行の投入、を同一スナップショット上で
//! 行って commit は 1 回だけにする（`matview_exec` の `CREATE MATERIALIZED VIEW` と同じ骨格）。途中の
//! どこで失敗しても commit 前に txn を破棄するため、表は残らない。クラッシュ時は redb の単一 commit が
//! 「作成前」か「作成後」のどちらかだけを見せる。CTAS が作る表は FK・DEFAULT・SEQUENCE を持たないため、
//! `WriteTarget::InTxn` で DEFERRED の検査が COMMIT へ先送りされる懸念は当たらない。
//!
//! テナント境界（P0）: 内側の問い合わせは書き込むセッションの `PolicyContext` で既存の読み取り経路
//! （RLS を暗黙に適用）にだけ通す（独自の走査を持たない）。挿入行の所有テナントと可視性は書き込み段が
//! `ctx` と `Private` に固定する。応答の行数は自セッションで挿入した件数だけ。内側の問い合わせの実カタログ
//! での再検証（`42P01` 等）は DDL 権限ゲートの後に行い、権限の無い主体へ表の存在を観測させない。
//! 評価中は書き込みゲートを保持するが、行数（`--max-result-rows`・`--max-insert-rows`・INDEX-4 の
//! バッチ上限）で有界にする。

use super::{EngineCore, ParsedSql};
use crate::catalog::{ColumnDef, ColumnType};
use crate::policy::PolicyContext;
use crate::result_limits::ResultLimits;
use crate::sql::allowlist::{SqlSurfaceError, Statement, ValidatedCreateTableAs};
use crate::sql::ddl::{map_create_table_catalog_error, CreateTableAsOutcome};
use crate::sql::exec::QueryResult;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::Storage;

impl EngineCore {
    /// `statement` の出力列と任意の列名リストから CTAS の列定義を導く。
    pub(crate) fn ctas_output_column_defs(
        &self,
        session: &crate::sql::mode::SessionState,
        statement: &Statement,
        column_names: &[String],
    ) -> Result<Vec<ColumnDef>, SqlSurfaceError> {
        let metas = self
            .describe_parsed_in_session_impl(
                session,
                &ParsedSql::Statement(statement.clone()),
                false,
                &[],
                &crate::result_limits::ResultLimits::default(),
            )?
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        let outputs = crate::sql::ctas::output_columns_from_metas(metas);
        crate::sql::ctas::derive_ctas_column_defs(&outputs, column_names)
    }

    /// `CREATE TABLE ... AS` の実行本体（SQL-52・Issue #1678）。呼び出し元の腕が DDL 権限ゲートと
    /// 作成先判定（`require_creation_target`）を通過済みであることが前提。
    ///
    /// 判定順（決定的）: `USING OPERATION_ID` `0A000` → 同名の関係の有無（`IF NOT EXISTS` は no-op、
    /// 無指定は `42P07`）→ 内側の問い合わせの実カタログ検証 → 台帳必須モードの `WITH DATA` `23502`
    /// → 書き込みゲート取得 → 出力列・列定義の導出と検査 → 表の作成 → （`WITH DATA`）評価・上限検査・
    /// 投入 → 世代 bump → commit（1 回だけ）。
    pub(super) fn execute_create_table_as(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        stmt: &ValidatedCreateTableAs,
    ) -> Result<CreateTableAsOutcome, SqlSurfaceError> {
        let internal = || SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        };
        let skipped = CreateTableAsOutcome {
            skipped: true,
            populated: false,
            rows: None,
        };
        // 台帳（#1679）が未実装のため、`operation_id` 付きは書き込みの前に拒否する（fail-closed）。
        if stmt.operation_id().is_some() {
            return Err(SqlSurfaceError::feature_not_supported(
                "CREATE TABLE AS with USING OPERATION_ID is not supported",
            ));
        }
        // 早期の名前衝突判定（高価な検証・評価の前に抜ける）。最終判定は下の write txn が担う。
        let exists = self
            .storage
            .relation_name_exists(stmt.table_name())
            .map_err(|_| internal())?;
        if exists {
            return if stmt.if_not_exists() {
                Ok(skipped)
            } else {
                Err(SqlSurfaceError::duplicate_table(
                    stmt.table_name().to_string(),
                ))
            };
        }
        // 構文段は構造だけを検証したため、実カタログでの検証（`42P01` 等）はここで行う。
        let query = crate::sql::allowlist::validate_sql_tokens_with_subquery_ctx_limits(
            &stmt.query_tokens,
            &self.storage,
            0,
            self.nesting_limits,
        )?;
        crate::sql::allowlist::require_row_query_statement(&query, "CREATE TABLE AS")?;
        // 台帳必須モードでは `operation_id` 無しの行投入を書き込みの前に拒否する（`WITH NO DATA` は行を
        // 書かないため対象外）。
        if stmt.with_data() {
            self.ledger_mode
                .resolve(None)
                .map_err(|_| SqlSurfaceError::MissingOperationId)?;
        }

        // 書き込みゲートを取る。以降の読み取りは、このゲートを保持している間は他の writer が commit できない
        // ため、write txn 開始時点のスナップショットと一致する。
        let write_txn = self
            .storage
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)
            .map_err(map_create_table_catalog_error)?;

        let columns = self.ctas_output_column_defs(session, &query, stmt.column_names())?;
        let validated = crate::sql::ctas::ctas_validated_create_table(
            stmt.table_name().to_string(),
            stmt.if_not_exists(),
            stmt.public_qualified,
            columns,
        );
        crate::sql::schema_dependency::check_create_table(&validated)?;
        let schema = crate::sql::ddl::build_table_schema(
            Some(&self.storage),
            &validated,
            &self.schema_limits,
        )?;
        // 配列列は代入変換が未対応で、0 行のときだけ成功する結果のぶれを避けるため、評価の前に決定的に拒否する。
        if stmt.with_data()
            && schema
                .columns
                .iter()
                .any(|c| matches!(c.ty, ColumnType::Array(_)))
        {
            return Err(SqlSurfaceError::feature_not_supported(
                "CREATE TABLE AS does not support array columns",
            ));
        }
        // 作った表は列の照合順序を引き継がないため、非バイト順照合の列を持つ表を読む問い合わせは拒否する。
        self.check_matview_body_collations(&*write_txn, &query)?;

        match Storage::create_table_in_write_txn(&write_txn, &schema) {
            Ok(()) => {}
            Err(crate::catalog::CatalogError::TableAlreadyExists(_)) if stmt.if_not_exists() => {
                return Ok(skipped);
            }
            Err(e) => return Err(map_create_table_catalog_error(e)),
        }

        let mut inserted: Option<u64> = None;
        if stmt.with_data() {
            inserted = Some(self.populate_ctas_table(ctx, session, &write_txn, stmt, query)?);
        }

        // 表の作成の世代 bump は commit の直前に行う（`commit_boundary::commit` の悉皆検査の契約）。
        crate::catalog::bump_table_generation_in_txn(&write_txn, stmt.table_name())
            .map_err(map_create_table_catalog_error)?;
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(crate::catalog::convert_storage_error)
            .map_err(map_create_table_catalog_error)?;
        Ok(CreateTableAsOutcome {
            skipped: false,
            populated: stmt.with_data(),
            rows: inserted,
        })
    }

    /// 内側の問い合わせを書き込むセッションの文脈で評価し、同じ write txn で作成済みの表へ投入する。
    /// 評価結果は全行を実体化してから上限（`--max-result-rows`・`--max-insert-rows`・INDEX-4）を判定し、
    /// すべて書き込みの前に行う（投入は既存の複数行 `VALUES`・`INSERT ... SELECT` と同じ書き込み経路。
    /// 第 2 の書き込み経路を作らない）。戻り値は挿入した行数。
    fn populate_ctas_table(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        write_txn: &redb::WriteTransaction,
        stmt: &ValidatedCreateTableAs,
        query: Statement,
    ) -> Result<u64, SqlSurfaceError> {
        let cap = self.insert_select_row_cap();
        let fetch = cap
            .checked_add(1)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        let limits = ResultLimits::new(fetch);

        // 時刻は文の先頭で 1 度だけ確定し、内側の問い合わせの `now()` で共有する（`INSERT ... SELECT` と同じ）。
        let outer_clock = crate::sql::time_fn::current_clock();
        let clock = match outer_clock {
            Some(c) => c,
            None => {
                let now = crate::sql::time_fn::now_micros()?;
                crate::sql::time_fn::StatementClock {
                    txn_start_micros: now,
                    stmt_start_micros: now,
                }
            }
        };
        let _expr_clock = if outer_clock.is_none() {
            Some(crate::sql::time_fn::ClockScope::enter(clock))
        } else {
            None
        };

        let outcome = if matches!(query, Statement::ConstSelect(_)) {
            // `FROM` なしの定数式はテーブルを読まないため、定数式の実行本体へ渡す。
            self.execute_validated_in_session(ctx, session, query)?
        } else {
            self.execute_read_statement_with_limits(ctx, session, query, write_txn, limits)?
        };
        let SqlOutcome::Query(QueryResult { columns, rows }) = outcome else {
            return Err(SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            });
        };
        let schema = super::table_schema_in(write_txn, stmt.table_name())?;
        if columns.len() != schema.columns.len() {
            return Err(SqlSurfaceError::Internal {
                detail: "CREATE TABLE AS result shape changed during the statement".to_string(),
            });
        }
        // 1 段目の上限。ここまで行は書いていない（表の作成も commit 前で、失敗すれば txn ごと破棄される）。
        if rows.len() > cap {
            return Err(SqlSurfaceError::payload_too_large(
                "CREATE TABLE AS result exceeds the row limit",
            ));
        }
        self.check_insert_row_count_limit(rows.len())?;
        let row_count = u64::try_from(rows.len()).map_err(|_| SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        })?;
        if rows.is_empty() {
            return Ok(0);
        }
        let targets = crate::sql::insert_select::resolve_target_columns(&schema, None)?;
        let bounds = crate::sql::insert_select::rows_to_bound_inserts(
            stmt.table_name(),
            &schema,
            &targets,
            rows,
            None,
            clock,
        )?;
        // 1 行でもバイト量・チャンク総量の上限（INDEX-4）を必ず通す（委譲先の書き込み段はこの検査を補わない）。
        self.validate_insert_row_batch_limits(&bounds)?;
        crate::sql::exec::execute_insert_batch_with_schema_in(
            crate::tenant::WriteTarget::InTxn(write_txn),
            ctx,
            &bounds,
            self.ledger_mode,
            Some(&schema),
        )?;
        Ok(row_count)
    }
}
/// 実ストレージ上の元表から CTAS の列定義を導く結合テスト（SQL-52 (1)・(5)）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnType, RowIdentity};
    use crate::kernel::CpuScalarProvider;
    use crate::policy::PolicyContext;
    use crate::schema_limits::SchemaLimits;
    use crate::sql::ctas::ctas_validated_create_table;
    use crate::sql::ddl::build_table_schema;
    use crate::sql::mode::SessionState;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn fixture(label: &str) -> (CleanupGuard, EngineCore) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let core = EngineCore::from_storage(
            Storage::open(&path).expect("open storage"),
            Box::new(CpuScalarProvider),
        );
        (guard, core)
    }

    fn ddl(core: &EngineCore, sql: &str) {
        let ctx = PolicyContext::with_visibilities("t1", [Visibility::Public, Visibility::Private])
            .expect("tenant");
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
    }

    fn defs(
        core: &EngineCore,
        sql: &str,
        names: &[&str],
    ) -> Result<Vec<ColumnDef>, SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_sql(sql, &core.storage).expect("valid select");
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        core.ctas_output_column_defs(&SessionState::default(), &stmt, &names)
    }

    fn setup(core: &EngineCore) {
        ddl(core, "CREATE TYPE mood AS ENUM ('happy', 'sad')");
        ddl(
            core,
            "CREATE TABLE src (a NUMERIC(10,2), b VARCHAR(8), c CHAR(4), d VECTOR(3), \
             e INTEGER[4], f mood, g TEXT)",
        );
    }

    #[test]
    fn sql52_1_qualified_types_flow_into_table_schema() {
        let (_g, core) = fixture("ctas-types");
        setup(&core);
        let d = defs(&core, "SELECT a, b, c, d, e, f, g FROM src LIMIT 1", &[]).unwrap();
        let v = ctas_validated_create_table("dst".to_string(), false, false, d);
        let schema = build_table_schema(Some(&core.storage), &v, &SchemaLimits::default()).unwrap();
        assert_eq!(schema.row_identity(), RowIdentity::Allocated);
        assert!(schema.primary_key().is_none());
        let src = core.storage.get_table_schema("src").expect("src");
        for (got, want) in schema.columns.iter().zip(src.columns.iter()) {
            assert_eq!(got.name, want.name);
            assert_eq!(got.ty, want.ty, "type of {}", got.name);
        }
        let nullable: Vec<bool> = schema.columns.iter().map(|c| c.nullable).collect();
        assert_eq!(nullable, [true, true, true, false, true, true, true]);
    }

    #[test]
    fn sql52_1_literals_and_expressions_have_types() {
        let (_g, core) = fixture("ctas-literals");
        setup(&core);
        // 文字列リテラルの投影は現行の SELECT 構文が受理しない（検証段で 42601）。
        // 受理される式は静的な型を持ち、Text 以外へ確定する。
        let d = defs(&core, "SELECT 1 + 1 AS n FROM src LIMIT 1", &[]).unwrap();
        assert!(d[0].ty != ColumnType::Text);
        let e = defs(&core, "SELECT 1 + 1 FROM src LIMIT 1", &[]).unwrap_err();
        assert_eq!(e.wire_code(), "42601");
        assert!(defs(&core, "SELECT 1 + 1 FROM src LIMIT 1", &["n"]).is_ok());
    }

    #[test]
    fn sql52_1_name_list_applies_to_described_columns() {
        let (_g, core) = fixture("ctas-id");
        setup(&core);
        let d = defs(&core, "SELECT g FROM src LIMIT 1", &["k"]).unwrap();
        assert_eq!(d[0].name, "k");
    }

    #[test]
    fn sql52_1_missing_source_fails_and_column_limit_applies() {
        let (_g, core) = fixture("ctas-limits");
        setup(&core);
        let missing =
            crate::sql::allowlist::validate_sql("SELECT a FROM nope LIMIT 1", &core.storage);
        assert_eq!(missing.unwrap_err().wire_code(), "42P01");
        let d = defs(&core, "SELECT a, b, c FROM src LIMIT 1", &[]).unwrap();
        let v = ctas_validated_create_table("dst".to_string(), false, false, d);
        let tight = SchemaLimits::new(2, 1024, 1024);
        let err = build_table_schema(Some(&core.storage), &v, &tight).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn sql52_1_enum_is_rejected_for_temp_route() {
        let (_g, core) = fixture("ctas-enum-temp");
        setup(&core);
        let d = defs(&core, "SELECT f FROM src LIMIT 1", &[]).unwrap();
        let v = ctas_validated_create_table("dst".to_string(), false, false, d);
        let err = build_table_schema(None, &v, &SchemaLimits::default()).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }
}
