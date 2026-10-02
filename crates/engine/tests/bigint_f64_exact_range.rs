//! `f64` で正確に表せない整数値（`|v| > 2^53`）の式評価での拒否が、SQL 表層の
//! 各経路で一貫して `22003`（`NumericOutOfRange`）になることの結合テスト
//! （Issue #1336・ポインタ: `docs/spec` TABLE-16・ERR-2）。拒否するという判定
//! （fail-closed）は不変で、分類だけを `22000` から `22003` へ是正した。
//!
//! 対象経路: CHECK 付き INSERT／UPDATE・`ADD COLUMN DEFAULT` した列の後段評価・
//! WHERE 式の列参照・WHERE の整数リテラル・行 `id` 比較。CHECK も式も無い
//! BIGINT 列は `i64` 全域を引き続き受理する（回帰防止）。RLS: 他テナントの
//! 行の値で自テナントの評価が失敗しない（存在オラクルにならない）ことも固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const EXACT: i64 = 1 << 53;
/// 境界値の集合（±2^53 は成功側、それ以外は拒否側）。
const OK_VALUES: [i64; 2] = [EXACT, -EXACT];
const BAD_VALUES: [i64; 4] = [EXACT + 1, -EXACT - 1, i64::MAX, i64::MIN];

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, c: &PolicyContext, sql: &str) -> Result<SqlOutcome, String> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(c, &mut s, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn count(core: &EngineCore, c: &PolicyContext, sql: &str) -> usize {
    match run(core, c, sql).expect("select ok") {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn insert(id: u64, v: i64, op: &str) -> String {
    format!("INSERT INTO t (id, b) VALUES ({id}, {v}) USING OPERATION_ID '{op}'")
}

#[test]
fn check_insert_rejects_beyond_exact_range_with_22003_and_no_side_effect() {
    let (core, path) = new_core("bigint-f64-check-insert");
    let _g = CleanupGuard(path);
    let a = ctx("alice");
    run(&core, &a, "CREATE TABLE t (b BIGINT CHECK (b = b))").expect("ddl");
    for (i, v) in OK_VALUES.iter().enumerate() {
        run(&core, &a, &insert(i as u64 + 1, *v, &format!("ok-{i}"))).expect("exact range ok");
    }
    for (i, v) in BAD_VALUES.iter().enumerate() {
        let err = run(&core, &a, &insert(100 + i as u64, *v, &format!("bad-{i}"))).unwrap_err();
        assert_eq!(err, "22003", "v={v}");
    }
    assert_eq!(
        count(&core, &a, "SELECT id FROM t LIMIT 100"),
        OK_VALUES.len()
    );
}

#[test]
fn check_update_rejects_beyond_exact_range_with_22003() {
    let (core, path) = new_core("bigint-f64-check-update");
    let _g = CleanupGuard(path);
    let a = ctx("alice");
    run(&core, &a, "CREATE TABLE t (b BIGINT CHECK (b = b))").expect("ddl");
    run(&core, &a, &insert(1, 1, "seed")).expect("seed");
    for (i, v) in OK_VALUES.iter().enumerate() {
        run(
            &core,
            &a,
            &format!("UPDATE t SET b = {v} WHERE id = 1 USING OPERATION_ID 'u-ok-{i}'"),
        )
        .expect("exact range ok");
    }
    for (i, v) in BAD_VALUES.iter().enumerate() {
        let err = run(
            &core,
            &a,
            &format!("UPDATE t SET b = {v} WHERE id = 1 USING OPERATION_ID 'u-bad-{i}'"),
        )
        .unwrap_err();
        assert_eq!(err, "22003", "v={v}");
    }
}

#[test]
fn add_column_default_binds_full_i64_but_later_check_evaluation_is_22003() {
    let a = ctx("alice");
    for v in OK_VALUES.iter().chain(BAD_VALUES.iter()) {
        let (core, path) = new_core("bigint-f64-add-default");
        let _g = CleanupGuard(path);
        run(&core, &a, "CREATE TABLE t (b BIGINT)").expect("ddl");
        run(&core, &a, &insert(1, 1, "seed")).expect("seed");
        // DEFAULT の束縛自体は i64 全域を受理する（挙動不変）。
        run(
            &core,
            &a,
            &format!("ALTER TABLE t ADD COLUMN c BIGINT DEFAULT {v}"),
        )
        .expect("default binds");
        let res = run(&core, &a, "ALTER TABLE t ADD CHECK (c = c)");
        if OK_VALUES.contains(v) {
            res.expect("exact range ok");
        } else {
            assert_eq!(res.unwrap_err(), "22003", "v={v}");
        }
    }
}

#[test]
fn where_expression_over_stored_bigint_is_22003_beyond_exact_range_only() {
    let (core, path) = new_core("bigint-f64-where-col");
    let _g = CleanupGuard(path);
    let a = ctx("alice");
    run(&core, &a, "CREATE TABLE t (b BIGINT)").expect("ddl");
    // CHECK も式も無い列は i64 全域を受理する（回帰防止）。
    for (i, v) in OK_VALUES.iter().enumerate() {
        run(&core, &a, &insert(i as u64 + 1, *v, &format!("ok-{i}"))).expect("insert");
    }
    assert_eq!(
        count(
            &core,
            &a,
            "SELECT id FROM t WHERE b + 0 <= 9007199254740992 LIMIT 100"
        ),
        2
    );
    for (i, v) in [i64::MAX, i64::MIN, EXACT + 1].iter().enumerate() {
        run(&core, &a, &insert(10 + i as u64, *v, &format!("big-{i}"))).expect("insert i64");
    }
    let err = run(
        &core,
        &a,
        "SELECT id FROM t WHERE b + 0 <= 9007199254740992 LIMIT 100",
    )
    .unwrap_err();
    assert_eq!(err, "22003");
}

#[test]
fn where_integer_literal_beyond_exact_range_is_22003() {
    let (core, path) = new_core("bigint-f64-where-lit");
    let _g = CleanupGuard(path);
    let a = ctx("alice");
    run(&core, &a, "CREATE TABLE t (b BIGINT)").expect("ddl");
    run(&core, &a, &insert(1, 1, "seed")).expect("seed");
    assert_eq!(
        count(
            &core,
            &a,
            &format!("SELECT id FROM t WHERE b + 0 = {EXACT} LIMIT 100")
        ),
        0
    );
    for lit in ["9007199254740993", "18446744073709551616"] {
        for col in ["b + 0", "id + 0"] {
            let err = run(
                &core,
                &a,
                &format!("SELECT id FROM t WHERE {col} = {lit} LIMIT 100"),
            )
            .unwrap_err();
            assert_eq!(err, "22003", "{col} {lit}");
        }
    }
}

#[test]
fn other_tenant_value_beyond_exact_range_does_not_fail_own_evaluation() {
    let (core, path) = new_core("bigint-f64-rls");
    let _g = CleanupGuard(path);
    let a = ctx("alice");
    let b = ctx("bob");
    run(&core, &a, "CREATE TABLE t (b BIGINT)").expect("ddl");
    run(&core, &a, &insert(1, 1, "a-1")).expect("alice row");
    run(&core, &b, &insert(2, EXACT + 1, "b-1")).expect("bob row");
    // bob の値は alice の式評価に到達しない（存在オラクルにならない）。
    assert_eq!(
        count(&core, &a, "SELECT id FROM t WHERE b + 0 > 0 LIMIT 100"),
        1
    );
    // 陽性対照: 自テナントの行なら 22003。
    let err = run(&core, &b, "SELECT id FROM t WHERE b + 0 > 0 LIMIT 100").unwrap_err();
    assert_eq!(err, "22003");
}
