//! `MERGE ... WHEN MATCHED`（UPDATE・DELETE・DO NOTHING）と多重一致の拒否（SQL-79・Issue #3353。親 #3349、
//! 設計記録 `docs/design/sql-merge.md` の D4・D5・D6）の SQL 経由の結合テスト。
//!
//! 契約: MATCHED の句だけで書かれた MERGE は、autocommit・明示 Tx のどちらでも実行され、行の内容と件数
//! （UPDATE と DELETE の実変更行数の合計。DO NOTHING は含めない）が決まる。1 つの対象行について UPDATE か
//! DELETE を選んだ組が 2 つ以上あれば文全体が `21000` になり、行・台帳・世代に何も残らない。対象外の形
//! （`RETURNING`・台帳記録・ビュー対象・文トリガ・利用者ポリシー）は固定文言の `0A000` で
//! 拒否する。他テナントの行は一致・件数・`21000` のどれにも現れない（3 テナントの越境の網羅は #3358）。
//! 期待値は設計記録の決定から導く（PG17 との突き合わせは #3358）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::parser::DmlLimits;
use engine::sql::SqlOutcome;
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
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ctx() -> PolicyContext {
    ctx_of("alice")
}

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, (String, String)> {
    core.execute_sql_in_session(&ctx_of(tenant), &mut SessionState::default(), sql)
        .map_err(|e| (e.wire_code().to_string(), e.client_message()))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, (String, String)> {
    run_as(core, "alice", sql)
}

/// MERGE の件数（コマンドタグ `MERGE n` の n）。
fn merged(core: &EngineCore, sql: &str) -> u64 {
    match run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Merge(o) => o.rows_affected,
        other => panic!("{sql}: unexpected outcome {other:?}"),
    }
}

fn err_code(core: &EngineCore, sql: &str) -> (String, String) {
    run(core, sql).expect_err(sql)
}

fn insert_t(core: &EngineCore, tenant: &str, rows: &[(u64, &str, Option<i32>)]) {
    for (id, k, x) in rows {
        let x = x.map_or("NULL".to_string(), |v| v.to_string());
        run_as(
            core,
            tenant,
            &format!("INSERT INTO t (id, embedding, k, x) VALUES ({id}, '[0.1,0.2]', '{k}', {x})"),
        )
        .expect("seed t");
    }
}

fn insert_s(core: &EngineCore, tenant: &str, rows: &[(u64, &str, Option<i32>)]) {
    for (id, k, v) in rows {
        let v = v.map_or("NULL".to_string(), |v| v.to_string());
        run_as(
            core,
            tenant,
            &format!("INSERT INTO s (id, embedding, k, v) VALUES ({id}, '[0.1,0.2]', '{k}', {v})"),
        )
        .expect("seed s");
    }
}

/// 標準の初期状態: t = {1:a/10, 2:b/20, 3:c/30}、s = {1:a/100, 3:c/300, 4:d/400}。
fn seed(core: &EngineCore) {
    insert_t(
        core,
        "alice",
        &[(1, "a", Some(10)), (2, "b", Some(20)), (3, "c", Some(30))],
    );
    insert_s(
        core,
        "alice",
        &[
            (1, "a", Some(100)),
            (3, "c", Some(300)),
            (4, "d", Some(400)),
        ],
    );
}

/// 対象表の内容（`id:k:x` を id 昇順に並べた文字列）。
fn t_rows(core: &EngineCore) -> String {
    rows_of(
        core,
        "alice",
        "SELECT id, k, x FROM t ORDER BY id LIMIT 100",
    )
}

fn rows_of(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .collect::<Vec<_>>()
            .join(" | "),
        other => panic!("{sql}: unexpected outcome {other:?}"),
    }
}

/// 全表の内容と世代（副作用ゼロの検査に使う）。
fn snapshot(core: &EngineCore) -> String {
    let t = t_rows(core);
    let s = rows_of(
        core,
        "alice",
        "SELECT id, k, v FROM s ORDER BY id LIMIT 100",
    );
    format!("{t}\n{s}")
}

/// 末尾 `;` なし・ありの両方で同じ件数・同じ結果になることを確かめる補助。
fn with_semicolon(sql: &str) -> [String; 2] {
    [sql.to_string(), format!("{sql};")]
}

#[test]
fn matched_update_sets_values_from_the_source_and_counts_changed_rows() {
    for text in
        with_semicolon("MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v")
    {
        let (core, _g) = new_core("sql79m-update");
        seed(&core);
        assert_eq!(merged(&core, &text), 2, "{text}");
        assert_eq!(
            t_rows(&core),
            "Integer(1):Text(\"a\"):SignedInteger(100) | Integer(2):Text(\"b\"):SignedInteger(20) | \
             Integer(3):Text(\"c\"):SignedInteger(300)",
            "{text}"
        );
    }
}

#[test]
fn matched_delete_removes_matching_rows_only() {
    for text in with_semicolon("MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE") {
        let (core, _g) = new_core("sql79m-delete");
        seed(&core);
        assert_eq!(merged(&core, &text), 2, "{text}");
        assert_eq!(
            t_rows(&core),
            "Integer(2):Text(\"b\"):SignedInteger(20)",
            "{text}"
        );
    }
}

