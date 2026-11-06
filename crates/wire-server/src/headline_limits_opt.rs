//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-ts-headline-bytes`／
//! `--max-ts-headline-fragments` opt-in CLI 引数のパーサ（Issue #2125。ポインタ:
//! `docs/spec/04-behavior/search.md` SEARCH-14）。
//!
//! `sparsevec_limits_opt`（`--max-sparsevec-nnz`）と同型の「プロセス起動時にのみ明示指定する
//! 注入点」。範囲（ハード上限以下への引き下げのみ）の判定は engine 側の単一情報源
//! [`engine::headline_limits::validate_max_output_bytes`]／
//! [`engine::headline_limits::validate_max_fragments`] に委ね、本モジュールは untrusted な
//! CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_headline_limits` へ 1 回だけ注入する（`docs/design/ts-headline-limits-cli.md`）。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::headline_limits::{validate_max_fragments, validate_max_output_bytes, HeadlineLimits};

/// `ts_headline` の出力バイト長上限を設定する CLI フラグ名。
pub const MAX_TS_HEADLINE_BYTES_FLAG: &str = "--max-ts-headline-bytes";

/// `ts_headline` の `MaxFragments` 上限を設定する CLI フラグ名。
pub const MAX_TS_HEADLINE_FRAGMENTS_FLAG: &str = "--max-ts-headline-fragments";

/// CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct HeadlineLimitsRawFlags<'a> {
    pub max_bytes: Option<&'a str>,
    pub max_fragments: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// 未パース値から [`HeadlineLimits`] を解決する純関数。未指定は既定値（ハード上限）、
/// 範囲外・不正値は `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: HeadlineLimitsRawFlags<'_>) -> Result<HeadlineLimits, String> {
    let defaults = HeadlineLimits::default();
    let bytes = match raw.max_bytes {
        None => defaults.max_output_bytes(),
        Some(r) => {
            let v = parse_strict_decimal(r).ok_or_else(|| {
                format!("{MAX_TS_HEADLINE_BYTES_FLAG} expects a non-negative integer, got {r:?}")
            })?;
            validate_max_output_bytes(v)
                .map_err(|e| format!("{MAX_TS_HEADLINE_BYTES_FLAG}: {e}"))?
        }
    };
    let fragments = match raw.max_fragments {
        None => defaults.max_fragments(),
        Some(r) => {
            let v = parse_strict_decimal(r).ok_or_else(|| {
                format!(
                    "{MAX_TS_HEADLINE_FRAGMENTS_FLAG} expects a non-negative integer, got {r:?}"
                )
            })?;
            validate_max_fragments(v)
                .map_err(|e| format!("{MAX_TS_HEADLINE_FRAGMENTS_FLAG}: {e}"))?
        }
    };
    Ok(HeadlineLimits::new(bytes, fragments))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(bytes: Option<&str>, frags: Option<&str>) -> Result<HeadlineLimits, String> {
        resolve(HeadlineLimitsRawFlags {
            max_bytes: bytes,
            max_fragments: frags,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None), Ok(HeadlineLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let low = one(Some("1"), Some("1")).expect("lower");
        assert_eq!((low.max_output_bytes(), low.max_fragments()), (1, 1));
        assert_eq!(
            one(Some("4194304"), Some("1024")),
            Ok(HeadlineLimits::default())
        );
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "4194305",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
        ] {
            let e = one(Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_TS_HEADLINE_BYTES_FLAG), "{bad}: {e}");
        }
        for bad in ["0", "1025", "+5", "x", ""] {
            let e = one(None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_TS_HEADLINE_FRAGMENTS_FLAG), "{bad}: {e}");
        }
    }
}
