//! データ変更 CTE の書き込み経路での実行の結合テスト（SQL-63、Issue #1876）。ポインタ:
//! `docs/spec/04-behavior/sql-surface.md` SQL-63 (1)〜(6)・RECOVER-1・RECOVER-11・
//! `docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (b)。
//!
//! 最上位の文として送られた `WITH d AS (INSERT|UPDATE|DELETE ... RETURNING ...) SELECT ... FROM d` が
//! 1 つの書き込みトランザクションで実行され、副文の `RETURNING` 行を主クエリが読めること、
//! 文末の `USING OPERATION_ID` が文単位で台帳に記録されること、未対応の形は書き込みトランザクションを
//! 開く前に固定 `0A000` で拒否され表が一切変わらないことを production 経路
//! （`EngineCore::execute_sql_in_session`／`execute_sql_in_txn`）で固定する。文開始時点の可視性・
//! 同一行の多重変更・複数のデータ変更 CTE は `sql63_data_modifying_cte_semantics.rs`（#1877）、
//! RLS・権限の境界テストは `rls10_data_modifying_cte_paths.rs`（#1878）が担当する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation id")
}

fn seed(storage: &Storage, tenant: &str, id: u64, lang: &str) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        Visibility::Private,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(format!("{tenant} {id}")),
        ],
        &op(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn open_with(
    name: &str,
    configure: impl FnOnce(EngineCore) -> EngineCore,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, "alice", 1, "ja");
    seed(&storage, "alice", 2, "en");
    seed(&storage, "bob", 3, "ja");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (configure(core), guard)
}

fn open_seeded() -> (EngineCore, CleanupGuard) {
    open_with("sql63-dm-cte-exec", |c| c)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn query_rows(outcome: SqlOutcome) -> Vec<Vec<String>> {
    match outcome {
        SqlOutcome::Query(result) => result
            .rows
            .iter()
            .map(|r| r.cells.iter().map(|c| format!("{c:?}")).collect())
            .collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn ok_rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    query_rows(run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e:?}")))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

/// テナントの可視行の（id, lang）を昇順で返す（書き込みの観測用）。
fn state(core: &EngineCore, tenant: &str) -> Vec<Vec<String>> {
    let mut rows = ok_rows(core, tenant, "SELECT id, lang FROM docs LIMIT 100");
    rows.sort();
    rows
}

#[test]
fn delete_returning_rows_are_read_by_the_main_query() {
    let (core, _g) = open_seeded();
    let rows = ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id, lang) \
         SELECT lang FROM d LIMIT 10",
    );
    assert_eq!(rows.len(), 1);
    assert!(rows[0][0].contains("ja"), "{rows:?}");
    // 書き込みも同じ文で成立している。他テナントの行は無傷。
    assert_eq!(state(&core, "alice").len(), 1);
    assert_eq!(state(&core, "bob").len(), 1);
}

#[test]
fn update_returning_with_outer_where_order_limit() {
    let (core, _g) = open_seeded();
    let rows = ok_rows(
        &core,
        "alice",
        "WITH d AS (UPDATE docs SET lang = 'xx' WHERE id > 0 RETURNING id, lang) \
         SELECT id FROM d WHERE id >= 1 ORDER BY id LIMIT 1",
    );
    assert_eq!(rows.len(), 1);
    // 副文は 2 行を更新している（外側の LIMIT は主クエリの結果だけを絞る）。
    let after = state(&core, "alice");
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|r| r[1].contains("xx")), "{after:?}");
    assert!(state(&core, "bob")[0][1].contains("ja"));
}

#[test]
fn insert_returning_with_column_list_rename_and_aggregate() {
    let (core, _g) = open_seeded();
    let rows = ok_rows(
        &core,
        "alice",
        "WITH d(x) AS (INSERT INTO docs (id, embedding, lang, body) \
         VALUES (10, '[1,2]', 'zz', 'b') RETURNING id) SELECT x FROM d LIMIT 10",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(state(&core, "alice").len(), 3);
    let counted = ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE lang = 'zz' RETURNING id) SELECT count(*) FROM d",
    );
    assert_eq!(counted.len(), 1);
    assert!(counted[0][0].contains('1'), "{counted:?}");
    assert_eq!(state(&core, "alice").len(), 2);
}

#[test]
fn writes_only_affect_the_session_tenant() {
    let (core, _g) = open_seeded();
    let bob_before = state(&core, "bob");
    let rows = ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE id > 0 RETURNING id) SELECT id FROM d LIMIT 100",
    );
    assert_eq!(rows.len(), 2);
    assert!(state(&core, "alice").is_empty());
    assert_eq!(state(&core, "bob"), bob_before);
}