#[test]
fn matched_do_nothing_changes_nothing_and_counts_zero() {
    for text in with_semicolon("MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DO NOTHING") {
        let (core, _g) = new_core("sql79m-nothing");
        seed(&core);
        let before = snapshot(&core);
        assert_eq!(merged(&core, &text), 0, "{text}");
        assert_eq!(before, snapshot(&core), "{text}");
    }
}

#[test]
fn first_true_clause_wins_and_unmatched_conditions_do_nothing() {
    let (core, _g) = new_core("sql79m-cond");
    seed(&core);
    // s.v: a=100, c=300。先に真になった句だけを採る（a は DELETE、c は UPDATE）。
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN MATCHED AND s.v < 200 THEN DELETE \
         WHEN MATCHED AND s.v > 0 THEN UPDATE SET x = -1 \
         WHEN MATCHED THEN DELETE",
    );
    assert_eq!(n, 2);
    assert_eq!(
        t_rows(&core),
        "Integer(2):Text(\"b\"):SignedInteger(20) | Integer(3):Text(\"c\"):SignedInteger(-1)"
    );
    // どの条件も真にならない組は何もしない。
    let before = snapshot(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED AND s.v < 0 THEN DELETE"
        ),
        0
    );
    assert_eq!(before, snapshot(&core));
}

#[test]
fn update_and_delete_can_be_mixed_and_the_count_is_their_sum() {
    let (core, _g) = new_core("sql79m-mixed");
    seed(&core);
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN MATCHED AND s.v > 200 THEN UPDATE SET x = s.v + t.x \
         WHEN MATCHED THEN DELETE",
    );
    assert_eq!(n, 2);
    assert_eq!(
        t_rows(&core),
        "Integer(2):Text(\"b\"):SignedInteger(20) | Integer(3):Text(\"c\"):SignedInteger(330)"
    );
}

/// 条件内の対象と元をまたぐ列同士の等値は、結合キーとして評価される（落とすと条件が黙って広がる）。
/// NULL との等値は真にならない。
#[test]
fn cross_relation_equality_inside_a_when_condition_is_enforced() {
    let (core, _g) = new_core("sql79m-condkey");
    insert_t(
        &core,
        "alice",
        &[(1, "a", Some(100)), (2, "a", Some(7)), (3, "a", None)],
    );
    insert_s(&core, "alice", &[(1, "a", Some(100))]);
    // ON は k のみ。WHEN の条件が x = v を要求する（t=100 だけが真。NULL の行は真にならない）。
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED AND t.x = s.v THEN DELETE",
    );
    assert_eq!(n, 1);
    assert_eq!(
        t_rows(&core),
        "Integer(2):Text(\"a\"):SignedInteger(7) | Integer(3):Text(\"a\"):Null"
    );
}

#[test]
fn set_accepts_literals_columns_expressions_default_and_aliases() {
    let (core, _g) = new_core("sql79m-setforms");
    seed(&core);
    let n = merged(
        &core,
        "MERGE INTO t AS d USING s AS e ON d.k = e.k WHEN MATCHED AND e.v = 100 THEN \
         UPDATE SET x = e.v * 2 + d.x, k = 'z'",
    );
    assert_eq!(n, 1);
    assert_eq!(
        t_rows(&core),
        "Integer(1):Text(\"z\"):SignedInteger(210) | Integer(2):Text(\"b\"):SignedInteger(20) | \
         Integer(3):Text(\"c\"):SignedInteger(30)"
    );
    // 対象の列から対象の列への代入と `DEFAULT`（列既定値なし = NULL）。
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED AND s.v = 300 THEN UPDATE SET x = DEFAULT",
    );
    assert_eq!(n, 1);
    assert_eq!(
        rows_of(&core, "alice", "SELECT id, x FROM t WHERE id = 3 LIMIT 10"),
        "Integer(3):Null"
    );
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED AND s.v = 100 THEN UPDATE SET k = t.k",
    );
    assert_eq!(
        n, 1,
        "id=1 は ON の id 一致かつ v=100 で、k の自己代入も 1 件に数える"
    );
}
// ---------------------------------------------------------------------
// 21000（多重一致）
// ---------------------------------------------------------------------

/// 対象 1 行 (id=1, k=a) に、元の 2 行 (id=1 v=100, id=5 v=500) が一致する状態にする。
fn seed_double_match(core: &EngineCore) {
    insert_t(core, "alice", &[(1, "a", Some(10)), (2, "b", Some(20))]);
    insert_s(core, "alice", &[(1, "a", Some(100)), (5, "a", Some(500))]);
}

#[test]
fn two_changing_matches_for_one_target_row_fail_with_21000_and_leave_nothing() {
    for (label, clauses) in [
        ("uu", "WHEN MATCHED THEN UPDATE SET x = s.v"),
        ("dd", "WHEN MATCHED THEN DELETE"),
        (
            "ud",
            "WHEN MATCHED AND s.v < 200 THEN UPDATE SET x = s.v WHEN MATCHED THEN DELETE",
        ),
        (
            "du",
            "WHEN MATCHED AND s.v < 200 THEN DELETE WHEN MATCHED THEN UPDATE SET x = s.v",
        ),
    ] {
        let (core, _g) = new_core(&format!("sql79m-21000-{label}"));
        seed_double_match(&core);
        let before = snapshot(&core);
        let sql = format!("MERGE INTO t USING s ON t.k = s.k {clauses}");
        for text in with_semicolon(&sql) {
            let (code, msg) = err_code(&core, &text);
            assert_eq!(code, "21000", "{label}: {text}");
            assert_eq!(
                msg, "MERGE command cannot affect row a second time",
                "{label}"
            );
            assert_eq!(before, snapshot(&core), "{label}: {text}");
        }
    }
}

