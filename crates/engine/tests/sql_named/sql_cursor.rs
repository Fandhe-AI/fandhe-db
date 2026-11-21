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

use crate::temp_db;
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

/// `FETCH` の拡張形（Issue #3173・WIRE-15）。`NEXT`・`IN`・`FORWARD n` と末尾 `;` を
/// 混ぜて払い出しても、同一スナップショットの参照カーソルと同じ並びで重複・欠落なく
/// 単調に進むこと。
#[test]
fn fetch_next_in_and_forward_advance_cursor_monotonically() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    for name in ["c", "ref"] {
        engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("DECLARE {name} CURSOR FOR SELECT id FROM {TABLE} LIMIT 100"),
            )
            .expect("declare");
    }

    let mut fetch = |sql: &str, expected_rows: usize| -> Vec<u64> {
        let outcome = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let SqlOutcome::Fetch(result) = outcome else {
            panic!("expected Fetch outcome for {sql}");
        };
        assert_eq!(result.rows.len(), expected_rows, "{sql}");
        row_ids(&result)
    };
    let mut got = Vec::new();
    got.extend(fetch("FETCH NEXT FROM c", 1));
    got.extend(fetch("FETCH 1 IN c", 1));
    got.extend(fetch("FETCH FORWARD 2 FROM c;", 2));
    got.extend(fetch("fetch next in c;", 1));
    assert!(fetch("FETCH NEXT FROM c", 0).is_empty());
    let reference = fetch("FETCH 10 FROM ref", 5);
    assert_eq!(got, reference);
}

/// 新しい形の `FETCH` でも別テナントは他者のカーソルを参照できず（`34000`・未定義と
/// 同一応答）、指定なし（NO SCROLL）のカーソルへの後ろ向きの取得（`PRIOR`）は `55000` で
/// Tx を `Failed` にする（Issue #3335）。
#[test]
fn fetch_next_in_respect_tenant_boundary_and_reject_backward_on_no_scroll() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    seed_rows(&engine, &alice, 2);

    let mut a_session = SessionState::default();
    let mut a_txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&alice, &mut a_session, &mut a_txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            &alice,
            &mut a_session,
            &mut a_txn,
            &format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10"),
        )
        .expect("declare");
    let err = engine
        .execute_sql_in_txn(&alice, &mut a_session, &mut a_txn, "FETCH PRIOR FROM c")
        .expect_err("PRIOR on a non-scroll cursor");
    assert_eq!(err.wire_code(), "55000");
    assert_eq!(a_txn.status(), TransactionStatus::Failed);

    for sql in [
        "FETCH NEXT FROM c",
        "FETCH 1 IN c",
        "FETCH NEXT FROM missing_cursor",
    ] {
        let mut b_session = SessionState::default();
        let mut b_txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&bob, &mut b_session, &mut b_txn, "BEGIN")
            .expect("bob begin");
        let err = engine
            .execute_sql_in_txn(&bob, &mut b_session, &mut b_txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "34000", "{sql}");
    }
}
// --- DECLARE の内側 SELECT への JOIN（Issue #3175） ------------------------------

const TAGS: &str = "tags";

/// `documents`・`tags` の 2 表を持つ `EngineCore`。両テナントに結合キー（id）が衝突する行を入れる。
/// tenant-a: documents 1..=3・tags 1..=2（id 3 は tags に無い。LEFT で NULL 側になる）、
/// tenant-b: documents 1..=3・tags 1..=3（tenant-a の結果へ混入してはならない）。
fn new_join_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-cursor-join");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create docs");
    storage.create_table(&schema(TAGS)).expect("create tags");
    let engine = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    let put = |engine: &EngineCore, session: &mut SessionState, tenant: &str, t: &str, id: u64| {
        engine
            .execute_sql_in_session(
                &ctx(tenant),
                session,
                &format!(
                    "INSERT INTO {t} (id, embedding, lang) VALUES ({id}, '[1.0, 0.0]', '{tenant}-{t}-{id}') \
                     USING OPERATION_ID 'j-{tenant}-{t}-{id}'"
                ),
            )
            .expect("seed insert");
    };
    for id in 1..=3 {
        put(&engine, &mut session, "tenant-a", TABLE, id);
        put(&engine, &mut session, "tenant-b", TABLE, id);
        put(&engine, &mut session, "tenant-b", TAGS, id);
    }
    for id in 1..=2 {
        put(&engine, &mut session, "tenant-a", TAGS, id);
    }
    (engine, path)
}

fn cell_texts(result: &engine::sql::exec::QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(|c| format!("{c:?}")).collect())
        .collect()
}

/// BEGIN → DECLARE → FETCH で全件読み → CLOSE → COMMIT し、読んだ行を返す。
fn fetch_all_via_cursor(
    engine: &EngineCore,
    caller: &PolicyContext,
    select: &str,
    fetch: &str,
) -> Vec<Vec<String>> {
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            caller,
            &mut session,
            &mut txn,
            &format!("DECLARE c CURSOR FOR {select}"),
        )
        .unwrap_or_else(|e| panic!("declare must be accepted: {select}: {e:?}"));
    let mut out = Vec::new();
    loop {
        let SqlOutcome::Fetch(r) = engine
            .execute_sql_in_txn(caller, &mut session, &mut txn, fetch)
            .expect("fetch")
        else {
            panic!("expected Fetch outcome");
        };
        if r.rows.is_empty() {
            break;
        }
        out.extend(cell_texts(&r));
    }
    engine
        .execute_sql_in_txn(caller, &mut session, &mut txn, "CLOSE c")
        .expect("close");
    engine
        .execute_sql_in_txn(caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    out
}