#[test]
fn unsupported_shapes_stop_with_0a000_and_change_nothing() {
    let (core, _g) = open_seeded();
    let before = (state(&core, "alice"), state(&core, "bob"));
    for sql in [
        // RETURNING なしの副文を参照
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT * FROM d LIMIT 10",
        // JOIN の主クエリでデータ変更 CTE を含む形
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
         SELECT d.id FROM d JOIN docs ON d.id = docs.id LIMIT 10",
        // ON CONFLICT 付き INSERT 副文が他の副文と同じ表を対象にする形（#1877）
        "WITH a AS (INSERT INTO docs (id, embedding, lang, body) \
         VALUES (1, '[1,2]', 'zz', 'b') ON CONFLICT (id) DO NOTHING RETURNING id), \
         b AS (DELETE FROM docs WHERE lang = 'en' RETURNING id) SELECT id FROM b LIMIT 10",
    ] {
        for tenant in ["alice", "bob"] {
            assert_eq!(
                code(&core, tenant, sql),
                "0A000",
                "tenant={tenant} sql={sql}"
            );
        }
    }
    assert_eq!((state(&core, "alice"), state(&core, "bob")), before);
}

#[test]
fn non_top_level_contexts_never_write() {
    let (core, _g) = open_seeded();
    let before = (state(&core, "alice"), state(&core, "bob"));
    let body =
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 10";
    for sql in [
        format!("DECLARE c CURSOR FOR {body}"),
        format!("COPY ({body}) TO STDOUT"),
        format!("CREATE VIEW v AS {body}"),
        format!("CREATE TABLE t2 AS {body}"),
        format!("EXPLAIN {body}"),
        format!("INSERT INTO docs (id, embedding, lang, body) {body}"),
    ] {
        let c = code(&core, "alice", &sql);
        assert!(
            ["0A000", "42601"].contains(&c.as_str()),
            "code={c} sql={sql}"
        );
    }
    assert_eq!((state(&core, "alice"), state(&core, "bob")), before);
}

#[test]
fn failure_after_the_write_leaves_the_table_unchanged() {
    // 副文は 2 行を削除するが、主クエリの結果行が上限（1 行）を超えて `54000` になる。
    let (core, _g) = open_with("sql63-dm-cte-cap", |c| {
        c.with_result_limits(ResultLimits::new(1))
    });
    let count = |core: &EngineCore| ok_rows(core, "alice", "SELECT count(*) FROM docs");
    let before = count(&core);
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH d AS (DELETE FROM docs WHERE id > 0 RETURNING id) SELECT id FROM d"
        ),
        "54000"
    );
    assert_eq!(count(&core), before, "副文の書き込みが残ってはならない");
}

#[test]
fn constraint_violation_in_the_substatement_rejects_the_whole_statement() {
    let (core, _g) = open_seeded();
    // 既存の id=1 と衝突する INSERT。
    let before = state(&core, "alice");
    let c = code(
        &core,
        "alice",
        "WITH d AS (INSERT INTO docs (id, embedding, lang, body) \
         VALUES (1, '[1,2]', 'zz', 'b') RETURNING id) SELECT id FROM d LIMIT 10",
    );
    assert_eq!(c, "23505");
    assert_eq!(state(&core, "alice"), before);
}

#[test]
fn operation_id_records_one_ledger_entry_and_resend_is_rejected() {
    let (core, _g) = open_seeded();
    let sql = "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
               SELECT id FROM d LIMIT 10 USING OPERATION_ID 'dm-1'";
    assert_eq!(ok_rows(&core, "alice", sql).len(), 1);
    assert!(matches!(
        core.operation_recorded(&ctx("alice"), TABLE, &op("dm-1"))
            .expect("lookup"),
        LedgerLookup::Recorded
    ));
    // 同一文の再送は再実行せず 23505。再送の間に別の行が増えても削除されない。
    seed_after(&core, "alice", 20, "ja");
    let before = state(&core, "alice");
    assert_eq!(code(&core, "alice", sql), "23505");
    assert_eq!(state(&core, "alice"), before);
    // 同じ operation_id で別の文は 22023。
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH d AS (DELETE FROM docs WHERE lang = 'en' RETURNING id) \
             SELECT id FROM d LIMIT 10 USING OPERATION_ID 'dm-1'"
        ),
        "22023"
    );
    assert_eq!(state(&core, "alice"), before);
}

fn seed_after(core: &EngineCore, tenant: &str, id: u64, lang: &str) {
    let sql = format!(
        "INSERT INTO docs (id, embedding, lang, body) VALUES ({id}, '[1,2]', '{lang}', 'b') \
         USING OPERATION_ID 'seed-after-{id}'"
    );
    run(core, tenant, &sql).expect("seed insert");
}

#[test]
fn omitted_operation_id_is_not_recorded() {
    let (core, _g) = open_seeded();
    ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 10",
    );
    // 句なしは台帳に記録せず、explicit NULL も同じ扱い。
    ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE lang = 'en' RETURNING id) \
         SELECT id FROM d LIMIT 10 USING OPERATION_ID NULL",
    );
}

#[test]
fn operation_id_inside_a_substatement_is_still_rejected() {
    let (core, _g) = open_seeded();
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id USING OPERATION_ID 'x') \
             SELECT id FROM d LIMIT 10"
        ),
        "42601"
    );
}

