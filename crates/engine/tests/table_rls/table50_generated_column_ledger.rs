//! 生成列つきの表への `operation_id` 付き書き込みの台帳（内容照合）マトリクスと 3 テナントの越境の結合テスト
//! （Issue #3332。ポインタ: `docs/spec/04-behavior/` の TABLE-50 (5)・RECOVER-11・TABLE-24 (3)・TASK-345、
//! 設計記録: `docs/design/generated-column.md`）。
//!
//! 台帳の契約そのものは変えない監査テスト。各経路で (i) 初回成功 (ii) 同じ文の再送は `23505` で何も変えない
//! (iii) 内容違いの再送は `22023` (iv) 省略形と `DEFAULT` 形は同じ内容、を確かめる。照合は計算より前に行われ、
//! 再送では生成列を再計算しない。台帳キーはテナントを含み、3 テナントの同じ `operation_id` は互いに触れない。

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TA: &str = "tenant-a";
const TB: &str = "tenant-b";
const TC: &str = "tenant-c";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 既定の台帳モード（台帳を実際に使う）で core を開く。
fn fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    (guard, core)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {} {e}", e.wire_code()))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn message(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.to_string(),
    }
}

fn show(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        other => format!("{other:?}"),
    }
}

fn grid(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    let result = match ok(core, tenant, sql) {
        SqlOutcome::Query(q) => q,
        SqlOutcome::Returning(r) => r.result,
        other => panic!("{sql}: expected rows, got {other:?}"),
    };
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(show).collect())
        .collect()
}