fn direct_rows(engine: &EngineCore, caller: &PolicyContext, select: &str) -> Vec<Vec<String>> {
    let mut session = SessionState::default();
    let SqlOutcome::Query(r) = engine
        .execute_sql_in_session(caller, &mut session, select)
        .expect("direct")
    else {
        panic!("expected Query outcome");
    };
    cell_texts(&r)
}

/// INNER／LEFT／CROSS／USING／集計形の JOIN カーソルが、直接実行と一致し、
/// 他テナントの行（結合キーが衝突）が混入しない（RLS 越境・外部結合を含む）。
#[test]
fn join_cursor_matches_direct_execution_and_isolates_tenants() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let selects = [
        "SELECT d.id, d.lang, t.lang FROM documents d INNER JOIN tags t ON d.id = t.id LIMIT 100",
        "SELECT d.id, d.lang, t.lang FROM documents d LEFT JOIN tags t ON d.id = t.id LIMIT 100",
        "SELECT d.id, t.id FROM documents d CROSS JOIN tags t LIMIT 100",
        "SELECT id, d.lang, t.lang FROM documents d JOIN tags t USING (id) LIMIT 100",
        "SELECT d.lang, COUNT(*) AS n FROM documents d JOIN tags t ON d.id = t.id GROUP BY d.lang LIMIT 100",
    ];
    for select in selects {
        let direct = direct_rows(&engine, &caller, select);
        assert!(!direct.is_empty(), "fixture must yield rows: {select}");
        for fetch in ["FETCH NEXT IN c", "FETCH FORWARD 2 IN c", "FETCH 10 FROM c"] {
            let got = fetch_all_via_cursor(&engine, &caller, select, fetch);
            assert_eq!(got, direct, "{select} via {fetch}");
            assert!(
                got.iter().flatten().all(|c| !c.contains("tenant-b")),
                "other tenant row leaked: {select}"
            );
        }
    }
}

/// LEFT JOIN の右辺は参照セッション自身の RLS で絞られる（tenant-a の id=3 は右辺が NULL）。
#[test]
fn join_cursor_left_side_null_when_other_tenant_has_match() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let rows = fetch_all_via_cursor(
        &engine,
        &caller,
        "SELECT d.id, t.lang FROM documents d LEFT JOIN tags t ON d.id = t.id LIMIT 100",
        "FETCH 10 FROM c",
    );
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().any(|r| r.iter().any(|c| c.contains("Null"))),
        "tenant-b's tags row must not fill tenant-a's id=3: {rows:?}"
    );
}

fn sorted(mut v: Vec<Vec<String>>) -> Vec<Vec<String>> {
    v.sort();
    v
}

/// Issue #3176: 集合演算（UNION／UNION ALL／INTERSECT／EXCEPT）と CTE 入力の JOIN の
/// カーソルが、直接実行と一致し、他テナントの行（キーが衝突）が混入しない。
/// 順序が保証されない形はソートして比べる。
#[test]
fn set_operation_and_cte_join_cursors_match_direct_execution_and_isolate_tenants() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let selects = [
        "SELECT id FROM documents UNION SELECT id FROM tags",
        "SELECT id FROM documents UNION ALL SELECT id FROM tags",
        "SELECT id FROM documents INTERSECT SELECT id FROM tags",
        "SELECT id FROM documents EXCEPT SELECT id FROM tags",
        "SELECT lang FROM documents UNION ALL SELECT lang FROM tags",
        "WITH w AS (SELECT id, lang FROM documents LIMIT 10) SELECT w.id, w.lang, t.lang FROM w JOIN tags t ON w.id = t.id LIMIT 100",
        "WITH w AS (SELECT id FROM documents LIMIT 10) SELECT w.id, t.lang FROM w LEFT JOIN tags t ON w.id = t.id LIMIT 100",
    ];
    for select in selects {
        let direct = sorted(direct_rows(&engine, &caller, select));
        assert!(!direct.is_empty(), "fixture must yield rows: {select}");
        for fetch in ["FETCH NEXT IN c", "FETCH FORWARD 2 IN c", "FETCH 10 FROM c"] {
            let got = sorted(fetch_all_via_cursor(&engine, &caller, select, fetch));
            assert_eq!(got, direct, "{select} via {fetch}");
            assert!(
                got.iter().flatten().all(|c| !c.contains("tenant-b")),
                "other tenant row leaked: {select}"
            );
        }
    }
}

/// 末尾 `;` を 1 個付けた形は受理し、`;;` は `42601` で拒否する。
#[test]
fn set_operation_and_cte_join_cursors_accept_one_trailing_semicolon() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    for select in [
        "SELECT id FROM documents UNION ALL SELECT id FROM tags;",
        "WITH w AS (SELECT id FROM documents LIMIT 10) SELECT w.id FROM w JOIN tags t ON w.id = t.id LIMIT 100;",
    ] {
        let rows = fetch_all_via_cursor(&engine, &caller, select, "FETCH 10 FROM c");
        assert!(!rows.is_empty(), "{select}");
    }
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "DECLARE c CURSOR FOR SELECT id FROM documents UNION ALL SELECT id FROM tags;;",
        )
        .expect_err("double semicolon");
    assert_eq!(err.wire_code(), "42601");
}

