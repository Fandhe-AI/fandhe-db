//! 衝突有無で応答時間分布を比較するベンチ `rls_timing_bench.rs` の時間非依存ロジック
//! （Issue #2079。親 #2078。ポインタ: `docs/spec/04-behavior/` RLS-9・RLS-10）。
//!
//! 役割: 他テナントにだけ親行・UNIQUE 値がある状態（flooded）とどこにも無い状態（baseline）で、
//! 同一の書き込み文の応答時間分布が区別できるかを測るための、env の fail-closed パース・形状定義・
//! seed 行の真実値・SQL 事前生成・応答の正規化と照合・分位点・生サンプル入り JSON の生成を提供する。
//! 呼び出し元は `rls_timing_bench.rs`（実測）と `tests/rls_timing_accept.rs`（`cargo test` での
//! 回帰検証）。`engine` には依存せず、エンジン呼び出し（seed 投入・SQL 実行）はベンチ側が担う。
//!
//! 判定基準（分位点差の上限）・実測値の記録・CI 配線は本モジュールの責務外（Issue #2080）。
//! spec 由来の閾値は一切持たず、pass/fail 判定もしない。

use std::fmt;
use std::time::Duration;

use super::chip::json_escape;
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
}

impl fmt::Display for RlsTimingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRounds(m) => write!(f, "invalid BENCH_RLS_TIMING_ROUNDS: {m}"),
            Self::InvalidIters(m) => write!(f, "invalid BENCH_RLS_TIMING_ITERS: {m}"),
            Self::InvalidShapes(m) => write!(f, "invalid BENCH_RLS_TIMING_SHAPES: {m}"),
            Self::EmptySamples => write!(f, "empty sample set"),
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
