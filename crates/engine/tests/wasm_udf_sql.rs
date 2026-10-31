//! WASM UDF のモジュール登録と SQL からの呼び出しの結合テスト（Issue #1926、
//! 対象ビヘイビア: EXT-9 ポインタ。`docs/spec/04-behavior/extensions.md`）。
//!
//! 実際の wasmtime でコンパイルしたモジュールを `register_wasm_udf_from_module` で登録し、
//! 結果列・`WHERE` から呼べること、定義エラーが SQLSTATE へ決定的に写像されレジストリを
//! 変えないこと、セッション分離を検証する。`wasm-udf` feature 無効では空になる
//! （既定ビルドの拒否は `tests/wasm_udf_feature_off.rs`）。環境依存を避けるため
//! 他テストへ束ねず単独で置く。

#![cfg(feature = "wasm-udf")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::wasm_udf::{SandboxLimits, WasmSqlType, WasmUdfSignature, MAX_MODULE_BYTES};

#[path = "support/wasm_encoder.rs"]
mod wasm_encoder;
use wasm_encoder::{plain_body, Spec, F64, I32};

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_db_path(label: &str) -> PathBuf {
    let seq = UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "fandhe-db-engine-wasm-udf-sql-{label}-{}-{seq}.redb",
        std::process::id()
    ));
    path
}

struct CleanupGuard(PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const CORPUS: [(u64, [f32; 3]); 3] = [
    (1, [3.0, 4.0, 0.0]),
    (2, [0.0, 0.0, 1.0]),
    (3, [1.0, 1.0, 1.0]),
];

/// `docs` テーブル（`embedding VECTOR(3)`）を持つ `EngineCore` を新設し、`CORPUS` を投入する。
fn new_core_with_docs() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("docs");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    for (id, emb) in &CORPUS {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &op_id,
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, guard)
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn float_cell(row: &engine::sql::exec::ResultRow, idx: usize) -> f64 {
    match row.cells.get(idx) {
        Some(Cell::Float(v)) => *v,
        other => panic!("expected Cell::Float at index {idx}, got {other:?}"),
    }
}

fn sig_vec_real_to_real() -> WasmUdfSignature {
    WasmUdfSignature {
        params: vec![WasmSqlType::Vector, WasmSqlType::Real],
        ret: WasmSqlType::Real,
    }
}

/// `f(ptr, len, s) = f32.load(ptr) * s`
/// WAT 相当: `local.get 0; f32.load; local.get 2; f32.mul`
fn first_times_scalar() -> Vec<u8> {
    Spec::new(plain_body(&[0x20, 0, 0x2A, 2, 0, 0x20, 2, 0x94])).build()
}

/// `f(ptr, len, s) = f32.convert_i32_s(len) * s`
fn len_times_scalar() -> Vec<u8> {
    Spec::new(plain_body(&[0x20, 1, 0xB2, 0x20, 2, 0x94])).build()
}

/// `f(...) = unreachable`
fn unreachable_module() -> Vec<u8> {
    Spec::new(plain_body(&[0x00])).build()
}

fn register(
    session: &mut SessionState,
    name: &str,
    bytes: &[u8],
) -> Result<(), engine::sql::allowlist::SqlSurfaceError> {
    session.register_wasm_udf_from_module(
        name,
        &sig_vec_real_to_real(),
        bytes,
        "f",
        SandboxLimits::default(),
    )
}

fn select(core: &EngineCore, session: &mut SessionState, sql: &str) -> QueryResult {
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    expect_query(
        core.execute_sql_in_session(&ctx, session, sql)
            .expect("query should succeed"),
    )
}

fn corpus_first(id: u64) -> f32 {
    CORPUS
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, e)| e[0])
        .expect("known id")
}

// --- 正常系（EXT-9） ---------------------------------------------------------------

