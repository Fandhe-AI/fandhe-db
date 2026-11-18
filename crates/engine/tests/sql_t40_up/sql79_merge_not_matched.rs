//! `MERGE ... WHEN NOT MATCHED [BY TARGET]` の INSERT と `WHEN NOT MATCHED BY SOURCE`（SQL-79・Issue #3354。
//! 親 #3349、設計記録 `docs/design/sql-merge.md` の D4・D6・§10）の SQL 経由の結合テスト。
//!
//! 契約: 元だけにある行（どの対象行にも `ON` が真で一致しなかった元の行。結合キーが NULL の行を含む）は
//! `WHEN NOT MATCHED [BY TARGET]` の句で、元に一致が無い対象行（結合キーが NULL の行を含む）は
//! `WHEN NOT MATCHED BY SOURCE` の句で、それぞれ宣言順に最初に条件が真になった句の操作を受ける。一致があるのに
//! MATCHED の句がどれも発火しない対象行は BY SOURCE を受けない。INSERT は既存の挿入経路（採番・DEFAULT・
//! 生成列・制約・トリガ）を通り、件数は UPDATE・DELETE・INSERT の実変更行数の合計（DO NOTHING は含めない）。
//! 上限（走査・影響行数・INSERT の行数・元の可視行数）は `54000` で、超過した文は何も書かない。他テナントの
//! Private 行は BY SOURCE の DELETE・UPDATE も INSERT の材料も受けない（3 テナントの網羅は `sql79_merge_cross_tenant.rs`）。
//! 期待値は設計記録の決定から導く（PG17 との突き合わせは `sql79_merge_pg17_parity.rs`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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

fn merged(core: &EngineCore, sql: &str) -> u64 {
    match run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Merge(o) => o.rows_affected,
        other => panic!("{sql}: unexpected outcome {other:?}"),
    }
}

fn err_code(core: &EngineCore, sql: &str) -> (String, String) {
    run(core, sql).expect_err(sql)
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
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
/// a・c は一致、b は元に無い（BY SOURCE）、d は対象に無い（BY TARGET）。
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

fn t_rows(core: &EngineCore) -> String {
    rows_of(
        core,
        "alice",
        "SELECT id, k, x FROM t ORDER BY id LIMIT 100",
    )
}

fn snapshot(core: &EngineCore) -> String {
    let s = rows_of(
        core,
        "alice",
        "SELECT id, k, v FROM s ORDER BY id LIMIT 100",
    );
    format!("{}\n{s}", t_rows(core))
}

fn with_semicolon(sql: &str) -> [String; 2] {
    [sql.to_string(), format!("{sql};")]
}

const INSERT_ALL: &str = "INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)";

fn row(id: u64, k: &str, x: Option<i64>) -> String {
    match x {
        Some(x) => format!("Integer({id}):Text(\"{k}\"):SignedInteger({x})"),
        None => format!("Integer({id}):Text(\"{k}\"):Null"),
    }
}

fn join(rows: &[String]) -> String {
    rows.join(" | ")
}

// ---------------------------------------------------------------------
// WHEN NOT MATCHED [BY TARGET] THEN INSERT
// ---------------------------------------------------------------------

#[test]
fn not_matched_inserts_source_only_rows_and_counts_them() {
    for text in with_semicolon(&format!(
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}"
    )) {
        let (core, _g) = new_core("sql79n-insert");
        seed(&core);
        assert_eq!(merged(&core, &text), 1, "{text}");
        assert_eq!(
            t_rows(&core),
            join(&[
                row(1, "a", Some(10)),
                row(2, "b", Some(20)),
                row(3, "c", Some(30)),
                row(4, "d", Some(400)),
            ]),
            "{text}"
        );
    }
}

#[test]
fn by_target_spelling_and_and_conditions_choose_the_clause() {
    for text in with_semicolon(&format!(
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN NOT MATCHED BY TARGET AND s.v > 450 THEN {INSERT_ALL} \
         WHEN NOT MATCHED BY TARGET AND s.v > 100 THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, -1) \
         WHEN NOT MATCHED THEN DO NOTHING"
    )) {
        let (core, _g) = new_core("sql79n-bytarget");
        seed(&core);
        insert_s(&core, "alice", &[(5, "e", Some(500)), (6, "f", Some(50))]);
        // e は先の句、d は 2 番目の句（x = -1）、f は DO NOTHING。
        assert_eq!(merged(&core, &text), 2, "{text}");
        assert_eq!(
            t_rows(&core),
            join(&[
                row(1, "a", Some(10)),
                row(2, "b", Some(20)),
                row(3, "c", Some(30)),
                row(4, "d", Some(-1)),
                row(5, "e", Some(500)),
            ]),
            "{text}"
        );
    }
}

