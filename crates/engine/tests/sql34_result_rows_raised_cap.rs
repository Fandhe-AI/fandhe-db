//! 結果行数上限を既定（10,000）より上げた場合の境界テスト（SQL-34。Issue #2211）。
//!
//! `sql34_limit_omitted.rs` は cap を小さく下げた側の境界を固定する。本ファイルは
//! `--max-result-rows` を上げた構成（`EngineCore::with_result_limits(ResultLimits::new(RAISED))`）で
//! 「cap ちょうどは成功、cap + 1 は `54000`」が 10,000 を超えても成り立つことを確かめる。
//! 集合演算の枝（`MAX_SET_OP_ROWS`＝10,000 固定）は別軸の上限で本フラグとは連動しない
//! （docs/design/result-limits-cli.md D7）。上げても先にそちらで拒否される現行挙動もここで固定する。

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

/// 既定の 10,000 を超える cap（実行時間を抑えるため最小限の上げ幅）。
const RAISED: usize = 10_001;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// alice の `t` に `RAISED + 1` 行、bob の `t` に 50 行を入れた engine を返す。
fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(RAISED));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER)",
    )
    .expect("ddl");
    seed(&core, "alice", 1..=(RAISED as i64 + 1));
    seed(&core, "bob", 100_001..=100_050);
    (core, CleanupGuard(path))
}

/// 1 文あたりの挿入行数上限（バッチ 64 件の制約）を超えないよう分割して投入する。
fn seed(core: &EngineCore, tenant: &str, ids: std::ops::RangeInclusive<i64>) {
    let all: Vec<i64> = ids.collect();
    for chunk in all.chunks(60) {
        let values: Vec<String> = chunk.iter().map(|i| format!("({i}, {i})")).collect();
        core.execute_insert_sql(
            &ctx(tenant),
            &format!("INSERT INTO t (id, k) VALUES {}", values.join(", ")),
        )
        .expect("seed");
    }
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

/// 上げた cap でも、省略・`LIMIT ALL` は cap ちょうどで成功し、cap + 1 で `54000`。
/// bob の行は件数に影響しない（RLS-7）。
#[test]
fn raised_cap_boundary_for_omitted_and_all() {
    let (core, _g) = open("sql34-raised");
    let at_cap = format!("SELECT id FROM t WHERE id <= {RAISED}");
    assert_eq!(rows(&core, &at_cap), RAISED);
    assert_eq!(rows(&core, &format!("{at_cap} LIMIT ALL")), RAISED);
    for sql in [
        "SELECT id FROM t",
        "SELECT id FROM t LIMIT ALL",
        "WITH c AS (SELECT id FROM t) SELECT * FROM c",
    ] {
        assert_eq!(code(&core, sql), "54000", "{sql}");
    }
}

/// 明示 `LIMIT n` は設定値まで受理し、超えれば従来どおり `22000`。
#[test]
fn raised_cap_explicit_limit_range() {
    let (core, _g) = open("sql34-raised-explicit");
    assert_eq!(
        rows(&core, &format!("SELECT id FROM t LIMIT {RAISED}")),
        RAISED
    );
    assert_eq!(
        code(&core, &format!("SELECT id FROM t LIMIT {}", RAISED + 1)),
        "22000"
    );
}

/// 別軸の固定上限: 集合演算の枝は cap を上げても 10,000 超で拒否される（本フラグと非連動）。
#[test]
fn set_operation_branch_limit_is_independent_of_the_flag() {
    let (core, _g) = open("sql34-raised-setop");
    let sql =
        format!("SELECT id FROM t WHERE id <= {RAISED} UNION ALL SELECT id FROM t WHERE id <= 1");
    assert_eq!(code(&core, &sql), "54000");
    // 全体件数も 10,000 固定（cap が 10,001 でも 10,001 件の結果は拒否される）。
    let total = "SELECT id FROM t WHERE id <= 5001 UNION ALL SELECT id FROM t WHERE id <= 5000";
    assert_eq!(code(&core, total), "54000");
    let ok = "SELECT id FROM t WHERE id <= 5000 UNION ALL SELECT id FROM t WHERE id <= 5000";
    assert_eq!(rows(&core, ok), 10_000);
}

/// ハード上限（100,000）: 取得上限 `cap + 1` が下流の固定上限（走査・可視行数）に先に当たらず、
/// ちょうどは成功し +1 は `54000` になる（limit-omitted-result-cap.md D1 の申し送りの確認）。
#[test]
fn hard_cap_boundary_is_not_masked_by_downstream_limits() {
    let hard = engine::result_limits::MAX_RESULT_ROWS_HARD_CAP;
    let path = unique_db_path("sql34-hard-cap");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(hard));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER)",
    )
    .expect("ddl");
    seed(&core, "alice", 1..=(hard as i64 + 1));
    let at_cap = format!("SELECT id FROM t WHERE id <= {hard}");
    assert_eq!(rows(&core, &at_cap), hard);
    assert_eq!(rows(&core, &format!("{at_cap} ORDER BY id DESC")), hard);
    assert_eq!(code(&core, "SELECT id FROM t"), "54000");
    assert_eq!(code(&core, "SELECT id FROM t ORDER BY id DESC"), "54000");
    assert_eq!(code(&core, "SELECT id FROM t LIMIT ALL"), "54000");
}
