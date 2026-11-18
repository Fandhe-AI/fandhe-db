//! `MERGE` 文の構文解析と、実行に結線されていない形の固定文言 `0A000`（SQL-79・Issue #3351・#3353・#3354。
//! 親 #3349、設計記録 `docs/design/sql-merge.md`）の SQL 経由の結合テスト。
//!
//! 契約: 構文解析と束縛（#3352。`sql79_merge_bind.rs`）を通った MERGE のうち、`WHEN MATCHED` は #3353 から、
//! `WHEN NOT MATCHED [BY TARGET|BY SOURCE]` は #3354 から、`RETURNING` は #3356 から実行される（行・件数は
//! `sql79_merge_matched.rs`・`sql79_merge_not_matched.rs`・`sql79_merge_returning.rs`）。台帳記録の形は固定文言の
//! `0A000` で止まり、行・台帳・世代に一切触れない（`USING OPERATION_ID` 付きを再送しても `23505` にならない）。
//! 構文エラーは `42601`、上限超過は `54000`、権限は書き込みゲートの `42501`。受け皿は #3357（台帳）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, ParsedSql};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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

/// 構文を通り、#3353 から実行される代表形（従来はいずれも `42601`。`WHEN MATCHED` だけ）。
const EXECUTED: &[&str] = &[
    "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
    "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE",
    "MERGE INTO t AS d USING s AS x ON d.k = x.k WHEN MATCHED AND x.v > 1 THEN UPDATE SET x = x.v + 1",
    "MERGE INTO t USING (SELECT k, v FROM s) AS q (k, v) ON t.k = q.k WHEN MATCHED THEN DO NOTHING",
];

/// 構文を通り、#3354 から実行される NOT MATCHED を含む形と、実行後の対象表の内容・件数
/// （初期状態は `seed`: t = {1:a/10, 2:b/20}、s = {1:a/100, 3:c/300}）。
const NOT_MATCHED_SHAPES: &[(&str, &str, &[&str])] = &[
    (
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
         INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v)",
        "Merge(MergeOutcome { rows_affected: 1 })",
        &[
            "Integer(1), Text(\"a\"), SignedInteger(10)",
            "Integer(2), Text(\"b\"), SignedInteger(20)",
            "Integer(3), Text(\"c\"), SignedInteger(300)",
        ],
    ),
    (
        "MERGE INTO t AS d USING s AS x ON d.k = x.k WHEN MATCHED AND x.v > 1 THEN UPDATE SET x = x.v + 1 \
         WHEN NOT MATCHED THEN INSERT (id, embedding, k) VALUES (x.id, x.embedding, x.k) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE",
        "Merge(MergeOutcome { rows_affected: 3 })",
        &[
            "Integer(1), Text(\"a\"), SignedInteger(101)",
            "Integer(3), Text(\"c\"), Null",
        ],
    ),
];

/// `RETURNING` つきの形（#3356 から実行される）。
const RETURNING_SHAPE: &str =
    "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE RETURNING t.k";

