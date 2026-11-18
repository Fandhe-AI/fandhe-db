//! `MERGE` と制約・生成列・トリガ・ビュー・RLS の規則（SQL-79・TABLE-43・TABLE-45・TABLE-50・RLS-13・Issue #3355。
//! 親 #3349、設計記録は `docs/design/sql-merge.md` の D8・D10・D12・§11）の SQL 経由の結合テスト。
//!
//! 契約:
//! - 文トリガは WHEN 句に現れた操作の種類ごとに 1 回（BEFORE は INSERT → UPDATE → DELETE、AFTER は
//!   DELETE → UPDATE → INSERT）。DO NOTHING は種類に数えず、書いた行が 0 件でも発火する。
//! - 利用者ポリシー: SELECT の `USING` で隠れた対象行は「一致なし」。UPDATE・DELETE を選んだ行の `USING` 違反と、
//!   新しい行の `WITH CHECK` 違反は `42501` で文全体を拒否し、行・世代に何も残さない。
//! - ビュー対象: ビュー述語の外の基底行は一致にも BY SOURCE にも入らない。`WITH CHECK OPTION` は `44000`。
//!   INSTEAD OF トリガを持つビューは固定文言の `0A000`（対応しない）。
//! - 生成列・CHECK・UNIQUE・FK は単独の DML と同じ結果。UPDATE 句と INSERT 句をまたぐ自己参照 FK は文単位の検査と同じ意味。
//! - 3 テナントの越境で、他テナントの行は一致・件数・エラーのどれの引き金にもならない。
//!
//! 期待値は設計記録の決定から導く（PG17 との突き合わせは #3358）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

