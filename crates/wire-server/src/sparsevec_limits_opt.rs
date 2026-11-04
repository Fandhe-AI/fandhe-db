//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-sparsevec-nnz` opt-in CLI 引数の
//! パーサ（Issue #2027。ポインタ: `docs/spec/04-behavior/` TABLE-47 (2)・SQL-46）。
//!
//! `jsonpath_limits_opt`（`--max-jsonpath-steps` 等）と同型の「プロセス起動時にのみ明示指定する
//! 注入点」。範囲（ハード上限以下への引き下げのみ）の判定は engine 側の単一情報源
//! [`engine::sparsevec_limits::validate_max_nnz`] に委ね、本モジュールは untrusted な CLI 文字列を
//! 厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_sparsevec_limits` へ 1 回だけ注入する（`docs/design/sparsevec-limits-cli.md`）。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::sparsevec_limits::{validate_max_nnz, SparseVecLimits};

/// SPARSEVEC の非ゼロ要素数上限を設定する CLI フラグ名。
pub const MAX_SPARSEVEC_NNZ_FLAG: &str = "--max-sparsevec-nnz";

/// CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct SparseVecLimitsRawFlags<'a> {
    pub max_nnz: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// 未パース値から [`SparseVecLimits`] を解決する純関数。未指定は既定値（ハード上限）、
/// 範囲外・不正値は `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: SparseVecLimitsRawFlags<'_>) -> Result<SparseVecLimits, String> {
    let Some(r) = raw.max_nnz else {
        return Ok(SparseVecLimits::default());
    };
    let v = parse_strict_decimal(r).ok_or_else(|| {
        format!("{MAX_SPARSEVEC_NNZ_FLAG} expects a non-negative integer, got {r:?}")
    })?;
    let nnz = validate_max_nnz(v).map_err(|e| format!("{MAX_SPARSEVEC_NNZ_FLAG}: {e}"))?;
    Ok(SparseVecLimits::new(nnz))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(nnz: Option<&str>) -> Result<SparseVecLimits, String> {
        resolve(SparseVecLimitsRawFlags { max_nnz: nnz })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None), Ok(SparseVecLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        assert_eq!(one(Some("1")).expect("lower").max_nnz(), 1);
        assert_eq!(one(Some("16000")), Ok(SparseVecLimits::default()));
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "16001",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
        ] {
            let e = one(Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_SPARSEVEC_NNZ_FLAG), "{bad}: {e}");
        }
    }
}
