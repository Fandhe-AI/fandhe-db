//! セッション・Tx を持たない発火文脈でトリガ関数の本体を実行するホスト（TABLE-45・SQL-68・RLS-10。
//! Issue #3184。設計は `docs/design/plpgsql-subset.md` の「Issue #3184 追記」と実行モデルの D1・D9・D10）。
//!
//! 呼び出しの流れは `tenant` の書き込み経路 → `trigger::fire` → `TriggerRuntime::invoke` →
//! [`PlpgsqlTriggerInvoker`](crate::trigger::call::PlpgsqlTriggerInvoker) → 本モジュールの
//! `impl TriggerHost for EngineCore`。発火点が持つのは外側の write txn と呼び出しテナントの
//! `PolicyContext` の借用（[`TriggerWriteHandle`](crate::trigger::write_handle::TriggerWriteHandle)）だけで、
//! `SessionState`・`SessionTransaction` は無い。そこで次の 2 つで補う。
//!
//! - 主体: 文の実行入口が張った発火元セッションの主体の写し（`trigger::subject`）から派生セッションを作る。
//!   ロール・`search_path`・一時カタログ等は発火元のまま（権限を広げない）。区間が無ければ `0A000`
//! - 実行先: `core::plpgsql_host` の `CorePlHost` を、Tx ではなく窓口を実行先にした形（`HostTxn::Trigger`）で
//!   使う。式評価・条件評価・`42702` 検査・`%TYPE` 解決は通常の本体と共有し、SQL の実行（[`run_tokens_trigger`]）
//!   だけを本モジュールの文種ゲートへ回す
//!
//! fail-closed: 受け付ける文は「`FROM` なしの `SELECT`」・「`INSERT ... VALUES`（`ON CONFLICT`・`RETURNING`・
//! ビュー・一時表・SCHEMA 修飾なし）」・「`UPDATE`（単一行 `id` 指定形と述語形。`RETURNING`・`FROM`・WHERE 内
//! サブクエリ・ビュー・一時表・SCHEMA 修飾なし。Issue #3186）」だけの明示的な許可リストで、それ以外は固定文言の
//! `0A000`（`DELETE`・表を読む `SELECT` は #3187 が窓口を広げる）。本体の書き込みは台帳に記録しない
//! （外側の文の 1 エントリだけが台帳に載る。D10）。

use super::plpgsql_host::{CorePlHost, HostTxn, PlCallOptions, TriggerInvocation};
use super::{session_time_zone_scope, EngineCore, ParsedSql};
use crate::catalog::function::Security;
use crate::catalog::trigger::returns_trigger;
use crate::plpgsql::ast::{ParseOptions, ReturnShape};
use crate::plpgsql::embedded::validate_embedded;
use crate::plpgsql::parse_body;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::{SqlSurfaceError, Statement, ValidatedInsert, ValidatedUpdateForm};
use crate::sql::lexer::Token;
use crate::sql::SqlOutcome as CoreOutcome;
use crate::trigger::call::{trigger_vars_from_call, TriggerCall, TriggerHost, TriggerOutcome};
use crate::trigger::write_handle::TriggerWriteHandle;

/// 発火元の主体の写しが無い（セッションを持たない入口から発火した）ときの固定文言。
#[cfg_attr(not(test), allow(dead_code))]
const NO_SUBJECT: &str = "trigger function execution requires a session context";

/// 本体で実行できない形の固定文言（値・表名・他テナントの情報を含めない）。
const BODY_UNSUPPORTED: &str = "this statement is not supported inside a trigger function body";