/// JOIN を枝に持つ集合演算は通常の SELECT でも未対応。cursor と直接実行で同じ wire_code（`42601`）になる。
#[test]
fn join_cursor_unsupported_forms_match_direct_execution_rejection() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    for select in [
        "SELECT d.id FROM documents d JOIN tags t ON d.id = t.id UNION SELECT id FROM tags LIMIT 5",
        "WITH w AS (SELECT id FROM documents LIMIT 5) SELECT id FROM w UNION SELECT id FROM tags",
    ] {
        let mut direct_session = SessionState::default();
        let direct_err = engine
            .execute_sql_in_session(&caller, &mut direct_session, select)
            .expect_err("direct execution rejects the form");
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let err = engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("DECLARE c CURSOR FOR {select}"),
            )
            .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601", "{select}");
        assert_eq!(err.wire_code(), direct_err.wire_code(), "{select}");
    }
}

fn count_rows(engine: &EngineCore, caller: &PolicyContext, table: &str) -> usize {
    direct_rows(engine, caller, &format!("SELECT id FROM {table} LIMIT 100")).len()
}

/// データ変更 CTE（書き込みを伴う `WITH`）はカーソルに載せず `0A000` で拒否し、行は変わらない。
// intentional: docs/design/sql-cursor.md
// 根拠は同書 §5（データ変更 CTE は PG17 と同じくカーソル不可。書き込みへ到達させない）。
#[test]
fn data_modifying_cte_cursor_is_rejected_with_0a000_and_changes_no_rows() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let other = ctx("tenant-b");
    let before = (
        count_rows(&engine, &caller, TAGS),
        count_rows(&engine, &other, TAGS),
    );
    for select in [
        "WITH d AS (DELETE FROM tags RETURNING id) SELECT id FROM d",
        "WITH d AS (DELETE FROM tags RETURNING id) SELECT d.id FROM d JOIN documents x ON d.id = x.id LIMIT 5",
    ] {
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let err = engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("DECLARE c CURSOR FOR {select}"),
            )
            .expect_err("data-modifying CTE must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{select}");
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    let after = (
        count_rows(&engine, &caller, TAGS),
        count_rows(&engine, &other, TAGS),
    );
    assert_eq!(before, after, "no rows may change");
}

/// 集合演算・CTE 入力の JOIN の結果が残容量を超えると生成中に `54000` で打ち切られ、
/// カーソルは登録されない（LIMIT 省略形も同じ）。
#[test]
fn set_operation_and_cte_join_cursors_respect_remaining_byte_budget() {
    let (engine, path) = new_large_text_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    for select in [
        "SELECT lang FROM documents UNION ALL SELECT lang FROM documents",
        "WITH w AS (SELECT id, lang FROM documents LIMIT 10) SELECT a.lang, b.lang FROM w a CROSS JOIN w b LIMIT 100",
    ] {
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let err = engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("DECLARE c CURSOR FOR {select}"),
            )
            .expect_err("result exceeds the cursor budget");
        assert_eq!(err.wire_code(), "54000", "{select}");
        let err = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
            .expect_err("cursor must not be registered");
        assert_ne!(err.wire_code(), "54000", "{select}");
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

/// 集合演算カーソルは自分の書き込みを反映し、writer 取得前に宣言したカーソルは BEGIN 時点を保つ。
#[test]
fn set_operation_cursor_sees_own_writes_and_snapshot_before_writer() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let select = "SELECT id FROM documents UNION ALL SELECT id FROM tags";
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
            &format!("DECLARE before CURSOR FOR {select}"),
        )
        .expect("declare before");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "INSERT INTO tags (id, embedding, lang) VALUES (3, '[1.0, 0.0]', 'new') USING OPERATION_ID 'j-own-s3'",
        )
        .expect("own write");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DECLARE after CURSOR FOR {select}"),
        )
        .expect("declare after");
    let mut counts = Vec::new();
    for name in ["before", "after"] {
        let SqlOutcome::Fetch(r) = engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("FETCH 20 FROM {name}"),
            )
            .expect("fetch")
        else {
            panic!("expected Fetch outcome");
        };
        counts.push(r.rows.len());
    }
    assert_eq!(counts, vec![5, 6]);
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// Tx 外の JOIN DECLARE は `25P01`（存在しない表を含んでも同じ）。
#[test]
fn join_declare_outside_transaction_is_25p01() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    for tbl in ["tags", "no_such_table"] {
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        let err = engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!(
                    "DECLARE c CURSOR FOR SELECT d.id FROM documents d JOIN {tbl} t ON d.id = t.id LIMIT 5"
                ),
            )
            .expect_err("outside tx");
        assert_eq!(err.wire_code(), "25P01", "{tbl}");
    }
}

