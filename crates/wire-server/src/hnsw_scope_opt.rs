//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--hnsw-scope`
//! CLI 引数の閉じた語彙パーサ（Issue #1065・オーナー判断 2026-09-28）。
//!
//! `--durability`（`durability_opt`）・`--search-engine`（`search_engine_opt`）と
//! 同型の「プロセス起動時にのみ明示指定する注入点」であり、
//! `engine::search_engine::HnswScope`（HNSW opt-in 時に HNSW 経路を使う
//! テーブルの範囲。`engine::core::EngineCore::with_hnsw_scope` へ渡す）へ
//! untrusted な CLI 文字列から到達する唯一の入口を本モジュールに置く。
//!
//! - `all`（既定）: HNSW opt-in 時は全テーブル HNSW（`USING hnsw` 宣言は
//!   カタログへ記録されるだけで経路を変えない。宣言導入前と同一の挙動）
//! - `declared`: `CREATE INDEX ... USING hnsw` を宣言したテーブルだけ HNSW、
//!   未宣言テーブルは厳密（brute-force）
//!
//! HNSW opt-in（`--search-engine hnsw*`）が無効な構成では値は参照されず
//! （全テーブル厳密のまま）、組合せによる起動エラーにもしない（オーナー判断:
//! opt-in 無効時は scope は無関係）。判定の詳細は `docs/design/
//! index-declaration-effects.md`「HNSW（テーブル単位）」参照。
//!
//! `HnswScope` に variant が追加される場合は本モジュールの [`TOKENS`]・
//! [`parse`]・[`token_for`] の `match` を明示的に更新する契約とする。

use engine::search_engine::HnswScope;

/// `--hnsw-scope` の CLI フラグ名。
pub const FLAG: &str = "--hnsw-scope";

/// `--hnsw-scope` が受理する語彙（順序は `parse` の分岐・エラーメッセージの
/// 一覧順・README 記載順の単一情報源）。
pub const TOKENS: [&str; 2] = ["all", "declared"];

/// `raw`（CLI 引数の値）を [`TOKENS`] の厳密一致でのみ受理する（trim・大文字
/// 小文字の読み替えはしない。`durability_opt::parse` と同じ「厳密一致のみ
/// 受理」方針。typo を黙って既定へ読み替えると意図と異なる経路で検索する事故を
/// fail-closed で防げなくなる）。
pub fn parse(raw: &str) -> Result<HnswScope, String> {
    match raw {
        "all" => Ok(HnswScope::All),
        "declared" => Ok(HnswScope::Declared),
        other => Err(format!("{FLAG} must be one of {TOKENS:?} (got {other:?})")),
    }
}

/// [`TOKENS`] のうち `scope` に対応する文字列表現（診断メッセージ用）。
pub fn token_for(scope: HnswScope) -> &'static str {
    match scope {
        HnswScope::All => "all",
        HnswScope::Declared => "declared",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_all_tokens() {
        for tok in TOKENS {
            assert!(parse(tok).is_ok(), "expected {tok:?} to be accepted");
        }
    }

    #[test]
    fn parse_all_maps_to_default() {
        assert_eq!(parse("all"), Ok(HnswScope::All));
        assert_eq!(HnswScope::All, HnswScope::default());
    }

    #[test]
    fn parse_declared_maps_to_declared_variant() {
        assert_eq!(parse("declared"), Ok(HnswScope::Declared));
    }

    #[test]
    fn parse_rejects_case_variants_whitespace_and_unknown_values() {
        for raw in [
            "All",
            "ALL",
            "Declared",
            " declared",
            "declared ",
            "",
            "none",
            "table",
            "all\n",
            "all,declared",
        ] {
            let err = parse(raw).expect_err("strict match only");
            assert!(err.contains(FLAG), "unexpected error: {err}");
        }
    }

    #[test]
    fn parse_rejects_control_character_injection() {
        // 制御文字を含む untrusted な引数も厳密一致で弾かれ、エラー文言へは
        // Debug 表記（`{:?}`）でエスケープされて埋め込まれる。
        let err = parse("declared\0bogus").expect_err("must reject control characters");
        assert!(err.contains(FLAG));
        assert!(!err.contains('\0'));
    }

    #[test]
    fn token_round_trips_through_parse() {
        for tok in TOKENS {
            let scope = parse(tok).expect("valid token");
            assert_eq!(token_for(scope), tok);
        }
    }
}
