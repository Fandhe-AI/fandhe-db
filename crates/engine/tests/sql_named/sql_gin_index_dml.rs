//! GIN 相当索引（`CREATE INDEX ... USING gin`）のキー抽出と DML 維持の結合テスト
//! （Issue #2207・INDEX-8。設計は `docs/design/gin-jsonb-index.md` D3〜D8・§8）。
//!
//! `sql_gin_index_ddl.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）で、
//! どの書き込み経路の後でも「索引の内容が全行を走査して抽出し直した結果と一致する」ことを固定する。
//! 検証は診断 API `EngineCore::verify_gin_index_read_only`（読み取り専用。再構築しない）で行い、
//! 演算子クラス 2 種（`jsonb_ops`・`jsonb_path_ops`）の索引を同じ列に併設して同時に確かめる:
//!
//! - 欠落（生存行の抽出キーにポスティングが無い）は常に 0 件
//! - 後片付けのある経路（削除・TRUNCATE・CASCADE）では stale も 0 件
//! - 他テナントの索引内容は自テナントの書き込みで変わらない（RLS-9）
//! - 上限超過は `54000` で副作用ゼロ、エラー文言にテナント・値・件数を含めない

use engine::catalog::{ColumnDef, ColumnType, GinIndexAudit, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const INDEXES: [&str; 2] = ["g_ops", "g_path"];

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("doc", ColumnType::Jsonb, true),
            ColumnDef::new("raw", ColumnType::Json, true),
            ColumnDef::new("body", ColumnType::Text, true),
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn open_fixture(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (path, guard)
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn run_ok(core: &EngineCore, session: &mut SessionState, tenant: &str, sql: &str) {
    if let Err(e) = run(core, session, tenant, sql) {
        panic!("{sql} must succeed, got {e:?}");
    }
}

fn err(core: &EngineCore, session: &mut SessionState, tenant: &str, sql: &str) -> SqlSurfaceError {
    match run(core, session, tenant, sql) {
        Ok(outcome) => panic!("{sql} must fail, got {outcome:?}"),
        Err(e) => e,
    }
}

fn create_indexes(core: &EngineCore, s: &mut SessionState) {
    run_ok(
        core,
        s,
        "alice",
        "CREATE INDEX g_ops ON docs USING gin (doc jsonb_ops)",
    );
    run_ok(
        core,
        s,
        "alice",
        "CREATE INDEX g_path ON docs USING gin (doc jsonb_path_ops)",
    );
}

fn audit(core: &EngineCore, tenant: &str, index: &str) -> GinIndexAudit {
    core.verify_gin_index_read_only(&ctx(tenant), index)
        .unwrap_or_else(|e| panic!("audit {index} for {tenant} failed: {e:?}"))
}

/// マーカー有効・欠落 0 を確かめる。`strict` なら stale も 0（後片付けのある経路）。
fn assert_clean(core: &EngineCore, tenants: &[&str], strict: bool) {
    for tenant in tenants {
        for index in INDEXES {
            let a = audit(core, tenant, index);
            assert!(
                a.marker_valid,
                "{index}/{tenant}: marker must be valid: {a:?}"
            );
            assert_eq!(a.missing, 0, "{index}/{tenant}: no missing postings: {a:?}");
            if strict {
                assert_eq!(a.stale, 0, "{index}/{tenant}: no stale entries: {a:?}");
            }
        }
    }
}

fn insert(core: &EngineCore, s: &mut SessionState, tenant: &str, id: u64, doc: &str) {
    run_ok(
        core,
        s,
        tenant,
        &format!(
            "INSERT INTO docs (id, doc, body, embedding) VALUES ({id}, '{doc}', 'b', '[1,0]')"
        ),
    );
}

fn count(core: &EngineCore, s: &mut SessionState, tenant: &str) -> usize {
    match run(
        core,
        s,
        tenant,
        "SELECT id FROM docs ORDER BY id LIMIT 10000",
    )
    .expect("select rows")
    {
        SqlOutcome::Query(q) => q.rows.len(),
        other => panic!("expected query outcome, got {other:?}"),
    }
}

fn seed(core: &EngineCore, s: &mut SessionState) {
    insert(
        core,
        s,
        "alice",
        1,
        r#"{"a":1,"tags":["x","y"],"n":{"k":1.0}}"#,
    );
    insert(core, s, "alice", 2, r#"["p","q",3]"#);
    insert(core, s, "alice", 3, r#""scalar""#);
    insert(core, s, "alice", 4, "{}");
    insert(core, s, "alice", 5, "[]");
    run_ok(
        core,
        s,
        "alice",
        "INSERT INTO docs (id, body, embedding) VALUES (6, 'nulldoc', '[1,0]')",
    );
    insert(core, s, "bob", 11, r#"{"b":2,"tags":["x"]}"#);
    insert(core, s, "bob", 12, r#"[1,2,3]"#);
}

/// `CREATE INDEX` が既存行（両テナント・NULL・空の容器を含む）から初回構築し、再オープン後も一致する。
#[test]
fn create_index_builds_from_existing_rows_for_all_tenants() {
    let (path, _guard) = open_fixture("gin-dml-build");
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        seed(&core, &mut s);
        create_indexes(&core, &mut s);
        assert_clean(&core, &["alice", "bob"], true);
    }
    let core = core_at(&path);
    assert_clean(&core, &["alice", "bob"], true);
    // 宣言前から居なかったテナントでも、行が無ければ欠落 0・stale 0（マーカーは遅延構築）。
    let a = audit(&core, "carol", "g_ops");
    assert_eq!((a.missing, a.stale), (0, 0));
}

/// 行が 1 件も無い作成直後の表（行テーブル未作成）でも診断 API は失敗せず、マーカー無効・欠落 0・stale 0 を返す。
#[test]
fn verify_on_empty_table_without_row_table_reports_zero() {
    let (path, _guard) = open_fixture("gin-dml-empty-verify");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    for index in INDEXES {
        let a = audit(&core, "alice", index);
        assert_eq!(
            a,
            GinIndexAudit {
                marker_valid: false,
                missing: 0,
                stale: 0
            },
            "{index}"
        );
    }
}

/// INSERT・UPDATE・DELETE（単一行・述語形）の各経路で索引が全行抽出と一致し、他テナントは不変。
#[test]
fn dml_paths_keep_the_index_equal_to_a_full_extraction() {
    let (path, _guard) = open_fixture("gin-dml-paths");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    seed(&core, &mut s);
    assert_clean(&core, &["alice", "bob"], false);
    let bob_before = [audit(&core, "bob", "g_ops"), audit(&core, "bob", "g_path")];

    run_ok(
        &core,
        &mut s,
        "alice",
        "INSERT INTO docs (id, doc, body, embedding) VALUES (21, '{\"m\":1}', 'b', '[1,0]'), \
         (22, '[\"m\",\"n\"]', 'b', '[1,0]')",
    );
    assert_clean(&core, &["alice", "bob"], false);

    // jsonb 列を書き換える UPDATE（単一行・述語形）と、jsonb 列を変えない UPDATE。
    run_ok(
        &core,
        &mut s,
        "alice",
        "UPDATE docs SET doc = '{\"z\":9}' WHERE id = 1",
    );
    assert_clean(&core, &["alice", "bob"], false);
    run_ok(
        &core,
        &mut s,
        "alice",
        "UPDATE docs SET doc = '[\"w\",7]' WHERE id > 20",
    );
    assert_clean(&core, &["alice", "bob"], false);
    run_ok(
        &core,
        &mut s,
        "alice",
        "UPDATE docs SET body = 'changed' WHERE id = 2",
    );
    assert_clean(&core, &["alice", "bob"], false);
    run_ok(
        &core,
        &mut s,
        "alice",
        "UPDATE docs SET doc = NULL WHERE id = 2",
    );
    assert_clean(&core, &["alice", "bob"], false);

    // 削除は後片付けがあるので stale も 0。
    run_ok(&core, &mut s, "alice", "DELETE FROM docs WHERE id = 3");
    assert_clean(&core, &["alice", "bob"], true);
    run_ok(&core, &mut s, "alice", "DELETE FROM docs WHERE id > 20");
    assert_clean(&core, &["alice", "bob"], true);

    assert_eq!(
        bob_before,
        [audit(&core, "bob", "g_ops"), audit(&core, "bob", "g_path")],
        "alice's writes must not change bob's index"
    );
}

/// UPSERT（DO UPDATE・DO NOTHING）。
#[test]
fn upsert_paths_keep_the_index_consistent() {
    let (path, _guard) = open_fixture("gin-dml-upsert");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    insert(&core, &mut s, "alice", 1, r#"{"a":1}"#);
    run_ok(
        &core,
        &mut s,
        "alice",
        "INSERT INTO docs (id, doc, body, embedding) VALUES (1, '{\"upd\":2}', 'b', '[1,0]') \
         ON CONFLICT (id) DO UPDATE SET doc = EXCLUDED.doc",
    );
    assert_clean(&core, &["alice"], true);
    run_ok(
        &core,
        &mut s,
        "alice",
        "INSERT INTO docs (id, doc, body, embedding) VALUES (1, '{\"ignored\":3}', 'b', '[1,0]') \
         ON CONFLICT (id) DO NOTHING",
    );
    assert_clean(&core, &["alice"], true);
}

/// TRUNCATE は自テナントの範囲だけを空にし（マーカーは有効のまま）、他テナントは不変。
#[test]
fn truncate_clears_only_the_callers_range() {
    let (path, _guard) = open_fixture("gin-dml-truncate");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    seed(&core, &mut s);
    let bob_before = [audit(&core, "bob", "g_ops"), audit(&core, "bob", "g_path")];
    run_ok(&core, &mut s, "alice", "TRUNCATE docs");
    assert_eq!(count(&core, &mut s, "alice"), 0);
    assert_clean(&core, &["alice", "bob"], true);
    assert_eq!(
        bob_before,
        [audit(&core, "bob", "g_ops"), audit(&core, "bob", "g_path")]
    );
    // TRUNCATE 後の書き込みも維持される。
    insert(&core, &mut s, "alice", 31, r#"{"after":1}"#);
    assert_clean(&core, &["alice", "bob"], true);
}

/// COPY FROM と分割実行 DML。
#[test]
fn copy_and_partitioned_dml_are_maintained() {
    use engine::core::CopyPlan;
    let (path, _guard) = open_fixture("gin-dml-copy");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);

    let policy = ctx("alice");
    let session = SessionState::default();
    let plan = core
        .begin_copy(
            &policy,
            &session,
            "COPY docs (id, doc, embedding) FROM STDIN USING OPERATION_ID 'gin-copy-1'",
        )
        .expect("begin copy");
    let mut copy = match plan {
        CopyPlan::From(c) => c,
        CopyPlan::To(..) => panic!("expected COPY FROM"),
    };
    copy.feed(b"41\t{\"c\":1,\"d\":[1,2]}\t[1.0,0.0]\n42\t[\"e\"]\t[0.0,1.0]\n")
        .expect("feed");
    let batch = copy.finish().expect("finish");
    core.commit_copy_in(&policy, batch).expect("commit copy");
    assert_clean(&core, &["alice"], false);

    run_ok(
        &core,
        &mut s,
        "alice",
        "UPDATE docs SET doc = '{\"part\":1}' WHERE id > 0 \
         USING OPERATION_ID 'gin-pdml-u' PARTITIONED CHUNK 1",
    );
    assert_clean(&core, &["alice"], false);
    run_ok(
        &core,
        &mut s,
        "alice",
        "DELETE FROM docs WHERE id > 0 USING OPERATION_ID 'gin-pdml-d' PARTITIONED CHUNK 1",
    );
    assert_clean(&core, &["alice"], true);
}

/// 明示トランザクション: ROLLBACK で索引が不変、COMMIT で反映される。
#[test]
fn rollback_leaves_the_index_unchanged_and_commit_applies_it() {
    let (path, _guard) = open_fixture("gin-dml-txn");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    insert(&core, &mut s, "alice", 1, r#"{"keep":1}"#);
    let before = [
        audit(&core, "alice", "g_ops"),
        audit(&core, "alice", "g_path"),
    ];

    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut tx = |sql: &str| core.execute_sql_in_txn(&c, &mut session, &mut txn, sql);
    tx("BEGIN").expect("begin");
    tx("INSERT INTO docs (id, doc, body, embedding) VALUES (2, '{\"gone\":1}', 'b', '[1,0]')")
        .expect("insert in txn");
    tx("ROLLBACK").expect("rollback");
    assert_eq!(
        before,
        [
            audit(&core, "alice", "g_ops"),
            audit(&core, "alice", "g_path")
        ]
    );

    tx("BEGIN").expect("begin");
    tx("INSERT INTO docs (id, doc, body, embedding) VALUES (3, '{\"kept\":1}', 'b', '[1,0]')")
        .expect("insert in txn");
    tx("COMMIT").expect("commit");
    assert_clean(&core, &["alice"], true);
}

/// ALTER TABLE: 他の列の ADD／DROP は索引を保ち、RENAME COLUMN は次の書き込みで遅延再構築される。
#[test]
fn alter_table_keeps_or_lazily_rebuilds_the_index() {
    let (path, _guard) = open_fixture("gin-dml-alter");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    seed(&core, &mut s);

    run_ok(
        &core,
        &mut s,
        "alice",
        "ALTER TABLE docs ADD COLUMN extra TEXT",
    );
    assert_clean(&core, &["alice", "bob"], true);
    run_ok(&core, &mut s, "alice", "ALTER TABLE docs DROP COLUMN extra");
    assert_clean(&core, &["alice", "bob"], true);

    // 列名はシグネチャに含まれるため、改名後は各テナントの次の書き込みまでマーカーが不一致になる。
    run_ok(
        &core,
        &mut s,
        "alice",
        "ALTER TABLE docs RENAME COLUMN doc TO payload",
    );
    assert!(!audit(&core, "alice", "g_ops").marker_valid);
    run_ok(
        &core,
        &mut s,
        "alice",
        "INSERT INTO docs (id, payload, body, embedding) VALUES (51, '{\"r\":1}', 'b', '[1,0]')",
    );
    assert_clean(&core, &["alice"], true);
    assert!(!audit(&core, "bob", "g_ops").marker_valid);
    run_ok(
        &core,
        &mut s,
        "bob",
        "INSERT INTO docs (id, payload, body, embedding) VALUES (52, '{\"r\":2}', 'b', '[1,0]')",
    );
    assert_clean(&core, &["alice", "bob"], true);

    run_ok(&core, &mut s, "alice", "ALTER TABLE docs RENAME TO docs2");
    assert_clean(&core, &["alice", "bob"], true);
}

/// DROP INDEX → 同名で再作成しても、削除済み索引の残骸を引き継がない。
#[test]
fn recreating_an_index_does_not_inherit_leftovers() {
    let (path, _guard) = open_fixture("gin-dml-recreate");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    seed(&core, &mut s);
    run_ok(&core, &mut s, "alice", "DROP INDEX g_ops");
    run_ok(&core, &mut s, "alice", "DROP INDEX g_path");
    run_ok(&core, &mut s, "alice", "DELETE FROM docs WHERE id = 1");
    run_ok(
        &core,
        &mut s,
        "alice",
        "UPDATE docs SET doc = '{\"new\":1}' WHERE id = 2",
    );
    create_indexes(&core, &mut s);
    assert_clean(&core, &["alice", "bob"], true);
}

/// FK `ON DELETE CASCADE` で消えた子行のポスティングも後片付けされる。
#[test]
fn foreign_key_cascade_delete_cleans_up_child_postings() {
    let (path, _guard) = open_fixture("gin-dml-cascade");
    let core = core_at(&path);
    let mut s = allowed_session();
    run_ok(
        &core,
        &mut s,
        "alice",
        "CREATE TABLE parent (pid BIGINT PRIMARY KEY)",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "CREATE TABLE child (cid BIGINT PRIMARY KEY, pid BIGINT REFERENCES parent(pid) \
         ON DELETE CASCADE, doc JSONB)",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "CREATE INDEX cg_ops ON child USING gin (doc jsonb_ops)",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "CREATE INDEX cg_path ON child USING gin (doc jsonb_path_ops)",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "INSERT INTO parent (pid) VALUES (1), (2)",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "INSERT INTO child (cid, pid, doc) VALUES (1, 1, '{\"a\":1}'), (2, 1, '[\"x\"]'), \
         (3, 2, '{\"b\":2}')",
    );
    for index in ["cg_ops", "cg_path"] {
        let a = audit(&core, "alice", index);
        assert!(a.marker_valid && a.missing == 0 && a.stale == 0, "{a:?}");
    }
    run_ok(&core, &mut s, "alice", "DELETE FROM parent WHERE pid = 1");
    for index in ["cg_ops", "cg_path"] {
        let a = audit(&core, "alice", index);
        assert!(a.marker_valid, "{a:?}");
        assert_eq!(
            (a.missing, a.stale),
            (0, 0),
            "{index}: cascade must clean up: {a:?}"
        );
    }
}

/// 一時表でも維持され、`ON COMMIT DELETE ROWS` と TRUNCATE が通る。
#[test]
fn temp_table_gin_index_is_maintained() {
    let (path, _guard) = open_fixture("gin-dml-temp");
    let core = core_at(&path);
    let mut s = allowed_session();
    run_ok(
        &core,
        &mut s,
        "alice",
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, doc JSONB) ON COMMIT DELETE ROWS",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "CREATE INDEX tg ON tmp USING gin (doc)",
    );
    // 明示トランザクションの COMMIT で `ON COMMIT DELETE ROWS` の掃除（一時コアの TRUNCATE 経路）を走らせる。
    // 一時表の索引内容は `verify_gin_index_read_only`（永続コアのみ）から見えないため、ここでは経路が
    // エラーなく通ることまでを固定する。
    let c = ctx("alice");
    let mut txn = core.new_session_transaction();
    let mut tx = |sql: &str| core.execute_sql_in_txn(&c, &mut s, &mut txn, sql);
    tx("BEGIN").expect("begin");
    tx("INSERT INTO tmp (id, doc) VALUES (7, '{\"t\":1}')").expect("insert in txn");
    tx("COMMIT").expect("commit runs ON COMMIT DELETE ROWS");
    run_ok(
        &core,
        &mut s,
        "alice",
        "INSERT INTO tmp (id, doc) VALUES (1, '{\"a\":1}')",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "UPDATE tmp SET doc = '[1]' WHERE id = 1",
    );
    run_ok(&core, &mut s, "alice", "DELETE FROM tmp WHERE id = 1");
    run_ok(&core, &mut s, "alice", "TRUNCATE tmp");
}

/// キー長の上限超過は `54000`。INSERT・UPDATE・COPY は副作用ゼロ、CREATE INDEX は宣言も残らない。
/// エラー文言にテナント・値・件数を含めない。
#[test]
fn oversized_keys_are_rejected_with_54000_and_no_side_effects() {
    let (path, _guard) = open_fixture("gin-dml-limit");
    let core = core_at(&path);
    let mut s = allowed_session();
    // 上限超過の行は索引が無い間に入れておく（CREATE INDEX の拒否を確かめるため）。
    let big_key = "k".repeat(4_097);
    insert(&core, &mut s, "alice", 1, &format!(r#"{{"{big_key}":1}}"#));
    let e = err(
        &core,
        &mut s,
        "alice",
        "CREATE INDEX g_ops ON docs USING gin (doc)",
    );
    assert_eq!(e.wire_code(), "54000");
    assert!(!e.to_string().contains("alice") && !e.to_string().contains("kkkk"));
    // 失敗した CREATE INDEX は宣言を残さない（下の再作成が同名で通ることで確かめる）。
    run_ok(&core, &mut s, "alice", "DELETE FROM docs WHERE id = 1");
    create_indexes(&core, &mut s);
    insert(&core, &mut s, "alice", 2, r#"{"ok":1}"#);
    let before = [
        audit(&core, "alice", "g_ops"),
        audit(&core, "alice", "g_path"),
    ];

    // INSERT: 行が入らず索引も不変。
    let e = err(
        &core,
        &mut s,
        "alice",
        &format!(
            "INSERT INTO docs (id, doc, body, embedding) VALUES (3, '{{\"{big_key}\":1}}', 'b', '[1,0]')"
        ),
    );
    assert_eq!(e.wire_code(), "54000");
    let msg = e.to_string();
    assert!(!msg.contains("alice") && !msg.contains("kkkk"), "{msg}");
    assert_eq!(count(&core, &mut s, "alice"), 1);
    assert_eq!(
        before,
        [
            audit(&core, "alice", "g_ops"),
            audit(&core, "alice", "g_path")
        ]
    );

    // UPDATE: 旧値のまま。
    let e = err(
        &core,
        &mut s,
        "alice",
        &format!("UPDATE docs SET doc = '{{\"{big_key}\":1}}' WHERE id = 2"),
    );
    assert_eq!(e.wire_code(), "54000");
    assert_eq!(
        before,
        [
            audit(&core, "alice", "g_ops"),
            audit(&core, "alice", "g_path")
        ]
    );
    assert_clean(&core, &["alice"], true);

    // COPY: 全体が拒否される。
    use engine::core::CopyPlan;
    let policy = ctx("alice");
    let session = SessionState::default();
    let plan = core
        .begin_copy(
            &policy,
            &session,
            "COPY docs (id, doc, embedding) FROM STDIN USING OPERATION_ID 'gin-limit-copy'",
        )
        .expect("begin copy");
    let CopyPlan::From(mut copy) = plan else {
        panic!("expected COPY FROM");
    };
    let line = format!("9\t{{\"{big_key}\":1}}\t[1.0,0.0]\n");
    copy.feed(line.as_bytes()).expect("feed");
    let batch = copy.finish().expect("finish");
    let e = core
        .commit_copy_in(&policy, batch)
        .expect_err("copy must be rejected");
    assert_eq!(e.wire_code(), "54000");
    assert_eq!(count(&core, &mut s, "alice"), 1);
    assert_eq!(
        before,
        [
            audit(&core, "alice", "g_ops"),
            audit(&core, "alice", "g_path")
        ]
    );
}

/// 1 値あたりのキー数の上限超過は `54000`（`jsonb_ops`: 約 8 万エントリ）。
#[test]
fn too_many_keys_in_one_value_is_rejected_with_54000() {
    let (path, _guard) = open_fixture("gin-dml-count-limit");
    let core = core_at(&path);
    let mut s = allowed_session();
    let body: Vec<String> = (0..40_000).map(|i| format!("\"k{i}\":{i}")).collect();
    let doc = format!("{{{}}}", body.join(","));
    let insert_sql =
        format!("INSERT INTO docs (id, doc, body, embedding) VALUES (1, '{doc}', 'b', '[1,0]')");
    // 対照: 索引が無ければ同じ行は受理される（54000 の出どころが GIN の上限であることの確認）。
    run_ok(&core, &mut s, "alice", &insert_sql);
    let e = err(
        &core,
        &mut s,
        "alice",
        "CREATE INDEX g_ops ON docs USING gin (doc jsonb_ops)",
    );
    assert_eq!(e.wire_code(), "54000");
    run_ok(&core, &mut s, "alice", "DELETE FROM docs WHERE id = 1");
    run_ok(
        &core,
        &mut s,
        "alice",
        "CREATE INDEX g_ops ON docs USING gin (doc jsonb_ops)",
    );
    let e = err(&core, &mut s, "alice", &insert_sql);
    assert_eq!(e.wire_code(), "54000");
    assert_eq!(count(&core, &mut s, "alice"), 0);
}

/// ビュー経由の DML と `ALTER TABLE ... SET SCHEMA` でも索引が維持される。
#[test]
fn view_dml_and_set_schema_keep_the_index_consistent() {
    let (path, _guard) = open_fixture("gin-dml-view-schema");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    seed(&core, &mut s);
    run_ok(
        &core,
        &mut s,
        "alice",
        "CREATE VIEW v AS SELECT id, doc, body, embedding FROM docs",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "INSERT INTO v (id, doc, body, embedding) VALUES (61, '{\"v\":1}', 'b', '[1,0]')",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "UPDATE v SET doc = '[\"vv\"]' WHERE id = 61",
    );
    assert_clean(&core, &["alice", "bob"], false);
    run_ok(&core, &mut s, "alice", "DELETE FROM v WHERE id = 61");
    assert_clean(&core, &["alice", "bob"], true);

    // テナント SCHEMA への実移動は #1717 の担当で未到達（`public` から `public` は no-op）。物理名は索引名由来で
    // シグネチャに表名・SCHEMA を含めないため、移動しても索引は有効のまま（no-op でも壊れないことだけ固定する）。
    run_ok(&core, &mut s, "alice", "ALTER TABLE docs SET SCHEMA public");
    assert_clean(&core, &["alice", "bob"], true);
}

/// `ALTER COLUMN ... TYPE JSONB USING <式>` は行を直接書き換えるが、GIN 索引は同一 txn で新しい値から
/// 再構築される（欠落 0）。再構築で抽出上限を超える行があれば `54000` で、行も索引も変わらない。
#[test]
fn alter_column_type_using_rebuilds_gin_index_and_enforces_limits() {
    let (path, _guard) = open_fixture("gin-dml-alter-using");
    let core = core_at(&path);
    let mut s = allowed_session();
    create_indexes(&core, &mut s);
    let big_key = "k".repeat(4_097);
    for (tenant, id, doc, raw) in [
        ("alice", 1, r#"{"a":1}"#, r#"{"zz":2,"tags":["m"]}"#),
        ("bob", 11, r#"{"b":1}"#, r#"[1,2]"#),
    ] {
        run_ok(
            &core,
            &mut s,
            tenant,
            &format!(
                "INSERT INTO docs (id, doc, raw, body, embedding) VALUES ({id}, '{doc}', '{raw}', 'b', '[1,0]')"
            ),
        );
    }
    assert_clean(&core, &["alice", "bob"], true);

    // 上限超過: bob の行（他テナント）の変換結果が超過しても拒否され、副作用はゼロ。
    run_ok(
        &core,
        &mut s,
        "bob",
        &format!("UPDATE docs SET raw = '{{\"{big_key}\":1}}' WHERE id = 11"),
    );
    let e = err(
        &core,
        &mut s,
        "alice",
        "ALTER TABLE docs ALTER COLUMN doc TYPE JSONB USING raw::jsonb",
    );
    assert_eq!(e.wire_code(), "54000");
    let msg = e.to_string();
    assert!(!msg.contains("bob") && !msg.contains("kkkk"), "{msg}");
    assert_clean(&core, &["alice", "bob"], true);

    // 超過行を直せば通り、索引は新しい値と一致する。
    run_ok(
        &core,
        &mut s,
        "bob",
        "UPDATE docs SET raw = '[1,2]' WHERE id = 11",
    );
    run_ok(
        &core,
        &mut s,
        "alice",
        "ALTER TABLE docs ALTER COLUMN doc TYPE JSONB USING raw::jsonb",
    );
    assert_clean(&core, &["alice", "bob"], true);
}
