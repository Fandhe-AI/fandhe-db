//! 疎検索（BM25 Okapi）モジュール（TASK-102、対象ビヘイビア: SEARCH-1, SEARCH-3。
//! ポインタ: `docs/spec/05-tasks.md` TASK-102・`docs/spec/04-behavior/search.md`）。
//! 関連: TASK-103, TASK-104, TASK-105。
//!
//! 責務境界: コーパスからトークン頻度・文書長統計を持つ [`SparseIndex`] を構築し、
//! クエリに対する BM25 スコア降順の Top-k 検索を提供する純関数的な API を提供し、
//! storage/catalog とは結線しない。[`SparseIndex::search`] はインデックス全体
//! （構築時のコーパス全体）を母数に統計・Top-k を計算するのに対し、
//! [`SparseIndex::search_within`] は呼び出し元が渡す可視集合（`visible_ids`）へ
//! 統計計算・候補選出そのものを縮約する版であり、`hybrid.rs::hybrid_search`
//! （TASK-103）がテナント境界（RLS 相当）を保つために後者を使う（Issue #36
//! codex-review P0 指摘対応。事後フィルタでは防げない理由は
//! [`SparseIndex::search_within`] のドキュメントを参照）。
//!
//! Okapi BM25（公知のアルゴリズム）を実装する。クエリ語 `q` に対する文書 `d` のスコアは
//! 各語 `t` について
//!
//! ```text
//! score(d, q) = Σ_t idf(t) * ( f(t, d) * (k1 + 1) )
//!                            / ( f(t, d) + k1 * (1 - b + b * |d| / avgdl) )
//! ```
//!
//! で与え、`idf(t) = ln( (N - df(t) + 0.5) / (df(t) + 0.5) + 1 )` とする（`+ 1` により
//! 常に非負となる）。`k1`・`b` は [`SparseIndex::with_params`] で調整可能とし、既定値は
//! `k1 = 1.2`, `b = 0.75`。クエリ時の上書きは [`Bm25Params`] と
//! [`SparseIndex::score_within_with_params`] で行い、索引（文書統計・postings）は
//! 変えない（SEARCH-13 のクエリ時適用に対応するポインタ。Issue #2121）。
//!
//! トークナイザ（[`tokenize`]）は ASCII 英数字・アンダースコアの連続を単語トークンとし、
//! CJK（ひらがな・カタカナ・CJK 統合漢字。カタカナ側は長音符 `ー`（U+30FC）を含むが
//! 記号の中黒 `・`（U+30FB）は単語区切りとして除外する）はユニグラム＋文字バイグラムを
//! 生成する（小文字化した上で処理）。既定（[`tokenize`]）ではユニグラムのうち助詞・
//! 機能語（TASK-105・SEARCH-5 の CJK ストップワード除去。詳細は [`tokenize_with_options`]）
//! の単独トークン化を抑制するが、CJK コンテンツ（内容語のバイグラム等）自体は保持する。
//! 対応範囲外の文字（全角英数字・アクセント付きラテン文字・ハングル・半角カタカナ等）は
//! 無音で破棄される。ASCII 単語の途中に出現した場合はその文字が欠落するだけでなく、
//! その位置で単語が分割される（前後が結合されるのではない。例: `"cafés"` →
//! `["caf", "s"]`）。この分割により生じた偽トークンは `term_freq`・`doc_freq`・
//! `doc_len` の統計を汚染しうる。
//!
//! [`SparseIndex::search`]／[`SparseIndex::search_within`] はいずれも文書集合を
//! 線形走査しない（Issue #390。可視ビットマップ＋posting 走査 1 パス化）。クエリ語の
//! 辞書（[`TermId`]）lookup を走査に入る前に一度だけ `O(Q)` で済ませ（Issue #388・
//! term インターニング）、共通コア（`score_by_postings`）がクエリ語ごとに転置索引
//! （`postings[t]`。Issue #389）だけを辿ってスコアを積むため、時間計算量は
//! `O(Q + Σ_{t∈Q} |postings(t)| + M log k)`（`Q`: クエリの一意語数、
//! `Σ|postings(t)|`: クエリ語の出現延べ件数、`M`: スコア `> 0` の一致文書数）で
//! あり、コーパス文書数 `N` そのものには比例しない。スコアアキュムレータ
//! （`acc`）の確保・ゼロ初期化は索引の寿命内で 1 回だけ `O(N)` で行い、以降の
//! 呼び出しは索引が保持するスクラッチプールを再利用するため `O(N)` の確保は
//! 発生しない（Issue #546。詳細は [`SparseIndex::score_by_postings`]・
//! [`SparseIndex::score_pass`] のドキュメンテーションコメント参照。Issue #390
//! レビュー指摘の解消）。長いクエリは
//! 走査対象の posting list 数を増幅するため、一意語数の上限を
//! `MAX_QUERY_TERMS` で検証し、超過時は `Err` を返す（fail-closed）。ただし
//! `tokenize()` 自体はクエリのバイト長に比例したコストを持つため、一意語数の少ない
//! 繰り返し入力（同じ語や区切り文字の反復）はこの検証だけでは防げない。そのため
//! クエリのバイト長にも `MAX_QUERY_BYTES` の上限を設け、`tokenize()` を呼ぶ前に
//! 検証する。
//!
//! [`SparseIndex::build`]/[`SparseIndex::with_params`] も同様の理由で、各文書に対して
//! `tokenize()` を呼ぶ前に文書 1 件のバイト長（`MAX_DOC_BYTES`）とコーパスの文書数
//! （`MAX_CORPUS_DOCS`）を検証する。この 2 つは互いに独立な検証のため、両方の上限
//! ちょうどの組み合わせだけではコーパス全体のバイト数を有界に保てない。そのため
//! 走査済み文書のバイト長累計にも `MAX_CORPUS_BYTES` の上限を設ける。ただしバイト長
//! の上限は `tokenize()` が生成するトークン数（＝ヒープ確保数）を直接制限しない
//! （CJK 入力はユニグラム＋バイグラムにより 1 文字あたり最大 2 トークンを生じる
//! ため、バイト長に比例しない）。そのため各文書を `tokenize()` した直後にも走査済み
//! トークン数の累計に `MAX_CORPUS_TOKENS` の上限を設ける。詳細は
//! [`SparseIndex::with_params`] を参照。
//!
//! 一致スパン（バイトオフセット）算出は子モジュール [`match_span`]（Issue #2124・
//! SEARCH-14 ポインタ）が担い、同じ分割規則（`walk_tokens`）を共有する。
//! 検索語の構文（フレーズ・`or`・除外・前方一致）の解析は子モジュール [`text_query`]
//! （Issue #2113・SEARCH-12 ポインタ）が担う。
//! 索引は `postings` と並列に出現位置 `positions` を持ち（Issue #2114）、フレーズ一致は
//! 子モジュール [`phrase`] が担う。スコア経路は位置を読まない。
//!
//! untrusted 入力の扱い: すべての処理を入力長に対して線形に保つ（バイグラム生成含む）。
//! `Vec::with_capacity` は入力を `chars()` で数えた実際の文字数からのみ見積もる。
//! クエリはバイト長を `MAX_QUERY_BYTES`、一意語数を `MAX_QUERY_TERMS` で上限検証し
//! （詳細は [`SparseIndex::search`]）、文書はバイト長を `MAX_DOC_BYTES`、コーパスの
//! 文書数を `MAX_CORPUS_DOCS`、コーパス全体のバイト長合計を `MAX_CORPUS_BYTES`、
//! コーパス全体のトークン数合計を `MAX_CORPUS_TOKENS` で上限検証する（詳細は
//! [`SparseIndex::with_params`]）。バイト長・件数の検証は `tokenize()` を呼ぶ前に
//! バイト長・件数のみを見る `O(1)` の判定で完結し追加アロケーションを要しないが、
//! トークン数の検証（`MAX_CORPUS_TOKENS`）だけは性質上 `tokenize()` の結果
//! （`Vec<String>` の長さ）が必要なため、`tokenize()` の直後・`term_freq`/`doc_freq`
//! の構築前に判定する。公開関数 [`tokenize`] 自体はこれらの上限を強制しない
//! （呼び出し側が上限検証済みの入力のみを渡す契約とする。詳細は [`tokenize`] の
//! ドキュメントを参照）。頻度・長さの演算はすべて `checked_*`/`saturating_*` を用い、
//! オーバーフローを未定義動作にしない。`tokenize()` 内の添字アクセスは事前の
//! ループ境界チェックにより範囲内が証明可能（panic しない）。

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;

/// 文書 ID。[`SparseIndex`] は呼び出し側が割り当てた ID をそのまま透過的に扱う。
pub type DocId = u64;

/// BM25 の Okapi パラメータ既定値（項の飽和度）。
const DEFAULT_K1: f64 = 1.2;
/// BM25 の Okapi パラメータ既定値（文書長正規化の強さ）。
const DEFAULT_B: f64 = 0.75;

/// クエリ時に受理する `k1` の数値安全上限（Issue #2121）。
///
/// 有限でも巨大な `k1`（例: `1e308`）では分子 `f * (k1 + 1)` や分母の
/// `k1 * len_norm` が `inf` に桁あふれし、`inf / inf = NaN` で候補が黙って落ちる
/// （fail-open）か `inf` スコアになる。1 文書の tf・文書長の上界
/// （[`MAX_CORPUS_TOKENS`] = 8e6）と `len / avgdl` の上界（[`MAX_CORPUS_DOCS`] = 1e5）
/// から、`1e200` なら分子・分母・累積和が最悪でも `1e215` 程度に収まり有限で、
/// 正の寄与が `0.0` に丸められない。値はデータに依存しない定数のため、拒否の有無から
/// 他テナントの存在・件数は推測できない。spec は範囲の上限を定めていないため、
/// 実装上の数値安全上限という解釈である（ADR `hybrid-tuning-parameters.md` 参照）。
const MAX_QUERY_K1: f64 = 1e200;

/// BM25 の `k1`・`b` の定義域検査（`with_params` と [`Bm25Params::new`] で共有）。
fn bm25_params_in_domain(k1: f64, b: f64) -> bool {
    k1.is_finite() && k1 >= 0.0 && b.is_finite() && (0.0..=1.0).contains(&b)
}

/// クエリ時に疎検索へ渡す BM25 パラメータ（Issue #2121。SEARCH-13 のクエリ時適用）。
///
/// sql 層（`HYBRID(..., k1 => .., b => ..)` の束縛結果）が生成し、
/// `hybrid.rs::hybrid_search_with_bm25` 経由で [`SparseIndex::score_within_with_params`]
/// に届く。索引の文書統計・postings・キャッシュキーには影響せず、スコア式の係数
/// だけに作用する。生成時に定義域と数値安全上限を検証するため、保持値は常に有効。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25Params {
    k1: f64,
    b: f64,
}

impl Bm25Params {
    /// 検証付きコンストラクタ。`k1` は有限・`0.0..=1e200`、`b` は有限・`[0.0, 1.0]`。
    /// 違反は [`SparseError::InvalidParams`]（sql 層で `22000` に写像される）。
    pub fn new(k1: f64, b: f64) -> Result<Self, SparseError> {
        if !bm25_params_in_domain(k1, b) || k1 > MAX_QUERY_K1 {
            return Err(SparseError::InvalidParams { k1, b });
        }
        Ok(Self { k1, b })
    }

    /// 項飽和度パラメータ。
    pub fn k1(&self) -> f64 {
        self.k1
    }

    /// 文書長正規化パラメータ。
    pub fn b(&self) -> f64 {
        self.b
    }
}

impl Default for Bm25Params {
    fn default() -> Self {
        Self {
            k1: DEFAULT_K1,
            b: DEFAULT_B,
        }
    }
}

pub mod match_span;
pub mod phrase;
pub mod text_eval;
pub mod text_query;

/// [`SparseIndex::search`] が受け付けるクエリの一意語数の上限。
///
/// `search()` は一致判定のためコーパスの全文書についてクエリの一意語集合を走査する
/// ため、処理コストはコーパス文書数とクエリの一意語数の積に比例して増える
/// （モジュールコメントの計算量を参照）。untrusted なクエリ文字列の語数を無制限に
/// 許すと、この積によって CPU コストを増幅させられる。1024 語は通常のキーワード
/// クエリを十分満たす一方、積を有界に保つための上限として妥当な値とする。
pub(crate) const MAX_QUERY_TERMS: usize = 1024;

/// [`SparseIndex::search`] が受け付けるクエリのバイト長の上限。
///
/// [`MAX_QUERY_TERMS`] は一意語数のみを制限するため、同じ語や区切り文字を大量に
/// 繰り返すクエリ（一意語数は少数のままバイト長だけが増大する入力）を防げない。
/// `tokenize()` はクエリ全体を小文字化して `Vec<char>` へ収集し各トークンごとに
/// `String` を確保するため、その繰り返しはバイト長に比例した CPU・メモリを消費する。
/// この検証は `query.len()`（バイト長）を `tokenize()` 呼び出し前に見るだけで済み、
/// 追加アロケーションなしで `O(1)` に判定できる。16 KiB は
/// `MAX_QUERY_TERMS`（1024 語）の一意語を平均的な単語長で表現するのに十分な余裕を
/// 持たせつつ（1 語あたり数バイト～十数バイト換算で 1024 語は数 KiB 程度に収まる）、
/// 繰り返し入力によるバイト数だけの増幅を有界に保つための上限とする。
const MAX_QUERY_BYTES: usize = 16 * 1024;

/// [`SparseIndex::search`]／[`SparseIndex::search_within`] が課すクエリ入力検証
/// （バイト長 [`MAX_QUERY_BYTES`]・一意語数 [`MAX_QUERY_TERMS`]）を、疎検索本体を
/// 呼ぶ前に単独で行う（`sql::using_plan::expanded_query_text` が組み立てる疎側
/// クエリ文字列〔`hybrid_search` の全文検索側入力〕を、密側の再埋め込み
/// （`Embedder::embed_batch`）前に検証するための呼び出し口。codex-review P1 指摘
/// 対応、PR #266）。
///
/// 上限値・判定順序（バイト長 → `tokenize()` → 一意語数）は [`SparseIndex::
/// search`]／[`SparseIndex::search_within`] 内の入力検証と同一の値・同一の
/// 判定条件を用いる（`MAX_QUERY_BYTES`・`MAX_QUERY_TERMS` の単一真実源を保つ）。
/// スコアリングに使う一意語集合の再構築コストを避けるため、`search`/
/// `search_within` 自体は本関数を経由せず従来どおり自前で検証する（多層防御。
/// 呼び出し元が異なるタイミング〔本関数は再埋め込み前、`search`系は疎検索
/// 実行直前〕で同じ契約を課すことが目的であり、実装の一本化ではない）。
pub(crate) fn validate_query_bounds(query: &str) -> Result<(), SparseError> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(SparseError::QueryTooLong {
            len: query.len(),
            max: MAX_QUERY_BYTES,
        });
    }

    let mut unique_terms: BTreeSet<String> = BTreeSet::new();
    for term in tokenize(query) {
        unique_terms.insert(term);
    }
    if unique_terms.len() > MAX_QUERY_TERMS {
        return Err(SparseError::TooManyQueryTerms {
            unique_terms: unique_terms.len(),
            max: MAX_QUERY_TERMS,
        });
    }
    Ok(())
}

/// [`SparseIndex::with_params`]（構築）が受け付ける 1 文書のバイト長の上限。
///
/// `with_params()` は各文書テキストに対して `tokenize()` を呼ぶため、
/// [`MAX_QUERY_BYTES`] と同じ理由（アロケーションを伴う走査コストがバイト長に
/// 比例する）で、文書 1 件のバイト長にも上限が必要になる。文書はクエリより
/// 大きな単位（チャンク・段落等）になりうるため、[`MAX_QUERY_BYTES`]（16 KiB）の
/// 64 倍にあたる 1 MiB を上限とする。
const MAX_DOC_BYTES: usize = 1024 * 1024;

/// [`SparseIndex::with_params`]（構築）が受け付けるコーパスの文書数の上限。
///
/// [`MAX_DOC_BYTES`] は 1 文書あたりのコストを有界にするが、文書数 `N` を
/// 無制限に許すとコーパス全体の処理コスト（総バイト数は最悪 `N * MAX_DOC_BYTES`
/// に達する）が無制限に増える。10 万件は単一の `SparseIndex` に載せるコーパス
/// として十分大きい一方、`N * MAX_DOC_BYTES` の積を有界に保つための上限として
/// 妥当な値とする。
pub(crate) const MAX_CORPUS_DOCS: usize = 100_000;

/// [`SparseIndex::with_params`]（構築）が受け付けるコーパス全体のバイト長合計の上限。
///
/// [`MAX_DOC_BYTES`]・[`MAX_CORPUS_DOCS`] は互いに独立な検証のため、両方の上限
/// ちょうど（1 MiB 文書 × 10 万件）を組み合わせると最大で約 100 GiB もの入力を
/// 許してしまい、単体では OOM を防げない。64 MiB は単一の `SparseIndex` に載せる
/// テキストコーパスの入力サイズとして十分実用的な規模を許容する上限とする。
///
/// ただしこのバイト長上限は入力テキストのサイズのみを制限し、`tokenize()` が
/// 生成するトークン数（＝ヒープオブジェクト・`BTreeMap` ノードの生成数）を直接
/// 制限しない。CJK 文字はユニグラム＋バイグラムを生成するため 1 文字（3 バイト）
/// あたり最大 2 トークンを生じ、バイト数から見積もれる最悪ケースのトークン数は
/// 大きい。実効的なヒープ割当量の上限は [`MAX_CORPUS_TOKENS`] が別途担う。
pub(crate) const MAX_CORPUS_BYTES: usize = 64 * 1024 * 1024;

// `MAX_CORPUS_BYTES` が `MAX_DOC_BYTES` の整数倍であることをコンパイル時に固定する。
// 境界値テストは MAX_DOC_BYTES サイズの文書を複数回参照して MAX_CORPUS_BYTES ちょうどの
// コーパスを組み立てるため（巨大な単一バッファの重複確保を避ける）、この関係が崩れると
// 境界値テストの前提が壊れる。
const _: () = assert!(MAX_CORPUS_BYTES.is_multiple_of(MAX_DOC_BYTES));
// 位置（Issue #2114）は 1 文書内の単位数で、単位数は文字数以下・文字数はバイト長以下の
// ため `MAX_DOC_BYTES` 以下になる。`u32` の位置カウンタが索引経路で飽和しないことを固定する。
const _: () = assert!(MAX_DOC_BYTES <= u32::MAX as usize);

/// [`SparseIndex::with_params`]（構築）が受け付けるコーパス全体のトークン数合計の上限。
///
/// [`MAX_CORPUS_BYTES`] は入力テキストのバイト数のみを制限し、`tokenize()` が
/// 生成するトークン数を直接制限しない。CJK 文字はユニグラム＋バイグラムを生成する
/// ため 1 文字（3 バイト）あたり最大 2 トークンとなり、64 MiB の CJK 主体の入力から
/// 数千万規模のトークンが生じうる。トークンは term 辞書（[`TermDictionary`]。
/// Issue #388・term インターニング）へ intern され、新出語のみが辞書内に `String` を
/// 1 回だけヒープ確保する（既知語は `TermId`〔`u32`〕のコピーのみで追加確保なし）。
/// 各文書側は `(TermId, u32)` の 8 バイトタプルとして `Vec` に保持するため、旧来の
/// `String` キー木構造（1 トークンごとに `String` の見出し用ヒープ確保が発生する
/// 構造）よりも実効メモリは小さくなる方向だが、新出語彙数の多い入力（CJK 一辺倒の
/// 入力等）では辞書側の `String` 確保が支配的になりうる。800 万トークンはこの見積もり
/// でも実行時メモリを現実的な範囲に収めつつ、典型的な（CJK 一辺倒ではない）コーパス
/// であれば [`MAX_CORPUS_BYTES`] に近いサイズでも許容できる規模の上限とする。
const MAX_CORPUS_TOKENS: usize = 8_000_000;

// [`TermId`] は build 時に確定する語彙内の term を `u32` で表す（Issue #388・
// term インターニング）。語彙数はコーパス全体のトークン数（[`MAX_CORPUS_TOKENS`]
// で上限検証済み）を超えないため、`u32` の表現域に収まることをコンパイル時に固定する
// （実行時のオーバーフローチェックはこの不変条件の防御線として [`TermDictionary::
// intern`] にも残す）。
const _: () = assert!(MAX_CORPUS_TOKENS <= u32::MAX as usize);

/// build 時に確定する語彙内の term 識別子（Issue #388・term インターニング。
/// 対象ビヘイビア: SEARCH-1, SEARCH-3）。0 始まりで [`TermDictionary::intern`] の
/// 初出順に採番する。文書側・コーパス側の統計を `String` キーの木構造ではなく
/// この `u32` キーで持つことで、ホットパス（[`SparseIndex::with_params`]・
/// [`SparseIndex::search`]・[`SparseIndex::search_within`]）の文字列比較・
/// `String::clone` を排除する。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TermId(u32);

/// `String` → [`TermId`] の辞書（Issue #388）。[`SparseIndex::with_params`] が
/// コーパス構築中に term を intern し、[`SparseIndex::search`]／
/// [`SparseIndex::search_within`] がクエリ語を `TermId` へ写像する lookup に使う。
///
/// `HashMap`（標準既定の `RandomState`／SipHash-1-3）を用いる。イテレーション順は
/// 不定であり、[`SparseIndex::approx_heap_bytes`] は容量見積りのため本辞書の
/// キーを走査する。ただしその走査は整数和（バイト数の合計）にのみ用いるため
/// 順序に依存せず、出力の決定性（スコアのビット一致・タイブレーク）には影響しない
/// （語彙順に依存する走査は `search`/`search_within` のクエリ側処理が
/// クエリ語の `String` 辞書順で行う。[`SparseIndex::search`] を参照）。
#[derive(Debug, Default)]
struct TermDictionary {
    ids: HashMap<String, TermId>,
}

impl TermDictionary {
    /// `term` を intern し [`TermId`] を返す。既知の語は追加確保なしで既存 ID を返し
    /// （`String::clone` を発生させない）、未知語は `term`（所有権ごと）を辞書へ
    /// move して新しい ID を採番する。
    ///
    /// 新規語彙数が `u32` の表現域を超える場合は fail-closed に `Err` を返す
    /// （`.claude/rules/coding-rust.md`: 整数演算は checked に倒す）。語彙数は
    /// コーパスの総トークン数を超えないため [`MAX_CORPUS_TOKENS`] の検証により
    /// 実質到達しないが、この不変条件が将来崩れても ID の衝突・サイレントな
    /// 統計汚染を起こさないための防御線として明示的に検証する。
    fn intern(&mut self, term: String) -> Result<TermId, SparseError> {
        if let Some(&id) = self.ids.get(term.as_str()) {
            return Ok(id);
        }
        let next = u32::try_from(self.ids.len()).map_err(|_| SparseError::TooManyTokens {
            total: self.ids.len(),
            // この分岐が到達するのは語彙数が `u32::MAX` に達した場合であり、
            // 呼び出し元（`with_params`）の `MAX_CORPUS_TOKENS` 検証により
            // 実質到達不能な防御的分岐（コメント参照）。`max` に `MAX_CORPUS_TOKENS`
            // を報告すると `total`（~4.29e9）が `max`（8,000,000）を超える
            // 自己矛盾したエラーペイロードになるため、この分岐の実際の境界値
            // （`u32` の表現域）を報告する。
            max: u32::MAX as usize,
        })?;
        let id = TermId(next);
        self.ids.insert(term, id);
        Ok(id)
    }

    /// `term` の [`TermId`] を返す（未知語は `None`）。クエリ側で構築後の辞書へ
    /// 追加せず参照のみ行う（不変な辞書として扱う）。
    fn lookup(&self, term: &str) -> Option<TermId> {
        self.ids.get(term).copied()
    }
}

/// 疎検索モジュールの公開エラー型。fail-closed 方針に従い、構築時の異常入力は
/// 曖昧に握りつぶさず `Err` として明示する（`.claude/rules/coding-rust.md`）。
///
/// `InvalidParams` が `f64` を保持するため `Eq` は導出しない（`PartialEq` のみ）。
#[derive(Debug, Clone, PartialEq)]
pub enum SparseError {
    /// コーパスが空で構築できない（Top-k 検索の対象が存在しないため）。
    EmptyCorpus,
    /// コーパス内に重複する `DocId` が存在する（統計の整合性が壊れるため拒否する）。
    DuplicateDocId(DocId),
    /// `k1`・`b` が不正（有限でない、または定義域外）。fail-open な空結果を返す
    /// 代わりに構築時点で拒否する（`.claude/rules/coding-rust.md`: エラー契約は
    /// fail-closed とする）。
    InvalidParams { k1: f64, b: f64 },
    /// クエリの一意語数が [`MAX_QUERY_TERMS`] を超える。長いクエリはコーパス文書数との
    /// 積で走査コストを増幅させるため（モジュールコメント参照）、`search()` の入口で
    /// fail-closed に拒否する（`.claude/rules/coding-rust.md`: untrusted 入力の長さは
    /// 上限検証してから処理する）。
    TooManyQueryTerms { unique_terms: usize, max: usize },
    /// クエリのバイト長が [`MAX_QUERY_BYTES`] を超える。`TooManyQueryTerms` は一意語数
    /// のみを制限するため、同じ語・区切り文字の繰り返しによるバイト長の増大は防げない
    /// （[`MAX_QUERY_BYTES`] のコメント参照）。`tokenize()` を呼ぶ前に `query.len()`
    /// （アロケーション不要）で判定し、`search()` の入口で fail-closed に拒否する。
    QueryTooLong { len: usize, max: usize },
    /// コーパス内の 1 文書のバイト長が [`MAX_DOC_BYTES`] を超える。`with_params()` は
    /// 各文書に対して `tokenize()` を呼ぶため、`QueryTooLong` と同じ理由で
    /// `tokenize()` を呼ぶ前に `text.len()`（アロケーション不要）で判定し、
    /// fail-closed に拒否する。
    DocTooLong {
        doc_id: DocId,
        len: usize,
        max: usize,
    },
    /// コーパスの文書数が [`MAX_CORPUS_DOCS`] を超える。`with_params()` の入口で
    /// （各文書の走査に入る前に）`docs.len()`（アロケーション不要）で判定し、
    /// fail-closed に拒否する。
    TooManyDocs { len: usize, max: usize },
    /// コーパス全体のバイト長合計が [`MAX_CORPUS_BYTES`] を超える。`MAX_DOC_BYTES`・
    /// `MAX_CORPUS_DOCS` は互いに独立な検証のため、両方の上限ちょうどの組み合わせでも
    /// 巨大な入力を許してしまう（[`MAX_CORPUS_BYTES`] のコメント参照）。`with_params()`
    /// は各文書の `tokenize()` を呼ぶ前に、それまでの文書バイト長の累計を
    /// `saturating_add` で求めて判定し、超過した時点で fail-closed に拒否する
    /// （オーバーフロー時は `total` を `usize::MAX` として報告し、必ず拒否する）。
    CorpusTooLarge { total: usize, max: usize },
    /// コーパス全体のトークン数合計が [`MAX_CORPUS_TOKENS`] を超える。`CorpusTooLarge`
    /// は入力テキストのバイト長のみを制限するため、CJK 主体の入力（ユニグラム＋
    /// バイグラムで 1 文字あたり最大 2 トークンを生じる）ではトークン数・ヒープ確保数
    /// がバイト長に比例しない（[`MAX_CORPUS_TOKENS`] のコメント参照）。`with_params()`
    /// は各文書を `tokenize()` した直後（`term_freq`・`doc_freq` を構築する前）に、
    /// それまでのトークン数の累計を `saturating_add` で求めて判定し、超過した時点で
    /// fail-closed に拒否する。
    TooManyTokens { total: usize, max: usize },
    /// 文書長クラス表（Issue #391・[`SparseIndex::len_classes`]／`doc_len_class`）の
    /// 構築時に不変条件が満たされなかった。`len_classes` は `doc_len_by_idx` の値
    /// 集合そのものから作るため理論上到達不能だが、`unwrap`/`expect` で処理せず
    /// fail-closed に構築を拒否する（`.claude/rules/coding-rust.md`）。
    ///
    /// `SparseError` は `#[non_exhaustive]` を持たない既公開 enum であり
    /// （`docs/design/error-enum-non-exhaustive-policy.md`）、本 variant の追加は
    /// 同 ADR の方針（既公開エラー enum への `#[non_exhaustive]` 後付け不採用）
    /// どおりソース非互換な破壊的変更として扱う（コミットの `BREAKING CHANGE:`
    /// フッタで明示する）。
    LenClassBuildFailed,
}