/// 失敗した文を再送しても台帳を消費しない（同じ `21000` のまま。`USING OPERATION_ID` なしの構成）。
#[test]
fn a_failed_merge_can_be_rerun_with_the_same_answer() {
    let (core, _g) = new_core("sql79m-21000-rerun");
    seed_double_match(&core);
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v";
    for _ in 0..3 {
        assert_eq!(err_code(&core, sql).0, "21000");
    }
}

/// 受理する境界: 変更する組が 1 つ以下なら、対象行が元の複数行と一致しても成功する。
#[test]
fn at_most_one_changing_match_per_target_row_is_accepted() {
    for (label, clauses, want) in [
        // 片方が DO NOTHING（先が DO NOTHING で後が UPDATE の順を含む）。
        (
            "nothing-first",
            "WHEN MATCHED AND s.v < 200 THEN DO NOTHING WHEN MATCHED THEN UPDATE SET x = s.v",
            "Integer(1):Text(\"a\"):SignedInteger(500)",
        ),
        (
            "nothing-last",
            "WHEN MATCHED AND s.v > 200 THEN DO NOTHING WHEN MATCHED THEN UPDATE SET x = s.v",
            "Integer(1):Text(\"a\"):SignedInteger(100)",
        ),
        // 片方の条件が偽（どの句も真にならない組は数えない）。
        (
            "cond-false",
            "WHEN MATCHED AND s.v > 200 THEN UPDATE SET x = s.v",
            "Integer(1):Text(\"a\"):SignedInteger(500)",
        ),
    ] {
        let (core, _g) = new_core(&format!("sql79m-ok-{label}"));
        seed_double_match(&core);
        let n = merged(
            &core,
            &format!("MERGE INTO t USING s ON t.k = s.k {clauses}"),
        );
        assert_eq!(n, 1, "{label}");
        assert_eq!(
            rows_of(
                &core,
                "alice",
                "SELECT id, k, x FROM t WHERE id = 1 LIMIT 10"
            ),
            want,
            "{label}"
        );
    }
}

/// 越境（P0）: もう 1 つの一致の候補が他テナントの Private 行のとき、`21000` にならず成功する。他テナントの
/// 対象行は更新・削除されない。
#[test]
fn another_tenants_private_rows_neither_match_nor_cause_21000() {
    let (core, _g) = new_core("sql79m-tenant");
    insert_t(&core, "alice", &[(1, "a", Some(10))]);
    insert_s(&core, "alice", &[(1, "a", Some(100))]);
    // bob の Private 行: 元に k='a' がもう 1 行、対象にも k='a' の行がある。
    insert_t(&core, "bob", &[(11, "a", Some(77))]);
    insert_s(&core, "bob", &[(12, "a", Some(900))]);
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
    );
    assert_eq!(n, 1, "他テナントの行は件数に入らない");
    assert_eq!(
        rows_of(&core, "alice", "SELECT id, x FROM t ORDER BY id LIMIT 10"),
        "Integer(1):SignedInteger(100)"
    );
    assert_eq!(
        rows_of(&core, "bob", "SELECT id, x FROM t ORDER BY id LIMIT 10"),
        "Integer(11):SignedInteger(77)",
        "他テナントの対象行は更新されない"
    );
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
    );
    assert_eq!(n, 1);
    assert_eq!(
        rows_of(&core, "bob", "SELECT id FROM t ORDER BY id LIMIT 10"),
        "Integer(11)",
        "他テナントの対象行は削除されない"
    );
}

// ---------------------------------------------------------------------
// 明示トランザクション
// ---------------------------------------------------------------------

fn in_txn(core: &EngineCore, statements: &[&str]) -> Vec<Result<SqlOutcome, (String, String)>> {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    statements
        .iter()
        .map(|sql| {
            core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, sql)
                .map_err(|e| (e.wire_code().to_string(), e.client_message()))
        })
        .collect()
}

#[test]
fn explicit_transaction_applies_on_commit_and_vanishes_on_rollback() {
    let (core, _g) = new_core("sql79m-txn");
    seed(&core);
    let before = snapshot(&core);
    let merge = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE";
    let out = in_txn(&core, &["BEGIN", merge, "ROLLBACK"]);
    assert!(
        matches!(out[1], Ok(SqlOutcome::Merge(o)) if o.rows_affected == 2),
        "{out:?}"
    );
    assert_eq!(before, snapshot(&core));
    let out = in_txn(&core, &["BEGIN", merge, "COMMIT"]);
    assert!(out.iter().all(Result::is_ok), "{out:?}");
    assert_eq!(t_rows(&core), "Integer(2):Text(\"b\"):SignedInteger(20)");
}

