//! `USING MODE` 句・`SET search_mode` セッション変数の優先順位解決（SQL-12）と
//! `precision` の確信度ゲート（SEARCH-9）が、wire v3 の簡易クエリプロトコル経由
//! （生バイトクライアント）で契約どおりの応答（`RowDescription`／`DataRow`／
//! `CommandComplete`／`ErrorResponse` の SQLSTATE）として観測できることを検証する
//! 結合テスト（TASK-165。ポインタ: `docs/spec/05-tasks.md` TASK-165、
//! `docs/spec/04-behavior/sql-surface.md` SQL-12、
//! `docs/spec/04-behavior/search.md` SEARCH-9）。
//!
//! ゲート閾値・優先順位規則そのものは `crates/engine/tests/sql_search_mode.rs`・
//! `sql_precision_mode.rs`（in-process）が既に確定オラクルとして検証済みのため、
//! 本ファイルは同じ規則を **wire フレーミング** 越しに再確認することに徹する
//! （閾値の再計算はしない。`crates/engine/src/precision.rs` の
//! `DEFAULT_DENSE_MIN_TOP1`＝0.80・`DEFAULT_DENSE_MIN_MARGIN`＝0.05・
//! `DEFAULT_MAX_RESULTS`＝1 を前提に、cosine 類似度が手計算で明確に判定できる
//! 独立コーパスを使う）。無改造の実クライアント（psql／psycopg／pg）を使う
//! 3 クライアント統合検証は `tests/three_client_e2e.rs`（`#[ignore]`）が担う。

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

/// `docs` テーブル（`embedding VECTOR(2)`）を持つ `EngineCore` を新設し、
/// 3 テナントそれぞれの `Public` 行を 1 件ずつ投入する。
///
/// クエリベクトル `[1,0]` に対する cosine 類似度は id1=1.0／id2=0.0／id3=-1.0 で
/// あり、`precision` の既定閾値（top1≥0.80・margin≥0.05）を Top-1 のみが明確に
/// 満たす（margin=1.0）。`[0.70710678,0.70710678]` を使うと id1=id2≈0.7071 で
/// top1 が閾値 0.80 未満となり、確信度ゲートが空集合を返す対照ケースになる
/// （どちらも手計算で検証可能な独立オラクル。`crates/engine/src/precision.rs` の
/// ゲート実装を読み直して再計算しない）。
fn new_core_three_tenant_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-search-mode-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");

    let corpus: [(&str, u64, [f32; 2]); 3] = [
        ("tenant-a", 1, [1.0, 0.0]),
        ("tenant-b", 2, [0.0, 1.0]),
        ("tenant-c", 3, [-1.0, 0.0]),
    ];
    for (tenant, id, emb) in corpus {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

const SELECT_RECALL_ORDER: &str = "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
const SELECT_PRECISION_CLAUSE: &str =
    "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'precision'";
const SELECT_RECALL_CLAUSE: &str =
    "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'recall'";
/// 45 度方向のクエリ（id1・id2 の cosine が共に約 0.7071 で top1 が既定閾値
/// 0.80 未満 → `precision` は空集合、`recall` は 3 行）。
const SELECT_LOW_CONFIDENCE: &str =
    "SELECT * FROM docs ORDER BY embedding <=> '[0.70710678,0.70710678]' LIMIT 3";
const SELECT_LOW_CONFIDENCE_PRECISION: &str =
"SELECT * FROM docs ORDER BY embedding <=> '[0.70710678,0.70710678]' LIMIT 3 USING MODE 'precision'";

fn connect_alice(addr: std::net::SocketAddr) -> std::net::TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn spawn_with_alice(core: Arc<EngineCore>) -> (std::net::TcpStream, common::UserStoreFile) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (connect_alice(addr), users_path)
}

