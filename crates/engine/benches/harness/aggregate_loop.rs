//! 集計の行ループ計測向けの時間非依存ロジック（Issue #1490。親 #1486・是正は #1487〜#1489、
//! 設計記録は #1491。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-25 の集計表層）。
//!
//! `aggregate_loop_bench.rs`（実測本体・時間依存）と `tests/relational_p95_accept.rs`
//! （`make ci` 対象の回帰）の 2 コンパイル単位から取り込まれる。タイマーには依存せず、
//! env のパース・arm の SQL 文・fixture から独立に導く期待値・1 セルあたり ns の導出・
//! 出力行の整形だけを置く（`relational_p95.rs` と同じ分離方針。統計は同モジュールのものを
//! 再利用し、計測プロトコルは `protocol` に従う）。
//!
//! 集計の行ループ（可視行 1 件ごとの `observe`）のコストは、行数 × 集計項目数の「セル」あたり
//! ns で表せる。退行が数 ns/セル単位でも全体に効くため、セルあたり ns を参考値として併記する。
//! SQL 文は定数のみから組み立て、未検証文字列を連結しない。出力にテナント ID・行の値・
//! SQL 全文は含めない。
//!
//! 依存は `std` と兄弟 harness（`relational_p95`）のみ。

use std::fmt;
use std::time::Duration;

use super::relational_p95::{RoundSummary, LANG_CARDINALITY};

/// fixture の自テナント行数（crossdb 再計測と同じ規模）。
pub const AGG_ROWS: usize = 25_000;
/// fixture のベクトル次元（集計経路は埋め込みをデコードしないため性能にはほぼ効かない）。
pub const AGG_DIM: u32 = 128;
/// 縮小規模の下限。
pub const MIN_ROWS: usize = 1_000;
/// ラウンド数の下限・上限・既定（`benchmark-judgement-policy` の N≥5）。
pub const MIN_ROUNDS: u32 = 5;
pub const MAX_ROUNDS: u32 = 50;
pub const DEFAULT_ROUNDS: u32 = 5;
/// 他テナント行の `lang` 値。自テナントの語彙（`l0`..`l15`）の外にあり、越境すれば
/// `COUNT(DISTINCT lang)` が変わる。
pub const OTHER_TENANT_LANG: &str = "x0";

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateLoopError {
    InvalidRounds(String),
    InvalidRows(String),
    /// GitHub Actions 下で専有環境の申告が渡された（hosted runner は専有でない）。
    DedicatedAttestedUnderGithubActions,
}

impl fmt::Display for AggregateLoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRounds(r) => write!(f, "invalid BENCH_AGGREGATE_LOOP_ROUNDS: {r}"),
            Self::InvalidRows(r) => write!(f, "invalid BENCH_AGGREGATE_LOOP_ROWS: {r}"),
            Self::DedicatedAttestedUnderGithubActions => write!(
                f,
                "BENCH_DEDICATED_ENV=1 is invalid under GitHub Actions (hosted runners are shared environments)"
            ),
        }
    }
}

impl std::error::Error for AggregateLoopError {}

/// `BENCH_AGGREGATE_LOOP_ROUNDS` を fail-closed にパースする（空・未設定は既定）。
pub fn parse_rounds(raw: Option<&str>) -> Result<u32, AggregateLoopError> {
    let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_ROUNDS);
    };
    let value: u32 = trimmed
        .parse()
        .map_err(|_| AggregateLoopError::InvalidRounds(format!("not an integer: {trimmed:?}")))?;
    if !(MIN_ROUNDS..=MAX_ROUNDS).contains(&value) {
        return Err(AggregateLoopError::InvalidRounds(format!(
            "must be in {MIN_ROUNDS}..={MAX_ROUNDS}, got {value}"
        )));
    }
    Ok(value)
}

/// `BENCH_AGGREGATE_LOOP_ROWS` を fail-closed にパースする。縮小方向（`MIN_ROWS..=AGG_ROWS`）のみ許す。
pub fn parse_rows(raw: Option<&str>) -> Result<usize, AggregateLoopError> {
    let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(AGG_ROWS);
    };
    let value: usize = trimmed
        .parse()
        .map_err(|_| AggregateLoopError::InvalidRows(format!("not an integer: {trimmed:?}")))?;
    if !(MIN_ROWS..=AGG_ROWS).contains(&value) {
        return Err(AggregateLoopError::InvalidRows(format!(
            "must be in {MIN_ROWS}..={AGG_ROWS}, got {value}"
        )));
    }
    Ok(value)
}

/// 規模ラベル（規定の行数なら `full`、それ以外は `reduced`）。
pub fn scale_label(rows: usize) -> &'static str {
    if rows == AGG_ROWS {
        "full"
    } else {
        "reduced"
    }
}

/// 他テナントの Private 行数。
pub fn other_rows(rows: usize) -> usize {
    (rows / 50).max(100)
}