#[test]
fn failed_merge_in_a_transaction_aborts_it_and_read_only_is_25006() {
    let (core, _g) = new_core("sql79m-txn-fail");
    seed_double_match(&core);
    let before = snapshot(&core);
    let out = in_txn(
        &core,
        &[
            "BEGIN",
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
            "SELECT 1",
            "ROLLBACK",
        ],
    );
    assert_eq!(out[1].as_ref().expect_err("21000").0, "21000");
    assert_eq!(out[2].as_ref().expect_err("aborted").0, "25P02");
    assert_eq!(before, snapshot(&core));
    let out = in_txn(
        &core,
        &[
            "BEGIN READ ONLY",
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DO NOTHING",
        ],
    );
    assert_eq!(out[1].as_ref().expect_err("read only").0, "25006");
}

// ---------------------------------------------------------------------
// 元の 3 種類（実表・単純形ビュー・部分クエリ）
// ---------------------------------------------------------------------

fn ddl(core: &EngineCore, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[test]
fn a_simple_view_source_applies_the_view_predicate() {
    let (core, _g) = new_core("sql79m-viewsrc");
    seed(&core);
    ddl(&core, "CREATE VIEW sv AS SELECT k, v FROM s WHERE v > 150");
    let n = merged(
        &core,
        "MERGE INTO t USING sv ON t.k = sv.k WHEN MATCHED THEN UPDATE SET x = sv.v",
    );
    assert_eq!(n, 1, "ビューの述語 v > 150 に合う c だけが一致する");
    assert_eq!(
        t_rows(&core),
        "Integer(1):Text(\"a\"):SignedInteger(10) | Integer(2):Text(\"b\"):SignedInteger(20) | \
         Integer(3):Text(\"c\"):SignedInteger(300)"
    );
}

#[test]
fn a_subquery_source_is_materialized_through_the_read_path() {
    let (core, _g) = new_core("sql79m-subq");
    seed(&core);
    let n = merged(
        &core,
        "MERGE INTO t USING (SELECT k, v + 1 AS w FROM s WHERE v >= 100) AS q ON t.k = q.k \
         WHEN MATCHED THEN UPDATE SET x = q.w",
    );
    assert_eq!(n, 2);
    assert_eq!(
        t_rows(&core),
        "Integer(1):Text(\"a\"):SignedInteger(101) | Integer(2):Text(\"b\"):SignedInteger(20) | \
         Integer(3):Text(\"c\"):SignedInteger(301)"
    );
    // 列別名・集計・定数式の元。
    let n = merged(
        &core,
        "MERGE INTO t USING (SELECT k, count(*) AS c FROM s GROUP BY k) AS g (kk, c) ON t.k = g.kk \
         WHEN MATCHED THEN UPDATE SET x = g.c",
    );
    assert_eq!(n, 2);
    assert_eq!(
        rows_of(&core, "alice", "SELECT id, x FROM t ORDER BY id LIMIT 10"),
        "Integer(1):SignedInteger(1) | Integer(2):SignedInteger(20) | Integer(3):SignedInteger(1)"
    );
    let n = merged(
        &core,
        "MERGE INTO t USING (SELECT 'b' AS k, 5 AS v) AS q ON t.k = q.k WHEN MATCHED THEN DELETE",
    );
    assert_eq!(n, 1);
    // 多重一致は部分クエリの元でも `21000`。
    let before = snapshot(&core);
    let (code, _) = err_code(
        &core,
        "MERGE INTO t USING (SELECT k FROM s UNION ALL SELECT k FROM s) AS q ON t.k = q.k \
         WHEN MATCHED THEN DELETE",
    );
    assert_eq!(code, "21000");
    assert_eq!(before, snapshot(&core));
}

/// 部分クエリの元は書き込むセッションの RLS を受ける（他テナントの Private 行は見えない）。
#[test]
fn a_subquery_source_does_not_see_other_tenants_private_rows() {
    let (core, _g) = new_core("sql79m-subq-tenant");
    insert_t(&core, "alice", &[(1, "a", Some(10))]);
    insert_s(&core, "bob", &[(2, "a", Some(900))]);
    let n = merged(
        &core,
        "MERGE INTO t USING (SELECT k, v FROM s) AS q ON t.k = q.k WHEN MATCHED THEN DELETE",
    );
    assert_eq!(n, 0);
    assert_eq!(t_rows(&core), "Integer(1):Text(\"a\"):SignedInteger(10)");
}

// ---------------------------------------------------------------------
// 対象外の形（固定文言の 0A000）
// ---------------------------------------------------------------------

fn expect_fixed_0a000(core: &EngineCore, sql: &str, message: &str) {
    let before = snapshot(core);
    for text in with_semicolon(sql) {
        let (code, msg) = err_code(core, &text);
        assert_eq!(code, "0A000", "{text}");
        assert_eq!(msg, message, "{text}");
        assert_eq!(before, snapshot(core), "{text}");
    }
}

// WHEN NOT MATCHED の句を含む形は #3354 から実行される（詳細は `sql79_merge_not_matched.rs`）。
// ここでは、MATCHED の句と同居しても MATCHED の結果（件数と行）が変わらないことだけを確かめる。
#[test]
fn matched_results_are_unchanged_when_not_matched_clauses_are_added() {
    let (core, _g) = new_core("sql79m-notmatched");
    seed(&core);
    // t = {1:a/10, 2:b/20, 3:c/30}、s = {1:a/100, 3:c/300, 4:d/400}。
    // MATCHED は a・c を DELETE、BY TARGET は d を INSERT、BY SOURCE は b を DO NOTHING。
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v) \
         WHEN NOT MATCHED BY SOURCE THEN DO NOTHING",
    );
    assert_eq!(n, 3);
    assert_eq!(
        t_rows(&core),
        "Integer(2):Text(\"b\"):SignedInteger(20) | Integer(4):Text(\"d\"):SignedInteger(400)"
    );
}

