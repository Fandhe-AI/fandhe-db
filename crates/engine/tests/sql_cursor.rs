//! カーソル（`DECLARE`/`FETCH`/`CLOSE`。WIRE-15・TASK-218）の結合テスト。
//! ポインタ: `docs/spec/05-tasks.md` TASK-218・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-15・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql31_transaction.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore::execute_sql_in_txn` を
//! production 経路として検証する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{TransactionLimits, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-cursor");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn new_core_with_limits(limits: TransactionLimits) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-cursor-limits");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_transaction_limits(limits),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[1.0, 0.0]', '{lang}') \
         USING OPERATION_ID '{op_id}'"
    )
}

fn seed_rows(engine: &EngineCore, caller: &PolicyContext, n: u64) {
    let mut session = SessionState::default();
    for id in 1..=n {
        engine
            .execute_sql_in_session(
                caller,
                &mut session,
                &insert_sql(id, "ja", &format!("seed-{id}")),
            )
            .expect("seed insert");
    }
}

fn row_ids(result: &engine::sql::exec::QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

/// (a) `BEGIN` → `DECLARE`（広域取得 `SELECT`）→ 複数回 `FETCH` の全ページの
/// 和集合が、同じ内容を直接実行した結果と一致すること（順序も含め決定的）。
#[test]
fn declare_scan_and_paginated_fetch_matches_direct_execution() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");

    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 100");
    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
            .expect("declare"),
        SqlOutcome::DeclareCursor
    );

    let mut fetched: Vec<u64> = Vec::new();
    loop {
        let outcome = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 2 FROM c")
            .expect("fetch");
        let SqlOutcome::Fetch(result) = outcome else {
            panic!("expected Fetch outcome");
        };
        if result.rows.is_empty() {
            break;
        }
        fetched.extend(row_ids(&result));
    }
    fetched.sort_unstable();

    assert_eq!(
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "CLOSE c")
            .expect("close"),
        SqlOutcome::CloseCursor
    );
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");

    let direct = engine
        .execute_sql_in_session(
            &caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} LIMIT 100"),
        )
        .expect("direct scan");
    let SqlOutcome::Query(direct_result) = direct else {
        panic!("expected Query outcome");
    };
    let mut direct_ids = row_ids(&direct_result);
    direct_ids.sort_unstable();

    assert_eq!(fetched, direct_ids);
}

/// `DECLARE`（集計 `SELECT`）も受理され、`FETCH` で単一行を取得できる。
#[test]
fn declare_aggregate_and_fetch_returns_single_row() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 3);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let declare_sql = format!("DECLARE agg CURSOR FOR SELECT COUNT(*) AS n FROM {TABLE}");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare aggregate");

    let outcome = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM agg")
        .expect("fetch");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome");
    };
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells, vec![Cell::Integer(3)]);

    // 末尾に達しているため 2 回目の FETCH は 0 行。
    let outcome = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM agg")
        .expect("second fetch");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome");
    };
    assert!(result.rows.is_empty());

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");
}

/// (b) トランザクション外の `DECLARE` は `25P01`、`FETCH`／`CLOSE` は `34000`。
#[test]
fn cursor_statements_outside_transaction_are_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();

    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    let err = engine
        .execute_sql_in_session(&caller, &mut session, &declare_sql)
        .expect_err("DECLARE outside transaction must be rejected");
    assert_eq!(err.wire_code(), "25P01");

    let err = engine
        .execute_sql_in_session(&caller, &mut session, "FETCH 1 FROM c")
        .expect_err("FETCH outside transaction must be rejected");
    assert_eq!(err.wire_code(), "34000");

    let err = engine
        .execute_sql_in_session(&caller, &mut session, "CLOSE c")
        .expect_err("CLOSE outside transaction must be rejected");
    assert_eq!(err.wire_code(), "34000");
}

/// (b') PR #1049 レビュー指摘（P1）の回帰: トランザクション外の `DECLARE` は、
/// 内側 SELECT が存在しないテーブルを指していても `25P01`
/// （[`engine::sql::allowlist::SqlSurfaceError::NoActiveSqlTransaction`]）を
/// 返す——`UndefinedTable`（テーブル存在確認）がトランザクション状態の判定
/// より先に走ってはならない。`execute_sql_in_session`（トランザクション文脈を
/// 持たない入口）・`execute_sql_in_txn`（`Idle`＝未 `BEGIN`）の双方で確認する。
#[test]
fn declare_outside_transaction_against_missing_table_is_25p01_not_undefined_table() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();

    let declare_sql = "DECLARE c CURSOR FOR SELECT id FROM missing_table LIMIT 10";

    let err = engine
        .execute_sql_in_session(&caller, &mut session, declare_sql)
        .expect_err("DECLARE outside transaction against a missing table must still be 25P01");
    assert_eq!(err.wire_code(), "25P01");

    let mut txn = engine.new_session_transaction();
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, declare_sql)
        .expect_err("DECLARE before BEGIN against a missing table must still be 25P01");
    assert_eq!(err.wire_code(), "25P01");
}

