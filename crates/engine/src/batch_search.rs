//! バッチクエリ・一括インデクシング専用の検索エンジン（TASK-128 ポインタ:
//! CORE-6, CORE-7, CORE-16）。`kernel.rs::SearchProvider`（単発クエリの窓口）は
//! 実装せず、単発経路（`core.rs::EngineCore::search`）から本モジュールへは
//! 構造的に接続できない。
//!
//! 常駐ベース行列を f16 2 要素/u32 パックで保持・走査する CPU 上の参照実装で
//! あり、実際の GPU（`wgpu`）実行は行わない。依存追加はユーザーの明示承認を
//! 経てから行う規約（.claude/rules/dependency-policy.md）のため、承認前は
//! アルゴリズム的な挙動（f16 パック表現・バッチ Top-k 選出・クエリ別テナント
//! マスク・動的窓集約）のみを依存クレートなしで実装する。GPU 実行への置き換えは
//! 承認後のフォローアップとする（TASK-128 ポインタ）。
//!
//! `TopKSelector`（`kernel.rs`）を選出段で共用し、選出規約（スコア降順・同点識別子
//! 昇順・非有限値除外）の二重管理を防ぐ。`core.rs::EngineCore` と同じ「(1) 選出前に
//! 候補をマスクで絞る、(2) 選出後にスロットを `(tenant_id, id)` へ解決して独立に
//! 再検証する」二重防御を踏襲する（対象ビヘイビア: TABLE-12・RLS-9。行 `id` の
//! 一意性スコープがテナント内に閉じたため、`core.rs::resolve_slot_hits` と同じ
//! 「候補識別子＝常駐行列のスロット番号」方式を採る。詳細は [`BatchHit`] 参照）。

use std::fmt;

use crate::kernel::{CandidateHit, SearchHit, TopKSelector};
use crate::policy::PolicyContext;
use crate::storage::Visibility;

/// バッチ 1 件あたりの許容クエリ数上限（防御的上限。`core.rs::MAX_SEARCH_K` と同じ
/// 桁感覚で、上限検証前にアロケーションへ使わない）。
pub const MAX_BATCH_QUERIES: usize = 4_096;

/// 一括インデクシング対象の行数上限（防御的上限）。
pub const MAX_BATCH_ROWS: usize = 1_000_000;

/// クエリ・格納ベクトルの次元数上限（防御的上限）。
pub const MAX_BATCH_DIM: usize = 8_192;

/// [`ResidentMatrix::build`] が確保してよい総バイト量の上限。`packed: Vec<u32>` に
/// 加え、同時に確保する `ids: Vec<u64>`・`tenant_ids: Vec<String>` の見積もりバイト量も
/// 合算した総量として扱う（`arena.rs::MAX_ARENA_TOTAL_BYTES`/`check_capacity` と同方針。
/// codex レビュー指摘対応: `MAX_BATCH_ROWS`・`MAX_BATCH_DIM` を独立にしか検証しないと、
/// 両方が上限値の場合に `packed` 単体で約 16.4GB の単一確保が発生し得るため、行数と
/// バイト量の両方で上限を課す）。
pub const MAX_BATCH_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

/// `BatchQuery::k` の許容上限（防御的上限）。`core.rs::MAX_SEARCH_K` と同じ値を
/// 独立に定義する（`core.rs` の定数は非公開で本モジュールから参照できないため）。
/// `kernel.rs::TopKSelector` 自体は `k` を検証しないため、複数クエリを同時に
/// 処理するバッチ経路では本モジュールが未検証の巨大な `k` をそのまま
/// 使わないよう明示的に拒否する。
pub const MAX_BATCH_K: usize = 10_000;

/// バッチ全体で許容する `sum(k)`（各クエリの `k` の総和）の上限（防御的上限。
/// codex P1 指摘対応）。クエリごとの `k` は [`MAX_BATCH_K`] で個別に上限を
/// 課しているが、[`MAX_BATCH_QUERIES`] × `MAX_BATCH_K`（最大 4,096 万）まで
/// 積が達しうる。`TopKSelector` は 1 クエリあたり最終的に高々 `k` 個の要素を
/// 保持する（`kernel.rs::TopKSelector::push` 参照）ため、全選出器が保持しうる
/// 要素数の総和はバッチ全体の `sum(k)` で抑えられる。1 個の常駐行列が持つ
/// 行数を超える数の hit を選出しても意味がない（[`MAX_BATCH_ROWS`] 行しか
/// 実データが存在しない）ため、`sum(k)` の上限を [`MAX_BATCH_ROWS`] と
/// 同じ桁に揃える。
pub const MAX_BATCH_TOTAL_K: usize = MAX_BATCH_ROWS;

/// [`DynamicWindowAggregator`] が確保してよい総バイト量の上限（防御的上限。
/// `ResidentMatrix::build` の [`MAX_BATCH_TOTAL_BYTES`] と同じ 1 GiB の
/// 桁感覚。codex P1 指摘対応: 以前は `DynamicWindowAggregator::push` が
/// 件数・次元・総容量を検証せず、内部 `Vec<Vec<f32>>` を無制限に成長
/// させていた）。
pub const MAX_BATCH_AGGREGATOR_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

/// `BatchEngine::batch_search` 1 回の走査で許容する総積和演算数
/// （`rows × queries × dim` の概算値）の上限（防御的上限。codex P1 指摘対応:
/// 計算量 DoS 対策）。行数（[`MAX_BATCH_ROWS`]=100 万）・クエリ数
/// （[`MAX_BATCH_QUERIES`]=4,096）・次元（[`MAX_BATCH_DIM`]=8,192）は
/// それぞれ独立に上限検証済みでも、積の理論上限は約 3.35 × 10^13 に達する。
/// `sum(k)` 上限（[`MAX_BATCH_TOTAL_K`]）を満たしていても、`MAX_BATCH_TOTAL_BYTES`
/// （1 GiB）の予算内で構築可能な常駐行列を最大クエリ数で走査させれば
/// 容易にこの規模へ到達する（例: 行数 1,000・次元 8,192 の行列は packed
/// バイト数が約 16MB で十分に構築可能だが、`MAX_BATCH_QUERIES` 件のクエリで
/// 走査すると積和は約 3.35 × 10^10 回に達する）。本モジュールは CPU
/// リファレンス実装（モジュール冒頭コメント参照）でスカラー積和を行うため、
/// 1 回の呼び出しが有限時間で完了するよう、スカラー積和が 1 秒あたり概ね
/// 10^9 回のオーダーで進む前提で数秒〜十数秒程度に収まる規模
/// （10^10 = 100 億回）を独立の上限として設ける。
pub const MAX_BATCH_WORK: usize = 10_000_000_000;

/// [`row_buffer_pool`] が保持してよいアイドルバッファの総バイト量上限（TASK-157・
/// CORE-15 ポインタ。設定値であり、外部入力からは到達不能な内部定数。CORE-12 と
/// 同じ「上書き機構自体を設けない」方針）。1 行分の f32 スクラッチバッファ
/// （最大 [`MAX_BATCH_DIM`] 要素 = 32KiB）を複数サイズクラス分アイドル保持できる
/// 程度の桁とし、[`MAX_BATCH_TOTAL_BYTES`]（常駐行列本体の上限。1 GiB）と比べて
/// 十分小さい値に抑える（プールは性能最適化であり、常駐行列本体の予算を圧迫しない）。
///
/// Issue #170（INDEX-4・TASK-122 実装後の再整合）: 一括投入経路（`incremental.rs`）は
/// 本プールを直接使わない。本プールのピーク理論上限（本定数＋最大サイズクラス 1 個分）は
/// 一括投入経路の合計予算の fail-closed 既定値 `incremental::MAX_INDEX_TOTAL_BYTES`
/// （64 MiB）以下という桁感を保つよう独立に設定してあるが、これは**プール単体の上限**が
/// 一括投入経路の上限と同程度の桁に収まることの documentation-level な整合であり、
/// 両経路が同一プロセス内で同時に確保し得るプロセス全体のピーク使用量
/// （概ね `incremental_usage + pool_peak`）を共通予算内に収める保証ではない
/// （両経路の合計を上限と比較する実行時の共有予算制御・一方の確保中に他方を縮小する
/// 仕組みは本モジュールに存在しない）。この「独立上限としての桁感」の大小関係のみを
/// `tests::pool_quota_stays_within_batch_limits_budget` で固定する（`buffer_pool.rs`
/// モジュールコメントの (b) に対応）。
const ROW_BUFFER_POOL_QUOTA_BYTES: usize = 16 * 1024 * 1024;

/// バッチ経路の行デコード用スクラッチバッファのプロセス共有プール（TASK-157・
/// CORE-15 ポインタ）。`run_batch_search` の複数回呼び出し（`BatchEngine::batch_search`・
/// `batch_fallback.rs::FallbackBatchEngine` 経由の双方）間でアロケータ呼び出しを
/// 削減するための最適化であり、プールが空でもフォールブルな新規確保にフォールバック
/// するため正当性には影響しない。`Mutex` はテスト・複数スレッドからの並行呼び出しに
/// 対して安全側に倒す（`BatchBackend: Send + Sync` の契約と整合）。
fn row_buffer_pool() -> &'static std::sync::Mutex<crate::buffer_pool::BufferPool> {
    static POOL: std::sync::OnceLock<std::sync::Mutex<crate::buffer_pool::BufferPool>> =
        std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        std::sync::Mutex::new(crate::buffer_pool::BufferPool::new(
            ROW_BUFFER_POOL_QUOTA_BYTES,
        ))
    })
}

/// バッチ検索エンジンのエラー（fail-closed）。メッセージは英語（wire プロトコル
/// 互換性・運用ツール連携のため。japanese-style.md 準拠）。他テナントのデータ・
/// 存在情報を含めない。
#[derive(Debug, Clone, PartialEq)]
pub enum BatchSearchError {
    /// バッチのクエリ件数が [`MAX_BATCH_QUERIES`] を超過した。
    TooManyQueries { count: usize, max: usize },
    /// 常駐行列の行数が [`MAX_BATCH_ROWS`] を超過した。
    TooManyRows { count: usize, max: usize },
    /// 次元数が [`MAX_BATCH_DIM`] を超過した、または 0。
    InvalidDim { dim: usize, max: usize },
    /// 行数・次元数はそれぞれ上限内でも、組み合わせた総確保バイト量が上限
    /// （`ResidentMatrix::build` の [`MAX_BATCH_TOTAL_BYTES`]、または
    /// `DynamicWindowAggregator` の [`MAX_BATCH_AGGREGATOR_TOTAL_BYTES`]）を
    /// 超過した。
    CapacityExceeded { total_bytes: usize, max: usize },
    /// クエリベクトルの次元が常駐行列の次元と一致しない。
    DimMismatch { expected: usize, found: usize },
    /// クエリベクトルに非有限値（NaN/Inf）が含まれる（untrusted 入力の明示拒否）。
    NonFiniteQuery { query_index: usize },
    /// 常駐行列構築時、`ids`/`vectors`/可視性マスクの長さ不整合。
    ArenaLengthMismatch,
    /// 常駐行列構築時、**同一テナント内**で `id` に重複があった（対象ビヘイビア:
    /// TABLE-12・RLS-9）。行 `id` の一意性スコープはテナント内に閉じているため、
    /// 異なるテナントが同じ `id` を持つことは正当な入力であり拒否しない
    /// （codex レビュー指摘対応の当初実装は `id` 単独の重複を拒否していたが、
    /// PR #205 で `(tenant_id, id)` 複合キーの重複判定へ改めた）。重複行そのものの
    /// id は他テナントの存在情報を漏らしうるためエラーには含めない。
    DuplicateRowId,
    /// 常駐行列構築時、`tenant_ids` の要素が [`crate::storage::MAX_TENANT_ID_LEN`]
    /// を超過した（Cursor Bugbot 指摘対応: 容量チェックが `MAX_TENANT_ID_LEN` で
    /// 予算計上する一方、実際の各要素長を検証せずに `clone` していたため、想定を
    /// 超える長さの文字列で予算超過が起こり得た）。実際の文字列は他テナントの
    /// 存在情報を漏らしうるため含めない。
    TenantIdTooLong { len: usize, max: usize },
    /// クエリの `k` が [`MAX_BATCH_K`] を超過した、または 0。
    InvalidK { k: usize, max: usize },
    /// バッチ全体の `sum(k)` が [`MAX_BATCH_TOTAL_K`] を超過した（codex P1
    /// 指摘対応: クエリ数・`k` を個別に上限検証するだけでは積が無制限になり、
    /// 通常のヒープ確保（`kernel.rs::TopKSelector` 内部の `BinaryHeap`）で
    /// 数千万要素規模の成長が起こり得た）。
    TotalKExceeded { total_k: usize, max: usize },
    /// `DynamicWindowAggregator::push` に渡されたクエリの次元が、同じ窓に
    /// 先に積まれたクエリの次元と一致しない（codex P1 指摘対応）。
    /// `DimMismatch`（常駐行列に対するクエリ次元不一致）と意図的に区別する:
    /// 呼び出し元（wire-server 等）がエラーを一括処理する場合でも、
    /// 「クエリがテーブルの次元に合わない」のか「同じ窓の他クエリと次元が
    /// 揃っていない」のかを区別できるようにするため（`wire_code` 契約上、
    /// 別条件として扱いうる）。
    WindowDimMismatch { expected: usize, found: usize },
    /// バッチ検索の総積和演算数（`rows × queries × dim`）が [`MAX_BATCH_WORK`]
    /// を超過した（codex P1 指摘対応: 計算量 DoS 対策。走査開始前に
    /// fail-closed で拒否する）。
    WorkBudgetExceeded { work: usize, max: usize },
    /// `check_capacity` 相当のアロケーション前上限検証を通過した後、実際の
    /// `try_reserve_exact` がメモリ不足で失敗した（Cursor Bugbot 指摘対応:
    /// `ResidentMatrix::build` は上限検証後も `HashSet::with_capacity`・
    /// `Vec::with_capacity`・`to_vec()`（失敗時に abort する内部確保）を
    /// 使っていたため、`arena.rs::ArenaError::AllocationFailed` と同じ方針で
    /// フォールブルな確保へ置き換えた。security.md「不安全な設計｜無制限
    /// リソース確保（DoS）」対応。メッセージはプログラム出力文字列のため英語）。
    AllocationFailed(String),
    /// バッチ内の結果が、選出後に独立再検証したクエリ別可視集合と食い違った
    /// （テナント混入の疑い。`core.rs::CoreError::ProviderResultRejected` と同じ
    /// fail-closed 思想。結果を一切返さない）。
    TenantMaskViolation,
    /// `batch_fallback.rs::BatchBackend`（差し替え可能な primary バックエンド。
    /// TASK-129・CORE-8 ポインタ）が返した結果が、id・可視性以外の構造契約
    /// （クエリ数との整合・件数上限 `k`・id 重複なし・スコア有限性・
    /// スコア降順/同点 id 昇順）を満たさなかった。`core.rs::CoreError::
    /// ProviderResultRejected` と同じ「untrusted provider 出力を信頼済み
    /// 集合と突き合わせて検証し、1 件でも違反すれば結果を一切返さない」
    /// fail-closed 思想を、id/tenant 混入固有の [`Self::TenantMaskViolation`]
    /// とは別に区別する（構造契約違反と可視性違反は異なる原因として wire_code
    /// 設計上も切り分けたいため）。
    PrimaryResultRejected,
}

impl fmt::Display for BatchSearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchSearchError::TooManyQueries { count, max } => {
                write!(f, "batch_search: too many queries: {count} (max {max})")
            }
            BatchSearchError::TooManyRows { count, max } => {
                write!(f, "batch_search: too many rows: {count} (max {max})")
            }
            BatchSearchError::InvalidDim { dim, max } => {
                write!(f, "batch_search: invalid dim: {dim} (max {max}, must be > 0)")
            }
            BatchSearchError::CapacityExceeded { total_bytes, max } => write!(
                f,
                "batch_search: total allocation bytes {total_bytes} exceeds limit {max}"
            ),
            BatchSearchError::DimMismatch { expected, found } => write!(
                f,
                "batch_search: query dim mismatch: expected={expected} found={found}"
            ),
            BatchSearchError::NonFiniteQuery { query_index } => write!(
                f,
                "batch_search: query {query_index} contains non-finite value"
            ),
            BatchSearchError::ArenaLengthMismatch => {
                write!(f, "batch_search: arena ids/vectors/mask length mismatch")
            }
            BatchSearchError::DuplicateRowId => {
                write!(f, "batch_search: resident matrix contains duplicate row ids")
            }
            BatchSearchError::TenantIdTooLong { len, max } => {
                write!(f, "batch_search: tenant_id length {len} exceeds limit {max}")
            }
            BatchSearchError::InvalidK { k, max } => {
                write!(f, "batch_search: invalid k: {k} (must be 1..={max})")
            }
            BatchSearchError::TotalKExceeded { total_k, max } => write!(
                f,
                "batch_search: total k across batch {total_k} exceeds limit {max}"
            ),
            BatchSearchError::WindowDimMismatch { expected, found } => write!(
                f,
                "batch_search: dynamic window aggregator dim mismatch: expected={expected} found={found}"
            ),
            BatchSearchError::WorkBudgetExceeded { work, max } => write!(
                f,
                "batch_search: batch work budget {work} exceeds limit {max}"
            ),
            BatchSearchError::AllocationFailed(msg) => {
                write!(f, "batch_search: allocation failed: {msg}")
            }
            BatchSearchError::TenantMaskViolation => {
                write!(f, "batch_search: result violated per-query tenant mask")
            }
            BatchSearchError::PrimaryResultRejected => write!(
                f,
                "batch_search: primary backend result violated batch result contract"
            ),
        }
    }
}

