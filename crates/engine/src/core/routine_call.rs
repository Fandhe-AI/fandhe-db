//! `LANGUAGE sql|plpgsql` 関数の呼び出し位置の実行（Issue #2218・SQL-67 (5)。設計は
//! `docs/design/plpgsql-subset.md` の「Issue #2218 追記」）。
//!
//! 役割: `FROM` の無い `SELECT` がセッションのルーチンを呼ぶとき、`execute_in_active_txn` の
//! `ConstSelect` 腕がここへ来る。式の畳み込み（`sql::routine_fold`）が評価した引数を [`RoutineCallHost`]
//! が受け取り、`core::plpgsql_host` の入口（`run_routine_active`）で本体を呼び出し元と同じ Tx・
//! `PolicyContext`（RLS）・権限で実行する。ルーチンを呼ぶ文は暗黙 Tx（`execute_parsed_in_session_inner`
//! が `execute_subquery_dml_autocommit` へ振り分ける）か明示 Tx の上でだけ実行されるため、本体の書き込みは
//! 外側の文と同じ write txn に載り、失敗時は副作用が残らない。テナントを切り替える API は持たない。
//!
//! 揮発性のゲート: `IMMUTABLE`／`STABLE` の関数は書き込めず（`0A000`）、その本体から呼んだ入れ子の関数も
//! 書き込めない（PostgreSQL より厳しいが fail-closed。[`NonWritingGuard`]）。呼び出し深さ（`54001`）と
//! 文数（`54000`）は `plpgsql::interp::ExecBudget` の共有カウンタが入れ子を跨いで数える。

use std::cell::Cell as StdCell;

use super::EngineCore;
use crate::plpgsql::host::PlScalar;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{SqlSurfaceError, ValidatedConstSelect};
use crate::sql::mode::SessionState;
use crate::sql::routine_fold::RoutineInvoker;
use crate::sql::transaction::SessionTransaction;
use crate::sql::udf_call::RoutineDefinition;

thread_local! {
    /// 書き込み不可の関数の実行中（入れ子の深さ）。0 でなければ入れ子の呼び出しも書き込めない。
    static NON_WRITING: StdCell<usize> = const { StdCell::new(0) };
}

/// 書き込み不可の関数の実行区間を現スレッドへ張る RAII（エラー・早期 return でも戻す）。
struct NonWritingGuard;

impl NonWritingGuard {
    fn enter() -> Self {
        NON_WRITING.with(|c| c.set(c.get().saturating_add(1)));
        Self
    }
}

impl Drop for NonWritingGuard {
    fn drop(&mut self) {
        NON_WRITING.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// 書き込み不可の関数の実行中か。
fn in_non_writing_call() -> bool {
    NON_WRITING.with(StdCell::get) > 0
}

/// [`RoutineInvoker`] の core 実装。1 回の文の実行の間だけ生きる（`'e` は `EngineCore` と Tx の寿命）。
struct RoutineCallHost<'a, 'e> {
    core: &'e EngineCore,
    ctx: &'a PolicyContext,
    session: &'a mut SessionState,
    txn: &'a mut SessionTransaction<'e>,
}

impl RoutineInvoker for RoutineCallHost<'_, '_> {
    fn invoke(
        &mut self,
        def: &RoutineDefinition,
        args: Vec<PlScalar>,
    ) -> Result<PlScalar, SqlSurfaceError> {
        let may_write = def.attrs.allows_writes() && !in_non_writing_call();
        let _guard = (!may_write).then(NonWritingGuard::enter);
        self.core
            .run_routine_active(self.ctx, self.session, self.txn, def, args, may_write)
    }
}

impl EngineCore {
    /// `parsed` が、セッションのルーチンを呼ぶ `FROM` なし `SELECT` か。autocommit の入口が暗黙 Tx へ
    /// 振り分ける判定に使う（ルーチンが 1 件も無いセッションでは式木を走査しない）。
    pub(super) fn calls_session_routine(session: &SessionState, parsed: &super::ParsedSql) -> bool {
        matches!(
            parsed,
            super::ParsedSql::Statement(crate::sql::allowlist::Statement::ConstSelect(v))
                if crate::sql::const_select::calls_routine(v, session.udfs())
        )
    }

    /// ルーチンを呼ぶ `FROM` なし `SELECT` を、開いている Tx（`InTransaction`）の上で実行する。呼び出し元は
    /// `execute_in_active_txn` の `ConstSelect` 腕だけ（エラー時の Tx 遷移は呼び出し元が担う）。
    pub(super) fn execute_const_select_routines<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        validated: &ValidatedConstSelect,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        // 本体の実行がセッションを可変借用するため、束縛・畳み込みにはレジストリの複製を使う
        // （本体は DDL を実行できず、実行中にレジストリは変わらない）。
        let udfs = session.udfs().clone();
        // `current_schema` 系を含む文だけ SCHEMA の実在を照会する（`execute_validated_in_session` と同じ）。
        let _schema_fn = if crate::sql::const_select::uses_schema_fn(validated) {
            let probe = |schema: &str| {
                self.storage
                    .schema_id(ctx.tenant_id(), schema)
                    .map(|id| id.is_some())
                    .map_err(crate::catalog::table_lookup_error)
            };
            let values = crate::sql::search_path::resolve_schema_functions(session, &probe)?;
            Some(crate::sql::search_path::SchemaFnScope::enter(
                crate::sql::search_path::SchemaFnMode::Values(values),
            ))
        } else {
            None
        };
        let mut host = RoutineCallHost {
            core: self,
            ctx,
            session,
            txn,
        };
        let result = crate::sql::const_select::run_with_routines(validated, &udfs, &mut host)?;
        Ok(crate::sql::SqlOutcome::Query(result))
    }
}
