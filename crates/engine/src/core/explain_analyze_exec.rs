//! `EXPLAIN ANALYZE <SELECT>` の実行本体（Issue #2054・SQL-76 (2)(3) のポインタ）。
//!
//! 呼び出し元は `core.rs` の `ParsedSql::ExplainAnalyze` の実行腕（autocommit・明示トランザクション）。
//! 静的な計画（オプションなしの `EXPLAIN` と同じ経路）を作り、続けて内側の SELECT を通常の SELECT と
//! 同じ実行器で実際に実行して結果の行数と経過時間を計画へ併記する（`sql::explain_analyze::annotate`）。
//! 結果の行はクライアントへ返さず捨てる。
//!
//! DML 版（Issue #2055）は [`EngineCore::run_explain_analyze_dml`]。計画用と実行用の `ParsedSql` を同じ検証済みの値
//! から作り（[`plan_parsed`]・[`exec_parsed`]）、呼び出し元が渡す通常の DML 実行関数で実際に書き込む。
//!
//! テナント境界: 内側の実行は通常の SELECT と同じ RLS・資源上限・文の期限の下で動くため、`actual_rows` は
//! 可視行に限られる。実行時のエラーは通常の SELECT と同じ `wire_code` でそのまま返し、計画は返さない。
//! 読み取り源は呼び出し元が `run` で決める（autocommit は確定済みの状態、明示トランザクションは
//! 通常の SELECT と同じ読み取り源）。

use super::{EngineCore, ParsedSql};
use crate::sql::allowlist::{DeleteStatement, ValidatedUpdateForm};
use crate::sql::allowlist::{SqlSurfaceError, Statement};
use crate::sql::explain_analyze::{
    annotate, executable_statement, AnalyzeStats, ExplainAnalyzeDml, ExplainAnalyzeDmlTarget,
    ExplainAnalyzeSelect,
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

/// 計画用の文（静的な `EXPLAIN <DML>` と同じ `ParsedSql`）。副作用なし。
fn plan_parsed(ea: &ExplainAnalyzeDml) -> ParsedSql {
    match &ea.target {
        ExplainAnalyzeDmlTarget::Dml(d) => ParsedSql::ExplainDml(d.clone()),
        ExplainAnalyzeDmlTarget::Insert(t) => ParsedSql::ExplainInsert(t.clone()),
    }
}

/// 実行用の文（同じ本文を `EXPLAIN` なしで検証したときの `ParsedSql`）。
/// 計画用と同じ検証済みの値から作るため、計画と実行の文はずれない。
fn exec_parsed(ea: &ExplainAnalyzeDml) -> ParsedSql {
    use crate::sql::explain_dml::ExplainDml;
    use crate::sql::insert_explain::ExplainInsertTarget;
    match &ea.target {
        ExplainAnalyzeDmlTarget::Dml(ExplainDml::Delete(v)) => {
            ParsedSql::Delete(DeleteStatement::Predicate(v.clone()))
        }
        ExplainAnalyzeDmlTarget::Dml(ExplainDml::Update(v)) => {
            ParsedSql::Update(ValidatedUpdateForm::Predicate(v.clone()))
        }
        ExplainAnalyzeDmlTarget::Insert(t) => match &**t {
            ExplainInsertTarget::Values(v) => ParsedSql::Insert((**v).clone()),
            ExplainInsertTarget::Select { insert, .. } => {
                ParsedSql::InsertSelect((**insert).clone())
            }
        },
    }
}

impl EngineCore {
    /// `ParsedSql::ExplainAnalyzeDml` の実行。`run` は単一の `ParsedSql` を呼び出し元の経路
    /// （autocommit／明示トランザクション）で実行する関数で、通常の DML と同じ書き込み経路を通る。
    /// 手順: (1) 静的な計画を作る（計時。失敗なら何も書かない）→ (2) 内側の DML を実際に実行する（計時）→
    /// (3) 計画へ併記して返す。根の段の `actual_rows` は文がクライアントへ返したはずの行数
    /// （`RETURNING` の可視行。無ければ 0）で、`RETURNING` の行そのものは捨てる。
    /// 自動では巻き戻さない（利用者が `BEGIN`／`ROLLBACK` で囲む）。
    pub(super) fn run_explain_analyze_dml(
        &self,
        session: &mut SessionState,
        ea: &ExplainAnalyzeDml,
        mut run: impl FnMut(&mut SessionState, &ParsedSql) -> Result<SqlOutcome, SqlSurfaceError>,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let exec = exec_parsed(ea);
        // 部分クエリ述語の DML は暗黙トランザクションへの振り分けが要る。EXPLAIN DML の構文段が拒否済みで
        // 到達しない想定だが、振り分けを黙って飛ばさないための保険（fail-closed）。
        if Self::is_subquery_predicate_dml(&exec) {
            return Err(internal());
        }
        let planning_start = Instant::now();
        let plan = match run(session, &plan_parsed(ea))? {
            SqlOutcome::Explain(plan) => plan,
            _ => return Err(internal()),
        };
        let planning_ms = planning_start.elapsed().as_secs_f64() * 1000.0;

        let exec_start = Instant::now();
        let rows = match run(session, &exec)? {
            SqlOutcome::Returning(o) => o.result.rows.len(),
            SqlOutcome::Insert(_) | SqlOutcome::Update(_) | SqlOutcome::Delete(_) => 0,
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