impl std::error::Error for BatchSearchError {}

// ---------------------------------------------------------------------
// CORE-16 ポインタ: 常駐コピー限定の f16 2 要素/u32 パック。
//
// 格納・CPU 経路・クエリベクトルの dtype は f32 のまま変更しない。本関数群は
// 常駐ベース行列相当の表現へ変換する層としてのみ使う（呼び出し元が f32 の
// オリジナルを保持し続ける前提）。IEEE 754 binary16 の丸めは
// round-to-nearest-even を実装する。
// ---------------------------------------------------------------------

/// f16 常駐表現の変換関数群（`f32_to_f16_bits`／`f16_bits_to_f32`）は
/// `crate::f16`（Issue #514）へ移設した。`pack_f16x2`／`unpack_f16x2` の
/// 公開シグネチャ・挙動は移設前とビット同一（`crate::f16` 側の unit test・
/// 本ファイル下部の往復テストで担保）。
use crate::f16::{f16_bits_to_f32, f32_to_f16_bits};

/// 2 要素の f32 を 1 個の u32 へ f16 パックする（CORE-16 ポインタ: 常駐コピーの
/// 表現）。下位 16 ビットに `a`、上位 16 ビットに `b` を格納する
/// （`unpack2x16float` 相当の並び順と一致させる）。
pub fn pack_f16x2(a: f32, b: f32) -> u32 {
    let lo = f32_to_f16_bits(a) as u32;
    let hi = f32_to_f16_bits(b) as u32;
    lo | (hi << 16)
}

/// [`pack_f16x2`] の逆変換。
pub fn unpack_f16x2(packed: u32) -> (f32, f32) {
    let lo = (packed & 0xFFFF) as u16;
    let hi = ((packed >> 16) & 0xFFFF) as u16;
    (f16_bits_to_f32(lo), f16_bits_to_f32(hi))
}

// ---------------------------------------------------------------------
// 常駐ベース行列（CORE-16 ポインタ: パック表現を保持する一括インデクシング結果）。
// ---------------------------------------------------------------------

/// `Vec::try_reserve_exact` の失敗を [`BatchSearchError::AllocationFailed`] へ変換する
/// 共通ヘルパー（[`ResidentMatrix::build`] 専用。`arena.rs` の同名ヘルパーと同方針
/// だが `pub(crate)` ではないため個別に持つ）。`try_reserve`（amortized 成長）では
/// なく `try_reserve_exact`（要求量ちょうど）を使うのは、呼び出し元が
/// アロケーション前に検証済みの論理必要量どおりに実確保量を抑えるため
/// （`arena.rs::try_reserve_exact` と同じ理由）。
///
/// `pub(crate)`: `gpu_batch.rs` の選出後解決ループも同じフォールブル確保方針を
/// 共有する（Issue #178。GPU 経路だけ abort-on-OOM な `Vec::with_capacity` に
/// 分岐しないよう単一の真実源にする）。
pub(crate) fn try_reserve_exact<T>(
    buf: &mut Vec<T>,
    additional: usize,
    what: &str,
) -> Result<(), BatchSearchError> {
    buf.try_reserve_exact(additional)
        .map_err(|e| BatchSearchError::AllocationFailed(format!("failed to reserve {what}: {e}")))
}

/// `&str` を `String` へフォールブルに複製する（`ResidentMatrix::build` が
/// `tenant_ids` の複製に使うのと同じ理由: `String::from`/`.into()`（`SearchHit::
/// new` が内部で使う）は失敗時に abort するため、選出後の解決ループのように
/// バッチ全体で最大 [`MAX_BATCH_TOTAL_K`] 回呼ばれうる経路では使わない）。
/// `pub(crate)`: `gpu_batch.rs` の選出後解決ループも `SearchHit::tenant_id` の
/// 複製に本ヘルパーを使う（Issue #178）。
pub(crate) fn try_owned_str(s: &str) -> Result<String, BatchSearchError> {
    let mut owned = String::new();
    owned.try_reserve_exact(s.len()).map_err(|e| {
        BatchSearchError::AllocationFailed(format!("failed to reserve tenant_id: {e}"))
    })?;
    owned.push_str(s);
    Ok(owned)
}

/// 1 テナント分の積和演算数（`rows × queries × dim`）を checked 演算で計算する。
/// 積のオーバーフローは [`MAX_BATCH_WORK`] 超過とみなす（[`compute_batch_work`]
/// 専用のヘルパー。境界値を直接検証できるよう独立関数へ切り出す）。
///
/// `pub(crate)`: `gpu_batch.rs` の GPU 経路も dispatch 前の総量ガードに同じ
/// 計算式を再利用する（CPU 経路と GPU 経路で計算量 DoS ガードの基準式が
/// 分岐しないよう、この関数を単一の真実源とする。Issue #178 レビュー
/// 指摘対応）。
pub(crate) fn compute_tenant_work(
    rows: usize,
    queries: usize,
    dim: usize,
) -> Result<usize, BatchSearchError> {
    rows.checked_mul(queries)
        .and_then(|v| v.checked_mul(dim))
        .ok_or(BatchSearchError::WorkBudgetExceeded {
            work: usize::MAX,
            max: MAX_BATCH_WORK,
        })
}

/// `BatchEngine::batch_search` の走査開始前に総積和演算数を見積もり、
/// [`MAX_BATCH_WORK`] 超過を fail-closed に拒否する（codex P1 指摘対応:
/// 計算量 DoS 対策）。
///
/// `per_tenant` はテナントごとの `(行数, クエリ数)` ペア（バッチのテナント
/// 集合と 1 対 1 対応する想定）。単純に「全一致行数 × 全クエリ数」で課金
/// すると、複数テナントが混在するバッチで過大計上になる（codex P1 指摘対応:
/// 実際の走査は行ごとに `PolicyContext::is_visible` を満たすクエリとしか
/// 積和しないため、実コストはテナントごとの `行数 × クエリ数` の総和で
/// あり、`(全行数) × (全クエリ数)` はテナント間の組み合わせも含む厳密な
/// 上位互換ではあるが不必要に過大 — 例: tenant-a/b が各 1,000 行・各 1,000
/// クエリの場合、実コストは `1,000 * 1,000 + 1,000 * 1,000` だが、
/// `(全行数) × (全クエリ数)` は `2,000 * 2,000` と 2 倍に見積もる）。
/// テナントごとの積を [`compute_tenant_work`] で個別に checked 演算し、
/// 総和も checked 演算で合算する（積・和のどちらのオーバーフローも
/// [`MAX_BATCH_WORK`] 超過とみなす）。実データ（常駐行列・クエリ列）を
/// 確保せずに境界値を直接検証できるよう独立関数へ切り出す
/// （`arena.rs::check_capacity` と同じテスト容易性の考え方）。
fn compute_batch_work(
    per_tenant: impl IntoIterator<Item = (usize, usize)>,
    dim: usize,
) -> Result<usize, BatchSearchError> {
    let mut total: usize = 0;
    for (rows, queries) in per_tenant {
        let tenant_work = compute_tenant_work(rows, queries, dim)?;
        total = total
            .checked_add(tenant_work)
            .ok_or(BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            })?;
        if total > MAX_BATCH_WORK {
            return Err(BatchSearchError::WorkBudgetExceeded {
                work: total,
                max: MAX_BATCH_WORK,
            });
        }
    }
    Ok(total)
}

/// 一括インデクシングで構築する常駐ベース行列。可視性フィルタ済みの全行を
/// f16 2 要素/u32 パックで保持する。呼び出し元（`core.rs` 相当）は元の f32
/// アリーナを別途保持し続け、本構造体はバッチ検索専用の副次表現として扱う。
/// 現段階は CPU 上でこの表現を保持・走査する参照実装であり、実際の GPU
/// 実行は行わない（モジュール冒頭コメント参照）。
#[derive(Debug)]
pub struct ResidentMatrix {
    ids: Vec<u64>,
    /// テナント境界判定用: 行 `i` の所属テナント ID（`PolicyContext::is_visible` の
    /// 単一照合パスに渡すため、呼び出し元が構築時に確定させる）。
    tenant_ids: Vec<String>,
    /// 可視性ラベル。`tenant_ids[i]` と対で `PolicyContext::is_visible` の
    /// 単一照合パスへ渡す（codex P0 指摘対応: `BatchEngine::batch_search`
    /// はテナント文字列の等価比較だけでなく、この可視性ラベルも合わせて
    /// `PolicyContext::is_visible` で判定する）。
    visibilities: Vec<Visibility>,
    dim: usize,
    /// `ids.len() * dim.div_ceil(2)` 要素のパック済み行列（行優先）。
    packed: Vec<u32>,
}

impl ResidentMatrix {
    /// 可視性フィルタ済みの行集合から常駐行列を構築する（CORE-16 ポインタ）。`ids` /
    /// `tenant_ids` / `visibilities` / `vectors`（`ids.len() * dim` 要素、
    /// f32・行優先）の長さが整合しない場合は [`BatchSearchError::ArenaLengthMismatch`]。
    /// 次元・行数の上限検証はアロケーション前に行う（untrusted な行数・次元を
    /// そのまま `Vec::with_capacity` へ渡さない。coding-rust.md 準拠）。
    ///
    /// `visibilities[i]` は `tenant_ids[i]` と対応する行 `i` の可視性ラベルで、
    /// `BatchEngine::batch_search` が `PolicyContext::is_visible` の単一
    /// 照合パスへ渡す（codex P0 指摘対応）。
    ///
    /// # 事前条件: 行の並び順（結果の同点タイブレークに直結する）
    ///
    /// 本メソッドは渡された行を**並べ替えず**、順序の検証も行わない。行 `i` は
    /// そのままスロット `i` になる。[`BatchHit`] の同点タイブレークは選出段の
    /// 候補識別子（＝スロット番号）昇順であるため、結果順序は呼び出し元が渡す
    /// 行の並びで決まる。
    ///
    /// 公開ドキュメント（README「検索結果順序」・`docs/design/
    /// rrf-tie-break-determinism.md`）が述べる `(tenant_id, id)` 昇順
    /// （単一テナント内では id 昇順）を満たすには、呼び出し元が
    /// `(tenant_id, id)` キーの昇順で行を渡す必要がある。これは `Storage` の
    /// 行キー順（`(tenant_id, id)`。TABLE-12）でスキャンした結果をそのまま
    /// 渡せば自然に満たされる事前条件であり、本メソッドで再ソートすると
    /// 一括インデクシング経路のコストが増えるため呼び出し元の責務としている
    /// （codex-review P1 指摘対応で明文化）。事前条件を満たさない並びを渡した
    /// 場合も結果は決定的（スロット昇順）だが、`(tenant_id, id)` 昇順には
    /// ならない。
    ///
    /// # 信頼境界（P0 レビュー対応で明文化）
    ///
    /// `tenant_ids` / `visibilities` は本メソッドの引数としては未認証の値である。
    /// `PolicyContext` によるテナント境界の強制は `batch_search` 側の
    /// クエリ入力（`BatchQuery::ctx`）にのみ適用され、本メソッドはその
    /// 判定対象となる行メタデータの出所までは検証できない。呼び出し元
    /// （wire-server から到達しない、engine 内部の一括インデクシング経路）は
    /// `core.rs::VectorCore::search` が `VectorArena::build_filtered` で
    /// 行うのと同様に、`Storage` から読み出した実テナント ID・可視性を
    /// そのまま渡す責務を負う。本メソッドは wire プロトコル入力からの
    /// 直接呼び出し先ではないため、untrusted なユーザー入力が
    /// `tenant_ids`/`visibilities` に混入しない前提で設計されている。
    pub fn build(
        ids: &[u64],
        tenant_ids: &[String],
        visibilities: &[Visibility],
        dim: usize,
        vectors: &[f32],
    ) -> Result<Self, BatchSearchError> {
        if dim == 0 || dim > MAX_BATCH_DIM {
            return Err(BatchSearchError::InvalidDim {
                dim,
                max: MAX_BATCH_DIM,
            });
        }
        if ids.len() > MAX_BATCH_ROWS {
            return Err(BatchSearchError::TooManyRows {
                count: ids.len(),
                max: MAX_BATCH_ROWS,
            });
        }
        if ids.len() != tenant_ids.len() || ids.len() != visibilities.len() {
            return Err(BatchSearchError::ArenaLengthMismatch);
        }

        let packed_per_row = dim.div_ceil(2);
        let packed_len =
            ids.len()
                .checked_mul(packed_per_row)
                .ok_or(BatchSearchError::TooManyRows {
                    count: ids.len(),
                    max: MAX_BATCH_ROWS,
                })?;

        // `packed` だけでなく、同時に確保する `ids`/`tenant_ids`（呼び出し元の
        // 所有権を持つコピー、下部の `ids.to_vec()`/`tenant_ids.to_vec()`）の
        // 見積もりバイト量も合算した総量を、`vectors` の長さ検証・実確保より前に
        // 検証する（arena.rs::check_capacity と同方針。行数・次元数を独立にしか
        // 検証しないと、両方が上限値の場合に `packed` 単体で約 16.4GB の巨大確保が
        // 発生し得るため。`vectors.len()` チェックより先に置くことで、テスト・
        // 呼び出し元が巨大な `vectors` バッファを用意しなくても本チェックへ
        // 到達できる）。
        let packed_bytes = packed_len.checked_mul(std::mem::size_of::<u32>()).ok_or(
            BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_TOTAL_BYTES,
            },
        )?;
        let per_row_aux_bytes = std::mem::size_of::<u64>()
            .checked_add(std::mem::size_of::<String>())
            .and_then(|v| v.checked_add(crate::storage::MAX_TENANT_ID_LEN as usize))
            .and_then(|v| v.checked_add(std::mem::size_of::<Visibility>()))
            .ok_or(BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_TOTAL_BYTES,
            })?;
        let aux_bytes =
            ids.len()
                .checked_mul(per_row_aux_bytes)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_TOTAL_BYTES,
                })?;
        let total_bytes =
            packed_bytes
                .checked_add(aux_bytes)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_TOTAL_BYTES,
                })?;
        if total_bytes > MAX_BATCH_TOTAL_BYTES {
            return Err(BatchSearchError::CapacityExceeded {
                total_bytes,
                max: MAX_BATCH_TOTAL_BYTES,
            });
        }

        let expected_len = ids
            .len()
            .checked_mul(dim)
            .ok_or(BatchSearchError::ArenaLengthMismatch)?;
        if vectors.len() != expected_len {
            return Err(BatchSearchError::ArenaLengthMismatch);
        }

        // (tenant_id, id) 複合キーの一意性を検証する（対象ビヘイビア: TABLE-12・
        // RLS-9。容量・長さチェックより後に置き、cheap な拒否経路
        // （`CapacityExceeded`・`ArenaLengthMismatch`）を先に済ませてから、
        // 検証コスト・確保量が `ids.len()` に比例する本チェックへ進む）。行 `id`
        // の一意性スコープはテナント内に閉じている（`catalog.rs::
        // user_rows_table_def`）ため、異なるテナントが同じ `id` を持つ入力は
        // 正当であり拒否しない。`run_batch_search` の選出後独立再検証は
        // 常駐行列のスロット番号を候補識別子にして `(tenant_id, id)` へ解決する
        // 方式（`resolve_batch_slot` 参照）へ改めたため、id 単独の重複を拒否する
        // 必要はなく、同一テナント内の重複だけを拒否すれば選出後再検証の一意性
        // 前提（スロット → 行が 1 対 1）は保たれる（PR #205: 旧実装は id 単独の
        // 重複を拒否しており、他テナントが同じ id を持つだけで正当な入力を誤って
        // 拒否していた）。
        // `HashSet::with_capacity` は失敗時に abort するため使わず、
        // `try_reserve`（フォールブル）で確保する（Cursor Bugbot 指摘対応）。
        let mut seen_row_keys: std::collections::HashSet<(&str, u64)> =
            std::collections::HashSet::new();
        seen_row_keys.try_reserve(ids.len()).map_err(|e| {
            BatchSearchError::AllocationFailed(format!("failed to reserve row key set: {e}"))
        })?;
        for (&id, tenant) in ids.iter().zip(tenant_ids) {
            if !seen_row_keys.insert((tenant.as_str(), id)) {
                return Err(BatchSearchError::DuplicateRowId);
            }
        }

        // `tenant_ids` の各要素長を検証する（Cursor Bugbot 指摘対応: 上の容量
        // チェックは `per_row_aux_bytes` を `MAX_TENANT_ID_LEN` で見積もって
        // いるが、実際の要素長を検証せずに `tenant_ids.to_vec()` していたため、
        // 見積もりを超える長さの文字列が混入すると総確保量が
        // `MAX_BATCH_TOTAL_BYTES` の予算を超過し得た）。`storage.rs::encode_row`
        // の tenant_id 長検証と同じ上限を fail-closed で適用する。
        for tenant in tenant_ids {
            let len = tenant.len();
            if len > crate::storage::MAX_TENANT_ID_LEN as usize {
                return Err(BatchSearchError::TenantIdTooLong {
                    len,
                    max: crate::storage::MAX_TENANT_ID_LEN as usize,
                });
            }
        }

        // 以降の 3 本のバッファはいずれも `check_capacity` 相当の上限検証を
        // 通過済みの量だけを `try_reserve_exact`（フォールブル・要求量ちょうど）で
        // 確保する。`Vec::with_capacity`・`to_vec()` は失敗時に abort するため
        // 使わない（Cursor Bugbot 指摘対応。`arena.rs::try_reserve_exact` と
        // 同方針）。
        let mut packed: Vec<u32> = Vec::new();
        try_reserve_exact(&mut packed, packed_len, "packed")?;
        for row in vectors.chunks(dim) {
            for pair in row.chunks(2) {
                let a = pair.first().copied().unwrap_or(0.0);
                let b = pair.get(1).copied().unwrap_or(0.0);
                packed.push(pack_f16x2(a, b));
            }
        }

        let mut owned_ids: Vec<u64> = Vec::new();
        try_reserve_exact(&mut owned_ids, ids.len(), "ids")?;
        owned_ids.extend_from_slice(ids);

        // `tenant_ids` はコンテナ（`Vec<String>` が保持する `String` ハンドル分）
        // をフォールブルに確保したうえで、各要素の実体（ヒープ上の文字列
        // バイト列）も `String::clone`（abort-on-OOM）ではなく
        // `try_reserve_exact` + `push_str` でフォールブルに構築する
        // （codex P1 指摘対応: 各要素は `MAX_TENANT_ID_LEN` で上限検証済みの
        // 小さい固定長だが、最大 `MAX_BATCH_ROWS`（100 万）回 `clone` が
        // 呼ばれるため、ホスト側のメモリ不足時に `Result` 契約を経ずに
        // abort しうる経路として残さない）。
        let mut owned_tenant_ids: Vec<String> = Vec::new();
        try_reserve_exact(&mut owned_tenant_ids, tenant_ids.len(), "tenant_ids")?;
        for tenant in tenant_ids {
            let mut owned = String::new();
            owned.try_reserve_exact(tenant.len()).map_err(|e| {
                BatchSearchError::AllocationFailed(format!("failed to reserve tenant_id: {e}"))
            })?;
            owned.push_str(tenant);
            owned_tenant_ids.push(owned);
        }

        // `visibilities` は `Copy` 型（ヒープ確保を持たない）なので、コンテナ
        // 自体のフォールブル確保だけで十分（`ids` と同じ扱い）。
        let mut owned_visibilities: Vec<Visibility> = Vec::new();
        try_reserve_exact(&mut owned_visibilities, visibilities.len(), "visibilities")?;
        owned_visibilities.extend_from_slice(visibilities);

        Ok(Self {
            ids: owned_ids,
            tenant_ids: owned_tenant_ids,
            visibilities: owned_visibilities,
            dim,
            packed,
        })
    }

    pub fn row_count(&self) -> usize {
        self.ids.len()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// パック済み行列（行優先・`row_count() * dim().div_ceil(2)` 要素）への
    /// 読み取り専用参照。`gpu_batch.rs::GpuBatchBackend` が GPU の STORAGE
    /// バッファへアップロードする実データとして使う（TASK-128〜130・Issue #178
    /// ポインタ）。CPU 参照実装（[`Self::row_f32_into`]）と表現を共有するだけで、
    /// 本メソッド自体はデコードを行わない。
    pub(crate) fn packed(&self) -> &[u32] {
        &self.packed
    }

    /// 行 `idx` を f16 往復で復元し、呼び出し元が用意したバッファへ書き込む
    /// （積和計算専用。格納表現自体は f32 のまま別途保持する呼び出し元の責務で
    /// あり、本メソッドはバッチスコア計算のためだけに使う）。バッファを
    /// 呼び出し元が使い回すことで、`BatchEngine::batch_search` がクエリ数
    /// 分だけ同一行を毎回ヒープ確保し直す（codex レビュー指摘対応）のを避ける。
    ///
    /// `pub(crate)`（Issue #542）: `gpu_batch::packed_i8::GpuI8BatchBackend`
    /// が i8 パック常駐の構築入力・候補生成後の f32 再スコア（真値は f16
    /// 復号 f32 dot のまま。CORE-8 の縮退対称性と同じ「候補生成のみ低精度・
    /// 最終スコアは f32」契約）の両方でこのデコードを再利用する。
    pub(crate) fn row_f32_into(&self, idx: usize, out: &mut Vec<f32>) -> Option<()> {
        let packed_per_row = self.dim.div_ceil(2);
        let start = idx.checked_mul(packed_per_row)?;
        let end = start.checked_add(packed_per_row)?;
        let row = self.packed.get(start..end)?;
        out.clear();
        for &p in row {
            let (a, b) = unpack_f16x2(p);
            out.push(a);
            if out.len() < self.dim {
                out.push(b);
            }
        }
        Some(())
    }
}

