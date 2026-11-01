//! 簡易クエリプロトコルが `engine::core::EngineCore` の SQL 表層へ実際に到達し、
//! wire v3 の結果セット／エラー応答として整形されることを検証する結合テスト
//! （TASK-73、対象ビヘイビア: WIRE-1。ポインタ: `docs/spec/05-tasks.md` TASK-73・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-1）。
//!
//! 生バイトの wire クライアント（`tests/common`）で、in-process サーバー
//! （`accept_loop_with_engine`）に対し C1 相当のクエリ・INSERT・SET・
//! 空クエリ・不正 UTF-8・エラー後の接続維持・3 テナント RLS 分離を検証する。
//! `INSERT` は wire の簡易クエリプロトコル経由で受理する（TASK-82・SQL-10。
//! `simple_query.rs` のモジュールコメント参照）。書き込む行は常に
//! `Visibility::Private` の固定仕様である一方、wire 認証経由の `PolicyContext`
//! は `Public` ＋ 自テナントの `Private` を許可可視性とする（RLS-11・
//! TASK-195。read-your-writes）ため、書いた本人は同一 wire セッションでも
//! その行を読み戻せる（下記
//! `wire1_insert_is_accepted_and_row_is_visible_over_wire_select_to_own_tenant`
//! が固定する契約）。他テナントの `Private` 行は引き続き不可視のまま。実
//! `psql` 等の外部クライアントを使う 3 クライアント統合検証は
//! `tests/extended_syntax_e2e.rs`（`#[ignore]`）が担い、本ファイルはその契約の
//! 中核（受信バイト列は同一）を常時（`make ci`）回帰保護する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

/// `docs` テーブル（`embedding VECTOR(3)` + `lang TEXT`）を持つ `EngineCore` を
/// 新設し、決定的な小規模コーパスを 1 テナント（`tenant-a`）分だけ投入する。
fn new_core_single_tenant() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire1-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let corpus: Vec<(u64, [f32; 3], &str)> = vec![
        (1, [1.0, 0.0, 0.0], "ja"),
        (2, [0.9, 0.1, 0.0], "en"),
        (3, [0.0, 1.0, 0.0], "ja"),
    ];
    for (id, emb, lang) in &corpus {
        // TASK-101（RECOVER-10）: 台帳は (tenant, table, operation_id) 単位で内容
        // ハッシュを持つため、内容の異なる複数行へ同一 operation_id を使い回すと
        // 2 件目以降が OperationIdContentMismatch で拒否される。行ごとに一意の
        // operation_id を使う。
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &op_id,
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// C1 相当（`ORDER BY <=> LIMIT`）: `RowDescription`・`DataRow`・
/// `CommandComplete("SELECT n")` が返り、id 列（`numeric`）が期待どおりであること。
#[test]
fn wire1_c1_query_returns_row_description_and_data_rows() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 3",
    );

    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "embedding", "lang"]);

    let mut ids = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        assert_eq!(row.len(), 3);
        ids.push(row[0].clone().expect("id is not null"));
    }
    // 最近傍は id=1（クエリと同一ベクトル）。
    assert_eq!(ids[0], "1");

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// `INSERT INTO <table> (...) VALUES (...) USING OPERATION_ID '<id>'`
/// （TASK-82・SQL-10）は wire の簡易クエリプロトコル経由で受理し
/// `CommandComplete("INSERT 0 1")` を返すこと（`EngineCore::
/// execute_sql_in_session` が先頭トークンを見て `execute_insert_sql`
/// （TASK-80）へ委譲する。`crates/engine/src/core.rs` 参照）。
///
/// SQL `INSERT` が書き込む行は常に `Visibility::Private`（`sql::exec::
/// execute_insert` の固定仕様）である一方、wire 認証経由の `PolicyContext`
/// （`auth::verify`）は `Public` ＋ 自テナントの `Private` を許可可視性とする
/// （RLS-11・TASK-195。read-your-writes）ため、書いた本人は**同一 wire
/// セッションの SELECT でもその行を読み戻せる**。本テストはその契約——(1) wire
/// 経由の `INSERT` 成功・(2) 直後の wire `SELECT` で可視・(3) engine API
/// （`Private` 可視 `PolicyContext`）でも同じく永続化済みとして読める——を
/// 1 つの契約として固定する。
#[test]
fn wire1_insert_is_accepted_and_row_is_visible_over_wire_select_to_own_tenant() {
    let (core, _guard) = new_core_single_tenant();
    // `spawn_server_with_engine` は `Arc<EngineCore>` の所有権を消費する
    // （サーバースレッドへ move）。永続化確認（下記）は同一 `Arc` の clone を
    // wire 接続とは独立に engine API 直呼び出しで使う。
    let core_for_verification = Arc::clone(&core);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (99, '[0.0,0.0,1.0]', 'fr') USING OPERATION_ID 'op-wire1-insert'",
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // (2) 同一 wire セッションの SELECT で書き込んだ id=99 が見える
    // （`Public` ＋ 自テナント `Private` 許可可視性の wire `PolicyContext`。
    // RLS-11）。既存 3 行 + id=99 の計 4 行が返ること。
    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[0.0,0.0,1.0]' LIMIT 4",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "embedding", "lang"]);
    let mut seen_ids = Vec::new();
    for _ in 0..4 {
        let row = read_data_row(&mut stream);
        seen_ids.push(row[0].clone().expect("id is not null"));
    }
    assert!(
        seen_ids.contains(&"99".to_string()),
        "wire SELECT must observe the own-tenant Private row written by wire INSERT, got {seen_ids:?}"
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 4");
    read_ready_for_query(&mut stream);

    // (3) engine API 側の `Private` 可視 `PolicyContext` でも、書き込んだ id=99
    // が同じく永続化済みとして読める（wire・engine API 間で可視性の食い違いが
    // 無いことの確認）。
    let private_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core_for_verification
        .execute_sql(
            &private_ctx,
            "SELECT * FROM docs ORDER BY embedding <=> '[0.0,0.0,1.0]' LIMIT 4",
        )
        .expect("engine-side SELECT with Private visibility must succeed");
    let engine_ids: Vec<String> = result.rows.iter().map(|row| row.id.to_string()).collect();
    assert!(
        engine_ids.contains(&"99".to_string()),
        "engine API with Private visibility must observe the persisted row, got {engine_ids:?}"
    );
}

/// `SET search_mode = '<literal>'` は `CommandComplete("SET")` を返す。
#[test]
fn wire1_set_search_mode_returns_set_tag() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "SET search_mode = 'recall'");
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SET");
    read_ready_for_query(&mut stream);
}

