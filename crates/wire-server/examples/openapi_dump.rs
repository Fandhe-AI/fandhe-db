//! `OP_SCHEMAS` から生成した OpenAPI 3.1 文書を標準出力へ書き出す手元検証用ツール
//! （Issue #2104。NOSQL-20 ポインタ）。`cargo run -q -p fandhe-db-wire-server
//! --example openapi_dump > <一時ディレクトリ>/openapi.json` で実行する。
//! 生成本体は `wire_server::http::query::openapi`（#2105 のゴールデン作成・#2244 の
//! 配信もそこを呼ぶ）。CI には載せない。

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
