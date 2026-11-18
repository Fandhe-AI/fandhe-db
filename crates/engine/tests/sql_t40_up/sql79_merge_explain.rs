//! `EXPLAIN [ANALYZE] MERGE` の結合テスト（SQL-79・SQL-75・SQL-40・RLS-10・RECOVER-10/11 のポインタ。
//! Issue #3357。設計記録 `docs/design/sql-merge.md` D13・§13）。
//!
//! 静的な `EXPLAIN` は `merge` の下に `join` と 2 つの `scan` を返し、行・台帳・writer・世代に触れない。
//! 出力は閉じた語彙と文面から決まる有無・個数だけで、行数・見積もり・表名・値を含まず、テナントによって
//! 変わらない。`EXPLAIN ANALYZE` は実際に実行し（autocommit は commit、明示 Tx は `ROLLBACK` で戻り、
//! `USING OPERATION_ID` は台帳に記録される）、束縛・変換の拒否は非 EXPLAIN と同じ `wire_code`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

/// mt(k, v)・ms(k, w)。tenant-a にだけ行がある。
fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE mt (k BIGINT, v TEXT)",
        "CREATE TABLE ms (k BIGINT, w TEXT)",
        "INSERT INTO mt (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO ms (k, w) VALUES (2, 'w2'), (3, 'w3'), (4, 'w4')",
    ] {
        exec_as(&core, "tenant-a", sql);
    }
    (core, guard)
}

fn run_in(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    run_in(core, tenant, &mut SessionState::default(), sql)
}

fn explain_lines(out: SqlOutcome) -> Vec<String> {
    match out {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    explain_lines(run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")))
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn snapshot(core: &EngineCore, table: &str) -> String {
    format!(
        "{:?}",
        run(
            core,
            "tenant-a",
            &format!("SELECT * FROM {table} ORDER BY k LIMIT 100")
        )
        .expect("select")
    )
}

/// 明示 Tx で `sqls` を順に実行し、最後に `finish`（`COMMIT`／`ROLLBACK`）で終える。
fn in_txn(
    core: &EngineCore,
    tenant: &str,
    begin: &str,
    sqls: &[&str],
    finish: &str,
) -> Vec<String> {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx(tenant);
    core.execute_sql_in_txn(&c, &mut session, &mut txn, begin)
        .expect("begin");
    let outs = sqls
        .iter()
        .map(
            |sql| match core.execute_sql_in_txn(&c, &mut session, &mut txn, sql) {
                Ok(o) => format!("{o:?}"),
                Err(e) => e.wire_code().to_string(),
            },
        )
        .collect();
    core.execute_sql_in_txn(&c, &mut session, &mut txn, finish)
        .expect("finish");
    outs
}

const MATCHED_ONLY: &str = "MERGE INTO mt USING ms ON mt.k = ms.k WHEN MATCHED THEN DELETE";

/// 期待する木。`join_type` と `when` の行・`join_keys`・`join_filter`・元の走査分類の行を差し替えられる。
fn tree(
    whens: &[&str],
    returning: &str,
    op: &str,
    join: [&str; 3],
    source: &[&str],
) -> Vec<String> {
    let mut v = vec![
        "merge".to_string(),
        format!("when_clauses: {}", whens.len()),
    ];
    v.extend(whens.iter().map(|w| w.to_string()));
    v.push(format!("returning: {returning}"));
    v.push(format!("operation_id: {op}"));
    v.extend(strs(&[
        "-> join",
        "   join_method: hash",
        &format!("   join_type: {}", join[0]),
        &format!("   join_keys: {}", join[1]),
        &format!("   join_filter: {}", join[2]),
        "  -> scan",
        "     relation: 1",
        "     scalar_plan: plain_scan",
        "     access_path: full_scan",
        "  -> scan",
        "     relation: 2",
    ]));
    v.extend(source.iter().map(|l| format!("     {l}")));
    v
}

const SCAN_FULL: [&str; 2] = ["scalar_plan: plain_scan", "access_path: full_scan"];

#[test]
fn matched_only_is_an_inner_join_tree() {
    let (core, _g) = seeded("x3357-inner");
    for sql in [
        format!("EXPLAIN {MATCHED_ONLY}"),
        format!("EXPLAIN {MATCHED_ONLY};"),
    ] {
        assert_eq!(
            lines(&core, "tenant-a", &sql),
            tree(
                &["when: matched delete condition=none"],
                "none",
                "none",
                ["inner", "1", "none"],
                &SCAN_FULL
            ),
            "{sql}"
        );
    }
}

#[test]
fn join_type_follows_the_when_clauses() {
    let (core, _g) = seeded("x3357-jointype");
    let cases = [
        (
            "MERGE INTO mt USING ms ON mt.k = ms.k WHEN MATCHED AND ms.w = 'w2' THEN UPDATE SET v = ms.w \
             WHEN NOT MATCHED THEN INSERT (k, v) VALUES (ms.k, ms.w) USING OPERATION_ID 'x-1'",
            tree(
                &[
                    "when: matched update condition=present",
                    "when: not_matched_by_target insert condition=none",
                ],
                "none",
                "present",
                ["right", "1", "none"],
                &SCAN_FULL,
            ),
        ),
        (
            "MERGE INTO mt USING ms ON mt.k = ms.k WHEN NOT MATCHED BY SOURCE THEN DELETE \
             WHEN MATCHED THEN DO NOTHING RETURNING mt.k",
            tree(
                &[
                    "when: not_matched_by_source delete condition=none",
                    "when: matched do_nothing condition=none",
                ],
                "present",
                "none",
                ["left", "1", "none"],
                &SCAN_FULL,
            ),
        ),
        (
            "MERGE INTO mt USING ms ON mt.k = ms.k WHEN NOT MATCHED BY SOURCE THEN DELETE \
             WHEN NOT MATCHED BY TARGET THEN DO NOTHING",
            tree(
                &[
                    "when: not_matched_by_source delete condition=none",
                    "when: not_matched_by_target do_nothing condition=none",
                ],
                "none",
                "none",
                ["full", "1", "none"],
                &SCAN_FULL,
            ),
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(
            lines(&core, "tenant-a", &format!("EXPLAIN {sql}")),
            expected,
            "{sql}"
        );
    }
}

#[test]
fn key_count_residual_and_source_kinds() {
    let (core, _g) = seeded("x3357-shape");
    // 等値キー 0 本（ON が不等号だけ）は残余ありのハッシュ結合。
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN MERGE INTO mt USING ms ON mt.k < ms.k WHEN MATCHED THEN DELETE",
    );
    assert!(out.contains(&"   join_keys: 0".to_string()), "{out:?}");
    assert!(
        out.contains(&"   join_filter: present".to_string()),
        "{out:?}"
    );
    // 複合キーと残余。
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN MERGE INTO mt USING ms ON mt.k = ms.k AND mt.v = ms.w AND mt.k < 9 WHEN MATCHED THEN DELETE",
    );
    assert!(out.contains(&"   join_keys: 2".to_string()), "{out:?}");
    // 元が単純形ビュー。
    exec_as(
        &core,
        "tenant-a",
        "CREATE VIEW msv AS SELECT k, w FROM ms WHERE k > 1",
    );
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN MERGE INTO mt USING msv ON mt.k = msv.k WHEN MATCHED THEN DELETE",
    );
    assert_eq!(out.first().map(String::as_str), Some("merge"), "{out:?}");
    // 元が部分クエリのときは実行せず固定の分類行にする。
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN MERGE INTO mt USING (SELECT k, w FROM ms) AS s ON mt.k = s.k WHEN MATCHED THEN DELETE",
    );
    assert_eq!(
        out,
        tree(
            &["when: matched delete condition=none"],
            "none",
            "none",
            ["inner", "1", "none"],
            &["source_kind: subquery"]
        )
    );
}

