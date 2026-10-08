//! チップ別手動計測オーケストレータ（`benches/chip_bench.rs`）が使う純関数群
//! （Issue #469・親 #456。`docs/design/ann-index-adoption.md` 系ポインタ:
//! CORE-9／CORE-10／CORE-16／TASK-132）。
//!
//! 本開発環境（QEMU 仮想 CPU・AVX-512 なし・NEON なし・非現実的なキャッシュ
//! 階層。`docs/design/chip-kernel-guidelines.md` §0.6）では Phase 4（チップ最適
//! カーネル）の採否判定に必要な実測ができないため、オーナー実機
//! （Apple M／AMD Zen 4・5／Intel）での手動計測（`make bench-tier` と同じ運用。
//! Issue #313）を 1 コマンドで回せるようにする。本モジュールは env パース・
//! 子プロセス出力の行パーサ・最小 JSON パーサ・集計・環境情報パーサ・JSON 生成の
//! 純関数のみを提供し、`Command` 起動・ファイル I/O は呼び出し元
//! （`chip_bench.rs`）が担う（`harness::dot_kernel`・`harness::bench_engine` と
//! 同じ「純関数 / 実行本体」の責務分離）。
//!
//! `super::` を参照しない。単体テストは本ファイルへインラインで置かない
//! （bench ターゲット〔`harness = false`〕のコンパイルで `#[cfg(test)]` ブロック
//! 自体はコンパイルされ `use super::*` が unused import になる。
//! `bench_engine.rs` 冒頭コメント参照）。テストは `tests/chip_bench_accept.rs`
//! に集約する。

use std::fmt;

// ---------------------------------------------------------------------
// エラー型
// ---------------------------------------------------------------------

/// 本モジュールのエラー型。呼び出し元は `Display` をそのまま `eprintln!` へ渡し
/// 非ゼロ終了する（fail-closed の入口。`bench_engine.rs::BenchEngineError` と
/// 同型）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChipError {
    RefusedUnderGitHubActions,
    InvalidEnv { name: &'static str, message: String },
    ParseFailure(String),
    Empty(String),
    MismatchedKeys(String),
}

impl fmt::Display for ChipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChipError::RefusedUnderGitHubActions => write!(
                f,
                "chip_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); \
                 this bench is manual-only and not wired into any workflow"
            ),
            ChipError::InvalidEnv { name, message } => write!(f, "{name}: {message}"),
            ChipError::ParseFailure(msg) => write!(f, "parse failure: {msg}"),
            ChipError::Empty(msg) => write!(f, "empty result: {msg}"),
            ChipError::MismatchedKeys(msg) => write!(f, "mismatched metric keys: {msg}"),
        }
    }
}

impl std::error::Error for ChipError {}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`dot_kernel.rs::refuse_under_github_actions`
/// と同一パターン）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), ChipError> {
    if under_github_actions {
        return Err(ChipError::RefusedUnderGitHubActions);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// env パース（fail-closed）
// ---------------------------------------------------------------------

/// `BENCH_CHIP_ROUNDS` が満たすべき下限（受け入れ条件のスモーク実行用途）。
pub const MIN_ROUNDS: u32 = 1;
/// `BENCH_CHIP_ROUNDS` の上限（無制限反復による長時間占有を防ぐ）。
pub const MAX_ROUNDS: u32 = 50;
/// 未設定時の既定ラウンド数。
pub const DEFAULT_ROUNDS: u32 = 5;
/// `docs/design/benchmark-judgement-policy.md` が要求する交互ペア数の最小値。
/// `rounds` がこれを下回る計測は「参考値」であることを summary.json 側で
/// 自己ラベルする（`meets_policy_min_rounds`）。
pub const POLICY_MIN_ROUNDS: u32 = 5;

/// `BENCH_CHIP_ROUNDS` を解決する（未設定・空 → [`DEFAULT_ROUNDS`]、
/// `MIN_ROUNDS..=MAX_ROUNDS` の正整数のみ受理）。
pub fn parse_rounds(raw: Option<&str>) -> Result<u32, ChipError> {
    let trimmed = raw.map(str::trim);
    let value: u32 = match trimmed {
        None | Some("") => return Ok(DEFAULT_ROUNDS),
        Some(s) => s.parse::<u32>().map_err(|_| ChipError::InvalidEnv {
            name: "BENCH_CHIP_ROUNDS",
            message: format!("must be a positive integer (got {s:?})"),
        })?,
    };
    if !(MIN_ROUNDS..=MAX_ROUNDS).contains(&value) {
        return Err(ChipError::InvalidEnv {
            name: "BENCH_CHIP_ROUNDS",
            message: format!("must be in {MIN_ROUNDS}..={MAX_ROUNDS} (got {value})"),
        });
    }
    Ok(value)
}

/// 1 ワークロード = 1 子プロセスとして起動する対象（固定順が既定集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Workload {
    DotKernel,
    KnnProfile,
    Feature128,
    Feature768,
}