// `RETURNING` つきは、応答の列と `merge_action()` を入れる #3356 まで拒否する。
// tracked: #3356
#[test]
fn returning_is_rejected_until_its_binding_is_wired() {
    let (core, _g) = new_core("sql79m-returning");
    seed(&core);
    expect_fixed_0a000(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING t.k",
        "feature not supported: MERGE with RETURNING is not supported yet",
    );
}

// 台帳へ記録する構成（`USING OPERATION_ID` つき）は、操作種別と文ハッシュを入れる #3357 まで拒否する。
// 再送しても台帳を消費しない。
// tracked: #3357
#[test]
fn a_ledger_recorded_operation_id_is_rejected_until_the_ledger_is_wired() {
    let (core, _g) = new_core("sql79m-opid");
    seed(&core);
    let before = snapshot(&core);
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE USING OPERATION_ID 'merge-op-1'";
    for _ in 0..2 {
        let (code, msg) = err_code(&core, sql);
        assert_eq!(code, "0A000");
        assert_eq!(
            msg,
            "feature not supported: MERGE with a ledger-recorded operation_id is not supported yet"
        );
        assert_eq!(before, snapshot(&core));
    }
}

// ビューを対象にする MERGE は、基底表へ書き換えて実行する（#3355）。ビュー述語の外の基底行は一致しない。
#[test]
fn a_view_target_applies_the_view_predicate() {
    let (core, _g) = new_core("sql79m-viewtarget");
    seed(&core);
    // t = {1:a/10, 2:b/20, 3:c/30}。b（x = 20）をビューの外へ出す。
    ddl(&core, "CREATE VIEW tv AS SELECT k, x FROM t WHERE x <> 20");
    for text in with_semicolon("MERGE INTO tv USING s ON tv.k = s.k WHEN MATCHED THEN DELETE") {
        let (core, _g) = new_core("sql79m-viewtarget-run");
        seed(&core);
        ddl(&core, "CREATE VIEW tv AS SELECT k, x FROM t WHERE x <> 20");
        assert_eq!(merged(&core, &text), 2, "{text}");
        assert_eq!(t_rows(&core), "Integer(2):Text(\"b\"):SignedInteger(20)");
    }
    // b を元に足しても、ビューの外の行は一致しないので消えない。
    insert_s(&core, "alice", &[(2, "b", Some(200))]);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO tv USING s ON tv.k = s.k WHEN MATCHED THEN DELETE"
        ),
        2
    );
    assert_eq!(t_rows(&core), "Integer(2):Text(\"b\"):SignedInteger(20)");
}

fn trigger_fn(core: &EngineCore) {
    ddl(
        core,
        "CREATE FUNCTION noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$",
    );
}

// 対象表に文トリガがあっても MERGE は実行され、文トリガは操作の種類ごとに 1 回だけ発火する（#3355。
// 発火の順序・回数の網羅は `sql79_merge_rules.rs`）。
#[test]
fn a_statement_trigger_on_the_target_fires_once_per_statement() {
    for text in
        with_semicolon("MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN DELETE")
    {
        let (core, _g) = new_core("sql79m-stmttrigger");
        // トリガは共有表（`Storage` 直作成）に定義できないため、SQL で作る利用者表を使う。
        private_tables(&core);
        ddl(&core, "CREATE TABLE audit (n INTEGER)");
        ddl(
            &core,
            "CREATE FUNCTION cnt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit (n) VALUES (1); RETURN NULL; END $$",
        );
        ddl(
            &core,
            "CREATE TRIGGER ts AFTER DELETE ON s.pt FOR EACH STATEMENT EXECUTE FUNCTION cnt()",
        );
        assert_eq!(merged(&core, &text), 1, "{text}");
        assert_eq!(private_snapshot(&core), "Text(\"b\"):SignedInteger(20)");
        assert_eq!(
            rows_of(&core, "alice", "SELECT n FROM audit LIMIT 10"),
            "SignedInteger(1)",
            "{text}"
        );
    }
}

/// 行トリガの検査用の利用者表（pt が対象、ps が元）。pt = {a:10, b:20}、ps = {a:100}。
fn private_tables(core: &EngineCore) {
    ddl(core, "CREATE SCHEMA s");
    ddl(core, "CREATE TABLE s.pt (k TEXT, x INTEGER)");
    ddl(core, "CREATE TABLE s.ps (k TEXT, v INTEGER)");
    run(core, "INSERT INTO s.pt (k, x) VALUES ('a', 10), ('b', 20)").expect("seed pt");
    run(core, "INSERT INTO s.ps (k, v) VALUES ('a', 100)").expect("seed ps");
}

fn private_snapshot(core: &EngineCore) -> String {
    rows_of(core, "alice", "SELECT k, x FROM s.pt ORDER BY k LIMIT 10")
}

