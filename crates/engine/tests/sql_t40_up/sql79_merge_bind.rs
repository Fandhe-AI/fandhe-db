//! `MERGE` の束縛（対象と元の列解決・型照合。SQL-79・Issue #3352。親 #3349、設計記録
//! `docs/design/sql-merge.md`）の SQL 経由の結合テスト。
//!
//! 契約: 束縛エラーは書き込みより前に列解決（`42703`・`42702`・`42P01`）・型照合（`42804`）・INSERT の規則
//! （`22000`）・ビューの形（`55000`）の SQLSTATE で返り、行・台帳・世代に触れない。束縛を通る形のうち
//! `WHEN MATCHED` だけの形は #3353 から実行され、実行に結線されていない形（NOT MATCHED・ビュー対象）は
//! 固定文言の `0A000` で止まる。Execute・明示 Tx・Describe は同じ束縛エラーを返す。末尾 `;` 付きの単独送信
//! （psql 流）も同じ結果になる。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
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

/// DDL 許可つきのセッションで実行する（ビューの作成用）。
fn ddl(core: &EngineCore, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn snapshot(core: &EngineCore) -> String {
    let t = run(core, "SELECT id, k, x FROM t ORDER BY id LIMIT 100").expect("scan t");
    let s = run(core, "SELECT id, k, v FROM s ORDER BY id LIMIT 100").expect("scan s");
    format!("{t}\n{s}")
}

/// 末尾 `;` なし・ありの両方で `want` のコードになること、副作用が無いことを確かめる。
fn expect_code(core: &EngineCore, sql: &str, want: &str) {
    let before = snapshot(core);
    for text in [sql.to_string(), format!("{sql};")] {
        let (code, msg) = run(core, &text).expect_err(&text);
        assert_eq!(code, want, "{text}: {msg}");
        if want == "0A000" {
            assert!(
                msg.starts_with("feature not supported: MERGE"),
                "{text}: {msg}"
            );
        }
        assert_eq!(before, snapshot(core), "{text}");
    }
}

#[test]
fn bound_matched_only_shapes_run() {
    for sql in [
        // 結合キー（型クラス一致）・SET の列参照と式。
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v + t.x, k = 'z'",
        // `ON t.id = s.id` は結合キーとして受理する。
        "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
        // 片側だけの条件。
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED AND s.v > 5 AND t.x < 100 THEN DELETE",
        // 部分クエリの元。
        "MERGE INTO t USING (SELECT k, v FROM s) AS q (k, v) ON t.k = q.k WHEN MATCHED THEN UPDATE SET x = q.v",
        // 別名どうし。
        "MERGE INTO t AS d USING s AS e ON d.k = e.k WHEN MATCHED THEN DO NOTHING",
    ] {
        for text in [sql.to_string(), format!("{sql};")] {
            let (core, _g) = new_core("sql79b-ok");
            seed(&core);
            let out = run(&core, &text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            assert!(out.starts_with("Merge("), "{text}: {out}");
        }
    }
}

// NOT MATCHED を含む形は束縛を通っても、実行を入れる #3354 まで固定文言で拒否する。
// tracked: #3354
#[test]
fn bound_not_matched_shapes_stop_with_the_fixed_0a000() {
    let (core, _g) = new_core("sql79b-notmatched");
    seed(&core);
    expect_code(
        &core,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED AND s.v > 5 AND t.x < 100 THEN DELETE \
         WHEN NOT MATCHED AND s.v > 1 THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.v) \
         WHEN NOT MATCHED BY SOURCE AND t.x > 1 THEN UPDATE SET x = t.x + 1",
        "0A000",
    );
}

#[test]
fn unknown_ambiguous_and_invisible_columns_are_rejected_before_any_write() {
    let (core, _g) = new_core("sql79b-cols");
    seed(&core);
    let head = "MERGE INTO t USING s ON t.k = s.k";
    for (sql, want) in [
        // 42703: 未知の列。
        ("MERGE INTO t USING s ON t.nope = s.k WHEN MATCHED THEN DELETE".to_string(), "42703"),
        (format!("{head} WHEN MATCHED AND s.nope > 1 THEN DELETE"), "42703"),
        (format!("{head} WHEN MATCHED THEN UPDATE SET nope = 1"), "42703"),
        (format!("{head} WHEN MATCHED THEN UPDATE SET x = s.nope"), "42703"),
        (
            format!("{head} WHEN NOT MATCHED THEN INSERT (id, embedding, nope) VALUES (s.id, s.embedding, 1)"),
            "42703",
        ),
        (
            format!("{head} WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.nope)"),
            "42703",
        ),
        // 42702: 両側にある非修飾の列。
        ("MERGE INTO t USING s ON k = s.k WHEN MATCHED THEN DELETE".to_string(), "42702"),
        (format!("{head} WHEN MATCHED THEN UPDATE SET x = k"), "42702"),
        // 42P01: 未知の修飾子。
        ("MERGE INTO t USING s ON z.k = s.k WHEN MATCHED THEN DELETE".to_string(), "42P01"),
        // NOT MATCHED [BY TARGET] では対象を参照できない（修飾は 42P01、対象にしか無い非修飾は 42703）。
        (
            format!("{head} WHEN NOT MATCHED AND t.x > 1 THEN INSERT (id, embedding, k) VALUES (s.id, s.embedding, s.k)"),
            "42P01",
        ),
        (
            format!("{head} WHEN NOT MATCHED AND x > 1 THEN INSERT (id, embedding, k) VALUES (s.id, s.embedding, s.k)"),
            "42703",
        ),
        (
            format!("{head} WHEN NOT MATCHED THEN INSERT (id, embedding, k) VALUES (t.id, s.embedding, s.k)"),
            "42P01",
        ),
        // NOT MATCHED BY SOURCE では元を参照できない。
        (format!("{head} WHEN NOT MATCHED BY SOURCE AND s.v > 1 THEN DELETE"), "42P01"),
        (format!("{head} WHEN NOT MATCHED BY SOURCE AND v > 1 THEN DELETE"), "42703"),
        (format!("{head} WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = s.v"), "42P01"),
    ] {
        expect_code(&core, &sql, want);
    }
}

#[test]
fn type_mismatches_and_insert_rules_follow_the_single_statement_codes() {
    let (core, _g) = new_core("sql79b-types");
    seed(&core);
    let head = "MERGE INTO t USING s ON t.k = s.k";
    for (sql, want) in [
        // 42804: ON の比較できない型・代入できない型。
        ("MERGE INTO t USING s ON t.x = s.k WHEN MATCHED THEN DELETE".to_string(), "42804"),
        (format!("{head} WHEN MATCHED THEN UPDATE SET x = s.k"), "42804"),
        (
            format!("{head} WHEN NOT MATCHED THEN INSERT (id, embedding, k, x) VALUES (s.id, s.embedding, s.k, s.k)"),
            "42804",
        ),
        // 従来方式の表で疑似列 id を省く。
        (
            format!("{head} WHEN NOT MATCHED THEN INSERT (embedding, k, x) VALUES (s.embedding, s.k, s.v)"),
            "22000",
        ),
        // 列リストの重複・個数の不一致。
        (
            format!("{head} WHEN NOT MATCHED THEN INSERT (id, id, embedding, k) VALUES (s.id, s.id, s.embedding, s.k)"),
            "22000",
        ),
        (
            format!("{head} WHEN NOT MATCHED THEN INSERT (id, embedding, k) DEFAULT VALUES"),
            "42601",
        ),
        // SET 左辺の重複・疑似列。
        (format!("{head} WHEN MATCHED THEN UPDATE SET x = 1, x = 2"), "22000"),
        (format!("{head} WHEN MATCHED THEN UPDATE SET id = 1"), "42601"),
        // リテラルの値エラーは単独の UPDATE と同じコード。
        (format!("{head} WHEN MATCHED THEN UPDATE SET x = 'abc'"), "22P02"),
    ] {
        let single = match want {
            "22P02" => Some(run(&core, "UPDATE t SET x = 'abc' WHERE id = 1").expect_err("single")),
            _ => None,
        };
        if let Some((single_code, _)) = single {
            assert_eq!(single_code, want, "単独 UPDATE と同じコード");
        }
        expect_code(&core, &sql, want);
    }
}

#[test]
fn subquery_source_columns_come_from_describe_without_running_the_inner_query() {
    let (core, _g) = new_core("sql79b-subq");
    seed(&core);
    let before = snapshot(&core);
    for (sql, want) in [
        // 列別名が出力列より多い。
        (
            "MERGE INTO t USING (SELECT k FROM s) AS q (a, b) ON t.k = q.a WHEN MATCHED THEN DELETE",
            "42P10",
        ),
        // 別名で改名した列。元の名前は見えない。
        (
            "MERGE INTO t USING (SELECT k, v FROM s) AS q (a, b) ON t.k = q.k WHEN MATCHED THEN DELETE",
            "42703",
        ),
        // 内側から対象を相関参照することはできない。
        (
            "MERGE INTO t USING (SELECT k FROM s WHERE s.k = t.k) AS q ON t.k = q.k WHEN MATCHED THEN DELETE",
            "42P01",
        ),
        // 型の照合は合成スキーマで行う（integer 列と text 列）。
        (
            "MERGE INTO t USING (SELECT k, v FROM s) AS q ON t.x = q.k WHEN MATCHED THEN DELETE",
            "42804",
        ),
    ] {
        expect_code(&core, sql, want);
    }
    assert_eq!(before, snapshot(&core));
    // 列別名で改名した部分クエリの元は、別名の列で ON・SET を書けば実行される。
    let out = run(
        &core,
        "MERGE INTO t USING (SELECT k, v FROM s) AS q (a, b) ON t.k = q.a WHEN MATCHED THEN UPDATE SET x = q.b",
    )
    .expect("renamed columns run");
    assert!(out.starts_with("Merge("), "{out}");
}

// 自動更新できるビューを対象にする形は束縛を通っても、ビュー書き込みとの結線を入れる #3355 まで拒否する。
// tracked: #3355
#[test]
fn view_targets_follow_the_single_view_write_rules() {
    let (core, _g) = new_core("sql79b-view");
    seed(&core);
    ddl(
        &core,
        "CREATE VIEW vt AS SELECT id, embedding, k, x FROM t WHERE x > 0",
    );
    ddl(&core, "CREATE VIEW vk AS SELECT id, embedding, k FROM t");
    ddl(&core, "CREATE VIEW vd AS SELECT DISTINCT k FROM t");
    ddl(
        &core,
        "CREATE VIEW va (id, embedding, key, x) AS SELECT id, embedding, k, x FROM t",
    );
    for (sql, want) in [
        // 自動更新できるビューは束縛を通る。
        (
            "MERGE INTO vt USING s ON vt.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
            "0A000",
        ),
        // 公開していない基底列は 42703。
        (
            "MERGE INTO vk USING s ON vk.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v",
            "42703",
        ),
        (
            "MERGE INTO vk USING s ON vk.x = s.v WHEN MATCHED THEN DELETE",
            "42703",
        ),
        // 更新できないビューは単独のビュー書き込みと同じ分類。
        (
            "MERGE INTO vd USING s ON vd.k = s.k WHEN MATCHED THEN DELETE",
            "55000",
        ),
    ] {
        expect_code(&core, sql, want);
    }
    // 列別名つきのビューは別の固定文言の 0A000（束縛の入口で拒否する）。
    let (code, msg) = run(
        &core,
        "MERGE INTO va USING s ON va.key = s.k WHEN MATCHED THEN DELETE",
    )
    .expect_err("va");
    assert_eq!(code, "0A000");
    assert!(msg.contains("column aliases"), "{msg}");
    let (_, msg) = run(
        &core,
        "MERGE INTO vd USING s ON vd.k = s.k WHEN MATCHED THEN DELETE",
    )
    .expect_err("vd");
    assert!(msg.contains("cannot merge into view"), "{msg}");
    let single = run(&core, "DELETE FROM vd WHERE k = 'a'").expect_err("single view delete");
    assert_eq!(single.0, "55000");
}

#[test]
fn explicit_transaction_and_describe_report_the_same_binding_error() {
    let (core, _g) = new_core("sql79b-txn");
    seed(&core);
    let before = snapshot(&core);
    let bad = "MERGE INTO t USING s ON t.nope = s.k WHEN MATCHED THEN DELETE";
    let ok = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE";

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, bad)
        .expect_err("bad merge in txn");
    assert_eq!(err.wire_code(), "42703");
    // 失敗した Tx は ROLLBACK 以外を受け付けない。
    assert!(core
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, "SELECT 1")
        .is_err());
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(before, snapshot(&core));

    // Describe は Execute と同じ束縛エラーを返し、束縛を通る形は結果列なしで成功する。
    let parsed = core.parse_sql(bad).expect("parse");
    let err = core
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .expect_err("describe");
    assert_eq!(err.wire_code(), "42703");
    let parsed = core.parse_sql(ok).expect("parse");
    assert_eq!(
        core.describe_parsed_in_session(&SessionState::default(), &parsed)
            .expect("describe"),
        None
    );
}

