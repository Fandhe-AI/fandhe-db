//! 結果行数上限の超過確定時の早期中断（SQL-34。Issue #1797）の結合テスト。
//!
//! `LIMIT` 省略・`LIMIT ALL` の文は、OFFSET 適用後の `cap + 1` 件目が確定した時点で走査・結合・
//! 集約を中断し `54000` を返す（部分結果は返さない）。中断の有無は「物理順で先頭側に `cap + 1`
//! 件の一致行、その後ろに WHERE の式評価で `22012`（ゼロ除算）を起こす行」を置いて判定する:
//! 早期中断していれば `54000`、走査を続ければ `22012` になる。HAVING 付き GROUP BY は
//! 完了後判定（早期確定できない）のままであることも固定する。wire の分割送出・キャンセルは #1798。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const CAP: usize = 3;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(CAP));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER, v INTEGER)",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, k INTEGER, w INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    (core, CleanupGuard(path))
}

/// `t` に `ids` の行を入れる（`k = id`・`v = id * 10`。`k` は全行で異なる）。
fn seed_t(core: &EngineCore, tenant: &str, ids: std::ops::RangeInclusive<i64>) {
    let values: Vec<String> = ids.map(|i| format!("({i}, {i}, {})", i * 10)).collect();
    core.execute_insert_sql(
        &ctx(tenant),
        &format!("INSERT INTO t (id, k, v) VALUES {}", values.join(", ")),
    )
    .expect("seed t");
}

fn seed_u(core: &EngineCore, tenant: &str, ids: std::ops::RangeInclusive<i64>) {
    let values: Vec<String> = ids.map(|i| format!("({i}, {i}, {})", i * 100)).collect();
    core.execute_insert_sql(
        &ctx(tenant),
        &format!("INSERT INTO u (id, k, w) VALUES {}", values.join(", ")),
    )
    .expect("seed u");
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("alice"), &mut SessionState::default(), sql)
}

