//! 衝突有無で応答時間分布を比較するベンチ `rls_timing_bench.rs` の時間非依存ロジック
//! （Issue #2079。親 #2078。ポインタ: `docs/spec/04-behavior/` RLS-9・RLS-10）。
//!
//! 役割: 他テナントにだけ親行・UNIQUE 値がある状態（flooded）とどこにも無い状態（baseline）で、
//! 同一の書き込み文の応答時間分布が区別できるかを測るための、env の fail-closed パース・形状定義・
//! seed 行の真実値・SQL 事前生成・応答の正規化と照合・分位点・生サンプル入り JSON の生成を提供する。
//! 呼び出し元は `rls_timing_bench.rs`（実測）と `tests/rls_timing_accept.rs`（`cargo test` での
//! 回帰検証）。`engine` には依存せず、エンジン呼び出し（seed 投入・SQL 実行）はベンチ側が担う。
//!
//! Issue #2080 で、(shape, arm) 要約から分位点差の判定（`judge_rows`・`overall`）と、生 JSON
//! （`raw.json`）を読み戻して `Round` 列を復元する `parse_raw_rounds`（再判定の入口）を加えた。
//! 判定基準は本リポで決めた公開値（固定帯・A/A 帯上限。`docs/design/rls-timing-bench.md`）で、
//! spec 由来の閾値は持たない。判定は参考値であり、終了コードへは反映しない（呼び出し元の責務）。

use std::fmt;
use std::time::Duration;

use super::chip::{json_escape, parse_json, JsonValue};
use super::relational_p95::quantile_nearest_rank;

pub const MIN_ROUNDS: u32 = 5;
pub const MAX_ROUNDS: u32 = 50;
pub const DEFAULT_ROUNDS: u32 = 5;
pub const MIN_ITERS: u32 = 200;
pub const MAX_ITERS: u32 = 20_000;
pub const DEFAULT_ITERS: u32 = 1_000;
/// 各 (shape, arm) の warmup 回数（計測プロトコルの下限）。
pub const WARMUP: u32 = 20;
/// 計測入力は決定的（乱数を使わない）。プロトコルが要求する固定シード。
pub const SEED: u64 = 2079;

pub const TENANT_VIEWER: &str = "bench-tenant-a";
pub const TENANT_FLOODERS: [&str; 2] = ["bench-tenant-b", "bench-tenant-c"];

/// 表定義（`tests/rls10_write_constraint_paths.rs` の `DDL` の部分集合）。
pub const DDL: [&str; 3] = [
    "CREATE TABLE uniq (code TEXT UNIQUE, a TEXT, b TEXT)",
    "CREATE TABLE parents (name TEXT)",
    "CREATE TABLE children (parent_id BIGINT REFERENCES parents, note TEXT)",
];

/// 本モジュールのエラー（Display は英語）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RlsTimingError {
    InvalidRounds(String),
    InvalidIters(String),
    InvalidShapes(String),
    EmptySamples,
    /// `GITHUB_ACTIONS` 下で専有環境を申告する矛盾した設定。
    ContradictoryEnv,
    /// 生 JSON の読み戻しに失敗した（構文・スキーマ・値域の違反）。
    InvalidRaw(String),
    /// 判定に必要な (shape, arm) の要約が揃っていない。
    MissingSummary(String),
}

impl fmt::Display for RlsTimingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRounds(m) => write!(f, "invalid BENCH_RLS_TIMING_ROUNDS: {m}"),
            Self::InvalidIters(m) => write!(f, "invalid BENCH_RLS_TIMING_ITERS: {m}"),
            Self::InvalidShapes(m) => write!(f, "invalid BENCH_RLS_TIMING_SHAPES: {m}"),
            Self::EmptySamples => write!(f, "empty sample set"),
            Self::InvalidRaw(m) => write!(f, "invalid raw json: {m}"),
            Self::MissingSummary(m) => write!(f, "missing summary: {m}"),
            Self::ContradictoryEnv => write!(
                f,
                "BENCH_DEDICATED_ENV=1 under GITHUB_ACTIONS is contradictory (hosted runners are not dedicated)"
            ),
        }
    }
}

