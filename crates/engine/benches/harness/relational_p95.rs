//! 述語（`OR`／`IN`）・スカラー `ORDER BY`・2 テーブル結合の p95 計測向けの時間非依存ロジック
//! （Issue #1204。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-24・SQL-25・SQL-28、
//! RLS-10）。
//!
//! `relational_p95_bench.rs`（実測本体・時間依存）と `tests/relational_p95_accept.rs`
//! （`make ci` 対象の回帰）の 2 コンパイル単位から `#[path]` で取り込まれる。実測タイマー
//! （`std::time::Instant`）には依存せず、env のパース・SQL 文の組み立て・fixture 値の生成・
//! ラウンド統計・出力行の整形だけを純関数として置く（`scan_stage_profile.rs` と同じ分離方針）。
//!
//! 計測プロトコルは `docs/design/benchmark-judgement-policy.md`（N≥5 ラウンド・arm 輪番・
//! min-of-N と median の併記・共有環境では spec 閾値の確定判定をしない）に従う。
//! SQL 文は定数と検証済みトークンのみから組み立て、未検証文字列を連結しない
//! （`.claude/rules/coding-rust.md`）。出力にテナント ID・行の値・SQL 全文は含めない。
//!
//! `std` と兄弟 harness（`sql_c1`・`accept`）のみに依存する。
//!
//! # 暗号用途禁止
//!
//! 値生成に使う [`mix64`] は非暗号のハッシュであり、ベンチ入力の擬似ランダム化専用。

use std::fmt;
use std::time::Duration;

use super::accept::p95_from_samples;
use super::sql_c1::VectorLiteral;

/// ラウンド数の下限・上限・既定（policy §3 の N≥5）。
pub const MIN_ROUNDS: u32 = 5;
pub const MAX_ROUNDS: u32 = 50;
pub const DEFAULT_ROUNDS: u32 = 5;

/// 行数 env の範囲。上限は spec 規模（既定）と同値で、縮小（スモーク）のみ許す。
pub const MIN_ROWS: usize = 1_000;
pub const MAX_ROWS: usize = 100_000;
pub const DEFAULT_ROWS: usize = 100_000;

/// 結合 fixture の各テーブルの行数（SQL-28 の計測規模に合わせる）。
pub const JOIN_ROWS: usize = 10_000;

/// `lang` 列のカーディナリティ。選択率は `pred_eq`≈1/16・`pred_or2`≈2/16・`pred_in8`≈8/16。
pub const LANG_CARDINALITY: u64 = 16;

/// 各文の `LIMIT`（`MAX_SEARCH_K` 以内）。
pub const PRED_LIMIT: usize = 10;
pub const WIDE_LIMIT: usize = 100;

/// 参照 arm のラベル（述語グループの比率の分母）。
pub const REFERENCE_ARM: &str = "pred_eq";

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationalP95Error {
    InvalidRounds(String),
    InvalidGroup(String),
    InvalidRows(String),
    EmptySamples,
    DegenerateRatio(&'static str),
    InvalidIdentifier(&'static str),
    RefusedUnderGithubActions,
}

impl fmt::Display for RelationalP95Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRounds(r) => write!(f, "invalid BENCH_RELATIONAL_P95_ROUNDS: {r}"),
            Self::InvalidGroup(r) => write!(f, "invalid BENCH_RELATIONAL_P95_GROUP: {r}"),
            Self::InvalidRows(r) => write!(f, "invalid BENCH_RELATIONAL_P95_ROWS: {r}"),
            Self::EmptySamples => write!(f, "empty sample set"),
            Self::DegenerateRatio(r) => write!(f, "degenerate ratio: {r}"),
            Self::InvalidIdentifier(field) => write!(f, "{field} is not a valid identifier"),
            Self::RefusedUnderGithubActions => write!(
                f,
                "relational_p95_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); this bench is manual-only and not wired into any workflow"
            ),
        }
    }
}

impl std::error::Error for RelationalP95Error {}

/// `GITHUB_ACTIONS` 下での実行を拒否する（実測値を public ログへ出さないため）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), RelationalP95Error> {
    if under_github_actions {
        return Err(RelationalP95Error::RefusedUnderGithubActions);
    }
    Ok(())
}

/// `BENCH_RELATIONAL_P95_ROUNDS` を fail-closed にパースする（空・未設定は既定）。
pub fn parse_rounds(raw: Option<&str>) -> Result<u32, RelationalP95Error> {
    let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_ROUNDS);
    };
    let value: u32 = trimmed
        .parse()
        .map_err(|_| RelationalP95Error::InvalidRounds(format!("not an integer: {trimmed:?}")))?;
    if !(MIN_ROUNDS..=MAX_ROUNDS).contains(&value) {
        return Err(RelationalP95Error::InvalidRounds(format!(
            "must be in {MIN_ROUNDS}..={MAX_ROUNDS}, got {value}"
        )));
    }
    Ok(value)
}

