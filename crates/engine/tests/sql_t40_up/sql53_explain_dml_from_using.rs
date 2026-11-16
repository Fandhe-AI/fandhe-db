//! `EXPLAIN [ANALYZE] UPDATE ... FROM`／`EXPLAIN [ANALYZE] DELETE ... USING` の結合テスト
//! （Issue #3293・SQL-53 (b)・SQL-75・SQL-76・SQL-40・RLS-10・RECOVER-10/11 ポインタ）。
//!
//! 静的な `EXPLAIN` は結合計画の木（`update`／`delete` の下に `join` と 2 つの `scan`）を返し、行・台帳・
//! writer・世代に触れない。出力は行数・見積もり・表名・値を含まず、テナントによって変わらない。
//! `EXPLAIN ANALYZE` は既存の DML ANALYZE と同じ契約で実際に実行する（autocommit は commit、明示 Tx は
//! `ROLLBACK` で戻り、`USING OPERATION_ID` は台帳に記録される）。束縛・変換の拒否は非 EXPLAIN と同じ
//! `wire_code`。ポインタ: `docs/design/update-from-delete-using.md`。

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

/// a(k, v)・pt(id PK, k, w)・ps(code TEXT PK, k, w)。tenant-a にだけ行がある。
fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE pt (id BIGINT PRIMARY KEY, k BIGINT, w TEXT)",
        "CREATE TABLE ps (code TEXT PRIMARY KEY, k BIGINT, w TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO pt (id, k, w) VALUES (1, 2, 'w2'), (2, 3, 'w3')",
        "INSERT INTO ps (code, k, w) VALUES ('c1', 2, 'w2')",
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

/// 表の全内容の文字列化（副作用ゼロの検証用）。
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
const UPD: &str = "UPDATE a SET v = 'z' FROM pt WHERE a.k = pt.k";
const DEL: &str = "DELETE FROM a USING pt WHERE a.k = pt.k";

const SCAN_FULL: [&str; 2] = ["scalar_plan: plain_scan", "access_path: full_scan"];