impl std::error::Error for RlsTimingError {}

fn parse_ranged(raw: Option<&str>, min: u32, max: u32, default: u32) -> Result<u32, String> {
    let Some(t) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(default);
    };
    let v: u32 = t.parse().map_err(|_| format!("not an integer: {t:?}"))?;
    if !(min..=max).contains(&v) {
        return Err(format!("must be in {min}..={max}, got {v}"));
    }
    Ok(v)
}

/// `BENCH_RLS_TIMING_ROUNDS` を fail-closed にパースする（空・未設定は既定）。
pub fn parse_rounds(raw: Option<&str>) -> Result<u32, RlsTimingError> {
    parse_ranged(raw, MIN_ROUNDS, MAX_ROUNDS, DEFAULT_ROUNDS).map_err(RlsTimingError::InvalidRounds)
}

/// `BENCH_RLS_TIMING_ITERS`（ラウンドあたりの計測回数）を fail-closed にパースする。
pub fn parse_iters(raw: Option<&str>) -> Result<u32, RlsTimingError> {
    parse_ranged(raw, MIN_ITERS, MAX_ITERS, DEFAULT_ITERS).map_err(RlsTimingError::InvalidIters)
}

/// 計測する書き込み文の形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    UniqueViolation,
    FkMissingParent,
    Normal,
}

pub const ALL_SHAPES: [Shape; 3] = [
    Shape::UniqueViolation,
    Shape::FkMissingParent,
    Shape::Normal,
];

/// 形状ごとの期待される応答（両 arm で同一であることが前提）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    Rejected(&'static str),
    Affected(u64),
}

impl Shape {
    pub fn token(self) -> &'static str {
        match self {
            Self::UniqueViolation => "unique_violation",
            Self::FkMissingParent => "fk_missing_parent",
            Self::Normal => "normal",
        }
    }

    pub fn expected(self) -> Expected {
        match self {
            Self::UniqueViolation => Expected::Rejected("23505"),
            Self::FkMissingParent => Expected::Rejected("23503"),
            Self::Normal => Expected::Affected(1),
        }
    }
}

/// `BENCH_RLS_TIMING_SHAPES` をパースする（カンマ区切り・完全一致・重複不可。空・未設定は全形状）。
pub fn parse_shapes(raw: Option<&str>) -> Result<Vec<Shape>, RlsTimingError> {
    let Some(t) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(ALL_SHAPES.to_vec());
    };
    let mut out: Vec<Shape> = Vec::new();
    for tok in t.split(',') {
        let shape = ALL_SHAPES
            .iter()
            .copied()
            .find(|s| s.token() == tok)
            .ok_or_else(|| RlsTimingError::InvalidShapes(format!("unknown shape: {tok:?}")))?;
        if out.contains(&shape) {
            return Err(RlsTimingError::InvalidShapes(format!(
                "duplicate shape: {tok:?}"
            )));
        }
        out.push(shape);
    }
    Ok(out)
}

/// 実行環境の自己申告の整合性を検査する（ホステッド runner は専有環境ではない）。
pub fn check_env_consistency(github_actions: bool, dedicated: bool) -> Result<(), RlsTimingError> {
    if github_actions && dedicated {
        return Err(RlsTimingError::ContradictoryEnv);
    }
    Ok(())
}

/// 比較する 2 状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    /// 閲覧テナントの行のみ。
    Baseline,
    /// 他 2 テナントが同一 id・同一 UNIQUE 値・同一 FK 親キーを持つ。
    Flooded,
}

pub const ALL_ARMS: [Arm; 2] = [Arm::Baseline, Arm::Flooded];

impl Arm {
    pub fn token(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Flooded => "flooded",
        }
    }
}

/// seed 列値（`engine::row_codec::Value` に依存しないための写像元）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedCol {
    Text(String),
    BigInt(i64),
}

/// 1 行の seed 定義（ベンチ側の真実値。production の判定に依存しない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedRow {
    pub table: &'static str,
    pub id: u64,
    pub cols: Vec<SeedCol>,
}

fn text(s: impl Into<String>) -> SeedCol {
    SeedCol::Text(s.into())
}