fn g(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn dump(core: &EngineCore, tenant: &str, table: &str) -> Vec<Vec<String>> {
    grid(
        core,
        tenant,
        &format!("SELECT k, a, name, b FROM {table} ORDER BY k, a"),
    )
}

/// `k` 主キーの生成列つき表を作る。
fn mk(core: &EngineCore, tenant: &str, table: &str) {
    ok(
        core,
        tenant,
        &format!(
            "CREATE TABLE {table} (k INTEGER PRIMARY KEY, a INTEGER, name TEXT, \
             b INTEGER GENERATED ALWAYS AS (a * 2) STORED)"
        ),
    );
}

/// 初回成功 → 同じ文の再送 `23505`（無変化）→ 内容違い `22023`（無変化）を確かめる。
fn assert_ledger(
    core: &EngineCore,
    tenant: &str,
    table: &str,
    first: &str,
    same: &[&str],
    other: &str,
) {
    ok(core, tenant, first);
    let before = dump(core, tenant, table);
    assert_eq!(code(core, tenant, first), "23505", "{first}");
    assert_eq!(dump(core, tenant, table), before, "{first}");
    for s in same {
        assert_eq!(code(core, tenant, s), "23505", "{s}");
        assert_eq!(dump(core, tenant, table), before, "{s}");
    }
    assert_eq!(code(core, tenant, other), "22023", "{other}");
    assert_eq!(dump(core, tenant, table), before, "{other}");
}

#[test]
fn insert_forms_ledger() {
    let (_gd, core) = fixture("gen-l-insert");
    mk(&core, TA, "t");
    assert_ledger(
        &core,
        TA,
        "t",
        "INSERT INTO t (k, a) VALUES (1, 5) USING OPERATION_ID 'i1'",
        &["INSERT INTO t (k, a, b) VALUES (1, 5, DEFAULT) USING OPERATION_ID 'i1'"],
        "INSERT INTO t (k, a) VALUES (1, 6) USING OPERATION_ID 'i1'",
    );
    assert_ledger(
        &core,
        TA,
        "t",
        "INSERT INTO t (k, a) VALUES (2, 1), (3, 2) USING OPERATION_ID 'i2'",
        &[],
        "INSERT INTO t (k, a) VALUES (2, 1), (3, 9) USING OPERATION_ID 'i2'",
    );
    ok(&core, TA, "CREATE TABLE src (k INTEGER, a INTEGER)");
    ok(&core, TA, "INSERT INTO src (k, a) VALUES (10, 1), (11, 2)");
    assert_ledger(
        &core,
        TA,
        "t",
        "INSERT INTO t (k, a) SELECT k, a FROM src USING OPERATION_ID 'i3'",
        &[],
        "INSERT INTO t (k, a) SELECT k, a + 1 FROM src USING OPERATION_ID 'i3'",
    );
    assert_ledger(
        &core,
        TA,
        "t",
        "INSERT INTO t (k, a) VALUES (20, 4) RETURNING k, b USING OPERATION_ID 'i4'",
        &[],
        "INSERT INTO t (k, a) VALUES (20, 5) RETURNING k, b USING OPERATION_ID 'i4'",
    );
    assert_eq!(
        grid(&core, TA, "SELECT k, b FROM t WHERE k = 20"),
        g(&[&["20", "8"]])
    );
}

#[test]
fn update_forms_ledger() {
    let (_gd, core) = fixture("gen-l-update");
    mk(&core, TA, "t");
    ok(
        &core,
        TA,
        "INSERT INTO t (k, a) VALUES (1, 1), (2, 2), (3, 3)",
    );
    assert_ledger(
        &core,
        TA,
        "t",
        "UPDATE t SET a = 10 WHERE k = 1 USING OPERATION_ID 'u1'",
        &["UPDATE t SET a = 10, b = DEFAULT WHERE k = 1 USING OPERATION_ID 'u1'"],
        "UPDATE t SET a = 11 WHERE k = 1 USING OPERATION_ID 'u1'",
    );
    assert_ledger(
        &core,
        TA,
        "t",
        "UPDATE t SET a = a + 1 WHERE k > 1 USING OPERATION_ID 'u2'",
        &["UPDATE t SET a = a + 1, b = DEFAULT WHERE k > 1 USING OPERATION_ID 'u2'"],
        "UPDATE t SET a = a + 2 WHERE k > 1 USING OPERATION_ID 'u2'",
    );
    ok(&core, TA, "CREATE TABLE src (k INTEGER, v INTEGER)");
    ok(&core, TA, "INSERT INTO src (k, v) VALUES (1, 100)");
    assert_ledger(
        &core,
        TA,
        "t",
        "UPDATE t SET a = src.v FROM src WHERE t.k = src.k USING OPERATION_ID 'u3'",
        &[],
        "UPDATE t SET a = src.v + 1 FROM src WHERE t.k = src.k USING OPERATION_ID 'u3'",
    );
    assert_ledger(
        &core,
        TA,
        "t",
        "UPDATE t SET a = a + 1 WHERE k > 0 USING OPERATION_ID 'u4' PARTITIONED CHUNK 1",
        &[],
        "UPDATE t SET a = a + 5 WHERE k > 0 USING OPERATION_ID 'u4' PARTITIONED CHUNK 1",
    );
}

#[test]
fn upsert_forms_ledger() {
    let (_gd, core) = fixture("gen-l-upsert");
    mk(&core, TA, "t");
    ok(&core, TA, "INSERT INTO t (k, a) VALUES (1, 1)");
    assert_ledger(
        &core,
        TA,
        "t",
        "INSERT INTO t (k, a) VALUES (1, 7) ON CONFLICT (k) DO NOTHING USING OPERATION_ID 'p1'",
        &[],
        "INSERT INTO t (k, a) VALUES (1, 8) ON CONFLICT (k) DO NOTHING USING OPERATION_ID 'p1'",
    );
    assert_ledger(
        &core,
        TA,
        "t",
        "INSERT INTO t (k, a) VALUES (1, 7) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'p2'",
        &[],
        "INSERT INTO t (k, a) VALUES (1, 8) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'p2'",
    );
    // UPSERT の `DO UPDATE SET g = DEFAULT` は構文として拒否済みで（42601）、内容照合の材料にそもそも入らない。
    assert_eq!(
        code(
            &core,
            TA,
            "INSERT INTO t (k, a) VALUES (1, 9) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a, b = DEFAULT USING OPERATION_ID 'p3'"
        ),
        "42601"
    );
}

#[test]
fn copy_from_ledger() {
    let (_gd, core) = fixture("gen-l-copy");
    mk(&core, TA, "t");
    let policy = ctx(TA);
    let session = SessionState::default();
    let copy = |data: &[u8], op: &str| {
        let sql = format!("COPY t (k, a) FROM STDIN USING OPERATION_ID '{op}'");
        core.begin_copy(&policy, &session, &sql)
            .and_then(|plan| match plan {
                CopyPlan::From(mut s) => {
                    s.feed(data)?;
                    let batch = s.finish()?;
                    core.commit_copy_in(&policy, batch)
                }
                CopyPlan::To(..) => panic!("expected COPY FROM plan"),
            })
    };
    copy(b"1\t5\n", "c1").expect("first COPY");
    let before = dump(&core, TA, "t");
    let err = copy(b"1\t5\n", "c1").expect_err("resend");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(dump(&core, TA, "t"), before);
    let err = copy(b"1\t6\n", "c1").expect_err("changed");
    assert_eq!(err.wire_code(), "22023");
    assert_eq!(dump(&core, TA, "t"), before);
}

#[test]
fn view_write_ledger() {
    let (_gd, core) = fixture("gen-l-view");
    mk(&core, TA, "t");
    ok(&core, TA, "CREATE VIEW tv AS SELECT k, a, name, b FROM t");
    assert_ledger(
        &core,
        TA,
        "t",
        "INSERT INTO tv (k, a) VALUES (1, 5) USING OPERATION_ID 'v1'",
        &[],
        "INSERT INTO tv (k, a) VALUES (1, 6) USING OPERATION_ID 'v1'",
    );
    assert_ledger(
        &core,
        TA,
        "t",
        "UPDATE tv SET a = 8 WHERE k = 1 USING OPERATION_ID 'v2'",
        &[],
        "UPDATE tv SET a = 9 WHERE k = 1 USING OPERATION_ID 'v2'",
    );
}

/// 再送では再計算せず、照合は計算より前に行われる（`22012` にならない）。
#[test]
fn resend_does_not_recompute_and_matches_before_computing() {
    let (_gd, core) = fixture("gen-l-norecompute");
    mk(&core, TA, "t");
    ok(
        &core,
        TA,
        "INSERT INTO t (k, a) VALUES (1, 0) USING OPERATION_ID 'o'",
    );
    ok(&core, TA, "DELETE FROM t WHERE k = 1");
    ok(
        &core,
        TA,
        "ALTER TABLE t ADD COLUMN q INTEGER GENERATED ALWAYS AS (10 / a) STORED",
    );
    assert_eq!(
        code(
            &core,
            TA,
            "INSERT INTO t (k, a) VALUES (1, 0) USING OPERATION_ID 'o'"
        ),
        "23505"
    );
    assert_eq!(
        grid(&core, TA, "SELECT k FROM t"),
        Vec::<Vec<String>>::new()
    );
    // 同じ op で内容を変え、評価で失敗する入力にしても 22023（22012 にならない）。
    ok(
        &core,
        TA,
        "CREATE TABLE d (k INTEGER, a INTEGER, g INTEGER GENERATED ALWAYS AS (10 / a) STORED)",
    );
    ok(
        &core,
        TA,
        "INSERT INTO d (k, a) VALUES (1, 5) USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        code(
            &core,
            TA,
            "INSERT INTO d (k, a) VALUES (1, 0) USING OPERATION_ID 'o2'"
        ),
        "22023"
    );
}

