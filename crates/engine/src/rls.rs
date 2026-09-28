//! 事前フィルタ方式によるテナント境界の再利用可能インデックス
//! （TASK-133・対象ビヘイビア: RLS-1, RLS-2, RLS-3, RLS-4）。
//!
//! テナント境界の判定は [`crate::policy::PolicyContext::is_visible`] の単一照合パスに
//! 限定し、本モジュール独自のテナント比較を新設しない（security.md P0）。
//! [`PrefilterIndex::build`] は構築時に渡された `PolicyContext` の可視性述語で
//! [`crate::arena::VectorArena::build_filtered`] を呼び、可視行だけを保持する縮約ビューを
//! 作る。[`PrefilterIndex`] は構築時に束縛した `PolicyContext`・`Storage` 参照を保持し、
//! [`PrefilterIndex::search`] は呼び出し時の `PolicyContext` との完全一致（不一致は
//! [`RlsError::ContextMismatch`]）と、provider 呼び出し前のアリーナ全行の現在の
//! tenant/visibility 再検証（構築時との不一致は [`RlsError::IndexStale`]）を fail-closed に
//! 行う。契約の詳細は [`PrefilterIndex::search`] のドキュメント参照。
//!
//! `core.rs::EngineCore`（`VectorCore::search`）は `core.rs::PrefilterCache`
//! （TASK-169）経由で [`PrefilterSnapshot`] をキャッシュし、世代整合を保った上で
//! 事前フィルタインデックスを再利用する（`VectorCore` trait のシグネチャは変更しない。
//! 詳細は `core.rs` モジュールドキュメント・`PrefilterSnapshot` のドキュメント参照）。
//!
//! 本モジュールはさらに [`SearchTimeFilter`]（TASK-134・対象ビヘイビア: RLS-1, RLS-3）を
//! 提供する。[`PrefilterIndex`] は構築時に `PolicyContext` を束縛するため、ポリシーが
//! リクエスト単位で動的に変わるワークロードでは毎回の再構築コストがかかる。
//! [`SearchTimeFilter`] はそのフォールバックで、可視行の縮約ビュー（アリーナ）を保持せず、
//! `search` 呼び出しごとにストレージをストリーミング走査して異なる `PolicyContext` を
//! 受け取れる。可視性判定は全行アリーナを外部（`SearchProvider`）へ渡すマスク方式ではなく、
//! 可視性判定とスコア計算を単一パスで trust boundary（本モジュール）内に閉じて行う
//! （[`SearchInput`] の「可視行のみを含む縮約ビュー」契約を維持するため）。
//! 静的ポリシー＝事前フィルタ（[`PrefilterIndex`]）／動的ポリシー＝検索時フィルタ
//! （[`SearchTimeFilter`]）の使い分け・切り替え判断は呼び出し元の責務とする
//! （本モジュールは両方の API を提供するのみ）。
//!
//! [`SearchTimeFilter`] は容量上限（[`crate::arena::MAX_ARENA_ROWS`]/
//! [`crate::arena::MAX_ARENA_TOTAL_BYTES`]、[`crate::arena::check_capacity`] 経由）を
//! [`PrefilterIndex`] と同じく「呼び出しテナントの可視行数・バイト量」基準で検査する
//! （両型とも他テナントのデータ量が対象テナントの検索可用性へ干渉しない契約で揃えている。
//! 詳細は [`SearchTimeFilter`] のドキュメント参照）。
//!
//! 本モジュールはさらに [`RlsSafetyNet`]（TASK-136・対象ビヘイビア: RLS-5。
//! `docs/spec/04-behavior/sql-surface.md` SQL-7）を提供する。`sql::exec` の DISTANCE
//! 段（および SCALAR 事後フィルタ）を通過した最終 `hits` を、束縛済み
//! `PolicyContext::is_visible` で再判定する第 2 層の防御であり、`sql::plan` に
//! あった同名の安全網（TASK-76）をこのモジュールへ再配置したもの。判定を独自実装
//! せず [`PolicyContext::is_visible`] へ委譲する点は本モジュールの他 API と同じ
//! 方針を踏襲する。安全網通過済みの `hits` は [`RlsVerifiedHits`]（witness 型）
//! としてのみ持ち出せるため、投影段（`sql::exec`）は安全網を経由しない生の
//! `Vec<(u64, f64)>` から投影へ到達する経路を型として作れない。
//!
//! 本モジュールはさらに [`ImplicitRlsHook`]（TASK-137・対象ビヘイビア: RLS-6, RLS-7。
//! ポインタ: `docs/spec/05-tasks.md` TASK-137・`docs/spec/04-behavior/rls.md`）を提供する。
//! 認証済みセッションからサーバー側で導出された `PolicyContext`（導出自体は
//! `wire-server/src/auth.rs`・TASK-67 の管轄）だけを入力とし、
//! `core.rs::EngineCore::search`/`get_row`・`sql/exec.rs::execute_statement`
//! の候補集合構築への単一注入点である。判定ロジック自体は新設せず
//! [`crate::policy::PolicyContext::is_visible`] へ委譲するだけに留める
//! （テナント比較の分岐を増やさない・security.md P0）。RLS 安全網（[`RlsSafetyNet`]）
//! とは独立した契約で、[`ImplicitRlsHook`] は候補構築時の暗黙事前フィルタへの
//! 注入点、[`RlsSafetyNet`] はその後段（DISTANCE/SCALAR 通過後）の再判定という
//! 異なる段で連携する（両者とも `PolicyContext::is_visible` へ委譲するのみで、
//! 独自のテナント比較を新設しない点は共通）。

use std::collections::HashMap;

use redb::{ReadableDatabase, ReadableTable};

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::CatalogError;
use crate::core::{provider_result_is_valid, validate_search_k, MAX_SEARCH_K};
use crate::kernel::{self, CandidateHit, KernelError, SearchHit, SearchInput, SearchProvider};
use crate::policy::PolicyContext;
use crate::storage::{Storage, Visibility};

/// RLS-6 / RLS-7 の暗黙適用フック（TASK-137）。
///
/// 認証済みセッションから導出済みの [`PolicyContext`] だけを束縛し、候補集合構築時の
/// 可視性フィルタ適用への単一注入点。呼び出し元は
/// `core.rs::EngineCore::search`/`get_row` と `sql/exec.rs::execute_statement`。
/// コンストラクタ・メソッドのいずれも `PolicyContext` 以外の入力を受け取らない。
///
/// この単一注入点の契約が MVP クエリカタログ（C1〜C4）以外の全読み取り経路へも
/// 一般化されて働くことは TASK-138（対象ビヘイビア: RLS-8）・`tests/rls_generalized.rs`
/// で機械検証する。
#[must_use]
pub struct ImplicitRlsHook<'c> {
    ctx: &'c PolicyContext,
}

impl<'c> ImplicitRlsHook<'c> {
    /// 認証済みセッションから導出済みの `ctx` を束縛する。
    #[inline]
    pub fn new(ctx: &'c PolicyContext) -> Self {
        Self { ctx }
    }

    /// 束縛済みの `PolicyContext`（読み取り専用）。
    #[inline]
    pub fn context(&self) -> &'c PolicyContext {
        self.ctx
    }

    /// 単点判定（`core.rs::EngineCore::get_row` 等）。
    /// [`PolicyContext::is_visible`] へ委譲するだけで独自比較を持たない。
    ///
    /// 全読み取り経路で毎行呼ばれるホットパス（Issue #354）のため `#[inline]` を付与し、
    /// 委譲先 [`PolicyContext::is_visible`] とあわせてインライン化されやすくする。
    #[inline]
    pub fn is_visible(&self, row_tenant: &str, row_visibility: Visibility) -> bool {
        self.ctx.is_visible(row_tenant, row_visibility)
    }

    /// 候補集合構築（`VectorArena::build_filtered`・
    /// `build_filtered_with_rows_in_txn` 系）へそのまま渡せる述語を返す。
    /// 返す関数も `PolicyContext` 以外の入力を一切持たない。
    #[inline]
    pub fn predicate(&self) -> impl Fn(&str, Visibility) -> bool + 'c {
        let ctx = self.ctx;
        move |tenant, visibility| ctx.is_visible(tenant, visibility)
    }
}

/// [`PrefilterIndex`] のエラー型。`core.rs::CoreError` とおおむね対称の設計だが、
/// `Policy` は持たず（`PolicyContext` の構築時検証は呼び出し元の責務で本モジュールには
/// 到達しない）、[`RlsError::ContextMismatch`] は `core.rs` 側に対応がない
/// （`EngineCore::search` はクエリ毎にアリーナを再構築するため構築時 ctx と検索時 ctx の
/// 食い違いという状態自体が存在しない。本モジュール特有のインデックス再利用に伴う
/// エラー種別）。
#[derive(Debug)]
pub enum RlsError {
    Arena(ArenaError),
    Kernel(KernelError),
    /// [`crate::storage::Storage::current_generation`] の読み取り失敗（[`Self::build`]
    /// 時のみ発生。`search` 時は [`RlsError::IndexStale`] へ丸め込む）。
    Storage(crate::storage::StorageError),
    /// `k == 0` または [`MAX_SEARCH_K`] 超過。
    InvalidK {
        k: usize,
    },
    /// 指定テーブルが存在しない（`core.rs::CoreError::NotFound` と同一契約: 不可視と
    /// 不存在を区別しない。`Display` へテーブル名を含めない。security.md P0）。
    NotFound,
    /// 検索時 `PolicyContext` が構築時と一致しない場合の fail-closed 拒否
    /// （`Display` はテナント ID・可視性集合を含まない。security.md P0）。
    ContextMismatch,
    /// provider 呼び出し前のアリーナ全行再検証で、構築時と現在の tenant/visibility が
    /// 一致しない行を検出した場合の fail-closed 拒否（`Display` は id・テナント ID を
    /// 含まない。呼び出し元は [`PrefilterIndex::build`] を呼び直すこと。security.md P0）。
    IndexStale,
    /// `SearchProvider` が返却した `Vec<`[`CandidateHit`]`>` が Top-k の契約に違反した
    /// （`core.rs::CoreError::ProviderResultRejected` と同一契約。判定は共有ヘルパ
    /// `provider_result_is_valid` で行う。fail-closed: 違反があれば結果を一切返さない）。
    ProviderResultRejected,
}

impl std::fmt::Display for RlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RlsError::Arena(e) => write!(f, "rls prefilter arena error: {e}"),
            RlsError::Kernel(e) => write!(f, "rls prefilter kernel error: {e}"),
            RlsError::Storage(e) => write!(f, "rls prefilter storage error: {e}"),
            RlsError::InvalidK { k } => {
                write!(f, "invalid k: {k} (must be 1..={MAX_SEARCH_K})")
            }
            RlsError::NotFound => write!(f, "not found"),
            RlsError::ContextMismatch => write!(
                f,
                "policy context does not match the context the index was built with"
            ),
            RlsError::IndexStale => write!(
                f,
                "prefilter index is stale: rebuild required before searching again"
            ),
            RlsError::ProviderResultRejected => write!(
                f,
                "search provider returned a hit outside the policy-visible id set"
            ),
        }
    }
}

impl std::error::Error for RlsError {}

impl From<ArenaError> for RlsError {
    fn from(e: ArenaError) -> Self {
        RlsError::Arena(e)
    }
}

impl From<KernelError> for RlsError {
    fn from(e: KernelError) -> Self {
        RlsError::Kernel(e)
    }
}

impl From<crate::storage::StorageError> for RlsError {
    fn from(e: crate::storage::StorageError) -> Self {
        RlsError::Storage(e)
    }
}