// ---------------------------------------------------------------------
// CORE-7 ポインタ: 動的窓集約バッファ。
// ---------------------------------------------------------------------

/// 集約するか否かの判定（副作用なしの純関数）。`pending_after_pop` はキューから
/// 1 件取り出した直後に後続が存在するかどうか（呼び出し側のキュー実装に依存
/// しない最小のインターフェース）。ディスパッチ決定表本体（`dispatch.rs::select_execution_path`。
/// TASK-155・CORE-11, 12）はこの判定を呼び出す側であり、本関数自体は経路選択ロジックを
/// 持たない（二重管理を避けるための契約）。
pub fn should_aggregate_into_batch(pending_after_pop: bool) -> bool {
    pending_after_pop
}

/// 動的窓での集約バッファ。[`should_aggregate_into_batch`] が `true` を返した
/// キュー取り出し文脈でのみ使う想定。件数・次元・総バイト量は [`Self::push`]
/// 自身が検証する（codex P1 指摘対応: 以前は公開 API である `push` が無検証で
/// 内部 `Vec` を無制限に成長させており、呼び出し側の自己規律のみに依存していた。
/// 呼び出し側のキュー実装がバグっていても本構造体単体で fail-closed に上限を
/// 保証する）。
#[derive(Debug, Default)]
pub struct DynamicWindowAggregator {
    queries: Vec<Vec<f32>>,
    /// 最初の [`Self::push`] で確定する次元。以降の `push` はこの次元との
    /// 一致を要求する（次元混在バッチを拒否する。[`Self::drain`] で窓を
    /// 使い切ると次の窓のためにリセットされる）。
    dim: Option<usize>,
}

impl DynamicWindowAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// クエリ 1 件を窓へ追加する。以下を fail-closed に検証してから
    /// フォールブルに確保する:
    /// - 件数が [`MAX_BATCH_QUERIES`] を超えないこと
    /// - 次元が 1 以上 [`MAX_BATCH_DIM`] 以下で、既存クエリと同一であること
    ///   （1 件目の `push` で次元を確定する）
    /// - 累積総バイト量が [`MAX_BATCH_AGGREGATOR_TOTAL_BYTES`] を超えないこと
    ///
    /// 内部 `Vec<Vec<f32>>` の成長は `Vec::push`（失敗時に abort する内部
    /// 確保）ではなく `try_reserve_exact` で行う（`ResidentMatrix::build` 用の
    /// 共通ヘルパーを再利用。coding-rust.md「無制限確保禁止」準拠）。渡された
    /// `query: Vec<f32>` 自体は呼び出し元が既に確保済みの所有権を移動する
    /// だけなので、要素本体の再確保は発生しない。
    pub fn push(&mut self, query: Vec<f32>) -> Result<(), BatchSearchError> {
        if self.queries.len() >= MAX_BATCH_QUERIES {
            return Err(BatchSearchError::TooManyQueries {
                count: self.queries.len() + 1,
                max: MAX_BATCH_QUERIES,
            });
        }
        let dim = query.len();
        if dim == 0 || dim > MAX_BATCH_DIM {
            return Err(BatchSearchError::InvalidDim {
                dim,
                max: MAX_BATCH_DIM,
            });
        }
        if let Some(expected) = self.dim {
            if expected != dim {
                return Err(BatchSearchError::WindowDimMismatch {
                    expected,
                    found: dim,
                });
            }
        }

        let per_query_bytes = dim.checked_mul(std::mem::size_of::<f32>()).ok_or(
            BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_AGGREGATOR_TOTAL_BYTES,
            },
        )?;
        let next_count =
            self.queries
                .len()
                .checked_add(1)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_AGGREGATOR_TOTAL_BYTES,
                })?;
        let total_bytes =
            next_count
                .checked_mul(per_query_bytes)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_AGGREGATOR_TOTAL_BYTES,
                })?;
        if total_bytes > MAX_BATCH_AGGREGATOR_TOTAL_BYTES {
            return Err(BatchSearchError::CapacityExceeded {
                total_bytes,
                max: MAX_BATCH_AGGREGATOR_TOTAL_BYTES,
            });
        }

        // amortized 成長: capacity が尽きたときだけ倍々に増やし、`push` の
        // たびに `try_reserve_exact(1)` する（= 呼び出し回数分の再確保・
        // memcpy が発生する）のを避ける。成長候補は既知の絶対上限
        // [`MAX_BATCH_QUERIES`] で頭打ちにするため、`arena.rs::
        // GrowableArenaBuffers::ensure_capacity` のような追加の容量見積もり
        // 検証（`check_capacity` 相当）は不要（本構造体の要素は `Vec<f32>`
        // ハンドルのみで、成長候補自体の総バイト量は上の総バイト量チェックが
        // 別途カバーする）。
        if self.queries.len() == self.queries.capacity() {
            let next_capacity = self
                .queries
                .capacity()
                .checked_mul(2)
                .unwrap_or(MAX_BATCH_QUERIES)
                .clamp(4, MAX_BATCH_QUERIES);
            let additional = next_capacity.saturating_sub(self.queries.capacity());
            if additional > 0 {
                try_reserve_exact(
                    &mut self.queries,
                    additional,
                    "dynamic window aggregator queries",
                )?;
            }
        }
        self.dim = Some(dim);
        self.queries.push(query);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.queries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }

    /// 窓を確定し、集約済みクエリ集合を取り出す（窓は 1 回使い切り）。次元の
    /// 確定状態もリセットし、次の窓では別次元のバッチを受け付けられるように
    /// する。
    pub fn drain(&mut self) -> Vec<Vec<f32>> {
        self.dim = None;
        std::mem::take(&mut self.queries)
    }
}

// ---------------------------------------------------------------------
// CORE-6/7 ポインタ: バッチ検索本体。スコア計算の積和は f16 往復した行から
// f32 で行う（実際の GPU 実行は追加しない。本モジュール冒頭コメント参照）。
// Top-k 抽出は `kernel.rs::TopKSelector` を共用する。
// ---------------------------------------------------------------------

/// クエリ 1 件分のバッチ入力。可視性判定は `core.rs::VectorCore::search` と
/// 同じ [`PolicyContext::is_visible`] の単一照合パスへ委譲する（codex P0
/// 指摘対応: 以前は `tenant_id: &str` を公開フィールドで直接受け取っており、
/// 呼び出し元が任意の文字列を指定するだけで他テナントの行を検索できて
/// しまっていた。本モジュールが独自にテナント文字列を比較する経路は作らず、
/// `PolicyContext` をテナント境界判定の唯一の正当な入力経路とする engine
/// 全体の既定パターン（`policy.rs` モジュール冒頭コメント・CORE-2 ポインタ）に揃える。
/// `PolicyContext` は空文字列・長さ超過のテナント ID を構築時に拒否する
/// （`policy.rs::PolicyContext::new`）ため、本構造体はテナント ID の検証を
/// 重複して行わない）。
pub struct BatchQuery<'a> {
    pub vector: &'a [f32],
    pub k: usize,
    pub ctx: &'a PolicyContext,
}

/// `BatchEngine::batch_search` の 1 クエリ分の結果。
///
/// 対象ビヘイビア: TABLE-12・RLS-9。要素はテナント修飾済みの [`SearchHit`]
/// であり、`(tenant_id, id)` で行を一意に解決できる契約（`core.rs::
/// VectorCore::search` の戻り値と同一契約。PR #205 で `CandidateHit` から
/// 変更した破壊的変更）。同点タイブレークは選出段の候補識別子（常駐行列の
/// 行スロット番号）の昇順であり、行ストアから構築する場合（#178）は
/// `(tenant_id, id)` キー順で行を渡す前提のため実質 `(tenant_id, id)` 昇順、
/// 単一テナント内では従来どおり `id` 昇順になる（`docs/design/
/// rrf-tie-break-determinism.md` の順序契約と整合）。
#[derive(Debug)]
pub struct BatchHit {
    pub hits: Vec<SearchHit>,
}

/// バッチ走査パイプラインの行ソース抽象（TASK-129・CORE-8 ポインタ）。
/// [`ResidentMatrix`]（f16 パック常駐行列。GPU 経路の CPU 参照実装）と
/// `batch_fallback.rs` の CPU 縮退用 f32 常駐行列の双方から、検証・テナント
/// マスク・選出後の独立再検証という共通パイプライン（[`run_batch_search`]）を
/// 共有するための内部抽象。呼び出し元は具象型を静的に知っているため `dyn` 化
/// せずジェネリクスで単相化する（object-safe である必要はない）。この抽象を
/// 介して両経路が同一の走査・マスクロジックを通ることが、CORE-8 が要求する
/// 「縮退後の Top-k 結果が CPU-SIMD 経路の結果と一致する」契約の構成的な
/// 裏付けになる（縮退経路だけ検査が緩い、という構造的な欠落を防ぐ）。
pub(crate) trait BatchRowSource {
    fn row_count(&self) -> usize;
    fn dim(&self) -> usize;
    fn ids(&self) -> &[u64];
    fn tenant_ids(&self) -> &[String];
    fn visibilities(&self) -> &[Visibility];
    /// 行 `idx` を f32 として解決し `out` へ書き込む（積和計算専用）。
    /// [`ResidentMatrix`] は f16 往復デコード、CPU 縮退経路は f32 原本の
    /// スライス参照というように、表現方式の差分だけがここに現れる。
    fn row_f32_into(&self, idx: usize, out: &mut Vec<f32>) -> Option<()>;
}

impl BatchRowSource for ResidentMatrix {
    fn row_count(&self) -> usize {
        self.ids.len()
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn ids(&self) -> &[u64] {
        &self.ids
    }

    fn tenant_ids(&self) -> &[String] {
        &self.tenant_ids
    }

    fn visibilities(&self) -> &[Visibility] {
        &self.visibilities
    }

    fn row_f32_into(&self, idx: usize, out: &mut Vec<f32>) -> Option<()> {
        // `ResidentMatrix` の同名の inherent メソッドを呼ぶ（メソッド解決は
        // inherent メソッドをトレイトメソッドより優先するため、ここでの
        // `self.row_f32_into(...)` は無限再帰にならない）。
        ResidentMatrix::row_f32_into(self, idx, out)
    }
}

/// 候補識別子（常駐行列のスロット番号）を `(tenant_id, id, visibility)` へ解決する
/// （対象ビヘイビア: TABLE-12・RLS-9。`core.rs::resolve_slot_hits` と同じ考え方。
/// PR #205）。`run_batch_search`（選出後の独立再検証）と `batch_fallback.rs::
/// revalidate_primary_hits`（untrusted primary backend 出力の再検証用マップ構築）
/// の両方から共用し、解決ロジックの二重管理を防ぐ。
///
/// スロットが範囲外（provider・backend の契約違反）の場合は `None` を返し、
/// 呼び出し元は結果を一切返さず fail-closed に拒否する。添字アクセス（`[]`）は
/// 使わず `get()` のみで解決する（coding-rust.md「untrusted 入力の扱い」対応）。
pub(crate) fn resolve_batch_slot<S: BatchRowSource>(
    source: &S,
    slot: u64,
) -> Option<(&str, u64, Visibility)> {
    let idx = usize::try_from(slot).ok()?;
    let tenant = source.tenant_ids().get(idx)?.as_str();
    let id = crate::storage::rowid::hit_id_for_response(
        crate::storage::rowid::RowId::from_arena_raw(*source.ids().get(idx)?),
    );
    let visibility = *source.visibilities().get(idx)?;
    Some((tenant, id, visibility))
}

/// バッチクエリ・一括インデクシング専用のエンジン（CORE-6/7 ポインタ）。
/// 単発クエリ経路（`kernel.rs::SearchProvider`）は実装せず、`core.rs::EngineCore`
/// から到達できない（型レベルでの分離）。現段階はスコア計算を CPU で行う
/// 参照実装であり、GPU 実行は行わない（モジュール冒頭コメント参照）。GPU
/// バックエンドの初期化失敗・実行時エラーからの CPU-SIMD 縮退は
/// `batch_fallback.rs::FallbackBatchEngine`（TASK-129・CORE-8 ポインタ）が
/// 本エンジンを primary バックエンドとしてラップして扱う。
pub struct BatchEngine {
    matrix: ResidentMatrix,
}

impl BatchEngine {
    /// 常駐行列から構築する。GPU デバイス初期化は行わない（依存制約により
    /// 実際の GPU 実行は追加していないため、本コンストラクタは常に成功する。
    /// 将来 GPU 実行を追加する場合、ここで初期化失敗を `Result::Err` として
    /// fail-closed に伝播する設計とする）。
    pub fn new(matrix: ResidentMatrix) -> Self {
        Self { matrix }
    }

