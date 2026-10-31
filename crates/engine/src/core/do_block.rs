//! `DO` ブロックの実行（SQL-68・Issue #2228。設計は `docs/design/plpgsql-subset.md` の
//! 「Issue #2228 追記」）。
//!
//! `ParsedSql::Do`（`sql::allowlist::do_block` が構造検証済み）の本体を、PL/pgSQL の構文解析器
//! （`plpgsql::parse_body`・戻り値の形は `Void`）→ 定義時検証（`validate_embedded`。core が許可リスト
//! 解析へ委譲）→ インタプリタ（`core::plpgsql_host` の入口）の順に処理する。呼び出し元は
//! `execute_in_active_txn`（暗黙・明示 Tx の上）だけで、autocommit の入口は `execute_subquery_dml_autocommit`
//! と同じ暗黙 Tx を張ってから同じ腕へ来る。したがって本体の全文は呼び出し元と同じ Tx・同じ
//! `PolicyContext`（RLS）で実行され、失敗時は副作用が残らない。テナント切り替えの API は持たない。
//!
//! 本体長は字句化の前に `PlpgsqlLimits` の上限で検査する（`parse_body` の先頭検査。`54000`）。`Ledgered`
//! 構成では operation_id を持てないため、本体の書き込みは `23502` で止まる（fail-closed）。

use super::plpgsql_host::{PlCallOptions, PlInvocation};
use super::EngineCore;
use crate::plpgsql::ast::{ParseOptions, ReturnShape};
use crate::plpgsql::embedded::validate_embedded;
use crate::plpgsql::interp::ReturnSpec;
use crate::plpgsql::parse_body;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{SqlSurfaceError, ValidatedDo};
use crate::sql::mode::SessionState;
use crate::sql::transaction::SessionTransaction;

impl EngineCore {
    /// 開いている Tx（`InTransaction`）の上で `DO` の本体を実行する。エラーの Tx 遷移（`Failed`）は
    /// 呼び出し元（`execute_parsed_in_txn_inner`）と入口（`run_plpgsql_in_txn`）が担う。
    pub(crate) fn execute_do_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        stmt: &ValidatedDo,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let limits = &self.plpgsql_limits;
        let opts = ParseOptions::new(Vec::new(), ReturnShape::Void)
            .with_limits(limits.max_nesting_depth(), limits.max_body_bytes());
        let mut body = parse_body(&stmt.body_source, &opts)?;
        validate_embedded(&mut body, self)?;
        let ret = ReturnSpec {
            shape: ReturnShape::Void,
            ty: None,
        };
        self.run_plpgsql_in_txn(
            ctx,
            session,
            txn,
            PlInvocation {
                body: &body,
                args: Vec::new(),
                ret: &ret,
                opts: PlCallOptions { may_write: true },
            },
        )?;
        Ok(crate::sql::SqlOutcome::Do)
    }
}
