//! 一時表への DML・索引・制約（TABLE-44・Issue #1684）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` の TABLE-44（SQL-31・RECOVER-1・SQL-23 を併用）。
//!
//! 同じ SQL の台本を「永続表」と「一時表」の両方で実行し、結果と `wire_code` が一致する
//! ことで、永続表と同じ観点（制約・`RETURNING`・UPSERT・述語形 DML・`TRUNCATE`・集計・JOIN・
//! トランザクション）が一時表でも成り立つことを確かめる。あわせて、一時表の文が同名の永続表へ
//! 作用しない（隠蔽・汚染の両方向）、`operation_id` 台帳の対象外、`ON COMMIT`、総バイト数の上限、
//! 索引、未対応の形の fail-closed な拒否を確かめる。

use engine::core::CopyPlan;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::temp_table_limits::TempTableLimits;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "tenant-a";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str, mode: LedgerMode) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode),
        guard,
    )
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

/// 結果の要約（行の中身まで比較できる文字列）。失敗は `wire_code`。
fn summarize(r: Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError>) -> String {
    match r {
        Ok(o) => format!("{o:?}"),
        Err(e) => format!("ERR {}", e.wire_code()),
    }
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    summarize(core.execute_sql_in_session(&ctx(TENANT), s, sql))
}