/// 全テナント共通の行（閲覧側の行そのもの。flooder も同一 id・同一値を持つ）。
pub fn common_rows(tenant: &str) -> Vec<SeedRow> {
    let mk = |table, id, cols| SeedRow { table, id, cols };
    vec![
        mk(
            "uniq",
            1,
            vec![text("u1"), text(format!("a-{tenant}")), text("b")],
        ),
        mk("parents", 1, vec![text(format!("p-{tenant}-1"))]),
        mk("parents", 2, vec![text(format!("p-{tenant}-2"))]),
        mk(
            "children",
            10,
            vec![SeedCol::BigInt(1), text(format!("c-{tenant}"))],
        ),
    ]
}

/// FK 親不在の形状が参照する親キー。flooder にだけ存在する。
pub const FOREIGN_ONLY_PARENT_ID: u64 = 7;
/// 「正常」形状の id の基点。反復 i は `NORMAL_ID_BASE + i`。
pub const NORMAL_ID_BASE: u64 = 1000;
/// 衝突形状（UNIQUE 違反・FK 親不在）が使う新規 id の基点（flooder の行と id が重ならない）。
pub const REJECT_ID_BASE: u64 = 1_000_000;

/// 「正常」形状の反復 i が使う UNIQUE 値。
pub fn normal_code(i: u32) -> String {
    format!("shared-{i}")
}

/// flooder だけが持つ追加行（閲覧側の書き込みで「他テナントにだけ存在する」値になる）。
/// `n_normal` は「正常」形状の反復数（warmup を含む）。
pub fn flood_extra_rows(tenant: &str, n_normal: u32) -> Vec<SeedRow> {
    let mut rows = vec![SeedRow {
        table: "parents",
        id: FOREIGN_ONLY_PARENT_ID,
        cols: vec![text(format!("p-{tenant}-7"))],
    }];
    for i in 0..n_normal {
        rows.push(SeedRow {
            table: "uniq",
            id: NORMAL_ID_BASE + u64::from(i),
            cols: vec![
                text(normal_code(i)),
                text(format!("na-{i}")),
                text(format!("nb-{i}")),
            ],
        });
    }
    rows
}

/// 反復 `0..n` 分の書き込み文を計測前に生成する。operation_id は (round, shape, i) で一意にし、
/// 台帳の再送経路を誤って測らない。SQL は固定テンプレートと数値だけで組み立てる。
pub fn build_statements(round: u32, shape: Shape, n: u32) -> Vec<String> {
    (0..n)
        .map(|i| {
            let op = format!("rt{round}-{}-{i}", shape.token());
            match shape {
                Shape::UniqueViolation => format!(
                    "INSERT INTO uniq (id, code, a, b) VALUES ({}, 'u1', 'x', 'y') \
                     USING OPERATION_ID '{op}'",
                    REJECT_ID_BASE + u64::from(i)
                ),
                Shape::FkMissingParent => format!(
                    "INSERT INTO children (id, parent_id, note) \
                     VALUES ({}, {FOREIGN_ONLY_PARENT_ID}, 'n') USING OPERATION_ID '{op}'",
                    REJECT_ID_BASE + u64::from(i)
                ),
                Shape::Normal => format!(
                    "INSERT INTO uniq (id, code, a, b) VALUES ({}, '{}', 'na-{i}', 'nb-{i}') \
                     USING OPERATION_ID '{op}'",
                    NORMAL_ID_BASE + u64::from(i),
                    normal_code(i)
                ),
            }
        })
        .collect()
}

/// 応答の正規化（`wire_code`・`client_message`・影響行数だけ。行内容やトークンは持たない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Affected(u64),
    Rejected {
        code: String,
        message: String,
    },
    /// 影響行数を持たない成功など、期待形状に当たらない応答。
    Other,
}

/// 応答が期待どおりか。
pub fn classify_matches(expected: Expected, outcome: &Outcome) -> bool {
    match (expected, outcome) {
        (Expected::Rejected(c), Outcome::Rejected { code, .. }) => c == code,
        (Expected::Affected(n), Outcome::Affected(m)) => n == *m,
        _ => false,
    }
}

