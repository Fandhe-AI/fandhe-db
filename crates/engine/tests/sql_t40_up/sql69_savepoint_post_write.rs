//! 書き込み後の savepoint への `ROLLBACK TO`（Issue #2879・
//! `docs/spec/04-behavior/sql-surface.md` SQL-69 のポインタ。失敗状態からの復帰は SQL-31、台帳は
//! RECOVER-1 のポインタ）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として、redb の API だけでは戻せない
//! 「書き込み済みの writer での savepoint」を、基底への復元 + 内容差分の再適用で戻せることを固定する。
//! 受け入れ条件は PostgreSQL 17 と同じ結果（巻き戻し後の読み取り・COMMIT 後の内容）、一意索引・
//! 台帳・カーソル・書き込み記録との整合、一時表を使った Tx、テナント境界を保つことの 5 点。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn fixture(tag: &str) -> (EngineCore, CleanupGuard, SessionState) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_write_lock_wait(std::time::Duration::from_millis(100));
    let mut s = ddl_session();
    for ddl in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, code TEXT UNIQUE)",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, v BIGINT)",
    ] {
        core.execute_sql_in_session(&ctx("sys"), &mut s, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {e}"));
    }
    (core, guard, s)
}

fn run<'e>(
    core: &'e EngineCore,
    tenant: &str,
    s: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_txn(&ctx(tenant), s, txn, sql)
}

fn ok<'e>(
    core: &'e EngineCore,
    tenant: &str,
    s: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) {
    run(core, tenant, s, txn, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn code<'e>(
    core: &'e EngineCore,
    tenant: &str,
    s: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) -> &'static str {
    run(core, tenant, s, txn, sql)
        .expect_err("expected an error")
        .wire_code()
}

fn pairs(out: SqlOutcome) -> Vec<(i64, i64)> {
    let SqlOutcome::Query(q) = out else {
        panic!("expected Query");
    };
    q.rows
        .iter()
        .map(|r| match r.cells.as_slice() {
            [Cell::SignedInteger(a), Cell::SignedInteger(b)] => (*a, *b),
            other => panic!("unexpected cells {other:?}"),
        })
        .collect()
}

const SEL_T: &str = "SELECT id, v FROM t ORDER BY id";
const SEL_TMP: &str = "SELECT id, v FROM tmp ORDER BY id";

fn in_txn<'e>(
    core: &'e EngineCore,
    tenant: &str,
    s: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) -> Vec<(i64, i64)> {
    pairs(run(core, tenant, s, txn, sql).expect("select"))
}

fn committed(core: &EngineCore, tenant: &str, sql: &str) -> Vec<(i64, i64)> {
    pairs(
        core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql)
            .expect("select"),
    )
}

fn ins(id: i64, v: i64, code: &str, op: &str) -> String {
    format!("INSERT INTO t (id, v, code) VALUES ({id}, {v}, '{code}') USING OPERATION_ID '{op}'")
}

/// 一時表は冪等台帳を持たないため `USING OPERATION_ID` を付けない（`_op` は呼び出し側の識別用）。
fn ins_tmp(id: i64, _op: &str) -> String {
    format!("INSERT INTO tmp (id, v) VALUES ({id}, {id})")
}

fn temp_fixture(tag: &str) -> (EngineCore, CleanupGuard, SessionState) {
    let (core, g, mut s) = fixture(tag);
    core.execute_sql_in_session(
        &ctx("a"),
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, v BIGINT)",
    )
    .unwrap();
    (core, g, s)
}

fn session_tmp(core: &EngineCore, s: &mut SessionState) -> Vec<(i64, i64)> {
    pairs(core.execute_sql_in_session(&ctx("a"), s, SEL_TMP).unwrap())
}