#[test]
fn insert_values_cover_omitted_lists_defaults_constants_and_expressions() {
    // 列リスト省略（宣言順に id・embedding・k・x を取る）。
    let (core, _g) = new_core("sql79n-omitted");
    seed(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
             INSERT VALUES (s.id, s.embedding, s.k, s.v)"
        ),
        1
    );
    assert!(t_rows(&core).ends_with(&row(4, "d", Some(400))));

    // 定数・DEFAULT・元の列を含む式。
    let (core, _g) = new_core("sql79n-exprs");
    seed(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
             INSERT (id, embedding, k, x) VALUES (s.id, '[0.5,0.5]', 'z', s.v * 2 + 1)"
        ),
        1
    );
    assert!(t_rows(&core).ends_with(&row(4, "z", Some(801))));
    assert_eq!(
        rows_of(
            &core,
            "alice",
            "SELECT embedding FROM t WHERE id = 4 LIMIT 1"
        ),
        "Vector([0.5, 0.5])"
    );

    let (core, _g) = new_core("sql79n-default");
    seed(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
             INSERT (id, embedding, k, x) VALUES (7, s.embedding, 'lit', DEFAULT)"
        ),
        1
    );
    assert!(t_rows(&core).ends_with(&row(7, "lit", None)));
}

#[test]
fn a_vector_column_is_copied_from_the_source() {
    let (core, _g) = new_core("sql79n-vector");
    seed(&core);
    run(&core, "UPDATE s SET embedding = '[0.9,0.1]' WHERE id = 4").expect("update s");
    merged(
        &core,
        &format!("MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}"),
    );
    assert_eq!(
        rows_of(
            &core,
            "alice",
            "SELECT embedding FROM t WHERE id = 4 LIMIT 1"
        ),
        "Vector([0.9, 0.1])"
    );
}

// ---------------------------------------------------------------------
// WHEN NOT MATCHED BY SOURCE
// ---------------------------------------------------------------------

#[test]
fn by_source_delete_update_do_nothing_and_conditions() {
    for text in
        with_semicolon("MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN DELETE")
    {
        let (core, _g) = new_core("sql79n-bs-delete");
        seed(&core);
        assert_eq!(merged(&core, &text), 1, "{text}");
        assert_eq!(
            t_rows(&core),
            join(&[row(1, "a", Some(10)), row(3, "c", Some(30))]),
            "{text}"
        );
    }
    let (core, _g) = new_core("sql79n-bs-update");
    seed(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = t.x + 1"
        ),
        1
    );
    assert_eq!(
        t_rows(&core),
        join(&[
            row(1, "a", Some(10)),
            row(2, "b", Some(21)),
            row(3, "c", Some(30)),
        ])
    );
    // AND 条件が偽なら何もしない。DO NOTHING は件数に含めない。
    let (core, _g) = new_core("sql79n-bs-cond");
    seed(&core);
    let before = snapshot(&core);
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE AND t.x > 100 THEN DELETE"
        ),
        0
    );
    assert_eq!(
        merged(
            &core,
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN DO NOTHING"
        ),
        0
    );
    assert_eq!(before, snapshot(&core));
}

#[test]
fn all_three_kinds_can_be_mixed_and_the_count_is_the_sum() {
    for text in with_semicolon(&format!(
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN MATCHED AND s.v > 200 THEN UPDATE SET x = s.v \
         WHEN MATCHED THEN DELETE \
         WHEN NOT MATCHED THEN {INSERT_ALL} \
         WHEN NOT MATCHED BY SOURCE THEN DELETE"
    )) {
        let (core, _g) = new_core("sql79n-mixed");
        seed(&core);
        // a: DELETE、c: UPDATE、d: INSERT、b: BY SOURCE の DELETE。
        assert_eq!(merged(&core, &text), 4, "{text}");
        assert_eq!(
            t_rows(&core),
            join(&[row(3, "c", Some(300)), row(4, "d", Some(400))]),
            "{text}"
        );
    }
}

// ---------------------------------------------------------------------
// NULL キー・重複・MATCHED の句が発火しない対象行
// ---------------------------------------------------------------------

#[test]
fn null_keys_are_unmatched_on_both_sides() {
    let (core, _g) = new_core("sql79n-null");
    insert_t(&core, "alice", &[(1, "a", None), (2, "b", Some(20))]);
    insert_s(&core, "alice", &[(5, "p", None), (6, "q", Some(20))]);
    let n = merged(
        &core,
        &format!(
            "MERGE INTO t USING s ON t.x = s.v WHEN MATCHED THEN DO NOTHING \
             WHEN NOT MATCHED THEN {INSERT_ALL} WHEN NOT MATCHED BY SOURCE THEN DELETE"
        ),
    );
    // NULL キーの元行 5 は INSERT、NULL キーの対象行 1 は BY SOURCE の DELETE。20 は一致。
    assert_eq!(n, 2);
    assert_eq!(
        t_rows(&core),
        join(&[row(2, "b", Some(20)), row(5, "p", None)])
    );
}