/// 同一 Tx 内の自分の書き込みが JOIN カーソルへ反映される。writer 取得前に宣言した
/// カーソルは BEGIN 時点を保つ。
#[test]
fn join_cursor_sees_own_writes_and_snapshot_before_writer() {
    let (engine, path) = new_join_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let select = "SELECT d.id FROM documents d JOIN tags t ON d.id = t.id LIMIT 100";
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
            &format!("DECLARE before CURSOR FOR {select}"),
        )
        .expect("declare before");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "INSERT INTO tags (id, embedding, lang) VALUES (3, '[1.0, 0.0]', 'new') USING OPERATION_ID 'j-own-3'",
        )
        .expect("own write");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DECLARE after CURSOR FOR {select}"),
        )
        .expect("declare after");
    let mut counts = Vec::new();
    for name in ["before", "after"] {
        let SqlOutcome::Fetch(r) = engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("FETCH 10 FROM {name}"),
            )
            .expect("fetch")
        else {
            panic!("expected Fetch outcome");
        };
        counts.push(r.rows.len());
    }
    assert_eq!(counts, vec![2, 3]);
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// JOIN の結果が残容量を超えると生成中に `54000` で打ち切られ、カーソルは登録されない。
#[test]
fn join_cursor_respects_remaining_byte_budget() {
    let (engine, path) = new_large_text_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "DECLARE c CURSOR FOR SELECT a.lang, b.lang FROM documents a CROSS JOIN documents b LIMIT 100",
        )
        .expect_err("join result exceeds the cursor budget");
    assert_eq!(err.wire_code(), "54000");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// `FETCH ALL`／`FETCH FORWARD ALL`（Issue #3174・WIRE-15）は残り全行を返して末尾へ
/// 進み、続く `FETCH ALL`・`FETCH NEXT` は 0 行。連結は参照カーソルと一致する。
#[test]
fn fetch_all_returns_rest_and_then_zero_rows() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    for name in ["c", "ref"] {
        engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("DECLARE {name} CURSOR FOR SELECT id FROM {TABLE} LIMIT 100"),
            )
            .expect("declare");
    }

    let mut fetch = |sql: &str, expected_rows: usize| -> Vec<u64> {
        let outcome = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let SqlOutcome::Fetch(result) = outcome else {
            panic!("expected Fetch outcome for {sql}");
        };
        assert_eq!(result.rows.len(), expected_rows, "{sql}");
        row_ids(&result)
    };
    let mut got = fetch("FETCH 2 FROM c", 2);
    got.extend(fetch("FETCH ALL FROM c;", 3));
    assert!(fetch("FETCH FORWARD ALL IN c", 0).is_empty());
    assert!(fetch("FETCH NEXT FROM c", 0).is_empty());
    let reference = fetch("FETCH 10 FROM ref", 5);
    assert_eq!(got, reference);
}

/// `FETCH ALL` でも別テナントは他者のカーソルを参照できず（`34000`・未定義と同一応答）、
/// 指定なし（NO SCROLL）のカーソルへの `BACKWARD ALL` は `55000` で Tx を `Failed` にする
/// （Issue #3335）。
#[test]
fn fetch_all_respects_tenant_boundary() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    seed_rows(&engine, &alice, 2);

    let mut a_session = SessionState::default();
    let mut a_txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&alice, &mut a_session, &mut a_txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            &alice,
            &mut a_session,
            &mut a_txn,
            &format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE} LIMIT 10"),
        )
        .expect("declare");
    let err = engine
        .execute_sql_in_txn(
            &alice,
            &mut a_session,
            &mut a_txn,
            "FETCH BACKWARD ALL FROM c",
        )
        .expect_err("BACKWARD ALL on a non-scroll cursor");
    assert_eq!(err.wire_code(), "55000");
    assert_eq!(a_txn.status(), TransactionStatus::Failed);

    for sql in [
        "FETCH ALL FROM c",
        "FETCH FORWARD ALL FROM c",
        "FETCH ALL FROM missing_cursor",
    ] {
        let mut b_session = SessionState::default();
        let mut b_txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&bob, &mut b_session, &mut b_txn, "BEGIN")
            .expect("bob begin");
        let err = engine
            .execute_sql_in_txn(&bob, &mut b_session, &mut b_txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "34000", "{sql}");
    }
}

// --- DECLARE の SCROLL／NO SCROLL（Issue #3334・WIRE-27） --------------------------

/// `SCROLL`／`NO SCROLL`／指定なし／`INSENSITIVE`／`ASENSITIVE`／`WITHOUT HOLD` のどのカーソルも、
/// 末尾 `;` 付きで宣言でき、前方向の FETCH は同じ行を同じページ分割で返す（末尾の後は 0 行）。
#[test]
fn scroll_no_scroll_and_unspecified_cursors_fetch_forward_identically() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN;")
        .expect("begin");
    let heads = [
        "DECLARE c0 CURSOR",
        "DECLARE c1 SCROLL CURSOR",
        "DECLARE c2 NO SCROLL CURSOR",
        "DECLARE c3 INSENSITIVE CURSOR",
        "DECLARE c4 ASENSITIVE SCROLL CURSOR",
        "DECLARE c5 CURSOR WITHOUT HOLD",
    ];
    for head in heads {
        engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                &format!("{head} FOR SELECT id FROM {TABLE} LIMIT 100;"),
            )
            .unwrap_or_else(|e| panic!("{head}: {e}"));
    }
    let mut fetch = |sql: &str| -> Vec<u64> {
        let outcome = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let SqlOutcome::Fetch(result) = outcome else {
            panic!("expected Fetch outcome for {sql}");
        };
        row_ids(&result)
    };
    let mut reference: Option<Vec<Vec<u64>>> = None;
    for i in 0..heads.len() {
        let pages = vec![
            fetch(&format!("FETCH 2 FROM c{i};")),
            fetch(&format!("FETCH 2 FROM c{i};")),
            fetch(&format!("FETCH 2 FROM c{i};")),
            fetch(&format!("FETCH 2 FROM c{i};")),
        ];
        assert_eq!(pages[0].len(), 2);
        assert_eq!(pages[2].len(), 1);
        assert!(pages[3].is_empty());
        match &reference {
            None => reference = Some(pages),
            Some(r) => assert_eq!(&pages, r, "c{i}"),
        }
    }
}