/// 事前フィルタ方式の再利用可能インデックス（TASK-133・RLS-1〜4）。
///
/// [`Self::build`] 構築時に束縛した [`PolicyContext`] の可視性述語で
/// [`VectorArena::build_filtered`] を呼び、可視行のみのカラムナ表現を保持する。
/// [`Self::search`] の契約は同メソッドのドキュメント参照。
///
/// [`Self::len`]・[`Self::is_empty`] は可視行数・行の有無という存在情報を返すため、
/// [`Self::built_ctx`] との完全一致を要求する（不一致は [`RlsError::ContextMismatch`]）。
/// [`Self::dim`]・[`Self::table_name`] は非機微情報のため `ctx` を要求しない。
/// `HashSet<T>` の `capacity()`（保持要素数ではなく確保済み容量）から、実バケット数の
/// 保守的な上限バイト数を見積もる（[`PrefilterSnapshot::approx_heap_bytes`] から呼ぶ。
/// codex-review P1 対応・PR #191）。
///
/// `std::collections::HashSet` の実装基盤 `hashbrown` は最大負荷率 7/8 で 2 のべき乗
/// サイズのテーブルを確保するため、実バケット数は `capacity() * 8 / 7` 以上になる。
/// ここでは `capacity()` を `8/7` 倍してから次の 2 のべき乗へ切り上げることで、
/// 実バケット数を下回らない（安全側＝上振れの）見積もりにする。1 バケットあたり
/// 要素サイズ + control byte 1 byte を課金する。オーバーフロー時は `usize::MAX` に
/// 飽和させる（総量上限判定を通過させない fail-closed 側に倒す。
/// .claude/rules/coding-rust.md: 整数演算は checked/saturating を使う）。
fn hash_set_conservative_bytes<T>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let scaled_up = capacity.saturating_mul(8) / 7;
    let buckets = scaled_up.checked_next_power_of_two().unwrap_or(usize::MAX);
    let per_bucket = std::mem::size_of::<T>().saturating_add(1);
    buckets.saturating_mul(per_bucket)
}

/// 事前フィルタ方式の再利用可能インデックスのうち、`&Storage` 参照を持たない
/// 内部スナップショット部分（TASK-133・TASK-169・RLS-1〜4）。
///
/// [`PrefilterIndex`] は本来 `&'s Storage` を保持するが、`core.rs::EngineCore` は
/// `Storage` を所有するため、`EngineCore` 内部のキャッシュ（`core.rs::PrefilterCache`）に
/// `PrefilterIndex<'s>` をそのまま格納すると自己参照構造体になってしまう。
/// `PrefilterSnapshot` は storage 参照を持たない構築結果だけを保持し、`search_with`・
/// `built_generation` 等のメソッドへ `&Storage` をそのつど引数で受け取ることで、
/// `Arc<PrefilterSnapshot>` を storage の生存期間から独立してキャッシュに保持できるように
/// する。[`PrefilterIndex`] は本型への薄いラッパーとして、既存の公開 API・契約（`ctx`
/// 完全一致・前後世代照合・provider 結果検証）をそのまま維持する。
pub(crate) struct PrefilterSnapshot {
    arena: VectorArena,
    /// provider へ渡す候補識別子（アリーナのスロット番号 0..n）。行 `id` は 1 つの
    /// 可視集合内で一意とは限らないため識別子に使えない（対象ビヘイビア: TABLE-12。
    /// `core::slot_ids_for` のドキュメント参照）。検索のたびに作り直さず構築時に保持する。
    slot_ids: Vec<u64>,
    /// `slot_ids` から作る「識別子 → 件数」の多重集合キャッシュ（provider 結果の
    /// 検証に使う。`core::visible_id_counts` で構築。スロット番号は重複しないため
    /// 各件数は 1 になるが、検証ヘルパを `core.rs`・`sql/exec.rs` と共有するため
    /// 同じ形で保持する）。
    visible_id_counts: HashMap<u64, usize>,
    /// 構築時に束縛した `PolicyContext` の複製。`ctx` 照合ゲートに使う。
    built_ctx: PolicyContext,
    /// 構築時に読んだストレージ世代（[`Storage::current_generation`]）。
    /// [`Self::search_with`] の失効検出に使う。
    built_generation: u64,
    /// 構築時に読んだテーブル単位世代（[`crate::catalog::table_generation_in_txn`]。
    /// `arena` を読んだのと同じ順序（世代読み取り → アリーナ構築）で束縛する。
    /// [`Self::search_with_hnsw`] が `search_or_fallback` へ渡す `read_txn` の
    /// テーブル世代と照合し、`arena`（このスナップショット）と `read_txn` の
    /// スナップショットが同一世代であることを保証する（Issue #409 codex-review P1。
    /// `docs/design/hnsw-generation-cache.md`「呼び出し元スナップショットとの世代整合」
    /// 節参照）。
    built_table_generation: u64,
}

impl PrefilterSnapshot {
    /// `table` に対し `ctx` の可視性述語で可視行のみのインデックスを構築する（RLS-1）。
    /// テーブル不存在は [`RlsError::NotFound`] へ丸め込む（存在情報を漏らさない。
    /// security.md P0）。容量超過・次元不整合は [`RlsError::Arena`] へ伝播する。
    pub(crate) fn build(
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
    ) -> Result<Self, RlsError> {
        // 世代を先に読んでからアリーナを構築する（アリーナ構築中に別の書き込みが
        // コミットされても、その変更を見落とさない方向の順序。`search_with` の doc 参照）。
        let built_generation = storage.current_generation()?;
        // テーブル単位世代も同じ順序（アリーナ構築前）で読む。`search_with_hnsw` が
        // `search_or_fallback` へ渡す `read_txn` の世代と、この `arena` を束縛した
        // 世代とを一致させるための基準値（[`Self::built_table_generation`] ドキュメント
        // 参照）。テーブル不存在は `build_filtered` と同じく [`RlsError::NotFound`] へ
        // 丸め込む（存在情報を漏らさない fail-closed 契約を世代読み取り経路でも維持）。
        // それ以外の読み取り失敗は `ArenaError::Catalog` 経由で `RlsError::Arena` へ丸める
        // （既存の `From<ArenaError> for RlsError` を再利用し、専用 variant を増やさない）。
        let built_table_generation = match storage.table_generation(table) {
            Ok(g) => g,
            Err(CatalogError::TableNotFound(_)) => return Err(RlsError::NotFound),
            Err(e) => return Err(ArenaError::Catalog(e).into()),
        };
        let arena = match VectorArena::build_filtered(storage, table, |tenant, visibility| {
            ctx.is_visible(tenant, visibility)
        }) {
            Ok(arena) => arena,
            Err(ArenaError::Catalog(CatalogError::TableNotFound(_))) => {
                return Err(RlsError::NotFound)
            }
            Err(e) => return Err(e.into()),
        };
        let slot_ids = crate::core::slot_ids_for(&arena)?;
        let visible_id_counts = crate::core::visible_id_counts(&slot_ids);
        Ok(Self {
            arena,
            slot_ids,
            visible_id_counts,
            built_ctx: ctx.clone(),
            built_generation,
            built_table_generation,
        })
    }

    /// 保持済みインデックスに対して Top-k 検索を行う（over-fetch なし・RLS-3）。
    ///
    /// `ctx` は構築時に束縛した `PolicyContext` と完全一致していなければ
    /// [`RlsError::ContextMismatch`] で fail-closed に拒否する。`k`・`query` の検証
    /// （`core.rs::EngineCore::search` と同一契約）の後、provider を呼ぶ**前**と
    /// provider から結果を受け取った**後**の 2 回、`storage`（呼び出し元が渡す。
    /// キャッシュ経由の再利用時も構築時と同一の `Storage` を渡す契約）の
    /// [`crate::storage::Storage::current_generation`] を呼んで構築時の値と比較する
    /// （いずれかで不一致なら [`RlsError::IndexStale`]）。世代はストレージ全体で、
    /// 実書き込みを伴うコミットのたびに単調増加する（`crate::storage::commit_write_txn`
    /// が `has_writes == true` のときのみ `crate::storage::bump_generation_and_commit`
    /// に委譲する契約。Issue #175。put を伴わない no-op commit では世代は進まない）
    /// ため、両方で一致することは「事前確認から事後確認までの間、行集合・内容とも一切
    /// 変更されていない」ことを意味する（安全性はこの前後比較のみで担保しており、
    /// `current_generation` 自体は世代値以外の一貫性を保証しない）。事前確認を通過した
    /// 場合のみ provider を呼び、戻り値を `provider_result_is_valid`（`core.rs`）で検証する
    /// （違反は [`RlsError::ProviderResultRejected`]）。事前・事後の確認自体は互いに独立した
    /// 読み取りであり、事後確認と `Ok(hits)` 返却の間に残るごく短いウィンドウは次回検索の
    /// 世代照合で扱う。TASK-133・TASK-169・RLS-1〜4 参照。
    ///
    /// DDL（`catalog.rs` の `create_table`／`alter_table_add_column`／`drop_table`）も
    /// すべて `bump_generation_and_commit` 経由で commit するため世代を進める。
    /// テーブルを drop して再作成した場合も同じ世代照合で検出され、drop 前に構築した
    /// スナップショットは drop 後・再作成後のいずれでも `IndexStale` になる
    /// （Issue #179。削除済み行・旧スキーマの行に基づく結果を返す経路を作らない）。
    pub(crate) fn search_with(
        &self,
        storage: &Storage,
        ctx: &PolicyContext,
        provider: &dyn SearchProvider,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        validate_search_k(k).map_err(|k| RlsError::InvalidK { k })?;

        if query.len() != self.arena.dim() as usize {
            return Err(RlsError::Kernel(KernelError::DimMismatch {
                expected: self.arena.dim(),
                found: query.len(),
            }));
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(RlsError::Kernel(KernelError::NonFiniteQuery));
        }

        // 事前の失効検出（上記ドキュメント参照）。世代の読み取り自体に失敗した場合も
        // 「現在の状態を確認できない」ため fail-closed に `IndexStale` とする。
        let pre_generation = storage
            .current_generation()
            .map_err(|_| RlsError::IndexStale)?;
        if pre_generation != self.built_generation {
            return Err(RlsError::IndexStale);
        }

        let input = SearchInput {
            ids: &self.slot_ids,
            vectors: self.arena.vectors(),
            dim: self.arena.dim(),
            query,
            k,
        };
        let hits = provider.search(input)?;

        if !provider_result_is_valid(&hits, k, &self.visible_id_counts) {
            return Err(RlsError::ProviderResultRejected);
        }
        // 検証済みの候補（識別子はスロット番号）をテナント修飾済みヒットへ解決する
        // （対象ビヘイビア: TABLE-12・RLS-9。`(tenant_id, id)` で呼び出し元が行を
        // 一意に解決できる公開契約。codex-review P1 指摘・PR #194）。
        let hits = crate::core::resolve_slot_hits(&self.arena, &hits)
            .ok_or(RlsError::ProviderResultRejected)?;

        // 事後の失効再検証（上記ドキュメント参照）。事前確認〜ここまでの間に別の書き込みが
        // コミットされている可能性があるため、世代を読み直して不一致なら結果を破棄する。
        let post_generation = storage
            .current_generation()
            .map_err(|_| RlsError::IndexStale)?;
        if post_generation != self.built_generation {
            return Err(RlsError::IndexStale);
        }

        Ok(hits)
    }