fn run_ok(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(TENANT), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(TENANT), s, sql) {
        Ok(o) => format!("ok:{o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn rows(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<Vec<String>> {
    match run_ok(core, s, sql) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| {
                let mut out = vec![row.id.to_string()];
                out.extend(row.cells.iter().map(|c| format!("{c:?}")));
                out
            })
            .collect(),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

const COLUMNS: &str =
    "(id BIGINT PRIMARY KEY, name TEXT NOT NULL, qty INTEGER DEFAULT 7 CHECK (qty >= 0), code TEXT UNIQUE)";

/// `{T}` を表名に置換する。
fn sub(sql: &str, table: &str) -> String {
    sql.replace("{T}", table)
}

/// 永続表と一時表で同じ結果になるべき台本。
const SCRIPT: &[&str] = &[
    "INSERT INTO {T} (id, name, qty, code) VALUES (1, 'a', 10, 'c1')",
    "INSERT INTO {T} (id, name, qty, code) VALUES (2, 'b', 20, 'c2'), (3, 'c', 30, 'c3')",
    // DEFAULT（qty 省略）。
    "INSERT INTO {T} (id, name, code) VALUES (4, 'd', 'c4')",
    // 重複 id・UNIQUE 違反・CHECK 違反・NOT NULL 違反。
    "INSERT INTO {T} (id, name, qty, code) VALUES (1, 'x', 1, 'cx')",
    "INSERT INTO {T} (id, name, qty, code) VALUES (9, 'x', 1, 'c1')",
    "INSERT INTO {T} (id, name, qty, code) VALUES (9, 'x', -1, 'cx')",
    "INSERT INTO {T} (id, qty, code) VALUES (9, 1, 'cx')",
    "SELECT id, name, qty, code FROM {T} ORDER BY id LIMIT 10",
    "SELECT id, name FROM {T} WHERE qty > 15 ORDER BY id LIMIT 10",
    "SELECT COUNT(*), SUM(qty), MIN(qty), MAX(qty) FROM {T}",
    "SELECT name, COUNT(*) FROM {T} GROUP BY name ORDER BY name LIMIT 10",
    // UPSERT。
    "INSERT INTO {T} (id, name, qty, code) VALUES (1, 'a2', 11, 'c1') ON CONFLICT (id) DO NOTHING",
    "INSERT INTO {T} (id, name, qty, code) VALUES (1, 'a3', 12, 'c1') ON CONFLICT (id) DO UPDATE SET name = 'a3', qty = 12",
    "SELECT id, name, qty FROM {T} WHERE id = 1 LIMIT 5",
    // RETURNING。
    "INSERT INTO {T} (id, name, qty, code) VALUES (5, 'e', 50, 'c5') RETURNING id, name",
    "UPDATE {T} SET qty = 51 WHERE id = 5 RETURNING id, qty",
    "UPDATE {T} SET qty = -5 WHERE id = 5",
    "UPDATE {T} SET qty = qty + 1 WHERE qty > 20",
    "SELECT id, qty FROM {T} ORDER BY id LIMIT 10",
    "DELETE FROM {T} WHERE id = 5 RETURNING id, name",
    "DELETE FROM {T} WHERE qty > 30",
    "SELECT id FROM {T} ORDER BY id LIMIT 10",
    "DELETE FROM {T} WHERE id = 3",
    "SELECT COUNT(*) FROM {T}",
    "TRUNCATE {T}",
    "SELECT COUNT(*) FROM {T}",
    "INSERT INTO {T} (id, name, qty, code) VALUES (1, 'again', 1, 'c1')",
    "SELECT id, name FROM {T} LIMIT 10",
];

fn run_script(core: &EngineCore, s: &mut SessionState, table: &str) -> Vec<String> {
    SCRIPT
        .iter()
        .map(|q| run(core, s, &sub(q, table)))
        .collect()
}

#[test]
fn temp_table_behaves_like_a_persistent_table_for_dml_and_constraints() {
    let (core, _g) = new_core("t44-dml-parity", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    run_ok(&core, &mut s, &format!("CREATE TABLE perm {COLUMNS}"));
    run_ok(&core, &mut s, &format!("CREATE TEMP TABLE tmp {COLUMNS}"));
    let perm = run_script(&core, &mut s, "perm");
    let temp = run_script(&core, &mut s, "tmp");
    assert_eq!(perm.len(), temp.len());
    for (i, (p, t)) in perm.iter().zip(&temp).enumerate() {
        assert_eq!(p, t, "script step {i}: {}", SCRIPT[i]);
    }
    // 台本が空振りしていないこと（拒否の観点が実際に発火している）。
    let errs: Vec<&String> = temp.iter().filter(|r| r.starts_with("ERR")).collect();
    assert!(
        errs.len() >= 5,
        "constraint violations must fire: {temp:#?}"
    );
}

// ---- 同名の永続表との分離（隠蔽・汚染の両方向。P0） ------------------------------------

fn names(core: &EngineCore, s: &mut SessionState, table: &str) -> Vec<String> {
    rows(
        core,
        s,
        &format!("SELECT id, name FROM {table} ORDER BY id LIMIT 100"),
    )
    .into_iter()
    // 宣言した `id` 列の値（`SignedInteger(n)`）と `name`。行の物理 rowid（`r[0]`）は意味を持たない
    // （TRUNCATE 後も採番は続く。TABLE-23・Issue #2199）。
    .map(|r| {
        let id = r[1]
            .trim_start_matches("SignedInteger(")
            .trim_end_matches(')');
        format!("{id}:{}", r[2])
    })
    .collect()
}

fn count(core: &EngineCore, s: &mut SessionState, table: &str) -> String {
    rows(core, s, &format!("SELECT COUNT(*) FROM {table}"))[0][1].clone()
}

#[test]
fn temp_statements_never_touch_a_same_named_persistent_table() {
    let (core, _g) = new_core("t44-dml-shadow", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    run_ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    for (id, n) in [(1, "perm-a"), (2, "perm-b"), (3, "perm-c")] {
        run_ok(
            &core,
            &mut s,
            &format!("INSERT INTO t (id, name, qty, code) VALUES ({id}, '{n}', {id}, 'p{id}')"),
        );
    }
    // 別セッションで永続表を読み、キャッシュを温めておく。
    let mut other = SessionState::default();
    let perm_before = names(&core, &mut other, "t");
    assert_eq!(
        perm_before,
        [
            "1:Text(\"perm-a\")",
            "2:Text(\"perm-b\")",
            "3:Text(\"perm-c\")"
        ]
    );
    assert_eq!(count(&core, &mut other, "t"), "Integer(3)");

    // 同名の一時表を作る。同じ id・同じ UNIQUE 値でも衝突しない（永続表に触れていれば 23505）。
    run_ok(&core, &mut s, &format!("CREATE TEMP TABLE t {COLUMNS}"));
    assert_eq!(
        count(&core, &mut s, "t"),
        "Integer(0)",
        "persistent rows are hidden"
    );
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, qty, code) VALUES (1, 'temp-a', 10, 'p1'), (9, 'temp-z', 90, 'p9')",
    );
    assert_eq!(
        names(&core, &mut s, "t"),
        ["1:Text(\"temp-a\")", "9:Text(\"temp-z\")"]
    );
    // 全文種で、一時表だけに作用する。
    run_ok(&core, &mut s, "UPDATE t SET name = 'temp-up' WHERE id = 1");
    run_ok(
        &core,
        &mut s,
        "UPDATE t SET qty = qty + 1 WHERE qty > 0 RETURNING id",
    );
    run_ok(&core, &mut s, "DELETE FROM t WHERE id = 9");
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (2, 'temp-b', 'q2') ON CONFLICT (id) DO NOTHING",
    );
    assert_eq!(
        names(&core, &mut s, "t"),
        ["1:Text(\"temp-up\")", "2:Text(\"temp-b\")"]
    );
    run_ok(&core, &mut s, "TRUNCATE t");
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (5, 'temp-5', 'p5')",
    );

    // 永続表は無傷（別セッション。キャッシュ済みの内容とも一致する）。
    assert_eq!(names(&core, &mut other, "t"), perm_before);
    assert_eq!(count(&core, &mut other, "t"), "Integer(3)");

    // 一時表を落とすと、同じセッションからも永続表が元のまま見える。
    run_ok(&core, &mut s, "DROP TABLE t");
    assert_eq!(names(&core, &mut s, "t"), perm_before);

    // 作り直した一時表は空で、前の一時表の内容も永続表の内容も持たない（世代キーのキャッシュ汚染なし）。
    run_ok(&core, &mut s, &format!("CREATE TEMP TABLE t {COLUMNS}"));
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (1, 'again', 'z1')",
    );
    assert_eq!(names(&core, &mut s, "t"), ["1:Text(\"again\")"]);
    run_ok(&core, &mut s, "DROP TABLE t");
    assert_eq!(names(&core, &mut s, "t"), perm_before);
}