#[test]
fn sql79_matched_only_shapes_parse_and_run() {
    for sql in EXECUTED {
        // psql 流の末尾 `;` 付きも同じ。
        for text in [sql.to_string(), format!("{sql};")] {
            let (core, _g) = new_core("sql79-run");
            seed(&core);
            let out = run(&core, &text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            assert!(out.starts_with("Merge("), "{text}: {out}");
        }
        let (core, _g) = new_core("sql79-parse");
        assert!(matches!(
            core.parse_sql(sql).expect("parse"),
            ParsedSql::Merge(_)
        ));
    }
}

// NOT MATCHED を含む形は #3354 から実行され、件数と実行後の行が決まる（末尾 `;` 付きも同じ）。
#[test]
fn sql79_not_matched_shapes_parse_and_run() {
    for (sql, outcome, rows) in NOT_MATCHED_SHAPES {
        for text in [sql.to_string(), format!("{sql};")] {
            let (core, _g) = new_core("sql79-nm-run");
            seed(&core);
            let out = run(&core, &text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            assert_eq!(out, *outcome, "{text}");
            let t = run(&core, "SELECT id, k, x FROM t ORDER BY id LIMIT 100").expect("scan t");
            for r in *rows {
                assert!(
                    t.contains(&format!("cells: [{r}]")),
                    "{text}: missing {r} in {t}"
                );
            }
            // 期待した行数と一致する（余計な行が残っていない）。
            assert_eq!(t.matches("ResultRow").count(), rows.len(), "{text}: {t}");
        }
        let (core, _g) = new_core("sql79-nm-parse");
        assert!(matches!(
            core.parse_sql(sql).expect("parse"),
            ParsedSql::Merge(_)
        ));
    }
}

// `RETURNING` つきは #3356 から実行され、変更した行を返す（末尾 `;` 付きも同じ）。
#[test]
fn sql79_returning_runs_and_returns_the_changed_rows() {
    for text in [RETURNING_SHAPE.to_string(), format!("{RETURNING_SHAPE};")] {
        let (core, _g) = new_core("sql79-returning");
        seed(&core);
        let out = run(&core, &text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        assert!(out.starts_with("Returning("), "{text}: {out}");
        assert!(out.contains("command: Merge"), "{text}: {out}");
        assert!(out.contains("rows_affected: 1"), "{text}: {out}");
        assert!(out.contains("Text(\"a\")"), "{text}: {out}");
        let t = run(&core, "SELECT id, k, x FROM t ORDER BY id LIMIT 100").expect("scan t");
        assert_eq!(t.matches("ResultRow").count(), 1, "{text}: {t}");
    }
}

// 台帳へ記録する構成（`USING OPERATION_ID` つき）は、操作種別と文ハッシュを入れる #3357 まで拒否する。
// tracked: #3357
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
fn sql79_explicit_transaction_runs_and_describe_binds() {
    let (core, _g) = new_core("sql79-txn");
    seed(&core);
    let before = snapshot(&core);
    let sql = EXECUTED[1];

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, sql)
        .expect("merge in txn");
    assert!(format!("{out:?}").starts_with("Merge("), "{out:?}");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(before, snapshot(&core));

    // Describe は Execute と同じ束縛を通る。`RETURNING` なしは結果列なし（拡張クエリの Parse／Describe が
    // ParameterDescription を返せる）、`RETURNING` つきは Execute の結果列と同じ列（#3356）。
    let parsed = core.parse_sql(sql).expect("parse");
    assert_eq!(
        core.describe_parsed_in_session(&SessionState::default(), &parsed)
            .expect("describe without RETURNING"),
        None
    );
    let parsed = core.parse_sql(RETURNING_SHAPE).expect("parse");
    let cols = core
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .expect("describe with RETURNING")
        .expect("RETURNING columns");
    assert_eq!(cols.len(), 1);
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
        // 非引用の `WHEN` は対象表の別名にできない。
        (
            "MERGE INTO t WHEN USING s ON t.k = s.k WHEN MATCHED THEN DELETE".to_string(),
            "42601",
        ),
        (
            "MERGE INTO t AS WHEN USING s ON t.k = s.k WHEN MATCHED THEN DELETE".to_string(),
            "42601",
        ),
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
/// 満たす主体は構文段を通って実行される（INSERT を含む形も INSERT 権限があれば実行される）。
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
    assert_eq!(
        as_bob(del),
        "ok",
        "権限を満たす主体は MATCHED だけの形を実行できる"
    );
    assert_eq!(as_bob(both), "42501", "INSERT を含む形は INSERT 権限も要る");
    operator("GRANT INSERT ON tx TO bob");
    assert_eq!(
        as_bob(both),
        "ok",
        "権限を満たす主体は NOT MATCHED の INSERT を含む形も実行できる"
    );
}

/// SET 右辺・VALUES の先頭が `CASE WHEN` の式でも列 `CASE` と誤認されず構文解析を通る
/// （UPDATE も NOT MATCHED の INSERT も実行される）。
#[test]
fn sql79_case_when_expression_in_set_and_values_parses() {
    let (core, _g) = new_core("sql79-case-when");
    seed(&core);
    let update = "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET x = CASE WHEN s.v > 1 THEN 1 ELSE 2 END";
    let out = run(&core, update).expect("update with CASE WHEN");
    assert!(out.starts_with("Merge("), "{out}");
    assert!(matches!(
        core.parse_sql(update).expect("parse"),
        ParsedSql::Merge(_)
    ));
    let insert = "MERGE INTO t USING s ON t.id = s.id WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, '[0.1,0.2]', s.k, CASE WHEN s.v > 1 THEN 1 ELSE 2 END)";
    // 更新で s.id = 1 は一致し、s.id = 3 だけが未一致（x = CASE WHEN 300 > 1 THEN 1 ...）として挿入される。
    let out = run(&core, insert).unwrap_or_else(|e| panic!("{insert}: {e:?}"));
    assert_eq!(out, "Merge(MergeOutcome { rows_affected: 1 })", "{insert}");
    let t = run(&core, "SELECT id, x FROM t ORDER BY id LIMIT 100").expect("scan");
    assert!(
        t.contains("Integer(3)") && t.contains("SignedInteger(1)"),
        "{t}"
    );
    assert!(matches!(
        core.parse_sql(insert).expect("parse"),
        ParsedSql::Merge(_)
    ));
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
