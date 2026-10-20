//! 検索カーネルの実行バックエンド provider 層（TASK-124・対象ビヘイビア: CORE-13）。
//!
//! `core.rs` の [`crate::core::EngineCore`] は具象バックエンド型（CPU 並列・GPU・将来
//! ANN）へ直接依存せず、本モジュールが定義する object-safe な [`SearchProvider`] trait
//! 経由で実行バックエンドを注入される。本モジュールはスカラー参照実装
//! [`CpuScalarProvider`] と、Top-k 選出の共通ヘルパ [`TopKSelector`] を提供する。
//! `TopKSelector` は `crates/engine/src/parallel_search.rs::ParallelSearchProvider`（TASK-126）
//! とも共用し、選出規約（スコア降順・同点 id 昇順・非有限値除外）の二重管理を防ぐ。
//! 既定コンストラクタが実際にどちらの provider を注入するかは `search_engine.rs`
//! （TASK-131・CORE-9 の差し替え点確定化レイヤ）経由で `core.rs::EngineCore::open` を参照。
//!
//! 経路選択の外部上書き機構（環境変数・設定フラグ等）は設けない（CORE-12）。
//! 実行経路（CPU-SIMD／GPU）自体の決定表は `dispatch.rs::select_execution_path`
//! （TASK-155・CORE-11, 12）に集約されており、本モジュールはあくまで provider
//! trait の窓口を提供する。

use std::fmt;

use crate::search_order;

/// **候補**検索結果 1 件（候補識別子とスコア）。[`SearchProvider`] の戻り値型。
///
/// `id` は行 `id` とは限らない「呼び出し元が定義した候補識別子」で、[`SearchInput::ids`]
/// に渡された値がそのまま返る（詳細は同フィールドのドキュメント参照）。行 `id` の
/// 一意性スコープはテナント内に閉じている（対象ビヘイビア: TABLE-12）ため、
/// 呼び出し元（`core.rs`・`sql/exec.rs`）は候補集合内で一意なスロット番号を渡し、
/// 行の同定（テナント・行 id への解決）は provider の外側で行う。
///
/// 公開 API（[`crate::core::VectorCore::search`]・`rls.rs` の各 `search`）が返すのは
/// テナント修飾済みの [`SearchHit`] であり、本型は provider 境界のみで使う。
/// `Copy` を維持するのは、`TopKSelector::push` が候補行ごと（最大で可視行数分＝
/// 検索 1 回あたり最大 `arena::MAX_ARENA_ROWS` 回）呼ばれるホットパスであり、
/// ヒープ確保を伴うフィールドを持たせられないため。
///
/// スコアは内積（dot product）で定義する（`arena.rs` の既存テストが検証基準に
/// 使っている尺度と揃える）。値が大きいほど上位。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateHit {
    pub id: u64,
    pub score: f32,
}

/// 検索結果 1 件（**行を一意に解決できる**テナント修飾済みのヒット）。
///
/// 対象ビヘイビア: TABLE-12・RLS-9（ポインタ: `docs/spec/04-behavior/data-model.md`
/// TABLE-12・`rls.md` RLS-9）。行 `id` の一意性スコープはテナント内に閉じているため、
/// `id` 単独では行を一意に指せない（自テナント行と他テナントの `Public` 行が同じ `id`
/// を持ちうる）。本型は所有テナントを併せて公開し、`(tenant_id, id)` で
/// [`crate::core::VectorCore::get_row`] から実際の行へ解決できる契約とする
/// （codex-review P1 指摘・PR #194 対応）。
///
/// `tenant_id` は「そのヒットの行が属するテナント」であり、検索した
/// `PolicyContext` のテナントとは限らない（他テナントの `Public` 行が可視な場合）。
/// 本型が生成されるのは呼び出し元が可視性判定（`PolicyContext::is_visible`）を
/// 通した行だけであり、不可視行のテナント名が本型経由で漏れることはない。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// ヒットした行が属するテナント（サーバー側で保持している行の帰属）。
    pub tenant_id: String,
    /// ヒットした行の `id`（テナント内で一意。TABLE-12）。
    pub id: u64,
    /// 内積スコア。値が大きいほど上位。
    pub score: f32,
}