#[test]
fn another_session_or_tenant_never_sees_a_temp_table() {
    let (core, _g) = new_core("t44-dml-isolation", LedgerMode::CompareOnlyWithoutLedger);
    let mut a = SessionState::default();
    run_ok(
        &core,
        &mut a,
        "CREATE TEMP TABLE only_a (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(
        &core,
        &mut a,
        "INSERT INTO only_a (id, name) VALUES (1, 'secret')",
    );
    let mut b = SessionState::default();
    assert_eq!(
        code(&core, &mut b, "SELECT id FROM only_a LIMIT 1"),
        "42P01"
    );
    assert_eq!(
        code(
            &core,
            &mut b,
            "INSERT INTO only_a (id, name) VALUES (2, 'x')"
        ),
        "42P01"
    );
    // 一時表を作ったセッションを別テナントの ctx で使うと、固定文言で拒否される（データを返さない）。
    let other_tenant =
        core.execute_sql_in_session(&ctx("tenant-b"), &mut a, "SELECT id FROM only_a LIMIT 1");
    let err = other_tenant.expect_err("tenant mismatch must fail closed");
    assert_eq!(err.wire_code(), "XX000");
    assert!(!format!("{err:?}{err}").contains("secret"));
}

// ---- ビュー・混在・未対応の形（fail-closed） --------------------------------------------

#[test]
fn views_and_mixed_statements_are_rejected_not_misrouted() {
    let (core, _g) = new_core("t44-dml-views", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    run_ok(&core, &mut s, &format!("CREATE TABLE t {COLUMNS}"));
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE p2 (id BIGINT PRIMARY KEY, note TEXT)",
    );
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, qty, code) VALUES (1, 'perm', 1, 'p1')",
    );
    run_ok(&core, &mut s, "INSERT INTO p2 (id, note) VALUES (1, 'n1')");
    run_ok(&core, &mut s, "CREATE VIEW v AS SELECT id, name FROM t");
    assert_eq!(
        rows(&core, &mut s, "SELECT id, name FROM v LIMIT 10").len(),
        1
    );

    run_ok(&core, &mut s, &format!("CREATE TEMP TABLE t {COLUMNS}"));
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, code) VALUES (1, 'temp', 't1')",
    );
    // 永続ビューは、同名の一時表ではなく永続表を読み続ける（一時表の行 'temp' を返したら fail-open）。
    let via_view = rows(&core, &mut s, "SELECT id, name FROM v LIMIT 10");
    assert_eq!(via_view.len(), 1);
    assert_eq!(via_view[0][2], "Text(\"perm\")");
    // 一時表を参照するビューは作れない。
    assert_eq!(
        code(&core, &mut s, "CREATE VIEW v2 AS SELECT id FROM t"),
        "0A000"
    );
    // 一時表と永続表の混在（JOIN・TRUNCATE・サブクエリ）は拒否される。
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT t.name, p2.note FROM t JOIN p2 ON t.id = p2.id LIMIT 10"
        ),
        "0A000"
    );
    assert_eq!(code(&core, &mut s, "TRUNCATE t, p2"), "0A000");
    assert!(core
        .execute_sql_in_session(
            &ctx(TENANT),
            &mut s,
            "SELECT name FROM t WHERE id IN (SELECT id FROM p2) LIMIT 10"
        )
        .is_err());
    // 振り分け対象外の文種。
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE t ADD COLUMN z TEXT"),
        "0A000"
    );
    // 失敗した文は何も変えない。
    assert_eq!(names(&core, &mut s, "t"), ["1:Text(\"temp\")"]);
    run_ok(&core, &mut s, "DROP TABLE t");
    assert_eq!(
        rows(&core, &mut s, "SELECT id, name FROM v LIMIT 10").len(),
        1
    );
    // 一時表どうしの JOIN・集計は使える。
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE a (id BIGINT PRIMARY KEY, k INTEGER)",
    );
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE b (id BIGINT PRIMARY KEY, k INTEGER, label TEXT)",
    );
    run_ok(&core, &mut s, "INSERT INTO a (id, k) VALUES (1, 7), (2, 8)");
    run_ok(
        &core,
        &mut s,
        "INSERT INTO b (id, k, label) VALUES (1, 7, 'seven')",
    );
    let joined = rows(
        &core,
        &mut s,
        "SELECT a.id, b.label FROM a JOIN b ON a.k = b.k LIMIT 10",
    );
    assert_eq!(joined.len(), 1);
}

// ---- operation_id 台帳の対象外（TABLE-44 (5)） ---------------------------------------------

#[test]
fn temp_writes_ignore_the_operation_id_ledger() {
    // 台帳あり・id 必須の構成（`--require-operation-id` 相当）でも、一時表への書き込みは id を要求しない。
    let (core, _g) = new_core("t44-dml-ledger", LedgerMode::Ledgered);
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE perm (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, name TEXT)",
    );
    assert_eq!(
        code(&core, &mut s, "INSERT INTO perm (id, name) VALUES (1, 'x')"),
        "23502",
        "persistent tables keep requiring operation_id"
    );
    run_ok(&core, &mut s, "INSERT INTO tmp (id, name) VALUES (1, 'x')");
    run_ok(&core, &mut s, "UPDATE tmp SET name = 'y' WHERE id = 1");
    run_ok(&core, &mut s, "DELETE FROM tmp WHERE id = 1");
    run_ok(&core, &mut s, "TRUNCATE tmp");
    // 再送しても台帳による重複判定は働かない（同じ内容を何度でも書ける）。
    run_ok(&core, &mut s, "INSERT INTO tmp (id, name) VALUES (1, 'x')");
    assert_eq!(
        code(&core, &mut s, "INSERT INTO tmp (id, name) VALUES (1, 'x')"),
        "23505"
    );
    // USING OPERATION_ID は黙って捨てずに拒否する。
    for sql in [
        "INSERT INTO tmp (id, name) VALUES (2, 'x') USING OPERATION_ID 'op-1'",
        "UPDATE tmp SET name = 'z' WHERE id = 1 USING OPERATION_ID 'op-2'",
        "DELETE FROM tmp WHERE id = 1 USING OPERATION_ID 'op-3'",
        "TRUNCATE tmp USING OPERATION_ID 'op-4'",
        "UPDATE tmp SET name = 'z' WHERE name = 'x' USING OPERATION_ID 'op-5' PARTITIONED",
        "DELETE FROM tmp WHERE name = 'x' USING OPERATION_ID 'op-6' PARTITIONED CHUNK 1",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    assert_eq!(names(&core, &mut s, "tmp"), ["1:Text(\"x\")"]);
    // 永続側の台帳は従来どおり動く。
    run_ok(
        &core,
        &mut s,
        "INSERT INTO perm (id, name) VALUES (1, 'x') USING OPERATION_ID 'perm-op-1'",
    );
    assert_eq!(names(&core, &mut s, "perm"), ["1:Text(\"x\")"]);
}

