//! `MERGE` 文の構文解析と実行の固定文言 `0A000`（SQL-79・Issue #3351。親 #3349、設計記録
//! `docs/design/sql-merge.md`）の SQL 経由の結合テスト。
//!
//! 契約: 構文解析を通った MERGE は、autocommit・明示 Tx・Describe のいずれでも固定文言の `0A000` で止まり、
//! 行・台帳・世代に一切触れない（`USING OPERATION_ID` 付きを再送しても `23505` にならない）。従来の
//! `42601` だった形が `0A000` になる。構文エラーは `42601`、上限超過は `54000`、権限は書き込みゲートの
//! `42501`。束縛・実行は #3352〜#3357 の担当（本ファイルは対象外）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, ParsedSql};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const FIXED_MESSAGE: &str = "feature not supported: MERGE is not supported yet";

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

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, sql: &str) -> Result<String, (String, String)> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
        .map_err(|e| (e.wire_code().to_string(), e.client_message()))
}

fn seed(core: &EngineCore) {
    for (id, k, x) in [(1, "a", 10), (2, "b", 20)] {
        run(
            core,
            &format!("INSERT INTO t (id, embedding, k, x) VALUES ({id}, '[0.1,0.2]', '{k}', {x})"),
        )
        .expect("seed t");
    }
    for (id, k, v) in [(1, "a", 100), (3, "c", 300)] {
        run(
            core,
            &format!("INSERT INTO s (id, embedding, k, v) VALUES ({id}, '[0.1,0.2]', '{k}', {v})"),
        )
        .expect("seed s");
    }
}

/// 全表の行（Debug 文字列）。副作用ゼロの検査に使う。
fn snapshot(core: &EngineCore) -> String {
    let t = run(core, "SELECT id, k, x FROM t ORDER BY id LIMIT 100").expect("scan t");
    let s = run(core, "SELECT id, k, v FROM s ORDER BY id LIMIT 100").expect("scan s");
    format!("{t}\n{s}")
}

/// 構文を通る代表形（従来はいずれも `42601`）。
const ACCEPTED: &[&str] = &[
    "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
    "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
    "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN INSERT (k, x) VALUES (s.k, s.v)",
    "MERGE INTO t AS d USING s AS x ON d.k = x.k WHEN MATCHED AND x.v > 1 THEN UPDATE SET x = x.v + 1 \
     WHEN NOT MATCHED THEN INSERT DEFAULT VALUES WHEN NOT MATCHED BY SOURCE THEN DELETE",
    "MERGE INTO t USING (SELECT k, v FROM s) AS q (k, v) ON t.k = q.k WHEN MATCHED THEN DO NOTHING",
    "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING t.k",
];

#[test]
fn sql79_accepted_shapes_stop_with_fixed_0a000_and_no_side_effects() {
    let (core, _g) = new_core("sql79-fixed");
    seed(&core);
    let before = snapshot(&core);
    for sql in ACCEPTED {
        // psql 流の末尾 `;` 付きも同じ。
        for text in [sql.to_string(), format!("{sql};")] {
            let (code, msg) = run(&core, &text).expect_err(&text);
            assert_eq!(
                (code.as_str(), msg.as_str()),
                ("0A000", FIXED_MESSAGE),
                "{text}"
            );
            assert_eq!(before, snapshot(&core), "{text}");
        }
        assert!(matches!(
            core.parse_sql(sql).expect("parse"),
            ParsedSql::Merge(_)
        ));
    }
}

#[test]
fn sql79_operation_id_is_not_consumed_by_the_fixed_rejection() {
    let (core, _g) = new_core("sql79-opid");
    seed(&core);
    let before = snapshot(&core);
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE USING OPERATION_ID 'merge-op-1'";
    for _ in 0..2 {
        let (code, _) = run(&core, sql).expect_err(sql);
        assert_eq!(code, "0A000");
    }
    assert_eq!(before, snapshot(&core));
}