/// SCROLL 指定でも別テナントの行は件数・行に現れない（他テナントの行の有無で FETCH の件数が
/// 変わらない。WIRE-27 (4)）。
#[test]
fn scroll_cursor_respects_tenant_boundary() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    seed_rows(&engine, &alice, 3);
    let mut seen = Vec::new();
    for with_bob_rows in [false, true] {
        if with_bob_rows {
            seed_rows(&engine, &bob, 7);
        }
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        engine
            .execute_sql_in_txn(
                &alice,
                &mut session,
                &mut txn,
                &format!("DECLARE c SCROLL CURSOR FOR SELECT id FROM {TABLE} LIMIT 100"),
            )
            .expect("declare");
        let outcome = engine
            .execute_sql_in_txn(&alice, &mut session, &mut txn, "FETCH ALL FROM c")
            .expect("fetch all");
        let SqlOutcome::Fetch(result) = outcome else {
            panic!("expected Fetch outcome");
        };
        seen.push(row_ids(&result));
        engine
            .execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0].len(), 3);
}

/// `WITH HOLD` は `0A000` で拒否され、Tx は `Failed`（`ROLLBACK` 以外は `25P02`）。
// tracked: #3340
#[test]
fn declare_with_hold_is_0a000_and_fails_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DECLARE c SCROLL CURSOR WITH HOLD FOR SELECT id FROM {TABLE} LIMIT 10;"),
        )
        .expect_err("WITH HOLD is unsupported");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 1 FROM c")
        .expect_err("failed txn");
    assert_eq!(err.wire_code(), "25P02");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// `BINARY` カーソルは `0A000`（意図した拒否。理由は設計記録）。
// intentional: docs/design/sql-cursor.md
#[test]
fn declare_binary_is_0a000() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DECLARE c BINARY CURSOR FOR SELECT id FROM {TABLE} LIMIT 10"),
        )
        .expect_err("BINARY is unsupported");
    assert_eq!(err.wire_code(), "0A000");
}

/// `SCROLL` と `NO SCROLL` の併記は `42601`。
#[test]
fn declare_conflicting_scroll_options_is_42601() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DECLARE c SCROLL NO SCROLL CURSOR FOR SELECT id FROM {TABLE} LIMIT 10"),
        )
        .expect_err("conflict");
    assert_eq!(err.wire_code(), "42601");
}

/// Tx の外の `DECLARE ... SCROLL ...` は従来どおり `25P01`。
#[test]
fn declare_scroll_outside_transaction_is_25p01() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let err = engine
        .execute_sql_in_session(
            &caller,
            &mut session,
            &format!("DECLARE c SCROLL CURSOR FOR SELECT id FROM {TABLE} LIMIT 10"),
        )
        .expect_err("outside txn");
    assert_eq!(err.wire_code(), "25P01");
}

/// 拡張クエリの Parse 経路（`parse_sql_prepared`）でも修飾語つきの `DECLARE` が構文として通る。
#[test]
fn parse_sql_prepared_accepts_declare_scroll() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    for sql in [
        "DECLARE c SCROLL CURSOR FOR SELECT id FROM missing_table LIMIT 10",
        "DECLARE c NO SCROLL CURSOR FOR SELECT id FROM missing_table LIMIT 10;",
    ] {
        engine
            .parse_sql_prepared(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

// --- FETCH の向き（Issue #3335・WIRE-27） ---------------------------------------------

type Txn<'a> = engine::sql::transaction::SessionTransaction<'a>;

/// `head` で宣言した 5 行（id 1..=5）のカーソル `c` を持つ Tx を開く。
fn open_cursor_txn<'a>(
    engine: &'a EngineCore,
    caller: &PolicyContext,
    head: &str,
) -> (SessionState, Txn<'a>) {
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(
            caller,
            &mut session,
            &mut txn,
            &format!("{head} FOR SELECT id FROM {TABLE} LIMIT 100"),
        )
        .expect("declare");
    (session, txn)
}

fn fetch_ids<'a>(
    engine: &'a EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut Txn<'a>,
    sql: &str,
) -> Vec<u64> {
    match engine.execute_sql_in_txn(caller, session, txn, sql) {
        Ok(SqlOutcome::Fetch(result)) => row_ids(&result),
        other => panic!("{sql}: {other:?}"),
    }
}