#[test]
fn duplicate_unmatched_source_rows_are_both_inserted_unless_a_constraint_stops_them() {
    let (core, _g) = new_core("sql79n-dup");
    seed(&core);
    insert_s(&core, "alice", &[(5, "d", Some(401))]);
    assert_eq!(
        merged(
            &core,
            &format!("MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}")
        ),
        2
    );
    assert!(t_rows(&core).ends_with(&format!(
        "{} | {}",
        row(4, "d", Some(400)),
        row(5, "d", Some(401))
    )));

    // 同じ id を 2 行が挿入しようとする文は、一意性違反で文全体が無効になる。
    let (core, _g) = new_core("sql79n-dup-unique");
    seed(&core);
    insert_s(&core, "alice", &[(5, "d", Some(401))]);
    let before = snapshot(&core);
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
               INSERT (id, embedding, k, x) VALUES (9, s.embedding, s.k, s.v)";
    assert_eq!(err_code(&core, sql).0, "23505");
    assert_eq!(before, snapshot(&core));
}

#[test]
fn a_matched_row_whose_matched_clauses_do_not_fire_does_not_fall_into_by_source() {
    let (core, _g) = new_core("sql79n-nofire");
    seed(&core);
    let before = snapshot(&core);
    // a・c は一致するが条件が偽。b だけが BY SOURCE。d は INSERT の条件が偽。
    let n = merged(
        &core,
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN MATCHED AND s.v < 0 THEN DELETE \
         WHEN NOT MATCHED AND s.v < 0 THEN INSERT (id, embedding, k) VALUES (s.id, s.embedding, s.k) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE",
    );
    assert_eq!(n, 1);
    assert_ne!(before, snapshot(&core));
    assert_eq!(
        t_rows(&core),
        join(&[row(1, "a", Some(10)), row(3, "c", Some(30))])
    );
}

#[test]
fn a_do_nothing_match_still_counts_as_matched() {
    let (core, _g) = new_core("sql79n-donothing-match");
    seed(&core);
    // a・c の MATCHED は DO NOTHING でも「一致」なので、BY TARGET にも BY SOURCE にも落ちない。
    let n = merged(
        &core,
        &format!(
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DO NOTHING \
             WHEN NOT MATCHED THEN {INSERT_ALL}"
        ),
    );
    assert_eq!(n, 1);
    assert_eq!(
        t_rows(&core),
        join(&[
            row(1, "a", Some(10)),
            row(2, "b", Some(20)),
            row(3, "c", Some(30)),
            row(4, "d", Some(400)),
        ])
    );
}

#[test]
fn an_empty_source_sends_every_target_row_to_by_source() {
    let (core, _g) = new_core("sql79n-empty-source");
    insert_t(&core, "alice", &[(1, "a", Some(1)), (2, "b", Some(2))]);
    let n = merged(
        &core,
        &format!(
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL} \
             WHEN NOT MATCHED BY SOURCE THEN DELETE"
        ),
    );
    assert_eq!(n, 2);
    assert_eq!(t_rows(&core), "");
}

#[test]
fn an_empty_target_inserts_every_source_row() {
    let (core, _g) = new_core("sql79n-empty-target");
    insert_s(&core, "alice", &[(1, "a", Some(1)), (2, "b", Some(2))]);
    let n = merged(
        &core,
        &format!("MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}"),
    );
    assert_eq!(n, 2);
    assert_eq!(
        t_rows(&core),
        join(&[row(1, "a", Some(1)), row(2, "b", Some(2))])
    );
}

#[test]
fn a_subquery_source_feeds_the_insert() {
    let (core, _g) = new_core("sql79n-subquery");
    seed(&core);
    let n = merged(
        &core,
        "MERGE INTO t USING (SELECT k, v FROM s WHERE v > 150) AS q ON t.k = q.k \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (99, '[0.1,0.2]', q.k, q.v) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE",
    );
    // q = {c/300, d/400}。c は一致、d は INSERT、a と b は BY SOURCE の DELETE。
    assert_eq!(n, 3);
    assert_eq!(
        t_rows(&core),
        join(&[row(3, "c", Some(30)), row(99, "d", Some(400))])
    );
}

// ---------------------------------------------------------------------
// 書き込み経路（採番・DEFAULT・制約・トリガ）
// ---------------------------------------------------------------------

fn private_tables(core: &EngineCore) {
    ddl(core, "CREATE SCHEMA s");
    ddl(core, "CREATE TABLE s.pt (k TEXT NOT NULL, x INTEGER)");
    ddl(core, "CREATE TABLE s.ps (k TEXT, v INTEGER)");
    run(core, "INSERT INTO s.pt (k, x) VALUES ('a', 10), ('b', 20)").expect("seed pt");
    run(
        core,
        "INSERT INTO s.ps (k, v) VALUES ('a', 100), ('c', 300), ('d', 400)",
    )
    .expect("seed ps");
}

fn pt_rows(core: &EngineCore) -> String {
    rows_of(core, "alice", "SELECT k, x FROM s.pt ORDER BY k LIMIT 20")
}