impl SearchHit {
    /// テナント修飾済みヒットを構築する（呼び出し元は候補スロット →
    /// `(tenant_id, id)` の解決を済ませてから呼ぶ）。
    pub fn new(tenant_id: impl Into<String>, id: u64, score: f32) -> Self {
        SearchHit {
            tenant_id: tenant_id.into(),
            id,
            score,
        }
    }
}

/// [`SearchProvider::search`] が返すエラー。
#[derive(Debug, Clone, PartialEq)]
pub enum KernelError {
    /// クエリベクトルの次元がアリーナの次元と一致しない。
    DimMismatch { expected: u32, found: usize },
    /// クエリベクトルの要素に非有限値（NaN・Inf）が含まれる。呼び出し元（wire 経路）からの
    /// untrusted 入力のため、`total_cmp` の順序に頼らず明示的に拒否する
    /// （coding-rust.md「untrusted 入力の扱い」対応。fail-closed）。
    NonFiniteQuery,
    /// `parallel_search.rs::ParallelSearchProvider` の並列ワーカースレッドが panic した。
    /// 部分結果を欠いたまま `Ok` を返すと該当パーティションの行が黙って選出対象から
    /// 消える（実質 fail-open）ため、検索全体を失敗として呼び出し元へ伝播させる
    /// （Issue #34 レビュー指摘対応）。
    WorkerPanicked,
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::DimMismatch { expected, found } => write!(
                f,
                "kernel query dim mismatch: expected={expected} found={found}"
            ),
            KernelError::NonFiniteQuery => write!(f, "kernel query contains non-finite value"),
            KernelError::WorkerPanicked => {
                write!(f, "kernel search worker thread panicked")
            }
        }
    }
}

impl std::error::Error for KernelError {}

/// 実行バックエンドの入力ビュー。`core.rs` がアリーナのカラムナ表現から、呼び出し元
/// `PolicyContext` の下で可視な行だけを抽出した縮約ビューとして組み立てる（codex P0
/// 指摘・Issue #137 対応）。provider 側は所有権を持たず、呼び出し中だけ borrow する
/// （object-safe を保つためジェネリクスを持たない参照渡しに統一する）。
///
/// 可視性判定はここへ渡す前に `core.rs` 側で完結しており、本構造体には不可視行の
/// id・ベクトルは一切含まれない。以前のバージョンはアリーナ全行を渡し、provider が
/// 呼び出す「規約」の `is_visible` クロージャでマスクする設計だったが、provider が
/// クロージャを無視すれば不可視行のベクトル・id を読み取れてしまう構造的な問題が
/// あった（他テナントのデータをそもそも provider のアドレス空間へ渡さない、という
/// より強い境界に変更した）。
pub struct SearchInput<'a> {
    /// `vectors` の行と 1 対 1 に対応する識別子。**呼び出し元が定義する識別子**であり、
    /// 行 `id` とは限らない（provider は値の意味を解釈せず、そのまま
    /// [`CandidateHit::id`] として返す）。行 `id` の一意性スコープはテナント内に閉じている
    /// （対象ビヘイビア: TABLE-12）ため、同一 `id` の可視行が複数含まれうる文脈では
    /// 呼び出し元が一意な識別子を渡す責務を負う: SQL 表層（`sql::exec`）は候補アリーナの
    /// スロット番号を渡し（投影・RRF 融合・疎コーパスの結合キーを一意にするため）、
    /// `core::EngineCore::search` は行 `id` をそのまま渡す（結果を id で返す契約のため。
    /// 重複しうることは `VectorCore::search` のドキュメント参照）。
    pub ids: &'a [u64],
    /// `ids.len() * dim` 要素のフラット化済みベクトル（行 i の embedding は
    /// `vectors[i * dim .. (i + 1) * dim]`）。可視行のみを含む（上記構造体ドキュメント
    /// 参照）。
    pub vectors: &'a [f32],
    pub dim: u32,
    pub query: &'a [f32],
    pub k: usize,
}