/// 計測グループ（1 プロセス 1 グループで回せるようにする。policy §5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Predicate,
    OrderBy,
    Join,
    All,
}

impl Group {
    /// このグループが `other`（単一グループ）を含むか。
    pub fn includes(self, other: Group) -> bool {
        self == Group::All || self == other
    }

    pub fn label(self) -> &'static str {
        match self {
            Group::Predicate => "predicate",
            Group::OrderBy => "order_by",
            Group::Join => "join",
            Group::All => "all",
        }
    }
}

/// `BENCH_RELATIONAL_P95_GROUP` を完全一致でパースする（空・未設定は `all`）。
pub fn parse_group(raw: Option<&str>) -> Result<Group, RelationalP95Error> {
    let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Group::All);
    };
    match trimmed {
        "predicate" => Ok(Group::Predicate),
        "order_by" => Ok(Group::OrderBy),
        "join" => Ok(Group::Join),
        "all" => Ok(Group::All),
        other => Err(RelationalP95Error::InvalidGroup(format!(
            "expected predicate|order_by|join|all, got {other:?}"
        ))),
    }
}

/// `BENCH_RELATIONAL_P95_ROWS` を fail-closed にパースする。縮小のみ許し、
/// 縮小 run は出力で `scale=reduced` と自己ラベルされ記録には使わない。
pub fn parse_rows_scale(raw: Option<&str>) -> Result<usize, RelationalP95Error> {
    let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_ROWS);
    };
    let value: usize = trimmed
        .parse()
        .map_err(|_| RelationalP95Error::InvalidRows(format!("not an integer: {trimmed:?}")))?;
    if !(MIN_ROWS..=MAX_ROWS).contains(&value) {
        return Err(RelationalP95Error::InvalidRows(format!(
            "must be in {MIN_ROWS}..={MAX_ROWS}, got {value}"
        )));
    }
    Ok(value)
}

/// 行数が既定規模かを表すラベル（`full` / `reduced`）。
pub fn scale_label(rows: usize) -> &'static str {
    if rows == DEFAULT_ROWS {
        "full"
    } else {
        "reduced"
    }
}

/// 他テナント（越境検査用）の Private 行数。
pub fn other_tenant_rows(rows: usize) -> usize {
    (rows / 50).max(100)
}

// --- SQL 文の組み立て ---

fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `[A-Za-z0-9_]+`（単一引用符・空白・記号を含められない）。
pub fn is_valid_value_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// テーブル名・列名の識別子検証。
pub fn check_identifier(field: &'static str, name: &str) -> Result<(), RelationalP95Error> {
    if is_valid_identifier(name) {
        Ok(())
    } else {
        Err(RelationalP95Error::InvalidIdentifier(field))
    }
}

/// 述語グループの arm ラベルと文（先頭が参照 arm）。
pub fn predicate_statements(
    table: &str,
    literal: &VectorLiteral,
) -> Result<Vec<(&'static str, String)>, RelationalP95Error> {
    check_identifier("table", table)?;
    let tail = format!("ORDER BY embedding <=> '{literal}' LIMIT {PRED_LIMIT}");
    let in_list = (0..8)
        .map(|i| format!("'{}'", lang_token(i)))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(vec![
        (
            REFERENCE_ARM,
            format!(
                "SELECT id FROM {table} WHERE lang = '{}' {tail}",
                lang_token(0)
            ),
        ),
        (
            "pred_or2",
            format!(
                "SELECT id FROM {table} WHERE lang = '{}' OR lang = '{}' {tail}",
                lang_token(0),
                lang_token(1)
            ),
        ),
        (
            "pred_in8",
            format!("SELECT id FROM {table} WHERE lang IN ({in_list}) {tail}"),
        ),
    ])
}

/// 順序グループの arm ラベルと文。
pub fn order_by_statements(table: &str) -> Result<Vec<(&'static str, String)>, RelationalP95Error> {
    check_identifier("table", table)?;
    Ok(vec![
        (
            "order_single",
            format!("SELECT id, qty FROM {table} ORDER BY qty LIMIT {WIDE_LIMIT}"),
        ),
        (
            "order_multi",
            format!(
                "SELECT id, lang, qty FROM {table} ORDER BY lang ASC, qty DESC LIMIT {WIDE_LIMIT}"
            ),
        ),
    ])
}