type Failure = (String, String);

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

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, Failure> {
    core.execute_sql_in_session(&ctx_of(tenant), &mut ddl_session(), sql)
        .map_err(|e| (e.wire_code().to_string(), e.client_message()))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, Failure> {
    run_as(core, "alice", sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn merged_as(core: &EngineCore, tenant: &str, sql: &str) -> u64 {
    match run_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Merge(o) => o.rows_affected,
        other => panic!("{sql}: unexpected outcome {other:?}"),
    }
}

fn merged(core: &EngineCore, sql: &str) -> u64 {
    merged_as(core, "alice", sql)
}

fn fails(core: &EngineCore, sql: &str) -> Failure {
    run(core, sql).expect_err(sql)
}

fn rows_as(core: &EngineCore, tenant: &str, sql: &str) -> String {
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

fn rows(core: &EngineCore, sql: &str) -> String {
    rows_as(core, "alice", sql)
}

/// 末尾 `;` なし・ありの両方を流す補助（psql 流の単独送信）。
fn with_semicolon(sql: &str) -> [String; 2] {
    [sql.to_string(), format!("{sql};")]
}

fn put_t(core: &EngineCore, tenant: &str, id: u64, k: &str, x: i32) {
    run_as(
        core,
        tenant,
        &format!("INSERT INTO t (id, embedding, k, x) VALUES ({id}, '[0.1,0.2]', '{k}', {x})"),
    )
    .expect("seed t");
}

fn put_s(core: &EngineCore, tenant: &str, id: u64, k: &str, v: i32) {
    run_as(
        core,
        tenant,
        &format!("INSERT INTO s (id, embedding, k, v) VALUES ({id}, '[0.1,0.2]', '{k}', {v})"),
    )
    .expect("seed s");
}

const T_ALL: &str = "SELECT id, k, x FROM t ORDER BY id LIMIT 100";

// ---------------------------------------------------------------------
// 文トリガ（D12）
// ---------------------------------------------------------------------

/// 対象 `s.pt`（k, x）・元 `s.ps`（k, v）・監査 `audit`（lv）を作る。pt = {a:10, b:20}、ps = {a:100, c:300}。
fn private_world(label: &str) -> (EngineCore, CleanupGuard) {
    let (core, g) = new_core(label);
    ok(&core, "CREATE SCHEMA s");
    ok(&core, "CREATE TABLE s.pt (k TEXT, x INTEGER, y INTEGER)");
    ok(&core, "CREATE TABLE s.ps (k TEXT, v INTEGER)");
    ok(&core, "CREATE TABLE audit (lv TEXT)");
    ok(
        &core,
        "INSERT INTO s.pt (k, x, y) VALUES ('a', 10, 0), ('b', 20, 0)",
    );
    ok(
        &core,
        "INSERT INTO s.ps (k, v) VALUES ('a', 100), ('c', 300)",
    );
    (core, g)
}

/// 文トリガ `<timing> <event> ON s.pt` を、`audit` へ `name` を 1 行書く関数で作る。
fn stmt_trigger(core: &EngineCore, name: &str, timing_event: &str) {
    ok(
        core,
        &format!(
            "CREATE FUNCTION f_{name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit (lv) VALUES ('{name}'); RETURN NULL; END $$"
        ),
    );
    ok(
        core,
        &format!(
            "CREATE TRIGGER g_{name} {timing_event} ON s.pt FOR EACH STATEMENT EXECUTE FUNCTION f_{name}()"
        ),
    );
}

fn all_six_triggers(core: &EngineCore) {
    for (n, te) in [
        ("bi", "BEFORE INSERT"),
        ("bu", "BEFORE UPDATE"),
        ("bd", "BEFORE DELETE"),
        ("ai", "AFTER INSERT"),
        ("au", "AFTER UPDATE"),
        ("ad", "AFTER DELETE"),
    ] {
        stmt_trigger(core, n, te);
    }
}

fn audit(core: &EngineCore) -> String {
    rows(core, "SELECT lv FROM audit LIMIT 100")
}

const MERGE_ALL: &str = "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k \
    WHEN MATCHED THEN UPDATE SET x = e.v \
    WHEN NOT MATCHED BY SOURCE THEN DELETE \
    WHEN NOT MATCHED THEN INSERT (k, x) VALUES (e.k, e.v)";

fn lv(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("Text({n:?})"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn statement_triggers_fire_once_per_kind_in_pg_order() {
    for (i, text) in with_semicolon(MERGE_ALL).into_iter().enumerate() {
        let (core, _g) = private_world(&format!("sql79r-stmt-order-{i}"));
        all_six_triggers(&core);
        // UPDATE は 1 行、DELETE は 1 行、INSERT は 1 行。種類ごとに 1 回ずつ。
        assert_eq!(merged(&core, &text), 3, "{text}");
        assert_eq!(
            audit(&core),
            lv(&["bi", "bu", "bd", "ad", "au", "ai"]),
            "{text}"
        );
        assert_eq!(
            rows(&core, "SELECT k, x FROM s.pt ORDER BY k LIMIT 10"),
            "Text(\"a\"):SignedInteger(100) | Text(\"c\"):SignedInteger(300)"
        );
    }
}

#[test]
fn statement_triggers_fire_once_even_when_many_rows_are_written() {
    let (core, _g) = private_world("sql79r-stmt-many");
    ok(
        &core,
        "INSERT INTO s.pt (k, x, y) VALUES ('c', 1, 0), ('d', 2, 0), ('e', 3, 0)",
    );
    ok(
        &core,
        "INSERT INTO s.ps (k, v) VALUES ('d', 4), ('e', 5), ('f', 6), ('g', 7)",
    );
    all_six_triggers(&core);
    // pt = {a, b, c, d, e}、ps = {a, c, d, e, f, g}。UPDATE は a・c・d・e、DELETE は b、INSERT は f・g。
    assert_eq!(merged(&core, MERGE_ALL), 4 + 1 + 2);
    assert_eq!(audit(&core), lv(&["bi", "bu", "bd", "ad", "au", "ai"]));
}

#[test]
fn only_kinds_named_in_when_clauses_fire_even_if_no_row_is_written() {
    let (core, _g) = private_world("sql79r-stmt-kinds");
    all_six_triggers(&core);
    // DO NOTHING だけの MERGE は種類に数えない。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN DO NOTHING"
        ),
        0
    );
    assert_eq!(audit(&core), "");
    // DELETE 句があるが、条件が偽で 1 行も消さない。DELETE の文トリガは発火する（PG17 と同じ）。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED AND e.v > 1000 THEN DELETE"
        ),
        0
    );
    assert_eq!(audit(&core), lv(&["bd", "ad"]));
}

#[test]
fn statement_triggers_fire_for_an_empty_source() {
    let (core, _g) = private_world("sql79r-stmt-empty");
    all_six_triggers(&core);
    ok(&core, "DELETE FROM s.ps WHERE k = 'a'");
    ok(&core, "DELETE FROM s.ps WHERE k = 'c'");
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v"
        ),
        0
    );
    assert_eq!(audit(&core), lv(&["bu", "au"]));
}