/// (b'') PR #1049 レビュー指摘（P1）の回帰・拡張クエリプロトコル経路:
/// [`EngineCore::parse_sql_prepared`]（Parse。`$n` を含まない SQL は
/// [`EngineCore::parse_sql`] と完全に同一の構造検証結果を返す契約）は、
/// `DECLARE` の内側 SELECT が存在しないテーブルを指していてもカタログへは
/// 問い合わせず構造検証だけで成功する——`core.rs::parse_tokens` の `DECLARE`
/// 分岐がカタログ照会（テーブル存在確認）を実行時（`Active` なトランザクション
/// 内での内側 SELECT 実際の実行）まで遅延させているため。Parse 単体では
/// テーブルの存在有無を一切観測できず、`UndefinedTable` を返してしまう
/// （＝トランザクション状態が未確定な Parse の時点でカタログの状態を漏らす）
/// 経路が無いことを固定する。
#[test]
fn parse_sql_prepared_declare_defers_catalog_lookup_past_parse() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);

    let declare_sql = "DECLARE c CURSOR FOR SELECT id FROM missing_table LIMIT 10";
    engine
        .parse_sql_prepared(declare_sql)
        .expect("Parse of DECLARE must not touch the catalog and must succeed structurally");
}

/// (c) `COMMIT`／`ROLLBACK` の後、開いていたカーソルは自動的に消える
/// （新しいトランザクションからの `FETCH` は `34000`）。`Failed` 中の
/// `FETCH`／`CLOSE`／`DECLARE` はいずれも `25P02`。
#[test]
fn cursors_are_closed_automatically_on_commit_and_rollback() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 2);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 1");
    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");

    // 新しいトランザクションからは同名のカーソルは存在しない。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 2");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("cursor must not survive COMMIT");
    assert_eq!(err.wire_code(), "34000");
    // エラーにより `Failed` へ遷移済み。`ROLLBACK` 以外は `25P02`。
    assert_eq!(txn.status(), TransactionStatus::Failed);
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("Failed transaction rejects FETCH");
    assert_eq!(err.wire_code(), "25P02");
    let declare_sql2 = format!("DECLARE c2 CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql2)
        .expect_err("Failed transaction rejects DECLARE");
    assert_eq!(err.wire_code(), "25P02");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    // ROLLBACK でも同様に自動クローズされる。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 3");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare again");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback 2");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 4");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("cursor must not survive ROLLBACK");
    assert_eq!(err.wire_code(), "34000");
}

/// スナップショットの一貫性（INSENSITIVE）: `DECLARE` の**後**に同じテーブルへ
/// 書き込むこと自体は妨げず、その書き込みは確定済みのカーソルの内容には影響しない。
/// 既に書き込み済みのテーブルに対する `DECLARE` は、自トランザクションの未 commit の
/// 変更を反映した内容で確定する（Issue #1179。書き込みトランザクションを読み取り源に
/// する）。
#[test]
fn declare_and_write_to_the_same_table_in_one_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 1);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &insert_sql(100, "ja", "op-w1"),
        )
        .expect("insert after DECLARE must still succeed (DECLARE is not a write)");
    // DECLARE 時点で確定したカーソルの内容（1 行）は、後続の書き込みの影響を受けない。
    let outcome = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM c")
        .expect("fetch");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome");
    };
    assert_eq!(result.rows.len(), 1);

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    // 先に書き込んでから DECLARE すると、未 commit の行を含む内容になる。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 2");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &insert_sql(101, "ja", "op-w2"),
        )
        .expect("insert before declare");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("DECLARE against an already-written table sees the uncommitted row");
    let outcome = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM c")
        .expect("fetch 2");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome");
    };
    assert_eq!(result.rows.len(), 2);
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback 2");
}

