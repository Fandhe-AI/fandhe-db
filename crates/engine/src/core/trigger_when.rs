//! 行トリガの `WHEN` 条件の定義時検査と発火時評価（TABLE-45・SQL-68・RLS-10。Issue #3190。
//! 設計は `docs/design/trigger-execution-model.md` の D7 と `docs/design/plpgsql-subset.md` の
//! 「Issue #3190 追記」）。
//!
//! 役割: 保存済みの条件テキスト（`TriggerDef::when_sql`。構文段 `sql::allowlist::trigger` が検査・正規化済み）を、
//! `BEGIN IF (<条件>) THEN RETURN NEW; END IF; RETURN NULL; END` の合成本体として既存の PL/pgSQL の条件評価へ
//! 載せる。新しい評価器は作らず、三値論理・数値の規則・NEW／OLD の束縛を通常のトリガ本体と共有する。
//! 呼び出し元は `core.rs` の `CREATE TRIGGER`（[`EngineCore::validate_trigger_when`]）と、発火側
//! （`trigger::fire` → `PlpgsqlTriggerInvoker::eval_when` → `impl TriggerHost for EngineCore`）。
//!
//! fail-closed: 評価は `may_write: false`（揮発性ゲートで書き込み文は構造上通らない）。保存値は本体へ
//! 連結する前に再度字句解析して構造（括弧の対応・`;` なし・サブクエリ／非組み込み関数なし）を検査する。
//! エラーは固定の英語文言で、値・列名・内部キー・他テナントの情報を含めない。

use super::plpgsql_host::{PlCallOptions, TriggerInvocation};
use super::EngineCore;
use crate::catalog::trigger::{TriggerEvents, TriggerLevel};
use crate::plpgsql::ast::{ParseOptions, ReturnShape};
use crate::plpgsql::embedded::validate_embedded;
use crate::plpgsql::parse_body;
use crate::sql::allowlist::{SqlSurfaceError, ValidatedCreateTrigger};
use crate::sql::lexer::Token;
use crate::trigger::call::{trigger_vars_from_call, TriggerCall, TriggerOp, TriggerOutcome};

/// 発火元の主体の写しが無いときの固定文言。
const NO_SUBJECT: &str = "trigger WHEN evaluation requires a session context";

/// 保存値が不正（破損・改ざん）のときの固定文言。
const INVALID_STORED_WHEN: &str = "trigger WHEN condition is not supported";

/// 条件テキストを構造検査して合成本体を作る。`return_old` が真なら `RETURN OLD`（DELETE 用）。
/// 検査は保存値の再検査を兼ねる（`;`・括弧の不整合・サブクエリ・非組み込み関数があれば `0A000`）。
fn compose_when_body(when_sql: &str, return_old: bool) -> Result<String, SqlSurfaceError> {
    let tokens = crate::sql::lexer::tokenize(when_sql)
        .map_err(|_| SqlSurfaceError::feature_not_supported(INVALID_STORED_WHEN))?;
    crate::sql::allowlist::validate_when_tokens(&tokens)
        .map_err(|_| SqlSurfaceError::feature_not_supported(INVALID_STORED_WHEN))?;
    let row = if return_old { "OLD" } else { "NEW" };
    Ok(format!(
        "BEGIN IF ({when_sql}) THEN RETURN {row}; END IF; RETURN NULL; END"
    ))
}

/// 条件テキスト内の `NEW`／`OLD` の参照の有無（大小無視）。
fn references(when_sql: &str, which: &str) -> bool {
    crate::sql::lexer::tokenize(when_sql).is_ok_and(|ts| {
        ts.iter().any(|t| {
            matches!(t, Token::QualifiedIdent { qualifier, .. } if qualifier.eq_ignore_ascii_case(which))
        })
    })
}