/// 基本形: `INSERT; SAVEPOINT; INSERT; ROLLBACK TO; COMMIT`。
#[test]
fn insert_savepoint_insert_rollback_to_commit() {
    let (core, _g, mut s) = fixture("sql69-pw-basic");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 10, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 20, "c2", "op-2"));
    assert_eq!(
        in_txn(&core, "a", &mut s, &mut txn, SEL_T),
        vec![(1, 10), (2, 20)]
    );
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    assert_eq!(in_txn(&core, "a", &mut s, &mut txn, SEL_T), vec![(1, 10)]);
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 10)]);
}

/// UPDATE・DELETE: savepoint 前の変更は残り、後の変更は元に戻る。
#[test]
fn update_and_delete_after_savepoint_are_undone() {
    let (core, _g, mut s) = fixture("sql69-pw-upd-del");
    let mut seed = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut seed, "BEGIN");
    for (i, c) in [(1, "c1"), (2, "c2"), (3, "c3")] {
        ok(
            &core,
            "a",
            &mut s,
            &mut seed,
            &ins(i, i * 10, c, &format!("seed-{i}")),
        );
    }
    ok(&core, "a", &mut s, &mut seed, "COMMIT");

    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "UPDATE t SET v = 11 WHERE id = 1 USING OPERATION_ID 'u-1'",
    );
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "DELETE FROM t WHERE id = 2 USING OPERATION_ID 'd-2'",
    );
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "UPDATE t SET v = 99 WHERE id = 1 USING OPERATION_ID 'u-1b'",
    );
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "DELETE FROM t WHERE id = 3 USING OPERATION_ID 'd-3'",
    );
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    assert_eq!(
        in_txn(&core, "a", &mut s, &mut txn, SEL_T),
        vec![(1, 11), (3, 30)]
    );
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 11), (3, 30)]);
}

/// 一意索引の整合: 巻き戻した一意キーを同じ Tx 内で再挿入でき、残した一意キーは衝突する。
#[test]
fn unique_index_matches_state_after_rollback_to() {
    let (core, _g, mut s) = fixture("sql69-pw-unique");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 10, "keep", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 20, "gone", "op-2"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    // 巻き戻した code は再利用できる。
    ok(&core, "a", &mut s, &mut txn, &ins(3, 30, "gone", "op-3"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    // 残した code は衝突する（23505）。失敗状態からも書き込み後の savepoint へ戻れる。
    assert_eq!(
        code(&core, "a", &mut s, &mut txn, &ins(4, 40, "keep", "op-4")),
        "23505"
    );
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    ok(&core, "a", &mut s, &mut txn, &ins(5, 50, "gone", "op-5"));
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 10), (5, 50)]);
}

/// 入れ子・反復・RELEASE。
#[test]
fn nested_repeated_and_released_savepoints() {
    let (core, _g, mut s) = fixture("sql69-pw-nested");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT s1");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "c2", "op-2"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT s2");
    ok(&core, "a", &mut s, &mut txn, &ins(3, 3, "c3", "op-3"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO s2");
    assert_eq!(
        in_txn(&core, "a", &mut s, &mut txn, SEL_T),
        vec![(1, 1), (2, 2)]
    );
    // 同じ savepoint へ複数回戻れる。
    ok(&core, "a", &mut s, &mut txn, &ins(4, 4, "c4", "op-4"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO s2");
    assert_eq!(
        in_txn(&core, "a", &mut s, &mut txn, SEL_T),
        vec![(1, 1), (2, 2)]
    );
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO s1");
    assert_eq!(in_txn(&core, "a", &mut s, &mut txn, SEL_T), vec![(1, 1)]);
    // s2 は s1 への巻き戻しで除かれている。
    assert_eq!(
        code(&core, "a", &mut s, &mut txn, "ROLLBACK TO s2"),
        "3B001"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK");

    // RELEASE した後は、その外側の savepoint へ戻せる。
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT outer_sp");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "c2", "op-2"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT inner_sp");
    ok(&core, "a", &mut s, &mut txn, &ins(3, 3, "c3", "op-3"));
    ok(&core, "a", &mut s, &mut txn, "RELEASE inner_sp");
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO outer_sp");
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 1)]);
}

