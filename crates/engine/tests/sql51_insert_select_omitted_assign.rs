//! `INSERT ... SELECT` の列リスト省略・省略列の DEFAULT 補完・代入変換の e2e
//! （SQL-51・Issue #2613。親: #1731。構文・束縛は #1732、実行本体は #1733）。
//!
//! 列リスト省略が表の宣言順へ展開されること、列リスト指定時の省略列に列 DEFAULT が入ること、
//! 内側 SELECT の結果列型から挿入先列型へ PostgreSQL の代入変換どおりに値が変換されること、
//! 変換できない・範囲外・NULL 違反は書き込み前に全体を拒否し行が増えないこと（fail-closed）、
//! エラー文言に行の値が載らないことを固定する。RLS・台帳・上限の既存挙動も省略形で確認する。
//! 他の `sql51_*` と同じ流儀（`unique_db_path`／`CleanupGuard`）。
//! ポインタ: `docs/design/insert-select.md`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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

fn setup(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE src (id BIGINT PRIMARY KEY, a INTEGER, t TEXT)",
        "CREATE TABLE dst (id BIGINT PRIMARY KEY, a BIGINT, t TEXT)",
        "CREATE TABLE dflt (id BIGINT PRIMARY KEY, a BIGINT, \
         t TEXT DEFAULT 'dv', n INTEGER NOT NULL DEFAULT 5, m INTEGER)",
        "CREATE TABLE nodflt (id BIGINT PRIMARY KEY, a BIGINT, c TEXT NOT NULL)",
        "CREATE SEQUENCE sq",
        "CREATE TABLE seqd (id BIGINT PRIMARY KEY, a BIGINT, s BIGINT DEFAULT nextval('sq'))",
        "CREATE TABLE cv_src (id BIGINT PRIMARY KEY, f DOUBLE PRECISION, nm NUMERIC(10,2), \
         i INTEGER, tx TEXT, ts TIMESTAMP, js JSON)",
        "CREATE TABLE cv_dst (id BIGINT PRIMARY KEY, fi INTEGER, ni INTEGER, it TEXT, \
         fn NUMERIC(10,1), jb JSONB, d DATE)",
    ] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
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
    (core, path)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn code(r: Result<SqlOutcome, SqlSurfaceError>) -> String {
    match r {
        Ok(v) => panic!("expected error, got {v:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn rows_affected(r: Result<SqlOutcome, SqlSurfaceError>) -> u64 {
    match r.expect("insert select") {
        SqlOutcome::Insert(o) => o.rows_affected,
        other => panic!("expected Insert, got {other:?}"),
    }
}

/// `SELECT` の結果を、行ごとにセルの Debug 表記を連結した文字列で返す（`id` 昇順で呼ぶこと）。
fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match run(core, tenant, sql).expect("select") {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn count(core: &EngineCore, table: &str) -> usize {
    rows(
        core,
        "alice",
        &format!("SELECT id FROM {table} ORDER BY id LIMIT 100"),
    )
    .len()
}

// 対象ビヘイビア: SQL-51（ポインタ）。列リスト省略は宣言順で、明示形と同じ行になる。
#[test]
fn omitted_column_list_inserts_in_declaration_order_like_explicit_form() {
    let (core, path) = setup("sql51-omit-order");
    let _g = CleanupGuard(path);
    assert_eq!(
        rows_affected(run(
            &core,
            "alice",
            "INSERT INTO dst SELECT id, a, t FROM src LIMIT 100 USING OPERATION_ID 'om-1'"
        )),
        2
    );
    let omitted = rows(
        &core,
        "alice",
        "SELECT id, a, t FROM dst ORDER BY id LIMIT 100",
    );
    assert_eq!(omitted.len(), 2);
    run(
        &core,
        "alice",
        "TRUNCATE TABLE dst USING OPERATION_ID 'om-trunc'",
    )
    .expect("truncate");
    run(
        &core,
        "alice",
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 100 USING OPERATION_ID 'om-2'",
    )
    .expect("explicit");
    let explicit = rows(
        &core,
        "alice",
        "SELECT id, a, t FROM dst ORDER BY id LIMIT 100",
    );
    assert_eq!(omitted, explicit);
    // 括弧付きの問い合わせも同じ
    run(
        &core,
        "alice",
        "TRUNCATE TABLE dst USING OPERATION_ID 'om-trunc2'",
    )
    .expect("truncate");
    run(
        &core,
        "alice",
        "INSERT INTO dst (SELECT id, a, t FROM src LIMIT 100) USING OPERATION_ID 'om-3'",
    )
    .expect("parenthesized");
    assert_eq!(
        rows(
            &core,
            "alice",
            "SELECT id, a, t FROM dst ORDER BY id LIMIT 100"
        ),
        explicit
    );
}

// 対象ビヘイビア: SQL-51（ポインタ）。出力列が少ない省略形は先頭の列へ当て、残りは DEFAULT 補完。
#[test]
fn omitted_column_list_with_fewer_outputs_fills_defaults() {
    let (core, path) = setup("sql51-omit-fewer");
    let _g = CleanupGuard(path);
    run(
        &core,
        "alice",
        "INSERT INTO dflt SELECT id, a FROM src LIMIT 100 USING OPERATION_ID 'fw-1'",
    )
    .expect("fewer outputs");
    let got = rows(
        &core,
        "alice",
        "SELECT id, a, t, n, m FROM dflt ORDER BY id LIMIT 100",
    );
    assert_eq!(got.len(), 2);
    for row in &got {
        assert!(row.contains("\"dv\""), "t は DEFAULT: {row}");
        assert!(row.contains('5'), "n は DEFAULT: {row}");
        assert!(
            row.ends_with("Null"),
            "m は NULL 許容で DEFAULT 無し: {row}"
        );
    }
    // 出力列が多い省略形は 42601 で、行は増えない
    let before = count(&core, "dst");
    assert_eq!(
        code(run(
            &core,
            "alice",
            "INSERT INTO dst SELECT id, a, t, id FROM src LIMIT 100 USING OPERATION_ID 'fw-2'"
        )),
        "42601"
    );
    assert_eq!(count(&core, "dst"), before);
}

// 対象ビヘイビア: SQL-51（ポインタ）。列リスト指定時の省略列は列 DEFAULT・NULL 許容は NULL。
// DEFAULT の無い NOT NULL 列は 23502 で、1 行も入らない。
#[test]
fn explicit_column_list_fills_defaults_and_rejects_not_null_without_default() {
    let (core, path) = setup("sql51-omit-defaults");
    let _g = CleanupGuard(path);
    run(
        &core,
        "alice",
        "INSERT INTO dflt (id, a) SELECT id, a FROM src LIMIT 100 USING OPERATION_ID 'df-1'",
    )
    .expect("defaults");
    let got = rows(
        &core,
        "alice",
        "SELECT t, n, m FROM dflt ORDER BY id LIMIT 100",
    );
    assert_eq!(got.len(), 2);
    for row in &got {
        assert!(row.starts_with("Text(\"dv\")"), "{row}");
        assert!(row.ends_with("Null"), "{row}");
    }
    assert_eq!(
        code(run(
            &core,
            "alice",
            "INSERT INTO nodflt (id, a) SELECT id, a FROM src LIMIT 100 \
             USING OPERATION_ID 'df-2'"
        )),
        "23502"
    );
    assert_eq!(count(&core, "nodflt"), 0);
}

// 対象ビヘイビア: SQL-51（ポインタ）。省略列の DEFAULT nextval は従来どおり 0A000（COPY と同じ）。
#[test]
fn omitted_sequence_default_stays_fail_closed() {
    let (core, path) = setup("sql51-omit-seq");
    let _g = CleanupGuard(path);
    assert_eq!(
        code(run(
            &core,
            "alice",
            "INSERT INTO seqd (id, a) SELECT id, a FROM src LIMIT 100 USING OPERATION_ID 'sq-1'"
        )),
        "0A000"
    );
    assert_eq!(count(&core, "seqd"), 0);
}

fn seed_cv(core: &EngineCore) {
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO cv_src (id, f, nm, i, tx, ts, js) VALUES \
         (1, 2.5, 12.50, 7, '42', '2024-03-05 10:20:30', '{\"k\": 1}'), \
         (2, 3.5, -2.50, 8, '43', '2024-12-31 23:59:59', '[1, 2]') \
         USING OPERATION_ID 'cv-seed'",
    )
    .expect("seed cv");
}

// 対象ビヘイビア: SQL-51（ポインタ）。代入変換は PostgreSQL と同じ値になる
// （浮動小数 → 整数は偶数丸め、numeric → 整数は 0 から遠い側へ四捨五入）。
#[test]
fn assignment_conversions_match_postgresql_values() {
    let (core, path) = setup("sql51-assign-ok");
    let _g = CleanupGuard(path);
    seed_cv(&core);
    run(
        &core,
        "alice",
        "INSERT INTO cv_dst (id, fi, ni, it, fn, jb, d) \
         SELECT id, f, nm, i, f, js, ts FROM cv_src LIMIT 100 USING OPERATION_ID 'as-1'",
    )
    .expect("assignment conversions");
    let got = rows(
        &core,
        "alice",
        "SELECT fi, ni, it FROM cv_dst ORDER BY id LIMIT 100",
    );
    assert_eq!(got.len(), 2);
    // 2.5 -> 2（偶数丸め）、3.5 -> 4。12.50 -> 13、-2.50 -> -3。7 -> '7'
    assert!(got[0].contains("2") && got[1].contains("4"), "{got:?}");
    assert!(got[0].contains("13") && got[1].contains("-3"), "{got:?}");
    assert!(
        got[0].contains("\"7\"") && got[1].contains("\"8\""),
        "{got:?}"
    );
    // float8 -> NUMERIC(10,1)、TIMESTAMP -> DATE
    let more = rows(
        &core,
        "alice",
        "SELECT fn, d FROM cv_dst ORDER BY id LIMIT 100",
    );
    assert_eq!(
        more,
        vec![
            "Numeric(Decimal { unscaled: 25, scale: 1 })|Date(19787)".to_string(),
            "Numeric(Decimal { unscaled: 35, scale: 1 })|Date(20088)".to_string(),
        ]
    );
}

// 対象ビヘイビア: SQL-51（ポインタ）。範囲外・変換不能は書き込み前に全体を拒否し、行は増えない。
#[test]
fn failing_conversions_reject_whole_statement() {
    let (core, path) = setup("sql51-assign-fail");
    let _g = CleanupGuard(path);
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO cv_src (id, f, nm, i, tx, ts, js) VALUES \
         (1, 2.5, 1.00, 1, 'a', '2024-03-05 10:20:30', '{}'), \
         (2, 3000000000.0, 1.00, 1, 'b', '2024-03-05 10:20:30', '{}') \
         USING OPERATION_ID 'cv-seed2'",
    )
    .expect("seed cv");
    // 2 行目の浮動小数が INTEGER の範囲外: 22003 で全体拒否。エラーに値は載らない。
    let err = run(
        &core,
        "alice",
        "INSERT INTO cv_dst (id, fi) SELECT id, f FROM cv_src LIMIT 100 \
         USING OPERATION_ID 'asf-1'",
    )
    .expect_err("out of range");
    assert_eq!(err.wire_code(), "22003");
    assert!(!err.to_string().contains("3000000000"), "{err}");
    assert!(!err.client_message().contains("3000000000"), "{err}");
    assert_eq!(count(&core, "cv_dst"), 0);
    // 型族をまたぐ変換不能の組は 42804（TEXT -> INTEGER、BOOLEAN -> INTEGER は静的に拒否）
    assert_eq!(
        code(run(
            &core,
            "alice",
            "INSERT INTO cv_dst (id, fi) SELECT id, tx FROM cv_src LIMIT 100 \
             USING OPERATION_ID 'asf-2'"
        )),
        "42804"
    );
    assert_eq!(count(&core, "cv_dst"), 0);
}

// 対象ビヘイビア: SQL-51（ポインタ）。省略形でも RLS・台帳・明示トランザクションの既存挙動を保つ。
#[test]
fn omitted_form_keeps_rls_ledger_and_transaction_behavior() {
    let (core, path) = setup("sql51-omit-misc");
    let _g = CleanupGuard(path);
    let sql = "INSERT INTO dst SELECT id, a, t FROM src LIMIT 100 USING OPERATION_ID 'mi-1'";
    assert_eq!(rows_affected(run(&core, "alice", sql)), 2);
    // bob の行は材料にならず、alice の挿入行は bob から見えない
    assert!(rows(&core, "bob", "SELECT id FROM dst ORDER BY id LIMIT 100").is_empty());
    // 同一 operation_id・同一文の再送は 23505、別文は 22023
    assert_eq!(code(run(&core, "alice", sql)), "23505");
    assert_eq!(
        code(run(
            &core,
            "alice",
            "INSERT INTO dst SELECT id, a, t FROM src WHERE a = 10 LIMIT 100 \
             USING OPERATION_ID 'mi-1'"
        )),
        "22023"
    );
    assert_eq!(count(&core, "dst"), 2);

    // 明示トランザクションの ROLLBACK／COMMIT
    run(
        &core,
        "alice",
        "TRUNCATE TABLE dst USING OPERATION_ID 'mi-trunc'",
    )
    .expect("truncate");
    let mut txn = core.new_session_transaction();
    let mut ts = SessionState::default();
    let c = ctx("alice");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &c,
        &mut ts,
        &mut txn,
        "INSERT INTO dst SELECT id, a, t FROM src LIMIT 100 USING OPERATION_ID 'mi-2'",
    )
    .expect("insert in txn");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, "dst"), 0);
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin 2");
    core.execute_sql_in_txn(
        &c,
        &mut ts,
        &mut txn,
        "INSERT INTO dst SELECT id, a, t FROM src LIMIT 100 USING OPERATION_ID 'mi-3'",
    )
    .expect("insert in txn 2");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count(&core, "dst"), 2);
}