    /// バッチ検索を実行する（CORE-6・CORE-7 ポインタ）。クエリごとに次元・非有限値・`k` を
    /// 検証したうえで、常駐行列の中からクエリの [`PolicyContext::is_visible`]
    /// を満たす行だけを候補として選出する（Top-k 選出段でのクエリ別可視行
    /// マスク。codex P0 指摘対応: テナント文字列の等価比較ではなく
    /// `PolicyContext::is_visible` の単一照合パスを使う）。選出後、結果 id を
    /// 独立に再計算した (tenant, visibility) 集合と突き合わせ、逸脱があれば
    /// 結果を一切返さず [`BatchSearchError::TenantMaskViolation`] を返す
    /// （`core.rs::EngineCore` と同じ二重防御。fail-closed）。
    ///
    /// 行列走査はクエリ外側ではなく行外側でループする（codex レビュー指摘対応:
    /// 旧実装はクエリループの内側で行を毎回 f16→f32 デコードしており、
    /// クエリ数×行数×dim のデコード・ヒープ確保が発生していた）。行 1 件を
    /// 1 回だけデコードし、その行のテナントに属するクエリ集合だけへ使い回す
    /// （Cursor Medium 指摘対応: 以前は内側ループがバッチの全クエリを舐めて
    /// `PolicyContext::is_visible` でテナント一致を都度判定しており、work
    /// budget をテナント別に精緻化しても実 wall-clock コストは
    /// O(matching_rows * queries.len()) のままだった。事前にテナントごとの
    /// クエリ index 一覧（`tenant_query_indices`）を作り、行のテナントに
    /// 対応する index だけを走査することで、内側ループの反復回数が
    /// テナント別課金と一致する）。選出結果の各クエリ内順序（スコア降順・
    /// 同点 id 昇順）は `TopKSelector` が保証するため、走査順序の変更による
    /// 結果の変化はない。
    ///
    /// 走査開始前に総積和演算数を [`MAX_BATCH_WORK`] と照合する（codex P1
    /// 指摘対応: 計算量 DoS 対策。`sum(k)` の上限（[`MAX_BATCH_TOTAL_K`]）を
    /// 満たしていても、走査対象の行を最大クエリ数で走査させられてしまうため、
    /// 独立に上限を課す）。課金はテナントごとの `行数 × クエリ数 × dim` の
    /// 総和で行う（Cursor Medium 指摘対応: 常駐行列の全行数を課金すると、
    /// バッチがごく一部のテナントしか触れない場合でも過大に見積もる。
    /// codex P1 追加指摘対応: 「全一致行数 × 全クエリ数」も、複数テナントが
    /// 混在するバッチでは実際に走査しないテナント間の組み合わせまで課金
    /// してしまい過大計上になる。[`compute_batch_work`] のドキュメンテーション
    /// コメント参照）。
    pub fn batch_search(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<BatchHit>, BatchSearchError> {
        run_batch_search(&self.matrix, queries)
    }
}

/// クエリ列の入力検証（次元・非有限値・`k`・バッチ件数・`sum(k)`）。
/// [`run_batch_search`] の走査本体（テナントマスク・計算量ガード・選出）とは
/// 独立した関数として切り出す（TASK-129・CORE-8 レビュー起因の P1 指摘
/// 対応・PR #152）。`batch_fallback.rs::FallbackBatchEngine::batch_search` は
/// 公開差し替え点である `BatchBackend`（primary。実 GPU/外部実装に差し替え
/// 可能）を呼び出す前に本関数を先行適用する。走査ロジックの内部（
/// `run_batch_search`）だけで検証すると、primary 実装が入力検証を行わず
/// 不正入力に対して `BatchExecError::Backend`（実行時エラー）を返した場合、
/// 検証前に primary へ処理が渡ってしまい、不正入力 1 件で `runtime_latched`
/// が恒久固定され、以降の正当な検索まで CPU 縮退経路へ固定されてしまう
/// （可用性バグ。入力検証エラーとバックエンド実行エラーを峻別するという
/// 本モジュール群の設計方針にも反する）。
///
/// 次元不一致・非有限値・`k` 範囲外は単一クエリだけを見て安価に判定できる
/// 入力エラーであり、走査コストと無関係に最優先で報告する（`core.rs::search`
/// と同じ方針。次元不一致のクライアントに計算量エラーを返さない）。
pub(crate) fn validate_batch_queries(
    dim: usize,
    queries: &[BatchQuery<'_>],
) -> Result<(), BatchSearchError> {
    if queries.len() > MAX_BATCH_QUERIES {
        return Err(BatchSearchError::TooManyQueries {
            count: queries.len(),
            max: MAX_BATCH_QUERIES,
        });
    }

    let mut total_k: usize = 0;
    for (query_index, q) in queries.iter().enumerate() {
        if q.vector.len() != dim {
            return Err(BatchSearchError::DimMismatch {
                expected: dim,
                found: q.vector.len(),
            });
        }
        if q.vector.iter().any(|v| !v.is_finite()) {
            return Err(BatchSearchError::NonFiniteQuery { query_index });
        }
        if q.k == 0 || q.k > MAX_BATCH_K {
            return Err(BatchSearchError::InvalidK {
                k: q.k,
                max: MAX_BATCH_K,
            });
        }
        total_k = total_k
            .checked_add(q.k)
            .ok_or(BatchSearchError::TotalKExceeded {
                total_k: usize::MAX,
                max: MAX_BATCH_TOTAL_K,
            })?;
    }
    if total_k > MAX_BATCH_TOTAL_K {
        return Err(BatchSearchError::TotalKExceeded {
            total_k,
            max: MAX_BATCH_TOTAL_K,
        });
    }
    Ok(())
}

/// [`BatchEngine::batch_search`] の走査パイプライン本体（TASK-129・CORE-8
/// ポインタで [`BatchRowSource`] 越しに共有化。挙動はリファクタ前と不変）。
/// `batch_fallback.rs::FallbackBatchEngine` の CPU 縮退経路もこの関数を直接
/// 呼ぶため、GPU 参照実装（[`ResidentMatrix`]）と CPU 縮退用 f32 常駐行列は
/// 検証・テナントマスク・選出後の独立再検証を完全に同一のコードパスで通る。
pub(crate) fn run_batch_search<S: BatchRowSource>(
    source: &S,
    queries: &[BatchQuery<'_>],
) -> Result<Vec<BatchHit>, BatchSearchError> {
    // 事前検証パス（1 巡目）: [`validate_batch_queries`] を参照。単独関数を
    // 通しても本関数単体で呼ばれた場合（`BatchEngine::batch_search`・CPU
    // 縮退経路の `run_batch_search` 直接呼び出し）の検証は従来どおり保たれる
    // （`FallbackBatchEngine::batch_search` からの先行呼び出しと合わせて
    // 二重に実行されるが、入力に対する冪等な検証であり結果は変わらない）。
    validate_batch_queries(source.dim(), queries)?;

    // このバッチに登場するテナント集合（`HashSet` にして行外側ループから
    // O(1) で参照できるようにする。バッチのクエリ件数は [`MAX_BATCH_QUERIES`]
    // で上限検証済みのため、集合サイズもそれに従う）。`ctx.tenant_id()` は
    // `PolicyContext` の検証済みアクセサであり、呼び出し元が別途指定できる
    // 生の文字列ではない（codex P0 指摘対応）。`HashSet::with_capacity`
    // ではなく `try_reserve`（フォールブル）で確保する（codex P1 指摘対応）。
    // 計算量ガード（下記）より前に構築する: ガードが課金すべき行数は
    // 常駐行列の全行ではなく、このバッチのテナント集合に一致する行数
    // だからである。
    let mut batch_tenants: std::collections::HashSet<&str> = std::collections::HashSet::new();
    batch_tenants.try_reserve(queries.len()).map_err(|e| {
        BatchSearchError::AllocationFailed(format!("failed to reserve batch tenants: {e}"))
    })?;
    for q in queries {
        batch_tenants.insert(q.ctx.tenant_id());
    }

    // テナントごとのクエリ件数（codex P1 指摘対応: 計算量ガードを
    // テナント単位で精緻化するため。集合サイズはバッチのテナント集合と
    // 同じ上限に従うため `try_reserve` の予約量は `batch_tenants.len()`
    // で十分）。
    let mut tenant_query_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    tenant_query_counts
        .try_reserve(batch_tenants.len())
        .map_err(|e| {
            BatchSearchError::AllocationFailed(format!(
                "failed to reserve tenant query counts: {e}"
            ))
        })?;
    for q in queries {
        let count = tenant_query_counts.entry(q.ctx.tenant_id()).or_insert(0);
        *count = count.saturating_add(1);
    }

    // テナントごとの一致行数（走査時に実際にデコード対象となる行数。
    // 下記の行外側ループが `tenant_query_indices` に対応クエリを持たない
    // 行を除外するため、それらの行は最初からデコードされない）。この
    // 数え上げ自体は O(rows) の線形走査で、[`MAX_BATCH_ROWS`] により
    // 上限が課されているため、走査本体（O(matching_rows * dim) の
    // デコード + テナント別の行×クエリ積和）よりも十分軽量である。
    let mut tenant_row_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    tenant_row_counts
        .try_reserve(batch_tenants.len())
        .map_err(|e| {
            BatchSearchError::AllocationFailed(format!("failed to reserve tenant row counts: {e}"))
        })?;
    // ポインタ: TASK-89 / TABLE-9。バッチのテナントに属さない行のうち
    // `Visibility::Public` の件数（`extra_public_row_count`）を、
    // `policy.rs::PolicyContext::is_visible` の判定と整合する形で数える
    // （詳細は `policy.rs` 参照）。全行数は [`ResidentMatrix::build`] で
    // [`MAX_BATCH_ROWS`] 以下に制限済みのため、ここでの全件走査も既存の
    // `tenant_row_counts` 数え上げと同じ計算量クラスに収まる。
    //
    // `tenant_public_row_counts` は codex P1 指摘対応（二重課金バグ修正）:
    // バッチテナントに属する `Public` 行についても、テナント別の内訳を
    // 別途持つ。下記の追加積和項でこの内訳が必要になる。
    let mut extra_public_row_count: usize = 0;
    let mut tenant_public_row_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    tenant_public_row_counts
        .try_reserve(batch_tenants.len())
        .map_err(|e| {
            BatchSearchError::AllocationFailed(format!(
                "failed to reserve tenant public row counts: {e}"
            ))
        })?;
    for (tenant, visibility) in source.tenant_ids().iter().zip(source.visibilities().iter()) {
        let is_public = *visibility == Visibility::Public;
        if batch_tenants.contains(tenant.as_str()) {
            let count = tenant_row_counts.entry(tenant.as_str()).or_insert(0);
            *count = count.saturating_add(1);
            if is_public {
                let public_count = tenant_public_row_counts.entry(tenant.as_str()).or_insert(0);
                *public_count = public_count.saturating_add(1);
            }
        } else if is_public {
            extra_public_row_count = extra_public_row_count.saturating_add(1);
        }
    }

    // TABLE-9 用: `Public` 可視性を許可集合に含むクエリの件数（テナントを
    // 問わず）。`PolicyContext::is_visible` は「許可集合に `Public` を含み
    // かつ行が `Public`」なら常に真を返す（`row_visibility == Visibility::
    // Public` の分岐がテナント一致を短絡する）ため、`ctx.tenant_id()` 自身を
    // 行テナントとして渡しても許可集合の判定だけが漏れなく取れる（新規の
    // `PolicyContext` API を増やさず、単一照合パス `is_visible` のみを使う）。
    //
    // `tenant_public_grant_query_counts` は codex P1 指摘対応: テナント別の
    // 内訳（下記の二重課金回避に使う）。
    let mut public_grant_query_count: usize = 0;
    let mut tenant_public_grant_query_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    tenant_public_grant_query_counts
        .try_reserve(batch_tenants.len())
        .map_err(|e| {
            BatchSearchError::AllocationFailed(format!(
                "failed to reserve tenant public grant query counts: {e}"
            ))
        })?;
    for q in queries {
        if q.ctx.is_visible(q.ctx.tenant_id(), Visibility::Public) {
            public_grant_query_count = public_grant_query_count.saturating_add(1);
            let count = tenant_public_grant_query_counts
                .entry(q.ctx.tenant_id())
                .or_insert(0);
            *count = count.saturating_add(1);
        }
    }

    // 走査開始前の計算量ガード（codex P1 指摘対応・Cursor Medium 指摘対応・
    // codex P1 追加指摘対応・TASK-89 追加指摘対応）。個別クエリの入力エラー
    // （次元不一致・非有限値・k 範囲外）を上の 1 巡目で先に確定させた上で、
    // 実際に走査されるテナントごとの (行数, クエリ数) の積を合算した総積和
    // 演算数を走査（選出器確保・行デコード）開始前に確定的に拒否する。「全
    // 一致行数 × 全クエリ数」で一括課金すると、複数テナントが混在する
    // バッチでは実コストより過大に見積もり、正当な入力を誤って拒否しうる
    // （`compute_batch_work` のドキュメンテーションコメント参照）。
    let work_pairs = batch_tenants.iter().map(|&tenant| {
        let rows = tenant_row_counts.get(tenant).copied().unwrap_or(0);
        let tenant_queries = tenant_query_counts.get(tenant).copied().unwrap_or(0);
        (rows, tenant_queries)
    });
    let tenant_work = compute_batch_work(work_pairs, source.dim())?;

    // TABLE-9 用の追加積和項: `Public` 行はテナント境界を跨いで `Public`
    // 許可済みクエリからも参照されるため、上記のテナント単位の積和だけでは
    // 実走査コストを課金しきれない（下記の行外側ループ参照）。ただし、
    // バッチテナントに属する `Public` 行と「その行と同一テナントの」
    // `Public` 許可クエリとの組は、既に `tenant_work`（そのテナントの
    // 全行数 × 全クエリ数）に含まれている（行外側ループも同一テナントの
    // クエリ集合 `query_indices` 側で走査し、`public_grant_query_indices`
    // 側は他テナントのものだけに絞る。下記の候補集合構築のコメント参照）。
    // ここで単純に「データセット全体の `Public` 行数 × `Public` 許可クエリ数」
    // を加算すると、この同一テナント分が二重に計上され、実コストが
    // `MAX_BATCH_WORK` 以下の正当なバッチを誤って拒否しうる（codex P1
    // 指摘対応）。
    //
    // そのため、テナントごとの `Public` 行には「他テナントの `Public`
    // 許可クエリ数」（`public_grant_query_count` から自テナント分の
    // `tenant_public_grant_query_counts` を差し引いた値）だけを掛ける。
    // バッチテナントに属さない `Public` 行（`extra_public_row_count`）は
    // 元々どのバッチテナントとも同一テナントの積和に含まれていないため、
    // 従来どおり `public_grant_query_count` をそのまま掛けて安全側に丸める。
    let mut cross_tenant_public_work = compute_tenant_work(
        extra_public_row_count,
        public_grant_query_count,
        source.dim(),
    )?;
    for &tenant in &batch_tenants {
        let public_rows = tenant_public_row_counts.get(tenant).copied().unwrap_or(0);
        if public_rows == 0 {
            continue;
        }
        let own_grant_queries = tenant_public_grant_query_counts
            .get(tenant)
            .copied()
            .unwrap_or(0);
        // `own_grant_queries` は `public_grant_query_count` の部分集合の
        // 件数であるため `saturating_sub` で常に非負に収まる。
        let other_grant_queries = public_grant_query_count.saturating_sub(own_grant_queries);
        let term = compute_tenant_work(public_rows, other_grant_queries, source.dim())?;
        cross_tenant_public_work = cross_tenant_public_work.checked_add(term).ok_or(
            BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            },
        )?;
    }
    // `checked_add` のオーバーフローと、加算成功後の `MAX_BATCH_WORK` 超過を
    // 区別して報告する（codex P1 指摘対応）。公開エラー契約上 `work` は
    // 「実際の総積和演算数」であるべきで、加算に成功した場合はその実値を
    // 返す。`usize::MAX` は `checked_add` が実際にオーバーフローした場合
    // （＝真の総和が `usize` で表現不能）にのみ用いる。
    let total_work = tenant_work.checked_add(cross_tenant_public_work).ok_or(
        BatchSearchError::WorkBudgetExceeded {
            work: usize::MAX,
            max: MAX_BATCH_WORK,
        },
    )?;
    if total_work > MAX_BATCH_WORK {
        return Err(BatchSearchError::WorkBudgetExceeded {
            work: total_work,
            max: MAX_BATCH_WORK,
        });
    }

    // 事前検証パス（2 巡目）: `sum(k)` の上限検証を通過した後、選出器
    // コンテナ（`Vec<TopKSelector>`）をフォールブルに確保し、各選出器の
    // 内部ヒープも `q.k`（バッチ全体で `sum(k) <= MAX_BATCH_TOTAL_K` を
    // 検証済み）分だけフォールブルに予約する（codex P1 指摘対応:
    // `Vec::with_capacity` も `TopKSelector::push` 内部の `BinaryHeap::push`
    // による amortized 成長も失敗時に abort するため使わない。
    // `try_reserve_exact` は `ResidentMatrix::build` 用に定義済みの
    // 共通ヘルパーを再利用する）。
    let mut selectors: Vec<TopKSelector> = Vec::new();
    try_reserve_exact(&mut selectors, queries.len(), "selectors")?;
    for q in queries {
        let mut selector = TopKSelector::new(q.k);
        selector.try_reserve(q.k).map_err(|e| {
            BatchSearchError::AllocationFailed(format!("failed to reserve selector heap: {e}"))
        })?;
        selectors.push(selector);
    }

    // テナントごとのクエリ index 一覧（Cursor Medium 指摘対応: work budget は
    // テナント別 `行数 × クエリ数 × dim` へ精緻化済みだが、実走査が依然
    // 「デコード済み各行 × バッチの全クエリ」を舐めて `is_visible` を判定する
    // O(matching_rows * queries.len()) のネストループのままだと、課金と
    // 実 wall-clock コストが乖離する。行外側ループが「その行のテナントに
    // 属するクエリ集合」だけを走査できるよう、事前にテナント → クエリ index
    // の一覧を構築する。各 `Vec<usize>` の容量は `tenant_query_counts` で
    // 既に数えたテナントごとのクエリ件数ちょうどに `try_reserve_exact` で
    // 予約するため、後続の `push` が amortized 成長で abort することはない）。
    let mut tenant_query_indices: std::collections::HashMap<&str, Vec<usize>> =
        std::collections::HashMap::new();
    tenant_query_indices
        .try_reserve(tenant_query_counts.len())
        .map_err(|e| {
            BatchSearchError::AllocationFailed(format!(
                "failed to reserve tenant query indices: {e}"
            ))
        })?;
    for (&tenant, &count) in tenant_query_counts.iter() {
        let mut indices: Vec<usize> = Vec::new();
        try_reserve_exact(&mut indices, count, "tenant query indices")?;
        tenant_query_indices.insert(tenant, indices);
    }
    for (idx, q) in queries.iter().enumerate() {
        if let Some(indices) = tenant_query_indices.get_mut(q.ctx.tenant_id()) {
            indices.push(idx);
        }
    }

    // ポインタ: TASK-89 / TABLE-9。`Public` 可視性を許可集合に含むクエリの
    // index 一覧（テナントを問わず。上の `public_grant_query_count` と同じ
    // 判定を使って再構築する）。行外側ループで `Public` 行に遭遇した際、
    // 「同一テナントのクエリ集合」（`tenant_query_indices`）に加えてこの
    // 一覧も候補にすることで、`policy.rs::PolicyContext::is_visible` の
    // 判定と本経路（`batch_search`/`batch_fallback`）の事前絞り込みを
    // 整合させる（`EngineCore::search`/`PrefilterIndex::search` との可視
    // 集合の非対称を解消する対応。詳細は `policy.rs` 参照）。
    let mut public_grant_query_indices: Vec<usize> = Vec::new();
    try_reserve_exact(
        &mut public_grant_query_indices,
        public_grant_query_count,
        "public grant query indices",
    )?;
    for (idx, q) in queries.iter().enumerate() {
        if q.ctx.is_visible(q.ctx.tenant_id(), Visibility::Public) {
            public_grant_query_indices.push(idx);
        }
    }

    // 行外側ループ: 行 1 件につき 1 回だけデコードし、その行に到達しうる
    // クエリ集合だけを走査する（Cursor Medium 指摘対応: 以前はクエリ側を
    // バッチ全体で舐める O(matching_rows * queries.len()) のネストループに
    // なっており、work budget をテナント別に精緻化しても実 wall-clock
    // コストは一致しなかった。`tenant_query_indices` で行のテナントに属する
    // クエリ index だけへ絞ることで、内側ループの反復回数がテナント別課金と
    // 一致する）。TASK-89（TABLE-9）対応で、`Public` 行は同一テナントの
    // クエリ集合に加え `public_grant_query_indices`（他テナントの `Public`
    // 許可クエリ）にも到達しうるため、デコード要否の判定はどちらかの候補
    // 集合が非空であることを見る（`row_is_reachable`）。`row_buf` は
    // `Vec::with_capacity`（abort-on-OOM）ではなく `try_reserve_exact` で
    // フォールブルに確保する（codex P1 指摘対応）。
    // TASK-157（対象ビヘイビア: CORE-15）: 行デコード用スクラッチバッファは
    // プロセス共有のサイズクラス別プール（`buffer_pool.rs`）から取得する。
    // `run_batch_search` は `BatchEngine::batch_search`（GPU 参照実装）・
    // `FallbackBatchEngine`（CPU 縮退経路）の双方から呼ばれる共通パイプラインで
    // あるため、繰り返し呼び出される実運用でアロケータ呼び出しが呼び出し回数に
    // 比例して増え続けるのを避ける（プールが空・容量不足の場合はフォールブルな
    // 新規確保にフォールバックするため、プール自体は最適化であり正当性には
    // 影響しない）。次元は本モジュールが既に検証済みの [`MAX_BATCH_DIM`] を
    // 上限として渡す（本モジュール独自の追加上限は新設しない）。
    let mut row_buf = row_buffer_pool()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .acquire(source.dim(), MAX_BATCH_DIM)
        .map_err(|e| BatchSearchError::AllocationFailed(e.to_string()))?;
    let empty_query_indices: Vec<usize> = Vec::new();
    for row_idx in 0..source.row_count() {
        let Some(row_tenant) = source.tenant_ids().get(row_idx).map(String::as_str) else {
            continue;
        };
        let Some(row_visibility) = source.visibilities().get(row_idx).copied() else {
            continue;
        };
        // 同一テナントのクエリ集合（`tenant_query_indices` のキー集合は
        // `batch_tenants` と一致する。属さないテナントの行は空スライスを
        // 使う）。
        let query_indices = tenant_query_indices
            .get(row_tenant)
            .unwrap_or(&empty_query_indices);
        let row_is_reachable = !query_indices.is_empty()
            || (row_visibility == Visibility::Public && !public_grant_query_indices.is_empty());
        if !row_is_reachable {
            continue;
        }
        // 候補識別子は行 `id` ではなく常駐行列のスロット番号（対象ビヘイビア:
        // TABLE-12・RLS-9。`core.rs::slot_ids_for`/`resolve_slot_hits` と同じ
        // 方式）。行 `id` の一意性スコープはテナント内に閉じているため、id を
        // 直接候補識別子にすると異なるテナントの同一 `id` 行を選出後に区別
        // できない。`u64::try_from` の checked 変換で拒否できない行は
        // 候補にしない（`MAX_BATCH_ROWS` 上限により通常到達しない防御的分岐）。
        let Ok(slot) = u64::try_from(row_idx) else {
            continue;
        };
        if source.row_f32_into(row_idx, &mut row_buf.buf).is_none() {
            continue;
        }

        // TASK-89（対象ビヘイビア: TABLE-9）: `Public` 行は同一テナントの
        // クエリ集合（`query_indices`）に加え、他テナントの `Public` 許可
        // クエリ（`public_grant_query_indices`）からも候補にする。同一
        // テナントのクエリは `query_indices` 側で既に走査するため、ここでは
        // 別テナントのものだけへ絞って二重に selector へ push しないようにする
        // （`policy.rs::PolicyContext::is_visible` の単一照合パスはそのまま
        // 維持し、事前絞り込みの候補集合だけを拡張する）。
        let candidate_indices = query_indices.iter().copied().chain(
            (row_visibility == Visibility::Public)
                .then_some(public_grant_query_indices.as_slice())
                .into_iter()
                .flatten()
                .copied()
                .filter(|&qi| {
                    queries
                        .get(qi)
                        .is_some_and(|q| q.ctx.tenant_id() != row_tenant)
                }),
        );

        for qi in candidate_indices {
            // untrusted 入力由来の添字ではなく、直前に `enumerate()` で
            // 自前生成した内部インデックスだが、coding-rust.md の方針に
            // 揃えて `[]` ではなく `get`/`get_mut` で明示的に処理する。
            let (Some(q), Some(selector)) = (queries.get(qi), selectors.get_mut(qi)) else {
                continue;
            };
            // (1) 選出前のマスク: `PolicyContext::is_visible` を満たす
            // 行だけを候補にする（codex P0 指摘対応: テナント文字列の
            // 等価比較だけでなく可視性ラベルも判定する。同一テナント側の
            // 候補はテナント一致が `tenant_query_indices` の絞り込みで
            // 保証済みだが、`Public` 許可の他テナント候補はテナントが
            // 異なるため、ここでの `is_visible` 呼び出しが可視性・テナント
            // 双方の最終判定を担う）。
            if !q.ctx.is_visible(row_tenant, row_visibility) {
                continue;
            }
            let score = crate::kernel::dot(&row_buf.buf, q.vector);
            if !score.is_finite() {
                continue;
            }
            selector.push(CandidateHit { id: slot, score });
        }
    }

    // 行外側ループが終わり `row_buf` を使い終えたら、プールへ返却する
    // （TASK-157・CORE-15 ポインタ）。以降の処理（選出結果の取り出し・独立
    // 再検証）は `row_buf` を参照しないため、ここで返却して次回呼び出しでの
    // 再利用可能性を最大化する。
    row_buffer_pool()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .release(row_buf);

    // `out` も `Vec::with_capacity`（abort-on-OOM）ではなく
    // `try_reserve_exact` でフォールブルに確保する（codex P1 指摘対応）。
    let mut out: Vec<BatchHit> = Vec::new();
    try_reserve_exact(&mut out, queries.len(), "batch results")?;
    for (q, selector) in queries.iter().zip(selectors) {
        let candidates = selector.into_sorted_vec();

        // (2) 選出後の独立再検証: 各ヒットのスロットをマスク実装（行 index
        // からの判定）とは独立に `(tenant_id, id, visibility)` へ解決し
        // （`resolve_batch_slot`）、`PolicyContext::is_visible` を満たすことを
        // 確認する（対象ビヘイビア: TABLE-12・RLS-9。`core.rs::resolve_slot_hits`
        // と同じ考え方）。解決不能・不可視のいずれも結果を一切返さず拒否する
        // （fail-closed）。`String` 確保はここ（高々 `k` 件）でのみ発生し、
        // 行外側ループのホットパスには入らない。
        //
        // 旧実装は `id_to_tenant` の登録対象を「バッチのテナントに属する行」
        // または「`Public` 許可クエリがある場合の `Public` 行」に絞っていたが
        // （バッチ外テナントの id が hit に混入した場合を確実にマップ不在に
        // する追加のフィルタ）、本実装は `resolve_batch_slot` で常駐行列の
        // 任意の行を解決したうえで `is_visible` のみを判定する。この単純化が
        // fail-closed 性を弱めないのは、`is_visible(t, v)` が真になるのは
        // (a) `t == q.ctx.tenant_id()`（＝バッチのテナント集合に属する）か、
        // (b) `v == Visibility::Public` かつ `q.ctx` が `Public` を許可
        // （＝「`Public` 許可クエリがある場合の `Public` 行」）のいずれか
        // だけであり、旧フィルタの受理条件を `is_visible` の判定が構造的に
        // 包含するため（旧フィルタが弾いていた行は `is_visible` も必ず偽になる）。
        let mut hits: Vec<SearchHit> = Vec::new();
        try_reserve_exact(&mut hits, candidates.len(), "batch resolved hits")?;
        for hit in &candidates {
            let Some((tenant, id, visibility)) = resolve_batch_slot(source, hit.id) else {
                return Err(BatchSearchError::TenantMaskViolation);
            };
            if !q.ctx.is_visible(tenant, visibility) {
                return Err(BatchSearchError::TenantMaskViolation);
            }
            // `SearchHit::new`（`impl Into<String>` 経由の `String::from`）では
            // なくフォールブルな `try_owned_str` で `tenant_id` を複製する
            // （`sum(k)` は最大 [`MAX_BATCH_TOTAL_K`]（100 万）まで達しうるため、
            // `ResidentMatrix::build` の `tenant_ids` 複製と同じ理由で
            // abort-on-OOM を避ける）。
            hits.push(SearchHit {
                tenant_id: try_owned_str(tenant)?,
                id,
                score: hit.score,
            });
        }

        out.push(BatchHit { hits });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // CORE-16 ポインタ: f16 往復精度。
    #[test]
    fn f16_roundtrip_preserves_reasonable_precision() {
        for v in [0.0f32, 1.0, -1.0, 0.5, 3.140625, -100.0, 1e-3] {
            let packed = pack_f16x2(v, 0.0);
            let (a, _b) = unpack_f16x2(packed);
            assert!(
                (a - v).abs() < 0.01,
                "roundtrip diverged too much: v={v} got={a}"
            );
        }
    }

    #[test]
    fn f16_roundtrip_handles_special_values() {
        let packed = pack_f16x2(0.0, -0.0);
        let (a, b) = unpack_f16x2(packed);
        assert_eq!(a, 0.0);
        assert_eq!(b, 0.0);

        let packed_inf = pack_f16x2(f32::INFINITY, f32::NEG_INFINITY);
        let (inf_a, inf_b) = unpack_f16x2(packed_inf);
        assert!(inf_a.is_infinite() && inf_a > 0.0);
        assert!(inf_b.is_infinite() && inf_b < 0.0);
    }

    // CORE-16 ポインタ・codex レビュー指摘対応: サブノーマル f16 の復元値を厳密一致で検証する
    // （`f16_roundtrip_preserves_reasonable_precision` の絶対誤差 `< 0.01` 判定では
    // サブノーマル域の値が 2^13 倍ずれても誤差が閾値未満に収まり検出できない）。
    // ここではビット列を直接与えて `unpack_f16x2` の公開契約どおり
    // `unpack2x16float` と同じ厳密値に一致することを確認する。
    #[test]
    fn f16_bits_to_f32_matches_exact_subnormal_values() {
        // bits=0x0001（mantissa=1） は最小の正のサブノーマル: 1 * 2^-24。
        let (lo, hi) = unpack_f16x2(0x0001);
        assert_eq!(lo, 1.0f32 / 16_777_216.0);
        assert_eq!(hi, 0.0);

        // hi レーン側（上位 16 ビット）も同じ経路を通ることを確認する。
        let (lo2, hi2) = unpack_f16x2(0x0001_0000);
        assert_eq!(lo2, 0.0);
        assert_eq!(hi2, 1.0f32 / 16_777_216.0);

        // bits=0x0200（mantissa=0x200=512） は 512 * 2^-24 = 2^-15。
        let (v, _) = unpack_f16x2(0x0200);
        assert_eq!(v, 512.0f32 / 16_777_216.0);

        // bits=0x03FF（mantissa=0x3FF=1023、最大のサブノーマル） は 1023 * 2^-24。
        let (v, _) = unpack_f16x2(0x03FF);
        assert_eq!(v, 1023.0f32 / 16_777_216.0);

        // 符号ビットもサブノーマル経路と独立に正しく伝播することを確認する
        // （bits=0x8001 は 0x0001 の符号反転）。
        let (v, _) = unpack_f16x2(0x8001);
        assert_eq!(v, -(1.0f32 / 16_777_216.0));
    }

    // codex レビュー指摘対応: `pack_f16x2`（`f32_to_f16_bits`）がサブノーマル域
    // （`exp <= 0`）を一律 0 へフラッシュしていたバグの回帰テスト。サブノーマル
    // f16 として厳密表現可能な値（k * 2^-24）は pack→unpack で厳密に一致する
    // ことを確認する（`unpack_f16x2` は既にサブノーマルへ対応済みのため、この
    // 往復テストで初めて `f32_to_f16_bits` 側のサブノーマル生成経路も検証できる）。
    #[test]
    fn f16_pack_roundtrips_exact_subnormal_values() {
        // 最小の正のサブノーマル: 1 * 2^-24。
        let packed = pack_f16x2(1.0f32 / 16_777_216.0, 0.0);
        let (lo, _) = unpack_f16x2(packed);
        assert_eq!(lo, 1.0f32 / 16_777_216.0);

        // 負値・hi レーンも同じ経路を通ることを確認する（512 * 2^-24 = 2^-15）。
        let packed = pack_f16x2(0.0, -(512.0f32 / 16_777_216.0));
        let (_, hi) = unpack_f16x2(packed);
        assert_eq!(hi, -(512.0f32 / 16_777_216.0));

        // 最大のサブノーマル: 1023 * 2^-24。
        let packed = pack_f16x2(1023.0f32 / 16_777_216.0, 0.0);
        let (v, _) = unpack_f16x2(packed);
        assert_eq!(v, 1023.0f32 / 16_777_216.0);
    }

    // 境界値: サブノーマル域での round-to-nearest-even 丸めを検証する
    // （flush-to-zero を仕様として残さない、という codex レビュー指摘対応の
    // 判断が実際に効いていることを確認する）。
    #[test]
    fn f16_pack_rounds_subnormal_boundary_values_to_nearest_even() {
        // 0 と最小サブノーマル (2^-24) のちょうど中間 (2^-25) は round-to-even で
        // 「偶数」側の 0 へ丸まる（f16 の 0 はサブノーマル仮数ビット列 0 であり
        // 偶数）。
        let tie_to_zero = 1.0f32 / 33_554_432.0; // 2^-25
        let (v, _) = unpack_f16x2(pack_f16x2(tie_to_zero, 0.0));
        assert_eq!(v, 0.0, "exact tie must round to even (zero)");

        // 中間点よりわずかに大きい値は切り上げられ、最小サブノーマルになる
        // （2^-25 の直後の f32 表現可能値を使う）。
        let just_above_tie = f32::from_bits(tie_to_zero.to_bits() + 1);
        let (v, _) = unpack_f16x2(pack_f16x2(just_above_tie, 0.0));
        assert_eq!(
            v,
            1.0f32 / 16_777_216.0,
            "value above the tie must round up"
        );

        // サブノーマル最大値 (1023 * 2^-24) と最小正規化数 (1024 * 2^-24 = 2^-14)
        // のちょうど中間 (1023.5 * 2^-24) は、丸め前の仮数下位ビットが奇数
        // (1023) のため round-to-even で偶数側 (1024 = 最小正規化数) へ切り上がる。
        let tie_to_normal = 1023.5f32 / 16_777_216.0;
        let (v, _) = unpack_f16x2(pack_f16x2(tie_to_normal, 0.0));
        assert_eq!(
            v,
            1.0f32 / 16_384.0,
            "tie at the subnormal/normal boundary must carry into the smallest normal"
        );
    }

    // 真のゼロ（+0.0/-0.0）はサブノーマル変換の暗黙ビット付与を経由せず、
    // そのまま符号付きゼロへ変換されることを確認する。
    #[test]
    fn f16_pack_preserves_signed_zero() {
        let (a, b) = unpack_f16x2(pack_f16x2(0.0, -0.0));
        assert_eq!(a, 0.0);
        assert!(a.is_sign_positive());
        assert_eq!(b, 0.0);
        assert!(b.is_sign_negative());
    }

    #[test]
    fn f16_pack_pair_roundtrips_both_elements() {
        let packed = pack_f16x2(2.5, -4.25);
        let (a, b) = unpack_f16x2(packed);
        assert!((a - 2.5).abs() < 1e-3);
        assert!((b - (-4.25)).abs() < 1e-3);
    }

    // CORE-7 ポインタ: 動的窓集約の判定（後続あり/なし）。
    #[test]
    fn should_aggregate_only_when_pending_follows() {
        assert!(should_aggregate_into_batch(true));
        assert!(!should_aggregate_into_batch(false));
    }

    #[test]
    fn aggregator_drains_pushed_queries_once() {
        let mut agg = DynamicWindowAggregator::new();
        assert!(agg.is_empty());
        agg.push(vec![1.0, 0.0]).expect("push ok");
        agg.push(vec![0.0, 1.0]).expect("push ok");
        assert_eq!(agg.len(), 2);
        let drained = agg.drain();
        assert_eq!(drained.len(), 2);
        assert!(agg.is_empty());
    }

    // codex P1 指摘対応: `push` は件数上限（`MAX_BATCH_QUERIES`）を超えると
    // 内部 `Vec` を無制限に成長させず fail-closed に拒否する。
    #[test]
    fn aggregator_push_rejects_over_max_queries() {
        let mut agg = DynamicWindowAggregator::new();
        for _ in 0..MAX_BATCH_QUERIES {
            agg.push(vec![1.0, 0.0]).expect("push ok");
        }
        let err = agg.push(vec![1.0, 0.0]).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::TooManyQueries {
                count: MAX_BATCH_QUERIES + 1,
                max: MAX_BATCH_QUERIES,
            }
        );
    }

    // codex P1 指摘対応: 次元は 1 件目の `push` で確定し、以降の `push` は
    // 同一次元しか受け付けない（次元混在バッチを拒否する）。
    #[test]
    fn aggregator_push_rejects_dimension_mismatch_after_first_push() {
        let mut agg = DynamicWindowAggregator::new();
        agg.push(vec![1.0, 0.0]).expect("push ok");
        let err = agg.push(vec![1.0, 0.0, 0.0]).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::WindowDimMismatch {
                expected: 2,
                found: 3
            }
        );
    }

    // codex P1 指摘対応: 次元 0 のクエリは受け付けない（`MAX_BATCH_DIM` と
    // 同じ検証を `ResidentMatrix::build` と揃える）。
    #[test]
    fn aggregator_push_rejects_zero_dim() {
        let mut agg = DynamicWindowAggregator::new();
        let err = agg.push(vec![]).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::InvalidDim {
                dim: 0,
                max: MAX_BATCH_DIM
            }
        );
    }

