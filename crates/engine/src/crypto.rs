//! engine クレートが自作する暗号プリミティブの公開窓口。
//!
//! 依存最小方針（`.claude/rules/dependency-policy.md`）により、外部の暗号系
//! クレートには依存せず標準ライブラリのみで実装する。`unsafe` は使わない。
//!
//! Issue #1962 で wire-server の `auth` から [`hmac_sha256`]・[`base64_std`]・
//! [`scram_verifier`]（SCRAM 検証子の生成・解析）・[`random`]（CSPRNG）を移設した
//! （engine の `CREATE ROLE ... PASSWORD` が検証子を作るため。wire-server は再公開する）。
//!
//! 主な公開は [`sha256`]（`md5` は SQL の `md5()` 専用で crate 内限定）。元は `recovery::content_hash`
//! （TASK-101・RECOVER-10）内に private 実装として存在していたが、
//! `wire-server` の SCRAM-SHA-256 認証（Issue #940・WIRE-18・TASK-222）の
//! HMAC-SHA-256／PBKDF2 が同じ SHA-256 圧縮関数を必要としたため、engine の
//! 公開 API へ昇格した（`wire-server` は engine に依存する構成のため、
//! ここに置くことで両クレートが単一実装を共有できる）。
pub mod base64_std;
pub mod hmac_sha256;
pub(crate) mod md5;
pub mod random;
pub mod scram_verifier;
pub mod sha256;

/// 2 つのバイト列を定数時間で比較する（長さが異なれば不一致。長さそのものは秘密ではない）。
/// MAC・トークン検証の比較で、先頭の不一致位置がタイミング差として漏れないようにするための
/// 共有ヘルパー（Issue #2093 の `sql::keyset` の封緘位置の検証が使う）。
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