impl EngineCore {
    /// `CREATE TRIGGER ... WHEN (...)` の定義時の意味検査（D7）。構文段の検査は済んでいる前提で、
    /// NEW／OLD の参照可否（`42P17`）・合成本体の分類（PL/pgSQL の定義時検証と同じ経路）・
    /// 列の存在を確かめる。`WHEN` が無ければ何もしない。
    pub(super) fn validate_trigger_when(
        &self,
        stmt: &ValidatedCreateTrigger,
    ) -> Result<(), SqlSurfaceError> {
        let Some(when_sql) = stmt.when_sql.as_deref() else {
            return Ok(());
        };
        let uses_new = references(when_sql, "NEW");
        let uses_old = references(when_sql, "OLD");
        if stmt.level == TriggerLevel::Statement && (uses_new || uses_old) {
            return Err(SqlSurfaceError::InvalidObjectDefinition {
                detail: "statement-level trigger WHEN conditions cannot reference NEW or OLD",
            });
        }
        if stmt.events & TriggerEvents::INSERT != 0 && uses_old {
            return Err(SqlSurfaceError::InvalidObjectDefinition {
                detail: "INSERT trigger WHEN conditions cannot reference OLD",
            });
        }
        if stmt.events & TriggerEvents::DELETE != 0 && uses_new {
            return Err(SqlSurfaceError::InvalidObjectDefinition {
                detail: "DELETE trigger WHEN conditions cannot reference NEW",
            });
        }
        if stmt.level == TriggerLevel::Statement {
            // 評価は行トリガ専用の枠組みのため、文トリガの WHEN は未対応（D7 より狭い。設計記録の既知の制限）。
            return Err(SqlSurfaceError::feature_not_supported(
                "WHEN on a statement-level trigger is not supported",
            ));
        }
        let only_delete = stmt.events == TriggerEvents::DELETE;
        let source = compose_when_body(when_sql, only_delete)?;
        let limits = &self.plpgsql_limits;
        let opts = ParseOptions::new(Vec::new(), ReturnShape::Trigger)
            .with_limits(limits.max_nesting_depth(), limits.max_body_bytes());
        let mut body = parse_body(&source, &opts)?;
        validate_embedded(&mut body, self)?;
        // 列の存在。表が無い・読めない場合は後続の `create_trigger` が固定の応答を返すため何もしない。
        if let Ok(schema) = self.storage.get_table_schema(&stmt.table) {
            if let Ok(tokens) = crate::sql::lexer::tokenize(when_sql) {
                for t in &tokens {
                    if let Token::QualifiedIdent { name, .. } = t {
                        // 発火時の NEW／OLD は利用者列だけが束縛される（`trigger_vars_from_call`）。
                        // 疑似列 `id` は束縛されないため、定義時に拒否する。
                        let known = schema.columns.iter().any(|c| &c.name == name);
                        if !known {
                            return Err(SqlSurfaceError::undefined_column(
                                "column in trigger WHEN condition",
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 行トリガの `WHEN` 条件を `call` の NEW／OLD で評価する。TRUE だけが `true`（FALSE・NULL は `false`）。
    /// 外側の write txn・呼び出しテナントの文脈の上で `may_write: false` として実行する。
    pub(super) fn eval_trigger_when_inner(
        &self,
        call: &TriggerCall<'_>,
        when_sql: &str,
    ) -> Result<bool, SqlSurfaceError> {
        let handle = call.writes;
        let ctx = handle.policy_context();
        let mut session = crate::trigger::subject::current()
            .ok_or_else(|| SqlSurfaceError::feature_not_supported(NO_SUBJECT))?;
        Self::check_resolution_tenant(&session, ctx)?;
        if call.level != TriggerLevel::Row {
            return Err(SqlSurfaceError::feature_not_supported(
                "WHEN on a statement-level trigger is not supported",
            ));
        }
        let source = compose_when_body(when_sql, call.op == TriggerOp::Delete)?;
        let limits = &self.plpgsql_limits;
        let opts = ParseOptions::new(Vec::new(), ReturnShape::Trigger)
            .with_limits(limits.max_nesting_depth(), limits.max_body_bytes());
        let mut body = parse_body(&source, &opts)?;
        validate_embedded(&mut body, self)?;
        let (table_name, table_schema) = self.trigger_table_display(ctx, call.table);
        let vars = trigger_vars_from_call(call, &table_name, &table_schema)?;
        let outcome = self.run_trigger_with_handle(
            ctx,
            &mut session,
            handle,
            TriggerInvocation {
                body: &body,
                vars,
                schema: call.schema,
                opts: PlCallOptions { may_write: false },
            },
        )?;
        Ok(matches!(outcome, TriggerOutcome::Proceed(_)))
    }
}
