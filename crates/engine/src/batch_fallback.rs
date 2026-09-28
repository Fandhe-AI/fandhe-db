//! GPU バックエンドの初期化失敗・実行時エラーに対する CPU-SIMD 縮退機構
//! （TASK-129・対象ビヘイビア: CORE-8）。
//!
//! `batch_search.rs::BatchEngine`（f16 パック常駐行列を走査する GPU 経路の CPU
//! 参照実装。TASK-128 ポインタ）を primary バックエンドとして [`BatchBackend`] に
//! 適合させ（[`GpuReferenceBackend`]）、初期化失敗・実行時エラー（デバイス
//! ロスト・カーネル起動失敗・転送失敗）の双方で [`FallbackBatchEngine`] が
//! f32 原本を保持する CPU 縮退経路（`kernel.rs::dot` + `kernel.rs::TopKSelector`。
//! CORE-3 と同一カーネル）へ panic なしに切り替える。走査・マスク・二重防御
//! ロジックは `batch_search.rs::run_batch_search`（行ソース抽象
//! `batch_search.rs::BatchRowSource`）を GPU 参照実装と共有するため、「縮退経路
//! だけ検査が緩い」という構造的な欠落を作らない。共有の結果、縮退後の Top-k は
//! CPU-SIMD 経路（`kernel.rs::CpuScalarProvider` と同じ選出規約）と構成的に
//! 一致する。
//!
//! 本モジュールが担うのは「選択後の実行時 fail-safe」（primary 失敗→CPU 縮退）であり、
//! 「実行前の経路選択」自体は `dispatch.rs::select_execution_path`（TASK-155・
//! CORE-11, 12）が担う。両者は排他ではなく直列（決定表→実行→縮退）の責務分担に
//! なる設計であり、[`FallbackBatchEngine::batch_search`] は冒頭で
//! `select_execution_path` を呼んで primary（GPU）を試みるか CPU 縮退経路へ直行するか
//! を決めてから実行する（配線済み）。
//!
//! # 実配線: `pending_after_pop`（動的窓判定）をバッチ経路の判断へ持ち込まない
//!
//! `select_execution_path` の `pending_after_pop` 入力（動的窓判定用）を供給する
//! キュー層は存在しない（`batch_search.rs::should_aggregate_into_batch`／
//! `DynamicWindowAggregator` は現状 `dispatch.rs` とテストからのみ呼ばれる）。当初
//! `pending_after_pop: false` 固定で単発クエリ（`batch_size == 1`）を毎回 CPU-SIMD
//! 直行にする配線を試作したが、`revalidate_primary_hits`・恒久故障ラッチ等の primary
//! 実行経路を単発クエリのバッチで検証しているテスト群が primary 未呼び出しのため
//! red になることを実測で確認した（本モジュールの呼び出しは既に集約済みのバッチ
//! であり、`batch_size == 1` であっても「単発クエリの動的窓判定」を適用すべき対象
//! ではない、という誤りが原因）。
//!
//! この誤りは `dispatch.rs::select_execution_path` 側で解消した:
//! `DispatchInput::for_batch` 経由（本モジュールが使うコンストラクタ）の入力は
//! 件数・`pending_after_pop` によらず常にバッチ扱いになる（CORE-6, 7, 8。
//! `for_single_query` 経由の入力だけが動的窓判定の対象になる）。これにより本
//! メソッドは `pending_after_pop: false` を渡すだけでよく（実際、`for_batch` は
//! この値を判断に使わない）、primary の呼び出し可否は「`self.primary` が
//! `Available` かどうか」だけで決まるという従来の挙動を、決定表を経由しながら
//! そのまま維持する。
//!
//! [`BatchBackend`] は将来の実 GPU/外部実装が差し込まれる公開差し替え点であり、
//! `Ok` を返した場合でも [`FallbackBatchEngine`] はその内容を無条件に信頼しない
//! （codex-review P0 指摘対応・PR #152）。primary が成功を返しても、
//! [`FallbackBatchEngine`] が構築時から保持する信頼済み CPU 常駐行列を基準に
//! `FallbackBatchEngine::revalidate_primary_hits` が独立に再検証し（結果件数・
//! 各クエリの `k`・id の存在・可視性・重複なし・スコア有限性・順序）、違反が
//! あれば結果を一切返さず `Err` で fail-closed に拒否する。
//!
//! # データ所有権・二重常駐について
//!
//! CORE-16 ポインタの方針（CPU 経路は f32 のまま維持）を満たすため、CPU 縮退
//! 用の行列は primary（GPU 参照実装の f16 パック常駐行列）とは独立に f32 原本を
//! 保持する（[`FallbackBatchEngine::build`] が両方を構築する）。これは常駐
//! コーパスの二重確保を意味するが、f16 パック行列（`ResidentMatrix::build` が
//! `batch_search.rs::MAX_BATCH_TOTAL_BYTES` で検証）と f32 縮退行列
//! （[`FallbackBatchEngine::build`] が同じ `MAX_BATCH_TOTAL_BYTES` 予算で
//! 独立に検証）のどちらも無制限確保にはならない設計とする。

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::batch_search::{
    run_batch_search, BatchEngine, BatchHit, BatchQuery, BatchRowSource, BatchSearchError,
    ResidentMatrix, MAX_BATCH_TOTAL_BYTES,
};
use crate::dispatch::{self, DispatchInput, ExecutionPath, GpuCapability};
use crate::storage::Visibility;

/// primary バックエンド実行時のエラー種別（CORE-8 ポインタ）。GPU デバイス
/// 初期化失敗と実行時エラー（デバイスロスト・カーネル起動失敗・転送失敗）を
/// 区別し、[`FallbackEvent`] のログ可視化で要因を判別できるようにする。
/// メッセージはプログラム出力文字列（英語）で、テナント ID・クエリ内容を
/// 含めない（security.md「機微情報の漏えい」対応）。
#[derive(Debug, Clone, PartialEq)]
pub enum BatchBackendError {
    /// バックエンドの初期化（デバイス確保等）が失敗した。
    InitFailed(String),
    /// 初期化済みのデバイスが実行中に失われた。
    DeviceLost(String),
    /// カーネル起動に失敗した。
    KernelLaunchFailed(String),
    /// ホスト⇔デバイス間のデータ転送に失敗した。
    TransferFailed(String),
}

impl BatchBackendError {
    /// [`FallbackEvent`] のログ可視化用に、要因を「初期化失敗」「実行時エラー」の
    /// 2 種別へ分類する。
    fn reason(&self) -> FallbackReason {
        match self {
            BatchBackendError::InitFailed(_) => FallbackReason::Init,
            BatchBackendError::DeviceLost(_)
            | BatchBackendError::KernelLaunchFailed(_)
            | BatchBackendError::TransferFailed(_) => FallbackReason::Runtime,
        }
    }
}

impl fmt::Display for BatchBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchBackendError::InitFailed(msg) => write!(f, "batch backend init failed: {msg}"),
            BatchBackendError::DeviceLost(msg) => write!(f, "batch backend device lost: {msg}"),
            BatchBackendError::KernelLaunchFailed(msg) => {
                write!(f, "batch backend kernel launch failed: {msg}")
            }
            BatchBackendError::TransferFailed(msg) => {
                write!(f, "batch backend transfer failed: {msg}")
            }
        }
    }
}

impl std::error::Error for BatchBackendError {}

/// [`BatchBackend::batch_search`] のエラー。入力検証エラー（fail-closed で
/// そのままクライアントへ返す・縮退させない）とバックエンド実行エラー（縮退
/// トリガ）を型で峻別する（TASK-129 設計の要）。入力検証エラーを縮退で握り
/// つぶすと不正入力が黙殺されてしまうため、この区別を型レベルで強制する。
#[derive(Debug, Clone, PartialEq)]
pub enum BatchExecError {
    /// クエリ・常駐行列の入力自体が不正（fail-closed。縮退させず `Err` を返す）。
    Input(BatchSearchError),
    /// バックエンドの実行に失敗した（縮退トリガ）。
    Backend(BatchBackendError),
}

impl fmt::Display for BatchExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchExecError::Input(e) => write!(f, "{e}"),
            BatchExecError::Backend(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BatchExecError {}

/// バッチ検索の実行バックエンド窓口（CORE-8 ポインタ）。object-safe
/// （`&self` メソッドのみ・ジェネリクスなし）を維持し、[`FallbackBatchEngine`]
/// が `Box<dyn BatchBackend>` として保持できることを前提にする。エラー注入
/// テストは本体へテスト専用の公開 API・feature を追加せず、本 trait を
/// テストコード側で実装したモックを差し込むことで実現する（test-support
/// feature 撤廃（Issue #137 対応）の方針を踏襲）。
///
/// # 公開差し替え点としての契約（codex-review P0 指摘対応・PR #152）
///
/// 本 trait は将来の実 GPU/外部実装が差し込まれる公開点であり、
/// [`FallbackBatchEngine`] は実装が誠実であることを前提にしない。`Ok` を
/// 返す場合、実装は以下を満たすことが期待される契約とする（満たさなくても
/// 型としては受理されるが、[`FallbackBatchEngine::batch_search`] が
/// `Self::revalidate_primary_hits` で独立に再検証し、違反があれば結果を
/// 一切返さず `Err` を返す）:
/// - `Vec<BatchHit>` の長さが `queries.len()` と一致する
/// - 各クエリの `hits.len()` が対応する `BatchQuery::k` 以下
/// - 各 `SearchHit` の `(tenant_id, id)` はそのバックエンドが走査対象とした常駐
///   行列に実在する行のものであり、かつ対応するクエリの
///   `BatchQuery::ctx.is_visible(..)` を満たす（対象ビヘイビア: TABLE-12・
///   RLS-9。行 `id` の一意性スコープはテナント内に閉じているため、`id` 単独
///   ではなく `(tenant_id, id)` の組で実在性を判定する）
/// - 各 `SearchHit::score` は有限値
/// - 同一クエリ内で `(tenant_id, id)` が重複しない
/// - スコア降順・同点は常駐行列の行順（`batch_search.rs::run_batch_search` が
///   実際に返す順序と同じ契約。`kernel.rs::TopKSelector::into_sorted_vec` の
///   識別子昇順タイブレークが常駐行列のスロット番号で行われるため）。ここで
///   言う「行順」は [`FallbackBatchEngine::build`] に渡された `ids`/
///   `tenant_ids`/`vectors` 配列の並び（＝本実装が独立再検証で使う
///   `self.cpu` のスロット番号）であり、`BatchBackend` 実装が内部で保持する
///   任意の順序ではない。実装が内部で行を並べ替える場合、その並べ替えを
///   考慮せずスコア降順・元の行順で同点タイブレークした結果を返す必要がある
///   （そうしないと (4) の順序検証で正当な結果が `PrimaryResultRejected` に
///   なる）
///
/// 逆に、[`FallbackBatchEngine`] 経由で呼ばれる実装は `queries` を自前で
/// 再検証する必要はない: [`FallbackBatchEngine::batch_search`] は
/// `batch_search.rs::validate_batch_queries`（次元一致・非有限値なし・
/// `k` が範囲内・バッチ件数上限内・`sum(k)` が上限内）を本 trait の呼び出し
/// より前に適用する（TASK-129・CORE-8 レビュー起因の P1 指摘対応・PR #152）。
/// この順序は可用性のための契約でもある: もし実装が独自の入力検証を行い、
/// 不正入力に対して `BatchExecError::Backend`（実行時エラー）を返すと、
/// `FallbackBatchEngine` はそれを「デバイスの恒久故障」と誤認して以降の
/// 全呼び出しを CPU 縮退へ固定してしまう。入力検証エラーは常に
/// `BatchExecError::Input` として返し、`BatchExecError::Backend` は実際の
/// バックエンド実行障害（デバイスロスト等）専用に使うこと。
pub trait BatchBackend: Send + Sync {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError>;
}

/// 既存 `BatchEngine`（f16 パック常駐行列を走査する GPU 経路の CPU 参照実装。
/// TASK-128 ポインタ）を primary バックエンドとして適合させるラッパー。本実装
/// からランタイムエラー（[`BatchExecError::Backend`]）が自発的に発生することは
/// ない（CPU 上で決定的に計算するため）。実 GPU への接続は
/// `gpu_batch.rs::GpuBatchBackend`（TASK-128〜130・Issue #178 ポインタ。
/// `Self::build_with_gpu` 経由）が担い、テスト・ベンチの配線疎通用に本型は
/// 残す（`Self::build_with_gpu_reference`）。
pub struct GpuReferenceBackend {
    engine: BatchEngine,
}

impl GpuReferenceBackend {
    pub fn new(matrix: ResidentMatrix) -> Self {
        Self {
            engine: BatchEngine::new(matrix),
        }
    }
}

impl BatchBackend for GpuReferenceBackend {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        self.engine
            .batch_search(queries)
            .map_err(BatchExecError::Input)
    }
}

