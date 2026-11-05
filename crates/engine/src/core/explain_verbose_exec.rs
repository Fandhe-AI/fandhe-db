//! `EXPLAIN (VERBOSE)` の実行本体（Issue #2060・SQL-76 (4)(5) のポインタ）。
//!
//! 呼び出し元は `core.rs` の `ParsedSql::ExplainVerbose` の実行腕（autocommit・明示トランザクション）。
//! 中身（SELECT 系の `EXPLAIN`／述語形 `UPDATE`・`DELETE` の `EXPLAIN`）を呼び出し元の読み取り源で
//! 通常どおり計画し、根の段へ `sql::explain_verbose` の追加行（`output:`・`distance_metric:`）を足す。
//! 文は実行せず、行・台帳・採番・索引構築のいずれにも触れない。
//!
//! 結果列の導出は Describe と同じ経路（`USING PLAN` の質問展開・埋め込みの I/O を起こさない束縛のみ）。
//! 権限・テナント境界は中身と同じ（権限ゲートが `ExplainVerbose` を中身として分類する）。追加行は
//! 列名と距離の種別だけで、行数・可視件数・他テナントに依存する値を含まない。

use super::{EngineCore, ParsedSql};
use crate::sql::allowlist::{ExplainTarget, SqlSurfaceError, Statement};
use crate::sql::explain_analyze::executable_statement;
use crate::sql::explain_dml::ExplainDml;
use crate::sql::explain_verbose::{insert_at_root, metric_line, output_line};
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

impl EngineCore {
    /// `ParsedSql::ExplainVerbose` の実行。`run` は中身を呼び出し元の読み取り源で計画する関数。
    pub(super) fn run_explain_verbose(
        &self,
        session: &mut SessionState,
        inner: &ParsedSql,
        run: impl FnOnce(&mut SessionState, &ParsedSql) -> Result<SqlOutcome, SqlSurfaceError>,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let plan = match run(session, inner)? {
            SqlOutcome::Explain(plan) => plan,
            _ => return Err(internal()),
        };
        let extra = self.verbose_lines(session, inner)?;
        Ok(SqlOutcome::Explain(insert_at_root(plan, extra)?))
    }

    /// 中身の種別ごとに追加行を作る。
    fn verbose_lines(
        &self,
        session: &SessionState,
        inner: &ParsedSql,
    ) -> Result<Vec<String>, SqlSurfaceError> {
        let mut lines = Vec::new();
        match inner {
            ParsedSql::Statement(Statement::Explain(target)) => {
                let probe = ParsedSql::Statement(executable_statement(target));
                // 未解決の部分クエリ（`IN (SELECT ...)`／`EXISTS`）を含む WHERE は Describe の束縛が受理しない
                // （`42601`）。計画段は部分クエリを解決して作れているため、この場合だけ `output:` を省く
                // （追加情報の欠落であり、権限・テナント境界には影響しない）。
                //
                // 粗いセッションゲート（`coarse_temp_gate`）は再適用しない。中身の EXPLAIN は呼び出し元の
                // 実行入口で検査・計画済みであり、ここで作った `probe` は振り分け情報（`SessionRouted`）を
                // 持たないため、再適用すると無関係な一時表の存在や `search_path` だけで永続表への
                // `EXPLAIN (VERBOSE)` が `0A000` になる。列の読み取り源は計画（`Statement::Explain` は
                // 確定済みカタログを自前で引く）と同じ最新の確定済みカタログに揃える。
                match self.describe_parsed_in_session_impl(
                    session,
                    &probe,
                    &crate::sql::parser::LiteralSkip::NONE,
                    &[],
                    &self.result_limits,
                ) {
                    Ok(Some(columns)) => {
                        let names: Vec<&str> = columns
                            .iter()
                            .map(crate::sql::view_buffered::column_name)
                            .collect();
                        lines.extend(output_line(names)?);
                    }
                    Ok(None) => {}
                    Err(e) if e.wire_code() == "42601" => {}
                    Err(e) => return Err(e),
                }
                // `USING PLAN` の検索の質問埋め込みもコサイン系（`hnsw_cache::cosine_query_metric`）。
                if let ExplainTarget::Search(v) = target {
                    let kind = if v.using_plan().is_some() {
                        crate::sql::lexer::DistanceKind::Cosine
                    } else {
                        v.distance_kind()
                    };
                    lines.push(metric_line(kind));
                }
            }
            ParsedSql::ExplainDml(explain) => {
                let (table, returning) = match explain {
                    ExplainDml::Delete(v) => (v.table_name(), v.returning()),
                    ExplainDml::Update(v) => (v.table_name(), v.returning()),
                };
                let schema = self.load_table_schema_for_dml(table)?;
                if let Some(projection) = crate::sql::parser::bind_returning(returning, &schema)? {
                    let columns = crate::sql::returning::column_meta(&projection, &schema)?;
                    let names: Vec<&str> = columns
                        .iter()
                        .map(crate::sql::view_buffered::column_name)
                        .collect();
                    lines.extend(output_line(names)?);
                }
            }
            _ => return Err(internal()),
        }
        Ok(lines)
    }
}