#[test]
fn update_of_statement_triggers_use_the_union_of_all_update_clauses() {
    let (core, _g) = private_world("sql79r-update-of");
    ok(
        &core,
        "CREATE FUNCTION f_uy() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit (lv) VALUES ('uy'); RETURN NULL; END $$",
    );
    ok(
        &core,
        "CREATE TRIGGER g_uy BEFORE UPDATE OF y ON s.pt FOR EACH STATEMENT EXECUTE FUNCTION f_uy()",
    );
    // y を書く句が 1 つでもあれば、選ばれた句が x だけを書いても発火する（和集合）。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k \
             WHEN MATCHED AND e.v > 1000 THEN UPDATE SET y = e.v \
             WHEN MATCHED THEN UPDATE SET x = e.v"
        ),
        1
    );
    assert_eq!(audit(&core), lv(&["uy"]));
    // y を書く句が無ければ発火しない。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v"
        ),
        1
    );
    assert_eq!(audit(&core), lv(&["uy"]));
}

#[test]
fn update_of_statement_triggers_see_a_generated_column_set_to_default() {
    let (core, _g) = new_core("sql79r-update-of-gen");
    ok(&core, "CREATE SCHEMA s");
    ok(
        &core,
        "CREATE TABLE s.pt (k TEXT, a INTEGER, g INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    ok(&core, "CREATE TABLE s.ps (k TEXT, v INTEGER)");
    ok(&core, "CREATE TABLE audit (lv TEXT)");
    ok(&core, "INSERT INTO s.pt (k, a) VALUES ('a', 1)");
    ok(&core, "INSERT INTO s.ps (k, v) VALUES ('a', 5)");
    for (n, timing) in [("bg", "BEFORE"), ("ag", "AFTER")] {
        ok(
            &core,
            &format!(
                "CREATE FUNCTION f_{n}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit (lv) VALUES ('{n}'); RETURN NULL; END $$"
            ),
        );
        ok(
            &core,
            &format!(
                "CREATE TRIGGER g_{n} {timing} UPDATE OF g ON s.pt FOR EACH STATEMENT EXECUTE FUNCTION f_{n}()"
            ),
        );
    }
    // `SET g = DEFAULT` は代入計画から除かれるが、構文上 g を挙げたので UPDATE OF g が発火する。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET g = DEFAULT"
        ),
        1
    );
    assert_eq!(audit(&core), lv(&["bg", "ag"]));
}