impl std::fmt::Display for SparseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SparseError::EmptyCorpus => write!(f, "sparse index corpus must not be empty"),
            SparseError::DuplicateDocId(id) => {
                write!(f, "duplicate doc id in corpus: {id}")
            }
            SparseError::InvalidParams { k1, b } => {
                write!(f, "invalid BM25 params: k1={k1}, b={b} (require k1 finite & >= 0.0, b finite & in [0.0, 1.0])")
            }
            SparseError::TooManyQueryTerms { unique_terms, max } => {
                write!(f, "too many unique query terms: {unique_terms} (max {max})")
            }
            SparseError::QueryTooLong { len, max } => {
                write!(f, "query too long: {len} bytes (max {max})")
            }
            SparseError::DocTooLong { doc_id, len, max } => {
                write!(
                    f,
                    "document too long: doc_id={doc_id}, {len} bytes (max {max})"
                )
            }
            SparseError::TooManyDocs { len, max } => {
                write!(f, "too many documents in corpus: {len} (max {max})")
            }
            SparseError::CorpusTooLarge { total, max } => {
                write!(f, "corpus too large: {total} bytes (max {max})")
            }
            SparseError::TooManyTokens { total, max } => {
                write!(f, "too many tokens in corpus: {total} (max {max})")
            }
            SparseError::LenClassBuildFailed => {
                write!(
                    f,
                    "internal invariant violated: doc length class table build failed"
                )
            }
        }
    }
}

impl std::error::Error for SparseError {}

/// Top-k 検索結果 1 件（文書 ID と BM25 スコア）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredDoc {
    /// スコア対象の文書 ID（呼び出し側が [`SparseIndex::build`]/`with_params` に渡した ID）。
    pub doc_id: DocId,
    /// BM25 スコア（降順ソート済み。同点は `doc_id` 昇順でタイブレークする）。
    pub score: f64,
}

/// [`SparseIndex::search`] が Top-k 選出中に走査する 1 件の候補。
///
/// `Ord` は最終的な検索結果の順序契約（スコア降順、同点は `doc_id` 昇順）で
/// 「大きい方が良い」を表すよう実装する。[`TopKSelector`]（Issue #391）が
/// この全順序だけを根拠に Top-k 集合を確定するため、選出アルゴリズムを
/// 差し替えても出力される集合・順序は変わらない（BM25 スコア計算自体の
/// 計算量は [`SparseIndex::search`] のドキュメントを参照）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    score: f64,
    doc_id: DocId,
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // スコアは `total_cmp` で全順序化する（NaN を含めても panic しない。スコアは
        // 有限値のみを積むため NaN は理論上生じない）。同点時は `doc_id` が小さいほど
        // 「良い」（`Ordering::Greater`）とするため、比較対象を入れ替えて逆順にする。
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.doc_id.cmp(&self.doc_id))
    }
}

/// [`SparseIndex::score_by_postings`] が posting 走査で見つかった候補
/// （スコア `> 0` の一致文書、`touched`）から Top-k を選び出すセレクタ
/// （Issue #391。tantivy の `TopNComputer` を参考にした「2k 件バッファ＋
/// `select_nth_unstable_by`」方式）。`BinaryHeap<Reverse<Candidate>>` への
/// 逐次 `O(log k)` 挿入の代わりに、バッファが `2 * k_eff` 件に達するたびに
/// [`slice::select_nth_unstable_by`] で上位 `k_eff` 件へ切り詰め、`threshold`
/// （切り詰め後の最下位候補）以下の以降の候補を `O(1)` で棄却する。出力する
/// Top-k 集合・順序は [`Candidate`] の全順序で一意に決まるため、選出方式の
/// 違いは結果に影響しない（旧 `BinaryHeap` 実装とビット一致・集合一致。
/// `crates/engine/tests/hybrid_profile_accept.rs::
/// profile_sparse_index_replica_matches_real_search_within` で検証）。
struct TopKSelector {
    /// `k.min(M)`（`M` はスコア `> 0` の一致文書数）。呼び出し元が事前に
    /// 有界化した値を渡す契約とし、本セレクタ自身は `k`／`M` を知らない。
    k_eff: usize,
    /// 保持中の候補（未整列）。容量は `2 * k_eff` で有界
    /// （untrusted な `k`／`M` からの無制限確保を避ける。
    /// `.claude/rules/coding-rust.md`）。
    buffer: Vec<Candidate>,
    /// 直近の切り詰め後、保持中で最も「悪い」候補（`k_eff` 番目に良い候補）。
    /// これ以下の候補は Top-k に入り得ないため `push` で即座に棄却する。
    threshold: Option<Candidate>,
}

impl TopKSelector {
    fn new(k_eff: usize) -> Self {
        Self {
            k_eff,
            // `2 * k_eff` は `k_eff <= touched.len()`（呼び出し元契約）かつ
            // `touched.len() <= self.doc_ids.len() <= MAX_CORPUS_DOCS` で有界。
            buffer: Vec::with_capacity(k_eff.saturating_mul(2)),
            threshold: None,
        }
    }

    /// 候補を 1 件追加する。`k_eff == 0` なら常に無視する。既に確定した
    /// `threshold` 以下（同点で `doc_id` が同等以上に悪い場合を含む）の候補は
    /// Top-k に入り得ないため `O(1)` で棄却する。
    fn push(&mut self, candidate: Candidate) {
        if self.k_eff == 0 {
            return;
        }
        if let Some(threshold) = self.threshold {
            if candidate <= threshold {
                return;
            }
        }
        self.buffer.push(candidate);
        if self.buffer.len() >= self.k_eff.saturating_mul(2) {
            self.shrink_to_k_eff();
        }
    }

    /// バッファを上位 `k_eff` 件へ切り詰め、`threshold` を更新する。
    /// `select_nth_unstable_by` は `k_eff >= 1` かつ `k_eff - 1` がバッファの
    /// 範囲内の場合のみ呼ぶ（範囲外呼び出しは panic するため、事前条件を
    /// 満たさない場合は無音で何もしない。呼び出し元は条件を満たす場合のみ
    /// 呼ぶため到達しないが、fail-closed のため防御する）。
    fn shrink_to_k_eff(&mut self) {
        if self.k_eff == 0 || self.k_eff > self.buffer.len() {
            return;
        }
        // `Candidate::Ord` の「大きい方が良い」を降順に並べる比較器（`b.cmp(a)`）。
        // 添字 `k_eff - 1` の要素はちょうど「上位 `k_eff` 件中で最も悪い候補」に
        // なり、それより前は同等以上に良い・後は同等以下に悪いという整列条件を
        // `select_nth_unstable_by` が保証する。
        // Candidate::Ord はスコア total_cmp に doc_id 昇順の明示的タイブレークを含む
        // 全順序（同点の doc_id が同一になることはない）ため、不安定な select_nth でも
        // 相対順序の非決定性は生じない。
        self.buffer
            .select_nth_unstable_by(self.k_eff - 1, |a, b| b.cmp(a)); // sort-determinism: allow Candidate::Ord は doc_id 昇順の明示的タイブレークを含む全順序
        self.buffer.truncate(self.k_eff);
        self.threshold = self.buffer.get(self.k_eff - 1).copied();
    }

    /// 最終的な Top-k をスコア降順（同点は `doc_id` 昇順）で確定する。
    fn into_sorted(mut self) -> Vec<Candidate> {
        if self.buffer.len() > self.k_eff {
            self.shrink_to_k_eff();
        }
        // Candidate::Ord はスコア total_cmp に doc_id 昇順の明示的タイブレークを含む
        // 全順序のため、不安定ソートでも出力順序は決定的。
        self.buffer.sort_unstable_by(|a, b| b.cmp(a)); // sort-determinism: allow Candidate::Ord は doc_id 昇順の明示的タイブレークを含む全順序
        self.buffer
    }
}

/// [`SparseIndex::score_within`] が返す、1 クエリ分のスコア済み候補集合
/// （Issue #392）。`hybrid.rs::sparse_refetch_loop`（疎側再取得ループ）は
/// `fetch_k` を倍増させながら同じクエリを何度も評価するが、疎側（BM25）は
/// 1 回の posting 走査でスコア `> 0` の全候補が確定するため、ラウンドごとに
/// 必要なのは「同じスコア集合からより深い Top-k を切り出す」ことだけである。
/// 本型はスコア計算そのもの（[`SparseIndex::score_pass`]）を 1 回だけ行い、
/// [`Self::top`] の複数回呼び出しで前方一致な Top-k 拡張を提供することで、
/// ラウンドごとの `acc: Vec<f64>` の `O(N)` 再確保・posting 再走査・Top-k
/// 再選出を避ける。
///
/// `candidates` は [`Candidate`] の全順序（スコア降順・同点は `doc_id` 昇順）
/// に従う Top-k 契約を構造的に保証する材料（スコア `> 0` の可視ヒット全件）で
/// あり、[`Self::top`] は `candidates` の先頭 `sorted_len` 件がこの順序で
/// 確定済みであるという不変条件を保ちながら未整列の尾部だけを段階的に
/// 整列する。
#[derive(Debug)]
pub struct SparseScored {
    candidates: Vec<Candidate>,
    /// `candidates[..sorted_len]` は既にスコア降順（同点 `doc_id` 昇順）に
    /// 確定済み。`sorted_len <= candidates.len()`（不変条件）。
    sorted_len: usize,
}

impl SparseScored {
    /// スコア `> 0` の可視ヒット総数（`M`）。
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// [`Self::len`] が `0` かどうか。
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// スコア降順（同点は `doc_id` 昇順）で先頭 `k.min(self.len())` 件を返す
    /// （[`SparseIndex::search_within`] と同一の順序契約）。
    ///
    /// 前方一致契約: `k1 <= k2` なら `top(k1)` は `top(k2)` の前方一致な
    /// 部分列になる（[`Candidate`] の全順序に基づき同一の候補配列から切り出す
    /// ため構造的に保証される）。冪等（同じ `k` を複数回呼んでも同じ列を返す）。
    ///
    /// `&mut self` である理由: 呼び出しごとに `candidates[sorted_len..]` を
    /// 段階的に整列して `sorted_len` を前進させる in-place 分割方式のため
    /// （`hybrid.rs::sparse_refetch_loop` は同一の `SparseScored` を再取得
    /// ラウンドごとに `top` で呼び直す）。
    ///
    /// 確保量: 新規確保は行わず、返す `Vec<ScoredDoc>` の長さも `min(k,
    /// self.len())` で有界（untrusted な `k` によって `M` を超えて増幅
    /// しない。`.claude/rules/coding-rust.md`）。
    pub fn top(&mut self, k: usize) -> Vec<ScoredDoc> {
        let want = k.min(self.candidates.len());
        if want == 0 {
            return Vec::new();
        }
        if want > self.sorted_len {
            let tail_start = self.sorted_len;
            // `need` は尾部のうち今回新たに確定させたい件数（>= 1。
            // `want > self.sorted_len` の分岐内のため常に正）。
            let need = want - tail_start;
            if let Some(tail) = self.candidates.get_mut(tail_start..) {
                // `select_nth_unstable_by` は事前条件（添字 < スライス長）を
                // 満たす場合のみ呼ぶ（`TopKSelector::shrink_to_k_eff` と同じ
                // fail-closed な防御。範囲外なら何もしない＝下の `sort_unstable_by`
                // だけで全尾部を整列することになり安全側に倒れる）。
                if need >= 1 && need <= tail.len() {
                    // Candidate::Ord はスコア total_cmp に doc_id 昇順の明示的
                    // タイブレークを含む全順序（同点の doc_id が同一になることは
                    // ない）ため、不安定な select_nth でも相対順序の非決定性は
                    // 生じない（`TopKSelector::shrink_to_k_eff` と同じ根拠）。
                    tail.select_nth_unstable_by(need - 1, |a, b| b.cmp(a)); // sort-determinism: allow Candidate::Ord は doc_id 昇順の明示的タイブレークを含む全順序
                }
            }
            if let Some(newly_sorted) = self.candidates.get_mut(tail_start..want) {
                // 同上の全順序により不安定ソートでも出力順序は決定的
                // （`TopKSelector::into_sorted` と同じ根拠）。
                newly_sorted.sort_unstable_by(|a, b| b.cmp(a)); // sort-determinism: allow Candidate::Ord は doc_id 昇順の明示的タイブレークを含む全順序
            }
            self.sorted_len = want;
        }
        self.candidates
            .get(..want)
            .map(|s| {
                s.iter()
                    .map(|&Candidate { score, doc_id }| ScoredDoc { doc_id, score })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 小文字化した 1 文字が ASCII 単語トークンの構成要素かどうか。
fn is_ascii_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 小文字化した 1 文字が CJK（ひらがな・カタカナ・CJK 統合漢字）かどうか。
/// `char` の範囲判定のみで実装し、外部の正規表現・Unicode データクレートに依存しない
/// （`.claude/rules/dependency-policy.md`: 依存最小方針）。カタカナのレンジ
/// （U+30A0..=U+30FF）のうち中黒 `・`（U+30FB）は記号であり単語区切りとして扱うため
/// このレンジから除外する（TASK-105・SEARCH-5 対応）。長音符 `ー`（U+30FC）は
/// カタカナ語（例: 「サーバー」）の構成要素であり除外すると内容語が壊れるため、
/// 引き続き CJK 扱いのまま保持する。
fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}' // ひらがな
        | '\u{30A0}'..='\u{30FA}' // カタカナ（中黒 U+30FB の手前まで）
        | '\u{30FC}'..='\u{30FF}' // カタカナ（長音符 U+30FC 以降。中黒のみ除外）
        | '\u{4E00}'..='\u{9FFF}' // CJK 統合漢字
    )
}

/// [`tokenize_with_options`] がユニグラムの単独トークン化を抑制する CJK 助詞・
/// 形式的機能語の 1 文字集合（TASK-105・対象ビヘイビア SEARCH-5。判断理由は
/// spec 側の定義を参照）。文字一致のみで判定するため、この 1 文字自体が内容語で
/// あるケースは区別できず除去対象になる（既知の制約。回避手段は
/// [`tokenize_with_options`] を参照）。
const CJK_STOPWORD_UNIGRAMS: [char; 10] =
    ['は', 'が', 'を', 'に', 'で', 'と', 'も', 'の', 'へ', 'や'];

/// 小文字化した 1 文字が [`CJK_STOPWORD_UNIGRAMS`] に含まれる助詞・機能語かどうか。
fn is_cjk_stopword_unigram(c: char) -> bool {
    CJK_STOPWORD_UNIGRAMS.contains(&c)
}

/// クエリ・文書テキストをトークン列へ分割する（TASK-102 の簡易トークナイザ、
/// CJK ストップワード除去は TASK-105・SEARCH-5 対応）。
///
/// [`tokenize_with_options`] を `remove_stopwords = true`（既定で除去 ON）で呼ぶ薄い
/// ラッパ。詳細な挙動・契約は [`tokenize_with_options`] を参照。
pub fn tokenize(text: &str) -> Vec<String> {
    tokenize_with_options(text, true)
}

/// [`tokenize`] の除去有無を選べる版（TASK-105・対象ビヘイビア SEARCH-5）。
///
/// 小文字化した上で、ASCII 英数字・アンダースコアの連続を単語トークンとし、CJK 文字は
/// 文字ユニグラム＋隣接 2 文字のバイグラムを生成する。`remove_stopwords = true` の場合、
/// [`CJK_STOPWORD_UNIGRAMS`] に含まれる文字がユニグラムとして単独出現した際にそのユニ
/// グラムだけを出力から除く（バイグラムは対象外）。除去有無で `SparseIndex::build`/
/// `search` の対称性は自動的に保たれる（`tokenize()` は常に除去 ON のため index/query
/// 間でトークナイザが一致する）。
///
/// 既知の制約: 除去は文字一致のみで判定するため、除去対象の 1 文字自体が内容語で
/// あるクエリ・文書は用法を区別できず該当文字が失われる。`SparseIndex` はこの除去を
/// 常に適用する（`tokenize()` 経由）ため回避できない。この挙動を避けたい呼び出し側は
/// `tokenize_with_options(text, false)` を直接呼ぶこと。除去有無によるランキング品質の
/// 比較測定は `crates/engine/tests/sparse_stopwords.rs` を参照。
///
/// 入力長に対して線形（`O(n)`）に処理し、`Vec` の初期容量は入力を `chars()` で数えた
/// 実際の文字数からのみ見積もる。トークンが 1 つも得られない入力（空文字列・記号のみ・
/// 助詞のみ等）は空の `Vec` を返す（呼び出し側はこれをエラーではなく空結果として扱う
/// 契約とする）。
///
/// 対応範囲は ASCII 単語文字と CJK（ひらがな・カタカナ・CJK 統合漢字）に限る。それ以外の
/// 文字（全角英数字・アクセント付きラテン文字・ハングル・半角カタカナ・中黒 `・`
/// U+30FB 等）は単語区切りとして無音で破棄され、ASCII 単語の途中に出現した場合は
/// その文字が欠落するだけでなく**その位置で単語が分割される**（前後が 1 トークンへ
/// 結合されるのではない。例: `"cafés"` → `["caf", "s"]`）。この分割で生じる偽トークンは
/// 統計（`term_freq`・`doc_freq`・`doc_len`）を汚染しうる。
///
/// 契約: この関数自体は `text` のバイト長・文字数に上限を設けない。呼び出し側
/// （[`SparseIndex::with_params`]・[`SparseIndex::search`]）が `tokenize()` を呼ぶ前に
/// それぞれの上限（`MAX_DOC_BYTES`・`MAX_QUERY_BYTES`）を検証する経路からのみ呼ばれる
/// ことを前提とする。本関数を直接 untrusted 入力に対して呼ぶ場合は、呼び出し側で
/// 同等の長さ検証を行うこと。
pub fn tokenize_with_options(text: &str, remove_stopwords: bool) -> Vec<String> {
    // 容量は検証済み長さの char 数のみを根拠にする（過小でも Vec は伸長する）。
    let mut tokens: Vec<String> = Vec::with_capacity(text.chars().count());
    walk_tokens(text, remove_stopwords, |token, _span| {
        tokens.push(token.to_owned());
        true
    });
    tokens
}

/// 分割規則の単一真実源となる範囲付きトークン walker（Issue #2124・SEARCH-14 ポインタ）。
///
/// [`walk_tokens_positioned`] から位置を捨てた薄いアダプタ。`tokenize_with_options`
/// （索引・クエリ）と `match_span`・`text_query`・`headline` が呼ぶ既存の入口で、
/// シグネチャと出力トークン列は位置対応化の前後で変わらない。
pub(crate) fn walk_tokens<F>(text: &str, remove_stopwords: bool, mut emit: F)
where
    F: FnMut(&str, std::ops::Range<usize>) -> bool,
{
    walk_tokens_positioned(text, remove_stopwords, |token, span, _pos| {
        emit(token, span)
    });
}

/// 位置つきの範囲付きトークン walker（Issue #2114・SEARCH-12 ポインタ。D4 の位置モデル）。
///
/// 分割規則そのものの正であり、[`walk_tokens`] はここから位置を捨てるだけである。
/// `text` を元の文字単位で辿り、各文字を `char::to_lowercase()` で展開した文字列に
/// 判定（[`is_ascii_word_char`]・[`is_cjk_char`]・[`is_cjk_stopword_unigram`]）を適用する。
/// トークンの範囲は、構成する生成文字が由来する元の文字のバイト範囲の和（`char_indices`
/// 由来のため常に UTF-8 の文字境界）で、全体の小文字化によるオフセットずれを起こさない。
/// トークン順は「ASCII 単語は区切りで flush、CJK は直前文字とのバイグラム → 自身の
/// ユニグラム」。`emit` が `false` を返した時点で走査を打ち切る。入力長に対して線形で、
/// トークンごとの `String` 確保はしない（再利用バッファ）。
///
/// 位置モデル（`emit` の第 3 引数）: 位置は「単位」の通し番号（`u32`、0 始まり）。
/// ASCII 単語は 1 単位、CJK は 1 文字ごとに 1 単位。バイグラムは先頭の文字の位置、
/// ユニグラムはその文字の位置を持つ。ストップワード文字はユニグラムが抑止されても
/// 位置を消費する。区切り（記号・空白・未対応文字）は位置を消費しない。カウンタは
/// `saturating_add`（索引経路では 1 文書の単位数が [`MAX_DOC_BYTES`] 以下で飽和しない）。
pub(crate) fn walk_tokens_positioned<F>(text: &str, remove_stopwords: bool, mut emit: F)
where
    F: FnMut(&str, std::ops::Range<usize>, u32) -> bool,
{
    let mut word = String::new();
    let mut word_start = 0usize;
    let mut word_end = 0usize;
    let mut buf = String::new();
    // 次に割り当てる位置（単位の通し番号）。
    let mut pos: u32 = 0;
    // 直前の生成文字が CJK のときのみ Some（元の文字の範囲と位置つき）。
    let mut prev_cjk: Option<(char, usize, usize, u32)> = None;

    for (src_start, src_char) in text.char_indices() {
        let src_end = src_start.saturating_add(src_char.len_utf8());
        for c in src_char.to_lowercase() {
            if is_ascii_word_char(c) {
                if word.is_empty() {
                    word_start = src_start;
                }
                word.push(c);
                word_end = src_end;
                prev_cjk = None;
                continue;
            }
            if !word.is_empty() {
                if !emit(&word, word_start..word_end, pos) {
                    return;
                }
                pos = pos.saturating_add(1);
                word.clear();
            }
            if is_cjk_char(c) {
                // バイグラム（直前が CJK の場合）を先に、続けて自身のユニグラムを出す。
                if let Some((p, p_start, _, p_pos)) = prev_cjk {
                    buf.clear();
                    buf.push(p);
                    buf.push(c);
                    if !emit(&buf, p_start..src_end, p_pos) {
                        return;
                    }
                }
                if !(remove_stopwords && is_cjk_stopword_unigram(c)) {
                    buf.clear();
                    buf.push(c);
                    if !emit(&buf, src_start..src_end, pos) {
                        return;
                    }
                }
                prev_cjk = Some((c, src_start, src_end, pos));
                pos = pos.saturating_add(1);
            } else {
                prev_cjk = None;
            }
        }
    }
    if !word.is_empty() {
        emit(&word, word_start..word_end, pos);
    }
}

/// 文書をトークン列と位置列へ同時に分割する（Issue #2114）。`with_params` の索引構築専用で、
/// ストップワード除去 ON の [`tokenize`] と同一のトークン列に、各トークンの位置を並べる。
fn tokenize_positioned(text: &str) -> (Vec<String>, Vec<u32>) {
    let cap = text.chars().count();
    let mut tokens: Vec<String> = Vec::with_capacity(cap);
    let mut positions: Vec<u32> = Vec::with_capacity(cap);
    walk_tokens_positioned(text, true, |token, _span, pos| {
        tokens.push(token.to_owned());
        positions.push(pos);
        true
    });
    (tokens, positions)
}

/// BM25 Okapi による疎検索インデックス。
///
/// `build`/`with_params` で構築後は不変（再構築のみで更新する）。storage/catalog との
/// 結線は行わず、コーパスは呼び出し側が渡した `(DocId, &str)` のスライスから構築する。
#[derive(Debug)]
pub struct SparseIndex {
    /// BM25 の項飽和度パラメータ（構築時に有限値かつ `>= 0.0` を検証済み）。
    k1: f64,
    /// BM25 の文書長正規化パラメータ（構築時に有限値かつ `[0.0, 1.0]` を検証済み）。
    b: f64,
    /// コーパス内の文書数（`N`）。
    doc_count: u32,
    /// 平均文書長（`avgdl`）。
    avg_doc_len: f64,
    /// build 時に確定した語彙の term 辞書（Issue #388・term インターニング）。
    /// `search`／`search_within` はクエリ語をこの辞書で [`TermId`] へ写像し、
    /// 未知語（辞書に存在しない語）は候補から除外する（旧実装で全文書 miss して
    /// `continue` していたのと同じ加算結果になる）。
    terms: TermDictionary,
    /// [`TermId`] を添字とする出現文書数（`df`）。長さは常に `terms.len()` と一致する
    /// 不変条件を構築時に保証する（[`SparseIndex::with_params`] が新語の intern の
    /// たびに `push(0)` して同期を保つ）。
    doc_freq: Vec<u32>,
    /// `DocId` → `doc_idx`（`doc_len`/`doc_ids`/`postings` 内側要素の添字空間）の
    /// 逆引き。[`VisibleBitmap::build`] が `visible_ids`（呼び出し元の可視集合）から
    /// 該当文書へ `O(1)` で辿るために使う（Issue #36 codex-review P0 指摘対応。
    /// 構築時の入力順は `doc_id` でソートされていないため、この逆引きマップなしでは
    /// 可視集合ごとに線形走査が必要になる）。値は構築時に自分自身が割り当てた
    /// `doc_idx` のみを保持するため、参照は常に `doc_len`/`doc_ids` の範囲内である
    /// （Issue #390。`docs: Vec<DocEntry>` 撤去後は `doc_len`/`doc_ids`/`postings` が
    /// 文書ごとの一次情報であり、`id_index` はそれらへの入口を担う）。
    id_index: HashMap<DocId, usize>,
    /// [`TermId`] を添字とする転置索引（posting list。Issue #389・親 Issue #386）。
    /// `postings[t]` は `TermId(t)` が出現する文書の `(doc_idx, tf)` の並びであり、
    /// `doc_idx` 昇順・重複なしを構築時（[`SparseIndex::with_params`]）に保証する
    /// （ソートは行わず、文書を `doc_idx` 昇順に処理する構築順序自体がこの不変条件を
    /// 満たす）。`doc_idx` は `doc_len`/`doc_ids` の添字と対応する。
    /// [`Self::score_by_postings`]（`search`/`search_within` が共有する 1 パス
    /// 走査コア。Issue #390）がクエリ語ごとにこの posting list だけを辿って
    /// スコアを積む主経路であり、全文書を線形走査する経路はもう存在しない。
    /// 不変条件は `postings.len() == doc_freq.len() == terms.ids.len()`・
    /// 各 `t` について `postings[t].len() == doc_freq[t] as usize`。
    postings: Vec<Vec<(u32, u32)>>,
    /// [`TermId`] を添字とする出現位置の配列（Issue #2114・SEARCH-12 ポインタ。D4 の位置
    /// モデル）。`positions[t]` は `postings[t]` の各要素 `(doc_idx, tf)` について、その
    /// 文書内での出現位置を `tf` 個ずつ posting の順に連結したもの（オフセット配列は持たず、
    /// 走査側が `tf` の累積で区間を切る）。`postings` とスコア経路は変えず、フレーズ一致
    /// （`sparse::phrase`）だけが読む。不変条件: `positions.len() == postings.len()`・
    /// 各 `t` で `positions[t].len() == Σ tf`・各文書の区間内で位置は厳密に昇順。
    positions: Vec<Vec<u32>>,
    /// `doc_idx` 添字の文書長配列（Issue #389）。`postings` 走査時に文書長正規化項
    /// （BM25 の `|d| / avgdl`）を `O(1)` で得るために [`Self::score_by_postings`]
    /// が参照する（Issue #390）。
    doc_len: Vec<u32>,
    /// `doc_idx` 添字の `DocId` 配列（Issue #389）。`postings` 走査結果
    /// （`doc_idx`）からスコア出力の `DocId` へ戻すために [`Self::score_by_postings`]
    /// が参照する（Issue #390）。
    doc_ids: Vec<DocId>,
    /// コーパス中に現れる文書長の相異なる値を昇順・重複なしで並べたクラス表
    /// （Issue #391）。tantivy 型の fieldnorm 量子化に相当するが、段数を
    /// 「相異なる文書長の個数」まで上げた厳密版であり、ロッシー量子化はしない
    /// （production はビット一致契約を維持する。256 段ロッシー量子化の実験は
    /// test-only。詳細は [`Self::score_by_postings`] のドキュメンテーション
    /// コメントを参照）。不変条件: 昇順・重複なし。
    len_classes: Vec<u32>,
    /// `doc_idx` 添字で、その文書長が [`Self::len_classes`] の何番目かを指す
    /// クラス添字配列（Issue #391）。不変条件:
    /// `len_classes[doc_len_class[i]] == doc_len[i]`（`i` は `doc_idx`）。
    doc_len_class: Vec<u32>,
    /// [`Self::score_pass`] が呼び出しのたびに新規確保していた `acc: Vec<f64>`
    /// （コーパス全体の doc_idx 空間・O(N) 確保）を索引の寿命内で再利用するための
    /// 有界プール（Issue #546。親 #545・#390 レビュー指摘「N 非依存化フォロー
    /// アップ」への対応）。`SparseIndexCache`（`sql/sparse_cache.rs`・Issue #357）
    /// が保持する `Arc<SparseIndex>` と寿命が一致するため、キャッシュされた索引が
    /// 世代整合を保っている間はこのプールも使い回され、索引が失効・再構築される
    /// たびにプールごと破棄される（「世代整合キャッシュ上の再利用バッファ」の
    /// 実体）。`&self`（非 `&mut self`）を維持したまま呼び出し間で状態を持ち越す
    /// ため `Mutex` で保護する（並行クエリからの同時アクセスに対応。プールが
    /// 枯渇した場合は [`Self::acquire_scratch`] が新規確保へフォールバックする
    /// ため正当性には影響しない）。
    scratch_pool: Mutex<Vec<ScoreScratch>>,
    /// 前方一致の展開用の順序付き語彙（Issue #2115・SEARCH-12 ポインタ。D6）。先頭が ASCII
    /// 単語文字の語だけを `(語, TermId)` で文字列の昇順に持つ。`terms`（`HashMap`）は変えず、
    /// `語幹*` の展開を範囲走査（`partition_point`）で行うための補助索引。CJK の前方一致は
    /// パーサが通常語へ落とす（D4）ため対象外で、CJK が語彙の大半を占めても重複保持が小さい。
    /// 不変条件: 昇順・重複なし・`terms` の ASCII 語と 1 対 1。
    prefix_vocab: Vec<(Box<str>, TermId)>,
}

/// [`SparseIndex::search_within`] が可視集合（`visible_ids`）を `doc_idx` 空間の
/// ビットマップへ変換したもの（Issue #390）。RLS 相当のテナント境界縮約契約
/// （[`SparseIndex::search_within`] のドキュメント参照）を保つため、統計の母数
/// （`n`・文書長合計）もこのビットマップ構築と同じ 1 回の走査で確定させ、
/// インデックス全体の `doc_count`/`avg_doc_len`/`doc_freq` を一切参照しない。
///
/// 確保量（`words` の長さ）はインデックス側の文書数（[`MAX_CORPUS_DOCS`] 以下で
/// 有界）でのみ決まり、untrusted な `visible_ids` の大きさで増幅しない
/// （`.claude/rules/coding-rust.md`: untrusted 入力によるリソース確保の増幅防止）。
struct VisibleBitmap {
    /// `doc_idx / 64` を添字とするビット集合。ビット `doc_idx % 64` が立っている
    /// 文書が可視。
    words: Vec<u64>,
    /// 可視文書数（`words` の立っているビット数）。
    n: usize,
    /// 可視文書の文書長合計（`avg_doc_len = total_len / n` の分子）。
    total_len: u64,
}

impl VisibleBitmap {
    /// `visible_ids` に含まれ、かつ `index` の構築時コーパスに実在する文書だけを
    /// ビットマップへ立てる。`id_index` に無い id（構築時のコーパス外）は
    /// `search_within` の既存契約どおり無音で無視する。
    fn build(index: &SparseIndex, visible_ids: &BTreeSet<DocId>) -> Self {
        let word_count = index.doc_ids.len().div_ceil(64);
        let mut words = vec![0u64; word_count];
        let mut n = 0usize;
        let mut total_len: u64 = 0;
        for id in visible_ids {
            // `id_index` は構築時に自分自身が割り当てた `doc_idx` のみを保持する
            // 不変条件があるため理論上 `words` の範囲内に収まるが、fail-closed の
            // ため `.get_mut()` で明示的に防御し、範囲外なら当該 id を無視する。
            let Some(&idx) = index.id_index.get(id) else {
                continue;
            };
            let Some(word) = words.get_mut(idx / 64) else {
                continue;
            };
            *word |= 1u64 << (idx % 64);
            n = n.saturating_add(1);
            let doc_len = index.doc_len.get(idx).copied().unwrap_or(0);
            total_len = total_len.saturating_add(u64::from(doc_len));
        }
        Self {
            words,
            n,
            total_len,
        }
    }