/// 既定のワークロード集合（`BENCH_CHIP_WORKLOADS` 未設定時。固定順）。
pub const DEFAULT_WORKLOADS: [Workload; 4] = [
    Workload::DotKernel,
    Workload::KnnProfile,
    Workload::Feature128,
    Workload::Feature768,
];

impl Workload {
    /// `BENCH_CHIP_WORKLOADS` の要素・summary.json のキーに使うトークン。
    pub fn token(self) -> &'static str {
        match self {
            Workload::DotKernel => "dot_kernel",
            Workload::KnnProfile => "knn_profile",
            Workload::Feature128 => "feature_128",
            Workload::Feature768 => "feature_768",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token {
            "dot_kernel" => Some(Workload::DotKernel),
            "knn_profile" => Some(Workload::KnnProfile),
            "feature_128" => Some(Workload::Feature128),
            "feature_768" => Some(Workload::Feature768),
            _ => None,
        }
    }

    /// 子プロセスの `cargo` へ渡す argv（固定値のみ・シェル不使用）。
    /// `chip_bench.rs::run_workload` から呼ばれる。`package` は呼び出し側が
    /// `env!("CARGO_PKG_NAME")` で渡す（パッケージ名を直書きすると改名時に
    /// `package ID specification` 不一致で起動に失敗するため。Issue #1482）。
    pub fn cargo_args(self, package: &str) -> Vec<String> {
        let parts: Vec<&str> = match self {
            Workload::DotKernel => vec!["bench", "--bench", "dot_kernel_bench", "-p", package],
            Workload::KnnProfile => vec!["bench", "--bench", "knn_profile_bench", "-p", package],
            Workload::Feature128 | Workload::Feature768 => {
                vec![
                    "run",
                    "--release",
                    "-p",
                    package,
                    "--example",
                    "feature_bench",
                ]
            }
        };
        parts.into_iter().map(String::from).collect()
    }

    /// このワークロードが子プロセスへ追加で設定する env（`feature_bench` の
    /// `BENCH_FEATURE_DIM`。dim 以外の `BENCH_*` 系 env は親環境から継承する
    /// 契約——README 参照）。
    pub fn extra_env(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Workload::Feature128 => &[("BENCH_FEATURE_DIM", "128")],
            Workload::Feature768 => &[("BENCH_FEATURE_DIM", "768")],
            Workload::DotKernel | Workload::KnnProfile => &[],
        }
    }
}

