//! サブクエリ内側の `LIMIT` 省略・`LIMIT ALL` の受理と結果行数上限（SQL-29・SQL-34。
//! Issue #1853、親 #1852）の結合テスト。
//!
//! 契約: 内側の `LIMIT` を省略できる。`IN`／`NOT IN` の内側は結果行数上限
//! （`EngineCore::with_result_limits`。CLI の `--max-result-rows` 相当）+ 1 件で取得を止め、
//! 超過は黙って切り詰めず `54000` で文全体を拒否する。`EXISTS` は 1 件、スカラー用途は
//! 2 件（`21000` 判定用）で有界のため `54000` にならない。上限は設定値に追随し、
//! 他テナントの行は数えない（RLS 相当は外側と同一）。未対応の形は従来どおり `42601`。
//! `sql34_limit_omitted.rs`（外側の省略）と同じハーネスを使う。

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

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str, cap: usize) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(cap));
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

/// `t` に `ids` の行を入れる（`k = id`）。
fn seed_t(core: &EngineCore, tenant: &str, ids: std::ops::RangeInclusive<i64>) {
    let values: Vec<String> = ids.map(|i| format!("({i}, {i}, {})", i * 10)).collect();
    core.execute_insert_sql(
        &ctx(tenant),
        &format!("INSERT INTO t (id, k, v) VALUES {}", values.join(", ")),
    )
    .expect("seed t");
}

/// `u` に `ids` の行を入れる（`k = id`）。
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

/// `IN`／`NOT IN` の内側省略・`ALL`: 内側がちょうど cap 件で成功（オラクル一致）、
/// cap + 1 件で `54000`。cap を 3 と 5 に変えて境界が設定値に追随することを固定する。
#[test]
fn in_and_not_in_follow_the_configured_cap() {
    for cap in [3usize, 5] {
        let (core, _g) = open(&format!("sql34-sub-in-{cap}"), cap);
        let cap_i = i64::try_from(cap).expect("small");
        // t は 1..=cap+2、u は 1..=cap（k = id）。
        seed_t(&core, "alice", 1..=cap_i + 2);
        seed_u(&core, "alice", 1..=cap_i);
        for inner in ["SELECT k FROM u", "SELECT k FROM u LIMIT ALL"] {
            let in_sql = format!("SELECT id FROM t WHERE k IN ({inner}) LIMIT {cap}");
            assert_eq!(rows(&core, &in_sql), cap, "cap={cap} {in_sql}");
            let not_in = format!("SELECT id FROM t WHERE k NOT IN ({inner})");
            assert_eq!(rows(&core, &not_in), 2, "cap={cap} {not_in}");
        }
        seed_u(&core, "alice", cap_i + 1..=cap_i + 1);
        for inner in ["SELECT k FROM u", "SELECT k FROM u LIMIT ALL"] {
            let in_sql = format!("SELECT id FROM t WHERE k IN ({inner}) LIMIT {cap}");
            assert_eq!(code(&core, &in_sql), "54000", "cap={cap} {in_sql}");
            let not_in = format!("SELECT id FROM t WHERE k NOT IN ({inner})");
            assert_eq!(code(&core, &not_in), "54000", "cap={cap} {not_in}");
        }
    }
}

/// 同じ 4 行のデータが、cap = 3 では `54000`、cap = 5 では成功する。
#[test]
fn same_data_flips_with_max_result_rows() {
    let sql = "SELECT id FROM t WHERE k IN (SELECT k FROM u) LIMIT 3";
    let (small, _g1) = open("sql34-sub-flip-3", 3);
    seed_t(&small, "alice", 1..=4);
    seed_u(&small, "alice", 1..=4);
    assert_eq!(code(&small, sql), "54000");
    let (large, _g2) = open("sql34-sub-flip-5", 5);
    seed_t(&large, "alice", 1..=4);
    seed_u(&large, "alice", 1..=4);
    assert_eq!(rows(&large, sql), 3);
}