#[test]
fn ids_are_allocated_by_the_write_path_for_tables_without_a_pseudo_id() {
    for text in with_semicolon(
        "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k \
         WHEN NOT MATCHED THEN INSERT (k, x) VALUES (e.k, e.v)",
    ) {
        let (core, _g) = new_core("sql79n-identity");
        private_tables(&core);
        assert_eq!(merged(&core, &text), 2, "{text}");
        assert_eq!(
            pt_rows(&core),
            "Text(\"a\"):SignedInteger(10) | Text(\"b\"):SignedInteger(20) | \
             Text(\"c\"):SignedInteger(300) | Text(\"d\"):SignedInteger(400)",
            "{text}"
        );
        // 採番した id は重複しない。
        assert_eq!(
            rows_of(&core, "alice", "SELECT COUNT(*) FROM s.pt"),
            "Integer(4)"
        );
    }
}

#[test]
fn default_values_and_column_defaults_are_applied_to_omitted_columns() {
    let (core, _g) = new_core("sql79n-defaults");
    ddl(&core, "CREATE SCHEMA s");
    ddl(
        &core,
        "CREATE TABLE s.dv (k TEXT DEFAULT 'dk', x INTEGER DEFAULT 7)",
    );
    ddl(&core, "CREATE TABLE s.ds (k TEXT)");
    run(&core, "INSERT INTO s.ds (k) VALUES ('p'), ('q')").expect("seed");
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.dv AS d USING s.ds AS e ON d.k = e.k WHEN NOT MATCHED THEN INSERT DEFAULT VALUES"
        ),
        2
    );
    assert_eq!(
        rows_of(&core, "alice", "SELECT k, x FROM s.dv ORDER BY k LIMIT 10"),
        "Text(\"dk\"):SignedInteger(7) | Text(\"dk\"):SignedInteger(7)"
    );
    // 列を挙げて一部だけ与える。残りは DEFAULT。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.dv AS d USING s.ds AS e ON d.k = e.k WHEN NOT MATCHED THEN INSERT (k) VALUES (e.k)"
        ),
        2
    );
    assert_eq!(
        rows_of(&core, "alice", "SELECT k, x FROM s.dv ORDER BY k LIMIT 10"),
        "Text(\"dk\"):SignedInteger(7) | Text(\"dk\"):SignedInteger(7) | \
         Text(\"p\"):SignedInteger(7) | Text(\"q\"):SignedInteger(7)"
    );
}

#[test]
fn a_default_nextval_column_is_sequenced_for_each_inserted_row() {
    let (core, _g) = new_core("sql79n-nextval");
    ddl(&core, "CREATE SCHEMA s");
    ddl(&core, "CREATE SEQUENCE s.sq");
    ddl(
        &core,
        "CREATE TABLE s.nv (k TEXT, n BIGINT DEFAULT nextval('s.sq'))",
    );
    ddl(&core, "CREATE TABLE s.ns (k TEXT)");
    run(&core, "INSERT INTO s.ns (k) VALUES ('p'), ('q'), ('r')").expect("seed");
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.nv AS d USING s.ns AS e ON d.k = e.k WHEN NOT MATCHED THEN INSERT (k) VALUES (e.k)"
        ),
        3
    );
    assert_eq!(
        rows_of(&core, "alice", "SELECT n FROM s.nv ORDER BY n LIMIT 10"),
        "SignedInteger(1) | SignedInteger(2) | SignedInteger(3)"
    );
}

#[test]
fn generated_columns_are_computed_for_inserted_rows() {
    let (core, _g) = new_core("sql79n-generated");
    ddl(&core, "CREATE SCHEMA s");
    ddl(
        &core,
        "CREATE TABLE s.g (k TEXT, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    ddl(&core, "CREATE TABLE s.gs (k TEXT, a INTEGER)");
    run(&core, "INSERT INTO s.gs (k, a) VALUES ('p', 3)").expect("seed");
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.g AS d USING s.gs AS e ON d.k = e.k WHEN NOT MATCHED THEN INSERT (k, a) VALUES (e.k, e.a)"
        ),
        1
    );
    assert_eq!(
        rows_of(&core, "alice", "SELECT k, a, b FROM s.g LIMIT 10"),
        "Text(\"p\"):SignedInteger(3):SignedInteger(6)"
    );
}

#[test]
fn constraint_violations_roll_back_the_whole_statement() {
    let (core, _g) = new_core("sql79n-constraints");
    private_tables(&core);
    run(&core, "INSERT INTO s.ps (k, v) VALUES (NULL, 5)").expect("seed null key");
    let before = pt_rows(&core);
    // NOT NULL（k に NULL）。UPDATE・DELETE が先に適用されていても、文全体が無効になる。
    let (code, _) = err_code(
        &core,
        "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k \
         WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT (k, x) VALUES (e.k, e.v)",
    );
    assert_eq!(code, "23502");
    assert_eq!(before, pt_rows(&core));
}