    /// `doc_idx` が可視ビットマップに含まれるか（範囲外は `false`。fail-closed）。
    fn contains(&self, doc_idx: u32) -> bool {
        let idx = doc_idx as usize;
        match self.words.get(idx / 64) {
            Some(word) => word & (1u64 << (idx % 64)) != 0,
            None => false,
        }
    }
}

/// [`SparseIndex::score_by_postings`] が母数（N・avgdl・df の算出方法）をどこから
/// 得るかを切り替えるための内部区分（Issue #390）。`search`（[`Self::All`]）は
/// インデックス全体を、`search_within`（[`Self::Visible`]）は可視ビットマップに
/// 縮約した部分集合を、それぞれ母数とする。
enum ScoreScope<'a> {
    /// インデックス全体（`self.doc_count`/`self.avg_doc_len`/`self.doc_freq`）を
    /// 母数とする（[`SparseIndex::search`]）。
    All,
    /// 可視ビットマップに縮約した部分集合を母数とする
    /// （[`SparseIndex::search_within`]。RLS 相当のテナント境界縮約契約）。
    Visible(&'a VisibleBitmap),
}

/// [`SparseIndex::score_pass`] が呼び出し間で使い回すスクラッチ一式（Issue #546。
/// 親 #545・#390 レビュー指摘「N 非依存化フォローアップ」への対応）。
///
/// 不変条件（プールへ返却する時点・プールから取り出した直後のいずれでも成立）:
/// `acc` の全要素は `0.0`、`touched`・`visible_hits`・`touched_classes` は空
/// （`clear()` 済みで capacity は保持）、`k1_len_norm_cache` は空。
/// [`SparseIndex::release_scratch`] がこの不変条件を回復してからプールへ戻す
/// （`touched` に記録された `doc_idx` のみをゼロ戻しする。「`acc[idx] == 0.0`
/// ⇔ 未加算」という [`SparseIndex::score_pass`] の不変条件により、`touched` に
/// 載っていない要素は既に `0.0` であるため、全要素走査によるゼロクリアと結果は
/// 同一）。`k1_len_norm_cache` も同様に `touched_classes`（初めて `entry` へ
/// 挿入した文書長クラスを記録した列。Issue #546 レビュー指摘・PR #565: 巨大な
/// `HashMap::clear()` は要素数でなく確保済み容量に比例するため、多数の相異なる
/// 文書長クラスにヒットしたクエリの後で 1 件しかヒットしない後続クエリが返却
/// されても容量分のコストを毎回払ってしまう）を使い、[`ScoreScratch::
/// clear_len_norm_cache`] でタッチしたキーだけを `remove` することで
/// `O(M)`（`M` = そのパスで実際にタッチした文書長クラス数）を維持する。
#[derive(Debug)]
struct ScoreScratch {
    acc: Vec<f64>,
    touched: Vec<u32>,
    visible_hits: Vec<(u32, u32)>,
    k1_len_norm_cache: HashMap<u32, f64>,
    /// `k1_len_norm_cache` へ初めて挿入した文書長クラスを挿入順に記録した列
    /// （Issue #546 レビュー指摘・PR #565）。`k1_len_norm_cache` のキー集合と
    /// 常に一致し、[`ScoreScratch::clear_len_norm_cache`] が `O(M)` での
    /// クリアに使う。
    touched_classes: Vec<u32>,
    /// `doc_idx` を添字とする「このパスで既に `touched` へ記録したか」の
    /// 世代カウンタ（`hnsw.rs::VisitedScratch` と同型の epoch 方式。codex-review
    /// 指摘・PR #565）。`touched.push` の要否を `acc[idx] == 0.0` の判定から
    /// 分離するために導入した。BM25 パラメータ（`k1`/`b`）は `with_params` で
    /// 有限・非負値のみ受理するが、`k1` に極端な有限値（例: `f64::MAX` 近傍）を
    /// 渡すと `kln`（`k1 * len_norm`）の計算が桁あふれし `f64::INFINITY` に
    /// なり得る。この場合 `denominator = f + kln` も無限大となり
    /// `idf * (numerator / denominator)` は有限値を無限大で割った `0.0` に
    /// 潰れるため、「`acc[idx] == 0.0` ⇔ 未加算」という設計時の不変条件
    /// （ドキュメンテーションコメント参照）が崩れ、同一 `doc_idx` への 2 回目
    /// 以降の加算（別のクエリ語がヒットした場合）でも `*slot == 0.0` が
    /// 成立し続けて `touched` へ重複記録され得る。`touched` はクエリ語数
    /// （untrusted な `query_ids.len()`）に比例して際限なく伸び、`n` を
    /// 超えて再確保され得る。以前（Issue #392 以前）は `ScorePass` の
    /// 呼び出しごとに新規確保・破棄されていたためこの超過分は都度解放
    /// されていたが、Issue #546 でスクラッチを索引の寿命内で使い回す
    /// プール方式へ変更したことで、この超過確保分がプールに保持され
    /// 続けてしまう（性能・保守性観点の指摘）。本フィールドは `acc` の値に
    /// 一切依存せず `doc_idx` ごとに厳密に 1 回だけ `touched` へ記録する
    /// ことを保証し、`touched.len()` を常に `n` 以下（[`ScoreScratch::new_for`]
    /// が確保した容量以下）に有界化する。
    visited_epoch: Vec<u64>,
    /// [`Self::visited_epoch`] の現在値。[`SparseIndex::score_pass`] の呼び出し
    /// ごとに 1 加算し、全要素を `O(N)` でクリアする代わりに世代比較で
    /// 「今回のパスで訪問済みか」を `O(1)` 判定する（`hnsw.rs::VisitedScratch`
    /// と同じ設計）。初期値 `0` は `visited_epoch`（`new_for` で全要素 `0`
    /// 初期化）と衝突しないよう、`score_pass` は使用前に必ず `wrapping_add(1)`
    /// してから比較する。
    epoch: u64,
}

impl ScoreScratch {
    /// 索引の文書数 `n`（`doc_ids.len()`）に対応する全ゼロの新規スクラッチを
    /// 確保する（[`SparseIndex::acquire_scratch`] のプール枯渇時フォールバック）。
    ///
    /// `touched`・`visible_hits` は `Vec::with_capacity(n)` で容量を `n` に
    /// 固定する（codex-review 指摘・PR #565: 空 `Vec::new()` から `push` で
    /// 成長させると標準ライブラリの倍々戦略により最終容量が要求長 `n` を
    /// 超えうる〔`n` が 2 の冪+1 付近だと最大で概ね `2n` 弱まで〕。両フィールドの
    /// 要素数は不変条件により常に `n` 以下（`touched` は相異なる `doc_idx` の
    /// 集合、`visible_hits` は 1 つの posting list 長で抑えられ、いずれも
    /// コーパス文書数 `n` を超えない）であるため、生成時に容量を `n` へ固定
    /// すれば以後の `push` で `n` を超えて再確保されることはなく、
    /// [`SparseIndex::approx_heap_bytes`] が計上する「容量 `n` ぶん」の
    /// 見積もりと実際の確保量が常に一致する。
    fn new_for(n: usize) -> Self {
        Self {
            acc: vec![0.0; n],
            touched: Vec::with_capacity(n),
            visible_hits: Vec::with_capacity(n),
            k1_len_norm_cache: HashMap::new(),
            // 相異なる文書長クラス数（`len_classes.len()`）は文書数 `n` を
            // 超えないため、`touched`/`visible_hits` と同じ理由で容量を `n` へ
            // 固定する（Issue #546 レビュー指摘・PR #565）。
            touched_classes: Vec::with_capacity(n),
            // `doc_idx` ごとに厳密 1 要素（`n` 個）の世代カウンタ。全要素 `0`
            // 初期化のため `epoch` の初期値 `0` とは呼び出し前の
            // `wrapping_add(1)` で確実に区別する（codex-review 指摘・PR #565）。
            visited_epoch: vec![0; n],
            epoch: 0,
        }
    }

    /// `k1_len_norm_cache` を `touched_classes` に記録された文書長クラスだけを
    /// `remove` することでクリアする（Issue #546 レビュー指摘・PR #565）。
    /// `HashMap::clear()` は確保済み容量（過去に大きく育った制御領域）に比例する
    /// 処理量になり得るため、実際にタッチしたキー集合（`O(M)`）だけを取り除く。
    /// `touched_classes` は `k1_len_norm_cache` のキー集合と 1 対 1 に対応する
    /// （挿入は必ず `entry().or_insert_with` の `Vacant` 分岐でのみ行われ、その
    /// 場でだけ `touched_classes` へ記録するため）。
    fn clear_len_norm_cache(&mut self) {
        for class in self.touched_classes.drain(..) {
            self.k1_len_norm_cache.remove(&class);
        }
    }
}

/// [`SparseIndex::score_pass`] の出力（Issue #392）。`acc` はコーパス全体の
/// `doc_idx` を添字とするスコアアキュムレータ（`> 0.0` の要素がヒット）、
/// `touched` はヒットした `doc_idx` を初回加算順（決定的）に記録した列。
/// この 2 つだけでスコア `> 0` の全候補を復元できるため、[`SparseIndex::
/// score_by_postings`]・[`SparseIndex::score_within`] の両方がこの中間表現を
/// 共有する。
///
/// 本体は索引が所有するスクラッチプール（[`ScoreScratch`]。Issue #546）を
/// 借りているだけのリースであり、`Drop` で索引のプールへ返却する
/// （[`SparseIndex::release_scratch`]）。`acc()`/`touched()` の借用アクセサ経由で
/// のみ中身を読む契約とし、値渡しでの分解（`let ScorePass { acc, touched } = ...`）
/// はしない（返却時のゼロ戻し・プール再利用ができなくなるため）。
struct ScorePass<'a> {
    index: &'a SparseIndex,
    // `release_scratch` を必ず一度だけ呼ぶため `Option` で保持し、`Drop` の中で
    // `take()` する（構造体分解を禁じているため通常のフィールドアクセスでは
    // 二重返却の心配はないが、`Drop::drop` は `&mut self` しか取れず所有権を
    // 動かせないため `Option::take` が必要になる）。
    scratch: Option<ScoreScratch>,
}

impl ScorePass<'_> {
    /// スコアアキュムレータ（`doc_idx` を添字。`> 0.0` の要素がヒット）への
    /// 借用アクセサ。
    fn acc(&self) -> &[f64] {
        match &self.scratch {
            Some(s) => s.acc.as_slice(),
            // `scratch` は `Drop` 実行時にのみ `None` になるため、通常の呼び出し
            // 経路では到達しない。到達した場合も空スライスへ fail-closed で
            // 倒し、`unwrap`/`expect`（`.claude/rules/coding-rust.md`）は使わない。
            None => &[],
        }
    }

    /// ヒットした `doc_idx` を初回加算順に記録した列への借用アクセサ。
    fn touched(&self) -> &[u32] {
        match &self.scratch {
            Some(s) => s.touched.as_slice(),
            None => &[],
        }
    }
}

impl Drop for ScorePass<'_> {
    fn drop(&mut self) {
        if let Some(scratch) = self.scratch.take() {
            self.index.release_scratch(scratch);
        }
    }
}

// `doc_idx`（`docs`/`doc_len`/`doc_ids`/`postings` 内側要素の添字）を `u32` で
// 表現する前提の静的検査（Issue #389）。[`MAX_CORPUS_DOCS`] が `u32::MAX` を
// 超えないことをコンパイル時に固定し、実行時の `doc_idx` 変換（`with_params` 内
// `u32::try_from`）が理論上失敗しないことの根拠とする。
const _: () = assert!(MAX_CORPUS_DOCS <= u32::MAX as usize);

/// [`SparseIndex::scratch_pool`] が同時に保持するスクラッチの上限本数（Issue #546）。
/// 並行クエリが同時に `score_pass` を呼ぶ本数を見込んだ有界値であり、これを
/// 超える同時実行数はプール枯渇として新規確保へフォールバックする（正当性には
/// 影響しない。`.claude/rules/coding-rust.md`: 無制限確保の禁止に従い、プール
/// 自体のサイズはこの定数で常に有界）。
const MAX_SCORE_SCRATCH_POOL: usize = 4;

impl SparseIndex {
    /// スクラッチプールから 1 本取り出す（[`Self::score_pass`] の入口）。プールが
    /// 空、または取り出したスクラッチの `acc` 長がこの索引の文書数と不一致
    /// （通常到達しないが、fail-closed のため明示的に検証する）の場合は
    /// [`ScoreScratch::new_for`] で新規確保する。ロックは pop のみの短時間保持。
    fn acquire_scratch(&self) -> ScoreScratch {
        let n = self.doc_ids.len();
        let popped = self
            .scratch_pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        match popped {
            Some(s) if s.acc.len() == n => s,
            _ => ScoreScratch::new_for(n),
        }
    }

    /// スクラッチをプールへ返却する（[`ScorePass::drop`] から呼ばれる）。返却前に
    /// `touched` に記録された `doc_idx` のみをゼロ戻しし（[`ScoreScratch`] の
    /// 不変条件を回復）、`touched`/`visible_hits` は `clear()`（capacity は
    /// 維持し次回確保コストを避ける）。`k1_len_norm_cache` は
    /// [`ScoreScratch::clear_len_norm_cache`] で `touched_classes` に記録された
    /// キーのみを `remove` する（Issue #546 レビュー指摘・PR #565: `HashMap::
    /// clear()` は確保済み容量に比例するため、多数の文書長クラスにヒットした
    /// 過去のクエリの容量が残ったまま `clear()` すると、後続の小さいクエリの
    /// 返却コストまで肥大化する）。プールが [`MAX_SCORE_SCRATCH_POOL`] に
    /// 達していれば返却せず破棄する（有界プール）。ロックは push のみの短時間
    /// 保持。
    fn release_scratch(&self, mut scratch: ScoreScratch) {
        for &doc_idx in &scratch.touched {
            if let Some(slot) = scratch.acc.get_mut(doc_idx as usize) {
                *slot = 0.0;
            }
        }
        scratch.touched.clear();
        scratch.visible_hits.clear();
        scratch.clear_len_norm_cache();

        let mut pool = self
            .scratch_pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if pool.len() < MAX_SCORE_SCRATCH_POOL {
            pool.push(scratch);
        }
    }

    /// Okapi BM25 の既定パラメータ（`k1 = 1.2`, `b = 0.75`）でインデックスを構築する。
    pub fn build(docs: &[(DocId, &str)]) -> Result<Self, SparseError> {
        Self::with_params(docs, DEFAULT_K1, DEFAULT_B)
    }

    /// `k1`・`b` を明示してインデックスを構築する。
    ///
    /// `k1`・`b` は構築時に検証する（有限値かつ `k1 >= 0.0`・`b` は `[0.0, 1.0]`）。
    /// 不正値は `search()` 内で NaN 伝播・ガード節（`if score > 0.0` 等）により
    /// サイレントな空結果へ落ちてしまい fail-open になるため、ここで拒否して
    /// fail-closed を保つ（`.claude/rules/coding-rust.md`）。
    ///
    /// `docs` はコーパス全体のサイズを制限する 4 段の上限検証を行う
    /// （`.claude/rules/coding-rust.md`: untrusted 入力の長さは上限検証してから
    /// 処理する）。文書数が [`MAX_CORPUS_DOCS`] を超える場合は走査に入る前に
    /// [`SparseError::TooManyDocs`] で拒否し、各文書のバイト長が [`MAX_DOC_BYTES`]
    /// を超える場合は該当文書の `tokenize()`（アロケーションを伴う）を呼ぶ前に
    /// [`SparseError::DocTooLong`] で拒否する。[`MAX_DOC_BYTES`]・[`MAX_CORPUS_DOCS`]
    /// は互いに独立な検証のため、それらの組み合わせだけでは総入力サイズを有界に
    /// 保てない（[`MAX_CORPUS_BYTES`] のコメント参照）。そのため、これまでに走査した
    /// 文書のバイト長の累計を `saturating_add` で求め、[`MAX_CORPUS_BYTES`] を超えた時点で
    /// 該当文書の `tokenize()` を呼ぶ前に [`SparseError::CorpusTooLarge`] で拒否する。
    /// さらに、バイト長上限は `tokenize()` が生成するトークン数（CJK 入力では
    /// バイト長に比例しない。[`MAX_CORPUS_TOKENS`] のコメント参照）を直接制限しない
    /// ため、該当文書の `tokenize()` を呼んだ直後（`term_freq`・`doc_freq` を構築する
    /// 前）に、それまでのトークン数の累計を `saturating_add` で求め、
    /// [`MAX_CORPUS_TOKENS`] を超えた時点で [`SparseError::TooManyTokens`] で拒否する。
    pub fn with_params(docs: &[(DocId, &str)], k1: f64, b: f64) -> Result<Self, SparseError> {
        if !bm25_params_in_domain(k1, b) {
            return Err(SparseError::InvalidParams { k1, b });
        }
        if docs.is_empty() {
            return Err(SparseError::EmptyCorpus);
        }
        if docs.len() > MAX_CORPUS_DOCS {
            return Err(SparseError::TooManyDocs {
                len: docs.len(),
                max: MAX_CORPUS_DOCS,
            });
        }

        // `DocId` → `doc_idx` の逆引き（Issue #389: `HashMap` 化）。従来の重複検査
        // 専用 `seen_ids: BTreeMap<DocId, ()>` を統合し、重複判定と逆引き構築を
        // 1 マップで兼ねる（`O(1)` 化・マップ 1 本削減）。挿入時に既存キーがあれば
        // 重複 `DocId` として拒否する（判定順序は従来どおり最優先）。
        let mut id_index: HashMap<DocId, usize> = HashMap::with_capacity(docs.len());
        let mut terms = TermDictionary::default();
        // [`TermId`] を添字とする df。`terms.intern()` が新語を採番するたびに `push(0)`
        // して `terms.len()` と長さを同期させる（Issue #388・term インターニング）。
        let mut doc_freq: Vec<u32> = Vec::new();
        // [`TermId`] を添字とする posting list（Issue #389）。`doc_freq` と同じ
        // タイミング（新語 intern 時）で `push(Vec::new())` し長さを同期させる。
        let mut postings: Vec<Vec<(u32, u32)>> = Vec::new();
        // [`TermId`] を添字とする出現位置（Issue #2114）。`postings` と同じタイミングで伸長する。
        let mut positions: Vec<Vec<u32>> = Vec::new();
        // `doc_idx` 添字の文書長・`DocId` 配列（Issue #389）。`id_index` と同時に
        // push するため長さは常に `doc_len_by_idx.len()` と一致する（Issue #390で
        // `DocEntry`/`entries` を撤去し、この 2 配列が文書ごとの一次情報になった）。
        let mut doc_len_by_idx: Vec<u32> = Vec::with_capacity(docs.len());
        let mut doc_ids_by_idx: Vec<DocId> = Vec::with_capacity(docs.len());
        let mut total_len: u64 = 0;
        // コーパス全体のバイト長累計。`saturating_add` によりオーバーフロー時は
        // `usize::MAX` へ飽和させる（この桁数の入力は現実的に想定しないが、
        // fail-closed のため未定義動作にせず必ず MAX_CORPUS_BYTES 超過として扱う）。
        let mut corpus_bytes_seen: usize = 0;
        // コーパス全体のトークン数累計（同様に `saturating_add` で飽和させる）。
        // バイト長の累計だけでは CJK 主体の入力によるトークン数・ヒープ確保数の
        // 増幅を捕捉できないため、こちらは各文書の `tokenize()` 直後に加算する
        // （MAX_CORPUS_TOKENS のコメント参照）。
        let mut corpus_tokens_seen: usize = 0;

        for &(doc_id, text) in docs {
            // `doc_len_by_idx.len()` は現在の文書に割り当てる `doc_idx`。挿入前に
            // 確定させることで、これから push する `doc_len_by_idx`/`doc_ids_by_idx`
            // すべてに対応する添字を一貫して使える（Issue #389）。
            let doc_idx = doc_len_by_idx.len();
            if id_index.insert(doc_id, doc_idx).is_some() {
                return Err(SparseError::DuplicateDocId(doc_id));
            }
            if text.len() > MAX_DOC_BYTES {
                return Err(SparseError::DocTooLong {
                    doc_id,
                    len: text.len(),
                    max: MAX_DOC_BYTES,
                });
            }
            corpus_bytes_seen = corpus_bytes_seen.saturating_add(text.len());
            if corpus_bytes_seen > MAX_CORPUS_BYTES {
                return Err(SparseError::CorpusTooLarge {
                    total: corpus_bytes_seen,
                    max: MAX_CORPUS_BYTES,
                });
            }

            let (doc_tokens, doc_positions) = tokenize_positioned(text);
            // `term_freq`/`doc_freq` の構築（トークンごとの追加ヒープ確保）に入る前に、
            // ここまでのトークン数累計を検証する（MAX_CORPUS_TOKENS のコメント参照）。
            corpus_tokens_seen = corpus_tokens_seen.saturating_add(doc_tokens.len());
            if corpus_tokens_seen > MAX_CORPUS_TOKENS {
                return Err(SparseError::TooManyTokens {
                    total: corpus_tokens_seen,
                    max: MAX_CORPUS_TOKENS,
                });
            }

            // `doc_tokens.len()` が `u32::MAX` を超える場合は `u32::MAX` に飽和させる
            // （文書長の理論上限を意図的に切り詰め、オーバーフローを未定義動作にしない）。
            // 実運用でこの桁数の単一文書は想定しないが、fail-closed のため checked に倒す。
            // `doc_tokens` を intern へ move する前に長さを退避する。
            let doc_len = u32::try_from(doc_tokens.len()).unwrap_or(u32::MAX);
            total_len = total_len.saturating_add(u64::from(doc_len));

            // 各トークンを辞書へ intern する（`String` の所有権を辞書へ move。
            // 既知語はヒープ確保なしで既存 `TermId` を返す）。新語が採番されるたびに
            // `doc_freq`／`postings` を `terms.len()` と同じ長さまで `push` して同期
            // させる（Issue #389: `postings` は `doc_freq` と対で伸長する）。
            let mut ids: Vec<(TermId, u32)> = Vec::with_capacity(doc_tokens.len());
            for (tok, pos) in doc_tokens.into_iter().zip(doc_positions) {
                let id = terms.intern(tok)?;
                if id.0 as usize >= doc_freq.len() {
                    doc_freq.push(0);
                    postings.push(Vec::new());
                    positions.push(Vec::new());
                }
                ids.push((id, pos));
            }

            // `TermId` 昇順にソートしランレングス圧縮して `(TermId, count)` へ畳み込む
            // （`term_freq` は各語 1 エントリへ圧縮済み・`TermId` 昇順）。同時に
            // `doc_freq`（この文書に出現した語のみ 1 件ずつ加算）を更新する。
            ids.sort_unstable();
            let mut term_freq: Vec<(TermId, u32)> = Vec::with_capacity(ids.len());
            // 位置は `(TermId, pos)` の全順序でソートされるため、語ごとに位置が昇順で並ぶ
            // （Issue #2114。タプルの全順序なので `sort_unstable` でも結果は決定的）。
            for (id, pos) in ids {
                if let Some(list) = positions.get_mut(id.0 as usize) {
                    list.push(pos);
                }
                match term_freq.last_mut() {
                    Some((last_id, count)) if *last_id == id => {
                        *count = count.saturating_add(1);
                    }
                    _ => {
                        term_freq.push((id, 1));
                        if let Some(df) = doc_freq.get_mut(id.0 as usize) {
                            *df = df.saturating_add(1);
                        }
                    }
                }
            }
            // ランレングス圧縮後、`term_freq` の容量は圧縮前の `ids.len()`
            // （元のトークン総数）分を保持したままになる。同一語を大量に繰り返す
            // 文書ではこの余剰容量が無視できず、term インターニングによる
            // メモリ削減の設計意図を損なうほか `approx_heap_bytes()` を過大評価させ
            // `PrefilterCache` の退避判定（Issue #357・sql/sparse_cache.rs）を
            // 必要以上に早める。実使用長まで縮めて解放する。
            term_freq.shrink_to_fit();

            // `doc_idx` は [`MAX_CORPUS_DOCS`] 以下（関数冒頭で検証済み）であり、
            // モジュール直下の `const _` 静的アサーションにより `MAX_CORPUS_DOCS` は
            // 常に `u32::MAX` 以下なので、この変換は理論上失敗しない。それでも
            // untrusted 入力由来の値を `unwrap`/`expect` で扱わない方針
            // （`.claude/rules/coding-rust.md`）に従い `unwrap_or` で飽和させる。
            let doc_idx_u32 = u32::try_from(doc_idx).unwrap_or(u32::MAX);
            // `term_freq` 確定後の文書は `doc_idx` 昇順に処理されるため、各
            // `postings[t]` へは常に `docs[doc_idx].doc_id` の位置を末尾 append する
            // だけで `doc_idx` 昇順・重複なし（同一文書は同一語を高々 1 回登録）を
            // 保つ（Issue #389・ソート不要な順序構築）。
            for &(id, tf) in &term_freq {
                if let Some(list) = postings.get_mut(id.0 as usize) {
                    list.push((doc_idx_u32, tf));
                }
            }

            doc_len_by_idx.push(doc_len);
            doc_ids_by_idx.push(doc_id);
        }

        // 語彙数分の `postings[t]` は `term_freq` と同じ理由（コメント参照）で成長
        // 余剰容量を持ち得るため、`approx_heap_bytes()` の過大評価を避けるため
        // 実使用長まで縮めて解放する（Issue #389）。
        for list in &mut postings {
            list.shrink_to_fit();
        }
        for list in &mut positions {
            list.shrink_to_fit();
        }

        // `doc_len_by_idx.len() == docs.len()` であり、`docs.is_empty()` は関数冒頭で
        // 拒否済みのため、ここでの `doc_count` は必ず 1 以上（0 除算にはならない）。
        let doc_count = u32::try_from(doc_len_by_idx.len()).unwrap_or(u32::MAX);
        let avg_doc_len = total_len as f64 / f64::from(doc_count);

        // 文書長クラス表の構築（Issue #391）。`doc_len_by_idx` の相異なる値を
        // 昇順・重複なしに並べたものが `len_classes`。各文書の `doc_len_class` は
        // その値が `len_classes` の何番目にあるかを二分探索で求める。`doc_len_by_idx`
        // の各値は必ず `len_classes` 内に存在する（`len_classes` はその値集合から
        // 直接作っているため）ので `binary_search` は理論上常に `Ok` を返すが、
        // untrusted 入力由来の値を `unwrap`/`expect` で扱わない方針
        // （`.claude/rules/coding-rust.md`）に従い、`Err` 側も添字 0 へ落とさず
        // 構築失敗として拒否する（fail-closed）。
        let mut len_classes: Vec<u32> = doc_len_by_idx.clone();
        len_classes.sort_unstable();
        len_classes.dedup();
        len_classes.shrink_to_fit();
        let mut doc_len_class: Vec<u32> = Vec::with_capacity(doc_len_by_idx.len());
        for &len in &doc_len_by_idx {
            let Ok(class_idx) = len_classes.binary_search(&len) else {
                // 構築ロジック上到達不能（`len_classes` は `doc_len_by_idx` の値集合
                // そのものから作られる）。到達した場合は不変条件が壊れている証拠
                // であり、無音の既定値へ倒さず明示的に拒否する。
                return Err(SparseError::LenClassBuildFailed);
            };
            let Ok(class_idx_u32) = u32::try_from(class_idx) else {
                return Err(SparseError::LenClassBuildFailed);
            };
            doc_len_class.push(class_idx_u32);
        }
        doc_len_class.shrink_to_fit();

        // 前方一致用の順序付き語彙（Issue #2115）。キーは一意なので安定ソートで全順序が決まる。
        let mut prefix_vocab: Vec<(Box<str>, TermId)> = terms
            .ids
            .iter()
            .filter(|(t, _)| t.chars().next().is_some_and(is_ascii_word_char))
            .map(|(t, &id)| (t.clone().into_boxed_str(), id))
            .collect();
        prefix_vocab.sort_by(|a, b| a.0.cmp(&b.0));
        prefix_vocab.shrink_to_fit();

        Ok(SparseIndex {
            k1,
            b,
            doc_count,
            avg_doc_len,
            terms,
            doc_freq,
            id_index,
            postings,
            positions,
            doc_len: doc_len_by_idx,
            doc_ids: doc_ids_by_idx,
            len_classes,
            doc_len_class,
            scratch_pool: Mutex::new(Vec::new()),
            prefix_vocab,
        })
    }