#[test]
fn statement_trigger_writes_follow_the_transaction() {
    let (core, _g) = private_world("sql79r-stmt-tx");
    all_six_triggers(&core);
    let run_txn = |statements: &[&str]| {
        let mut session = ddl_session();
        let mut txn = core.new_session_transaction();
        for sql in statements {
            core.execute_sql_in_txn(&ctx_of("alice"), &mut session, &mut txn, sql)
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
    };
    run_txn(&["BEGIN", MERGE_ALL, "ROLLBACK"]);
    assert_eq!(audit(&core), "");
    assert_eq!(
        rows(&core, "SELECT k, x FROM s.pt ORDER BY k LIMIT 10"),
        "Text(\"a\"):SignedInteger(10) | Text(\"b\"):SignedInteger(20)"
    );
    run_txn(&["BEGIN", MERGE_ALL, "COMMIT"]);
    assert_eq!(audit(&core), lv(&["bi", "bu", "bd", "ad", "au", "ai"]));
}

#[test]
fn a_failing_merge_leaves_no_statement_trigger_writes() {
    let (core, _g) = private_world("sql79r-stmt-fail");
    all_six_triggers(&core);
    // 多重一致（21000）は書き込み前の失敗。BEFORE STATEMENT が書いた監査も残らない。
    ok(&core, "INSERT INTO s.ps (k, v) VALUES ('a', 101)");
    let (code, _) = fails(
        &core,
        "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v",
    );
    assert_eq!(code, "21000");
    assert_eq!(audit(&core), "");
}

// ---------------------------------------------------------------------
// 利用者ポリシー（RLS-13）
// ---------------------------------------------------------------------

fn policy_world(label: &str) -> (EngineCore, CleanupGuard) {
    let (core, g) = new_core(label);
    for (id, k, x) in [
        (1, "a", 10),
        (2, "b", 20),
        (3, "c", 30),
        (4, "h", -5),
        (5, "u", 150),
    ] {
        put_t(&core, "alice", id, k, x);
    }
    ok(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ok(&core, "CREATE POLICY p_sel ON t FOR SELECT USING (x > 0)");
    ok(
        &core,
        "CREATE POLICY p_upd ON t FOR UPDATE USING (x < 100) WITH CHECK (x < 500)",
    );
    ok(&core, "CREATE POLICY p_del ON t FOR DELETE USING (x < 25)");
    ok(
        &core,
        "CREATE POLICY p_ins ON t FOR INSERT WITH CHECK (x < 500)",
    );
    (core, g)
}

/// t の中身を、ポリシーを通さず確かめるため RLS を一時的に切らずに、全行が見える `x` の合計で比べる代わりに
/// 状態スナップショットとして行文字列を取る（SELECT のポリシーで隠れる行 `h` は別途確認する）。
fn t_state(core: &EngineCore) -> String {
    rows(core, T_ALL)
}

const MATCH_UPDATE: &str = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v";

#[test]
fn a_row_hidden_by_the_select_policy_is_not_matched() {
    for text in with_semicolon(
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN MATCHED THEN UPDATE SET x = s.v \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)",
    ) {
        let (core, _g) = policy_world("sql79r-pol-hidden");
        // h は SELECT で隠れるため一致せず、BY TARGET の INSERT へ回る（id 9 で新規）。
        put_s(&core, "alice", 9, "h", 7);
        put_s(&core, "alice", 1, "a", 100);
        assert_eq!(merged(&core, &text), 2, "{text}");
        assert_eq!(
            t_state(&core),
            "Integer(1):Text(\"a\"):SignedInteger(100) | Integer(2):Text(\"b\"):SignedInteger(20) | \
             Integer(3):Text(\"c\"):SignedInteger(30) | Integer(5):Text(\"u\"):SignedInteger(150) | \
             Integer(9):Text(\"h\"):SignedInteger(7)",
            "{text}"
        );
    }
}

#[test]
fn update_using_violations_reject_the_whole_statement_without_writing() {
    let (core, _g) = policy_world("sql79r-pol-upd-using");
    put_s(&core, "alice", 1, "a", 100);
    put_s(&core, "alice", 5, "u", 1);
    let before = t_state(&core);
    for text in with_semicolon(MATCH_UPDATE) {
        // u（x = 150）は UPDATE の USING（x < 100）を満たさない。a の更新も取り消される。
        let (code, msg) = fails(&core, &text);
        assert_eq!(code, "42501", "{text}");
        assert!(msg.contains("row-level security"), "{msg}");
        assert_eq!(before, t_state(&core), "{text}");
    }
}

#[test]
fn delete_using_violations_reject_the_whole_statement_without_writing() {
    let (core, _g) = policy_world("sql79r-pol-del-using");
    put_s(&core, "alice", 1, "a", 1);
    put_s(&core, "alice", 3, "c", 1);
    let before = t_state(&core);
    // c（x = 30）は DELETE の USING（x < 25）を満たさない。
    let (code, _) = fails(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
    );
    assert_eq!(code, "42501");
    assert_eq!(before, t_state(&core));
    // a だけなら通る。
    ok(&core, "DELETE FROM s WHERE k = 'c'");
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE"
        ),
        1
    );
}

#[test]
fn by_source_rows_are_also_checked_against_the_using_expressions() {
    let (core, _g) = policy_world("sql79r-pol-bysource");
    put_s(&core, "alice", 1, "a", 1);
    let before = t_state(&core);
    // ソースに無い対象行（b・c・u）が BY SOURCE の DELETE を受ける。c（30）は DELETE の USING を満たさない。
    let (code, _) = fails(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN DELETE",
    );
    assert_eq!(code, "42501");
    assert_eq!(before, t_state(&core));
    // u（150）は UPDATE の USING を満たさない。
    let (code, _) = fails(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = 1",
    );
    assert_eq!(code, "42501");
    assert_eq!(before, t_state(&core));
}