/// 分位点（nearest-rank。`samples_ns` から同じ規則で再計算できる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantiles {
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub n: usize,
}

fn dur_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

pub fn quantiles(samples: &[Duration]) -> Result<Quantiles, RlsTimingError> {
    let q = |p: f64| {
        quantile_nearest_rank(samples, p)
            .map(dur_ns)
            .map_err(|_| RlsTimingError::EmptySamples)
    };
    Ok(Quantiles {
        p50_ns: q(0.50)?,
        p95_ns: q(0.95)?,
        p99_ns: q(0.99)?,
        n: samples.len(),
    })
}

/// 1 (round, shape, arm) セルの生サンプル。
#[derive(Debug, Clone)]
pub struct Cell {
    pub shape: Shape,
    pub arm: Arm,
    pub mismatches: u32,
    pub samples: Vec<Duration>,
}

/// 1 ラウンド分（`cells` は実行順）。
#[derive(Debug, Clone)]
pub struct Round {
    pub index: u32,
    pub cells: Vec<Cell>,
}

/// (shape, arm) ごとの要約（全ラウンドをプールした分位点とラウンド別分位点）。
#[derive(Debug, Clone)]
pub struct SummaryRow {
    pub shape: Shape,
    pub arm: Arm,
    pub pooled: Quantiles,
    pub per_round: Vec<(u32, Quantiles)>,
}

pub fn summarize(rounds: &[Round], shapes: &[Shape]) -> Result<Vec<SummaryRow>, RlsTimingError> {
    let mut out = Vec::new();
    for &shape in shapes {
        for arm in ALL_ARMS {
            let mut pooled: Vec<Duration> = Vec::new();
            let mut per_round = Vec::new();
            for r in rounds {
                for c in r.cells.iter().filter(|c| c.shape == shape && c.arm == arm) {
                    pooled.extend_from_slice(&c.samples);
                    per_round.push((r.index, quantiles(&c.samples)?));
                }
            }
            out.push(SummaryRow {
                shape,
                arm,
                pooled: quantiles(&pooled)?,
                per_round,
            });
        }
    }
    Ok(out)
}

/// 標準出力用の 1 行（単位 µs。小数 1 桁）。
pub fn render_summary_line(label: &str, shape: Shape, arm: Arm, q: &Quantiles) -> String {
    let us = |ns: u64| ns as f64 / 1000.0;
    format!(
        "rls_timing {label} shape={} arm={} n={} p50_us={:.1} p95_us={:.1} p99_us={:.1}",
        shape.token(),
        arm.token(),
        q.n,
        us(q.p50_ns),
        us(q.p95_ns),
        us(q.p99_ns)
    )
}

/// JSON の `env` 節と自己ラベルに使う情報。
#[derive(Debug, Clone)]
pub struct EnvInfo {
    pub os: String,
    pub arch: String,
    pub logical_cpus: usize,
    pub isa: String,
    pub loadavg: String,
    pub dedicated_env_attested: bool,
    pub github_actions: bool,
    pub git_commit: Option<String>,
}

fn quantiles_json(q: &Quantiles) -> String {
    format!(
        "{{\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"n\":{}}}",
        q.p50_ns, q.p95_ns, q.p99_ns, q.n
    )
}

fn str_json(s: &str) -> String {
    format!("\"{}\"", json_escape(s))
}