/// SCROLL カーソルの全ての向きの行と位置（手計算した表。PG17 実測との突き合わせは
/// `sql_cursor_pg17_parity.rs`）。
/// 同じ列を末尾 `;` なし・ありの両方で流す。位置は 0（先頭の前）から始まる。
#[test]
fn scroll_cursor_fetch_directions_follow_position_model() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);
    let script: &[(&str, &[u64])] = &[
        ("FETCH FIRST FROM c", &[1]),
        ("FETCH LAST FROM c", &[5]),
        ("FETCH PRIOR FROM c", &[4]),
        ("FETCH ABSOLUTE 2 FROM c", &[2]),
        ("FETCH RELATIVE 2 FROM c", &[4]),
        ("FETCH RELATIVE -3 FROM c", &[1]),
        ("FETCH ABSOLUTE -2 FROM c", &[4]),
        ("FETCH 0 FROM c", &[4]),
        ("FETCH RELATIVE 0 FROM c", &[4]),
        ("FETCH BACKWARD 0 FROM c", &[4]),
        ("FETCH NEXT FROM c", &[5]),
        ("FETCH NEXT FROM c", &[]),
        ("FETCH 0 FROM c", &[]),
        ("FETCH BACKWARD 2 FROM c", &[5, 4]),
        ("FETCH ABSOLUTE 0 FROM c", &[]),
        ("FETCH NEXT FROM c", &[1]),
        ("FETCH FORWARD 2 FROM c", &[2, 3]),
        ("FETCH BACKWARD ALL FROM c", &[2, 1]),
        ("FETCH FORWARD ALL FROM c", &[1, 2, 3, 4, 5]),
        ("FETCH BACKWARD ALL FROM c", &[5, 4, 3, 2, 1]),
    ];
    for suffix in ["", ";"] {
        let (mut session, mut txn) = open_cursor_txn(&engine, &caller, "DECLARE c SCROLL CURSOR");
        for (sql, expect) in script {
            let got = fetch_ids(
                &engine,
                &caller,
                &mut session,
                &mut txn,
                &format!("{sql}{suffix}"),
            );
            assert_eq!(&got, expect, "{sql}{suffix}");
        }
    }
}

/// 端の外・向きの反転・件数なしの語・向きの省略。
#[test]
fn scroll_cursor_edges_inversion_and_optional_count() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);
    let (mut session, mut txn) = open_cursor_txn(&engine, &caller, "DECLARE c SCROLL CURSOR");
    let mut f = |sql: &str| fetch_ids(&engine, &caller, &mut session, &mut txn, sql);
    // 末尾の後（ABSOLUTE N+1）の PRIOR は N 行目。
    assert!(f("FETCH ABSOLUTE 6 FROM c;").is_empty());
    assert_eq!(f("FETCH PRIOR FROM c;"), vec![5]);
    // 先頭の前（ABSOLUTE -(N+1)）の NEXT は 1 行目。
    assert!(f("FETCH ABSOLUTE -6 FROM c;").is_empty());
    assert_eq!(f("FETCH NEXT FROM c;"), vec![1]);
    // 巨大な位置は 0 行で端の外へ置く（走査しない）。
    assert!(f("FETCH ABSOLUTE 4294967295 FROM c").is_empty());
    assert_eq!(f("FETCH BACKWARD FROM c"), vec![5]);
    assert!(f("FETCH RELATIVE -4294967295 FROM c").is_empty());
    assert_eq!(f("FETCH FORWARD FROM c"), vec![1]);
    // 向きの反転。
    f("FETCH ABSOLUTE 4 FROM c");
    assert_eq!(f("FETCH -2 FROM c"), vec![3, 2]);
    assert_eq!(f("FETCH FORWARD -1 FROM c"), vec![1]);
    assert_eq!(f("FETCH BACKWARD -2 FROM c"), vec![2, 3]);
    // 向きの省略（FROM／IN）は NEXT。
    assert_eq!(f("FETCH FROM c;"), vec![4]);
    assert_eq!(f("FETCH IN c"), vec![5]);
    // 末尾の後からの BACKWARD 2・BACKWARD ALL。
    f("FETCH ABSOLUTE 9 FROM c");
    assert_eq!(f("FETCH BACKWARD 2 FROM c"), vec![5, 4]);
    f("FETCH ABSOLUTE 9 FROM c");
    assert_eq!(f("FETCH BACKWARD ALL FROM c"), vec![5, 4, 3, 2, 1]);
}

/// 構文段の拒否（件数の上限超過・u32 超過・余剰な件数）は Tx を `Failed` にする。
#[test]
fn fetch_direction_syntax_rejections_fail_the_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);
    for (sql, code) in [
        ("FETCH BACKWARD 10001 FROM c", "22000"),
        ("FETCH ABSOLUTE 4294967296 FROM c", "42601"),
        ("FETCH PRIOR 1 FROM c", "42601"),
    ] {
        let (mut session, mut txn) = open_cursor_txn(&engine, &caller, "DECLARE c SCROLL CURSOR");
        let err = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), code, "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed, "{sql}");
    }
}