/// 集計の外側でも同じ境界になる。
#[test]
fn aggregate_outer_shares_the_boundary() {
    let (core, _g) = open("sql34-sub-agg", 3);
    seed_t(&core, "alice", 1..=5);
    seed_u(&core, "alice", 1..=3);
    assert_eq!(
        rows(&core, "SELECT COUNT(*) FROM t WHERE k IN (SELECT k FROM u)"),
        1
    );
    seed_u(&core, "alice", 4..=4);
    assert_eq!(
        code(&core, "SELECT COUNT(*) FROM t WHERE k IN (SELECT k FROM u)"),
        "54000"
    );
}

/// `EXISTS`／`NOT EXISTS` は内側が cap を超えても成功し、真偽は可視行の有無で決まる。
#[test]
fn exists_is_bounded_and_ignores_the_cap() {
    let (core, _g) = open("sql34-sub-exists", 3);
    seed_t(&core, "alice", 1..=2);
    assert_eq!(
        rows(&core, "SELECT id FROM t WHERE EXISTS (SELECT k FROM u)"),
        0
    );
    assert_eq!(
        rows(&core, "SELECT id FROM t WHERE NOT EXISTS (SELECT k FROM u)"),
        2
    );
    seed_u(&core, "alice", 1..=10);
    assert_eq!(
        rows(&core, "SELECT id FROM t WHERE EXISTS (SELECT k FROM u)"),
        2
    );
    assert_eq!(
        rows(
            &core,
            "SELECT id FROM t WHERE NOT EXISTS (SELECT k FROM u LIMIT ALL)"
        ),
        0
    );
}

/// WHERE のスカラー比較: 0 行は空、1 行は一致、2 行以上は cap を超えても `21000`。
#[test]
fn scalar_compare_is_bounded_and_reports_cardinality() {
    let (core, _g) = open("sql34-sub-scalar", 3);
    seed_t(&core, "alice", 1..=3);
    let sql = "SELECT id FROM t WHERE k = (SELECT k FROM u)";
    assert_eq!(rows(&core, sql), 0);
    seed_u(&core, "alice", 2..=2);
    assert_eq!(rows(&core, sql), 1);
    seed_u(&core, "alice", 3..=3);
    assert_eq!(code(&core, sql), "21000");
    seed_u(&core, "alice", 4..=9);
    assert_eq!(code(&core, sql), "21000");
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE k = (SELECT k FROM u LIMIT ALL)"
        ),
        "21000"
    );
}

/// 投影位置のスカラー: 0 行は NULL、1 行は値、2 行以上は `21000`。
#[test]
fn projection_scalar_is_bounded_and_reports_cardinality() {
    let (core, _g) = open("sql34-sub-proj", 3);
    seed_t(&core, "alice", 1..=2);
    let sql = "SELECT (SELECT k FROM u) FROM t";
    assert_eq!(rows(&core, sql), 2);
    seed_u(&core, "alice", 1..=1);
    assert_eq!(rows(&core, sql), 2);
    seed_u(&core, "alice", 2..=8);
    assert_eq!(code(&core, sql), "21000");
}

/// 他テナントの行は内側の上限に数えない。
#[test]
fn other_tenant_rows_do_not_count_toward_the_inner_cap() {
    let (core, _g) = open("sql34-sub-rls", 3);
    seed_t(&core, "alice", 1..=3);
    seed_u(&core, "alice", 1..=3);
    seed_u(&core, "bob", 101..=120);
    assert_eq!(
        rows(
            &core,
            "SELECT id FROM t WHERE k IN (SELECT k FROM u) LIMIT 3"
        ),
        3
    );
    assert_eq!(
        rows(&core, "SELECT id FROM t WHERE k NOT IN (SELECT k FROM u)"),
        0
    );
}