#[test]
fn sql79_explicit_transaction_and_describe_use_the_same_fixed_error() {
    let (core, _g) = new_core("sql79-txn");
    seed(&core);
    let before = snapshot(&core);
    let sql = ACCEPTED[1];

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, sql)
        .expect_err("merge in txn");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(err.client_message(), FIXED_MESSAGE);
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(before, snapshot(&core));

    let parsed = core.parse_sql(sql).expect("parse");
    let err = core
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .expect_err("describe");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(err.client_message(), FIXED_MESSAGE);
}

#[test]
fn sql79_rejections_keep_their_sqlstate() {
    let (core, _g) = new_core("sql79-reject");
    seed(&core);
    let head = "MERGE INTO t USING s ON t.k = s.k";
    let cases: Vec<(String, &str)> = vec![
        // 構文エラー。
        (head.to_string(), "42601"),
        (format!("{head} WHEN MATCHED DELETE"), "42601"),
        (format!("{head} WHEN MATCHED THEN INSERT DEFAULT VALUES"), "42601"),
        (
            format!("{head} WHEN MATCHED THEN DELETE WHEN MATCHED THEN DO NOTHING"),
            "42601",
        ),
        (
            "MERGE INTO t USING (SELECT k FROM s) ON t.k = k WHEN MATCHED THEN DELETE".to_string(),
            "42601",
        ),
        // 値位置の `$n` は構文段が受理しない（束縛は #3352）。リテラル化された ON は受理される。
        (format!("{head} WHEN MATCHED THEN DELETE PARTITIONED"), "0A000"),
        (
            "MERGE INTO t USING s, t AS u ON t.k = s.k WHEN MATCHED THEN DELETE".to_string(),
            "0A000",
        ),
        (
            "MERGE INTO t USING LATERAL (SELECT 1) x ON t.k = x.k WHEN MATCHED THEN DELETE".to_string(),
            "0A000",
        ),
        (
            "WITH w AS (SELECT k FROM s) MERGE INTO t USING w ON t.k = w.k WHEN MATCHED THEN DELETE".to_string(),
            "0A000",
        ),
        (
            "WITH w AS (MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE) SELECT 1".to_string(),
            "0A000",
        ),
        // 上限。
        (
            format!("{head}{}", " WHEN MATCHED AND s.v = 1 THEN DO NOTHING".repeat(17)),
            "54000",
        ),
        // 存在しない表。
        (
            "MERGE INTO nope USING s ON nope.k = s.k WHEN MATCHED THEN DELETE".to_string(),
            "42P01",
        ),
        (
            "MERGE INTO t USING nope ON t.k = nope.k WHEN MATCHED THEN DELETE".to_string(),
            "42P01",
        ),
    ];
    let before = snapshot(&core);
    for (sql, want) in cases {
        let (code, _) = run(&core, &sql).expect_err(&sql);
        assert_eq!(code, want, "{sql}");
    }
    assert_eq!(before, snapshot(&core));
}

#[test]
fn sql79_unknown_schema_matches_the_existing_dml_answer() {
    let (core, _g) = new_core("sql79-schema");
    let merge = run(
        &core,
        "MERGE INTO nosuch.t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
    )
    .expect_err("merge");
    let update =
        run(&core, "UPDATE nosuch.t SET x = s.v FROM s WHERE t.k = s.k").expect_err("update from");
    assert_eq!(
        merge.0, update.0,
        "存在しない SCHEMA の応答は既存の DML と同じ"
    );
}

#[test]
fn sql79_temp_table_involvement_is_0a000() {
    let (core, _g) = new_core("sql79-temp");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE TEMP TABLE tmp (k TEXT, v INT)",
    )
    .expect("temp table");
    for sql in [
        "MERGE INTO tmp USING s ON tmp.k = s.k WHEN MATCHED THEN DELETE",
        "MERGE INTO t USING tmp ON t.k = tmp.k WHEN MATCHED THEN DELETE",
    ] {
        let err = core
            .execute_sql_in_session(&ctx(), &mut session, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}");
    }
}