#[test]
fn with_check_violations_are_rejected_for_update_and_insert() {
    let (core, _g) = policy_world("sql79r-pol-check");
    put_s(&core, "alice", 1, "a", 900);
    let before = t_state(&core);
    // UPDATE の WITH CHECK（x < 500）違反。
    let (code, _) = fails(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
    );
    assert_eq!(code, "42501");
    assert_eq!(before, t_state(&core));
    // INSERT の WITH CHECK 違反。
    put_s(&core, "alice", 8, "n", 900);
    let (code, _) = fails(
        &core,
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)",
    );
    assert_eq!(code, "42501");
    assert_eq!(before, t_state(&core));
    // 通るときは件数と行が期待どおり。
    ok(&core, "UPDATE s SET v = 400 WHERE k = 'a'");
    ok(&core, "UPDATE s SET v = 450 WHERE k = 'n'");
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k \
             WHEN MATCHED THEN UPDATE SET x = s.v \
             WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)"
        ),
        2
    );
}

#[test]
fn a_unique_violation_on_insert_is_the_same_as_a_single_insert() {
    let (core, _g) = new_core("sql79r-pol-unique");
    put_t(&core, "alice", 1, "a", 10);
    put_t(&core, "alice", 2, "h", -5);
    ok(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ok(&core, "CREATE POLICY p_all ON t FOR ALL USING (x > 0)");
    // 疑似列 id の重複は SELECT で隠れる行でも一意性違反（23505）になる。
    put_s(&core, "alice", 2, "z", 9);
    let (code, _) = fails(
        &core,
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)",
    );
    assert_eq!(code, "23505");
}

// ---------------------------------------------------------------------
// ビュー対象（TABLE-43）
// ---------------------------------------------------------------------

fn view_world(label: &str) -> (EngineCore, CleanupGuard) {
    let (core, g) = new_core(label);
    put_t(&core, "alice", 1, "a", 10);
    put_t(&core, "alice", 2, "b", -5);
    put_t(&core, "alice", 3, "c", 30);
    put_s(&core, "alice", 1, "a", 100);
    put_s(&core, "alice", 4, "d", 400);
    put_s(&core, "alice", 5, "b", 500);
    ok(
        &core,
        "CREATE VIEW tv AS SELECT id, embedding, k, x FROM t WHERE x > 0",
    );
    (core, g)
}

#[test]
fn view_targets_apply_the_view_predicate_to_every_clause() {
    for text in with_semicolon(
        "MERGE INTO tv USING s ON tv.k = s.k \
         WHEN MATCHED THEN UPDATE SET x = s.v \
         WHEN NOT MATCHED BY SOURCE THEN DELETE \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)",
    ) {
        let (core, _g) = view_world("sql79r-view-pred");
        // a は更新、c（ソースに無い）は削除。b（x = -5）はビューの外なので一致にも BY SOURCE にも入らず、
        // ソースの b は BY TARGET の INSERT へ回る。d も INSERT。
        assert_eq!(merged(&core, &text), 4, "{text}");
        assert_eq!(
            rows(&core, T_ALL),
            "Integer(1):Text(\"a\"):SignedInteger(100) | Integer(2):Text(\"b\"):SignedInteger(-5) | \
             Integer(4):Text(\"d\"):SignedInteger(400) | Integer(5):Text(\"b\"):SignedInteger(500)",
            "{text}"
        );
    }
}

#[test]
fn view_targets_enforce_with_check_option() {
    let (core, _g) = view_world("sql79r-view-check");
    ok(
        &core,
        "CREATE VIEW tvc AS SELECT id, embedding, k, x FROM t WHERE x > 0 WITH CHECK OPTION",
    );
    ok(&core, "UPDATE s SET v = -1 WHERE k = 'a'");
    let before = rows(&core, T_ALL);
    let (code, _) = fails(
        &core,
        "MERGE INTO tvc USING s ON tvc.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
    );
    assert_eq!(code, "44000");
    assert_eq!(before, rows(&core, T_ALL));
    // INSERT の結果行も検査される（ビューの述語を満たさない行は書けない）。a は一致して更新されず（v = -1 は
    // 上の UPDATE が拒否済み）、新規の n（v = -3）が述語に反する。
    ok(&core, "UPDATE s SET v = 100 WHERE k = 'a'");
    put_s(&core, "alice", 6, "n", -3);
    let (code, _) = fails(
        &core,
        "MERGE INTO tvc USING s ON tvc.k = s.k \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)",
    );
    assert_eq!(code, "44000");
    assert_eq!(before, rows(&core, T_ALL));
}