/// [`SearchProvider::search_subset`] の入力ビュー（Issue #654）。
///
/// `SqlArenaCache`（Issue #363）がヒットしたクエリで `sql::scalar_index::ScalarIndex`
/// （Issue #473・#474）が候補行を絞り込んだ場合、従来は候補行を新規 `VectorArena` へ
/// 複製（`arena.rs::build_from_cached_rls_rows_subset`）してから [`SearchProvider::search`]
/// へ渡していた。本型は複製せずキャッシュ済みスナップショットの行列全体
/// （[`Self::vectors`]）を借用したまま、[`Self::slots`] が指す行だけを候補にして
/// 探索するための入力を表す。
///
/// [`SearchInput`] を再利用せず専用型にしているのは、`SearchInput::ids.len() * dim ==
/// SearchInput::vectors.len()` という既存契約を崩さないため。本型は `ids` を持たず、
/// 返る [`CandidateHit::id`] は常に `slots[i] as u64`（呼び出し元がスロット番号として
/// 解釈する。`sql::exec` の「provider へ渡す id はアリーナのスロット番号」という
/// 既存契約と同型）に固定される。
pub struct SubsetSearchInput<'a> {
    /// `vectors` の行番号（`0..rows`）。呼び出し元契約として狭義昇順・重複なし
    /// （`sql::scalar_index::ScalarIndex::resolve_candidates` の戻り値と同じ形状）。
    /// provider 側はこの契約を検証しない代わりに、範囲外の行番号は
    /// [`CpuScalarProvider::search_subset`]・既定実装のいずれも黙って skip する
    /// （`SearchInput` の `ids`/`vectors` 不整合行と同じ縮退規約。呼び出し規約違反への
    /// 多層防御であり、正常系のオーバーヘッドにはならない）。
    pub slots: &'a [u32],
    /// `rows * dim` 要素のフラット行列（可視行のみを含む。[`SearchInput::vectors`] と
    /// 同じレイアウト）。
    pub vectors: &'a [f32],
    pub dim: u32,
    pub query: &'a [f32],
    pub k: usize,
}

/// コアが依存する検索バックエンドの窓口（CORE-13）。object-safe（ジェネリクスなし・
/// `&self` メソッドのみ）を維持し、`Box<dyn SearchProvider>` として `core.rs` に
/// 保持されることを前提とする。
pub trait SearchProvider: Send + Sync {
    /// `input` に含まれる行（呼び出し元があらかじめ可視行だけへ絞り込み済み）から
    /// 総当たり Top-k 検索を行う。
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError>;

    /// `input.vectors`（行列全体）を複製せず借用したまま、`input.slots` が指す行
    /// だけを候補にした総当たり Top-k 検索を行う（Issue #654）。
    ///
    /// 既定実装は `slots` の行を一時バッファへ gather してから [`Self::search`] へ
    /// 委譲する（従来の「候補行を複製してから `search`」経路と同じ計算・同じ Top-k
    /// 選出規約になる）。この既定実装があるため、本メソッド追加は既存のカスタム
    /// `SearchProvider` 実装を無変更のままコンパイル・同一結果に保つ（trait への
    /// メソッド追加という公開 API 変更の破壊的影響を打ち消す）。`vectors` の複製を
    /// 避ける最適化そのものは [`CpuScalarProvider::search_subset`]・
    /// `parallel_search.rs::ParallelSearchProvider::search_subset`（Issue #654）が
    /// オーバーライドで提供する。
    fn search_subset(
        &self,
        input: SubsetSearchInput<'_>,
    ) -> Result<Vec<CandidateHit>, KernelError> {
        let dim = input.dim as usize;
        if input.query.len() != dim {
            return Err(KernelError::DimMismatch {
                expected: input.dim,
                found: input.query.len(),
            });
        }
        if input.query.iter().any(|v| !v.is_finite()) {
            return Err(KernelError::NonFiniteQuery);
        }
        if input.k == 0 || input.slots.is_empty() {
            return Ok(Vec::new());
        }
        // untrusted 経路由来ではない（`slots.len()` は呼び出し元〔`sql::exec`〕が
        // 候補削減で決める値だが、`MAX_ARENA_ROWS` で上限が掛かっている）が、
        // 無制限確保を避けるため `try_reserve_exact` で明示的に処理する
        // （.claude/rules/coding-rust.md「untrusted 入力の扱い」と同じ流儀を
        // ここでも踏襲する）。
        let gather_len = input.slots.len().saturating_mul(dim);
        let mut gathered: Vec<f32> = Vec::new();
        gathered
            .try_reserve_exact(gather_len)
            .map_err(|_| KernelError::DimMismatch {
                expected: input.dim,
                found: gather_len,
            })?;
        let mut ids: Vec<u64> = Vec::new();
        ids.try_reserve_exact(input.slots.len())
            .map_err(|_| KernelError::DimMismatch {
                expected: input.dim,
                found: input.slots.len(),
            })?;
        for &slot in input.slots {
            let start = (slot as usize).saturating_mul(dim);
            let end = start.saturating_add(dim);
            let Some(row) = input.vectors.get(start..end) else {
                // `SearchInput` の行単位 skip 規約（`CpuScalarProvider::search`
                // ドキュメント参照）と同じ理由で、範囲外の行だけを候補から外す。
                continue;
            };
            gathered.extend_from_slice(row);
            ids.push(slot as u64);
        }
        self.search(SearchInput {
            ids: &ids,
            vectors: &gathered,
            dim: input.dim,
            query: input.query,
            k: input.k,
        })
    }
}