/// 冪等台帳: savepoint 後に登録した `operation_id` は再利用でき、前の ID は `25000` のまま。
#[test]
fn operation_ids_follow_the_savepoint_watermark() {
    let (core, _g, mut s) = fixture("sql69-pw-ledger");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "c2", "op-2"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "c2", "op-2"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    assert_eq!(
        code(&core, "a", &mut s, &mut txn, &ins(9, 9, "c9", "op-1")),
        "25000"
    );
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK");
    assert!(committed(&core, "a", SEL_T).is_empty());
}

/// 書き込み記録: 巻き戻した後の読み取りと COMMIT は、残した書き込みだけを反映する。
#[test]
fn write_marks_follow_the_rollback() {
    let (core, _g, mut s) = fixture("sql69-pw-marks");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "INSERT INTO u (id, v) VALUES (5, 5) USING OPERATION_ID 'u-5'",
    );
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    assert_eq!(in_txn(&core, "a", &mut s, &mut txn, SEL_T), vec![(1, 1)]);
    assert!(in_txn(
        &core,
        "a",
        &mut s,
        &mut txn,
        "SELECT id, v FROM u ORDER BY id"
    )
    .is_empty());
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 1)]);
    assert!(committed(&core, "a", "SELECT id, v FROM u ORDER BY id").is_empty());
}

/// 失敗状態からの復帰（SQL-31 のポインタ）: 書き込み後の savepoint の後で文エラー → `ROLLBACK TO`。
#[test]
fn failed_state_recovers_to_post_write_savepoint() {
    let (core, _g, mut s) = fixture("sql69-pw-failed");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "c2", "op-2"));
    // 一意制約違反で失敗。
    assert_eq!(
        code(&core, "a", &mut s, &mut txn, &ins(3, 3, "c1", "op-3")),
        "23505"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    assert_eq!(in_txn(&core, "a", &mut s, &mut txn, SEL_T), vec![(1, 1)]);
    ok(&core, "a", &mut s, &mut txn, &ins(4, 4, "c4", "op-4"));
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 1), (4, 4)]);
}

/// カーソル: savepoint より前に宣言したカーソルは開いたまま、後に宣言したものは閉じる。
#[test]
fn cursors_follow_the_savepoint() {
    let (core, _g, mut s) = fixture("sql69-pw-cursor");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "DECLARE before CURSOR FOR SELECT id, v FROM t",
    );
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "c2", "op-2"));
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "DECLARE after CURSOR FOR SELECT id, v FROM t",
    );
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    ok(&core, "a", &mut s, &mut txn, "FETCH 10 FROM before");
    assert_eq!(
        code(&core, "a", &mut s, &mut txn, "FETCH 10 FROM after"),
        "34000"
    );
}

/// テナント境界: テナント A の `ROLLBACK TO` はテナント B の確定済みの行へ影響しない。
#[test]
fn rollback_to_does_not_touch_other_tenants_rows() {
    let (core, _g, mut s) = fixture("sql69-pw-tenant");
    let mut sb = SessionState::default();
    core.execute_sql_in_session(&ctx("b"), &mut sb, &ins(100, 100, "b1", "b-1"))
        .unwrap();
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "a1", "a-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "a2", "a-2"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 1)]);
    assert_eq!(committed(&core, "b", SEL_T), vec![(100, 100)]);
}

/// 一時表を使った Tx でも `ROLLBACK TO` が通る（savepoint の前後とも一時表を使う形）。
#[test]
fn temp_table_rows_follow_the_rollback() {
    let (core, _g, mut s) = temp_fixture("sql69-pw-temp");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins_tmp(1, "t-1"));
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins_tmp(2, "t-2"));
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "c2", "op-2"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    assert_eq!(in_txn(&core, "a", &mut s, &mut txn, SEL_TMP), vec![(1, 1)]);
    assert_eq!(in_txn(&core, "a", &mut s, &mut txn, SEL_T), vec![(1, 1)]);
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(session_tmp(&core, &mut s), vec![(1, 1)]);
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 1)]);
}