    // codex P1 指摘対応: `drain` は次元の確定状態もリセットするため、次の窓は
    // 別次元のバッチを受け付けられる。
    #[test]
    fn aggregator_drain_resets_dimension_for_next_window() {
        let mut agg = DynamicWindowAggregator::new();
        agg.push(vec![1.0, 0.0]).expect("push ok");
        agg.drain();
        agg.push(vec![1.0, 0.0, 0.0])
            .expect("次元 3 の別窓は許可される");
        assert_eq!(agg.len(), 1);
    }

    // Cursor Bugbot 指摘対応: 上限検証済みの量を超えて `try_reserve_exact` が
    // 実際にメモリ不足になった場合、`Vec::with_capacity`/`push` のように abort
    // せず `Err(BatchSearchError::AllocationFailed)` を返すことを検証する
    // （`arena.rs` の同種テストと同方針）。`isize::MAX` 超のレイアウトは Rust の
    // アロケーション API 契約上、実メモリを確保しようとする前に即座に拒否
    // されるため、CI 環境で実際に大量のメモリを消費せず決定的に再現できる。
    #[test]
    fn try_reserve_exact_converts_oversized_request_to_allocation_failed_without_aborting() {
        let mut buf: Vec<u8> = Vec::new();
        let oversized = (isize::MAX as usize).saturating_add(1);
        let result = try_reserve_exact(&mut buf, oversized, "test buffer");
        assert!(matches!(result, Err(BatchSearchError::AllocationFailed(_))));
    }