#[test]
fn a_view_with_instead_of_triggers_is_rejected_without_writing() {
    let (core, _g) = private_world("sql79r-view-insteadof");
    ok(
        &core,
        "CREATE VIEW s.pv AS SELECT k, x FROM s.pt WHERE x > 0",
    );
    ok(
        &core,
        "CREATE FUNCTION f_io() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$",
    );
    ok(
        &core,
        "CREATE TRIGGER g_io INSTEAD OF DELETE ON s.pv FOR EACH ROW EXECUTE FUNCTION f_io()",
    );
    let before = rows(&core, "SELECT k, x FROM s.pt ORDER BY k LIMIT 10");
    for text in
        with_semicolon("MERGE INTO s.pv AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN DELETE")
    {
        let (code, msg) = fails(&core, &text);
        assert_eq!(code, "0A000", "{text}");
        assert_eq!(
            msg,
            "feature not supported: MERGE into a view with INSTEAD OF triggers is not supported",
            "{text}"
        );
        assert_eq!(
            before,
            rows(&core, "SELECT k, x FROM s.pt ORDER BY k LIMIT 10")
        );
    }
    // INSTEAD OF の無いイベントだけを使う MERGE は、自動書き換え経路で実行される。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.pv AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v"
        ),
        1
    );
}

#[test]
fn a_view_that_is_not_auto_updatable_stays_55000() {
    let (core, _g) = view_world("sql79r-view-55000");
    ok(&core, "CREATE VIEW tvd AS SELECT DISTINCT k, x FROM t");
    let (code, _) = fails(
        &core,
        "MERGE INTO tvd USING s ON tvd.k = s.k WHEN MATCHED THEN DELETE",
    );
    assert_eq!(code, "55000");
}

#[test]
fn view_targets_run_statement_triggers_of_the_base_table() {
    let (core, _g) = private_world("sql79r-view-stmt");
    ok(
        &core,
        "CREATE VIEW s.pv AS SELECT k, x FROM s.pt WHERE x > 0",
    );
    all_six_triggers(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.pv AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v"
        ),
        1
    );
    assert_eq!(audit(&core), lv(&["bu", "au"]));
}

// ---------------------------------------------------------------------
// 制約・生成列
// ---------------------------------------------------------------------

#[test]
fn generated_columns_are_computed_by_every_operation() {
    let (core, _g) = new_core("sql79r-generated");
    ok(&core, "CREATE SCHEMA s");
    ok(
        &core,
        "CREATE TABLE s.g (k TEXT, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    ok(&core, "INSERT INTO s.g (k, a) VALUES ('x', 1), ('y', 2)");
    put_s(&core, "alice", 1, "x", 10);
    put_s(&core, "alice", 2, "z", 20);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.g AS d USING s AS e ON d.k = e.k \
             WHEN MATCHED THEN UPDATE SET a = e.v \
             WHEN NOT MATCHED THEN INSERT (k, a) VALUES (e.k, e.v)"
        ),
        2
    );
    assert_eq!(
        rows(&core, "SELECT k, a, b FROM s.g ORDER BY k LIMIT 10"),
        "Text(\"x\"):SignedInteger(10):SignedInteger(20) | Text(\"y\"):SignedInteger(2):SignedInteger(4) | \
         Text(\"z\"):SignedInteger(20):SignedInteger(40)"
    );
    // 生成列への代入は単独の DML と同じく拒否する（`428C9`）。
    let (code, _) = fails(
        &core,
        "MERGE INTO s.g AS d USING s AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET b = 1",
    );
    assert_eq!(code, "428C9");
}

#[test]
fn check_and_unique_constraints_apply_to_every_operation() {
    let (core, _g) = new_core("sql79r-constraints");
    ok(&core, "CREATE SCHEMA s");
    ok(
        &core,
        "CREATE TABLE s.c (k TEXT UNIQUE, x INTEGER CHECK (x > 0))",
    );
    ok(&core, "INSERT INTO s.c (k, x) VALUES ('a', 1), ('b', 2)");
    put_s(&core, "alice", 1, "a", -1);
    let before = rows(&core, "SELECT k, x FROM s.c ORDER BY k LIMIT 10");
    let (code, _) = fails(
        &core,
        "MERGE INTO s.c AS d USING s AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v",
    );
    assert_eq!(code, "23514");
    let (code, _) = fails(
        &core,
        "MERGE INTO s.c AS d USING s AS e ON d.k = e.k \
         WHEN NOT MATCHED THEN INSERT (k, x) VALUES (e.k, 5) \
         WHEN MATCHED THEN UPDATE SET k = 'b'",
    );
    assert_eq!(code, "23505");
    let (code, _) = fails(
        &core,
        "MERGE INTO s.c AS d USING s AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET k = 'b'",
    );
    assert_eq!(code, "23505");
    assert_eq!(
        before,
        rows(&core, "SELECT k, x FROM s.c ORDER BY k LIMIT 10")
    );
}