    /// クエリに対する BM25 スコア降順の Top-k を返す。
    ///
    /// クエリからトークンが 1 つも得られない場合（空文字列・記号のみ等）は空の `Vec` を
    /// 返す仕様とする（エラーにはしない。決定的な空結果への fail-closed 寄りの挙動）。
    /// `k == 0` の場合、または `k` がコーパスの文書数を上回る場合もそれぞれ空/全件で
    /// 決定的に扱う。スコア同点時は `doc_id` 昇順でタイブレークする（再現性確保）。
    ///
    /// 契約: 入力検証（クエリのバイト長・一意語数の上限検証）は `k == 0` の早期 return
    /// より常に優先される。すなわち `k == 0` であっても、クエリが [`MAX_QUERY_BYTES`]・
    /// [`MAX_QUERY_TERMS`] を超えていれば空の `Vec` ではなく `Err` を返す（fail-closed の
    /// 原則を優先し、入力検証の可否が `k` の値に依存しない統一された契約とする）。
    /// 実装上もバイト長検証 → `tokenize()` → 一意語数検証の全経路を終えてから
    /// `k == 0` を判定する順序を維持する。
    ///
    /// 計算量（Issue #390。可視ビットマップ＋posting 走査 1 パス化）: クエリ語の辞書
    /// lookup（[`TermId`] への写像）を走査に入る前に一度だけ `O(Q)` で行い
    /// （Issue #388・term インターニング）、その後は文書を線形走査せずクエリ語ごとに
    /// [`Self::postings`] だけを辿る（[`Self::score_by_postings`]）ため
    /// `O(Q + Σ_{t∈Q} |postings(t)| + M log k)`（`Σ|postings(t)|`: クエリ語の出現
    /// 延べ件数、`M`: スコア `> 0` の一致文書数）。クエリの一意語数 `Q` が大きいほど
    /// 走査対象の posting list 数が増えるため、`Q` が [`MAX_QUERY_TERMS`] を超える
    /// 場合は走査に入る前に [`SparseError::TooManyQueryTerms`] で拒否する。また
    /// `tokenize()` はクエリ全体を走査してアロケーションを行うため、一意語数の少ない
    /// 繰り返し入力（同じ語や区切り文字の反復）でもバイト長に比例したコストがかかる。
    /// そのためクエリのバイト長が [`MAX_QUERY_BYTES`] を超える場合は、`tokenize()` を
    /// 呼ぶ前に [`SparseError::QueryTooLong`] で拒否する（両者とも fail-closed。
    /// `.claude/rules/coding-rust.md`: untrusted 入力の長さは上限検証してから処理する）。
    /// 上記はスコア計算対象の走査量に関する計算量であり、[`Self::score_by_postings`]
    /// が使うスコアアキュムレータの確保・ゼロ初期化（`O(N)`。コーパス文書数）は
    /// 索引の寿命内で 1 回だけ発生し、以降の呼び出しは索引が保持するスクラッチ
    /// プールを再利用する（Issue #546。詳細は [`Self::score_by_postings`]・
    /// [`Self::score_pass`] のドキュメンテーションコメントを参照。Issue #390
    /// レビュー指摘の解消）。
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<ScoredDoc>, SparseError> {
        // アロケーションを伴う `tokenize()` を呼ぶ前に、バイト長（`str::len()`。文字列を
        // 走査しない `O(1)` 操作）で untrusted なクエリを検証する。一意語数の検証
        // （`MAX_QUERY_TERMS`）だけでは、同じ語・区切り文字を大量に繰り返す入力の
        // バイト長そのものの増大を防げないため、この検証が必要になる。
        if query.len() > MAX_QUERY_BYTES {
            return Err(SparseError::QueryTooLong {
                len: query.len(),
                max: MAX_QUERY_BYTES,
            });
        }

        let query_terms = tokenize(query);

        // 重複クエリ語の IDF を二重計上しないよう一意化する（順序は不問。BTreeMap で決定的に）。
        // `k == 0` の早期 return より前にここまで（バイト長・一意語数）の入力検証を
        // すべて終える。入力検証の可否が `k` の値に依存しない統一された契約とするため
        // （doc コメントの契約を参照）。
        let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
        for t in &query_terms {
            unique_terms.insert(t.clone(), ());
        }

        if unique_terms.len() > MAX_QUERY_TERMS {
            return Err(SparseError::TooManyQueryTerms {
                unique_terms: unique_terms.len(),
                max: MAX_QUERY_TERMS,
            });
        }

        // 入力検証をすべて終えた後で、決定的な空結果ケース（`k == 0`・クエリがトークン
        // を 1 つも含まない）を処理する。
        if k == 0 || query_terms.is_empty() {
            return Ok(Vec::new());
        }

        // クエリ語を辞書で `TermId` へ写像する（Issue #388・term インターニング）。
        // `unique_terms`（`String` 辞書順）の走査順をそのまま保つ `Vec<TermId>` を
        // 作ることで、下記スコア加算のループ順序＝f64 の加算順が旧実装（`BTreeMap<String,_>`
        // を辞書順走査）とビット一致する（スコアのビット一致は本モジュールの不変条件。
        // `make bench-hybrid-profile` の `replica_matches_real` 完全一致検証・
        // `sparse_cache_recall.rs` の cold/hot 等価性・Recall ゲート層 A の固定値
        // アサーションが依存する）。辞書に存在しない語（未知語）はここで除外する
        // （旧実装では全文書で `term_freq.get` が miss して `continue` していたのと
        // 加算結果は同一であり、フィルタ後にコーパス側で改めて miss 判定する必要はない）。
        let query_ids: Vec<TermId> = unique_terms
            .keys()
            .filter_map(|t| self.terms.lookup(t))
            .collect();

        if query_ids.is_empty() {
            return Ok(Vec::new());
        }

        Ok(self.score_by_postings(&query_ids, ScoreScope::All, k))
    }

    /// `idf(t) = ln( (N - df + 0.5) / (df + 0.5) + 1 )`。`+ 1` により常に非負となるため、
    /// 負の IDF に対する特別な補正処理は不要（モジュールコメントの式を参照）。母数
    /// `n`（文書数）を外部から指定できる版で、[`Self::score_by_postings`] が
    /// [`ScoreScope::All`] ではインデックス全体の `self.doc_count` を、
    /// [`ScoreScope::Visible`] では可視ビットマップに縮約した部分集合の文書数を、
    /// それぞれ `n` として渡すことで同一の IDF 計算式を共有する（Issue #390）。
    fn idf_for(n: f64, df: u32) -> f64 {
        let df = f64::from(df);
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    }

    /// [`Self::search`] の可視性縮約版（Issue #36 codex-review P0 指摘対応）。
    ///
    /// `search()` は文書数（N）・逆文書頻度（df）をインデックス全体（構築時の
    /// コーパス全体）から計算するため、呼び出し元が結果を `visible_ids` で事後
    /// フィルタするだけでは (a) `visible_ids` 外の文書が Top-k のヒープ選出枠
    /// （`k` 件のプール）を占有して可視文書を押し出す、(b) `doc_count`/`doc_freq`
    /// を通じて可視集合外の文書の内容・存在が可視文書の IDF・順位へ影響する、
    /// という 2 つの経路でテナント境界（RLS 相当）を弱める。件数・順位の違いから
    /// 他テナントの存在情報を推測できてしまうため、事後フィルタでは防げない
    /// （統計計算・候補選出そのものを可視集合へ限定する必要がある）。
    ///
    /// 本メソッドは統計（文書数・平均文書長・df）と Top-k のヒープ選出の両方を
    /// `visible_ids` に含まれる文書だけへ限定して計算し直す（Issue #390。可視
    /// ビットマップ [`VisibleBitmap`] へ縮約したうえで [`Self::score_by_postings`]
    /// を [`ScoreScope::Visible`] で呼び、`self.doc_count`/`self.avg_doc_len`/
    /// `self.doc_freq`〔インデックス全体の統計〕を一切参照しない）。`visible_ids` に
    /// 含まれるがインデックス構築時のコーパスに存在しない id は無視する
    /// （該当文書が単に存在しないものとして扱うだけであり、可視性判定そのものを
    /// 緩めるものではない。可視性判定自体は `visible_ids` を渡す呼び出し元
    /// （`hybrid.rs::hybrid_search`）の責務のまま変わらない）。
    ///
    /// クエリのバイト長・一意語数検証、`k == 0`・無トークンクエリでの空結果、
    /// 同点 `doc_id` 昇順のタイブレークといった契約は [`Self::search`] と同一。
    ///
    /// 計算量（Issue #390）: [`VisibleBitmap::build`] が可視集合を `doc_idx`
    /// ビットマップへ変換する `O(|visible_ids|)` に加え、[`Self::score_by_postings`]
    /// がクエリ語ごとに [`Self::postings`] を辿って可視ビットで絞り込むため
    /// `O(|visible_ids| + Σ_{t∈Q} |postings(t)| + M log k)`（[`Self::search`] の
    /// 計算量コメント参照）。可視集合の大きさに関わらずビットマップの確保量は
    /// インデックス側の文書数（[`MAX_CORPUS_DOCS`] 以下）で有界。
    /// [`Self::score_by_postings`] のスコアアキュムレータ確保・ゼロ初期化
    /// （`O(N)`）は索引の寿命内で 1 回だけ発生し、以降の呼び出しは索引が保持する
    /// スクラッチプールを再利用する（Issue #546）ため、`search_within` が hybrid
    /// の疎側再取得ループから 1 クエリあたり複数回呼ばれても（Issue #387）
    /// この確保は繰り返し発生しない（詳細は同メソッド・[`Self::score_pass`] の
    /// ドキュメンテーションコメント参照。Issue #390 レビュー指摘の解消）。
    pub fn search_within(
        &self,
        query: &str,
        k: usize,
        visible_ids: &BTreeSet<DocId>,
    ) -> Result<Vec<ScoredDoc>, SparseError> {
        // 入力検証（クエリのバイト長・一意語数上限）は `search()` と同一の順序・契約を
        // 維持する（`k == 0`・空可視集合より常に優先する）。
        if query.len() > MAX_QUERY_BYTES {
            return Err(SparseError::QueryTooLong {
                len: query.len(),
                max: MAX_QUERY_BYTES,
            });
        }

        let query_terms = tokenize(query);

        let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
        for t in &query_terms {
            unique_terms.insert(t.clone(), ());
        }

        if unique_terms.len() > MAX_QUERY_TERMS {
            return Err(SparseError::TooManyQueryTerms {
                unique_terms: unique_terms.len(),
                max: MAX_QUERY_TERMS,
            });
        }

        if k == 0 || query_terms.is_empty() || visible_ids.is_empty() {
            return Ok(Vec::new());
        }

        // クエリ語を辞書で `TermId` へ写像する（`search()` と同一の理由・同一の
        // 辞書順維持。Issue #388・term インターニング）。
        let query_ids: Vec<TermId> = unique_terms
            .keys()
            .filter_map(|t| self.terms.lookup(t))
            .collect();

        if query_ids.is_empty() {
            return Ok(Vec::new());
        }

        // 可視集合を `doc_idx` ビットマップへ変換すると同時に、統計の母数
        // （N・文書長合計）も可視部分集合のみから確定させる（Issue #390。
        // インデックス全体の `self.doc_count`/`self.avg_doc_len`/`self.doc_freq`
        // は一切参照しない。本メソッド追加の動機（b）を断つ）。
        let bitmap = VisibleBitmap::build(self, visible_ids);
        if bitmap.n == 0 {
            return Ok(Vec::new());
        }

        // 選出ロジック（ヒープ・タイブレーク）は `search()` と同一だが、母数を
        // 可視ビットマップに限定することで、`visible_ids` 外の文書が Top-k の
        // プールを占有できないようにする（本メソッド追加の動機（a））。
        Ok(self.score_by_postings(&query_ids, ScoreScope::Visible(&bitmap), k))
    }

    /// [`Self::search_within`] の「スコアリングと Top-k 選出を分離した」版
    /// （Issue #392）。呼び出し元が渡す `k` に対して 1 回限りの Top-k を返す
    /// `search_within` とは異なり、本メソッドは可視集合に縮約したスコア
    /// `> 0` の全候補を 1 回のスコア計算（[`Self::score_pass`]）で確定させた
    /// [`SparseScored`] を返し、呼び出し元は [`SparseScored::top`] を複数回
    /// 呼んで `k` を伸ばしながら Top-k を切り出せる。
    ///
    /// `hybrid.rs::sparse_refetch_loop`（疎側再取得ループ。境界の同点グループ
    /// 完全化のため `fetch_k` を倍増させながら同じクエリを再評価する）向けの
    /// 最適化であり、`search_within` を単体で使う既存の呼び出し元
    /// （`tests/hybrid.rs` の独立オラクル等）はそのまま `search_within` を
    /// 使い続けられる（両者は別経路のまま残し、出力の一致は単体テストで
    /// 固定する。両経路とも [`Self::score_pass`] を共有するため、ロジックの
    /// 二重実装ではない）。
    ///
    /// 入力検証（クエリのバイト長・一意語数上限）・空結果ケース
    /// （無トークンクエリ・空可視集合）・RLS 相当のテナント境界縮約契約
    /// （統計母数・候補選出を `visible_ids` へ限定し、インデックス全体の
    /// `self.doc_count`/`self.avg_doc_len`/`self.doc_freq` を参照しない）は
    /// [`Self::search_within`] と同一（同メソッドのドキュメンテーション
    /// コメント参照）。`k == 0` 判定は本メソッドには存在しない
    /// （`k` を受け取らないため。[`SparseScored::top`] 側が `k == 0` を
    /// 空 `Vec` として扱う）。
    pub fn score_within(
        &self,
        query: &str,
        visible_ids: &BTreeSet<DocId>,
    ) -> Result<SparseScored, SparseError> {
        // 索引自身の構築済みパラメータは `with_params` の定義域検査を通過済み。
        // クエリ時上書き専用の数値安全上限（`MAX_QUERY_K1`）は適用しない
        // （`k1 > 1e200` で構築した既存索引の挙動を変えない）。
        self.score_within_checked(query, visible_ids, self.params())
    }

    /// 構築時の `k1`・`b` を [`Bm25Params`] として返す。
    pub fn params(&self) -> Bm25Params {
        Bm25Params {
            k1: self.k1,
            b: self.b,
        }
    }

    /// [`Self::score_within`] のクエリ時パラメータ上書き版（Issue #2121。SEARCH-13）。
    ///
    /// `params` はスコア式の係数だけに使い、索引・文書統計（N・avgdl・df）は変えない。
    /// このためキャッシュ済み索引を `k1`・`b` が違うクエリ間で再利用でき、スクラッチの
    /// `k1 * len_norm` キャッシュはパスごとに消えるため、異なる `params` のパスが
    /// 混ざらない。`self.params()` を渡した結果は [`Self::score_within`] とビット一致する。
    /// 入口で `params` を再検証し（多層防御）、非有限スコアを検出したら
    /// [`SparseError::InvalidParams`] で fail-closed にする（NaN は `> 0.0` 判定で
    /// 黙って落ちるため）。
    pub fn score_within_with_params(
        &self,
        query: &str,
        visible_ids: &BTreeSet<DocId>,
        params: Bm25Params,
    ) -> Result<SparseScored, SparseError> {
        // 索引自身のパラメータ（`hybrid_search` 経路が `params()` で渡す）は
        // `with_params` で検証済みのため、クエリ時上限 `MAX_QUERY_K1` を再適用しない。
        let params = if params == self.params() {
            params
        } else {
            Bm25Params::new(params.k1, params.b)?
        };
        self.score_within_checked(query, visible_ids, params)
    }

    /// [`Self::score_within`]・[`Self::score_within_with_params`] 共通の本体。
    /// `params` は呼び出し側で検証済み（索引自身の値、または `Bm25Params::new` 経由）の前提。
    fn score_within_checked(
        &self,
        query: &str,
        visible_ids: &BTreeSet<DocId>,
        params: Bm25Params,
    ) -> Result<SparseScored, SparseError> {
        if query.len() > MAX_QUERY_BYTES {
            return Err(SparseError::QueryTooLong {
                len: query.len(),
                max: MAX_QUERY_BYTES,
            });
        }

        let query_terms = tokenize(query);

        let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
        for t in &query_terms {
            unique_terms.insert(t.clone(), ());
        }

        if unique_terms.len() > MAX_QUERY_TERMS {
            return Err(SparseError::TooManyQueryTerms {
                unique_terms: unique_terms.len(),
                max: MAX_QUERY_TERMS,
            });
        }

        let empty = || SparseScored {
            candidates: Vec::new(),
            sorted_len: 0,
        };

        if query_terms.is_empty() || visible_ids.is_empty() {
            return Ok(empty());
        }

        let query_ids: Vec<TermId> = unique_terms
            .keys()
            .filter_map(|t| self.terms.lookup(t))
            .collect();

        if query_ids.is_empty() {
            return Ok(empty());
        }

        let bitmap = VisibleBitmap::build(self, visible_ids);
        if bitmap.n == 0 {
            return Ok(empty());
        }

        let pass = self.score_pass(&query_ids, ScoreScope::Visible(&bitmap), params);
        let acc = pass.acc();
        let touched = pass.touched();

        // touched 順（決定的。posting 走査でヒットを初めて記録した順）のまま
        // 候補列を材料化する。`SparseScored::top` はこの列を全順序
        // （`Candidate::Ord`）に基づき整列するため、ここでの並び順自体は
        // 最終的な出力順序に影響しない。
        let mut candidates = Vec::with_capacity(touched.len());
        for &doc_idx in touched {
            let idx = doc_idx as usize;
            let Some(&score) = acc.get(idx) else {
                continue;
            };
            if !score.is_finite() {
                return Err(SparseError::InvalidParams {
                    k1: params.k1,
                    b: params.b,
                });
            }
            if score > 0.0 {
                let Some(&doc_id) = self.doc_ids.get(idx) else {
                    continue;
                };
                candidates.push(Candidate { score, doc_id });
            }
        }

        Ok(SparseScored {
            candidates,
            sorted_len: 0,
        })
    }

    /// `search`/`search_within` が共有する 1 パススコアリングコア（Issue #390。
    /// 旧実装は可視部分集合へ縮約した文書配列を全件線形走査し、クエリ語ごとに
    /// 各文書の `term_freq` を二分探索していたが、本関数はクエリ語ごとに
    /// [`Self::postings`]（該当語が実際に出現する文書だけの一覧）だけを辿る。
    ///
    /// `scope` が [`ScoreScope::All`] なら `search()`（インデックス全体を母数）、
    /// [`ScoreScope::Visible`] なら `search_within()`（可視ビットマップに縮約した
    /// 部分集合を母数。RLS 相当のテナント境界縮約契約）として振る舞う。
    ///
    /// スコアのビット一致契約: 文書ごとの加算順は「クエリ語の辞書順・その文書に
    /// 実際に出現した語のみ」であり、これは旧実装（文書を外側ループ・クエリ語を
    /// 内側ループで辞書順走査）と同一の加算列になるため f64 の結果はビット一致
    /// する（`query_ids` は呼び出し元で辞書順に構築済み。本関数はクエリ語を
    /// 外側ループにするが、各文書のアキュムレータへは term 発生順に足し込むため、
    /// 1 文書内で見た加算順序自体は変わらない）。
    ///
    /// 計算量（Issue #390 レビュー指摘・Issue #546 で解消）: 走査そのものは
    /// `postings`/可視ビットマップだけを辿るため `search`/`search_within` の
    /// ドキュメンテーションコメントが述べる `O(Q + Σ|postings(t)| + M log k)` に
    /// 従う。`acc`（doc_idx を添字とするスコアアキュムレータ）はコーパス全体の
    /// 文書数 `N`（`self.doc_ids.len()`）で確保・ゼロ初期化する必要があるが、
    /// これは索引の寿命内で 1 回だけ発生する（[`SparseIndex::score_pass`] が
    /// `&self`（非 `&mut self`）シグネチャを維持したまま索引が保持する有界
    /// スクラッチプール〔[`ScoreScratch`]〕を呼び出し間で使い回し、`touched` で
    /// タッチ済み要素のみをゼロ戻しして返却する）。`search_within` は hybrid の
    /// 疎側再取得ループから 1 クエリあたり複数回呼ばれる（Issue #387）が、
    /// 2 回目以降の呼び出しでは `O(N)` の確保・ゼロ初期化は発生しない。詳細・
    /// 設計判断は [`SparseIndex::scratch_pool`] のドキュメンテーションコメント・
    /// `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #546」節を参照
    /// （旧来の申し送り〔Issue #390 レビュー指摘・#392 領域〕はこの Issue で
    /// 解消した）。
    ///
    /// BM25 文書長正規化項のテーブル化・Top-k 選出方式（Issue #391。テーブル化を
    /// 遅延キャッシュ方式へ改めた経緯は PR #426 の codex-review 指摘参照）:
    /// ヒットごとの `len_norm`（`1-b+b*doc_len/avgdl` 由来の `k1 * len_norm`）は
    /// クエリ内で `avgdl`（母数 `n`／`avg_doc_len`。[`ScoreScope`] により変わる）が
    /// 確定した後は文書長のみに依存するため、文書長クラス（[`Self::len_classes`]）
    /// ごとに実際にヒットループでタッチした時点で 1 回だけ計算し `HashMap` へ
    /// キャッシュする（tantivy の fieldnorm 量子化に相当。ただし本実装はロッシー
    /// 量子化をせず「相異なる文書長 = クラス」の厳密写像とする。式・演算順は旧来の
    /// インライン計算と完全に同一であるため `denominator` は旧実装とビット一致
    /// する）。`len_classes` 全体を走査前に事前構築する方式では実際にヒットする
    /// クラス数（<= M）に関わらず `len_classes.len()`（最悪 `O(N)`）分の計算が
    /// 毎クエリ発生し、選択性の高いクエリで本関数の計算量契約から外れるため、
    /// 遅延計算へ変更した。Top-k 選出は [`TopKSelector`]（2k バッファ＋
    /// `select_nth_unstable_by`）を使う。いずれも `Candidate` の全順序によって
    /// 出力集合・順序が一意に決まるため、旧実装（インライン計算＋
    /// `BinaryHeap<Reverse<Candidate>>`）とは出力がビット一致・集合一致する。
    fn score_by_postings(
        &self,
        query_ids: &[TermId],
        scope: ScoreScope<'_>,
        k: usize,
    ) -> Vec<ScoredDoc> {
        let pass = self.score_pass(query_ids, scope, self.params());
        let acc = pass.acc();
        let touched = pass.touched();

        // Top-k 選出（Issue #391・[`TopKSelector`]）。`k_eff` はコーパス側で
        // 有界な `touched.len()` と untrusted な `k` の小さい方であり、
        // `TopKSelector` の確保量（`O(k_eff)`）を有界化する
        // （`.claude/rules/coding-rust.md`: untrusted 入力によるリソース確保の
        // 増幅防止）。
        let k_eff = k.min(touched.len());
        let mut selector = TopKSelector::new(k_eff);
        for &doc_idx in touched {
            let idx = doc_idx as usize;
            // `touched` は `acc.get_mut(idx)` が `Some` を返した添字のみを記録している
            // ため理論上範囲内だが、`.claude/rules/coding-rust.md`（`[]` 禁止）に従い
            // `.get()` で明示的に防御する（範囲外は fail-closed で候補から除外）。
            let Some(&score) = acc.get(idx) else {
                continue;
            };
            if score > 0.0 {
                let Some(&doc_id) = self.doc_ids.get(idx) else {
                    continue;
                };
                selector.push(Candidate { score, doc_id });
            }
        }

        selector
            .into_sorted()
            .into_iter()
            .map(|Candidate { score, doc_id }| ScoredDoc { doc_id, score })
            .collect()
    }

    /// [`Self::score_by_postings`]・[`Self::score_within`] が共有する 1 パス
    /// スコア計算コア（Issue #392）。クエリ語の posting 走査によりスコア
    /// `> 0` の全候補を求める処理そのものを切り出したもので、Top-k 選出方式
    /// （[`TopKSelector`] による 1 回限りの選出か、[`SparseScored::top`] による
    /// 複数ラウンドの前方一致な切り出しか）には関知しない。呼び出し元
    /// （`hybrid.rs::sparse_refetch_loop`）が疎側再取得ループで `fetch_k` を
    /// 倍増させながら同じクエリを何度も評価する際、本関数は 1 回だけ呼べば
    /// 済む（スコア > 0 の候補集合はクエリ・可視集合が変わらない限り不変な
    /// ため）。分離前は `search_within` を呼ぶたびに本関数相当の処理
    /// （`acc: Vec<f64>` の `O(N)` 確保・ゼロ初期化を含む）をラウンドごとに
    /// 繰り返していた。
    fn score_pass(
        &self,
        query_ids: &[TermId],
        scope: ScoreScope<'_>,
        params: Bm25Params,
    ) -> ScorePass<'_> {
        let (n, avg_doc_len) = match scope {
            ScoreScope::All => (f64::from(self.doc_count), self.avg_doc_len),
            ScoreScope::Visible(bitmap) => {
                (bitmap.n as f64, bitmap.total_len as f64 / bitmap.n as f64)
            }
        };

        // 索引の寿命内で使い回すスクラッチを取り出す（Issue #546）。
        // [`Self::acquire_scratch`]/[`Self::release_scratch`] の不変条件により
        // `acc` は全要素 `0.0`・`touched`/`visible_hits`/`k1_len_norm_cache` は
        // 空の状態で渡ってくるが、防御的に（呼び出し元・プール実装の変更に対する
        // 縮退耐性として）ここでも軽量なフィールド（`touched`/`visible_hits`/
        // `k1_len_norm_cache`。全要素走査を要する `acc` は対象外）を明示的に
        // クリアし、母数の異なる前回パスの値が混入しないことを構造的に保証する
        // （テナント縮約契約〔RLS 相当〕を壊さないための多重防御）。
        // `k1_len_norm_cache` は `HashMap::clear()` ではなく
        // [`ScoreScratch::clear_len_norm_cache`]（`touched_classes` に記録した
        // キーのみ `remove`）を使う（Issue #546 レビュー指摘・PR #565: この
        // 防御的クリアはパス毎に必ず実行されるため、`clear()` のまま残すと
        // 確保済み容量に比例するコストを毎クエリ払ってしまう）。
        let mut scratch = self.acquire_scratch();
        scratch.touched.clear();
        scratch.visible_hits.clear();
        scratch.clear_len_norm_cache();

        // BM25 の `k1 * (k1+1)` 側の定数。
        // 文書長クラス別 `k1 * len_norm` テーブル値はクエリ内で `avgdl` が確定した
        // 後は文書長のみに依存するため以前は `len_classes` 全体を走査して事前構築
        // していたが（Issue #391）、これは実際にヒットする文書長クラス数（<= M）に
        // 関わらず `len_classes.len()`（コーパス中の相異なる文書長数。最悪 `O(N)`）
        // 分の計算を常に行ってしまい、選択性の高いクエリで `search`/`search_within`
        // の `O(Q + Σ|postings(t)| + M log k)` 契約から外れるという codex-review
        // 指摘（PR #426）に対応し、ヒットループ内で実際にタッチしたクラスのみを
        // 遅延計算してキャッシュする方式へ変更した。式・演算順（`k1_len_norm[c]`
        // は旧実装の `self.k1 * len_norm` と同一算出）は不変のため `denominator` は
        // 引き続きビット一致する。`HashMap` のキー・値は決定的に定まる（`class` →
        // 一意な `f64`）ためスコア自体の決定性に影響しない。
        let k1_plus_1 = params.k1 + 1.0;