/// ベクトル順位付けの検索 `SELECT`（`ORDER BY <=>`）は `DECLARE` の内側として
/// 受理しない（`42601`）。
#[test]
fn declare_rejects_vector_ranking_select() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");

    let declare_sql = format!(
        "DECLARE c CURSOR FOR SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"
    );
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect_err("vector ranking SELECT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// RLS: 他テナント・`Private` 行を混在させても、カーソル経由の全ページに
/// 不許可行が 0 件であること。別セッション（別テナント）のカーソル名は
/// `34000`（不在と同一の応答）になる。
#[test]
fn cursor_respects_rls_and_does_not_leak_other_tenant_cursor_names() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");

    let mut seed_session = SessionState::default();
    engine
        .execute_sql_in_session(&alice, &mut seed_session, &insert_sql(1, "ja", "op-a1"))
        .expect("alice row 1");
    engine
        .execute_sql_in_session(&bob, &mut seed_session, &insert_sql(2, "ja", "op-b1"))
        .expect("bob row 1");

    let mut alice_session = SessionState::default();
    let mut alice_txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&alice, &mut alice_session, &mut alice_txn, "BEGIN")
        .expect("alice begin");
    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 100");
    engine
        .execute_sql_in_txn(&alice, &mut alice_session, &mut alice_txn, &declare_sql)
        .expect("alice declare");
    let outcome = engine
        .execute_sql_in_txn(
            &alice,
            &mut alice_session,
            &mut alice_txn,
            "FETCH 100 FROM c",
        )
        .expect("alice fetch");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome");
    };
    // alice のカーソルには alice 自身の行（id=1）のみが含まれ、bob の行
    // （id=2）は含まれない。
    assert_eq!(row_ids(&result), vec![1]);
    engine
        .execute_sql_in_txn(&alice, &mut alice_session, &mut alice_txn, "COMMIT")
        .expect("alice commit");

    // bob のセッション（別トランザクション）から alice のカーソル名 "c" を
    // 参照しても、単に存在しないカーソルと同じ応答になる。
    let mut bob_session = SessionState::default();
    let mut bob_txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn, "BEGIN")
        .expect("bob begin");
    let err_other = engine
        .execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn, "FETCH 1 FROM c")
        .expect_err("bob cannot see alice's cursor");
    assert_eq!(err_other.wire_code(), "34000");
    bob_txn.fail();
    let mut bob_txn2 = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn2, "BEGIN")
        .expect("bob begin 2");
    let err_missing = engine
        .execute_sql_in_txn(
            &bob,
            &mut bob_session,
            &mut bob_txn2,
            "FETCH 1 FROM missing_cursor",
        )
        .expect_err("bob cursor genuinely missing");
    assert_eq!(err_missing.wire_code(), "34000");
    assert_eq!(err_other.to_string(), err_missing.to_string());
}

/// 同時カーソル数の上限（`MAX_CURSORS_PER_SESSION`＝16）を超えると `54000`。
#[test]
fn declare_beyond_cursor_count_limit_is_rejected() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");

    for i in 0..engine::sql::cursor::MAX_CURSORS_PER_SESSION {
        let declare_sql = format!("DECLARE c{i} CURSOR FOR SELECT id FROM {TABLE} LIMIT 1");
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
            .unwrap_or_else(|e| panic!("declare {i} should succeed: {e}"));
    }

    let overflow_sql = format!("DECLARE overflow CURSOR FOR SELECT id FROM {TABLE} LIMIT 1");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &overflow_sql)
        .expect_err("17th cursor must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

/// トランザクションの持続時間上限（`TransactionLimits::max_duration`）を
/// 超えると、カーソルごと `Failed` へ遷移して使えなくなる（保持期間の上限）。
#[test]
fn cursor_becomes_unusable_after_transaction_duration_limit() {
    let limits = TransactionLimits {
        max_duration: std::time::Duration::from_millis(50),
        max_statements: 1_000,
    };
    let (engine, path) = new_core_with_limits(limits);
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 1);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let declare_sql = format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare_sql)
        .expect("declare");

    std::thread::sleep(std::time::Duration::from_millis(120));

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("expired transaction must reject FETCH");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

// --- ビュー越しのカーソル（PR #1049 レビュー指摘 codex P1 の回帰防止） -------
//
// `DECLARE` の構文解析段はカタログを参照しない構造検証のみ（トランザクション外
// `25P01` 契約のため）で、`Active` 内の実行時に内側 SELECT を実カタログで
// 再検証する。この再検証が通常の広域取得 `SELECT` と同じ経路でビューを展開し、
// 参照セッション自身の `PolicyContext` で RLS が暗黙適用されることを固定する
// （TABLE-18・SQL-23・RLS-10 (b) のポインタ。`table18_view.rs` と同じ fixture
// 構成）。