/// 結合グループの文（2 テーブル等価結合。`LIMIT` 必須）。
pub fn join_statement(
    left: &str,
    right: &str,
) -> Result<(&'static str, String), RelationalP95Error> {
    check_identifier("left", left)?;
    check_identifier("right", right)?;
    Ok((
        "join_inner",
        format!(
            "SELECT {left}.title, {right}.name FROM {left} JOIN {right} ON {left}.author_id = {right}.id LIMIT {WIDE_LIMIT}"
        ),
    ))
}

// --- fixture の決定的な値生成 ---

/// 非暗号の 64bit 混合（splitmix64 の finalizer）。`qty` を id 順と相関させないためだけに使う。
pub fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// `l0`..`l15` の語彙。
pub fn lang_token(index: u64) -> &'static str {
    const LANGS: [&str; 16] = [
        "l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "l10", "l11", "l12", "l13",
        "l14", "l15",
    ];
    LANGS
        .get((index % LANG_CARDINALITY) as usize)
        .copied()
        .unwrap_or("l0")
}

/// 行 id に対する `lang` 値（`id % 16`）。
pub fn lang_for_id(id: u64) -> &'static str {
    lang_token(id % LANG_CARDINALITY)
}

/// 行 id に対する `qty` 値（id 順と無相関・0..1_000_000）。
pub fn qty_for_id(id: u64) -> i64 {
    (mix64(id) % 1_000_000) as i64
}

/// 文書 id に対する結合キー（右表 id の範囲に全件が一致する）。
pub fn author_id_for_doc(id: u64, authors_rows: u64) -> i64 {
    (id % authors_rows.max(1)) as i64
}

/// `lang` が `pred_*` arm の期待集合（`l0`..`l{n-1}`）に属するか。
pub fn lang_in_first_n(lang: &str, n: u64) -> bool {
    (0..n).any(|i| lang_token(i) == lang)
}

/// 列が非減少か（`descending` なら非増加）。
pub fn is_sorted_by_direction(values: &[i64], descending: bool) -> bool {
    values.windows(2).all(|w| match w {
        [a, b] => {
            if descending {
                a >= b
            } else {
                a <= b
            }
        }
        _ => true,
    })
}

/// 他テナント sentinel 行の `qty`。偶数 id は最小（`order_single` の先頭）、奇数 id は最大
/// （`order_multi` の `l0` 内先頭）になる値で、越境すれば上位へ必ず現れる fixture にする。
pub fn sentinel_qty(id: u64) -> i64 {
    if id.is_multiple_of(2) {
        -1
    } else {
        2_000_000
    }
}

/// 文脈から見える docs 行 `(id, lang, qty)`。自テナント `0..own_rows` と、`other_rows > 0` のとき
/// 他テナント sentinel（`own_rows..own_rows + other_rows`・`lang = l0`・`sentinel_qty`）を含む。
/// 対象テナント文脈は `other_rows = 0`、他テナント文脈（両 visibility 可視）は `other_rows = other` を渡す。
pub fn visible_doc_rows(own_rows: u64, other_rows: u64) -> Vec<(u64, &'static str, i64)> {
    let own = (0..own_rows).map(|id| (id, lang_for_id(id), qty_for_id(id)));
    let other = (own_rows..own_rows + other_rows).map(|id| (id, lang_token(0), sentinel_qty(id)));
    own.chain(other).collect()
}