// 対象表の利用者ポリシーは操作ごとに合成して適用される（#3355。網羅は `sql79_merge_rules.rs`）。
// SELECT で隠れる行は一致せず、UPDATE・DELETE の USING を満たさない行を選ぶと文全体が `42501` になる。
#[test]
fn a_user_policy_on_the_target_is_applied_per_operation() {
    let (core, _g) = new_core("sql79m-policy");
    seed(&core);
    ddl(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ddl(&core, "CREATE POLICY p ON t FOR ALL USING (x > 15)");
    // x > 15 の行（b・c）だけが見え、a（10）は一致しない。b・c のうち s に居る c だけが消える。
    for text in with_semicolon("MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE") {
        let (core, _g) = new_core("sql79m-policy-run");
        seed(&core);
        ddl(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
        ddl(&core, "CREATE POLICY p ON t FOR ALL USING (x > 15)");
        assert_eq!(merged(&core, &text), 1, "{text}");
        assert_eq!(
            t_rows(&core),
            "Integer(2):Text(\"b\"):SignedInteger(20)",
            "他は SELECT で隠れる a（10）が消えず、c が消える。{text}"
        );
    }
    // 見える行を BY SOURCE で UPDATE し、新しい行が USING 兼 WITH CHECK を満たさなければ文全体が 42501。
    let before = snapshot(&core);
    for text in with_semicolon(
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = 0",
    ) {
        let (code, _) = err_code(&core, &text);
        assert_eq!(code, "42501", "{text}");
        assert_eq!(before, snapshot(&core), "{text}");
    }
}

// ---------------------------------------------------------------------
// 既存の書き込み経路を通る周辺の規則（行トリガ・生成列・制約）
// ---------------------------------------------------------------------

#[test]
fn row_triggers_fire_through_the_existing_write_paths() {
    let (core, _g) = new_core("sql79m-rowtrigger");
    private_tables(&core);
    ddl(
        &core,
        "CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.x := NEW.x + 1000; RETURN NEW; END $$",
    );
    ddl(
        &core,
        "CREATE TRIGGER tb BEFORE UPDATE ON s.pt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    let n = merged(
        &core,
        "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v",
    );
    assert_eq!(n, 1);
    assert_eq!(
        private_snapshot(&core),
        "Text(\"a\"):SignedInteger(1100) | Text(\"b\"):SignedInteger(20)"
    );
}

/// BEFORE ROW トリガが行を飛ばす（`RETURN NULL`）のは正常な動作で、MERGE は失敗せず実変更件数を返す。
#[test]
fn a_before_row_trigger_skipping_a_row_does_not_fail_the_merge() {
    let (core, _g) = new_core("sql79m-trigger-skip");
    private_tables(&core);
    trigger_fn(&core);
    ddl(
        &core,
        "CREATE TRIGGER tskip BEFORE UPDATE ON s.pt FOR EACH ROW EXECUTE FUNCTION noop()",
    );
    let before = private_snapshot(&core);
    let n = merged(
        &core,
        "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v",
    );
    assert_eq!(n, 0);
    assert_eq!(before, private_snapshot(&core));
}

/// `UPDATE OF` トリガは、各行で選ばれた句の SET 列だけで判定する（別の句の列では発火しない）。
#[test]
fn update_of_triggers_use_the_set_columns_of_the_chosen_clause() {
    let (core, _g) = new_core("sql79m-update-of");
    ddl(&core, "CREATE SCHEMA s");
    ddl(&core, "CREATE TABLE s.q (k TEXT, x INTEGER, y INTEGER)");
    ddl(&core, "CREATE TABLE s.qs (k TEXT, v INTEGER)");
    run(
        &core,
        "INSERT INTO s.q (k, x, y) VALUES ('a', 1, 1), ('b', 2, 2)",
    )
    .expect("seed q");
    run(
        &core,
        "INSERT INTO s.qs (k, v) VALUES ('a', 100), ('b', 10)",
    )
    .expect("seed qs");
    ddl(
        &core,
        "CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.x := NEW.x + 1000; RETURN NEW; END $$",
    );
    ddl(
        &core,
        "CREATE TRIGGER tx BEFORE UPDATE OF x ON s.q FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    let n = merged(
        &core,
        "MERGE INTO s.q AS d USING s.qs AS e ON d.k = e.k \
         WHEN MATCHED AND e.v > 50 THEN UPDATE SET x = e.v \
         WHEN MATCHED THEN UPDATE SET y = e.v",
    );
    assert_eq!(n, 2);
    assert_eq!(
        rows_of(&core, "alice", "SELECT k, x, y FROM s.q ORDER BY k LIMIT 10"),
        "Text(\"a\"):SignedInteger(1100):SignedInteger(1) | Text(\"b\"):SignedInteger(2):SignedInteger(10)"
    );
}

#[test]
fn generated_columns_are_recomputed_by_a_matched_update() {
    let (core, _g) = new_core("sql79m-generated");
    seed(&core);
    ddl(
        &core,
        "CREATE TABLE g (k TEXT, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    run(&core, "INSERT INTO g (k, a) VALUES ('a', 1), ('z', 5)").expect("seed g");
    let n = merged(
        &core,
        "MERGE INTO g USING s ON g.k = s.k WHEN MATCHED THEN UPDATE SET a = s.v",
    );
    assert_eq!(n, 1);
    assert_eq!(
        rows_of(&core, "alice", "SELECT k, a, b FROM g ORDER BY k LIMIT 10"),
        "Text(\"a\"):SignedInteger(100):SignedInteger(200) | Text(\"z\"):SignedInteger(5):SignedInteger(10)"
    );
}

/// NOT NULL への NULL 代入・UNIQUE 違反などは単独の UPDATE と同じコードで、副作用が残らない。
#[test]
fn constraint_violations_roll_back_the_whole_statement() {
    let (core, _g) = new_core("sql79m-constraints");
    ddl(
        &core,
        "CREATE TABLE u (k TEXT NOT NULL, n INTEGER NOT NULL, UNIQUE (n))",
    );
    run(&core, "INSERT INTO u (k, n) VALUES ('a', 1), ('b', 2)").expect("seed u");
    run(
        &core,
        "INSERT INTO s (id, embedding, k, v) VALUES (1, '[0.1,0.2]', 'a', NULL)",
    )
    .expect("seed s");
    let before = rows_of(&core, "alice", "SELECT k, n FROM u ORDER BY k LIMIT 10");
    let (code, _) = err_code(
        &core,
        "MERGE INTO u USING s ON u.k = s.k WHEN MATCHED THEN UPDATE SET n = s.v",
    );
    assert_eq!(code, "23502");
    let (code, _) = err_code(
        &core,
        "MERGE INTO u USING s ON u.k = s.k WHEN MATCHED THEN UPDATE SET n = 2",
    );
    assert_eq!(code, "23505");
    assert_eq!(
        before,
        rows_of(&core, "alice", "SELECT k, n FROM u ORDER BY k LIMIT 10")
    );
}
// ---------------------------------------------------------------------
// 上限（超過は 54000・副作用ゼロ）と台帳の構成
// ---------------------------------------------------------------------

fn limited_core(
    label: &str,
    tweak: impl FnOnce(EngineCore) -> EngineCore,
) -> (EngineCore, CleanupGuard) {
    let (core, guard) = new_core(label);
    (tweak(core), guard)
}

fn dml_limit(n: usize) -> DmlLimits {
    DmlLimits {
        max_affected_rows: std::num::NonZeroUsize::new(n),
        max_insert_rows_per_statement: None,
    }
}

#[test]
fn the_affected_row_limit_counts_updates_and_deletes_together() {
    let sql = "MERGE INTO t USING s ON t.k = s.k \
               WHEN MATCHED AND s.v > 200 THEN UPDATE SET x = 0 WHEN MATCHED THEN DELETE";
    let (core, _g) = limited_core("sql79m-lim-affected-ng", |c| {
        c.with_dml_limits(dml_limit(1))
    });
    seed(&core);
    let before = snapshot(&core);
    for _ in 0..2 {
        assert_eq!(err_code(&core, sql).0, "54000");
        assert_eq!(before, snapshot(&core));
    }
    let (core, _g) = limited_core("sql79m-lim-affected-ok", |c| {
        c.with_dml_limits(dml_limit(2))
    });
    seed(&core);
    assert_eq!(merged(&core, sql), 2);
}

#[test]
fn the_join_pair_limit_is_counted_before_deduplication() {
    // 一致ペアは 2（a と c）。上限 1 は 54000、上限 2 は成功。
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DO NOTHING";
    let (core, _g) = limited_core("sql79m-lim-pairs-ng", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 1))
    });
    seed(&core);
    assert_eq!(err_code(&core, sql).0, "54000");
    let (core, _g) = limited_core("sql79m-lim-pairs-ok", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 2))
    });
    seed(&core);
    assert_eq!(merged(&core, sql), 0);
}