/// GitHub Actions 下の専有環境申告を拒否する。起動そのものは許す（本ベンチは bench.yml から
/// 参考値として実行する）が、hosted runner は共有環境なので専有の申告は誤りであり fail-closed に倒す。
pub fn refuse_dedicated_under_github_actions(
    under_github_actions: bool,
    dedicated: bool,
) -> Result<(), AggregateLoopError> {
    if under_github_actions && dedicated {
        return Err(AggregateLoopError::DedicatedAttestedUnderGithubActions);
    }
    Ok(())
}

/// 計測 arm。`items` は 1 行あたりの集計項目数（セル数の算出に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm {
    pub label: &'static str,
    pub sql: &'static str,
    pub items: usize,
    /// `EXPLAIN` の `access_path:` が取るべきトークン（行ループを測っていることの自己検査用）。
    pub access_path: &'static str,
}

/// crossdb の agg_count／agg_multi と同形の文、および COUNT(DISTINCT)。
pub const ARMS: [Arm; 3] = [
    Arm {
        label: "agg_count",
        sql: "SELECT COUNT(*) FROM docs",
        items: 1,
        access_path: "visible_bitmap_cache",
    },
    Arm {
        label: "agg_multi",
        sql: "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM docs",
        items: 5,
        access_path: "visible_bitmap_cache",
    },
    Arm {
        label: "agg_count_distinct",
        sql: "SELECT COUNT(DISTINCT lang) FROM docs",
        items: 1,
        access_path: "full_scan",
    },
];

/// 可視行集合に対する集計の期待値（エンジンとは独立に fixture の規則から導く）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Expected {
    pub count: u64,
    pub sum: u64,
    pub min: u64,
    pub max: u64,
    pub avg: f64,
    pub distinct_lang: u64,
}

/// どのテナント文脈から見るか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewer {
    /// 自テナント（id `0..own`）。他テナントの Private 行は見えない。
    Own,
    /// 他テナント（自分の Private 行 id `own..own+other` ＋ 自テナントの Public 行）。
    Other,
}

/// 可視行 id が `0..n` の連続であることを使って期待値を導く（fixture は id を連番で投入する）。
/// 0 行、または算術オーバーフローなら `None`。
pub fn expected_aggregates(own: u64, other: u64, viewer: Viewer) -> Option<Expected> {
    let n = match viewer {
        Viewer::Own => own,
        Viewer::Other => own.checked_add(other)?,
    };
    if n == 0 {
        return None;
    }
    let sum = n.checked_mul(n.checked_sub(1)?)? / 2;
    let own_langs = LANG_CARDINALITY.min(own);
    let distinct_lang = match viewer {
        Viewer::Own => own_langs,
        Viewer::Other if other > 0 => own_langs.checked_add(1)?,
        Viewer::Other => own_langs,
    };
    Some(Expected {
        count: n,
        sum,
        min: 0,
        max: n.checked_sub(1)?,
        avg: sum as f64 / n as f64,
        distinct_lang,
    })
}

/// 1 セル（行 × 集計項目）あたりの ns の参考値。0 行・0 項目・オーバーフローは `None`。
pub fn per_cell_ns(d: Duration, rows: usize, items: usize) -> Option<f64> {
    let cells = rows.checked_mul(items)?;
    if cells == 0 {
        return None;
    }
    Some(d.as_secs_f64() * 1e9 / cells as f64)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// ラウンド行（`relational_p95` と同じ並び。行頭のみ `aggregate_loop:`）。
pub fn render_round_line(
    arm: &str,
    round: usize,
    p95: Duration,
    median: Duration,
    loadavg: &str,
) -> String {
    format!(
        "aggregate_loop: group=aggregate arm={arm} round={round} p95={:.3}ms median={:.3}ms loadavg={loadavg}",
        ms(p95),
        ms(median)
    )
}

/// 要約行（`relational_p95` と同じ並び。比率は出さない）。
pub fn render_summary_line(arm: &str, rows: usize, scale: &str, s: &RoundSummary) -> String {
    format!(
        "aggregate_loop: group=aggregate arm={arm} rows={rows} scale={scale} min_of_n={:.3}ms median={:.3}ms max={:.3}ms run_to_run_band={:.1}%",
        ms(s.min),
        ms(s.median),
        ms(s.max),
        s.run_to_run_band_pct
    )
}

/// 導出行（セルあたり ns の参考値）。
pub fn render_derived_line(arm: &Arm, rows: usize, s: &RoundSummary) -> String {
    let fmt_ns = |d: Duration| match per_cell_ns(d, rows, arm.items) {
        Some(v) => format!("{v:.3}"),
        None => "n/a".to_string(),
    };
    format!(
        "aggregate_loop: arm={} rows={rows} items={} cells={} ns_per_cell_min={} ns_per_cell_median={}",
        arm.label,
        arm.items,
        rows.saturating_mul(arm.items),
        fmt_ns(s.min),
        fmt_ns(s.median)
    )
}

/// 閾値行。spec の基準を持たない参考実測なので常に未評価。
pub fn render_threshold_line() -> String {
    "threshold_judgement: not evaluated (no spec criteria for this group; regression reference values only)".to_string()
}