/// 空白のみのクエリは engine を呼ばず `EmptyQueryResponse` + `ReadyForQuery` を返す
/// （簡易クエリプロトコルの規定挙動）。
#[test]
fn wire1_empty_query_returns_empty_query_response() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "   ");
    expect_empty_query_response(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 不正 UTF-8 のクエリ本文は `08P01`（protocol_violation）で fail-closed に切断する
/// （`ReadyForQuery` は返らない）。
#[test]
fn wire1_non_utf8_query_is_rejected_and_connection_closes() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // 型バイト 'Q' + 不正 UTF-8 バイト列 + 終端 NUL。
    send_length_prefixed_message(&mut stream, b'Q', &[0xFFu8, 0x00]);
    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// 許可リスト外の構文（`42601`）はエラー応答後も接続を維持し、続くクエリが
/// 成功すること（簡易クエリのエラーは切断しない。拡張クエリプロトコルの
/// WIRE-8 切断契約とは独立）。TASK-99（RECOVER-8）の「回復可能エラー
/// （`Result::Err`）は ERR-1 応答後も処理継続」側の対応テスト（panic 側の
/// fail-fast は `engine::recovery::fail_fast` を参照）。
#[test]
fn wire1_sql_error_keeps_connection_and_next_query_succeeds() {
    let (core, _guard) = new_core_single_tenant();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // `DROP TABLE docs` は Issue #902（SQL-23・TASK-203）以降、許可リスト外の
    // 構文ではなく DDL 実行権限ゲート（`42501`。既定拒否）で拒否され、`GRANT` も Issue #1965（SQL-40）
    // 以降は現在ロールの束縛ゲート（`42501`）に到達するようになったため、「許可リスト外の構文
    // （`42601`）」の代表例としては使えない。恒久的に許可リスト外のまま残る構文（`VACUUM` は SQL 表層に
    // 到達経路自体を持たない）へ差し替える。
    send_simple_query(&mut stream, "VACUUM docs");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
    );
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// Vector 列は `[v1,v2,...]` 形式の text、NULL は長さ -1 として符号化される。
#[test]
fn wire1_vector_and_null_cells_are_text_encoded() {
    let path = temp_db::unique_db_path("wire1-nulls");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 2.0]), Value::Null],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert row");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,2.0]' LIMIT 1",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[1].as_deref(), Some("[1,2]"));
    assert_eq!(row[2], None, "NULL lang cell must decode to None");
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    drop(guard);
}