#[test]
fn the_source_visible_row_limit_applies_to_tables_and_subqueries() {
    for sql in [
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
        "MERGE INTO t USING (SELECT k FROM s) AS q ON t.k = q.k WHEN MATCHED THEN DELETE",
    ] {
        let (core, _g) = limited_core("sql79m-lim-input", |c| {
            c.with_join_limits(JoinSizeLimits::new(2, 1, 100_000))
        });
        seed(&core);
        let before = snapshot(&core);
        assert_eq!(err_code(&core, sql).0, "54000", "{sql}");
        assert_eq!(before, snapshot(&core), "{sql}");
    }
}

#[test]
fn the_target_scan_limit_applies_and_leaves_nothing() {
    let (core, _g) = limited_core("sql79m-lim-scan", |c| {
        c.with_scan_limits(engine::scan_limits::ScanLimits::new(2))
    });
    seed(&core);
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE";
    assert_eq!(err_code(&core, sql).0, "54000");
}

/// 台帳の構成: 台帳を持たない構成は実行でき、台帳必須の構成は `operation_id` を欠くと `23502`
/// （`USING OPERATION_ID` を付けた形は #3357 まで固定文言の `0A000`。上のテスト）。
#[test]
fn ledger_modes_without_a_recording_path_follow_the_fail_closed_rules() {
    let (core, _g) = limited_core("sql79m-ledger-compare", |c| {
        c.with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
    });
    seed(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE"
        ),
        2
    );
    let (core, _g) = limited_core("sql79m-ledger-required", |c| {
        c.with_ledger_mode(LedgerMode::Ledgered)
    });
    assert_eq!(
        err_code(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE"
        )
        .0,
        "23502"
    );
}