        // `doc_idx` を添字とするスコアアキュムレータ（f64。ビット一致契約のため
        // f32 化はしない）。`idf`・分子は数学的には常に正で `denominator > 0.0`
        // ガードを満たす限り各項の加算は正の寄与を持つ想定だったが（Issue #390
        // 設計判断）、極端な BM25 パラメータでは寄与が `0.0` へ丸め込まれ得る
        // ため（[`ScoreScratch::visited_epoch`] ドキュメンテーションコメント
        // 参照。codex-review 指摘・PR #565）、`touched` への記録要否は
        // `acc[idx] == 0.0` ではなく `visited_epoch` の世代比較で判定する。
        // これにより `touched` へ初めて触れた doc_idx だけを記録し、最終的な
        // Top-k 選出をヒットした文書（`M` 件）だけへ限定する。
        //
        // 索引の寿命内で使い回すスクラッチ（[`ScoreScratch`]。Issue #546）の
        // フィールドを直接使う。取り出した時点で全要素 `0.0`（Issue #390 の
        // 不変条件と同一）であり、以前は呼び出しのたびに `vec![0.0; N]` で
        // 新規確保・ゼロ初期化していた（`O(N)`）ものを、索引と同寿命のバッファ
        // 再利用へ置換した（詳細は [`SparseIndex::scratch_pool`] のドキュメント
        // コメント参照）。
        let ScoreScratch {
            acc,
            touched,
            visible_hits,
            k1_len_norm_cache,
            touched_classes,
            visited_epoch,
            epoch,
        } = &mut scratch;

        // このパス専用の世代へ進める（[`ScoreScratch::visited_epoch`]
        // ドキュメンテーションコメント参照。codex-review 指摘・PR #565）。
        // `wrapping_add` により `u64` を使い切っても panic せず次の世代へ
        // 循環する（到達しない分岐だが `.claude/rules/coding-rust.md` の
        // 整数演算規約に従い明示する）。
        *epoch = epoch.wrapping_add(1);
        let current_epoch = *epoch;