/// 期待する木（relation 2 の走査分類の行を差し替えられる）。
fn tree(head: &[&str], source: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = head.iter().map(|s| s.to_string()).collect();
    v.extend(strs(&[
        "-> join",
        "   join_method: hash",
        "   join_type: inner",
        "   join_keys: 1",
        "   join_filter: none",
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

#[test]
fn static_output_shape_update_and_delete() {
    let (core, _g) = seeded("x3293-shape");
    let upd_head = [
        "update",
        "assignments: 1",
        "filter: none",
        "returning: none",
        "operation_id: none",
    ];
    let del_head = [
        "delete",
        "filter: none",
        "returning: none",
        "operation_id: none",
    ];
    for sql in [format!("EXPLAIN {UPD}"), format!("EXPLAIN {UPD};")] {
        assert_eq!(
            lines(&core, "tenant-a", &sql),
            tree(&upd_head, &SCAN_FULL),
            "{sql}"
        );
    }
    for sql in [format!("EXPLAIN {DEL}"), format!("EXPLAIN {DEL};")] {
        assert_eq!(
            lines(&core, "tenant-a", &sql),
            tree(&del_head, &SCAN_FULL),
            "{sql}"
        );
    }
    // 別名・対象側述語・RETURNING・USING OPERATION_ID。
    assert_eq!(
        lines(
            &core,
            "tenant-a",
            "EXPLAIN UPDATE a x SET v = p.w FROM pt p WHERE x.k = p.k AND x.v = 'a1' \
             RETURNING x.k USING OPERATION_ID 'op-1'",
        ),
        tree(
            &[
                "update",
                "assignments: 1",
                "filter: present",
                "returning: present",
                "operation_id: present",
            ],
            &SCAN_FULL,
        )
    );
    assert_eq!(
        lines(
            &core,
            "tenant-a",
            "EXPLAIN DELETE FROM a AS x USING pt AS p WHERE x.k = p.k AND x.v = 'a1' \
             RETURNING p.w USING OPERATION_ID 'op-2'",
        ),
        tree(
            &[
                "delete",
                "filter: present",
                "returning: present",
                "operation_id: present"
            ],
            &SCAN_FULL,
        )
    );
}

/// 複合キー（等値キー 2 本）は `join_keys: 2`。
#[test]
fn composite_key_reports_key_count() {
    let (core, _g) = seeded("x3293-composite");
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN UPDATE a SET v = 'z' FROM pt WHERE a.k = pt.k AND a.v = pt.w",
    );
    assert!(out.contains(&"   join_keys: 2".to_string()), "{out:?}");
}

/// source 側が PK 等価なら実経路（PK 点検索）と同じ分類を表示する。
#[test]
fn source_side_uses_the_shared_scan_classification() {
    let (core, _g) = seeded("x3293-source-pk");
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN UPDATE a SET v = ps.w FROM ps WHERE a.k = ps.k AND ps.code = 'c1'",
    );
    assert!(
        out.contains(&"     scalar_plan: index_primary_key".to_string())
            && out.contains(&"     access_path: primary_key_lookup".to_string()),
        "{out:?}"
    );
}

/// 自己結合（同じ表を別名で参照）も受理する。
#[test]
fn self_join_is_accepted() {
    let (core, _g) = seeded("x3293-self");
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN UPDATE a SET v = b.v FROM a b WHERE a.k = b.k",
    );
    assert_eq!(out.first().map(String::as_str), Some("update"));
}

/// 静的な EXPLAIN は行・台帳に触れない。`USING OPERATION_ID` 付きの後で同じ ID の本実行が成功する。
#[test]
fn static_explain_has_no_effects_and_does_not_consume_the_ledger() {
    let (core, _g) = seeded("x3293-noeffect");
    let (a0, p0) = (snapshot(&core, "a"), snapshot(&core, "pt"));
    for sql in [
        format!("EXPLAIN {UPD} USING OPERATION_ID 'noeffect-u'"),
        format!("EXPLAIN {DEL} USING OPERATION_ID 'noeffect-d'"),
        format!("EXPLAIN {UPD} RETURNING a.k"),
    ] {
        lines(&core, "tenant-a", &sql);
    }
    assert_eq!((snapshot(&core, "a"), snapshot(&core, "pt")), (a0, p0));
    // 同じ operation_id の本実行が成功する（EXPLAIN が台帳を消費していない）。
    assert!(run(
        &core,
        "tenant-a",
        &format!("{UPD} USING OPERATION_ID 'noeffect-u'")
    )
    .is_ok());
    // 明示 Tx 内でも writer を取らず、READ ONLY でも実行できる。
    let outs = in_txn(
        &core,
        "tenant-a",
        "BEGIN READ ONLY",
        &[&format!("EXPLAIN {UPD}"), &format!("EXPLAIN {DEL}")],
        "ROLLBACK",
    );
    assert!(outs.iter().all(|o| o.starts_with("Explain")), "{outs:?}");
}

/// 出力は行のあるテナントと無いテナントで完全に一致する（件数・存在を出さない）。
#[test]
fn output_is_identical_across_tenants() {
    let (core, _g) = seeded("x3293-tenant");
    for sql in [format!("EXPLAIN {UPD}"), format!("EXPLAIN {DEL}")] {
        let with_rows = lines(&core, "tenant-a", &sql);
        let without_rows = lines(&core, "tenant-empty", &sql);
        assert_eq!(with_rows, without_rows, "{sql}");
    }
}

/// 束縛・変換の拒否は非 EXPLAIN と同じ `wire_code`（副作用なし）。
#[test]
fn rejections_match_the_non_explain_statement() {
    let (core, _g) = seeded("x3293-codes");
    let (a0, p0) = (snapshot(&core, "a"), snapshot(&core, "pt"));
    for sql in [
        // 未知の表・列。
        "UPDATE a SET v = 'z' FROM nope WHERE a.k = nope.k",
        "UPDATE a SET v = 'z' FROM pt WHERE a.k = pt.nope",
        // 等値キーの型不一致。
        "UPDATE a SET v = 'z' FROM pt WHERE a.v = pt.k",
        // あいまいな列参照。
        "UPDATE a SET v = 'z' FROM pt WHERE k = pt.k",
        // 等値キーも残余も無い片側だけの述語（変換器の対象外。非等値の列比較は Issue #3295 で受理）。
        "UPDATE a SET v = 'z' FROM pt WHERE a.k > 1",
        "DELETE FROM a USING pt WHERE pt.k IS NULL",
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
    assert_eq!((snapshot(&core, "a"), snapshot(&core, "pt")), (a0, p0));
}

/// `EXPLAIN (VERBOSE)` は Describe と同じ列を `output:` へ出し、`FORMAT JSON` は JSON を返す。
#[test]
fn verbose_and_json_forms() {
    let (core, _g) = seeded("x3293-verbose");
    let out = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (VERBOSE) {UPD} RETURNING a.k, pt.w"),
    );
    assert!(out.contains(&"output: k, w".to_string()), "{out:?}");
    let plain = lines(&core, "tenant-a", &format!("EXPLAIN (VERBOSE) {DEL}"));
    assert!(!plain.iter().any(|l| l.starts_with("output:")), "{plain:?}");
    match run(&core, "tenant-a", &format!("EXPLAIN (FORMAT JSON) {UPD}")) {
        Ok(SqlOutcome::Explain(r)) => {
            let text = format!("{:?}", r.rows);
            assert!(text.contains("join"), "{text}");
        }
        other => panic!("expected JSON explain, got {other:?}"),
    }
}

/// ANALYZE（autocommit）は実際に更新・削除し commit する。`actual_rows` は RETURNING の可視行数。
#[test]
fn analyze_autocommit_applies_and_commits() {
    let (core, _g) = seeded("x3293-analyze-auto");
    let before = snapshot(&core, "a");
    let out = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) {UPD}"),
    );
    assert!(out.contains(&"actual_rows: 0".to_string()), "{out:?}");
    assert!(out.contains(&"actual_loops: 1".to_string()), "{out:?}");
    assert_ne!(
        snapshot(&core, "a"),
        before,
        "UPDATE must have been applied"
    );
    assert!(snapshot(&core, "a").contains("\"z\""));

    let out = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) {DEL} RETURNING a.k;"),
    );
    assert!(out.contains(&"actual_rows: 2".to_string()), "{out:?}");
    assert!(
        !snapshot(&core, "a").contains("\"z\""),
        "rows must be deleted"
    );
}