/// スキーマ変更を挟んだ再送が `23505` で行を変えない。
#[test]
fn resend_after_schema_changes_is_still_a_duplicate() {
    let (_gd, core) = fixture("gen-l-schema");
    mk(&core, TA, "t");
    let stmt = "INSERT INTO t (k, a) VALUES (1, 5) USING OPERATION_ID 's1'";
    ok(&core, TA, stmt);
    ok(
        &core,
        TA,
        "ALTER TABLE t ADD COLUMN c INTEGER GENERATED ALWAYS AS (a + 1) STORED",
    );
    assert_eq!(code(&core, TA, stmt), "23505");
    ok(&core, TA, "UPDATE t SET a = 6 WHERE k = 1");
    assert_eq!(code(&core, TA, stmt), "23505");
    assert_eq!(
        grid(&core, TA, "SELECT k, a, b, c FROM t"),
        g(&[&["1", "6", "12", "7"]])
    );
    ok(&core, TA, "ALTER TABLE t ALTER COLUMN c DROP EXPRESSION");
    assert_eq!(code(&core, TA, stmt), "23505");
}

/// 計算の失敗で失敗した文は台帳を残さず、同じ op で直した文は通る。
#[test]
fn failed_computation_leaves_no_ledger() {
    let (_gd, core) = fixture("gen-l-fail");
    ok(
        &core,
        TA,
        "CREATE TABLE t (k INTEGER PRIMARY KEY, a INTEGER, g INTEGER GENERATED ALWAYS AS (10 / a) STORED)",
    );
    assert_eq!(
        code(
            &core,
            TA,
            "INSERT INTO t (k, a) VALUES (1, 0) USING OPERATION_ID 'f1'"
        ),
        "22012"
    );
    ok(
        &core,
        TA,
        "INSERT INTO t (k, a) VALUES (1, 2) USING OPERATION_ID 'f1'",
    );
    assert_eq!(
        code(
            &core,
            TA,
            "UPDATE t SET a = 0 WHERE k = 1 USING OPERATION_ID 'f2'"
        ),
        "22012"
    );
    ok(
        &core,
        TA,
        "UPDATE t SET a = 5 WHERE k = 1 USING OPERATION_ID 'f2'",
    );
    assert_eq!(grid(&core, TA, "SELECT k, g FROM t"), g(&[&["1", "2"]]));
}