#[test]
fn check_and_foreign_key_violations_are_rejected_without_side_effects() {
    let (core, _g) = new_core("sql79n-check-fk");
    ddl(&core, "CREATE SCHEMA s");
    ddl(&core, "CREATE TABLE s.parent (id BIGINT PRIMARY KEY)");
    ddl(
        &core,
        "CREATE TABLE s.child (k TEXT, v INTEGER CHECK (v > 0), p BIGINT REFERENCES s.parent (id))",
    );
    ddl(&core, "CREATE TABLE s.cs (k TEXT, v INTEGER, p BIGINT)");
    run(&core, "INSERT INTO s.parent (id) VALUES (1)").expect("seed parent");
    let merge = "MERGE INTO s.child AS d USING s.cs AS e ON d.k = e.k \
                 WHEN NOT MATCHED THEN INSERT (k, v, p) VALUES (e.k, e.v, e.p)";
    run(&core, "INSERT INTO s.cs (k, v, p) VALUES ('a', -1, 1)").expect("seed cs");
    assert_eq!(err_code(&core, merge).0, "23514");
    run(&core, "DELETE FROM s.cs WHERE k = 'a'").expect("clear");
    run(&core, "INSERT INTO s.cs (k, v, p) VALUES ('a', 1, 99)").expect("seed cs");
    assert_eq!(err_code(&core, merge).0, "23503");
    assert_eq!(
        rows_of(&core, "alice", "SELECT COUNT(*) FROM s.child"),
        "Integer(0)"
    );
}

#[test]
fn row_triggers_fire_for_inserted_and_by_source_rows() {
    let (core, _g) = new_core("sql79n-triggers");
    private_tables(&core);
    ddl(
        &core,
        "CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.x := NEW.x + 1000; RETURN NEW; END $$",
    );
    ddl(
        &core,
        "CREATE TRIGGER ti BEFORE INSERT ON s.pt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    ddl(
        &core,
        "CREATE TRIGGER tu BEFORE UPDATE ON s.pt FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    let n = merged(
        &core,
        "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k \
         WHEN NOT MATCHED THEN INSERT (k, x) VALUES (e.k, e.v) \
         WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = d.x",
    );
    // c・d は INSERT（+1000）、b は BY SOURCE の UPDATE（+1000）、a は一致で何もしない。
    assert_eq!(n, 3);
    assert_eq!(
        pt_rows(&core),
        "Text(\"a\"):SignedInteger(10) | Text(\"b\"):SignedInteger(1020) | \
         Text(\"c\"):SignedInteger(1300) | Text(\"d\"):SignedInteger(1400)"
    );
}

// ---------------------------------------------------------------------
// トランザクション
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
    let (core, _g) = new_core("sql79n-txn");
    seed(&core);
    let before = snapshot(&core);
    let merge = format!(
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL} \
         WHEN NOT MATCHED BY SOURCE THEN DELETE"
    );
    let out = in_txn(&core, &["BEGIN", &merge, "ROLLBACK"]);
    assert!(
        matches!(out[1], Ok(SqlOutcome::Merge(o)) if o.rows_affected == 2),
        "{out:?}"
    );
    assert_eq!(before, snapshot(&core));
    let out = in_txn(&core, &["BEGIN", &merge, "COMMIT"]);
    assert!(out.iter().all(Result::is_ok), "{out:?}");
    assert_eq!(
        t_rows(&core),
        join(&[
            row(1, "a", Some(10)),
            row(3, "c", Some(30)),
            row(4, "d", Some(400)),
        ])
    );
}

#[test]
fn a_failed_insert_aborts_the_transaction_and_read_only_is_25006() {
    let (core, _g) = new_core("sql79n-txn-fail");
    seed(&core);
    insert_s(&core, "alice", &[(5, "e", Some(500))]);
    let before = snapshot(&core);
    let dup = "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
               INSERT (id, embedding, k, x) VALUES (9, s.embedding, s.k, s.v)";
    let out = in_txn(&core, &["BEGIN", dup, "SELECT 1", "ROLLBACK"]);
    assert_eq!(out[1].as_ref().expect_err("23505").0, "23505");
    assert_eq!(out[2].as_ref().expect_err("aborted").0, "25P02");
    assert_eq!(before, snapshot(&core));
    let out = in_txn(
        &core,
        &[
            "BEGIN READ ONLY",
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN DELETE",
        ],
    );
    assert_eq!(out[1].as_ref().expect_err("read only").0, "25006");
}

// ---------------------------------------------------------------------
// 上限（超過は 54000・副作用ゼロ）
// ---------------------------------------------------------------------

fn limited_core(
    label: &str,
    tweak: impl FnOnce(EngineCore) -> EngineCore,
) -> (EngineCore, CleanupGuard) {
    let (core, guard) = new_core(label);
    (tweak(core), guard)
}