/// 生サンプル入りの JSON を組み立てる（serde 非依存の最小ライタ。`schema_version: 1`）。
/// ローカルの絶対パスは含めない。
pub fn render_json(
    env: &EnvInfo,
    iters: u32,
    shapes: &[Shape],
    rounds: &[Round],
) -> Result<String, RlsTimingError> {
    let summary = summarize(rounds, shapes)?;
    let mut s = String::new();
    s.push_str("{\"schema_version\":1,\"bench\":\"rls_timing_bench\",\"issue\":2079,");
    s.push_str("\"quantile_method\":\"nearest_rank\",\"unit\":\"ns\",");
    s.push_str(&format!(
        "\"env\":{{\"os\":{},\"arch\":{},\"logical_cpus\":{},\"isa\":{},\"loadavg\":{},\
         \"dedicated_env_attested\":{},\"github_actions\":{},\"git_commit\":{}}},",
        str_json(&env.os),
        str_json(&env.arch),
        env.logical_cpus,
        str_json(&env.isa),
        str_json(&env.loadavg),
        env.dedicated_env_attested,
        env.github_actions,
        env.git_commit
            .as_deref()
            .map_or("null".to_string(), str_json)
    ));
    let shape_list: Vec<String> = shapes.iter().map(|x| str_json(x.token())).collect();
    s.push_str(&format!(
        "\"config\":{{\"rounds\":{},\"warmup\":{WARMUP},\"measured\":{iters},\"seed\":{SEED},\
         \"shapes\":[{}],\"order_rule\":\"shape and arm order rotate per round\"}},",
        rounds.len(),
        shape_list.join(",")
    ));
    s.push_str("\"rounds\":[");
    for (ri, r) in rounds.iter().enumerate() {
        if ri > 0 {
            s.push(',');
        }
        s.push_str(&format!("{{\"round\":{},\"cells\":[", r.index));
        for (ci, c) in r.cells.iter().enumerate() {
            if ci > 0 {
                s.push(',');
            }
            let expected = match c.shape.expected() {
                Expected::Rejected(code) => format!("rejected:{code}"),
                Expected::Affected(n) => format!("affected:{n}"),
            };
            let samples: Vec<String> = c.samples.iter().map(|d| dur_ns(*d).to_string()).collect();
            s.push_str(&format!(
                "{{\"shape\":{},\"arm\":{},\"expected_outcome\":{},\"mismatches\":{},\
                 \"samples_ns\":[{}]}}",
                str_json(c.shape.token()),
                str_json(c.arm.token()),
                str_json(&expected),
                c.mismatches,
                samples.join(",")
            ));
        }
        s.push_str("]}");
    }
    s.push_str("],\"summary\":[");
    for (i, row) in summary.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let per: Vec<String> = row
            .per_round
            .iter()
            .map(|(r, q)| {
                format!(
                    "{{\"round\":{r},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{},\"n\":{}}}",
                    q.p50_ns, q.p95_ns, q.p99_ns, q.n
                )
            })
            .collect();
        s.push_str(&format!(
            "{{\"shape\":{},\"arm\":{},\"pooled\":{},\"per_round\":[{}]}}",
            str_json(row.shape.token()),
            str_json(row.arm.token()),
            quantiles_json(&row.pooled),
            per.join(",")
        ));
    }
    s.push_str("]}\n");
    Ok(s)
}
// ---------------------------------------------------------------------
// 判定（Issue #2080）
// ---------------------------------------------------------------------

/// 固定相対帯（p50）。`benchmark-judgement-policy.md` §4・Issue #401 の継承値。
pub const FIXED_BAND_P50: f64 = 0.05;
/// 固定相対帯（p95・p99）。
pub const FIXED_BAND_TAIL: f64 = 0.10;
/// A/A 帯の上限（p50）。超過は同一 arm 内のノイズが大きすぎて判定に使えない。
pub const AA_BAND_CAP_P50: f64 = 0.25;
/// A/A 帯の上限（p95・p99）。
pub const AA_BAND_CAP_TAIL: f64 = 0.50;

/// 判定対象の分位点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantile {
    P50,
    P95,
    P99,
}

pub const ALL_QUANTILES: [Quantile; 3] = [Quantile::P50, Quantile::P95, Quantile::P99];

impl Quantile {
    pub fn token(self) -> &'static str {
        match self {
            Self::P50 => "p50",
            Self::P95 => "p95",
            Self::P99 => "p99",
        }
    }

    pub fn pick(self, q: &Quantiles) -> u64 {
        match self {
            Self::P50 => q.p50_ns,
            Self::P95 => q.p95_ns,
            Self::P99 => q.p99_ns,
        }
    }

    pub fn fixed_band(self) -> f64 {
        match self {
            Self::P50 => FIXED_BAND_P50,
            _ => FIXED_BAND_TAIL,
        }
    }

    pub fn aa_cap(self) -> f64 {
        match self {
            Self::P50 => AA_BAND_CAP_P50,
            _ => AA_BAND_CAP_TAIL,
        }
    }
}