    /// [`Self::search_with`] の HNSW 世代整合キャッシュ結線版（Issue #409）。
    /// `core.rs::EngineCore::search_with_snapshot` が `hnsw_state`（`SearchEngineKind::
    /// Hnsw` opt-in 構築時のみ `Some`）を保持する場合にこちらを呼ぶ。
    ///
    /// 契約は [`Self::search_with`] と同一（`ctx` 完全一致・`k`／`query` 検証・
    /// provider 呼び出し前後のストレージ世代照合・`provider_result_is_valid` に
    /// よる Top-k 契約検証）で、探索本体だけが異なる: `provider.search` を直接
    /// 呼ぶ代わりに、本スナップショットの可視全集合アリーナ（`FullVisible` 形状。
    /// `sql::exec::execute_statement_with_cache` のフィルタなし DISTANCE クエリと
    /// 同じ形状）を `sql::hnsw_cache::search_or_fallback` へ渡し、索引済みノード＋
    /// 未索引分 brute-force の併用探索を試みる。HNSW 索引固有のエラー・写像検証
    /// 失敗はすべて `search_or_fallback` 内部で当該呼び出しの brute-force 縮退へ
    /// 吸収され、本メソッドへは伝播しない（`sql::hnsw_cache` モジュールドキュメント
    /// 「fail-closed 契約」節）。
    ///
    /// テナント境界は [`Self::search_with`] と同じ多層防御で維持する:
    /// (1) 索引構築入力は本スナップショットの構築時点で `ctx` 可視行のみ
    /// （`Self::build`）、(2) `search_or_fallback` 内部のスロット写像・キー照合・
    /// `kernel::dot` 再計算、(3) 本メソッドの `provider_result_is_valid`。
    /// `sql::hnsw_cache::HnswIndexCache` は `(table, ctx)` 単位で世代整合キャッシュ
    /// されるため、他テナント・他ポリシーの索引を取り違えて参照することはない。
    pub(crate) fn search_with_hnsw(
        &self,
        storage: &Storage,
        ctx: &PolicyContext,
        access: &crate::sql::hnsw_cache::HnswCacheAccess<'_>,
        provider: &dyn SearchProvider,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        validate_search_k(k).map_err(|k| RlsError::InvalidK { k })?;

        if query.len() != self.arena.dim() as usize {
            return Err(RlsError::Kernel(KernelError::DimMismatch {
                expected: self.arena.dim(),
                found: query.len(),
            }));
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(RlsError::Kernel(KernelError::NonFiniteQuery));
        }

        // 事前の失効検出（[`Self::search_with`] と同一契約）。
        let pre_generation = storage
            .current_generation()
            .map_err(|_| RlsError::IndexStale)?;
        if pre_generation != self.built_generation {
            return Err(RlsError::IndexStale);
        }

        // `search_or_fallback` はテーブル世代を `read_txn` から読むため、ここで
        // 1 つ開いて渡す（`storage.current_generation`〔上記の世代照合〕とは
        // 別の読み取り）。
        let read_txn = storage
            .db()
            .begin_read()
            .map_err(|_| RlsError::IndexStale)?;

        // `self.arena` は `built_table_generation` 時点のスナップショットで固定
        // されている。`pre_generation` 確認（上記）から `read_txn` を開くまでの間に
        // 別の書き込みがコミットされていると、`read_txn` は新世代を見るのに
        // `self.arena` は旧世代のままという不整合が起こり得る。この状態のまま
        // `search_or_fallback` を呼ぶと、同関数は `read_txn` から得た新世代番号を
        // 使って旧アリーナから索引を構築・登録してしまい、事後の世代照合
        // （下記）でこの呼び出し自体は `IndexStale` にできても、誤登録した
        // キャッシュエントリは残ってしまう（次の新世代クエリが `Lookup::Ready` で
        // 誤って再利用し得る。codex-review P1 指摘・Issue #409）。そのため
        // `read_txn` のテーブル世代を `built_table_generation` と直接照合し、
        // 一致する場合のみキャッシュ経由の探索を行う。不一致時はキャッシュへ
        // 一切触れず、`search_with` と同じ brute-force 経路へ縮退する
        // （この呼び出しの結果は後述の事後照合でどのみち `IndexStale` になる
        // 見込みが高いが、無駄な索引構築を避けるためここで早期に切り替える）。
        let read_txn_table_generation =
            crate::catalog::table_generation_in_txn(&read_txn, self.arena.table_name());
        let raw = if read_txn_table_generation.ok() == Some(self.built_table_generation) {
            crate::sql::hnsw_cache::search_or_fallback(
                access,
                &read_txn,
                self.arena.table_name(),
                ctx,
                &self.arena,
                &self.slot_ids,
                provider,
                query,
                k,
            )
            .map_err(RlsError::Kernel)?
        } else {
            let input = SearchInput {
                ids: &self.slot_ids,
                vectors: self.arena.vectors(),
                dim: self.arena.dim(),
                query,
                k,
            };
            provider.search(input)?
        };

        if !provider_result_is_valid(&raw, k, &self.visible_id_counts) {
            return Err(RlsError::ProviderResultRejected);
        }
        let hits = crate::core::resolve_slot_hits(&self.arena, &raw)
            .ok_or(RlsError::ProviderResultRejected)?;

        // 事後の失効再検証（[`Self::search_with`] と同一契約）。
        let post_generation = storage
            .current_generation()
            .map_err(|_| RlsError::IndexStale)?;
        if post_generation != self.built_generation {
            return Err(RlsError::IndexStale);
        }

        Ok(hits)
    }

    /// インデックスが保持する可視行数を返す。`ctx` は構築時 `PolicyContext` と完全一致
    /// していなければ [`RlsError::ContextMismatch`]（存在情報を漏らさない。security.md P0）。
    pub(crate) fn len(&self, ctx: &PolicyContext) -> Result<usize, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        Ok(self.arena.len())
    }

    /// 可視行が 0 件かを返す（`ctx` 照合は [`Self::len`] と同じ）。
    pub(crate) fn is_empty(&self, ctx: &PolicyContext) -> Result<bool, RlsError> {
        if ctx != &self.built_ctx {
            return Err(RlsError::ContextMismatch);
        }
        Ok(self.arena.is_empty())
    }

    /// 検索対象ベクトルの次元（`ctx` 不要。テーブル定義由来の非機微情報）。
    pub(crate) fn dim(&self) -> u32 {
        self.arena.dim()
    }

    /// 構築元のテーブル名（`ctx` 不要。呼び出し元が渡した引数の反映）。
    pub(crate) fn table_name(&self) -> &str {
        self.arena.table_name()
    }

    /// 構築時に束縛した `PolicyContext`（`core.rs::PrefilterCache` のキー照合に使う）。
    pub(crate) fn built_ctx(&self) -> &PolicyContext {
        &self.built_ctx
    }

    /// 構築時に読んだストレージ世代（`core.rs::PrefilterCache` の世代整合判定に使う）。
    pub(crate) fn built_generation(&self) -> u64 {
        self.built_generation
    }

    /// このスナップショットが常駐時に占める概算ヒープ使用量
    /// （`core.rs::PrefilterCache` の容量上限判定に使う。[`VectorArena::approx_heap_bytes`]
    /// と `visible_id_set` の概算合計。詳細は同メソッドのドキュメント参照）。
    ///
    /// `visible_id_set: HashSet<u64>` は `len()`（要素数）ではなく `capacity()`
    /// （現在のテーブルサイズで確保済みの容量。未使用分含む）を基準に見積もる
    /// （codex-review P1 対応・PR #191。`len()` ベースは amortized 成長で確保済みの
    /// 未使用容量・swiss table の control byte オーバーヘッドを無視し、実確保量を
    /// 過小評価するため）。`hashbrown`（`std::collections::HashSet` の実装基盤）は
    /// 最大負荷率 7/8 でテーブルを 2 のべき乗サイズに確保するため、実バケット数は
    /// `capacity() * 8 / 7` 以上になる。ここでは `capacity()` をそのまま `8/7` 倍し
    /// 2 のべき乗へ切り上げて実バケット数の保守的な上限を見積もり、1 バケットあたり
    /// 要素（`u64`）8 byte + control byte 1 byte で概算する（実際の確保量を
    /// 下回らない方向に丸める。totalへの過小評価は total 上限による DoS 防御の
    /// 意味を失わせるため、安全側＝上振れに倒す）。
    pub(crate) fn approx_heap_bytes(&self) -> usize {
        let arena_bytes = self.arena.approx_heap_bytes();
        let id_set_bytes =
            hash_set_conservative_bytes::<(u64, usize)>(self.visible_id_counts.capacity());
        // provider 入力用スロット識別子（`slot_ids`）の実確保量も計上する
        // （TABLE-12 対応で追加した保持データ。キャッシュ総量上限の見積もりから
        // 漏らさない）。
        let slot_ids_bytes = self
            .slot_ids
            .capacity()
            .saturating_mul(std::mem::size_of::<u64>());
        arena_bytes
            .saturating_add(id_set_bytes)
            .saturating_add(slot_ids_bytes)
    }
}

/// 事前フィルタ方式の再利用可能インデックス（TASK-133・RLS-1〜4）。
///
/// [`Self::build`] 構築時に束縛した [`PolicyContext`] の可視性述語で
/// [`VectorArena::build_filtered`] を呼び、可視行のみのカラムナ表現を保持する。
/// [`Self::search`] の契約は同メソッドのドキュメント参照。
///
/// [`Self::len`]・[`Self::is_empty`] は可視行数・行の有無という存在情報を返すため、
/// 構築時 `PolicyContext` との完全一致を要求する（不一致は [`RlsError::ContextMismatch`]）。
/// [`Self::dim`]・[`Self::table_name`] は非機微情報のため `ctx` を要求しない。
///
/// 本型は storage 非依存の [`PrefilterSnapshot`] を `&'s Storage` とともに保持するだけの
/// 薄いラッパーで、実処理は `PrefilterSnapshot` 側のメソッドへ委譲する（TASK-169:
/// `core.rs::EngineCore` がキャッシュに保持できるよう `PrefilterSnapshot` を切り出した
/// 経緯は同型のドキュメント参照）。
pub struct PrefilterIndex<'s> {
    inner: PrefilterSnapshot,
    storage: &'s Storage,
}

impl<'s> PrefilterIndex<'s> {
    /// `table` に対し `ctx` の可視性述語で可視行のみのインデックスを構築する（RLS-1）。
    /// テーブル不存在は [`RlsError::NotFound`] へ丸め込む（存在情報を漏らさない。
    /// security.md P0）。容量超過・次元不整合は [`RlsError::Arena`] へ伝播する。
    pub fn build(storage: &'s Storage, table: &str, ctx: &PolicyContext) -> Result<Self, RlsError> {
        let inner = PrefilterSnapshot::build(storage, table, ctx)?;
        Ok(Self { inner, storage })
    }

    /// 保持済みインデックスに対して Top-k 検索を行う（契約は
    /// [`PrefilterSnapshot::search_with`] のドキュメント参照。TASK-133・RLS-1〜4）。
    pub fn search(
        &self,
        ctx: &PolicyContext,
        provider: &dyn SearchProvider,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, RlsError> {
        self.inner
            .search_with(self.storage, ctx, provider, query, k)
    }

    /// インデックスが保持する可視行数を返す（契約は [`PrefilterSnapshot::len`] 参照）。
    pub fn len(&self, ctx: &PolicyContext) -> Result<usize, RlsError> {
        self.inner.len(ctx)
    }

    /// 可視行が 0 件かを返す（契約は [`PrefilterSnapshot::is_empty`] 参照）。
    pub fn is_empty(&self, ctx: &PolicyContext) -> Result<bool, RlsError> {
        self.inner.is_empty(ctx)
    }

    /// 検索対象ベクトルの次元（`ctx` 不要。テーブル定義由来の非機微情報）。
    pub fn dim(&self) -> u32 {
        self.inner.dim()
    }

    /// 構築元のテーブル名（`ctx` 不要。呼び出し元が渡した引数の反映）。
    pub fn table_name(&self) -> &str {
        self.inner.table_name()
    }
}

/// 検索時フィルタ方式の再利用可能インデックス（TASK-134・RLS-1, RLS-3）。
///
/// [`PrefilterIndex`] との本質的な差分は「`PolicyContext` を構築時に束縛しない」こと。
/// [`PrefilterIndex`] のような可視行の縮約ビュー（[`VectorArena`]）は保持しない。
/// [`Self::search`]・[`Self::len`]・[`Self::is_empty`] が呼び出しの都度、単一の
/// `redb::ReadTransaction` 上でテーブル行をストリーミング走査し、`ctx.is_visible` を
/// 通過した行だけを扱う（同一インスタンスをリクエスト単位で異なるポリシーへ使い回せる、
/// 動的ポリシー用のフォールバック）。
///
/// **アリーナ保持方式（旧実装）からの是正（codex P0）**: 旧実装は `ctx` を持たない
/// [`VectorArena::build`]（無フィルタ）でテーブル全行を一括デコードして保持していたため、
/// アリーナ容量上限（[`crate::arena::MAX_ARENA_ROWS`]/[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）
/// の判定基準が「テーブル全体（全テナント合算）の行数・バイト量」になり、対象テナントと
/// 無関係な他テナントの行量だけで検索全体が `CapacityExceeded` になり得た
/// （[`VectorArena::build_filtered`] のドキュメントが「以前のバグとして修正した」と記す
/// cross-tenant 可用性干渉と同種の問題を、この新公開経路で再導入していた）。本実装は
/// アリーナを一切保持せず、[`Self::search`] が行ごとに `tenant_id`/`visibility` を
/// 先に decode して `ctx.is_visible` を評価し、不可視行は embedding の decode・
/// スコア計算を一切行わずスキップする（[`VectorArena::build_filtered`] と同じ
/// 「可視性判定を embedding decode より前に行う」順序。不可視行の破損状態が対象テナントの
/// 検索を失敗させることも、その存在情報を漏らすこともない）。容量上限
/// （[`crate::arena::check_capacity`] を [`crate::arena::MAX_ARENA_ROWS`]/
/// [`crate::arena::MAX_ARENA_TOTAL_BYTES`] で呼ぶ）は可視行の行数・バイト量にのみ適用する
/// ため、他テナント行はカウントに入らず、[`PrefilterIndex`] と同じ「呼び出しテナントの
/// 可視行数」基準で揃う。Top-k 選出は要求 k 件（`k <= `[`MAX_SEARCH_K`]）分のヒープにしか
/// 保持しないため実メモリの無制限確保はそもそも起きないが、容量上限自体は
/// [`PrefilterIndex`] と同一の「呼び出しテナントの論理データ量」契約として適用する。
///
/// **世代の事前・事後照合（[`PrefilterIndex`]）が本型に不要な理由**: [`PrefilterIndex`]
/// は構築時スナップショットを保持し続けるため、`search` 時点のストレージ状態との食い違い
/// （失効）を検出する必要がある。本型はアリーナを保持せず、`search`・`len`・`is_empty`
/// いずれも呼び出しの都度、単一の read トランザクション上でテーブルを走査する
/// （＝走査自体が常に「現在の」スナップショット）ため、構築時と検索時の状態が食い違う
/// という状態自体が生じない。
///
/// 可視性判定とスコア計算は本モジュール（trust boundary 内）で完結させ、不可視行の
/// id・ベクトルを [`SearchProvider`] のアドレス空間へ一切渡さない（provider を経由せず
/// 本型が自前でスコア計算するため、そもそも `SearchProvider` のアドレス空間が存在しない）。
pub struct SearchTimeFilter<'s> {
    storage: &'s Storage,
    table_name: String,
    /// [`Self::build`] 時にカタログスキーマから取得・検証済みの埋め込み次元。
    /// テーブルにベクトル列を追加する経路（`ALTER TABLE`）は存在しないため
    /// （[`crate::catalog::Storage::alter_table_add_column`] は列追加のみ）、
    /// 構築後に変わらない値としてキャッシュしてよい。
    dim: u32,
}

