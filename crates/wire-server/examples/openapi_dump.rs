//! `OP_SCHEMAS` から生成した OpenAPI 3.1 文書を標準出力へ書き出す手元検証用ツール
//! （Issue #2104。NOSQL-20 ポインタ）。`cargo run -q -p fandhe-db-wire-server
//! --example openapi_dump > <一時ディレクトリ>/openapi.json` で実行する。
//! 生成本体は `wire_server::http::query::openapi`（#2244 の配信もそこを呼ぶ）。
//!
//! ゴールデン `crates/wire-server/docs/openapi.json`（Issue #2105）の正式な再生成手段でもある:
//! `cargo run -q -p fandhe-db-wire-server --example openapi_dump >
//! crates/wire-server/docs/openapi.json`。`OP_SCHEMAS` かクレート版を変えたら実行する。
//! 一致検査は `tests/http_nosql/nosql20_openapi_golden.rs`。本ツール自体は CI に載せない。

use std::process::ExitCode;

use wire_server::http::query::openapi::render_pretty;

fn main() -> ExitCode {
    match render_pretty() {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("failed to generate the OpenAPI document");
            ExitCode::FAILURE
        }
    }
}