thread_local! {
    /// トリガ本体の SQL の構文解析中だけ立つ。立っている間、書き込み文の `operation_id` 必須化
    /// （台帳必須の構成の `23502`）を解析段で課さない（本体の書き込みは台帳に載らない。D10）。
    static BODY_PARSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// トリガ本体の構文解析区間を張る RAII ガード。drop で直前の状態へ戻す。
struct BodyParseScope {
    prev: bool,
}

impl BodyParseScope {
    fn enter() -> Self {
        Self {
            prev: BODY_PARSE.with(|c| c.replace(true)),
        }
    }
}

impl Drop for BodyParseScope {
    fn drop(&mut self) {
        BODY_PARSE.with(|c| c.set(self.prev));
    }
}

/// 本体の `INSERT` が張った採番文脈の破棄後に、外側の文の払い出し値の記録を戻す RAII ガード。
/// `SequenceScope` より先に宣言して使う（drop は宣言の逆順）。
struct RestoreAssigned(Option<Vec<(usize, usize, i64)>>);

impl Drop for RestoreAssigned {
    fn drop(&mut self) {
        for (row, col, v) in self.0.take().unwrap_or_default() {
            crate::sql::sequence_scope::record_assigned(row, col, v);
        }
    }
}

fn unsupported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

impl TriggerHost for EngineCore {
    /// トリガ関数 1 回の実行。エラーは出口で内部キー（`#<id>.<name>`）を利用者向けの名前へ戻す
    /// （`scrub_error`）。Tx の `Failed` 化は発火側（`tenant` → `core` の既存経路）が担う。
    fn invoke_trigger(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        let ctx = call.writes.policy_context();
        self.invoke_trigger_inner(call)
            .map_err(|e| self.scrub_error(ctx, e))
    }
}

// 本番の invoker は #3185 が既定へ差し替えるまでテストからだけ構築されるため、呼び出し連鎖の末端も
// 通常ビルドでは未使用になる。
#[cfg_attr(not(test), allow(dead_code))]
impl EngineCore {
    /// 構文解析が書き込み文の `operation_id` 必須化の判定に使う台帳モード。通常は構成のとおりで、
    /// トリガ本体の解析中（[`BodyParseScope`]）だけ必須化を外す（`Ledgered` を `Optional` へ。台帳を持たない構成は
    /// そのまま）。外側の文の必須検査はトリガを呼ぶ前に済んでいる。
    pub(super) fn parse_ledger_mode(&self) -> LedgerMode {
        match self.ledger_mode {
            LedgerMode::Ledgered if BODY_PARSE.with(std::cell::Cell::get) => LedgerMode::Optional,
            mode => mode,
        }
    }

    /// [`TriggerHost::invoke_trigger`] の本体。関数定義の取得 → 定義時と同じ検証 → 変数の組み立て →
    /// 実行、の順に進む（実行モデル D9）。
    fn invoke_trigger_inner(
        &self,
        call: &TriggerCall<'_>,
    ) -> Result<TriggerOutcome, SqlSurfaceError> {
        let handle = call.writes;
        let ctx = handle.policy_context();
        // 主体の写し（発火元セッション）から派生セッションを作る。区間が無ければ拒否（fail-closed）。
        let mut session =
            crate::trigger::subject::current().ok_or_else(|| unsupported(NO_SUBJECT))?;
        // 写しのテナント束縛が発火文脈のテナントと食い違う使い回しは拒否する。
        Self::check_resolution_tenant(&session, ctx)?;

        // 関数定義は呼び出しテナントの scope からだけ引く（他テナントの定義は構造上引けず、不在と同じ応答）。
        let def = handle
            .function_def(call.function_key)?
            .ok_or_else(|| SqlSurfaceError::undefined_function("trigger function"))?;
        if !returns_trigger(&def) {
            return Err(unsupported(
                "the function is not a PL/pgSQL trigger function",
            ));
        }
        // 実行モデルの D9: SECURITY DEFINER のトリガ関数は未対応（所有者の権限へ昇格する経路を作らない）。
        if def.attrs.security == Security::Definer {
            return Err(unsupported(
                "SECURITY DEFINER trigger functions are not supported",
            ));
        }
        let limits = &self.plpgsql_limits;
        let opts = ParseOptions::new(Vec::new(), ReturnShape::Trigger)
            .with_limits(limits.max_nesting_depth(), limits.max_body_bytes());
        let mut body = parse_body(&def.body_source, &opts)?;
        validate_embedded(&mut body, self)?;

        // `TG_TABLE_NAME`・`TG_TABLE_SCHEMA` は利用者向けの名前へ戻す（内部キーを出さない）。
        let (table_name, table_schema) = self.trigger_table_display(ctx, call.table);
        let vars = trigger_vars_from_call(call, &table_name, &table_schema)?;
        self.run_trigger_with_handle(
            ctx,
            &mut session,
            handle,
            TriggerInvocation {
                body: &body,
                vars,
                schema: call.schema,
                opts: PlCallOptions {
                    may_write: def.attrs.allows_writes(),
                },
            },
        )
    }

    /// 表キーを（`TG_TABLE_NAME`, `TG_TABLE_SCHEMA`）へ。裸の名前は `public`、SCHEMA 配下の内部キー
    /// `#<id>.<name>` は呼び出しテナントの SCHEMA 名へ逆引きする（失敗時は `#<id>.` を落とした名前）。
    fn trigger_table_display(&self, ctx: &PolicyContext, key: &str) -> (String, String) {
        if !key.starts_with('#') {
            return (key.to_string(), "public".to_string());
        }
        let names = self.schema_display_names(ctx);
        let shown = crate::sql::relation_name::display_relation_keys(key, &names);
        match shown.split_once('.') {
            Some((schema, name)) if !schema.is_empty() => (name.to_string(), schema.to_string()),
            _ => (shown, "public".to_string()),
        }
    }

    /// トリガ本体の `INSERT ... VALUES` 1 文の実行（`ValidatedInsert` は許可リスト解析済み）。SQL `INSERT` と
    /// 同じ束縛・実行関数（`bind_insert_form`・`execute_insert_*_in`）を通し、外側の write txn は窓口の中に
    /// 留める。台帳は記録しない（`LedgerMode::Optional` で `operation_id` なし）。行数・バッチの上限は通常の
    /// `INSERT` と同じ。入れ子の発火には束縛済みの通過証を引き継ぐ（深さ上限 `54001`）。
    fn execute_trigger_insert(
        &self,
        handle: &TriggerWriteHandle<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &ValidatedInsert,
    ) -> Result<crate::sql::exec::InsertOutcome, SqlSurfaceError> {
        // 窓口が受けない形（`insert_row` と同じく、SCHEMA 修飾の内部キー・`ON CONFLICT`・`RETURNING`）。
        if stmt.on_conflict.is_some()
            || stmt.returning.is_some()
            || stmt.table_name.starts_with('#')
        {
            return Err(unsupported(BODY_UNSUPPORTED));
        }
        self.check_insert_row_count_limit(stmt.rows.len())?;
        // スキーマは外側と同じ write txn から読む（同じ Tx 内の DDL を取り込む）。
        let schema = handle.table_schema(&stmt.table_name)?;
        let _sparsevec_limits_scope = self.sparsevec_limits_scope();
        // 通常の `INSERT` と同じ契約で、束縛前に既定時刻の文脈を張る。外側の文が張った時刻（`INSERT` の
        // 既定時刻・式スコープの時刻）があれば引き継ぎ、無ければ（`UPDATE`／`DELETE` 等）ここで確定する。
        let clock = trigger_fallback_clock()?;
        let _default_clock_scope = crate::sql::time_fn::DefaultClockScope::enter(clock);
        // 採番の文脈（`nextval`・`id` 省略）は派生セッションの採番状態と発火元ロールで張る。外側の文が
        // 払い出した値の記録（`RETURNING` 用）は張り直しで消えるため、退避して戻す。
        // 先に宣言したガードが後に drop されるため、`SequenceScope` の破棄の後に戻る。
        let _restore_assigned = RestoreAssigned(Some(crate::sql::sequence_scope::take_assigned()));
        let _sequence_scope = crate::sql::sequence_scope::SequenceScope::enter(
            std::sync::Arc::clone(self.storage.sequence_allocator()),
            session.sequences_handle(),
            crate::sql::schema_privilege::acl_checked_principal(session),
        );
        let bound = crate::sql::parser::bind_insert_form(stmt, &schema)?;
        let runtime = self.trigger_runtime.bind(self);
        handle.run_insert(&stmt.table_name, |target| match bound {
            crate::sql::parser::BoundInsertForm::Row(b) => {
                crate::sql::exec::execute_insert_with_schema_in(
                    target,
                    ctx,
                    &b,
                    LedgerMode::CompareOnlyWithoutLedger,
                    None,
                    Some(&runtime),
                )
            }
            crate::sql::parser::BoundInsertForm::RowBatch(bs) => {
                self.validate_insert_row_batch_limits(&bs)?;
                crate::sql::exec::execute_insert_batch_with_schema_in(
                    target,
                    ctx,
                    &bs,
                    LedgerMode::CompareOnlyWithoutLedger,
                    Some(&schema),
                    Some(&runtime),
                )
            }
            // ファイル形・UPSERT は窓口では受けない（許可リスト）。
            _ => Err(unsupported(BODY_UNSUPPORTED)),
        })
    }
}

impl EngineCore {
    /// トリガ本体の `UPDATE` 1 文の実行（Issue #3186。`ValidatedUpdateForm` は許可リスト解析済み）。SQL `UPDATE` と
    /// 同じ束縛・実行関数（`bind_update_form`／`prepare_predicate_update`・`execute_update_with_schema_in`／
    /// `execute_predicate_update_in`）を通し、外側の write txn は窓口の中に留める。台帳は
    /// `CompareOnlyWithoutLedger` を明示して要求も記録もしない（`dml_ledger_mode()` は使わない。D10）。
    /// 入れ子の発火には束縛済みの通過証を引き継ぐ（深さ上限 `54001`）。
    fn execute_trigger_update(
        &self,
        handle: &TriggerWriteHandle<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        form: &ValidatedUpdateForm,
        has_subquery: bool,
    ) -> Result<crate::sql::exec::UpdateOutcome, SqlSurfaceError> {
        let (table, has_returning) = match form {
            ValidatedUpdateForm::Single(v) => (v.table_name.as_str(), v.returning.is_some()),
            ValidatedUpdateForm::Predicate(v) => (v.table_name(), v.returning().is_some()),
        };
        // 窓口が受けない形（SCHEMA 修飾の内部キー・`RETURNING`・表を読む WHERE のサブクエリ〔#3187〕）。
        if table.starts_with('#') || has_returning || has_subquery {
            return Err(unsupported(BODY_UNSUPPORTED));
        }
        // 外側の BEFORE ROW の行ごとループの最中に同じ表を書くと、外側が本体の更新を上書きする（D6）。
        if crate::tenant::trigger_row_loop::is_row_loop_active(table) {
            return Err(unsupported(BODY_UNSUPPORTED));
        }
        // スキーマは外側と同じ write txn から読む（同じ Tx 内の DDL を取り込む）。
        let schema = handle.table_schema(table)?;
        let _sparsevec_limits_scope = self.sparsevec_limits_scope();
        // 既定時刻・採番の文脈は `execute_trigger_insert` と同じ張り方（`SET col = DEFAULT`・`now()`・`nextval`）。
        let clock = trigger_fallback_clock()?;
        let _default_clock_scope = crate::sql::time_fn::DefaultClockScope::enter(clock);
        let _restore_assigned = RestoreAssigned(Some(crate::sql::sequence_scope::take_assigned()));
        let _sequence_scope = crate::sql::sequence_scope::SequenceScope::enter(
            std::sync::Arc::clone(self.storage.sequence_allocator()),
            session.sequences_handle(),
            crate::sql::schema_privilege::acl_checked_principal(session),
        );
        let runtime = self.trigger_runtime.bind(self);
        match form {
            ValidatedUpdateForm::Single(stmt) => {
                let bound = {
                    let set_columns: Vec<&str> =
                        stmt.assignments.iter().map(|(n, _)| n.as_str()).collect();
                    let _defer =
                        self.defer_not_null_scope_for_update(ctx, &schema.name, &set_columns);
                    crate::sql::parser::bind_update_form(form, &schema, session.udfs())?
                };
                let crate::sql::parser::BoundUpdateForm::Single(bound) = bound else {
                    return Err(SqlSurfaceError::Internal {
                        detail: "internal: bound predicate UPDATE from a single-row statement"
                            .to_string(),
                    });
                };
                handle.run_write(table, |target| {
                    crate::sql::exec::execute_update_with_schema_in(
                        target,
                        ctx,
                        &bound,
                        LedgerMode::CompareOnlyWithoutLedger,
                        Some(&schema),
                        Some(&runtime),
                    )
                })
            }
            ValidatedUpdateForm::Predicate(validated) => {
                let (predicate, hash, legacy) = {
                    let set_columns: Vec<&str> = validated
                        .assignments()
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .collect();
                    let _defer =
                        self.defer_not_null_scope_for_update(ctx, &schema.name, &set_columns);
                    Self::prepare_predicate_update(validated, validated, &schema, session.udfs())?
                };
                handle.run_write(table, |target| {
                    crate::sql::exec::execute_predicate_update_in(
                        target,
                        ctx,
                        &predicate,
                        LedgerMode::CompareOnlyWithoutLedger,
                        &schema,
                        &hash,
                        &legacy,
                        self.dml_limits.max_affected_rows,
                        self.scan_limits.max_scanned_rows(),
                        Some(&runtime),
                    )
                })
            }
        }
    }
}

/// トリガ本体の文が使う時刻の文脈。外側の文が張った時刻（`INSERT` の既定時刻・式スコープの時刻）が
/// あれば引き継ぎ、無ければ（`UPDATE`／`DELETE` 等）ここで確定する。
fn trigger_fallback_clock() -> Result<crate::sql::time_fn::StatementClock, SqlSurfaceError> {
    if let Some(c) =
        crate::sql::time_fn::current_default_clock().or_else(crate::sql::time_fn::current_clock)
    {
        return Ok(c);
    }
    let now = crate::sql::time_fn::now_micros()?;
    Ok(crate::sql::time_fn::StatementClock {
        txn_start_micros: now,
        stmt_start_micros: now,
    })
}

impl EngineCore {
    /// トリガ発火 1 回の式スコープの時刻。既に張られていれば `None`（外側の文・上位の発火と共有）、
    /// 無ければ [`trigger_fallback_clock`] で 1 度だけ確定した時刻を張って返す。
    pub(super) fn trigger_firing_clock_scope(
        &self,
    ) -> Result<Option<crate::sql::time_fn::ClockScope>, SqlSurfaceError> {
        match crate::sql::time_fn::current_clock() {
            Some(_) => Ok(None),
            None => Ok(Some(crate::sql::time_fn::ClockScope::enter(
                trigger_fallback_clock()?,
            ))),
        }
    }
}

impl CorePlHost<'_, '_> {
    /// トリガ発火文脈（[`HostTxn::Trigger`]）の 1 文の実行。許可リスト解析（権限ゲートは派生セッションの
    /// ロールで効く）→ 共通ゲート（文種・`USING OPERATION_ID`／`PARTITIONED`・揮発性）→ 本モジュールの
    /// 許可リストの順で、許可した形以外は固定文言の `0A000`（既存の `execute_in_active_txn` へ流さない）。
    pub(super) fn run_tokens_trigger(
        &mut self,
        tokens: Vec<Token>,
    ) -> Result<CoreOutcome, SqlSurfaceError> {
        let HostTxn::Trigger(backend) = &self.txn else {
            return Err(SqlSurfaceError::Internal {
                detail: "PL/pgSQL host backend mismatch".to_string(),
            });
        };
        let handle = backend.handle;
        // 式スコープの時刻は発火 1 回の入口（`trigger_firing_clock_scope`）が張っている。無い入口から直接
        // 呼ばれた場合に備え、ここでは無ければ補う（通常は張り済みで何もしない）。
        let _clock_scope = self.core.trigger_firing_clock_scope()?;
        let parsed = {
            // parse 中の定数式畳み込みがセッション TimeZone を読む。
            let _scopes = session_time_zone_scope(self.session);
            let _body_parse = BodyParseScope::enter();
            let resolver = self.core.schema_resolver_for(self.ctx, self.session);
            // 発火文脈には `BEGIN` 時点のスナップショットが無い。確定済みのカタログで解析する。
            self.core.parse_tokens_gated(
                tokens,
                self.session,
                self.ctx.tenant_id(),
                Some(&resolver),
                None,
            )?
        };
        let writes = self.gate_parsed(&parsed)?;
        // WHERE 内のサブクエリは表を読む（#3187 の範囲）ため、束縛・実行の前に判定しておく。
        let has_subquery = EngineCore::is_subquery_predicate_dml(&parsed);
        match parsed {
            // `FROM` なしの定数式 `SELECT`（式評価・条件の `where_probe`）。表を読まず、ルーチン呼び出しも
            // 含まない形に限る。派生セッションの UDF 登録は空なので、ルーチン呼び出しは解析段で `42883` になる
            // （fail-closed。下の判定は登録が非空になる将来の変更への保険）。
            ParsedSql::Statement(Statement::ConstSelect(validated)) if !writes => {
                if crate::sql::const_select::calls_routine(&validated, self.session.udfs()) {
                    return Err(unsupported(BODY_UNSUPPORTED));
                }
                self.core.execute_validated_in_session(
                    self.ctx,
                    self.session,
                    Statement::ConstSelect(validated),
                )
            }
            ParsedSql::Insert(stmt) if writes => {
                // 書き込みの試行を数える（`EXCEPTION` が巻き戻せないブロックで捕捉しないため。成功・失敗を問わない）。
                if let HostTxn::Trigger(b) = &mut self.txn {
                    b.write_attempts = b.write_attempts.saturating_add(1);
                }
                self.core
                    .execute_trigger_insert(handle, self.ctx, self.session, &stmt)
                    .map(CoreOutcome::Insert)
            }
            ParsedSql::Update(form) if writes => {
                // 書き込みの試行を数える（`INSERT` と同じ理由）。
                if let HostTxn::Trigger(b) = &mut self.txn {
                    b.write_attempts = b.write_attempts.saturating_add(1);
                }
                self.core
                    .execute_trigger_update(handle, self.ctx, self.session, &form, has_subquery)
                    .map(CoreOutcome::Update)
            }
            // `DELETE`・表を読む `SELECT`・`INSERT ... SELECT`・`UPDATE ... FROM`・ビュー・データ変更 CTE・
            // 一時表への振り分け等はすべて拒否する（許可リスト。`DELETE`・表を読む `SELECT` は #3187 が窓口を広げる）。
            _ => Err(unsupported(BODY_UNSUPPORTED)),
        }
    }
}