/// `BENCH_CHIP_WORKLOADS` を解決する（未設定・空 → [`DEFAULT_WORKLOADS`]、
/// カンマ区切り・前後空白許容・未知トークン／重複／空要素は `Err`）。
pub fn parse_workloads(raw: Option<&str>) -> Result<Vec<Workload>, ChipError> {
    let trimmed = raw.map(str::trim);
    match trimmed {
        None | Some("") => Ok(DEFAULT_WORKLOADS.to_vec()),
        Some(s) => {
            let mut seen: Vec<Workload> = Vec::new();
            for part in s.split(',') {
                let token = part.trim();
                if token.is_empty() {
                    return Err(ChipError::InvalidEnv {
                        name: "BENCH_CHIP_WORKLOADS",
                        message: "contains an empty element".to_string(),
                    });
                }
                let workload =
                    Workload::from_token(token).ok_or_else(|| ChipError::InvalidEnv {
                        name: "BENCH_CHIP_WORKLOADS",
                        message: format!("unknown workload {token:?}"),
                    })?;
                if seen.contains(&workload) {
                    return Err(ChipError::InvalidEnv {
                        name: "BENCH_CHIP_WORKLOADS",
                        message: format!("duplicate workload {token:?}"),
                    });
                }
                seen.push(workload);
            }
            Ok(seen)
        }
    }
}

/// `BENCH_DEDICATED_ENV` の専有環境自己申告判定（`sql_c1_bench.rs`・
/// `scan_stage_profile_bench.rs` と同一イディオム: `trim() == "1"` のみ true）。
pub fn dedicated_env_attested(raw: Option<&str>) -> bool {
    raw.map(str::trim) == Some("1")
}

// ---------------------------------------------------------------------
// 子プロセス出力の行パーサ
// ---------------------------------------------------------------------

/// `dot_kernel_bench` の実測行（`label=current` のみ）。
#[derive(Debug, Clone, PartialEq)]
pub struct DotKernelSample {
    pub working_set: String,
    pub dim: u32,
    pub rows: u64,
    pub median_ms: f64,
    pub ns_per_dot: f64,
}

fn extract_kv(line: &str, key: &str) -> Option<String> {
    for token in line.split_whitespace() {
        if let Some(rest) = token.strip_prefix(key) {
            return Some(rest.to_string());
        }
    }
    None
}

/// `dot_kernel: label=current working_set=<ws> dim=<n> rows=<n> median_ms=<f> ns_per_dot=<f>`
/// を読む（`harness::dot_kernel::render_line` の出力形式）。`label=current` 以外・
/// 形式不一致は `None`（未知行を黙って無視する契約。子が出す他の行——`env:` や
/// `diagnostic_ab` 等——を誤って拾わない）。
pub fn parse_dot_kernel_line(line: &str) -> Option<DotKernelSample> {
    if !line.starts_with("dot_kernel: label=current ") {
        return None;
    }
    let working_set = extract_kv(line, "working_set=")?;
    let dim: u32 = extract_kv(line, "dim=")?.parse().ok()?;
    let rows: u64 = extract_kv(line, "rows=")?.parse().ok()?;
    let median_ms: f64 = extract_kv(line, "median_ms=")?.parse().ok()?;
    let ns_per_dot: f64 = extract_kv(line, "ns_per_dot=")?.parse().ok()?;
    Some(DotKernelSample {
        working_set,
        dim,
        rows,
        median_ms,
        ns_per_dot,
    })
}

/// `dot_kernel: diagnostic_ab dim=<n> rows=<n> simd_vs_scalar_ratio=<f> class=<c>` を読む。
#[derive(Debug, Clone, PartialEq)]
pub struct DotKernelDiag {
    pub dim: u32,
    pub rows: u64,
    pub simd_vs_scalar_ratio: f64,
    pub class: String,
}

pub fn parse_dot_kernel_diag_line(line: &str) -> Option<DotKernelDiag> {
    if !line.starts_with("dot_kernel: diagnostic_ab ") {
        return None;
    }
    let dim: u32 = extract_kv(line, "dim=")?.parse().ok()?;
    let rows: u64 = extract_kv(line, "rows=")?.parse().ok()?;
    let simd_vs_scalar_ratio: f64 = extract_kv(line, "simd_vs_scalar_ratio=")?.parse().ok()?;
    let class = extract_kv(line, "class=")?;
    Some(DotKernelDiag {
        dim,
        rows,
        simd_vs_scalar_ratio,
        class,
    })
}