        for &term in query_ids {
            let Some(list) = self.postings.get(term.0 as usize) else {
                continue;
            };
            let (df, hits): (u32, &[(u32, u32)]) = match scope {
                ScoreScope::All => (
                    self.doc_freq.get(term.0 as usize).copied().unwrap_or(0),
                    list.as_slice(),
                ),
                ScoreScope::Visible(bitmap) => {
                    visible_hits.clear();
                    let mut count: u32 = 0;
                    for &(doc_idx, tf) in list {
                        if bitmap.contains(doc_idx) {
                            visible_hits.push((doc_idx, tf));
                            count = count.saturating_add(1);
                        }
                    }
                    (count, visible_hits.as_slice())
                }
            };
            let idf = Self::idf_for(n, df);
            for &(doc_idx, tf) in hits {
                let idx = doc_idx as usize;
                // `doc_len_class` は `doc_len` と同じ添字空間・同じ長さで build 時に
                // 同期構築される（不変条件）。`.get()` で範囲外を防御しつつ、
                // 対応するテーブル値をクラスごとに初回タッチ時のみ計算しキャッシュへ
                // 記録する（範囲外は fail-closed で当該ヒットを棄却する）。
                let Some(&class) = self.doc_len_class.get(idx) else {
                    continue;
                };
                let Some(&len) = self.len_classes.get(class as usize) else {
                    continue;
                };
                // `entry` の `Vacant` 分岐でのみ新規キーを挿入し、その場で
                // `touched_classes` へ記録する（Issue #546 レビュー指摘・
                // PR #565）。`touched_classes` が `k1_len_norm_cache` のキー集合と
                // 1 対 1 に対応するのはこの 1 箇所だけが挿入経路であるため。
                let kln = match k1_len_norm_cache.entry(class) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let value = params.k1
                            * (1.0 - params.b
                                + params.b * (f64::from(len) / avg_doc_len.max(f64::MIN_POSITIVE)));
                        entry.insert(value);
                        touched_classes.push(class);
                        value
                    }
                };
                let f = f64::from(tf);
                let numerator = f * k1_plus_1;
                let denominator = f + kln;
                // `f >= 1`（posting に載っている時点で出現回数は 1 以上）かつ
                // `k1 >= 0`・`len_norm >= 0`（`b` は `[0.0, 1.0]` に構築時検証済み）
                // であり、`denominator >= f >= 1.0` となる。このガードは実質到達
                // しないが `search`/旧 `search_within` と同一の防御として残す。
                if denominator > 0.0 {
                    let Some(slot) = acc.get_mut(idx) else {
                        continue;
                    };
                    // `touched` への記録要否は `acc[idx] == 0.0` の値そのものでは
                    // なく `visited_epoch` の世代比較で判定する（codex-review
                    // 指摘・PR #565。[`ScoreScratch::visited_epoch`] ドキュメン
                    // テーションコメント参照）。`idf * (numerator / denominator)`
                    // は数学的には常に正だが、極端な BM25 パラメータ
                    // （`k1` が桁あふれを起こす有限値等）では `denominator` が
                    // `f64::INFINITY` に発散し寄与が `0.0` へ丸め込まれ得るため、
                    // `*slot == 0.0` を「未加算」の代理指標として使うと同一
                    // `doc_idx` が複数回 `touched` へ積まれ、`n` を超えて
                    // 再確保され得る。`visited_epoch` は `acc` の値に依存せず
                    // `doc_idx` ごとに厳密 1 回だけ記録するためこれを防ぐ。
                    let Some(epoch_slot) = visited_epoch.get_mut(idx) else {
                        continue;
                    };
                    if *epoch_slot != current_epoch {
                        *epoch_slot = current_epoch;
                        touched.push(doc_idx);
                    }
                    *slot += idf * (numerator / denominator);
                }
            }
        }

        ScorePass {
            index: self,
            scratch: Some(scratch),
        }
    }

    /// `HashMap` の 1 エントリあたりのアロケータ管理領域・制御バイト分の概算
    /// オーバーヘッド（Issue #357 レビュー指摘対応・codex-review P1:
    /// [`Self::approx_heap_bytes`] が文字列長・キー/値サイズしか加算せず
    /// `MAX_SPARSE_CACHE_TOTAL_BYTES` の DoS 防御を過少計上で回避できた問題への
    /// 対応。本関数はそもそも「厳密なメモリ計測ではなく DoS 対策のための粗い
    /// 上限判定用」（下記コメント）のため、実確保量を下回らないよう安全側に
    /// 保守的な固定値を使う。`terms`（[`TermDictionary`]）・`id_index`
    /// （Issue #389 で `BTreeMap` から `HashMap` へ置換）双方の `HashMap` に
    /// 共用する）。
    const HASHMAP_ENTRY_OVERHEAD_BYTES: usize = 48;

    /// この `SparseIndex` が保持するヒープ確保分の概算バイト量（Issue #357・
    /// `sql/sparse_cache.rs::SparseIndexCache` の容量判定用）。`dictionary.rs::
    /// Dictionary::approx_heap_bytes` と同じ「厳密なメモリ計測ではなく DoS 対策の
    /// ための粗い上限判定用」という位置づけの概算。Issue #390 で `docs`
    /// （`Vec<DocEntry>`）を撤去したため以下のみを計上する: `terms`（辞書に保持
    /// する `String` 語彙のキー容量＋`HashMap` のエントリ・容量余裕分）・`doc_freq`
    /// （`Vec<u32>` の確保領域）・`postings`（外側 `Vec<Vec<_>>` の確保領域＋各内側
    /// `Vec<(u32, u32)>` の確保領域）・`doc_len`／`doc_ids`（各 `Vec` の確保領域）・
    /// `id_index`（`HashMap` のエントリ分＋容量余裕分。`terms` と同じ算出方式）。
    /// `String` は `len()` ではなく `capacity()` を使う（成長により確保容量が長さを
    /// 上回り得るため、下回らない側の概算にする）。
    pub fn approx_heap_bytes(&self) -> usize {
        let terms_keys: usize = self
            .terms
            .ids
            .keys()
            .map(|k| {
                k.capacity()
                    .saturating_add(std::mem::size_of::<TermId>())
                    .saturating_add(Self::HASHMAP_ENTRY_OVERHEAD_BYTES)
            })
            .fold(0usize, |acc, n| acc.saturating_add(n));
        // `HashMap` の容量（`capacity()`）は要素数より大きい（ロードファクタ余裕）。
        // その余裕分を保守的に別途加算し、キー実体の集計（`terms_keys`）だけでは
        // 捉えられない `HashMap` 自体の確保領域を過少計上しないようにする。
        let terms_table = self
            .terms
            .ids
            .capacity()
            .saturating_mul(std::mem::size_of::<(String, TermId)>().saturating_add(1));
        let terms = terms_keys.saturating_add(terms_table);
        let doc_freq: usize = self
            .doc_freq
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        // `postings` 外側 `Vec` の確保領域＋各内側 `Vec<(u32, u32)>` の確保領域
        // （Issue #389）。内側 `Vec` は build 時に `shrink_to_fit()` 済みだが、
        // 容量ベースで数える方針（下回らない側の概算）を他フィールドと統一する。
        let postings_outer = self
            .postings
            .capacity()
            .saturating_mul(std::mem::size_of::<Vec<(u32, u32)>>());
        let postings_inner: usize = self
            .postings
            .iter()
            .map(|list| {
                list.capacity()
                    .saturating_mul(std::mem::size_of::<(u32, u32)>())
            })
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let postings = postings_outer.saturating_add(postings_inner);
        let doc_len: usize = self
            .doc_len
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let doc_ids: usize = self
            .doc_ids
            .capacity()
            .saturating_mul(std::mem::size_of::<DocId>());
        // `id_index`（`HashMap`）は `terms.ids` と同じ算出方式（要素分＋容量分）。
        // キー・値は固定長（`String` のようなヒープ間接参照を持たない）ため
        // 要素分はエントリサイズ＋オーバーヘッドの単純積で足りる。
        let id_index_entries: usize = self.id_index.len().saturating_mul(
            std::mem::size_of::<(DocId, usize)>()
                .saturating_add(Self::HASHMAP_ENTRY_OVERHEAD_BYTES),
        );
        let id_index_table = self
            .id_index
            .capacity()
            .saturating_mul(std::mem::size_of::<(DocId, usize)>().saturating_add(1));
        let id_index = id_index_entries.saturating_add(id_index_table);
        // 文書長クラス表（Issue #391）: `len_classes`（相異なる文書長の値。通常
        // `doc_len` より小さいが上限は同じ `MAX_CORPUS_DOCS` で有界）・
        // `doc_len_class`（`doc_len` と同じ長さ・同じ要素サイズ `u32`）。
        let len_classes: usize = self
            .len_classes
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let doc_len_class: usize = self
            .doc_len_class
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        // スクラッチプール（[`ScoreScratch`]・Issue #546）の**決定的な上限値**を
        // 加算する。実際のプール充填率（プールが空・部分充填・満杯のいずれか）に
        // 依存させず、常に [`MAX_SCORE_SCRATCH_POOL`] 本ぶんの最悪確保量を計上する
        // ことで、`SparseIndexCache::insert`（`sql/sparse_cache.rs`・Issue #357）が
        // 挿入時点で 1 回だけ評価する容量判定（`MAX_SPARSE_CACHE_TOTAL_BYTES`）が
        // 実確保量を下回らないようにする（Issue #357 レビュー指摘・codex-review P1:
        // 過少計上によるこの容量判定の回避を防ぐ、という本関数の既存方針を踏襲）。
        // 1 本あたり: `acc`（`Vec<f64>`。長さ `N`）・`touched`/`visible_hits`/
        // `touched_classes` の容量（[`ScoreScratch::new_for`] が `Vec::
        // with_capacity(N)` で `N` に固定するため実確保量と一致する。Issue #546
        // レビュー指摘・PR #565: 以前は空 `Vec::new()` から `push` で育てており、
        // 標準ライブラリの倍々戦略により最終容量が `N` を超えうる〔`N` が
        // 2 の冪+1 付近だと最大で概ね `2N` 弱まで〕過小計上の余地があった）・
        // `k1_len_norm_cache`（`HashMap<u32, f64>`。相異なる文書長クラス数 =
        // `len_classes.len()` が上限）。`N = MAX_CORPUS_DOCS`（100,000）のとき
        // 1 本あたり概算 (8 + 4 + 4 + 8) × 100,000 ≈ 2.4 MB、
        // `MAX_SCORE_SCRATCH_POOL`（4 本）で約 9.6 MB／索引。
        // `MAX_SPARSE_CACHE_TOTAL_BYTES`（= `MAX_ARENA_TOTAL_BYTES` = 1 GiB）に
        // 対し十分小さい桁に収まる。
        let n = self.doc_ids.len();
        let scratch_acc = n.saturating_mul(std::mem::size_of::<f64>());
        let scratch_touched = n.saturating_mul(std::mem::size_of::<u32>());
        let scratch_visible_hits = n.saturating_mul(std::mem::size_of::<(u32, u32)>());
        let scratch_touched_classes = n.saturating_mul(std::mem::size_of::<u32>());
        // `touched` への重複記録防止用の世代カウンタ（[`ScoreScratch::
        // visited_epoch`]。codex-review 指摘・PR #565）。`touched` と同じ `n`
        // 要素だが `u64` のため 1 要素あたり 8 バイト。
        let scratch_visited_epoch = n.saturating_mul(std::mem::size_of::<u64>());
        let scratch_len_norm_cache = self.len_classes.len().saturating_mul(
            std::mem::size_of::<(u32, f64)>().saturating_add(Self::HASHMAP_ENTRY_OVERHEAD_BYTES),
        );
        let scratch_one = scratch_acc
            .saturating_add(scratch_touched)
            .saturating_add(scratch_visible_hits)
            .saturating_add(scratch_touched_classes)
            .saturating_add(scratch_visited_epoch)
            .saturating_add(scratch_len_norm_cache);
        let scratch_pool_upper_bound = scratch_one.saturating_mul(MAX_SCORE_SCRATCH_POOL);
        terms
            .saturating_add(doc_freq)
            .saturating_add(postings)
            .saturating_add(self.approx_position_heap_bytes())
            .saturating_add(self.approx_prefix_vocab_heap_bytes())
            .saturating_add(doc_len)
            .saturating_add(doc_ids)
            .saturating_add(id_index)
            .saturating_add(len_classes)
            .saturating_add(doc_len_class)
            .saturating_add(scratch_pool_upper_bound)
    }

    /// 前方一致用の順序付き語彙 `prefix_vocab`（Issue #2115）のヒープ確保分の概算バイト量。
    /// 外側 `Vec` の容量と各 `Box<str>` の長さを数える（下回らない側の概算）。
    pub fn approx_prefix_vocab_heap_bytes(&self) -> usize {
        let outer = self
            .prefix_vocab
            .capacity()
            .saturating_mul(std::mem::size_of::<(Box<str>, TermId)>());
        let inner: usize = self
            .prefix_vocab
            .iter()
            .map(|(t, _)| t.len())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        outer.saturating_add(inner)
    }

    /// 位置配列 `positions`（Issue #2114）のヒープ確保分の概算バイト量。
    ///
    /// [`Self::approx_heap_bytes`] が合算する内訳で、位置による追加メモリを単独で測る
    /// 計測用（docs の実測記録・`hybrid_profile` ベンチ）にも公開する。外側 `Vec` と各内側
    /// `Vec<u32>` を容量ベースで数える（他フィールドと同じ「下回らない側」の概算）。
    pub fn approx_position_heap_bytes(&self) -> usize {
        let outer = self
            .positions
            .capacity()
            .saturating_mul(std::mem::size_of::<Vec<u32>>());
        let inner: usize = self
            .positions
            .iter()
            .map(|list| list.capacity().saturating_mul(std::mem::size_of::<u32>()))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        outer.saturating_add(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 書き換え前の実装（参照オラクル）。walker 版との完全一致を対照テストで固定する。
    fn tokenize_reference(text: &str, remove_stopwords: bool) -> Vec<String> {
        let lower: Vec<char> = text.to_lowercase().chars().collect();
        let mut tokens: Vec<String> = Vec::with_capacity(lower.len());
        let mut word = String::new();
        let mut i = 0usize;
        while i < lower.len() {
            let c = lower[i];
            if is_ascii_word_char(c) {
                word.push(c);
                i += 1;
                continue;
            }
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            if is_cjk_char(c) {
                if !(remove_stopwords && is_cjk_stopword_unigram(c)) {
                    tokens.push(c.to_string());
                }
                if let Some(&next) = lower.get(i + 1) {
                    if is_cjk_char(next) {
                        let mut bigram = String::new();
                        bigram.push(c);
                        bigram.push(next);
                        tokens.push(bigram);
                    }
                }
            }
            i += 1;
        }
        if !word.is_empty() {
            tokens.push(word);
        }
        tokens
    }

    #[test]
    fn walker_matches_reference_tokenizer_on_edge_cases() {
        let cases = [
            "",
            "!!! ???",
            "Hello, world_1 foo",
            "東京都に行く",
            "Rust で東京タワー",
            "İstanbul",
            "\u{212A}elvin K",
            "straße",
            "e\u{301}cole",
            "ab😀cd",
            "cafés",
            "ΑΣ ΑΣΑ",
            "サーバー・東京",
            "ＡＢＣ１２３",
            "한글",
            "ｱｲｳ",
            "東京の都",
            "のはがを",
            "a東b京c",
            "x_1 y_2",
            "\u{3042}\u{3042}\u{3093}",
        ];
        for t in cases {
            for rs in [true, false] {
                assert_eq!(
                    tokenize_with_options(t, rs),
                    tokenize_reference(t, rs),
                    "{t:?} {rs}"
                );
            }
        }
    }

    #[test]
    fn walker_matches_reference_tokenizer_on_pseudo_random_input() {
        let alphabet: Vec<char> =
            "aAzZ09_ -!,東京都京の は日本語ーカタ・İ\u{212A}ßΣσé😀ＡｱＢ한\u{301}"
                .chars()
                .collect();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..3000 {
            let mut s = String::new();
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state >> 59) as usize + 1;
            for _ in 0..len {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let idx = ((state >> 33) as usize) % alphabet.len();
                s.push(alphabet[idx]);
            }
            for rs in [true, false] {
                assert_eq!(
                    tokenize_with_options(&s, rs),
                    tokenize_reference(&s, rs),
                    "{s:?} {rs}"
                );
            }
        }
    }

    // --- トークナイザ（境界値含む） ---

    // --- 位置モデル（Issue #2114・D4） ---

    fn positioned(text: &str) -> Vec<(String, u32)> {
        let mut v = Vec::new();
        walk_tokens_positioned(text, true, |t, _, p| {
            v.push((t.to_owned(), p));
            true
        });
        v
    }

    fn p(items: &[(&str, u32)]) -> Vec<(String, u32)> {
        items.iter().map(|&(t, p)| (t.to_owned(), p)).collect()
    }

    #[test]
    fn positions_ascii_words_count_one_unit_each() {
        assert_eq!(positioned("tokyo tower"), p(&[("tokyo", 0), ("tower", 1)]));
        assert_eq!(positioned("a-b"), p(&[("a", 0), ("b", 1)]));
        assert_eq!(positioned("a -- , b"), p(&[("a", 0), ("b", 1)]));
    }

    #[test]
    fn positions_cjk_bigram_takes_first_char_position() {
        assert_eq!(
            positioned("東京都"),
            p(&[("東", 0), ("東京", 0), ("京", 1), ("京都", 1), ("都", 2)])
        );
    }

    #[test]
    fn positions_stopword_char_consumes_a_position() {
        assert_eq!(
            positioned("東京の店"),
            p(&[
                ("東", 0),
                ("東京", 0),
                ("京", 1),
                ("京の", 1),
                ("の店", 2),
                ("店", 3)
            ])
        );
    }

    #[test]
    fn positions_mixed_ascii_and_cjk() {
        assert_eq!(
            positioned("cafe 東京"),
            p(&[("cafe", 0), ("東", 1), ("東京", 1), ("京", 2)])
        );
    }

    #[test]
    fn positions_empty_input_yields_nothing() {
        assert!(positioned("").is_empty());
        assert!(positioned("  ,. ").is_empty());
    }

    #[test]
    fn positions_property_matches_tokenizer_and_is_monotonic() {
        let alphabet: Vec<char> = "ab c-東京都の店ー,Z_9 ".chars().collect();
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..300 {
            let mut text = String::new();
            for _ in 0..40 {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                let r = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
                text.push(alphabet[(r >> 33) as usize % alphabet.len()]);
            }
            let got = positioned(&text);
            let toks: Vec<String> = got.iter().map(|(t, _)| t.clone()).collect();
            assert_eq!(toks, tokenize(&text));
            let units = text.chars().count() as u32;
            let mut last = 0;
            for (_, pos) in &got {
                assert!(*pos >= last, "positions must be non-decreasing");
                assert!(*pos < units);
                last = *pos;
            }
        }
    }

    #[test]
    fn build_positions_satisfy_invariants() {
        let docs: Vec<(DocId, &str)> = vec![
            (1, "a b a c a"),
            (2, "東京都の店 a"),
            (3, "b b"),
            (4, "zzz"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        assert_eq!(idx.positions.len(), idx.postings.len());
        for (list, pos) in idx.postings.iter().zip(&idx.positions) {
            let total: u32 = list.iter().map(|&(_, tf)| tf).sum();
            assert_eq!(pos.len(), total as usize);
            let mut off = 0usize;
            for &(_, tf) in list {
                let seg = &pos[off..off + tf as usize];
                assert!(seg.windows(2).all(|w| w[0] < w[1]));
                off += tf as usize;
            }
        }
    }

    #[test]
    fn approx_heap_bytes_accounts_positions() {
        let docs: Vec<(DocId, &str)> = vec![(1, "a b a c a"), (2, "b b b b")];
        let idx = SparseIndex::build(&docs).unwrap();
        let total_tokens: usize = 9;
        assert!(idx.approx_position_heap_bytes() >= 4 * total_tokens);
        assert!(idx.approx_heap_bytes() > idx.approx_position_heap_bytes());
    }

    #[test]
    fn tokenize_ascii_words() {
        assert_eq!(tokenize("Hello World"), vec!["hello", "world"]);
    }

    #[test]
    fn tokenize_empty_string_yields_no_tokens() {
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn tokenize_punctuation_only_yields_no_tokens() {
        assert!(tokenize("!!! ??? ...").is_empty());
    }

    #[test]
    fn tokenize_cjk_yields_unigrams_and_bigrams() {
        // 「東京都」→ ユニグラム(東,京,都) + バイグラム(東京,京都)
        let toks = tokenize("東京都");
        assert_eq!(toks, vec!["東", "東京", "京", "京都", "都"]);
    }

    #[test]
    fn tokenize_mixed_ascii_and_cjk() {
        let toks = tokenize("vector検索");
        assert_eq!(toks, vec!["vector", "検", "検索", "索"]);
    }

    // --- 対応範囲外文字の無音破棄（現挙動の固定。モジュールコメント参照） ---

    #[test]
    fn tokenize_fullwidth_alnum_yields_no_tokens() {
        // 全角英数字は ASCII 単語文字でも CJK レンジでもないため、無音で破棄される。
        assert!(tokenize("ＶＥＣＴＯＲ").is_empty());
    }

    #[test]
    fn tokenize_accented_latin_is_silently_dropped() {
        // アクセント付きラテン文字（'é'）は対応範囲外のため欠落する。単語末尾のケースだけ
        // では「分割」と「結合」の挙動を区別できないため、区別可能なケースは
        // tokenize_accented_latin_mid_word_splits_the_word で pin する。
        assert_eq!(tokenize("café"), vec!["caf"]);
    }

    #[test]
    fn tokenize_accented_latin_mid_word_splits_the_word() {
        // アクセント付きラテン文字が単語の途中に出現すると、その文字が欠落するだけでなく
        // その位置で単語が分割される（前後の ASCII 部分が結合されるのではない）。
        assert_eq!(tokenize("cafés"), vec!["caf", "s"]);
    }

    #[test]
    fn tokenize_hangul_yields_no_tokens() {
        assert!(tokenize("안녕").is_empty());
    }

    #[test]
    fn tokenize_halfwidth_katakana_yields_no_tokens() {
        // 半角カタカナは `is_cjk_char` のレンジ（全角カタカナ U+30A0..=U+30FF）に含まれない。
        assert!(tokenize("ﾃｽﾄ").is_empty());
    }

    // --- CJK ストップワード除去（TASK-105・対象ビヘイビア SEARCH-5） ---

    #[test]
    fn tokenize_particle_unigram_is_removed_by_default() {
        // 「本の」→ ユニグラム(本, の) + バイグラム(本の) のうち、助詞「の」の
        // 単独ユニグラムのみが既定（除去 ON）で出力から除かれる。
        let toks = tokenize("本の");
        assert_eq!(toks, vec!["本", "本の"]);
        assert!(!toks.contains(&"の".to_string()));
    }

    #[test]
    fn tokenize_with_options_remove_stopwords_false_keeps_particle_unigram() {
        // 除去 OFF（`remove_stopwords = false`）では助詞ユニグラムも保持される
        // （除去有無の比較測定のための対称 API。関連: SEARCH-5）。
        let toks = tokenize_with_options("本の", false);
        assert_eq!(toks, vec!["本", "本の", "の"]);
    }

    #[test]
    fn tokenize_particle_inside_bigram_is_preserved() {
        // 助詞文字が内容語の内部（バイグラム）に現れる場合は除去しない
        // （例: 「もの」の「の」。CJK コンテンツ自体を保持する方針）。
        let toks = tokenize("もの");
        assert_eq!(toks, vec!["もの"]);
        // 「も」「の」は単独ユニグラムとして除去されるが、バイグラム「もの」は残る。
        assert!(toks.contains(&"もの".to_string()));
        assert!(!toks.contains(&"も".to_string()));
        assert!(!toks.contains(&"の".to_string()));
    }

    #[test]
    fn tokenize_nakaguro_is_treated_as_separator() {
        // 中黒「・」は記号として単語区切りに扱われ、トークンを生成しない
        // （TASK-105 対応で is_cjk_char のレンジから除外）。
        let toks = tokenize("東京・大阪");
        assert!(!toks.iter().any(|t| t.contains('・')));
        // 区切りの前後にある「東京」「大阪」はそれぞれ独立に CJK トークン化される
        // （中黒をまたぐバイグラムは生成されない）。
        assert!(toks.contains(&"東京".to_string()));
        assert!(toks.contains(&"大阪".to_string()));
        assert!(!toks.iter().any(|t| t.contains("京・")));
        assert!(!toks.iter().any(|t| t.contains("・大")));
    }

    #[test]
    fn tokenize_choonpu_is_preserved_as_cjk_content() {
        // 長音符「ー」はカタカナ語の構成要素のため CJK 扱いのまま保持する
        // （例: 「サーバー」。中黒のみを除外し長音符は除外しない）。
        let toks = tokenize("サーバー");
        assert!(toks.contains(&"サー".to_string()));
        assert!(toks.contains(&"ーバ".to_string()));
        assert!(toks.contains(&"バー".to_string()));
    }

    #[test]
    fn tokenize_cjk_punctuation_still_yields_no_tokens() {
        // 句読点「、」「。」は is_cjk_char のレンジ外のため、TASK-105 前後で挙動は
        // 変わらずトークン化されない（現状挙動の固定）。
        assert!(tokenize("、。").is_empty());
    }

    // --- SparseIndex 構築・境界値 ---

    #[test]
    fn build_rejects_empty_corpus() {
        let docs: Vec<(DocId, &str)> = vec![];
        assert_eq!(
            SparseIndex::build(&docs).unwrap_err(),
            SparseError::EmptyCorpus
        );
    }

    #[test]
    fn build_rejects_duplicate_doc_id() {
        let docs = vec![(1u64, "alpha"), (1u64, "beta")];
        assert_eq!(
            SparseIndex::build(&docs).unwrap_err(),
            SparseError::DuplicateDocId(1)
        );
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let docs = vec![(1u64, "alpha beta"), (2u64, "gamma delta")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.search("", 10).unwrap().is_empty());
        assert!(idx.search("!!!", 10).unwrap().is_empty());
    }

    #[test]
    fn search_k_zero_returns_empty() {
        let docs = vec![(1u64, "alpha beta")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.search("alpha", 0).unwrap().is_empty());
    }

    #[test]
    fn search_k_larger_than_corpus_returns_all_matching() {
        let docs = vec![(1u64, "alpha beta"), (2u64, "alpha gamma")];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("alpha", 100).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_tie_breaks_by_doc_id_ascending() {
        // 2 文書が完全に同一内容（同スコア）になるようにする。
        let docs = vec![(2u64, "alpha beta"), (1u64, "alpha beta")];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert!((results[0].score - results[1].score).abs() < 1e-12);
        assert_eq!(results[0].doc_id, 1);
        assert_eq!(results[1].doc_id, 2);
    }

    #[test]
    fn search_selection_boundary_prefers_smaller_doc_id_on_tie() {
        // 4 文書すべてが同スコアになるようにし、k=2（コーパスの半分）で打ち切る。
        // ヒープによる Top-k 選出でも、同点の切り捨て境界で doc_id が小さい方を
        // 優先して残すこと（＝出力順のタイブレークだけでなく、選出自体の境界でも
        // 同じ優先順位が一貫して使われること）を確認する。
        let docs = vec![
            (30u64, "alpha beta"),
            (10u64, "alpha beta"),
            (40u64, "alpha beta"),
            (20u64, "alpha beta"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("alpha", 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].doc_id, 10);
        assert_eq!(results[1].doc_id, 20);
    }

    #[test]
    fn search_keyword_match_ranks_above_non_match() {
        let docs = vec![
            (1u64, "the quick brown fox jumps over the lazy dog"),
            (2u64, "vector databases store embeddings for search"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("vector embeddings", 10).unwrap();
        assert_eq!(results[0].doc_id, 2);
    }

    #[test]
    fn search_is_deterministic_across_repeated_calls() {
        let docs = vec![
            (1u64, "alpha beta gamma"),
            (2u64, "beta gamma delta"),
            (3u64, "gamma delta epsilon"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let first = idx.search("gamma delta", 10).unwrap();
        let second = idx.search("gamma delta", 10).unwrap();
        assert_eq!(first, second);
    }

    // --- search_within（Issue #36 codex-review P0 指摘対応: テナント境界縮約） ---

    #[test]
    fn search_within_excludes_invisible_docs_from_pool_occupation() {
        // [P0] `search()`（インデックス全体を母数に Top-k を選出する API）を事後
        // フィルタするだけでは、不可視文書が Top-k のプールを占有して可視文書を
        // 押し出す経路を防げない。doc_id=3 は "cat" を大量に繰り返すため
        // `search()` では 1 位を独占するが、可視集合 `{1, 2}` には含まれない。
        let docs = vec![
            (1u64, "cat"),
            (2u64, "cat"),
            (3u64, "cat cat cat cat cat cat cat cat cat cat"),
        ];
        let idx = SparseIndex::build(&docs).expect("build ok");

        // 旧実装が脆弱だったことの回帰確認: `search()` は k=1 で不可視の id=3 だけを
        // 返す（呼び出し元がこれを可視集合で事後フィルタすると空になり、可視文書
        // id=1・id=2 のどちらも返せない）。
        let legacy = idx.search("cat", 1).expect("search ok");
        assert_eq!(legacy[0].doc_id, 3);

        // `search_within` は id=3 を候補にすら含めないため、k=1 でも可視文書
        // （id=1・id=2 は同点のため doc_id 昇順で id=1）が正しく返る。
        let visible: BTreeSet<u64> = [1u64, 2u64].into_iter().collect();
        let scoped = idx
            .search_within("cat", 1, &visible)
            .expect("search_within ok");
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].doc_id, 1);
    }

    #[test]
    fn search_within_statistics_are_isolated_from_invisible_docs() {
        // [P0] 可視文書の IDF・avgdl（＝スコア）が、共有インデックスに含まれる
        // 不可視文書の有無・内容によって変化しないことを確認する（`doc_count`/
        // `doc_freq` を通じた統計汚染の防止）。同一の可視部分集合（id=1 のみが
        // "cat" を含む）に対し、共有インデックスへ大量の不可視文書（"cat" を含む
        // ものを含む）を追加しても `search_within` のスコアは不変であるべき。
        let small_docs = vec![(1u64, "cat"), (2u64, "dog")];
        let small_idx = SparseIndex::build(&small_docs).expect("build ok");
        let visible: BTreeSet<u64> = [1u64].into_iter().collect();
        let small_result = small_idx
            .search_within("cat", 10, &visible)
            .expect("search_within ok");

        let mut large_docs: Vec<(u64, &str)> = vec![(1u64, "cat"), (2u64, "dog")];
        let filler: Vec<(u64, &str)> = (100u64..150).map(|id| (id, "cat cat cat")).collect();
        large_docs.extend(filler.iter().copied());
        let large_idx = SparseIndex::build(&large_docs).expect("build ok");
        let large_result = large_idx
            .search_within("cat", 10, &visible)
            .expect("search_within ok");

        assert_eq!(
            small_result, large_result,
            "invisible corpus growth must not change visible-only score"
        );
    }

    #[test]
    fn search_within_ignores_visible_ids_absent_from_corpus() {
        // `visible_ids` に構築時のコーパスへ存在しない id が混ざっていても panic せず、
        // 単に無視されることを確認する（呼び出し元の可視集合が本インデックス外の id を
        // 含みうる契約。`hybrid.rs::hybrid_search` の呼び出し文脈を参照）。
        let docs = vec![(1u64, "cat"), (2u64, "dog")];
        let idx = SparseIndex::build(&docs).expect("build ok");
        let visible: BTreeSet<u64> = [1u64, 999u64].into_iter().collect();
        let result = idx
            .search_within("cat", 10, &visible)
            .expect("search_within ok");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].doc_id, 1);
    }

    #[test]
    fn search_within_empty_visible_ids_returns_empty() {
        let docs = vec![(1u64, "cat")];
        let idx = SparseIndex::build(&docs).expect("build ok");
        let visible: BTreeSet<u64> = BTreeSet::new();
        let result = idx
            .search_within("cat", 10, &visible)
            .expect("search_within ok");
        assert!(result.is_empty());
    }

    #[test]
    fn search_within_handles_visible_bitmap_word_boundary_at_64_docs() {
        // [P0 相当の境界確認] `VisibleBitmap`（Issue #390）は `doc_idx` を 64 件単位
        // の `u64` ワードへ詰める。ワード境界（doc_idx=63/64/65）をまたぐ可視集合で
        // 取りこぼし・誤ヒットが無いことに加え、統計（N・avgdl・df）がビットマップ
        // 経由でも正しく縮約されていることをスコアそのもので確認する。
        //
        // 全文書が同一本文（同点）だとメンバーシップ判定の bug がスコアへ現れず
        // 検出できないため（advisor 指摘）、`generate_mixed_corpus`（tf・doc_len が
        // 文書ごとに異なる決定的コーパス）を使い、境界をまたぐ可視集合の結果を
        // 参照実装（`reference_search_top_k`）とスコアのビット一致まで突き合わせる。
        let corpus = generate_mixed_corpus(100);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();

        // doc_id と doc_idx は挿入順に一致する。ワード境界（63/64/65）をまたぎ、
        // かつ境界外（62・66）を可視集合から意図的に外して縮約の効き目も見る。
        let visible: BTreeSet<u64> = [60u64, 61, 63, 64, 65, 67, 68].into_iter().collect();

        let mut ref_docs: BTreeMap<DocId, (BTreeMap<String, u32>, u32)> = BTreeMap::new();
        let mut ref_doc_freq: BTreeMap<String, u32> = BTreeMap::new();
        let mut ref_total_len: u64 = 0;
        for &(doc_id, text) in &refs {
            if !visible.contains(&doc_id) {
                continue;
            }
            let toks = tokenize(text);
            let mut tf: BTreeMap<String, u32> = BTreeMap::new();
            for t in &toks {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            let doc_len = u32::try_from(toks.len()).unwrap();
            ref_total_len += u64::from(doc_len);
            for t in tf.keys() {
                *ref_doc_freq.entry(t.clone()).or_insert(0) += 1;
            }
            ref_docs.insert(doc_id, (tf, doc_len));
        }
        let ref_n = visible.len() as f64;
        let ref_avg_doc_len = ref_total_len as f64 / ref_n;
        let stats = ReferenceCorpusStats {
            doc_freq_all: &ref_doc_freq,
            n: ref_n,
            avg_doc_len: ref_avg_doc_len,
            k1: DEFAULT_K1,
            b: DEFAULT_B,
        };

        for query in ["alpha beta", "検索", "gamma delta epsilon"] {
            let results = idx.search_within(query, 10, &visible).unwrap();
            let expected_top_k = reference_search_top_k(query, &ref_docs, &stats, 10);
            assert_eq!(
                results.iter().map(|s| s.doc_id).collect::<Vec<_>>(),
                expected_top_k.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
                "query={query:?}: ワード境界をまたぐ可視集合での返却 doc_id が参照実装と一致しない"
            );
            for scored in &results {
                let (tf, doc_len) = ref_docs.get(&scored.doc_id).unwrap();
                let expected = reference_bm25_score(query, tf, *doc_len, &stats);
                assert_eq!(
                    scored.score.total_cmp(&expected),
                    std::cmp::Ordering::Equal,
                    "query={query:?} doc_id={} score={} expected={}",
                    scored.doc_id,
                    scored.score,
                    expected
                );
            }
        }
    }

    #[test]
    fn search_within_query_too_long_is_rejected_before_visible_ids_check() {
        // [Low 相当の一貫性確認] `search()` と同じ契約: クエリのバイト長検証は
        // 可視集合の中身（空かどうか等）より常に優先される。
        let docs = vec![(1u64, "cat")];
        let idx = SparseIndex::build(&docs).expect("build ok");
        let visible: BTreeSet<u64> = BTreeSet::new();
        let long_query = "a".repeat(17 * 1024);
        let err = idx.search_within(&long_query, 10, &visible).unwrap_err();
        assert!(matches!(err, SparseError::QueryTooLong { .. }));
    }

    #[test]
    fn build_with_symbol_only_document_included_in_avg_doc_len() {
        // 記号のみの文書（トークン化結果が空）が混在するコーパスでも構築・検索が
        // 破綻しないことを確認する（avg_doc_len のゼロ割ガードの回帰検出）。
        // doc2 のトークン数は 0 のため avgdl = (2 tokens + 0 tokens) / 2 docs = 1.0 となり、
        // ゼロ割ガード（`avg_doc_len.max(f64::MIN_POSITIVE)`）を経由せず素の平均が使われる。
        let docs = vec![(1u64, "alpha beta"), (2u64, "!!! ???")];
        let idx = SparseIndex::build(&docs).unwrap();

        let idf_alpha = ((2.0 - 1.0 + 0.5) / (1.0 + 0.5) + 1.0f64).ln();
        let k1 = 1.2f64;
        let b = 0.75f64;
        let f = 1.0f64;
        let len_norm = 1.0 - b + b * (2.0 / 1.0);
        let expected = idf_alpha * (f * (k1 + 1.0)) / (f + k1 * len_norm);

        let results = idx.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
        assert!((results[0].score - expected).abs() < 1e-9);
    }

    #[test]
    fn build_with_all_symbol_only_documents_does_not_panic() {
        // 全文書がトークン化結果ゼロ（記号のみ）のコーパスは EmptyCorpus ではなく正常に
        // 構築でき、`avg_doc_len` は 0 除算なく 0.0 になる。ただし全文書の `term_freq` が
        // 空のため、search() 内でどのクエリ語も `doc.term_freq.get(term)` にヒットせず、
        // avg_doc_len を参照する `len_norm` の計算（0 除算ガード `max(f64::MIN_POSITIVE)`
        // 含む）自体は実行されない。ここで確認するのは「avg_doc_len == 0.0 のまま
        // 構築・検索が NaN・panic なく決定的に空を返す」ことであり、search() 内のゼロ割
        // ガード行そのものの実行はカバーしない。
        let docs = vec![(1u64, "!!!"), (2u64, "???")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.search("alpha", 10).unwrap().is_empty());
    }

    #[test]
    fn search_cjk_query_matches_expected_document() {
        let docs = vec![
            (1u64, "東京都渋谷区のカフェ"),
            (2u64, "大阪府大阪市のレストラン"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let results = idx.search("東京", 10).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].doc_id, 1);
    }

    // --- 手計算による BM25 期待値一致（数式レベルの一致を担保） ---

    #[test]
    fn idf_matches_hand_computed_formula() {
        // N=3, df("shared")=3（全文書に出現）→ idf = ln((3-3+0.5)/(3+0.5)+1)
        let docs = vec![(1u64, "shared"), (2u64, "shared"), (3u64, "shared")];
        let idx = SparseIndex::build(&docs).unwrap();
        let expected_idf = ((3.0 - 3.0 + 0.5) / (3.0 + 0.5) + 1.0f64).ln();
        // 単一トークン文書（doc_len == avgdl）なので長さ正規化項は 1 になり、
        // score = idf * (1 * (k1+1)) / (1 + k1) = idf。
        let results = idx.search("shared", 10).unwrap();
        for r in &results {
            assert!((r.score - expected_idf).abs() < 1e-9);
        }
    }

    #[test]
    fn score_matches_hand_computed_bm25_value() {
        // 単純な 2 文書コーパスで BM25 スコアを手計算し一致を確認する。
        // doc1: "alpha alpha beta" (len=3), doc2: "beta beta beta" (len=3)
        // avgdl = 3, N = 2
        let docs = vec![(1u64, "alpha alpha beta"), (2u64, "beta beta beta")];
        let idx = SparseIndex::build(&docs).unwrap();

        // query "alpha": df(alpha) = 1 → idf = ln((2-1+0.5)/(1+0.5)+1) = ln(2.0)
        let idf_alpha = ((2.0 - 1.0 + 0.5) / (1.0 + 0.5) + 1.0f64).ln();
        // doc1 での f(alpha, doc1) = 2, k1=1.2, b=0.75, doc_len=3, avgdl=3 → len_norm = 1
        let k1 = 1.2f64;
        let b = 0.75f64;
        let f = 2.0f64;
        let len_norm = 1.0 - b + b * (3.0 / 3.0);
        let expected = idf_alpha * (f * (k1 + 1.0)) / (f + k1 * len_norm);

        let results = idx.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
        assert!((results[0].score - expected).abs() < 1e-9);
    }

    #[test]
    fn score_matches_hand_computed_bm25_value_with_doc_len_ne_avgdl() {
        // doc_len != avgdl のケース（文書長正規化項 len_norm が 1 にキャンセルしない）で
        // 手計算値と一致することを確認する。b の符号反転・比率逆転・項の脱落等の回帰を
        // 検出するための境界値テスト。
        // doc1: "alpha alpha beta beta beta" (len=5)
        // doc2: "beta" (len=1)
        // avgdl = (5 + 1) / 2 = 3, N = 2
        let docs = vec![(1u64, "alpha alpha beta beta beta"), (2u64, "beta")];
        let idx = SparseIndex::build(&docs).unwrap();

        // query "alpha": df(alpha) = 1 → idf = ln((2-1+0.5)/(1+0.5)+1) = ln(2.0)
        let idf_alpha = ((2.0 - 1.0 + 0.5) / (1.0 + 0.5) + 1.0f64).ln();
        // doc1 での f(alpha, doc1) = 2, k1=1.2, b=0.75, doc_len=5, avgdl=3
        let k1 = 1.2f64;
        let b = 0.75f64;
        let f = 2.0f64;
        let len_norm = 1.0 - b + b * (5.0 / 3.0);
        let expected = idf_alpha * (f * (k1 + 1.0)) / (f + k1 * len_norm);

        let results = idx.search("alpha", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
        assert!((results[0].score - expected).abs() < 1e-9);
    }

    // --- パラメータ検証（fail-closed） ---

    #[test]
    fn with_params_rejects_nan_k1() {
        let docs = vec![(1u64, "alpha beta")];
        let err = SparseIndex::with_params(&docs, f64::NAN, 0.75).unwrap_err();
        assert!(matches!(err, SparseError::InvalidParams { .. }));
    }

    #[test]
    fn with_params_rejects_negative_k1() {
        let docs = vec![(1u64, "alpha beta")];
        let err = SparseIndex::with_params(&docs, -1.0, 0.75).unwrap_err();
        assert!(matches!(err, SparseError::InvalidParams { .. }));
    }

    #[test]
    fn with_params_rejects_b_out_of_unit_range() {
        let docs = vec![(1u64, "alpha beta")];
        assert!(matches!(
            SparseIndex::with_params(&docs, 1.2, 1.5).unwrap_err(),
            SparseError::InvalidParams { .. }
        ));
        assert!(matches!(
            SparseIndex::with_params(&docs, 1.2, -0.1).unwrap_err(),
            SparseError::InvalidParams { .. }
        ));
    }

    #[test]
    fn with_params_rejects_infinite_b() {
        let docs = vec![(1u64, "alpha beta")];
        let err = SparseIndex::with_params(&docs, 1.2, f64::INFINITY).unwrap_err();
        assert!(matches!(err, SparseError::InvalidParams { .. }));
    }

    #[test]
    fn with_params_accepts_boundary_b_values() {
        let docs = vec![(1u64, "alpha beta")];
        assert!(SparseIndex::with_params(&docs, 1.2, 0.0).is_ok());
        assert!(SparseIndex::with_params(&docs, 1.2, 1.0).is_ok());
        assert!(SparseIndex::with_params(&docs, 0.0, 0.75).is_ok());
    }

    // --- クエリの一意語数上限（fail-closed。MAX_QUERY_TERMS 境界） ---

    /// `n` 個の相異なる ASCII 単語トークン（`"w0" "w1" ... "w{n-1}"`）からなるクエリ文字列
    /// を組み立てる（`tokenize()` は ASCII 単語をそのまま 1 トークン化するため、
    /// 一意語数がちょうど `n` になる）。
    fn distinct_term_query(n: usize) -> String {
        (0..n)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn search_accepts_query_at_max_query_terms_boundary() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = distinct_term_query(MAX_QUERY_TERMS);
        // 上限ちょうどは拒否されない（一致文書がないため結果は空だが、Err にはならない）。
        assert!(idx.search(&query, 10).unwrap().is_empty());
    }

    #[test]
    fn search_rejects_query_exceeding_max_query_terms() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = distinct_term_query(MAX_QUERY_TERMS + 1);
        let err = idx.search(&query, 10).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyQueryTerms {
                unique_terms: MAX_QUERY_TERMS + 1,
                max: MAX_QUERY_TERMS,
            }
        );
    }

    // --- クエリのバイト長上限（fail-closed。MAX_QUERY_BYTES 境界） ---

    #[test]
    fn search_accepts_query_at_max_query_bytes_boundary() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        // ASCII 単語文字のみからなる 1 トークン（一意語数は 1 のため MAX_QUERY_TERMS
        // には抵触しない）で、バイト長だけを上限ちょうどにする。同じ語・区切り文字の
        // 反復で一意語数を増やさずにバイト長だけを膨らませる攻撃形を模した境界値。
        let query = "a".repeat(MAX_QUERY_BYTES);
        assert_eq!(query.len(), MAX_QUERY_BYTES);
        assert!(idx.search(&query, 10).unwrap().is_empty());
    }

    #[test]
    fn search_rejects_query_exceeding_max_query_bytes() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = "a".repeat(MAX_QUERY_BYTES + 1);
        let err = idx.search(&query, 10).unwrap_err();
        assert_eq!(
            err,
            SparseError::QueryTooLong {
                len: MAX_QUERY_BYTES + 1,
                max: MAX_QUERY_BYTES,
            }
        );
    }

    // --- `validate_query_bounds`（`search`/`search_within` 呼び出し前の単独検証。
    //     codex-review P1 指摘対応、PR #266） ---

    #[test]
    fn validate_query_bounds_accepts_query_within_limits() {
        assert!(validate_query_bounds("alpha beta").is_ok());
    }

    #[test]
    fn validate_query_bounds_rejects_query_exceeding_max_query_bytes() {
        let query = "a".repeat(MAX_QUERY_BYTES + 1);
        let err = validate_query_bounds(&query).unwrap_err();
        assert_eq!(
            err,
            SparseError::QueryTooLong {
                len: MAX_QUERY_BYTES + 1,
                max: MAX_QUERY_BYTES,
            }
        );
    }

    #[test]
    fn validate_query_bounds_rejects_query_exceeding_max_query_terms() {
        // `distinct_term_query` は本ファイル内の既存ヘルパー（一意語数上限テスト群で
        // 使用済み）。同じ生成規則を再利用し、値の食い違いを避ける。
        let query = distinct_term_query(MAX_QUERY_TERMS + 1);
        let err = validate_query_bounds(&query).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyQueryTerms {
                unique_terms: MAX_QUERY_TERMS + 1,
                max: MAX_QUERY_TERMS,
            }
        );
    }

    // --- 文書 1 件のバイト長上限（fail-closed。MAX_DOC_BYTES 境界） ---

    #[test]
    fn build_accepts_doc_at_max_doc_bytes_boundary() {
        let text = "a".repeat(MAX_DOC_BYTES);
        let docs = vec![(1u64, text.as_str())];
        assert!(SparseIndex::build(&docs).is_ok());
    }

    #[test]
    fn build_rejects_doc_exceeding_max_doc_bytes() {
        let text = "a".repeat(MAX_DOC_BYTES + 1);
        let docs = vec![(1u64, text.as_str())];
        let err = SparseIndex::build(&docs).unwrap_err();
        assert_eq!(
            err,
            SparseError::DocTooLong {
                doc_id: 1,
                len: MAX_DOC_BYTES + 1,
                max: MAX_DOC_BYTES,
            }
        );
    }

    // --- コーパス文書数上限（fail-closed。MAX_CORPUS_DOCS 境界） ---

    #[test]
    fn build_accepts_corpus_at_max_corpus_docs_boundary() {
        let docs: Vec<(DocId, &str)> = (0..MAX_CORPUS_DOCS as u64)
            .map(|id| (id, "alpha"))
            .collect();
        assert!(SparseIndex::build(&docs).is_ok());
    }

    #[test]
    fn build_rejects_corpus_exceeding_max_corpus_docs() {
        let docs: Vec<(DocId, &str)> = (0..(MAX_CORPUS_DOCS as u64 + 1))
            .map(|id| (id, "alpha"))
            .collect();
        let err = SparseIndex::build(&docs).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyDocs {
                len: MAX_CORPUS_DOCS + 1,
                max: MAX_CORPUS_DOCS,
            }
        );
    }

    // --- コーパス全体のバイト長上限（fail-closed。MAX_CORPUS_BYTES 境界） ---

    #[test]
    fn build_accepts_corpus_at_max_corpus_bytes_boundary() {
        // `MAX_DOC_BYTES` サイズの単一バッファを `MAX_CORPUS_BYTES / MAX_DOC_BYTES` 回
        // 参照するコーパス（合計はちょうど `MAX_CORPUS_BYTES`）を組み立てる。文書ごとに
        // 個別のバッファを確保せず、1 つの `text` を使い回すことで無駄な重複確保を避ける。
        let text = "a".repeat(MAX_DOC_BYTES);
        let doc_count = MAX_CORPUS_BYTES / MAX_DOC_BYTES;
        let docs: Vec<(DocId, &str)> = (0..doc_count as u64)
            .map(|id| (id, text.as_str()))
            .collect();
        assert!(SparseIndex::build(&docs).is_ok());
    }

    #[test]
    fn build_rejects_corpus_exceeding_max_corpus_bytes() {
        let text = "a".repeat(MAX_DOC_BYTES);
        let doc_count = MAX_CORPUS_BYTES / MAX_DOC_BYTES;
        let mut docs: Vec<(DocId, &str)> = (0..doc_count as u64)
            .map(|id| (id, text.as_str()))
            .collect();
        // 1 バイトの文書を追加し、合計をちょうど MAX_CORPUS_BYTES + 1 にする。
        docs.push((doc_count as u64, "a"));
        let err = SparseIndex::build(&docs).unwrap_err();
        assert_eq!(
            err,
            SparseError::CorpusTooLarge {
                total: MAX_CORPUS_BYTES + 1,
                max: MAX_CORPUS_BYTES,
            }
        );
    }

    // --- コーパス全体のトークン数上限（fail-closed。MAX_CORPUS_TOKENS 境界） ---

    /// 合計トークン数がちょうど `target_tokens` になるコーパスを、CJK の同一文字を
    /// 繰り返した文書の組み合わせで構築する（token/byte 比を意図的に大きくし、
    /// `MAX_CORPUS_BYTES` の遥か手前で `MAX_CORPUS_TOKENS` に到達させるため）。
    /// `tokenize()` は同一 CJK 文字（3 バイト/文字）を N 回繰り返した入力から常に
    /// `2 * N - 1` 個のトークン（ユニグラム N 個＋隣接バイグラム `N - 1` 個）を生成する
    /// ため、この式を逆算して任意の奇数トークン数を持つ文書を組み立てられる。
    /// 1 文書あたりの繰り返し数は `MAX_DOC_BYTES` を超えないよう上限を設ける。
    fn cjk_corpus_with_token_count(target_tokens: usize) -> Vec<(DocId, String)> {
        let max_n_per_doc = MAX_DOC_BYTES / 3;
        let max_tokens_per_doc = 2 * max_n_per_doc - 1;

        let mut docs: Vec<(DocId, String)> = Vec::new();
        let mut remaining = target_tokens;
        let mut doc_id: DocId = 0;
        while remaining > 0 {
            let mut t = remaining.min(max_tokens_per_doc);
            if t.is_multiple_of(2) {
                // 2 * N - 1 は常に奇数のため、偶数になった場合は 1 引いて次の文書へ
                // 繰り越す（ループはいずれ収束し、最終的な合計は target_tokens に一致）。
                t -= 1;
            }
            let n = t.div_ceil(2);
            docs.push((doc_id, "東".repeat(n)));
            doc_id += 1;
            remaining -= t;
        }
        docs
    }

    #[test]
    fn build_accepts_corpus_at_max_corpus_tokens_boundary() {
        let owned_docs = cjk_corpus_with_token_count(MAX_CORPUS_TOKENS);
        let docs: Vec<(DocId, &str)> = owned_docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
        assert!(SparseIndex::build(&docs).is_ok());
    }

    #[test]
    fn build_rejects_corpus_exceeding_max_corpus_tokens() {
        let owned_docs = cjk_corpus_with_token_count(MAX_CORPUS_TOKENS + 1);
        let docs: Vec<(DocId, &str)> = owned_docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
        let err = SparseIndex::build(&docs).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyTokens {
                total: MAX_CORPUS_TOKENS + 1,
                max: MAX_CORPUS_TOKENS,
            }
        );
    }

    // --- 入力検証の優先順位（fail-closed。k == 0 の早期 return より常に優先） ---

    #[test]
    fn search_rejects_overlong_query_even_when_k_is_zero() {
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = "a".repeat(MAX_QUERY_BYTES + 1);
        let err = idx.search(&query, 0).unwrap_err();
        assert_eq!(
            err,
            SparseError::QueryTooLong {
                len: MAX_QUERY_BYTES + 1,
                max: MAX_QUERY_BYTES,
            }
        );
    }

    #[test]
    fn search_rejects_too_many_query_terms_even_when_k_is_zero() {
        // バイト長は上限内でも一意語数が MAX_QUERY_TERMS を超えるクエリは、k == 0 でも
        // TooManyQueryTerms を返す（tokenize → 一意化 → MAX_QUERY_TERMS 検証を終えてから
        // k == 0 の早期 return を処理する順序の回帰検出）。
        let docs = vec![(1u64, "alpha")];
        let idx = SparseIndex::build(&docs).unwrap();
        let query = distinct_term_query(MAX_QUERY_TERMS + 1);
        let err = idx.search(&query, 0).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyQueryTerms {
                unique_terms: MAX_QUERY_TERMS + 1,
                max: MAX_QUERY_TERMS,
            }
        );
    }

    // --- スコアスクラッチプール（Issue #546。acc: Vec<f64> の索引寿命内再利用） ---

    #[test]
    fn scratch_reuse_matches_fresh_index_bit_exact() {
        // 同一索引を交互に異なるクエリ・可視集合（部分可視・全可視・未知語のみ・
        // 空）で複数回叩き、都度 `SparseIndex::build` した「使い捨て」索引の結果と
        // ビット一致することを確認する。プールからの取り出し・返却（ゼロ戻し）が
        // 加算結果へ影響しないことの根拠。
        let docs: Vec<(DocId, &str)> = vec![
            (1, "alpha beta gamma"),
            (2, "beta gamma delta"),
            (3, "gamma delta epsilon"),
            (4, "delta epsilon alpha"),
            (5, "epsilon alpha beta"),
        ];
        let reused = SparseIndex::build(&docs).unwrap();

        let all_ids: BTreeSet<DocId> = docs.iter().map(|(id, _)| *id).collect();
        let partial_ids: BTreeSet<DocId> = [1u64, 3, 5].into_iter().collect();
        let cases: Vec<(&str, &BTreeSet<DocId>)> = vec![
            ("alpha beta", &all_ids),
            ("gamma", &partial_ids),
            ("zzz_unknown_term", &all_ids),
            ("delta epsilon", &partial_ids),
            ("alpha", &all_ids),
        ];

        for (query, visible) in &cases {
            let fresh = SparseIndex::build(&docs).unwrap();

            let reused_search = reused.search(query, 10).unwrap();
            let fresh_search = fresh.search(query, 10).unwrap();
            assert_eq!(
                reused_search
                    .iter()
                    .map(|d| (d.doc_id, d.score.to_bits()))
                    .collect::<Vec<_>>(),
                fresh_search
                    .iter()
                    .map(|d| (d.doc_id, d.score.to_bits()))
                    .collect::<Vec<_>>(),
                "search が使い捨て索引とビット一致しない: query={query}"
            );

            let reused_within = reused.search_within(query, 10, visible).unwrap();
            let fresh_within = fresh.search_within(query, 10, visible).unwrap();
            assert_eq!(
                reused_within
                    .iter()
                    .map(|d| (d.doc_id, d.score.to_bits()))
                    .collect::<Vec<_>>(),
                fresh_within
                    .iter()
                    .map(|d| (d.doc_id, d.score.to_bits()))
                    .collect::<Vec<_>>(),
                "search_within が使い捨て索引とビット一致しない: query={query}"
            );

            let mut reused_scored = reused.score_within(query, visible).unwrap();
            let mut fresh_scored = fresh.score_within(query, visible).unwrap();
            assert_eq!(
                reused_scored
                    .top(10)
                    .iter()
                    .map(|d| (d.doc_id, d.score.to_bits()))
                    .collect::<Vec<_>>(),
                fresh_scored
                    .top(10)
                    .iter()
                    .map(|d| (d.doc_id, d.score.to_bits()))
                    .collect::<Vec<_>>(),
                "score_within+top が使い捨て索引とビット一致しない: query={query}"
            );
        }
    }

    #[test]
    fn scratch_pool_returns_all_zero_and_bounded() {
        // ヒットありクエリを複数回実行した後、プール内に残る全スクラッチの
        // `acc` が全ゼロ・`touched`/`visible_hits`/`k1_len_norm_cache` が空・
        // プール長が MAX_SCORE_SCRATCH_POOL 以下であることを検証する
        // （release_scratch がゼロ戻し・clear を確実に行っている根拠）。
        let docs: Vec<(DocId, &str)> = vec![(1, "alpha beta"), (2, "beta gamma")];
        let idx = SparseIndex::build(&docs).unwrap();

        for _ in 0..5 {
            let _ = idx.search("alpha beta gamma", 10).unwrap();
        }

        let pool = idx.scratch_pool.lock().unwrap();
        assert!(pool.len() <= MAX_SCORE_SCRATCH_POOL);
        for scratch in pool.iter() {
            assert!(
                scratch.acc.iter().all(|&v| v == 0.0),
                "プールへ返却されたスクラッチの acc が全ゼロでない"
            );
            assert!(scratch.touched.is_empty());
            assert!(scratch.visible_hits.is_empty());
            assert!(scratch.k1_len_norm_cache.is_empty());
            assert!(scratch.touched_classes.is_empty());
        }
    }

    #[test]
    fn extreme_bm25_params_do_not_duplicate_touched_or_grow_scratch_beyond_corpus_size() {
        // codex-review 指摘（Issue #546・PR #565）: `k1` に桁あふれを起こす
        // 極端な有限値（`b=1.0` かつ文書長 > 平均文書長で `kln` が
        // `f64::INFINITY` へ発散する組み合わせ）を渡すと、`idf * (numerator /
        // denominator)` が `0.0` へ丸め込まれ、「`acc[idx] == 0.0` ⇔ 未加算」
        // という旧来の判定方法が崩れる。この場合でも `touched`（延いては
        // 索引の寿命内で使い回すスクラッチプールが保持する容量）が文書数
        // `n` を超えて成長しない（`visited_epoch` による分離。[`ScoreScratch::
        // visited_epoch`] 参照）ことと、Top-k 出力に同一 `doc_id` の重複が
        // 生じないことを固定する。
        let docs: Vec<(DocId, &str)> = vec![
            // 複数の相異なるクエリ語すべてが同一文書にヒットする構成
            // （旧実装ではこの文書の doc_idx が touched へ語数分だけ重複
            // push され得た）。
            (1, "alpha beta gamma delta epsilon zeta eta theta"),
            (2, "alpha"),
        ];
        let idx = SparseIndex::with_params(&docs, 1e308, 1.0).unwrap();

        for _ in 0..3 {
            let results = idx
                .search("alpha beta gamma delta epsilon zeta eta theta", 10)
                .unwrap();
            let doc_ids: Vec<DocId> = results.iter().map(|d| d.doc_id).collect();
            let unique: BTreeSet<DocId> = doc_ids.iter().copied().collect();
            assert_eq!(
                doc_ids.len(),
                unique.len(),
                "Top-k 出力に同一 doc_id の重複がある: {doc_ids:?}"
            );
        }

        let n = docs.len();
        let pool = idx.scratch_pool.lock().unwrap();
        for scratch in pool.iter() {
            assert!(
                scratch.touched.capacity() <= n,
                "touched の容量が文書数 n={n} を超えて再確保された: capacity={}",
                scratch.touched.capacity()
            );
        }
    }

    #[test]
    fn concurrent_queries_on_shared_index_are_bit_identical() {
        // MAX_SCORE_SCRATCH_POOL を超えるスレッド数で同一索引を同時に叩き、
        // プール枯渇時の新規確保フォールバックを踏んでもなお単一スレッド結果と
        // ビット一致することを確認する（Mutex による排他・fail-closed な長さ
        // 検証のいずれもスコアの正当性を損なわない）。
        use std::sync::Arc;
        use std::thread;

        let docs: Vec<(DocId, &str)> = (0..50)
            .map(|i| {
                let text = match i % 3 {
                    0 => "alpha beta gamma",
                    1 => "beta gamma delta",
                    _ => "gamma delta epsilon",
                };
                (i as DocId, text)
            })
            .collect();
        let idx = Arc::new(SparseIndex::build(&docs).unwrap());

        let expected = idx.search("alpha gamma epsilon", 20).unwrap();
        let expected_bits: Vec<(DocId, u64)> = expected
            .iter()
            .map(|d| (d.doc_id, d.score.to_bits()))
            .collect();

        let thread_count = MAX_SCORE_SCRATCH_POOL + 2;
        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                let idx = Arc::clone(&idx);
                thread::spawn(move || {
                    let result = idx.search("alpha gamma epsilon", 20).unwrap();
                    result
                        .iter()
                        .map(|d| (d.doc_id, d.score.to_bits()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        for handle in handles {
            let actual_bits = handle.join().unwrap();
            assert_eq!(actual_bits, expected_bits);
        }
    }

    #[test]
    fn acquire_rejects_scratch_with_mismatched_length() {
        // プールへ長さ不一致の ScoreScratch を意図的に混入させても、
        // acquire_scratch が fail-closed に破棄し新規確保へフォールバックする
        // ため、結果は fresh 索引と一致する。
        let docs: Vec<(DocId, &str)> = vec![(1, "alpha beta"), (2, "beta gamma")];
        let idx = SparseIndex::build(&docs).unwrap();

        {
            let mut pool = idx.scratch_pool.lock().unwrap();
            pool.push(ScoreScratch::new_for(idx.doc_ids.len() + 1));
        }

        let result = idx.search("alpha beta", 10).unwrap();
        let fresh = SparseIndex::build(&docs).unwrap();
        let expected = fresh.search("alpha beta", 10).unwrap();
        assert_eq!(
            result
                .iter()
                .map(|d| (d.doc_id, d.score.to_bits()))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|d| (d.doc_id, d.score.to_bits()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn approx_heap_bytes_includes_scratch_pool_upper_bound() {
        // approx_heap_bytes はスクラッチプールの決定的な上限値（プールの実際の
        // 充填率に関わらず MAX_SCORE_SCRATCH_POOL 本ぶんの最悪確保量）を含む。
        let docs: Vec<(DocId, &str)> = vec![(1, "alpha beta"), (2, "beta gamma")];
        let idx = SparseIndex::build(&docs).unwrap();
        let n = idx.doc_ids.len();
        let lower_bound = n
            .saturating_mul(std::mem::size_of::<f64>())
            .saturating_mul(MAX_SCORE_SCRATCH_POOL);
        assert!(idx.approx_heap_bytes() >= lower_bound);
    }

    // --- approx_heap_bytes（Issue #357・sql/sparse_cache.rs の容量判定用） ---

    #[test]
    fn approx_heap_bytes_is_positive_for_nonempty_corpus() {
        let docs = vec![(1u64, "alpha beta"), (2u64, "beta gamma")];
        let idx = SparseIndex::build(&docs).unwrap();
        assert!(idx.approx_heap_bytes() > 0);
    }

    #[test]
    fn approx_heap_bytes_grows_with_corpus_size() {
        let small = vec![(1u64, "alpha")];
        let large = vec![
            (1u64, "alpha beta gamma delta epsilon"),
            (2u64, "zeta eta theta"),
        ];
        let small_idx = SparseIndex::build(&small).unwrap();
        let large_idx = SparseIndex::build(&large).unwrap();
        assert!(large_idx.approx_heap_bytes() > small_idx.approx_heap_bytes());
    }

    // --- term インターニング（Issue #388） ---

    #[test]
    fn term_dictionary_interns_same_string_to_same_id_and_assigns_ids_in_first_seen_order() {
        let mut dict = TermDictionary::default();
        let a1 = dict.intern("alpha".to_string()).unwrap();
        let b1 = dict.intern("beta".to_string()).unwrap();
        let a2 = dict.intern("alpha".to_string()).unwrap();
        assert_eq!(a1, a2, "同じ文字列は同じ TermId を返す");
        assert_ne!(a1, b1);
        assert_eq!(a1.0, 0, "初出順に 0 始まりで採番される");
        assert_eq!(b1.0, 1);
        assert_eq!(dict.lookup("alpha"), Some(a1));
        assert_eq!(dict.lookup("gamma"), None, "未 intern の語は None");
    }

    #[test]
    fn postings_reconstruct_correct_term_frequency_for_repeated_terms() {
        // Issue #390 で `DocEntry`/`docs`（毎文書の `term_freq` 保持配列）を撤去した
        // ため、この不変条件は `postings`（term 添字の転置索引）側から検証する。
        let docs = vec![
            (1u64, "alpha beta alpha gamma beta alpha"),
            (2u64, "beta gamma delta"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        // doc_id=1 の "alpha" は 3 回出現する。
        let doc1_idx = *idx.id_index.get(&1u64).unwrap() as u32;
        let alpha_id = idx.terms.lookup("alpha").unwrap();
        let list = &idx.postings[alpha_id.0 as usize];
        let tf = list
            .iter()
            .find(|&&(d, _)| d == doc1_idx)
            .map(|&(_, tf)| tf);
        assert_eq!(tf, Some(3), "\"alpha\" は doc_id=1 に 3 回出現する");
    }

    #[test]
    fn doc_freq_len_equals_dictionary_len_and_matches_manual_count() {
        let docs = vec![
            (1u64, "alpha beta"),
            (2u64, "beta gamma"),
            (3u64, "alpha gamma delta"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        assert_eq!(idx.doc_freq.len(), idx.terms.ids.len());

        // "alpha" は doc 1・3 の 2 件に出現、"beta" は doc 1・2 の 2 件、
        // "gamma" は doc 2・3 の 2 件、"delta" は doc 3 の 1 件。
        let alpha = idx.terms.lookup("alpha").unwrap();
        let beta = idx.terms.lookup("beta").unwrap();
        let gamma = idx.terms.lookup("gamma").unwrap();
        let delta = idx.terms.lookup("delta").unwrap();
        assert_eq!(idx.doc_freq[alpha.0 as usize], 2);
        assert_eq!(idx.doc_freq[beta.0 as usize], 2);
        assert_eq!(idx.doc_freq[gamma.0 as usize], 2);
        assert_eq!(idx.doc_freq[delta.0 as usize], 1);
    }

    // --- 転置索引（posting list）・doc_len／doc_ids 配列（Issue #389） ---

    /// 決定的 LCG（線形合同法）で ASCII・CJK・繰り返し語・記号のみ文書を混ぜた
    /// コーパスを生成する（テスト専用。乱数源は固定シードで再現性を持つ）。
    fn generate_mixed_corpus(n: usize) -> Vec<(DocId, String)> {
        let ascii_words = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let cjk_words = ["検索", "東京都", "評価"];
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            // 標準的な LCG パラメータ（Numerical Recipes）。テスト専用の決定的
            // 疑似乱数であり暗号用途ではない。
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state
        };
        (0..n)
            .map(|i| {
                let doc_id = i as u64;
                let text = match i % 4 {
                    0 => {
                        // ASCII 語を複数回・複数語混ぜる（重複語の圧縮経路を通す）。
                        let a = ascii_words[(next() as usize) % ascii_words.len()];
                        let b = ascii_words[(next() as usize) % ascii_words.len()];
                        format!("{a} {a} {b}")
                    }
                    1 => {
                        // CJK 語（ユニグラム＋バイグラム展開経路を通す）。
                        let w = cjk_words[(next() as usize) % cjk_words.len()];
                        w.to_string()
                    }
                    2 => {
                        // 記号のみ（トークンなし文書）。
                        "!!! ???".to_string()
                    }
                    _ => {
                        // ASCII と CJK の混在。
                        let a = ascii_words[(next() as usize) % ascii_words.len()];
                        let w = cjk_words[(next() as usize) % cjk_words.len()];
                        format!("{a}{w}")
                    }
                };
                (doc_id, text)
            })
            .collect()
    }

    #[test]
    fn postings_len_equals_dictionary_and_doc_freq_len() {
        let corpus = generate_mixed_corpus(300);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        assert_eq!(idx.postings.len(), idx.doc_freq.len());
        assert_eq!(idx.postings.len(), idx.terms.ids.len());
    }

    #[test]
    fn postings_are_sorted_by_doc_idx_ascending_without_duplicates() {
        let corpus = generate_mixed_corpus(300);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        for list in &idx.postings {
            let doc_idxs: Vec<u32> = list.iter().map(|&(doc_idx, _)| doc_idx).collect();
            let mut sorted = doc_idxs.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                doc_idxs, sorted,
                "posting list は doc_idx 昇順ソート済み・重複なしという不変条件を持つ"
            );
        }
    }

    #[test]
    fn postings_reconstruct_tf_and_df_matching_manual_tokenization_for_all_docs() {
        // Issue #390 で `DocEntry`/`docs` を撤去したため、`postings` から復元した
        // tf/df を `tokenize()` の手計算（`BTreeMap<String, u32>`）と直接突き合わせる。
        let corpus = generate_mixed_corpus(400);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();

        // 総 posting 数 == Σ 各文書の一意語数（双方向の突合の前提。旧実装〔`DocEntry`
        // 経由〕の同名アサーションと同じ独立したグローバル件数チェックを維持する。
        // これが無いと「余分な posting が 1 件混入し `doc_freq` も同時にズレる」
        // ような build のバグを、後段の per-term 突合だけでは検出できない
        // （advisor 指摘。テスト弱体化の防止・`.claude/rules/coding-rust.md`）。
        let mut total_expected_tf_entries: usize = 0;
        for &(doc_id, text) in &refs {
            let doc_idx = *idx.id_index.get(&doc_id).unwrap() as u32;
            let mut expected_tf: BTreeMap<String, u32> = BTreeMap::new();
            for t in tokenize(text) {
                *expected_tf.entry(t).or_insert(0) += 1;
            }
            total_expected_tf_entries += expected_tf.len();
            for (term, &expected_count) in &expected_tf {
                let term_id = idx.terms.lookup(term).unwrap();
                let list = &idx.postings[term_id.0 as usize];
                let actual = list.iter().find(|&&(d, _)| d == doc_idx).map(|&(_, tf)| tf);
                assert_eq!(
                    actual,
                    Some(expected_count),
                    "term={term:?} doc_id={doc_id} の tf が postings 復元値と一致しない"
                );
            }
        }
        let total_postings: usize = idx.postings.iter().map(std::vec::Vec::len).sum();
        assert_eq!(
            total_postings, total_expected_tf_entries,
            "postings の総エントリ数は全文書の一意語数合計と一致する（余分な posting・欠落の検出）"
        );
        // df との一致: 各 term の postings.len() == doc_freq[term]。
        for (term_id, list) in idx.postings.iter().enumerate() {
            assert_eq!(
                list.len(),
                idx.doc_freq[term_id] as usize,
                "postings[{term_id}].len() は doc_freq[{term_id}] と一致する"
            );
        }
    }

    #[test]
    fn doc_len_and_doc_ids_arrays_match_input_order_and_token_counts() {
        // Issue #390 で `DocEntry`/`docs` を撤去したため、`doc_len`/`doc_ids` は
        // build 時の入力順（`refs`）・`tokenize()` の手計算長と直接突き合わせる。
        let corpus = generate_mixed_corpus(200);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        assert_eq!(idx.doc_len.len(), refs.len());
        assert_eq!(idx.doc_ids.len(), refs.len());
        for (i, &(doc_id, text)) in refs.iter().enumerate() {
            assert_eq!(idx.doc_ids[i], doc_id, "doc_ids は build 時の入力順を保つ");
            let expected_len = u32::try_from(tokenize(text).len()).unwrap();
            assert_eq!(idx.doc_len[i], expected_len);
        }
    }

    #[test]
    fn id_index_maps_every_doc_id_to_its_position() {
        let corpus = generate_mixed_corpus(200);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        assert_eq!(idx.id_index.len(), refs.len());
        for (i, &(doc_id, _)) in refs.iter().enumerate() {
            assert_eq!(idx.id_index.get(&doc_id), Some(&i));
        }
    }

    #[test]
    fn approx_heap_bytes_accounts_postings_and_doc_arrays() {
        // 同一語彙で文書数を増やすと、postings/doc_len/doc_ids 分の増加により
        // approx_heap_bytes は単調増加する（Issue #389）。
        let small: Vec<(DocId, &str)> = vec![(1, "alpha beta"), (2, "beta gamma")];
        let large: Vec<(DocId, &str)> = vec![
            (1, "alpha beta"),
            (2, "beta gamma"),
            (3, "alpha gamma"),
            (4, "beta delta"),
            (5, "gamma delta"),
        ];
        let idx_small = SparseIndex::build(&small).unwrap();
        let idx_large = SparseIndex::build(&large).unwrap();
        assert!(idx_large.approx_heap_bytes() > idx_small.approx_heap_bytes());
    }

    #[test]
    fn search_ignores_unknown_query_terms_but_still_validates_max_query_terms() {
        let docs = vec![(1u64, "alpha beta"), (2u64, "gamma delta")];
        let idx = SparseIndex::build(&docs).unwrap();

        // 未知語のみのクエリはコーパスとの一致が無いため空結果（Err にはならない）。
        let results = idx.search("zeta omega", 10).unwrap();
        assert!(results.is_empty());

        // MAX_QUERY_TERMS の判定は辞書で未知語を除外する前の一意語数で行う
        // （辞書に存在しない語だけで構成されたクエリでも上限超過は拒否する）。
        let unknown_terms: String = (0..=MAX_QUERY_TERMS)
            .map(|i| format!("unknownterm{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let err = idx.search(&unknown_terms, 10).unwrap_err();
        assert_eq!(
            err,
            SparseError::TooManyQueryTerms {
                unique_terms: MAX_QUERY_TERMS + 1,
                max: MAX_QUERY_TERMS,
            }
        );
    }

    /// 旧実装（`BTreeMap<String, u32>` によるクエリ語走査）の参照実装。Issue #388 の
    /// term インターニング後もスコアがビット一致することを検証するための最小再実装
    /// （`benches/harness/hybrid_profile.rs::replica_matches_real` と同種の検証を
    /// `make ci` 経路〔単体テスト〕へ持ち込む）。
    /// 参照実装（旧 `BTreeMap<String, u32>` 方式）が母数として参照する統計一式。
    /// `reference_bm25_score` の引数を集約し（`clippy::too_many_arguments` 回避）、
    /// `search`（コーパス全体を母数）・`search_within`（可視部分集合を母数）の
    /// 両方をこの 1 つの構造体で表現する。
    struct ReferenceCorpusStats<'a> {
        doc_freq_all: &'a BTreeMap<String, u32>,
        n: f64,
        avg_doc_len: f64,
        k1: f64,
        b: f64,
    }

    fn reference_bm25_score(
        query: &str,
        doc_term_freq: &BTreeMap<String, u32>,
        doc_len: u32,
        stats: &ReferenceCorpusStats<'_>,
    ) -> f64 {
        let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
        for t in tokenize(query) {
            unique_terms.insert(t, ());
        }
        let mut score = 0.0f64;
        for term in unique_terms.keys() {
            let Some(&f) = doc_term_freq.get(term) else {
                continue;
            };
            let df = *stats.doc_freq_all.get(term).unwrap_or(&0);
            let idf = ((stats.n - f64::from(df) + 0.5) / (f64::from(df) + 0.5) + 1.0).ln();
            let numerator = f64::from(f) * (stats.k1 + 1.0);
            let len_norm = 1.0 - stats.b
                + stats.b * (f64::from(doc_len) / stats.avg_doc_len.max(f64::MIN_POSITIVE));
            let denominator = f64::from(f) + stats.k1 * len_norm;
            if denominator > 0.0 {
                score += idf * (numerator / denominator);
            }
        }
        score
    }

    /// `reference_bm25_score` を全文書へ適用し、`SparseIndex::search`/
    /// `search_within` と同じ選出規約（`score > 0.0` のみ候補化・スコア降順→
    /// `doc_id` 昇順のタイブレーク・上位 `k` 件切り出し）で Top-k を再現する
    /// （Issue #388 レビュー指摘対応: 個々の返却行のスコア一致だけでなく
    /// `results.len()` と返却 `doc_id` 集合そのものを参照実装と突き合わせるための
    /// ヘルパー。term インターニングによる取りこぼし・過剰ヒットの vacuous pass を
    /// 防ぐ）。
    fn reference_search_top_k(
        query: &str,
        ref_docs: &BTreeMap<DocId, (BTreeMap<String, u32>, u32)>,
        stats: &ReferenceCorpusStats<'_>,
        k: usize,
    ) -> Vec<(DocId, f64)> {
        let mut scored: Vec<(DocId, f64)> = ref_docs
            .iter()
            .filter_map(|(&doc_id, (tf, doc_len))| {
                let score = reference_bm25_score(query, tf, *doc_len, stats);
                (score > 0.0).then_some((doc_id, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.truncate(k);
        scored
    }

    #[test]
    fn search_score_is_bit_identical_to_reference_btreemap_implementation() {
        let docs = vec![
            (1u64, "the quick brown fox jumps over the lazy dog"),
            (2u64, "a quick brown dog outpaces a quick fox"),
            (3u64, "lazy cats and lazy dogs sleep all day"),
            (4u64, "東京都は日本の首都である"),
            (5u64, "京都は日本の古都である"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();

        // 参照実装用に旧構造（BTreeMap<String,u32>）を手計算で組み立てる。
        let mut ref_docs: BTreeMap<DocId, (BTreeMap<String, u32>, u32)> = BTreeMap::new();
        let mut ref_doc_freq: BTreeMap<String, u32> = BTreeMap::new();
        let mut ref_total_len: u64 = 0;
        for &(doc_id, text) in &docs {
            let toks = tokenize(text);
            let mut tf: BTreeMap<String, u32> = BTreeMap::new();
            for t in &toks {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            let doc_len = u32::try_from(toks.len()).unwrap();
            ref_total_len += u64::from(doc_len);
            for t in tf.keys() {
                *ref_doc_freq.entry(t.clone()).or_insert(0) += 1;
            }
            ref_docs.insert(doc_id, (tf, doc_len));
        }
        let ref_n = docs.len() as f64;
        let ref_avg_doc_len = ref_total_len as f64 / ref_n;

        for query in [
            "quick fox",
            "lazy dog cats",
            "東京 京都",
            "日本 首都",
            "no match here",
        ] {
            let results = idx.search(query, 10).unwrap();
            let stats = ReferenceCorpusStats {
                doc_freq_all: &ref_doc_freq,
                n: ref_n,
                avg_doc_len: ref_avg_doc_len,
                k1: DEFAULT_K1,
                b: DEFAULT_B,
            };
            // まず件数・返却 doc_id 集合そのものを参照実装の Top-k 選出と突き合わせる
            // （取りこぼし・余分ヒットは個々のスコア一致チェックでは検出できない）。
            let expected_top_k = reference_search_top_k(query, &ref_docs, &stats, 10);
            assert_eq!(
                results.len(),
                expected_top_k.len(),
                "query={query:?}: 返却件数が参照実装の Top-k と一致しない"
            );
            assert_eq!(
                results.iter().map(|s| s.doc_id).collect::<Vec<_>>(),
                expected_top_k.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
                "query={query:?}: 返却 doc_id の並びが参照実装の Top-k と一致しない"
            );
            for scored in &results {
                let (tf, doc_len) = ref_docs.get(&scored.doc_id).unwrap();
                let expected = reference_bm25_score(query, tf, *doc_len, &stats);
                assert_eq!(
                    scored.score.total_cmp(&expected),
                    std::cmp::Ordering::Equal,
                    "query={query:?} doc_id={} score={} expected={}",
                    scored.doc_id,
                    scored.score,
                    expected
                );
            }
        }
    }

    #[test]
    fn search_within_score_is_bit_identical_to_reference_for_visible_subset() {
        let docs = vec![
            (1u64, "the quick brown fox jumps over the lazy dog"),
            (2u64, "a quick brown dog outpaces a quick fox"),
            (3u64, "lazy cats and lazy dogs sleep all day"),
            (4u64, "quick cats nap all afternoon"),
        ];
        let idx = SparseIndex::build(&docs).unwrap();
        let visible: BTreeSet<DocId> = [1u64, 2u64, 4u64].into_iter().collect();

        // 可視部分集合のみから参照実装用の統計を組み立てる（search_within と同じ縮約）。
        let mut ref_docs: BTreeMap<DocId, (BTreeMap<String, u32>, u32)> = BTreeMap::new();
        let mut ref_doc_freq: BTreeMap<String, u32> = BTreeMap::new();
        let mut ref_total_len: u64 = 0;
        for &(doc_id, text) in &docs {
            if !visible.contains(&doc_id) {
                continue;
            }
            let toks = tokenize(text);
            let mut tf: BTreeMap<String, u32> = BTreeMap::new();
            for t in &toks {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            let doc_len = u32::try_from(toks.len()).unwrap();
            ref_total_len += u64::from(doc_len);
            for t in tf.keys() {
                *ref_doc_freq.entry(t.clone()).or_insert(0) += 1;
            }
            ref_docs.insert(doc_id, (tf, doc_len));
        }
        let ref_n = visible.len() as f64;
        let ref_avg_doc_len = ref_total_len as f64 / ref_n;

        for query in ["quick fox", "lazy dog cats", "quick", "no match here"] {
            let results = idx.search_within(query, 10, &visible).unwrap();
            let stats = ReferenceCorpusStats {
                doc_freq_all: &ref_doc_freq,
                n: ref_n,
                avg_doc_len: ref_avg_doc_len,
                k1: DEFAULT_K1,
                b: DEFAULT_B,
            };
            let expected_top_k = reference_search_top_k(query, &ref_docs, &stats, 10);
            assert_eq!(
                results.len(),
                expected_top_k.len(),
                "query={query:?}: 返却件数が参照実装の Top-k と一致しない"
            );
            assert_eq!(
                results.iter().map(|s| s.doc_id).collect::<Vec<_>>(),
                expected_top_k.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
                "query={query:?}: 返却 doc_id の並びが参照実装の Top-k と一致しない"
            );
            for scored in &results {
                let (tf, doc_len) = ref_docs.get(&scored.doc_id).unwrap();
                let expected = reference_bm25_score(query, tf, *doc_len, &stats);
                assert_eq!(
                    scored.score.total_cmp(&expected),
                    std::cmp::Ordering::Equal,
                    "query={query:?} doc_id={} score={} expected={}",
                    scored.doc_id,
                    scored.score,
                    expected
                );
            }
        }
    }

    #[test]
    fn approx_heap_bytes_accounts_dictionary_and_term_freq_vectors() {
        let small = vec![(1u64, "alpha")];
        let large = vec![(1u64, "alpha beta gamma delta epsilon zeta eta theta")];
        let small_idx = SparseIndex::build(&small).unwrap();
        let large_idx = SparseIndex::build(&large).unwrap();
        assert!(
            large_idx.approx_heap_bytes() > small_idx.approx_heap_bytes(),
            "語彙数の増加に伴い approx_heap_bytes は単調増加する"
        );
    }

    // --- 文書長クラス表・Top-k select_nth 型選出（Issue #391） ---

    #[test]
    fn len_classes_are_sorted_unique_and_map_back_to_doc_len() {
        let corpus = generate_mixed_corpus(500);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();

        assert_eq!(
            idx.doc_len.len(),
            idx.doc_len_class.len(),
            "doc_len_class は doc_len と同じ長さ（doc_idx 空間）を持つ"
        );
        // 昇順・重複なし。
        let mut sorted = idx.len_classes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            idx.len_classes, sorted,
            "len_classes は昇順・重複なしの不変条件を持つ"
        );
        // 添字経由で doc_len を再現できる。
        for (i, &doc_len) in idx.doc_len.iter().enumerate() {
            let class = idx.doc_len_class[i];
            assert_eq!(
                idx.len_classes[class as usize], doc_len,
                "doc_idx={i} の doc_len_class がクラス表経由で元の doc_len を再現しない"
            );
        }
    }

    #[test]
    fn len_norm_table_reproduces_inline_formula_bitwise() {
        // `score_by_postings` が構築する `k1_len_norm` テーブルはこのテストから
        // 直接は見えないため、`doc_len_class` 経由の間接参照（`len_classes[class]`）
        // が、各文書の `doc_len` を直接使った旧来のインライン計算式
        // （`self.k1 * (1.0 - self.b + self.b * (doc_len / avgdl.max(MIN_POSITIVE)))`）
        // とビット一致することを、文書ごとに `doc_len_class` 添字を経由して固定する
        // （テーブル化の間接参照そのものを検証対象にする。式を 2 通りに書いて
        // 比較するだけの自明比較にしない）。
        let corpus = generate_mixed_corpus(300);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        let avg_doc_len = idx.avg_doc_len;

        let len_norm_of = |len: u32| -> f64 {
            idx.k1 * (1.0 - idx.b + idx.b * (f64::from(len) / avg_doc_len.max(f64::MIN_POSITIVE)))
        };

        for (doc_idx, &doc_len) in idx.doc_len.iter().enumerate() {
            let class = idx.doc_len_class[doc_idx];
            // クラス表経由（`doc_len_class[doc_idx]` → `len_classes[class]`）で
            // 求めた長さが、文書自身の `doc_len` を直接使った値とビット一致するか。
            let via_class = len_norm_of(idx.len_classes[class as usize]);
            let via_doc_len = len_norm_of(doc_len);
            assert_eq!(
                via_class.total_cmp(&via_doc_len),
                std::cmp::Ordering::Equal,
                "doc_idx={doc_idx} doc_len={doc_len} class={class}: \
                 doc_len_class 経由の k1*len_norm がインライン計算とビット一致しない"
            );
        }
    }

    #[test]
    fn search_and_search_within_top_k_bit_identical_to_reference_on_mixed_corpus() {
        let corpus = generate_mixed_corpus(2_000);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();

        // 参照実装用の全文書統計。
        let mut ref_docs: BTreeMap<DocId, (BTreeMap<String, u32>, u32)> = BTreeMap::new();
        let mut ref_doc_freq: BTreeMap<String, u32> = BTreeMap::new();
        let mut ref_total_len: u64 = 0;
        for &(doc_id, text) in &refs {
            let toks = tokenize(text);
            let mut tf: BTreeMap<String, u32> = BTreeMap::new();
            for t in &toks {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            let doc_len = u32::try_from(toks.len()).unwrap();
            ref_total_len += u64::from(doc_len);
            for t in tf.keys() {
                *ref_doc_freq.entry(t.clone()).or_insert(0) += 1;
            }
            ref_docs.insert(doc_id, (tf, doc_len));
        }
        let ref_n = refs.len() as f64;
        let ref_avg_doc_len = ref_total_len as f64 / ref_n;
        let stats_all = ReferenceCorpusStats {
            doc_freq_all: &ref_doc_freq,
            n: ref_n,
            avg_doc_len: ref_avg_doc_len,
            k1: DEFAULT_K1,
            b: DEFAULT_B,
        };

        // 可視部分集合（偶数 doc_id のみ）と、それに限定した参照統計。
        let visible: BTreeSet<DocId> = refs
            .iter()
            .map(|&(id, _)| id)
            .filter(|id| id % 2 == 0)
            .collect();
        let mut ref_docs_visible: BTreeMap<DocId, (BTreeMap<String, u32>, u32)> = BTreeMap::new();
        let mut ref_doc_freq_visible: BTreeMap<String, u32> = BTreeMap::new();
        let mut ref_total_len_visible: u64 = 0;
        for &(doc_id, text) in &refs {
            if !visible.contains(&doc_id) {
                continue;
            }
            let toks = tokenize(text);
            let mut tf: BTreeMap<String, u32> = BTreeMap::new();
            for t in &toks {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            let doc_len = u32::try_from(toks.len()).unwrap();
            ref_total_len_visible += u64::from(doc_len);
            for t in tf.keys() {
                *ref_doc_freq_visible.entry(t.clone()).or_insert(0) += 1;
            }
            ref_docs_visible.insert(doc_id, (tf, doc_len));
        }
        let ref_n_visible = visible.len() as f64;
        let ref_avg_doc_len_visible = ref_total_len_visible as f64 / ref_n_visible;
        let stats_visible = ReferenceCorpusStats {
            doc_freq_all: &ref_doc_freq_visible,
            n: ref_n_visible,
            avg_doc_len: ref_avg_doc_len_visible,
            k1: DEFAULT_K1,
            b: DEFAULT_B,
        };

        let m = refs.len();
        let ks = [1usize, 5, 20, 100, m / 2, m, m + 1];
        for query in [
            "alpha beta",
            "検索 評価",
            "alpha検索",
            "gamma delta epsilon",
        ] {
            for &k in &ks {
                // search（全体母数）。
                let results = idx.search(query, k).unwrap();
                let expected = reference_search_top_k(query, &ref_docs, &stats_all, k);
                assert_eq!(
                    results.len(),
                    expected.len(),
                    "search query={query:?} k={k}: 件数不一致"
                );
                assert_eq!(
                    results.iter().map(|s| s.doc_id).collect::<Vec<_>>(),
                    expected.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
                    "search query={query:?} k={k}: doc_id 列不一致"
                );
                for (scored, &(_, exp_score)) in results.iter().zip(expected.iter()) {
                    assert_eq!(
                        scored.score.total_cmp(&exp_score),
                        std::cmp::Ordering::Equal,
                        "search query={query:?} k={k} doc_id={}: スコア不一致",
                        scored.doc_id
                    );
                }

                // search_within（可視部分集合母数）。
                let results_within = idx.search_within(query, k, &visible).unwrap();
                let expected_within =
                    reference_search_top_k(query, &ref_docs_visible, &stats_visible, k);
                assert_eq!(
                    results_within.len(),
                    expected_within.len(),
                    "search_within query={query:?} k={k}: 件数不一致"
                );
                assert_eq!(
                    results_within.iter().map(|s| s.doc_id).collect::<Vec<_>>(),
                    expected_within
                        .iter()
                        .map(|&(id, _)| id)
                        .collect::<Vec<_>>(),
                    "search_within query={query:?} k={k}: doc_id 列不一致"
                );
                for (scored, &(_, exp_score)) in results_within.iter().zip(expected_within.iter()) {
                    assert_eq!(
                        scored.score.total_cmp(&exp_score),
                        std::cmp::Ordering::Equal,
                        "search_within query={query:?} k={k} doc_id={}: スコア不一致",
                        scored.doc_id
                    );
                }
            }
        }
    }

    #[test]
    fn score_within_top_is_bit_identical_to_search_within_on_mixed_corpus() {
        // `score_within(q, vis).top(k)` と `search_within(q, k, vis)` が同一の
        // 出力（doc_id 列・スコアともビット一致）を返すことを固定する
        // （Issue #392。両者は [`SparseIndex::score_pass`] を共有するが、
        // Top-k 選出の実装〔`TopKSelector` 対 `SparseScored::top`〕が異なる
        // ため、出力が一致することそのものが本テストの検証対象になる）。
        let corpus = generate_mixed_corpus(2_000);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();

        let visible: BTreeSet<DocId> = refs
            .iter()
            .map(|&(id, _)| id)
            .filter(|id| id % 2 == 0)
            .collect();
        let m = visible.len();
        // 疎側再取得ループの倍増スケジュール相当の値を含める（Issue #320・#392）。
        let ks = [1usize, 5, 20, 100, m / 2, m, m + 1, 2, 4, 8, 16, 32, 64];
        for query in [
            "alpha beta",
            "検索 評価",
            "alpha検索",
            "gamma delta epsilon",
        ] {
            let mut scored = idx.score_within(query, &visible).unwrap();
            for &k in &ks {
                let via_scored = scored.top(k);
                let via_search_within = idx.search_within(query, k, &visible).unwrap();
                assert_eq!(
                    via_scored, via_search_within,
                    "query={query:?} k={k}: score_within(..).top(k) と \
                     search_within(.., k, ..) が一致しない"
                );
            }
        }
    }

    #[test]
    fn sparse_scored_top_is_prefix_consistent_and_idempotent() {
        // `top(k1)` → `top(k2 > k1)` → `top(k1)` の順に呼んでも前方一致・
        // 冪等であることを固定する（Issue #392。[`SparseScored::top`] の
        // ドキュメンテーションコメントの契約）。
        let corpus = generate_mixed_corpus(500);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        let visible: BTreeSet<DocId> = refs.iter().map(|&(id, _)| id).collect();

        let mut scored = idx.score_within("alpha beta", &visible).unwrap();
        let m = scored.len();
        assert!(m > 40, "テスト前提: 十分なヒット数が必要 (m={m})");

        let k1 = 5usize;
        let k2 = 40usize;
        let top_k1_first = scored.top(k1);
        let top_k2 = scored.top(k2);
        let top_k1_again = scored.top(k1);

        assert_eq!(
            top_k1_first, top_k1_again,
            "同じ k を複数回呼んでも同一列を返す（冪等性）"
        );
        assert_eq!(
            top_k2[..k1],
            top_k1_first[..],
            "top(k1) は top(k2) の前方一致な部分列である"
        );

        // k > M（全ヒット数）は M 件で頭打ちになる。
        let top_over = scored.top(m + 10);
        assert_eq!(top_over.len(), m);
        assert_eq!(&top_over[..k2], &top_k2[..]);
    }

    #[test]
    fn sparse_scored_top_k_zero_and_empty_scored_return_empty() {
        let corpus = generate_mixed_corpus(50);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        let visible: BTreeSet<DocId> = refs.iter().map(|&(id, _)| id).collect();

        let mut scored = idx.score_within("alpha beta", &visible).unwrap();
        assert!(scored.top(0).is_empty());

        // 未知語のみのクエリ・空可視集合はいずれも空の `SparseScored` を返す。
        let mut scored_unknown = idx
            .score_within("zzz_not_a_real_token_zzz", &visible)
            .unwrap();
        assert!(scored_unknown.is_empty());
        assert!(scored_unknown.top(10).is_empty());

        let mut scored_empty_visible = idx.score_within("alpha beta", &BTreeSet::new()).unwrap();
        assert!(scored_empty_visible.is_empty());
        assert!(scored_empty_visible.top(10).is_empty());
    }

    #[test]
    fn score_within_keeps_working_for_index_k1_above_query_upper_bound() {
        // クエリ時上書きの k1 上限（1e200）は索引自身のパラメータ経路に波及しない。
        let idx = SparseIndex::with_params(&[(1, "apple"), (2, "banana")], 1e201, 0.75).unwrap();
        let visible: BTreeSet<DocId> = [1, 2].into_iter().collect();
        let mut scored = idx.score_within("apple", &visible).unwrap();
        let top = scored.top(5);
        assert_eq!(top.len(), 1);
        assert!(top[0].score.is_finite() && top[0].score > 0.0);
        // 上書き経路では従来どおり拒否される。
        assert!(Bm25Params::new(1e201, 0.75).is_err());
    }

    #[test]
    fn score_within_input_validation_precedes_empty_cases() {
        // `search_within_query_too_long_is_rejected_before_visible_ids_check`
        // と対（Issue #392）。`QueryTooLong`・`TooManyQueryTerms` が空可視集合
        // より優先して評価される契約は `score_within` でも同一。
        let corpus = generate_mixed_corpus(20);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        let empty_visible: BTreeSet<DocId> = BTreeSet::new();

        let too_long_query = "a".repeat(MAX_QUERY_BYTES + 1);
        let err = idx
            .score_within(&too_long_query, &empty_visible)
            .unwrap_err();
        assert!(matches!(err, SparseError::QueryTooLong { .. }));

        let too_many_terms_query = (0..=MAX_QUERY_TERMS)
            .map(|i| format!("distinctterm{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let err = idx
            .score_within(&too_many_terms_query, &empty_visible)
            .unwrap_err();
        assert!(matches!(err, SparseError::TooManyQueryTerms { .. }));
    }

    #[test]
    fn score_within_statistics_are_isolated_from_invisible_docs() {
        // `search_within_statistics_are_isolated_from_invisible_docs` と対
        // （Issue #392）。可視部分集合（id=1 のみ "cat" を含む）を固定したまま
        // 共有インデックスへ大量の不可視文書（"cat" を含むものを含む）を追加
        // しても `score_within` のスコアが不変であること（RLS 相当のテナント
        // 境界縮約契約の回帰。`doc_count`/`doc_freq` を通じた統計汚染の防止）。
        let small_docs = vec![(1u64, "cat"), (2u64, "dog")];
        let small_idx = SparseIndex::build(&small_docs).expect("build ok");
        let visible: BTreeSet<u64> = [1u64].into_iter().collect();
        let small_result = small_idx
            .score_within("cat", &visible)
            .expect("score_within ok")
            .top(10);

        let mut large_docs: Vec<(u64, &str)> = vec![(1u64, "cat"), (2u64, "dog")];
        let filler: Vec<(u64, &str)> = (100u64..150).map(|id| (id, "cat cat cat")).collect();
        large_docs.extend(filler.iter().copied());
        let large_idx = SparseIndex::build(&large_docs).expect("build ok");
        let large_result = large_idx
            .score_within("cat", &visible)
            .expect("score_within ok")
            .top(10);

        assert_eq!(
            small_result, large_result,
            "invisible corpus growth must not change visible-only score"
        );
    }

    /// 決定的 xorshift による疑似乱数列（テスト専用。暗号用途ではない）。
    fn xorshift_next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// 決定的乱数で `Candidate` 列（同点を多数含む）を生成する。
    fn generate_random_candidates(m: usize, seed: u64) -> Vec<Candidate> {
        let mut state = seed;
        (0..m)
            .map(|i| {
                // スコアの取り得る値を小さい集合に絞ることで同点を多発させる
                // （境界の決定性テストが有効になる条件を作る）。
                let bucket = xorshift_next(&mut state) % 10;
                Candidate {
                    score: bucket as f64,
                    doc_id: i as u64,
                }
            })
            .collect()
    }

    /// `TopKSelector` の出力が「全件を `Candidate::Ord` で降順ソートした先頭 k_eff 件」
    /// と集合・順序ともに一致することを、`M`/`k` の境界（`M > 2k`・`M <= k`・
    /// `k == 0`・`M == 0`）を網羅して確認する。
    #[test]
    fn top_k_selector_matches_full_sort_for_random_candidates() {
        for &(m, k) in &[
            (0usize, 5usize),
            (5, 0),
            (3, 10),
            (10, 3),
            (200, 7),
            (200, 100),
            (201, 201),
        ] {
            let candidates = generate_random_candidates(m, 0x9e37_79b9_7f4a_7c15 ^ (m as u64));
            let k_eff = k.min(m);

            let mut expected = candidates.clone();
            // Candidate::Ord は doc_id 昇順の明示的タイブレークを含む全順序のため、
            // 不安定ソートでも期待値の順序は決定的。
            expected.sort_unstable_by(|a, b| b.cmp(a)); // sort-determinism: allow Candidate::Ord は doc_id 昇順の明示的タイブレークを含む全順序
            expected.truncate(k_eff);

            let mut selector = TopKSelector::new(k_eff);
            for &c in &candidates {
                selector.push(c);
            }
            let actual = selector.into_sorted();

            assert_eq!(
                actual, expected,
                "m={m} k={k}: TopKSelector の出力が全件ソート上位 k_eff 件と一致しない"
            );
        }
    }

    #[test]
    fn top_k_selector_boundary_tie_group_prefers_smaller_doc_id() {
        // 同点スコアの候補群が k の境界をまたぐケース。`Candidate::Ord` の
        // タイブレーク（doc_id 昇順が「良い」）により、境界を跨ぐ同点グループの
        // うち doc_id が小さい側だけが残ることを固定する（`hybrid.rs` の境界同点
        // グループ完全化・再取得ループの前提契約）。
        let mut candidates: Vec<Candidate> = (0..10u64)
            .map(|doc_id| Candidate { score: 1.0, doc_id })
            .collect();
        // 非同点の高スコア候補を混ぜて「同点グループが境界に来る」状況を作る。
        candidates.push(Candidate {
            score: 5.0,
            doc_id: 100,
        });

        let k_eff = 4; // 高スコア 1 件 + 同点グループの先頭 3 件（doc_id 0,1,2）。
        let mut selector = TopKSelector::new(k_eff);
        for &c in &candidates {
            selector.push(c);
        }
        let actual = selector.into_sorted();

        assert_eq!(actual.len(), k_eff);
        assert_eq!(actual[0].doc_id, 100);
        let tie_ids: Vec<u64> = actual[1..].iter().map(|c| c.doc_id).collect();
        assert_eq!(
            tie_ids,
            vec![0, 1, 2],
            "境界を跨ぐ同点グループは doc_id が小さい側だけが残る"
        );
    }

    #[test]
    fn search_within_top_k_cut_inside_tie_group_is_deterministic_and_prefix_consistent() {
        // 同一スコアになる文書群を作り、k を増やしたときに結果が前方一致（prefix）
        // で伸びることを確認する（`hybrid.rs::sparse_refetch_loop` の倍増再取得と
        // 整合する契約）。全文書が同じ 1 語のみを含む文書は tf・doc_len が揃うため
        // スコアが完全に一致する。
        let docs: Vec<(DocId, String)> = (0..30u64).map(|id| (id, "alpha".to_string())).collect();
        let refs: Vec<(DocId, &str)> = docs.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        let visible: BTreeSet<DocId> = refs.iter().map(|&(id, _)| id).collect();

        let mut prev: Vec<DocId> = Vec::new();
        for k in [1usize, 5, 10, 20, 30, 31] {
            let results = idx.search_within("alpha", k, &visible).unwrap();
            let ids: Vec<DocId> = results.iter().map(|s| s.doc_id).collect();
            assert!(
                ids.starts_with(&prev),
                "k={k}: 前回（k={}）の結果が今回の結果の前方一致になっていない: prev={prev:?} ids={ids:?}",
                prev.len()
            );
            prev = ids;
        }
        assert_eq!(prev.len(), 30, "可視文書数を超える k は全件で頭打ちになる");
    }

    #[test]
    fn search_within_len_norm_table_uses_visible_avgdl_only() {
        // 可視外に極端な長さの文書を追加してもテーブル・スコアが不変であることを
        // 確認する（RLS 縮約契約の回帰。可視部分集合の avgdl のみをテーブル構築に
        // 使う契約が Issue #391 のテーブル化で崩れていないことの直接的な保証）。
        let base_docs = vec![
            (1u64, "alpha beta"),
            (2u64, "alpha beta gamma"),
            (3u64, "alpha"),
        ];
        let visible: BTreeSet<DocId> = [1u64, 2u64, 3u64].into_iter().collect();

        let idx_base = SparseIndex::build(&base_docs).unwrap();
        let baseline = idx_base.search_within("alpha", 10, &visible).unwrap();

        // 可視外に極端に長い文書を大量に追加する（avgdl を大きく歪める）。
        let mut extended_docs = base_docs.clone();
        let long_text: String = "zeta ".repeat(500);
        for extra_id in 100u64..110u64 {
            extended_docs.push((extra_id, long_text.as_str()));
        }
        let idx_extended = SparseIndex::build(&extended_docs).unwrap();
        let extended = idx_extended.search_within("alpha", 10, &visible).unwrap();

        assert_eq!(
            baseline, extended,
            "可視外の文書長がテーブル・スコアへ影響してはならない（RLS 縮約契約）"
        );
    }

    #[test]
    fn approx_heap_bytes_accounts_len_class_arrays() {
        // 文書長のバリエーションを増やすと len_classes/doc_len_class 分の増加により
        // approx_heap_bytes は単調増加する（Issue #391）。
        let uniform: Vec<(DocId, &str)> = (0..20u64).map(|id| (id, "alpha beta")).collect();
        let varied: Vec<(DocId, &str)> = (0..20u64)
            .map(|id| {
                let text = match id % 4 {
                    0 => "alpha",
                    1 => "alpha beta",
                    2 => "alpha beta gamma",
                    _ => "alpha beta gamma delta epsilon zeta eta",
                };
                (id, text)
            })
            .collect();
        let idx_uniform = SparseIndex::build(&uniform).unwrap();
        let idx_varied = SparseIndex::build(&varied).unwrap();
        assert!(
            idx_varied.approx_heap_bytes() > idx_uniform.approx_heap_bytes(),
            "len_classes/doc_len_class の増加分だけ approx_heap_bytes は単調増加する"
        );
    }

    /// 256 段ロッシー fieldnorm 量子化を production クラス表（[`Self::len_classes`]。
    /// 厳密写像）と比較する手動実験（Issue #391 Step 4）。tantivy のテーブル値は
    /// 転記せず、本テストのみで完結する自作の代表値写像規則を使う。`--nocapture`
    /// で変動件数（doc_id 列の不一致数・同点グループ構成の変化）を出力する。
    /// `content_hash.rs` の手動実測 `#[ignore]` テストと同じ位置づけであり、CI の
    /// 常時実行対象ではない。
    ///
    /// 判断（Issue #391）: production は本テストの結果によらず厳密クラス表
    /// （`len_classes`）を採用し、256 段ロッシー量子化は不採用（Rejected）とする。
    /// 理由はビット一致契約（`replica_matches_real`・Recall 層 A 固定値・
    /// `sparse_cache_recall.rs` cold/hot 等価性が前提とする）に反すること自体であり、
    /// 変動件数が 0 であっても採否は変わらない。性能上もクラス数に依存しないため
    /// 厳密表に対する優位はない。詳細は
    /// `docs/design/hybrid-rrf-latency-breakdown.md`「Issue #391」節参照。
    #[test]
    #[ignore = "手動実験専用。--ignored --nocapture で実行する"]
    fn fieldnorm_256_quantization_rank_divergence_report() {
        const QUANT_LEVELS: u32 = 256;

        /// 文書長を 256 段の代表値へ写像する自作規則（test-only）。小さい長さは
        /// 厳密に、以降は幾何的に粗くする単純な区分（外部実装の転記ではない）。
        fn quantize_len(len: u32, max_len: u32) -> u32 {
            if max_len == 0 || len == 0 {
                return 0;
            }
            let ratio = f64::from(len) / f64::from(max_len.max(1));
            let level = (ratio * f64::from(QUANT_LEVELS - 1)).round() as u32;
            let level = level.min(QUANT_LEVELS - 1);
            // レベルから代表長へ戻す（区分の中間的な長さを採用）。
            ((f64::from(level) / f64::from(QUANT_LEVELS - 1)) * f64::from(max_len)).round() as u32
        }

        let corpus = generate_mixed_corpus(5_000);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        let max_len = idx.doc_len.iter().copied().max().unwrap_or(0);

        let queries = [
            "alpha beta",
            "検索",
            "gamma delta",
            "評価 東京都",
            "epsilon",
        ];
        let ks = [10usize, 20, 100];

        let mut total_compared = 0usize;
        let mut total_diverged = 0usize;

        for &query in &queries {
            for &k in &ks {
                // 厳密クラス表（production）の Top-k。
                let precise = idx.search(query, k).unwrap();

                // 代表値量子化版の Top-k を、production と同じ posting 走査ロジックを
                // 手計算で再現して求める（`score_by_postings` の非公開実装は呼べない
                // ため、同じ式・同じ選出規約をここで再構成する。ビット一致は目的とせず
                // 「量子化による順位変動の有無」のみを測る）。
                let query_terms = tokenize(query);
                let mut unique_terms: BTreeMap<String, ()> = BTreeMap::new();
                for t in &query_terms {
                    unique_terms.insert(t.clone(), ());
                }
                let query_ids: Vec<TermId> = unique_terms
                    .keys()
                    .filter_map(|t| idx.terms.lookup(t))
                    .collect();

                let n = f64::from(idx.doc_count);
                let avg_doc_len = idx.avg_doc_len;
                let mut acc: BTreeMap<DocId, f64> = BTreeMap::new();
                for &term in &query_ids {
                    let Some(list) = idx.postings.get(term.0 as usize) else {
                        continue;
                    };
                    let df = idx.doc_freq.get(term.0 as usize).copied().unwrap_or(0);
                    let idf = SparseIndex::idf_for(n, df);
                    for &(doc_idx, tf) in list {
                        let raw_len = idx.doc_len[doc_idx as usize];
                        let q_len = quantize_len(raw_len, max_len);
                        let f = f64::from(tf);
                        let numerator = f * (idx.k1 + 1.0);
                        let len_norm = 1.0 - idx.b
                            + idx.b * (f64::from(q_len) / avg_doc_len.max(f64::MIN_POSITIVE));
                        let denominator = f + idx.k1 * len_norm;
                        if denominator > 0.0 {
                            let doc_id = idx.doc_ids[doc_idx as usize];
                            *acc.entry(doc_id).or_insert(0.0) += idf * (numerator / denominator);
                        }
                    }
                }
                let mut quantized: Vec<(DocId, f64)> =
                    acc.into_iter().filter(|&(_, s)| s > 0.0).collect();
                quantized.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                quantized.truncate(k);

                let precise_ids: Vec<DocId> = precise.iter().map(|s| s.doc_id).collect();
                let quantized_ids: Vec<DocId> = quantized.iter().map(|&(id, _)| id).collect();

                total_compared += 1;
                if precise_ids != quantized_ids {
                    total_diverged += 1;
                }
                println!(
                    "query={query:?} k={k}: precise={precise_ids:?} quantized={quantized_ids:?} diverged={}",
                    precise_ids != quantized_ids
                );
            }
        }

        println!(
            "fieldnorm_256_quantization_rank_divergence_report: {total_diverged}/{total_compared} 件で Top-k の doc_id 列が量子化により変動"
        );
        // 本テストは実験レポートであり、変動件数の多寡そのものを合否条件にしない
        // （production の採否判断はビット一致契約が根拠であり、変動件数の実測値に
        // 依存しない。上記コメント参照）。ただし比較自体が実行されたことは確認する。
        assert!(total_compared > 0);
    }

    /// Issue #2121: クエリ時パラメータ版は既定 params で `score_within` とビット一致し、
    /// `k1`／`b` を変えた呼び出しを挟んでもスクラッチ再利用で値が混入しない。
    #[test]
    fn score_within_with_params_default_is_bit_identical_and_isolated() {
        let corpus = generate_mixed_corpus(300);
        let refs: Vec<(DocId, &str)> = corpus.iter().map(|(id, s)| (*id, s.as_str())).collect();
        let idx = SparseIndex::build(&refs).unwrap();
        let visible: BTreeSet<DocId> = refs.iter().map(|&(id, _)| id).collect();
        let bits = |s: &SparseScored| -> Vec<(DocId, u64)> {
            s.candidates
                .iter()
                .map(|c| (c.doc_id, c.score.to_bits()))
                .collect()
        };
        let base = idx.score_within("alpha beta", &visible).unwrap();
        let other = Bm25Params::new(0.3, 0.1).unwrap();
        for _ in 0..3 {
            let same = idx
                .score_within_with_params("alpha beta", &visible, idx.params())
                .unwrap();
            assert_eq!(bits(&base), bits(&same));
            let changed = idx
                .score_within_with_params("alpha beta", &visible, other)
                .unwrap();
            assert_ne!(bits(&base), bits(&changed), "k1/b change must alter scores");
            let again = idx.score_within("alpha beta", &visible).unwrap();
            assert_eq!(bits(&base), bits(&again));
        }
    }

    /// Issue #2121: `Bm25Params::new` の定義域・数値安全上限・境界値。
    #[test]
    fn bm25_params_new_validates_domain_and_safety_cap() {
        for (k1, b) in [
            (f64::NAN, 0.5),
            (f64::INFINITY, 0.5),
            (-0.1, 0.5),
            (1.0, f64::NAN),
            (1.0, -0.1),
            (1.0, 1.1),
            (1e308, 0.5),
            (MAX_QUERY_K1 * 10.0, 0.5),
        ] {
            assert!(
                matches!(
                    Bm25Params::new(k1, b),
                    Err(SparseError::InvalidParams { .. })
                ),
                "{k1} {b}"
            );
        }
        for (k1, b) in [(0.0, 0.0), (0.0, 1.0), (1.2, 0.75), (MAX_QUERY_K1, 1.0)] {
            assert!(Bm25Params::new(k1, b).is_ok(), "{k1} {b}");
        }
        assert_eq!(Bm25Params::default(), Bm25Params::new(1.2, 0.75).unwrap());
    }

    /// Issue #2121: 上限ちょうどの `k1` でも全候補のスコアが有限・正で、脱落しない。
    #[test]
    fn score_within_with_params_at_cap_keeps_all_candidates_finite() {
        let idx =
            SparseIndex::build(&[(1, "apple zzz"), (2, "apple apple zzz zzz zzz zzz")]).unwrap();
        let visible: BTreeSet<DocId> = [1, 2].into_iter().collect();
        let p = Bm25Params::new(MAX_QUERY_K1, 1.0).unwrap();
        let scored = idx.score_within_with_params("apple", &visible, p).unwrap();
        assert_eq!(scored.candidates.len(), 2);
        assert!(scored
            .candidates
            .iter()
            .all(|c| c.score.is_finite() && c.score > 0.0));
    }

    /// Issue #2121: `k1 = 0` は全一致文書が同点になり doc_id 昇順で決定的。
    #[test]
    fn score_within_with_params_k1_zero_ties_by_doc_id() {
        let idx = SparseIndex::build(&[
            (3, "apple apple apple"),
            (1, "apple zzz zzz"),
            (2, "apple apple zzz"),
        ])
        .unwrap();
        let visible: BTreeSet<DocId> = [1, 2, 3].into_iter().collect();
        let p = Bm25Params::new(0.0, 0.75).unwrap();
        let mut scored = idx.score_within_with_params("apple", &visible, p).unwrap();
        let ids: Vec<DocId> = scored.top(3).iter().map(|d| d.doc_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