impl<'s> SearchTimeFilter<'s> {
    /// `table` の存在とベクトル次元だけを検証する（行走査は行わない。O(スキーマ) の
    /// コストでテーブル行数に依存しない）。テーブル不存在は [`RlsError::NotFound`] へ
    /// 丸め込む（存在情報を漏らさない。security.md P0。[`PrefilterIndex::build`] と
    /// 同一契約）。
    pub fn build(storage: &'s Storage, table: &str) -> Result<Self, RlsError> {
        let read_txn = storage
            .db()
            .begin_read()
            .map_err(crate::storage::StorageError::from)?;
        let dim = match crate::arena::validated_vector_dim_in_txn(&read_txn, table) {
            Ok(dim) => dim,
            Err(ArenaError::Catalog(CatalogError::TableNotFound(_))) => {
                return Err(RlsError::NotFound)
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            storage,
            table_name: table.to_string(),
            dim,
        })
    }

    /// `ctx` の可視性述語で Top-k 検索を行う（RLS-1: 不許可行の混入 0 件・RLS-3:
    /// over-fetch なしで k 件充足）。契約は上記型ドキュメント参照。
    ///
    /// `k`・`query` の検証（[`PrefilterIndex::search`] と同一契約）の後、単一の
    /// read トランザクション上でテーブル行を走査する。行ごとにまず
    /// [`crate::storage::decode_row_tenant_and_visibility`] で `tenant_id`/`visibility`
    /// のみを decode し、`ctx.is_visible` が偽の行は embedding decode・スコア計算を
    /// 行わずスキップする。可視行は完全 decode 後、可視行数基準の容量検査
    /// （[`crate::arena::check_capacity`]）を経て [`kernel::dot`] でスコア計算し、
    /// [`kernel::TopKSelector::push`] へ渡す。次元不一致（[`ArenaError::DimMismatch`]）は
    /// この時点で呼び出し元 `ctx` から可視と確定した行のみで起こるため、行 id を含めて
    /// fail-closed に伝播してよい（[`PrefilterIndex::build`] と同じ契約。上記型ドキュメント
    /// 参照）。最後に `provider_result_is_valid`（`core.rs`。件数上限・スコア有限性・
    /// 重複なし・順序を検証）で選出結果を機械照合する（違反は
    /// [`RlsError::ProviderResultRejected`]）。
    pub fn search(
        &self,
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, RlsError> {
        self.search_with_limits(
            ctx,
            query,
            k,
            crate::arena::MAX_ARENA_ROWS,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
        )
    }

    /// [`Self::search`] の容量上限パラメータ化版。実装は本関数に集約し、[`Self::search`]
    /// は本番用の定数（[`crate::arena::MAX_ARENA_ROWS`]・[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）
    /// で呼び出すだけの薄いラッパーにする（`arena.rs::VectorArena::build_filtered_with_limits`
    /// と同じ理由: 本番の 1,000,000 行・1 GiB 相当のデータセットをテストごとに用意するのは
    /// 非現実的なため、境界値検証を `#[cfg(test)]` から小さい上限値で再現する）。
    fn search_with_limits(
        &self,
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Vec<SearchHit>, RlsError> {
        validate_search_k(k).map_err(|k| RlsError::InvalidK { k })?;

        if query.len() != self.dim as usize {
            return Err(RlsError::Kernel(KernelError::DimMismatch {
                expected: self.dim,
                found: query.len(),
            }));
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(RlsError::Kernel(KernelError::NonFiniteQuery));
        }

        let Some(table) = self.open_row_table()? else {
            // 1 行も書き込まれていないテーブル（`VectorArena::build` の空アリーナ相当）。
            return Ok(Vec::new());
        };

        let mut selector = kernel::TopKSelector::new(k);
        let mut visible_row_count: usize = 0;
        // 候補識別子は走査中に採番するスキャンローカルのスロット番号（0 起点）。
        // 行 `id` は 1 つの可視集合内で一意とは限らない（対象ビヘイビア: TABLE-12）ため
        // 識別子には使えない。`(tenant_id, id)` はスロット番号を添字とする本 `Vec` に
        // 保持し、最後に選出された高々 `k` 件だけをテナント修飾済みヒットへ解決する
        // （`String` 確保をホットパスである `TopKSelector::push` の外へ出す）。
        let mut visible_rows: Vec<(String, u64)> = Vec::new();
        // 候補識別子の「識別子 → 件数」多重集合（検証ヘルパを `core.rs` と共有するための
        // 形。スロット番号は重複しないため各件数は 1 になる）。
        let mut visible_id_counts: HashMap<u64, usize> = HashMap::new();
        for entry in table.iter().map_err(crate::storage::StorageError::from)? {
            let (key, value) = entry.map_err(crate::storage::StorageError::from)?;
            // 複合キーの第 2 要素が行 `id`（TABLE-12）。
            let (_key_tenant, id) = key.value();
            let buf = value.value();

            // 可視性判定を embedding decode より前に行う（上記型ドキュメント参照。
            // `VectorArena::build_filtered` と同じ順序）。
            // `tenant_id` は `buf` を借用した `&str`（ヒープアロケーションなし。Issue #174）。
            let (tenant_id, visibility) =
                crate::storage::decode_row_tenant_and_visibility(buf).map_err(ArenaError::from)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // ここに到達するのは呼び出し元 `ctx` から可視な行のみ。次元不一致はこの行
            // 自身の id を含めて fail-closed に伝播してよい（呼び出し元が既に到達できる
            // 情報。`PrefilterIndex::build` と同じ契約。上記型ドキュメント参照）。
            let mut row = crate::storage::decode_row(id, buf).map_err(ArenaError::from)?;
            let found_dim =
                u32::try_from(row.embedding.len()).map_err(|_| ArenaError::DimMismatch {
                    id,
                    expected: self.dim,
                    found: u32::MAX,
                })?;
            if found_dim != self.dim {
                return Err(ArenaError::DimMismatch {
                    id,
                    expected: self.dim,
                    found: found_dim,
                }
                .into());
            }

            // 容量上限は可視行のみに適用する（他テナント行はカウントに入らない。
            // 上記型ドキュメント参照）。`arena.rs::check_capacity` をそのまま再利用し、
            // 判定ロジックを重複実装しない。
            visible_row_count = visible_row_count
                .checked_add(1)
                .ok_or(ArenaError::CapacityExceeded)?;
            crate::arena::check_capacity(visible_row_count, self.dim, max_rows, max_bytes)
                .map_err(RlsError::from)?;

            let score = kernel::dot(&row.embedding, query);
            if !score.is_finite() {
                // 格納ベクトルの NaN/Inf 混入・オーバーフローによる非有限化を除外する
                // （`kernel.rs::CpuScalarProvider` と同方針）。
                continue;
            }

            // スロット採番は「実際に選出候補へ入れる行」に限る（非有限スコアで
            // スキップした行は採番しない）。確保はフォールブルにし、上限は直前の
            // `check_capacity` が可視行数に対して既に検証している。
            let slot = visible_rows.len();
            let slot_id = u64::try_from(slot).map_err(|_| ArenaError::CapacityExceeded)?;
            visible_rows.try_reserve(1).map_err(|e| {
                ArenaError::AllocationFailed(format!("failed to reserve row key: {e}"))
            })?;
            // `row` はこの行の走査でしか使わないため、`tenant_id` は clone せず所有権ごと
            // 移す（可視行ごとに走る経路で短命な `String` を二重確保しない）。
            visible_rows.push((std::mem::take(&mut row.tenant_id), id));
            let counted = visible_id_counts.entry(slot_id).or_insert(0);
            *counted = counted.saturating_add(1);

            selector.push(CandidateHit { id: slot_id, score });
        }
        let hits = selector.into_sorted_vec();

        // 返却直前の機械検証（件数上限・スコア有限性・重複上限・順序の 4 点）。
        // `PrefilterIndex::search` と異なり本型は `dyn SearchProvider` を経由せず、
        // `hits` は上記ループで `ctx.is_visible` を通過した行からのみ inline 生成される
        // ため (3) は常に真になるが、(4)（同じ行を複数回返していないこと）を実効的に
        // 保つため、多重集合は `hits` 自身ではなく走査中に数えた可視行の実件数から
        // 構築する（TABLE-12 の重複 id の扱いは `core::provider_result_is_valid` 参照）。
        if !provider_result_is_valid(&hits, k, &visible_id_counts) {
            return Err(RlsError::ProviderResultRejected);
        }

        // 選出された候補（スロット番号）を `(tenant_id, id)` のテナント修飾済みヒットへ
        // 解決する（対象ビヘイビア: TABLE-12・RLS-9）。スロットが範囲外なら部分結果を
        // 返さず fail-closed に拒否する。
        let mut out = Vec::new();
        out.try_reserve_exact(hits.len())
            .map_err(|e| ArenaError::AllocationFailed(format!("failed to reserve hits: {e}")))?;
        for hit in &hits {
            let slot = usize::try_from(hit.id).map_err(|_| RlsError::ProviderResultRejected)?;
            let (tenant_id, row_id) = visible_rows
                .get(slot)
                .ok_or(RlsError::ProviderResultRejected)?;
            out.push(SearchHit::new(tenant_id.as_str(), *row_id, hit.score));
        }
        Ok(out)
    }

    /// `ctx` の可視性述語で数えた可視行数を返す（存在情報のため `ctx` 必須。
    /// [`PrefilterIndex::len`] と異なり `ContextMismatch` は返さず、渡された `ctx` で
    /// 都度判定する——[`Self`] は構築時にポリシーを束縛しないため）。ヘッダのみ decode
    /// して数える全件走査（embedding decode・容量上限検査は行わない。カウントのみで
    /// 追加のアロケーションを伴わないため）。
    ///
    /// **[`Self::search`] の返却件数との乖離（片方向のみ）**: `is_empty(ctx)` が `true`
    /// なら `search` は必ず 0 件を返す（両者とも同じ `ctx.is_visible` 述語で判定する
    /// ため）。逆に `len(ctx) >= 1`（`is_empty(ctx)` が `false`）であっても `search` が
    /// 0 件になり得る——`search` はさらに、次元不一致行（fail-closed に `Err` を返す）・
    /// スコアが非有限になる行（格納ベクトルへの NaN/Inf 混入）も除外するため
    /// （[`Self::search`] 参照）。`len`/`is_empty` はこれらを判定しない。
    /// [`PrefilterIndex::len`]/[`PrefilterIndex::is_empty`] も同じ片方向の乖離を持つ
    /// （`search` に渡した `SearchProvider` が破損行・非有限スコア行を除外しうるため）。
    pub fn len(&self, ctx: &PolicyContext) -> Result<usize, RlsError> {
        let Some(table) = self.open_row_table()? else {
            return Ok(0);
        };
        let mut count: usize = 0;
        for entry in table.iter().map_err(crate::storage::StorageError::from)? {
            let (_key, value) = entry.map_err(crate::storage::StorageError::from)?;
            let (tenant_id, visibility) =
                crate::storage::decode_row_tenant_and_visibility(value.value())
                    .map_err(ArenaError::from)?;
            if ctx.is_visible(tenant_id, visibility) {
                count = count.checked_add(1).ok_or(ArenaError::CapacityExceeded)?;
            }
        }
        Ok(count)
    }

    /// 可視行が 0 件かを返す（`ctx` 判定は [`Self::len`] と同じ述語。先頭から可視行が
    /// 見つかり次第打ち切るため全件走査の [`Self::len`] より軽い）。[`Self::search`] との
    /// 乖離は [`Self::len`] のドキュメント参照。
    pub fn is_empty(&self, ctx: &PolicyContext) -> Result<bool, RlsError> {
        let Some(table) = self.open_row_table()? else {
            return Ok(true);
        };
        for entry in table.iter().map_err(crate::storage::StorageError::from)? {
            let (_key, value) = entry.map_err(crate::storage::StorageError::from)?;
            let (tenant_id, visibility) =
                crate::storage::decode_row_tenant_and_visibility(value.value())
                    .map_err(ArenaError::from)?;
            if ctx.is_visible(tenant_id, visibility) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 検索対象ベクトルの次元（`ctx` 不要。テーブル定義由来の非機微情報）。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 構築元のテーブル名（`ctx` 不要。呼び出し元が渡した引数の反映）。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// `self.table_name` の行テーブルを新しい read トランザクション上で開く
    /// （[`Self::search`]・[`Self::len`]・[`Self::is_empty`] 共通のエントリポイント）。
    /// 1 行も書き込まれていない（テーブル未作成）場合は `Ok(None)`
    /// （[`VectorArena::build`] の空アリーナ相当）。`redb::ReadOnlyTable` は内部で
    /// トランザクションガードを `Arc` 保持するため、返り値は呼び出し元が `read_txn` を
    /// 生かし続けなくても単独で使える（`redb::ReadTransaction::open_table` の戻り値契約）。
    #[allow(clippy::type_complexity)]
    fn open_row_table(
        &self,
    ) -> Result<Option<redb::ReadOnlyTable<(&'static str, u64), &'static [u8]>>, RlsError> {
        let read_txn = self
            .storage
            .db()
            .begin_read()
            .map_err(crate::storage::StorageError::from)?;
        let row_table_name = crate::catalog::user_rows_table_name(&self.table_name);
        // 物理キーは `(tenant_id, id)`（対象ビヘイビア: TABLE-12）。走査はテーブル全行を
        // 対象とし（テナントで絞らない）、可視性判定は `ctx.is_visible` の単一照合パスへ
        // 委譲する契約を変えない。旧フォーマット DB は
        // `catalog::map_row_table_error` 経由で fail-closed に拒否する。
        match read_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name)) {
            Ok(t) => Ok(Some(t)),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(RlsError::Arena(ArenaError::Catalog(
                crate::catalog::map_row_table_error(e),
            ))),
        }
    }
}

/// RLS 実行時安全網（TASK-136・RLS-5）。`sql::exec` の DISTANCE 段（および SCALAR
/// 事後フィルタ）を通過した最終 `hits`（`(id, score)` の順序付き列）を、束縛済み
/// [`PolicyContext`] で再判定する第 2 層の防御。判定は必ず
/// [`PolicyContext::is_visible`] へ委譲し、本モジュール独自のテナント比較を新設
/// しない（[`PrefilterIndex`]・[`SearchTimeFilter`] と同方針。security.md P0）。
///
/// `HINT ORDER` の内容・`rls_predicate_present` の有無に関係なく `sql::exec` から
/// 無条件に呼ばれる契約（呼び出し元がこの適用を分岐させる余地を API に持たせない）。
#[must_use]
pub struct RlsSafetyNet<'c> {
    ctx: &'c PolicyContext,
}

impl<'c> RlsSafetyNet<'c> {
    /// 束縛済み `PolicyContext` を保持する安全網を構築する。
    pub fn new(ctx: &'c PolicyContext) -> Self {
        RlsSafetyNet { ctx }
    }

    /// `hits` の相対順序を保ちつつ、`is_visible` が `false` の行、および
    /// `label_of` がラベルを引けない行（データ不整合。fail-closed に除去）を除く。
    /// `label_of` は候補構築時と同一スナップショット（`arena`）由来の借用
    /// `&str` を返す想定で、ヒット単位の `String` 確保を発生させない。
    pub fn apply<'a, F>(&self, hits: Vec<(u64, f64)>, label_of: F) -> RlsVerifiedHits
    where
        F: Fn(u64) -> Option<(&'a str, Visibility)>,
    {
        let original_len = hits.len();
        let filtered: Vec<(u64, f64)> = hits
            .into_iter()
            .filter(|(id, _)| match label_of(*id) {
                Some((tenant, visibility)) => self.ctx.is_visible(tenant, visibility),
                None => false,
            })
            .collect();
        let dropped = original_len.saturating_sub(filtered.len());
        RlsVerifiedHits {
            hits: filtered,
            dropped,
        }
    }
}

/// [`RlsSafetyNet::apply`] を通過した hits だけが持てる witness 型。構築経路は
/// [`RlsSafetyNet::apply`] のみに限定する（`Default`・`From<Vec<_>>` は実装しない）。
/// `sql::exec` の投影段はこの型からしか hits を取り出せないため、安全網を経由
/// しない生の `Vec<(u64, f64)>` から投影へ到達する経路を型として作れない。
#[must_use]
pub struct RlsVerifiedHits {
    hits: Vec<(u64, f64)>,
    dropped: usize,
}

impl RlsVerifiedHits {
    /// 検証済み hits を借用で返す。
    pub fn hits(&self) -> &[(u64, f64)] {
        &self.hits
    }

    /// 検証済み hits を所有権ごと取り出す。
    pub fn into_hits(self) -> Vec<(u64, f64)> {
        self.hits
    }

    /// 検証済み hits の件数。
    pub fn len(&self) -> usize {
        self.hits.len()
    }

    /// 検証済み hits が空か。
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// 安全網が除去した件数。0 でなければ事前フィルタの迂回を示す観測値だが、
    /// エラー・応答へは載せない（他テナントの存在情報を漏らさない。
    /// security.md P0）。テスト・将来の内部メトリクス用。
    pub fn dropped(&self) -> usize {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::kernel::CpuScalarProvider;
    use crate::storage::{RowInput, Visibility};

    fn schema_for(table_name: &str, dim: u32) -> TableSchema {
        TableSchema::new(
            table_name,
            vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
        )
    }

    // 一時ディレクトリ（`TempDir` / `tempdir()`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: `SEQ` 通番対策込みでこのモジュール内に
    // 複製していたが、`DatabaseAlreadyOpen` フレーク対策が他の複製へ波及しなかったため）。
    use crate::test_util::temp_db::tempdir;

    fn open_storage(dir: &std::path::Path) -> Storage {
        Storage::open(dir.join("db.redb")).expect("open storage")
    }

    fn insert(storage: &Storage, table: &str, id: u64, tenant: &str, vis: Visibility, v: &[f32]) {
        storage
            .insert_row_into_table(
                table,
                id,
                &RowInput {
                    tenant_id: tenant,
                    visibility: vis,
                    embedding: v,
                    metadata: &[],
                },
            )
            .expect("insert row");
    }

    // 対象ビヘイビア: RLS-1。他テナント・不可視行は構築時点でインデックスへ含まれず、
    // 検索結果にも混入しない。他テナント行の不可視性を検証する行は `Private` にする
    // （ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn build_excludes_invisible_rows_and_search_never_returns_them() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Private,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");
        assert_eq!(index.len(&ctx).expect("len ok"), 1);

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 対象ビヘイビア: RLS-1。可視行が 0 件でも空結果を返す（拒否ではない）。
    // 可視行 0 件を保つ行は `Private` にする（ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn empty_visible_set_returns_empty_result() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-b",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");
        assert!(index.is_empty(&ctx).expect("is_empty ok"));

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 5)
            .expect("search ok");
        assert!(hits.is_empty());
    }

    // `core.rs::EngineCore::search`/`get_row` と対称: テーブル不存在は他テナントの
    // 存在情報を漏らさず `RlsError::NotFound` へ丸め込まれ、`Display` にテーブル名を
    // 含まない（本 Issue のレビュー指摘対応）。
    #[test]
    fn build_returns_not_found_without_leaking_table_name_for_missing_table() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let result = PrefilterIndex::build(&storage, "no_such_table", &ctx);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("missing table must be rejected"),
        };
        assert!(matches!(err, RlsError::NotFound));
        assert_eq!(err.to_string(), "not found");
        assert!(!err.to_string().contains("no_such_table"));
    }