/// `knn_profile_bench` の段別実測行。
#[derive(Debug, Clone, PartialEq)]
pub struct KnnStageSample {
    pub name: String,
    pub rows: u64,
    pub median_ms: f64,
    pub ns_per_row: f64,
}

/// `stage(<name>): rows=<n> median=<f>ms ns_per_row=<f>` を読む
/// （`harness::knn_profile::render_stage_line` の出力形式。`median=` の値側に
/// `ms` 接尾辞が付く点が `dot_kernel` の `median_ms=` とキー形式が異なるため
/// 専用パーサとする）。`diff(...)`／`residual(...)` 行は `None`。
pub fn parse_knn_stage_line(line: &str) -> Option<KnnStageSample> {
    let rest = line.strip_prefix("stage(")?;
    let (name, rest) = rest.split_once("): ")?;
    let rows: u64 = extract_kv(rest, "rows=")?.parse().ok()?;
    let median_raw = extract_kv(rest, "median=")?;
    let median_str = median_raw.strip_suffix("ms")?;
    let median_ms: f64 = median_str.parse().ok()?;
    let ns_per_row: f64 = extract_kv(rest, "ns_per_row=")?.parse().ok()?;
    Some(KnnStageSample {
        name: name.to_string(),
        rows,
        median_ms,
        ns_per_row,
    })
}

// ---------------------------------------------------------------------
// 最小 JSON パーサ（`feature_bench` の stdout 最終行を読むためだけの用途）
// ---------------------------------------------------------------------

/// JSON パーサへの入力長上限（無制限確保防止。feature_bench の 13 フェーズ出力は
/// 数 KiB 程度のため十分な余裕を持たせる）。
pub const JSON_MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
/// JSON パーサの再帰深さ上限（スタック溢れ防止）。
pub const JSON_MAX_DEPTH: usize = 64;