/// NO SCROLL（指定なしを含む）: 前方向の移動は受理し、後ろへ戻る取得は `55000`。
#[test]
fn no_scroll_cursor_accepts_forward_moves_and_rejects_backward_scans() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);
    for head in ["DECLARE c CURSOR", "DECLARE c NO SCROLL CURSOR"] {
        let (mut session, mut txn) = open_cursor_txn(&engine, &caller, head);
        let mut f = |sql: &str| fetch_ids(&engine, &caller, &mut session, &mut txn, sql);
        // 先頭の前: FETCH 0・ABSOLUTE 0 は 0 行、FIRST は 1 行目。
        assert!(f("FETCH 0 FROM c").is_empty());
        assert!(f("FETCH ABSOLUTE 0 FROM c;").is_empty());
        assert_eq!(f("FETCH FIRST FROM c;"), vec![1]);
        // 前方向の RELATIVE・先の位置への ABSOLUTE・BACKWARD の負の件数（前方向）。
        assert_eq!(f("FETCH RELATIVE 2 FROM c"), vec![3]);
        assert_eq!(f("FETCH ABSOLUTE 5 FROM c"), vec![5]);
        assert!(f("FETCH BACKWARD -1 FROM c").is_empty());

        // 5 行目まで進めた位置から、後ろへ戻る形はすべて 55000 で位置を変えない。
        for sql in [
            "FETCH LAST FROM c",
            "FETCH ABSOLUTE 2 FROM c",
            "FETCH ABSOLUTE -1 FROM c",
            "FETCH 0 FROM c",
            "FETCH BACKWARD 0 FROM c",
            "FETCH FORWARD -1 FROM c",
            "FETCH -1 FROM c",
            "FETCH RELATIVE -1 FROM c",
            "FETCH BACKWARD FROM c",
            "FETCH BACKWARD ALL FROM c",
        ] {
            let (mut s2, mut t2) = open_cursor_txn(&engine, &caller, head);
            fetch_ids(&engine, &caller, &mut s2, &mut t2, "FETCH 5 FROM c");
            let err = engine
                .execute_sql_in_txn(&caller, &mut s2, &mut t2, sql)
                .expect_err(sql);
            assert_eq!(err.wire_code(), "55000", "{head}: {sql}");
            assert_eq!(t2.status(), TransactionStatus::Failed);
        }
    }
}

/// 拒否（`55000`）は位置を変えない: SAVEPOINT 内で拒否 → ROLLBACK TO で Tx を戻しても、
/// savepoint 前に宣言したカーソルの位置は保たれ、次の NEXT は直前の続きを返す。
///
/// PG17 17.11 はこの形でカーソル自身の実行が失敗した（`55000`）カーソルを使えなくし、続く
/// `FETCH NEXT` も `55000`（portal cannot be run）を返す。本実装は SQL-69 (5)・WIRE-27 (2) の
/// ポインタ先の契約に従い位置を保つ（意図した差。`docs/design/sql-cursor.md`「PG17 との意図した差」）。
#[test]
fn no_scroll_rejection_leaves_position_after_rollback_to_savepoint() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);
    let (mut session, mut txn) = open_cursor_txn(&engine, &caller, "DECLARE c CURSOR");
    assert_eq!(
        fetch_ids(&engine, &caller, &mut session, &mut txn, "FETCH 2 FROM c"),
        vec![1, 2]
    );
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "SAVEPOINT s")
        .expect("savepoint");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH PRIOR FROM c")
        .expect_err("backward on no scroll");
    assert_eq!(err.wire_code(), "55000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK TO s")
        .expect("rollback to");
    assert_eq!(
        fetch_ids(
            &engine,
            &caller,
            &mut session,
            &mut txn,
            "FETCH NEXT FROM c"
        ),
        vec![3]
    );
}

/// 新しい各形で、不在名・別テナントのカーソル名は `34000`（未定義と同じ応答）。他テナントの行の
/// 有無で `LAST`・`ABSOLUTE -1`・`BACKWARD ALL` の行と件数は変わらない。
#[test]
fn fetch_directions_respect_tenant_boundary() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    seed_rows(&engine, &alice, 3);
    let mut seen = Vec::new();
    for with_bob_rows in [false, true] {
        if with_bob_rows {
            seed_rows(&engine, &bob, 7);
        }
        let (mut session, mut txn) = open_cursor_txn(&engine, &alice, "DECLARE c SCROLL CURSOR");
        let mut f = |sql: &str| fetch_ids(&engine, &alice, &mut session, &mut txn, sql);
        seen.push((
            f("FETCH LAST FROM c"),
            f("FETCH ABSOLUTE -1 FROM c"),
            f("FETCH BACKWARD ALL FROM c"),
        ));
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0].0, vec![3]);
    assert_eq!(seen[0].2, vec![2, 1]);

    // alice のカーソルが存在する状態でも、bob からは不在名と同じ 34000。
    let (_a_session, _a_txn) = open_cursor_txn(&engine, &alice, "DECLARE c SCROLL CURSOR");
    for sql in [
        "FETCH PRIOR FROM c",
        "FETCH FIRST FROM c",
        "FETCH LAST FROM c",
        "FETCH ABSOLUTE -1 FROM c",
        "FETCH RELATIVE 1 FROM c",
        "FETCH BACKWARD ALL FROM c",
        "FETCH 0 FROM c",
        "FETCH LAST FROM missing_cursor",
    ] {
        let mut b_session = SessionState::default();
        let mut b_txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&bob, &mut b_session, &mut b_txn, "BEGIN")
            .expect("bob begin");
        let err = engine
            .execute_sql_in_txn(&bob, &mut b_session, &mut b_txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "34000", "{sql}");
    }
}

/// 拡張クエリの Parse 経路でも新しい向きの `FETCH` が構文として通る。
#[test]
fn parse_sql_prepared_accepts_fetch_directions() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    for sql in [
        "FETCH ABSOLUTE -1 FROM c",
        "FETCH BACKWARD ALL FROM c;",
        "FETCH 0 FROM c",
        "FETCH PRIOR IN c",
    ] {
        engine
            .parse_sql_prepared(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// `FROM`／`IN` の省略は向きではなく文法の任意要素のため今回の範囲外で、前方向へ読み替えず
/// `42601` で拒否する（fail-closed）。
// tracked: #3534
#[test]
fn fetch_without_from_or_in_is_rejected_for_now() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 2);
    for sql in ["FETCH c", "FETCH PRIOR c", "FETCH NEXT c"] {
        let (mut session, mut txn) = open_cursor_txn(&engine, &caller, "DECLARE c SCROLL CURSOR");
        let err = engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed, "{sql}");
    }
}