#[test]
fn ledgered_mode_requires_the_statement_level_operation_id() {
    let (core, _g) = open_with("sql63-dm-cte-ledgered", |c| {
        c.with_ledger_mode(LedgerMode::Ledgered)
    });
    let before = state(&core, "alice");
    let bare = "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
                SELECT id FROM d LIMIT 10";
    assert_eq!(code(&core, "alice", bare), "23502");
    assert_eq!(
        code(&core, "alice", &format!("{bare} USING OPERATION_ID NULL")),
        "23502"
    );
    assert_eq!(state(&core, "alice"), before);
    // 文末で指定すれば、副文が `operation_id` を持たなくても成立する。
    assert_eq!(
        ok_rows(
            &core,
            "alice",
            &format!("{bare} USING OPERATION_ID 'dm-l1'")
        )
        .len(),
        1
    );
}

#[test]
fn compare_only_without_ledger_never_touches_the_ledger() {
    let (core, _g) = open_with("sql63-dm-cte-nolegder", |c| {
        c.with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
    });
    ok_rows(
        &core,
        "alice",
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
         SELECT id FROM d LIMIT 10 USING OPERATION_ID 'dm-n1'",
    );
    assert!(matches!(
        core.operation_recorded(&ctx("alice"), TABLE, &op("dm-n1"))
            .expect("lookup"),
        LedgerLookup::NoLedger
    ));
}

struct Tx<'e> {
    core: &'e EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Tx<'e> {
    fn new(core: &'e EngineCore, tenant: &str) -> Self {
        Self {
            core,
            ctx: ctx(tenant),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        }
    }

    fn run(&mut self, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
    }

    fn ok(&mut self, sql: &str) -> SqlOutcome {
        self.run(sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed in the transaction: {e:?}"))
    }
}

#[test]
fn explicit_transaction_sees_the_write_and_rollback_discards_it() {
    let (core, _g) = open_seeded();
    let before = state(&core, "alice");
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    let rows = query_rows(tx.ok(
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 10",
    ));
    assert_eq!(rows.len(), 1);
    let inside = query_rows(tx.ok("SELECT id, lang FROM docs LIMIT 100"));
    assert_eq!(inside.len(), 1, "own uncommitted delete is visible");
    // 他セッションからは未 commit の変更は見えない。
    assert_eq!(state(&core, "alice"), before);
    tx.ok("ROLLBACK");
    assert_eq!(state(&core, "alice"), before);
}

#[test]
fn explicit_transaction_commit_persists_and_operation_id_reuse_is_rejected() {
    let (core, _g) = open_seeded();
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
         SELECT id FROM d LIMIT 10 USING OPERATION_ID 'dm-tx'",
    );
    // 同じ operation_id を同一トランザクション内で別の書き込みへ使い回すと拒否され、中断状態になる。
    let err = tx
        .run(
            "WITH d AS (DELETE FROM docs WHERE lang = 'en' RETURNING id) \
             SELECT id FROM d LIMIT 10 USING OPERATION_ID 'dm-tx'",
        )
        .expect_err("reuse in the same transaction");
    assert!(!err.wire_code().is_empty());
    assert_eq!(tx.txn.status(), TransactionStatus::Failed);
    tx.ok("ROLLBACK");
    assert_eq!(
        state(&core, "alice").len(),
        2,
        "rollback discards everything"
    );

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
         SELECT id FROM d LIMIT 10 USING OPERATION_ID 'dm-tx2'",
    );
    tx.ok("COMMIT");
    assert_eq!(state(&core, "alice").len(), 1);
    assert!(matches!(
        core.operation_recorded(&ctx("alice"), TABLE, &op("dm-tx2"))
            .expect("lookup"),
        LedgerLookup::Recorded
    ));
}

#[test]
fn describe_matches_execute_columns_and_writes_nothing() {
    let (core, _g) = open_seeded();
    let before = state(&core, "alice");
    let sql = "WITH d(x) AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id, lang) \
               SELECT lang, x FROM d LIMIT 10";
    let session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse must not write");
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(state(&core, "alice"), before, "describe must not write");
    let outcome = run(&core, "alice", sql).expect("execute");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query");
    };
    assert_eq!(described.len(), result.columns.len());
    assert_eq!(format!("{described:?}"), format!("{:?}", result.columns));
}

#[test]
fn temp_table_targets_are_rejected_without_writing() {
    let (core, _g) = open_seeded();
    let mut session = SessionState::default();
    let c = ctx("alice");
    core.execute_sql_in_session(&c, &mut session, "CREATE TEMP TABLE tt (n BIGINT)")
        .expect("create temp table");
    core.execute_sql_in_session(&c, &mut session, "INSERT INTO tt (n) VALUES (1)")
        .expect("seed temp row");
    let err = core
        .execute_sql_in_session(
            &c,
            &mut session,
            "WITH d AS (DELETE FROM tt RETURNING n) SELECT n FROM d LIMIT 10",
        )
        .expect_err("temp targets are not supported");
    assert_eq!(err.wire_code(), "0A000");
    let outcome = core
        .execute_sql_in_session(&c, &mut session, "SELECT n FROM tt LIMIT 10")
        .expect("temp row survives");
    assert_eq!(query_rows(outcome).len(), 1);
}
