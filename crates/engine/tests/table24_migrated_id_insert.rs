//! 移行済みの既存表の `INSERT` で疑似列 `id` を省略したときの IDENTITY 相当の採番の結合テスト
//! （Issue #1699。親 #1696）。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-23・TABLE-24、
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10、`rls.md` RLS-9・RLS-10。
//! 設計記録は `docs/design/migrated-id-identity.md`（D1・D4・D6・D8・D9・D10）。
//!
//! 検証の軸: 従来方式の表を持つ DB を閉じて開き直す（#2200 の移行 + #1699 の SEQUENCE 割り当て）と、
//!
//! - `id` を省略した単一行 INSERT が所有 SEQUENCE（`<table>_id_seq`）から自テナントの最大 id + 1 を採番し、
//!   `RETURNING id`・`currval` と一致する
//! - 明示した `id` の INSERT は従来どおりで SEQUENCE を進めない。採番値が既存行と衝突したら `23505` で、
//!   自動で次の値へ飛ばない
//! - `USING OPERATION_ID` の再送は値を消費せず `23505`、内容違いは `22023`
//! - テナント間で採番が独立し、他テナントの最大値・件数が採番値にもエラーにも現れない
//! - 範囲外の形（UPSERT・ビュー経由）と従来方式のままの表は固定のエラーで拒否される

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Visibility;

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

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session_legacy(&ctx(tenant), s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    let mut s = session();
    run(core, tenant, &mut s, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    let mut s = session();
    match run(core, tenant, &mut s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// `RETURNING id` の 1 行目の `id`。
fn returned_id(out: &SqlOutcome) -> u64 {
    match out {
        SqlOutcome::Returning(r) => match r.result.rows.first().and_then(|row| row.cells.first()) {
            Some(Cell::Integer(v)) => *v,
            other => panic!("id cell {other:?}"),
        },
        other => panic!("expected RETURNING, got {other:?}"),
    }
}

/// 省略 INSERT を 1 回実行して払い出された `id` を返す。
fn insert_omit(core: &EngineCore, tenant: &str, n: i64) -> u64 {
    returned_id(&ok(
        core,
        tenant,
        &format!("INSERT INTO items (n) VALUES ({n}) RETURNING id"),
    ))
}

fn count(core: &EngineCore, tenant: &str) -> usize {
    core.execute_sql(&ctx(tenant), "SELECT id FROM items LIMIT 1000")
        .expect("select")
        .rows
        .len()
}

/// 従来方式の表 `items (n BIGINT)` に tenant-a の id 3・10 と tenant-b の id 5 を入れ、閉じて開き直す。
fn fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    {
        let core = EngineCore::open(&path).expect("open");
        let mut s = session();
        run(&core, "sys", &mut s, "CREATE TABLE items (n BIGINT)").expect("create");
        for (t, id) in [("tenant-a", 3), ("tenant-a", 10), ("tenant-b", 5)] {
            ok(
                &core,
                t,
                &format!("INSERT INTO items (id, n) VALUES ({id}, {id})"),
            );
        }
    }
    let core = EngineCore::open(&path).expect("reopen migrates");
    (guard, core)
}

/// 受け入れ 1・4: 省略すると自テナントの最大 id + 1 から採番し、`RETURNING id` と読み出しが一致する。
/// テナントごとに独立し、行の無いテナントは 1 から始まる。
#[test]
fn omitted_id_is_assigned_per_tenant_from_max_id() {
    let (_g, core) = fixture("t24-mig-omit");
    assert_eq!(insert_omit(&core, "tenant-a", 100), 11);
    assert_eq!(insert_omit(&core, "tenant-a", 101), 12);
    assert_eq!(insert_omit(&core, "tenant-b", 200), 6);
    assert_eq!(insert_omit(&core, "tenant-c", 300), 1);
    let r = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id, n FROM items WHERE id = 11 LIMIT 10",
        )
        .expect("select");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(count(&core, "tenant-a"), 4);
    assert_eq!(count(&core, "tenant-b"), 2);
}

/// `RETURNING *` と `currval('<table>_id_seq')` も採番値と一致する（セッション内）。
#[test]
fn returning_star_and_currval_match_the_assigned_value() {
    let (_g, core) = fixture("t24-mig-currval");
    let mut s = session();
    let out = run(
        &core,
        "tenant-a",
        &mut s,
        "INSERT INTO items (n) VALUES (7) RETURNING *",
    )
    .expect("insert");
    match out {
        SqlOutcome::Returning(r) => {
            assert_eq!(r.rows_affected, 1);
            let ids: Vec<&Cell> = r.result.rows.iter().map(|row| &row.cells[0]).collect();
            assert!(matches!(ids.as_slice(), [Cell::Integer(11)]), "{ids:?}");
        }
        other => panic!("{other:?}"),
    }
    // 同一セッションの currval は直前の採番値。
    run(
        &core,
        "tenant-a",
        &mut s,
        "INSERT INTO items (id, n) VALUES (500, currval('items_id_seq'))",
    )
    .expect("currval insert");
    let r = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id, n FROM items WHERE id = 500 LIMIT 10",
        )
        .expect("select");
    assert!(matches!(r.rows[0].cells[1], Cell::SignedInteger(11)));
}