/// BOOLEAN 列（TABLE-13・TASK-196、Issue #883）が簡易クエリ経由で `t`/`f`/
/// NULL のテキスト表現へ写像されることを固定する（`result_encoder.rs` の
/// `Cell::Bool` 分岐。RowDescription の OID 公告（`bool`・OID 16。WIRE-13・
/// TASK-200・Issue #895）は `result_encoder::column_wire_type_matrix` で
/// 固定済みのためここでは対象外・列自体の値往復のみを検証する）。
#[test]
fn wire1_boolean_column_is_t_f_null_text_encoded() {
    let path = temp_db::unique_db_path("wire1-boolean");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("flag", ColumnType::Boolean, true),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for (id, vec_val, flag) in [
        (1u64, [1.0, 0.0], Value::Bool(true)),
        (2, [0.0, 1.0], Value::Bool(false)),
        (3, [0.5, 0.5], Value::Null),
    ] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("test-op-boolean-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec_val.to_vec()), flag],
            &op_id,
        )
        .expect("insert row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "SELECT id, flag FROM docs WHERE flag LIMIT 10");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(row[1].as_deref(), Some("t"));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, flag FROM docs WHERE flag = false LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("2"));
    assert_eq!(row[1].as_deref(), Some("f"));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // NULL flag 行（id=3）は `WHERE flag`／`WHERE flag = false` のいずれにも
    // 一致しない（COUNT(*) が両条件とも id=3 を含まない 1 件のままである
    // ことで間接的に確認する。上記 2 クエリの単一行アサーションと合わせて
    // 3 行中「flag=true が 1 件・flag=false が 1 件・残り 1 件は非該当」を
    // 固定する）。
    send_simple_query(&mut stream, "SELECT COUNT(*) FROM docs");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("3"));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    drop(guard);
}

/// NUMERIC 列（TABLE-13〔検討中〕・TASK-197、Issue #885）が簡易クエリ経由で
/// 正規テキスト表現（`Decimal::Display`。ゼロ埋め・符号付き・NULL 区別）へ
/// 写像されることと、桁あふれが `22003` の `ErrorResponse` になることを固定
/// する。RowDescription の OID 1700（`numeric`）公告自体は
/// `result_encoder::numeric_scalar_column_wire_type_is_numeric_oid_1700_not_text`
/// で固定済みのためここでは対象外（本テストの `read_row_description` は
/// 列名のみ取得し OID を検証しない）。
#[test]
fn wire1_numeric_column_is_canonical_text_encoded_and_overflow_is_22003() {
    let path = temp_db::unique_db_path("wire1-numeric");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for (id, vec_val, price) in [
        (
            1u64,
            [1.0, 0.0],
            Value::Numeric(engine::numeric::Decimal::from_parts(150, 2).expect("valid scale")),
        ),
        (
            2,
            [0.0, 1.0],
            Value::Numeric(engine::numeric::Decimal::from_parts(-150, 2).expect("valid scale")),
        ),
        (3, [0.5, 0.5], Value::Null),
    ] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("test-op-numeric-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec_val.to_vec()), price],
            &op_id,
        )
        .expect("insert row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "SELECT id, price FROM docs LIMIT 10");
    let _columns = read_row_description(&mut stream);
    let mut by_id = std::collections::BTreeMap::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        by_id.insert(row[0].clone(), row[1].clone());
    }
    assert_eq!(
        by_id.get(&Some("1".to_string())),
        Some(&Some("1.50".to_string()))
    );
    assert_eq!(
        by_id.get(&Some("2".to_string())),
        Some(&Some("-1.50".to_string()))
    );
    assert_eq!(by_id.get(&Some("3".to_string())), Some(&None));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // 桁あふれ（丸め後 1000.00 は NUMERIC(5,2) の上限を超える）は 22003。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, price) VALUES (4, '[0.1,0.2]', 999.995) \
         USING OPERATION_ID 'op-overflow'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22003");
    read_ready_for_query(&mut stream);

    drop(guard);
}