/// 明示 Tx（writer 取得前）の Execute は `BEGIN` 時点のスナップショットで束縛し、Describe と一致する。
#[test]
fn explicit_transaction_binds_against_the_begin_snapshot() {
    let (core, _g) = new_core("sql79b-snap");
    seed(&core);
    let merge = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = 1";

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    // BEGIN 後に別セッションが参照列を削除する。
    ddl(&core, "ALTER TABLE t DROP COLUMN x");

    let parsed = core.parse_sql(merge).expect("parse");
    let described = core.describe_parsed_in_txn(&session, &txn, &parsed);
    assert_eq!(described.expect("describe in txn"), None);
    let err = core
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, merge)
        .expect_err("merge in txn");
    // 束縛は通る（BEGIN 時点の列がある）。実行は writer 取得時の世代検査で、BEGIN 後の別セッションの
    // DDL を `40001` として拒否する（束縛を通らないことで 42703 にはならない）。
    assert_eq!(err.wire_code(), "40001", "{}", err.client_message());
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    // Tx の外（最新カタログ）では列が無いので 42703。
    let (code, msg) = run(&core, merge).expect_err("merge outside txn");
    assert_eq!(code, "42703", "{msg}");
}

// 権限を満たす NOT MATCHED の INSERT の形は束縛を通っても、実行を入れる #3354 まで拒否する。
// tracked: #3354
#[test]
fn the_privilege_gate_runs_before_binding() {
    let (core, _g) = new_core("sql79b-priv");
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
    operator("CREATE ROLE alice");
    operator("CREATE ROLE bob");
    operator("CREATE TABLE tx (k BIGINT, v TEXT)");
    operator("CREATE TABLE sy (k BIGINT, w TEXT)");
    let as_bob = |sql: &str| -> String {
        let mut s = SessionState::default();
        s.bind_role_ids(2, 2);
        match core.execute_sql_in_session(&tctx(), &mut s, sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.wire_code().to_string(),
        }
    };
    // 権限の無い主体には未知の列の束縛エラーが見えない（列の存在を観測させない）。
    assert_eq!(
        as_bob("MERGE INTO tx USING sy ON tx.nope = sy.k WHEN MATCHED THEN DELETE"),
        "42501"
    );
    // 対象の SELECT だけを持ち元の SELECT を持たない主体には、元の列・型による束縛エラー
    // （42703・42804・0A000）が見えない。元の表・ビュー・部分クエリのどれでも 42501 で止まる。
    operator("GRANT SELECT ON tx TO bob");
    for sql in [
        "MERGE INTO tx USING sy ON tx.k = sy.guess WHEN MATCHED THEN DO NOTHING",
        "MERGE INTO tx USING sy ON tx.k = sy.k WHEN MATCHED THEN DO NOTHING",
        "MERGE INTO tx USING (SELECT k FROM sy) AS q ON tx.k = q.guess WHEN MATCHED THEN DO NOTHING",
    ] {
        assert_eq!(as_bob(sql), "42501", "{sql}");
    }
    operator("GRANT SELECT ON sy TO bob");
    operator("GRANT DELETE ON tx TO bob");
    assert_eq!(
        as_bob("MERGE INTO tx USING sy ON tx.nope = sy.k WHEN MATCHED THEN DELETE"),
        "42703"
    );
    // 権限を満たす主体は MATCHED だけの形を実行できる。
    assert_eq!(
        as_bob("MERGE INTO tx USING sy ON tx.k = sy.k WHEN MATCHED THEN DELETE"),
        "ok"
    );
    operator("GRANT INSERT ON tx TO bob");
    assert_eq!(
        as_bob(
            "MERGE INTO tx USING sy ON tx.k = sy.k WHEN NOT MATCHED THEN INSERT (k, v) VALUES (sy.k, sy.w)"
        ),
        "0A000",
        "移行済みの表は疑似列 id を省いてよい（束縛は通り、実行の拒否は 0A000）"
    );
    assert_eq!(
        as_bob("MERGE INTO tx USING sy ON tx.k = sy.k WHEN NOT MATCHED THEN INSERT DEFAULT VALUES"),
        "0A000"
    );
}