#[test]
fn foreign_keys_across_update_and_insert_clauses_follow_statement_semantics() {
    let (core, _g) = new_core("sql79r-fk-self");
    ok(&core, "CREATE SCHEMA s");
    ok(
        &core,
        "CREATE TABLE s.fk (code INTEGER PRIMARY KEY, parent INTEGER, FOREIGN KEY (parent) REFERENCES s.fk (code))",
    );
    ok(
        &core,
        "INSERT INTO s.fk (code, parent) VALUES (1, NULL), (2, NULL)",
    );
    // ソース: code 2 は parent 3 へ更新（同じ MERGE の INSERT が作る行）、code 3 は新規（parent 1）。
    ok(&core, "CREATE TABLE s.fs (code INTEGER, par INTEGER)");
    ok(&core, "INSERT INTO s.fs (code, par) VALUES (2, 3), (3, 1)");
    let sql = "MERGE INTO s.fk AS d USING s.fs AS e ON d.code = e.code \
         WHEN MATCHED THEN UPDATE SET parent = e.par \
         WHEN NOT MATCHED THEN INSERT (code, parent) VALUES (e.code, e.par)";
    assert_eq!(merged(&core, sql), 2);
    assert_eq!(
        rows(&core, "SELECT code, parent FROM s.fk ORDER BY code LIMIT 10"),
        "SignedInteger(1):Null | SignedInteger(2):SignedInteger(3) | SignedInteger(3):SignedInteger(1)"
    );
    // 存在しない親（9）を参照する更新は 23503 で、何も残らない。
    ok(&core, "DELETE FROM s.fs WHERE code = 3");
    ok(&core, "UPDATE s.fs SET par = 9 WHERE code = 2");
    let before = rows(
        &core,
        "SELECT code, parent FROM s.fk ORDER BY code LIMIT 10",
    );
    let (code, _) = fails(
        &core,
        "MERGE INTO s.fk AS d USING s.fs AS e ON d.code = e.code \
         WHEN MATCHED THEN UPDATE SET parent = e.par",
    );
    assert_eq!(code, "23503");
    assert_eq!(
        before,
        rows(
            &core,
            "SELECT code, parent FROM s.fk ORDER BY code LIMIT 10"
        )
    );
}

// ---------------------------------------------------------------------
// 3 テナントの越境
// ---------------------------------------------------------------------

fn three_tenants(label: &str) -> (EngineCore, CleanupGuard) {
    let (core, g) = new_core(label);
    for (i, tenant) in ["alice", "bob", "carol"].into_iter().enumerate() {
        let base = (i as u64 + 1) * 100;
        put_t(&core, tenant, base + 1, "a", 10 * (i as i32 + 1));
        put_t(&core, tenant, base + 2, "b", 20 * (i as i32 + 1));
        put_s(&core, tenant, base + 1, "a", 1000 * (i as i32 + 1));
        put_s(&core, tenant, base + 3, "z", 7 * (i as i32 + 1));
    }
    (core, g)
}

fn foreign_state(core: &EngineCore) -> String {
    ["bob", "carol"]
        .into_iter()
        .map(|t| {
            format!(
                "{} / {}",
                rows_as(core, t, T_ALL),
                rows_as(core, t, "SELECT id, k, v FROM s ORDER BY id LIMIT 100")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn merge_results_depend_only_on_the_calling_tenants_rows() {
    let (core, _g) = three_tenants("sql79r-tenants");
    let foreign = foreign_state(&core);
    // MATCHED UPDATE・BY SOURCE DELETE・BY TARGET INSERT。alice の a は更新、b は削除、z は挿入。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k \
             WHEN MATCHED THEN UPDATE SET x = s.v \
             WHEN NOT MATCHED BY SOURCE THEN DELETE \
             WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)"
        ),
        3
    );
    assert_eq!(
        rows(&core, T_ALL),
        "Integer(101):Text(\"a\"):SignedInteger(1000) | Integer(103):Text(\"z\"):SignedInteger(7)"
    );
    assert_eq!(foreign, foreign_state(&core));
}

#[test]
fn other_tenants_rows_do_not_trigger_cardinality_or_policy_errors() {
    let (core, _g) = three_tenants("sql79r-tenants-errors");
    // bob・carol に同じキーの行が複数あっても、alice の MERGE は 21000 にならない。
    put_s(&core, "bob", 190, "a", 1);
    put_s(&core, "carol", 191, "a", 2);
    let foreign = foreign_state(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v"
        ),
        1
    );
    // 利用者ポリシーが他テナントの行（alice には見えない）で違反の引き金にならない。
    ok(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ok(
        &core,
        "CREATE POLICY p_all ON t FOR ALL USING (x > 0 AND x < 5000)",
    );
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v"
        ),
        1
    );
    assert_eq!(foreign, foreign_state(&core));
}

