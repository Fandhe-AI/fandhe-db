//! opt-in の `--require-where-for-dml`（Issue #2210。SQL-36 のポインタのみ）の結合テスト。
//! `EngineCore::with_require_where_for_dml(true)` のとき、`WHERE` 省略の SQL `UPDATE`／`DELETE` が
//! 変更開始前に `42601` で拒否され、明示した述語・既定（無効）の挙動・テナント境界は変わらないことを
//! 固定する。`sql36_where_omitted_dml.rs` と同じ流儀（実 `Storage`＋`execute_sql_in_session`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn new_core(require: bool) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql36-require-where");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_require_where_for_dml(require),
        guard,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .map_err(|e| e.wire_code().to_string())
}

fn ddl(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(ctx, &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn seed(core: &EngineCore, ctx: &PolicyContext, tag: &str, ids: &[u64]) {
    for id in ids {
        run(
            core,
            ctx,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.{id},0.1]', 'ja') \
                 USING OPERATION_ID 'seed-{tag}-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("seed {tag}-{id}: {e}"));
    }
}

fn ids_of(core: &EngineCore, ctx: &PolicyContext) -> Vec<u64> {
    let r = core
        .execute_sql(ctx, &format!("SELECT id FROM {TABLE} LIMIT 100"))
        .expect("select");
    let mut v: Vec<u64> = r
        .rows
        .iter()
        .map(|row| match row.cells.first() {
            Some(Cell::Integer(n)) => *n,
            Some(Cell::SignedInteger(n)) => u64::try_from(*n).expect("id >= 0"),
            other => panic!("unexpected id cell: {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn count_lang(core: &EngineCore, ctx: &PolicyContext, lang: &str) -> usize {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {TABLE} WHERE lang = '{lang}' LIMIT 100"),
    )
    .expect("select")
    .rows
    .len()
}

/// 受け入れ条件 (a): 無効（既定）では WHERE 省略の DML が成功し全行が対象になる。
#[test]
fn disabled_accepts_omitted_where() {
    let (core, _g) = new_core(false);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1, 2, 3]);
    run(
        &core,
        &a,
        &format!("UPDATE {TABLE} SET lang = 'en' USING OPERATION_ID 'u'"),
    )
    .expect("update");
    assert_eq!(count_lang(&core, &a, "en"), 3);
    run(
        &core,
        &a,
        &format!("DELETE FROM {TABLE} USING OPERATION_ID 'd'"),
    )
    .expect("delete");
    assert!(ids_of(&core, &a).is_empty());
}

/// 受け入れ条件 (b): 有効では 42601 で拒否され、何も変更されない（他テナントも不変）。
#[test]
fn enabled_rejects_omitted_where_without_side_effects() {
    let (core, _g) = new_core(true);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    seed(&core, &a, "a", &[1, 2, 3]);
    seed(&core, &b, "b", &[11, 12]);
    for sql in [
        format!("DELETE FROM {TABLE}"),
        format!("UPDATE {TABLE} SET lang = 'en'"),
        format!("DELETE FROM {TABLE} RETURNING id USING OPERATION_ID 'x'"),
        format!("UPDATE {TABLE} SET lang = 'en' RETURNING id USING OPERATION_ID 'x'"),
    ] {
        assert_eq!(run(&core, &a, &sql).unwrap_err(), "42601", "{sql}");
    }
    assert_eq!(ids_of(&core, &a), vec![1, 2, 3]);
    assert_eq!(ids_of(&core, &b), vec![11, 12]);
    assert_eq!(count_lang(&core, &a, "en"), 0);

    // 拒否は台帳を消費しない: 同じ operation_id で WHERE 付きの文が成功する。
    run(
        &core,
        &a,
        &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'x'"),
    )
    .expect("same id after rejection");
    assert_eq!(ids_of(&core, &a), vec![2, 3]);
}

/// 明示した述語（単一行形・非恒真）は有効でも受理される。
#[test]
fn enabled_accepts_explicit_predicates() {
    let (core, _g) = new_core(true);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1, 2, 3]);
    run(
        &core,
        &a,
        &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'u1'"),
    )
    .expect("single row");
    run(
        &core,
        &a,
        &format!("UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'u2'"),
    )
    .expect("predicate");
    run(
        &core,
        &a,
        &format!("DELETE FROM {TABLE} WHERE lang = 'fr' USING OPERATION_ID 'd1'"),
    )
    .expect("predicate delete");
    assert_eq!(ids_of(&core, &a), vec![1]);
}