/// `id` 1〜4 は tenant-a（4 のみ Private）、5・6 は tenant-b（6 のみ Private）、
/// 7 は tenant-c（Public）。3 のみ `lang = 'en'`。
fn seed_view_fixture(storage: &Storage) {
    let rows: [(&str, u64, &str, Visibility); 7] = [
        ("tenant-a", 1, "ja", Visibility::Public),
        ("tenant-a", 2, "ja", Visibility::Public),
        ("tenant-a", 3, "en", Visibility::Public),
        ("tenant-a", 4, "ja", Visibility::Private),
        ("tenant-b", 5, "ja", Visibility::Public),
        ("tenant-b", 6, "ja", Visibility::Private),
        ("tenant-c", 7, "ja", Visibility::Public),
    ];
    for (tenant, id, lang, visibility) in rows {
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx(tenant),
            id,
            visibility,
            &[
                engine::row_codec::Value::Vector(vec![id as f32, 0.0]),
                engine::row_codec::Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("view-seed-{id}"))
                .expect("valid operation id"),
        )
        .expect("seed typed row");
    }
}

/// 基底テーブル作成・fixture 投入・ビュー作成まで済ませた `EngineCore`。
fn new_view_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-cursor-view");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    seed_view_fixture(&storage);
    let engine = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    create_ja_view(&engine);
    (engine, path)
}

fn create_ja_view(engine: &EngineCore) {
    let mut session = SessionState::default();
    session.allow_ddl();
    engine
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            &format!("CREATE VIEW ja_docs AS SELECT id, lang FROM {TABLE} WHERE lang = 'ja'"),
        )
        .expect("create view");
}

/// `BEGIN` → `DECLARE ... FOR SELECT ... FROM <view>` → `FETCH` の全ページの
/// 和集合が、同じセッション `ctx` で基底テーブルへ直接問い合わせた結果と一致し、
/// 他テナントの Private 行（作成者 tenant-a の 4・tenant-b の 6）が混入しない。
#[test]
fn declare_over_view_resolves_view_and_applies_session_rls() {
    let (engine, path) = new_view_core();
    let _cleanup = CleanupGuard(path);

    for tenant in ["tenant-a", "tenant-b", "tenant-c"] {
        let caller = ctx(tenant);
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        assert_eq!(
            engine
                .execute_sql_in_txn(
                    &caller,
                    &mut session,
                    &mut txn,
                    "DECLARE v CURSOR FOR SELECT id, lang FROM ja_docs LIMIT 100",
                )
                .expect("DECLARE over a view must resolve the view"),
            SqlOutcome::DeclareCursor
        );
        let mut fetched: Vec<u64> = Vec::new();
        loop {
            let SqlOutcome::Fetch(result) = engine
                .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 2 FROM v")
                .expect("fetch")
            else {
                panic!("expected Fetch outcome");
            };
            if result.rows.is_empty() {
                break;
            }
            for row in &result.rows {
                assert_eq!(
                    row.cells.last(),
                    Some(&Cell::Text("ja".to_string())),
                    "tenant={tenant}: rows through the view must satisfy the view predicate"
                );
            }
            fetched.extend(row_ids(&result));
        }
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
            .expect("commit");
        fetched.sort_unstable();

        let SqlOutcome::Query(direct) = engine
            .execute_sql_in_session(
                &caller,
                &mut SessionState::default(),
                &format!("SELECT id FROM {TABLE} WHERE lang = 'ja' LIMIT 100"),
            )
            .expect("direct scan")
        else {
            panic!("expected Query outcome");
        };
        let mut expected = row_ids(&direct);
        expected.sort_unstable();
        assert_eq!(
            fetched, expected,
            "tenant={tenant}: cursor over the view must match the direct query under the same ctx"
        );

        match tenant {
            "tenant-a" => assert_eq!(fetched, vec![1, 2, 4, 5, 7]),
            "tenant-b" => assert_eq!(fetched, vec![1, 2, 5, 6, 7]),
            _ => assert_eq!(fetched, vec![1, 2, 5, 7]),
        }
    }
}

/// ビューを指す `DECLARE` もトランザクション外では実カタログを照会せず
/// `25P01` のまま（構造検証段でビュー展開を行わない契約の維持）。
#[test]
fn declare_over_view_outside_transaction_is_still_25p01() {
    let (engine, path) = new_view_core();
    let _cleanup = CleanupGuard(path);

    let err = engine
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut SessionState::default(),
            "DECLARE v CURSOR FOR SELECT id FROM ja_docs LIMIT 10",
        )
        .expect_err("DECLARE outside a transaction must be rejected");
    assert_eq!(err.wire_code(), "25P01");
}