#[test]
fn sql79_require_operation_id_matches_other_dml() {
    let path = unique_db_path("sql79-require");
    let storage = Storage::open(&path).expect("open storage");
    let _g = CleanupGuard(path);
    for name in ["t", "s"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("k", ColumnType::Text, false)],
            ))
            .expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    let merge = run(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
    )
    .expect_err("merge");
    let delete = run(&core, "DELETE FROM t USING s WHERE t.k = s.k").expect_err("delete");
    assert_eq!(merge.0, "23502");
    assert_eq!(merge.0, delete.0);
}

/// 権限: 書き込みゲートは WHEN に現れた操作の和と SELECT を対象表に課す。足りない主体は `42501`、
/// 満たす主体は構文段を通って固定の `0A000`。
#[test]
fn sql79_privilege_bits_follow_the_when_actions() {
    let (core, _g) = new_core("sql79-priv");
    let tctx = || {
        PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    };
    let operator = |sql: &str| {
        let mut op = SessionState::default();
        op.bind_role_ids(1, 1);
        op.allow_ddl();
        core.execute_sql_in_session(&tctx(), &mut op, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    };
    let as_bob = |sql: &str| -> String {
        let mut s = SessionState::default();
        s.bind_role_ids(2, 2);
        match core.execute_sql_in_session(&tctx(), &mut s, sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.wire_code().to_string(),
        }
    };
    operator("CREATE ROLE alice");
    operator("CREATE ROLE bob");
    operator("CREATE TABLE tx (k BIGINT, v TEXT)");
    operator("CREATE TABLE sy (k BIGINT, w TEXT)");
    let del = "MERGE INTO tx USING sy ON tx.k = sy.k WHEN MATCHED THEN DELETE";
    let both = "MERGE INTO tx USING sy ON tx.k = sy.k WHEN MATCHED THEN DELETE \
                WHEN NOT MATCHED THEN INSERT (k) VALUES (sy.k)";
    assert_eq!(as_bob(del), "42501");
    operator("GRANT SELECT ON tx TO bob");
    operator("GRANT SELECT ON sy TO bob");
    assert_eq!(
        as_bob(del),
        "42501",
        "SELECT だけでは DELETE を含む MERGE を通さない"
    );
    operator("GRANT DELETE ON tx TO bob");
    assert_eq!(as_bob(del), "0A000");
    assert_eq!(as_bob(both), "42501", "INSERT を含む形は INSERT 権限も要る");
    operator("GRANT INSERT ON tx TO bob");
    assert_eq!(as_bob(both), "0A000");
}

/// SET 右辺・VALUES の先頭が `CASE WHEN` の式でも列 `CASE` と誤認されず構文解析を通る（0A000 で止まる）。
#[test]
fn sql79_case_when_expression_in_set_and_values_parses() {
    let (core, _g) = new_core("sql79-case-when");
    seed(&core);
    for sql in [
        "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET x = CASE WHEN s.v > 1 THEN 1 ELSE 2 END",
        "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, '[0.1,0.2]', s.k, CASE WHEN s.v > 1 THEN 1 ELSE 2 END)",
    ] {
        let (code, msg) = run(&core, sql).expect_err(sql);
        assert_eq!((code.as_str(), msg.as_str()), ("0A000", FIXED_MESSAGE), "{sql}");
        assert!(matches!(core.parse_sql(sql).expect("parse"), ParsedSql::Merge(_)));
    }
}

/// 既存の `UPDATE ... FROM` の SET 右辺が `CASE WHEN` で始まる式でも受理される。
#[test]
fn sql79_update_from_set_case_when_is_accepted() {
    let (core, _g) = new_core("sql79-upd-case");
    seed(&core);
    run(
        &core,
        "UPDATE t SET x = CASE WHEN s.v > 150 THEN 1 ELSE 2 END FROM s WHERE t.id = s.id",
    )
    .expect("update from with CASE WHEN");
}