/// 判定結果。集約時の優先は `Inconclusive` ＞ `Distinguishable` ＞ `Indistinguishable`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Indistinguishable,
    Distinguishable,
    Inconclusive,
}

impl Verdict {
    pub fn token(self) -> &'static str {
        match self {
            Self::Indistinguishable => "indistinguishable",
            Self::Distinguishable => "distinguishable",
            Self::Inconclusive => "inconclusive",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Indistinguishable => 0,
            Self::Distinguishable => 1,
            Self::Inconclusive => 2,
        }
    }

    /// 優先順位の高い方を返す。
    pub fn merge(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// 対称相対差 `|a-b| / max(a,b)`（引数の順序に依存しない。両方 0 なら 0）。
pub fn relative_diff(a: u64, b: u64) -> f64 {
    let hi = a.max(b);
    if hi == 0 {
        return 0.0;
    }
    (hi - a.min(b)) as f64 / hi as f64
}

/// 同一 arm のラウンド別分位点の run-to-run 全幅 `(max-min)/min`（`policy` §4 の参照区間と同式）。
/// ラウンドが無い・最小値が 0 の場合は `+inf`（fail-closed で判定不能へ倒す）。
pub fn aa_band(per_round: &[u64]) -> f64 {
    let (Some(&min), Some(&max)) = (per_round.iter().min(), per_round.iter().max()) else {
        return f64::INFINITY;
    };
    if min == 0 {
        return f64::INFINITY;
    }
    (max - min) as f64 / min as f64
}

/// 1 分位点の判定内訳。
#[derive(Debug, Clone, Copy)]
pub struct QuantileJudgement {
    pub quantile: Quantile,
    pub baseline_ns: u64,
    pub flooded_ns: u64,
    pub delta: f64,
    pub aa: f64,
    pub fixed_band: f64,
    pub aa_cap: f64,
    pub verdict: Verdict,
}

/// 1 形状の判定（3 分位点の内訳と集約結果）。
#[derive(Debug, Clone)]
pub struct ShapeJudgement {
    pub shape: Shape,
    pub quantiles: Vec<QuantileJudgement>,
    pub verdict: Verdict,
}

/// 1 分位点を判定する。`aa > cap` なら判定不能、`delta > max(固定帯, aa)` なら区別可能。
pub fn judge_quantile(q: Quantile, base: &SummaryRow, flood: &SummaryRow) -> QuantileJudgement {
    let b = q.pick(&base.pooled);
    let f = q.pick(&flood.pooled);
    let rounds =
        |row: &SummaryRow| -> Vec<u64> { row.per_round.iter().map(|(_, x)| q.pick(x)).collect() };
    let (ab, af) = (aa_band(&rounds(base)), aa_band(&rounds(flood)));
    let aa = if ab >= af { ab } else { af };
    let delta = relative_diff(b, f);
    let (fixed_band, aa_cap) = (q.fixed_band(), q.aa_cap());
    let verdict = if aa > aa_cap {
        Verdict::Inconclusive
    } else if delta > fixed_band.max(aa) {
        Verdict::Distinguishable
    } else {
        Verdict::Indistinguishable
    };
    QuantileJudgement {
        quantile: q,
        baseline_ns: b,
        flooded_ns: f,
        delta,
        aa,
        fixed_band,
        aa_cap,
        verdict,
    }
}

/// `summarize` の出力を形状ごとに判定する。baseline／flooded のどちらかが欠ければ拒否する。
pub fn judge_rows(rows: &[SummaryRow]) -> Result<Vec<ShapeJudgement>, RlsTimingError> {
    let mut shapes: Vec<Shape> = Vec::new();
    for r in rows {
        if !shapes.contains(&r.shape) {
            shapes.push(r.shape);
        }
    }
    let mut out = Vec::new();
    for shape in shapes {
        let find = |arm: Arm| {
            rows.iter()
                .find(|r| r.shape == shape && r.arm == arm)
                .ok_or_else(|| {
                    RlsTimingError::MissingSummary(format!(
                        "shape={} arm={}",
                        shape.token(),
                        arm.token()
                    ))
                })
        };
        let (base, flood) = (find(Arm::Baseline)?, find(Arm::Flooded)?);
        let quantiles: Vec<QuantileJudgement> = ALL_QUANTILES
            .iter()
            .map(|&q| judge_quantile(q, base, flood))
            .collect();
        let verdict = quantiles
            .iter()
            .fold(Verdict::Indistinguishable, |acc, j| acc.merge(j.verdict));
        out.push(ShapeJudgement {
            shape,
            quantiles,
            verdict,
        });
    }
    Ok(out)
}

/// 全体の判定。形状が無い場合は判定不能（pass にしない）。全形状が区別不能のときだけ
/// `Indistinguishable`（= pass）。
pub fn overall(judgements: &[ShapeJudgement]) -> Verdict {
    if judgements.is_empty() {
        return Verdict::Inconclusive;
    }
    judgements
        .iter()
        .fold(Verdict::Indistinguishable, |acc, j| acc.merge(j.verdict))
}

/// 全体判定の標準出力用 1 行（`pass`／`fail` と理由。reason は区別可能・判定不能の形状と分位点）。
pub fn render_overall_line(judgements: &[ShapeJudgement]) -> String {
    let v = overall(judgements);
    let mut reasons: Vec<String> = Vec::new();
    for j in judgements {
        for qj in &j.quantiles {
            if qj.verdict != Verdict::Indistinguishable {
                reasons.push(format!(
                    "{}:{}:{}",
                    j.shape.token(),
                    qj.quantile.token(),
                    qj.verdict.token()
                ));
            }
        }
    }
    let reason = if v == Verdict::Indistinguishable {
        "all_indistinguishable".to_string()
    } else if reasons.is_empty() {
        "no_shapes".to_string()
    } else {
        reasons.join(",")
    };
    let word = if v == Verdict::Indistinguishable {
        "pass"
    } else {
        "fail"
    };
    format!(
        "rls_timing overall={word} verdict={} reason={reason}",
        v.token()
    )
}

/// 分位点ごとの判定 1 行（単位 µs・比は小数 3 桁）。
pub fn render_verdict_line(shape: Shape, j: &QuantileJudgement) -> String {
    let us = |ns: u64| ns as f64 / 1000.0;
    format!(
        "rls_timing verdict shape={} q={} baseline_us={:.1} flooded_us={:.1} delta={:.3} \
         aa={:.3} fixed_band={:.2} aa_cap={:.2} result={}",
        shape.token(),
        j.quantile.token(),
        us(j.baseline_ns),
        us(j.flooded_ns),
        j.delta,
        j.aa,
        j.fixed_band,
        j.aa_cap,
        j.verdict.token()
    )
}

// ---------------------------------------------------------------------
// 生 JSON の読み戻し（再判定の入口。入力は untrusted として検証する）
// ---------------------------------------------------------------------

fn raw_err<T>(msg: impl Into<String>) -> Result<T, RlsTimingError> {
    Err(RlsTimingError::InvalidRaw(msg.into()))
}

/// JSON 数値を非負整数（2^53 未満）として取り出す。
fn as_uint(v: &JsonValue, what: &str) -> Result<u64, RlsTimingError> {
    let Some(n) = v.as_f64() else {
        return raw_err(format!("{what} is not a number"));
    };
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n >= 9_007_199_254_740_992.0 {
        return raw_err(format!("{what} is not a non-negative integer"));
    }
    Ok(n as u64)
}

fn field<'a>(v: &'a JsonValue, key: &str) -> Result<&'a JsonValue, RlsTimingError> {
    v.get(key)
        .map_or_else(|| raw_err(format!("missing key {key:?}")), Ok)
}