/// SQL-12: `USING MODE 'precision'` は明確な Top-1 のみを返し、
/// `USING MODE 'recall'`／句なしは候補全件（3 行）を返す。
#[test]
fn sql12_using_mode_precision_returns_top1_only_and_recall_returns_all() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    // M1: 句なし（既定 recall）。
    send_simple_query(&mut stream, SELECT_RECALL_ORDER);
    let _columns = read_row_description(&mut stream);
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    assert_eq!(ids, vec!["1", "2", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);

    // M2: 句 precision（明確な勝者 → Top-1 のみ）。
    send_simple_query(&mut stream, SELECT_PRECISION_CLAUSE);
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    // M3: 句 recall（既定と同一）。
    send_simple_query(&mut stream, SELECT_RECALL_CLAUSE);
    let _columns = read_row_description(&mut stream);
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    assert_eq!(ids, vec!["1", "2", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// SEARCH-9: 低確信度（top1 < 0.80）の `precision` は `ErrorResponse` ではなく
/// `RowDescription` → `DataRow` 0 件 → `CommandComplete("SELECT 0")` →
/// `ReadyForQuery` という **通常応答** として空集合を返す（fail-closed だが
/// wire レベルではエラーにしない）。同一クエリの `recall` は 3 行返り、空集合が
/// `precision` 固有であることの対照を取る。
#[test]
fn search9_precision_low_confidence_returns_empty_result_set_as_normal_response() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    // M4: precision（低確信 → 空集合、エラーではない）。
    send_simple_query(&mut stream, SELECT_LOW_CONFIDENCE_PRECISION);
    let mut header = [0u8; 1];
    use std::io::Read;
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(
        header[0], b'T',
        "empty precision result must still start with RowDescription, not ErrorResponse"
    );
    // RowDescription の残りを読み切る（型バイトは既読なので common ヘルパーは
    // 使わず、長さプレフィックスに従って本文を破棄する）。
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let tag = read_command_complete(&mut stream);
    assert_eq!(
        tag, "SELECT 0",
        "low-confidence precision must yield 0 rows"
    );
    read_ready_for_query(&mut stream);

    // M4': 同一クエリの recall は対照的に 3 行返る。
    send_simple_query(&mut stream, SELECT_LOW_CONFIDENCE);
    let _columns = read_row_description(&mut stream);
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    assert_eq!(ids, vec!["1", "2", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// SQL-12: `SET search_mode = 'precision'` は同一接続内の後続の句なし `SELECT`
/// に適用される（セッション変数の接続スコープ）。
#[test]
fn sql12_set_search_mode_precision_applies_to_subsequent_select_on_same_connection() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SET search_mode = 'precision'");
    assert_eq!(read_command_complete(&mut stream), "SET");
    read_ready_for_query(&mut stream);

    // M5: 句なし SELECT が SET の precision を継承し、Top-1 のみ返る。
    send_simple_query(&mut stream, SELECT_RECALL_ORDER);
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// SQL-12: クエリ句はセッション変数より優先する（双方向）。
#[test]
fn sql12_query_clause_overrides_session_variable_in_both_directions() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    // M6: SET precision → 句 recall（句が勝ち 3 行）。
    send_simple_query(&mut stream, "SET search_mode = 'precision'");
    assert_eq!(read_command_complete(&mut stream), "SET");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, SELECT_RECALL_CLAUSE);
    let _columns = read_row_description(&mut stream);
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    assert_eq!(ids, vec!["1", "2", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);

    // M7: SET recall → 句 precision（句が勝ち Top-1 のみ）。
    send_simple_query(&mut stream, "SET search_mode = 'recall'");
    assert_eq!(read_command_complete(&mut stream), "SET");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, SELECT_PRECISION_CLAUSE);
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// SQL-12: 未知モード値は `USING MODE`（R1）・`SET`（R2）いずれも `22000` で拒否し
/// 接続は維持される。R2 では失敗した `SET` が直前のセッション値を変えないこと
/// （黙った部分更新にしない。`crates/engine/tests/sql_search_mode.rs` の
/// `failed_set_search_mode_leaves_session_unchanged` と同じ契約）を、直後の
/// 句なし `SELECT` が直前に設定した `recall`（3 行）のまま応答することで
/// wire 越しに確認する。
#[test]
fn sql12_unknown_mode_literal_is_rejected_with_22000_and_connection_survives() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    // R1: クエリ句の未知モード値。
    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'fuzzy'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);

    // 直前に recall を明示しておき、次の失敗した SET が変えないことを確認できる
    // 状態を作る。
    send_simple_query(&mut stream, "SET search_mode = 'recall'");
    assert_eq!(read_command_complete(&mut stream), "SET");
    read_ready_for_query(&mut stream);

    // R2: SET の未知モード値。
    send_simple_query(&mut stream, "SET search_mode = 'fuzzy'");
    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);

    // 失敗した SET はセッションを変えない：句なし SELECT は直前の recall のまま
    // 3 行返る（precision へ黙って切り替わっていない）。
    send_simple_query(&mut stream, SELECT_RECALL_ORDER);
    let _columns = read_row_description(&mut stream);
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    assert_eq!(ids, vec!["1", "2", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// SQL-12: `USING MODE $1`（拡張クエリの束縛パラメータ形式）は MVP では
/// 文字列リテラル規範形のみ受理するため `42601` で構文エラー拒否する。
#[test]
fn sql12_dollar_parameter_form_is_rejected_with_42601() {
    let (core, _guard) = new_core_three_tenant_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE $1",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// SQL-12: `SET search_mode` の接続スコープは接続間で漏えいしない。接続 A で
/// `precision` に切り替えても、別接続 B の句なし `SELECT` は既定 `recall`
/// （3 行）のまま。
#[test]
fn sql12_session_search_mode_does_not_leak_across_connections() {
    let (core, _guard) = new_core_three_tenant_docs();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);

    let mut stream_a = connect_alice(addr);
    send_simple_query(&mut stream_a, "SET search_mode = 'precision'");
    assert_eq!(read_command_complete(&mut stream_a), "SET");
    read_ready_for_query(&mut stream_a);

    let mut stream_b = connect_alice(addr);
    send_simple_query(&mut stream_b, SELECT_RECALL_ORDER);
    let _columns = read_row_description(&mut stream_b);
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(read_data_row(&mut stream_b)[0].clone().expect("id"));
    }
    assert_eq!(
        ids,
        vec!["1", "2", "3"],
        "connection B must still be on default recall despite connection A's SET"
    );
    assert_eq!(read_command_complete(&mut stream_b), "SELECT 3");
    read_ready_for_query(&mut stream_b);

    // 接続 A 自身は precision のままであることも確認する（漏えいがないことの
    // 対照として、A 側の効果自体は保たれている）。
    send_simple_query(&mut stream_a, SELECT_RECALL_ORDER);
    let _columns = read_row_description(&mut stream_a);
    let row = read_data_row(&mut stream_a);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(read_command_complete(&mut stream_a), "SELECT 1");
    read_ready_for_query(&mut stream_a);
}

/// SEARCH-9 × RLS: 他テナントの `Private` 行がクエリと完全一致するベクトルを
/// 持っていても、`precision` の確信度ゲートの候補にすら入らない（wire 認証経路の
/// `PolicyContext` は `Public` のみ許可。ポインタ: RLS-6）。他テナントの存在情報が
/// ゲート結果（Top-1 のみ）に影響しないことを確認する。
#[test]
fn search9_precision_gate_does_not_surface_private_rows_of_other_tenants() {
    // `EngineCore::from_storage` は内部 `Storage` への直接アクセスを公開しない
    // ため、`new_core_three_tenant_docs` は使わず、他テナントの `Private` 行を
    // 追加投入できるようこのテスト専用に `Storage` から作り直す（3 テナント
    // `Public` コーパスは同関数と同じ値を使い、独立オラクルとしての一貫性を保つ）。
    let path = temp_db::unique_db_path("wire-search-mode-rls");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");

    let public_corpus: [(&str, u64, [f32; 2]); 3] = [
        ("tenant-a", 1, [1.0, 0.0]),
        ("tenant-b", 2, [0.0, 1.0]),
        ("tenant-c", 3, [-1.0, 0.0]),
    ];
    for (tenant, id, emb) in public_corpus {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        // TASK-94・RECOVER-3: 台帳の重複拒否が入ったため、行ごとに一意な
        // `operation_id` を使う（固定文言の使い回しは同一テナント内の 2 回目以降が
        // `23505` になる）。
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    // tenant-b の Private 行（クエリと完全一致するベクトル）。
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    // TASK-101（RECOVER-10）: tenant-b は上のループで既に "test-op" を使用している
    // ため（id=2, Public）、同じ operation_id を別内容で再利用すると
    // OperationIdContentMismatch になる。別の operation_id を使う。
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_b,
        12,
        Visibility::Private,
        &[Value::Vector(vec![1.0, 0.0])],
        &engine::recovery::required_op_id::OperationId::parse("test-op-private-b")
            .expect("valid operation_id"),
    )
    .expect("insert private row");

    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, SELECT_PRECISION_CLAUSE);
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(
        row[0].as_deref(),
        Some("1"),
        "other tenant's Private row must not enter the precision gate's candidate set"
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    drop(guard);
}
