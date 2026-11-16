//! ビューを FROM／USING 側に置く `UPDATE ... FROM`／`DELETE ... USING` の SQL 経由テスト
//! （SQL-53 (b)・RLS-10・RECOVER-10/11・Issue #3292）。
//!
//! 役割: 単純形ビュー（列別名なし）を読み取り専用の source として受理する契約（ビュー述語の畳み込み・
//! 公開列だけの束縛・基底表への RLS 適用・台帳）と、拒否のまま残す形（target がビュー・列別名つき・
//! 評価後射影形）が副作用ゼロで fail-closed になることを固定する。
//! ポインタ: `docs/design/update-from-delete-using.md`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    for (name, col) in [("t", "x"), ("s", "v")] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("k", ColumnType::Text, false),
                    ColumnDef::new(col, ColumnType::Integer, true),
                ],
            ))
            .expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, CleanupGuard(path))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<String, SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
}

fn ddl(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(ctx, &mut session, sql)
        .expect(sql);
}

fn code(r: Result<String, SqlSurfaceError>) -> String {
    r.expect_err("must be rejected").wire_code().to_string()
}

fn insert(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, k: &str, n: i64) {
    let col = if table == "t" { "x" } else { "v" };
    run(
        core,
        ctx,
        &format!(
            "INSERT INTO {table} (id, embedding, k, {col}) VALUES ({id}, '[0.1,0.2]', '{k}', {n})"
        ),
    )
    .expect("insert");
}

fn rows_t(core: &EngineCore, ctx: &PolicyContext) -> String {
    run(core, ctx, "SELECT id, k, x FROM t ORDER BY id LIMIT 100").expect("scan t")
}

/// t: (1,a,10) (2,b,20) (3,c,30)。s: (1,a,100) (2,b,200) (3,c,300)。ビュー `sv` は `v >= 150` の行だけ。
fn seed(core: &EngineCore, ctx: &PolicyContext) {
    for (id, k, x) in [(1, "a", 10), (2, "b", 20), (3, "c", 30)] {
        insert(core, ctx, "t", id, k, x);
    }
    for (id, k, v) in [(1, "a", 100), (2, "b", 200), (3, "c", 300)] {
        insert(core, ctx, "s", id, k, v);
    }
    ddl(
        core,
        ctx,
        "CREATE VIEW sv AS SELECT id, k, v FROM s WHERE v >= 150",
    );
}

fn tag(kind: &str, n: u64) -> String {
    match kind {
        "UPD" => format!("Update(UpdateOutcome {{ rows_affected: {n} }})"),
        _ => format!("Delete(DeleteOutcome {{ rows_affected: {n} }})"),
    }
}

fn explicit_one(core: &EngineCore, ctx: &PolicyContext, sql: &str, finish: &str) -> String {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(ctx, &mut session, &mut txn, sql)
        .map(|o| format!("{o:?}"))
        .expect(sql);
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, finish)
        .expect("finish");
    out
}

const UPD: &str = "UPDATE t SET x = sv.v FROM sv WHERE t.k = sv.k";
const DEL: &str = "DELETE FROM t USING sv WHERE t.k = sv.k";

/// ビュー述語で落ちた基底行は結合に入る前に除かれる（autocommit・明示 Tx とも同じタグ・同じ行）。
#[test]
fn view_source_applies_view_predicate_in_autocommit_and_explicit_tx() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-view-pred");
    seed(&core, &alice);
    assert_eq!(run(&core, &alice, UPD).expect("upd"), tag("UPD", 2));
    let rows = rows_t(&core, &alice);
    assert!(rows.contains("200") && rows.contains("300"), "{rows}");
    assert!(rows.contains("10"), "a is untouched: {rows}");

    let (core, _g) = new_core("sql53-view-del");
    seed(&core, &alice);
    assert_eq!(explicit_one(&core, &alice, DEL, "ROLLBACK"), tag("DEL", 2));
    assert!(rows_t(&core, &alice).contains("\"b\""), "rolled back");
    assert_eq!(explicit_one(&core, &alice, DEL, "COMMIT"), tag("DEL", 2));
    let rows = rows_t(&core, &alice);
    assert!(rows.contains("\"a\"") && !rows.contains("\"b\""), "{rows}");
}