#[test]
fn the_by_source_target_scan_limit_applies_and_leaves_nothing() {
    let (core, _g) = limited_core("sql79n-lim-scan", |c| {
        c.with_scan_limits(engine::scan_limits::ScanLimits::new(2))
    });
    seed(&core);
    let before = snapshot(&core);
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN DELETE";
    assert_eq!(err_code(&core, sql).0, "54000");
    assert_eq!(before, snapshot(&core));
    // BY TARGET だけの文も、一致の判定のために対象を走査するため同じ上限を受ける。
    let sql = format!("MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}");
    assert_eq!(err_code(&core, &sql).0, "54000");
    assert_eq!(before, snapshot(&core));
}

#[test]
fn the_affected_row_limit_counts_inserts_too() {
    let sql = format!(
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL} \
         WHEN NOT MATCHED BY SOURCE THEN DELETE"
    );
    let (core, _g) = limited_core("sql79n-lim-affected-ng", |c| {
        c.with_dml_limits(DmlLimits {
            max_affected_rows: std::num::NonZeroUsize::new(1),
            max_insert_rows_per_statement: None,
        })
    });
    seed(&core);
    let before = snapshot(&core);
    assert_eq!(err_code(&core, &sql).0, "54000");
    assert_eq!(before, snapshot(&core));
    let (core, _g) = limited_core("sql79n-lim-affected-ok", |c| {
        c.with_dml_limits(DmlLimits {
            max_affected_rows: std::num::NonZeroUsize::new(2),
            max_insert_rows_per_statement: None,
        })
    });
    seed(&core);
    assert_eq!(merged(&core, &sql), 2);
}

#[test]
fn the_insert_row_limit_applies_to_the_merge_insert_rows() {
    let sql = format!("MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}");
    let (core, _g) = limited_core("sql79n-lim-insert-ng", |c| {
        c.with_dml_limits(DmlLimits {
            max_affected_rows: None,
            max_insert_rows_per_statement: std::num::NonZeroUsize::new(1),
        })
    });
    seed(&core);
    insert_s(&core, "alice", &[(5, "e", Some(500))]);
    let before = snapshot(&core);
    assert_eq!(err_code(&core, &sql).0, "54000");
    assert_eq!(before, snapshot(&core));
}

/// INSERT のバッチ上限（④チャンク総量）は句ごとではなく文全体の累計で判定する（INDEX-4・Issue #3354）。
#[test]
fn the_insert_batch_chunk_limit_is_cumulative_across_insert_clauses() {
    let sql = format!(
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN NOT MATCHED AND s.v < 450 THEN {INSERT_ALL} \
         WHEN NOT MATCHED THEN {INSERT_ALL}"
    );
    let limits = |chunks: usize| engine::batch_limits::BatchLimits {
        max_batch_chunks: chunks,
        ..Default::default()
    };
    // 未一致の 2 行（d・e）が別々の句へ振り分けられても、句ごとの 1 行ではなく文全体の 2 行で判定される。
    let (core, _g) = limited_core("sql79n-lim-chunks-ng", |c| c.with_batch_limits(limits(1)));
    seed(&core);
    insert_s(&core, "alice", &[(5, "e", Some(500))]);
    let before = snapshot(&core);
    assert_eq!(err_code(&core, &sql).0, "54000");
    assert_eq!(before, snapshot(&core));
    let (core, _g) = limited_core("sql79n-lim-chunks-ok", |c| c.with_batch_limits(limits(2)));
    seed(&core);
    insert_s(&core, "alice", &[(5, "e", Some(500))]);
    assert_eq!(merged(&core, &sql), 2);
}

#[test]
fn the_source_visible_row_limit_applies() {
    let (core, _g) = limited_core("sql79n-lim-input", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 1, 100_000))
    });
    seed(&core);
    let before = snapshot(&core);
    let sql = format!("MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}");
    assert_eq!(err_code(&core, &sql).0, "54000");
    assert_eq!(before, snapshot(&core));
}

// ---------------------------------------------------------------------
// テナント境界と権限
// ---------------------------------------------------------------------

#[test]
fn another_tenants_private_rows_are_neither_deleted_updated_nor_inserted() {
    let (core, _g) = new_core("sql79n-tenant");
    insert_t(&core, "alice", &[(1, "a", Some(10))]);
    insert_t(&core, "bob", &[(2, "b", Some(20))]);
    insert_s(&core, "alice", &[(3, "c", Some(30))]);
    insert_s(&core, "bob", &[(4, "d", Some(40))]);
    let n = merged(
        &core,
        &format!(
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL} \
             WHEN NOT MATCHED BY SOURCE THEN DELETE"
        ),
    );
    // alice から見える対象は a だけ（BY SOURCE で DELETE）、元は c だけ（INSERT）。bob の行は触れない。
    assert_eq!(n, 2);
    assert_eq!(t_rows(&core), row(3, "c", Some(30)));
    assert_eq!(
        rows_of(&core, "bob", "SELECT id, k, x FROM t ORDER BY id LIMIT 100"),
        row(2, "b", Some(20))
    );
    assert_eq!(
        rows_of(&core, "bob", "SELECT id, k, v FROM s ORDER BY id LIMIT 100"),
        "Integer(4):Text(\"d\"):SignedInteger(40)"
    );
}