#[test]
fn view_and_statement_trigger_variants_do_not_leak_across_tenants() {
    let (core, _g) = three_tenants("sql79r-tenants-view");
    ok(
        &core,
        "CREATE VIEW tv AS SELECT id, embedding, k, x FROM t WHERE x > 0",
    );
    let foreign = foreign_state(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO tv USING s ON tv.k = s.k WHEN NOT MATCHED BY SOURCE THEN DELETE"
        ),
        1
    );
    assert_eq!(
        rows(&core, T_ALL),
        "Integer(101):Text(\"a\"):SignedInteger(10)"
    );
    assert_eq!(foreign, foreign_state(&core));
}

// ---------------------------------------------------------------------
// 権限（D10）: 元側の読み取り
// ---------------------------------------------------------------------

fn role_session(tenant: &str, role_id: u64) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(role_id, role_id);
    assert!(s.bind_resolution_tenant(tenant));
    s
}

fn operator_session(tenant: &str) -> SessionState {
    let mut s = ddl_session();
    assert!(s.bind_resolution_tenant(tenant));
    s
}

#[test]
fn the_source_side_requires_select_privilege() {
    let (core, _g) = new_core("sql79r-privilege");
    let ctx = ctx_of("alice");
    // 役割 own（id 1）が表の所有者、r（id 2）が MERGE を実行する主体。
    let mut op = operator_session("alice");
    for sql in ["CREATE ROLE own", "CREATE ROLE r"] {
        core.execute_sql_in_session(&ctx, &mut op, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let mut owner = role_session("alice", 1);
    owner.allow_ddl();
    for sql in [
        "CREATE TABLE pt (k TEXT, x INTEGER)",
        "CREATE TABLE ps (k TEXT, v INTEGER)",
        "INSERT INTO pt (k, x) VALUES ('a', 10)",
        "INSERT INTO ps (k, v) VALUES ('a', 100)",
        "CREATE VIEW psv AS SELECT k, v FROM ps",
        "GRANT SELECT, UPDATE, DELETE, INSERT ON pt TO r",
    ] {
        core.execute_sql_in_session(&ctx, &mut owner, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let attempt = |sql: &str| {
        let mut s = role_session("alice", 2);
        core.execute_sql_in_session(&ctx, &mut s, sql)
            .map_err(|e| e.wire_code().to_string())
    };
    let sources = [
        "MERGE INTO pt USING ps ON pt.k = ps.k WHEN MATCHED THEN UPDATE SET x = ps.v",
        "MERGE INTO pt USING psv ON pt.k = psv.k WHEN MATCHED THEN UPDATE SET x = psv.v",
        "MERGE INTO pt USING (SELECT k, v FROM ps) AS q ON pt.k = q.k WHEN MATCHED THEN UPDATE SET x = q.v",
        "MERGE INTO pt USING (SELECT k, v FROM ps WHERE v > 0) AS q ON pt.k = q.k \
         WHEN MATCHED THEN UPDATE SET x = q.v",
    ];
    for sql in sources {
        assert_eq!(attempt(sql), Err("42501".to_string()), "{sql}");
    }
    assert_eq!(
        rows(&core, "SELECT k, x FROM pt LIMIT 10"),
        "Text(\"a\"):SignedInteger(10)"
    );
    // 元の表・ビューに SELECT を与えると成功する（権限検査が生きていることの陽性対照）。
    for sql in ["GRANT SELECT ON ps TO r", "GRANT SELECT ON psv TO r"] {
        core.execute_sql_in_session(&ctx, &mut owner, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in sources {
        assert!(attempt(sql).is_ok(), "{sql}: {:?}", attempt(sql));
    }
}