fn rows(core: &EngineCore, sql: &str) -> usize {
    match run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, sql) {
        Ok(o) => panic!("expected error for {sql}, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// `WHERE 100 / k > 0`: `k = 0` の行に到達すると `22012` で失敗する。
const WHERE_DIV: &str = "WHERE 100 / k > 0";

/// `t` に ids 1..=cap+1 の一致行（物理順で先頭）と、末尾に `k = 0` の毒行 (id 100) を入れる。
fn seed_poisoned(core: &EngineCore) {
    seed_t(core, "alice", 1..=(CAP as i64 + 1));
    core.execute_insert_sql(&ctx("alice"), "INSERT INTO t (id, k, v) VALUES (100, 0, 0)")
        .expect("seed poison");
}

/// 経路 (B)・ウィンドウ・GROUP BY・DISTINCT は `cap + 1` 件目の確定で止まり、毒行へ到達しない。
#[test]
fn materializing_paths_stop_before_reaching_the_poison_row() {
    let (core, _g) = open("sql1797-stop");
    seed_poisoned(&core);
    for sql in [
        format!("SELECT * FROM t {WHERE_DIV} ORDER BY v"),
        format!("SELECT * FROM t {WHERE_DIV} ORDER BY v DESC LIMIT ALL"),
        format!("SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS rn FROM t {WHERE_DIV}"),
        format!("SELECT k, SUM(v) AS s FROM t {WHERE_DIV} GROUP BY k"),
        format!("SELECT DISTINCT k FROM t {WHERE_DIV}"),
        format!("SELECT * FROM t {WHERE_DIV}"),
        format!("SELECT * FROM t {WHERE_DIV} ORDER BY id"),
    ] {
        assert_eq!(code(&core, &sql), "54000", "{sql}");
    }
}

/// HAVING 付き GROUP BY は早期確定できず、走査を最後まで続ける（毒行で `22012`）。
#[test]
fn having_group_by_is_judged_after_completion() {
    let (core, _g) = open("sql1797-having");
    seed_poisoned(&core);
    assert_eq!(
        code(
            &core,
            &format!("SELECT k, SUM(v) AS s FROM t {WHERE_DIV} GROUP BY k HAVING s >= 0")
        ),
        "22012"
    );
}

/// OFFSET を含む閾値 `offset + cap`: cap + OFFSET 件までは成功し、超えた時点で `54000`。
#[test]
fn threshold_includes_the_offset() {
    let (core, _g) = open("sql1797-offset");
    seed_t(&core, "alice", 1..=(CAP as i64 + 2));
    for q in [
        "SELECT * FROM t ORDER BY v OFFSET 2",
        "SELECT * FROM t OFFSET 2",
        "SELECT k, SUM(v) AS s FROM t GROUP BY k OFFSET 2",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS rn FROM t OFFSET 2",
    ] {
        assert_eq!(rows(&core, q), CAP, "{q}");
    }
    for q in [
        "SELECT * FROM t ORDER BY v OFFSET 1",
        "SELECT * FROM t OFFSET 1",
        "SELECT k, SUM(v) AS s FROM t GROUP BY k OFFSET 1",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS rn FROM t OFFSET 1",
    ] {
        assert_eq!(code(&core, q), "54000", "{q}");
    }
}

/// 不可視（他テナント）行は計数されない: 大量の他テナント行があっても自テナント cap 件は成功。
#[test]
fn early_stop_does_not_count_invisible_rows() {
    let (core, _g) = open("sql1797-rls");
    seed_t(&core, "bob", 1000..=1050);
    seed_t(&core, "alice", 1..=(CAP as i64));
    for q in [
        "SELECT * FROM t",
        "SELECT * FROM t ORDER BY v",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS rn FROM t",
        "SELECT k, SUM(v) AS s FROM t GROUP BY k",
        "SELECT DISTINCT k FROM t",
    ] {
        assert_eq!(rows(&core, q), CAP, "{q}");
    }
}

/// JOIN: 残余述語なしの最終段停止とソート前判定。cap ちょうどは成功、cap + 1 で `54000`。
#[test]
fn join_stops_at_the_threshold() {
    let (core, _g) = open("sql1797-join");
    seed_t(&core, "alice", 1..=(CAP as i64));
    seed_u(&core, "alice", 1..=(CAP as i64));
    let plain = "SELECT t.id, u.w FROM t JOIN u ON t.k = u.k";
    assert_eq!(rows(&core, plain), CAP);
    assert_eq!(rows(&core, &format!("{plain} OFFSET 1")), CAP - 1);
    seed_t(&core, "alice", 4..=4);
    seed_u(&core, "alice", 4..=4);
    assert_eq!(code(&core, plain), "54000");
    assert_eq!(code(&core, &format!("{plain} OFFSET 0")), "54000");
    assert_eq!(rows(&core, &format!("{plain} OFFSET 1")), CAP);
    // 残余述語ありは完了後の件数で判定する（4 ペア成立しても述語で 0 件になれば成功）。
    assert_eq!(rows(&core, &format!("{plain} WHERE t.v = u.w")), 0);
}

/// カーソルの内側 SELECT も同じ早期中断を共有する（毒行に到達せず `54000`）。
#[test]
fn cursor_inner_query_shares_the_early_stop() {
    let (core, _g) = open("sql1797-cursor");
    seed_poisoned(&core);
    let caller = ctx("alice");
    for inner in [
        format!("SELECT id FROM t {WHERE_DIV} ORDER BY v"),
        format!("SELECT k, SUM(v) AS s FROM t {WHERE_DIV} GROUP BY k"),
    ] {
        // 失敗したトランザクションは Failed になるため、文ごとに新しいセッションで試す。
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("DECLARE c CURSOR FOR {inner}"),
            )
            .expect_err("declare over cap");
        assert_eq!(err.wire_code(), "54000", "{inner}");
    }
}
