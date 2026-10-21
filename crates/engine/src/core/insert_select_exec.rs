//! `INSERT ... SELECT` の実行本体（SQL-51・Issue #1733。親: #1731）。
//!
//! 責務境界: 構文検証と束縛（列数・型の静的照合）は構文・束縛側（#1732）、内容照合ハッシュの
//! 文正規化（#1734）は台帳側の担当。本モジュールは検証済みの内側 SELECT を **書き込むセッションの
//! `PolicyContext`** で既存の読み取り経路（`execute_read_statement_with_limits`）に通して実体化し、
//! 結果行を `Vec<BoundInsert>` へ変換して、複数行 `VALUES`・COPY と同じ書き込み経路
//! （`sql::exec::execute_insert_*_in`）へ渡すところまでを担う（第 2 の書き込み経路を作らない）。
//!
//! テナント境界（P0）: 内側 SELECT は RLS を暗黙に適用する既存経路でしか評価しない（独自の走査を
//! 持たない）ため、他テナントの Private 行・不可視行は材料にならない。挿入行の所有テナントと
//! 可視性は書き込み段が `ctx` と `Private` に固定する。エラー文言には行の値・他テナントの存在を
//! 含めない（行数超過は固定文言の `54000`）。
//!
//! 原子性・上限: 内側の結果は全行を実体化してから書き込む（Halloween 問題なし。自己参照の
//! `INSERT INTO t SELECT ... FROM t` でも挿入行を同じ文は読まない）。上限は 2 段で、実体化時に
//! `--max-result-rows`（`cap+1` 超過検出）、挿入前に `--max-insert-rows` と INDEX-4 のバッチ上限を
//! 判定する。すべて書き込みの前に行うため、超過・変換失敗では何も書かれない。書き込みは既存の
//! 単一 write txn で行い、制約違反なども txn の abort で全体が拒否される（部分挿入なし）。
//!
//! スナップショット: autocommit は確定済みスナップショットで評価し、挿入は
//! `WriteTarget::Autocommit`（DEFERRED の FK 検査を含む既存の挙動）で行う。明示トランザクションは
//! 共有 write txn を読み取り源にも書き込み先にも使う（自トランザクションの未 commit 変更が見え、
//! ROLLBACK で破棄される）。設計判断は `docs/design/insert-select.md`。
//!
//! 呼び出し元: 構文・束縛側（#1732）が `ParsedSql` の INSERT ... SELECT 腕を結線するまでは
//! crate 内テストからのみ呼ばれる（結線は autocommit 腕・明示トランザクション腕の 2 箇所）。

// 結線前は crate 内テストからのみ呼ばれる（上記）。
#![cfg_attr(not(test), allow(dead_code))]

use super::EngineCore;
use crate::catalog::{CatalogError, TableSchema};
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::OperationId;
use crate::result_limits::{ResultLimits, MAX_RESULT_ROWS_HARD_CAP};
use crate::sql::allowlist::{Projection, SqlSurfaceError, Statement};
use crate::sql::exec::{InsertOutcome, QueryResult, ReturningOutcome};
use crate::sql::insert_select::{resolve_target_columns, rows_to_bound_inserts};
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::tenant::WriteTarget;

/// 実行モード。autocommit か、明示トランザクションの共有 write txn か。
pub(super) enum InsertSelectTxn<'a> {
    Autocommit,
    InTxn(&'a redb::WriteTransaction),
}

/// 検証済みの `INSERT ... SELECT` 1 文（#1732 の束縛結果から組み立てる受け口）。
pub(super) struct InsertSelectRequest<'a> {
    /// 挿入先の表名（存在確認済み）。
    pub table_name: &'a str,
    /// 列リスト。`None` は省略（宣言順。`resolve_target_columns` 参照）。
    pub columns: Option<&'a [String]>,
    /// 検証済みの内側 SELECT。LIMIT 省略時の `cap+1` 付与は束縛側の責務だが、実行側も
    /// 結果行数を `cap` と照合して超過を `54000` にする。
    pub select: Statement,
    pub operation_id: Option<OperationId>,
    pub returning: Option<&'a Projection>,
    /// `ON CONFLICT` が付いているか（併用は別 Issue のため `0A000`）。
    pub has_on_conflict: bool,
}