// 対象表の INSERT 側の利用者ポリシー（`WITH CHECK`）は、単独の INSERT と同じく新しい行へ掛かる（#3355）。
// 満たす行は挿入され、満たさない行があれば文全体が `42501` になり行は何も書かれない。
#[test]
fn an_insert_policy_on_the_target_checks_the_new_rows() {
    let (core, _g) = new_core("sql79n-policy");
    seed(&core);
    ddl(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ddl(&core, "CREATE POLICY p_sel ON t FOR SELECT USING (true)");
    ddl(&core, "CREATE POLICY p ON t FOR INSERT WITH CHECK (x > 0)");
    // 元の d（400）は WITH CHECK を満たすので挿入される。
    for (i, text) in with_semicolon(&format!(
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}"
    ))
    .into_iter()
    .enumerate()
    {
        let (core, _g) = new_core(&format!("sql79n-policy-ok-{i}"));
        seed(&core);
        ddl(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
        ddl(&core, "CREATE POLICY p_sel ON t FOR SELECT USING (true)");
        ddl(&core, "CREATE POLICY p ON t FOR INSERT WITH CHECK (x > 0)");
        assert_eq!(merged(&core, &text), 1, "{text}");
        assert_eq!(
            t_rows(&core),
            join(&[
                row(1, "a", Some(10)),
                row(2, "b", Some(20)),
                row(3, "c", Some(30)),
                row(4, "d", Some(400)),
            ]),
            "{text}"
        );
    }
    // 元に x <= 0 の行が混ざると、文全体が拒否され、満たす行も書かれない。
    insert_s(&core, "alice", &[(5, "e", Some(-1))]);
    let before = snapshot(&core);
    for text in with_semicolon(&format!(
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN {INSERT_ALL}"
    )) {
        let (code, msg) = err_code(&core, &text);
        assert_eq!(code, "42501", "{text}");
        assert_eq!(
            msg, "new row violates row-level security policy for table \"t\"",
            "{text}"
        );
        assert_eq!(before, snapshot(&core), "{text}");
    }
}

#[test]
fn the_insert_privilege_is_required_for_a_merge_with_an_insert_clause() {
    let (core, _g) = new_core("sql79n-priv");
    let operator = |sql: &str| {
        let mut op = SessionState::default();
        op.bind_role_ids(1, 1);
        op.allow_ddl();
        core.execute_sql_in_session(&ctx(), &mut op, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    };
    let as_bob = |sql: &str| -> String {
        let mut s = SessionState::default();
        s.bind_role_ids(2, 2);
        match core.execute_sql_in_session(&ctx(), &mut s, sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.wire_code().to_string(),
        }
    };
    operator("CREATE ROLE alice");
    operator("CREATE ROLE bob");
    operator("CREATE TABLE tx (k BIGINT, v TEXT)");
    operator("CREATE TABLE sy (k BIGINT, w TEXT)");
    operator("GRANT SELECT ON tx TO bob");
    operator("GRANT SELECT ON sy TO bob");
    operator("GRANT DELETE ON tx TO bob");
    let ins = "MERGE INTO tx USING sy ON tx.k = sy.k WHEN NOT MATCHED THEN INSERT (k, v) VALUES (sy.k, sy.w)";
    let bs = "MERGE INTO tx USING sy ON tx.k = sy.k WHEN NOT MATCHED BY SOURCE THEN DELETE";
    assert_eq!(as_bob(ins), "42501", "INSERT 権限が無い");
    assert_eq!(
        as_bob(bs),
        "ok",
        "BY SOURCE の DELETE は DELETE 権限だけで足りる"
    );
    operator("GRANT INSERT ON tx TO bob");
    assert_eq!(as_bob(ins), "ok");
}

// ---------------------------------------------------------------------
// 列参照のコピー（型が異なる代入・疑似列 id）と DEFAULT 時刻
// ---------------------------------------------------------------------

#[test]
fn column_copies_convert_assignable_types_like_insert_select() {
    let (core, _g) = new_core("sql79n-convert");
    ddl(&core, "CREATE SCHEMA s");
    ddl(&core, "CREATE TABLE s.cn (k TEXT, n NUMERIC(10,2))");
    ddl(&core, "CREATE TABLE s.cs (k TEXT, i INTEGER)");
    run(
        &core,
        "INSERT INTO s.cs (k, i) VALUES ('p', 5), ('q', NULL)",
    )
    .expect("seed");
    // 整数 → NUMERIC は式経路が受けない代入先の型だが、INSERT ... SELECT と同じ代入変換で運ぶ。
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.cn AS d USING s.cs AS e ON d.k = e.k \
             WHEN NOT MATCHED THEN INSERT (k, n) VALUES (e.k, e.i)"
        ),
        2
    );
    assert_eq!(
        rows_of(&core, "alice", "SELECT k, n FROM s.cn ORDER BY k LIMIT 10"),
        rows_of(
            &core,
            "alice",
            "SELECT k, CAST(i AS NUMERIC(10,2)) FROM s.cs ORDER BY k LIMIT 10"
        )
    );
}

