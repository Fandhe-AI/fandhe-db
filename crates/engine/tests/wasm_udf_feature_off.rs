//! `wasm-udf` feature 無効の既定ビルドでの WASM UDF 拒否テスト（Issue #1926、
//! 対象ビヘイビア: EXT-9 ポインタ）。登録 API も `LANGUAGE wasm` も `0A000` で拒否し、
//! レジストリを変えないこと（fail-closed。`docs/design/wasmtime-adoption.md` 第 4 節 1）。
//! feature 有効ビルドでは空になる。`make test-default-build` から単独実行する。

#![cfg(not(feature = "wasm-udf"))]

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;
use engine::wasm_udf::{SandboxLimits, WasmSqlType, WasmUdfSignature};

/// 最小の有効モジュール相当のバイト列（空モジュール。feature 無効では中身を見ずに拒否する）。
const MINIMAL_MODULE: &[u8] = b"\0asm\x01\0\0\0";

#[test]
fn register_from_module_is_0a000_and_registry_unchanged() {
    let mut session = SessionState::default();
    let sig = WasmUdfSignature {
        params: vec![WasmSqlType::Vector, WasmSqlType::Real],
        ret: WasmSqlType::Real,
    };
    let err = session
        .register_wasm_udf_from_module("wfn", &sig, MINIMAL_MODULE, "f", SandboxLimits::default())
        .expect_err("must be rejected without the wasm-udf feature");
    assert_eq!(err.wire_code(), "0A000");
    assert!(session.udfs().is_empty());
}

#[test]
fn create_function_language_wasm_is_0a000() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "fandhe-db-engine-wasm-udf-off-{}.redb",
        std::process::id()
    ));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = Cleanup(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "CREATE FUNCTION wfn(v float8, s float8) RETURNS float8 LANGUAGE wasm AS $$ x $$",
        )
        .expect_err("LANGUAGE wasm must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert!(session.udfs().is_empty());
}
