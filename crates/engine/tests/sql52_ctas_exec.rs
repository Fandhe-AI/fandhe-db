//! `CREATE TABLE ... AS`（CTAS）の SQL 経路 e2e（SQL-52 (1)〜(5)・Issue #1678）。
//!
//! 構文の受理、表の作成と行の投入が 1 つの write txn で原子的に行われること（途中で失敗しても表が
//! 残らない）、`WITH [NO] DATA`・`IF NOT EXISTS`、RLS（他テナントの行が投入されない）、DDL 権限ゲートが
//! 元表の存在確認より先に働くこと（存在オラクルの防止）、未対応形の fail-closed を固定する。
//! `sql51_insert_select_exec.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`）。
//! `operation_id` の台帳・冪等な再送・入力側上限の境界テストは #1679 の担当。
//! ポインタ: `docs/design/ctas-column-inference.md`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::result_limits::ResultLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup_with(
    label: &str,
    tune: impl FnOnce(EngineCore) -> EngineCore,
) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE src (id BIGINT PRIMARY KEY, a INTEGER, t TEXT)",
        "CREATE TABLE arr (id BIGINT PRIMARY KEY, tags INTEGER[])",
    ] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    // alice の行 2 件・bob の行 1 件（同じ表。RLS で互いに不可視）
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, a, t) VALUES (1, 10, 'x'), (2, 20, 'y') USING OPERATION_ID 'seed-a'",
    )
    .expect("seed alice");
    core.execute_insert_sql(
        &ctx("bob"),
        "INSERT INTO src (id, a, t) VALUES (3, 30, 'z') USING OPERATION_ID 'seed-b'",
    )
    .expect("seed bob");
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO arr (id, tags) VALUES (1, '{1,2}') USING OPERATION_ID 'seed-arr'",
    )
    .expect("seed arr");
    // 上限・台帳モードの調整は種データの投入後に適用する（種データ自体が上限に掛からないように）。
    (tune(core), path)
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf) {
    setup_with(label, |c| c)
}

/// DDL 権限つきセッションで実行する。
fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn code(r: Result<SqlOutcome, SqlSurfaceError>) -> String {
    match r {
        Ok(v) => panic!("expected error, got {v:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn ctas(core: &EngineCore, tenant: &str, sql: &str) -> engine::sql::ddl::CreateTableAsOutcome {
    match run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::CreateTableAs(o) => o,
        other => panic!("expected CreateTableAs, got {other:?}"),
    }
}

/// テナントから見える行数。表が無ければ `42P01` で失敗する（表の存在確認にも使う）。
fn count(core: &EngineCore, tenant: &str, table: &str) -> Result<u64, String> {
    match run(core, tenant, &format!("SELECT COUNT(*) FROM {table}")) {
        Ok(SqlOutcome::Query(r)) => match r.rows[0].cells[0] {
            engine::sql::exec::Cell::Integer(v) => Ok(v),
            ref o => panic!("unexpected {o:?}"),
        },
        Ok(other) => panic!("expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

// 対象ビヘイビア: SQL-52 (1)(2)。作成と投入が通り、投入した行を SELECT で読める。件数は自分の文脈だけ。
#[test]
fn creates_table_and_copies_rows_with_own_tenant_count() {
    let (core, path) = setup("sql52-basic");
    let _g = CleanupGuard(path);
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE d AS SELECT id, a, t FROM src LIMIT 100",
    );
    assert!(o.populated() && !o.skipped());
    assert_eq!(o.rows(), Some(2));
    assert_eq!(count(&core, "alice", "d"), Ok(2));
    // 作った表は共有されるが、行は alice の Private 行のため bob からは 0 件に見える（RLS）。
    assert_eq!(count(&core, "bob", "d"), Ok(0));
    // bob の CTAS には bob 自身の行（1 件）だけが入り、alice の行は材料にならない。
    let ob = ctas(
        &core,
        "bob",
        "CREATE TABLE d_bob AS SELECT id, a, t FROM src LIMIT 100",
    );
    assert_eq!(ob.rows(), Some(1));
    assert_eq!(count(&core, "bob", "d_bob"), Ok(1));
    assert_eq!(count(&core, "alice", "d_bob"), Ok(0));
}

// 対象ビヘイビア: SQL-52 (1)。列名リストで出力列名を上書きでき、型は引き継がれる。
#[test]
fn column_name_list_overrides_output_names() {
    let (core, path) = setup("sql52-colnames");
    let _g = CleanupGuard(path);
    ctas(
        &core,
        "alice",
        "CREATE TABLE d2 (x, y) AS SELECT a, t FROM src LIMIT 100",
    );
    match run(&core, "alice", "SELECT x, y FROM d2 LIMIT 10").expect("select") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
    // 元の列名では読めない。
    assert_eq!(
        code(run(&core, "alice", "SELECT a FROM d2 LIMIT 10")),
        "42703"
    );
}

// 対象ビヘイビア: SQL-52 (3)。`WITH NO DATA` は空の表を作り、内側の問い合わせは評価しない。
#[test]
fn with_no_data_creates_empty_table() {
    let (core, path) = setup("sql52-nodata");
    let _g = CleanupGuard(path);
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE d3 AS SELECT id, a FROM src LIMIT 100 WITH NO DATA",
    );
    assert!(!o.populated() && !o.skipped());
    assert_eq!(o.rows(), None);
    assert_eq!(count(&core, "alice", "d3"), Ok(0));
    // 元表が無い問い合わせは `WITH NO DATA` でも 42P01（列の導出に元表が要る）。表は残らない。
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE d4 AS SELECT id FROM missing_src LIMIT 1 WITH NO DATA"
        )),
        "42P01"
    );
    assert_eq!(count(&core, "alice", "d4"), Err("42P01".to_string()));
}

