//! `nextval` と `currval`（TABLE-24・SQL-23、Issue #1690）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-24 (1)(2)(3)(4)(5)、
//! `docs/spec/04-behavior/error-format.md` ERR-6、`docs/spec/04-behavior/recovery.md`
//! RECOVER-10・RECOVER-11。設計記録は `docs/design/sequence-namespace-and-ledger.md`。
//!
//! 実 `Storage` ＋ `CpuScalarProvider`、`EngineCore::execute_sql_in_session`／
//! `execute_sql_in_txn` を production 経路として使う。検証する契約:
//!
//! - INSERT の VALUES 要素と列 DEFAULT で払い出せる。複数行 VALUES は行の順に連番になる
//! - `RETURNING` が払い出した値を返す（TABLE-24 (5)）
//! - `currval` はセッション内の直近値。未呼び出しは `55000`、同一行の評価順は列順
//! - NO CYCLE の上限・下限は `2200H`、CYCLE は折り返す
//! - ROLLBACK は値を戻さない（欠番）が `currval` は保持される（TABLE-24 (1)(4)）
//! - テナントごとに独立して採番し、未知の名前の `42P01` はテナントによらず同一（TABLE-24 (2)）
//! - 再起動後に続きの値が出る。同じ `operation_id` の再送は値を消費しない（TABLE-24 (3)）
//! - 未対応の位置（SELECT・WHERE・UPDATE SET・UPSERT・COPY 等）は `0A000`

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

/// DB・コア・DDL 権限つきセッションを用意し、`SEQUENCE s`（既定）と表 `t (n BIGINT)` を作る。
fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore, SessionState) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    run(&core, "sys", &mut s, "CREATE SEQUENCE s").expect("create sequence");
    run(&core, "sys", &mut s, "CREATE TABLE t (n BIGINT)").expect("create table");
    (path, guard, core, s)
}

fn run(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session_legacy(&ctx(tenant), session, sql)
}

fn ok(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> SqlOutcome {
    run(core, tenant, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, tenant, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// `n` 列を id 昇順で返す。
fn column_n(core: &EngineCore, tenant: &str) -> Vec<i64> {
    let r = core
        .execute_sql(&ctx(tenant), "SELECT id, n FROM t LIMIT 1000")
        .expect("select");
    let mut rows: Vec<(u64, i64)> = r
        .rows
        .iter()
        .map(|row| {
            let id = match row.cells[0] {
                Cell::Integer(v) => v,
                ref o => panic!("id cell {o:?}"),
            };
            let n = match row.cells[1] {
                Cell::SignedInteger(v) => v,
                ref o => panic!("n cell {o:?}"),
            };
            (id, n)
        })
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, n)| n).collect()
}

/// 採番の印の列は仮値 `Null` で運ばれるが、INSERT バイト量上限（INDEX-4）には固定幅で計上される
/// （codex-review P1）。BIGINT 列だけの複数行 VALUES が上限超過で `54000` に拒否され、
/// 採番（値の消費）も行の書き込みも起きない。
#[test]
fn nextval_columns_count_toward_insert_byte_limit() {
    let path = unique_db_path("table24-nextval-byte-limit");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path).with_batch_limits(engine::batch_limits::BatchLimits {
        max_batch_total_bytes: 16,
        ..engine::batch_limits::BatchLimits::default()
    });
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s");
    ok(&core, "sys", &mut s, "CREATE TABLE t (n BIGINT)");
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE TABLE d (k BIGINT DEFAULT nextval('s'), tag TEXT)",
    );

    // 3 行 x 8 バイト = 24 バイトが上限 16 を超える。
    assert_eq!(
        code(
            &core,
            "sys",
            &mut s,
            "INSERT INTO t (id, n) VALUES (1, nextval('s')), (2, nextval('s')), (3, nextval('s')) \
             USING OPERATION_ID 'op-big'"
        ),
        "54000"
    );
    // DEFAULT nextval の省略列も計上される（3 行 x (8 + 4) バイト）。
    assert_eq!(
        code(
            &core,
            "sys",
            &mut s,
            "INSERT INTO d (id, tag) VALUES (1, 'a'), (2, 'b'), (3, 'c') \
             USING OPERATION_ID 'op-dflt'"
        ),
        "54000"
    );
    assert!(column_n(&core, "sys").is_empty());
    // 上限内（1 行 8 バイト）なら成功し、拒否された文は値を消費していない。
    ok(&core, "sys", &mut s, &insert_next(1, "op-small"));
    assert_eq!(column_n(&core, "sys"), vec![1]);
}

fn insert_next(id: u64, op: &str) -> String {
    format!("INSERT INTO t (id, n) VALUES ({id}, nextval('s')) USING OPERATION_ID '{op}'")
}

