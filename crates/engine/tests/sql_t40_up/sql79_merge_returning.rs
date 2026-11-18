//! `MERGE ... RETURNING` と `merge_action()`、応答のコマンドタグ（SQL-79・SQL-53・TABLE-43・TABLE-45・TABLE-50・
//! Issue #3356。親 #3349、設計記録 `docs/design/sql-merge.md` の §12）の SQL 経由の結合テスト。
//!
//! 契約: `RETURNING` つきの MERGE は、INSERT・UPDATE は書いた後の値、DELETE は削除前の値を返し、元の列も返せる
//! （元の行が無い BY SOURCE の行では NULL）。`*` は元の全列に続けて対象の全列（PostgreSQL 17 と同じ）。
//! `merge_action()` は `INSERT`／`UPDATE`／`DELETE`（列名 `merge_action`・型 text）。結果行の並びは対象候補の
//! 走査順（UPDATE・DELETE）→ 元の走査順の INSERT。件数（`rows_affected`）は `RETURNING` の有無によらず実変更
//! 行数の合計（DO NOTHING は数えない）。束縛エラー・未対応の形は書き込みより前に返り、副作用ゼロ。
//! 期待値は PostgreSQL 17 の意味論（merge.sgml・回帰テスト merge.out）から導く（完全な突き合わせは #3358）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::ColumnMeta;
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

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, Failure> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx_of(tenant), &mut session, sql)
        .map_err(|e| (e.wire_code().to_string(), e.client_message()))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, Failure> {
    run_as(core, "alice", sql)
}

fn ddl(core: &EngineCore, sql: &str) {
    run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn put(core: &EngineCore, tenant: &str, table: &str, col: &str, rows: &[(u64, &str, i32)]) {
    for (id, k, v) in rows {
        run_as(
            core,
            tenant,
            &format!(
                "INSERT INTO {table} (id, embedding, k, {col}) VALUES ({id}, '[0.1,0.2]', '{k}', {v})"
            ),
        )
        .expect("seed");
    }
}

/// 標準の初期状態: t = {1:a/10, 2:b/20, 3:c/30}、s = {1:a/100, 3:c/300, 4:d/400}。
fn seed(core: &EngineCore) {
    put(
        core,
        "alice",
        "t",
        "x",
        &[(1, "a", 10), (2, "b", 20), (3, "c", 30)],
    );
    put(
        core,
        "alice",
        "s",
        "v",
        &[(1, "a", 100), (3, "c", 300), (4, "d", 400)],
    );
}

/// 応答: (コマンド種別の Debug, 件数, 列名, 行（セルの Debug を `:` で連結）)。
struct Returned {
    command: String,
    affected: u64,
    columns: Vec<String>,
    rows: Vec<String>,
}

fn column_name(c: &ColumnMeta) -> String {
    match c {
        ColumnMeta::Id => "id".to_string(),
        ColumnMeta::AliasedId { name }
        | ColumnMeta::Scalar { name, .. }
        | ColumnMeta::Computed { name, .. }
        | ColumnMeta::Record { name, .. } => name.clone(),
    }
}

fn returned_as(core: &EngineCore, tenant: &str, sql: &str) -> Returned {
    match run_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Returning(o) => Returned {
            command: format!("{:?}", o.command),
            affected: o.rows_affected,
            columns: o.result.columns.iter().map(column_name).collect(),
            rows: o
                .result
                .rows
                .iter()
                .map(|r| {
                    r.cells
                        .iter()
                        .map(|c| format!("{c:?}"))
                        .collect::<Vec<_>>()
                        .join(":")
                })
                .collect(),
        },
        other => panic!("{sql}: unexpected outcome {other:?}"),
    }
}