impl EngineCore {
    /// 内側 SELECT の行数上限。`--max-result-rows` を上限に、`+1` で超過を検出するため
    /// ハード上限の 1 つ手前へ丸める（matview の文脈ごと上限と同じ式。fail-closed）。
    pub(super) fn insert_select_row_cap(&self) -> usize {
        self.result_limits
            .max_result_rows()
            .min(MAX_RESULT_ROWS_HARD_CAP.saturating_sub(1))
    }

    /// `INSERT ... SELECT` の実行本体。手順は先頭の `//!` を参照（全検査を書き込みの前に行う）。
    pub(super) fn execute_insert_select(
        &self,
        mode: InsertSelectTxn<'_>,
        ctx: &PolicyContext,
        session: &mut SessionState,
        req: InsertSelectRequest<'_>,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        if req.has_on_conflict {
            return Err(SqlSurfaceError::feature_not_supported(
                "ON CONFLICT is not supported with INSERT ... SELECT",
            ));
        }
        let schema: TableSchema =
            self.storage
                .get_table_schema(req.table_name)
                .map_err(|e| match e {
                    CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
                    // 格納済みカタログ断片・I/O 情報を wire へ出さない固定文言（`execute_insert_form` と同じ）。
                    _ => SqlSurfaceError::Internal {
                        detail: "failed to load table schema".to_string(),
                    },
                })?;
        // RETURNING は評価より前に束縛する（未知の列は SELECT を走らせる前に `42703`）。
        let returning = crate::sql::parser::bind_returning(req.returning, &schema)?;
        let targets = resolve_target_columns(&schema, req.columns)?;

        let cap = self.insert_select_row_cap();
        let fetch = cap
            .checked_add(1)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        let limits = ResultLimits::new(fetch);

