//! MATERIALIZED VIEW への索引と `DROP MATERIALIZED VIEW`／依存の連鎖削除（TABLE-42 (5)(6)・
//! TABLE-37・TABLE-36 (4)・INDEX-7・INDEX-9、Issue #1667）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` の TABLE-42・TABLE-37、RLS-10 (b)。
//!
//! `table42_create_materialized_view.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。
//!
//! 検証する契約:
//! 1. 索引の作成・削除は matview の読み取り結果を変えず、他テナントの行・件数は観測できない
//! 2. UNIQUE 索引は文脈（テナント）ごとに判定する。重複時の 23505 は固定文言でカタログ不変
//! 3. DROP は全文脈の実体と索引宣言を消し、同名の再作成で旧データが復活しない（Public のみの
//!    文脈を含め `42P01`）。種別違いは `42809`（`IF EXISTS` でも）。権限は存在確認より先
//! 4. RESTRICT は `2BP01`（副作用ゼロ）、CASCADE は表→ビュー→matview・matview→matview の
//!    各経路を依存する側から削除する

use engine::catalog::{CascadedObject, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn public_only_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn insert_row(
    storage: &Storage,
    tenant: &str,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// alice: public 3・private 1。bob: public 1・private 2。carol: private 1。
fn seed(storage: &Storage) {
    insert_row(storage, "alice", 1, "ja", "a1", Visibility::Public);
    insert_row(storage, "alice", 2, "ja", "a2", Visibility::Public);
    insert_row(storage, "alice", 3, "en", "a3", Visibility::Public);
    insert_row(storage, "alice", 4, "ja", "a4", Visibility::Private);
    insert_row(storage, "bob", 5, "ja", "b1", Visibility::Public);
    insert_row(storage, "bob", 6, "ja", "b2", Visibility::Private);
    insert_row(storage, "bob", 7, "en", "b3", Visibility::Private);
    insert_row(storage, "carol", 8, "ja", "c1", Visibility::Private);
}

fn open_storage(label: &str) -> (Storage, std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage);
    (storage, path, guard)
}

fn all_contexts() -> Vec<PolicyContext> {
    vec![ctx("alice"), ctx("bob"), ctx("carol")]
}

fn open_core(label: &str) -> (EngineCore, std::path::PathBuf, CleanupGuard) {
    let (storage, path, guard) = open_storage(label);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_materialization_contexts(all_contexts());
    (core, path, guard)
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = allowed_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
}

fn ok(core: &EngineCore, sql: &str) -> SqlOutcome {
    run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn code(core: &EngineCore, sql: &str) -> String {
    run(core, sql)
        .err()
        .unwrap_or_else(|| panic!("must fail: {sql}"))
        .wire_code()
        .to_string()
}

fn query_with(
    core: &EngineCore,
    context: &PolicyContext,
    sql: &str,
) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(context, &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    query_with(core, &ctx(tenant), sql)
}

fn read_code(core: &EngineCore, context: &PolicyContext, sql: &str) -> String {
    query_with(core, context, sql)
        .err()
        .unwrap_or_else(|| panic!("must fail: {sql}"))
        .wire_code()
        .to_string()
}

fn rows_of(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| {
            r.cells
                .iter()
                .map(|c| format!("{c:?}"))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// 各テナントで matview の読み取りと本文の直接実行の結果が一致する。
fn assert_matches_direct(core: &EngineCore, mv_sql: &str, direct_sql: &str) {
    for tenant in ["alice", "bob", "carol"] {
        let via_mv = query(core, tenant, mv_sql).unwrap_or_else(|e| panic!("{mv_sql}: {e:?}"));
        let direct =
            query(core, tenant, direct_sql).unwrap_or_else(|e| panic!("{direct_sql}: {e:?}"));
        assert_eq!(rows_of(&via_mv), rows_of(&direct), "tenant {tenant}");
    }
}

const MV_BODY: &str = "CREATE MATERIALIZED VIEW mv_b AS SELECT lang, body FROM docs";
const READ_MV: &str = "SELECT lang, body FROM mv_b ORDER BY body LIMIT 100";
const READ_DIRECT: &str = "SELECT lang, body FROM docs ORDER BY body LIMIT 100";

// --- 索引 -----------------------------------------------------------------------------

#[test]
fn index_creation_keeps_reads_identical_and_tenant_isolated() {
    let (core, _p, _g) = open_core("t42i-iso");
    ok(&core, MV_BODY);
    assert_matches_direct(&core, READ_MV, READ_DIRECT);
    ok(&core, "CREATE INDEX ix_body ON mv_b (body)");
    ok(&core, "CREATE UNIQUE INDEX ux_body ON mv_b (body)");
    // 索引の作成後も、各テナントは自分の文脈の行だけを読む。
    assert_matches_direct(&core, READ_MV, READ_DIRECT);
    let bob = query(&core, "bob", READ_MV).unwrap();
    let text = rows_of(&bob).join("\n");
    assert!(text.contains("b2") && text.contains("b3"));
    assert!(!text.contains("a4"), "alice private leaked");
    assert!(!text.contains("c1"), "carol private leaked");
    // 実体化対象外の文脈は索引があっても `55000`（実体は文脈キー完全一致）。
    assert_eq!(
        read_code(&core, &public_only_ctx("alice"), READ_MV),
        "55000"
    );
    assert_eq!(read_code(&core, &ctx("dave"), READ_MV), "55000");
    // DROP INDEX で外せる。読み取りは不変。
    ok(&core, "DROP INDEX ux_body");
    ok(&core, "DROP INDEX ix_body");
    assert_matches_direct(&core, READ_MV, READ_DIRECT);
}

#[test]
fn index_declaration_errors_follow_the_sql_contract() {
    let (core, _p, _g) = open_core("t42i-errors");
    ok(&core, MV_BODY);
    ok(&core, "CREATE INDEX ix_body ON mv_b (body)");
    // IF NOT EXISTS は同名の索引があれば no-op。
    ok(&core, "CREATE INDEX IF NOT EXISTS ix_body ON mv_b (body)");
    // 名前衝突（索引・表・matview）。
    for name in ["ix_body", "docs", "mv_b"] {
        assert_eq!(
            code(&core, &format!("CREATE INDEX {name} ON mv_b (lang)")),
            "42P07"
        );
    }
    // matview に暗黙の id 列は無い。
    assert_eq!(code(&core, "CREATE INDEX ix_id ON mv_b (id)"), "42703");
    assert_eq!(code(&core, "CREATE INDEX ix_x ON mv_b (nope)"), "42703");
    // HNSW は対象外。
    assert_eq!(
        code(&core, "CREATE INDEX ix_h ON mv_b USING hnsw (body)"),
        "0A000"
    );
    // 存在しない matview は 42P01、索引名・ビュー名を指定すると 42809。
    assert_eq!(code(&core, "CREATE INDEX ix_m ON ghost (body)"), "42P01");
    assert_eq!(code(&core, "CREATE INDEX ix_m ON ix_body (body)"), "42809");
    // DDL 権限が無ければ存在の有無によらず 42501（名前解決より先）。
    for sql in [
        "CREATE INDEX ix_p ON mv_b (body)",
        "CREATE INDEX ix_p ON ghost (body)",
        "DROP INDEX ix_body",
    ] {
        let mut session = SessionState::default();
        let e = core
            .execute_sql_in_session(&ctx("alice"), &mut session, sql)
            .expect_err("denied");
        assert_eq!(e.wire_code(), "42501", "{sql}");
    }
}

#[test]
fn unique_index_is_checked_per_tenant_context_without_leaking() {
    let (storage, _p, _g) = open_storage("t42i-unique");
    // bob の文脈（public 1 + 自分の private）だけに body の重複を作る。
    insert_row(&storage, "bob", 9, "en", "b2", Visibility::Private);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_materialization_contexts(all_contexts());
    ok(&core, MV_BODY);
    // alice・carol の文脈は一意だが、bob の文脈に重複があるため作成は拒否される。
    let mut session = allowed_session();
    let e = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut session,
            "CREATE UNIQUE INDEX ux_b ON mv_b (body)",
        )
        .expect_err("duplicate in bob's context");
    assert_eq!(e.wire_code(), "23505");
    let msg = format!("{e} {e:?}");
    for leaked in ["bob", "b2", "alice", "carol"] {
        assert!(!msg.contains(leaked), "error leaks {leaked}: {msg}");
    }
    // カタログは不変: 同名の通常索引をそのまま作れる（宣言が残っていない）。
    ok(&core, "CREATE INDEX ux_b ON mv_b (body)");
    // 一意なキー（lang+body の組）なら全文脈で成立する。
    ok(&core, "CREATE UNIQUE INDEX ux_lb ON mv_b (lang, body)");
    // 文脈をまたいだ同値は重複ではない（a1/b1 の lang は共有しても組は文脈内で別）。
    let alice = query(&core, "alice", READ_MV).unwrap();
    assert_eq!(alice.rows.len(), 5);
}

// --- DROP ---------------------------------------------------------------------------

#[test]
fn drop_removes_every_context_and_recreate_does_not_resurrect_data() {
    let (core, path, guard) = open_core("t42d-drop");
    ok(&core, MV_BODY);
    ok(&core, "CREATE UNIQUE INDEX ux_body ON mv_b (body)");
    assert!(matches!(
        ok(&core, "DROP MATERIALIZED VIEW mv_b"),
        SqlOutcome::DropMaterializedView(_)
    ));
    // 全文脈（Public のみの文脈・未注入のテナントを含む）で 42P01。55000 ではない。
    for context in [
        ctx("alice"),
        ctx("bob"),
        ctx("carol"),
        public_only_ctx("alice"),
        ctx("dave"),
    ] {
        assert_eq!(read_code(&core, &context, READ_MV), "42P01");
    }
    // 同名で WITH NO DATA 再作成 -> 全文脈 55000（旧実体が復活しない）。索引名も再利用できる。
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_b AS SELECT lang, body FROM docs WITH NO DATA",
    );
    for context in [
        ctx("alice"),
        ctx("bob"),
        ctx("carol"),
        public_only_ctx("bob"),
    ] {
        assert_eq!(read_code(&core, &context, READ_MV), "55000");
    }
    ok(&core, "CREATE INDEX ux_body ON mv_b (body)");
    ok(&core, "DROP MATERIALIZED VIEW mv_b");
    // 再オープン後も削除状態が保たれる。
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_materialization_contexts(all_contexts());
    assert_eq!(read_code(&core, &ctx("alice"), READ_MV), "42P01");
    drop(guard);
}

#[test]
fn drop_contract_errors_multi_and_if_exists() {
    let (core, _p, _g) = open_core("t42d-contract");
    ok(&core, MV_BODY);
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_c AS SELECT body FROM docs",
    );
    ok(&core, "CREATE VIEW v_d AS SELECT body FROM docs");
    ok(&core, "CREATE INDEX ix_body ON mv_b (body)");
    assert_eq!(code(&core, "DROP MATERIALIZED VIEW ghost"), "42P01");
    // IF EXISTS は不在だけを無視し、種別違いは 42809 のまま。
    match ok(&core, "DROP MATERIALIZED VIEW IF EXISTS ghost") {
        SqlOutcome::DropMaterializedView(o) => assert!(o.skipped()),
        other => panic!("unexpected {other:?}"),
    }
    for name in ["docs", "v_d", "ix_body"] {
        for sql in [
            format!("DROP MATERIALIZED VIEW {name}"),
            format!("DROP MATERIALIZED VIEW IF EXISTS {name}"),
        ] {
            assert_eq!(code(&core, &sql), "42809", "{sql}");
        }
    }
    // 逆向きの種別違い（既存契約）: DROP VIEW / DROP TABLE / DROP INDEX に matview 名。
    for sql in [
        "DROP VIEW mv_b",
        "DROP TABLE mv_b",
        "DROP INDEX mv_b",
        "DROP TABLE IF EXISTS mv_b",
    ] {
        assert_eq!(code(&core, sql), "42809", "{sql}");
    }
    // 複数指定は原子的: 1 件でも不在なら何も消えない。
    assert_eq!(
        code(&core, "DROP MATERIALIZED VIEW mv_b, ghost, mv_c"),
        "42P01"
    );
    assert!(query(&core, "alice", READ_MV).is_ok());
    // IF EXISTS 付きの複数指定・重複指定・RESTRICT 明示。
    ok(
        &core,
        "DROP MATERIALIZED VIEW IF EXISTS mv_b, ghost, mv_c, mv_b RESTRICT",
    );
    assert_eq!(read_code(&core, &ctx("alice"), READ_MV), "42P01");
    // 構文: 余剰トークン・末尾カンマは 42601。
    for sql in [
        "DROP MATERIALIZED VIEW",
        "DROP MATERIALIZED VIEW a,",
        "DROP MATERIALIZED VIEW a b",
        "DROP MATERIALIZED VIEW a CASCADE RESTRICT",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn drop_permission_gate_comes_first_and_transactions_are_rejected() {
    let (core, _p, _g) = open_core("t42d-gate");
    ok(&core, MV_BODY);
    for sql in [
        "DROP MATERIALIZED VIEW mv_b",
        "DROP MATERIALIZED VIEW ghost",
        "DROP MATERIALIZED VIEW docs",
    ] {
        let mut session = SessionState::default();
        let e = core
            .execute_sql_in_session(&ctx("alice"), &mut session, sql)
            .expect_err("denied");
        assert_eq!(e.wire_code(), "42501", "{sql}");
    }
    assert!(query(&core, "alice", READ_MV).is_ok());
    let caller = ctx("alice");
    let mut session = allowed_session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "DROP MATERIALIZED VIEW mv_b",
        )
        .expect_err("ddl in txn");
    assert_eq!(e.wire_code(), "0A000");
}

// --- 依存（RESTRICT／CASCADE）---------------------------------------------------------

/// `relation` が表・ビュー・matview のいずれとしても存在しない（`DROP MATERIALIZED VIEW` が
/// 種別違い `42809` ではなく不在 `42P01` を返す）。
fn gone(core: &EngineCore, relation: &str) -> bool {
    code(core, &format!("DROP MATERIALIZED VIEW {relation}")) == "42P01"
}

fn cascaded_of(outcome: &SqlOutcome) -> Vec<CascadedObject> {
    match outcome {
        SqlOutcome::DropTable(o) => o.cascaded().to_vec(),
        SqlOutcome::DropView(o) => o.cascaded().to_vec(),
        SqlOutcome::DropMaterializedView(o) => o.cascaded().to_vec(),
        other => panic!("unexpected {other:?}"),
    }
}

fn mv(name: &str) -> CascadedObject {
    CascadedObject::MaterializedView {
        name: name.to_string(),
    }
}

#[test]
fn restrict_and_cascade_follow_table_view_matview_chains() {
    let (core, _p, _g) = open_core("t42d-chain");
    ok(&core, "CREATE VIEW v1 AS SELECT lang, body FROM docs");
    ok(&core, "CREATE VIEW v2 AS SELECT lang, body FROM v1");
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_on_v2 AS SELECT body FROM v2",
    );
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_on_mv AS SELECT body FROM mv_on_v2",
    );
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_direct AS SELECT body FROM docs",
    );
    // RESTRICT は 2BP01 で副作用ゼロ。
    for sql in [
        "DROP TABLE docs",
        "DROP VIEW v1",
        "DROP VIEW v2",
        "DROP MATERIALIZED VIEW mv_on_v2",
        "DROP TABLE docs RESTRICT",
    ] {
        assert_eq!(code(&core, sql), "2BP01", "{sql}");
    }
    for relation in ["mv_on_v2", "mv_on_mv", "mv_direct"] {
        assert!(
            query(
                &core,
                "alice",
                &format!("SELECT body FROM {relation} LIMIT 1")
            )
            .is_ok(),
            "{relation}"
        );
    }
    // 並べた名前どうしの依存は依存に数えない。
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_pair_a AS SELECT body FROM docs",
    );
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_pair_b AS SELECT body FROM mv_pair_a",
    );
    ok(&core, "DROP MATERIALIZED VIEW mv_pair_a, mv_pair_b");

    // matview -> matview の CASCADE。
    let cascaded = cascaded_of(&ok(&core, "DROP MATERIALIZED VIEW mv_on_v2 CASCADE"));
    assert_eq!(cascaded, vec![mv("mv_on_mv")]);
    assert!(gone(&core, "mv_on_mv") && gone(&core, "mv_on_v2"));
    // 巻き戻して、ビュー -> ビュー -> matview の CASCADE。
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_on_v2 AS SELECT body FROM v2",
    );
    let cascaded = cascaded_of(&ok(&core, "DROP VIEW v1 CASCADE"));
    let pos = |o: &CascadedObject| cascaded.iter().position(|c| c == o).expect("present");
    assert!(
        pos(&mv("mv_on_v2"))
            < pos(&CascadedObject::View {
                name: "v2".to_string()
            })
    );
    assert!(gone(&core, "mv_on_v2"));
    // 表 -> matview の CASCADE（表そのものも消える）。
    let cascaded = cascaded_of(&ok(&core, "DROP TABLE docs CASCADE"));
    assert!(cascaded.contains(&mv("mv_direct")));
    assert!(gone(&core, "mv_direct") && gone(&core, "docs"));
}