/// `order_single`（`ORDER BY qty` 昇順・`LIMIT limit`）で可視行 `visible` から期待される先頭 `limit` 件の
/// `qty` 列。`qty` は同値がありうるため、同値境界での id 差を許すよう値列で照合する。
pub fn expected_order_single(visible: &[(u64, &'static str, i64)], limit: usize) -> Vec<i64> {
    let mut qty: Vec<i64> = visible.iter().map(|r| r.2).collect();
    qty.sort_unstable();
    qty.truncate(limit);
    qty
}

/// `order_multi`（`ORDER BY lang ASC, qty DESC`・`LIMIT limit`）で可視行から期待される先頭 `limit` 件の
/// `(lang, qty)` 列。`lang` は文字列（バイト）順。同値タプルの id 差は許すため値の組で照合する。
pub fn expected_order_multi(
    visible: &[(u64, &'static str, i64)],
    limit: usize,
) -> Vec<(&'static str, i64)> {
    let mut rows: Vec<(&'static str, i64)> = visible.iter().map(|r| (r.1, r.2)).collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0).then(b.1.cmp(&a.1)));
    rows.truncate(limit);
    rows
}

/// `ids` が `expected_sorted`（昇順・重複なし）と過不足なく一致するか。件数だけの照合では
/// 重複と欠落が同時に起きても通過するため、整列して要素ごとに照合する。
pub fn is_exact_id_set(ids: &[u64], expected_sorted: &[u64]) -> bool {
    let mut got = ids.to_vec();
    got.sort_unstable();
    got == expected_sorted
}

/// 検索カーネルのスコア（内積。大きいほど近い。`kernel.rs` の `dot`）に対応する距離 `-dot(a, b)`。
/// `ORDER BY embedding <=> q` は正規化なしの内積で並ぶため、期待順序を fixture から独立に求める
/// 参照実装は f64 の負の内積とする（コサイン距離だと順位が入れ替わる）。
pub fn inner_product_distance(a: &[f32], b: &[f32]) -> f64 {
    -a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum::<f64>()
}

/// sentinel ベクトルのクエリ方向の係数。sentinel = `query * factor` の内積は `factor * |q|^2` で、
/// fixture 全行の内積の最大値 `max_fixture_dot` の 2 倍を超える（順位は正規化しない内積で決まるため、
/// 「クエリと同一ベクトル」では上位に入る保証にならない）。呼び出し側は生成時にさらに
/// `sentinel_dominates` で保証を assert する。
pub fn sentinel_scale(max_fixture_dot: f64, query_norm_sq: f64) -> f32 {
    if query_norm_sq <= 0.0 {
        return 2.0;
    }
    let factor = (2.0 * max_fixture_dot.max(0.0) / query_norm_sq).ceil() + 2.0;
    factor.min(f64::from(f32::MAX)) as f32
}

/// sentinel の内積が fixture 全行の内積の最大値を厳密に上回るか。
pub fn sentinel_dominates(sentinel_dot: f64, max_fixture_dot: f64) -> bool {
    sentinel_dot > max_fixture_dot
}

/// 距離の同値境界を許容する幅（エンジンの f32 内積と f64 参照実装の累積誤差を吸収する）。
pub const DISTANCE_EPS: f64 = 1e-3;

/// 述語 arm（`ORDER BY embedding <=> q LIMIT k`）の返却 id 列が期待どおりかを照合する。
/// `ranked` は述語を満たす全自テナント行の `(id, 距離)` を距離昇順に並べたもの。
/// 件数が `min(k, ranked.len())`・id が重複しない・返却行が上位 k の距離内（境界は `DISTANCE_EPS`）・
/// 境界より明確に近い行の取りこぼしが無い・返却順が距離の昇順、をすべて満たすときだけ true。
pub fn topk_matches(ids: &[u64], ranked: &[(u64, f64)], k: usize) -> bool {
    let want = k.min(ranked.len());
    if ids.len() != want || want == 0 {
        return want == 0 && ids.is_empty();
    }
    let dist: std::collections::HashMap<u64, f64> = ranked.iter().copied().collect();
    let mut seen = std::collections::HashSet::new();
    let Some(cutoff) = ranked.get(want - 1).map(|r| r.1) else {
        return false;
    };
    let mut prev = f64::NEG_INFINITY;
    for id in ids {
        let Some(d) = dist.get(id).copied() else {
            return false;
        };
        if !seen.insert(*id) || d > cutoff + DISTANCE_EPS || d + DISTANCE_EPS < prev {
            return false;
        }
        prev = prev.max(d);
    }
    ranked
        .iter()
        .take_while(|r| r.1 < cutoff - DISTANCE_EPS)
        .all(|r| seen.contains(&r.0))
}

// --- 統計 ---

/// 1 arm の全ラウンド要約。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoundSummary {
    pub min: Duration,
    pub median: Duration,
    pub max: Duration,
    /// `(max - min) / min` を百分率で表したラン間の幅。
    pub run_to_run_band_pct: f64,
}

/// ラウンドごとの p95 列から min-of-N・median・max・ラン間幅を求める。
pub fn summarize_rounds(per_round_p95: &[Duration]) -> Result<RoundSummary, RelationalP95Error> {
    let min = per_round_p95
        .iter()
        .copied()
        .min()
        .ok_or(RelationalP95Error::EmptySamples)?;
    let max = per_round_p95
        .iter()
        .copied()
        .max()
        .ok_or(RelationalP95Error::EmptySamples)?;
    if min.is_zero() {
        return Err(RelationalP95Error::DegenerateRatio(
            "min of round p95 is zero",
        ));
    }
    let mut sorted = per_round_p95.to_vec();
    sorted.sort();
    let mid = sorted.len() / 2;
    let median = if sorted.len().is_multiple_of(2) {
        match (sorted.get(mid.wrapping_sub(1)), sorted.get(mid)) {
            (Some(a), Some(b)) => (*a + *b) / 2,
            _ => return Err(RelationalP95Error::EmptySamples),
        }
    } else {
        sorted
            .get(mid)
            .copied()
            .ok_or(RelationalP95Error::EmptySamples)?
    };
    let band = (max.as_secs_f64() - min.as_secs_f64()) / min.as_secs_f64() * 100.0;
    Ok(RoundSummary {
        min,
        median,
        max,
        run_to_run_band_pct: band,
    })
}

/// 1 ラウンド分の生サンプルから p95 を取る（`accept::p95_from_samples` の委譲）。
pub fn round_p95(samples: &[Duration]) -> Result<Duration, RelationalP95Error> {
    p95_from_samples(samples).map_err(|_| RelationalP95Error::EmptySamples)
}

/// arm の min-of-N と参照 arm の min-of-N の比。分母 0 は拒否。
pub fn ratio_vs_reference(arm_min: Duration, ref_min: Duration) -> Result<f64, RelationalP95Error> {
    if ref_min.is_zero() {
        return Err(RelationalP95Error::DegenerateRatio("reference min is zero"));
    }
    Ok(arm_min.as_secs_f64() / ref_min.as_secs_f64())
}

/// ラウンドごとに開始位置をずらした arm の実行順（輪番。policy §3）。
pub fn rotate_arms(round_index: usize, n_arms: usize) -> Vec<usize> {
    if n_arms == 0 {
        return Vec::new();
    }
    (0..n_arms).map(|i| (i + round_index) % n_arms).collect()
}

/// 参照 arm を各候補の直前に挟む実行順（policy §3 の `baseline/cand1/baseline/cand2/...` 輪番）。
///
/// 候補の並びだけをラウンドごとに回転させ、各候補の直前に必ず参照 arm を置く。返すのは
/// arm 添字列で、参照 arm 添字は/// 候補ごとに 1 回ずつ現れる。参照 arm が無い場合は `rotate_arms` と同じ扱いにする。
pub fn interleave_with_reference(
    round_index: usize,
    n_arms: usize,
    reference: Option<usize>,
) -> Vec<usize> {
    let Some(r) = reference.filter(|r| *r < n_arms) else {
        return rotate_arms(round_index, n_arms);
    };
    let cands: Vec<usize> = (0..n_arms).filter(|i| *i != r).collect();
    if cands.is_empty() {
        return vec![r];
    }
    let n = cands.len();
    let mut order = Vec::with_capacity(n * 2);
    for i in 0..n {
        order.push(r);
        if let Some(c) = cands.get((i + round_index) % n) {
            order.push(*c);
        }
    }
    order
}

// --- 出力行（英語。テナント ID・行値・SQL 全文は含めない） ---

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

pub fn render_round_line(
    group: &str,
    arm: &str,
    round: usize,
    p95: Duration,
    median: Duration,
    loadavg: &str,
) -> String {
    format!(
        "relational_p95: group={group} arm={arm} round={round} p95={:.3}ms median={:.3}ms loadavg={loadavg}",
        ms(p95),
        ms(median)
    )
}

pub fn render_summary_line(
    group: &str,
    arm: &str,
    rows: usize,
    scale: &str,
    summary: &RoundSummary,
    ratio: Option<(f64, Duration)>,
) -> String {
    // 比率の分母（その候補の直前に測った参照 arm の min）を併記し、表示値から比率を再現できるようにする。
    // 参照 arm 自身の `min_of_n` はラウンドごとに 1 回だけの系列で、分母とは別の系列（対応付け集計）。
    let ratio_part = match ratio {
        Some((r, ref_min)) => format!(
            " ratio_vs_{REFERENCE_ARM}={r:.3} ref_min_paired={:.3}ms",
            ms(ref_min)
        ),
        None => String::new(),
    };
    format!(
        "relational_p95: group={group} arm={arm} rows={rows} scale={} min_of_n={:.3}ms median={:.3}ms max={:.3}ms run_to_run_band={:.1}%{ratio_part}",
        scale,
        ms(summary.min),
        ms(summary.median),
        ms(summary.max),
        summary.run_to_run_band_pct
    )
}

pub fn render_threshold_line(dedicated: bool) -> String {
    if dedicated {
        "threshold_judgement: dedicated environment attested; compare min_of_n against the spec criteria manually".to_string()
    } else {
        "threshold_judgement: not evaluated (shared environment; reference values only)".to_string()
    }
}