        // 内側 SELECT を書き込むセッションの文脈で評価する（RLS 暗黙適用）。
        let outcome = match &mode {
            InsertSelectTxn::Autocommit => {
                let read_txn = self.begin_read_txn()?;
                self.execute_read_statement_with_limits(
                    ctx, session, req.select, &read_txn, limits,
                )?
            }
            InsertSelectTxn::InTxn(write_txn) => self
                .execute_read_statement_with_limits(ctx, session, req.select, *write_txn, limits)?,
        };
        let SqlOutcome::Query(QueryResult { columns, rows }) = outcome else {
            return Err(SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            });
        };
        if columns.len() != targets.len() {
            return Err(SqlSurfaceError::Internal {
                detail: "INSERT ... SELECT result shape changed during the statement".to_string(),
            });
        }
        // 1 段目の上限。ここまで何も書いていない。
        if rows.len() > cap {
            return Err(SqlSurfaceError::payload_too_large(
                "INSERT ... SELECT result exceeds the row limit",
            ));
        }
        // 2 段目の上限の前に、行数だけで弾けるものを先に判定する（変換コストを払わない）。
        self.check_insert_row_count_limit(rows.len())?;

        let stmt_start_micros = crate::sql::time_fn::now_micros()?;
        let clock = crate::sql::time_fn::StatementClock {
            txn_start_micros: stmt_start_micros,
            stmt_start_micros,
        };
        let bounds = rows_to_bound_inserts(
            req.table_name,
            &schema,
            &targets,
            rows,
            req.operation_id.as_ref(),
            clock,
        )?;
        if bounds.len() > 1 {
            self.validate_insert_row_batch_limits(&bounds)?;
        }

        // 0 行は書き込みも台帳記録もしない（既存のバッチ経路は空を `22000` にするためここで分岐）。
        if bounds.is_empty() {
            return Ok(match returning {
                None => SqlOutcome::Insert(InsertOutcome {
                    rows_affected: 0,
                    incremental: None,
                }),
                Some(projection) => SqlOutcome::Returning(ReturningOutcome {
                    command: crate::sql::returning::DmlCommand::Insert,
                    rows_affected: 0,
                    result: QueryResult {
                        columns: crate::sql::returning::column_meta(&projection, &schema)?,
                        rows: Vec::new(),
                    },
                }),
            });
        }

        // `id` を省略した行（移行済み表）の採番は書き込み段が文脈から行う。autocommit で文脈が無い
        // 場合だけ、セッション状態なしの採番スコープを張る（`execute_insert_returning_in` と同じ）。
        let _local_scope = match (&mode, bounds.iter().any(|b| b.id_omitted)) {
            (InsertSelectTxn::Autocommit, true)
                if crate::sql::sequence_scope::current().is_none() =>
            {
                Some(
                    crate::sql::sequence_scope::SequenceScope::enter_sessionless(
                        std::sync::Arc::clone(self.storage.sequence_allocator()),
                    ),
                )
            }
            _ => None,
        };
        let _default_clock = crate::sql::time_fn::DefaultClockScope::enter(clock);

        let target = match &mode {
            InsertSelectTxn::Autocommit => WriteTarget::Autocommit(&self.storage),
            InsertSelectTxn::InTxn(write_txn) => WriteTarget::InTxn(write_txn),
        };
        match returning {
            None => crate::sql::exec::execute_insert_batch_with_schema_in(
                target,
                ctx,
                &bounds,
                self.ledger_mode,
                Some(&schema),
            )
            .map(SqlOutcome::Insert),
            Some(projection) => crate::sql::exec::execute_insert_returning_in(
                target,
                ctx,
                &bounds,
                self.ledger_mode,
                &projection,
                &schema,
            )
            .map(SqlOutcome::Returning),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType};
    use crate::sql::exec::Cell;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::tempdir;

    fn open(dir: &std::path::Path) -> EngineCore {
        let storage = Storage::open(dir.join("db.redb")).expect("open storage");
        for name in ["src", "dst"] {
            storage
                .create_table(&TableSchema::new(
                    name,
                    vec![
                        ColumnDef::new("embedding", ColumnType::Vector(2), false),
                        ColumnDef::new("lang", ColumnType::Text, false),
                    ],
                ))
                .expect("create table");
        }
        EngineCore::from_storage(storage, Box::new(crate::kernel::CpuScalarProvider))
    }

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    }

    fn run(core: &EngineCore, c: &PolicyContext, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
        let mut s = SessionState::default();
        core.execute_sql_in_session(c, &mut s, sql)
    }

    fn seed(core: &EngineCore, c: &PolicyContext, table: &str, n: u64, op: &str) {
        let rows: Vec<String> = (1..=n)
            .map(|i| format!("({i}, '[{i}.0,0.0]', 'ja')"))
            .collect();
        run(
            core,
            c,
            &format!(
                "INSERT INTO {table} (id, embedding, lang) VALUES {} USING OPERATION_ID '{op}'",
                rows.join(", ")
            ),
        )
        .expect("seed");
    }

    fn select_stmt(core: &EngineCore, sql: &str) -> Statement {
        let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
        crate::sql::allowlist::validate_sql_tokens(&tokens, &core.storage).expect("validate")
    }

    fn count(core: &EngineCore, c: &PolicyContext, table: &str) -> u64 {
        let SqlOutcome::Query(q) =
            run(core, c, &format!("SELECT COUNT(*) FROM {table}")).expect("count")
        else {
            panic!("expected query");
        };
        match q.rows[0].cells[0] {
            Cell::Integer(v) => v,
            ref o => panic!("unexpected {o:?}"),
        }
    }

    fn insert_select(
        core: &EngineCore,
        c: &PolicyContext,
        select: &str,
        op: &str,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let cols = vec![
            "id".to_string(),
            "embedding".to_string(),
            "lang".to_string(),
        ];
        let mut s = SessionState::default();
        core.execute_insert_select(
            InsertSelectTxn::Autocommit,
            c,
            &mut s,
            InsertSelectRequest {
                table_name: "dst",
                columns: Some(&cols),
                select: select_stmt(core, select),
                operation_id: Some(OperationId::parse(op).expect("op id")),
                returning: None,
                has_on_conflict: false,
            },
        )
    }

    // 対象ビヘイビア: SQL-51・RLS-7（ポインタ）。他テナントの Private 行は材料にならない。
    #[test]
    fn inner_select_applies_rls_of_writing_session() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        let b = ctx("tenant-b");
        seed(&core, &a, "src", 2, "op-seed-a");
        seed(&core, &b, "src", 3, "op-seed-b");

        let out = insert_select(
            &core,
            &a,
            "SELECT id, embedding, lang FROM src LIMIT 100",
            "op-is-a",
        )
        .expect("insert select");
        assert!(matches!(
            out,
            SqlOutcome::Insert(InsertOutcome {
                rows_affected: 2,
                ..
            })
        ));
        assert_eq!(count(&core, &a, "dst"), 2);
        // B の視点では A の挿入行は見えない（所有は A の Private 行）。
        assert_eq!(count(&core, &b, "dst"), 0);
    }

    // 対象ビヘイビア: SQL-51・RLS-10（ポインタ）。他テナントの行だけに当たる場合は
    // 該当行が存在しない場合と同じ応答（`INSERT 0 0`）になる。
    #[test]
    fn invisible_rows_behave_like_no_rows() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        let b = ctx("tenant-b");
        seed(&core, &b, "src", 3, "op-seed-b");

        let out = insert_select(
            &core,
            &a,
            "SELECT id, embedding, lang FROM src LIMIT 100",
            "op-is-none",
        )
        .expect("insert select");
        assert!(matches!(
            out,
            SqlOutcome::Insert(InsertOutcome {
                rows_affected: 0,
                ..
            })
        ));
        assert_eq!(count(&core, &a, "dst"), 0);
    }

    // 対象ビヘイビア: SQL-51・SQL-34（ポインタ）。結果行数上限の超過は `54000`・副作用なし。
    #[test]
    fn result_row_cap_overflow_is_54000_without_side_effects() {
        let dir = tempdir();
        let core = open(dir.path()).with_result_limits(ResultLimits::new(2));
        let a = ctx("tenant-a");
        seed(&core, &a, "src", 5, "op-seed-a");

        let err = insert_select(
            &core,
            &a,
            "SELECT id, embedding, lang FROM src LIMIT 3",
            "op-is-cap",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "54000");
        assert_eq!(count(&core, &a, "dst"), 0);
    }

    // 対象ビヘイビア: SQL-51・INDEX-4（ポインタ）。挿入行数上限の超過も `54000`・副作用なし。
    #[test]
    fn insert_row_limit_overflow_is_54000_without_side_effects() {
        let dir = tempdir();
        let core = open(dir.path()).with_dml_limits(crate::sql::parser::DmlLimits {
            max_insert_rows_per_statement: std::num::NonZeroUsize::new(2),
            ..Default::default()
        });
        let a = ctx("tenant-a");
        seed(&core, &a, "src", 2, "op-seed-a");
        // 上限 2 のため seed は 2 行まで。3 行目は別 INSERT で足す。
        run(
            &core,
            &a,
            "INSERT INTO src (id, embedding, lang) VALUES (3, '[3.0,0.0]', 'ja') \
             USING OPERATION_ID 'op-seed-3'",
        )
        .expect("seed 3");

        let err = insert_select(
            &core,
            &a,
            "SELECT id, embedding, lang FROM src LIMIT 100",
            "op-is-limit",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "54000");
        assert_eq!(count(&core, &a, "dst"), 0);
    }

    // 対象ビヘイビア: SQL-51（ポインタ）。途中行の制約違反で文全体が拒否され 1 行も残らない。
    #[test]
    fn constraint_violation_rejects_whole_statement() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        seed(&core, &a, "src", 3, "op-seed-a");
        seed(&core, &a, "dst", 1, "op-seed-dst");

        // dst に id=1 が既にあり、src の id=1 と衝突する。
        let err = insert_select(
            &core,
            &a,
            "SELECT id, embedding, lang FROM src LIMIT 100",
            "op-is-dup",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "23505");
        assert_eq!(count(&core, &a, "dst"), 1);
    }

    // 対象ビヘイビア: SQL-51・RECOVER-10（ポインタ）。同じ operation_id・同じ文の再送は二重挿入しない。
    #[test]
    fn same_operation_id_resend_does_not_double_insert() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        seed(&core, &a, "src", 2, "op-seed-a");
        let sql = "SELECT id, embedding, lang FROM src LIMIT 100";
        insert_select(&core, &a, sql, "op-is-resend").expect("first");
        let _ = insert_select(&core, &a, sql, "op-is-resend");
        assert_eq!(count(&core, &a, "dst"), 2);
    }

    // 対象ビヘイビア: SQL-51・SQL-21（ポインタ）。RETURNING は VALUES 形と同じ列・行を返す。
    #[test]
    fn returning_matches_values_form() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        seed(&core, &a, "src", 2, "op-seed-a");

        let returning_stmt = crate::sql::allowlist::validate_insert(
            "INSERT INTO dst (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') \
             RETURNING id, lang USING OPERATION_ID 'op-shape'",
            &core.storage,
            core.ledger_mode,
        )
        .expect("validate");
        let projection = returning_stmt.returning.clone().expect("returning");
        let cols = vec![
            "id".to_string(),
            "embedding".to_string(),
            "lang".to_string(),
        ];
        let mut s = SessionState::default();
        let out = core
            .execute_insert_select(
                InsertSelectTxn::Autocommit,
                &a,
                &mut s,
                InsertSelectRequest {
                    table_name: "dst",
                    columns: Some(&cols),
                    select: select_stmt(
                        &core,
                        "SELECT id, embedding, lang FROM src ORDER BY id LIMIT 100",
                    ),
                    operation_id: Some(OperationId::parse("op-is-ret").expect("op")),
                    returning: Some(&projection),
                    has_on_conflict: false,
                },
            )
            .expect("insert select returning");
        let SqlOutcome::Returning(sel) = out else {
            panic!("expected returning");
        };

        let values = run(
            &core,
            &a,
            "INSERT INTO src (id, embedding, lang) VALUES (11, '[1.0,0.0]', 'ja'), \
             (12, '[2.0,0.0]', 'ja') RETURNING id, lang USING OPERATION_ID 'op-values'",
        )
        .expect("values");
        let SqlOutcome::Returning(val) = values else {
            panic!("expected returning");
        };
        assert_eq!(sel.result.columns, val.result.columns);
        assert_eq!(sel.rows_affected, 2);
        assert_eq!(sel.result.rows.len(), val.result.rows.len());
        // 疑似列 id の値だけが異なる（挿入元の id と VALUES の id）。ほかのセルは一致する。
        assert_eq!(sel.result.rows[0].cells[1..], val.result.rows[0].cells[1..]);
    }

    // 対象ビヘイビア: SQL-51・SQL-21（ポインタ）。0 行でも RETURNING の列だけを返す。
    #[test]
    fn zero_rows_with_returning_returns_columns_only() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        let stmt = crate::sql::allowlist::validate_insert(
            "INSERT INTO dst (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') \
             RETURNING * USING OPERATION_ID 'op-shape'",
            &core.storage,
            core.ledger_mode,
        )
        .expect("validate");
        let projection = stmt.returning.clone().expect("returning");
        let cols = vec![
            "id".to_string(),
            "embedding".to_string(),
            "lang".to_string(),
        ];
        let mut s = SessionState::default();
        let out = core
            .execute_insert_select(
                InsertSelectTxn::Autocommit,
                &a,
                &mut s,
                InsertSelectRequest {
                    table_name: "dst",
                    columns: Some(&cols),
                    select: select_stmt(&core, "SELECT id, embedding, lang FROM src LIMIT 100"),
                    operation_id: Some(OperationId::parse("op-zero").expect("op")),
                    returning: Some(&projection),
                    has_on_conflict: false,
                },
            )
            .expect("insert select");
        let SqlOutcome::Returning(r) = out else {
            panic!("expected returning");
        };
        assert_eq!(r.rows_affected, 0);
        assert!(r.result.rows.is_empty());
        assert!(!r.result.columns.is_empty());
    }

    // 対象ビヘイビア: SQL-51・SQL-49（ポインタ）。ON CONFLICT との併用は `0A000`。
    #[test]
    fn on_conflict_is_rejected_fail_closed() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        let mut s = SessionState::default();
        let err = core
            .execute_insert_select(
                InsertSelectTxn::Autocommit,
                &a,
                &mut s,
                InsertSelectRequest {
                    table_name: "dst",
                    columns: None,
                    select: select_stmt(&core, "SELECT id, embedding, lang FROM src LIMIT 1"),
                    operation_id: None,
                    returning: None,
                    has_on_conflict: true,
                },
            )
            .unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    fn ddl(core: &EngineCore, c: &PolicyContext, sql: &str) {
        let mut s = SessionState::default();
        s.allow_ddl();
        core.execute_sql_in_session(c, &mut s, sql).expect("ddl");
    }

    fn insert_select_into(
        core: &EngineCore,
        c: &PolicyContext,
        mode: InsertSelectTxn<'_>,
        table: &str,
        cols: &[&str],
        select: &str,
        op: &str,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let cols: Vec<String> = cols.iter().map(|c| c.to_string()).collect();
        let mut s = SessionState::default();
        core.execute_insert_select(
            mode,
            c,
            &mut s,
            InsertSelectRequest {
                table_name: table,
                columns: Some(&cols),
                select: select_stmt(core, select),
                operation_id: Some(OperationId::parse(op).expect("op id")),
                returning: None,
                has_on_conflict: false,
            },
        )
    }

    // 対象ビヘイビア: SQL-51（ポインタ）。自己参照でも挿入行を同じ文が読まない（行数がちょうど 2 倍）。
    #[test]
    fn self_reference_doubles_row_count_exactly() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        ddl(
            &core,
            &a,
            "CREATE TABLE selfref (n INTEGER NOT NULL, lang TEXT)",
        );
        run(
            &core,
            &a,
            "INSERT INTO selfref (n, lang) VALUES (1, 'a'), (2, 'b'), (3, 'c') \
             USING OPERATION_ID 'op-seed-self'",
        )
        .expect("seed");
        insert_select_into(
            &core,
            &a,
            InsertSelectTxn::Autocommit,
            "selfref",
            &["n", "lang"],
            "SELECT n, lang FROM selfref LIMIT 100",
            "op-self",
        )
        .expect("self insert select");
        assert_eq!(count(&core, &a, "selfref"), 6);
    }

    // 対象ビヘイビア: SQL-51（ポインタ）。代入変換（整数の拡大・NUMERIC 再検証）と、
    // 変換失敗（VARCHAR 超過 `22001`・NOT NULL `23502`）で何も残らないこと。
    #[test]
    fn assignment_conversion_and_failures_leave_nothing() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        ddl(&core, &a, "CREATE TABLE s1 (n INTEGER, t TEXT)");
        ddl(
            &core,
            &a,
            "CREATE TABLE d1 (b BIGINT NOT NULL, m NUMERIC(10,2), v VARCHAR(3))",
        );
        run(
            &core,
            &a,
            "INSERT INTO s1 (n, t) VALUES (1, 'abc'), (2, 'abcd') USING OPERATION_ID 'op-s1'",
        )
        .expect("seed");

        let err = insert_select_into(
            &core,
            &a,
            InsertSelectTxn::Autocommit,
            "d1",
            &["b", "m", "v"],
            "SELECT n, n, t FROM s1 LIMIT 100",
            "op-conv-bad",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22001");
        assert_eq!(count(&core, &a, "d1"), 0);

        let out = insert_select_into(
            &core,
            &a,
            InsertSelectTxn::Autocommit,
            "d1",
            &["b", "m", "v"],
            "SELECT n, n, t FROM s1 WHERE n = 1 LIMIT 100",
            "op-conv-ok",
        )
        .expect("convert");
        assert!(matches!(
            out,
            SqlOutcome::Insert(InsertOutcome {
                rows_affected: 1,
                ..
            })
        ));
        assert_eq!(count(&core, &a, "d1"), 1);

        // NOT NULL 列（省略列 b）への NULL。
        let err = insert_select_into(
            &core,
            &a,
            InsertSelectTxn::Autocommit,
            "d1",
            &["v"],
            "SELECT t FROM s1 WHERE n = 1 LIMIT 100",
            "op-conv-null",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "23502");
        assert_eq!(count(&core, &a, "d1"), 1);
    }

    // 対象ビヘイビア: SQL-51・SQL-31（ポインタ）。明示トランザクションでは自分の未 commit の行を
    // 読み取り源にも使え、破棄（ROLLBACK 相当）で何も残らず、commit で残る。
    #[test]
    fn in_txn_sees_own_writes_and_rolls_back() {
        let dir = tempdir();
        let core = open(dir.path());
        let a = ctx("tenant-a");
        ddl(&core, &a, "CREATE TABLE s2 (n INTEGER)");
        ddl(&core, &a, "CREATE TABLE d2 (n INTEGER)");
        run(
            &core,
            &a,
            "INSERT INTO s2 (n) VALUES (1), (2) USING OPERATION_ID 'op-s2'",
        )
        .expect("seed");

        {
            let txn = core.storage.begin_write_txn().expect("write txn");
            insert_select_into(
                &core,
                &a,
                InsertSelectTxn::InTxn(&txn),
                "d2",
                &["n"],
                "SELECT n FROM s2 LIMIT 100",
                "op-txn-1",
            )
            .expect("first");
            // 自トランザクションの未 commit 行を内側 SELECT が読む（d2 -> d2）。
            let out = insert_select_into(
                &core,
                &a,
                InsertSelectTxn::InTxn(&txn),
                "d2",
                &["n"],
                "SELECT n FROM d2 LIMIT 100",
                "op-txn-2",
            )
            .expect("second reads own rows");
            assert!(matches!(
                out,
                SqlOutcome::Insert(InsertOutcome {
                    rows_affected: 2,
                    ..
                })
            ));
            // 破棄（ROLLBACK 相当）。
        }
        assert_eq!(count(&core, &a, "d2"), 0);

        let txn = core.storage.begin_write_txn().expect("write txn");
        insert_select_into(
            &core,
            &a,
            InsertSelectTxn::InTxn(&txn),
            "d2",
            &["n"],
            "SELECT n FROM s2 LIMIT 100",
            "op-txn-3",
        )
        .expect("insert");
        crate::recovery::commit_boundary::commit(txn).expect("commit");
        assert_eq!(count(&core, &a, "d2"), 2);
    }
}
