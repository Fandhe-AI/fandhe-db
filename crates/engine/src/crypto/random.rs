//! OS の CSPRNG（`/dev/urandom`）読み取り。
//!
//! 新規依存を追加せず標準ライブラリのみで実装する（Unix 前提。読めない場合は
//! エラーを返す fail-closed）。wire-server の `auth::read_urandom`（salt・nonce・
//! セッショントークン生成）と engine の `sql::role_ddl`（`CREATE ROLE ... PASSWORD`
//! の SCRAM salt 生成。Issue #1962）が共有する。

use std::io::Read;

/// 一度に要求できる最大バイト数。呼び出し側のバグによる巨大確保を防ぐ。
pub const MAX_RANDOM_LEN: usize = 4096;

/// `/dev/urandom` から `len` バイトを読む。`len` が上限超過なら `InvalidInput`。
pub fn read_urandom(len: usize) -> std::io::Result<Vec<u8>> {
    if len > MAX_RANDOM_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "random length exceeds limit",
        ));
    }
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}