    // `PrefilterSnapshot::build` はアリーナ構築前にテーブル単位世代を読むが、その経路でも
    // テーブル不存在は `RlsError::NotFound` へ丸め込まれ、`RlsError::Arena` 経由で存在情報を
    // 漏らさない（PR #435 Bugbot 指摘対応）。
    #[test]
    fn snapshot_build_returns_not_found_without_leaking_table_name_for_missing_table() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let result = PrefilterSnapshot::build(&storage, "no_such_table", &ctx);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("missing table must be rejected"),
        };
        assert!(matches!(err, RlsError::NotFound));
        assert_eq!(err.to_string(), "not found");
        assert!(!err.to_string().contains("no_such_table"));
    }

    #[test]
    fn search_rejects_k_zero_and_over_limit() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 0),
            Err(RlsError::InvalidK { k: 0 })
        ));
        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], MAX_SEARCH_K + 1),
            Err(RlsError::InvalidK { .. })
        ));
    }

    #[test]
    fn search_rejects_dim_mismatch_and_non_finite_query() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0, 0.0], 1),
            Err(RlsError::Kernel(KernelError::DimMismatch { .. }))
        ));
        assert!(matches!(
            index.search(&ctx, &CpuScalarProvider, &[f32::NAN, 0.0], 1),
            Err(RlsError::Kernel(KernelError::NonFiniteQuery))
        ));
    }

    // ctx 束縛の検証: 同一テーブルでも構築時 ctx のテナントに紐づく行しか返らない
    // （一致 ctx での正常系）。テナントごとの束縛を検証する行は `Private` にする
    // （ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn index_is_bound_to_the_tenant_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");
        let hits = index_a
            .search(&ctx_a, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert!(hits.iter().all(|h| h.id == 1));

        let ctx_b = PolicyContext::with_visibilities("tenant-b", [Visibility::Private])
            .expect("valid tenant");
        let index_b = PrefilterIndex::build(&storage, "docs", &ctx_b).expect("build index");
        let hits = index_b
            .search(&ctx_b, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert!(hits.iter().all(|h| h.id == 2));
    }

    // 構築時とは別テナントの ctx でインデックスを転用しようとした場合、可視行が
    // 存在していても `RlsError::ContextMismatch` で拒否される。
    #[test]
    fn search_rejects_a_different_tenant_context_than_the_one_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");

        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let result = index_a.search(&ctx_b, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::ContextMismatch)));
    }

    // 構築時とは許可可視性集合が狭い ctx（Private 許可の取り消し）は転用とみなし拒否する。
    #[test]
    fn search_rejects_a_context_narrowed_from_build_time_visibility() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx_private =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx_private).expect("build index");

        let ctx_narrowed = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(matches!(
            index.search(&ctx_narrowed, &CpuScalarProvider, &[1.0, 0.0], 10),
            Err(RlsError::ContextMismatch)
        ));

        // 構築時と完全一致する ctx（別インスタンスだが値は等しい）は受理される。
        let ctx_same =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert_eq!(ctx_same, ctx_private);
        let hits = index
            .search(&ctx_same, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("identical context must be accepted");
        assert_eq!(hits.len(), 1);
    }

    // 構築時よりも許可可視性集合が広い ctx も転用とみなし拒否する。
    #[test]
    fn search_rejects_a_context_widened_from_build_time_visibility() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx_public_only = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx_public_only).expect("build index");

        let ctx_widened =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert!(matches!(
            index.search(&ctx_widened, &CpuScalarProvider, &[1.0, 0.0], 10),
            Err(RlsError::ContextMismatch)
        ));
    }

    // 不正 provider（可視集合外の id を捏造して返す）は fail-closed に拒否される
    // （`core.rs::CoreError::ProviderResultRejected` と同一契約の再現）。
    struct RogueProvider;
    impl SearchProvider for RogueProvider {
        fn search(&self, _input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            Ok(vec![CandidateHit {
                id: 9_999_999,
                score: 1.0,
            }])
        }
    }

    #[test]
    fn rogue_provider_result_outside_visible_set_is_rejected() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let result = index.search(&ctx, &RogueProvider, &[1.0, 0.0], 1);
        assert!(matches!(result, Err(RlsError::ProviderResultRejected)));
    }

    // 構築後に行の tenant_id がストレージ側で書き換わった場合、同一 ctx で検索しても
    // 旧行を返さず `RlsError::IndexStale` で拒否する。
    #[test]
    fn search_rejects_when_a_hit_row_tenant_changed_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後に行 1 を他テナントへ書き換える（同一 id への upsert）。
        insert(
            &storage,
            "docs",
            1,
            "tenant-b",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // テーブルを drop すると、drop 前に構築したインデックスへの検索は世代不一致により
    // `RlsError::IndexStale` で拒否される（Issue #179。旧行を返す経路がないことの確認）。
    #[test]
    fn search_rejects_after_table_is_dropped() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        storage.drop_table("docs").expect("drop table");

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // drop → 同名・同次元での再作成をまたいでも、旧インデックスは stale のまま。
    // 新規に構築したインデックスは再作成後の新規行のみを返す（旧行を含まない）。
    #[test]
    fn search_rejects_after_table_is_dropped_and_recreated_with_new_rows() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let stale_index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        storage.drop_table("docs").expect("drop table");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("recreate table");
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let stale_result = stale_index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(stale_result, Err(RlsError::IndexStale)));

        let fresh_index = PrefilterIndex::build(&storage, "docs", &ctx).expect("rebuild index");
        let hits = fresh_index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search succeeds on fresh index");
        let ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![2]);
    }

    // tenant_id は変わらず visibility だけが構築時 ctx の許可範囲外へ書き換わった場合も
    // 同様に `RlsError::IndexStale` で拒否する。
    #[test]
    fn search_rejects_when_a_hit_row_visibility_changed_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後に行 1 の visibility を ctx が許可しない Private へ書き換える。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // tenant_id・visibility は変わらず embedding だけが構築後に書き換わった場合も
    // 世代カウンタの不一致により `RlsError::IndexStale` で拒否する（TASK-133 P1 対応）。
    #[test]
    fn search_rejects_when_embedding_only_is_updated_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // tenant/visibility は同一のまま embedding だけを書き換える。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[0.0, 1.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // 構築後に `crate::txn::WriteTxn` 経由で行を上書きした場合も、`ROWS_TABLE` 直接の
    // `insert_row_into_table`（catalog.rs 経由）と同様に世代カウンタの不一致により
    // `RlsError::IndexStale` で拒否する（Issue #175。「実書き込みの有無」判定の消費者側
    // 退行検査。世代はストレージ全体で 1 つのため、対象テーブル外の書き込みでも失効する
    // 現行契約自体は変えない）。
    #[test]
    fn search_rejects_after_write_txn_overwrite_commit() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // WriteTxn 経由で行 1 の embedding を上書きする（RowInput の tenant_id は
        // catalog.rs 側テーブル名プレフィックスの都合上、insert 側と別経路の
        // 生 put のため id は catalog 側と衝突しない別 id を使う）。
        let mut txn = storage.begin_write().expect("begin_write");
        txn.put(
            1_000_000,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[0.0, 1.0],
                metadata: &[],
            },
        )
        .expect("put");
        txn.commit().expect("commit with put");

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // put を 1 度も呼ばない `WriteTxn` の commit（no-op commit）は世代を進めないため
    // （Issue #175）、構築済みインデックスはそのまま `search` を継続できる。
    // 本 Issue の効果（実書き込みなし commit で世代を不変にすることによる過剰失効の
    // 削減）を消費者側で固定する。
    #[test]
    fn search_still_succeeds_after_noop_write_txn_commit() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let txn = storage.begin_write().expect("begin_write");
        txn.commit().expect("commit without put");

        let result = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search should still succeed after no-op commit");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 1);
    }

    // 構築後に新規可視行が挿入された場合も世代カウンタの不一致により
    // `RlsError::IndexStale` で拒否する（TASK-133 P1 対応）。
    #[test]
    fn search_rejects_when_a_new_visible_row_is_inserted_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後に ctx から可視な新規行を挿入する。
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.0, 1.0],
        );

        let result = index.search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // 未変更のストレージに対しては引き続き正常にヒットを返すことを確認する
    // （再検証の追加で過剰拒否（over-rejection）を起こしていないことのガード）。
    #[test]
    fn search_still_returns_hits_when_storage_is_unchanged_since_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let hits = index
            .search(&ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 行不存在（削除相当）の再検証パスは `catalog.rs::tests::` 側でカバーする
    // （`get_row_headers_from_table` の `None` 分岐）。

    // `len`/`is_empty` は構築時 ctx との一致を要求し、別テナントの ctx（tenant-b。
    // 自身の可視行を持つ）を渡した場合はどちらも `RlsError::ContextMismatch` になる。
    // `index_a.len(&ctx_a) == 1` という前提を保つため tenant-b 側は `Private` にする
    // （ポインタ: TASK-89 / TABLE-9）。
    #[test]
    fn len_and_is_empty_reject_a_context_different_from_the_one_used_at_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let index_a = PrefilterIndex::build(&storage, "docs", &ctx_a).expect("build index");

        // 一致する ctx では引き続き正常に値を返す（過剰拒否のガード）。
        assert_eq!(index_a.len(&ctx_a).expect("len ok"), 1);
        assert!(!index_a.is_empty(&ctx_a).expect("is_empty ok"));

        // 別テナントの ctx（tenant-b。tenant-a のインデックスへ渡す）はどちらも拒否する。
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        assert!(matches!(
            index_a.len(&ctx_b),
            Err(RlsError::ContextMismatch)
        ));
        assert!(matches!(
            index_a.is_empty(&ctx_b),
            Err(RlsError::ContextMismatch)
        ));
    }

    /// 呼び出し回数を記録してから [`CpuScalarProvider`] へ委譲する計装 provider
    /// （`tests/rls_prefilter.rs::CountingProvider` と同型の複製）。
    struct RecordingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl RecordingProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl SearchProvider for RecordingProvider {
        fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            CpuScalarProvider.search(input)
        }
    }

    // 失効した行が Top-k の外であっても provider は一切呼ばれないことを検証する。
    #[test]
    fn search_rejects_before_calling_provider_even_when_the_stale_row_is_outside_top_k() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        // クエリ [1.0, 0.0] に対する内積スコア: id=1 が最高位（1.0）、id=2 が中位（0.5）、
        // id=3 が最下位（0.1）。k=1 なら id=3 は本来 Top-k に入らない。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Public,
            &[0.1, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        // 構築後、Top-1 には入らない id=3 だけを他テナントへ書き換える。
        insert(
            &storage,
            "docs",
            3,
            "tenant-b",
            Visibility::Public,
            &[0.1, 0.0],
        );

        let provider = RecordingProvider::new();
        let result = index.search(&ctx, &provider, &[1.0, 0.0], 1);
        assert!(matches!(result, Err(RlsError::IndexStale)));
        assert_eq!(
            provider.call_count(),
            0,
            "provider must not be called once any arena row fails the pre-check"
        );
    }

    // ストレージが構築後に変化していない場合は provider がちょうど 1 回呼ばれる
    // （過剰拒否のガード）。
    #[test]
    fn search_calls_provider_exactly_once_when_no_row_is_stale() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let provider = RecordingProvider::new();
        let hits = index
            .search(&ctx, &provider, &[1.0, 0.0], 1)
            .expect("search ok");
        assert_eq!(provider.call_count(), 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    /// provider 自身が `search` 呼び出し内で（`self.storage` を通じて）ストレージへ
    /// 書き込みコミットを行う計装 provider。事前の世代確認〜事後の世代再確認の間に
    /// 別の書き込みが割り込むケースを決定的に再現するために使う。
    struct WritingDuringSearchProvider<'s> {
        storage: &'s Storage,
    }
    impl SearchProvider for WritingDuringSearchProvider<'_> {
        fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
            self.storage
                .insert_row_into_table(
                    "docs",
                    999,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[0.0, 0.0],
                        metadata: &[],
                    },
                )
                .expect("write during provider search");
            CpuScalarProvider.search(input)
        }
    }

    // 事後の世代再検証（codex 指摘対応）: provider 実行中にストレージへの書き込みが
    // コミットされた場合、事前確認は通過していても事後の世代再確認で不一致が検出され
    // `RlsError::IndexStale` になることを検証する。
    #[test]
    fn search_rejects_when_storage_is_written_to_during_provider_execution() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let index = PrefilterIndex::build(&storage, "docs", &ctx).expect("build index");

        let provider = WritingDuringSearchProvider { storage: &storage };
        let result = index.search(&ctx, &provider, &[1.0, 0.0], 10);
        assert!(matches!(result, Err(RlsError::IndexStale)));
    }

    // ------------------------------------------------------------------
    // SearchTimeFilter（TASK-134・RLS-1, RLS-3）
    // ------------------------------------------------------------------

    // 対象ビヘイビア: RLS-1（TASK-89 / TABLE-9 の可視性判定を前提とする）。複数テナント・
    // 可視性混在データで、検索結果に不許可行の混入が 0 件であること（結果全件を
    // `ctx.is_visible` で機械照合する）。他テナント・不可視行の判別には `Private` を使う
    // （`Public` は TASK-89 でテナント横断の共有可視性へ変わったため、`Public` では
    // 他テナント不可視の判別ができない。`PrefilterIndex` 側の同種テストと同方針）。
    #[test]
    fn search_time_filter_never_returns_invisible_rows() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Private,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );

        let ctx = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 10).expect("search ok");
        // id=2（他テナントの Private）・id=3（自テナントだが Public で ctx 未許可）は
        // いずれも混入しない（RLS-1）。
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 対象ビヘイビア: RLS-1（動的ポリシー・TASK-89 / TABLE-9 の可視性判定を前提とする）。
    // 同一 `SearchTimeFilter` インスタンスに対し異なる `PolicyContext` で連続検索し、
    // 各回とも当該 ctx の可視行のみが返ること（再構築不要のフォールバック特性の確認。
    // `PrefilterIndex` はこの用途では ctx ごとに再構築が必要）。テナントごとの分離を
    // 判別するため `Private` を使う（`Public` はテナント横断の共有可視性のため判別に
    // 使えない）。
    #[test]
    fn search_time_filter_reuses_the_same_instance_across_different_policy_contexts() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        let hits_a = filter.search(&ctx_a, &[1.0, 0.0], 10).expect("search ok");
        assert_eq!(hits_a.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1]);

        let ctx_b = PolicyContext::with_visibilities("tenant-b", [Visibility::Private])
            .expect("valid tenant");
        let hits_b = filter.search(&ctx_b, &[1.0, 0.0], 10).expect("search ok");
        assert_eq!(hits_b.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);
    }

    // 対象ビヘイビア: RLS-3。可視行数 >= k のとき結果がちょうど k 件で、単一パスのみで
    // 充足する（over-fetch なし）。
    #[test]
    fn search_time_filter_returns_exactly_k_hits_when_enough_visible_rows_exist() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        for (id, x) in [(1, 1.0), (2, 0.8), (3, 0.6), (4, 0.4)] {
            insert(
                &storage,
                "docs",
                id,
                "tenant-a",
                Visibility::Public,
                &[x, 0.0],
            );
        }
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 2).expect("search ok");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 1);
        assert_eq!(hits[1].id, 2);
    }

    // 対象ビヘイビア: RLS-3。可視行数 < k のときは可視行全件を返し、エラーにしない。
    #[test]
    fn search_time_filter_returns_all_visible_rows_when_fewer_than_k() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 10).expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 対象ビヘイビア: RLS-1 と RLS-3 の判別テスト（本 Issue のレビュー指摘対応・TASK-89 /
    // TABLE-9 の可視性判定を前提とする）。不可視行が可視行よりスコア上位に来て k 枠を
    // 奪う状況を作る（tenant-b の 2 行が最上位スコアを占め、ctx=tenant-a・k=2 では
    // tenant-a の 2 行のみが正解）。「全行で top-k を選んでから可視性でフィルタする」
    // 誤実装（可視性を最後に後付けする RLS-3 違反の典型パターン）だと、上位 k 件が
    // すべて不可視行で埋まり最終結果が空になる。可視性判定をスコア計算前に行う正しい
    // 実装でのみ `[3, 4]` が返る。tenant-b の行は `Private` にする（`Public` はテナント
    // 横断の共有可視性のため他テナント不可視の判別に使えない）。
    #[test]
    fn search_time_filter_visibility_is_applied_before_top_k_selection_not_after() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-b",
            Visibility::Private,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Private,
            &[0.9, 0.0],
        );
        insert(
            &storage,
            "docs",
            3,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );
        insert(
            &storage,
            "docs",
            4,
            "tenant-a",
            Visibility::Public,
            &[0.4, 0.0],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 2).expect("search ok");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 3);
        assert_eq!(hits[1].id, 4);
    }

    // fail-closed 系: k == 0 / MAX_SEARCH_K 超過は InvalidK。
    #[test]
    fn search_time_filter_rejects_k_zero_and_over_limit() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        assert!(matches!(
            filter.search(&ctx, &[1.0, 0.0], 0),
            Err(RlsError::InvalidK { k: 0 })
        ));
        assert!(matches!(
            filter.search(&ctx, &[1.0, 0.0], MAX_SEARCH_K + 1),
            Err(RlsError::InvalidK { .. })
        ));
    }

    // fail-closed 系: 次元不一致・非有限クエリは Kernel(...)。
    #[test]
    fn search_time_filter_rejects_dim_mismatch_and_non_finite_query() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        assert!(matches!(
            filter.search(&ctx, &[1.0, 0.0, 0.0], 1),
            Err(RlsError::Kernel(KernelError::DimMismatch { .. }))
        ));
        assert!(matches!(
            filter.search(&ctx, &[f32::NAN, 0.0], 1),
            Err(RlsError::Kernel(KernelError::NonFiniteQuery))
        ));
    }

    // fail-closed 系: 存在しないテーブルは NotFound（存在情報を漏らさない）。
    #[test]
    fn search_time_filter_build_returns_not_found_for_missing_table() {
        let dir = tempdir();
        let storage = open_storage(dir.path());

        let result = SearchTimeFilter::build(&storage, "no_such_table");
        assert!(matches!(result, Err(RlsError::NotFound)));
    }

    // 対象ビヘイビア: RLS-1（構造的是正: ストリーミング走査への切り替え）。
    // `SearchTimeFilter::build` は行走査自体を行わないため、次元不一致で壊れた行が
    // 呼び出し元とは無関係な他テナント（tenant-b）に属していても `build` 自体は成功する。
    // さらに `search`（ctx=tenant-a）はその破損行を可視性判定の時点で除外し、embedding の
    // decode を一切行わないため、破損に触れずエラーにもならず正常な結果を返す
    // （他テナントの破損状態が対象テナントの検索可用性へ干渉しないことの直接確認）。
    // `insert_row_into_table` は挿入時点で次元検証するため、`arena.rs::tests` と同じ手法
    // （検証を経由しない生の write トランザクション）で次元不一致行を直接書き込む。
    #[test]
    fn search_time_filter_skips_a_foreign_tenants_corrupted_row_without_decoding_it() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create table");
        // tenant-a の正常行（呼び出し元想定のテナント）。
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 2.0, 3.0, 4.0],
        );
        // 呼び出し元とは無関係な tenant-b の行 id=2 を、次元不一致状態で直接書き込む。
        // `Private`（テナント横断で不可視。TASK-89 / TABLE-9 で `Public` はテナント横断の
        // 共有可視性になったため、可視性の分離判別には `Private` を使う）にする。
        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let row_table_name = crate::catalog::user_rows_table_name("docs");
                let mut row_table = write_txn
                    .open_table(crate::catalog::user_rows_table_def(&row_table_name))
                    .expect("open row table");
                let encoded = crate::storage::encode_row(&RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: &[1.0, 2.0],
                    metadata: &[],
                })
                .expect("encode mismatched-dim row for tenant-b");
                row_table
                    .insert(("tenant-b", 2u64), encoded.as_slice())
                    .expect("insert mismatched-dim row bypassing dim validation");
            }
            write_txn.commit().expect("commit mismatched-dim row");
        }

        // build は行走査をしないため、他テナントの破損行があっても成功する。
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build must not scan rows");

        // ctx=tenant-a（既定は Public のみ許可）は tenant-b の Private 行（id=2）を
        // 不可視にする。破損行 id=2 は可視性判定の時点でスキップされ embedding decode に
        // 到達しないため、search はエラーにならない。
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let hits = filter
            .search(&ctx, &[1.0, 2.0, 3.0, 4.0], 10)
            .expect("corrupted foreign-tenant row must not affect this tenant's search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
    }

    // 対象ビヘイビア: RLS-1（codex P0 の直接的な再現テスト）。容量上限（ここでは
    // `search_with_limits` 経由で小さい `max_rows` を注入）が「呼び出しテナントの可視行数」
    // 基準であり、対象テナントと無関係な他テナントの行量に左右されないことを検証する
    // （`arena.rs::build_filtered_capacity_check_is_based_on_visible_rows_not_total_table_rows`
    // と同じ構造）。tenant-b（不可視想定）の行を上限を上回る本数だけ挿入しても、
    // tenant-a（可視想定）の行が上限以下なら search は成功する。
    #[test]
    fn search_time_filter_capacity_check_is_based_on_visible_rows_not_total_table_rows() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");

        // tenant-b（不可視想定）の行を、可視行数の上限（3）を上回る本数だけ挿入する。
        for id in 0..8u64 {
            insert(
                &storage,
                "docs",
                id,
                "tenant-b",
                Visibility::Private,
                &[9.0, 9.0],
            );
        }
        // tenant-a（可視想定）の行は上限（3）以下の 2 件のみ。
        for id in 8..10u64 {
            insert(
                &storage,
                "docs",
                id,
                "tenant-a",
                Visibility::Public,
                &[1.0, 1.0],
            );
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let max_rows = 3usize;
        let max_bytes = usize::MAX; // 本テストでは行数上限だけを検証対象にする。
        let hits = filter
            .search_with_limits(&ctx, &[1.0, 1.0], 10, max_rows, max_bytes)
            .expect(
                "capacity check must be based on the 2 visible rows (<= max_rows), \
                 not the 10 total table rows (> max_rows)",
            );
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.id == 8 || h.id == 9));
    }

    // 対象ビヘイビア: RLS-1。可視行数自体が上限を超えるテナントの検索は、他テナントの
    // 存在に関係なく `CapacityExceeded` で拒否される（fail-closed。無制限確保を防ぐ本来の
    // 目的が保たれていることの確認）。
    #[test]
    fn search_time_filter_capacity_check_rejects_when_own_visible_rows_exceed_the_limit() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");

        for id in 0..4u64 {
            insert(
                &storage,
                "docs",
                id,
                "tenant-a",
                Visibility::Public,
                &[1.0, 1.0],
            );
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let max_rows = 3usize; // 可視行 4 件 > 上限 3 件。
        let max_bytes = usize::MAX;
        let result = filter.search_with_limits(&ctx, &[1.0, 1.0], 10, max_rows, max_bytes);
        assert!(matches!(
            result,
            Err(RlsError::Arena(ArenaError::CapacityExceeded))
        ));
    }

    // 世代の事前・事後照合を撤廃したことの直接確認（上記型ドキュメント参照）:
    // `build` 後に別の書き込みがコミットされていても、`search` は都度ストレージを
    // ストリーミング走査するため、新規行を含めた最新状態を返す（`IndexStale` にはならない）。
    #[test]
    fn search_time_filter_reflects_writes_committed_after_build() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.5, 0.0],
        );

        let hits = filter
            .search(&ctx, &[1.0, 0.0], 10)
            .expect("search must reflect the write committed after build");
        assert_eq!(
            hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![1, 2],
            "the row inserted after build must be visible to the next search"
        );
    }

    // 選出規約: スコア降順・同点 id 昇順が `CpuScalarProvider` の結果と一致する
    // （`TopKSelector` 共用の回帰確認）。
    #[test]
    fn search_time_filter_selection_matches_cpu_scalar_provider_tie_break() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        // 3 行とも同スコア（1.0）。タイブレークは id 昇順。
        for id in [3u64, 2, 1] {
            insert(
                &storage,
                "docs",
                id,
                "tenant-a",
                Visibility::Public,
                &[1.0, 0.0],
            );
        }
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let hits = filter.search(&ctx, &[1.0, 0.0], 1).expect("search ok");
        assert_eq!(hits, vec![SearchHit::new("tenant-a", 1, 1.0)]);
    }

    // `len`/`is_empty` は渡された ctx の可視性述語で都度判定する（`PrefilterIndex` と
    // 異なり ContextMismatch を返さない）。テナントごとの分離を判別するため `Private` を
    // 使う（`Public` は TASK-89 / TABLE-9 でテナント横断の共有可視性へ変わったため
    // 判別に使えない）。
    #[test]
    fn search_time_filter_len_and_is_empty_use_the_given_context_each_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Private,
            &[1.0, 0.0],
        );
        insert(
            &storage,
            "docs",
            2,
            "tenant-b",
            Visibility::Private,
            &[1.0, 0.0],
        );

        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");

        let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        assert_eq!(filter.len(&ctx_a).expect("len ok"), 1);
        assert!(!filter.is_empty(&ctx_a).expect("is_empty ok"));

        let ctx_c = PolicyContext::with_visibilities("tenant-c", [Visibility::Private])
            .expect("valid tenant");
        assert_eq!(filter.len(&ctx_c).expect("len ok"), 0);
        assert!(filter.is_empty(&ctx_c).expect("is_empty ok"));
    }

    #[test]
    fn search_time_filter_dim_and_table_name_do_not_require_a_context() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 3))
            .expect("create table");
        let filter = SearchTimeFilter::build(&storage, "docs").expect("build filter");
        assert_eq!(filter.dim(), 3);
        assert_eq!(filter.table_name(), "docs");
    }

    // --- RlsSafetyNet（TASK-136・RLS-5。`sql::plan::apply_rls_safety_net` から移設） -----

    #[test]
    fn safety_net_removes_invisible_ids_and_keeps_order() {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let net = RlsSafetyNet::new(&ctx);
        let hits = vec![(1, 0.1), (2, 0.2), (3, 0.3)];
        // id=2 は tenant-b の Private（呼び出し側の ctx では不可視。Public は
        // テナントを問わず可視のため、除去対象には Private を使う）。
        let label_of = |id: u64| -> Option<(&str, Visibility)> {
            match id {
                1 => Some(("tenant-a", Visibility::Public)),
                2 => Some(("tenant-b", Visibility::Private)),
                3 => Some(("tenant-a", Visibility::Private)),
                _ => None,
            }
        };
        let verified = net.apply(hits, label_of);
        assert_eq!(verified.hits(), &[(1, 0.1), (3, 0.3)]);
        assert_eq!(verified.dropped(), 1);
        assert_eq!(verified.len(), 2);
        assert!(!verified.is_empty());
    }

    #[test]
    fn safety_net_fail_closed_drops_ids_with_missing_label() {
        let ctx = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
            .expect("valid tenant");
        let net = RlsSafetyNet::new(&ctx);
        let hits = vec![(1, 0.1), (99, 0.9)];
        let label_of = |id: u64| -> Option<(&str, Visibility)> {
            if id == 1 {
                Some(("tenant-a", Visibility::Public))
            } else {
                None
            }
        };
        let verified = net.apply(hits, label_of);
        assert_eq!(verified.into_hits(), vec![(1, 0.1)]);
    }

    #[test]
    fn safety_net_on_empty_hits_returns_empty() {
        let ctx = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
            .expect("valid tenant");
        let net = RlsSafetyNet::new(&ctx);
        let verified = net.apply(Vec::new(), |_| None);
        assert!(verified.is_empty());
        assert_eq!(verified.dropped(), 0);
    }

    #[test]
    fn safety_net_dropped_is_zero_when_all_visible() {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let net = RlsSafetyNet::new(&ctx);
        let hits = vec![(1, 0.1), (2, 0.2)];
        let label_of = |id: u64| -> Option<(&str, Visibility)> {
            match id {
                1 => Some(("tenant-a", Visibility::Public)),
                2 => Some(("tenant-a", Visibility::Private)),
                _ => None,
            }
        };
        let verified = net.apply(hits, label_of);
        assert_eq!(verified.dropped(), 0);
        assert_eq!(verified.len(), 2);
    }

    #[test]
    fn safety_net_still_removes_other_tenant_private_even_when_ctx_allows_private() {
        // ctx が Private を許可していても、`is_visible` への委譲がテナント一致を
        // 要求するため他テナントの Private 行は除去される（`policy.rs` の判定へ
        // 委譲していることの回帰。本モジュール独自の緩い比較を持たない）。
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let net = RlsSafetyNet::new(&ctx);
        let hits = vec![(1, 0.1), (2, 0.2)];
        let label_of = |id: u64| -> Option<(&str, Visibility)> {
            match id {
                1 => Some(("tenant-a", Visibility::Private)),
                2 => Some(("tenant-b", Visibility::Private)),
                _ => None,
            }
        };
        let verified = net.apply(hits, label_of);
        assert_eq!(verified.hits(), &[(1, 0.1)]);
        assert_eq!(verified.dropped(), 1);
    }

    #[test]
    fn safety_net_matches_is_visible_across_all_tenant_and_visibility_combinations() {
        // 判定が `PolicyContext::is_visible` と全組（テナント × 可視性）で一致する
        // ことの機械照合（安全網が独自ロジックへ乖離していないことの回帰）。
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let net = RlsSafetyNet::new(&ctx);
        let tenants = ["tenant-a", "tenant-b"];
        let visibilities = [Visibility::Public, Visibility::Private];
        let mut hits = Vec::new();
        let mut labels: HashMap<u64, (&str, Visibility)> = HashMap::new();
        let mut expected_visible: HashSet<u64> = HashSet::new();
        let mut next_id = 1u64;
        for tenant in tenants {
            for visibility in visibilities {
                let id = next_id;
                next_id += 1;
                hits.push((id, id as f64));
                labels.insert(id, (tenant, visibility));
                if ctx.is_visible(tenant, visibility) {
                    expected_visible.insert(id);
                }
            }
        }
        let verified = net.apply(hits, |id| labels.get(&id).copied());
        let got_visible: HashSet<u64> = verified.hits().iter().map(|(id, _)| *id).collect();
        assert_eq!(got_visible, expected_visible);
    }

    // 対象ビヘイビア: RLS-1〜4（TASK-169）。`PrefilterSnapshot::search_with` は
    // `PrefilterIndex::search` と同一契約（世代の前後照合による失効検出）を、
    // 呼び出し元が渡す `&Storage` に対して満たす（`core.rs::PrefilterCache` が
    // storage 参照を保持せずキャッシュできることの直接的な前提）。
    #[test]
    fn snapshot_search_with_rejects_after_a_write_bumps_the_generation() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let snapshot = PrefilterSnapshot::build(&storage, "docs", &ctx).expect("build snapshot");

        // 構築直後は世代が一致するため成功する。
        assert!(snapshot
            .search_with(&storage, &ctx, &CpuScalarProvider, &[1.0, 0.0], 10)
            .is_ok());

        // 別の書き込みコミットで世代が進むと、以後の検索は stale として拒否される
        // （fail-closed。古い可視行集合の結果を返す経路を作らない）。
        insert(
            &storage,
            "docs",
            2,
            "tenant-a",
            Visibility::Public,
            &[0.0, 1.0],
        );
        assert!(matches!(
            snapshot.search_with(&storage, &ctx, &CpuScalarProvider, &[1.0, 0.0], 10),
            Err(RlsError::IndexStale)
        ));
    }

    // 対象ビヘイビア: RLS-1（TASK-169）。構築時と異なる `PolicyContext` は
    // fail-closed に拒否する（`core.rs::PrefilterCache` のキー照合が万一崩れても
    // 本メソッド自身が防御する二重チェック）。
    #[test]
    fn snapshot_search_with_rejects_a_context_different_from_build_time() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let snapshot = PrefilterSnapshot::build(&storage, "docs", &ctx_a).expect("build snapshot");

        assert!(matches!(
            snapshot.search_with(&storage, &ctx_b, &CpuScalarProvider, &[1.0, 0.0], 10),
            Err(RlsError::ContextMismatch)
        ));
    }

    // 対象ビヘイビア: TASK-169（`core.rs::PrefilterCache` の容量上限判定の前提）。
    // 可視行を持つスナップショットの概算ヒープ使用量は 0 より大きい。
    #[test]
    fn snapshot_approx_heap_bytes_is_positive_when_index_holds_rows() {
        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        insert(
            &storage,
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 0.0],
        );
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let snapshot = PrefilterSnapshot::build(&storage, "docs", &ctx).expect("build snapshot");

        assert!(snapshot.approx_heap_bytes() > 0);
        assert_eq!(snapshot.built_ctx(), &ctx);
    }

    // 対象ビヘイビア: security.md「不安全な設計｜無制限リソース確保（DoS）」
    // （codex-review P1 対応・PR #191）。`hash_set_conservative_bytes` は `len()`
    // （要素数）ではなく `capacity()`（bucket/control-byte 込みの確保量）ベースで
    // 見積もらなければならない。要素を全削除しても capacity は解放されない
    // `HashSet` を作り、len ベースの旧計算（0）より大きい値が返ることを確認する
    // （旧実装は len==0 のとき 0 を返し、実確保量を無視していた＝本テストは
    // 退行防止）。
    #[test]
    fn hash_set_conservative_bytes_charges_unused_capacity_not_just_len() {
        let mut set: std::collections::HashSet<u64> =
            std::collections::HashSet::with_capacity(1024);
        for i in 0..1024u64 {
            set.insert(i);
        }
        set.clear();
        assert_eq!(set.len(), 0);
        assert!(set.capacity() >= 1024);

        let bytes = hash_set_conservative_bytes::<u64>(set.capacity());
        assert!(
            bytes > 0,
            "capacity-based estimate must charge unused capacity even when len()==0"
        );
        // capacity 分の要素サイズ（8 byte/要素）は最低限含まれていること。
        assert!(bytes >= set.capacity() * std::mem::size_of::<u64>());
    }

    #[test]
    fn hash_set_conservative_bytes_is_zero_for_zero_capacity() {
        assert_eq!(hash_set_conservative_bytes::<u64>(0), 0);
    }

    // TASK-137: フックは独自比較を持たず `PolicyContext::is_visible` へ
    // 委譲するだけであることを確認する。
    #[test]
    fn implicit_hook_delegates_to_policy_context_is_visible() {
        let ctx = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
            .expect("valid tenant");
        let hook = ImplicitRlsHook::new(&ctx);

        for tenant in ["tenant-a", "tenant-b"] {
            for visibility in [Visibility::Public, Visibility::Private] {
                assert_eq!(
                    hook.is_visible(tenant, visibility),
                    ctx.is_visible(tenant, visibility)
                );
                assert_eq!(
                    (hook.predicate())(tenant, visibility),
                    ctx.is_visible(tenant, visibility)
                );
            }
        }
        assert_eq!(hook.context() as *const PolicyContext, &ctx as *const _);
    }

    // codex-review P1 指摘（Issue #409・PR #435）: `search_with_hnsw` の事前世代
    // 照合（`pre_generation`）通過後・`read_txn` を開くまでの間に別の書き込みが
    // コミットされると、`self.arena` は旧世代のまま `read_txn` だけが新世代を
    // 観測する不整合が起こり得る。この状態のまま `search_or_fallback` を呼ぶと
    // 旧アリーナから構築した索引が新世代のエントリとして誤登録され、事後照合で
    // 今回の呼び出し自体は失効させても誤登録キャッシュは残ってしまう。実際の
    // スレッド間レースは非決定的なため、ここでは `PrefilterSnapshot` の
    // フィールドを直接構築してレース着地後と同じ状態（グローバル世代は最新に
    // 一致・`built_table_generation` だけが構築時点のまま乖離）を決定的に
    // 再現する。修正後は `search_or_fallback` を呼ばず brute-force へ縮退し、
    // キャッシュへ一切登録しないことを固定する。
    #[test]
    fn search_with_hnsw_falls_back_without_registering_cache_when_read_txn_table_generation_diverges_from_snapshot(
    ) {
        use crate::hnsw::provider::HnswSearchProvider;
        use crate::hnsw::{HnswParams, ValidatedHnswParams};
        use crate::sql::hnsw_cache::{HnswCacheAccess, HnswIndexCache};

        const DIM: usize = 8;
        // `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超で索引構築対象になる行数。
        const ROWS: usize = 1_100;

        let dir = tempdir();
        let storage = open_storage(dir.path());
        storage
            .create_table(&schema_for("docs", DIM as u32))
            .expect("create table");

        for i in 0..ROWS {
            let mut v = vec![0.0f32; DIM];
            v[i % DIM] = 1.0;
            insert(
                &storage,
                "docs",
                i as u64,
                "tenant-a",
                Visibility::Public,
                &v,
            );
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let snapshot = PrefilterSnapshot::build(&storage, "docs", &ctx).expect("build snapshot");

        // 別トランザクションで同一行数の更新（行 0 の embedding を差し替え）を
        // commit し、テーブル世代・グローバル世代を進める。`snapshot` は
        // 更新前のベクトルのまま固定される。
        let mut updated = vec![0.0f32; DIM];
        updated[0] = -1.0;
        insert(
            &storage,
            "docs",
            0,
            "tenant-a",
            Visibility::Public,
            &updated,
        );

        let current_generation = storage.current_generation().expect("current generation");
        let current_table_generation = {
            let read_txn = storage.db().begin_read().expect("begin read");
            crate::catalog::table_generation_in_txn(&read_txn, "docs").expect("table generation")
        };
        assert_ne!(current_table_generation, snapshot.built_table_generation);

        // レース着地後の状態を直接構築する: 事前世代照合（グローバル世代）は
        // 通過させつつ、`built_table_generation` だけをスナップショット構築時点の
        // まま残す。
        let racy_snapshot = PrefilterSnapshot {
            arena: snapshot.arena,
            slot_ids: snapshot.slot_ids.clone(),
            visible_id_counts: snapshot.visible_id_counts.clone(),
            built_ctx: snapshot.built_ctx.clone(),
            built_generation: current_generation,
            built_table_generation: snapshot.built_table_generation,
        };

        let params = ValidatedHnswParams::new(HnswParams::default()).expect("valid hnsw params");
        let hnsw_provider = HnswSearchProvider::new(params);
        let cache = HnswIndexCache::new();
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: hnsw_provider,
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let cpu = CpuScalarProvider;

        let query = vec![1.0f32; DIM];
        let hits = racy_snapshot
            .search_with_hnsw(&storage, &ctx, &access, &cpu, &query, 5)
            .expect("search succeeds via brute-force fallback");
        assert_eq!(hits.len(), 5);

        // brute-force 縮退が使われ、旧世代の `racy_snapshot.arena` から構築した
        // 索引が新テーブル世代のエントリとしてキャッシュへ登録されていないこと
        // （修正前は `builds`/`entries` が非 0 になり得た）。
        let stats = cache.stats();
        assert_eq!(stats.builds, 0);
        assert_eq!(stats.entries, 0);

        // 縮退経路の結果が `racy_snapshot.arena`（旧世代スナップショット）上の
        // brute-force と一致すること（不可視行を含まない・テナント境界を
        // 破らないことも `resolve_slot_hits` の写像を経由して確認する）。
        let expected_raw = {
            let input = SearchInput {
                ids: &racy_snapshot.slot_ids,
                vectors: racy_snapshot.arena.vectors(),
                dim: racy_snapshot.arena.dim(),
                query: &query,
                k: 5,
            };
            cpu.search(input).expect("cpu search")
        };
        let expected_hits = crate::core::resolve_slot_hits(&racy_snapshot.arena, &expected_raw)
            .expect("resolve slot hits");
        assert_eq!(hits, expected_hits);
    }

    // TASK-137: fail-closed の回帰確認。
    #[test]
    fn implicit_hook_never_admits_other_tenant_private_rows() {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let hook = ImplicitRlsHook::new(&ctx);

        assert!(hook.is_visible("tenant-a", Visibility::Private));
        assert!(!hook.is_visible("tenant-b", Visibility::Private));
        assert!(hook.is_visible("tenant-b", Visibility::Public));
    }
}
