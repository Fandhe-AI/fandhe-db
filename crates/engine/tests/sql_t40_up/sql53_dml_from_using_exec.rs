//! `UPDATE ... FROM`／`DELETE ... USING` の SQL 経由の実行（SQL-53 (b)・RLS-10・Issue #3202）の受け入れテスト。
//!
//! 契約: autocommit（暗黙トランザクション）と明示トランザクションで `UPDATE n`／`DELETE n` が一致する。
//! 多重一致は対象行ごとに 1 回だけ、走査順で最初の source 行を採る。他テナントの Private な行は
//! source にも対象にもならない。`PARTITIONED` 併用は構文段で `42601`、`READ ONLY` Tx は `25006`。
//! 網羅的な受け入れは #3205 の担当で、本ファイルは dispatch の最小固定に留める。
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
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("x", ColumnType::Integer, true),
            ],
        ))
        .expect("create t");
    storage
        .create_table(&TableSchema::new(
            "s",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("v", ColumnType::Integer, true),
            ],
        ))
        .expect("create s");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<String, SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
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

/// 呼び出し元から見える `t` の `(id, x)` を id 昇順で返す（Debug 文字列の比較用）。
fn xs(core: &EngineCore, ctx: &PolicyContext) -> String {
    run(core, ctx, "SELECT id, x FROM t ORDER BY id LIMIT 100").expect("scan")
}

/// t: (1,a,10) (2,b,20) (3,c,30)。s: (1,a,100) (2,b,200)。
fn seed(core: &EngineCore, ctx: &PolicyContext) {
    for (id, k, x) in [(1, "a", 10), (2, "b", 20), (3, "c", 30)] {
        insert(core, ctx, "t", id, k, x);
    }
    for (id, k, v) in [(1, "a", 100), (2, "b", 200)] {
        insert(core, ctx, "s", id, k, v);
    }
}

const UPD: &str = "UPDATE t SET x = s.v FROM s WHERE t.k = s.k";
const DEL: &str = "DELETE FROM t USING s WHERE t.k = s.k";

fn explicit(core: &EngineCore, ctx: &PolicyContext, sqls: &[&str]) -> Vec<String> {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let outs = sqls
        .iter()
        .map(|sql| {
            let o = core
                .execute_sql_in_txn(ctx, &mut session, &mut txn, sql)
                .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            format!("{o:?}")
        })
        .collect();
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    outs
}

/// autocommit と明示 Tx で、タグも結果行も一致する（psql 形の末尾 `;` を含む）。
#[test]
fn autocommit_and_explicit_transaction_agree() {
    for sql in [
        UPD.to_string(),
        format!("{UPD};"),
        DEL.to_string(),
        format!("{DEL};"),
    ] {
        let alice = ctx("alice");
        let (c1, _g1) = new_core("sql53-exec-auto");
        seed(&c1, &alice);
        let auto = run(&c1, &alice, &sql).expect("autocommit");
        let (c2, _g2) = new_core("sql53-exec-txn");
        seed(&c2, &alice);
        let txn = explicit(&c2, &alice, &[&sql]);
        assert_eq!(vec![auto.clone()], txn, "{sql}");
        assert_eq!(xs(&c1, &alice), xs(&c2, &alice), "{sql}");
        let want = if sql.starts_with("UPDATE") {
            "Update(UpdateOutcome { rows_affected: 2 })"
        } else {
            "Delete(DeleteOutcome { rows_affected: 2 })"
        };
        assert_eq!(auto, want, "{sql}");
    }
}

/// 多重一致: 対象 1 行に source 2 行が一致しても 1 回だけ更新し、値は走査順で最初の source 行のもの。
#[test]
fn multiple_matches_apply_once_with_the_first_source_row() {
    let alice = ctx("alice");
    for explicit_tx in [false, true] {
        let (core, _g) = new_core("sql53-exec-multi");
        seed(&core, &alice);
        insert(&core, &alice, "s", 3, "a", 999);
        let out = if explicit_tx {
            explicit(&core, &alice, &[UPD]).remove(0)
        } else {
            run(&core, &alice, UPD).expect("update")
        };
        assert_eq!(out, "Update(UpdateOutcome { rows_affected: 2 })");
        let rows = xs(&core, &alice);
        assert!(rows.contains("100") && !rows.contains("999"), "{rows}");
    }
    let (core, _g) = new_core("sql53-exec-multi-del");
    seed(&core, &alice);
    insert(&core, &alice, "s", 3, "a", 999);
    assert_eq!(
        run(&core, &alice, DEL).expect("delete"),
        "Delete(DeleteOutcome { rows_affected: 2 })"
    );
}

/// RLS-10: 他テナントの Private な source 行は一致せず、他テナントの対象行は変更されない。
#[test]
fn other_tenant_private_rows_are_neither_matched_nor_modified() {
    let (core, _g) = new_core("sql53-exec-tenant");
    let alice = ctx("alice");
    let bob = ctx("bob");
    seed(&core, &alice);
    insert(&core, &bob, "s", 10, "c", 777);
    insert(&core, &bob, "t", 1, "a", 5);
    assert_eq!(
        run(&core, &alice, UPD).expect("update"),
        "Update(UpdateOutcome { rows_affected: 2 })"
    );
    let rows = xs(&core, &alice);
    assert!(
        rows.contains("100") && rows.contains("200") && !rows.contains("777"),
        "{rows}"
    );
    assert!(xs(&core, &bob).contains('5'));
    assert_eq!(
        run(&core, &alice, DEL).expect("delete"),
        "Delete(DeleteOutcome { rows_affected: 2 })"
    );
    assert!(xs(&core, &bob).contains('5'), "bob's row must remain");
}

/// `PARTITIONED` 併用は構文段で `42601`（分割実行器へ結合形が入る経路は無い）。行は変わらない。
// intentional: docs/design/update-from-delete-using.md
#[test]
fn partitioned_is_rejected_in_autocommit_and_explicit_transaction() {
    let (core, _g) = new_core("sql53-exec-partitioned");
    let alice = ctx("alice");
    seed(&core, &alice);
    let before = xs(&core, &alice);
    let sqls = [
        "UPDATE t SET x = s.v FROM s WHERE t.k = s.k PARTITIONED",
        "DELETE FROM t USING s WHERE t.k = s.k PARTITIONED",
    ];
    for sql in sqls {
        let err = run(&core, &alice, sql).expect_err("autocommit");
        assert_eq!(err.wire_code(), "42601", "{sql}");
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .expect_err("explicit");
        assert_eq!(err.wire_code(), "42601", "{sql}");
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    assert_eq!(before, xs(&core, &alice));
}

/// `READ ONLY` Tx では書き込みとして `25006`（Tx は `Failed`）。行は変わらない。
#[test]
fn read_only_transaction_rejects_joined_dml() {
    let (core, _g) = new_core("sql53-exec-readonly");
    let alice = ctx("alice");
    seed(&core, &alice);
    let before = xs(&core, &alice);
    for sql in [UPD, DEL] {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN READ ONLY")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .expect_err("read only");
        assert_eq!(err.wire_code(), "25006", "{sql}");
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    assert_eq!(before, xs(&core, &alice));
}