/// 最小 JSON 値。`feature_bench` の出力（object／array／string／number／
/// true／false／null）のみを対象とする。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    pub fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// object から `key` を引く（浅い探索。ネストは呼び出し元が再帰的に辿る）。
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        self.as_object()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> JsonParser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), ChipError> {
        if self.bump() == Some(b) {
            Ok(())
        } else {
            Err(ChipError::ParseFailure(format!(
                "expected {:?} at byte {}",
                b as char, self.pos
            )))
        }
    }

    fn parse_value(&mut self) -> Result<JsonValue, ChipError> {
        self.skip_ws();
        self.depth += 1;
        if self.depth > JSON_MAX_DEPTH {
            return Err(ChipError::ParseFailure(
                "max recursion depth exceeded".to_string(),
            ));
        }
        let result = match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => self.parse_literal("true", JsonValue::Bool(true)),
            Some(b'f') => self.parse_literal("false", JsonValue::Bool(false)),
            Some(b'n') => self.parse_literal("null", JsonValue::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(ChipError::ParseFailure(format!(
                "unexpected byte at {}",
                self.pos
            ))),
        };
        self.depth -= 1;
        result
    }

    fn parse_literal(&mut self, lit: &str, value: JsonValue) -> Result<JsonValue, ChipError> {
        let end = self.pos + lit.len();
        if end <= self.bytes.len() && &self.bytes[self.pos..end] == lit.as_bytes() {
            self.pos = end;
            Ok(value)
        } else {
            Err(ChipError::ParseFailure(format!(
                "expected literal {lit:?} at byte {}",
                self.pos
            )))
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, ChipError> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => {
                    return Err(ChipError::ParseFailure(format!(
                        "expected ',' or '}}' at byte {}",
                        self.pos
                    )))
                }
            }
        }
        Ok(JsonValue::Object(entries))
    }

    fn parse_array(&mut self) -> Result<JsonValue, ChipError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => {
                    return Err(ChipError::ParseFailure(format!(
                        "expected ',' or ']' at byte {}",
                        self.pos
                    )))
                }
            }
        }
        Ok(JsonValue::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, ChipError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            // エスケープなしの区間はバイト単位ではなく UTF-8 文字列スライスとして
            // まとめて追記する（`"`／`\\` は ASCII のみが取り得るバイト値のため、
            // 直前の区間の開始位置から現在位置までは常にコードポイント境界に
            // 揃っており、有効な UTF-8 部分文字列として安全に切り出せる）。
            let run_start = self.pos;
            loop {
                match self.peek() {
                    Some(b'"') | Some(b'\\') | None => break,
                    Some(_) => self.pos += 1,
                }
            }
            if self.pos > run_start {
                let slice = std::str::from_utf8(&self.bytes[run_start..self.pos])
                    .map_err(|_| ChipError::ParseFailure("invalid utf-8 in string".to_string()))?;
                out.push_str(slice);
            }
            let b = self
                .bump()
                .ok_or_else(|| ChipError::ParseFailure("unterminated string".to_string()))?;
            match b {
                b'"' => break,
                b'\\' => {
                    let esc = self.bump().ok_or_else(|| {
                        ChipError::ParseFailure("unterminated escape".to_string())
                    })?;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'u' => {
                            if self.pos + 4 > self.bytes.len() {
                                return Err(ChipError::ParseFailure(
                                    "truncated \\u escape".to_string(),
                                ));
                            }
                            let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
                                .map_err(|_| {
                                    ChipError::ParseFailure("invalid \\u escape".to_string())
                                })?;
                            let code = u32::from_str_radix(hex, 16).map_err(|_| {
                                ChipError::ParseFailure("invalid \\u escape".to_string())
                            })?;
                            self.pos += 4;
                            let ch = char::from_u32(code).ok_or_else(|| {
                                ChipError::ParseFailure("invalid \\u code point".to_string())
                            })?;
                            out.push(ch);
                        }
                        other => {
                            return Err(ChipError::ParseFailure(format!(
                                "unsupported escape {:?}",
                                other as char
                            )))
                        }
                    }
                }
                _ => unreachable!("run loop only stops at '\"', '\\\\', or end of input"),
            }
        }
        Ok(out)
    }

    fn parse_number(&mut self) -> Result<JsonValue, ChipError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| ChipError::ParseFailure("invalid number bytes".to_string()))?;
        text.parse::<f64>()
            .map(JsonValue::Number)
            .map_err(|_| ChipError::ParseFailure(format!("invalid number {text:?}")))
    }
}

/// `text` を JSON としてパースする（object／array／string／number／true／
/// false／null のみ。入力長・再帰深さに上限を設ける。無効な JSON・空入力・
/// 末尾の余分なトークンはすべて `Err`）。
pub fn parse_json(text: &str) -> Result<JsonValue, ChipError> {
    if text.len() > JSON_MAX_INPUT_BYTES {
        return Err(ChipError::ParseFailure(format!(
            "input exceeds {JSON_MAX_INPUT_BYTES} bytes"
        )));
    }
    let mut parser = JsonParser::new(text.as_bytes());
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(ChipError::ParseFailure(
            "trailing data after top-level value".to_string(),
        ));
    }
    Ok(value)
}