    // ResidentMatrix の上限・整合性検証（untrusted 入力の防御的上限）。
    #[test]
    fn resident_matrix_rejects_zero_dim() {
        let err = ResidentMatrix::build(&[1], &["t".to_string()], &[Visibility::Public], 0, &[])
            .unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::InvalidDim {
                dim: 0,
                max: MAX_BATCH_DIM
            }
        );
    }

    #[test]
    fn resident_matrix_rejects_combined_capacity_even_when_rows_and_dim_are_individually_within_limits(
    ) {
        // MAX_BATCH_ROWS・MAX_BATCH_DIM をそれぞれ単独では超えないが、組み合わせた
        // 総確保バイト量が MAX_BATCH_TOTAL_BYTES を超えるケース（codex レビュー指摘:
        // 独立検証だけでは packed 単体で約 16.4GB の単一確保が発生し得る）。
        // 容量チェックは `vectors.len()` の整合性検証より前に実行されるため、
        // 巨大な `vectors` バッファを実際に確保しなくても本エラーへ到達できる
        // （`ids`/`tenant_ids` は実データが必要だが、MAX_BATCH_ROWS 行でも
        // 数十 MB 程度で現実的に確保可能）。
        let ids: Vec<u64> = (0..MAX_BATCH_ROWS as u64).collect();
        let tenants: Vec<String> = std::iter::repeat_n("t".to_string(), MAX_BATCH_ROWS).collect();
        let visibilities: Vec<Visibility> =
            std::iter::repeat_n(Visibility::Public, MAX_BATCH_ROWS).collect();
        let err =
            ResidentMatrix::build(&ids, &tenants, &visibilities, MAX_BATCH_DIM, &[]).unwrap_err();
        match err {
            BatchSearchError::CapacityExceeded { total_bytes, max } => {
                assert_eq!(max, MAX_BATCH_TOTAL_BYTES);
                assert!(total_bytes > max);
            }
            other => panic!("expected CapacityExceeded, got {other:?}"),
        }
    }

    #[test]
    fn resident_matrix_rejects_length_mismatch() {
        let err = ResidentMatrix::build(
            &[1, 2],
            &["t".to_string()],
            &[Visibility::Public],
            2,
            &[1.0, 0.0],
        )
        .unwrap_err();
        assert_eq!(err, BatchSearchError::ArenaLengthMismatch);
    }

