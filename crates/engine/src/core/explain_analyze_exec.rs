//! `EXPLAIN ANALYZE <SELECT>` の実行本体（Issue #2054・SQL-76 (2)(3) のポインタ）。
//!
//! 呼び出し元は `core.rs` の `ParsedSql::ExplainAnalyze` の実行腕（autocommit・明示トランザクション）。
//! 静的な計画（オプションなしの `EXPLAIN` と同じ経路）を作り、続けて内側の SELECT を通常の SELECT と
//! 同じ実行器で実際に実行して結果の行数と経過時間を計画へ併記する（`sql::explain_analyze::annotate`）。
//! 結果の行はクライアントへ返さず捨てる。
//!
//! テナント境界: 内側の実行は通常の SELECT と同じ RLS・資源上限・文の期限の下で動くため、`actual_rows` は
//! 可視行に限られる。実行時のエラーは通常の SELECT と同じ `wire_code` でそのまま返し、計画は返さない。
//! 読み取り源は呼び出し元が `run` で決める（autocommit は確定済みの状態、明示トランザクションは
//! 通常の SELECT と同じ読み取り源）。

use super::EngineCore;
use crate::sql::allowlist::{SqlSurfaceError, Statement};
use crate::sql::explain_analyze::{
    annotate, executable_statement, AnalyzeStats, ExplainAnalyzeSelect,
};
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use std::time::Instant;

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

impl EngineCore {
    /// `ParsedSql::ExplainAnalyze` の実行。`run` は単一の文を呼び出し元の読み取り源で実行する関数。
    /// 手順: (1) 静的な計画を作る（計時）→ (2) 内側の SELECT を実行して行数を数える（計時）→
    /// (3) 計画へ併記して返す。
    pub(super) fn run_explain_analyze(
        &self,
        session: &mut SessionState,
        ea: &ExplainAnalyzeSelect,
        mut run: impl FnMut(&mut SessionState, Statement) -> Result<SqlOutcome, SqlSurfaceError>,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let planning_start = Instant::now();
        let plan = match run(session, Statement::Explain(ea.target.clone()))? {
            SqlOutcome::Explain(plan) => plan,
            _ => return Err(internal()),
        };
        let planning_ms = planning_start.elapsed().as_secs_f64() * 1000.0;

        let exec_start = Instant::now();
        let rows = match run(session, executable_statement(&ea.target))? {
            SqlOutcome::Query(result) => result.rows.len(),
            _ => return Err(internal()),
        };
        let execution_ms = exec_start.elapsed().as_secs_f64() * 1000.0;

        let stats = AnalyzeStats {
            rows,
            planning_ms,
            execution_ms,
        };
        Ok(SqlOutcome::Explain(annotate(
            plan, &stats, ea.timing, ea.summary,
        )?))
    }
}