/// 受け入れ 2・3: 明示 `id` は従来どおり受理され SEQUENCE を進めない。採番値が既存行と衝突したら
/// `23505` で、自動で次の値へ飛ばない（衝突で消費した値は戻らない）。
#[test]
fn explicit_id_does_not_advance_sequence_and_collision_is_23505() {
    let (_g, core) = fixture("t24-mig-explicit");
    ok(
        &core,
        "tenant-a",
        "INSERT INTO items (id, n) VALUES (500, 1)",
    );
    assert_eq!(insert_omit(&core, "tenant-a", 2), 11);
    // 次の採番値 12 を先に明示で埋めると、省略 INSERT は 12 で衝突する。
    ok(
        &core,
        "tenant-a",
        "INSERT INTO items (id, n) VALUES (12, 3)",
    );
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "INSERT INTO items (n) VALUES (4) RETURNING id"
        ),
        "23505"
    );
    // 衝突した 12 は消費済みで、次は 13。
    assert_eq!(insert_omit(&core, "tenant-a", 5), 13);
    assert_eq!(count(&core, "tenant-a"), 6);
}

/// `USING OPERATION_ID`: 付けても付けなくても採番でき、再送は値を消費せず台帳由来の `23505`、
/// 同じ operation_id で内容が違えば `22023`。
#[test]
fn operation_id_resend_does_not_consume_a_value() {
    let (_g, core) = fixture("t24-mig-opid");
    let sql = "INSERT INTO items (n) VALUES (9) RETURNING id USING OPERATION_ID 'op-1'";
    assert_eq!(returned_id(&ok(&core, "tenant-a", sql)), 11);
    assert_eq!(code(&core, "tenant-a", sql), "23505");
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "INSERT INTO items (n) VALUES (10) RETURNING id USING OPERATION_ID 'op-1'"
        ),
        "22023"
    );
    // 再送・不一致で値が消費されていなければ、次は 12。
    assert_eq!(insert_omit(&core, "tenant-a", 11), 12);
    // 台帳は省略の文と `id = 0` の明示文を区別する（別ドメイン）。
    let explicit_zero = "INSERT INTO items (id, n) VALUES (0, 9) USING OPERATION_ID 'op-1'";
    assert_ne!(code(&core, "tenant-a", explicit_zero), "23505");
}

/// 明示トランザクション: COMMIT した行は残り、ROLLBACK した行は消える（欠番になる。D7）。
#[test]
fn explicit_transaction_rollback_leaves_a_gap() {
    let (_g, core) = fixture("t24-mig-txn");
    let mut s = session();
    let c = ctx("tenant-a");
    let mut txn = core.new_session_transaction();
    for sql in ["BEGIN", "INSERT INTO items (n) VALUES (1)", "ROLLBACK"] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(count(&core, "tenant-a"), 2);
    assert_eq!(insert_omit(&core, "tenant-a", 2), 12);
    for sql in ["BEGIN", "INSERT INTO items (n) VALUES (3)", "COMMIT"] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(count(&core, "tenant-a"), 4);
}

/// 上限: 状態が `i64::MAX` なら `2200H` で、行は増えない。
#[test]
fn exhausted_sequence_reports_2200h_without_writing() {
    let (_g, core) = fixture("t24-mig-limit");
    ok(
        &core,
        "tenant-a",
        "SELECT setval('items_id_seq', 9223372036854775807)",
    );
    assert_eq!(
        code(&core, "tenant-a", "INSERT INTO items (n) VALUES (1)"),
        "2200H"
    );
    assert_eq!(count(&core, "tenant-a"), 2);
    // 他テナントの状態は影響を受けない。
    assert_eq!(insert_omit(&core, "tenant-b", 1), 6);
}

/// 従来方式のままの表（開き直すまで）は `id` 必須の `22000` を維持する。
#[test]
fn legacy_table_still_requires_id() {
    let (_g, core) = fixture("t24-mig-legacy");
    let mut s = session();
    run(&core, "sys", &mut s, "CREATE TABLE fresh (n BIGINT)").expect("create");
    assert_eq!(
        code(&core, "tenant-a", "INSERT INTO fresh (n) VALUES (1)"),
        "22000"
    );
}

/// 範囲外の形（UPSERT）で `id` を省略すると固定の `0A000`。行は書かれない。複数行 VALUES は
/// Issue #1700 で受理に変わった（`table24_migrated_id_batch.rs` が検証する）。
#[test]
fn out_of_scope_shapes_reject_omitted_id() {
    let (_g, core) = fixture("t24-mig-shapes");
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "INSERT INTO items (n) VALUES (1) ON CONFLICT (id) DO NOTHING"
        ),
        "0A000"
    );
    assert_eq!(count(&core, "tenant-a"), 2);
    // 明示 id の複数行は従来どおり。
    ok(
        &core,
        "tenant-a",
        "INSERT INTO items (id, n) VALUES (20, 1), (21, 2)",
    );
    assert_eq!(count(&core, "tenant-a"), 4);
}

/// ビュー経由で基表の `id` を省略する INSERT は `0A000`（ADR D9。受理は後続）。
#[test]
fn view_insert_omitting_id_is_rejected() {
    let (_g, core) = fixture("t24-mig-view");
    let mut s = session();
    run(
        &core,
        "sys",
        &mut s,
        "CREATE VIEW v AS SELECT id, n FROM items",
    )
    .expect("view");
    assert_eq!(
        code(&core, "tenant-a", "INSERT INTO v (n) VALUES (1)"),
        "0A000"
    );
    ok(&core, "tenant-a", "INSERT INTO v (id, n) VALUES (40, 1)");
}