/// 縮退の要因種別（ログ可視化用。CORE-8 ポインタ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// primary バックエンドの構築（初期化）が失敗した。
    Init,
    /// primary バックエンドは初期化済みだが、実行時エラー（デバイスロスト・
    /// カーネル起動失敗・転送失敗）により縮退した。
    Runtime,
}

impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FallbackReason::Init => write!(f, "init"),
            FallbackReason::Runtime => write!(f, "runtime"),
        }
    }
}

/// 縮退イベント 1 件（CORE-8 ポインタ）。テナント ID・クエリ内容は含めない
/// （security.md「機微情報の漏えい」対応。Display 衛生をユニットテストで
/// 固定する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FallbackEvent {
    pub reason: FallbackReason,
    /// 切り替え先バックエンドの識別子（現状は常に `"cpu-simd"`）。
    pub target: &'static str,
}

impl fmt::Display for FallbackEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "batch_fallback: switched to {} backend (reason={})",
            self.target, self.reason
        )
    }
}

/// 縮退発生の可視化フック（CORE-8 ポインタ:「黙って性能劣化しない」契約）。
/// engine には log/tracing 系依存が存在しないため（dependency-policy.md:
/// 依存追加はユーザー承認制）、依存追加なしの注入型オブザーバとして設計する。
/// 既定実装 [`StderrFallbackObserver`] は英語 1 行を stderr へ出力する
/// （wire-server 側の将来のログ機構へ差し替え可能）。テストは記録型オブザーバで
/// 発生回数・要因・切り替え先をアサートする。
pub trait FallbackObserver: Send + Sync {
    fn on_fallback(&self, event: FallbackEvent);
}

/// 既定の縮退オブザーバ。stderr へ英語 1 行を出力する。
#[derive(Debug, Default, Clone, Copy)]
pub struct StderrFallbackObserver;

impl FallbackObserver for StderrFallbackObserver {
    fn on_fallback(&self, event: FallbackEvent) {
        // 診断ログの書き込み失敗（読み手が閉じた後の `EPIPE` 等）はプロセスを
        // 落とさない（RECOVER-8 の例外、Issue #1081。
        // `docs/design/stderr-log-write-failure.md`）。
        crate::log_stderr!("{event}");
    }
}

/// CPU 縮退経路が走査する f32 常駐行列（CORE-8 ポインタ）。`batch_search.rs::
/// ResidentMatrix` の f16 パックとは独立に f32 原本を保持する（本モジュール
/// 冒頭コメント「データ所有権・二重常駐について」参照）。
struct CpuFallbackMatrix {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    dim: usize,
    vectors: Vec<f32>,
}

impl BatchRowSource for CpuFallbackMatrix {
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
        let start = idx.checked_mul(self.dim)?;
        let end = start.checked_add(self.dim)?;
        let row = self.vectors.get(start..end)?;
        out.clear();
        out.extend_from_slice(row);
        Some(())
    }
}

/// primary バックエンドの保持状態。初期化失敗を検知した後は毎回 primary の
/// 呼び出しを試みず（無駄な失敗呼び出しを避ける）、以降の `batch_search` は
/// 常に CPU 縮退経路を使う。実行時エラー（デバイスロスト等）による恒久故障の
/// ラッチは `FallbackBatchEngine::runtime_latched`（`AtomicBool`）が別途担う
/// （`&self` の `batch_search` から更新できるよう `PrimarySlot` 自体は不変の
/// まま、ラッチ判定だけ内部可変性で扱う。CORE-8 レビュー起因）。
enum PrimarySlot {
    Available(Box<dyn BatchBackend>),
    Unavailable,
}

/// バッチ経路の新しい公開入口（CORE-8 ポインタ）。primary バックエンド
/// （通常は [`GpuReferenceBackend`]）を実行し、バックエンド実行エラー
/// （[`BatchExecError::Backend`]）の場合のみ CPU-SIMD 縮退経路
/// （`batch_search.rs::run_batch_search` を CPU 縮退用行列で呼ぶ。CORE-3 と
/// 同一カーネル）へ再実行する。ただし実行時エラー（デバイスロスト・カーネル
/// 起動失敗・転送失敗）は GPU デバイスの恒久故障を示すことが多いため、初回
/// 検知時に `runtime_latched` をラッチし、以降の呼び出しは primary を再試行
/// せず直接 CPU 経路を使う（無制限の stderr 出力・primary への無駄な再試行
/// コストを防ぐ。CORE-8 レビュー起因）。入力検証エラー
/// （[`BatchExecError::Input`]）は縮退させず fail-closed に `Err` をそのまま
/// 返す（不正入力を縮退で黙殺しない、という本モジュールの設計の要）。
pub struct FallbackBatchEngine {
    primary: PrimarySlot,
    /// primary の実行時エラーによる恒久故障ラッチ（CORE-8 レビュー起因）。
    /// `false`→`true` の遷移が最初に成功した呼び出しだけが縮退イベントを
    /// observer へ通知し、以降の呼び出しは黙って CPU 経路を使う
    /// （`compare_exchange` で二重通知を防ぐ。`&self` の `batch_search` から
    /// 更新するため `AtomicBool` による内部可変性を用いる）。
    runtime_latched: std::sync::atomic::AtomicBool,
    cpu: CpuFallbackMatrix,
    observer: Box<dyn FallbackObserver>,
}

