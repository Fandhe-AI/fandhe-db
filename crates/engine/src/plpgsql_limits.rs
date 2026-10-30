//! PL/pgSQL 構文段のハード上限定数（Issue #2224。設計は `docs/design/plpgsql-subset.md` D7・SQL-68）。
//!
//! 本ファイルは定数だけを置く。`PlpgsqlLimits` 構造体・CLI（`--max-plpgsql-*`）・
//! `EngineCore::with_plpgsql_limits` は #2225 がここへ追加し、起動時に下げた値を
//! `plpgsql::ParseOptions` へ渡す。構文解析器（`plpgsql::parse_body`）が参照する。

/// 本体（原文）の最大バイト数。格納上限（`catalog::function`）と同値になるよう参照で共有する。
/// 字句解析の前に検査する（untrusted 入力に対する無制限確保の禁止）。
pub(crate) const HARD_MAX_PLPGSQL_BODY_BYTES: usize =
    crate::catalog::function::MAX_FUNCTION_BODY_BYTES;

/// ブロック・制御構文の入れ子の深さの上限（超過は `54001`）。再帰下降のスタック保護も兼ねる。
pub(crate) const HARD_MAX_PLPGSQL_NESTING_DEPTH: usize = 32;

/// 本体の走査で許容するトークン数の上限（超過は `54000`）。
pub(crate) const HARD_MAX_PLPGSQL_TOKENS: usize = 65_536;

/// 1 個の埋め込み文・式が参照できる異なる変数の数の上限（超過は `54000`。`sql::params` の
/// 1 文あたり番号上限と同値）。
pub(crate) const MAX_PLPGSQL_VARS_PER_STATEMENT: usize = crate::sql::params::MAX_PARAMS as usize;
