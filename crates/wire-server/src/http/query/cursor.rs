//! `scan` の不透明カーソル（`after`／`next_cursor`）の AAD 構築と文字列化
//! （Issue #2094・NOSQL-23・TASK-253。ポインタ: `docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-23）。
//!
//! 呼び出し文脈: [`super::scan`] が、engine の
//! [`engine::sql::keyset::KeysetPosition::seal`]／[`open`](engine::sql::keyset::KeysetPosition::open)
//! （encrypt-then-MAC。単一実装）へ渡す AAD の構築と、封緘バイト列 ⇔ 標準 base64 文字列の変換を
//! 本モジュールへ委ねる。暗号処理そのものは持たず（第 2 の実装を作らない）、wire 側の責務は
//! 「何にカーソルを束縛するか」（テナント・認証主体・表・`sort`）と、受信文字列の確保前の長さ制限に限る。
//!
//! 失敗はすべて [`CursorError::Invalid`] の 1 種に畳む（base64 不正・長さ超過・MAC 不一致・
//! 版違い・形式不正を区別させない。失敗理由を観測させるオラクルを作らない）。受信経路のため
//! `unwrap`／`expect`／添字アクセスを使わない。

use engine::sql::allowlist::{ScalarOrderKey, SqlSurfaceError, MAX_SCALAR_ORDER_KEYS};
use engine::sql::keyset::{KeysetPosition, KeysetSealKey, MAX_KEYSET_SEALED_BYTES};

use super::base64_std::{decode_base64_std, encode_base64_std};

/// AAD の先頭に置く用途ラベル（別用途の封緘との取り違えを防ぐドメイン分離）。
const AAD_LABEL: &[u8] = b"fandhe-db scan cursor v1";

/// 受信カーソルの失敗（理由を区別しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorError {
    /// 復号・検証できないカーソル（`22000`。固定文言）。
    Invalid,
    /// AAD を組み立てられない（長さが `u32` を超える・`sort` 件数が上限超過。到達しない想定の fail-closed）。
    Unbindable,
}

/// 長さを前置して 1 要素を AAD へ追記する（境界の曖昧さを無くす）。
fn put_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CursorError> {
    let len = u32::try_from(bytes.len()).map_err(|_| CursorError::Unbindable)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// カーソルを束縛する AAD を構築する。テナント・認証主体・表・`sort`（列名と方向）を、
/// 長さ前置の連結で正準化する（`table=ab,user=c` と `table=a,user=bc` が衝突しない）。
/// `filter`／`columns`／`limit` は束縛しない（ページ間で変えてよい。位置の型照合は engine が行う）。
pub fn build_aad(
    tenant: &str,
    user: &str,
    table: &str,
    sort: &[ScalarOrderKey],
) -> Result<Vec<u8>, CursorError> {
    if sort.len() > MAX_SCALAR_ORDER_KEYS {
        return Err(CursorError::Unbindable);
    }
    let count = u8::try_from(sort.len()).map_err(|_| CursorError::Unbindable)?;
    let mut out = Vec::new();
    out.extend_from_slice(AAD_LABEL);
    put_len_prefixed(&mut out, tenant.as_bytes())?;
    put_len_prefixed(&mut out, user.as_bytes())?;
    put_len_prefixed(&mut out, table.as_bytes())?;
    out.push(count);
    for key in sort {
        put_len_prefixed(&mut out, key.column.as_bytes())?;
        out.push(u8::from(key.descending));
    }
    Ok(out)
}

/// 位置を封緘して標準 base64 文字列（`next_cursor`）にする。上限超過等は engine の分類を透過する。
pub fn encode_cursor(
    position: &KeysetPosition,
    key: &KeysetSealKey,
    aad: &[u8],
) -> Result<String, SqlSurfaceError> {
    let sealed = position.seal(key, aad)?;
    Ok(encode_base64_std(&sealed))
}

/// 受信した `after` 文字列を開封して位置へ戻す。base64 の復号は確保前に
/// [`MAX_KEYSET_SEALED_BYTES`] で打ち切り、以降の失敗はすべて [`CursorError::Invalid`]。
pub fn decode_cursor(
    cursor: &str,
    key: &KeysetSealKey,
    aad: &[u8],
) -> Result<KeysetPosition, CursorError> {
    let max = u32::try_from(MAX_KEYSET_SEALED_BYTES).map_err(|_| CursorError::Invalid)?;
    let sealed = decode_base64_std(cursor, max).map_err(|_| CursorError::Invalid)?;
    KeysetPosition::open(key, aad, &sealed).map_err(|_| CursorError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sort(col: &str, desc: bool) -> ScalarOrderKey {
        ScalarOrderKey {
            column: col.to_string(),
            descending: desc,
        }
    }

    fn key(b: u8) -> KeysetSealKey {
        KeysetSealKey::from_bytes([b; 32])
    }

    #[test]
    fn aad_has_no_boundary_ambiguity() {
        let a = build_aad("t", "c", "ab", &[]).expect("aad");
        let b = build_aad("t", "bc", "a", &[]).expect("aad");
        assert_ne!(a, b);
    }

    #[test]
    fn aad_distinguishes_every_bound_element() {
        let base = build_aad("t", "u", "tbl", &[sort("a", false)]).expect("aad");
        for other in [
            build_aad("t2", "u", "tbl", &[sort("a", false)]),
            build_aad("t", "u2", "tbl", &[sort("a", false)]),
            build_aad("t", "u", "tbl2", &[sort("a", false)]),
            build_aad("t", "u", "tbl", &[sort("b", false)]),
            build_aad("t", "u", "tbl", &[sort("a", true)]),
            build_aad("t", "u", "tbl", &[]),
        ] {
            assert_ne!(base, other.expect("aad"));
        }
    }

    #[test]
    fn aad_rejects_too_many_sort_keys() {
        let keys: Vec<_> = (0..=MAX_SCALAR_ORDER_KEYS)
            .map(|i| sort(&format!("c{i}"), false))
            .collect();
        assert_eq!(
            build_aad("t", "u", "tbl", &keys),
            Err(CursorError::Unbindable)
        );
    }

    #[test]
    fn garbage_cursors_all_map_to_the_same_error() {
        let k = key(1);
        let aad = build_aad("t", "u", "tbl", &[]).expect("aad");
        let oversized = "A".repeat(200_000);
        for bad in ["", "!!!!", "abc", "AAAA", "AAAA====", oversized.as_str()] {
            assert_eq!(
                decode_cursor(bad, &k, &aad).unwrap_err(),
                CursorError::Invalid
            );
        }
    }
}