// ---------------------------------------------------------------------
// feature_bench 出力の解釈
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FeaturePhase {
    pub name: String,
    pub min_us: f64,
    pub p50_us: f64,
    pub p95_us: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeatureBenchResult {
    pub dim: u32,
    pub engine: String,
    pub scale: u64,
    pub rows_total: u64,
    pub phases: Vec<FeaturePhase>,
}

/// `feature_bench` の stdout 最終行（`{"meta":...,"phases":[...]}` で始まる行）
/// を読む。`meta.dim` が `expected_dim` と一致しない場合は `Err`
/// （fail-closed。dim 上書き env が子へ渡っていないことの検出）。
pub fn parse_feature_bench_output(
    stdout: &str,
    expected_dim: u32,
) -> Result<FeatureBenchResult, ChipError> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("{\"meta\":"))
        .ok_or_else(|| {
            ChipError::ParseFailure("no {\"meta\":...} line found in stdout".to_string())
        })?;
    let value = parse_json(line.trim())?;
    let meta = value
        .get("meta")
        .ok_or_else(|| ChipError::ParseFailure("missing meta object".to_string()))?;
    let dim =
        meta.get("dim")
            .and_then(JsonValue::as_f64)
            .ok_or_else(|| ChipError::ParseFailure("missing meta.dim".to_string()))? as u32;
    if dim != expected_dim {
        return Err(ChipError::ParseFailure(format!(
            "meta.dim mismatch: expected {expected_dim}, got {dim}"
        )));
    }
    let engine = meta
        .get("engine")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();
    let scale = meta.get("scale").and_then(JsonValue::as_f64).unwrap_or(0.0) as u64;
    let rows_total = meta
        .get("rows_total")
        .and_then(JsonValue::as_f64)
        .unwrap_or(0.0) as u64;
    let phases_json = value
        .get("phases")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| ChipError::ParseFailure("missing phases array".to_string()))?;
    let mut phases = Vec::with_capacity(phases_json.len());
    for p in phases_json {
        let name = p
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| ChipError::ParseFailure("phase missing name".to_string()))?
            .to_string();
        let min_us = p
            .get("min_us")
            .and_then(JsonValue::as_f64)
            .ok_or_else(|| ChipError::ParseFailure(format!("phase {name} missing min_us")))?;
        let p50_us = p
            .get("p50_us")
            .and_then(JsonValue::as_f64)
            .ok_or_else(|| ChipError::ParseFailure(format!("phase {name} missing p50_us")))?;
        let p95_us = p
            .get("p95_us")
            .and_then(JsonValue::as_f64)
            .ok_or_else(|| ChipError::ParseFailure(format!("phase {name} missing p95_us")))?;
        phases.push(FeaturePhase {
            name,
            min_us,
            p50_us,
            p95_us,
        });
    }
    if phases.is_empty() {
        return Err(ChipError::Empty("phases array is empty".to_string()));
    }
    Ok(FeatureBenchResult {
        dim,
        engine,
        scale,
        rows_total,
        phases,
    })
}

// ---------------------------------------------------------------------
// 集計
// ---------------------------------------------------------------------

/// 1 メトリクス（ラウンドをまたいで集めた値列）の集計結果。
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSeries {
    pub values: Vec<f64>,
    pub min: f64,
    pub median: f64,
    pub max: f64,
    /// `(max - min) / min * 100`（`docs/design/benchmark-judgement-policy.md`
    /// §4 の参照区間帯の式と同名で記録する）。`min` が 0 の場合はゼロ除算で
    /// 算出不能なため `None`（ばらつきが実際に 0 な `Some(0.0)` とは区別する。
    /// codex-review 指摘: PR #560）。
    pub reference_band_pct: Option<f64>,
}

