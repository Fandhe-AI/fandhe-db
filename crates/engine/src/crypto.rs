//! engine クレートが自作する暗号プリミティブの公開窓口。
//!
//! 依存最小方針（`.claude/rules/dependency-policy.md`）により、外部の暗号系
//! クレートには依存せず標準ライブラリのみで実装する。`unsafe` は使わない。
//!
//! 公開するのは [`sha256`] のみ（`md5` は SQL の `md5()` 専用で crate 内限定）。元は `recovery::content_hash`
//! （TASK-101・RECOVER-10）内に private 実装として存在していたが、
//! `wire-server` の SCRAM-SHA-256 認証（Issue #940・WIRE-18・TASK-222）の
//! HMAC-SHA-256／PBKDF2 が同じ SHA-256 圧縮関数を必要としたため、engine の
//! 公開 API へ昇格した（`wire-server` は engine に依存する構成のため、
//! ここに置くことで両クレートが単一実装を共有できる）。
pub(crate) mod md5;
pub mod sha256;