/// `DATE`／`TIMESTAMP` 列（TABLE-13・TASK-197、Issue #884）が簡易クエリ経由で
/// ISO テキスト表現（`engine::datetime::format_date`／`format_timestamp`）で
/// 往復することを固定する（`wire1_boolean_column_is_t_f_null_text_encoded` と
/// 同じ流儀）。
#[test]
fn wire1_datetime_columns_are_iso_text_encoded() {
    let path = temp_db::unique_db_path("wire1-datetime");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "events",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("day", ColumnType::Date, true),
                ColumnDef::new("at", ColumnType::Timestamp, true),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for (id, vec_val, day, at) in [
        (1u64, [1.0, 0.0], Value::Date(0), Value::Timestamp(0)),
        (2, [0.0, 1.0], Value::Null, Value::Null),
    ] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("test-op-datetime-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "events",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec_val.to_vec()), day, at],
            &op_id,
        )
        .expect("insert row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT id, day, at FROM events WHERE id = 1 LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(row[1].as_deref(), Some("1970-01-01"));
    assert_eq!(row[2].as_deref(), Some("1970-01-01 00:00:00"));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, day, at FROM events WHERE id = 2 LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("2"));
    assert_eq!(row[1].as_deref(), None);
    assert_eq!(row[2].as_deref(), None);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    drop(guard);
}