/// 3 テナントの表で、束縛の結果が他テナントの行・存在に依存しない（束縛は行を読まず、自テナントの
/// スキーマだけで判定する）。他テナントだけが持つ名前は自テナントからは未知として扱う。
#[test]
fn binding_does_not_depend_on_other_tenants_rows() {
    let (core, _g) = new_core("sql79b-rls");
    let tenant = |name: &str| {
        PolicyContext::with_visibilities(name, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    };
    let run_as = |name: &str, sql: &str| {
        core.execute_sql_in_session(&tenant(name), &mut SessionState::default(), sql)
            .map(|o| format!("{o:?}"))
            .map_err(|e| (e.wire_code().to_string(), e.client_message()))
    };
    // alice だけが行を持つ。bob・carol は空。
    run_as(
        "alice",
        "INSERT INTO t (id, embedding, k, x) VALUES (1, '[0.1,0.2]', 'a', 10)",
    )
    .expect("seed");
    let sql = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET x = s.v";
    let bad = "MERGE INTO t USING s ON t.nope = s.k WHEN MATCHED THEN DELETE";
    let answers: Vec<_> = ["alice", "bob", "carol"]
        .iter()
        .map(|n| (run_as(n, sql), run_as(n, bad)))
        .collect();
    assert!(answers.windows(2).all(|w| w[0] == w[1]), "{answers:?}");
    // 元が空なので、どのテナントでも件数 0 で成功する（他テナントの行の有無に依存しない）。
    assert_eq!(
        answers[0].0.as_ref().expect("runs"),
        "Merge(MergeOutcome { rows_affected: 0 })"
    );
    assert_eq!(answers[0].1.as_ref().expect_err("bad").0, "42703");
    // エラー文言は件数・値を含まない。
    assert!(!answers[0].1.as_ref().expect_err("bad").1.contains("10"));
}
