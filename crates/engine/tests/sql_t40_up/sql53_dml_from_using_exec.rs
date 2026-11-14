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
// ---- RETURNING（Issue #3204）----

fn returning(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<engine::sql::exec::ReturningOutcome, SqlSurfaceError> {
    match core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)? {
        engine::sql::SqlOutcome::Returning(r) => Ok(r),
        other => panic!("{sql}: expected Returning, got {other:?}"),
    }
}

fn names(r: &engine::sql::exec::ReturningOutcome) -> Vec<String> {
    use engine::sql::exec::ColumnMeta;
    r.result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "id".to_string(),
            ColumnMeta::Scalar { name, .. } => name.clone(),
            other => panic!("unexpected column {other:?}"),
        })
        .collect()
}

fn int(n: i64) -> engine::sql::exec::Cell {
    engine::sql::exec::Cell::SignedInteger(n)
}

fn id_cell(n: u64) -> engine::sql::exec::Cell {
    engine::sql::exec::Cell::Integer(n)
}

/// 対象表は更新後の値、FROM 表は SET に使った一致行の値を返す。修飾あり・なしの列参照・`;` 付き。
#[test]
fn update_from_returning_returns_new_target_and_matched_source_values() {
    let alice = ctx("alice");
    for sql in [
        "UPDATE t SET x = s.v FROM s WHERE t.k = s.k RETURNING t.id, s.v, x",
        "UPDATE t SET x = s.v FROM s WHERE t.k = s.k RETURNING t.id, s.v, x;",
    ] {
        let (core, _g) = new_core("sql53-ret-upd");
        seed(&core, &alice);
        let r = returning(&core, &alice, sql).expect(sql);
        assert_eq!(r.rows_affected, 2, "{sql}");
        assert_eq!(names(&r), vec!["id", "v", "x"], "{sql}");
        let rows: Vec<_> = r.result.rows.iter().map(|r| r.cells.clone()).collect();
        assert_eq!(
            rows,
            vec![
                vec![id_cell(1), int(100), int(100)],
                vec![id_cell(2), int(200), int(200)],
            ],
            "{sql}"
        );
    }
}

/// `*` は対象表 → FROM 表の宣言順（PG 17 と同じ）。`s.*` は FROM 表だけ。DELETE は削除前の値。
#[test]
fn star_expands_target_then_from_and_delete_returns_pre_delete_values() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-ret-star");
    seed(&core, &alice);
    let r = returning(
        &core,
        &alice,
        "DELETE FROM t USING s WHERE t.k = s.k RETURNING *",
    )
    .expect("delete returning");
    assert_eq!(r.rows_affected, 2);
    assert_eq!(
        names(&r),
        vec!["id", "embedding", "k", "x", "id", "embedding", "k", "v"]
    );
    let first = &r.result.rows[0].cells;
    assert_eq!(first[0], id_cell(1));
    assert_eq!(first[2], engine::sql::exec::Cell::Text("a".into()));
    assert_eq!(first[3], int(10));
    assert_eq!(first[4], id_cell(1));
    assert_eq!(first[7], int(100));
    assert!(!xs(&core, &alice).contains("10"));

    let (core, _g) = new_core("sql53-ret-qstar");
    seed(&core, &alice);
    let r = returning(
        &core,
        &alice,
        "UPDATE t SET x = 1 FROM s WHERE t.k = s.k RETURNING s.*",
    )
    .expect("qualified star");
    assert_eq!(names(&r), vec!["id", "embedding", "k", "v"]);
    assert_eq!(r.result.rows.len(), 2);
}

/// `RETURNING` 専用の source 列（VECTOR）は SET の式評価ビューを通らない。SET が source の
/// 整数列を読む UPDATE・対象表の式だけの UPDATE・DELETE のいずれでも、非 NULL の VECTOR 列を返せる。
#[test]
fn returning_source_vector_column_does_not_enter_set_expression_view() {
    use engine::sql::exec::Cell;
    let alice = ctx("alice");
    for sql in [
        "UPDATE t SET x = s.v FROM s WHERE t.k = s.k RETURNING s.*",
        "UPDATE t SET x = x + 1 FROM s WHERE t.k = s.k RETURNING s.*",
        "DELETE FROM t USING s WHERE t.k = s.k RETURNING s.*",
    ] {
        let (core, _g) = new_core("sql53-ret-vec");
        seed(&core, &alice);
        let r = returning(&core, &alice, sql).expect(sql);
        assert_eq!(r.rows_affected, 2, "{sql}");
        assert_eq!(names(&r), vec!["id", "embedding", "k", "v"], "{sql}");
        assert_eq!(r.result.rows.len(), 2, "{sql}");
        for row in &r.result.rows {
            assert!(
                !matches!(row.cells.get(1), Some(Cell::Null) | None),
                "{sql}: embedding cell must be returned"
            );
        }
    }
}