// ---- トランザクション -----------------------------------------------------------------

fn tx<'e>(
    core: &'e EngineCore,
    s: &mut SessionState,
    t: &mut SessionTransaction<'e>,
    sql: &str,
) -> Result<SqlOutcome, String> {
    core.execute_sql_in_txn(&ctx(TENANT), s, t, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn tx_count<'e>(
    core: &'e EngineCore,
    s: &mut SessionState,
    t: &mut SessionTransaction<'e>,
    table: &str,
) -> String {
    match tx(core, s, t, &format!("SELECT COUNT(*) FROM {table}")).expect("count") {
        SqlOutcome::Query(r) => format!("{:?}", r.rows[0].cells[0]),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn explicit_transactions_apply_commit_and_rollback_to_temp_tables() {
    let (core, _g) = new_core("t44-dml-txn", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE perm (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
    );
    let mut t = core.new_session_transaction();

    // ROLLBACK: 自分の書き込みは同じブロックから見え、巻き戻すと永続・一時の両方が消える。
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    tx(
        &core,
        &mut s,
        &mut t,
        "INSERT INTO tmp (id, name) VALUES (1, 'a'), (2, 'b')",
    )
    .unwrap();
    tx(
        &core,
        &mut s,
        &mut t,
        "INSERT INTO perm (id, name) VALUES (1, 'p')",
    )
    .unwrap();
    assert_eq!(tx_count(&core, &mut s, &mut t, "tmp"), "Integer(2)");
    tx(
        &core,
        &mut s,
        &mut t,
        "UPDATE tmp SET name = 'a2' WHERE id = 1",
    )
    .unwrap();
    tx(&core, &mut s, &mut t, "DELETE FROM tmp WHERE id = 2").unwrap();
    assert_eq!(tx_count(&core, &mut s, &mut t, "tmp"), "Integer(1)");
    tx(&core, &mut s, &mut t, "ROLLBACK").unwrap();
    assert_eq!(count(&core, &mut s, "tmp"), "Integer(0)");
    assert_eq!(count(&core, &mut s, "perm"), "Integer(0)");

    // COMMIT: 両方が残る。
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    tx(
        &core,
        &mut s,
        &mut t,
        "INSERT INTO tmp (id, name) VALUES (1, 'a')",
    )
    .unwrap();
    tx(
        &core,
        &mut s,
        &mut t,
        "INSERT INTO perm (id, name) VALUES (1, 'p')",
    )
    .unwrap();
    tx(&core, &mut s, &mut t, "COMMIT").unwrap();
    assert_eq!(count(&core, &mut s, "tmp"), "Integer(1)");
    assert_eq!(count(&core, &mut s, "perm"), "Integer(1)");

    // 失敗した文はトランザクションを Failed にし、ROLLBACK で一時側の書き込みも消える。
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    tx(
        &core,
        &mut s,
        &mut t,
        "INSERT INTO tmp (id, name) VALUES (2, 'b')",
    )
    .unwrap();
    assert_eq!(
        tx(
            &core,
            &mut s,
            &mut t,
            "INSERT INTO tmp (id, name) VALUES (1, 'dup')"
        )
        .unwrap_err(),
        "23505"
    );
    assert_eq!(t.status(), TransactionStatus::Failed);
    assert_eq!(
        tx(&core, &mut s, &mut t, "SELECT id FROM tmp LIMIT 1").unwrap_err(),
        "25P02"
    );
    tx(&core, &mut s, &mut t, "ROLLBACK").unwrap();
    assert_eq!(count(&core, &mut s, "tmp"), "Integer(1)");

    // 索引 DDL は明示トランザクション内では拒否される（永続側の DDL と同じ）。
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    assert_eq!(
        tx(&core, &mut s, &mut t, "CREATE INDEX i ON tmp (name)").unwrap_err(),
        "0A000"
    );
    tx(&core, &mut s, &mut t, "ROLLBACK").unwrap();

    // 永続側だけの書き込みを含むトランザクションも、一時表を壊さず COMMIT できる。
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    tx(
        &core,
        &mut s,
        &mut t,
        "INSERT INTO perm (id, name) VALUES (2, 'q')",
    )
    .unwrap();
    assert_eq!(tx_count(&core, &mut s, &mut t, "perm"), "Integer(2)");
    tx(&core, &mut s, &mut t, "COMMIT").unwrap();
    assert_eq!(count(&core, &mut s, "perm"), "Integer(2)");
    assert_eq!(count(&core, &mut s, "tmp"), "Integer(1)");
}

#[test]
fn on_commit_delete_rows_and_drop_apply_through_sql() {
    let (core, _g) = new_core("t44-dml-oncommit", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = SessionState::default();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE d (id BIGINT PRIMARY KEY, name TEXT) ON COMMIT DELETE ROWS",
    );
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE p (id BIGINT PRIMARY KEY, name TEXT)",
    );
    // 自動コミットの文の終わりで DELETE ROWS の表は空になる。
    run_ok(&core, &mut s, "INSERT INTO d (id, name) VALUES (1, 'x')");
    run_ok(&core, &mut s, "INSERT INTO p (id, name) VALUES (1, 'x')");
    assert_eq!(count(&core, &mut s, "d"), "Integer(0)");
    assert_eq!(count(&core, &mut s, "p"), "Integer(1)");
    // 明示トランザクションの中では見え、COMMIT で消える。
    let mut t = core.new_session_transaction();
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    tx(
        &core,
        &mut s,
        &mut t,
        "INSERT INTO d (id, name) VALUES (2, 'y')",
    )
    .unwrap();
    assert_eq!(tx_count(&core, &mut s, &mut t, "d"), "Integer(1)");
    tx(&core, &mut s, &mut t, "COMMIT").unwrap();
    assert_eq!(count(&core, &mut s, "d"), "Integer(0)");
    assert_eq!(count(&core, &mut s, "p"), "Integer(1)");
    // ON COMMIT DROP の表は同じ文の終わりで消える。
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE x (id BIGINT PRIMARY KEY, name TEXT) ON COMMIT DROP",
    );
    assert_eq!(code(&core, &mut s, "SELECT id FROM x LIMIT 1"), "42P01");
}

// ---- 総バイト数の上限 -----------------------------------------------------------------------

#[test]
fn temp_byte_limit_yields_54000_and_keeps_earlier_rows() {
    let path = unique_db_path("t44-dml-limit");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_temp_table_limits(TempTableLimits::new(4, 256 * 1024));
    let mut s = SessionState::default();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE big (id BIGINT PRIMARY KEY, body TEXT)",
    );
    let body = "z".repeat(4000);
    let mut ok = 0u64;
    let mut failure = None;
    for i in 1..=200u64 {
        match core.execute_sql_in_session(
            &ctx(TENANT),
            &mut s,
            &format!("INSERT INTO big (id, body) VALUES ({i}, '{body}')"),
        ) {
            Ok(_) => ok += 1,
            Err(e) => {
                failure = Some(e.wire_code().to_string());
                break;
            }
        }
    }
    assert_eq!(failure.as_deref(), Some("54000"), "limit must be reached");
    assert!(ok >= 10, "rows before the limit must be accepted (ok={ok})");
    // 超過した文は何も残さず、それ以前の行は残る。
    assert_eq!(count(&core, &mut s, "big"), format!("Integer({ok})"));
    // 空きができれば再び書ける。
    run_ok(&core, &mut s, "TRUNCATE big");
    run_ok(
        &core,
        &mut s,
        "INSERT INTO big (id, body) VALUES (1, 'small')",
    );
    // 明示トランザクションでも超過すると Failed になり、巻き戻る。
    let mut t = core.new_session_transaction();
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    let mut failure = None;
    for i in 2..=300u64 {
        if let Err(c) = tx(
            &core,
            &mut s,
            &mut t,
            &format!("INSERT INTO big (id, body) VALUES ({i}, '{body}')"),
        ) {
            failure = Some(c);
            break;
        }
    }
    assert_eq!(failure.as_deref(), Some("54000"));
    assert_eq!(t.status(), TransactionStatus::Failed);
    tx(&core, &mut s, &mut t, "ROLLBACK").unwrap();
    assert_eq!(count(&core, &mut s, "big"), "Integer(1)");
}

// ---- 索引 ----------------------------------------------------------------------------------------

#[test]
fn temp_indexes_and_unique_indexes_work() {
    let (core, _g) = new_core("t44-dml-index", LedgerMode::CompareOnlyWithoutLedger);
    // DDL 権限なし: 一時表の索引は権限ゲートの対象外（セッション内で閉じる）。
    let mut s = SessionState::default();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, name TEXT, qty INTEGER)",
    );
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, qty) VALUES (1, 'a', 5), (2, 'b', 5), (3, 'c', 6)",
    );
    run_ok(&core, &mut s, "CREATE INDEX by_name ON t (name)");
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM t WHERE name = 'b' LIMIT 10").len(),
        1
    );
    // 既存行が重複していれば UNIQUE 索引は作れない。
    assert_eq!(
        code(&core, &mut s, "CREATE UNIQUE INDEX u_qty ON t (qty)"),
        "23505"
    );
    run_ok(&core, &mut s, "CREATE UNIQUE INDEX u_name ON t (name)");
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO t (id, name, qty) VALUES (4, 'a', 9)"
        ),
        "23505"
    );
    assert_eq!(
        code(&core, &mut s, "CREATE INDEX by_name ON t (name)"),
        "42P07"
    );
    run_ok(&core, &mut s, "DROP INDEX u_name");
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, qty) VALUES (4, 'a', 9)",
    );
    // 削除後は同名を作り直せる。
    run_ok(&core, &mut s, "DROP INDEX by_name");
    run_ok(&core, &mut s, "CREATE INDEX by_name ON t (name)");
    // 未対応の索引（HNSW・存在しない表）は拒否される。
    assert!(core
        .execute_sql_in_session(
            &ctx(TENANT),
            &mut s,
            "CREATE INDEX h ON t USING hnsw (name)"
        )
        .is_err());
    // 一時表でない名前への索引は従来の永続の経路（DDL 権限ゲート）に回る。
    assert_eq!(
        code(&core, &mut s, "CREATE INDEX z ON nope (name)"),
        "42501"
    );
}
/// 一時表への式索引（INDEX-10、Issue #1648）は永続表と同じ経路（束縛・永続化）で作成でき、
/// 同じ分類で拒否される。式が評価できる DML は成功し、評価エラーになる DML は永続表と同じ
/// SQLSTATE で拒否される（Issue #1649）。一時表への `ALTER` は未対応。
#[test]
fn temp_expression_indexes_share_the_persistent_path() {
    let (core, _g) = new_core("t44-dml-expr-index", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = SessionState::default();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, name TEXT, qty INTEGER)",
    );
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, qty) VALUES (1, 'A', 5), (2, 'b', 5)",
    );
    run_ok(&core, &mut s, "CREATE INDEX by_lower ON t ((lower(name)))");
    assert_eq!(
        code(&core, &mut s, "CREATE INDEX by_lower ON t ((lower(name)))"),
        "42P07"
    );
    assert_eq!(
        code(&core, &mut s, "CREATE INDEX bad ON t ((lower(nope)))"),
        "42703"
    );
    assert_eq!(
        code(&core, &mut s, "CREATE INDEX bad ON t ((now()))"),
        "42P17"
    );
    // 式が評価できる DML は式索引の有無に影響されない。
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name, qty) VALUES (3, 'C', 7)",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM t WHERE qty = 5 LIMIT 10").len(),
        2
    );
    // 評価エラーになる DML は永続表と同じ SQLSTATE で拒否される（Issue #1649）。
    run_ok(&core, &mut s, "CREATE INDEX by_div ON t ((100 / qty))");
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO t (id, name, qty) VALUES (4, 'D', 0)"
        ),
        "22012"
    );
    assert_eq!(
        code(&core, &mut s, "UPDATE t SET qty = 0 WHERE id = 1"),
        "22012"
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT id FROM t WHERE qty = 5 LIMIT 10").len(),
        2
    );
    run_ok(&core, &mut s, "DROP INDEX by_div");
    run_ok(&core, &mut s, "DROP INDEX by_lower");
}