#[test]
fn real_module_in_result_column_matches_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    register(&mut session, "wfn", &first_times_scalar()).expect("register");
    register(&mut session, "wlen", &len_times_scalar()).expect("register");

    let result = select(
        &core,
        &mut session,
        "SELECT id, wfn(embedding, 2.0) AS a, wlen(embedding, 2.0) AS b FROM docs \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    assert_eq!(result.rows.len(), 3);
    for row in &result.rows {
        assert_eq!(
            float_cell(row, 1),
            f64::from(corpus_first(row.id) * 2.0),
            "row {}",
            row.id
        );
        // ベクトル長 3 が正しく渡る。
        assert_eq!(float_cell(row, 2), 6.0, "row {}", row.id);
    }
}

#[test]
fn real_module_in_where_matches_independent_oracle() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    register(&mut session, "wfn", &first_times_scalar()).expect("register");

    let result = select(
        &core,
        &mut session,
        "SELECT id FROM docs WHERE wfn(embedding, 1.0) > 0.5 \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    let mut expected: Vec<u64> = CORPUS
        .iter()
        .filter(|(_, e)| e[0] > 0.5)
        .map(|(id, _)| *id)
        .collect();
    expected.sort_unstable();
    // 「常に真」「常に偽」では通らない（3 行中 2 行だけが通る）。
    assert_eq!(expected, vec![1, 3]);
    let mut got: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    got.sort_unstable();
    assert_eq!(got, expected);
}

#[test]
fn real_module_in_both_positions_with_entry_name_differing_from_function_name() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    // entry は "f"、関数名は "wfn"。
    register(&mut session, "wfn", &first_times_scalar()).expect("register");

    let result = select(
        &core,
        &mut session,
        "SELECT id, wfn(embedding, 3.0) AS s FROM docs WHERE wfn(embedding, 3.0) > 1.0 \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 3]);
    for row in &result.rows {
        assert_eq!(float_cell(row, 1), f64::from(corpus_first(row.id) * 3.0));
    }
}

#[test]
fn double_precision_declaration_is_accepted() {
    let mut session = SessionState::default();
    let sig = WasmUdfSignature {
        params: vec![WasmSqlType::Vector, WasmSqlType::DoublePrecision],
        ret: WasmSqlType::DoublePrecision,
    };
    session
        .register_wasm_udf_from_module(
            "wd",
            &sig,
            &first_times_scalar(),
            "f",
            SandboxLimits::default(),
        )
        .expect("register");
}

// --- 定義エラー（レジストリ不変・固定文言） ----------------------------------------

/// 登録が `code` で拒否され、レジストリが空のまま、エラー文言に entry 名を含まず、
/// 続く同名の正常登録が成功すること（部分登録が残らないこと）を確かめる。
fn assert_rejected_then_registrable(sig: &WasmUdfSignature, bytes: &[u8], entry: &str, code: &str) {
    let mut session = SessionState::default();
    let err = session
        .register_wasm_udf_from_module("wfn", sig, bytes, entry, SandboxLimits::default())
        .expect_err("must be rejected");
    assert_eq!(err.wire_code(), code, "{err}");
    assert!(
        !err.to_string().contains("secret_entry"),
        "entry name must not leak: {err}"
    );
    assert!(session.udfs().is_empty(), "registry must be unchanged");
    register(&mut session, "wfn", &first_times_scalar()).expect("register after failure");
}

#[test]
fn module_validation_errors_map_to_42p13() {
    let good = sig_vec_real_to_real();
    let mut with_import = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    with_import.func_imports = 1;
    let mut no_memory = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    no_memory.memory_export = None;
    let mut bad_export_sig = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    bad_export_sig.params = vec![I32, I32, F64];
    let mut big_memory = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    big_memory.memory_min = Some(200);

    let cases: Vec<(Vec<u8>, &str)> = vec![
        (with_import.build(), "f"),
        (no_memory.build(), "f"),
        (first_times_scalar(), "secret_entry"),
        (bad_export_sig.build(), "f"),
        (b"not wasm".to_vec(), "f"),
        (big_memory.build(), "f"),
    ];
    for (bytes, entry) in cases {
        assert_rejected_then_registrable(&good, &bytes, entry, "42P13");
    }
}