fn returned(core: &EngineCore, sql: &str) -> Returned {
    returned_as(core, "alice", sql)
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

/// 全表の行。副作用ゼロの検査に使う。
fn snapshot(core: &EngineCore) -> String {
    let t = rows_of(
        core,
        "alice",
        "SELECT id, k, x FROM t ORDER BY id LIMIT 100",
    );
    let s = rows_of(
        core,
        "alice",
        "SELECT id, k, v FROM s ORDER BY id LIMIT 100",
    );
    format!("{t}\n{s}")
}

fn with_semicolon(sql: &str) -> [String; 2] {
    [sql.to_string(), format!("{sql};")]
}

const ALL_CLAUSES: &str = "MERGE INTO t USING s ON t.k = s.k \
     WHEN MATCHED AND s.v > 200 THEN DELETE \
     WHEN MATCHED THEN UPDATE SET x = s.v \
     WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = 0 \
     WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)";

// 4 種の操作を 1 文で混ぜる。UPDATE は更新後、DELETE は削除前、INSERT は書いた値。BY SOURCE の元の列は NULL。
// 並びは対象候補の走査順（id 昇順）→ INSERT（merge.out の RETURNING の例と同じ並び）。
#[test]
fn every_action_returns_its_row_with_merge_action() {
    for text in with_semicolon(&format!(
        "{ALL_CLAUSES} RETURNING merge_action(), t.id, t.k, t.x, s.k AS sk, s.v"
    )) {
        let (core, _g) = new_core("sql79r-all");
        seed(&core);
        let r = returned(&core, &text);
        assert_eq!(r.command, "Merge", "{text}");
        assert_eq!(r.affected, 4, "{text}");
        assert_eq!(
            r.columns,
            ["merge_action", "id", "k", "x", "sk", "v"],
            "{text}"
        );
        assert_eq!(
            r.rows,
            [
                "Text(\"UPDATE\"):Integer(1):Text(\"a\"):SignedInteger(100):Text(\"a\"):SignedInteger(100)",
                "Text(\"UPDATE\"):Integer(2):Text(\"b\"):SignedInteger(0):Null:Null",
                "Text(\"DELETE\"):Integer(3):Text(\"c\"):SignedInteger(30):Text(\"c\"):SignedInteger(300)",
                "Text(\"INSERT\"):Integer(4):Text(\"d\"):SignedInteger(400):Text(\"d\"):SignedInteger(400)",
            ],
            "{text}"
        );
    }
}

// 裸の `*` は元の全列に続けて対象の全列（PostgreSQL 17 の merge.sgml）。`merge_action()` は別名を付けられる。
#[test]
fn star_expands_source_columns_before_target_columns() {
    let (core, _g) = new_core("sql79r-star");
    seed(&core);
    let r = returned(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING *, merge_action() AS act",
    );
    assert_eq!(r.affected, 2);
    assert_eq!(
        r.columns,
        [
            "id",
            "embedding",
            "k",
            "v",
            "id",
            "embedding",
            "k",
            "x",
            "act"
        ]
    );
    assert_eq!(r.rows.len(), 2);
    // 1 行目: 元 s(1:a/100)、対象 t(1:a/10)（削除前の値）。
    assert!(
        r.rows[0].starts_with("Integer(1):")
            && r.rows[0].contains("Text(\"a\"):SignedInteger(100):Integer(1):")
            && r.rows[0].ends_with("Text(\"a\"):SignedInteger(10):Text(\"DELETE\")"),
        "{}",
        r.rows[0]
    );
}

// `t.*`・`s.*`・修飾／非修飾の列・疑似列 `id`・`AS` 別名・対象列だけの式。
#[test]
fn item_shapes_resolve_like_other_joined_dml() {
    let (core, _g) = new_core("sql79r-shapes");
    seed(&core);
    let r = returned(
        &core,
        "MERGE INTO t AS d USING s AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET x = e.v \
         RETURNING e.id AS sid, d.x + 1 AS bumped, x, e.v",
    );
    assert_eq!(r.columns, ["sid", "bumped", "x", "v"]);
    assert_eq!(
        r.rows,
        [
            "Integer(1):SignedInteger(101):SignedInteger(100):SignedInteger(100)",
            "Integer(3):SignedInteger(301):SignedInteger(300):SignedInteger(300)",
        ]
    );
    let r = returned(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DO NOTHING RETURNING t.*",
    );
    assert_eq!(r.affected, 0);
    assert!(r.rows.is_empty());
}

// 件数は RETURNING の有無によらず UPDATE・DELETE・INSERT の実変更行数の合計（DO NOTHING は数えない）。
#[test]
fn the_count_is_the_same_with_and_without_returning() {
    let body = "WHEN MATCHED AND s.v > 200 THEN DELETE WHEN MATCHED THEN DO NOTHING \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)";
    let (core, _g) = new_core("sql79r-count-a");
    seed(&core);
    let plain = match run(&core, &format!("MERGE INTO t USING s ON t.k = s.k {body}")).unwrap() {
        SqlOutcome::Merge(o) => o.rows_affected,
        other => panic!("{other:?}"),
    };
    let (core, _g) = new_core("sql79r-count-b");
    seed(&core);
    let r = returned(
        &core,
        &format!("MERGE INTO t USING s ON t.k = s.k {body} RETURNING merge_action()"),
    );
    assert_eq!(plain, 2);
    assert_eq!(r.affected, plain);
    assert_eq!(r.rows, ["Text(\"DELETE\")", "Text(\"INSERT\")"]);
}

// 元が部分クエリ・ビュー、対象がビュー（公開列だけ）。
#[test]
fn subquery_and_view_sources_and_view_targets() {
    let (core, _g) = new_core("sql79r-views");
    seed(&core);
    let r = returned(
        &core,
        "MERGE INTO t USING (SELECT k, v FROM s) AS q ON t.k = q.k WHEN MATCHED THEN UPDATE SET x = q.v \
         RETURNING t.k, q.v",
    );
    assert_eq!(
        r.rows,
        [
            "Text(\"a\"):SignedInteger(100)",
            "Text(\"c\"):SignedInteger(300)"
        ]
    );
    ddl(&core, "CREATE VIEW sv AS SELECT k, v FROM s WHERE v < 350");
    ddl(&core, "CREATE VIEW tv AS SELECT k, x FROM t WHERE x <> 20");
    let r = returned(
        &core,
        "MERGE INTO tv USING sv ON tv.k = sv.k WHEN MATCHED THEN DELETE RETURNING *",
    );
    // `*` は元ビューの公開列（k, v）に続けて対象ビューの公開列（k, x）。a・c（先の UPDATE で x = 100・300。ビュー述語 x <> 20 を満たす）が消える。
    assert_eq!(r.columns, ["k", "v", "k", "x"]);
    assert_eq!(
        r.rows,
        [
            "Text(\"a\"):SignedInteger(100):Text(\"a\"):SignedInteger(100)",
            "Text(\"c\"):SignedInteger(300):Text(\"c\"):SignedInteger(300)"
        ]
    );
}

fn user_tables(core: &EngineCore) {
    ddl(core, "CREATE SCHEMA s");
    ddl(core, "CREATE SEQUENCE s.sq");
    ddl(
        core,
        "CREATE TABLE s.nv (k TEXT, n BIGINT DEFAULT nextval('s.sq'))",
    );
    ddl(
        core,
        "CREATE TABLE s.g (k TEXT, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    ddl(core, "CREATE TABLE s.src (k TEXT, a INTEGER)");
    run(core, "INSERT INTO s.src (k, a) VALUES ('p', 3), ('q', 4)").expect("seed src");
}

// INSERT は DEFAULT・採番・生成列を反映した最終値を返す（採番方式の表なので行 id は隠れる）。
#[test]
fn inserted_rows_carry_defaults_sequences_and_generated_values() {
    let (core, _g) = new_core("sql79r-final-values");
    user_tables(&core);
    let r = returned(
        &core,
        "MERGE INTO s.nv AS d USING s.src AS e ON d.k = e.k WHEN NOT MATCHED THEN INSERT (k) VALUES (e.k) \
         RETURNING d.k, d.n, merge_action()",
    );
    assert_eq!(r.affected, 2);
    assert_eq!(
        r.rows,
        [
            "Text(\"p\"):SignedInteger(1):Text(\"INSERT\")",
            "Text(\"q\"):SignedInteger(2):Text(\"INSERT\")"
        ]
    );
    let r = returned(
        &core,
        "MERGE INTO s.g AS d USING s.src AS e ON d.k = e.k WHEN NOT MATCHED THEN INSERT (k, a) VALUES (e.k, e.a) \
         RETURNING d.k, d.a, d.b",
    );
    assert_eq!(
        r.rows,
        [
            "Text(\"p\"):SignedInteger(3):SignedInteger(6)",
            "Text(\"q\"):SignedInteger(4):SignedInteger(8)"
        ]
    );
    // 生成列を持つ UPDATE も更新後の値（再計算済み）を返す。
    let r = returned(
        &core,
        "MERGE INTO s.g AS d USING s.src AS e ON d.k = e.k WHEN MATCHED THEN UPDATE SET a = e.a + 10 \
         RETURNING d.k, d.a, d.b",
    );
    assert_eq!(
        r.rows,
        [
            "Text(\"p\"):SignedInteger(13):SignedInteger(26)",
            "Text(\"q\"):SignedInteger(14):SignedInteger(28)"
        ]
    );
}

// 明示 Tx: RETURNING つきが実行でき、ROLLBACK で痕跡が残らない。失敗すると Tx は Failed になる。
#[test]
fn explicit_transactions_run_returning_and_roll_back() {
    let (core, _g) = new_core("sql79r-txn");
    seed(&core);
    let before = snapshot(&core);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx_of("alice"), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(
            &ctx_of("alice"),
            &mut session,
            &mut txn,
            &format!("{ALL_CLAUSES} RETURNING merge_action()"),
        )
        .expect("merge in txn");
    match out {
        SqlOutcome::Returning(o) => {
            assert_eq!(o.rows_affected, 4);
            assert_eq!(o.result.rows.len(), 4);
        }
        other => panic!("{other:?}"),
    }
    core.execute_sql_in_txn(&ctx_of("alice"), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(before, snapshot(&core));

    // COMMIT すれば残る。
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx_of("alice"), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &ctx_of("alice"),
        &mut session,
        &mut txn,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING t.k",
    )
    .expect("merge");
    core.execute_sql_in_txn(&ctx_of("alice"), &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(
        rows_of(&core, "alice", "SELECT k FROM t ORDER BY id LIMIT 10"),
        "Text(\"b\")"
    );
}

// Describe は Execute の結果列と同じ（`merge_action` は text）。
#[test]
fn describe_returns_the_same_columns_as_execute() {
    let (core, _g) = new_core("sql79r-describe");
    seed(&core);
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING t.k, s.v, merge_action()";
    let parsed = core.parse_sql(sql).expect("parse");
    let described: Vec<String> = core
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .expect("describe")
        .expect("columns")
        .iter()
        .map(column_name)
        .collect();
    let executed = returned(&core, sql).columns;
    assert_eq!(described, executed);
    assert_eq!(executed, ["k", "v", "merge_action"]);
    let metas = core
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .unwrap()
        .unwrap();
    assert!(matches!(
        metas.last(),
        Some(ColumnMeta::Scalar {
            ty: ColumnType::Text,
            ..
        })
    ));
}

// 束縛エラーと未対応の形は書き込みより前に返り、副作用ゼロ（台帳も消費しない）。
#[test]
fn rejections_keep_their_sqlstate_and_leave_no_side_effects() {
    let (core, _g) = new_core("sql79r-reject");
    seed(&core);
    let before = snapshot(&core);
    let head = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE";
    let cases: Vec<(String, &str)> = vec![
        // 束縛エラー。
        (format!("{head} RETURNING nope"), "42703"),
        (format!("{head} RETURNING k"), "42702"),
        (format!("{head} RETURNING z.k"), "42P01"),
        (format!("{head} RETURNING count(*)"), "42601"),
        (format!("{head} RETURNING (SELECT 1)"), "42601"),
        (format!("{head} RETURNING $1"), "42601"),
        // 元の列を式の中で参照する形（既存の結合 DML と同じ境界）。
        (format!("{head} RETURNING t.x + s.v"), "0A000"),
        // `merge_action()` は最上位項目だけ。
        (format!("{head} RETURNING merge_action() || 'x'"), "0A000"),
        (format!("{head} RETURNING upper(merge_action())"), "0A000"),
        (format!("{head} RETURNING merge_action(1)"), "0A000"),
        // 台帳記録（#3357）。
        (
            format!("{head} RETURNING t.k USING OPERATION_ID 'op-1'"),
            "0A000",
        ),
        // データ変更 CTE の中の MERGE。
        (
            format!("WITH m AS ({head} RETURNING t.k) SELECT * FROM m"),
            "0A000",
        ),
    ];
    for (sql, code) in cases {
        for text in with_semicolon(&sql) {
            let (got, _) = run(&core, &text).expect_err(&text);
            assert_eq!(got, code, "{text}");
            assert_eq!(before, snapshot(&core), "{text}");
        }
    }
}

// `merge_action()` は MERGE の RETURNING の外では `42601`（`42883` に落とさない）。
#[test]
fn merge_action_outside_a_merge_returning_is_a_syntax_error() {
    let (core, _g) = new_core("sql79r-outside");
    seed(&core);
    let before = snapshot(&core);
    for sql in [
        "SELECT merge_action() FROM t",
        "SELECT k FROM t WHERE merge_action() = 'x'",
        "UPDATE t SET x = x + 1 WHERE k = 'a' RETURNING merge_action()",
        "DELETE FROM t WHERE k = 'a' RETURNING merge_action()",
        "INSERT INTO t (id, embedding, k, x) VALUES (9, '[0.1,0.2]', 'z', 1) RETURNING merge_action()",
        "UPDATE t SET x = s.v FROM s WHERE t.k = s.k RETURNING merge_action()",
        "DELETE FROM t USING s WHERE t.k = s.k RETURNING merge_action()",
        "MERGE INTO t USING s ON merge_action() = 'x' WHEN MATCHED THEN DELETE",
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED AND merge_action() = 'x' THEN DELETE",
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = merge_action()",
    ] {
        let (code, _) = run(&core, sql).expect_err(sql);
        assert_eq!(code, "42601", "{sql}");
        assert_eq!(before, snapshot(&core), "{sql}");
    }
    // 列参照としての `merge_action`（括弧なし）は従来どおり未知列。
    let (code, _) = run(&core, "SELECT merge_action FROM t").expect_err("column");
    assert_eq!(code, "42703");
}

// 該当トリガを持つ表との併用は固定文言の `0A000`。BEFORE STATEMENT が書いた行も残さない。
#[test]
fn returning_on_a_table_with_triggers_is_rejected_before_any_write() {
    let (core, _g) = new_core("sql79r-trigger");
    ddl(&core, "CREATE SCHEMA s");
    ddl(&core, "CREATE TABLE s.pt (k TEXT, x INTEGER)");
    ddl(&core, "CREATE TABLE s.ps (k TEXT, v INTEGER)");
    ddl(&core, "CREATE TABLE audit (n INTEGER)");
    ddl(
        &core,
        "CREATE FUNCTION cnt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit (n) VALUES (1); RETURN NULL; END $$",
    );
    ddl(
        &core,
        "CREATE TRIGGER tb BEFORE DELETE ON s.pt FOR EACH STATEMENT EXECUTE FUNCTION cnt()",
    );
    run(&core, "INSERT INTO s.pt (k, x) VALUES ('a', 10), ('b', 20)").expect("seed pt");
    run(&core, "INSERT INTO s.ps (k, v) VALUES ('a', 100)").expect("seed ps");
    let sql =
        "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN DELETE RETURNING d.k";
    for text in with_semicolon(sql) {
        let (code, msg) = run(&core, &text).expect_err(&text);
        assert_eq!(code, "0A000", "{text}");
        assert_eq!(
            msg,
            "feature not supported: MERGE with RETURNING on a table with triggers is not supported",
            "{text}"
        );
        assert_eq!(
            rows_of(&core, "alice", "SELECT n FROM audit LIMIT 10"),
            "",
            "{text}"
        );
        assert_eq!(
            rows_of(&core, "alice", "SELECT k FROM s.pt ORDER BY k LIMIT 10"),
            "Text(\"a\"):Text(\"b\")".replace(':', " | "),
            "{text}"
        );
    }
    // RETURNING なしなら従来どおり実行される（トリガの契約は変えない）。
    run(
        &core,
        "MERGE INTO s.pt AS d USING s.ps AS e ON d.k = e.k WHEN MATCHED THEN DELETE",
    )
    .expect("without RETURNING");
}

// テナント境界: 他テナントの行は返却行にも件数にも現れない。
#[test]
fn other_tenants_rows_never_appear_in_the_result() {
    let (core, _g) = new_core("sql79r-tenant");
    put(&core, "alice", "t", "x", &[(1, "a", 10)]);
    put(&core, "bob", "t", "x", &[(2, "a", 20), (3, "b", 30)]);
    put(&core, "alice", "s", "v", &[(1, "a", 100)]);
    put(&core, "bob", "s", "v", &[(2, "b", 200)]);
    let r = returned(
        &core,
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN MATCHED THEN UPDATE SET x = s.v \
         WHEN NOT MATCHED BY SOURCE THEN DELETE \
         RETURNING merge_action(), t.k, t.x, s.v",
    );
    assert_eq!(r.affected, 1);
    assert_eq!(
        r.rows,
        ["Text(\"UPDATE\"):Text(\"a\"):SignedInteger(100):SignedInteger(100)"]
    );
    // bob の行は触られていない。
    assert_eq!(
        rows_of(&core, "bob", "SELECT k, x FROM t ORDER BY id LIMIT 10"),
        "Text(\"a\"):SignedInteger(20) | Text(\"b\"):SignedInteger(30)"
    );
}

// 利用者ポリシーで隠れた対象行は一致にも返却にも入らない。
#[test]
fn rows_hidden_by_a_select_policy_are_not_returned() {
    let (core, _g) = new_core("sql79r-policy");
    put(&core, "alice", "t", "x", &[(1, "a", 10), (2, "h", -5)]);
    put(&core, "alice", "s", "v", &[(1, "a", 100), (2, "h", 7)]);
    ddl(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ddl(&core, "CREATE POLICY p_sel ON t FOR SELECT USING (x > 0)");
    ddl(
        &core,
        "CREATE POLICY p_upd ON t FOR UPDATE USING (true) WITH CHECK (true)",
    );
    let r = returned(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v RETURNING t.k, t.x",
    );
    assert_eq!(r.affected, 1);
    assert_eq!(r.rows, ["Text(\"a\"):SignedInteger(100)"]);
}