/// 明示 Tx でも同じ結果になり、ROLLBACK で巻き戻る。Describe は Execute と同じ列を返す。
#[test]
fn explicit_transaction_and_describe_agree_with_autocommit() {
    let alice = ctx("alice");
    let sql = "UPDATE t SET x = s.v FROM s WHERE t.k = s.k RETURNING t.id, s.v";
    let (c1, _g1) = new_core("sql53-ret-auto");
    seed(&c1, &alice);
    let auto = returning(&c1, &alice, sql).expect("auto");
    let (c2, _g2) = new_core("sql53-ret-txn");
    seed(&c2, &alice);
    let before = xs(&c2, &alice);
    let mut session = SessionState::default();
    let mut txn = c2.new_session_transaction();
    c2.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = c2
        .execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
        .expect("txn");
    assert_eq!(out, engine::sql::SqlOutcome::Returning(auto.clone()));
    c2.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(before, xs(&c2, &alice));

    let parsed = c2.parse_sql(sql).expect("parse");
    let meta = c2
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(meta, auto.result.columns);
}

/// RLS-10: 他テナントの Private な source 行は一致せず返却にも現れない。他テナントの対象行も返らない。
#[test]
fn returning_never_exposes_other_tenant_private_rows() {
    let (core, _g) = new_core("sql53-ret-tenant");
    let alice = ctx("alice");
    let bob = ctx("bob");
    seed(&core, &alice);
    insert(&core, &bob, "s", 10, "c", 777);
    insert(&core, &bob, "t", 1, "a", 5);
    let r = returning(
        &core,
        &alice,
        "UPDATE t SET x = s.v FROM s WHERE t.k = s.k RETURNING t.x, s.v",
    )
    .expect("update");
    assert_eq!(r.rows_affected, 2);
    let rows: Vec<_> = r.result.rows.iter().map(|r| r.cells.clone()).collect();
    assert_eq!(
        rows,
        vec![vec![int(100), int(100)], vec![int(200), int(200)]]
    );
    let r = returning(
        &core,
        &alice,
        "DELETE FROM t USING s WHERE t.k = s.k RETURNING s.v",
    )
    .expect("delete");
    assert_eq!(r.rows_affected, 2);
    assert!(xs(&core, &bob).contains('5'), "bob's row must remain");
}

/// 未対応の形は fail-closed（副作用ゼロ）: 別名・関数呼び出し・項目なし・曖昧な列・`USING OPERATION_ID` 併用。
// intentional: docs/design/sql-returning.md
// tracked: #3203
#[test]
fn unsupported_returning_forms_fail_closed_without_side_effects() {
    let (core, _g) = new_core("sql53-ret-reject");
    let alice = ctx("alice");
    seed(&core, &alice);
    let before = xs(&core, &alice);
    for (sql, code) in [
        (
            "UPDATE t SET x = 1 FROM s WHERE t.k = s.k RETURNING t.x AS y",
            "42601",
        ),
        (
            "UPDATE t SET x = 1 FROM s WHERE t.k = s.k RETURNING lower(s.k)",
            "42601",
        ),
        ("DELETE FROM t USING s WHERE t.k = s.k RETURNING", "42601"),
        ("DELETE FROM t USING s WHERE t.k = s.k RETURNING k", "42702"),
        (
            "DELETE FROM t USING s WHERE t.k = s.k RETURNING z.k",
            "42P01",
        ),
        (
            "UPDATE t SET x = 1 FROM s WHERE t.k = s.k RETURNING s.nope",
            "42703",
        ),
        (
            "UPDATE t SET x = 1 FROM s WHERE t.k = s.k RETURNING s.v USING OPERATION_ID 'r1'",
            "0A000",
        ),
        (
            "UPDATE t SET x = 1 FROM s WHERE t.k = s.k USING OPERATION_ID 'r2' RETURNING s.v",
            "42601",
        ),
    ] {
        let err = run(&core, &alice, sql).expect_err(sql);
        assert_eq!(err.wire_code(), code, "{sql}");
    }
    assert_eq!(before, xs(&core, &alice));
}