/// ビュー経由でも基底表は呼び出し元の RLS で走査される。他テナントの Private 行は結合に入らない。
#[test]
fn view_source_never_joins_other_tenant_private_rows() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    let (core, _g) = new_core("sql53-view-rls");
    insert(&core, &alice, "t", 1, "z", 1);
    insert(&core, &bob, "s", 9, "z", 999);
    ddl(
        &core,
        &alice,
        "CREATE VIEW sv AS SELECT id, k, v FROM s WHERE v >= 150",
    );
    assert_eq!(run(&core, &alice, UPD).expect("upd"), tag("UPD", 0));
    assert_eq!(run(&core, &alice, DEL).expect("del"), tag("DEL", 0));
    assert!(rows_t(&core, &alice).contains("\"z\""));
}

/// ビューが公開しない基底列は束縛に参加しない（`42703`）。
#[test]
fn view_hidden_columns_are_not_bound() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-view-hidden");
    seed(&core, &alice);
    ddl(&core, &alice, "CREATE VIEW sk AS SELECT k FROM s");
    let before = rows_t(&core, &alice);
    for sql in [
        "UPDATE t SET x = sk.v FROM sk WHERE t.k = sk.k",
        "UPDATE t SET x = 1 FROM sk WHERE t.k = sk.k AND sk.v = '100'",
        "UPDATE t SET x = 1 FROM sk WHERE t.k = sk.k RETURNING sk.v",
    ] {
        assert_eq!(
            code(run(&core, &alice, sql)),
            "42703",
            "{sql}: {:?}",
            run(&core, &alice, sql)
        );
    }
    assert_eq!(before, rows_t(&core, &alice));
}

/// `RETURNING sk.*` はビューの公開列だけを返す。
#[test]
fn view_returning_star_lists_only_visible_columns() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-view-returning");
    seed(&core, &alice);
    ddl(
        &core,
        &alice,
        "CREATE VIEW sk AS SELECT k FROM s WHERE v >= 150",
    );
    let out = run(
        &core,
        &alice,
        "UPDATE t SET x = 0 FROM sk WHERE t.k = sk.k RETURNING sk.*",
    )
    .expect("returning");
    assert!(out.contains("\"b\"") && out.contains("\"c\""), "{out}");
    assert!(!out.contains("200"), "hidden column must not appear: {out}");
}

/// ビューの基底表が target 自身でも成立する（source の走査は書き込みより前に完了する）。
#[test]
fn view_over_the_target_table_is_a_self_join() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-view-self");
    seed(&core, &alice);
    ddl(
        &core,
        &alice,
        "CREATE VIEW tv AS SELECT id, k FROM t WHERE x < 25",
    );
    assert_eq!(
        run(&core, &alice, "DELETE FROM t USING tv WHERE t.k = tv.k").expect("del"),
        tag("DEL", 2)
    );
}

/// 台帳: `USING OPERATION_ID` 付きは再送が `23505`（二重適用しない）。
#[test]
fn view_source_resend_with_operation_id_is_rejected() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-view-op");
    seed(&core, &alice);
    let sql = format!("{UPD} USING OPERATION_ID 'view-op-1'");
    assert_eq!(run(&core, &alice, &sql).expect("first"), tag("UPD", 2));
    assert_eq!(code(run(&core, &alice, &sql)), "23505");
}

/// 拒否のまま: target がビュー・列別名つきビュー・評価後射影形ビュー。副作用ゼロ。
/// 更新可能ビュー・公開名の書き換え・本文の実行は別の穴として意図的に拒否する。
/// intentional: docs/design/update-from-delete-using.md
#[test]
fn unsupported_view_forms_fail_closed_without_side_effects() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-view-reject");
    seed(&core, &alice);
    ddl(
        &core,
        &alice,
        "CREATE VIEW va (kk, vv) AS SELECT k, v FROM s",
    );
    ddl(
        &core,
        &alice,
        "CREATE VIEW vb AS SELECT k, v FROM s LIMIT 2",
    );
    let before = rows_t(&core, &alice);
    for sql in [
        "DELETE FROM sv USING s WHERE sv.k = s.k",
        "UPDATE sv SET v = 1 FROM s WHERE sv.k = s.k",
        "UPDATE t SET x = 1 FROM va WHERE t.k = va.kk",
        "DELETE FROM t USING vb WHERE t.k = vb.k",
    ] {
        assert_eq!(code(run(&core, &alice, sql)), "0A000", "{sql}");
    }
    assert_eq!(before, rows_t(&core, &alice));
}