    // 対象ビヘイビア: TABLE-12・RLS-9。同一テナント内の id 重複は fail-closed で
    // 拒否する（`(tenant_id, id)` 複合キーが行を一意に定めなくなり、選出後再検証
    // が同一テナント内の別行を取り違えうるため）。
    #[test]
    fn resident_matrix_rejects_duplicate_ids() {
        let ids = [1u64, 1];
        let tenants = ["tenant-a".to_string(), "tenant-a".to_string()];
        let visibilities = [Visibility::Public, Visibility::Public];
        let vectors = [1.0f32, 0.0, 0.0, 1.0];
        let err = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors).unwrap_err();
        assert_eq!(err, BatchSearchError::DuplicateRowId);
    }

    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205）。行 `id` の一意性スコープは
    // テナント内に閉じているため、異なるテナントが同じ `id` を持つ入力は正当で
    // あり `DuplicateRowId` にならない。
    #[test]
    fn resident_matrix_accepts_same_id_across_tenants() {
        let ids = [1u64, 1];
        let tenants = ["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = [Visibility::Public, Visibility::Public];
        let vectors = [1.0f32, 0.0, 0.0, 1.0];
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors).unwrap();
        assert_eq!(matrix.row_count(), 2);
    }

    // Cursor Bugbot 指摘対応: 容量チェックは `MAX_TENANT_ID_LEN` で予算計上する
    // だけで、実際の各 `tenant_ids` 要素長を検証していなかった。上限超過の
    // tenant_id を含む入力を fail-closed で拒否することを確認する。
    #[test]
    fn resident_matrix_rejects_oversized_tenant_id() {
        let ids = [1u64];
        let oversized = "t".repeat(crate::storage::MAX_TENANT_ID_LEN as usize + 1);
        let tenants = [oversized.clone()];
        let visibilities = [Visibility::Public];
        let vectors = [1.0f32, 0.0];
        let err = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::TenantIdTooLong {
                len: oversized.len(),
                max: crate::storage::MAX_TENANT_ID_LEN as usize,
            }
        );
    }

    // codex P1 指摘対応: `tenant_ids` の各要素を `String::clone`（abort-on-OOM）
    // ではなく `try_reserve_exact` + `push_str` でフォールブルに構築するよう
    // 変更したため、マルチバイト UTF-8（バイト長と文字数が異なる）を含む
    // tenant_id でも内容が過不足なく複製されることを回帰確認する。内部
    // フィールドは非公開のため、`batch_search` のテナント一致マスクを
    // 経由した機能的な等価性チェックで検証する（1 バイトでも欠落・破損
    // すれば文字列比較が食い違い、該当行が誤ってマスクされるかクエリ側
    // マスクに一致しなくなるため十分な検出力を持つ）。
    #[test]
    fn resident_matrix_preserves_multibyte_tenant_id_via_fallible_clone() {
        let multibyte_tenant = "tenant-日本語-🎉".to_string();
        let ids = [1u64];
        let tenants = [multibyte_tenant.clone()];
        let visibilities = [Visibility::Public];
        let vectors = [1.0f32, 0.0];
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx = PolicyContext::new(&multibyte_tenant).expect("valid tenant");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            ctx: &ctx,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(
            results[0].hits.first().map(|h| h.id),
            Some(1),
            "tenant_id must round-trip byte-for-byte through the fallible clone"
        );
    }

    fn build_two_tenant_matrix() -> ResidentMatrix {
        // tenant-a: id=1,2 / tenant-b: id=3,4。dim=2。全行 Public。
        let ids = vec![1u64, 2, 3, 4];
        let tenants = vec![
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ];
        let visibilities = vec![Visibility::Public; 4];
        let vectors = vec![1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0];
        ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors).expect("valid matrix")
    }

    /// [`build_two_tenant_matrix`] と同じ id・テナント・ベクトル配置だが、全行
    /// `Private` にした版。テナント分離そのものを検証するテストは本フィクスチャを
    /// 使う（ポインタ: TASK-89 / TABLE-9）。
    fn build_two_tenant_matrix_private() -> ResidentMatrix {
        let ids = vec![1u64, 2, 3, 4];
        let tenants = vec![
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ];
        let visibilities = vec![Visibility::Private; 4];
        let vectors = vec![1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0];
        ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors).expect("valid matrix")
    }

    /// [`PolicyContext::new`]（`Public` のみ許可）のテスト用ショートハンド。
    fn ctx(tenant_id: &str) -> PolicyContext {
        PolicyContext::new(tenant_id).expect("valid tenant")
    }

    /// `Private` を明示許可した [`PolicyContext`] のテスト用ショートハンド
    /// （[`build_two_tenant_matrix_private`] と組で使う）。
    fn private_ctx(tenant_id: &str) -> PolicyContext {
        PolicyContext::with_visibilities(tenant_id, [Visibility::Private]).expect("valid tenant")
    }

    // CORE-7 ポインタ・テナント境界（P0）: 混在テナントバッチで混入 0 件。
    // 検査は実装（`batch_search` 内のマスク）から独立に、返却 id → tenant を
    // 再計算して確認する（実装と検査器の経路分離）。codex P0 指摘対応:
    // `BatchQuery` はもはや呼び出し元が指定する生の `tenant_id: &str` を
    // 持たない（型定義から削除済み）。テナント境界は `ctx`（`PolicyContext`）
    // 経由でのみ engine 側が決定するため、本テストは「`tenant-a` の
    // `PolicyContext` を渡すクエリが、行列に同居する `tenant-b` の行へは
    // 構造的に到達できない」ことを検証する。`Private` フィクスチャで検証する
    // （ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn batch_search_excludes_other_tenant_rows() {
        let matrix = build_two_tenant_matrix_private();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = private_ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(results.len(), 1);
        // 独立検査器: 返った id が期待テナントの id 集合に含まれるかを、
        // engine 内部のマスク実装を経由せず直接確認する。
        let tenant_a_ids: std::collections::HashSet<u64> = [1, 2].into_iter().collect();
        for hit in &results[0].hits {
            assert!(
                tenant_a_ids.contains(&hit.id),
                "tenant-a query must not return id={} (other tenant)",
                hit.id
            );
        }
        assert!(!results[0].hits.is_empty());
    }

    // 対象ビヘイビア: TABLE-9（正方向・レビュー起因の回帰）。`batch_search` が
    // `EngineCore::search`（`core.rs`）と同一の可視集合を返すことを検証する
    // （`policy.rs::PolicyContext::is_visible` の単一照合パスが経路によらず
    // 同じ判定を返すという設計の要）。混入 0 件だけでは判定の正方向を保証
    // しないため、tenant-b の行が実際に返ることまで確認する（ポインタ:
    // TASK-89 / TABLE-9）。
    #[test]
    fn batch_search_includes_other_tenant_public_rows_matching_engine_core() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        // 既定の `ctx()` は `Public` のみ許可（`Private` は明示付与が必要）。
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(results.len(), 1);

        // 正方向の確認: tenant-b の id（3, 4）が返却集合に含まれる。
        let returned_ids: std::collections::HashSet<u64> =
            results[0].hits.iter().map(|h| h.id).collect();
        assert!(
            returned_ids.contains(&3) || returned_ids.contains(&4),
            "TABLE-9 mutual visibility must be exercised via batch_search, not just absent"
        );

        // `EngineCore`/`PrefilterIndex` 経路との対称性: 同一データ・同一 ctx
        // に対する `core::VectorCore::search` の可視 id 集合と一致すること。
        // `build_two_tenant_matrix` は全行 `Public` のため、全 4 行が
        // `ctx_a`（`Public` 許可）から可視である。
        let all_ids: std::collections::HashSet<u64> = [1, 2, 3, 4].into_iter().collect();
        assert_eq!(returned_ids, all_ids);
    }

    // codex P0 指摘対応の回帰テスト: `PolicyContext` は他テナントの `tenant_id`
    // を偽装する経路を提供しない。`tenant-a` の `PolicyContext` で発行した
    // クエリが `tenant-b` の行を一切返さないことを、バッチ内に両テナントの
    // クエリが混在する状況でも確認する（`tenant-b` 側のクエリも同時に検証し、
    // 相互のテナント越え漏えいが無いことを両方向で確認する）。`Private`
    // フィクスチャで検証する（ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn batch_search_cannot_cross_tenant_boundary_via_policy_context() {
        let matrix = build_two_tenant_matrix_private();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = private_ctx("tenant-a");
        let ctx_b = private_ctx("tenant-b");
        let queries = vec![
            BatchQuery {
                vector: &query_vec,
                k: 10,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_vec,
                k: 10,
                ctx: &ctx_b,
            },
        ];
        let results = engine.batch_search(&queries).expect("batch search ok");
        let tenant_a_ids: std::collections::HashSet<u64> = [1, 2].into_iter().collect();
        let tenant_b_ids: std::collections::HashSet<u64> = [3, 4].into_iter().collect();
        for hit in &results[0].hits {
            assert!(
                tenant_a_ids.contains(&hit.id) && !tenant_b_ids.contains(&hit.id),
                "tenant-a ctx leaked a tenant-b row: id={}",
                hit.id
            );
        }
        for hit in &results[1].hits {
            assert!(
                tenant_b_ids.contains(&hit.id) && !tenant_a_ids.contains(&hit.id),
                "tenant-b ctx leaked a tenant-a row: id={}",
                hit.id
            );
        }
    }

    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205）。行 `id` の一意性スコープは
    // テナント内に閉じているため、異なるテナントが同じ `id`（ここでは `1`）を
    // 持つ `Private` フィクスチャで、`tenant-a` のクエリが自テナントの行
    // `(tenant-a, 1)` だけを返し、`tenant-b` の同 `id` 行が混入しないことを
    // 確認する（選出後の独立再検証が `(tenant_id, id)` 基準であることの
    // 直接的な回帰）。
    #[test]
    fn batch_search_returns_only_own_row_when_other_tenant_shares_id() {
        let ids = [1u64, 1];
        let tenants = ["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = [Visibility::Private, Visibility::Private];
        let vectors = [1.0f32, 0.0, 1.0, 0.0];
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = private_ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(results[0].hits.len(), 1);
        assert_eq!(results[0].hits[0].tenant_id, "tenant-a");
        assert_eq!(results[0].hits[0].id, 1);
    }

    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205）。`Public` 行で `tenant-a`・
    // `tenant-b` が同じ `id`（`1`）を共有する場合、`PolicyContext::new`
    // （`Public` のみ許可）のクエリからは両テナントの行が別ヒットとして返り、
    // 各ヒットの `tenant_id` が正しく行の所属を示すことを確認する。
    #[test]
    fn batch_search_returns_both_tenants_public_rows_sharing_id() {
        let ids = [1u64, 1];
        let tenants = ["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = [Visibility::Public, Visibility::Public];
        let vectors = [1.0f32, 0.0, 1.0, 0.0];
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        let returned: std::collections::HashSet<(&str, u64)> = results[0]
            .hits
            .iter()
            .map(|h| (h.tenant_id.as_str(), h.id))
            .collect();
        assert_eq!(results[0].hits.len(), 2, "both shared-id rows must return");
        assert!(returned.contains(&("tenant-a", 1)));
        assert!(returned.contains(&("tenant-b", 1)));
    }

    // codex P0 指摘対応: `Visibility::Private` の行は、`PolicyContext::new`
    // （既定・最小権限で `Public` のみ許可）では不可視のままであることを
    // 確認する（`policy.rs::private_requires_explicit_grant` と同じ CORE-2 ポインタの
    // 既定方針を `batch_search` 経路でも維持する）。
    #[test]
    fn batch_search_excludes_private_rows_without_explicit_grant() {
        let ids = [1u64, 2];
        let tenants = ["tenant-a".to_string(), "tenant-a".to_string()];
        let visibilities = [Visibility::Public, Visibility::Private];
        let vectors = [1.0f32, 0.0, 1.0, 0.0];
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, 2, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a,
        }];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(
            results[0].hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![1],
            "Private row (id=2) must stay invisible without explicit grant"
        );

        // 明示付与すれば可視になることも確認する（黙示の昇格ではないことの対照）。
        let ctx_a_with_private =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let queries_with_grant = vec![BatchQuery {
            vector: &query_vec,
            k: 10,
            ctx: &ctx_a_with_private,
        }];
        let results_with_grant = engine
            .batch_search(&queries_with_grant)
            .expect("batch search ok");
        let ids_with_grant: std::collections::HashSet<u64> =
            results_with_grant[0].hits.iter().map(|h| h.id).collect();
        assert!(ids_with_grant.contains(&2));
    }

    // codex レビュー指摘対応（行外側ループへの構造変更）: 複数クエリが異なる
    // テナントを持つバッチで、選出器とクエリの対応がずれていないことを検証する
    // （行外側ループ・選出器の事前生成・共有 `row_buf` の組み合わせで、選出器の
    // 取り違えが起きうる構造のため）。テナント間の行が結果に混ざらないことも
    // 併せて確認するため `Private` フィクスチャで検証する（ポインタ:
    // TASK-89 / TABLE-9）。
    #[test]
    fn batch_search_keeps_per_query_results_correct_across_different_tenants() {
        let matrix = build_two_tenant_matrix_private();
        let engine = BatchEngine::new(matrix);
        let query_a = [1.0f32, 0.0];
        let query_b = [0.0f32, 1.0];
        let ctx_a = private_ctx("tenant-a");
        let ctx_b = private_ctx("tenant-b");
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 10,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_b,
                k: 10,
                ctx: &ctx_b,
            },
        ];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].hits.first().map(|h| h.id), Some(1));
        assert_eq!(results[1].hits.first().map(|h| h.id), Some(4));
        let tenant_a_ids: std::collections::HashSet<u64> = [1, 2].into_iter().collect();
        let tenant_b_ids: std::collections::HashSet<u64> = [3, 4].into_iter().collect();
        for hit in &results[0].hits {
            assert!(tenant_a_ids.contains(&hit.id));
        }
        for hit in &results[1].hits {
            assert!(tenant_b_ids.contains(&hit.id));
        }
    }

    // codex レビュー指摘対応: 同一テナント・異なるベクトルの複数クエリが、
    // 共有される行デコード（`row_buf`）を経由してもクエリごとに正しいスコアで
    // 選出されることを検証する（`k` もクエリごとに異なる値を与え、事前生成した
    // 選出器がクエリと 1:1 で対応することも確認する）。
    #[test]
    fn batch_search_scores_each_query_independently_against_shared_decoded_row() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_a = [1.0f32, 0.0];
        let query_b = [0.0f32, 1.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 1,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_b,
                k: 10,
                ctx: &ctx_a,
            },
        ];
        let results = engine.batch_search(&queries).expect("batch search ok");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].hits.len(), 1, "k=1 must be honored");
        assert_eq!(results[0].hits.first().map(|h| h.id), Some(1));
        assert_eq!(results[1].hits.first().map(|h| h.id), Some(2));
    }

    // negative test: マスク段を意図的に無効化した内部構成（テスト内部限定。公開
    // API・cargo feature としては露出しない。Issue #137 の feature 廃止判断を踏襲）
    // で、上記の独立検査器が違反を検出できることを確認する。
    #[test]
    fn independent_checker_detects_masked_tenant_violation() {
        let matrix = build_two_tenant_matrix();
        // マスクを経由せず「全行を無条件に候補にする」経路を直接模した結果集合
        // （実装コードのマスク段を使わず、テストがここで意図的に違反を作る）。
        // 候補識別子は本体コードと同じくスロット番号（対象ビヘイビア:
        // TABLE-12・RLS-9。PR #205）。
        let simulated_unmasked_hits = [
            CandidateHit { id: 0, score: 1.0 }, // slot 0 = (tenant-a, 1): 正当
            CandidateHit { id: 2, score: 1.0 }, // slot 2 = (tenant-b, 3): 混入（検出されるべき）
        ];
        // 期待集合は `(tenant_id, id)` キー（`resolve_batch_slot` と同じ解決結果）。
        let tenant_a_keys: std::collections::HashSet<(&str, u64)> =
            [("tenant-a", matrix.ids[0]), ("tenant-a", matrix.ids[1])]
                .into_iter()
                .collect();
        let violation =
            simulated_unmasked_hits
                .iter()
                .any(|hit| match resolve_batch_slot(&matrix, hit.id) {
                    Some((tenant, id, _visibility)) => !tenant_a_keys.contains(&(tenant, id)),
                    None => true,
                });
        assert!(
            violation,
            "independent checker failed to detect a tenant mask violation"
        );
    }

    #[test]
    fn batch_search_rejects_dim_mismatch() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::DimMismatch {
                expected: 2,
                found: 3
            }
        );
    }

    #[test]
    fn batch_search_rejects_non_finite_query() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [f32::NAN, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::NonFiniteQuery { query_index: 0 });
    }

    // codex レビュー指摘対応: 行外側ループへの構造変更後もクエリ間で選出器が
    // 独立に保たれることを踏まえ、`k` の防御的上限を検証する（`kernel.rs::
    // TopKSelector` 自体は `k` を検証しないため、本モジュールが未検証の巨大な
    // `k` をそのまま複数選出器へ使わないことを確認する）。
    #[test]
    fn batch_search_rejects_invalid_k() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: 0,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::InvalidK {
                k: 0,
                max: MAX_BATCH_K
            }
        );

        let queries = vec![BatchQuery {
            vector: &query_vec,
            k: MAX_BATCH_K + 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::InvalidK {
                k: MAX_BATCH_K + 1,
                max: MAX_BATCH_K
            }
        );
    }

    // codex P1 指摘対応: `sum(k)` がちょうど [`MAX_BATCH_TOTAL_K`] に等しい
    // 境界（超過ではない）は許可されることを確認する。この確認がないと
    // `batch_search_rejects_total_k_over_limit`（超過側のみ検証）だけでは
    // 比較演算子の off-by-one（`>` を `>=` に取り違える等）を検出できない。
    #[test]
    fn batch_search_accepts_total_k_exactly_at_limit() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        // 100 件 * k=10_000 = 1,000,000 == MAX_BATCH_TOTAL_K ちょうど。
        let query_count = MAX_BATCH_TOTAL_K / MAX_BATCH_K;
        assert_eq!(query_count * MAX_BATCH_K, MAX_BATCH_TOTAL_K);
        let queries: Vec<BatchQuery<'_>> = (0..query_count)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: MAX_BATCH_K,
                ctx: &ctx_a,
            })
            .collect();
        let results = engine
            .batch_search(&queries)
            .expect("sum(k) at the limit must be accepted");
        assert_eq!(results.len(), query_count);
    }

    // codex P1 指摘対応: 各クエリの `k` は個別に [`MAX_BATCH_K`] 以内でも、
    // バッチ全体の `sum(k)` が [`MAX_BATCH_TOTAL_K`] を超える場合は
    // fail-closed に拒否する（個別上限のみでは積が無制限になり得たため）。
    #[test]
    fn batch_search_rejects_total_k_over_limit() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        // 101 件 * k=10_000 = 1,010,000 > MAX_BATCH_TOTAL_K(1,000,000)。
        // 各クエリの k は MAX_BATCH_K(10_000) ちょうどで個別上限は満たす。
        let query_count = MAX_BATCH_TOTAL_K / MAX_BATCH_K + 1;
        let queries: Vec<BatchQuery<'_>> = (0..query_count)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: MAX_BATCH_K,
                ctx: &ctx_a,
            })
            .collect();
        let expected_total_k = query_count * MAX_BATCH_K;
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::TotalKExceeded {
                total_k: expected_total_k,
                max: MAX_BATCH_TOTAL_K,
            }
        );
    }

    #[test]
    fn batch_search_rejects_too_many_queries() {
        let matrix = build_two_tenant_matrix();
        let engine = BatchEngine::new(matrix);
        let query_vec = [1.0f32, 0.0];
        let ctx_a = ctx("tenant-a");
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES + 1)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::TooManyQueries {
                count: MAX_BATCH_QUERIES + 1,
                max: MAX_BATCH_QUERIES,
            }
        );
    }

    // codex P1 指摘対応: `sum(k)` の上限を満たしていても、行数×クエリ数×次元の
    // 積（計算量）が [`MAX_BATCH_WORK`] を超える場合は走査開始前に fail-closed
    // で拒否する（rows=1,000・dim=8,192 は `MAX_BATCH_TOTAL_BYTES` の 1 GiB
    // 予算内で現実的に構築できる小さな行列だが、`MAX_BATCH_QUERIES` 件の
    // クエリで走査すると積和は約 3.35 × 10^10 回に達し、`MAX_BATCH_WORK`
    // （10^10）を超える）。`compute_batch_work` を直接呼び、巨大な行列・
    // クエリ列を実際に確保せず境界を検証する（`arena.rs::check_capacity` の
    // 直接テストと同じ考え方）。
    #[test]
    fn compute_batch_work_rejects_over_limit() {
        let err = compute_batch_work([(1_000, MAX_BATCH_QUERIES)], MAX_BATCH_DIM).unwrap_err();
        match err {
            BatchSearchError::WorkBudgetExceeded { work, max } => {
                assert_eq!(max, MAX_BATCH_WORK);
                assert!(work > max);
            }
            other => panic!("expected WorkBudgetExceeded, got {other:?}"),
        }
    }

    // codex P1 指摘対応: `rows × queries × dim` がちょうど [`MAX_BATCH_WORK`]
    // に等しい境界（超過ではない）は許可されることを確認する（off-by-one の
    // 検出用。過大側のみのテストでは `>` と `>=` の取り違えを検出できない）。
    #[test]
    fn compute_batch_work_accepts_exactly_at_limit() {
        // 1,000,000 * 100 * 100 = MAX_BATCH_WORK(10^10) ちょうど（単一テナント）。
        let rows = 1_000_000usize;
        let queries = 100usize;
        let dim = 100usize;
        assert_eq!(rows * queries * dim, MAX_BATCH_WORK);
        let work = compute_batch_work([(rows, queries)], dim).expect("work at the limit must pass");
        assert_eq!(work, MAX_BATCH_WORK);
    }

    // codex P1 指摘対応: `compute_batch_work` の積が `usize` をオーバーフロー
    // する巨大な入力でも panic せず `WorkBudgetExceeded` を返すことを確認する
    // （coding-rust.md「整数演算は checked_*/saturating_* を使う」準拠）。
    #[test]
    fn compute_batch_work_does_not_overflow_on_huge_inputs() {
        let err = compute_batch_work([(usize::MAX, usize::MAX)], usize::MAX).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            }
        );
    }

    // codex P1 指摘対応: テナントごとの積の合算（総和）自体が `usize` を
    // オーバーフローする場合も panic せず `WorkBudgetExceeded` を返すことを
    // 確認する（`compute_tenant_work` 単体の overflow 検出だけでは
    // `compute_batch_work` 内の合算 `checked_add` の回帰を検出できないため）。
    // 1 つ目のテナントは上限内に収まる小さな work（`total` が早期リターン
    // されない）を持たせ、2 つ目のテナントは単体では overflow しない
    // （`rows * queries * dim` が `usize::MAX` に収まる）が、既存の `total`
    // への加算では `usize::MAX` を超えるように選ぶ。
    #[test]
    fn compute_batch_work_does_not_overflow_when_per_tenant_sum_overflows() {
        let per_tenant = [(1usize, 1usize), (usize::MAX, 1usize)];
        let err = compute_batch_work(per_tenant, 1).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::WorkBudgetExceeded {
                work: usize::MAX,
                max: MAX_BATCH_WORK,
            }
        );
    }

    // codex P1 追加指摘対応: レビューで指摘された具体例（tenant-a/b が各
    // 1,000 行、バッチ全体で合計 1,000 クエリ（tenant-a/b に 500 件ずつ）、
    // dim=8,192）を直接検証する。「全一致行数（2,000）× 全クエリ数
    // （1,000）」で一括課金すると 2,000 * 1,000 * 8,192 ≈ 1.6384 × 10^10 で
    // `MAX_BATCH_WORK`（10^10）を超過し、正当な入力を誤って拒否してしまう
    // （＝今回のバグ）。テナントごとの正しい課金
    // `1,000 * 500 * 8,192 + 1,000 * 500 * 8,192` = 8.192 × 10^9 は
    // `MAX_BATCH_WORK` の範囲内であり、受理されるべきことを確認する。
    #[test]
    fn compute_batch_work_matches_codex_review_example() {
        let dim = 8_192usize;
        let per_tenant = [(1_000usize, 500usize), (1_000usize, 500usize)];

        // 誤った「全一致行数 × 全クエリ数」の一括課金は超過することを前提として
        // 明示する（この誤った計算式が起こす過大計上こそが今回のバグ）。
        let naive_total_rows = 2_000usize;
        let naive_total_queries = 1_000usize;
        assert!(naive_total_rows * naive_total_queries * dim > MAX_BATCH_WORK);

        // テナントごとの正しい課金は上限内で受理される。
        let work = compute_batch_work(per_tenant, dim)
            .expect("per-tenant work budget must accept the codex review example");
        assert_eq!(work, 1_000 * 500 * dim * 2);
        assert!(work <= MAX_BATCH_WORK);
    }

    // codex P1 指摘対応: `batch_search` 経由でも計算量ガードが実際に効くことを
    // 確認する（小さな行列 + 最大クエリ数 + 最大次元の組み合わせで、実データを
    // 現実的なサイズに保ったまま `MAX_BATCH_WORK` 超過を再現する）。
    #[test]
    fn batch_search_rejects_work_budget_over_limit() {
        // rows=1,000・dim=8,192（MAX_BATCH_DIM）で packed バイト数は
        // 約 16MB 程度に収まり、現実的に構築できる。
        let rows = 1_000usize;
        let dim = MAX_BATCH_DIM;
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenants: Vec<String> = std::iter::repeat_n("tenant-a".to_string(), rows).collect();
        let visibilities: Vec<Visibility> = std::iter::repeat_n(Visibility::Public, rows).collect();
        let vectors: Vec<f32> = std::iter::repeat_n(0.0f32, rows * dim).collect();
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, dim, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);

        let query_vec: Vec<f32> = std::iter::repeat_n(0.0f32, dim).collect();
        let ctx_a = ctx("tenant-a");
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();
        let err = engine.batch_search(&queries).unwrap_err();
        match err {
            BatchSearchError::WorkBudgetExceeded { work, max } => {
                assert_eq!(max, MAX_BATCH_WORK);
                assert!(work > max);
            }
            other => panic!("expected WorkBudgetExceeded, got {other:?}"),
        }
    }

    // Cursor Medium 指摘対応: 計算量ガードの課金対象は常駐行列の全行数ではなく、
    // バッチのテナント集合に実際に一致する行数（＝走査時に実際にデコードされる
    // 行数）でなければならない。tenant-a を 10 行・tenant-b を 990 行持つ
    // 常駐行列（全 1,000 行）に対し、tenant-a だけを最大クエリ数で検索する
    // バッチは、全行数（1,000）を課金すれば
    // `batch_search_rejects_work_budget_over_limit` と同じ構成で
    // `WorkBudgetExceeded` になってしまうが、実際に走査するのは tenant-a の
    // 10 行だけ（10 * MAX_BATCH_QUERIES * MAX_BATCH_DIM ≈ 3.36 × 10^8 <
    // MAX_BATCH_WORK）なので許可されるべきことを確認する（行数は実行時間を
    // 抑えるため境界からは離れた小さい値を選ぶ。境界そのものの検証は
    // `compute_batch_work_accepts_exactly_at_limit` 等が別途担う）。tenant-b 側は
    // `Private` にして「`ctx_a` が実際に見るのは tenant-a の行だけ」という前提を
    // 保つ（ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn batch_search_work_budget_charges_only_matching_tenant_rows() {
        let tenant_a_rows = 10usize;
        let tenant_b_rows = 990usize;
        let rows = tenant_a_rows + tenant_b_rows;
        let dim = MAX_BATCH_DIM;
        let ids: Vec<u64> = (0..rows as u64).collect();
        let mut tenants: Vec<String> = std::iter::repeat_n("tenant-a".to_string(), tenant_a_rows)
            .chain(std::iter::repeat_n("tenant-b".to_string(), tenant_b_rows))
            .collect();
        tenants.truncate(rows);
        let visibilities: Vec<Visibility> = std::iter::repeat_n(Visibility::Public, tenant_a_rows)
            .chain(std::iter::repeat_n(Visibility::Private, tenant_b_rows))
            .collect();
        let vectors: Vec<f32> = std::iter::repeat_n(0.0f32, rows * dim).collect();
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, dim, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);

        // 全行数（1,000）で課金すると work = 1,000 * MAX_BATCH_QUERIES *
        // MAX_BATCH_DIM は MAX_BATCH_WORK を超過する（上の
        // `batch_search_rejects_work_budget_over_limit` と同一規模）ことを
        // 前提として明示する。
        assert!(compute_batch_work([(rows, MAX_BATCH_QUERIES)], dim).is_err());
        // tenant-a の行数だけで課金すれば上限内であることも前提として明示する
        // （本テストのバッチは tenant-a のみを検索するため、テナントごとの
        // 課金でも単一テナント分＝この値と一致する）。
        assert!(compute_batch_work([(tenant_a_rows, MAX_BATCH_QUERIES)], dim).is_ok());

        let query_vec: Vec<f32> = std::iter::repeat_n(0.0f32, dim).collect();
        let ctx_a = ctx("tenant-a");
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();

        let result = engine.batch_search(&queries);
        assert!(
            result.is_ok(),
            "expected success when only the small tenant-a slice is searched, got {result:?}"
        );
    }

    /// TASK-89（TABLE-9）レビュー起因の追加テスト用フィクスチャ: tenant-a を
    /// 1 行、バッチに登場しない tenant-c を `extra_rows` 行、共に `Public` で
    /// 持つ常駐行列。`tenant-c` はどのクエリのテナントとも一致しないため、
    /// TABLE-9 対応前は行外側ループのゲートで最初から除外され課金対象にも
    /// ならなかった。以下の 2 テストは、この「バッチに登場しないテナントの
    /// `Public` 行」が計算量ガードへ正しく計上されること（過大な
    /// `extra_rows` は `WorkBudgetExceeded`）と、`Public` を許可しないクエリ
    /// では逆にこの拡張分のコストを一切払わないこと（`public_grant_query_
    /// count_nonzero` ゲートが効くこと）の両方を確認する。
    fn build_tenant_a_and_foreign_public_matrix(extra_rows: usize) -> ResidentMatrix {
        let rows = 1 + extra_rows;
        let dim = MAX_BATCH_DIM;
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenants: Vec<String> = std::iter::once("tenant-a".to_string())
            .chain(std::iter::repeat_n("tenant-c".to_string(), extra_rows))
            .collect();
        let visibilities: Vec<Visibility> = std::iter::repeat_n(Visibility::Public, rows).collect();
        let vectors: Vec<f32> = std::iter::repeat_n(0.0f32, rows * dim).collect();
        ResidentMatrix::build(&ids, &tenants, &visibilities, dim, &vectors).expect("valid matrix")
    }

    // TASK-89（TABLE-9）レビュー起因の回帰: バッチに 1 件も登場しないテナント
    // （tenant-c）の `Public` 行であっても、`Public` を許可するクエリからは
    // TABLE-9 により到達しうるため、その分の積和コストが計算量ガードに
    // 計上され `WorkBudgetExceeded` になることを確認する（過小計上のまま
    // 走査本体まで進むと、計算量 DoS ガードに穴が空く）。
    #[test]
    fn batch_search_charges_foreign_tenant_public_rows_reachable_via_table9() {
        // tenant-a 単独では compute_tenant_work(1, MAX_BATCH_QUERIES,
        // MAX_BATCH_DIM) = 33,554,432 で全く問題にならない規模だが、
        // tenant-c の `Public` 行 400 件（`extra_public_row_count`）×
        // `public_grant_query_count`（MAX_BATCH_QUERIES）× dim を加えると
        // MAX_BATCH_WORK（1e10）を超過する（400 * 4096 * 8192 =
        // 13,421,772,800）。tenant-a 自身の `Public` 行は同一テナントの
        // クエリとの組が `tenant_work` に既に含まれるため、追加積和項からは
        // 除外される（codex P1 指摘対応: 二重課金回避）。
        let matrix = build_tenant_a_and_foreign_public_matrix(400);
        let engine = BatchEngine::new(matrix);

        let query_vec: Vec<f32> = std::iter::repeat_n(0.0f32, MAX_BATCH_DIM).collect();
        // 既定の `ctx()` は `Public` のみ許可（`public_grant_query_count` に
        // 数えられる）。
        let ctx_a = ctx("tenant-a");
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();

        let err = engine.batch_search(&queries).unwrap_err();
        match err {
            BatchSearchError::WorkBudgetExceeded { work, max } => {
                assert_eq!(max, MAX_BATCH_WORK);
                // codex P1 指摘対応: `checked_add` 自体はオーバーフローしていない
                // （通常の予算超過）ため、`work` には実際の総積和演算数
                // （33,554,432 + 13,421,772,800）を返す。`usize::MAX` は
                // `checked_add` が真にオーバーフローした場合専用のセンチネル。
                assert_eq!(
                    work, 13_455_327_232,
                    "actual total work expected, not the overflow sentinel"
                );
            }
            other => panic!("expected WorkBudgetExceeded, got {other:?}"),
        }
    }

    // TASK-89（TABLE-9）レビュー起因の回帰（上のテストと対で確認）:
    // `Public` を許可しないクエリ（`Private` のみ許可）だけのバッチでは、
    // `public_grant_query_count` が 0 になり、バッチに登場しない tenant-c の
    // `Public` 行はそもそも到達不能（TABLE-9 は `Public` 許可クエリにしか
    // 効かない）。`cross_tenant_public_work` は `public_grant_query_count` を
    // 乗じる積で計算されるため、この場合は同じ常駐行列（tenant-c 側の行数は
    // 上のテストと同じ 400 件）でもコストを一切払わず成功し、tenant-c の id も
    // 一切返らないことを確認する。
    #[test]
    fn batch_search_does_not_charge_foreign_public_rows_when_no_query_grants_public() {
        let matrix = build_tenant_a_and_foreign_public_matrix(400);
        let engine = BatchEngine::new(matrix);

        let query_vec: Vec<f32> = std::iter::repeat_n(0.0f32, MAX_BATCH_DIM).collect();
        let ctx_a = private_ctx("tenant-a");
        let queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();

        let results = engine
            .batch_search(&queries)
            .expect("must succeed: no query in this batch grants Public");
        // tenant-a 自身の唯一の行（id=0）も `Public` だが、ctx は `Private`
        // のみ許可のためこの行自体も不可視。結果は全クエリで空になる。
        for hit_set in &results {
            assert!(
                hit_set.hits.is_empty(),
                "no row must be visible for this ctx (see PolicyContext::is_visible)"
            );
        }
    }

    // codex レビュー追加指摘対応: 次元不一致という軽量に判定できる入力エラーは
    // `compute_batch_work` の計算量ガードより先に報告されるべき（core.rs::search
    // と同じ「安価で具体的なエラーを優先する」方針）。work budget 超過を
    // 引き起こす規模のクエリ列に次元不一致クエリを混ぜても、返るのは
    // `DimMismatch` であって `WorkBudgetExceeded` ではないことを確認する。
    #[test]
    fn batch_search_reports_dim_mismatch_before_work_budget_guard() {
        let rows = 1_000usize;
        let dim = MAX_BATCH_DIM;
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenants: Vec<String> = std::iter::repeat_n("tenant-a".to_string(), rows).collect();
        let visibilities: Vec<Visibility> = std::iter::repeat_n(Visibility::Public, rows).collect();
        let vectors: Vec<f32> = std::iter::repeat_n(0.0f32, rows * dim).collect();
        let matrix = ResidentMatrix::build(&ids, &tenants, &visibilities, dim, &vectors)
            .expect("valid matrix");
        let engine = BatchEngine::new(matrix);

        // rows * MAX_BATCH_QUERIES * dim は MAX_BATCH_WORK を超過する規模
        // （`batch_search_rejects_work_budget_over_limit` と同一構成）。
        let query_vec: Vec<f32> = std::iter::repeat_n(0.0f32, dim).collect();
        let wrong_dim_query: Vec<f32> = std::iter::repeat_n(0.0f32, dim - 1).collect();
        let ctx_a = ctx("tenant-a");
        let mut queries: Vec<BatchQuery<'_>> = (0..MAX_BATCH_QUERIES)
            .map(|_| BatchQuery {
                vector: &query_vec,
                k: 1,
                ctx: &ctx_a,
            })
            .collect();
        queries[0] = BatchQuery {
            vector: &wrong_dim_query,
            k: 1,
            ctx: &ctx_a,
        };

        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(
            err,
            BatchSearchError::DimMismatch {
                expected: dim,
                found: dim - 1,
            }
        );
    }

    // BatchEngine が SearchProvider を実装しないこと（単発経路へ構造的に
    // 接続できないことのコンパイル時の裏付け）は型シグネチャそのものが保証する
    // ため実行時テストは不要（`kernel.rs::SearchProvider` を implement していない）。

    // Issue #170（INDEX-4・TASK-122 実装後のバッチ経路バッファプール上限再整合）:
    // `buffer_pool.rs`（TASK-157・CORE-15）導入時に申し送りとして残った、プール上限と
    // 一括投入経路（`batch_limits.rs`・TASK-122）の上限との再整合を、以下の 3 テストで
    // 固定する。いずれも環境変数で注入可能な `batch_limits.rs` の実行時値ではなく、
    // fail-closed 既定のフォールバック定数（本リポの公開定数）に対して検証する
    // （実行環境やテスト実行順に依存しない整合検証とするため）。

    // 不変条件 (a): バッチ経路で到達しうる最大サイズクラス 1 個分が本プールの総量上限
    // 以下であり、プールが「格納せず即破棄」に縮退しないこと（`buffer_pool.rs` モジュール
    // コメントの (a) に対応）。静的な大小関係に加え、実際に最大クラスのバッファが
    // `release` 後にアイドル保持されることも動的に確認する。
    #[test]
    fn pool_max_size_class_stays_retainable_within_quota() {
        let max_class = match crate::buffer_pool::size_class_for(MAX_BATCH_DIM) {
            Some(class) => class,
            None => panic!("MAX_BATCH_DIM must map to a valid size class"),
        };
        assert!(
            crate::buffer_pool::class_bytes(max_class) <= ROW_BUFFER_POOL_QUOTA_BYTES,
            "max size class ({} bytes) must fit within the pool quota ({} bytes), \
             otherwise the pool degrades to allocate-and-discard for every batch call",
            crate::buffer_pool::class_bytes(max_class),
            ROW_BUFFER_POOL_QUOTA_BYTES
        );

        let mut pool = crate::buffer_pool::BufferPool::new(ROW_BUFFER_POOL_QUOTA_BYTES);
        let pooled = pool
            .acquire(MAX_BATCH_DIM, MAX_BATCH_DIM)
            .expect("MAX_BATCH_DIM must be acceptable at max_elems == MAX_BATCH_DIM");
        pool.release(pooled);
        assert!(
            pool.retained_bytes() > 0,
            "the max-class buffer must be retained idle after release, not discarded"
        );
    }

    // 不変条件 (b): プールのピーク理論上限（総量上限＋最大クラス 1 個分。CORE-15 の
    // ピーク有界性テストと同じ計算式）が一括投入経路の合計予算の fail-closed 既定値
    // （`incremental::MAX_INDEX_TOTAL_BYTES`）以下であること。
    //
    // 注意（Issue #170 codex-review 指摘対応）: このテストが固定するのはプール単体の
    // ピーク上限が一括投入経路の予算と同程度の桁に収まる、という **独立上限としての
    // 桁感**のみである。プールは一括投入経路を直接使わないが（モジュール冒頭コメント
    // 参照）、両経路は同一プロセス内で同時に確保され得るため、プロセス全体の実ピーク
    // 使用量は概ね `incremental_usage + pool_peak` であり、本テストの
    // `pool_peak <= MAX_INDEX_TOTAL_BYTES` という比較だけでは「両経路が同一プロセスの
    // 共有メモリ予算内に収まる」ことは何も保証しない。両経路の合計を共通上限と比較する
    // 実行時の共有予算制御や、一方の確保中に他方を縮小・解放する仕組みは本モジュールに
    // 存在せず、本テストもそれを代替しない（各経路が自身の上限内に収まることのみを
    // 独立に固定する）。
    #[test]
    fn pool_quota_stays_within_batch_limits_budget() {
        let max_class = match crate::buffer_pool::size_class_for(MAX_BATCH_DIM) {
            Some(class) => class,
            None => panic!("MAX_BATCH_DIM must map to a valid size class"),
        };
        let peak_theoretical_upper_bound =
            ROW_BUFFER_POOL_QUOTA_BYTES.saturating_add(crate::buffer_pool::class_bytes(max_class));
        assert!(
            peak_theoretical_upper_bound <= crate::incremental::MAX_INDEX_TOTAL_BYTES,
            "pool peak theoretical upper bound ({peak_theoretical_upper_bound} bytes) must not \
             exceed the bulk-ingest fallback budget (incremental::MAX_INDEX_TOTAL_BYTES = {} \
             bytes); this is an independent-quota sanity bound only — it does NOT establish a \
             combined per-process budget across the pool and bulk-ingest paths (the process-wide \
             peak when both are live concurrently is roughly incremental_usage + pool_peak, which \
             this assertion does not bound)",
            crate::incremental::MAX_INDEX_TOTAL_BYTES
        );
    }

    // 不変条件 (c): 一括投入経路が正当に生成しうる次元（`storage::MAX_EMBEDDING_DIM` まで）
    // のうち `MAX_BATCH_DIM` 超のものは、本プールの `acquire` がアロケーション前に
    // 拒否すること。`MAX_BATCH_DIM < MAX_EMBEDDING_DIM`（一括投入経路はバッチ経路より
    // 高次元を受理しうる）という前提を明文化したうえで、境界（`MAX_BATCH_DIM` ちょうどは
    // 受理・`MAX_BATCH_DIM + 1` は拒否）を固定する。
    #[test]
    fn pool_rejects_dims_beyond_max_batch_dim_before_allocation() {
        assert!(
            (MAX_BATCH_DIM as u64) < (crate::storage::MAX_EMBEDDING_DIM as u64),
            "bulk-ingest path (storage::MAX_EMBEDDING_DIM) must be able to accept dims beyond \
             what the batch path (MAX_BATCH_DIM) accepts, otherwise invariant (c) is vacuous"
        );

        let mut pool = crate::buffer_pool::BufferPool::new(ROW_BUFFER_POOL_QUOTA_BYTES);

        // 境界: MAX_BATCH_DIM ちょうどは受理される。
        let accepted = pool
            .acquire(MAX_BATCH_DIM, MAX_BATCH_DIM)
            .expect("MAX_BATCH_DIM must be accepted at the boundary");
        pool.release(accepted);
        let retained_before_rejection = pool.retained_bytes();

        // 境界: MAX_BATCH_DIM + 1 はアロケーション前に拒否される。
        let err = pool
            .acquire(MAX_BATCH_DIM + 1, MAX_BATCH_DIM)
            .expect_err("MAX_BATCH_DIM + 1 must be rejected before allocation");
        assert_eq!(
            err,
            crate::buffer_pool::BufferPoolError::RequestTooLarge {
                len: MAX_BATCH_DIM + 1,
                max: MAX_BATCH_DIM,
            }
        );
        assert_eq!(
            pool.retained_bytes(),
            retained_before_rejection,
            "a rejected request must not allocate or retain any additional buffer"
        );
    }
}