fn str_field<'a>(v: &'a JsonValue, key: &str) -> Result<&'a str, RlsTimingError> {
    field(v, key)?
        .as_str()
        .map_or_else(|| raw_err(format!("{key:?} is not a string")), Ok)
}

fn array_field<'a>(v: &'a JsonValue, key: &str) -> Result<&'a [JsonValue], RlsTimingError> {
    field(v, key)?
        .as_array()
        .map_or_else(|| raw_err(format!("{key:?} is not an array")), Ok)
}

fn shape_of(token: &str) -> Result<Shape, RlsTimingError> {
    ALL_SHAPES
        .iter()
        .copied()
        .find(|s| s.token() == token)
        .map_or_else(|| raw_err(format!("unknown shape {token:?}")), Ok)
}

fn arm_of(token: &str) -> Result<Arm, RlsTimingError> {
    ALL_ARMS
        .iter()
        .copied()
        .find(|a| a.token() == token)
        .map_or_else(|| raw_err(format!("unknown arm {token:?}")), Ok)
}

/// `render_json` が書いた `raw.json` を読み戻し、計測した形状と `Round` 列を復元する。
/// `schema_version`・`quantile_method`・形状／arm の語彙・サンプルの値域（非負整数・非空）・
/// ラウンドごとの全 (shape, arm) セルの存在と重複・`mismatches == 0` を検証し、外れれば拒否する。
/// 入力長の上限は `parse_json` が課す（呼び出し側も読み込み前に同じ上限で検査する）。
pub fn parse_raw_rounds(text: &str) -> Result<(Vec<Shape>, Vec<Round>), RlsTimingError> {
    let root = parse_json(text).map_err(|e| RlsTimingError::InvalidRaw(format!("{e:?}")))?;
    if as_uint(field(&root, "schema_version")?, "schema_version")? != 1 {
        return raw_err("unsupported schema_version");
    }
    if str_field(&root, "quantile_method")? != "nearest_rank" {
        return raw_err("unsupported quantile_method");
    }
    let config = field(&root, "config")?;
    let mut shapes: Vec<Shape> = Vec::new();
    for t in array_field(config, "shapes")? {
        let Some(tok) = t.as_str() else {
            return raw_err("config.shapes entry is not a string");
        };
        let shape = shape_of(tok)?;
        if shapes.contains(&shape) {
            return raw_err(format!("duplicate shape {tok:?}"));
        }
        shapes.push(shape);
    }
    if shapes.is_empty() {
        return raw_err("config.shapes is empty");
    }
    let raw_rounds = array_field(&root, "rounds")?;
    if raw_rounds.is_empty() {
        return raw_err("rounds is empty");
    }
    let mut rounds: Vec<Round> = Vec::new();
    for rr in raw_rounds {
        let index = u32::try_from(as_uint(field(rr, "round")?, "round")?)
            .map_err(|_| RlsTimingError::InvalidRaw("round index out of range".into()))?;
        if rounds.iter().any(|r| r.index == index) {
            return raw_err(format!("duplicate round {index}"));
        }
        let mut cells: Vec<Cell> = Vec::new();
        for c in array_field(rr, "cells")? {
            let shape = shape_of(str_field(c, "shape")?)?;
            let arm = arm_of(str_field(c, "arm")?)?;
            if !shapes.contains(&shape) {
                return raw_err("cell shape is not in config.shapes");
            }
            if cells.iter().any(|x| x.shape == shape && x.arm == arm) {
                return raw_err("duplicate cell in a round");
            }
            let mismatches = u32::try_from(as_uint(field(c, "mismatches")?, "mismatches")?)
                .map_err(|_| RlsTimingError::InvalidRaw("mismatches out of range".into()))?;
            if mismatches != 0 {
                return raw_err("cell has mismatched responses");
            }
            let raw_samples = array_field(c, "samples_ns")?;
            if raw_samples.is_empty() {
                return raw_err("samples_ns is empty");
            }
            let mut samples = Vec::new();
            for s in raw_samples {
                samples.push(Duration::from_nanos(as_uint(s, "samples_ns entry")?));
            }
            cells.push(Cell {
                shape,
                arm,
                mismatches,
                samples,
            });
        }
        for &shape in &shapes {
            for arm in ALL_ARMS {
                if !cells.iter().any(|x| x.shape == shape && x.arm == arm) {
                    return raw_err(format!(
                        "round {index} lacks cell shape={} arm={}",
                        shape.token(),
                        arm.token()
                    ));
                }
            }
        }
        rounds.push(Round { index, cells });
    }
    Ok((shapes, rounds))
}