/// savepoint の後に初めて一時表を使った場合、`ROLLBACK TO` は一時側を基底（空）へ戻す。
#[test]
fn temp_table_first_used_after_savepoint_is_emptied() {
    let (core, _g, mut s) = temp_fixture("sql69-pw-temp-late");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins_tmp(7, "t-7"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    assert!(in_txn(&core, "a", &mut s, &mut txn, SEL_TMP).is_empty());
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert!(session_tmp(&core, &mut s).is_empty());
}

/// 一時表だけを書いた Tx（永続側は未書き込み）でも savepoint から戻せる。
#[test]
fn temp_only_transaction_rolls_back_to_savepoint() {
    let (core, _g, mut s) = temp_fixture("sql69-pw-temp-only");
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins_tmp(1, "t-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(&core, "a", &mut s, &mut txn, &ins_tmp(2, "t-2"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(session_tmp(&core, &mut s), vec![(1, 1)]);
}

/// 多くのカタログ機能（索引・GIN・ビュー・マテビュー・採番・ロール）を持つ DB でも、書き込み後の
/// savepoint が差分を取れる（= engine が使う全テーブルの型の組が `table_image` に列挙されている）。
/// 型の組の取りこぼしは `0A000` になる（fail-closed）ため、ここで検知する。
#[test]
fn rich_catalog_database_is_fully_covered_by_the_image() {
    let (core, _g, mut s) = fixture("sql69-pw-rich");
    for ddl in [
        "CREATE INDEX i_v ON t (v)",
        "CREATE TABLE g (id BIGINT PRIMARY KEY, doc JSONB)",
        "CREATE INDEX g_ops ON g USING gin (doc jsonb_ops)",
        "CREATE VIEW vw AS SELECT id FROM u",
        "CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM u",
        "CREATE SEQUENCE sq",
        "CREATE ROLE rr",
    ] {
        core.execute_sql_in_session(&ctx("sys"), &mut s, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {e}"));
    }
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "INSERT INTO g (id, doc) VALUES (1, '{\"a\":1}') USING OPERATION_ID 'g-1'",
    );
    ok(&core, "a", &mut s, &mut txn, &ins(2, 2, "c2", "op-2"));
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    assert_eq!(in_txn(&core, "a", &mut s, &mut txn, SEL_T), vec![(1, 1)]);
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 1)]);
}

/// 採番は非トランザクショナル（PostgreSQL と同じ）: 巻き戻した後も払い出し済みの値は再利用されない。
#[test]
fn sequence_values_are_not_reused_after_rollback_to() {
    let (core, _g, mut s) = fixture("sql69-pw-seq");
    core.execute_sql_in_session(&ctx("sys"), &mut s, "CREATE SEQUENCE sq")
        .unwrap();
    let mut txn = core.new_session_transaction();
    ok(&core, "a", &mut s, &mut txn, "BEGIN");
    ok(&core, "a", &mut s, &mut txn, &ins(1, 1, "c1", "op-1"));
    ok(&core, "a", &mut s, &mut txn, "SAVEPOINT sp");
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "INSERT INTO t (id, v, code) VALUES (2, nextval('sq'), 'c2') USING OPERATION_ID 'op-2'",
    );
    ok(&core, "a", &mut s, &mut txn, "ROLLBACK TO sp");
    ok(
        &core,
        "a",
        &mut s,
        &mut txn,
        "INSERT INTO t (id, v, code) VALUES (3, nextval('sq'), 'c3') USING OPERATION_ID 'op-3'",
    );
    ok(&core, "a", &mut s, &mut txn, "COMMIT");
    assert_eq!(committed(&core, "a", SEL_T), vec![(1, 1), (3, 2)]);
}