/// 3 テナントが同じ `operation_id` で別内容を書いても互いに干渉せず、他テナントの存在を漏らさない。
#[test]
fn three_tenants_same_operation_id_do_not_interfere() {
    let (_gd, core) = fixture("gen-l-tenants");
    mk(&core, TA, "t");
    let stmt =
        |v: i32| format!("INSERT INTO t (k, a) VALUES ({v}, {v}) USING OPERATION_ID 'shared'");
    for (t, v) in [(TA, 1), (TB, 2), (TC, 3)] {
        ok(&core, t, &stmt(v));
    }
    for (t, v, w) in [(TA, 1, 2), (TB, 2, 3), (TC, 3, 1)] {
        assert_eq!(code(&core, t, &stmt(v)), "23505");
        assert_eq!(code(&core, t, &stmt(w)), "22023");
        let m = message(&core, t, &stmt(w));
        for other in [TA, TB, TC] {
            assert!(!m.contains(other), "{m}");
        }
        assert_eq!(
            grid(&core, t, "SELECT k, a, b FROM t"),
            g(&[&[&v.to_string(), &v.to_string(), &(v * 2).to_string()]])
        );
    }
    // 述語形 UPDATE・UPSERT は他テナントの行と生成列の値を変えない。
    ok(&core, TA, "UPDATE t SET a = a + 100 WHERE k > 0");
    ok(
        &core,
        TA,
        "INSERT INTO t (k, a) VALUES (2, 50) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a",
    );
    assert_eq!(
        grid(&core, TB, "SELECT k, a, b FROM t"),
        g(&[&["2", "2", "4"]])
    );
    assert_eq!(
        grid(&core, TC, "SELECT k, a, b FROM t"),
        g(&[&["3", "3", "6"]])
    );
}

/// 生成列の UNIQUE は、同じ生成値が別テナントにあっても通常の列と同じ振る舞いで、存在を漏らさない。
#[test]
fn generated_unique_behaves_like_plain_column_across_tenants() {
    let (_gd, core) = fixture("gen-l-unique");
    ok(
        &core,
        TA,
        "CREATE TABLE u (k INTEGER, a INTEGER, g INTEGER GENERATED ALWAYS AS (a * 2) STORED UNIQUE)",
    );
    ok(
        &core,
        TA,
        "CREATE TABLE p (k INTEGER, a INTEGER, g INTEGER UNIQUE)",
    );
    ok(&core, TB, "INSERT INTO u (k, a) VALUES (1, 5)");
    ok(&core, TB, "INSERT INTO p (k, a, g) VALUES (1, 5, 10)");
    let plain = run(&core, TC, "INSERT INTO p (k, a, g) VALUES (1, 5, 10)");
    let generated = run(&core, TC, "INSERT INTO u (k, a) VALUES (1, 5)");
    match (&plain, &generated) {
        (Ok(_), Ok(_)) => {}
        (Err(a), Err(b)) => assert_eq!(a.wire_code(), b.wire_code()),
        other => panic!("generated UNIQUE must behave like a plain column: {other:?}"),
    }
}

/// `ADD COLUMN ... GENERATED` の評価エラーが他テナントの行に起因しても、文言に ID・値を含めない。
#[test]
fn add_column_error_from_other_tenants_rows_is_opaque() {
    let (_gd, core) = fixture("gen-l-add");
    mk(&core, TA, "t");
    ok(
        &core,
        TB,
        "INSERT INTO t (k, a, name) VALUES (1, 0, 'boomB')",
    );
    ok(
        &core,
        TC,
        "INSERT INTO t (k, a, name) VALUES (1, 0, 'boomC')",
    );
    let sql = "ALTER TABLE t ADD COLUMN q INTEGER GENERATED ALWAYS AS (10 / a) STORED";
    let before = (dump(&core, TB, "t"), dump(&core, TC, "t"));
    let m = message(&core, TA, sql);
    for bad in [TA, TB, TC, "boomB", "boomC"] {
        assert!(!m.contains(bad), "{m}");
    }
    assert_eq!((dump(&core, TB, "t"), dump(&core, TC, "t")), before);
}
