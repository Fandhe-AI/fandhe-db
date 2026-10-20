//! 一時表のテナント境界・RLS 不変性・永続カタログ非汚染（TABLE-44・Issue #1685）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` の TABLE-44 の (1)（他セッション・他テナント
//! からの不可視）・(2)（切断後に残骸が無いこと）・(6)（NoSQL 表層から見えないこと）。
//!
//! `table44_temp_table_dml.rs`（同一テナント別セッション・同一 `SessionState` の別テナント ctx
//! の 1 ケース）と `table44_temp_table_ddl.rs` が一時表の意味論を固定済みのため、本ファイルは
//! それらを越える観点だけを足す。
//!
//! - 全文種（SELECT/INSERT/UPDATE/DELETE/TRUNCATE/CREATE INDEX/DROP INDEX/DROP TABLE/COPY）で、
//!   他セッション・他テナントの応答が「その名前が一度も存在しない DB での応答」と
//!   `wire_code`・`Display` 文言の両方で一致すること（応答差分による存在情報の漏えいが無い）
//! - 同名の一時表が 3 者で独立していること
//! - 他テナントの行が同名の永続表に大量にあっても、一時表の結果が 1 バイトも変わらないこと
//!   （`rls10_relational_paths.rs` と同じ「他テナント行の有無に対する不変性」の流儀）
//! - 一時表・一時索引が永続カタログ・永続ファイルへ一切到達しないこと（接続破棄後に残骸が無い）
//! - `SessionState` を受け取らない入口（NoSQL 表層が使う形）から一時表名が解決されないこと
//!
//! 呼び出し元の文脈: wire-server の接続ハンドラは接続ごとの `SessionState` を `engine` の
//! `execute_sql_in_session` 系へ渡す。本ファイルはその engine 側の契約だけを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";
/// 他セッションの応答へ混入してはならないカナリア文字列。
const CANARY: &str = "tmp-canary-7f3a";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_at(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    (core_at(&path), guard)
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

/// 応答の要約。成功は `OK`、失敗は `wire_code` と `Display` 文言（クライアントに見える全て）。
fn outcome(r: Result<impl std::fmt::Debug, SqlSurfaceError>, include_ok_body: bool) -> String {
    match r {
        Ok(o) if include_ok_body => format!("OK {o:?}"),
        Ok(_) => "OK".to_string(),
        Err(e) => format!("ERR {} {e}", e.wire_code()),
    }
}

fn run(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    outcome(
        core.execute_sql_in_session_legacy(&ctx(tenant), s, sql),
        true,
    )
}

fn run_ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session_legacy(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
}

fn query_rows(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> Vec<String> {
    match run_ok(core, tenant, s, sql) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| {
                let cells: Vec<String> = row.cells.iter().map(|c| format!("{c:?}")).collect();
                format!("{}|{}", row.id, cells.join(","))
            })
            .collect(),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

/// `COPY ... TO STDOUT` の入口（`CopyPlan` 経由）の応答要約。
fn copy_outcome(core: &EngineCore, tenant: &str, s: &SessionState, sql: &str) -> String {
    match core.begin_copy(&ctx(tenant), s, sql) {
        Ok(CopyPlan::To(_, _)) => "OK COPY TO".to_string(),
        Ok(_) => "OK COPY OTHER".to_string(),
        Err(e) => format!("ERR {} {e}", e.wire_code()),
    }
}

/// セッション A に一時表 `secret_t`（UNIQUE 列・一時索引・カナリア行つき）を用意する。
fn setup_secret_temp_table(core: &EngineCore, a: &mut SessionState) {
    run_ok(
        core,
        TENANT_A,
        a,
        "CREATE TEMP TABLE secret_t (id BIGINT PRIMARY KEY, code TEXT UNIQUE, n INTEGER)",
    );
    run_ok(core, TENANT_A, a, "CREATE INDEX secret_idx ON secret_t (n)");
    run_ok(
        core,
        TENANT_A,
        a,
        &format!("INSERT INTO secret_t (id, code, n) VALUES (1, '{CANARY}', 7), (2, 'c2', 8)"),
    );
}

/// セッションを持たない入口 1 つ分の呼び出し（応答の要約を返す）。
type Probe = fn(&EngineCore, &PolicyContext) -> String;

/// 他者が試す文種の一覧（一時表を壊す・読む・隠す全ての入口）。
const FOREIGN_STATEMENTS: &[&str] = &[
    "SELECT id, code FROM secret_t LIMIT 10",
    "SELECT COUNT(*) FROM secret_t",
    "INSERT INTO secret_t (id, code, n) VALUES (9, 'x', 1)",
    "UPDATE secret_t SET n = 0 WHERE id = 1",
    "DELETE FROM secret_t WHERE id = 1",
    "TRUNCATE secret_t",
    "CREATE INDEX other_idx ON secret_t (n)",
    "DROP INDEX secret_idx",
    "DROP TABLE secret_t",
];

// ---- (1) 他セッション・他テナントからの不可視（全文種） -----------------------------------

/// TABLE-44 (1): 他セッション・他テナントの応答が「一度も存在しなかった DB」の応答と完全に一致する。
#[test]
fn other_session_and_tenant_cannot_resolve_temp_table_in_any_statement_kind() {
    let (core, _g) = new_core("t44-iso-kinds");
    let (control, _gc) = new_core("t44-iso-kinds-control");

    let mut a = SessionState::default();
    setup_secret_temp_table(&core, &mut a);

    // DDL 権限の有無の両方で比べる（権限エラーで差が隠れる経路と、カタログまで到達する経路）。
    for ddl in [false, true] {
        for tenant in [TENANT_A, TENANT_B] {
            // 同一テナントの別セッション（B）と、別テナント（C）。
            let mk = || {
                if ddl {
                    ddl_session()
                } else {
                    SessionState::default()
                }
            };
            let mut other = mk();
            let mut baseline = mk();
            for sql in FOREIGN_STATEMENTS {
                let got = run(&core, tenant, &mut other, sql);
                let want = run(&control, tenant, &mut baseline, sql);
                assert_eq!(got, want, "ddl={ddl} tenant={tenant} sql={sql}");
                assert!(!got.contains(CANARY), "canary leaked: {sql}");
            }
            let got = copy_outcome(&core, tenant, &other, "COPY secret_t TO STDOUT");
            let want = copy_outcome(&control, tenant, &baseline, "COPY secret_t TO STDOUT");
            assert_eq!(got, want, "ddl={ddl} tenant={tenant} COPY TO");
            assert!(got.starts_with("ERR"), "COPY must not resolve: {got}");
        }
    }

    // 上の試行が A の一時表へ何も作用していない（読み取りが元のまま）。
    assert_eq!(
        query_rows(
            &core,
            TENANT_A,
            &mut a,
            "SELECT id, code FROM secret_t ORDER BY id LIMIT 10"
        )
        .len(),
        2
    );
}

// ---- 同名の独立性 -----------------------------------------------------------------------

/// TABLE-44 (1): 同名の一時表を 3 者（A・同一テナント別セッション・別テナント）が作っても独立する。
#[test]
fn same_temp_name_in_two_sessions_and_tenants_is_independent() {
    let (core, _g) = new_core("t44-iso-same-name");
    let mut a = SessionState::default();
    let mut b = SessionState::default();
    let mut c = SessionState::default();
    for (tenant, s) in [(TENANT_A, &mut a), (TENANT_A, &mut b), (TENANT_B, &mut c)] {
        run_ok(
            &core,
            tenant,
            s,
            "CREATE TEMP TABLE shared_name (id BIGINT PRIMARY KEY, v TEXT)",
        );
    }
    run_ok(
        &core,
        TENANT_A,
        &mut a,
        "INSERT INTO shared_name (id, v) VALUES (1, 'a1'), (2, 'a2')",
    );
    run_ok(
        &core,
        TENANT_A,
        &mut b,
        "INSERT INTO shared_name (id, v) VALUES (1, 'b1')",
    );
    run_ok(
        &core,
        TENANT_B,
        &mut c,
        "INSERT INTO shared_name (id, v) VALUES (1, 'c1'), (2, 'c2'), (3, 'c3')",
    );

    let sel = "SELECT id, v FROM shared_name ORDER BY id LIMIT 10";
    let ra = query_rows(&core, TENANT_A, &mut a, sel);
    let rb = query_rows(&core, TENANT_A, &mut b, sel);
    let rc = query_rows(&core, TENANT_B, &mut c, sel);
    assert_eq!(ra.len(), 2);
    assert_eq!(rb.len(), 1);
    assert_eq!(rc.len(), 3);
    assert!(ra.iter().all(|r| r.contains("a")), "{ra:?}");
    assert!(rb.iter().all(|r| r.contains("b1")), "{rb:?}");
    assert!(rc.iter().all(|r| r.contains("c")), "{rc:?}");

    // 一者の TRUNCATE は他者へ波及しない。
    run_ok(&core, TENANT_A, &mut a, "TRUNCATE shared_name");
    assert_eq!(query_rows(&core, TENANT_A, &mut b, sel).len(), 1);
    assert_eq!(query_rows(&core, TENANT_B, &mut c, sel).len(), 3);
}

// ---- RLS の不変性（他テナント行の有無） -----------------------------------------------------

const INVARIANCE_COLUMNS: &str =
    "(name TEXT NOT NULL, qty INTEGER DEFAULT 7 CHECK (qty >= 0), code TEXT UNIQUE)";

/// 一時表側の列定義。一時表は採番方式で暗黙の `id` を持たないため、`id` を宣言した列にする
/// （永続表側は従来方式で疑似列 `id` を持つ。TABLE-23・Issue #2199）。
const INVARIANCE_TEMP_COLUMNS: &str =
    "(id BIGINT PRIMARY KEY, name TEXT NOT NULL, qty INTEGER DEFAULT 7 CHECK (qty >= 0), code TEXT UNIQUE)";

/// 一時表 `t` に対して流す台本。永続表 `t` の他テナント行の有無で結果が変わらないことを比べる。
const INVARIANCE_SCRIPT: &[&str] = &[
    "INSERT INTO t (id, name, qty, code) VALUES (1, 'a', 10, 'c1'), (2, 'b', 20, 'c2')",
    "INSERT INTO t (id, name, code) VALUES (3, 'c', 'c3')",
    "INSERT INTO t (id, name, qty, code) VALUES (1, 'dup', 1, 'cx')",
    "INSERT INTO t (id, name, qty, code) VALUES (9, 'dup', 1, 'c1')",
    "INSERT INTO t (id, name, qty, code) VALUES (9, 'neg', -1, 'cy')",
    "SELECT id, name, qty, code FROM t ORDER BY id LIMIT 1000",
    "SELECT COUNT(*), SUM(qty), MIN(qty), MAX(qty) FROM t",
    "SELECT name, COUNT(*) FROM t GROUP BY name ORDER BY name LIMIT 10",
    "UPDATE t SET qty = qty + 1 WHERE qty > 5 RETURNING id, qty",
    "DELETE FROM t WHERE id = 2 RETURNING id, name",
    "SELECT id, qty FROM t ORDER BY id LIMIT 1000",
    "TRUNCATE t",
    "SELECT COUNT(*) FROM t",
    "INSERT INTO t (id, name, qty, code) VALUES (1, 'again', 1, 'c1')",
    "SELECT id, name FROM t LIMIT 1000",
];

/// `other_rows` 件の tenant-b 行を持つ同名の永続表 `t` を用意した DB で、tenant-a の台本を流す。
fn run_invariance_script(label: &str, other_rows: usize, public_only: bool) -> Vec<String> {
    let (core, _g) = new_core(label);
    if other_rows > 0 {
        let mut ddl = ddl_session();
        run_ok(
            &core,
            TENANT_B,
            &mut ddl,
            &format!("CREATE TABLE t {INVARIANCE_COLUMNS}"),
        );
        let mut id = 1usize;
        while id <= other_rows {
            let end = (id + 49).min(other_rows);
            let values: Vec<String> = (id..=end)
                .map(|i| format!("({i}, 'other-{i}', {i}, 'other-code-{i}')"))
                .collect();
            run_ok(
                &core,
                TENANT_B,
                &mut ddl,
                &format!(
                    "INSERT INTO t (id, name, qty, code) VALUES {}",
                    values.join(", ")
                ),
            );
            id = end + 1;
        }
    }
    let ctx_a = if public_only {
        PolicyContext::new(TENANT_A).expect("valid tenant")
    } else {
        ctx(TENANT_A)
    };
    let mut s = SessionState::default();
    let mut out = vec![outcome(
        core.execute_sql_in_session_legacy(
            &ctx_a,
            &mut s,
            &format!("CREATE TEMP TABLE t {INVARIANCE_TEMP_COLUMNS}"),
        ),
        true,
    )];
    for sql in INVARIANCE_SCRIPT {
        out.push(outcome(
            core.execute_sql_in_session_legacy(&ctx_a, &mut s, sql),
            true,
        ));
    }
    out
}

/// TABLE-44 (1): 他テナントの行が同名の永続表に大量にあっても、一時表の結果は変わらない。
/// Public のみの文脈（INSERT は常に Private 固定のため自分の行も見えない。永続表と同じ規則）でも
/// 同様に不変であること。
#[test]
fn temp_table_results_are_invariant_to_other_tenant_rows_in_a_same_named_persistent_table() {
    for public_only in [false, true] {
        let empty = run_invariance_script("t44-iso-inv-empty", 0, public_only);
        let busy = run_invariance_script("t44-iso-inv-busy", 500, public_only);
        assert_eq!(empty, busy, "public_only={public_only}");
        // 台本が空虚でないこと（拒否だらけではなく、実際に行を返す文が含まれる）。
        assert!(
            empty.iter().filter(|r| r.starts_with("OK")).count() >= 8,
            "script mostly rejected: {empty:?}"
        );
        assert!(
            !empty.iter().any(|r| r.contains("other-")),
            "another tenant's rows leaked: {empty:?}"
        );
    }
}

// ---- エラー応答の同一性 ---------------------------------------------------------------------

/// TABLE-44 (1): 他セッションが同名の一時表を持つか否かで、こちらのエラー応答が変わらない
/// （応答差分で他セッションの一時表の存在を推測できない）。
#[test]
fn error_responses_do_not_reveal_other_sessions_temp_tables() {
    let (with_peer, _g1) = new_core("t44-iso-err-with");
    let (without_peer, _g2) = new_core("t44-iso-err-without");

    let mut a = SessionState::default();
    setup_secret_temp_table(&with_peer, &mut a);

    let probes: Vec<&str> = FOREIGN_STATEMENTS
        .iter()
        .copied()
        .chain([
            "SELECT id FROM secret_t WHERE code = 'x' LIMIT 1",
            "INSERT INTO secret_t (id, code, n) VALUES (1, 'dup', 1) ON CONFLICT (id) DO NOTHING",
        ])
        .collect();
    for tenant in [TENANT_A, TENANT_B] {
        let mut x = ddl_session();
        let mut y = ddl_session();
        for sql in &probes {
            let got = run(&with_peer, tenant, &mut x, sql);
            let want = run(&without_peer, tenant, &mut y, sql);
            assert_eq!(got, want, "tenant={tenant} sql={sql}");
            assert!(!got.contains(CANARY), "{got}");
        }
    }
}

// ---- 永続カタログの非汚染と破棄 ---------------------------------------------------------------

/// TABLE-44 (2): 一時表・一時索引・トランザクション中の書き込みは永続カタログ・ファイルへ
/// 到達せず、セッション破棄・コア破棄・再オープン後にも残骸が無い。
#[test]
fn temp_tables_never_reach_the_persistent_catalog_or_file() {
    let path = unique_db_path("t44-iso-catalog");
    let _g = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "perm",
                vec![ColumnDef::new("a", ColumnType::Text, true)],
            ))
            .expect("create perm");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);

        let mut a = SessionState::default();
        setup_secret_temp_table(&core, &mut a);
        run_ok(
            &core,
            TENANT_A,
            &mut a,
            "CREATE TEMP TABLE scratch (id BIGINT PRIMARY KEY, v TEXT) ON COMMIT DELETE ROWS",
        );

        let mut t: SessionTransaction<'_> = core.new_session_transaction();
        for sql in [
            "BEGIN",
            "INSERT INTO secret_t (id, code, n) VALUES (10, 'tx-commit', 1)",
            "COMMIT",
            "BEGIN",
            "INSERT INTO secret_t (id, code, n) VALUES (11, 'tx-rollback', 1)",
            "INSERT INTO scratch (id, v) VALUES (1, 'x')",
            "ROLLBACK",
        ] {
            core.execute_sql_in_txn(&ctx(TENANT_A), &mut a, &mut t, sql)
                .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()));
        }
        assert_eq!(
            query_rows(
                &core,
                TENANT_A,
                &mut a,
                "SELECT id FROM secret_t ORDER BY id LIMIT 10"
            )
            .len(),
            3
        );
        // 接続破棄に相当（セッションとトランザクションを落とす）。コアは生かしたまま。
        drop(t);
        drop(a);
        let mut fresh = SessionState::default();
        let want = run(
            &core,
            TENANT_A,
            &mut fresh,
            "SELECT id FROM secret_t LIMIT 1",
        );
        assert!(
            want.starts_with("ERR 42P01"),
            "dropped session left a table: {want}"
        );
    }

    // コアも落として再オープンし、永続側に一時表由来の名前が一切無いことを確かめる。
    let storage = Storage::open(&path).expect("reopen storage");
    assert_eq!(
        storage.list_tables().expect("list tables"),
        vec!["perm".to_string()]
    );
    assert!(storage.list_indexes().expect("list indexes").is_empty());
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let mut s = SessionState::default();
    assert!(
        run(&core, TENANT_A, &mut s, "SELECT id FROM secret_t LIMIT 1").starts_with("ERR 42P01")
    );
    // 同名を作り直すと空の表になる（行が復活しない）。
    run_ok(
        &core,
        TENANT_A,
        &mut s,
        "CREATE TEMP TABLE secret_t (id BIGINT PRIMARY KEY, code TEXT UNIQUE, n INTEGER)",
    );
    assert!(query_rows(&core, TENANT_A, &mut s, "SELECT id FROM secret_t LIMIT 10").is_empty());
}