/// ANALYZE（明示 Tx）は同じ Tx 上で実行され、`ROLLBACK` で戻り `COMMIT` で残る。`READ ONLY` は `25006`。
#[test]
fn analyze_in_explicit_transaction() {
    let (core, _g) = seeded("x3293-analyze-txn");
    let before = snapshot(&core, "a");
    let outs = in_txn(
        &core,
        "tenant-a",
        "BEGIN",
        &[&format!("EXPLAIN ANALYZE {UPD}")],
        "ROLLBACK",
    );
    assert!(outs[0].starts_with("Explain"), "{outs:?}");
    assert_eq!(snapshot(&core, "a"), before, "ROLLBACK must undo ANALYZE");

    in_txn(
        &core,
        "tenant-a",
        "BEGIN",
        &[&format!("EXPLAIN ANALYZE {UPD}")],
        "COMMIT",
    );
    assert_ne!(snapshot(&core, "a"), before, "COMMIT must keep ANALYZE");

    let outs = in_txn(
        &core,
        "tenant-a",
        "BEGIN READ ONLY",
        &[&format!("EXPLAIN ANALYZE {DEL}")],
        "ROLLBACK",
    );
    assert_eq!(outs, vec!["25006".to_string()]);
}

/// ANALYZE の `USING OPERATION_ID` は台帳に記録される: 同じ ID の再送は `23505`、別の文での再利用は `22023`。
#[test]
fn analyze_records_operation_id() {
    let (core, _g) = seeded("x3293-analyze-ledger");
    let sql = format!("EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) {UPD} USING OPERATION_ID 'an-1'");
    lines(&core, "tenant-a", &sql);
    // 同じ文（ANALYZE でも非 EXPLAIN でも）の再送は重複。
    assert_eq!(code(&core, &sql), "23505");
    assert_eq!(
        code(&core, &format!("{UPD} USING OPERATION_ID 'an-1'")),
        "23505"
    );
    // 別の文への再利用は拒否される。
    assert_eq!(
        code(&core, &format!("{DEL} USING OPERATION_ID 'an-1'")),
        "22023"
    );
}