// 対象ビヘイビア: SQL-52 (3)。`IF NOT EXISTS` は既存名で no-op（元表が無くても成功）、無指定は 42P07。
#[test]
fn if_not_exists_is_noop_and_plain_duplicate_is_rejected() {
    let (core, path) = setup("sql52-ine");
    let _g = CleanupGuard(path);
    ctas(
        &core,
        "alice",
        "CREATE TABLE d5 AS SELECT id FROM src LIMIT 100",
    );
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE IF NOT EXISTS d5 AS SELECT id FROM missing_src LIMIT 1",
    );
    assert!(o.skipped() && !o.populated());
    // 既存の表の中身は変わらない。
    assert_eq!(count(&core, "alice", "d5"), Ok(2));
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE d5 AS SELECT id FROM src LIMIT 100"
        )),
        "42P07"
    );
    // 既存の元表と同名でも同じ。
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE src AS SELECT id FROM src LIMIT 100"
        )),
        "42P07"
    );
}

// 対象ビヘイビア: SQL-52 (2)。途中で失敗したら表が残らない（行数超過・未対応の列型・元表の不在）。
#[test]
fn failure_leaves_no_table_behind() {
    // `--max-result-rows` 相当を 1 にすると、通常の SELECT と同じく LIMIT 2 は範囲外（22000）。
    // 列定義の導出と実行が同じ設定を参照するため、拒否されても表は残らない。
    let (core, path) = setup_with("sql52-atomic", |c| {
        c.with_result_limits(ResultLimits::new(1))
    });
    let _g = CleanupGuard(path);
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE f1 AS SELECT id FROM src LIMIT 2"
        )),
        "22000"
    );
    assert_eq!(count(&core, "alice", "f1"), Err("42P01".to_string()));

    // 配列列は評価の前に 0A000。表は残らない。
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE f2 AS SELECT id, tags FROM arr LIMIT 1"
        )),
        "0A000"
    );
    assert_eq!(count(&core, "alice", "f2"), Err("42P01".to_string()));

    // 元表の不在は 42P01。
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE f3 AS SELECT id FROM nope LIMIT 1"
        )),
        "42P01"
    );
    assert_eq!(count(&core, "alice", "f3"), Err("42P01".to_string()));

    // 行数が上限内（1 行）なら通る。失敗後も書き込みゲートが解放されている。
    let o = ctas(
        &core,
        "bob",
        "CREATE TABLE f4 AS SELECT id FROM src LIMIT 1",
    );
    assert_eq!(o.rows(), Some(1));
}