// ---- COPY ---------------------------------------------------------------------------------------

/// `COPY ... FROM STDIN` を wire-server と同じ形（`begin_copy_in_txn` → `feed` → `finish` →
/// `commit_copy_in_txn_in_session`）で駆動する。
fn copy_in<'e>(
    core: &'e EngineCore,
    s: &mut SessionState,
    t: &mut SessionTransaction<'e>,
    sql: &str,
    data: &[u8],
) -> Result<u64, String> {
    let c = ctx(TENANT);
    let map = |e: engine::sql::allowlist::SqlSurfaceError| e.wire_code().to_string();
    let plan = core.begin_copy_in_txn(&c, s, t, sql).map_err(map)?;
    let CopyPlan::From(mut copy) = plan else {
        panic!("expected COPY FROM plan");
    };
    copy.feed(data).map_err(map)?;
    let batch = copy.finish().map_err(map)?;
    core.commit_copy_in_txn_in_session(&c, s, t, batch)
        .map(|o| o.rows_affected)
        .map_err(map)
}

fn copy_out<'e>(
    core: &'e EngineCore,
    s: &mut SessionState,
    t: &mut SessionTransaction<'e>,
    sql: &str,
) -> Result<usize, String> {
    match core.begin_copy_in_txn(&ctx(TENANT), s, t, sql) {
        Ok(CopyPlan::To(_, result)) => Ok(result.rows.len()),
        Ok(CopyPlan::From(_)) => panic!("expected COPY TO plan"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

#[test]
fn copy_from_and_to_work_on_temp_tables_without_touching_a_same_named_persistent_table() {
    let (core, _g) = new_core("t44-dml-copy", LedgerMode::Ledgered);
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(
        &core,
        &mut s,
        "INSERT INTO t (id, name) VALUES (1, 'perm') USING OPERATION_ID 'perm-1'",
    );
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE d (id BIGINT PRIMARY KEY, name TEXT) ON COMMIT DELETE ROWS",
    );
    let mut t = core.new_session_transaction();

    // `--require-operation-id` 相当の構成でも id 不要。永続表の同 id と衝突しない。
    let n = copy_in(
        &core,
        &mut s,
        &mut t,
        "COPY t (id, name) FROM STDIN",
        b"1\talpha\n2\tbeta\n",
    );
    assert_eq!(n, Ok(2));
    assert_eq!(
        names(&core, &mut s, "t"),
        ["1:Text(\"alpha\")", "2:Text(\"beta\")"]
    );
    assert_eq!(
        copy_out(
            &core,
            &mut s,
            &mut t,
            "COPY (SELECT id, name FROM t LIMIT 10) TO STDOUT"
        ),
        Ok(2)
    );
    // 重複は 23505、USING OPERATION_ID は 0A000。
    assert_eq!(
        copy_in(
            &core,
            &mut s,
            &mut t,
            "COPY t (id, name) FROM STDIN",
            b"1\tdup\n"
        ),
        Err("23505".to_string())
    );
    assert_eq!(
        copy_in(
            &core,
            &mut s,
            &mut t,
            "COPY t (id, name) FROM STDIN USING OPERATION_ID 'x'",
            b"9\tz\n"
        ),
        Err("0A000".to_string())
    );
    // 自動コミットの COPY の後に ON COMMIT DELETE ROWS が適用される。
    assert_eq!(
        copy_in(
            &core,
            &mut s,
            &mut t,
            "COPY d (id, name) FROM STDIN",
            b"1\tx\n"
        ),
        Ok(1)
    );
    assert_eq!(count(&core, &mut s, "d"), "Integer(0)");

    // 明示トランザクション: COMMIT で確定、ROLLBACK で破棄。
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    assert_eq!(
        copy_in(
            &core,
            &mut s,
            &mut t,
            "COPY t (id, name) FROM STDIN",
            b"7\tseven\n"
        ),
        Ok(1)
    );
    assert_eq!(
        copy_out(
            &core,
            &mut s,
            &mut t,
            "COPY (SELECT id, name FROM t LIMIT 10) TO STDOUT"
        ),
        Ok(3)
    );
    tx(&core, &mut s, &mut t, "ROLLBACK").unwrap();
    assert_eq!(count(&core, &mut s, "t"), "Integer(2)");
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    assert_eq!(
        copy_in(
            &core,
            &mut s,
            &mut t,
            "COPY t (id, name) FROM STDIN",
            b"7\tseven\n"
        ),
        Ok(1)
    );
    tx(&core, &mut s, &mut t, "COMMIT").unwrap();
    assert_eq!(count(&core, &mut s, "t"), "Integer(3)");

    // 永続表は無傷。
    run_ok(&core, &mut s, "DROP TABLE t");
    assert_eq!(names(&core, &mut s, "t"), ["1:Text(\"perm\")"]);
    // 永続表向けの COPY は従来どおり（`--require-operation-id` 相当の構成では operation_id が要る）。
    assert_eq!(
        copy_in(
            &core,
            &mut s,
            &mut t,
            "COPY t (id, name) FROM STDIN",
            b"5\tx\n"
        ),
        Err("23502".to_string())
    );
    assert_eq!(
        copy_in(
            &core,
            &mut s,
            &mut t,
            "COPY t (id, name) FROM STDIN USING OPERATION_ID 'perm-copy'",
            b"5\tx\n"
        ),
        Ok(1)
    );
}

#[test]
fn copy_batch_from_a_temp_table_never_commits_through_the_persistent_entry_points() {
    let (core, _g) = new_core("t44-dml-copy-entry", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = SessionState::default();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    let plan = core
        .begin_copy(&ctx(TENANT), &s, "COPY t (id, name) FROM STDIN")
        .expect("plan");
    let CopyPlan::From(mut copy) = plan else {
        panic!("expected COPY FROM plan");
    };
    copy.feed(b"1\tx\n").unwrap();
    let batch = copy.finish().unwrap();
    // セッションを持たない永続の入口は一時表向けのバッチを拒否する（永続 DB へ書かない）。
    let err = core
        .commit_copy_in(&ctx(TENANT), batch)
        .expect_err("must refuse");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(count(&core, &mut s, "t"), "Integer(0)");
}

// ---- 準備済み文（拡張クエリ）の再検証 -----------------------------------------------------------

#[test]
fn prepared_statements_are_revalidated_against_the_temp_catalog() {
    let (core, _g) = new_core("t44-dml-prepared", LedgerMode::CompareOnlyWithoutLedger);
    let c = ctx(TENANT);
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE other (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (1, 'perm')");

    // 一時表を作る前に準備した永続側の文。
    let on_t = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id, name FROM t LIMIT 5", &[])
        .expect("prepare on t");
    let on_other = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id, name FROM other LIMIT 5", &[])
        .expect("prepare on other");
    let plain = core
        .parse_sql("SELECT id, name FROM t LIMIT 5")
        .expect("plain parse");

    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(&core, &mut s, "INSERT INTO t (id, name) VALUES (7, 'temp')");

    // 同名の一時表ができた後は、一時表の名前を含む準備済み文は陳腐化として拒否される
    // （永続表 `t` の行 'perm' を黙って返さない。一時表の行 'temp' を返すのでもない）。
    let err = core
        .bind_prepared_in_session(&s, &on_t, &[])
        .expect_err("stale prepared");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(
        core.describe_prepared_in_session(&s, &on_t)
            .expect_err("stale describe")
            .wire_code(),
        "0A000"
    );
    // セッションを持たない parse で作った文は、粗いゲートで拒否される。
    assert_eq!(
        core.execute_parsed_in_session(&c, &mut s, &plain)
            .expect_err("coarse gate")
            .wire_code(),
        "0A000"
    );
    // 一時表の名前を含まない準備済み文は、一時表ができた後も実行できる。
    let bound = core
        .bind_prepared_in_session(&s, &on_other, &[])
        .expect("bind unrelated statement");
    assert!(matches!(
        core.execute_parsed_in_session(&c, &mut s, &bound),
        Ok(SqlOutcome::Query(_))
    ));
    assert!(core.describe_prepared_in_session(&s, &on_other).is_ok());

    // 一時表を対象に準備した文は一時表を読み、一時カタログが変わると陳腐化する。
    let temp_prepared = core
        .parse_sql_prepared_with_declared_in_session(&s, "SELECT id, name FROM t LIMIT 5", &[])
        .expect("prepare on temp t");
    let temp_bound = core
        .bind_prepared_in_session(&s, &temp_prepared, &[])
        .expect("bind on temp t");
    match core.execute_parsed_in_session(&c, &mut s, &temp_bound) {
        Ok(SqlOutcome::Query(r)) => {
            assert_eq!(r.rows.len(), 1);
            assert_eq!(format!("{:?}", r.rows[0].cells[1]), "Text(\"temp\")");
        }
        other => panic!("expected the temp row, got {other:?}"),
    }
    run_ok(&core, &mut s, "DROP TABLE t");
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    for result in [
        core.execute_parsed_in_session(&c, &mut s, &temp_bound)
            .map(|_| ()),
        core.bind_prepared_in_session(&s, &temp_prepared, &[])
            .map(|_| ()),
        core.describe_prepared_in_session(&s, &temp_prepared)
            .map(|_| ()),
    ] {
        assert_eq!(result.expect_err("stale").wire_code(), "0A000");
    }
    // 一時表の文を、永続コア向けの形に包み直して渡すことはできない（包みを外した形は別物）。
    assert!(matches!(
        temp_bound.peel_session_route(),
        engine::core::ParsedSql::Statement(_)
    ));
}

// ---- カーソル ---------------------------------------------------------------------------------------

#[test]
fn cursors_over_temp_tables_are_rejected() {
    let (core, _g) = new_core("t44-dml-cursor", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = SessionState::default();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    let mut t = core.new_session_transaction();
    tx(&core, &mut s, &mut t, "BEGIN").unwrap();
    assert_eq!(
        tx(
            &core,
            &mut s,
            &mut t,
            "DECLARE c CURSOR FOR SELECT id FROM t LIMIT 5"
        )
        .unwrap_err(),
        "0A000"
    );
    assert_eq!(t.status(), TransactionStatus::Failed);
    tx(&core, &mut s, &mut t, "ROLLBACK").unwrap();
}

// ---- DDL の容量上限・準備済み DROP INDEX の再検証 ---------------------------------------------

#[test]
fn temp_ddl_cannot_exceed_the_byte_limit() {
    let path = unique_db_path("t44-dml-ddl-limit");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_temp_table_limits(TempTableLimits::new(4, 1));
    let mut s = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx(TENANT),
            &mut s,
            "CREATE TEMP TABLE big (id BIGINT PRIMARY KEY, body TEXT)",
        )
        .expect_err("DDL over the byte limit");
    assert_eq!(err.wire_code(), "54000");
    // 確定されていない（同名の表は存在しない）。
    let err = core
        .execute_sql_in_session(&ctx(TENANT), &mut s, "SELECT id FROM big LIMIT 1")
        .expect_err("table must not exist");
    assert_eq!(err.wire_code(), "42P01");
}

#[test]
fn prepared_drop_index_does_not_hit_a_later_temp_index_of_the_same_name() {
    let (core, _g) = new_core("t44-dml-prep-drop-ix", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE p (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(&core, &mut s, "CREATE INDEX ix ON p (name)");
    let prepared = core
        .parse_sql_prepared_with_declared_in_session(&s, "DROP INDEX ix", &[])
        .expect("prepare");
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, name TEXT)",
    );
    run_ok(&core, &mut s, "CREATE INDEX ix ON tmp (name)");
    let err = core
        .bind_prepared_in_session(&s, &prepared, &[])
        .expect_err("stale prepared DROP INDEX");
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn temp_table_name_colliding_with_a_temp_index_follows_the_persistent_rules() {
    let (core, _g) = new_core("t44-dml-ix-name", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, a TEXT)",
    );
    run_ok(&core, &mut s, "CREATE INDEX ix ON t (a)");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TEMP TABLE ix (id BIGINT PRIMARY KEY, a TEXT)"
        ),
        "42P07"
    );
    assert!(
        code(
            &core,
            &mut s,
            "CREATE TEMP TABLE IF NOT EXISTS ix (id BIGINT PRIMARY KEY, a TEXT)"
        )
        .starts_with("ok:"),
        "IF NOT EXISTS must be a no-op success"
    );
}

#[test]
fn temp_create_table_if_not_exists_index_collision_wins_over_later_rejections() {
    let (core, _g) = new_core("t44-dml-ix-early", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, a TEXT)",
    );
    run_ok(&core, &mut s, "CREATE INDEX ix ON t (a)");
    // 同名索引があれば、FK 拒否（0A000）より前に no-op 成功する（永続表と同順序）。
    assert!(
        code(
            &core,
            &mut s,
            "CREATE TEMP TABLE IF NOT EXISTS ix (id BIGINT PRIMARY KEY, a TEXT, b TEXT REFERENCES t (a))"
        )
        .starts_with("ok:"),
        "IF NOT EXISTS collision must succeed before FK rejection"
    );
}

#[test]
fn temp_create_index_if_not_exists_keeps_the_existing_index() {
    let (core, _g) = new_core("t44-dml-ix-noop", LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    run_ok(
        &core,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, a TEXT)",
    );
    run_ok(&core, &mut s, "CREATE INDEX ix ON t (a)");
    assert!(code(&core, &mut s, "CREATE INDEX IF NOT EXISTS ix ON t (a)").starts_with("ok:"));
    // 既存索引が残っている（DROP INDEX が成功し、再度の DROP は 42704）。
    assert!(code(&core, &mut s, "DROP INDEX ix").starts_with("ok:"));
    assert_eq!(code(&core, &mut s, "DROP INDEX ix"), "42704");
}