#[test]
fn static_explain_has_no_effects_and_does_not_consume_the_ledger() {
    let (core, _g) = seeded("x3357-noeffect");
    let (t0, s0) = (snapshot(&core, "mt"), snapshot(&core, "ms"));
    let sql = format!("{MATCHED_ONLY} USING OPERATION_ID 'noeffect'");
    lines(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    assert_eq!((snapshot(&core, "mt"), snapshot(&core, "ms")), (t0, s0));
    // 同じ operation_id の本実行が成功し、その再送は台帳が `23505`。
    assert!(run(&core, "tenant-a", &sql).is_ok());
    assert_eq!(code(&core, &sql), "23505");
    // READ ONLY の明示 Tx でも実行できる。
    let outs = in_txn(
        &core,
        "tenant-a",
        "BEGIN READ ONLY",
        &[&format!("EXPLAIN {MATCHED_ONLY}")],
        "ROLLBACK",
    );
    assert!(outs.iter().all(|o| o.starts_with("Explain")), "{outs:?}");
}

#[test]
fn output_is_identical_across_tenants_and_row_counts() {
    let (core, _g) = seeded("x3357-tenant");
    let sql = format!("EXPLAIN {MATCHED_ONLY}");
    assert_eq!(
        lines(&core, "tenant-a", &sql),
        lines(&core, "tenant-empty", &sql)
    );
}

#[test]
fn rejections_match_the_non_explain_statement() {
    let (core, _g) = seeded("x3357-codes");
    let (t0, s0) = (snapshot(&core, "mt"), snapshot(&core, "ms"));
    for sql in [
        // 未知の表・列。
        "MERGE INTO mt USING nope ON mt.k = nope.k WHEN MATCHED THEN DELETE",
        "MERGE INTO mt USING ms ON mt.k = ms.nope WHEN MATCHED THEN DELETE",
        "MERGE INTO mt USING ms ON mt.k = ms.k WHEN MATCHED THEN UPDATE SET nope = 1",
        // 等値キーの型不一致。
        "MERGE INTO mt USING ms ON mt.v = ms.k WHEN MATCHED THEN DELETE",
    ] {
        let plain = code(&core, sql);
        assert_eq!(
            code(&core, &format!("EXPLAIN {sql}")),
            plain,
            "EXPLAIN {sql}"
        );
        assert_eq!(
            code(&core, &format!("EXPLAIN ANALYZE {sql}")),
            plain,
            "EXPLAIN ANALYZE {sql}"
        );
    }
    assert_eq!((snapshot(&core, "mt"), snapshot(&core, "ms")), (t0, s0));
}

// 未対応の形は固定の拒否のまま。
// intentional: docs/design/sql-merge.md
#[test]
fn unsupported_shapes_are_rejected_fail_closed() {
    let (core, _g) = seeded("x3357-reject");
    // `$n` は EXPLAIN の既存規則どおり `42601`。
    let err = core
        .parse_sql_prepared(
            "EXPLAIN MERGE INTO mt USING ms ON mt.k = ms.k WHEN MATCHED THEN UPDATE SET v = $1",
        )
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "42601");
    // 一時表の関与は非 EXPLAIN と同じ `0A000`（一時表はセッション固有なので同じセッションで作る）。
    let mut session = SessionState::default();
    session.allow_ddl();
    run_in(
        &core,
        "tenant-a",
        &mut session,
        "CREATE TEMP TABLE tmp_s (k BIGINT, w TEXT)",
    )
    .expect("temp table");
    let plain = "MERGE INTO mt USING tmp_s ON mt.k = tmp_s.k WHEN MATCHED THEN DELETE";
    for sql in [plain.to_string(), format!("EXPLAIN {plain}")] {
        assert_eq!(
            run_in(&core, "tenant-a", &mut session, &sql).expect_err(&sql),
            "0A000",
            "{sql}"
        );
    }
}