// --- MOVE（Issue #3336・WIRE-27） --------------------------------------------------

/// `SqlOutcome::Move(n)` の n を返す（行は返さない）。
fn move_count<'a>(
    engine: &'a EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut Txn<'a>,
    sql: &str,
) -> u64 {
    match engine.execute_sql_in_txn(caller, session, txn, sql) {
        Ok(SqlOutcome::Move(n)) => n,
        other => panic!("{sql}: {other:?}"),
    }
}

/// 受入条件の 3 形（`MOVE FORWARD 2 IN`・`MOVE BACKWARD 1 FROM`・`MOVE ABSOLUTE 0 IN`）で、
/// 件数が同じ向きの FETCH の行数と一致し、後続 FETCH が移動先の次から始まる。
/// 末尾 `;` あり・なしの両方を流す。
#[test]
fn move_updates_position_and_following_fetch_starts_after_it() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);
    for suffix in ["", ";"] {
        let (mut session, mut txn) = open_cursor_txn(&engine, &caller, "DECLARE c SCROLL CURSOR");
        let mut m = |sql: &str| move_count(&engine, &caller, &mut session, &mut txn, sql);
        assert_eq!(m(&format!("MOVE FORWARD 2 IN c{suffix}")), 2);
        assert_eq!(m(&format!("MOVE BACKWARD 1 FROM c{suffix}")), 1);
        assert_eq!(m(&format!("MOVE ABSOLUTE 0 IN c{suffix}")), 0);
        assert_eq!(m(&format!("move forward 10 in c{suffix}")), 5);
        assert_eq!(m(&format!("MOVE ABSOLUTE 3 IN c{suffix}")), 1);
        assert_eq!(m(&format!("MOVE ABSOLUTE 10 IN c{suffix}")), 0);
        assert_eq!(m(&format!("MOVE BACKWARD ALL FROM c{suffix}")), 5);
        assert_eq!(m(&format!("MOVE FORWARD 2 IN c{suffix}")), 2);
        let got = fetch_ids(
            &engine,
            &caller,
            &mut session,
            &mut txn,
            "FETCH NEXT FROM c",
        );
        assert_eq!(got, vec![3], "{suffix}");
    }
}

/// Tx 外・不在は `34000`、NO SCROLL の逆方向は `55000`（その後 Tx は Failed で `25P02`）。
#[test]
fn move_rejections_match_fetch_sqlstates() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 5);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "MOVE NEXT FROM c")
        .expect_err("outside a transaction");
    assert_eq!(err.wire_code(), "34000");

    let (mut session, mut txn) = open_cursor_txn(&engine, &caller, "DECLARE c NO SCROLL CURSOR");
    assert_eq!(
        move_count(&engine, &caller, &mut session, &mut txn, "MOVE 2 IN c"),
        2
    );
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "MOVE BACKWARD 1 FROM c")
        .expect_err("backward on NO SCROLL");
    assert_eq!(err.wire_code(), "55000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "MOVE NEXT FROM c")
        .expect_err("failed transaction");
    assert_eq!(err.wire_code(), "25P02");

    let (mut session, mut txn) = open_cursor_txn(&engine, &caller, "DECLARE c CURSOR");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "MOVE NEXT FROM nope")
        .expect_err("missing cursor");
    assert_eq!(err.wire_code(), "34000");
}

/// 別テナントは他者のカーソルへ MOVE できず（不在と同じ `34000`）、件数は自テナントの可視行だけに依存する。
#[test]
fn move_respects_tenant_boundary_and_counts_only_visible_rows() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    seed_rows(&engine, &alice, 3);
    seed_rows(&engine, &bob, 5);
    let (mut a_session, mut a_txn) = open_cursor_txn(&engine, &alice, "DECLARE c SCROLL CURSOR");
    assert_eq!(
        move_count(
            &engine,
            &alice,
            &mut a_session,
            &mut a_txn,
            "MOVE FORWARD ALL IN c"
        ),
        3
    );
    for sql in ["MOVE NEXT FROM c", "MOVE NEXT FROM missing_cursor"] {
        let mut b_session = SessionState::default();
        let mut b_txn = engine.new_session_transaction();
        engine
            .execute_sql_in_txn(&bob, &mut b_session, &mut b_txn, "BEGIN")
            .expect("bob begin");
        let err = engine
            .execute_sql_in_txn(&bob, &mut b_session, &mut b_txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "34000", "{sql}");
    }
}

/// Describe は行なし（`None`）。構文の拒否は `FETCH` と同じ `42601`。
#[test]
fn move_describes_as_no_data_and_rejects_bad_syntax() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    seed_rows(&engine, &caller, 2);
    let (session, txn) = open_cursor_txn(&engine, &caller, "DECLARE c SCROLL CURSOR");
    let parsed = engine.parse_sql("MOVE NEXT FROM c").expect("parse MOVE");
    assert!(engine
        .describe_parsed_in_txn(&session, &txn, &parsed)
        .expect("describe")
        .is_none());
    for sql in [
        "MOVE c",
        "MOVE NEXT c",
        "MOVE PRIOR 1 FROM c",
        "MOVE $1 FROM c",
    ] {
        let err = engine.parse_sql(sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}