/// `values`（空でない前提。空は `Err`）から [`MetricSeries`] を作る。
pub fn aggregate(values: &[f64]) -> Result<MetricSeries, ChipError> {
    if values.is_empty() {
        return Err(ChipError::Empty(
            "cannot aggregate an empty series".to_string(),
        ));
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    let min = sorted[0];
    let max = sorted[n - 1];
    let reference_band_pct = if min != 0.0 {
        Some((max - min) / min * 100.0)
    } else {
        None
    };
    Ok(MetricSeries {
        values: values.to_vec(),
        min,
        median,
        max,
        reference_band_pct,
    })
}

// ---------------------------------------------------------------------
// 環境情報パーサ（best-effort。読み取り失敗は空値へ落とす）
// ---------------------------------------------------------------------

/// `/proc/cpuinfo` から拾う x86 関心フラグ（固定リスト。Issue #313・#365 系の
/// 判別変数）。
pub const X86_INTEREST_FLAGS: &[&str] = &[
    "sse4_2",
    "avx",
    "avx2",
    "fma",
    "f16c",
    "avx512f",
    "avx512bw",
    "avx512vl",
    "avx512vnni",
    "avx512_bf16",
    "avx512_fp16",
    "avx_vnni",
    "amx_tile",
    "amx_bf16",
    "amx_int8",
];

/// `/proc/cpuinfo`（aarch64）から拾う関心フラグ。
pub const AARCH64_INTEREST_FLAGS: &[&str] = &[
    "asimd", "fphp", "asimdhp", "asimddp", "bf16", "i8mm", "sve", "sve2", "sme",
];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CpuInfo {
    pub model_name: Option<String>,
    pub flags: Vec<String>,
}

/// `/proc/cpuinfo` のテキストから `model name`・`flags`／`Features` 行を読む。
/// `interest`（呼び出し元が arch に応じて [`X86_INTEREST_FLAGS`]／
/// [`AARCH64_INTEREST_FLAGS`] を渡す）に含まれるフラグのみを残す
/// （関心リスト外のノイズを summary.json に持ち込まない）。
pub fn parse_proc_cpuinfo(text: &str, interest: &[&str]) -> CpuInfo {
    let mut model_name = None;
    let mut flags = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if model_name.is_none() && (key == "model name" || key == "Model" || key == "Hardware") {
            model_name = Some(value.to_string());
        }
        if key == "flags" || key == "Features" {
            for tok in value.split_whitespace() {
                if interest.contains(&tok) && !flags.contains(&tok.to_string()) {
                    flags.push(tok.to_string());
                }
            }
        }
    }
    CpuInfo { model_name, flags }
}

/// macOS `sysctl <keys...>` の `key: value` 出力行を読む。
pub fn parse_sysctl_lines(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            out.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    out
}

/// `/sys/devices/system/cpu/cpu0/cache/index*/size` の `"48M"`／`"32K"` 形式を
/// バイト数へ変換する。不正な形式は `None`（best-effort。呼び出し元は失敗を
/// 「不明」として無視してよい）。
pub fn parse_cache_size(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if let Some(digits) = trimmed.strip_suffix('K') {
        digits.parse::<u64>().ok().map(|v| v * 1024)
    } else if let Some(digits) = trimmed.strip_suffix('M') {
        digits.parse::<u64>().ok().map(|v| v * 1024 * 1024)
    } else if let Some(digits) = trimmed.strip_suffix('G') {
        digits.parse::<u64>().ok().map(|v| v * 1024 * 1024 * 1024)
    } else {
        trimmed.parse::<u64>().ok()
    }
}

// ---------------------------------------------------------------------
// JSON 生成
// ---------------------------------------------------------------------

/// 文字列を JSON 文字列リテラルの中身へエスケープする（`"`・`\`・制御文字。
/// cpuinfo のモデル名は `(`・`@` 等を含むが JSON 上はエスケープ不要）。
pub fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `f64` を JSON 数値として整形する（NaN／Inf は `null` に落とす。JSON は
/// NaN/Inf を表現できないため）。
pub fn json_number(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.6}")
    } else {
        "null".to_string()
    }
}

/// `Option<f64>` を JSON 数値として整形する（`None` は `null`。`min` が 0 で
/// `reference_band_pct` が算出不能な場合に使う）。
pub fn json_number_opt(value: Option<f64>) -> String {
    match value {
        Some(v) => json_number(v),
        None => "null".to_string(),
    }
}