// ---------------------------------------------------------------------
// 実行しない形（D14。意図的な拒否）
// ---------------------------------------------------------------------

// PL/pgSQL・DO の中の MERGE と、データ変更 CTE の中の MERGE は本 Issue でも実行できない。
// intentional: docs/design/sql-merge.md
// 根拠は同書 D14（受け皿なし・対応しない）。行を書かない。
#[test]
fn merge_inside_do_blocks_and_data_modifying_ctes_stays_rejected() {
    let (core, _g) = new_core("sql79m-d14");
    seed(&core);
    let before = snapshot(&core);
    let merge = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE";
    let mut session = SessionState::default();
    session.allow_ddl();
    for sql in [
        format!("DO $$ BEGIN {merge}; END $$"),
        format!("WITH w AS ({merge}) SELECT 1"),
    ] {
        let err = core
            .execute_sql_in_session(&ctx(), &mut session, &sql)
            .expect_err(&sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}");
    }
    assert_eq!(before, snapshot(&core));
}

// 一時表が関与する MERGE は実行しない（`temp_dispatch` に MERGE が無い）。
// intentional: docs/design/sql-merge.md
// 根拠は同書 D14（受け皿なし）。
#[test]
fn a_temp_table_involvement_stays_rejected_and_writes_nothing() {
    let (core, _g) = new_core("sql79m-temp");
    seed(&core);
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE TEMP TABLE tmp (k TEXT, v INT)",
    )
    .expect("temp table");
    let before = snapshot(&core);
    for sql in [
        "MERGE INTO tmp USING s ON tmp.k = s.k WHEN MATCHED THEN DELETE",
        "MERGE INTO t USING tmp ON t.k = tmp.k WHEN MATCHED THEN DELETE",
    ] {
        let err = core
            .execute_sql_in_session(&ctx(), &mut session, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}");
    }
    assert_eq!(before, snapshot(&core));
}
// ---------------------------------------------------------------------
// 評価の経路の網羅（疑似列 id・自己結合・式を経由しない単純コピー）
// ---------------------------------------------------------------------

/// 元の疑似列 `id` と対象の `id` の等値は、条件内の結合キーとして評価される（元の走査投影に `id` が入る）。
#[test]
fn a_when_condition_can_compare_the_pseudo_columns_id() {
    let (core, _g) = new_core("sql79m-pseudo-id");
    seed(&core);
    // ON は k のみ。t.id = s.id は a（1 = 1）と c（3 = 3）で真。
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED AND t.id = s.id THEN DELETE",
    );
    assert_eq!(n, 2);
    assert_eq!(t_rows(&core), "Integer(2):Text(\"b\"):SignedInteger(20)");
}

/// 対象と元が同じ表（自己結合）でも、適用前に全ての一致を確定してから書く。
#[test]
fn a_self_merge_decides_every_match_before_writing() {
    let (core, _g) = new_core("sql79m-self");
    seed(&core);
    let n = merged(
        &core,
        "MERGE INTO t USING t AS u ON t.id = u.id WHEN MATCHED AND u.x > 15 THEN DELETE",
    );
    assert_eq!(n, 2);
    assert_eq!(t_rows(&core), "Integer(1):Text(\"a\"):SignedInteger(10)");
}

/// 式を経由できない代入先の型（UUID）への元の列の単純コピー。NULL を NOT NULL 列へ入れる組は `23502`。
#[test]
fn a_uuid_column_is_copied_without_going_through_an_expression() {
    let (core, _g) = new_core("sql79m-uuid-copy");
    ddl(&core, "CREATE TABLE ct (k TEXT, u UUID)");
    ddl(&core, "CREATE TABLE cs (k TEXT, u UUID)");
    run(
        &core,
        "INSERT INTO ct (k, u) VALUES ('a', '00000000-0000-0000-0000-000000000001')",
    )
    .expect("seed ct");
    run(
        &core,
        "INSERT INTO cs (k, u) VALUES ('a', '00000000-0000-0000-0000-000000000002')",
    )
    .expect("seed cs");
    let n = merged(
        &core,
        "MERGE INTO ct USING cs ON ct.k = cs.k WHEN MATCHED THEN UPDATE SET u = cs.u",
    );
    assert_eq!(n, 1);
    assert_eq!(
        rows_of(&core, "alice", "SELECT k, u FROM ct ORDER BY k LIMIT 10"),
        rows_of(&core, "alice", "SELECT k, u FROM cs ORDER BY k LIMIT 10")
    );
}

/// 部分クエリの元が結果行数の上限を超えると `54000`（黙って切り詰めず、書き込み前に拒否する）。
#[test]
fn a_subquery_source_over_the_result_row_cap_is_54000_without_side_effects() {
    let (core, _g) = limited_core("sql79m-lim-subq-cap", |c| {
        c.with_result_limits(engine::result_limits::ResultLimits::new(100))
    });
    seed(&core);
    insert_s(
        &core,
        "alice",
        &(10u64..120).map(|i| (i, "z", Some(1))).collect::<Vec<_>>(),
    );
    let before = snapshot(&core);
    let sql = "MERGE INTO t USING (SELECT k FROM s) AS q ON t.k = q.k WHEN MATCHED THEN DELETE";
    assert_eq!(err_code(&core, sql).0, "54000");
    assert_eq!(before, snapshot(&core));
}