#[test]
fn cascade_reports_only_catalog_object_names() {
    let (core, _p, _g) = open_core("t42d-names");
    ok(&core, MV_BODY);
    ok(&core, "CREATE UNIQUE INDEX ux_body ON mv_b (body)");
    let e = run(&core, "DROP TABLE docs").expect_err("restrict");
    assert_eq!(e.wire_code(), "2BP01");
    let msg = format!("{e} {e:?}");
    for leaked in ["alice", "bob", "carol", "a1", "b2", "c1"] {
        assert!(!msg.contains(leaked), "2BP01 leaks {leaked}: {msg}");
    }
    let cascaded = cascaded_of(&ok(&core, "DROP TABLE docs CASCADE"));
    let rendered = format!("{cascaded:?}");
    for leaked in ["alice", "bob", "carol", "a1", "b2", "c1", "ux_body"] {
        assert!(!rendered.contains(leaked), "cascaded leaks {leaked}");
    }
    // matview の索引宣言も同時に消えている: 同名の索引を別の matview へ作り直せる。
    ok(&core, "CREATE TABLE t2 (body TEXT)");
    ok(
        &core,
        "CREATE MATERIALIZED VIEW mv_n AS SELECT body FROM t2",
    );
    ok(&core, "CREATE INDEX ux_body ON mv_n (body)");
}