/// DML の WHERE 内サブクエリも同じ上限。超過時は何も変更しない。
#[test]
fn dml_where_subquery_enforces_the_cap_without_side_effects() {
    let (core, _g) = open("sql34-sub-dml", 3);
    seed_t(&core, "alice", 1..=3);
    seed_u(&core, "alice", 1..=4);
    assert_eq!(
        code(
            &core,
            "DELETE FROM t WHERE k IN (SELECT k FROM u) USING OPERATION_ID 'op-1853-a'"
        ),
        "54000"
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET v = 0 WHERE k IN (SELECT k FROM u) USING OPERATION_ID 'op-1853-b'"
        ),
        "54000"
    );
    assert_eq!(rows(&core, "SELECT id FROM t WHERE v > 0 LIMIT 3"), 3);
}

/// 明示 `LIMIT` の範囲検証も設定値に追随する（cap = 3 で `LIMIT 4` は `22000`）。
#[test]
fn explicit_inner_limit_range_follows_the_configured_cap() {
    let (core, _g) = open("sql34-sub-explicit", 3);
    seed_t(&core, "alice", 1..=2);
    seed_u(&core, "alice", 1..=2);
    assert_eq!(
        rows(
            &core,
            "SELECT id FROM t WHERE k IN (SELECT k FROM u LIMIT 3)"
        ),
        2
    );
    for sql in [
        "SELECT id FROM t WHERE k IN (SELECT k FROM u LIMIT 4)",
        "SELECT id FROM t WHERE EXISTS (SELECT k FROM u LIMIT 4)",
        "SELECT id FROM t WHERE k = (SELECT k FROM u LIMIT 4)",
    ] {
        assert_eq!(code(&core, sql), "22000", "{sql}");
    }
}

/// 未対応の形は `LIMIT` 省略でも従来どおり `42601`。
#[test]
fn unsupported_inner_forms_stay_rejected() {
    let (core, _g) = open("sql34-sub-unsupported", 3);
    seed_t(&core, "alice", 1..=2);
    seed_u(&core, "alice", 1..=2);
    for sql in [
        "SELECT id FROM t WHERE k IN (SELECT ROW_NUMBER() OVER (ORDER BY w) FROM u)",
        "SELECT id FROM t WHERE k IN (SELECT k FROM u UNION SELECT k FROM u)",
        "SELECT id FROM t WHERE k IN (SELECT u.k FROM u JOIN t ON u.k = t.k)",
        // 相関 `IN`（LIMIT なし）は Issue #1844、単一の裸の列の相関スカラーは Issue #1847 で
        // 実行できる。式投影の相関スカラーは引き続き拒否する。
        "SELECT id FROM t WHERE k = (SELECT k + 1 FROM u WHERE w = v)",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

/// cap が最小値（1）でも、省略形スカラーは 0 行・1 行で成功し、2 行以上は `21000`。
/// 基数判定用の 2 件取得は内部プローブであり、利用者の明示 `LIMIT` の範囲検証や
/// 事後の行数ガードで `22000` にならない。
#[test]
fn scalar_with_omitted_limit_works_at_minimum_cap() {
    for (n_inner, label) in [(0i64, "zero"), (1, "one"), (2, "two"), (3, "three")] {
        let (core, _g) = open(&format!("sql34-sub-mincap-{label}"), 1);
        seed_t(&core, "alice", 1..=1);
        if n_inner > 0 {
            seed_u(&core, "alice", 1..=n_inner);
        }
        let projection = "SELECT (SELECT k FROM u) FROM t LIMIT 1";
        let predicate = "SELECT id FROM t WHERE k = (SELECT k FROM u) LIMIT 1";
        if n_inner <= 1 {
            assert_eq!(rows(&core, projection), 1, "{projection} (inner={n_inner})");
            assert_eq!(
                rows(&core, predicate),
                usize::try_from(n_inner).expect("small"),
                "{predicate} (inner={n_inner})"
            );
        } else {
            for sql in [projection, predicate] {
                assert_eq!(code(&core, sql), "21000", "{sql} (inner={n_inner})");
            }
        }
    }
}