#[test]
fn nextval_in_values_assigns_consecutive_values() {
    let (_p, _g, core, mut s) = fixture("seq-nv-basic");
    ok(&core, "alice", &mut s, &insert_next(1, "op1"));
    ok(&core, "alice", &mut s, &insert_next(2, "op2"));
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
}

#[test]
fn multi_row_values_get_per_row_values_in_statement_order() {
    let (_p, _g, core, mut s) = fixture("seq-nv-multi");
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (1, nextval('s')), (2, nextval('s')), (3, nextval('s')) \
         USING OPERATION_ID 'op1'",
    );
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3]);
}

#[test]
fn sequence_options_drive_the_value_series() {
    let path = unique_db_path("seq-nv-options");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE TABLE t (n BIGINT)");
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE SEQUENCE up START WITH 10 INCREMENT BY 5",
    );
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE SEQUENCE down START WITH 0 INCREMENT BY -3 MINVALUE -100 MAXVALUE 0",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (1, nextval('up')), (2, nextval('up')), (3, nextval('down')), \
         (4, nextval('down')) USING OPERATION_ID 'op1'",
    );
    assert_eq!(column_n(&core, "alice"), vec![10, 15, 0, -3]);
}

#[test]
fn default_nextval_fills_omitted_columns_per_row() {
    let path = unique_db_path("seq-nv-default");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s");
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE TABLE t (n BIGINT DEFAULT nextval('s'), tag TEXT)",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, tag) VALUES (1, 'a'), (2, 'b'), (3, 'c') USING OPERATION_ID 'op1'",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, tag) VALUES (4, 'd') USING OPERATION_ID 'op2'",
    );
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3, 4]);
    // 明示値は DEFAULT を適用せず、採番もしない。
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n, tag) VALUES (5, 100, 'e') USING OPERATION_ID 'op3'",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, tag) VALUES (6, 'f') USING OPERATION_ID 'op4'",
    );
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3, 4, 100, 5]);
}

#[test]
fn insert_returning_reports_the_assigned_values() {
    let (_p, _g, core, mut s) = fixture("seq-nv-returning");
    let out = ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (1, nextval('s')), (2, nextval('s')) \
         RETURNING n USING OPERATION_ID 'op1'",
    );
    match out {
        SqlOutcome::Returning(o) => {
            let cells: Vec<Cell> = o.result.rows.iter().map(|r| r.cells[0].clone()).collect();
            assert_eq!(cells, vec![Cell::SignedInteger(1), Cell::SignedInteger(2)]);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
}

#[test]
fn currval_is_session_local_and_55000_before_nextval() {
    let (_p, _g, core, mut s) = fixture("seq-cv-basic");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, n) VALUES (1, currval('s')) USING OPERATION_ID 'op0'"
        ),
        "55000"
    );
    ok(&core, "alice", &mut s, &insert_next(1, "op1"));
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (2, currval('s')) USING OPERATION_ID 'op2'",
    );
    assert_eq!(column_n(&core, "alice"), vec![1, 1]);
    // 別セッションは未呼び出しのまま。
    let mut other = ddl_session();
    assert_eq!(
        code(
            &core,
            "alice",
            &mut other,
            "INSERT INTO t (id, n) VALUES (3, currval('s')) USING OPERATION_ID 'op3'"
        ),
        "55000"
    );
}

#[test]
fn currval_in_the_same_row_sees_the_preceding_nextval() {
    let path = unique_db_path("seq-cv-same-row");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s");
    ok(&core, "sys", &mut s, "CREATE TABLE t (n BIGINT, m BIGINT)");
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n, m) VALUES (1, nextval('s'), currval('s')), \
         (2, nextval('s'), currval('s')) USING OPERATION_ID 'op1'",
    );
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id, n, m FROM t LIMIT 10")
        .expect("select");
    for row in &r.rows {
        assert_eq!(row.cells[1], row.cells[2]);
    }
}

#[test]
fn no_cycle_limit_is_2200h_and_stays_so() {
    let path = unique_db_path("seq-limit");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE TABLE t (n BIGINT)");
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s MAXVALUE 2");
    ok(&core, "alice", &mut s, &insert_next(1, "op1"));
    ok(&core, "alice", &mut s, &insert_next(2, "op2"));
    for (i, op) in ["op3", "op4"].iter().enumerate() {
        assert_eq!(
            code(&core, "alice", &mut s, &insert_next(3 + i as u64, op)),
            "2200H"
        );
    }
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
}