/// 既定の CPU-only 参照実装。内積スコアでの総当たり Top-k（`O(n log k)`、`BinaryHeap`
/// による部分ソート）を単一スレッドで行う。内積カーネル自体は `isa.rs`
/// （TASK-156・CORE-14）の実行時検出結果に従う（`dot` 参照。対応 CPU では SIMD、
/// 非対応環境ではスカラー逐次和）。スレッド並列化された
/// [`crate::parallel_search::ParallelSearchProvider`]（TASK-126）の正解値検証用の参照実装も兼ねる。
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuScalarProvider;

impl SearchProvider for CpuScalarProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        let dim = input.dim as usize;
        if input.query.len() != dim {
            return Err(KernelError::DimMismatch {
                expected: input.dim,
                found: input.query.len(),
            });
        }
        // クエリは wire 経路からの untrusted 入力であり得るため、次元検証の直後に
        // 明示的に拒否する（`total_cmp` は NaN を最大値扱いにするため、ここで
        // 弾かないと不正なクエリ 1 件が Top-k を恒久的に占有し得る）。
        if input.query.iter().any(|v| !v.is_finite()) {
            return Err(KernelError::NonFiniteQuery);
        }
        if input.k == 0 || input.ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut selector = TopKSelector::new(input.k);
        for (idx, &id) in input.ids.iter().enumerate() {
            let start = idx.saturating_mul(dim);
            let end = start.saturating_add(dim);
            let Some(vector) = input.vectors.get(start..end) else {
                // アリーナ側の不変条件（`vectors.len() == ids.len() * dim`）が破れている。
                // untrusted 入力由来ではないが、添字アクセスで panic させず該当行だけを
                // 候補から除外する（呼び出し全体は `Ok` のまま。破損行 1 件を混入させない
                // という意味では安全側だが、検索全体を拒否するわけではないため厳密な
                // fail-closed ではない点に注意。共有参照実装として
                // `parallel_search.rs::search_range` と同一の挙動を維持する）。
                continue;
            };
            let score = dot(vector, input.query);
            if !score.is_finite() {
                // 格納ベクトルの NaN/Inf 混入、またはオーバーフローによる内積の非有限化
                // （Medium 指摘対応）。`total_cmp` は +NaN を最大値扱いにするため、
                // 有限値チェックなしでは 1 行の非有限スコアが Top-k を恒久的に占有し
                // 正当な上位ヒットを押し出しかねない。fail-closed に当該行を除外する。
                continue;
            }
            selector.push(CandidateHit { id, score });
        }
        Ok(selector.into_sorted_vec())
    }

    /// 行列全体（`input.vectors`）を複製せず、`input.slots` が指す行だけを直接
    /// 参照して総当たり Top-k を選出する（Issue #654）。[`SearchProvider::search`]
    /// と同じ検証順・同じ除外規約（範囲外行・非有限スコアの skip）を踏襲するため、
    /// 候補行を事前に複製してから `search` を呼んだ場合とビット同一の結果になる。
    fn search_subset(
        &self,
        input: SubsetSearchInput<'_>,
    ) -> Result<Vec<CandidateHit>, KernelError> {
        let dim = input.dim as usize;
        if input.query.len() != dim {
            return Err(KernelError::DimMismatch {
                expected: input.dim,
                found: input.query.len(),
            });
        }
        if input.query.iter().any(|v| !v.is_finite()) {
            return Err(KernelError::NonFiniteQuery);
        }
        if input.k == 0 || input.slots.is_empty() {
            return Ok(Vec::new());
        }

        let mut selector = TopKSelector::new(input.k);
        for &slot in input.slots {
            let start = (slot as usize).saturating_mul(dim);
            let end = start.saturating_add(dim);
            let Some(vector) = input.vectors.get(start..end) else {
                continue;
            };
            let score = dot(vector, input.query);
            if !score.is_finite() {
                continue;
            }
            selector.push(CandidateHit {
                id: slot as u64,
                score,
            });
        }
        Ok(selector.into_sorted_vec())
    }
}