impl FallbackBatchEngine {
    /// 元データ（可視性フィルタ済みの行集合。`ResidentMatrix::build` と同じ
    /// 信頼境界: `tenant_ids`/`visibilities` は untrusted なユーザー入力では
    /// なく、呼び出し元が `Storage` から読み出した実データを渡す責務を負う）
    /// から primary バックエンドと CPU 縮退用の f32 常駐行列を構築する。
    ///
    /// `backend_factory` は primary バックエンドの初期化を担う（実 GPU 接続時
    /// はデバイス確保等を行う想定。[`GpuReferenceBackend`] は常に成功する）。
    /// 初期化が [`BatchBackendError`] を返した場合、本コンストラクタ自体は
    /// panic せず `Ok`（CPU 専用モード）で成立させ、縮退イベントを 1 件通知
    /// する（CORE-8: 初期化失敗からの縮退）。一方、`ids`/`tenant_ids`/
    /// `visibilities`/`vectors` 自体のデータ不整合・容量超過
    /// （[`BatchSearchError`] 系）は縮退させず `Err` で fail-closed に返す
    /// （構築段階の入力エラーも「縮退しない」対象として扱う）。
    pub fn build(
        ids: &[u64],
        tenant_ids: &[String],
        visibilities: &[Visibility],
        dim: usize,
        vectors: &[f32],
        backend_factory: impl FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>, BatchBackendError>,
        observer: Box<dyn FallbackObserver>,
    ) -> Result<Self, BatchSearchError> {
        // CPU 縮退用 f32 行列の容量検証（本モジュール冒頭コメント「データ
        // 所有権・二重常駐について」参照）。`ResidentMatrix::build`（f16 パック、
        // f32 のおよそ半分のバイト数）より先に本チェックを置く: f32 原本は
        // f16 パックの約 2 倍のバイト量になるため、f16 側は
        // `MAX_BATCH_TOTAL_BYTES` に収まるが f32 側だけが超過する行数・次元の
        // 組み合わせが実在する。`ResidentMatrix::build` を先に呼ぶと、その
        // 組み合わせでは f16 側の検証を素通りしてから `vectors.len()` の長さ
        // チェック（巨大バッファが必要でテスト到達が困難）へ進んでしまい、
        // 本チェックへのテストカバレッジが失われるため、`ResidentMatrix::build`
        // 側の容量検証（`batch_search.rs::ResidentMatrix::build` の
        // `packed_bytes`/`aux_bytes` 検証、`vectors.len()` チェックより前に
        // 置かれている）と同じ設計方針で、実データ確保より前に配置する。
        //
        // `CpuFallbackMatrix` は f32 ベクトル本体だけでなく `ids`/`tenant_ids`/
        // `visibilities` も独立に複製して保持する（下部の `owned_*` 構築）ため、
        // 容量予算はベクトル本体（f32_bytes）と aux バイト（Cursor Bugbot 指摘
        // 対応: `ids`/`tenant_ids`/`visibilities` の複製分）の両方を合算した
        // 総量で検証する。aux 側の見積もりは `ResidentMatrix::build` の
        // `per_row_aux_bytes`（`batch_search.rs`）と同じ式（`u64` + `String` +
        // `MAX_TENANT_ID_LEN` + `Visibility` のサイズ）を用い、二重管理を避ける。
        let f32_bytes = ids
            .len()
            .checked_mul(dim)
            .and_then(|n| n.checked_mul(std::mem::size_of::<f32>()))
            .ok_or(BatchSearchError::CapacityExceeded {
                total_bytes: usize::MAX,
                max: MAX_BATCH_TOTAL_BYTES,
            })?;
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
        let fallback_total_bytes =
            f32_bytes
                .checked_add(aux_bytes)
                .ok_or(BatchSearchError::CapacityExceeded {
                    total_bytes: usize::MAX,
                    max: MAX_BATCH_TOTAL_BYTES,
                })?;
        if fallback_total_bytes > MAX_BATCH_TOTAL_BYTES {
            return Err(BatchSearchError::CapacityExceeded {
                total_bytes: fallback_total_bytes,
                max: MAX_BATCH_TOTAL_BYTES,
            });
        }

        // primary（f16 パック常駐行列）を構築する。次元・行数・容量（f16 分）・
        // 重複 id・tenant_id 長の検証はここで一元的に行われ
        // （`ResidentMatrix::build`）、CPU 縮退用の f32 行列はこの検証を通過した
        // 後に同じ元データから独立に構築する（検証ロジックの二重管理を避ける）。
        let resident = ResidentMatrix::build(ids, tenant_ids, visibilities, dim, vectors)?;

        // 以降のフォールブル確保は `ResidentMatrix::build` と同方針
        // （`Vec::with_capacity`・`to_vec()`・`String::clone` は abort-on-OOM
        // のため使わず、`try_reserve_exact` で明示的に処理する）。
        let mut owned_vectors: Vec<f32> = Vec::new();
        owned_vectors
            .try_reserve_exact(vectors.len())
            .map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve fallback vectors: {e}"
                ))
            })?;
        owned_vectors.extend_from_slice(vectors);

        let mut owned_ids: Vec<u64> = Vec::new();
        owned_ids.try_reserve_exact(ids.len()).map_err(|e| {
            BatchSearchError::AllocationFailed(format!("failed to reserve fallback ids: {e}"))
        })?;
        owned_ids.extend_from_slice(ids);

        let mut owned_tenant_ids: Vec<String> = Vec::new();
        owned_tenant_ids
            .try_reserve_exact(tenant_ids.len())
            .map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve fallback tenant_ids: {e}"
                ))
            })?;
        for tenant in tenant_ids {
            let mut owned = String::new();
            owned.try_reserve_exact(tenant.len()).map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve fallback tenant_id: {e}"
                ))
            })?;
            owned.push_str(tenant);
            owned_tenant_ids.push(owned);
        }

        let mut owned_visibilities: Vec<Visibility> = Vec::new();
        owned_visibilities
            .try_reserve_exact(visibilities.len())
            .map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve fallback visibilities: {e}"
                ))
            })?;
        owned_visibilities.extend_from_slice(visibilities);

        let cpu = CpuFallbackMatrix {
            ids: owned_ids,
            tenant_ids: owned_tenant_ids,
            visibilities: owned_visibilities,
            dim,
            vectors: owned_vectors,
        };

        let primary = match backend_factory(resident) {
            Ok(backend) => PrimarySlot::Available(backend),
            Err(init_err) => {
                // `backend_factory` の戻り値型は `BatchBackendError` 全種別を
                // 許容するため、初期化時にも Runtime 相当のエラー（実 GPU
                // 接続時のデバイス確保中の転送失敗等）が返り得る。panic させず
                // `init_err.reason()` の判別結果をそのまま可視化する
                // （coding-rust.md「ライブラリコードでは Result を返し、panic
                // させない」・モジュール冒頭コメントの「初期化失敗と実行時
                // エラーを区別する」設計方針に従う）。
                observer.on_fallback(FallbackEvent {
                    reason: init_err.reason(),
                    target: "cpu-simd",
                });
                PrimarySlot::Unavailable
            }
        };

        Ok(Self {
            primary,
            runtime_latched: std::sync::atomic::AtomicBool::new(false),
            cpu,
            observer,
        })
    }

    /// [`GpuReferenceBackend`] を primary として使う既定コンストラクタ。実 GPU
    /// 未接続の現段階では常に初期化成功する（モジュール冒頭コメント参照）ため、
    /// 通常の呼び出し元はこちらを使う。
    pub fn build_with_gpu_reference(
        ids: &[u64],
        tenant_ids: &[String],
        visibilities: &[Visibility],
        dim: usize,
        vectors: &[f32],
        observer: Box<dyn FallbackObserver>,
    ) -> Result<Self, BatchSearchError> {
        Self::build(
            ids,
            tenant_ids,
            visibilities,
            dim,
            vectors,
            |matrix| Ok(Box::new(GpuReferenceBackend::new(matrix)) as Box<dyn BatchBackend>),
            observer,
        )
    }

    /// 実 GPU（`gpu_batch.rs::GpuBatchBackend`。`wgpu`。TASK-128〜130・Issue #178
    /// ポインタ）を primary として使うコンストラクタ。GPU デバイスの初期化に
    /// 失敗した場合（GPU 非搭載・ドライバ不整合等）は panic せず縮退イベントを
    /// 1 件通知して CPU 専用モードで構築が成立する（[`Self::build`] の契約と
    /// 同じ。CORE-8: 初期化失敗からの縮退）。`wgpu` 依存はオーナー承認済み
    /// （2026-08-26。`crates/engine/Cargo.toml` コメント参照）。
    pub fn build_with_gpu(
        ids: &[u64],
        tenant_ids: &[String],
        visibilities: &[Visibility],
        dim: usize,
        vectors: &[f32],
        observer: Box<dyn FallbackObserver>,
    ) -> Result<Self, BatchSearchError> {
        Self::build(
            ids,
            tenant_ids,
            visibilities,
            dim,
            vectors,
            |matrix| {
                crate::gpu_batch::GpuBatchBackend::try_new(matrix)
                    .map(|backend| Box::new(backend) as Box<dyn BatchBackend>)
            },
            observer,
        )
    }

    /// primary バックエンド（[`BatchBackend`]）が返した成功結果を、
    /// `FallbackBatchEngine` が構築時から保持する信頼済み CPU 常駐行列
    /// （`self.cpu`。`Storage` 由来の実データであり untrusted ではない。
    /// [`Self::build`] のドキュメンテーションコメント「信頼境界」参照）を
    /// 基準に独立再検証する（codex-review P0 指摘対応・PR #152）。
    ///
    /// `BatchBackend` は object-safe な公開差し替え点であり、将来の実
    /// GPU/外部実装が結果件数・`k`・id の存在・可視性を満たさない
    /// `Vec<BatchHit>` を返しうる。`run_batch_search`（`batch_search.rs`）が
    /// 自身の `TopKSelector` の出力を再検証する既存ロジック（本ファイル
    /// 冒頭 import の `run_batch_search` が呼ぶ末尾チェック）は、選出器が
    /// 構造的に保証済みの性質（件数上限・重複なし・有限値・順序）を前提に
    /// id/tenant の可視性だけを見ており、`GpuReferenceBackend`
    /// （`run_batch_search` を内部で通る現状唯一の実装）には十分だが、
    /// 任意の `BatchBackend` 実装には不十分である。そのため本メソッドは
    /// `core.rs::CoreError::ProviderResultRejected` と同じ「untrusted provider
    /// 出力を信頼済み集合と突き合わせて検証し、1 件でも違反すれば結果を
    /// 一切返さない」設計を踏襲し、構造契約（クエリ数との整合・件数上限・
    /// id 重複なし・スコア有限性・スコア降順/同点 id 昇順）と id/tenant/
    /// 可視性の両方を検証する。
    ///
    /// 検証コストは呼び出しごとに O(matching_rows + sum(hits)) 増える
    /// （`id_to_tenant` の構築が主要因）。これは「バックエンドを信頼しない」
    /// ことの対価であり、無条件で `Ok(hits)` を返す旧実装より高コストだが、
    /// テナント境界（security.md P0）を優先する。
    ///
    /// 違反を検知した場合、CPU 縮退への再実行・`runtime_latched` のラッチ・
    /// `FallbackObserver` への通知のいずれも行わず `Err` をそのまま返す
    /// （`BatchExecError::Input`/`TenantMaskViolation` の扱いと同じ方針。
    /// モジュール冒頭の [`FallbackBatchEngine`] ドキュメンテーションコメント
    /// 「入力検証エラーは縮退させず fail-closed に `Err` をそのまま返す」を
    /// 適用する）。理由: `runtime_latched` は「GPU デバイスの恒久故障」を
    /// 表すためのラッチであり（[`FallbackBatchEngine`] のフィールドコメント
    /// 参照）、結果契約違反はデバイス故障とは異なる種類の異常である。もし
    /// ここでラッチすると、2 回目以降の呼び出しは CPU 縮退経由で `Ok` を
    /// 返すようになり、検知したテナント混入・構造違反の兆候を暗黙に
    /// 消してしまう（fail-open 化。security.md「fail-closed を維持する」に
    /// 反する）。呼び出しのたびに検証し、違反があれば毎回 `Err` を返す方が
    /// 安全側である。
    fn revalidate_primary_hits(
        &self,
        queries: &[BatchQuery<'_>],
        hits: &[BatchHit],
    ) -> Result<(), BatchSearchError> {
        // (0) 全体の結果構造がクエリ数と整合すること。件数不足・過剰の
        // どちらも同じ違反として扱う（`zip` で暗黙に切り詰めると、過剰分の
        // 検証をすり抜けてしまうため、比較を先に行う）。
        if hits.len() != queries.len() {
            return Err(BatchSearchError::PrimaryResultRejected);
        }

        // このバッチに登場するテナント集合（`run_batch_search` の
        // `batch_tenants` と同じ考え方）。`id_to_tenant` をこの集合に
        // 属する行だけへ絞ることで、バッチ外テナントの id が万一混入しても
        // マップ不在 → 違反として確実に拒否できる（fail-closed 側の効果）。
        let mut batch_tenants: HashSet<&str> = HashSet::new();
        batch_tenants.try_reserve(queries.len()).map_err(|e| {
            BatchSearchError::AllocationFailed(format!(
                "failed to reserve batch tenants for primary result revalidation: {e}"
            ))
        })?;
        for q in queries {
            batch_tenants.insert(q.ctx.tenant_id());
        }

        // ポインタ: TASK-89 / TABLE-9（`policy.rs::PolicyContext::is_visible` の
        // 判定に整合させる）。`run_batch_search`（`batch_search.rs`）側から
        // バッチ外テナントの行が返りうるケースがあるため、この独立
        // 再検証（`revalidate_primary_hits`）はその経路とは別に自前で
        // `id_to_tenant` を組むため、`run_batch_search` と同じ拡張をここにも
        // 反映しないと、正当な hit を `TenantMaskViolation` として誤検知する
        // （`ctx.tenant_id()` を行テナントとして渡すことで許可集合だけを
        // `is_visible` 経由で判定する。`batch_search.rs::run_batch_search` の
        // 同名ロジック参照）。
        let public_grant_query_count_nonzero = queries
            .iter()
            .any(|q| q.ctx.is_visible(q.ctx.tenant_id(), Visibility::Public));

        // (tenant_id, id) → (slot, visibility) の逆引き表（対象ビヘイビア:
        // TABLE-12・RLS-9。PR #205）。`self.cpu`（信頼済み CPU 常駐行列）は
        // `ResidentMatrix::build` と同じ一意性検証を経ている（[`Self::build`]）
        // ため、`(tenant_id, id)` は常駐行列内のスロットを一意に決める。
        // 行 `id` の一意性スコープはテナント内に閉じているため、`id` 単独では
        // 逆引きに使えない（自テナント行と他テナントの `Public` 行が同じ `id`
        // を持ちうる）。
        let matching_row_count = self
            .cpu
            .tenant_ids
            .iter()
            .zip(self.cpu.visibilities.iter())
            .filter(|(t, v)| {
                batch_tenants.contains(t.as_str())
                    || (public_grant_query_count_nonzero && **v == Visibility::Public)
            })
            .count();
        let mut row_key_to_slot: HashMap<(&str, u64), (usize, Visibility)> = HashMap::new();
        row_key_to_slot
            .try_reserve(matching_row_count)
            .map_err(|e| {
                BatchSearchError::AllocationFailed(format!(
                    "failed to reserve row-key map for primary result revalidation: {e}"
                ))
            })?;
        for (idx, ((id, tenant), visibility)) in self
            .cpu
            .ids
            .iter()
            .zip(self.cpu.tenant_ids.iter())
            .zip(self.cpu.visibilities.iter())
            .enumerate()
        {
            let is_reachable_public =
                public_grant_query_count_nonzero && *visibility == Visibility::Public;
            if batch_tenants.contains(tenant.as_str()) || is_reachable_public {
                row_key_to_slot.insert((tenant.as_str(), *id), (idx, *visibility));
            }
        }

        for (q, batch_hit) in queries.iter().zip(hits) {
            // (1) 件数が要求 k を超えない。
            if batch_hit.hits.len() > q.k {
                return Err(BatchSearchError::PrimaryResultRejected);
            }

            let mut seen_row_keys: HashSet<(&str, u64)> = HashSet::new();
            seen_row_keys
                .try_reserve(batch_hit.hits.len())
                .map_err(|e| {
                    BatchSearchError::AllocationFailed(format!(
                        "failed to reserve seen row-key set for primary result revalidation: {e}"
                    ))
                })?;
            let mut prev: Option<(f32, usize)> = None;
            for hit in &batch_hit.hits {
                // (2) スコアが有限（NaN/Inf でない）。非有限スコアは全順序を
                // 持たず、後続の順序検証（`total_cmp`）が無意味になるため
                // 他の検証より先に弾く（`core.rs::search` と同じ順序）。
                if !hit.score.is_finite() {
                    return Err(BatchSearchError::PrimaryResultRejected);
                }
                // (3) `(tenant_id, id)` が重複しない（同じ行が同一クエリ内で
                // 複数回返らない。バックエンドが自称するテナント名をそのまま
                // 使うが、後続 (5) でその名が信頼済み行と一致することを
                // 独立に検証するため、偽装テナント名による重複回避は
                // (5) で別途拒否される）。
                if !seen_row_keys.insert((hit.tenant_id.as_str(), hit.id)) {
                    return Err(BatchSearchError::PrimaryResultRejected);
                }
                // (5) `(tenant_id, id)` が信頼済み常駐行列に存在し、かつ
                // そのクエリの `PolicyContext::is_visible` を満たす（他
                // テナント id・捏造 id・可視性偽装・テナント名偽装（実在
                // する id に別テナント名を付けたヒット）のいずれも
                // マップ不在として拒否する。テナント混入固有の違反のため
                // `TenantMaskViolation` を使う）。(4) の順序判定に使う
                // スロットを得るため、(4) より先に行う。
                let Some(&(slot, visibility)) =
                    row_key_to_slot.get(&(hit.tenant_id.as_str(), hit.id))
                else {
                    return Err(BatchSearchError::TenantMaskViolation);
                };
                if !q.ctx.is_visible(hit.tenant_id.as_str(), visibility) {
                    return Err(BatchSearchError::TenantMaskViolation);
                }
                // (4) スコア降順・同点は常駐行列のスロット昇順
                // （`batch_search.rs::run_batch_search` が実際に返す順序と
                // 同じ契約。`kernel.rs::TopKSelector::into_sorted_vec` の
                // 識別子昇順タイブレークが選出段でスロット番号に対して
                // 行われるため）。
                if let Some((prev_score, prev_slot)) = prev {
                    let out_of_order = match prev_score.total_cmp(&hit.score) {
                        std::cmp::Ordering::Less => true,
                        std::cmp::Ordering::Equal => prev_slot >= slot,
                        std::cmp::Ordering::Greater => false,
                    };
                    if out_of_order {
                        return Err(BatchSearchError::PrimaryResultRejected);
                    }
                }
                prev = Some((hit.score, slot));
            }
        }

        Ok(())
    }

    /// バッチ検索を実行する（CORE-8 ポインタ）。
    ///
    /// primary（[`BatchBackend`]。公開差し替え点で将来の実 GPU/外部実装に
    /// 差し替え可能）・CPU 縮退経路のいずれを呼ぶよりも先に、共有の入力検証
    /// [`crate::batch_search::validate_batch_queries`] を適用する（TASK-129・
    /// CORE-8 レビュー起因の P1 指摘対応・PR #152）。この順序が重要な理由:
    /// 検証前に primary へ処理を渡すと、入力検証を行わない実装が不正入力
    /// （次元不一致・非有限値・`k` 範囲外・バッチ件数超過等）に対して
    /// `BatchExecError::Backend`（実行時エラー）を返しうる。これを検知すると
    /// 下記の実行時エラー処理が `runtime_latched` を恒久ラッチしてしまい、
    /// 悪意ある/バグのある単一の不正入力だけで以降の正当な検索まで CPU
    /// 縮退経路へ固定される可用性バグになる。先行検証により、不正入力は
    /// primary 呼び出し・ラッチ更新・observer 通知のいずれも発生させずに
    /// `Err` を返す。
    ///
    /// 先行検証を通過した後、primary が利用可能かつ実行時ラッチ未発火なら
    /// primary を実行し、[`BatchExecError::Backend`]（実行時エラー）の場合
    /// のみ CPU 縮退経路へ再実行しつつ `runtime_latched` をラッチする（初回
    /// 検知時のみ observer へ通知。以降は primary を再試行せず直接 CPU 経路
    /// へ進む。「無制限の stderr 出力・primary への無駄な再試行コストを防ぐ」
    /// という CORE-8 レビュー起因の要件）。[`BatchExecError::Input`]（primary
    /// 自身が返す入力エラー。上記の先行検証とは別に、primary 固有の検証
    /// ロジックが返しうる `BatchSearchError`）は縮退させず `Err` をそのまま
    /// 返す（TenantMaskViolation を含め、縮退イベントを発生させない）。primary
    /// が初期化失敗済み（[`PrimarySlot::Unavailable`]）の場合は最初から CPU
    /// 経路を使う（追加の縮退イベントは発生させない。構築時に既に 1 件
    /// 通知済みのため）。primary が成功を返した場合も、その結果をそのまま
    /// 信頼せず [`Self::revalidate_primary_hits`] で独立再検証する
    /// （codex-review P0 指摘対応・PR #152）。違反時は CPU 縮退へ再実行せず
    /// `Err` をそのまま返す（同メソッドのドキュメンテーションコメント参照）。
    pub fn batch_search(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<BatchHit>, BatchSearchError> {
        use std::sync::atomic::Ordering;

        // primary・CPU 縮退のどちらへも処理を渡す前に、入力自体の妥当性を
        // 確定させる（本メソッドのドキュメンテーションコメント「先行検証」
        // 参照）。`self.cpu.dim` は [`Self::build`] が `ResidentMatrix::build`
        // （primary 用）と `CpuFallbackMatrix`（CPU 縮退用）の双方へ同じ
        // `dim` 引数を渡して構築するため、両経路で共通の値として使える。
        // これは `backend_factory`（[`Self::build`] の引数）が受け取った
        // `ResidentMatrix` をそのまま primary の走査対象として使う、という
        // 暗黙の契約に依存する: `backend_factory` が別の次元を持つ行列で
        // primary を構築した場合、本チェックはその primary にとって正しい
        // 検証にならない（現状の唯一の実装 [`GpuReferenceBackend::new`] は
        // 渡された `ResidentMatrix` をそのまま使うためこの契約を満たす）。
        crate::batch_search::validate_batch_queries(self.cpu.dim, queries)?;

        // 空バッチ（`queries.is_empty()`）を先行検証直後に確定的に扱う（Cursor Bugbot
        // Medium 指摘対応・PR #158）。`validate_batch_queries` は件数 0 を有効な入力
        // として受理する（走査すべき行が単に存在しないだけであり、`batch_search.rs`
        // 全体の契約として空バッチはエラーではない）一方、`DispatchInput::for_batch`
        // は `batch_size == 0` を不正入力として拒否する（決定表が確定させるべき経路が
        // 存在しないため。`dispatch.rs` の fail-closed な検証方針）。この 2 つの
        // 契約差を先行検証の直後で吸収せずに `runtime_latched` の後段まで進めると、
        // 空バッチの成否が「これまでに primary が実行時失敗して縮退済みか」という
        // 無関係な状態に依存してしまう（ラッチ済みなら `run_batch_search` へ直行して
        // 成功、未ラッチなら後段の `DispatchInput::for_batch` が `InvalidBatchSize` を
        // 返し失敗、という同一入力に対する非決定的な挙動）。ここで確定させることで、
        // 空バッチは常に成功（空の結果）となり、`dispatch` の経路選択自体を
        // 呼び出さない（選択すべき経路が存在しないため）。
        if queries.is_empty() {
            return Ok(Vec::new());
        }

        if self.runtime_latched.load(Ordering::Acquire) {
            return run_batch_search(&self.cpu, queries);
        }

        // 実行経路の決定（TASK-155・対象ビヘイビア: CORE-11, CORE-12）。primary が構築
        // 成功している（[`PrimarySlot::Available`]）場合のみ、その backend への参照を
        // witness として [`GpuCapability::proven`] へ渡す（codex-review P1 指摘対応・
        // PR #158: `proven` は検証済み backend への参照を提示できない限り呼べないため、
        // 未検証の GPU capability を `dispatch` へ持ち込む経路は構造的にない。CORE-12）。
        // バッチ経路は件数によらず常にバッチ扱いになる決定表の性質（`dispatch.rs`
        // モジュールドキュメント参照）により、`self.primary` が `Available` なら常に
        // primary を試みる・`Unavailable` なら常に CPU 縮退経路を使うという、以下の
        // 分岐が従来持っていた挙動をそのまま維持する（旧: 本 `match` 自体が経路選択を
        // 担っていたが、経路選択の判断自体は `select_execution_path` へ委譲した）。
        let gpu = match &self.primary {
            PrimarySlot::Available(backend) => Some(GpuCapability::proven(backend.as_ref())),
            PrimarySlot::Unavailable => None,
        };
        let dispatch_input = DispatchInput::for_batch(gpu, self.cpu.dim, queries.len())
            .map_err(to_batch_search_error)?;
        let execution_path =
            dispatch::select_execution_path(dispatch_input).map_err(to_batch_search_error)?;

        let use_primary = matches!(execution_path, ExecutionPath::Gpu);

        match (&self.primary, use_primary) {
            (PrimarySlot::Available(backend), true) => match backend.batch_search(queries) {
                Ok(hits) => {
                    // primary バックエンドは公開差し替え点（[`BatchBackend`]）であり、
                    // 将来の実 GPU/外部実装が任意の `Vec<BatchHit>` を返しうる
                    // （codex-review P0 指摘対応・PR #152）。`GpuReferenceBackend`
                    // （現状唯一の実装）は内部で `run_batch_search` を通るため
                    // 自己無矛盾だが、`BatchBackend` トレイト自体はそれを保証しない。
                    // ここで信頼済み CPU 常駐行列（`self.cpu`）を基準に、
                    // `core.rs::CoreError::ProviderResultRejected` と同じ
                    // 「untrusted provider 出力を独立に再検証する」設計で
                    // 成功結果も無条件で信頼しない（詳細は
                    // `Self::revalidate_primary_hits` 参照）。
                    self.revalidate_primary_hits(queries, &hits)?;
                    Ok(hits)
                }
                Err(BatchExecError::Input(e)) => Err(e),
                Err(BatchExecError::Backend(err)) => {
                    // ラッチの初回発火（false → true）に成功した呼び出しだけが
                    // observer へ通知する。`compare_exchange` の失敗（既に他の
                    // 呼び出しがラッチ済み）は通知しない: 同時実行下でも
                    // イベント二重発行を避けるための直列化点。
                    if self
                        .runtime_latched
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        self.observer.on_fallback(FallbackEvent {
                            reason: err.reason(),
                            target: "cpu-simd",
                        });
                    }
                    run_batch_search(&self.cpu, queries)
                }
            },
            // `select_execution_path` は GPU capability を渡した場合にのみ
            // `ExecutionPath::Gpu` を返す（`dispatch.rs` の決定表参照）ため、
            // `use_primary == true` かつ `self.primary` が `Unavailable` という
            // 組み合わせは到達しない。到達した場合も CPU 縮退経路へ倒す
            // fail-closed な catch-all として扱う（`self.primary` が `Available` でも
            // `use_primary == false` になることはない前提だが、決定表側の将来変更で
            // 前提が崩れても panic せず CPU 経路へ倒す）。
            (_, _) => run_batch_search(&self.cpu, queries),
        }
    }
}