/// 同一トランザクション内で基底テーブルへ書き込んだ後は、ビュー越しの `DECLARE` も
/// 展開後の基底テーブルの未 commit の変更を反映する（Issue #1179）。
#[test]
fn declare_over_view_after_writing_its_base_table_sees_the_uncommitted_row() {
    let (engine, path) = new_view_core();
    let _cleanup = CleanupGuard(path);

    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let before = {
        engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                "DECLARE v0 CURSOR FOR SELECT id FROM ja_docs LIMIT 10",
            )
            .expect("declare before the write");
        let SqlOutcome::Fetch(result) = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM v0")
            .expect("fetch before")
        else {
            panic!("expected Fetch outcome");
        };
        result.rows.len()
    };
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &insert_sql(100, "ja", "view-w1"),
        )
        .expect("insert into base table");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "DECLARE v CURSOR FOR SELECT id FROM ja_docs LIMIT 10",
        )
        .expect("DECLARE over a view whose base table was written sees the uncommitted row");
    let SqlOutcome::Fetch(result) = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM v")
        .expect("fetch after")
    else {
        panic!("expected Fetch outcome");
    };
    assert_eq!(result.rows.len(), before + 1);
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

// --- 既存カーソル保持量を差し引いた生成予算（PR #1049 レビュー指摘 codex P1） ----

/// 1 行あたり約 3 MiB の `TEXT` を持つ 3 行（計約 9 MiB）を投入した `EngineCore`。
fn new_large_text_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-cursor-remaining-budget");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let big = "x".repeat(3 * 1024 * 1024);
    for id in 1..=3u64 {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx("tenant-a"),
            id,
            Visibility::Public,
            &[
                engine::row_codec::Value::Vector(vec![1.0, 0.0]),
                engine::row_codec::Value::Text(big.clone()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("big-{id}"))
                .expect("valid operation id"),
        )
        .expect("seed large row");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// 2 本目の `DECLARE` は、既存カーソルの保持量（約 9 MiB）を差し引いた残容量を
/// 内側 SELECT の生成予算として受け取り、生成途中で打ち切られる（`declare` の
/// 合計上限判定まで約 18 MiB を確保しない）。1 本目を `CLOSE` すると残容量が戻り
/// 同じ `DECLARE` が成功する。
#[test]
fn second_declare_uses_remaining_session_budget_during_generation() {
    let (engine, path) = new_large_text_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");

    let declare =
        |name: &str| format!("DECLARE {name} CURSOR FOR SELECT id, lang FROM {TABLE} LIMIT 10");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare("c1"))
        .expect("first ~9 MiB cursor fits in the 16 MiB session budget");

    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare("c2"))
        .expect_err("second ~9 MiB cursor must exceed the remaining budget");
    assert_eq!(err.wire_code(), "54000");
    assert!(
        err.to_string().contains("scan result exceeds capacity"),
        "must be cut during generation by the remaining budget, not after it: {err}"
    );
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    // `CLOSE` で保持量が解放されれば、同じ内容の 2 本目は成功する。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin 2");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare("c1"))
        .expect("declare c1");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "CLOSE c1")
        .expect("close c1");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &declare("c2"))
        .expect("remaining budget is restored after CLOSE");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback 2");
}

// --- Failed 中の FETCH の Describe（PR #1049 レビュー指摘 Cursor Bugbot Low） -----

/// `Failed` なトランザクションでの `FETCH` の Describe（拡張クエリの Bind／
/// Describe が使う `describe_parsed_in_txn`）は、Execute と同じく `25P02` を返す
/// （カーソル破棄済みでも `34000` にしない）。
#[test]
fn describe_fetch_in_failed_transaction_is_25p02() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 2);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10"),
        )
        .expect("declare");

    let parsed = engine.parse_sql("FETCH 1 FROM c").expect("parse FETCH");
    assert!(
        engine
            .describe_parsed_in_txn(&session, &txn, &parsed)
            .expect("describe in Active")
            .is_some(),
        "open cursor must describe its columns while Active"
    );

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM missing")
        .expect_err("unknown cursor fails the transaction");
    assert_eq!(txn.status(), TransactionStatus::Failed);

    let err = engine
        .describe_parsed_in_txn(&session, &txn, &parsed)
        .expect_err("describe in Failed must be rejected");
    assert_eq!(err.wire_code(), "25P02");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("execute in Failed must be rejected");
    assert_eq!(err.wire_code(), "25P02", "describe and execute must agree");
}