/// 内積（dot product）。実体は `isa.rs::current().dot`（TASK-156・CORE-14）へ
/// 委譲し、実行時検出された ISA（AVX2+FMA・AVX-512・NEON。非対応環境では
/// `isa::dot_scalar` と同じ左から右への逐次和）を使う。
///
/// `parallel_search.rs::search_range`・`batch_search.rs`・`rls.rs` からも同一関数
/// として呼ばれる（Issue #34 レビュー指摘対応: 加算順序を分岐させると
/// `ParallelSearchProvider` 等と本 provider の Top-k 集合・順序が丸め誤差で
/// 食い違い得るため、`pub(crate)` にして共有し、単一のカーネル・単一の加算順序で
/// あることを構造的に保証する。ISA 検出はプロセス内で単調なため、実行中に加算順序が
/// 変わることはない）。
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    crate::isa::current().dot(a, b)
}

/// 4 行 × 1 クエリの内積（`dot` の行ブロック版。Issue #510・TASK-156・CORE-14。
/// ポインタ: `docs/design/dot-kernel-row-block.md`）。
///
/// 実体は `isa.rs::SimdKernel::dot_block4` へ委譲する。契約は各要素が
/// `dot(rows[i], query)` とビット同一であること（1 行版と同じ加算順序を
/// 内部で共有するため）。`parallel_search.rs::search_range` が 4 行単位の
/// ブロックカーネルとして呼ぶ本番経路であり、行間でクエリのロードを 1 回に
/// 共有することで load 帯域を削減する（行内の演算順・アキュムレータ構造は
/// `dot` と完全に同一に保つため、Top-k の集合・順序への影響はない）。
pub(crate) fn dot_block4(rows: [&[f32]; 4], query: &[f32]) -> [f32; 4] {
    crate::isa::current().dot_block4(rows, query)
}

/// ヒープ内の同点タイブレーク規約（Low 指摘対応）: スコアが同じ場合は id が小さい方を
/// 「強い」候補として扱う（[`TopKSelector::into_sorted_vec`] の `sort_by` が返却直前に
/// id 昇順で安定させるのと選出段の基準を揃え、ヒープ挿入順・入力順に依存しない決定的な
/// 選出にする）。`f32` は全順序を持たないため `total_cmp` を使う。ただし非有限スコアは
/// [`TopKSelector::push`] が事前に弾くため、ここで比較する値は常に有限値になる。
struct MinHeapItem(CandidateHit);
impl PartialEq for MinHeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for MinHeapItem {}
impl PartialOrd for MinHeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MinHeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // 最小ヒープ用に「より劣る側が小さい」順序へ揃える（同点順序は D9 の順序キー経由）。
        search_order::cmp_score_desc_then_tie(
            other.0.score,
            search_order::tie_key_for_response_id(other.0.id),
            self.0.score,
            search_order::tie_key_for_response_id(self.0.id),
        )
    }
}

/// Top-k 選出の共通ヘルパ（対象ビヘイビア: CORE-4）。スコア最小のヒープを保持し、
/// サイズ `k` を超えたら最小要素を捨てる方式（事前に全件確保しない・`O(n log k)`）。
///
/// [`CpuScalarProvider`]（本ファイル）と `parallel_search.rs::ParallelSearchProvider`
/// （TASK-126）の両方から使われる。後者はスレッドごとに本セレクタで部分 Top-k を
/// 選出したうえで、部分結果を同じセレクタへ再度 push してマージする
/// （分割数・スレッド数に依らず選出規約が一意に決まる設計。CORE-3・SEARCH-4）。
pub(crate) struct TopKSelector {
    k: usize,
    heap: std::collections::BinaryHeap<std::cmp::Reverse<MinHeapItem>>,
}