// 対象ビヘイビア: SQL-52 (2)。1 回の投入件数の上限（`--max-insert-rows` 相当）を超えたら表は残らない。
#[test]
fn insert_row_limit_failure_leaves_no_table_behind() {
    let (core, path) = setup_with("sql52-insert-cap", |c| {
        c.with_dml_limits(engine::sql::parser::DmlLimits {
            max_affected_rows: None,
            max_insert_rows_per_statement: std::num::NonZeroUsize::new(1),
        })
    });
    let _g = CleanupGuard(path);
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE g1 AS SELECT id FROM src LIMIT 100"
        )),
        "54000"
    );
    assert_eq!(count(&core, "alice", "g1"), Err("42P01".to_string()));
}

// 対象ビヘイビア: SQL-52。起動時設定で引き上げた LIMIT 範囲は列定義の導出でも効く（既定 10,000 超）。
#[test]
fn raised_result_limit_applies_to_column_derivation() {
    let (core, path) = setup_with("sql52-raised-limit", |c| {
        c.with_result_limits(ResultLimits::new(20_000))
    });
    let _g = CleanupGuard(path);
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE big AS SELECT a FROM src LIMIT 15000",
    );
    assert!(!o.skipped());
    assert_eq!(count(&core, "alice", "big"), Ok(2));
}

// 対象ビヘイビア: SQL-52 (1)。0 行の問い合わせは空の表を作り、行数は 0。
#[test]
fn zero_rows_creates_empty_table() {
    let (core, path) = setup("sql52-zero");
    let _g = CleanupGuard(path);
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE z AS SELECT id, a FROM src WHERE a > 1000 LIMIT 10",
    );
    assert!(o.populated());
    assert_eq!(o.rows(), Some(0));
    assert_eq!(count(&core, "alice", "z"), Ok(0));
}

// 対象ビヘイビア: SQL-52 (1)。`public.` 修飾の作成先・参照先を解決できる。
#[test]
fn public_qualified_names_are_resolved() {
    let (core, path) = setup("sql52-qualified");
    let _g = CleanupGuard(path);
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE public.q AS SELECT id FROM public.src LIMIT 100",
    );
    assert_eq!(o.rows(), Some(2));
    assert_eq!(count(&core, "alice", "public.q"), Ok(2));
}

// 対象ビヘイビア: SQL-52 (4)。DDL 権限ゲートが元表の存在確認より先に働く（存在オラクルの防止）。
#[test]
fn ddl_permission_gate_precedes_catalog_checks() {
    let (core, path) = setup("sql52-perm");
    let _g = CleanupGuard(path);
    let mut s = SessionState::default(); // DDL 権限なし
    for sql in [
        "CREATE TABLE p1 AS SELECT id FROM src LIMIT 10",
        "CREATE TABLE p2 AS SELECT id FROM no_such_table LIMIT 10",
        "CREATE TABLE IF NOT EXISTS src AS SELECT id FROM src LIMIT 10",
    ] {
        let e = core
            .execute_sql_in_session(&ctx("alice"), &mut s, sql)
            .expect_err("must be rejected");
        assert_eq!(e.wire_code(), "42501", "{sql}");
    }
    assert_eq!(count(&core, "alice", "p1"), Err("42P01".to_string()));
}