#[test]
fn the_source_pseudo_id_is_only_usable_as_the_insert_id() {
    let (core, _g) = new_core("sql79n-pseudo-id");
    seed(&core);
    let before = snapshot(&core);
    let (code, msg) = err_code(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
         INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.id)",
    );
    assert_eq!(code, "0A000");
    assert_eq!(
        msg,
        "feature not supported: MERGE: the pseudo column id of the USING source can be used only as the id value of an INSERT"
    );
    assert_eq!(before, snapshot(&core));
}

#[test]
fn a_null_or_negative_source_id_is_rejected_without_side_effects() {
    let (core, _g) = new_core("sql79n-bad-id");
    seed(&core);
    insert_s(&core, "alice", &[(5, "e", None)]);
    let before = snapshot(&core);
    // 疑似列 id へ NULL の元の列を流す形は `22000`（INSERT ... SELECT と同じ規則）。文全体が無効になる。
    let (code, _) = err_code(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
         INSERT (id, embedding, k, x) VALUES (s.v, s.embedding, s.k, 1)",
    );
    assert_eq!(code, "22000");
    assert_eq!(before, snapshot(&core));
}

#[test]
fn default_now_columns_share_the_statement_clock_in_an_explicit_transaction() {
    let (core, _g) = new_core("sql79n-clock");
    ddl(&core, "CREATE SCHEMA s");
    ddl(
        &core,
        "CREATE TABLE s.ck (k TEXT, at TIMESTAMP DEFAULT now())",
    );
    ddl(&core, "CREATE TABLE s.cks (k TEXT)");
    run(&core, "INSERT INTO s.cks (k) VALUES ('p'), ('q')").expect("seed");
    let out = in_txn(
        &core,
        &[
            "BEGIN",
            "MERGE INTO s.ck AS d USING s.cks AS e ON d.k = e.k WHEN NOT MATCHED THEN INSERT (k) VALUES (e.k)",
            "INSERT INTO s.ck (k) VALUES ('plain')",
            "COMMIT",
        ],
    );
    assert!(out.iter().all(Result::is_ok), "{out:?}");
    // 同じ Tx 内の MERGE の INSERT と単独の INSERT は、DEFAULT now() に同じ（トランザクション開始）時刻を書く。
    assert_eq!(
        rows_of(&core, "alice", "SELECT COUNT(DISTINCT at) FROM s.ck"),
        "Integer(1)"
    );
}

#[test]
fn self_referencing_foreign_key_is_checked_after_all_insert_clauses() {
    let (core, _g) = new_core("sql79n-self-fk");
    ddl(&core, "CREATE SCHEMA s");
    ddl(
        &core,
        "CREATE TABLE s.ft (k INTEGER PRIMARY KEY, p INTEGER REFERENCES s.ft (k))",
    );
    ddl(&core, "CREATE TABLE s.fs (k INTEGER, p INTEGER)");
    // 1 つ目の句の行 (1, 2) が 2 つ目の句の行 (2, NULL) を参照する。文単位の検査と同じく全句の書き込み後に検査する。
    run(&core, "INSERT INTO s.fs (k, p) VALUES (1, 2), (2, NULL)").expect("seed");
    assert_eq!(
        merged(
            &core,
            "MERGE INTO s.ft AS d USING s.fs AS e ON d.k = e.k \
             WHEN NOT MATCHED AND e.k = 1 THEN INSERT (k, p) VALUES (e.k, e.p) \
             WHEN NOT MATCHED THEN INSERT (k, p) VALUES (e.k, e.p)"
        ),
        2
    );
    assert_eq!(
        rows_of(&core, "alice", "SELECT COUNT(*) FROM s.ft"),
        "Integer(2)"
    );
    // 全句の書き込み後も満たされない参照は 23503 で、文は何も書かない。
    ddl(
        &core,
        "CREATE TABLE s.ft2 (k INTEGER PRIMARY KEY, p INTEGER REFERENCES s.ft2 (k))",
    );
    ddl(&core, "CREATE TABLE s.fs2 (k INTEGER, p INTEGER)");
    run(&core, "INSERT INTO s.fs2 (k, p) VALUES (3, 99), (4, NULL)").expect("seed");
    let (code, _) = err_code(
        &core,
        "MERGE INTO s.ft2 AS d USING s.fs2 AS e ON d.k = e.k \
         WHEN NOT MATCHED AND e.k = 3 THEN INSERT (k, p) VALUES (e.k, e.p) \
         WHEN NOT MATCHED THEN INSERT (k, p) VALUES (e.k, e.p)",
    );
    assert_eq!(code, "23503");
    assert_eq!(
        rows_of(&core, "alice", "SELECT COUNT(*) FROM s.ft2"),
        "Integer(0)"
    );
}