// ---- セッションを持たない入口 -----------------------------------------------------------------

/// TABLE-44 (6): `SessionState` を受け取らない入口（NoSQL 表層と同じ形）は一時表名を解決しない。
/// 同名の永続表があれば永続表にだけ作用し、A の一時表は無傷である。
#[test]
fn sessionless_entry_points_never_resolve_temp_tables() {
    let (core, _g) = new_core("t44-iso-sessionless");
    let (control, _gc) = new_core("t44-iso-sessionless-control");

    let mut a = SessionState::default();
    setup_secret_temp_table(&core, &mut a);

    // 一時表にしか無い名前は、一度も存在しなかった DB と同じ応答。
    let c = ctx(TENANT_A);
    let probes: [(&str, Probe); 5] = [
        ("select", |e, c| {
            outcome(e.execute_sql(c, "SELECT id FROM secret_t LIMIT 5"), true)
        }),
        ("insert", |e, c| {
            outcome(
                e.execute_insert_sql(c, "INSERT INTO secret_t (id, code, n) VALUES (9, 'x', 1)"),
                true,
            )
        }),
        ("update", |e, c| {
            outcome(
                e.execute_update_sql(c, "UPDATE secret_t SET n = 0 WHERE id = 1"),
                true,
            )
        }),
        ("delete", |e, c| {
            outcome(
                e.execute_delete_sql(c, "DELETE FROM secret_t WHERE id = 1"),
                true,
            )
        }),
        ("truncate", |e, c| {
            outcome(e.execute_truncate_sql(c, "TRUNCATE secret_t"), true)
        }),
    ];
    for (name, probe) in probes {
        let got = probe(&core, &c);
        let want = probe(&control, &c);
        assert_eq!(got, want, "sessionless {name}");
        assert!(!got.contains(CANARY), "{name}: {got}");
    }

    // 同名の永続表があると、セッションを持たない入口は永続表のほうへ作用する。
    let mut ddl = ddl_session();
    run_ok(
        &core,
        TENANT_A,
        &mut ddl,
        "CREATE TABLE secret_t (code TEXT UNIQUE, n INTEGER)",
    );
    core.execute_insert_sql(
        &c,
        "INSERT INTO secret_t (id, code, n) VALUES (1, 'perm-row', 1)",
    )
    .expect("persistent insert");
    let via_core = core
        .execute_sql(&c, "SELECT id, code FROM secret_t LIMIT 10")
        .expect("persistent select");
    assert_eq!(
        via_core.rows.len(),
        1,
        "sessionless select must see only the persistent row"
    );
    assert!(format!("{:?}", via_core.rows).contains("perm-row"));
    core.execute_delete_sql(&c, "DELETE FROM secret_t WHERE id = 1")
        .expect("persistent delete");
    // A の一時表（カナリア行つき）は無傷。
    let rows = query_rows(
        &core,
        TENANT_A,
        &mut a,
        "SELECT id, code FROM secret_t ORDER BY id LIMIT 10",
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().any(|r| r.contains(CANARY)));
}