// 対象ビヘイビア: SQL-52 (5)。未対応の形は fail-closed（構文 42601／未対応 0A000）。
#[test]
fn unsupported_forms_fail_closed() {
    let (core, path) = setup("sql52-unsupported");
    let _g = CleanupGuard(path);
    for (sql, expected) in [
        ("CREATE TABLE u AS", "42601"),
        (
            "CREATE TABLE u AS EXPLAIN SELECT id FROM src LIMIT 1",
            "42601",
        ),
        ("CREATE TABLE u AS SELECT id FROM src LIMIT 1 WITH", "42601"),
        (
            "CREATE TABLE u AS SELECT id FROM src LIMIT 1 extra",
            "42601",
        ),
        (
            "CREATE TABLE u (a, b) AS SELECT id FROM src LIMIT 1",
            "42601",
        ),
        (
            "CREATE TABLE u AS SELECT id FROM src LIMIT 1 USING OPERATION_ID 'op-1'",
            "0A000",
        ),
        ("CREATE TEMP TABLE u AS SELECT id FROM src LIMIT 1", "0A000"),
        (
            "CREATE TEMP TABLE u (a) AS SELECT id FROM src LIMIT 1",
            "0A000",
        ),
    ] {
        assert_eq!(code(run(&core, "alice", sql)), expected, "{sql}");
    }
    assert_eq!(count(&core, "alice", "u"), Err("42P01".to_string()));
}

// 対象ビヘイビア: SQL-52 (5)・SQL-31。明示トランザクション内の CTAS は DDL と同じ 0A000。
#[test]
fn explicit_transaction_is_rejected() {
    let (core, path) = setup("sql52-txn");
    let _g = CleanupGuard(path);
    let mut txn = core.new_session_transaction();
    let mut ts = SessionState::default();
    ts.allow_ddl();
    let c = ctx("alice");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(
            &c,
            &mut ts,
            &mut txn,
            "CREATE TABLE tx AS SELECT id FROM src LIMIT 10",
        )
        .expect_err("must be rejected");
    assert_eq!(e.wire_code(), "0A000");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, "alice", "tx"), Err("42P01".to_string()));
}

// 対象ビヘイビア: SQL-52 (5)・TABLE-44。一時表を読む CTAS は 0A000（永続の元表と混在も同じ）。
#[test]
fn temp_table_source_is_rejected() {
    let (core, path) = setup("sql52-temp");
    let _g = CleanupGuard(path);
    let mut s = SessionState::default();
    s.allow_ddl();
    let c = ctx("alice");
    core.execute_sql_in_session(&c, &mut s, "CREATE TEMP TABLE tt (a TEXT)")
        .expect("temp table");
    let e = core
        .execute_sql_in_session(&c, &mut s, "CREATE TABLE tc AS SELECT a FROM tt LIMIT 10")
        .expect_err("must be rejected");
    assert_eq!(e.wire_code(), "0A000");
    assert_eq!(count(&core, "alice", "tc"), Err("42P01".to_string()));
}

// 対象ビヘイビア: SQL-52 (5)・TABLE-44。拡張クエリの Parse 経路でも、一時表を読む CTAS は 0A000。
#[test]
fn temp_table_source_is_rejected_in_prepared_parse() {
    let (core, path) = setup("sql52-temp-prep");
    let _g = CleanupGuard(path);
    let mut s = SessionState::default();
    s.allow_ddl();
    let c = ctx("alice");
    core.execute_sql_in_session(&c, &mut s, "CREATE TEMP TABLE tt (a TEXT)")
        .expect("temp table");
    match core.parse_sql_prepared_with_declared_in_session(
        &s,
        "CREATE TABLE tc AS SELECT a FROM tt LIMIT 10",
        &[],
    ) {
        Ok(_) => panic!("must be rejected"),
        Err(e) => assert_eq!(e.wire_code(), "0A000"),
    }
}

// 対象ビヘイビア: SQL-52・RECOVER-1。台帳必須モードの `WITH DATA` は operation_id 無しのため 23502、
// `WITH NO DATA` は行を書かないので通る（台帳は #1679 の担当）。
#[test]
fn ledgered_mode_requires_operation_id_for_data_only() {
    let (core, path) = setup_with("sql52-ledgered", |c| {
        c.with_ledger_mode(LedgerMode::Ledgered)
    });
    let _g = CleanupGuard(path);
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE l1 AS SELECT id FROM src LIMIT 10"
        )),
        "23502"
    );
    assert_eq!(count(&core, "alice", "l1"), Err("42P01".to_string()));
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE l2 AS SELECT id FROM src LIMIT 10 WITH NO DATA",
    );
    assert!(!o.populated());
}