/// [`DispatchError`] を [`BatchSearchError`] へ写像する（`FallbackBatchEngine::
/// batch_search` が `select_execution_path` 呼び出し前に `crate::batch_search::
/// validate_batch_queries` で既に `dim`・`batch_size` を検証済みのため、通常到達
/// しない防御的経路。CORE-11 の決定表が返す `DispatchError` variant と
/// `BatchSearchError` の対応する variant を 1:1 で写す）。
fn to_batch_search_error(e: dispatch::DispatchError) -> BatchSearchError {
    match e {
        dispatch::DispatchError::InvalidDim { dim, max } => {
            BatchSearchError::InvalidDim { dim, max }
        }
        dispatch::DispatchError::InvalidBatchSize { batch_size, max } => {
            BatchSearchError::TooManyQueries {
                count: batch_size,
                max,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyContext;

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id")
    }

    /// `Private` を明示許可した [`PolicyContext`] のテスト用ショートハンド。
    /// テナント分離そのものを検証するテストは `Private` フィクスチャと組で使う
    /// （ポインタ: TASK-89 / TABLE-9）。
    fn private_ctx(tenant: &str) -> PolicyContext {
        PolicyContext::with_visibilities(tenant, [Visibility::Private]).expect("valid tenant id")
    }

    struct RecordingObserver {
        events: std::sync::Mutex<Vec<FallbackEvent>>,
    }

    impl RecordingObserver {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<FallbackEvent> {
            self.events.lock().expect("lock").clone()
        }
    }

    impl FallbackObserver for RecordingObserver {
        fn on_fallback(&self, event: FallbackEvent) {
            self.events.lock().expect("lock").push(event);
        }
    }

    // Display 衛生（security.md「機微情報の漏えい」対応）: エラー・イベントの
    // 出力文字列にテナント情報・クエリ内容が含まれないことを固定する。
    #[test]
    fn backend_error_display_excludes_tenant_and_query_content() {
        let err = BatchBackendError::DeviceLost("nvidia-driver-reset".to_string());
        let rendered = err.to_string();
        assert!(rendered.contains("device lost"));
        assert!(!rendered.contains("tenant"));
    }

    #[test]
    fn fallback_event_display_is_english_and_stable() {
        let event = FallbackEvent {
            reason: FallbackReason::Runtime,
            target: "cpu-simd",
        };
        assert_eq!(
            event.to_string(),
            "batch_fallback: switched to cpu-simd backend (reason=runtime)"
        );
    }

    #[test]
    fn backend_error_reason_classifies_init_vs_runtime() {
        assert_eq!(
            BatchBackendError::InitFailed("x".to_string()).reason(),
            FallbackReason::Init
        );
        for err in [
            BatchBackendError::DeviceLost("x".to_string()),
            BatchBackendError::KernelLaunchFailed("x".to_string()),
            BatchBackendError::TransferFailed("x".to_string()),
        ] {
            assert_eq!(err.reason(), FallbackReason::Runtime);
        }
    }

    // CPU 縮退用 f32 行列の容量検証（本モジュール冒頭コメント「データ所有権・
    // 二重常駐について」対応）。f32 原本は f16 パック（`ResidentMatrix::build`
    // が独立に検証）のおよそ 2 倍のバイト量になるため、f16 側は
    // `MAX_BATCH_TOTAL_BYTES`（1 GiB）に収まるが f32 側だけが超過する境界が
    // 存在する（f16 packed ≈ rows*dim*2 + 補助データ、f32 原本 = rows*dim*4）。
    // 本テストは dim=8192（`MAX_BATCH_DIM` 上限）・rows=40,000 でその境界を
    // 選び、f16 は約 0.62 GiB（収まる）・f32 は約 1.22 GiB（超過）となる
    // 組み合わせを使う。`FallbackBatchEngine::build` は本チェックを
    // `ResidentMatrix::build` より前に実行するため（同関数のコメント参照）、
    // `vectors` は境界チェックへ到達する前段では参照されず、テストのために
    // 数 GB のバッファを実際に確保する必要はない（意図的に短い配列で足りる）。
    #[test]
    fn build_rejects_fallback_matrix_capacity_over_limit() {
        let dim = 8_192usize;
        let rows = 40_000usize; // f16 packed ≈0.62 GiB（収まる）・f32 原本 ≈1.22 GiB（超過）
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenant_ids: Vec<String> = vec!["tenant-a".to_string(); rows];
        let visibilities = vec![Visibility::Public; rows];
        let vectors = vec![0.0f32; 1]; // 意図的に短い: 容量チェックが長さ検証より先に走ることを確認する
        let observer = Box::new(RecordingObserver::new());
        let result = FallbackBatchEngine::build_with_gpu_reference(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
            observer,
        );
        let err = match result {
            Ok(_) => panic!("expected CapacityExceeded, got Ok"),
            Err(e) => e,
        };
        assert!(matches!(err, BatchSearchError::CapacityExceeded { .. }));
    }

    // Cursor Bugbot 指摘対応（PR #152）: `FallbackBatchEngine::build` の容量検証が
    // f32 ベクトル本体（`f32_bytes`）のみで、`ids`/`tenant_ids`/`visibilities` の
    // 複製分（aux バイト。`CpuFallbackMatrix` が独立に保持する）が計上されて
    // いなかった欠落の回帰テスト。本テストは f32 ベクトル本体だけなら
    // `MAX_BATCH_TOTAL_BYTES`（1 GiB）に収まるが、aux バイトを加算すると
    // 超過する境界（rows=1,000,000 [`MAX_BATCH_ROWS`] ・dim=200: f32_bytes は
    // 約 0.75 GiB・aux_bytes は約 0.27 GiB・合算で約 1.01 GiB）を選び、
    // aux バイトを計上しない実装では見逃されていた `CapacityExceeded` を
    // 拾えることを固定する。
    #[test]
    fn build_rejects_fallback_matrix_capacity_over_limit_due_to_aux_bytes_alone() {
        let dim = 200usize;
        let rows = 1_000_000usize; // MAX_BATCH_ROWS ちょうど
        let ids: Vec<u64> = (0..rows as u64).collect();
        let tenant_ids: Vec<String> = vec!["tenant-a".to_string(); rows];
        let visibilities = vec![Visibility::Public; rows];
        let vectors = vec![0.0f32; 1]; // 意図的に短い: 容量チェックが長さ検証より先に走ることを確認する
        let observer = Box::new(RecordingObserver::new());
        let result = FallbackBatchEngine::build_with_gpu_reference(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
            observer,
        );
        let err = match result {
            Ok(_) => panic!(
                "expected CapacityExceeded once aux bytes are counted, got Ok \
                 (f32 body alone would fit under MAX_BATCH_TOTAL_BYTES)"
            ),
            Err(e) => e,
        };
        assert!(matches!(err, BatchSearchError::CapacityExceeded { .. }));
    }

    // 入力エラー峻別（TASK-129 設計の要）: 構築段階のデータ不整合
    // （`ArenaLengthMismatch`）は `BatchBackendError` 経由の縮退トリガでは
    // なく、`ResidentMatrix::build` の通常の fail-closed エラーとしてそのまま
    // 返る（縮退イベントは発生しない）。
    #[test]
    fn build_propagates_input_errors_without_triggering_fallback() {
        let ids = [1u64, 2];
        let tenant_ids = ["tenant-a".to_string(), "tenant-a".to_string()];
        let visibilities = [Visibility::Public, Visibility::Public];
        let vectors = [1.0f32, 0.0]; // ids.len()=2, dim=2 のはずが要素数が不足
        let observer = Box::new(RecordingObserver::new());
        let result = FallbackBatchEngine::build_with_gpu_reference(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &vectors,
            observer,
        );
        let err = match result {
            Ok(_) => panic!("expected ArenaLengthMismatch, got Ok"),
            Err(e) => e,
        };
        assert_eq!(err, BatchSearchError::ArenaLengthMismatch);
    }

    fn build_engine_with_backend(
        backend_factory: impl FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>, BatchBackendError>,
        observer: Box<dyn FallbackObserver>,
    ) -> FallbackBatchEngine {
        let ids = [1u64, 2, 3, 4];
        let tenant_ids = [
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ];
        let visibilities = [
            Visibility::Public,
            Visibility::Public,
            Visibility::Public,
            Visibility::Public,
        ];
        #[rustfmt::skip]
        let vectors = [
            1.0, 0.0,
            0.0, 1.0,
            2.0, 0.0,
            0.0, 2.0,
        ];
        FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &vectors,
            backend_factory,
            observer,
        )
        .expect("build ok")
    }

    /// [`build_engine_with_backend`] と同じ id・テナント・ベクトル配置だが、全行
    /// `Private` にした版。テナント分離そのものを検証するテストは本フィクスチャを
    /// 使う（ポインタ: TASK-89 / TABLE-9）。
    fn build_engine_with_backend_private(
        backend_factory: impl FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>, BatchBackendError>,
        observer: Box<dyn FallbackObserver>,
    ) -> FallbackBatchEngine {
        let ids = [1u64, 2, 3, 4];
        let tenant_ids = [
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ];
        let visibilities = [
            Visibility::Private,
            Visibility::Private,
            Visibility::Private,
            Visibility::Private,
        ];
        #[rustfmt::skip]
        let vectors = [
            1.0, 0.0,
            0.0, 1.0,
            2.0, 0.0,
            0.0, 2.0,
        ];
        FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &vectors,
            backend_factory,
            observer,
        )
        .expect("build ok")
    }

    /// `tenant-a`・`tenant-b` が同じ `id`（`1`）を共有する `Public` フィクスチャ
    /// （対象ビヘイビア: TABLE-12・RLS-9。PR #205）。行 `id` の一意性スコープが
    /// テナント内に閉じたことで正当となる入力を、`revalidate_primary_hits` が
    /// 誤って拒否しないことの検証に使う。
    fn build_engine_with_backend_shared_id(
        backend_factory: impl FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>, BatchBackendError>,
        observer: Box<dyn FallbackObserver>,
    ) -> FallbackBatchEngine {
        let ids = [1u64, 1];
        let tenant_ids = ["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = [Visibility::Public, Visibility::Public];
        #[rustfmt::skip]
        let vectors = [
            1.0, 0.0,
            1.0, 0.0,
        ];
        FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &vectors,
            backend_factory,
            observer,
        )
        .expect("build ok")
    }

    /// [`build_engine_with_backend_shared_id`] と同じ id・テナント・ベクトル
    /// 配置だが、全行 `Private` にした版（対象ビヘイビア: TABLE-12・RLS-9。
    /// PR #205）。テナント分離そのもの（他テナントの同 `id` 行が混入しない
    /// こと）を検証するテストは本フィクスチャを使う。
    fn build_engine_with_backend_shared_id_private(
        backend_factory: impl FnOnce(ResidentMatrix) -> Result<Box<dyn BatchBackend>, BatchBackendError>,
        observer: Box<dyn FallbackObserver>,
    ) -> FallbackBatchEngine {
        let ids = [1u64, 1];
        let tenant_ids = ["tenant-a".to_string(), "tenant-b".to_string()];
        let visibilities = [Visibility::Private, Visibility::Private];
        #[rustfmt::skip]
        let vectors = [
            1.0, 0.0,
            1.0, 0.0,
        ];
        FallbackBatchEngine::build(
            &ids,
            &tenant_ids,
            &visibilities,
            2,
            &vectors,
            backend_factory,
            observer,
        )
        .expect("build ok")
    }

    /// primary バックエンドの実行時エラーを注入するモック（本体へテスト専用
    /// API を追加せず、`BatchBackend` trait をテストコード側で実装する
    /// エラー注入手段。Issue #137 対応の test-support feature 撤廃方針を踏襲）。
    /// `calls` は呼び出し回数を記録し、`runtime_latched` ラッチ発火後は
    /// primary が再試行されないこと（CORE-8 レビュー起因の要件）を
    /// 呼び出し回数で直接検証できるようにする。
    struct FailingBackend {
        err: BatchBackendError,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FailingBackend {
        fn new(err: BatchBackendError) -> Self {
            Self {
                err,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl BatchBackend for FailingBackend {
        fn batch_search(
            &self,
            _queries: &[BatchQuery<'_>],
        ) -> Result<Vec<BatchHit>, BatchExecError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(BatchExecError::Backend(self.err.clone()))
        }
    }

    /// `FailingBackend` を `Arc` 越しにテストコードと `FallbackBatchEngine`
    /// の両方から共有するための委譲実装（`runtime_error_latches_after_first_
    /// failure_and_stops_retrying_primary` が呼び出し回数をエンジン構築後にも
    /// 検査できるようにするため）。
    impl BatchBackend for std::sync::Arc<FailingBackend> {
        fn batch_search(
            &self,
            queries: &[BatchQuery<'_>],
        ) -> Result<Vec<BatchHit>, BatchExecError> {
            self.as_ref().batch_search(queries)
        }
    }

    /// primary バックエンドが `TenantMaskViolation` 等の入力エラーを返すことを
    /// 模したモック（縮退トリガにならないことの検証用）。
    struct InputErrorBackend(BatchSearchError);

    impl BatchBackend for InputErrorBackend {
        fn batch_search(
            &self,
            _queries: &[BatchQuery<'_>],
        ) -> Result<Vec<BatchHit>, BatchExecError> {
            Err(BatchExecError::Input(self.0.clone()))
        }
    }

    // オラクル: 可視行だけを渡した `kernel::CpuScalarProvider::search` の結果
    // （スコア降順・同点 id 昇順まで完全一致）と縮退後の Top-k を突き合わせる。
    //
    // `batch_search.rs::run_batch_search` は行外側ループの計算量最適化として、
    // 行をその `tenant_id` に一致するクエリ集合に加え、TASK-89（TABLE-9）
    // 対応で他テナントの `Public` 許可クエリからも候補にする
    // （`public_grant_query_indices`）。最終判定は常に
    // `PolicyContext::is_visible` の単一照合パスへ委ねるため、オラクルも
    // 同じ述語（`ctx.is_visible(tenant, Visibility::Public)`）だけで
    // 可視行を絞り込む（テナント一致の事前フィルタは行わない）。
    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205）。候補識別子は行 `id` ではなく
    // 元配列内のスロット index を使う（本体コードと同じ「候補識別子＝スロット
    // 番号」方式）。行 `id` の一意性スコープはテナント内に閉じているため、
    // `id` 値そのものを識別子にすると異なるテナントが同じ `id` を共有する
    // フィクスチャで逆引きが曖昧になる（`CpuScalarProvider::search` が返す
    // `CandidateHit::id` は入力 `ids` 配列の値をそのまま返すため）。
    fn oracle_search(
        ids: &[u64],
        tenant_ids: &[&str],
        vectors_per_row: &[[f32; 2]],
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
    ) -> Vec<crate::kernel::SearchHit> {
        use crate::kernel::{CpuScalarProvider, SearchHit, SearchInput, SearchProvider};
        let mut visible_slots: Vec<u64> = Vec::new();
        let mut visible_vectors = Vec::new();
        for (idx, ((_id, &tenant), row)) in
            ids.iter().zip(tenant_ids).zip(vectors_per_row).enumerate()
        {
            if ctx.is_visible(tenant, Visibility::Public) {
                visible_slots.push(idx as u64);
                visible_vectors.extend_from_slice(row);
            }
        }
        let hits = CpuScalarProvider
            .search(SearchInput {
                ids: &visible_slots,
                vectors: &visible_vectors,
                dim: 2,
                query,
                k,
            })
            .expect("oracle search ok");
        hits.into_iter()
            .map(|hit| {
                let idx = hit.id as usize;
                SearchHit::new(tenant_ids[idx], ids[idx], hit.score)
            })
            .collect()
    }

    // CORE-8: 初期化失敗注入 → 構築は `Ok`（CPU 専用モード）・検索は `Ok`・
    // イベントちょうど 1 件（要因=init・切り替え先=cpu-simd）・Top-k がオラクル
    // 一致。
    #[test]
    fn init_failure_falls_back_to_cpu_with_one_event_and_matches_oracle() {
        let observer = std::sync::Arc::new(RecordingObserver::new());
        let observer_for_engine: Box<dyn FallbackObserver> = {
            struct ArcObserver(std::sync::Arc<RecordingObserver>);
            impl FallbackObserver for ArcObserver {
                fn on_fallback(&self, event: FallbackEvent) {
                    self.0.on_fallback(event);
                }
            }
            Box::new(ArcObserver(observer.clone()))
        };

        let engine = build_engine_with_backend(
            |_matrix| Err(BatchBackendError::InitFailed("no gpu device".to_string())),
            observer_for_engine,
        );

        let events_after_build = observer.events();
        assert_eq!(events_after_build.len(), 1);
        assert_eq!(events_after_build[0].reason, FallbackReason::Init);
        assert_eq!(events_after_build[0].target, "cpu-simd");

        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx_a,
        }];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 1);

        let ids = [1u64, 2, 3, 4];
        let tenant_ids = ["tenant-a", "tenant-a", "tenant-b", "tenant-b"];
        let vectors_per_row = [[1.0, 0.0], [0.0, 1.0], [2.0, 0.0], [0.0, 2.0]];
        let expected = oracle_search(&ids, &tenant_ids, &vectors_per_row, &ctx_a, &query, 2);
        assert_eq!(hits[0].hits, expected);

        // 検索を重ねても追加の縮退イベントは発生しない（構築時の 1 件のみ）。
        engine.batch_search(&queries).expect("search ok");
        assert_eq!(observer.events().len(), 1);
    }

    // CORE-8 レビュー起因の回帰テスト: `backend_factory`（初期化）が Runtime
    // 分類のエラー（実 GPU 接続時のデバイス確保中の転送失敗等）を返しても
    // panic せず、[`FallbackEvent::reason`] が実際の分類（Runtime）を反映する
    // ことを検証する（誤って `FallbackReason::Init` にハードコードしない）。
    #[test]
    fn init_time_runtime_classified_error_does_not_panic_and_reports_runtime_reason() {
        let observer = std::sync::Arc::new(RecordingObserver::new());
        let observer_for_engine: Box<dyn FallbackObserver> = {
            struct ArcObserver(std::sync::Arc<RecordingObserver>);
            impl FallbackObserver for ArcObserver {
                fn on_fallback(&self, event: FallbackEvent) {
                    self.0.on_fallback(event);
                }
            }
            Box::new(ArcObserver(observer.clone()))
        };

        let engine = build_engine_with_backend(
            |_matrix| {
                Err(BatchBackendError::TransferFailed(
                    "device probe transfer".to_string(),
                ))
            },
            observer_for_engine,
        );

        let events_after_build = observer.events();
        assert_eq!(events_after_build.len(), 1);
        assert_eq!(events_after_build[0].reason, FallbackReason::Runtime);
        assert_eq!(events_after_build[0].target, "cpu-simd");

        // CPU 縮退経路として引き続き検索は成功する。
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx_a,
        }];
        engine.batch_search(&queries).expect("search ok");
    }

    // CORE-8: 実行時エラー注入（デバイスロスト・カーネル起動失敗・転送失敗の
    // 各種別）→ 構築は primary 利用可能のまま `Ok`・検索は `Ok`・イベント要因が
    // Runtime・Top-k がオラクル一致。
    #[test]
    fn runtime_errors_fall_back_to_cpu_and_match_oracle() {
        for backend_err in [
            BatchBackendError::DeviceLost("lost".to_string()),
            BatchBackendError::KernelLaunchFailed("launch".to_string()),
            BatchBackendError::TransferFailed("transfer".to_string()),
        ] {
            let observer = RecordingObserver::new();
            let engine = build_engine_with_backend(
                |_matrix| {
                    Ok(Box::new(FailingBackend::new(backend_err.clone())) as Box<dyn BatchBackend>)
                },
                Box::new(observer),
            );

            let ctx_b = ctx("tenant-b");
            let query = [1.0f32, 0.0];
            let queries = vec![BatchQuery {
                vector: &query,
                k: 2,
                ctx: &ctx_b,
            }];
            let hits = engine.batch_search(&queries).expect("search ok");

            let ids = [1u64, 2, 3, 4];
            let tenant_ids = ["tenant-a", "tenant-a", "tenant-b", "tenant-b"];
            let vectors_per_row = [[1.0, 0.0], [0.0, 1.0], [2.0, 0.0], [0.0, 2.0]];
            let expected = oracle_search(&ids, &tenant_ids, &vectors_per_row, &ctx_b, &query, 2);
            assert_eq!(hits[0].hits, expected);
        }
    }

    // 実行時エラーが発生した回だけ、要因種別が正しくイベントへ反映されることを
    // 確認する（`FallbackReason::Runtime` に丸められるが、元のエラー種別に
    // 依らず一貫して Runtime に分類されることが本テストの主眼）。
    #[test]
    fn runtime_error_event_reason_is_runtime() {
        let observer = std::sync::Arc::new(RecordingObserver::new());
        struct ArcObserver(std::sync::Arc<RecordingObserver>);
        impl FallbackObserver for ArcObserver {
            fn on_fallback(&self, event: FallbackEvent) {
                self.0.on_fallback(event);
            }
        }
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(FailingBackend::new(BatchBackendError::DeviceLost(
                    "lost".to_string(),
                ))) as Box<dyn BatchBackend>)
            },
            Box::new(ArcObserver(observer.clone())),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 1,
            ctx: &ctx_a,
        }];
        engine.batch_search(&queries).expect("search ok");
        let events = observer.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, FallbackReason::Runtime);
        assert_eq!(events[0].target, "cpu-simd");
    }

    // セキュリティ観点（security.md「不安全な設計」対応）: primary が入力エラー
    // （`TenantMaskViolation` を含む `BatchSearchError`）を返した場合は縮退させず
    // `Err` をそのまま返す。縮退で不正入力・整合性違反を黙殺しない、という
    // 本モジュールの設計の要を検証する。
    #[test]
    fn input_errors_from_primary_are_not_treated_as_fallback_trigger() {
        let observer = std::sync::Arc::new(RecordingObserver::new());
        struct ArcObserver(std::sync::Arc<RecordingObserver>);
        impl FallbackObserver for ArcObserver {
            fn on_fallback(&self, event: FallbackEvent) {
                self.0.on_fallback(event);
            }
        }
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(
                    Box::new(InputErrorBackend(BatchSearchError::TenantMaskViolation))
                        as Box<dyn BatchBackend>,
                )
            },
            Box::new(ArcObserver(observer.clone())),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::TenantMaskViolation);
        // 入力エラーは縮退トリガではないため、イベントは 1 件も発生しない。
        assert!(observer.events().is_empty());
    }

    // マルチテナントバッチで縮退後も他テナント行の混入 0 件であること・
    // クエリごとに独立した結果順序が保たれることを検証する。`Private`
    // フィクスチャで検証する（ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn fallback_search_does_not_leak_rows_across_tenants_in_multi_tenant_batch() {
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend_private(
            |_matrix| {
                Ok(Box::new(FailingBackend::new(BatchBackendError::DeviceLost(
                    "lost".to_string(),
                ))) as Box<dyn BatchBackend>)
            },
            Box::new(observer),
        );
        let ctx_a = private_ctx("tenant-a");
        let ctx_b = private_ctx("tenant-b");
        let query_a = [1.0f32, 1.0];
        let query_b = [1.0f32, 1.0];
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 4,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_b,
                k: 4,
                ctx: &ctx_b,
            },
        ];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 2);
        // tenant-a の行 id は {1, 2}、tenant-b の行 id は {3, 4}。
        for hit in &hits[0].hits {
            assert!(
                hit.id == 1 || hit.id == 2,
                "tenant-a leaked row id={}",
                hit.id
            );
        }
        for hit in &hits[1].hits {
            assert!(
                hit.id == 3 || hit.id == 4,
                "tenant-b leaked row id={}",
                hit.id
            );
        }
    }

    // 対象ビヘイビア: TABLE-12・RLS-9・TABLE-9（PR #205）。CPU 縮退経路
    // （`run_batch_search` を `self.cpu` に対して直接通す経路）でも、異なる
    // テナントが同じ `id` を持つ `Public` フィクスチャに対し、両テナントの
    // 行がそれぞれ別ヒットとして返り（TABLE-9 の相互可視性）、各ヒットの
    // `tenant_id` が正しく行の所属を示すことを確認する（primary 経路のみの
    // 回帰にしない）。テナント分離そのもの（他テナントの同 `id` 行が
    // 混入しないこと）は `Private` フィクスチャを使う
    // `fallback_search_isolates_rows_when_tenants_share_id` で検証する。
    #[test]
    fn fallback_search_returns_both_tenants_public_rows_sharing_id_via_cpu_fallback() {
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend_shared_id(
            |_matrix| {
                Ok(Box::new(FailingBackend::new(BatchBackendError::DeviceLost(
                    "lost".to_string(),
                ))) as Box<dyn BatchBackend>)
            },
            Box::new(observer),
        );
        let ctx_a = ctx("tenant-a");
        let ctx_b = ctx("tenant-b");
        let query = [1.0f32, 0.0];
        let queries = vec![
            BatchQuery {
                vector: &query,
                k: 4,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query,
                k: 4,
                ctx: &ctx_b,
            },
        ];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].hits.len(), 2, "id=1 shared by both tenants (Public) must both return for tenant-a's is_visible-permissive ctx");
        assert_eq!(hits[1].hits.len(), 2);
        for hit in &hits[0].hits {
            assert_eq!(hit.id, 1);
        }
        for hit in &hits[1].hits {
            assert_eq!(hit.id, 1);
        }
        let tenants_a: std::collections::HashSet<&str> =
            hits[0].hits.iter().map(|h| h.tenant_id.as_str()).collect();
        let tenants_b: std::collections::HashSet<&str> =
            hits[1].hits.iter().map(|h| h.tenant_id.as_str()).collect();
        assert!(tenants_a.contains("tenant-a") && tenants_a.contains("tenant-b"));
        assert!(tenants_b.contains("tenant-a") && tenants_b.contains("tenant-b"));
    }

    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205 §1 目的 3 のコア）。CPU 縮退
    // 経路で、異なるテナントが同じ `id`（`1`）を持つ `Private` フィクスチャに
    // 対し、各テナントのクエリが自テナントの行だけを受け取り、他テナントの
    // 同 `id` 行が一切混入しないことを確認する（primary 経路の同種テストは
    // `batch_search.rs::batch_search_returns_only_own_row_when_other_tenant_
    // shares_id`）。
    #[test]
    fn fallback_search_isolates_rows_when_tenants_share_id() {
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend_shared_id_private(
            |_matrix| {
                Ok(Box::new(FailingBackend::new(BatchBackendError::DeviceLost(
                    "lost".to_string(),
                ))) as Box<dyn BatchBackend>)
            },
            Box::new(observer),
        );
        let ctx_a = private_ctx("tenant-a");
        let ctx_b = private_ctx("tenant-b");
        let query = [1.0f32, 0.0];
        let queries = vec![
            BatchQuery {
                vector: &query,
                k: 4,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query,
                k: 4,
                ctx: &ctx_b,
            },
        ];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].hits.len(), 1);
        assert_eq!(hits[0].hits[0].tenant_id, "tenant-a");
        assert_eq!(hits[0].hits[0].id, 1);
        assert_eq!(hits[1].hits.len(), 1);
        assert_eq!(hits[1].hits[0].tenant_id, "tenant-b");
        assert_eq!(hits[1].hits[0].id, 1);
    }

    // 正常時（primary 成功）はイベント 0 件・縮退再実行なしであることを確認する。
    #[test]
    fn successful_primary_search_emits_no_fallback_event() {
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend(
            |matrix| Ok(Box::new(GpuReferenceBackend::new(matrix)) as Box<dyn BatchBackend>),
            Box::new(observer),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 1,
            ctx: &ctx_a,
        }];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 1);
    }

    // 決定性: 同一入力の再実行が同一結果を返すことを確認する（1 回目で
    // `runtime_latched` がラッチされた後の 2 回目以降も、CPU 縮退経路が内部
    // 状態を持ち越さず結果が揺れないこと）。
    #[test]
    fn fallback_search_is_deterministic_across_repeated_calls() {
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(
                    Box::new(FailingBackend::new(BatchBackendError::TransferFailed(
                        "transfer".to_string(),
                    ))) as Box<dyn BatchBackend>,
                )
            },
            Box::new(observer),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 1.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx_a,
        }];
        let first = engine.batch_search(&queries).expect("search ok");
        let second = engine.batch_search(&queries).expect("search ok");
        assert_eq!(first[0].hits, second[0].hits);
    }

    // CORE-8 レビュー起因の回帰テスト: 実行時エラーによる恒久故障ラッチの
    // 検証。primary が呼び出しのたびに実行時エラーを返す状況で
    // `batch_search` を複数回呼んでも、primary の呼び出し回数はラッチ発火の
    // 1 回に留まり（無駄な再試行コストを防ぐ）、observer への通知も 1 件に
    // 留まる（無制限の stderr 出力を防ぐ）ことを、`FailingBackend::call_count`
    // で直接検証する（イベント件数だけでは primary 再試行の有無を証明できない
    // ため）。
    #[test]
    fn runtime_error_latches_after_first_failure_and_stops_retrying_primary() {
        let backend = std::sync::Arc::new(FailingBackend::new(BatchBackendError::DeviceLost(
            "lost".to_string(),
        )));

        // `Box<dyn BatchBackend>` として engine へ所有権を渡しつつ、テスト側で
        // 呼び出し回数を検査できるよう `Arc` 越しに共有する
        // （`impl BatchBackend for Arc<FailingBackend>` を下で定義）。
        let backend_for_engine = backend.clone();
        let observer = RecordingObserver::new();
        let engine = build_engine_with_backend(
            move |_matrix| Ok(Box::new(backend_for_engine) as Box<dyn BatchBackend>),
            Box::new(observer),
        );

        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 1,
            ctx: &ctx_a,
        }];

        for _ in 0..5 {
            engine
                .batch_search(&queries)
                .expect("search ok via cpu fallback");
        }

        assert_eq!(
            backend.call_count(),
            1,
            "primary must be retried only once before the runtime latch engages"
        );
    }

    // --- codex-review P0 指摘対応（PR #152）: primary バックエンド成功結果の
    // 独立再検証（`revalidate_primary_hits`）の回帰テスト群。
    //
    // `BatchBackend` は公開差し替え点であり、将来の実 GPU/外部実装が任意の
    // `Vec<BatchHit>` を返しうる。以下のテストは `Ok` を返しつつ結果契約に
    // 違反する「悪性バックエンド」を模したモックを使い、成功結果であっても
    // 無条件で信頼せず拒否されることを確認する。

    /// 任意の `Vec<BatchHit>` を返す悪性バックエンドのモック（クエリ内容を
    /// 無視し、クロージャで用意した結果をそのまま返す。`BatchHit`/`CandidateHit`
    /// は `Clone` ではないため、呼び出しごとに結果を構築するクロージャとして
    /// 保持する）。
    struct MaliciousBackend<F: Fn() -> Vec<BatchHit> + Send + Sync> {
        make_hits: F,
    }

    impl<F: Fn() -> Vec<BatchHit> + Send + Sync> BatchBackend for MaliciousBackend<F> {
        fn batch_search(
            &self,
            _queries: &[BatchQuery<'_>],
        ) -> Result<Vec<BatchHit>, BatchExecError> {
            Ok((self.make_hits)())
        }
    }

    // 捏造 id（常駐行列に存在しない id）を返す悪性バックエンドは拒否される。
    #[test]
    fn revalidation_rejects_fabricated_id_not_in_matrix() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![crate::kernel::SearchHit::new("tenant-a", 9_999, 1.0)],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::TenantMaskViolation);
    }

    // 他テナントの実在 id（tenant-b の行 id=3）を tenant-a のクエリへ混入させる
    // 悪性バックエンドは拒否される（本指摘のコア: マスク漏れ・結果破損による
    // 他テナントの存在情報漏えいを防ぐ）。id=3 が引き続き tenant-a から不可視で
    // あることを保つ `Private` フィクスチャで検証する（ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn revalidation_rejects_other_tenant_id() {
        let engine = build_engine_with_backend_private(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![crate::kernel::SearchHit::new("tenant-b", 3, 1.0)],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = private_ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::TenantMaskViolation);
    }

    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205）。実在する `id`（tenant-b の
    // 行 id=3）に、実際には別テナントの `tenant_id`（tenant-a）を付けて
    // 詐称する悪性バックエンドは拒否される。行 `id` の一意性スコープが
    // テナント内に閉じたことで「id さえ実在すればよい」検証では防げない
    // 新しい偽装経路であり、`(tenant_id, id)` の組で信頼済み常駐行列と
    // 突き合わせることで拒否できることを確認する。
    #[test]
    fn revalidation_rejects_hit_with_mislabeled_tenant() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![crate::kernel::SearchHit::new("tenant-a", 3, 1.0)],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::TenantMaskViolation);
    }

    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205）。同一クエリ内で同じ
    // `(tenant_id, id)` の組を複数回返す悪性バックエンドは拒否される
    // （`revalidation_rejects_duplicate_id_within_one_query` の
    // `(tenant_id, id)` 版。スコアを変えても重複判定はキーのみで行う）。
    #[test]
    fn revalidation_rejects_duplicate_row_key() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![
                                crate::kernel::SearchHit::new("tenant-a", 1, 2.0),
                                crate::kernel::SearchHit::new("tenant-a", 1, 1.0),
                            ],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::PrimaryResultRejected);
    }

    // 対象ビヘイビア: TABLE-12・RLS-9（PR #205）。異なるテナントが同じ `id`
    // を持つ `Public` フィクスチャに対し、両テナントを正しくラベル付けした
    // 結果を返すバックエンドは独立再検証を通過する（正当な入力を誤って
    // `DuplicateRowId`/`TenantMaskViolation` 扱いしないことの回帰）。
    #[test]
    fn revalidation_accepts_same_id_across_tenants_when_both_visible() {
        let engine = build_engine_with_backend_shared_id(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![
                                crate::kernel::SearchHit::new("tenant-a", 1, 1.0),
                                crate::kernel::SearchHit::new("tenant-b", 1, 1.0),
                            ],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        // `Public` 行を両方見るため、許可集合に `Public` を含むクエリで検索する
        // （`PolicyContext::new` の既定は `Public` のみ許可）。
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let hits = engine.batch_search(&queries).expect("search ok");
        let returned: std::collections::HashSet<(&str, u64)> = hits[0]
            .hits
            .iter()
            .map(|h| (h.tenant_id.as_str(), h.id))
            .collect();
        assert!(returned.contains(&("tenant-a", 1)));
        assert!(returned.contains(&("tenant-b", 1)));
    }

    // クエリの `k` を超える件数を返す悪性バックエンドは拒否される（id 自体は
    // 有効なテナント内 id でも件数上限違反として弾く）。
    #[test]
    fn revalidation_rejects_hit_count_over_k() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![
                                crate::kernel::SearchHit::new("tenant-a", 1, 2.0),
                                crate::kernel::SearchHit::new("tenant-a", 2, 1.0),
                            ],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 1,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::PrimaryResultRejected);
    }

    // 結果件数（`Vec<BatchHit>` の長さ）がクエリ数（1 件）より多い悪性
    // バックエンドは拒否される（`zip` による暗黙の切り詰めで過剰分の検証を
    // すり抜けないことの回帰）。
    #[test]
    fn revalidation_rejects_result_count_more_than_queries() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || vec![BatchHit { hits: vec![] }, BatchHit { hits: vec![] }],
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::PrimaryResultRejected);
    }

    // 結果件数がクエリ数（1 件）より少ない（0 件）悪性バックエンドも同様に
    // 拒否される。
    #[test]
    fn revalidation_rejects_result_count_fewer_than_queries() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: Vec::new,
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::PrimaryResultRejected);
    }

    // 同一クエリ内で id が重複する悪性バックエンドは拒否される。
    #[test]
    fn revalidation_rejects_duplicate_id_within_one_query() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![
                                crate::kernel::SearchHit::new("tenant-a", 1, 2.0),
                                crate::kernel::SearchHit::new("tenant-a", 1, 1.0),
                            ],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::PrimaryResultRejected);
    }

    // 非有限スコア（NaN）を返す悪性バックエンドは拒否される。
    #[test]
    fn revalidation_rejects_non_finite_score() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![crate::kernel::SearchHit::new("tenant-a", 1, f32::NAN)],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::PrimaryResultRejected);
    }

    // スコア降順・同点 id 昇順の契約に違反する悪性バックエンドは拒否される。
    #[test]
    fn revalidation_rejects_out_of_order_hits() {
        let engine = build_engine_with_backend(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![
                                // 昇順（本来は降順であるべき）で返す違反。
                                crate::kernel::SearchHit::new("tenant-a", 1, 1.0),
                                crate::kernel::SearchHit::new("tenant-a", 2, 2.0),
                            ],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];
        let err = engine.batch_search(&queries).unwrap_err();
        assert_eq!(err, BatchSearchError::PrimaryResultRejected);
    }

    // ポジティブコントロール: 正当な `GpuReferenceBackend`（`run_batch_search` を
    // 内部で通る）の結果は独立再検証を通過し、オラクル（可視行だけを渡した
    // `CpuScalarProvider::search`）と完全一致する。これがないと、
    // 「常に拒否する」誤った実装でも上の否定的テスト群がすべて通ってしまう。
    #[test]
    fn revalidation_accepts_legitimate_gpu_reference_backend_and_matches_oracle() {
        let engine = build_engine_with_backend(
            |matrix| Ok(Box::new(GpuReferenceBackend::new(matrix)) as Box<dyn BatchBackend>),
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 2,
            ctx: &ctx_a,
        }];
        let hits = engine.batch_search(&queries).expect("search ok");

        let ids = [1u64, 2, 3, 4];
        let tenant_ids = ["tenant-a", "tenant-a", "tenant-b", "tenant-b"];
        let vectors_per_row = [[1.0, 0.0], [0.0, 1.0], [2.0, 0.0], [0.0, 2.0]];
        let expected = oracle_search(&ids, &tenant_ids, &vectors_per_row, &ctx_a, &query, 2);
        assert_eq!(hits[0].hits, expected);
    }

    // ポジティブコントロール（複数クエリ・複数テナント版）: 単一クエリだけの
    // 上のテストでは `revalidate_primary_hits` 内部のクエリ別 `zip`・
    // クエリごとの `seen_ids` リセット・複数テナントにまたがる
    // `batch_tenants`/`id_to_tenant` 構築を検証できない（`FailingBackend` 経由の
    // 既存テストは CPU 縮退経路を通るため `revalidate_primary_hits` 自体を
    // 経由しない）。2 クエリ・2 テナントで primary（`GpuReferenceBackend`）を
    // 実行し、各クエリの結果がそれぞれのオラクルと一致することを確認する。
    #[test]
    fn revalidation_accepts_legitimate_multi_query_multi_tenant_batch_and_matches_oracle() {
        let engine = build_engine_with_backend(
            |matrix| Ok(Box::new(GpuReferenceBackend::new(matrix)) as Box<dyn BatchBackend>),
            Box::new(RecordingObserver::new()),
        );
        let ctx_a = ctx("tenant-a");
        let ctx_b = ctx("tenant-b");
        let query_a = [1.0f32, 1.0];
        let query_b = [1.0f32, 1.0];
        let queries = vec![
            BatchQuery {
                vector: &query_a,
                k: 2,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &query_b,
                k: 2,
                ctx: &ctx_b,
            },
        ];
        let hits = engine.batch_search(&queries).expect("search ok");
        assert_eq!(hits.len(), 2);

        let ids = [1u64, 2, 3, 4];
        let tenant_ids = ["tenant-a", "tenant-a", "tenant-b", "tenant-b"];
        let vectors_per_row = [[1.0, 0.0], [0.0, 1.0], [2.0, 0.0], [0.0, 2.0]];
        let expected_a = oracle_search(&ids, &tenant_ids, &vectors_per_row, &ctx_a, &query_a, 2);
        let expected_b = oracle_search(&ids, &tenant_ids, &vectors_per_row, &ctx_b, &query_b, 2);
        assert_eq!(hits[0].hits, expected_a);
        assert_eq!(hits[1].hits, expected_b);
    }

    // ラッチしない・縮退イベントを発生させないことの回帰: 違反検知後の
    // 2 回目以降の呼び出しも `Err` を返し続け（CPU 縮退へ静かに切り替わらない）、
    // observer への通知も一切発生しない（違反はデバイス恒久故障とは異なる
    // 種類の異常であり、`runtime_latched` を流用すると 2 回目以降が `Ok` を
    // 返すようになり検知結果が消えてしまうため。`revalidate_primary_hits` の
    // ドキュメンテーションコメント「ラッチ決定」参照）。id=3 が引き続き tenant-a
    // から不可視であることを保つ `Private` フィクスチャで検証する
    // （`revalidation_rejects_other_tenant_id` と同じ方針。ポインタ:
    // TASK-89 / TABLE-9）。
    #[test]
    fn revalidation_violation_does_not_latch_and_emits_no_fallback_event() {
        let observer = std::sync::Arc::new(RecordingObserver::new());
        struct ArcObserver(std::sync::Arc<RecordingObserver>);
        impl FallbackObserver for ArcObserver {
            fn on_fallback(&self, event: FallbackEvent) {
                self.0.on_fallback(event);
            }
        }
        let engine = build_engine_with_backend_private(
            |_matrix| {
                Ok(Box::new(MaliciousBackend {
                    make_hits: || {
                        vec![BatchHit {
                            hits: vec![crate::kernel::SearchHit::new("tenant-b", 3, 1.0)],
                        }]
                    },
                }) as Box<dyn BatchBackend>)
            },
            Box::new(ArcObserver(observer.clone())),
        );
        let ctx_a = private_ctx("tenant-a");
        let query = [1.0f32, 0.0];
        let queries = vec![BatchQuery {
            vector: &query,
            k: 4,
            ctx: &ctx_a,
        }];

        for _ in 0..3 {
            let err = engine.batch_search(&queries).unwrap_err();
            assert_eq!(err, BatchSearchError::TenantMaskViolation);
        }
        assert!(
            observer.events().is_empty(),
            "a result-contract violation must not be treated as a fallback trigger"
        );
    }
}