#[test]
fn cycle_wraps_in_both_directions() {
    let path = unique_db_path("seq-cycle");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE TABLE t (n BIGINT)");
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE SEQUENCE up MINVALUE 1 MAXVALUE 2 CYCLE",
    );
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE SEQUENCE down START 2 INCREMENT -1 MINVALUE 1 MAXVALUE 2 CYCLE",
    );
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (1, nextval('up')), (2, nextval('up')), (3, nextval('up')), \
         (4, nextval('down')), (5, nextval('down')), (6, nextval('down')) USING OPERATION_ID 'op1'",
    );
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 1, 2, 1, 2]);
}

#[test]
fn rollback_does_not_return_values_but_currval_survives() {
    let (_p, _g, core, _s) = fixture("seq-rollback");
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN".to_string(),
        insert_next(1, "op1"),
        "ROLLBACK".to_string(),
    ] {
        core.execute_sql_in_txn(&c, &mut session, &mut txn, &sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert!(column_n(&core, "alice").is_empty());
    // currval は ROLLBACK 後も保持される（PostgreSQL と同じ）。
    core.execute_sql_in_session_legacy(
        &c,
        &mut session,
        "INSERT INTO t (id, n) VALUES (5, currval('s')) USING OPERATION_ID 'op5'",
    )
    .expect("currval after rollback");
    // 値 1 は戻らず欠番になる（次の払い出しは 2）。
    core.execute_sql_in_session_legacy(&c, &mut session, &insert_next(2, "op2"))
        .expect("autocommit insert");
    assert_eq!(column_n(&core, "alice"), vec![2, 1]);
}

#[test]
fn commit_in_explicit_transaction_persists_values() {
    let (_p, _g, core, _s) = fixture("seq-commit");
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN".to_string(),
        insert_next(1, "op1"),
        insert_next(2, "op2"),
        "COMMIT".to_string(),
    ] {
        core.execute_sql_in_txn(&c, &mut session, &mut txn, &sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
}

#[test]
fn tenants_have_independent_series_and_unknown_name_is_uniform() {
    let (_p, _g, core, mut s) = fixture("seq-tenants");
    ok(&core, "alice", &mut s, &insert_next(1, "a1"));
    ok(&core, "alice", &mut s, &insert_next(2, "a2"));
    let mut sb = ddl_session();
    ok(&core, "bob", &mut sb, &insert_next(1, "b1"));
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
    assert_eq!(column_n(&core, "bob"), vec![1]);
    for tenant in ["alice", "bob", "carol"] {
        let mut ss = ddl_session();
        assert_eq!(
            code(
                &core,
                tenant,
                &mut ss,
                "INSERT INTO t (id, n) VALUES (9, nextval('nothere')) USING OPERATION_ID 'x9'"
            ),
            "42P01"
        );
    }
}

#[test]
fn values_continue_after_reopen() {
    let (p, _g, core, mut s) = fixture("seq-reopen");
    ok(&core, "alice", &mut s, &insert_next(1, "op1"));
    ok(&core, "alice", &mut s, &insert_next(2, "op2"));
    drop(core);
    let core = open_core(&p);
    let mut s2 = ddl_session();
    ok(&core, "alice", &mut s2, &insert_next(3, "op3"));
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3]);
}

#[test]
fn resend_with_same_operation_id_does_not_consume_a_value() {
    let (_p, _g, core, mut s) = fixture("seq-resend");
    ok(&core, "alice", &mut s, &insert_next(1, "op1"));
    // 同一文の再送は台帳照合で `23505`。値を消費しない。
    assert_eq!(
        code(&core, "alice", &mut s, &insert_next(1, "op1")),
        "23505"
    );
    ok(&core, "alice", &mut s, &insert_next(2, "op2"));
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
}

#[test]
fn default_ddl_checks_existence_type_and_dependency() {
    let path = unique_db_path("seq-default-ddl");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s");
    // 存在しない SEQUENCE は 42P01。
    assert_eq!(
        code(
            &core,
            "sys",
            &mut s,
            "CREATE TABLE a (n BIGINT DEFAULT nextval('nope'))"
        ),
        "42P01"
    );
    // 整数以外の列は型不一致。
    assert_ne!(
        code(
            &core,
            "sys",
            &mut s,
            "CREATE TABLE b (n TEXT DEFAULT nextval('s'))"
        ),
        "00000"
    );
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE TABLE c (n BIGINT DEFAULT nextval('s'))",
    );
    // 依存する DEFAULT があれば DROP SEQUENCE は 2BP01。
    assert_eq!(code(&core, "sys", &mut s, "DROP SEQUENCE s"), "2BP01");
    // DEFAULT を外せば DROP できる。
    ok(
        &core,
        "sys",
        &mut s,
        "ALTER TABLE c ALTER COLUMN n DROP DEFAULT",
    );
    ok(&core, "sys", &mut s, "DROP SEQUENCE s");
    // SET DEFAULT nextval は受理し、ADD COLUMN DEFAULT nextval は 0A000。
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s2");
    ok(
        &core,
        "sys",
        &mut s,
        "ALTER TABLE c ALTER COLUMN n SET DEFAULT nextval('s2')",
    );
    assert_eq!(
        code(
            &core,
            "sys",
            &mut s,
            "ALTER TABLE c ADD COLUMN m BIGINT DEFAULT nextval('s2')"
        ),
        "0A000"
    );
}