/// `WHERE TRUE` の結果は本フラグの有無で一致する（明示した述語には効かない）。
#[test]
fn where_true_outcome_is_independent_of_flag() {
    let outcome = |require: bool| {
        let (core, _g) = new_core(require);
        let a = ctx_for("tenant-a");
        seed(&core, &a, "a", &[1, 2]);
        let r = run(
            &core,
            &a,
            &format!("DELETE FROM {TABLE} WHERE TRUE USING OPERATION_ID 't'"),
        )
        .map(|_| ());
        (r, ids_of(&core, &a))
    };
    assert_eq!(outcome(true), outcome(false));
}

/// PARTITIONED（実行・EXPLAIN）も WHERE 省略なら拒否される。
#[test]
fn enabled_rejects_partitioned_and_explain() {
    let (core, _g) = new_core(true);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1, 2, 3]);
    for sql in [
        format!("DELETE FROM {TABLE} USING OPERATION_ID 'p' PARTITIONED CHUNK 2"),
        format!("UPDATE {TABLE} SET lang = 'en' USING OPERATION_ID 'p' PARTITIONED CHUNK 2"),
        format!("EXPLAIN DELETE FROM {TABLE} USING OPERATION_ID 'p' PARTITIONED CHUNK 2"),
        format!(
            "EXPLAIN UPDATE {TABLE} SET lang = 'en' USING OPERATION_ID 'p' PARTITIONED CHUNK 2"
        ),
    ] {
        assert_eq!(run(&core, &a, &sql).unwrap_err(), "42601", "{sql}");
    }
    assert_eq!(ids_of(&core, &a), vec![1, 2, 3]);
}

/// ビュー経由でも WHERE 省略は拒否され、基表は変わらない。WHERE 付きは成功する。
#[test]
fn enabled_rejects_omitted_where_through_view() {
    let (core, _g) = new_core(true);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1, 2]);
    ddl(
        &core,
        &a,
        "CREATE VIEW v AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    assert_eq!(run(&core, &a, "DELETE FROM v").unwrap_err(), "42601");
    assert_eq!(
        run(&core, &a, "UPDATE v SET lang = 'en'").unwrap_err(),
        "42601"
    );
    assert_eq!(ids_of(&core, &a), vec![1, 2]);
    run(
        &core,
        &a,
        "DELETE FROM v WHERE id = 1 USING OPERATION_ID 'vd'",
    )
    .expect("view delete with where");
    assert_eq!(ids_of(&core, &a), vec![2]);
}

/// 一時表への WHERE 省略も拒否される。
#[test]
fn enabled_rejects_omitted_where_on_temp_table() {
    let (core, _g) = new_core(true);
    let a = ctx_for("tenant-a");
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &a,
        &mut s,
        "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, n INTEGER)",
    )
    .expect("create temp");
    core.execute_sql_in_session(&a, &mut s, "INSERT INTO t (id, n) VALUES (1, 1)")
        .expect("insert");
    for sql in ["DELETE FROM t", "UPDATE t SET n = 2"] {
        let e = core.execute_sql_in_session(&a, &mut s, sql).unwrap_err();
        assert_eq!(e.wire_code(), "42601", "{sql}");
    }
    core.execute_sql_in_session(&a, &mut s, "DELETE FROM t WHERE id = 1")
        .expect("with where");
}

/// 判定は表の存在確認より前: 存在しない表でも 42601（存在の手掛かりを返さない）。
#[test]
fn enabled_rejects_before_table_existence_check() {
    let (core, _g) = new_core(true);
    let a = ctx_for("tenant-a");
    assert_eq!(run(&core, &a, "DELETE FROM nosuch").unwrap_err(), "42601");
    assert_eq!(
        run(&core, &a, "UPDATE nosuch SET x = 1").unwrap_err(),
        "42601"
    );
}

/// EXPLAIN ... PARTITIONED でも WHERE 省略の拒否は表の存在確認より前: 不存在表と既存表で
/// 応答が同じ 42601（表の存在有無を漏らさない）。
#[test]
fn enabled_explain_partitioned_rejects_before_table_existence_check() {
    let (core, _g) = new_core(true);
    let a = ctx_for("tenant-a");
    seed(&core, &a, "a", &[1]);
    for table in [TABLE, "nosuch"] {
        let del = format!("EXPLAIN DELETE FROM {table} USING OPERATION_ID 'p' PARTITIONED CHUNK 2");
        assert_eq!(run(&core, &a, &del).unwrap_err(), "42601", "{del}");
        let upd = format!(
            "EXPLAIN UPDATE {table} SET lang = 'en' USING OPERATION_ID 'p' PARTITIONED CHUNK 2"
        );
        assert_eq!(run(&core, &a, &upd).unwrap_err(), "42601", "{upd}");
    }
}