// 拡張クエリプロトコル: `$n` を含む CTAS は Parse 段で 0A000。
#[test]
fn bind_parameters_are_rejected() {
    let (core, path) = setup("sql52-params");
    let _g = CleanupGuard(path);
    let e = core
        .parse_sql_prepared("CREATE TABLE b AS SELECT id FROM src WHERE a = $1 LIMIT 10")
        .err()
        .map(|e| e.wire_code().to_string());
    assert_eq!(e.as_deref(), Some("0A000"));
}

// 対象ビヘイビア: SQL-52 (2)(5)。非バイト順照合の列を持つ表を読む CTAS は、照合を黙って落とさないよう
// 0A000 で拒否し、表を残さない。
#[test]
fn non_byte_order_collation_source_is_rejected() {
    let (core, path) = setup("sql52-collation");
    let _g = CleanupGuard(path);
    run(
        &core,
        "alice",
        "CREATE TABLE col (id BIGINT PRIMARY KEY, t TEXT COLLATE \"und-x-icu\")",
    )
    .expect("create col");
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE col2 AS SELECT id, t FROM col LIMIT 10"
        )),
        "0A000"
    );
    assert_eq!(count(&core, "alice", "col2"), Err("42P01".to_string()));
}

// 対象ビヘイビア: SQL-52 (1)。内側の問い合わせの時刻関数・ビュー・JOIN も通る（JOIN の相手が非バイト順
// 照合の列を持つ表なら 0A000）。
#[test]
fn time_functions_views_and_joins_in_inner_query() {
    let (core, path) = setup("sql52-shapes");
    let _g = CleanupGuard(path);
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE tn AS SELECT id, now() AS ts FROM src LIMIT 10",
    );
    assert_eq!(o.rows(), Some(2));
    run(&core, "alice", "CREATE VIEW sv AS SELECT id, a FROM src").expect("create view");
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE tv AS SELECT id, a FROM sv LIMIT 10",
    );
    assert_eq!(o.rows(), Some(2));
    run(
        &core,
        "alice",
        "CREATE TABLE col (id BIGINT PRIMARY KEY, t TEXT COLLATE \"und-x-icu\")",
    )
    .expect("create col");
    assert_eq!(
        code(run(
            &core,
            "alice",
            "CREATE TABLE tj AS SELECT src.id AS id, col.t AS t FROM src \
             JOIN col ON src.id = col.id LIMIT 10"
        )),
        "0A000"
    );
    assert_eq!(count(&core, "alice", "tj"), Err("42P01".to_string()));
}

// 対象ビヘイビア: SQL-52 (1)。VECTOR・日付・真偽・浮動小数・NUMERIC を含む列型が投入まで通る
// （列型の修飾は保たれる）。
#[test]
fn column_types_round_trip_through_ctas() {
    let (core, path) = setup("sql52-types");
    let _g = CleanupGuard(path);
    run(
        &core,
        "alice",
        "CREATE TABLE ty (id BIGINT PRIMARY KEY, v VECTOR(2), d DATE, b BOOLEAN, \
         f DOUBLE PRECISION, n NUMERIC(10,2), s VARCHAR(5))",
    )
    .expect("create ty");
    run(
        &core,
        "alice",
        "INSERT INTO ty (id, v, d, b, f, n, s) VALUES \
         (1, '[1.0,2.0]', '2024-01-02', true, 1.5, 3.25, 'ab') USING OPERATION_ID 'seed-ty'",
    )
    .expect("seed ty");
    let o = ctas(
        &core,
        "alice",
        "CREATE TABLE ty2 AS SELECT id, v, d, b, f, n, s FROM ty LIMIT 10",
    );
    assert_eq!(o.rows(), Some(1));
    assert_eq!(count(&core, "alice", "ty2"), Ok(1));
}