#[test]
fn verbose_and_json_forms() {
    let (core, _g) = seeded("x3357-verbose");
    let out = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (VERBOSE) {MATCHED_ONLY} RETURNING mt.k, ms.w"),
    );
    assert!(out.contains(&"output: k, w".to_string()), "{out:?}");
    let plain = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (VERBOSE) {MATCHED_ONLY}"),
    );
    assert!(!plain.iter().any(|l| l.starts_with("output:")), "{plain:?}");
    match run(
        &core,
        "tenant-a",
        &format!("EXPLAIN (FORMAT JSON) {MATCHED_ONLY}"),
    ) {
        Ok(SqlOutcome::Explain(r)) => {
            let text = format!("{:?}", r.rows);
            assert!(text.contains("join"), "{text}");
        }
        other => panic!("expected JSON explain, got {other:?}"),
    }
}

#[test]
fn analyze_autocommit_applies_commits_and_records_the_ledger() {
    let (core, _g) = seeded("x3357-analyze-auto");
    let sql = format!("{MATCHED_ONLY} RETURNING mt.k USING OPERATION_ID 'an-1'");
    let out = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) {sql}"),
    );
    assert!(out.contains(&"actual_rows: 2".to_string()), "{out:?}");
    let after = snapshot(&core, "mt");
    assert!(
        !after.contains("\"a2\"") && !after.contains("\"a3\""),
        "{after}"
    );
    // 台帳に記録されているので、同じ文の再送は `23505`。
    assert_eq!(code(&core, &sql), "23505");
    // RETURNING なし: `actual_rows` は 0。
    let (core, _g) = seeded("x3357-analyze-auto2");
    let out = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) {MATCHED_ONLY}"),
    );
    assert!(out.contains(&"actual_rows: 0".to_string()), "{out:?}");
}

#[test]
fn analyze_in_explicit_transaction_follows_rollback() {
    let (core, _g) = seeded("x3357-analyze-txn");
    let before = snapshot(&core, "mt");
    let outs = in_txn(
        &core,
        "tenant-a",
        "BEGIN",
        &[&format!("EXPLAIN ANALYZE {MATCHED_ONLY}")],
        "ROLLBACK",
    );
    assert!(outs[0].starts_with("Explain"), "{outs:?}");
    assert_eq!(snapshot(&core, "mt"), before);
    let outs = in_txn(
        &core,
        "tenant-a",
        "BEGIN READ ONLY",
        &[&format!("EXPLAIN ANALYZE {MATCHED_ONLY}")],
        "ROLLBACK",
    );
    assert_eq!(outs[0], "25006");
}