/// 3 テナント（alice/bob/carol）が wire 経由で同一 C1 を実行したとき、
/// 各テナントは自分自身の `Private` 行のみ可視で他テナントの `Private` 行は
/// 見えず（`auth::verify` が導出する `PolicyContext` は `Public` ＋ 自テナント
/// `Private` を許可。RLS-11・TASK-195・ポインタ: RLS-6）、`Public` 行は
/// テナント跨ぎで全員に見えること（`PolicyContext::is_visible` の許可可視性
/// 判定。ポインタ: RLS-7）を確認する。
///
/// 各テナントの `StartupMessage` の `user` パラメータから **サーバー側 `auth::verify`
/// が導出したテナント ID のみ**が `PolicyContext` へ渡ること（クライアント自己申告の
/// `database` パラメータ等はテナント決定に使わない。ポインタ: WIRE-2）を、
/// 3 ユーザーがそれぞれ自分のテナントの `Public` 行を見分けられることで
/// 間接的に確認する（テナント混線があれば行の内訳が一致しなくなる）。
#[test]
fn wire1_three_tenant_visibility_public_shared_own_private_visible() {
    let path = temp_db::unique_db_path("wire1-rls");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");

    // 各テナントに Public 行 1 件・Private 行 1 件を投入する（Public 行の
    // ベクトルはテナントごとに異なる近傍点とし、混線があれば発覚するようにする）。
    let tenants: [(&str, u64, u64, [f32; 2]); 3] = [
        ("tenant-a", 1u64, 11u64, [1.0, 0.0]),
        ("tenant-b", 2u64, 12u64, [0.0, 1.0]),
        ("tenant-c", 3u64, 13u64, [-1.0, 0.0]),
    ];
    for (tenant, public_id, private_id, dir) in tenants {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        // TASK-101（RECOVER-10）: 台帳は (tenant, table, operation_id) 単位で内容
        // ハッシュを持つため、同一テナント内で内容の異なる複数行へ同一 operation_id
        // を使い回すと 2 件目以降が OperationIdContentMismatch で拒否される。
        let public_op_id = format!("test-op-public-{tenant}");
        let private_op_id = format!("test-op-private-{tenant}");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            public_id,
            Visibility::Public,
            &[Value::Vector(dir.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&public_op_id)
                .expect("valid operation_id"),
        )
        .expect("insert public row");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            private_id,
            Visibility::Private,
            &[Value::Vector(dir.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&private_op_id)
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);

    // Public 行 3 件は常に可視集合に含まれる（テナント跨ぎで全員に見える）。
    let public_ids: std::collections::BTreeSet<&str> = ["1", "2", "3"].into();

    for (user, pw, own_private_id) in [
        ("alice", "pw-alice", "11"),
        ("bob", "pw-bob", "12"),
        ("carol", "pw-carol", "13"),
    ] {
        let mut stream = authenticate_to_ready_for_query(addr, user, pw);
        send_simple_query(
            &mut stream,
            "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        );
        let _columns = read_row_description(&mut stream);
        // LIMIT 10 だが可視行はちょうど 4 件（Public 3 件 + 自テナントの
        // `Private` 1 件。RLS-11・TASK-195。他テナントの `Private` 行は候補に
        // すら入らない。`VectorArena::build_filtered` が構築時点で除外する）。
        let mut seen_ids = std::collections::BTreeSet::new();
        for _ in 0..4 {
            let row = read_data_row(&mut stream);
            let id = row[0].clone().expect("id is not null");
            seen_ids.insert(id);
        }
        let mut expected_ids: std::collections::BTreeSet<String> =
            public_ids.iter().map(|s| s.to_string()).collect();
        expected_ids.insert(own_private_id.to_string());
        assert_eq!(
            seen_ids, expected_ids,
            "tenant {user} must see the 3 Public rows and only its own Private row"
        );
        let tag = read_command_complete(&mut stream);
        assert_eq!(tag, "SELECT 4");
        read_ready_for_query(&mut stream);
    }

    drop(guard);
}

/// UUID 列（TABLE-13〔検討中〕・TASK-197、Issue #887）が簡易クエリ経由で
/// 正規テキスト表現（小文字 `8-4-4-4-12`。大文字入力の正規化・NULL 区別を含む）
/// へ写像されることと、厳密文法違反が `22P02` の `ErrorResponse` になることを
/// 固定する（`wire1_numeric_column_is_canonical_text_encoded_and_overflow_is_22003`
/// と同じ流儀）。RowDescription の OID（`uuid`・OID 2950。U10・WIRE-13・
/// TASK-200・Issue #895）公告自体は `result_encoder::column_wire_type_matrix`
/// で固定済みのためここでは対象外（本テストの `read_row_description` は
/// 列名のみ取得し OID を検証しない）。
#[test]
fn wire1_uuid_column_is_canonical_text_encoded_and_malformed_literal_is_22p02() {
    let path = temp_db::unique_db_path("wire1-uuid");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("ext_id", ColumnType::Uuid, true),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for (id, vec_val, ext_id) in [
        (
            1u64,
            [1.0, 0.0],
            Value::Uuid(
                engine::uuid::parse_uuid_text("12345678-9ABC-DEF0-1234-56789ABCDEF0")
                    .expect("valid uuid literal"),
            ),
        ),
        (2, [0.0, 1.0], Value::Null),
    ] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("test-op-uuid-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec_val.to_vec()), ext_id],
            &op_id,
        )
        .expect("insert row");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));

    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "SELECT id, ext_id FROM docs LIMIT 10");
    let _columns = read_row_description(&mut stream);
    let mut by_id = std::collections::BTreeMap::new();
    for _ in 0..2 {
        let row = read_data_row(&mut stream);
        by_id.insert(row[0].clone(), row[1].clone());
    }
    // 大文字入力は小文字の正規テキストとして往復する。
    assert_eq!(
        by_id.get(&Some("1".to_string())),
        Some(&Some("12345678-9abc-def0-1234-56789abcdef0".to_string()))
    );
    assert_eq!(by_id.get(&Some("2".to_string())), Some(&None));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // 厳密文法違反（ハイフンなし）は 22P02。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, ext_id) VALUES (3, '[0.1,0.2]', \
         '123456789abcdef0123456789abcdef01234') USING OPERATION_ID 'op-malformed'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22P02");
    read_ready_for_query(&mut stream);

    drop(guard);
}