#[test]
fn declared_signature_mismatch_is_42p13() {
    let bad = [
        (
            vec![WasmSqlType::Real, WasmSqlType::Real],
            WasmSqlType::Real,
        ),
        (vec![WasmSqlType::Vector], WasmSqlType::Real),
        (
            vec![WasmSqlType::Vector, WasmSqlType::Real, WasmSqlType::Real],
            WasmSqlType::Real,
        ),
        (
            vec![WasmSqlType::Vector, WasmSqlType::Real],
            WasmSqlType::Vector,
        ),
    ];
    for (params, ret) in bad {
        let sig = WasmUdfSignature { params, ret };
        assert_rejected_then_registrable(&sig, &first_times_scalar(), "f", "42P13");
    }
}

#[test]
fn oversized_module_is_54000() {
    let bytes = vec![0u8; MAX_MODULE_BYTES + 1];
    assert_rejected_then_registrable(&sig_vec_real_to_real(), &bytes, "f", "54000");
}

#[test]
fn name_collisions_are_42723_and_keep_existing_registration() {
    let (core, _guard) = new_core_with_docs();
    let mut session = SessionState::default();
    register(&mut session, "wfn", &first_times_scalar()).expect("register");
    let err = register(&mut session, "WFN", &len_times_scalar()).expect_err("duplicate");
    assert_eq!(err.wire_code(), "42723");
    let err = register(&mut session, "vec_sum", &len_times_scalar()).expect_err("reserved");
    assert_eq!(err.wire_code(), "42723");
    // 既存の登録は元のモジュールのまま。
    let result = select(
        &core,
        &mut session,
        "SELECT id, wfn(embedding, 1.0) AS a FROM docs \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    for row in &result.rows {
        assert_eq!(float_cell(row, 1), f64::from(corpus_first(row.id)));
    }
}

// --- 実行時・分離・既存拒否経路 -----------------------------------------------------

#[test]
fn runtime_trap_is_row_error_and_session_survives() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    register(&mut session, "boom", &unreachable_module()).expect("register");
    register(&mut session, "wfn", &first_times_scalar()).expect("register");

    // 実行時上限の `54000` 化は #1927 の範囲。ここでは現状の `22000` 写像だけを固定する。
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, boom(embedding, 1.0) AS a FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect_err("trap must fail the statement");
    assert_eq!(err.wire_code(), "22000");
    let result = select(
        &core,
        &mut session,
        "SELECT id, wfn(embedding, 1.0) AS a FROM docs \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn registration_is_session_local() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut a = SessionState::default();
    let mut b = SessionState::default();
    register(&mut a, "wfn", &first_times_scalar()).expect("register");
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut b,
            "SELECT id, wfn(embedding, 1.0) AS a FROM docs \
             ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
        )
        .expect_err("other session has no such function");
    assert_eq!(err.wire_code(), "42883");
}

#[test]
fn sql_definition_with_language_wasm_is_0a000() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "CREATE FUNCTION wfn(v float8, s float8) RETURNS float8 LANGUAGE wasm AS $$ x $$",
        )
        .expect_err("SQL definition is out of scope");
    assert_eq!(err.wire_code(), "0A000");
    assert!(session.udfs().is_empty());
}

#[test]
fn expression_index_over_wasm_function_is_rejected() {
    let (core, _guard) = new_core_with_docs();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    register(&mut session, "wfn", &first_times_scalar()).expect("register");
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "CREATE INDEX wi ON docs ((wfn(embedding, 1.0)))",
        )
        .expect_err("expression index over a WASM function must be rejected");
    assert_eq!(err.wire_code(), "0A000");
}