/// セッション無しの入口・分割実行・`$n`・ANALYZE と JSON の併用は拒否する（fail-closed）。
#[test]
fn unsupported_forms_stay_fail_closed() {
    let (core, _g) = seeded("x3293-reject");
    let before = snapshot(&core, "a");
    // セッション無しの入口。
    let r = core.execute_sql(&ctx("tenant-a"), &format!("EXPLAIN {UPD}"));
    assert_eq!(r.expect_err("rejected").wire_code(), "42601");
    // 分割実行の併用は構文段で拒否。
    assert_eq!(
        code(
            &core,
            "EXPLAIN DELETE FROM a USING pt WHERE a.k = pt.k PARTITIONED"
        ),
        "42601"
    );
    // `$n` は EXPLAIN 前置でも Parse で拒否（受理は別 Issue）。
    for sql in [
        "EXPLAIN UPDATE a SET v = 'z' FROM pt WHERE a.k = pt.k AND pt.w = $1",
        "EXPLAIN ANALYZE DELETE FROM a USING pt WHERE a.k = pt.k AND pt.w = $1",
        "EXPLAIN (ANALYZE) UPDATE a SET v = $1 FROM pt WHERE a.k = pt.k",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
    // ANALYZE と JSON の併用は既存の述語形 DML と同じ `0A000`。
    assert_eq!(
        code(&core, &format!("EXPLAIN (ANALYZE, FORMAT JSON) {UPD}")),
        "0A000"
    );
    assert_eq!(snapshot(&core, "a"), before);
}

/// target がビューの形と一時表が関与する形は拒否する。FROM／USING 側の単純形ビューは
/// #3292 で受理済みのため、EXPLAIN も同じ計画を返す。
#[test]
fn views_and_temp_tables_are_rejected() {
    let (core, _g) = seeded("x3293-view-temp");
    exec_as(&core, "tenant-a", "CREATE VIEW va AS SELECT k, v FROM a");
    let sql = "EXPLAIN UPDATE va SET v = 'z' FROM pt WHERE va.k = pt.k";
    let c = code(&core, sql);
    assert!(c == "0A000" || c == "42601", "{sql}: {c}");
    exec_as(
        &core,
        "tenant-a",
        "EXPLAIN DELETE FROM a USING va WHERE a.k = va.k",
    );
    let c = ctx("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&c, &mut session, "CREATE TEMP TABLE tt (k BIGINT)")
        .expect("temp");
    for sql in [
        "EXPLAIN DELETE FROM a USING tt WHERE a.k = tt.k",
        "EXPLAIN UPDATE tt SET k = 1 FROM pt WHERE tt.k = pt.k",
    ] {
        let err = core
            .execute_sql_in_session(&c, &mut session, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}");
    }
}

/// 権限は非 EXPLAIN と同じ（対象表の UPDATE／DELETE と SELECT、FROM／USING 側の SELECT）。EXPLAIN は抜け道にならない。
#[test]
fn privileges_match_the_non_explain_statement() {
    let path = unique_db_path("x3293-acl");
    let _guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    let tctx = || {
        PolicyContext::with_visibilities("t1", [Visibility::Public, Visibility::Private])
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
    for sql in ["CREATE ROLE alice", "CREATE ROLE bob"] {
        operator(sql);
    }
    operator("CREATE TABLE tx (k BIGINT, v TEXT)");
    operator("CREATE TABLE sy (k BIGINT, w TEXT)");
    operator("INSERT INTO tx (k, v) VALUES (1, 'a')");
    operator("INSERT INTO sy (k, w) VALUES (1, 'b')");
    let upd = "UPDATE tx SET v = sy.w FROM sy WHERE tx.k = sy.k";
    let del = "DELETE FROM tx USING sy WHERE tx.k = sy.k";
    let all = |sql: &str| {
        [
            as_bob(sql),
            as_bob(&format!("EXPLAIN {sql}")),
            as_bob(&format!("EXPLAIN ANALYZE {sql}")),
        ]
    };
    let denied = ["42501", "42501", "42501"].map(String::from);
    // 権限なし。
    for sql in [upd, del] {
        assert_eq!(all(sql), denied, "{sql}");
    }
    // 対象表の更新・削除権限だけでは足りない（FROM／USING 側の SELECT が要る）。
    operator("GRANT UPDATE ON tx TO bob");
    operator("GRANT DELETE ON tx TO bob");
    operator("GRANT SELECT ON tx TO bob");
    for sql in [upd, del] {
        assert_eq!(all(sql), denied, "{sql}");
    }
    // FROM／USING 側の SELECT を付与すると通る。
    operator("GRANT SELECT ON sy TO bob");
    assert_eq!(as_bob(&format!("EXPLAIN {upd}")), "ok");
    assert_eq!(as_bob(&format!("EXPLAIN {del}")), "ok");
    assert_eq!(as_bob(upd), "ok");
}