impl TopKSelector {
    /// 容量 `k` の選出器を作る。`k == 0` の場合は何を push しても常に空集合を返す。
    pub(crate) fn new(k: usize) -> Self {
        Self {
            k,
            // `k` は `SearchProvider` trait 経由で外部（wire-server 等の呼び出し元）から
            // 到達しうる値で、`core.rs` は `MAX_SEARCH_K` でクランプするが本 provider 自体は
            // 検証しない。`with_capacity(k)` で事前確保すると未検証の巨大な `k` がそのまま
            // アロケーションサイズになってしまう（coding-rust.md「無制限確保禁止」）ため、
            // 事前確保はせず push 時に自然成長させる（旧 `CpuScalarProvider` 実装と同じ挙動。
            // 実際に保持する要素数は `push` のロジックにより高々 `k` に抑えられる）。
            heap: std::collections::BinaryHeap::new(),
        }
    }

    /// 内部ヒープへ `additional` 件分の容量をフォールブルに予約する。[`Self::new`]
    /// があえて事前確保しない理由（未検証の `k` を確保サイズへ直接使わない）とは
    /// 矛盾しない: 本メソッドは呼び出し元が別途総量を上限検証済みであることを
    /// 前提にした任意 API である（`batch_search.rs::BatchEngine::batch_search` が
    /// バッチ全体の `sum(k)` を検証してから各選出器へ呼ぶ想定）。呼び出し元が
    /// 本メソッドを使わず素朴に `push` するだけの経路（`CpuScalarProvider`・
    /// `ParallelSearchProvider`）は今までどおり amortized 成長のままでよい
    /// （codex P1 指摘対応: `BinaryHeap::push` の内部確保は abort-on-OOM のため、
    /// 総量が上限検証済みの呼び出し元には `Result` 契約の確保手段を用意する。
    /// security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    pub(crate) fn try_reserve(
        &mut self,
        additional: usize,
    ) -> Result<(), std::collections::TryReserveError> {
        self.heap.try_reserve(additional)
    }

    /// 候補 1 件を選出器へ投入する。非有限スコア（NaN/Inf）は fail-closed に無視する
    /// （呼び出し元が事前に除外している場合でも、二重の安全網として機能する）。
    pub(crate) fn push(&mut self, hit: CandidateHit) {
        if self.k == 0 || !hit.score.is_finite() {
            return;
        }
        let candidate = MinHeapItem(hit);
        if self.heap.len() < self.k {
            self.heap.push(std::cmp::Reverse(candidate));
        } else if let Some(std::cmp::Reverse(top)) = self.heap.peek() {
            if candidate > *top {
                self.heap.pop();
                self.heap.push(std::cmp::Reverse(candidate));
            }
        }
    }

    /// 選出結果をスコア降順（同点は候補識別子 [`CandidateHit::id`] の昇順）で確定して返す
    /// （`docs/design/rrf-tie-break-determinism.md` の順序契約）。
    pub(crate) fn into_sorted_vec(self) -> Vec<CandidateHit> {
        let mut out: Vec<CandidateHit> = self
            .heap
            .into_iter()
            .map(|std::cmp::Reverse(item)| item.0)
            .collect();
        out.sort_by(|a, b| {
            search_order::cmp_score_desc_then_tie(
                a.score,
                search_order::tie_key_for_response_id(a.id),
                b.score,
                search_order::tie_key_for_response_id(b.id),
            )
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 可視性マスクの適用（不可視行が Top-k に混ざらないこと）は、以前は本モジュールの
    // `SearchInput::is_visible` クロージャで検証していたが、codex P0 指摘（Issue #137）
    // 対応で `SearchInput` はコア側（`core.rs`）が可視行だけへ絞り込んだ縮約ビューのみを
    // 受け取る設計へ変更した。そのため可視性マスクの検証は本モジュールの責務外になり、
    // `crates/engine/tests/vector_core.rs::search_excludes_private_rows_of_the_same_tenant_when_ctx_disallows_private`
    // （CORE-2）へ移動している。本モジュールはあくまで「渡された入力の中での Top-k 選出」
    // のみを検証する。

    #[test]
    fn top_k_returns_highest_dot_product_scores() {
        let ids = [1u64, 2, 3, 4];
        // dim=2 の 4 行。クエリ [1.0, 0.0] との内積は id 昇順に 1.0, 2.0, 0.0, 3.0。
        let vectors = [1.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 2,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert_eq!(
            hits,
            vec![
                CandidateHit { id: 4, score: 3.0 },
                CandidateHit { id: 2, score: 2.0 }
            ]
        );
    }

    #[test]
    fn non_finite_query_is_rejected() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        for query in [
            [f32::NAN, 0.0],
            [f32::INFINITY, 0.0],
            [0.0, f32::NEG_INFINITY],
        ] {
            let input = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: 2,
                query: &query,
                k: 1,
            };
            let err = CpuScalarProvider.search(input).unwrap_err();
            assert_eq!(err, KernelError::NonFiniteQuery, "query={query:?}");
        }
    }

    #[test]
    fn non_finite_stored_score_is_excluded_and_legitimate_hits_keep_rank() {
        let ids = [1u64, 2, 3];
        // id=2 の行は NaN を含み、内積が NaN になる（Top-k を占有してはならない）。
        let vectors = [1.0, 0.0, f32::NAN, 0.0, 2.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 2,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert_eq!(
            hits,
            vec![
                CandidateHit { id: 3, score: 2.0 },
                CandidateHit { id: 1, score: 1.0 }
            ]
        );
    }

    #[test]
    fn tied_scores_prefer_smaller_id_at_selection_boundary() {
        let ids = [3u64, 2, 1];
        // 3 行とも同スコア。k=1 のため、選出段のタイブレークで最終結果が決まる
        // （挿入順は降順 id だが、結果は最小 id を選ぶ）。
        let vectors = [1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 1,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert_eq!(hits, vec![CandidateHit { id: 1, score: 1.0 }]);
    }

    // TASK-84（対応 Issue #61）: PoC-10 が指摘した「同点タイブレーク欠如による
    // 非決定性」の回帰テスト。`tied_scores_prefer_smaller_id_at_selection_boundary`
    // は k=1（単一勝者）のみを検証するため、本テストは k>1 で複数件が採用される
    // 場合の順序全体（id 昇順）を検証する。挿入順は id 降順（シャッフル相当）で
    // 与え、`push` の走査順に依存せず結果が id 昇順になることを確認する。
    #[test]
    fn tied_scores_are_ordered_by_id_ascending_when_multiple_survive() {
        let ids = [5u64, 4, 3, 2, 1];
        // 5 行とも同スコア（内積はすべて 1.0）。
        let vectors = [1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 3,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert_eq!(
            hits,
            vec![
                CandidateHit { id: 1, score: 1.0 },
                CandidateHit { id: 2, score: 1.0 },
                CandidateHit { id: 3, score: 1.0 },
            ]
        );
    }

    #[test]
    fn dim_mismatch_query_is_rejected() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        let query = [1.0, 0.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 1,
        };
        let err = CpuScalarProvider.search(input).unwrap_err();
        assert_eq!(
            err,
            KernelError::DimMismatch {
                expected: 2,
                found: 3
            }
        );
    }

    #[test]
    fn k_zero_returns_empty() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 0,
        };
        let hits = CpuScalarProvider.search(input).expect("search ok");
        assert!(hits.is_empty());
    }

    // object-safety の固定（CORE-13）: `Box<dyn SearchProvider>` として保持できること。
    #[test]
    fn provider_is_object_safe() {
        let _boxed: Box<dyn SearchProvider> = Box::new(CpuScalarProvider);
        // `search_subset` も trait メソッドとして同じ `dyn` 経由で呼べること
        // （Issue #654: object-safety を壊していないことの直接検証）。
        let ids = [10u64];
        let vectors = [1.0f32, 0.0];
        let query = [1.0f32, 0.0];
        let boxed: Box<dyn SearchProvider> = Box::new(CpuScalarProvider);
        let hits = boxed
            .search_subset(SubsetSearchInput {
                slots: &[0],
                vectors: &vectors,
                dim: 2,
                query: &query,
                k: 1,
            })
            .expect("search_subset ok");
        let _ = ids;
        assert_eq!(hits, vec![CandidateHit { id: 0, score: 1.0 }]);
    }

    // `CpuScalarProvider::search_subset` の直接参照版（オーバーライド）は、
    // `slots` の行を事前に複製してから `search` を呼んだ場合とビット同一の
    // 結果になること（Issue #654 要件 2）。
    #[test]
    fn search_subset_matches_gathered_search_reference() {
        let dim = 4usize;
        let rows = 9usize;
        let mut vectors = Vec::with_capacity(rows * dim);
        for i in 0..rows {
            for d in 0..dim {
                vectors.push(((i * dim + d) % 13) as f32 * 0.1 - 0.5);
            }
        }
        let query = [0.3f32, -0.1, 0.2, 0.05];
        // 昇順・重複なし・末尾に範囲外スロットを 1 件混ぜて skip 規約も検証する。
        let slots: Vec<u32> = vec![0, 2, 3, 5, 8, 100];

        let subset_hits = CpuScalarProvider
            .search_subset(SubsetSearchInput {
                slots: &slots,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k: 4,
            })
            .expect("search_subset ok");

        // 参照実装: `slots` の行を手で gather してから通常の `search` を呼ぶ
        // （範囲外スロット 100 は自然に vectors.get で弾かれる）。
        let mut gathered = Vec::new();
        let mut ids = Vec::new();
        for &slot in &slots {
            let start = (slot as usize) * dim;
            if let Some(row) = vectors.get(start..start + dim) {
                gathered.extend_from_slice(row);
                ids.push(slot as u64);
            }
        }
        let reference_hits = CpuScalarProvider
            .search(SearchInput {
                ids: &ids,
                vectors: &gathered,
                dim: dim as u32,
                query: &query,
                k: 4,
            })
            .expect("search ok");

        assert_eq!(subset_hits, reference_hits);
        assert!(!subset_hits.is_empty());
    }

    // 既定実装（gather 経由）のみを使うカスタム provider（`search` だけを
    // 実装し `search_subset` はオーバーライドしない）でも、
    // `CpuScalarProvider::search_subset`（直接参照オーバーライド）とビット同一の
    // 結果になること（Issue #654 要件: 既存カスタム provider 互換）。
    #[derive(Debug, Default, Clone, Copy)]
    struct DefaultOnlyProvider;
    impl SearchProvider for DefaultOnlyProvider {
        fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            CpuScalarProvider.search(input)
        }
    }

    #[test]
    fn default_search_subset_matches_overridden_implementation() {
        let dim = 3usize;
        let rows = 6usize;
        let mut vectors = Vec::with_capacity(rows * dim);
        for i in 0..rows {
            for d in 0..dim {
                vectors.push(((i * dim + d) % 7) as f32 * 0.2 - 0.6);
            }
        }
        let query = [0.1f32, 0.2, -0.3];
        let slots: Vec<u32> = vec![1, 2, 4, 5];

        let via_default = DefaultOnlyProvider
            .search_subset(SubsetSearchInput {
                slots: &slots,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k: 3,
            })
            .expect("default search_subset ok");
        let via_override = CpuScalarProvider
            .search_subset(SubsetSearchInput {
                slots: &slots,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k: 3,
            })
            .expect("overridden search_subset ok");

        assert_eq!(via_default, via_override);
    }

    #[test]
    fn search_subset_k_zero_or_empty_slots_returns_empty() {
        let vectors = [1.0f32, 0.0];
        let query = [1.0f32, 0.0];
        let empty_slots: [u32; 0] = [];
        let hits = CpuScalarProvider
            .search_subset(SubsetSearchInput {
                slots: &[0],
                vectors: &vectors,
                dim: 2,
                query: &query,
                k: 0,
            })
            .expect("k=0 ok");
        assert!(hits.is_empty());
        let hits = CpuScalarProvider
            .search_subset(SubsetSearchInput {
                slots: &empty_slots,
                vectors: &vectors,
                dim: 2,
                query: &query,
                k: 1,
            })
            .expect("empty slots ok");
        assert!(hits.is_empty());
    }

    #[test]
    fn search_subset_rejects_dim_mismatch_and_non_finite_query() {
        let vectors = [1.0f32, 0.0];
        let bad_dim_query = [1.0f32, 0.0, 0.0];
        let err = CpuScalarProvider
            .search_subset(SubsetSearchInput {
                slots: &[0],
                vectors: &vectors,
                dim: 2,
                query: &bad_dim_query,
                k: 1,
            })
            .unwrap_err();
        assert_eq!(
            err,
            KernelError::DimMismatch {
                expected: 2,
                found: 3
            }
        );

        let nan_query = [f32::NAN, 0.0];
        let err = CpuScalarProvider
            .search_subset(SubsetSearchInput {
                slots: &[0],
                vectors: &vectors,
                dim: 2,
                query: &nan_query,
                k: 1,
            })
            .unwrap_err();
        assert_eq!(err, KernelError::NonFiniteQuery);
    }
}
