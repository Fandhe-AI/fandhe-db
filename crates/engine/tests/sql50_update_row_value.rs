//! `UPDATE ... SET (a, b) = (...)`（行値の代入。SQL-50 (3)、Issue #1747）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql.md` SQL-50・SQL-45、`recovery.md` RECOVER-10・
//! RECOVER-11、`rls.md` RLS-9・RLS-10。設計記録は `docs/design/update-single-row.md`
//! 「Issue #1747 追記」。
//!
//! 実 `Storage` の production 経路で、(1) 列ごとの SET と同じ結果（id 指定形・述語形・
//! 更新前の値参照・ROW・DEFAULT 要素・RETURNING）、(2) 個数不一致などの fail-closed 拒否と
//! 副作用ゼロ、(3) 同一 `OPERATION_ID` の再送（展開後の形で内容照合）、(4) テナント境界を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut session, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn ok(core: &EngineCore, sql: &str) -> SqlOutcome {
    ok_as(core, "owner", sql)
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "owner", sql) {
        Ok(o) => panic!("statement must fail: {sql} -> {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Update(o) => o.rows_affected,
        other => panic!("expected Update outcome, got {other:?}"),
    }
}

fn rows_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match ok_as(core, tenant, sql) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn returning_of(out: &SqlOutcome) -> (Vec<String>, Vec<Vec<Cell>>) {
    match out {
        SqlOutcome::Returning(r) => (
            r.result.columns.iter().map(|c| format!("{c:?}")).collect(),
            r.result.rows.iter().map(|row| row.cells.clone()).collect(),
        ),
        other => panic!("expected RETURNING, got {other:?}"),
    }
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn int(v: i64) -> Cell {
    Cell::SignedInteger(v)
}

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    ok(
        &core,
        "CREATE TABLE t (a TEXT, c TEXT DEFAULT 'dflt', n INTEGER DEFAULT 7, o TEXT)",
    );
    for id in 1..=3 {
        ok(
            &core,
            &format!(
                "INSERT INTO t (id, a, c, n, o) VALUES ({id}, 'a{id}', 'c{id}', {id}, 'o{id}') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    (core, CleanupGuard(path))
}

fn t_rows(core: &EngineCore) -> Vec<Vec<Cell>> {
    rows_as(
        core,
        "owner",
        "SELECT a, c, n, o FROM t ORDER BY id LIMIT 100",
    )
}

/// 行値の形と列ごとの形は、id 指定形・述語形・WHERE 省略・PARTITIONED で同じ結果になる。
#[test]
fn row_value_matches_per_column_form_in_all_where_forms() {
    let (core, _g) = setup("sql50-row-forms");
    let (core2, _g2) = setup("sql50-row-forms-ref");
    let pairs = [
        (
            "UPDATE t SET (a, n) = ('x', 10) WHERE id = 1 USING OPERATION_ID 'r1'",
            "UPDATE t SET a = 'x', n = 10 WHERE id = 1 USING OPERATION_ID 'r1'",
            1,
        ),
        (
            "UPDATE t SET (a, n) = ('y', 20) WHERE id > 1 USING OPERATION_ID 'r2'",
            "UPDATE t SET a = 'y', n = 20 WHERE id > 1 USING OPERATION_ID 'r2'",
            2,
        ),
        (
            "UPDATE t SET (o, c) = ('p', 'q') USING OPERATION_ID 'r3'",
            "UPDATE t SET o = 'p', c = 'q' USING OPERATION_ID 'r3'",
            3,
        ),
        (
            "UPDATE t SET (o, c) = ('z', 'w') WHERE id > 0 USING OPERATION_ID 'r4' PARTITIONED",
            "UPDATE t SET o = 'z', c = 'w' WHERE id > 0 USING OPERATION_ID 'r4' PARTITIONED",
            3,
        ),
    ];
    for (row_sql, col_sql, n) in pairs {
        assert_eq!(affected(ok(&core, row_sql)), n, "{row_sql}");
        assert_eq!(affected(ok(&core2, col_sql)), n, "{col_sql}");
        assert_eq!(t_rows(&core), t_rows(&core2), "{row_sql}");
    }
}

/// 要素は更新前の値で評価される（入れ替えが成立する）。ROW・1 列 ROW・混在・DEFAULT 要素も受理。
#[test]
fn row_value_uses_old_values_and_accepts_row_default_and_mixed() {
    let (core, _g) = setup("sql50-row-old");
    ok(
        &core,
        "UPDATE t SET (a, o) = (o, a) WHERE id = 1 USING OPERATION_ID 's1'",
    );
    assert_eq!(
        t_rows(&core)[0],
        vec![text("o1"), text("c1"), int(1), text("a1")]
    );
    ok(
        &core,
        "UPDATE t SET (n, o) = ROW(n + 1, upper(a)) WHERE id = 2 USING OPERATION_ID 's2'",
    );
    assert_eq!(
        t_rows(&core)[1],
        vec![text("a2"), text("c2"), int(3), text("A2")]
    );
    ok(
        &core,
        "UPDATE t SET (n) = ROW(5) WHERE id = 3 USING OPERATION_ID 's3'",
    );
    assert_eq!(t_rows(&core)[2][2], int(5));
    ok(
        &core,
        "UPDATE t SET a = 'm', (n, o) = (3, 'z') WHERE id = 3 USING OPERATION_ID 's4'",
    );
    assert_eq!(
        t_rows(&core)[2],
        vec![text("m"), text("c3"), int(3), text("z")]
    );
    ok(
        &core,
        "UPDATE t SET (c, n, o) = (DEFAULT, DEFAULT, DEFAULT) WHERE id = 1 \
         USING OPERATION_ID 's5'",
    );
    assert_eq!(
        t_rows(&core)[0],
        vec![text("o1"), text("dflt"), int(7), Cell::Null]
    );
}

/// RETURNING の列名・行が列ごとの形と同じ。
#[test]
fn row_value_returning_matches_per_column_form() {
    let (core, _g) = setup("sql50-row-ret");
    let a = ok(
        &core,
        "UPDATE t SET (a, n) = ('x', 9) WHERE id = 1 RETURNING a, n USING OPERATION_ID 'q1'",
    );
    let b = ok(
        &core,
        "UPDATE t SET a = 'x', n = 9 WHERE id = 1 RETURNING a, n USING OPERATION_ID 'q2'",
    );
    // 2 回目は更新前の値が異なるため、列名と行（更新後の値）のみ比較する。
    assert_eq!(returning_of(&a), returning_of(&b));
}

/// 拒否系はいずれも副作用ゼロ。個数不一致・括弧のみ 1 列は `42601`、部分クエリは `0A000`。
#[test]
fn row_value_rejections_are_fail_closed_without_side_effects() {
    let (core, _g) = setup("sql50-row-rej");
    let before = t_rows(&core);
    let cases = [
        ("(a, n) = ('x')", "42601"),
        ("(a, n) = ('x', 1, 2)", "42601"),
        ("(a, n) = ROW()", "42601"),
        ("(a) = ('x')", "42601"),
        ("() = ()", "42601"),
        ("(a, n) = 5", "42601"),
        ("(a, n) = (SELECT 'x', 1)", "0A000"),
        ("(a, n) = (VALUES ('x', 1))", "0A000"),
        ("(a, n) = ($1, $2)", "42601"),
        ("(a, zz) = ('x', 1)", "42703"),
    ];
    for (set, want) in cases {
        let sql = format!("UPDATE t SET {set} WHERE id = 1 USING OPERATION_ID 'rej'");
        assert_eq!(code(&core, &sql), want, "{set}");
    }
    // 型不一致・重複列・保護列は列ごとの形と同じコード。
    let same = [
        ("(a, n) = ('x', 'notint')", "a = 'x', n = 'notint'"),
        ("(a, a) = ('x', 'y')", "a = 'x', a = 'y'"),
        ("(tenant_id) = ROW('x')", "tenant_id = 'x'"),
    ];
    for (row_set, col_set) in same {
        let r = code(
            &core,
            &format!("UPDATE t SET {row_set} WHERE id = 1 USING OPERATION_ID 'rej'"),
        );
        let c = code(
            &core,
            &format!("UPDATE t SET {col_set} WHERE id = 1 USING OPERATION_ID 'rej'"),
        );
        assert_eq!(r, c, "{row_set}");
    }
    assert_eq!(t_rows(&core), before);
}

/// 再送: 行値の形と展開後の列ごとの形は同じ内容で `23505`、値違いは `22023`。
#[test]
fn row_value_resend_uses_expanded_form_for_content_check() {
    let (core, _g) = setup("sql50-row-resend");
    ok(
        &core,
        "UPDATE t SET (a, n) = ('x', 1) WHERE id = 1 USING OPERATION_ID 'rs'",
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET a = 'x', n = 1 WHERE id = 1 USING OPERATION_ID 'rs'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET (a, n) = ('x', 1) WHERE id = 1 USING OPERATION_ID 'rs'"
        ),
        "23505"
    );
    // 順序違いの扱いは列ごとの形と同じ（展開後の形で照合するため差が出ない）。
    assert_eq!(
        code(
            &core,
            "UPDATE t SET (n, a) = (1, 'x') WHERE id = 1 USING OPERATION_ID 'rs'"
        ),
        code(
            &core,
            "UPDATE t SET n = 1, a = 'x' WHERE id = 1 USING OPERATION_ID 'rs'"
        ),
    );
    // 値が異なれば内容違いで `22023`。
    assert_eq!(
        code(
            &core,
            "UPDATE t SET (a, n) = ('x', 2) WHERE id = 1 USING OPERATION_ID 'rs'"
        ),
        "22023"
    );
}

/// 他テナントの行は更新されず、応答は列ごとの形と同じ（存在を推測できない）。
#[test]
fn row_value_respects_tenant_boundary() {
    let (core, _g) = setup("sql50-row-tenant");
    let row_out = run(
        &core,
        "other",
        "UPDATE t SET (a, n) = ('x', 10) WHERE id = 1 USING OPERATION_ID 'tn1'",
    );
    let col_out = run(
        &core,
        "other",
        "UPDATE t SET a = 'x', n = 10 WHERE id = 1 USING OPERATION_ID 'tn2'",
    );
    match (row_out, col_out) {
        (Ok(a), Ok(b)) => assert_eq!(affected(a), affected(b)),
        (Err(a), Err(b)) => assert_eq!(a.wire_code(), b.wire_code()),
        (a, b) => panic!("tenant responses differ: {a:?} vs {b:?}"),
    }
    assert_eq!(
        t_rows(&core)[0],
        vec![text("a1"), text("c1"), int(1), text("o1")]
    );
}