#[test]
fn smallint_column_out_of_range_is_22003() {
    let path = unique_db_path("seq-smallint");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE SEQUENCE big START 40000");
    ok(&core, "sys", &mut s, "CREATE TABLE t (n SMALLINT)");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, n) VALUES (1, nextval('big')) USING OPERATION_ID 'op1'"
        ),
        "22003"
    );
}

#[test]
fn unsupported_positions_are_fail_closed() {
    let (_p, _g, core, mut s) = fixture("seq-unsupported");
    ok(&core, "alice", &mut s, &insert_next(1, "op1"));
    for sql in [
        "SELECT nextval('s') FROM t LIMIT 1",
        "SELECT id FROM t WHERE n = nextval('s') LIMIT 1",
        "INSERT INTO t (id, n) VALUES (7, nextval('s')) ON CONFLICT (id) DO NOTHING \
         USING OPERATION_ID 'u2'",
        "INSERT INTO t (id, n) VALUES (8, nextval('public.s')) USING OPERATION_ID 'u3'",
    ] {
        assert_eq!(code(&core, "alice", &mut s, sql), "0A000", "{sql}");
    }
    // UPDATE の SET 右辺は構文段で拒否される（式位置の nextval は受理しない）。
    assert!(["0A000", "42601"].contains(&code(
        &core,
        "alice",
        &mut s,
        "UPDATE t SET n = nextval('s') WHERE id = 1 USING OPERATION_ID 'u1'"
    )));
    // 修飾名・合成式でない不正な名前は構文違反。
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, n) VALUES (8, nextval('bad name')) USING OPERATION_ID 'u4'"
        ),
        "42601"
    );
    // 失敗した文は値を巻き込まず、表も変わらない。
    assert_eq!(column_n(&core, "alice"), vec![1]);
}

#[test]
fn nextval_cannot_be_shadowed_by_a_udf_name() {
    let (_p, _g, core, mut s) = fixture("seq-udf");
    assert_ne!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE FUNCTION nextval(x) AS x + 1"
        ),
        "00000"
    );
}

#[test]
fn catalog_api_roundtrips_a_sequence_default() {
    let path = unique_db_path("seq-catalog-api");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s");
    drop(core);
    let storage = Storage::open(&path).expect("open");
    storage
        .create_table(&TableSchema::new(
            "api",
            vec![ColumnDef::new("n", ColumnType::BigInt, false)
                .with_default(engine::catalog::ColumnDefault::Sequence("s".to_string()))],
        ))
        .expect("create table with sequence default");
    drop(storage);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("api").expect("schema");
    assert_eq!(
        schema.columns[0].default_expr,
        Some(engine::catalog::ColumnDefault::Sequence("s".to_string()))
    );
    // 未知の SEQUENCE や非整数列は Rust API 直接でも拒否される。
    assert!(storage
        .create_table(&TableSchema::new(
            "api2",
            vec![ColumnDef::new("n", ColumnType::BigInt, false)
                .with_default(engine::catalog::ColumnDefault::Sequence("zzz".to_string()))],
        ))
        .is_err());
    assert!(storage
        .create_table(&TableSchema::new(
            "api3",
            vec![ColumnDef::new("n", ColumnType::Text, false)
                .with_default(engine::catalog::ColumnDefault::Sequence("s".to_string()))],
        ))
        .is_err());
}

/// 一時表（セッション専用の別 DB）は永続 SEQUENCE を解決できないため、`DEFAULT nextval` の作成と
/// INSERT VALUES の `nextval` はどちらも `42P01` ではなく `0A000` で明示的に拒否される。
#[test]
fn temp_table_with_nextval_is_rejected_explicitly() {
    let (_path, _g, core, mut s) = fixture("seq-temp");
    assert_eq!(
        code(
            &core,
            "sys",
            &mut s,
            "CREATE TEMP TABLE tt_def (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('s'))"
        ),
        "0A000"
    );
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, n BIGINT)",
    );
    assert_eq!(
        code(
            &core,
            "sys",
            &mut s,
            "INSERT INTO tt (id, n) VALUES (1, nextval('s'))"
        ),
        "0A000"
    );
    // 永続 SEQUENCE の状態は消費されない（拒否は採番前）。
    ok(
        &core,
        "sys",
        &mut s,
        "INSERT INTO t (id, n) VALUES (1, nextval('s'))",
    );
    assert_eq!(column_n(&core, "sys"), vec![1]);
}
