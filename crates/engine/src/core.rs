//! プロトコル非依存のコア API 層（TASK-124・対象ビヘイビア: CORE-1）。
//!
//! `wire-server`（pg wire v3）や他の将来プロトコル実装は、本モジュールが定義する
//! [`VectorCore`] trait のみに依存する。コア API の変更なしにプロトコル実装を
//! 追加・変更できることを構造で担保する（trait は object-safe。シグネチャ安定性の
//! CI 機械チェックは TASK-125 で導入済み。チェック実体は
//! `scripts/check_core_api.sh` と `tests/core_api_stability.rs`）。
//!
//! [`EngineCore`] が製品実装で、`storage.rs`（永続化）・`catalog.rs`（スキーマ）・
//! `arena.rs`（検索対象のカラムナビュー構築）・`kernel.rs`（実行バックエンド provider）・
//! `policy.rs`（テナント境界・可視性判定）を束ねる。テナント判定は必ず
//! [`crate::policy::PolicyContext::is_visible`] の単一照合パス経由で行い、
//! 不可視行と不存在行の応答を区別しない（存在情報を漏らさない。security.md 準拠）。
//!
//! `EngineCore::search` は [`SearchProvider`] を `Box<dyn SearchProvider>`（CORE-13）で
//! 差し替え可能にしているが、provider は untrusted 実装（GPU/ANN バックエンド差し替え等）
//! でありうる。テナント境界の適用を「provider が可視性チェックに従う」という規約だけに
//! 委ねると、provider 実装がそれを無視して不可視行（他テナント行を含む）のベクトル・id を
//! 読み取る／外部送信することを防げない（AGENTS.md P0「テナント境界の弱体化」）。そのため
//! `EngineCore::search` は二重の防御を持つ: (1) [`VectorArena::build_filtered`] へ
//! [`crate::rls::ImplicitRlsHook::predicate`]（TASK-137・対象ビヘイビア: RLS-6, RLS-7）を
//! 渡し、不可視行を
//! アリーナ構築時点で確保しない（`arena.rs` のドキュメント参照。以前はアリーナ全行を
//! 構築してから可視行だけを別バッファへ再確保・全コピーしており、1 検索あたりの
//! ピークメモリが最大で 2 倍になっていたが、構築時フィルタにより単一確保で完結する。
//! codex P2 対応）。`kernel::SearchInput` へはこの構築時フィルタ済みアリーナの
//! `ids`/`vectors` をそのまま渡すため、不可視データはそもそも provider のアドレス空間へ
//! 渡らない。(2) それでも provider が戻り値へアリーナ外の `id`（捏造や実装バグ）を
//! 含めた場合に備え、戻り値をコア側で計算した可視行 id 集合と突き合わせて再検証し、
//! 逸脱があれば結果を一切返さず `CoreError::ProviderResultRejected` で拒否する
//! （fail-closed。テナント分離を provider 実装の正しさに依存させない）。
//!
//! `EngineCore::search` は `rls.rs::PrefilterSnapshot`（TASK-133）を
//! [`PrefilterCache`] 経由でキャッシュし、毎クエリの `VectorArena::build_filtered`
//! 再構築を避ける（TASK-169・対象ビヘイビア: RLS-1〜4）。キャッシュキーは
//! `(table, ctx)` の完全一致で、失効は `storage.rs::bump_generation_and_commit`
//! による単調増加世代（[`crate::storage::Storage::current_generation`]）との
//! 前後照合で検出する。世代不一致・ロック毒化・スナップショット自身の
//! `ContextMismatch`/`IndexStale` はいずれも「キャッシュを使わない」側へ縮退し
//! （[`EngineCore::search_uncached`] を都度 1 回だけ呼ぶ）、stale なインデックスの
//! 結果を返す経路を作らない（fail-closed。詳細は [`PrefilterCache`] のドキュメント
//! 参照）。構築完了から挿入までの間に世代が進んだ（挿入対象自身が stale になった）場合も
//! 同様に [`PrefilterCache::insert`] が `None` を返し、呼び出し元は
//! `search_uncached` へ 1 回だけ縮退する（TASK-169・Issue #280。stale な
//! スナップショットを呼び出し元へ露出させない）。可視性判定・provider 結果の
//! 二重防御（構築時フィルタ＋`provider_result_is_valid`）はキャッシュ経路・
//! 非キャッシュ経路のいずれでも
//! `PrefilterSnapshot::search_with`／[`EngineCore::search_uncached`] が同一に適用する。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::{table_lookup_error, CatalogError, TableSchema};
use crate::dispatch::{self, DispatchError, DispatchInput, ExecutionPath};
use crate::kernel::{CandidateHit, KernelError, SearchHit, SearchInput, SearchProvider};
use crate::policy::{PolicyContext, PolicyError};
use crate::recovery::required_op_id::{LedgerMode, OperationId};
use crate::rls::{ImplicitRlsHook, PrefilterSnapshot, RlsError};
use crate::search_engine;
use crate::storage::{Row, Storage, StorageError};
use redb::ReadableDatabase;

/// 検索 `k` の上限。上限検証前にアロケーションへ使わないための防御的定数
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応。`catalog.rs::MAX_LIST_TABLES`
/// と同程度の桁に揃える）。`rls.rs`（TASK-133）の `PrefilterIndex::search` も同一の上限を
/// 共有するため `pub(crate)`（二重定義しない）。
pub(crate) const MAX_SEARCH_K: usize = 10_000;

/// `k` の範囲検証（`k == 0` または [`MAX_SEARCH_K`] 超過を拒否）。`core.rs::EngineCore::search`
/// と `rls.rs::PrefilterIndex::search` が同一の上限判定を共有するためのヘルパー
/// （二重管理を防ぐ）。呼び出し元はエラー型ごとに `Err(k)` を自分の variant へ写像する。
pub(crate) fn validate_search_k(k: usize) -> Result<(), usize> {
    if k == 0 || k > MAX_SEARCH_K {
        Err(k)
    } else {
        Ok(())
    }
}

/// `SearchProvider` 戻り値の Top-k 契約検証（本ファイルのモジュールドキュメント (1)〜(5)、
/// および [`VectorCore::search`] 実装内コメント参照）。`core.rs::EngineCore::search` と
/// `rls.rs::PrefilterIndex::search` の両方が provider を「untrusted 実装でありうる」前提で
/// 扱うため、単一走査の検証ロジックを本関数へ集約する（二重管理・契約の食い違いを防ぐ。
/// fail-closed: 1 件でも違反すれば `false` を返し、呼び出し元は結果を一切返さず拒否する）。
///
/// 第 3 引数は「可視行の id → その id を持つ可視行の件数」の多重集合（[`visible_id_counts`]
/// で構築する）。単なる `HashSet<u64>` ではないのは、行 `id` の一意性スコープが
/// テナント内に閉じた（対象ビヘイビア: TABLE-12）ことで、1 つの `PolicyContext` から
/// 可視な行に同一 `id` が複数現れうるため（自テナント行と、他テナントの `Public` 行が
/// 同じ `id` を持つ場合）。件数を上限として重複を許すことで、(4) の防御力
/// （「provider は実在する可視行の数を超えて同じ id を返せない」）を落とさずに
/// TABLE-12 が正当化する構成を受理する。set へ戻す（＝重複を一律拒否する）と、
/// 他テナントが同じ `id` の `Public` 行を作るだけで検索が失敗する
/// テナント間の可用性干渉になるため戻してはならない。
pub(crate) fn provider_result_is_valid(
    hits: &[CandidateHit],
    k: usize,
    visible_id_counts: &HashMap<u64, usize>,
) -> bool {
    // (1) 件数が要求 k を超えない。
    if hits.len() > k {
        return false;
    }
    let mut seen_ids: HashMap<u64, usize> = HashMap::with_capacity(hits.len());
    let mut prev: Option<&CandidateHit> = None;
    for hit in hits {
        // (2) スコアが有限（NaN/Inf でない）。非有限スコアは全順序を持たず、後続の順序
        // 検証（`total_cmp`）が無意味になるため他の検証より先に弾く。
        if !hit.score.is_finite() {
            return false;
        }
        // (3) 縮約ビュー（＝可視行）の id 集合に属する（不可視 id・捏造 id の拒否）。
        let Some(available) = visible_id_counts.get(&hit.id) else {
            return false;
        };
        // (4) 同じ id の出現回数が、その id を持つ可視行の実数を超えない
        // （同じ行を複数回返す provider の拒否。TABLE-12 により id の重複自体は
        // 起こりうるため、集合ではなく多重集合で判定する。上記ドキュメント参照）。
        let seen = seen_ids.entry(hit.id).or_insert(0);
        *seen = match seen.checked_add(1) {
            Some(next) => next,
            None => return false,
        };
        if *seen > *available {
            return false;
        }
        // (5) スコア降順・同点は**候補識別子**の昇順（`kernel.rs::CpuScalarProvider` が
        // 実際に返す順序と同じ契約。識別子は呼び出し元定義で、`core.rs`・`sql/exec.rs` は
        // アリーナのスロット番号を渡すため実質 `(tenant_id, id)` 昇順になり、単一テナント
        // 内では従来どおり行 `id` 昇順と一致する。`docs/design/rrf-tie-break-determinism.md`
        // の順序契約〈安定ソート + 識別子昇順タイブレーク〉と整合）。`total_cmp` は (2) で
        // 有限性を確認済みのため NaN の順序上の扱いには依存しない。
        if let Some(p) = prev {
            // 同点時は識別子の昇順。TABLE-12 により同点・同 id の並びが正当に起こりうる
            // （異なるテナントの同一 id 行が同じスコアになる場合）ため、狭義単調増加
            // （`p.id >= hit.id` を違反とする）ではなく広義単調増加で判定する。
            // 同一行の重複返却は (4) の多重集合上限が引き続き遮断する。
            let out_of_order = match p.score.total_cmp(&hit.score) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Equal => p.id > hit.id,
                std::cmp::Ordering::Greater => false,
            };
            if out_of_order {
                return false;
            }
        }
        prev = Some(hit);
    }
    true
}

/// 可視行の id 列（`arena.ids()` 等）から [`provider_result_is_valid`] の第 3 引数
/// （id → 可視行件数の多重集合）を構築する共通ヘルパ。`core.rs`・`rls.rs`・
/// `sql/exec.rs` の 3 経路が同一の構築方法を共有し、片側だけが集合へ退化するのを防ぐ
/// （TABLE-12 の重複 id の扱いは 1 か所で決める）。
/// 候補アリーナのスロット番号（0..n）を provider 入力用の `Vec<u64>` として作る
/// （対象ビヘイビア: TABLE-12）。
///
/// provider へ渡す識別子に行 `id` を使えないのは、行 `id` の一意性スコープが
/// テナント内に閉じており、1 つの可視集合に同じ `id` の行が複数含まれうるため
/// （自テナント行と他テナントの `Public` 行）。スロット番号は可視集合内で一意かつ
/// `(tenant_id, id)` の行と 1 対 1 に対応するので、provider 結果の重複検証・
/// ヒットの行解決の双方が曖昧にならない（`sql/exec.rs` も同じ方式）。
/// 確保はフォールブル（`try_reserve_exact`）で行う（coding-rust.md「無制限確保禁止」）。
pub(crate) fn slot_ids_for(arena: &VectorArena) -> Result<Vec<u64>, ArenaError> {
    let len = arena.ids().len();
    let mut slot_ids: Vec<u64> = Vec::new();
    slot_ids
        .try_reserve_exact(len)
        .map_err(|e| ArenaError::AllocationFailed(format!("failed to reserve slot ids: {e}")))?;
    for slot in 0..len {
        let slot_id = u64::try_from(slot).map_err(|_| ArenaError::CapacityExceeded)?;
        slot_ids.push(slot_id);
    }
    Ok(slot_ids)
}

/// provider が返した候補ヒット（識別子はスロット番号）を、テナント修飾済みの
/// 公開ヒット [`CandidateHit`] → [`crate::kernel::SearchHit`] へ解決する
/// （対象ビヘイビア: TABLE-12・RLS-9。codex-review P1 指摘・PR #194）。
///
/// スロットが範囲外（provider の契約違反・データ不整合）の場合は `None` を返し、
/// 呼び出し元は結果を一切返さず fail-closed に拒否する（部分的な解決はしない）。
/// `String` 確保はここ（高々 `k` 件）でのみ発生し、候補行ごとに走る
/// `TopKSelector::push` のホットパスには入らない。
pub(crate) fn resolve_slot_hits(
    arena: &VectorArena,
    candidates: &[CandidateHit],
) -> Option<Vec<SearchHit>> {
    let mut out = Vec::new();
    out.try_reserve_exact(candidates.len()).ok()?;
    for hit in candidates {
        let slot = usize::try_from(hit.id).ok()?;
        let tenant = arena.tenant_id(slot)?;
        let id = *arena.ids().get(slot)?;
        out.push(SearchHit::new(tenant, id, hit.score));
    }
    Some(out)
}

pub(crate) fn visible_id_counts(ids: &[u64]) -> HashMap<u64, usize> {
    let mut counts: HashMap<u64, usize> = HashMap::with_capacity(ids.len());
    for id in ids {
        let entry = counts.entry(*id).or_insert(0);
        *entry = entry.saturating_add(1);
    }
    counts
}

/// [`PrefilterCache`] のエントリ数上限（TASK-169・security.md「不安全な設計｜
/// 無制限リソース確保（DoS）」対応）。線形走査で照合するキャッシュのため、上限は
/// 実用上の再利用シナリオ（少数テーブル × 少数ポリシーの組み合わせ）を十分満たしつつ、
/// 走査コストが問題にならない桁に留める。
const MAX_PREFILTER_CACHE_ENTRIES: usize = 32;

/// [`PrefilterCache`] が保持するスナップショット群の概算バイト量の合計上限（TASK-169）。
/// アリーナ本体の常駐上限（[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）と同じ桁に揃え、
/// キャッシュ全体の常駐メモリがアリーナ 1 個分の上限を大きく超えないようにする。
const MAX_PREFILTER_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// [`PrefilterCache`] の観測用統計（TASK-169）。テナント ID・行 ID 等の機微情報は
/// 一切含まない（カウンタのみ。security.md「情報漏えい」対応）。`VectorCore` trait
/// には載せない固有 API（`EngineCore::prefilter_cache_stats`）としてのみ公開する。
#[derive(Debug, Clone, Copy, Default)]
pub struct PrefilterCacheStats {
    /// キャッシュヒット数（世代整合まで確認できた再利用）。
    pub hits: u64,
    /// キャッシュミス数（未登録、または世代不一致で破棄した後の再構築）。
    pub misses: u64,
    /// 世代不一致・`search_with` 内の `IndexStale` 検出による破棄回数。
    pub stale_evictions: u64,
    /// 容量上限超過による LRU 追い出し回数。
    pub capacity_evictions: u64,
    /// 現在キャッシュが保持しているエントリ数。
    pub entries: usize,
}

/// [`PrefilterCache`] の 1 エントリ（TASK-169）。`table`・`ctx` の組がキャッシュキー
/// （`PolicyContext` は `Hash` を実装しないため `HashMap` ではなく `Vec` 線形走査で
/// 照合する。エントリ数は [`MAX_PREFILTER_CACHE_ENTRIES`] で小さく抑えるため走査コストは
/// 問題にならない）。
struct CacheEntry {
    table: String,
    snapshot: Arc<PrefilterSnapshot>,
    /// LRU 追い出し判定用の単調シーケンス（アクセスのたびに更新）。
    last_used: u64,
}

/// ロックが保護する可変状態（[`RwLock`] 内側）。
#[derive(Default)]
struct CacheState {
    entries: Vec<CacheEntry>,
}

/// `EngineCore::search` が再利用する `rls.rs::PrefilterSnapshot` の世代整合キャッシュ
/// （TASK-133 の `PrefilterIndex` を製品検索経路へ配線する TASK-169・対象ビヘイビア:
/// RLS-1〜4）。
///
/// **キー**: `(table, ctx)` の完全一致。`PolicyContext` はテナント ID・許可可視性集合を
/// 含む値のため、キーが一致する限り「同じテナント・同じ可視性境界で構築された
/// スナップショット」であることが保証される（構築時フィルタの根拠は
/// `PrefilterSnapshot::build` 側にある。本キャッシュはそれを取り違えないことのみ担保する）。
///
/// **失効**: [`Self::lookup`] はヒット時に `snapshot.built_generation()` を
/// `storage.current_generation()` と突き合わせ、不一致（または世代読み取り自体の失敗）
/// なら該当エントリを破棄してミス扱いにする（fail-closed: 古い可能性のあるものは
/// 使わない）。それでも `search_with` 内で `IndexStale`/`ContextMismatch` を検出した
/// 場合（他スレッドによる直後の書き込みでの競合等）は [`Self::evict`] で破棄し、
/// 呼び出し元（`EngineCore::search`）が非キャッシュ経路へ 1 回だけ縮退する
/// （`core.rs` モジュールドキュメント参照。stale な結果を返す経路は存在しない）。
///
/// **同期**: [`RwLock`] は書き込みロック（LRU の `last_used` 更新のため）で `Arc` を
/// clone した後すぐに解放し、検索自体はロック外で実行する（ロック保持を lookup/insert
/// の短時間に限定する）。ロック毒化
/// （パニックしたスレッドが保持中だった場合）は `unwrap` せず「キャッシュ無効」として
/// 縮退する（fail-closed。.claude/rules/coding-rust.md: 受信データ経路で `unwrap` を
/// 使わない方針をキャッシュ層にも適用する）。
///
/// **容量**: [`MAX_PREFILTER_CACHE_ENTRIES`]・[`MAX_PREFILTER_CACHE_TOTAL_BYTES`] を
/// 超えないよう、挿入時に (1) 現在世代と一致しないエントリを先に全破棄し、(2) それでも
/// 超過するなら `last_used` が最小のエントリから追い出す（LRU）。単体で総量上限を
/// 超えるスナップショットはキャッシュへ挿入せず、その 1 回の検索限りで使い捨てる
/// （常駐させない）。
pub(crate) struct PrefilterCache {
    state: RwLock<CacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_evictions: AtomicU64,
    capacity_evictions: AtomicU64,
}

impl PrefilterCache {
    fn new() -> Self {
        Self {
            state: RwLock::new(CacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            capacity_evictions: AtomicU64::new(0),
        }
    }

    /// `(table, ctx)` に一致し、かつ現在の世代と整合するエントリを探す。
    /// 世代不一致のエントリは見つけ次第破棄する（fail-closed。上記型ドキュメント参照）。
    /// ロック毒化・世代読み取り失敗はいずれも「見つからなかった」として扱う。
    fn lookup(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
    ) -> Option<Arc<PrefilterSnapshot>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);

        let mut guard = self.state.write().ok()?;
        // 世代はロック取得後に読み直す（github-actions/codex-review P1 指摘）。
        // ロック取得を待っている間に他スレッドが新しい世代のエントリを挿入し得るため、
        // ロック取得前に読んだ古い世代値のままだと、その新しい有効エントリを
        // 「不一致」と誤判定して破棄してしまう（fail-closed の意図に反する誤破棄）。
        // ロック保持中に読む値なら、この呼び出し内で以降エントリが変化しないことを
        // 保証できる。
        let current_generation = storage.current_generation().ok()?;
        let position = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.snapshot.built_ctx() == ctx)?;
        // Vec::get だけでは境界チェック後に別スレッドが縮めうる余地はない
        // （書き込みロック保持中のため、この関数呼び出し内で `guard.entries` は
        // 単独所有されている）。添字アクセス自体は自明に有効な `position` に対してのみ
        // 行う。
        let stale = guard
            .entries
            .get(position)
            .map(|e| e.snapshot.built_generation() != current_generation)
            .unwrap_or(true);
        if stale {
            guard.entries.remove(position);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let snapshot = {
            let entry = guard.entries.get_mut(position)?;
            entry.last_used = seq;
            Arc::clone(&entry.snapshot)
        };
        drop(guard);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(snapshot)
    }

    /// 新規構築したスナップショットを挿入する。挿入対象自身が既に古い（並行書き込みで
    /// 世代が進んだ）場合・世代を確認できない場合は `None` を返し、キャッシュへ
    /// 反映しないだけでなく呼び出し元へも一切渡さない（Issue #280。
    /// [`DictionaryCache::insert`] と同じ契約に統一。以前は判定不能・stale のいずれも
    /// 構築済みの `Arc` をそのまま返しており、呼び出し元は世代不一致を承知の上で
    /// stale なスナップショットを一時的に検索へ使う経路が残っていた。`search_with`
    /// 側の前後世代照合で結果自体が漏れることはなかったが〔`core.rs` モジュール
    /// ドキュメント参照〕、契約としては [`DictionaryCache::insert`] と食い違って
    /// いた）。単体で総量上限を超える場合はキャッシュしないが、現在世代と整合済み
    /// なので呼び出し元へは `Some` で返し、その場限りで使う（型ドキュメント参照）。
    ///
    /// `storage` は (0) 挿入対象自身の世代整合チェックと (1) の世代不整合エントリの
    /// 一括破棄で「現在の実世代」（[`Storage::current_generation`]）を判定するために
    /// 使う。以前は `snapshot.built_generation()`（= このスナップショット自身の構築
    /// 時点の世代）を現在世代の代用にしていたが、これは挿入対象のスナップショットが
    /// 並行書き込みで既に古くなっている場合、真に新しい（現在世代と一致する）既存
    /// エントリまで「不一致」として誤って全破棄してしまう不具合があった
    /// （Cursor Bugbot 指摘）。
    /// `storage.current_generation()` の読み取りに失敗した場合、およびロックが
    /// 毒化している場合は世代整合を判定できないため `None` を返す（fail-closed:
    /// 「判定できないなら使わせない」側へ倒す。[`DictionaryCache::insert`] と同じ方針）。
    ///
    /// 世代の読み取りは書き込みロック取得後に行う（github-actions/codex-review P1
    /// 指摘）。ロック取得前に読むと、ロック待機中に他スレッドがより新しい世代の
    /// エントリを挿入し得るため、その古い世代値のまま (1) の一括破棄を行うと、
    /// 直後に挿入されたばかりの真に新しい有効エントリまで「不一致」として誤って
    /// 削除してしまう。ロック保持中に読んだ値なら、この呼び出し内で以降エントリが
    /// 変化しないことを保証できる。
    fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        snapshot: PrefilterSnapshot,
    ) -> Option<Arc<PrefilterSnapshot>> {
        let snapshot = Arc::new(snapshot);
        self.misses.fetch_add(1, Ordering::Relaxed);

        // ロック毒化時は世代整合を判定できないため `None`（fail-closed。
        // [`DictionaryCache::insert`] と同じ契約に統一。Issue #280）。
        let Ok(mut guard) = self.state.write() else {
            return None;
        };

        // (0) 挿入対象自身が現在世代と一致するか確認する（対象外スレッド指摘の修正）。
        // 世代が読み取れない、または挿入対象が既に古い場合はキャッシュへ反映せず、
        // 呼び出し元へも一切渡さない（`None`。Issue #280）。ここでリターンすることで、
        // 後続の「同一キー破棄」ステップに到達させない。すなわち並行書き込みで自身が
        // stale になった挿入が、別スレッドが直前に挿入した現在世代の有効エントリを
        // 上書き・削除する経路を断つ（型ドキュメント参照）。ロック保持中に読むため、
        // 以降のこの関数内の判定と齟齬が生じない。
        let Ok(current_generation) = storage.current_generation() else {
            return None;
        };
        if snapshot.built_generation() != current_generation {
            return None;
        }

        // ここまで到達した時点で `snapshot` 自身は現在世代と一致していることが
        // 保証されている。以降の容量判定・LRU 追い出しはキャッシュへの常駐可否のみを
        // 左右し、呼び出し元へ `Some` で返すことは変えない。
        let own_bytes = snapshot.approx_heap_bytes();
        if own_bytes > MAX_PREFILTER_CACHE_TOTAL_BYTES {
            // 単体で総量上限を超えるスナップショットは常駐させない（DoS 対策。
            // 型ドキュメント参照）。呼び出し元へはそのまま返し、1 回の検索限りで使う。
            return Some(snapshot);
        }

        // 同一 (table, ctx) キーの既存エントリは挿入前に取り除く（Cursor Bugbot 指摘:
        // 常に push するだけだと同一キーが重複登録され、[`Self::lookup`] は先頭一致
        // しか参照しないため後続の重複が [`MAX_PREFILTER_CACHE_ENTRIES`] を無駄に
        // 消費し続ける）。キーは `(table, ctx)` の完全一致（型ドキュメント参照）。
        // 上記 (0) により、ここに到達する時点で `snapshot` 自身は現在世代と一致
        // していることが保証されている（stale な挿入で既存の有効エントリを消さない）。
        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.snapshot.built_ctx() == ctx)
        {
            guard.entries.remove(pos);
        }

        // (1) 現在世代と不整合なエントリを先に全破棄する（型ドキュメント参照）。
        // 現在世代は (0) で読み取り済みの `current_generation`（挿入対象自身の世代と
        // 一致することを確認済み）をそのまま使う（挿入対象スナップショット自身の世代を
        // 代用しない、という以前の修正意図は変わらない。二重読み取りによるレースを
        // 避けるため同一ロック内で 1 回だけ読んだ値を使い回す）。
        let before = guard.entries.len();
        guard
            .entries
            .retain(|e| e.snapshot.built_generation() == current_generation);
        let removed_stale = before.saturating_sub(guard.entries.len());
        if removed_stale > 0 {
            self.stale_evictions
                .fetch_add(removed_stale as u64, Ordering::Relaxed);
        }

        // (2) それでも件数・総量が上限を超えるなら `last_used` 最小から追い出す。
        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.snapshot.approx_heap_bytes())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_PREFILTER_CACHE_ENTRIES
            || total_bytes.saturating_add(own_bytes) > MAX_PREFILTER_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                // これ以上追い出せるエントリがない（空）のに超過している場合は、
                // 挿入自体を諦めてキャッシュを汚さない（type ドキュメント参照）。
                // 挿入対象自身は現在世代と整合済みなので `Some` で返す。
                return Some(snapshot);
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.snapshot.approx_heap_bytes());
            self.capacity_evictions.fetch_add(1, Ordering::Relaxed);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(CacheEntry {
            table: table.to_string(),
            snapshot: Arc::clone(&snapshot),
            last_used: seq,
        });
        Some(snapshot)
    }

    /// `search_with` が返した `IndexStale`/`ContextMismatch` を受けて、該当エントリを
    /// 破棄する（他スレッドの直後の書き込みとの競合等。型ドキュメント参照）。
    /// `Arc::ptr_eq` で対象を特定するため、既に別のスナップショットへ差し替わっていた
    /// 場合は何もしない（誤って無関係なエントリを破棄しない）。
    fn evict(&self, table: &str, snapshot: &Arc<PrefilterSnapshot>) {
        let Ok(mut guard) = self.state.write() else {
            return;
        };
        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && Arc::ptr_eq(&e.snapshot, snapshot))
        {
            guard.entries.remove(pos);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stats(&self) -> PrefilterCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        PrefilterCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            entries,
        }
    }
}

/// [`DictionaryCache`] のエントリ数上限（TASK-109・PLAN-5。[`PrefilterCache`]
/// （TASK-169）と同じ DoS 対策方針を踏襲する）。
const MAX_DICTIONARY_CACHE_ENTRIES: usize = 16;

/// [`DictionaryCache`] が保持する [`crate::dictionary::Dictionary`] 群の概算バイト量の
/// 合計上限（[`MAX_PREFILTER_CACHE_TOTAL_BYTES`] と同じ桁に揃える）。
const MAX_DICTIONARY_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// [`DictionaryCache`] の 1 エントリ。`table`・`ctx` の組がキャッシュキー
/// （[`PrefilterCache`] の `CacheEntry` と同じ理由で `HashMap` ではなく `Vec` 線形走査）。
struct DictCacheEntry {
    table: String,
    ctx: PolicyContext,
    dictionary: Arc<crate::dictionary::Dictionary>,
    /// `dictionary` から 1 度だけ構築した正規化索引（[`crate::tiering::classify`]
    /// が使う）。`dictionary` と同じ寿命（同一世代の間だけキャッシュされ、世代が
    /// 変われば両方まとめて破棄・再構築される）で共有することで、
    /// `crate::tiering::classify` がリクエストごとに辞書全量を再正規化する経路を
    /// 断つ（codex-review P1 指摘対応・PR #261。`tiering.rs::NormalizedDictionaryIndex`
    /// ドキュメント参照）。
    normalized_index: Arc<crate::tiering::NormalizedDictionaryIndex>,
    built_generation: u64,
    approx_bytes: usize,
    /// LRU 追い出し判定用の単調シーケンス（アクセスのたびに更新）。
    last_used: u64,
}

/// ロックが保護する可変状態（[`RwLock`] 内側）。
#[derive(Default)]
struct DictCacheState {
    entries: Vec<DictCacheEntry>,
}

/// `EngineCore::dictionary_snapshot` が再利用する辞書的情報源（TASK-109・PLAN-5。
/// ポインタ: `docs/spec/04-behavior/query-planning.md` PLAN-5）の世代整合キャッシュ。
/// [`PrefilterCache`]（TASK-169）と同一の失効規約（fail-closed・世代不一致で破棄・
/// ロック取得後に世代を読み直す・容量超過は LRU 追い出し）を踏襲する。
///
/// **失効の粒度**: `storage.current_generation()` はテーブル・書き込み種別を問わず
/// 任意の write commit で単調増加する（[`Storage::current_generation`] ドキュメント
/// 参照）。そのため本キャッシュはこのテーブル自身への書き込みだけでなく、無関係な
/// 他テーブルへの書き込みでも保守的に失効する（テーブル単位の精密な失効は持たない）。
/// これは意図的な単純化であり、誤って古い辞書を返す経路（fail-open）よりも安全側
/// （過剰な再構築）に倒す設計判断である（security.md「fail-closed を維持する」）。
///
/// **再構築のトリガー**: ファイル形 `INSERT`（単発・バッチとも）は
/// `tenant::replace_typed_rows_by_text_key` が世代を bump するため、次回
/// `dictionary_snapshot` 呼び出し時に自動的に再構築され増分インデックスの結果が
/// 反映される（TASK-120 との連動）。本キャッシュは post-commit フックを持たず、
/// 参照時に世代を突き合わせるだけの構成のため、バッチ途中失敗時の不整合や
/// プロセス再起動時の消失を気にする必要がない（`redb` からの再構築で自己回復する）。
pub(crate) struct DictionaryCache {
    state: RwLock<DictCacheState>,
    seq: AtomicU64,
}

impl DictionaryCache {
    fn new() -> Self {
        Self {
            state: RwLock::new(DictCacheState::default()),
            seq: AtomicU64::new(0),
        }
    }

    /// `(table, ctx)` に一致し、現在世代と整合するエントリを探す。世代不一致・
    /// ロック毒化・世代読み取り失敗はいずれも「見つからなかった」として扱う
    /// （fail-closed。[`PrefilterCache::lookup`] と同じ方針）。
    /// キャッシュヒット時は `(dictionary, normalized_index)` を返す（両者は
    /// 同一エントリ・同一世代から `Arc::clone` するだけで、辞書の再走査・
    /// 正規化索引の再構築のいずれも発生しない）。
    fn lookup(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
    ) -> Option<(
        Arc<crate::dictionary::Dictionary>,
        Arc<crate::tiering::NormalizedDictionaryIndex>,
    )> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.state.write().ok()?;
        // 世代はロック取得後に読み直す（`PrefilterCache::lookup` と同じ理由。
        // ロック取得前に読むとロック待機中の他スレッドの挿入を誤って「不一致」と
        // 判定しうる）。
        let current_generation = storage.current_generation().ok()?;
        let position = guard
            .entries
            .iter()
            .position(|e| e.table == table && &e.ctx == ctx)?;
        let stale = guard
            .entries
            .get(position)
            .map(|e| e.built_generation != current_generation)
            .unwrap_or(true);
        if stale {
            guard.entries.remove(position);
            return None;
        }
        let result = {
            let entry = guard.entries.get_mut(position)?;
            entry.last_used = seq;
            (
                Arc::clone(&entry.dictionary),
                Arc::clone(&entry.normalized_index),
            )
        };
        Some(result)
    }

    /// 新規構築した辞書を挿入する。単体で総量上限を超える場合はキャッシュしないが
    /// 呼び出し元へは返す（[`PrefilterCache::insert`] と同じ方針）。挿入対象自身が
    /// 既に古い（並行書き込みで世代が進んだ）場合・世代を確認できない場合は
    /// `None` を返し、呼び出し元へは一切渡さない（PR #249 codex-review P1
    /// 指摘対応: 従来はこの場合もキャッシュへの反映だけを諦め、構築済みの
    /// `Arc` はそのまま呼び出し元へ返していた。`dictionary_snapshot` 側は
    /// 行走査完了後に一度 `observed_generation` を確認しているが、その確認と
    /// 本関数の書き込みロック取得の間に別の書き込みがコミットされる競合が
    /// あり、その場合でも「正常に構築できたスナップショット」として書き込み
    /// 未反映の辞書を返してしまっていた。世代確認・キャッシュ反映可否の決定を
    /// この関数内の単一のロック区間へ統合し、呼び出し元
    /// `EngineCore::dictionary_snapshot` は `None` を「再試行のシグナル」として
    /// 扱う）。
    fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        dictionary: crate::dictionary::Dictionary,
        built_generation: u64,
        approx_bytes: usize,
    ) -> Option<(
        Arc<crate::dictionary::Dictionary>,
        Arc<crate::tiering::NormalizedDictionaryIndex>,
    )> {
        // 正規化索引（`tiering::classify` が使う小文字化済みパス・シンボル名）は
        // ここ・辞書スナップショット構築の 1 回だけ構築する（codex-review P1
        // 指摘対応・PR #261。[`DictCacheEntry::normalized_index`] ドキュメント
        // 参照）。以後は `dictionary` と同じ `Arc` 共有・同じ失効規約で使い回す。
        let normalized_index = Arc::new(crate::tiering::NormalizedDictionaryIndex::build(
            &dictionary,
        ));
        // 正規化索引の保持分（小文字化パス・シンボル名の複製）も容量上限の対象に
        // 含める（漏らすと CPU-DoS 対策のつもりが総量上限の過小評価という別の
        // メモリ上限 P1 になる）。
        let approx_bytes = approx_bytes.saturating_add(normalized_index.approx_heap_bytes());
        let dictionary = Arc::new(dictionary);

        let Ok(mut guard) = self.state.write() else {
            // ロック毒化時は世代整合を検証できないため fail-closed 側へ倒し、
            // 呼び出し元へは何も返さない（Issue #280 で `PrefilterCache::insert` も
            // 同契約に統一済み。本関数は世代確認自体をこのロック区間に統合したため、
            // ロックが取れない時点で世代確認もできていない）。
            return None;
        };

        let Ok(current_generation) = storage.current_generation() else {
            return None;
        };
        if built_generation != current_generation {
            // 挿入対象自身が既に古い（並行書き込みで世代が進んだ）。キャッシュへ
            // 反映しないだけでなく、呼び出し元へも渡さない（上記ドキュメント参照）。
            return None;
        }

        if approx_bytes > MAX_DICTIONARY_CACHE_TOTAL_BYTES {
            return Some((dictionary, normalized_index));
        }

        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && &e.ctx == ctx)
        {
            guard.entries.remove(pos);
        }
        guard
            .entries
            .retain(|e| e.built_generation == current_generation);

        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.approx_bytes)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_DICTIONARY_CACHE_ENTRIES
            || total_bytes.saturating_add(approx_bytes) > MAX_DICTIONARY_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                return Some((dictionary, normalized_index));
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.approx_bytes);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(DictCacheEntry {
            table: table.to_string(),
            ctx: ctx.clone(),
            dictionary: Arc::clone(&dictionary),
            normalized_index: Arc::clone(&normalized_index),
            built_generation,
            approx_bytes,
            last_used: seq,
        });
        Some((dictionary, normalized_index))
    }
}

/// `rls.rs::RlsError` を `CoreError` へ写像する（TASK-169）。呼び出し元
/// （[`EngineCore::search`]）は `IndexStale`/`ContextMismatch` をキャッシュ縮退の
/// トリガーとして先に `match` で処理するため、本関数へはそれ以外の 6 variant のみが
/// 渡る契約とする（呼び出し元の網羅性はコンパイラの exhaustiveness ではなく
/// 呼び出し順序で担保するため、防御的に到達した場合も fail-closed な variant
/// （`ProviderResultRejected`）へ丸め込まず、意味的に対応する variant へ写像する）。
fn map_rls_error(e: RlsError) -> CoreError {
    match e {
        RlsError::Arena(e) => CoreError::Arena(e),
        RlsError::Kernel(e) => CoreError::Kernel(e),
        RlsError::Storage(e) => CoreError::Storage(e),
        RlsError::InvalidK { k } => CoreError::InvalidK { k },
        RlsError::NotFound => CoreError::NotFound,
        RlsError::ProviderResultRejected => CoreError::ProviderResultRejected,
        // 呼び出し元が先に処理する契約の防御的分岐（型ドキュメント参照）。ここへ到達した
        // 場合も fail-closed に拒否する。
        RlsError::ContextMismatch | RlsError::IndexStale => CoreError::ProviderResultRejected,
    }
}

/// `VectorCore` 公開 API のエラー型。下位層（`storage`/`catalog`/`arena`/`kernel`/`policy`）
/// のエラーを一本化しつつ、不可視行と不存在行を [`CoreError::NotFound`] に統合する
/// （呼び出し元へ存在情報を漏らさないため。エラーメッセージはプログラム出力文字列のため英語）。
///
/// `#[non_exhaustive]` は付与しない: 本 enum は既に公開済みであり、後付けで
/// `#[non_exhaustive]` を付けると下流の網羅的 `match` がコンパイル不能になる
/// （それ自体が破壊的変更のため、`#[non_exhaustive]` 化で互換性を装うのではなく
/// 付けないままにする。codex-review PR #252 P1 指摘）。`QueryPlannerUnavailable`・
/// `QueryPlanning`（TASK-110・PLAN-1）・`EmbedderUnavailable`・`QueryEmbedding`
/// （TASK-114・PLAN-10）を含む variant の追加は、`non_exhaustive` 化の有無に
/// 関わらず既存の網羅的 `match` を壊す破壊的変更であることに変わりはない
/// （PR 本文の変更点に明記する）。
#[derive(Debug)]
pub enum CoreError {
    Storage(StorageError),
    Catalog(CatalogError),
    Arena(ArenaError),
    Kernel(KernelError),
    Policy(PolicyError),
    /// `k == 0` または [`MAX_SEARCH_K`] 超過。
    InvalidK {
        k: usize,
    },
    /// 指定行が存在しない、または呼び出し元のテナント・可視性から見えない
    /// （区別しない。fail-closed）。
    NotFound,
    /// `SearchProvider` が返却した `Vec<`[`CandidateHit`]`>` が Top-k の契約
    /// （以下のいずれか）を満たさなかった（codex P0/P1・Issue #137 対応。fail-closed）:
    /// (1) 件数が要求 `k` を超える、(2) コア側で計算した可視行 id 集合に含まれない
    /// `id` を含む（他テナントの id 捏造・実装バグを含む）、(3) `id` が重複する、
    /// (4) スコアが非有限（NaN・Inf）、(5) スコア降順・同点は `id` 昇順という順序
    /// 契約に違反する。違反の種類ごとにエラーを分けると provider 内部の実装詳細が
    /// 呼び出し元へ漏れかねないため、区別せず本 variant に統一する（他テナントの
    /// 存在情報も含めない）。
    ProviderResultRejected,
    /// `dispatch.rs::select_execution_path`（TASK-155・CORE-11）への入力構築が失敗した
    /// （fail-closed。`dim`・`batch_size` の上限検証はここより前段（本モジュールの
    /// スキーマ照合）で既に一致条件を満たしているはずのため、通常到達しない防御的分岐）。
    Dispatch(DispatchError),
    /// `dispatch.rs::select_execution_path` が `ExecutionPath::Gpu` を返した
    /// （CORE-11/12）。単発クエリ経路（`core.rs::EngineCore::search`）は GPU
    /// capability を構造的に持たない（`DispatchInput::for_single_query` は GPU
    /// capability を引数に取らない）ため、決定表が正しく動作する限り到達しない
    /// はずの防御的分岐。GPU 実行を提供する `SearchProvider` 実装が後続タスクで
    /// 追加されるまで fail-closed に拒否する。
    GpuPathUnavailable,
    /// LLM クエリプランニング（TASK-110・PLAN-1）の [`Self::query_planner`] が
    /// 未注入だった（`embedder` 未構成時のファイル形 `INSERT` 拒否と同じ
    /// fail-closed 方針。既定で参照実装を暗黙採用しない）。
    QueryPlannerUnavailable,
    /// LLM クエリプランニング（TASK-110・PLAN-1）のプロンプト組み立て・LLM 呼び出し・
    /// 応答パースのいずれかが失敗した（詳細は [`crate::query_planner::PlanError`]）。
    QueryPlanning(crate::query_planner::PlanError),
    /// 再埋め込み規則（TASK-114・PLAN-10）の [`Self::plan_and_embed_query`] が
    /// 呼ばれたが [`Self::embedder`] が未注入だった（ファイル形 `INSERT` の
    /// embedder 未構成拒否と同じ fail-closed 方針。既定で参照実装を暗黙採用しない）。
    EmbedderUnavailable,
    /// 再埋め込み規則（TASK-114・PLAN-10）の [`Self::plan_and_embed_query`] における
    /// 再埋め込み（[`crate::query_planner::reembed_expansion`]）が失敗した
    /// （テーブル宣言次元と embedder 次元の不一致・埋め込みサービスの不正応答等。
    /// 詳細は [`crate::embedding::EmbedError`]）。
    QueryEmbedding(crate::embedding::EmbedError),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Storage(e) => write!(f, "core storage error: {e}"),
            CoreError::Catalog(e) => write!(f, "core catalog error: {e}"),
            CoreError::Arena(e) => write!(f, "core arena error: {e}"),
            CoreError::Kernel(e) => write!(f, "core kernel error: {e}"),
            CoreError::Policy(e) => write!(f, "core policy error: {e}"),
            CoreError::InvalidK { k } => write!(f, "invalid k: {k} (must be 1..={MAX_SEARCH_K})"),
            CoreError::NotFound => write!(f, "not found"),
            CoreError::ProviderResultRejected => write!(
                f,
                "search provider returned a hit outside the policy-visible id set"
            ),
            CoreError::Dispatch(e) => write!(f, "core dispatch error: {e}"),
            CoreError::GpuPathUnavailable => {
                write!(
                    f,
                    "dispatch selected a gpu execution path with no gpu-capable provider wired"
                )
            }
            CoreError::QueryPlannerUnavailable => write!(f, "no query planner configured"),
            CoreError::QueryPlanning(e) => write!(f, "core query planning error: {e}"),
            CoreError::EmbedderUnavailable => write!(f, "no embedder configured"),
            CoreError::QueryEmbedding(e) => write!(f, "core query embedding error: {e}"),
        }
    }
}

impl std::error::Error for CoreError {}

/// [`EngineCore::open_with_engine`] 専用のエラー型（Issue #407・ANN opt-in 結線）。
///
/// `open_with_engine` は本 Issue で新設する API のため、既に公開済みの
/// [`CoreError`]（`#[non_exhaustive]` を付けない既存方針。新規 variant 追加は
/// 既存利用者の網羅的 `match` を壊す破壊的変更になる）へ variant を追加せず、
/// 独立の型として新設した（codex-review P1 指摘・Issue #407 追記。AGENTS.md
/// 「公開 API・エラー契約の互換性（P1）」が要求する spec 側の対応する定義変更が
/// 無いまま `CoreError` を破壊的変更するのを避けるため）。`CoreError` への
/// 暗黙 `From` 変換は用意しない（`?` 経由で意図せず `CoreError` 側へ新しい
/// エラー経路が生まれるのを防ぐ）。
#[derive(Debug)]
pub enum OpenWithEngineError {
    /// `Storage::open` が失敗した（詳細は [`StorageError`]）。
    Storage(StorageError),
    /// `kind`（`SearchEngineKind::Hnsw`）の検証エラー（詳細は
    /// [`crate::search_engine::SearchEngineError`]）。`SearchEngineKind::Hnsw` は
    /// [`crate::hnsw::ValidatedHnswParams`] を保持し検証済みであることが型で
    /// 保証されているため（`search_engine.rs` モジュールドキュメント「不正な
    /// `HnswParams` を型で到達不能にする」節。codex-review P1 指摘・Issue #407・
    /// PR #433 追記）、[`EngineCore::open_with_engine`] 自体はこの variant を
    /// 返さない。未検証の `HnswParams` を検証する入口は
    /// [`crate::search_engine::hnsw_kind`] であり、そちらで先に拒否される。
    SearchEngine(crate::search_engine::SearchEngineError),
}

impl std::fmt::Display for OpenWithEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenWithEngineError::Storage(e) => write!(f, "core storage error: {e}"),
            OpenWithEngineError::SearchEngine(e) => write!(f, "core search engine error: {e}"),
        }
    }
}

impl std::error::Error for OpenWithEngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenWithEngineError::Storage(e) => Some(e),
            OpenWithEngineError::SearchEngine(e) => Some(e),
        }
    }
}

impl From<DispatchError> for CoreError {
    fn from(e: DispatchError) -> Self {
        CoreError::Dispatch(e)
    }
}

impl From<crate::query_planner::PlanError> for CoreError {
    fn from(e: crate::query_planner::PlanError) -> Self {
        CoreError::QueryPlanning(e)
    }
}

/// [`EngineCore::query_planner`] が保持する注入形態（TASK-115・PLAN-8）。単一クライアント
/// （TASK-110 の既存契約。[`EngineCore::with_query_planner`]）と、質問類型（`crate::tiering`）
/// に応じてティア別クライアントを振り分ける構成（[`EngineCore::with_tiered_query_planner`]）の
/// 二択を型で表し、両方同時設定という不整合状態を作れなくする。
enum PlannerBinding {
    /// TASK-110 時点の単一クライアント注入（既存の [`EngineCore::with_query_planner`]・
    /// [`EngineCore::plan_query`] の挙動・契約を変えない）。
    Single(Box<dyn crate::query_planner::LlmClient>),
    /// TASK-115・PLAN-8 のティア別クライアント注入（`crate::tiering::TieredPlanner`）。
    Tiered(crate::tiering::TieredPlanner),
}

impl From<crate::embedding::EmbedError> for CoreError {
    fn from(e: crate::embedding::EmbedError) -> Self {
        CoreError::QueryEmbedding(e)
    }
}

impl From<StorageError> for CoreError {
    fn from(e: StorageError) -> Self {
        CoreError::Storage(e)
    }
}

impl From<CatalogError> for CoreError {
    fn from(e: CatalogError) -> Self {
        CoreError::Catalog(e)
    }
}

impl From<ArenaError> for CoreError {
    fn from(e: ArenaError) -> Self {
        CoreError::Arena(e)
    }
}

impl From<KernelError> for CoreError {
    fn from(e: KernelError) -> Self {
        CoreError::Kernel(e)
    }
}

impl From<PolicyError> for CoreError {
    fn from(e: PolicyError) -> Self {
        CoreError::Policy(e)
    }
}

/// wire-server（および将来の他プロトコル実装）が依存する唯一の窓口（CORE-1）。
/// 最小 API（policy 付き検索・行取得）のみを持ち、認証・SQL 表層は後続タスクで
/// 拡張する。object-safe を保つ（ジェネリクスなし）。
pub trait VectorCore: Send + Sync {
    /// `table` に対して `query` の Top-k 検索を行う。`ctx` のテナント・可視性を満たさない
    /// 行は結果に含めない（CORE-2）。
    ///
    /// 返却される [`SearchHit`] は `(tenant_id, id)` で行を一意に指す（対象ビヘイビア:
    /// TABLE-12・RLS-9。codex-review P1 指摘・PR #194 対応）。行 `id` の一意性スコープは
    /// テナント内に閉じているため**同一 `id` の hit が複数返り得る**（自テナントの行と、
    /// 他テナントの `Public` 行が同じ `id` を持つ場合。いずれも `ctx` から可視な行であり、
    /// テナント境界の侵害ではない）が、`tenant_id` が付随するため呼び出し元は常に
    /// 両者を判別でき、`get_row(ctx, table, &hit.tenant_id, hit.id)` でそのヒット自身の
    /// 行を取得できる。
    ///
    /// 順序はスコア降順・同点は候補識別子（内部のスロット番号。実質 `(tenant_id, id)`
    /// 昇順で、単一テナント内では行 `id` 昇順と一致）で決定的
    /// （`docs/design/rrf-tie-break-determinism.md` の順序契約）。
    fn search(
        &self,
        ctx: &PolicyContext,
        table: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, CoreError>;

    /// `table` から `id` の行を 1 件取得する。不可視・不存在は区別せず
    /// [`CoreError::NotFound`] を返す。
    /// `(tenant_id, id)` で行を 1 件取得する（対象ビヘイビア: TABLE-12・RLS-9）。
    ///
    /// 行 `id` の一意性スコープはテナント内に閉じているため、行を指すには所有テナントが
    /// 必要（[`crate::kernel::SearchHit::tenant_id`] をそのまま渡せる契約。
    /// codex-review P1 指摘・PR #194）。取得できるのは `ctx` から**可視な**行のみで、
    /// 不存在と不可視は区別せず [`CoreError::NotFound`] に統一する（他テナント行の
    /// 存在探査に使えないようにする fail-closed な扱い。security.md P0）。
    fn get_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        tenant_id: &str,
        id: u64,
    ) -> Result<Row, CoreError>;
}

/// [`EngineCore::plan_using_plan_expansion`] の戻り値。`USING PLAN` 経路の
/// I/O（LLM 展開・再埋め込み）結果のみを保持し、列インデックス解決（スキーマ
/// 依存）は含まない（呼び出し元が I/O 完了後に取得し直した最新スキーマで
/// `sql::using_plan::bind_expansion` へ渡す。同メソッドのドキュメント参照）。
struct UsingPlanExpansionResult {
    expansion: crate::query_planner::QueryExpansion,
    query_vector: Vec<f32>,
    resolved_mode: crate::sql::mode::ResolvedMode,
}

/// [`EngineCore::execute_bound_plan_search_in_session`] の `bind` closure が
/// 返す束縛済み部品（TASK-186・NOSQL-2。Issue #764）。`Ranking::Hybrid` の
/// `query`（再埋め込みベクトル）・`text_column_index`・`query_text`・`mode`
/// （`resolved_mode`）は engine 側（[`EngineCore::run_using_plan_select`] の
/// `bind` closure 内、[`crate::sql::using_plan::bind_expansion`] と同一の
/// ロジック）が組み立てるため、本構造体は含まない（NoSQL 表層からクエリ
/// ベクトル・ランキング方式・モードを差し替えられない設計。
/// `.claude/rules/security.md`「アクセス制御の不備」対応）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PlanSearchBinding {
    projection: Vec<crate::sql::parser::ProjectedColumn>,
    metadata_filters: Vec<crate::declarative_filter::MetadataFilter>,
}

impl PlanSearchBinding {
    pub fn new(
        projection: Vec<crate::sql::parser::ProjectedColumn>,
        metadata_filters: Vec<crate::declarative_filter::MetadataFilter>,
    ) -> Self {
        Self {
            projection,
            metadata_filters,
        }
    }
}

/// `VectorCore` の製品実装。永続化・カタログ・アリーナ構築・検索 provider を束ねる。
///
/// 実行バックエンド実装型へ直接依存せず `Box<dyn SearchProvider>` で保持する（CORE-13）。
/// 既定コンストラクタ（[`Self::open`]）は `search_engine::default_engine()`（TASK-131・
/// CORE-9。現時点は CPU-only のマルチスレッド並列総当たり Top-k、ベクトル化は行わない）を
/// 注入し、この構成だけで全機能が成立する。将来の ANN provider 追加は
/// `search_engine.rs` 側の選択肢拡張で完結し、本構造体・`VectorCore` の API は変わらない。
pub struct EngineCore {
    storage: Storage,
    provider: Box<dyn SearchProvider>,
    /// `rls.rs::PrefilterSnapshot` の世代整合キャッシュ（TASK-169）。
    /// [`VectorCore::search`] 実装がこれを経由して事前フィルタインデックスを再利用する
    /// （詳細は [`PrefilterCache`] のドキュメント参照）。
    prefilter_cache: PrefilterCache,
    /// `precision` モードの実行契約を制御するサーバー側設定値（TASK-162・SEARCH-9）。
    /// クエリ・セッション変数から到達できる経路を持たない（差し替えは
    /// [`Self::with_precision_policy`] のみ。`crate::precision` モジュール
    /// ドキュメントの fail-open 不在の設計制約を参照）。
    precision_policy: crate::precision::PrecisionPolicy,
    /// ファイル形 `INSERT`（TASK-120・INDEX-1, INDEX-2）のチャンク本文をベクトルへ
    /// 変換する注入点。未設定（`None`）はファイル形 `INSERT` を fail-closed に
    /// 拒否する契約（既定で参照実装を暗黙採用しない。`embedding.rs` モジュール
    /// ドキュメント参照）。差し替えは [`Self::with_embedder`] のみ。
    embedder: Option<Box<dyn crate::embedding::Embedder>>,
    /// LLM クエリプランニング（TASK-110・PLAN-1／TASK-115・PLAN-8）のクエリ展開
    /// クライアント注入点。未設定（`None`）は [`Self::plan_query`] を fail-closed に
    /// 拒否する契約（`embedder` と同じ流儀。既定で参照実装を暗黙採用しない）。
    /// 単一クライアント注入（[`Self::with_query_planner`]）とティア別クライアント注入
    /// （[`Self::with_tiered_query_planner`]・TASK-115・`crate::tiering`）のどちらか
    /// 一方のみを保持する契約を [`PlannerBinding`] の型で表す（両方同時設定という
    /// 不整合状態を型で作れなくする。`Option<PlannerBinding>` の内側に 2 つ目の
    /// `Option` を並べる設計だと両方 `Some` の状態を許してしまうため採らない）。
    query_planner: Option<PlannerBinding>,
    /// ファイル形 `INSERT` のチャンク化・チャンク数上限設定（TASK-120）。
    /// 差し替えは [`Self::with_incremental_config`] のみ。
    incremental_config: crate::incremental::IncrementalConfig,
    /// `operation_id` 必須化ガード（TASK-92・対象ビヘイビア: RECOVER-1）を制御する
    /// サーバー側構成。クエリ・セッション変数から到達できる経路を持たない（差し替えは
    /// [`Self::with_ledger_mode`] のみ。`crate::recovery::required_op_id` モジュール
    /// ドキュメント参照）。
    ledger_mode: LedgerMode,
    /// 一括投入（複数ファイルのバッチ投入）に対する 4 種の処理量上限（TASK-122・
    /// 対象ビヘイビア: INDEX-4）。差し替えは [`Self::with_batch_limits`] のみ。
    /// `crate::batch_limits` モジュールドキュメント参照。
    batch_limits: crate::batch_limits::BatchLimits,
    /// 明示トランザクション（SQL-31・TASK-221）の持続時間・文数の上限。差し替えは
    /// [`Self::with_transaction_limits`] のみ。
    transaction_limits: crate::sql::transaction::TransactionLimits,
    /// `dictionary.rs` の辞書的情報源（TASK-109・PLAN-5）の世代整合キャッシュ。
    /// [`Self::dictionary_snapshot`] がこれを経由して再構築を再利用する（詳細は
    /// [`DictionaryCache`] のドキュメント参照）。
    dictionary_cache: DictionaryCache,
    /// 辞書的情報源抽出の設定（TASK-109・PLAN-5）。差し替えは
    /// [`Self::with_dictionary_config`] のみ。クエリ・セッション変数から到達できる
    /// 経路は持たない（[`Self::with_precision_policy`] と同じ流儀）。
    dictionary_config: crate::dictionary::DictionaryConfig,
    /// `sql::exec::execute_statement` の hybrid 実行が参照する
    /// `crate::sparse::SparseIndex`（BM25 語彙・統計）のテーブル世代整合キャッシュ
    /// （Issue #357）。詳細は `sql/sparse_cache.rs::SparseIndexCache` のドキュメント
    /// 参照。
    sparse_index_cache: crate::sql::sparse_cache::SparseIndexCache,
    /// SQL 表層（`sql::exec::execute_statement_with_cache`）専用の `VectorArena`
    /// テーブル世代整合キャッシュ（Issue #363）。[`Self::execute_validated_in_session`]
    /// の `Statement::Select` アームがこれを経由してアリーナ再構築（redb 全行走査・
    /// デコード）を同一テーブル世代内で再利用する（詳細は
    /// `sql::arena_cache::SqlArenaCache` のドキュメント参照）。
    sql_arena_cache: crate::sql::arena_cache::SqlArenaCache,
    /// スカラー列二次索引（`sql::exec::execute_statement_with_cache` の gated
    /// 構築。Issue #473）のテーブル世代整合キャッシュ。索引はまだ誰にも
    /// 消費されない派生データであり（候補削減は Issue #474）、
    /// [`Self::scalar_index_cache_stats`] のみが観測用に参照する。詳細は
    /// `sql::scalar_index::ScalarIndexCache` のドキュメント参照。
    scalar_index_cache: crate::sql::scalar_index::ScalarIndexCache,
    /// `sql::aggregate::execute_aggregate`（`GROUP BY` なし・`WHERE` なしの
    /// `DecodeTier::Fast` 単一行集計）専用の可視行テーブル世代整合キャッシュ
    /// （Issue #478）。ヒット時は `user_rows/{table}` を一切開かずに `COUNT(*)`
    /// 等を計算できる（詳細は `sql::visible_cache::VisibleBitmapCache` のドキュメント
    /// 参照）。`GROUP BY` 集計（`sql::group_by`）・`WHERE` 付き集計・SELECT
    /// （DISTANCE/hybrid）経路はこのキャッシュの対象外（スコープ外。ADR
    /// `docs/design/visible-bitmap-cache.md` 参照）。
    visible_bitmap_cache: crate::sql::visible_cache::VisibleBitmapCache,
    /// 構築時に明示指定された [`crate::search_engine::SearchEngineKind`]（Issue #407）。
    /// [`Self::open_with_engine`]／[`Self::from_storage_with_engine`] 経由なら
    /// `Some(kind)`、任意 provider を直接注入する [`Self::with_provider`]／
    /// [`Self::from_storage`] 経由（`kind` と `provider` の対応をこの構造体自身は
    /// 検証できない）なら `None`。`#[non_exhaustive]` な `SearchEngineKind` を返す
    /// 診断用のアクセサ（[`Self::search_engine_kind`]）で、#411 の `EXPLAIN` 露出が
    /// 参照する契約点。
    search_engine_kind: Option<crate::search_engine::SearchEngineKind>,
    /// `search_engine_kind == Some(SearchEngineKind::Hnsw(..))` の場合にのみ構築する
    /// HNSW 索引のテーブル世代整合キャッシュ（Issue #408・親 #402・前提 #404〜#407）。
    /// [`Self::execute_validated_in_session`] の `Statement::Select` アームがこれを
    /// 経由して `Ranking::Distance`（フィルタなし）クエリの DISTANCE 段を索引探索へ
    /// 載せ替える。`SearchEngineKind::Hnsw` 以外（既定エンジン・`with_provider` 等）
    /// では `None` のまま（常に従来どおり全件 brute-force。詳細は
    /// `sql::hnsw_cache::HnswIndexCache` のドキュメント参照）。
    hnsw_state: Option<HnswEngineState>,
}
/// [`EngineCore::hnsw_state`] が保持する状態束（Issue #408）。`provider` は
/// [`crate::hnsw::provider::HnswSearchProvider`]（`Copy`）のコピーであり、
/// `sql::hnsw_cache::search_or_fallback` が `effective_ef`／構築パラメータへ
/// アクセスするために使う（`self.provider`〔`Box<dyn SearchProvider>`〕とは別に
/// 具体型のコピーを保持する。`Box<dyn SearchProvider>` からは `HnswSearchProvider`
/// 固有のメソッドへダウンキャストできないため）。
struct HnswEngineState {
    provider: crate::hnsw::provider::HnswSearchProvider,
    cache: crate::sql::hnsw_cache::HnswIndexCache,
}

/// [`EngineCore::dictionary_snapshot`] が要求する `path`/`body` 列
/// （列名の存在・`ColumnType::Text`・non-nullable の 3 条件、TASK-109・PLAN-5）を
/// `schema` が満たすか検証し、満たす場合はそれぞれの列インデックスを返す。
///
/// `USING PLAN('<query>')`（TASK-77・SQL-5）の LLM 呼び出し（`EngineCore::
/// plan_using_plan_expansion` 内の `plan_query`）は内部で `dictionary_snapshot` を
/// 呼ぶため、この判定条件を満たさないスキーマは最終的に `dictionary_snapshot` の
/// 失敗として現れる。呼び出し元ごとに要求するエラー型が異なる
/// （`dictionary_snapshot` は `CoreError`、`USING PLAN` の事前検証は
/// `SqlSurfaceError::invalid_input`〔`22000`〕）ため、本関数は判定条件のみを
/// 共有し、固定メッセージの `String` を返す（各呼び出し元が自分の契約へ変換する。
/// codex-review P1 指摘対応、PR #266）。
fn dictionary_required_columns(
    schema: &crate::catalog::TableSchema,
) -> Result<(usize, usize), String> {
    let path_idx = schema
        .columns
        .iter()
        .position(|c| c.name == "path" && c.ty == crate::catalog::ColumnType::Text && !c.nullable)
        .ok_or_else(|| {
            "table has no non-nullable text path column required for dictionary extraction"
                .to_string()
        })?;
    let body_idx = schema
        .columns
        .iter()
        .position(|c| c.name == "body" && c.ty == crate::catalog::ColumnType::Text && !c.nullable)
        .ok_or_else(|| {
            "table has no non-nullable text body column required for dictionary extraction"
                .to_string()
        })?;
    Ok((path_idx, body_idx))
}

/// [`Self::execute_insert_sql`]・[`Self::execute_insert_sql_batch`] が
/// `sql::allowlist::validate_insert` へ渡す [`crate::sql::allowlist::TableLookup`]
/// 実装（Issue #485・単文 INSERT 経路の上位段改善）。
///
/// 従来は 1 文の INSERT につき `validate_insert` 内の `Storage::table_exists`
/// （`get_table_schema` 呼び出し・read txn #1）と、束縛のために呼ぶ
/// `Storage::get_table_schema`（read txn #2）の 2 回、スキーマを別々の
/// read txn・別々のデコードで取得していた（write txn 内で行う
/// `require_table_schema_write` の TOCTOU 再読込とは別に、読み取り専用の
/// 事前検証段だけで 2 回）。本構造体は `table_exists` が最初に取得した
/// スキーマを [`std::cell::RefCell`] へ保持し、直後の束縛が
/// [`Self::take_schema`] で再取得できるようにすることで、read txn・
/// decode を 1 回に減らす（`docs/design/ingest-stage-profile.md`
/// 「Issue #485 追記」参照）。
///
/// スキーマを保持する契約は「`table_exists` に最後に渡されたテーブル名と
/// 一致する場合のみ」に限定する（`take_schema` 側でも名前を突き合わせる。
/// `validate_insert` は 1 文につき `table_exists` を 1 回しか呼ばないため
/// 通常は不一致が起きないが、将来の呼び出し順変更に対する fail-closed な
/// 防御であり、不一致時は呼び出し元が既存の `get_table_schema` 経路へ
/// フォールバックする）。カタログ照会自体が失敗した場合のエラー写像は
/// `catalog::table_lookup_error`（`impl TableLookup for Storage` と共有）
/// を使い、`table_exists` 単体と文言・`wire_code` を一致させる。
/// [`EngineCore::begin_copy`]（Issue #939・WIRE-17）の戻り値。`FROM STDIN` 形は
/// ストリーミング取り込みセッション（[`crate::sql::copy::CopyInSession`]）を、
/// `TO STDOUT` 形は即座に実行済みの結果集合（広域取得と同じ実行本体。
/// `EngineCore::run_scan_plan` 参照）を保持する。
#[derive(Debug)]
pub enum CopyPlan {
    // `CopyInSession` を直接（Box なしで）保持する。`TableSchema` へのフィールド
    // 追加（TABLE-19・Issue #901）で `CopyInSession` 自体のサイズが増え、
    // 一時的に `Box` でこの variant を包む対応を検討したが、公開 enum の
    // variant 内包型を変える破壊的変更になるため、代わりに
    // `CopyInSession` 内部の非公開フィールド `schema` を Box 化して
    // `clippy::large_enum_variant` を解消した（`sql::copy::CopyInSession`
    // 参照）。ここでの内包型・意味論はいずれも無変更。
    From(crate::sql::copy::CopyInSession),
    To(
        crate::sql::allowlist::CopyFormat,
        crate::sql::exec::QueryResult,
    ),
}

/// [`EngineCore::parse_sql`] の結果（Issue #933・TASK-71・WIRE-11）。SQL テキストの
/// 許可リスト検証（`sql::allowlist` の各 `validate_*`）を通過した束縛前の構文形を
/// 一意に運ぶ。行・台帳・世代のいずれにも触れておらず、
/// [`EngineCore::execute_parsed_in_session`]（実行）・
/// [`EngineCore::describe_parsed_in_session`]（結果列メタデータの導出）の
/// いずれの入力にもなる（拡張クエリプロトコルの Parse が構築し、Describe・
/// Execute の双方が同じ値を再利用できるようにするための型。#934 の Execute が
/// 実際にこの再利用を行う）。
///
/// `#[non_exhaustive]`: 将来の許可形状追加（新しい DML・DDL 相当の文種別）で
/// variant を増やしても、クレート外の網羅 `match` を破壊的変更にしないため。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ParsedSql {
    /// `SELECT`／`SET search_mode`／`CREATE FUNCTION`／`EXPLAIN`（許可リスト
    /// 検証は `sql::allowlist::validate_sql` が担う）。
    Statement(crate::sql::allowlist::Statement),
    /// `INSERT`（単一行・複数行 `VALUES`・`ON CONFLICT`・`RETURNING` を含む。
    /// SQL-10・SQL-16・SQL-20・SQL-21）。
    Insert(crate::sql::allowlist::ValidatedInsert),
    /// `TRUNCATE TABLE ... USING OPERATION_ID '<id>'`（SQL-22）。
    Truncate(crate::sql::allowlist::ValidatedTruncate),
    /// `DELETE`（単一行・`id` 完全一致形／述語形。SQL-18・SQL-19）。
    Delete(crate::sql::allowlist::DeleteStatement),
    /// `UPDATE`（単一行・`id` 完全一致形／述語形。SQL-17・SQL-19）。
    Update(crate::sql::allowlist::ValidatedUpdateForm),
    /// `BEGIN`／`COMMIT`／`ROLLBACK`（SQL-31・TASK-221）。トランザクション文脈を
    /// 持たない [`EngineCore::execute_parsed_in_session`] はこの variant を
    /// `0A000` で拒否する（トランザクション対応の実行入口は
    /// [`EngineCore::execute_sql_in_txn`]／[`EngineCore::execute_parsed_in_txn`]）。
    Transaction(crate::sql::transaction::TxnControl),
    /// `DROP TABLE <table>`（SQL-23・TASK-203、Issue #902）。DDL 実行権限ゲート
    /// （`sql::ddl::require_ddl_permission`）の判定は
    /// `EngineCore::execute_parsed_in_session` が担い、`validate_drop_table_tokens`
    /// 自体はカタログ照会を一切行わない（`ValidatedDropTable` ドキュメント参照）。
    ///
    /// **BREAKING CHANGE**（Issue #902）: 本 variant の追加により `ParsedSql` を
    /// 網羅的にマッチする既存コード（`crate::core::EngineCore`）はすべて
    /// 更新済み。クレート外で `ParsedSql` を網羅的にマッチするコードがあれば
    /// 追随が必要。
    DropTable(crate::sql::allowlist::ValidatedDropTable),
    /// `CREATE TABLE <table> (...)`（SQL-23・TASK-85・TASK-202、Issue #899）。
    /// DDL 実行権限ゲート（`sql::ddl::require_ddl_permission`）の判定は
    /// [`DropTable`](Self::DropTable) と同じく `EngineCore::
    /// execute_parsed_in_session` が担い、`validate_create_table_tokens` 自体は
    /// カタログ照会（テーブル名の重複判定）を一切行わない
    /// （`ValidatedCreateTable` ドキュメント参照。既存名との衝突判定は
    /// `sql::ddl::execute_create_table` が単一の書き込みトランザクション内で
    /// TOCTOU なく行う）。
    ///
    /// **BREAKING CHANGE**（Issue #899）: 本 variant の追加により `ParsedSql` を
    /// 網羅的にマッチする既存コード（`crate::core::EngineCore`）はすべて
    /// 更新済み。クレート外で `ParsedSql` を網羅的にマッチするコードがあれば
    /// 追随が必要。
    CreateTable(crate::sql::allowlist::ValidatedCreateTable),
    /// `DECLARE`/`FETCH`/`CLOSE`（カーソル。WIRE-15・TASK-218）。トランザクション
    /// 文脈を持たない [`EngineCore::execute_parsed_in_session`] はこの variant を
    /// `DECLARE` なら `25P01`（`NoActiveSqlTransaction`）、`FETCH`／`CLOSE` なら
    /// `34000`（`InvalidCursorName`）で拒否する（カーソルは明示トランザクション
    /// 内でのみ有効。`sql::cursor` モジュールドキュメント参照）。
    ///
    /// **BREAKING CHANGE**（WIRE-15・TASK-218）: 本 variant の追加により
    /// `ParsedSql` を網羅的にマッチする既存コード（`crate::core::EngineCore`）は
    /// すべて更新済み。クレート外で `ParsedSql` を網羅的にマッチするコードが
    /// あれば追随が必要。
    Cursor(crate::sql::cursor::CursorStatement),
    /// `ALTER TABLE <table> ADD COLUMN <column> <type>`（SQL-23・TASK-202、
    /// Issue #900）。DDL 実行権限ゲート（`sql::ddl::require_ddl_permission`）の
    /// 判定は [`DropTable`](Self::DropTable)／[`CreateTable`](Self::CreateTable)
    /// と同じく `EngineCore::execute_parsed_in_session` が担い、
    /// `validate_alter_table_tokens` 自体はカタログ照会（テーブル・列・ENUM 型の
    /// 存在確認）を一切行わない（`ValidatedAlterTableAddColumn` ドキュメント参照）。
    ///
    /// **BREAKING CHANGE**（Issue #900）: 本 variant の追加により `ParsedSql` を
    /// 網羅的にマッチする既存コード（`crate::core::EngineCore`）はすべて
    /// 更新済み。クレート外で `ParsedSql` を網羅的にマッチするコードがあれば
    /// 追随が必要。
    AlterTable(crate::sql::allowlist::ValidatedAlterTableAddColumn),
    /// `CREATE VIEW <name> AS <body>`（TABLE-18・SQL-23・TASK-205、
    /// Issue #909）。DDL 実行権限ゲート（`sql::ddl::require_ddl_permission`）の
    /// 判定は `EngineCore::execute_parsed_in_session` が担い、
    /// `validate_create_view_tokens` 自体はカタログ照会を一切行わない
    /// （`ValidatedDropTable` と同じ設計）。
    ///
    /// **BREAKING CHANGE**（Issue #909）: 本 variant の追加により `ParsedSql` を
    /// 網羅的にマッチする既存コードはすべて更新済み。
    CreateView(crate::sql::allowlist::ValidatedCreateView),
    /// `DROP VIEW <name>`（TABLE-18・SQL-23・TASK-205、Issue #909）。
    ///
    /// **BREAKING CHANGE**（Issue #909）: 本 variant の追加により `ParsedSql` を
    /// 網羅的にマッチする既存コードはすべて更新済み。
    DropView(crate::sql::allowlist::ValidatedDropView),
    /// `CREATE INDEX <name> ON <table> [USING hnsw] (<col>[, ...])`（TASK-206・
    /// INDEX-7・SQL-23、Issue #908）。DDL 実行権限ゲート
    /// （`sql::ddl::require_ddl_permission`）の判定は `EngineCore::
    /// execute_parsed_in_session` が担い、`validate_create_index_tokens` 自体は
    /// カタログ照会を一切行わない（`CreateView` と同じ設計）。
    CreateIndex(crate::sql::allowlist::ValidatedCreateIndex),
    /// `DROP INDEX <name>`（TASK-206・INDEX-7・SQL-23、Issue #908）。
    DropIndex(crate::sql::allowlist::ValidatedDropIndex),
}

/// `parsed` が保持する `operation_id`（`USING OPERATION_ID '<id>'`。書き込み系
/// 文のみ保持しうる）。[`EngineCore::execute_parsed_in_txn`] が同一トランザクション
/// 内での再利用検査（[`crate::sql::transaction::SessionTransaction::
/// check_and_register_statement`]）に使う。明示トランザクション内で対応外の
/// 文種別（[`EngineCore::execute_in_active_txn`] が `0A000` で拒否する）は
/// `None` を返す（どのみち実行されないため再利用検査は不要）。
fn parsed_operation_id(parsed: &ParsedSql) -> Option<&str> {
    match parsed {
        ParsedSql::Insert(stmt) => stmt.operation_id.as_ref().map(|id| id.as_str()),
        ParsedSql::Truncate(stmt) => stmt.operation_id.as_ref().map(|id| id.as_str()),
        _ => None,
    }
}

/// [`EngineCore::parse_sql_prepared`] の結果（Issue #935・WIRE-12・TASK-217）。
/// 拡張クエリプロトコルの Parse が `$n` を含む SQL テキストを値未確定のまま
/// 構造検証した「束縛前テンプレート」を運ぶ。行・台帳・世代のいずれにも触れて
/// いない。[`EngineCore::bind_prepared`]（Bind。実値を束縛して [`ParsedSql`] を
/// 得る）・[`EngineCore::describe_prepared_in_session`]（Describe。値未確定の
/// ままダミー値束縛済みの結果を再利用する）の入力になる。
///
/// フィールドは非公開（構築は [`EngineCore::parse_sql_prepared`] のみ）。値は
/// 常に `tokens` 内の [`crate::sql::lexer::Token::StringLiteral`]（Bind 時に
/// 置換されるまでは `Token::Param`）としてのみ保持され、SQL テキストへは
/// 一度も戻らない（インジェクション経路を作らない設計。`sql::params`
/// モジュールドキュメント参照）。
#[derive(Debug, Clone)]
pub struct PreparedSql {
    tokens: Vec<crate::sql::lexer::Token>,
    param_count: u16,
    dummy_parsed: ParsedSql,
    /// `dummy_parsed` の `ORDER BY` ベクトルリテラル（`Distance` 形。
    /// [`crate::sql::allowlist::OrderByForm::Distance`]）が、元の SQL テキストの
    /// `$n`（[`crate::sql::params::order_by_distance_literal_is_param`]）に由来する
    /// ダミー値かどうか。`true` の場合に限り `describe_prepared_in_session` は
    /// このリテラルの実パースを省略してよい（PR #1012 レビュー指摘対応:
    /// `$n` を含まない文では `false` のままとなり、`dummy_parsed` に残る実
    /// リテラルは通常の Describe と同じく必ず検証される）。
    order_by_distance_literal_is_param: bool,
    /// `dummy_parsed` の `WHERE` 等価述語（[`crate::sql::allowlist::
    /// WherePredicate::Equality`]）を出現順に並べ、各々の値が元の SQL
    /// テキストの `$n`（[`crate::sql::params::where_equality_literal_is_param`]）
    /// に由来するダミー値かどうかを示す。`true` の位置に限り
    /// `describe_prepared_in_session` は ENUM 列の語彙照合を省略してよい
    /// （PR #1012 Cursor Bugbot 指摘対応: `$n` を含まない等価述語では `false`
    /// のままとなり、実リテラルは通常の Describe と同じく必ず検証される）。
    where_equality_dummy_flags: Vec<bool>,
}

impl PreparedSql {
    /// 文が要求するパラメータ数（`$n` の最大番号。1 始まり。`$n` を含まない
    /// 文は 0）。[`EngineCore::bind_prepared`] に渡す `values` の個数と一致する
    /// 必要がある。
    pub fn param_count(&self) -> u16 {
        self.param_count
    }
}

struct InsertSchemaLookup<'a> {
    storage: &'a Storage,
    cached: std::cell::RefCell<Option<(String, TableSchema)>>,
}

impl<'a> InsertSchemaLookup<'a> {
    fn new(storage: &'a Storage) -> Self {
        Self {
            storage,
            cached: std::cell::RefCell::new(None),
        }
    }

    /// `table_exists` が最後に取得したスキーマを、要求されたテーブル名と
    /// 一致する場合にのみ返す（消費型。1 回取り出したら空になる）。
    fn take_schema(&self, table_name: &str) -> Option<TableSchema> {
        let mut cached = self.cached.borrow_mut();
        match cached.take() {
            Some((name, schema)) if name == table_name => Some(schema),
            other => {
                // 名前不一致・未保持のいずれも fail-closed に空へ戻す
                // （呼び出し元は既存の `get_table_schema` 経路へフォール
                // バックするため、ここで誤ったスキーマを返してはならない）。
                *cached = other.filter(|(name, _)| name == table_name);
                None
            }
        }
    }
}

impl crate::sql::allowlist::TableLookup for InsertSchemaLookup<'_> {
    fn table_exists(&self, name: &str) -> Result<bool, crate::sql::allowlist::SqlSurfaceError> {
        match self.storage.get_table_schema(name) {
            Ok(schema) => {
                *self.cached.borrow_mut() = Some((name.to_string(), schema));
                Ok(true)
            }
            Err(CatalogError::TableNotFound(_)) => Ok(false),
            Err(other) => Err(table_lookup_error(other)),
        }
    }
}

impl EngineCore {
    /// 指定パスの `redb` データベースを開き、既定の検索エンジン
    /// （[`crate::search_engine::default_engine`]）を注入した `EngineCore` を構築する。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        // `open_with_engine`（`OpenWithEngineError` を返す opt-in 経路）へは
        // 委譲しない: `search_engine::default_engine()` は infallible（`build` と
        // 同じく `SearchEngineKind` が保証する検証済み値しか受け取らない。
        // `search_engine.rs` モジュールドキュメント参照）なので、`CoreError` 側の
        // 型を一切変えずに済む（codex-review P1 指摘・Issue #407 追記。
        // `OpenWithEngineError` 新設の経緯は `docs/design/hnsw-search-engine-wiring.md`
        // 参照）。
        let provider = search_engine::default_engine();
        let storage = Storage::open(path)?;
        Ok(Self::assemble(
            storage,
            provider,
            Some(search_engine::default_kind()),
        ))
    }

    /// [`Self::open`] の durability 版（Issue #849・構築時オプション）。
    /// `durability` に既定値（[`crate::storage::WriteDurability::default`]）を渡した
    /// 場合は [`Self::open`] と完全に同一の挙動になる（両者は同じ
    /// [`Storage::open_with_durability`] 呼び出しへ委譲し実装を共有するため、
    /// この等価性は構造的に保証される）。durability を非既定値にした場合の
    /// 損失ウィンドウは [`crate::storage::WriteDurability`] のドキュメンテーション
    /// コメント・`docs/design/ingest-write-path.md`「Issue #849 追記」節参照。
    pub fn open_with_durability(
        path: impl AsRef<Path>,
        durability: crate::storage::WriteDurability,
    ) -> Result<Self, CoreError> {
        let provider = search_engine::default_engine();
        let storage = Storage::open_with_durability(path, durability)?;
        Ok(Self::assemble(
            storage,
            provider,
            Some(search_engine::default_kind()),
        ))
    }

    /// 検索 provider を差し替えて構築する（テスト・将来の GPU/ANN provider 導入用）。
    ///
    /// `provider` は呼び出し元が直接組み立てた値であり、対応する
    /// [`crate::search_engine::SearchEngineKind`] を構造的に持たないため
    /// [`Self::search_engine_kind`] は常に `None` を返す（Issue #407。opt-in で
    /// `kind` を伴う構築は [`Self::open_with_engine`] を使う）。
    pub fn with_provider(
        path: impl AsRef<Path>,
        provider: Box<dyn SearchProvider>,
    ) -> Result<Self, CoreError> {
        let storage = Storage::open(path)?;
        Ok(Self::assemble(storage, provider, None))
    }

    /// 指定パスの `redb` データベースを開き、`kind` が構築する検索エンジンを
    /// 注入した `EngineCore` を構築する（Issue #407・opt-in 経路。ADR
    /// `docs/design/ann-index-adoption.md` B 案「テーブル単位カタログ属性は対象外」の
    /// とおり、本関数の呼び出し元がプロセス起動時に明示指定する以外の経路
    /// （環境変数・SQL 構文・セッション変数）は持たない）。
    ///
    /// `kind`（`SearchEngineKind::Hnsw` の場合は
    /// [`crate::hnsw::ValidatedHnswParams`] を保持）は型で検証済みであることが
    /// 保証されているため（`search_engine.rs` モジュールドキュメント参照）、
    /// 本関数自体は Hnsw 検証を理由に失敗しない。未検証の `HnswParams` を渡したい
    /// 呼び出し元は、先に [`crate::search_engine::hnsw_kind`] で `kind` を得ること
    /// （fail-closed に拒否されれば `EngineCore` は構築されない）。
    ///
    /// 戻り値の型は既存の公開 [`CoreError`] ではなく専用の [`OpenWithEngineError`]
    /// とする（`CoreError` は既に公開済みの `#[non_exhaustive]` を付けない enum
    /// であり、本関数のためだけに新規 variant を追加すると既存利用者の網羅的
    /// `match` を壊す破壊的変更になる。本関数自体は Issue #407 で新設する API
    /// のため、独立の戻り値型を選ぶこと自体は破壊的変更に当たらない。
    /// codex-review P1 指摘・Issue #407 追記。経緯は
    /// `docs/design/hnsw-search-engine-wiring.md` 参照）。
    pub fn open_with_engine(
        path: impl AsRef<Path>,
        kind: crate::search_engine::SearchEngineKind,
    ) -> Result<Self, OpenWithEngineError> {
        let provider = search_engine::build(kind);
        let storage = Storage::open(path).map_err(OpenWithEngineError::Storage)?;
        Ok(Self::assemble(storage, provider, Some(kind)))
    }

    /// 既に開いた `Storage` の所有権を受け取って構築する（呼び出し元がテーブル作成・
    /// 行投入等を `Storage` の公開 API で済ませてから `EngineCore` へ引き渡す用途。
    /// `Storage::open` 自体は既に公開 API であるため、本関数はテナント境界の迂回経路を
    /// 新設しない）。
    ///
    /// 一方向の所有権移動のみを許し、`EngineCore` から生の `Storage` を取り出す経路は
    /// 公開しない（旧 `Self::storage` アクセサ・`test-support` feature を廃止した。
    /// codex P0-2・Issue #137 対応）。構築後は [`VectorCore::get_row`]・
    /// [`VectorCore::search`] が経由する [`crate::policy::PolicyContext::is_visible`] の
    /// 単一照合パスだけが `Storage` への到達経路になる（security.md P0「テナント分離の
    /// 検査を外す/緩める/バイパス経路を作らない」）。
    ///
    /// [`Self::with_provider`] と同じ理由で [`Self::search_engine_kind`] は常に
    /// `None`（Issue #407。`kind` を伴う構築は [`Self::from_storage_with_engine`]）。
    pub fn from_storage(storage: Storage, provider: Box<dyn SearchProvider>) -> Self {
        Self::assemble(storage, provider, None)
    }

    /// [`Self::from_storage`] の `kind` 版（Issue #407・opt-in 経路）。`kind`
    /// （`SearchEngineKind::Hnsw` の場合は [`crate::hnsw::ValidatedHnswParams`] を
    /// 保持）は型で検証済みであることが保証されているため、本関数は infallible
    /// （[`Self::open_with_engine`] と異なり `Storage::open` のような他の失敗要因も
    /// 持たない）。未検証の `HnswParams` を渡したい呼び出し元は、先に
    /// [`crate::search_engine::hnsw_kind`] で `kind` を得ること。
    pub fn from_storage_with_engine(
        storage: Storage,
        kind: crate::search_engine::SearchEngineKind,
    ) -> Self {
        let provider = search_engine::build(kind);
        Self::assemble(storage, provider, Some(kind))
    }

    /// [`Self::with_provider`]／[`Self::open_with_engine`]／[`Self::from_storage`]／
    /// [`Self::from_storage_with_engine`] が共有する構築処理。フィールド初期値は
    /// 4 経路すべてで同一であり（`storage`・`provider`・`search_engine_kind` 以外は
    /// すべて既定値）、重複する struct literal を 1 箇所へ集約する
    /// （Issue #407。以前は `with_provider`／`from_storage` の 2 箇所に同一の
    /// struct literal があった）。
    fn assemble(
        storage: Storage,
        provider: Box<dyn SearchProvider>,
        search_engine_kind: Option<crate::search_engine::SearchEngineKind>,
    ) -> Self {
        // Issue #408: `SearchEngineKind::Hnsw` が明示指定された経路（`open_with_engine`・
        // `from_storage_with_engine`）でのみキャッシュ状態を構築する。既定エンジン・
        // `with_provider`／`from_storage`（`kind` 不明）は `None` のまま（従来どおり
        // 全件 brute-force。`sql::exec::execute_statement_with_cache` は `hnsw_cache`
        // が `None` の場合キャッシュへ一切触れない）。
        let hnsw_state = match &search_engine_kind {
            Some(crate::search_engine::SearchEngineKind::Hnsw(params)) => Some(HnswEngineState {
                provider: crate::hnsw::provider::HnswSearchProvider::new(*params),
                cache: crate::sql::hnsw_cache::HnswIndexCache::new(),
            }),
            _ => None,
        };
        Self {
            storage,
            provider,
            prefilter_cache: PrefilterCache::new(),
            precision_policy: crate::precision::PrecisionPolicy::default(),
            embedder: None,
            query_planner: None,
            incremental_config: crate::incremental::IncrementalConfig::default(),
            ledger_mode: LedgerMode::default(),
            batch_limits: crate::batch_limits::BatchLimits::default(),
            transaction_limits: crate::sql::transaction::TransactionLimits::default(),
            dictionary_cache: DictionaryCache::new(),
            dictionary_config: crate::dictionary::DictionaryConfig::default(),
            sparse_index_cache: crate::sql::sparse_cache::SparseIndexCache::new(),
            sql_arena_cache: crate::sql::arena_cache::SqlArenaCache::new(),
            scalar_index_cache: crate::sql::scalar_index::ScalarIndexCache::new(),
            visible_bitmap_cache: crate::sql::visible_cache::VisibleBitmapCache::new(),
            search_engine_kind,
            hnsw_state,
        }
    }

    /// 構築時に明示指定された [`crate::search_engine::SearchEngineKind`] を返す
    /// （Issue #407。[`Self::open`]／[`Self::open_with_engine`] 経由なら値を持ち、
    /// [`Self::with_provider`]／[`Self::from_storage`] 経由（provider を直接注入し
    /// `kind` との対応を検証できない）なら `None`。`VectorCore` trait には載せない
    /// 固有メソッド（`core_api.snapshot` の対象外。`prefilter_cache_stats` と同じ
    /// 方針）。#411 の `EXPLAIN` 露出が参照する契約点）。
    pub fn search_engine_kind(&self) -> Option<crate::search_engine::SearchEngineKind> {
        self.search_engine_kind
    }

    /// [`PrefilterCache`] の現在の統計を返す（TASK-169。テスト・運用観測用）。
    /// テナント ID・行 ID 等の機微情報は含まない（[`PrefilterCacheStats`] 参照）。
    /// `VectorCore` trait には載せない固有メソッド（`core_api.snapshot` の対象外）。
    pub fn prefilter_cache_stats(&self) -> PrefilterCacheStats {
        self.prefilter_cache.stats()
    }

    /// [`crate::sql::sparse_cache::SparseIndexCache`] の現在の統計を返す
    /// （Issue #357。テスト・運用観測用）。テナント ID・行 ID 等の機微情報は含まない
    /// （`SparseIndexCacheStats` 参照）。`VectorCore` trait には載せない固有メソッド
    /// （`core_api.snapshot` の対象外）。
    pub fn sparse_index_cache_stats(&self) -> crate::sql::sparse_cache::SparseIndexCacheStats {
        self.sparse_index_cache.stats()
    }

    /// `sql::arena_cache::SqlArenaCache` の現在の統計を返す（Issue #363。
    /// テスト・運用観測用）。テナント ID・行 ID 等の機微情報は含まない
    /// （`SqlArenaCacheStats` 参照）。`VectorCore` trait には載せない固有メソッド
    /// （`core_api.snapshot` の対象外。`prefilter_cache_stats` と同じ方針）。
    pub fn sql_arena_cache_stats(&self) -> crate::sql::arena_cache::SqlArenaCacheStats {
        self.sql_arena_cache.stats()
    }

    /// `sql::scalar_index::ScalarIndexCache` の現在の統計を返す（Issue #473。
    /// テスト・運用観測用）。テナント ID・行 ID・スカラー値等の機微情報は含まない
    /// （`ScalarIndexCacheStats` 参照）。`VectorCore` trait には載せない固有
    /// メソッド（`core_api.snapshot` の対象外。`sql_arena_cache_stats` と同じ
    /// 方針）。索引はまだ SQL 実行結果を左右しない（Issue #474 が消費する）ため、
    /// このメソッドは observability 専用であり検索結果には影響しない。
    pub fn scalar_index_cache_stats(&self) -> crate::sql::scalar_index::ScalarIndexCacheStats {
        self.scalar_index_cache.stats()
    }

    /// `sql::visible_cache::VisibleBitmapCache` の現在の統計を返す（Issue #478。
    /// テスト・運用観測用）。テナント ID・行 ID・可視件数等の機微情報は含まない
    /// （`VisibleBitmapCacheStats` 参照）。`VectorCore` trait には載せない固有
    /// メソッド（`core_api.snapshot` の対象外。`sql_arena_cache_stats` と同じ方針）。
    pub fn visible_bitmap_cache_stats(&self) -> crate::sql::visible_cache::VisibleBitmapCacheStats {
        self.visible_bitmap_cache.stats()
    }

    /// `sql::hnsw_cache::HnswIndexCache` の現在の統計を返す（Issue #408。
    /// テスト・運用観測用）。`search_engine_kind` が `SearchEngineKind::Hnsw` 以外
    /// （`hnsw_state` が `None`）の場合は既定値（すべて 0）を返す。テナント ID・
    /// 行 ID 等の機微情報は含まない。`VectorCore` trait には載せない固有メソッド
    /// （`core_api.snapshot` の対象外。`sql_arena_cache_stats` と同じ方針）。
    pub fn hnsw_index_cache_stats(&self) -> crate::sql::hnsw_cache::HnswIndexCacheStats {
        self.hnsw_state
            .as_ref()
            .map(|s| s.cache.stats())
            .unwrap_or_default()
    }

    /// `precision` モードの実行契約に使う [`crate::precision::PrecisionPolicy`] を
    /// 差し替えたビルダーを返す（TASK-162・SEARCH-9）。所有権を消費するビルダー
    /// メソッドとし、`&mut self` セッターは公開しない（構築後に一部だけ差し替えて
    /// 中途半端な状態を作れないようにする）。`SessionState`・SQL 構文からはこの値へ
    /// 到達できない（`crate::precision` モジュールドキュメント参照）。
    pub fn with_precision_policy(mut self, policy: crate::precision::PrecisionPolicy) -> Self {
        self.precision_policy = policy;
        self
    }

    /// ファイル形 `INSERT`（TASK-120・INDEX-1, INDEX-2）のベクトル化に使う
    /// [`crate::embedding::Embedder`] を注入したビルダーを返す（所有権を消費する
    /// ビルダーメソッドとし、[`Self::with_precision_policy`] と同じ流儀。未呼び出し
    /// なら `None` のままで、ファイル形 `INSERT` は fail-closed に拒否される）。
    pub fn with_embedder(mut self, embedder: Box<dyn crate::embedding::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// LLM クエリプランニング（TASK-110・PLAN-1）のクエリ展開に使う
    /// [`crate::query_planner::LlmClient`] を注入したビルダーを返す（所有権を消費する
    /// ビルダーメソッドとし、[`Self::with_embedder`] と同じ流儀。未呼び出しなら
    /// `None` のままで、[`Self::plan_query`] は fail-closed に拒否される）。
    pub fn with_query_planner(mut self, client: Box<dyn crate::query_planner::LlmClient>) -> Self {
        self.query_planner = Some(PlannerBinding::Single(client));
        self
    }

    /// 質問類型推定・ティアリング（TASK-115・PLAN-8）に基づき、対話ティア／高精度
    /// ティアそれぞれの [`crate::query_planner::LlmClient`] を注入したビルダーを返す
    /// （所有権を消費するビルダーメソッドとし、[`Self::with_query_planner`] と同じ流儀。
    /// [`Self::with_query_planner`] と排他: 後に呼んだ側が [`Self::query_planner`] を
    /// 上書きする。判定基準 [`crate::tiering::TieringCriteria`] は本リポの実装既定値で、
    /// 呼び出し元が差し替え可能（`docs/design/query-tiering-criteria.md` 参照）。
    pub fn with_tiered_query_planner(
        mut self,
        dialogue: Box<dyn crate::query_planner::LlmClient>,
        high_precision: Box<dyn crate::query_planner::LlmClient>,
        criteria: crate::tiering::TieringCriteria,
    ) -> Self {
        self.query_planner = Some(PlannerBinding::Tiered(crate::tiering::TieredPlanner::new(
            dialogue,
            high_precision,
            criteria,
        )));
        self
    }

    /// ファイル形 `INSERT` のチャンク化・チャンク数上限設定
    /// （[`crate::incremental::IncrementalConfig`]）を差し替えたビルダーを返す
    /// （TASK-120）。未呼び出しなら `IncrementalConfig::default()`。
    pub fn with_incremental_config(
        mut self,
        config: crate::incremental::IncrementalConfig,
    ) -> Self {
        self.incremental_config = config;
        self
    }

    /// `operation_id` 必須化ガード（TASK-92・対象ビヘイビア: RECOVER-1）を制御する
    /// [`LedgerMode`] を差し替えたビルダーを返す（[`Self::with_precision_policy`] と
    /// 同型: 所有権を消費するビルダーメソッドとし、`&mut self` セッターは公開しない）。
    /// `SessionState`・SQL 構文からはこの値へ到達できない（`crate::recovery::
    /// required_op_id` モジュールドキュメント参照）。
    pub fn with_ledger_mode(mut self, mode: LedgerMode) -> Self {
        self.ledger_mode = mode;
        self
    }

    /// 一括投入（複数ファイルのバッチ投入）に対する 4 種の処理量上限
    /// （[`crate::batch_limits::BatchLimits`]）を差し替えたビルダーを返す
    /// （TASK-122・対象ビヘイビア: INDEX-4。[`Self::with_incremental_config`] と同じ
    /// 流儀。未呼び出しなら `BatchLimits::default()`）。
    pub fn with_batch_limits(mut self, limits: crate::batch_limits::BatchLimits) -> Self {
        self.batch_limits = limits;
        self
    }

    /// 明示トランザクション（SQL-31・TASK-221）の持続時間・文数の上限
    /// （[`crate::sql::transaction::TransactionLimits`]）を差し替えたビルダーを
    /// 返す（[`Self::with_batch_limits`] と同じ流儀。未呼び出しなら
    /// `TransactionLimits::default()`）。書き込みロック待ちの上限
    /// （`55P03` を返すまでの時間）は独立に `Storage::with_write_lock_wait` で
    /// 設定する（`Storage` を構築する側の責務）。
    pub fn with_transaction_limits(
        mut self,
        limits: crate::sql::transaction::TransactionLimits,
    ) -> Self {
        self.transaction_limits = limits;
        self
    }

    /// 新規の [`crate::sql::transaction::SessionTransaction`]（`Idle`）を作る。
    /// `wire-server` が接続（セッション）ごとに 1 つ保持し、
    /// [`Self::execute_sql_in_txn`]／[`Self::execute_parsed_in_txn`] へ `&mut` で
    /// 渡す想定（SQL-31・TASK-221）。
    pub fn new_session_transaction(&self) -> crate::sql::transaction::SessionTransaction<'_> {
        crate::sql::transaction::SessionTransaction::new(self.transaction_limits)
    }

    /// 辞書的情報源抽出（TASK-109・PLAN-5）の設定
    /// （[`crate::dictionary::DictionaryConfig`]）を差し替えたビルダーを返す
    /// （[`Self::with_precision_policy`] と同じ流儀。未呼び出しなら
    /// `DictionaryConfig::default()`）。
    ///
    /// `dictionary_cache`（[`DictionaryCache`]）も同時に再初期化する（PR #249
    /// codex-review P1 指摘対応）。`DictionaryCache` のキャッシュキーは
    /// `(table, ctx)` と `storage.current_generation()` のみで、設定値
    /// （`enable_file_tree`・`enable_term_index`・`top_terms` 等）を含まない。
    /// 設定だけを差し替えて既存キャッシュを温存すると、`dictionary_snapshot` を
    /// 一度でも呼んだ後に本メソッドで設定変更しても、書き込みで世代が進むまでは
    /// 旧設定で構築した `Arc<Dictionary>` を返し続けてしまう。設定変更は稀な操作
    /// のため、キャッシュ全体を破棄して次回参照時に新設定で再構築させる単純な
    /// 方針を採る（fail-closed。古い設定の結果を新設定のものとして黙って返す
    /// 経路を残さない）。
    pub fn with_dictionary_config(mut self, config: crate::dictionary::DictionaryConfig) -> Self {
        self.dictionary_config = config;
        self.dictionary_cache = DictionaryCache::new();
        self
    }

    /// `table` の辞書的情報源スナップショットを返す（TASK-109・PLAN-5。TASK-110 の
    /// LLM クエリプランニングが固定接頭辞コンテキストとして消費する入口）。
    /// `VectorCore` trait へは昇格しない固有メソッド（`core-api-check` の対象外。
    /// `Self::with_incremental_config` 等と同じ理由）。
    ///
    /// [`DictionaryCache`] を経由し、世代整合が取れていれば再構築せず再利用する
    /// （[`DictionaryCache`] のドキュメント参照）。キャッシュミス時は
    /// `tenant::visible_rows`（`ctx` の可視性判定込み。テナント境界は完全にこの
    /// 経路が担う）でテーブル全行を取得し、スキーマから `path`/`body` 列を解決して
    /// `crate::dictionary::DictionaryBuilder` へ供給する。`path`/`body` 列を持たない
    /// テーブルは `CatalogError::Invalid` の固定英語メッセージで拒否する
    /// （既存 `execute_file_insert` 系と同じ「存在情報を漏らさない」写像方針）。
    ///
    /// スキーマ・世代の取得（`schema_read_txn`）と行走査（`tenant::visible_rows`）は
    /// 別スナップショットになりうる（`visible_rows` はページング走査中に複数回
    /// `read_txn` を開くため、単一 `read_txn` へ統合できない）。そのため行走査後に
    /// `Storage::current_generation` を再確認し、スキーマ取得時点の世代
    /// （`built_generation`）と食い違っていれば「新スキーマ前提の行を旧スキーマで
    /// デコードして破損行として黙ってスキップし、不完全な辞書をあたかも正規の
    /// ものとして返す」（PR #249 codex-review P1 指摘）事態を防ぐため、その回の
    /// 結果を破棄して最新スナップショットで再試行する（[`MAX_SNAPSHOT_RETRIES`]
    /// 回まで。以降も食い違い続ける場合は書き込みが止まらないとみなしエラーで
    /// 拒否する。fail-closed。security.md「不安全な設計」対応）。
    pub fn dictionary_snapshot(
        &self,
        ctx: &PolicyContext,
        table: &str,
    ) -> Result<Arc<crate::dictionary::Dictionary>, CoreError> {
        self.dictionary_snapshot_with_index(ctx, table)
            .map(|(dictionary, _index)| dictionary)
    }

    /// [`Self::dictionary_snapshot`] と同じ辞書スナップショットに加え、
    /// [`crate::tiering::classify`] 用に 1 度だけ構築した正規化索引
    /// （[`crate::tiering::NormalizedDictionaryIndex`]）も返す（codex-review P1
    /// 指摘対応・PR #261）。呼び出し元は `Self::expand_query`（[`PlannerBinding::Tiered`]
    /// 構成時のティア判定）のみ。`dictionary`・`normalized_index` は
    /// [`DictionaryCache`] の同一エントリから `Arc::clone` するため、キャッシュ
    /// ヒット時は辞書の再走査・索引の再構築のいずれも発生しない
    /// （[`DictCacheEntry::normalized_index`] ドキュメント参照）。
    fn dictionary_snapshot_with_index(
        &self,
        ctx: &PolicyContext,
        table: &str,
    ) -> Result<
        (
            Arc<crate::dictionary::Dictionary>,
            Arc<crate::tiering::NormalizedDictionaryIndex>,
        ),
        CoreError,
    > {
        if let Some(hit) = self.dictionary_cache.lookup(&self.storage, table, ctx) {
            return Ok(hit);
        }

        const MAX_SNAPSHOT_RETRIES: u32 = 5;
        for _ in 0..MAX_SNAPSHOT_RETRIES {
            // スキーマと「構築時の世代」を単一の `read_txn`（同一スナップショット）
            // から読む（TASK-109・PLAN-5 レビュー対応: Cursor Bugbot Medium
            // "Snapshot mixes schema and rows"）。以前はスキーマ取得
            // （`Storage::get_table_schema`）と世代取得（`Storage::current_generation`）
            // が別々の `read_txn` だったため、両呼び出しの間に並行
            // `ALTER TABLE ADD COLUMN` がコミットされると、`built_generation` は
            // 新世代を指すのに `schema` は旧世代のまま、という食い違ったペアを
            // 観測しうった。スキーマと世代を同一スナップショットで揃えることで、
            // `built_generation` が常に `schema` を取得した時点の世代と一致する
            // ことを保証する。
            let schema_read_txn = self.storage.db().begin_read().map_err(StorageError::from)?;
            let schema = crate::catalog::get_table_schema_in_txn(&schema_read_txn, table)?;
            let built_generation = crate::storage::current_generation_in_txn(&schema_read_txn)?;
            drop(schema_read_txn);

            // `path`/`body` は列名の存在・`ColumnType::Text` に加え non-nullable
            // であることまで検証する（PR #249 codex-review P1 指摘: 同名の非 Text
            // 列や nullable な列を持つテーブルを受理すると、後段で `Value::Text`
            // 以外（型不一致）または `Value::Null`（nullable 列に NULL が入った行）
            // に一致した行が黙ってスキップされ、成功応答の空/不完全な辞書を
            // 返してしまう。スキーマ不整合は fail-closed で固定メッセージの
            // `CatalogError::Invalid` として拒否し、他テナントのデータ・存在情報は
            // 含めない）。判定条件は [`dictionary_required_columns`] へ切り出し、
            // `USING PLAN`（TASK-77・SQL-5）の LLM 呼び出し前スキーマ事前検証
            // （`Self::execute_sql_in_session` の `Statement::Select` アーム、
            // `using_plan()` が `Some` の分岐を参照）と単一の判定基準を共有する
            // （codex-review P1 指摘対応、PR #266: 事前検証が無いと、この判定結果は
            // 呼び出し元で `CoreError::Catalog(CatalogError::Invalid)` を経由して
            // 一律 `Internal`〔`XX000`〕へ丸められ、body 列欠落等の通常の利用者
            // スキーマ不備が本来の `SqlSurfaceError::InvalidInput`〔`22000`〕として
            // 返らなかった）。
            let (path_idx, body_idx) = dictionary_required_columns(&schema)
                .map_err(|msg| CoreError::from(CatalogError::Invalid(msg)))?;

            let rows =
                crate::tenant::visible_rows(&self.storage, table, ctx).map_err(|e| match e {
                    crate::tenant::TenantError::Catalog(e) => CoreError::from(e),
                    // 走査量上限超過は他テナントの存在情報を含まない固定メッセージへ
                    // 丸める（`TenantError` 自体が既にその契約を満たすが、
                    // `CoreError` 側の型に昇格し直す。security.md「エラー・ログ
                    // 経由で他テナントのデータ・存在情報を漏らさない」）。
                    crate::tenant::TenantError::TooManyVisibleRows { .. }
                    | crate::tenant::TenantError::TooManyRowsScanned { .. } => {
                        CoreError::from(CatalogError::Invalid(
                            "too many rows to build dictionary snapshot".to_string(),
                        ))
                    }
                    // `verify_hits` 専用の variant で `visible_rows` からは返らない
                    // 防御的分岐。
                    crate::tenant::TenantError::HitOutsideVisibleSet => {
                        CoreError::from(CatalogError::Invalid(
                            "unexpected tenant boundary error while building dictionary snapshot"
                                .to_string(),
                        ))
                    }
                })?;

            // 行走査完了後の実世代を確認する。`schema`/`built_generation` 取得後
            // かつ `visible_rows` の走査完了前に書き込みがコミットされていると、
            // 走査結果に旧スキーマでは正しくデコードできない新世代の行が混じり
            // うる。ここで不一致を検出したら、その回の `rows`/`schema` を丸ごと
            // 破棄し、最新スナップショットを取り直して再試行する（不完全な辞書を
            // 正規の結果として返さない）。
            let observed_generation = self.storage.current_generation()?;
            if observed_generation != built_generation {
                continue;
            }

            let config = self.dictionary_config.clone();
            let mut builder = crate::dictionary::DictionaryBuilder::new(config);
            for row in &rows {
                let Ok(values) = crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
                else {
                    // デコード失敗行を黙ってスキップすると、内容を欠いた
                    // `Dictionary` が `truncated: false` のまま正常なキャッシュ
                    // エントリとして保存され、後続の LLM プランニングから完全な
                    // スナップショットと区別できなくなる（PR #249 codex-review P1
                    // 指摘）。データ破損は recall 側の安全劣化（切り詰め）とは
                    // 性質が異なるため、fail-closed に構築全体を中止しキャッシュへ
                    // 保存しない。他テナントのデータ・存在情報は含めない固定
                    // メッセージで拒否する。
                    return Err(CoreError::from(CatalogError::Invalid(
                        "failed to decode a visible row while building dictionary snapshot"
                            .to_string(),
                    )));
                };
                // 上記のスキーマ検証で path_idx/body_idx は non-nullable な
                // `ColumnType::Text` 列を指すことを保証しているため、ここで
                // `Value::Text` 以外（`Value::Null`・型不一致）に一致することは
                // 想定しない。想定外のズレを黙って読み飛ばすと不完全な辞書が
                // 正常なキャッシュエントリとして保存されうる（PR #249
                // codex-review P1 指摘と同種の懸念）ため、防御的に fail-closed で
                // 拒否する（他テナントのデータ・存在情報を含めない固定メッセージ）。
                let path = match values.get(path_idx) {
                    Some(crate::row_codec::Value::Text(s)) => s.as_str(),
                    _ => {
                        return Err(CoreError::from(CatalogError::Invalid(
                            "path column value was unexpectedly not text while building \
                             dictionary snapshot"
                                .to_string(),
                        )));
                    }
                };
                let body = match values.get(body_idx) {
                    Some(crate::row_codec::Value::Text(s)) => s.as_str(),
                    _ => {
                        return Err(CoreError::from(CatalogError::Invalid(
                            "body column value was unexpectedly not text while building \
                             dictionary snapshot"
                                .to_string(),
                        )));
                    }
                };
                builder.ingest(path, body);
            }
            let dictionary = builder.finish();
            let approx_bytes = dictionary.approx_heap_bytes();
            // `DictionaryCache::insert` は自身のロック区間内で世代を再確認し、
            // 挿入対象（このスナップショット）が既に古くなっていれば `None` を
            // 返す（PR #249 codex-review P1 指摘対応: 上の `observed_generation`
            // チェックとこの呼び出しの間に別の書き込みがコミットされる競合を
            // 検出する最後の砦）。`None` は書き込み未反映のスナップショットを
            // 呼び出し元へ渡さないためのシグナルであり、ループ先頭へ戻って
            // 最新スナップショットで再試行する（不完全な辞書を正規の結果として
            // 返さない）。
            if let Some(hit) = self.dictionary_cache.insert(
                &self.storage,
                table,
                ctx,
                dictionary,
                built_generation,
                approx_bytes,
            ) {
                return Ok(hit);
            }
        }

        // 継続的な並行書き込みで整合したスナップショットを得られなかった。
        // 不完全な辞書を黙って返さず fail-closed に拒否する。
        Err(CoreError::from(CatalogError::Invalid(
            "dictionary snapshot generation kept changing during row scan; retry later".to_string(),
        )))
    }

    /// `table` に対する自然言語 `question` を LLM クエリプランニング（TASK-110・
    /// PLAN-1）で展開し、[`crate::query_planner::QueryExpansion`] を返す。
    /// `VectorCore` trait へは昇格しない固有メソッド（`core-api-check` の対象外。
    /// `Self::dictionary_snapshot` 等と同じ理由）。
    ///
    /// [`Self::query_planner`] が未注入（`None`）の場合は
    /// [`CoreError::QueryPlannerUnavailable`] で fail-closed に拒否する（`embedder`
    /// 未構成時のファイル形 `INSERT` 拒否と同じ方針。既定で参照実装を暗黙採用しない）。
    ///
    /// 固定接頭辞は必ず `(table, ctx)` 単位の [`Self::dictionary_snapshot`] から
    /// 都度レンダリングし、テナントをまたぐ接頭辞キャッシュは持たない
    /// （security.md「テナント境界」対応。レンダリング自体は
    /// [`DictionaryCache`] 経由で世代整合キャッシュされるため、LLM レイテンシ比で
    /// 無視できるコストに収まる）。LLM 応答は
    /// [`crate::query_planner::parse_expansion`] が厳格パースするため、
    /// プロンプトインジェクションによる異常出力の影響は検証済みの
    /// `QueryExpansion` に閉じる（`query_planner.rs` モジュールドキュメント参照）。
    pub fn plan_query(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
    ) -> Result<crate::query_planner::QueryExpansion, CoreError> {
        let (expansion, _classification) = self.expand_query(ctx, table, question)?;
        Ok(expansion)
    }

    /// [`Self::plan_query`] と同じ契約に加え、[`Self::query_planner`] が
    /// [`PlannerBinding::Tiered`] 構成の場合に採用した質問類型・ティア
    /// （[`crate::tiering::Classification`]）も `Some` で返す（TASK-115・PLAN-8。
    /// TASK-116 のティア別レイテンシ検証、将来の EXPLAIN 露出〔SQL-6・PLAN-11／
    /// TASK-164〕の足場）。[`PlannerBinding::Single`] 構成の場合は分類自体を行わず
    /// `None` を返す（単一クライアントへそのまま委譲する `Self::plan_query` の既存
    /// 挙動を変えないため）。
    ///
    /// `VectorCore` trait へは昇格しない固有メソッド（`core-api-check` の対象外。
    /// `Self::plan_query`・`Self::dictionary_snapshot` と同じ理由）。
    pub fn plan_query_with_classification(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
    ) -> Result<
        (
            crate::query_planner::QueryExpansion,
            Option<crate::tiering::Classification>,
        ),
        CoreError,
    > {
        self.expand_query(ctx, table, question)
    }

    /// `table` に対する自然言語 `question` を LLM クエリプランニング（TASK-110・
    /// PLAN-1）で展開し、明示指定（`query_mode`・`session_mode`）とプランナー推定
    /// （展開結果の `mode_hint`）から解決した実効モードまで含めて返す
    /// （TASK-164・PLAN-11。解決契約は `sql::mode` モジュールドキュメント、
    /// spec のビヘイビア定義〔PLAN-11〕を参照）。`VectorCore` trait へは昇格しない
    /// 固有メソッド（[`Self::plan_query`] と同じ理由。`core-api-check` の対象外）。
    ///
    /// モード解決は `sql::mode::resolve_mode_with_planner` へ委譲する。呼び出し元
    /// （wire-server の接続ハンドラ・将来の `USING PLAN` 結線。TASK-77/78 の管轄）は
    /// `query_mode`（クエリ句由来）・`session_mode`（`SessionState::search_mode()`
    /// 由来）を渡す。展開自体の fail-closed 契約（LLM 未接続・プロンプト超過・
    /// LLM 応答不正はすべて `Err`）は [`Self::plan_query`] と共有する（モードの
    /// fail-safe とは独立したエラー系統。`query_planner.rs` モジュールドキュメント
    /// 参照）。質問類型・ティア（TASK-115・PLAN-8）の判定結果は本メソッドの戻り値
    /// には含まれない（必要な呼び出し元は [`Self::plan_query_with_classification`]
    /// を使う）。
    pub fn plan_query_with_mode(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
        query_mode: Option<crate::sql::mode::SearchMode>,
        session_mode: Option<crate::sql::mode::SearchMode>,
    ) -> Result<crate::query_planner::PlannedQuery, CoreError> {
        let (expansion, _classification) = self.expand_query(ctx, table, question)?;
        let resolved = crate::sql::mode::resolve_mode_with_planner(
            query_mode,
            session_mode,
            expansion.mode_hint,
        );
        Ok(crate::query_planner::PlannedQuery::new(expansion, resolved))
    }

    /// [`Self::plan_query`]・[`Self::plan_query_with_classification`]・
    /// [`Self::plan_query_with_mode`] が共有する展開フロー本体（辞書スナップショット
    /// 取得 → ティア判定〔[`PlannerBinding::Tiered`] 構成時のみ〕 → 固定接頭辞
    /// レンダリング → LLM 呼び出し → 厳格パース）。
    fn expand_query(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
    ) -> Result<
        (
            crate::query_planner::QueryExpansion,
            Option<crate::tiering::Classification>,
        ),
        CoreError,
    > {
        let binding = self
            .query_planner
            .as_ref()
            .ok_or(CoreError::QueryPlannerUnavailable)?;
        let (dictionary, normalized_index) = self.dictionary_snapshot_with_index(ctx, table)?;

        let (client, classification): (&dyn crate::query_planner::LlmClient, _) = match binding {
            PlannerBinding::Single(client) => (client.as_ref(), None),
            PlannerBinding::Tiered(tiered) => {
                // `normalized_index` は辞書スナップショット（`dictionary`）と同じ
                // `DictionaryCache` エントリから 1 度だけ構築済み（codex-review P1
                // 指摘対応・PR #261）。`tiered.select` はここでは借用のみで済み、
                // リクエストごとの辞書全量の複製・走査は発生しない。
                let (client, classification) = tiered.select(question, &normalized_index);
                (client, Some(classification))
            }
        };

        let prefix = crate::query_planner::render_prompt_prefix(&dictionary);
        let prompt = crate::query_planner::render_full_prompt(&prefix, question)?;
        let response = client.complete(&prompt)?;
        let expansion = crate::query_planner::parse_expansion(&response)?;
        Ok((expansion, classification))
    }

    /// `table` に対する自然言語 `question` を [`Self::plan_query`]（TASK-110・PLAN-1）で
    /// 展開し、展開結果を再埋め込み規則（TASK-114・PLAN-10）に従って再埋め込みした
    /// [`crate::query_planner::EmbeddedQuery`] を返す。`VectorCore` trait へは昇格しない
    /// 固有メソッド（[`Self::plan_query`] と同じ理由。`core-api-check` の対象外）。
    ///
    /// 処理順序（fail-closed。`incremental.rs::chunk_phase` が embedder 次元不一致を
    /// チャンク化・埋め込みサービス呼び出しより前に検出する前例と同じ流儀で、
    /// LLM 呼び出しという比較的高コストな I/O の前に構成不備を検出する）:
    /// 1. [`Self::embedder`] 未注入は [`CoreError::EmbedderUnavailable`] で拒否
    /// 2. 対象テーブルの宣言次元（`VECTOR(N)`）と `embedder.dim()` の不一致は
    ///    [`CoreError::QueryEmbedding`]（[`crate::embedding::EmbedError::DimMismatch`]）で拒否
    /// 3. [`Self::plan_query`] で LLM 展開（`query_planner` 未注入は既存の
    ///    [`CoreError::QueryPlannerUnavailable`]。LLM 展開が失敗した場合はここで
    ///    エラーが伝播し、以降の埋め込み呼び出しは実行しない）
    /// 4. [`crate::query_planner::reembed_expansion`] で再埋め込み
    ///
    /// テーブルスキーマ参照は `execute_insert_sql`
    /// 等の既存経路（`self.storage.get_table_schema`）と同じ流儀を用い、新規の
    /// RLS バイパス経路は作らない（対象はテーブル構造メタデータでありテナント行
    /// データではないため、`ctx` によるフィルタ対象外。`(table, ctx)` に対する
    /// テナント境界の担保は [`Self::plan_query`] 内の [`Self::dictionary_snapshot`]
    /// が引き続き担う）。
    pub fn plan_and_embed_query(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
    ) -> Result<crate::query_planner::EmbeddedQuery, CoreError> {
        let embedder = self
            .embedder
            .as_deref()
            .ok_or(CoreError::EmbedderUnavailable)?;

        let schema = self.storage.get_table_schema(table)?;
        let table_dim = schema.vector_dim().ok_or_else(|| {
            CoreError::Catalog(CatalogError::Invalid(
                "table has no VECTOR column".to_string(),
            ))
        })?;
        let embedder_dim = embedder.dim();
        if embedder_dim != table_dim {
            return Err(CoreError::QueryEmbedding(
                crate::embedding::EmbedError::DimMismatch {
                    expected: table_dim,
                    got: embedder_dim as usize,
                },
            ));
        }

        let expansion = self.plan_query(ctx, table, question)?;
        let embedding = crate::query_planner::reembed_expansion(embedder, question, &expansion)?;
        Ok(crate::query_planner::EmbeddedQuery {
            expansion,
            embedding,
        })
    }

    /// SQL 表層の単一文実行エントリポイント（TASK-75、対象ビヘイビア: SQL-1〜4）。
    /// `VectorCore` trait への昇格は行わない固有メソッドとする（`crates/engine/api/
    /// core_api.snapshot`・`make core-api-check` が対象とするのは `VectorCore`
    /// trait 本体のみのため、本メソッドの追加はコア API シグネチャ安定性チェックに
    /// 影響しない。trait への統合可否は wire 統合タスク（TASK-68〜73）が判断する）。
    ///
    /// `sql::allowlist::validate_statement`（構造検証）→
    /// `sql::parser::bind`（意味論検証・束縛）→ `sql::exec::execute_statement`
    /// （RLS→SCALAR→DISTANCE 固定順の実行）の順に呼ぶ。RLS 適用は `ctx` の下で
    /// 無条件に行われ、SQL 文中の `visible()` 呼び出しの有無に依存しない（SQL-3・
    /// RLS-7。`sql::exec` のモジュールドキュメント参照）。
    ///
    /// スキーマ取得（`bind` 用）・候補走査（`sql::exec::execute_statement` 内の
    /// `VectorArena::build_filtered_with_rows_in_txn`）を単一の `read_txn`
    /// （同一スナップショット）上で行う（Issue #56 レビュー指摘対応・codex P1:
    /// 以前は `Storage::get_table_schema` が別トランザクションでスキーマを取得し、
    /// `execute_statement` がさらに別トランザクションで行走査していたため、この間に
    /// `alter_table_add_column` がコミットされると、`bind` が束縛した旧スキーマで
    /// 新スナップショットの行を検索することになり、新設列の欠落や `row_codec`
    /// デコード失敗（`XX000` 相当）を招き得た。`catalog::get_table_schema_in_txn` で
    /// スキーマを取得したのと同一の `read_txn` を `execute_statement` へ渡すことで、
    /// スキーマ取得・bind・候補走査を単一スナップショットへ閉じ込める）。
    pub fn execute_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        // セッション変数を持たない後方互換 API（TASK-75 由来）。`SET search_mode` 等
        // セッションを要する statement はこのエントリポイントでは受理しない
        // （黙った no-op にしない）。
        //
        // statement 種別の判定を [`crate::sql::allowlist::validate_sql`] で先に行い、
        // `SetSearchMode` はリテラル値の妥当性を検証する前に一律 `42601` で拒否する
        // （codex-review P1 指摘対応: 以前は `execute_sql_in_session` へ委譲していたため、
        // 内部で `SearchMode::parse_literal` が先に走り、無効なリテラル値（例:
        // `SET search_mode = 'fuzzy'`）は `22000` を返す一方、有効な値は `42601` を
        // 返すという、同じ「このエントリポイントでは非対応」の statement が値によって
        // 異なるエラーコードを返す非決定的な契約になっていた。fail-closed の観点でも
        // エラー契約は入力の意味論的妥当性に関わらず一貫させるべきであり、statement
        // 種別のみで判定する）。
        match crate::sql::allowlist::validate_sql(sql, &self.storage)? {
            crate::sql::allowlist::Statement::SetSearchMode { .. } => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "SET search_mode requires a session-aware entry point",
                ))
            }
            // TASK-79（SQL-9）: UDF 定義はセッションへ登録する statement のため、
            // セッションを持たないこのエントリポイントでは受理しない（`SET` と同じ
            // 「値の妥当性に関わらず一律 `42601`」の決定的な契約を踏襲する）。
            crate::sql::allowlist::Statement::CreateFunction { .. } => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "CREATE FUNCTION requires a session-aware entry point",
                ))
            }
            stmt @ crate::sql::allowlist::Statement::Select(_) => {
                // `SELECT` と判定済みのため、[`Self::execute_validated_in_session`] が
                // `SqlOutcome::SetSearchMode`／`SqlOutcome::CreateFunction` を返すことは
                // ない。ここで得た `stmt`（`validate_sql` 済み）をそのまま
                // `execute_validated_in_session` へ渡し、SQL 文字列の再パースを
                // 避ける（Issue #314・SQL-1・TASK-83 条件7: SQL 表層 C1 p95 の
                // 固定コスト削減。以前は `execute_sql_in_session(ctx, &mut session,
                // sql)` を呼び直しており、同一 `sql` を 2 回構文解析していた）。
                let mut session = crate::sql::mode::SessionState::default();
                match self.execute_validated_in_session(ctx, &mut session, stmt)? {
                    crate::sql::SqlOutcome::Query(result) => Ok(result),
                    crate::sql::SqlOutcome::SetSearchMode(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_)
                    | crate::sql::SqlOutcome::Truncate(_)
                    | crate::sql::SqlOutcome::Delete(_)
                    | crate::sql::SqlOutcome::Returning(_)
                    | crate::sql::SqlOutcome::Update(_)
                    | crate::sql::SqlOutcome::CreateTable(_)
                    | crate::sql::SqlOutcome::DropTable(_)
                    | crate::sql::SqlOutcome::AlterTable(_)
                    | crate::sql::SqlOutcome::CreateView(_)
                    | crate::sql::SqlOutcome::DropView(_)
                    | crate::sql::SqlOutcome::CreateIndex(_)
                    | crate::sql::SqlOutcome::DropIndex(_)
                    | crate::sql::SqlOutcome::Begin
                    | crate::sql::SqlOutcome::Commit
                    | crate::sql::SqlOutcome::Rollback
                    | crate::sql::SqlOutcome::DeclareCursor
                    | crate::sql::SqlOutcome::Fetch(_)
                    | crate::sql::SqlOutcome::CloseCursor => {
                        Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "unexpected non-Query outcome for a statement already classified as Select"
                                .to_string(),
                        })
                    }
                }
            }
            // TASK-166（SQL-13）: 集計 SELECT は UDF 呼び出しをその引数に含みうる
            // ため（セッション UDF レジストリ参照）、`Select` と同じくセッションを
            // 要する実行本体（`execute_validated_in_session`）へ委譲する。UDF を
            // 持たない空セッションで束縛するため、セッションに定義済みの UDF を
            // 参照する集計は（`Select` と同様）このセッションなしエントリポイント
            // では使えない（`22000`。「未知の関数」として拒否される）。
            stmt @ crate::sql::allowlist::Statement::Aggregate(_) => {
                // `stmt` は上と同じく `validate_sql` 済みのため再パースしない
                // （Issue #314・SQL-1・TASK-83 条件7）。
                let mut session = crate::sql::mode::SessionState::default();
                match self.execute_validated_in_session(ctx, &mut session, stmt)? {
                    crate::sql::SqlOutcome::Query(result) => Ok(result),
                    crate::sql::SqlOutcome::SetSearchMode(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_)
                    | crate::sql::SqlOutcome::Truncate(_)
                    | crate::sql::SqlOutcome::Delete(_)
                    | crate::sql::SqlOutcome::Returning(_)
                    | crate::sql::SqlOutcome::Update(_)
                    | crate::sql::SqlOutcome::CreateTable(_)
                    | crate::sql::SqlOutcome::DropTable(_)
                    | crate::sql::SqlOutcome::AlterTable(_)
                    | crate::sql::SqlOutcome::CreateView(_)
                    | crate::sql::SqlOutcome::DropView(_)
                    | crate::sql::SqlOutcome::CreateIndex(_)
                    | crate::sql::SqlOutcome::DropIndex(_)
                    | crate::sql::SqlOutcome::Begin
                    | crate::sql::SqlOutcome::Commit
                    | crate::sql::SqlOutcome::Rollback
                    | crate::sql::SqlOutcome::DeclareCursor
                    | crate::sql::SqlOutcome::Fetch(_)
                    | crate::sql::SqlOutcome::CloseCursor => {
                        Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "unexpected non-Query outcome for a statement already classified as Aggregate"
                                .to_string(),
                        })
                    }
                }
            }
            // Issue #454: 広域取得（`Scan`）はセッション UDF レジストリを参照する
            // 式項目・式述語を持ちうるため、`Select`・`Aggregate` と同じくセッション
            // を要する実行本体（`execute_validated_in_session`）へ委譲する。
            stmt @ crate::sql::allowlist::Statement::Scan(_) => {
                // `stmt` は上と同じく `validate_sql` 済みのため再パースしない
                // （Issue #314・SQL-1・TASK-83 条件7 の踏襲）。
                let mut session = crate::sql::mode::SessionState::default();
                match self.execute_validated_in_session(ctx, &mut session, stmt)? {
                    crate::sql::SqlOutcome::Query(result) => Ok(result),
                    crate::sql::SqlOutcome::SetSearchMode(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_)
                    | crate::sql::SqlOutcome::Truncate(_)
                    | crate::sql::SqlOutcome::Delete(_)
                    | crate::sql::SqlOutcome::Returning(_)
                    | crate::sql::SqlOutcome::Update(_)
                    | crate::sql::SqlOutcome::CreateTable(_)
                    | crate::sql::SqlOutcome::DropTable(_)
                    | crate::sql::SqlOutcome::AlterTable(_)
                    | crate::sql::SqlOutcome::CreateView(_)
                    | crate::sql::SqlOutcome::DropView(_)
                    | crate::sql::SqlOutcome::CreateIndex(_)
                    | crate::sql::SqlOutcome::DropIndex(_)
                    | crate::sql::SqlOutcome::Begin
                    | crate::sql::SqlOutcome::Commit
                    | crate::sql::SqlOutcome::Rollback
                    | crate::sql::SqlOutcome::DeclareCursor
                    | crate::sql::SqlOutcome::Fetch(_)
                    | crate::sql::SqlOutcome::CloseCursor => {
                        Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "unexpected non-Query outcome for a statement already classified as Scan"
                                .to_string(),
                        })
                    }
                }
            }
            // TASK-78（SQL-6）: `EXPLAIN` は検索本体を実行しない別の応答形
            // （`SqlOutcome::Explain`）を返すため、`QueryResult` のみを返す本
            // エントリポイントでは受理しない（`SET`・`CREATE FUNCTION` と同じ
            // 「値の妥当性に関わらず一律 `42601`」の決定的な契約を踏襲する）。
            crate::sql::allowlist::Statement::Explain(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "EXPLAIN requires a session-aware entry point",
                ))
            }
        }
    }

    /// 接続（セッション）単位の [`crate::sql::mode::SessionState`] を受け取って SQL 文を
    /// 実行する（TASK-161・SQL-12 の公開 API）。`wire-server` の接続ハンドラ
    /// （TASK-73・TASK-165 の管轄）が 1 接続につき 1 個の `SessionState` を所有し、
    /// クエリのたびに `&mut` で本メソッドへ渡す想定（`EngineCore` 自体は
    /// セッション状態を保持しない。接続間でモードが混線しない構造を型で担保する
    /// ため。`sql::mode` モジュールドキュメント参照）。
    ///
    /// `SET search_mode = '<literal>'` はリテラル値が `recall`／`precision` のいずれか
    /// である場合にのみ `session` を更新する（検証→代入の順）。失敗した `SET` は
    /// `session` を一切変更しない（部分更新＝黙った既定化と同種の fail-open を防ぐ）。
    /// `table_name` の `read_txn` を新規に開き、同一スナップショット上でスキーマを
    /// 取得して両方を返す（[`Self::execute_sql_in_session`] の各 `Statement` アームが
    /// 共有する定型処理）。呼び出し元は返した `read_txn` を、必要な処理
    /// （束縛・`execute_statement`）が終わるまでにドロップしてよい――本メソッド自体は
    /// 開いた `read_txn` を長時間保持しない（`USING PLAN` 経路が LLM 呼び出し・
    /// 再埋め込みの間 `read_txn` を保持しないようにする分割の一部。呼び出し元の
    /// モジュールドキュメント参照）。テーブル不存在は
    /// [`crate::sql::allowlist::SqlSurfaceError::UndefinedTable`] へ丸め込む。
    fn read_txn_with_schema(
        &self,
        table_name: &str,
    ) -> Result<
        (redb::ReadTransaction, crate::catalog::TableSchema),
        crate::sql::allowlist::SqlSurfaceError,
    > {
        let read_txn = self.storage.db().begin_read().map_err(|e| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!(
                    "failed to begin read transaction: {}",
                    StorageError::from(e)
                ),
            }
        })?;
        let schema = crate::catalog::get_table_schema_in_txn(&read_txn, table_name).map_err(
            |e| match e {
                CatalogError::TableNotFound(name) => {
                    crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                }
                // `CatalogError::Invalid`（識別子形式不正）は `catalog::table_lookup_error`
                // （`impl TableLookup for Storage` と共有する単一の写像本体）へ委譲し、
                // SQL 表層（`validate_sql` の `TableLookup::table_exists` 経由）と同じ
                // `42601`（unsupported syntax）へ分類する。ここで独自に `Internal`
                // （`XX000`）へ丸め込むと、[`Self::execute_bound_scan_in_session`]・
                // [`Self::execute_bound_aggregate_in_session`] のドキュメンテーション
                // コメントが謳う「SQL 経路と同一のエラー分類・露出範囲」契約が、
                // 不正なテーブル名に対してだけ破られてしまう（Issue #728 PR #788
                // レビュー指摘）。`Invalid` 以外（`Backend`／`CorruptSchema` 等の内部
                // 破損系）は `table_lookup_error` 内で引き続き `Internal` に丸め込まれる。
                other => crate::catalog::table_lookup_error(other),
            },
        )?;
        Ok((read_txn, schema))
    }

    /// SQL テキストを許可リスト検証だけ行い、実行（行・台帳・世代への到達）は
    /// 一切行わない入口（Issue #933・TASK-71・WIRE-11。拡張クエリプロトコルの
    /// Parse／Describe が Execute から分離して呼べるようにするための分割）。
    /// [`Self::execute_sql_in_session`] はこの結果をそのまま
    /// [`Self::execute_parsed_in_session`] へ渡すだけの合成になっており、
    /// 検証の判定順序・エラー分類は本メソッド分割の前後で一切変わらない。
    ///
    /// `INSERT`／`TRUNCATE`／`DELETE`／`UPDATE` は `sql::allowlist::validate_sql`
    /// の許可形状に含まれないため、先頭トークンだけを覗いて専用の
    /// `validate_*_tokens` 系関数へ振り分ける（`execute_sql_in_session` が従来
    /// 行っていた分岐をそのまま移設。各分岐のコメント・Issue ポインタは移設元と
    /// 同一）。
    pub fn parse_sql(
        &self,
        sql: &str,
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        let tokens = crate::sql::lexer::tokenize(sql)?;
        self.parse_tokens(tokens)
    }

    /// TABLE-18・SQL-23・TASK-205（Issue #909）: `INSERT`／`TRUNCATE`／
    /// `DELETE`／`UPDATE` の構造検証（`validate_*_tokens`）はカタログを
    /// 「テーブルとして」照会するため、対象名がビューであっても
    /// `SqlSurfaceError::UndefinedTable`（`42P01`）を返す。書き込み系文が
    /// ビューを対象にした場合は「ビューへの書き込みは非対応」（`42809`）と
    /// 区別する必要があるため、`UndefinedTable` に限り対象名がビューとして
    /// 存在するかを 1 回だけ読み直し、存在すれば `WrongObjectType` へ読み替える
    /// （`sql::allowlist::validate_sql_tokens` の `Statement::Scan` 分岐が使う
    /// `sql::view::resolve_from` とは別の入口だが、いずれも
    /// `catalog::Storage::view_definition` を単一の情報源とする）。他の
    /// エラー（構文エラー等）はそのまま透過する。
    ///
    /// ビュー定義の読み直し自体が失敗した場合（redb I/O エラー・破損した
    /// ビュー定義〔`CatalogError::CorruptSchema`〕等）は「ビューではない」と
    /// 同一視して `UndefinedTable` を返してはならない（破損したビューを
    /// 「存在しない」と誤報告する fail-open になる。codex-review P1・PR #1048）。
    /// `Ok(None)`（ビューとして存在しない）と `Err` を分け、`Err` は
    /// `impl TableLookup for Storage::view_definition` と同じ
    /// [`crate::catalog::table_lookup_error`] で写像する（`Backend`／
    /// `CorruptSchema` は `Internal`〔`XX000`〕。第 2 の写像を作らない）。
    fn reclassify_write_to_view_error(
        &self,
        e: crate::sql::allowlist::SqlSurfaceError,
    ) -> crate::sql::allowlist::SqlSurfaceError {
        if let crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name } = &e {
            // 索引名（TASK-206・INDEX-7、Issue #908）もビューと同じくテーブルと
            // relation 名前空間を共有する行を持たないオブジェクトのため、書き込み
            // 系 DML の対象にされた場合は `42809` へ揃える。
            let wrong_kind = match self.storage.view_definition(name) {
                Ok(Some(_)) => true,
                Ok(None) => match self.storage.index_exists(name) {
                    Ok(found) => found,
                    Err(lookup_err) => return crate::catalog::table_lookup_error(lookup_err),
                },
                Err(lookup_err) => return crate::catalog::table_lookup_error(lookup_err),
            };
            if wrong_kind {
                return crate::sql::allowlist::SqlSurfaceError::WrongObjectType {
                    name: name.clone(),
                };
            }
        }
        e
    }

    /// [`Self::parse_sql`] の字句解析済みトークン列版（Issue #935・WIRE-12。
    /// `sql::allowlist::validate_sql`/`validate_sql_tokens` の分割と同じ理由）。
    /// [`Self::parse_sql_prepared`]・[`Self::bind_prepared`] が、拡張クエリ
    /// プロトコルの `$n` を実値の `Token::StringLiteral` へ置換したトークン列を
    /// SQL テキストへ戻さずここへ直接渡すための入口（第 2 の実行器を作らない
    /// 設計）。判定順序・エラー分類は [`Self::parse_sql`] と完全に同一
    /// （[`Self::parse_sql`] は字句解析した上でここへ委譲するだけの薄い
    /// ラッパーになった）。
    fn parse_tokens(
        &self,
        tokens: Vec<crate::sql::lexer::Token>,
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        let txn_control_head = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name))
                if name.eq_ignore_ascii_case("BEGIN")
                    || name.eq_ignore_ascii_case("COMMIT")
                    || name.eq_ignore_ascii_case("ROLLBACK")
        );
        if txn_control_head {
            let ctrl = crate::sql::transaction::validate_transaction_control_tokens(&tokens)?;
            return Ok(ParsedSql::Transaction(ctrl));
        }

        let is_insert_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("INSERT")
        );
        if is_insert_statement {
            let lookup = InsertSchemaLookup::new(&self.storage);
            let stmt =
                crate::sql::allowlist::validate_insert_tokens(&tokens, &lookup, self.ledger_mode)
                    .map_err(|e| self.reclassify_write_to_view_error(e))?;
            return Ok(ParsedSql::Insert(stmt));
        }

        let is_truncate_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("TRUNCATE")
        );
        if is_truncate_statement {
            let stmt = crate::sql::allowlist::validate_truncate_tokens(
                &tokens,
                &self.storage,
                self.ledger_mode,
            )
            .map_err(|e| self.reclassify_write_to_view_error(e))?;
            return Ok(ParsedSql::Truncate(stmt));
        }

        let is_delete_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("DELETE")
        );
        if is_delete_statement {
            let stmt = crate::sql::allowlist::validate_delete_statement_tokens(
                &tokens,
                &self.storage,
                self.ledger_mode,
            )
            .map_err(|e| self.reclassify_write_to_view_error(e))?;
            return Ok(ParsedSql::Delete(stmt));
        }

        let is_update_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("UPDATE")
        );
        if is_update_statement {
            let stmt = crate::sql::allowlist::validate_update_form_tokens(
                &tokens,
                &self.storage,
                self.ledger_mode,
            )
            .map_err(|e| self.reclassify_write_to_view_error(e))?;
            return Ok(ParsedSql::Update(stmt));
        }

        let is_declare_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("DECLARE")
        );
        if is_declare_statement {
            // カタログ照会（テーブル存在確認）は意図的にここでは行わない
            // （`sql::cursor::validate_declare_tokens_structure_only` 参照。
            // PR #1049 レビュー指摘 P1 対応）。
            let stmt = crate::sql::cursor::validate_declare_tokens_structure_only(&tokens)?;
            return Ok(ParsedSql::Cursor(stmt));
        }

        let is_fetch_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("FETCH")
        );
        if is_fetch_statement {
            let stmt = crate::sql::cursor::validate_fetch_tokens(&tokens)?;
            return Ok(ParsedSql::Cursor(stmt));
        }

        let is_close_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("CLOSE")
        );
        if is_close_statement {
            let stmt = crate::sql::cursor::validate_close_tokens(&tokens)?;
            return Ok(ParsedSql::Cursor(stmt));
        }

        // TASK-206・INDEX-7・SQL-23（Issue #908）: `DROP INDEX` は汎用の
        // `is_drop_statement`（`DROP TABLE` 扱い）より前に分岐する。
        // `validate_drop_index_tokens` はカタログ照会を一切行わない
        // （`DropTable`・`DropView` と同じ設計）。
        if crate::sql::allowlist::is_drop_index_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_drop_index_tokens(&tokens)?;
            return Ok(ParsedSql::DropIndex(stmt));
        }

        let is_drop_view_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("DROP")
        ) && matches!(
            tokens.get(1),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("VIEW")
        );
        if is_drop_view_statement {
            // TABLE-18・SQL-23・TASK-205（Issue #909）: `validate_drop_view_tokens`
            // もカタログ照会を一切行わない（`DropTable` と同じ設計）。
            let stmt = crate::sql::allowlist::validate_drop_view_tokens(&tokens)?;
            return Ok(ParsedSql::DropView(stmt));
        }

        let is_drop_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("DROP")
        );
        if is_drop_statement {
            // `validate_drop_table_tokens` はカタログ照会を一切行わない
            // （DDL 実行権限ゲートを存在確認より前に通す契約。`ValidatedDropTable`
            // ドキュメント参照）。権限判定・実行本体は
            // `execute_parsed_in_session` の `DropTable` 分岐が担う。
            let stmt = crate::sql::allowlist::validate_drop_table_tokens(&tokens)?;
            return Ok(ParsedSql::DropTable(stmt));
        }

        let is_create_view_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("CREATE")
        ) && matches!(
            tokens.get(1),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("VIEW")
        );
        if is_create_view_statement {
            // TABLE-18・SQL-23・TASK-205（Issue #909）: `validate_create_view_tokens`
            // もカタログ照会を一切行わない（`DropTable`・`DropView` と同じ設計。
            // `is_create_function_statement`〔`sql::allowlist::validate_sql_tokens`
            // 内〕より前にここで分岐するため、`CREATE VIEW` のトークン列が
            // `parse_create_function` へ渡ることはない）。
            let stmt = crate::sql::allowlist::validate_create_view_tokens(&tokens)?;
            return Ok(ParsedSql::CreateView(stmt));
        }

        // TASK-206・INDEX-7・SQL-23（Issue #908）: `CREATE INDEX` も
        // `CreateView` と同じくカタログ照会を一切行わない構造検証のみで束縛し、
        // `sql::allowlist::validate_sql_tokens` 内の `CREATE FUNCTION` 分岐へ
        // 渡さない。
        if crate::sql::allowlist::is_create_index_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_create_index_tokens(&tokens)?;
            return Ok(ParsedSql::CreateIndex(stmt));
        }

        // `DropTable` と同じ設計（Issue #899・#902 で DDL 実行権限ゲートの判定
        // 順序を統一）: `validate_create_table_tokens` はカタログ照会（テーブル名
        // の重複判定）を一切行わない。権限判定・実行本体は
        // `execute_parsed_in_session` の `CreateTable` 分岐が担う。
        if crate::sql::allowlist::is_create_table_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_create_table_tokens(&tokens)?;
            return Ok(ParsedSql::CreateTable(stmt));
        }

        // TASK-202（SQL-23。Issue #900）: `ALTER TABLE ... ADD COLUMN ...` は
        // DDL であり `USING OPERATION_ID` を取らないため `LedgerMode`・
        // `TableLookup` のいずれも渡さない（`ValidatedAlterTableAddColumn` の
        // ドキュメント参照。カタログ照会は権限ゲート通過後の実行段が担う）。
        let is_alter_table_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("ALTER")
        );
        if is_alter_table_statement {
            let stmt = crate::sql::allowlist::validate_alter_table_tokens(&tokens)?;
            return Ok(ParsedSql::AlterTable(stmt));
        }

        let stmt = crate::sql::allowlist::validate_sql_tokens(&tokens, &self.storage)?;
        Ok(ParsedSql::Statement(stmt))
    }

    /// Parse（拡張クエリプロトコルの 'P' 種別。Issue #935・WIRE-12・TASK-217）:
    /// `$n` を含みうる SQL テキストを構造検証だけ行い、実行（行・台帳・世代への
    /// 到達）は一切行わない。値はまだ受け取っていないため、各 `$n` は位置に
    /// 依存しない固定ダミー値へ一時的に置換したうえで構造検証を通す
    /// （`sql::params` モジュールドキュメント参照。ダミー置換後の `Statement` は
    /// 破棄せず [`PreparedSql::dummy_parsed`] として Describe 専用に保持する——
    /// 実行には決して使わない）。
    ///
    /// `$n` が [`crate::sql::params`] の許可位置以外に現れる場合・パラメータ番号が
    /// 上限を超える場合のエラーは通常の構造検証エラーと同じ分類
    /// （`42601`／`54000`）で返る。`$n` を含まない SQL は
    /// [`Self::parse_sql`] と完全に同一の結果を `param_count() == 0` として包む。
    pub fn parse_sql_prepared(
        &self,
        sql: &str,
    ) -> Result<PreparedSql, crate::sql::allowlist::SqlSurfaceError> {
        let tokens = crate::sql::lexer::tokenize_with_params(sql)?;
        // TABLE-18・SQL-23・TASK-205（Issue #909）: `CREATE VIEW`／`DROP VIEW`
        // は DDL であり、`sql::params` が受理する `$n` の許可位置一覧
        // （WHERE 等号右辺・VALUES 節等）はいずれも DML 向けの規範形のため、
        // これらの DDL トークン列に紛れ込んだ `$n` は位置に関わらず一律で
        // 拒否する（PostgreSQL の DDL がバインドパラメータを受け付けない
        // のと同じ方針。`sql::params::validate_param_positions` の汎用位置
        // 判定へ DDL 分岐を混在させない設計）。
        let is_create_or_drop_view = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name))
                if name.eq_ignore_ascii_case("CREATE") || name.eq_ignore_ascii_case("DROP")
        ) && matches!(
            tokens.get(1),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("VIEW")
        );
        if is_create_or_drop_view
            && tokens
                .iter()
                .any(|t| matches!(t, crate::sql::lexer::Token::Param(_)))
        {
            return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "bind parameters are not supported in CREATE VIEW / DROP VIEW",
            ));
        }
        let param_count = crate::sql::params::validate_param_positions(&tokens)?;
        // ダミー置換前の元トークン列から判定する（置換後は `$n` 自体が
        // 消えるため、置換後トークン列からは判定できない）。
        let order_by_distance_literal_is_param =
            crate::sql::params::order_by_distance_literal_is_param(&tokens);
        let where_equality_dummy_flags =
            crate::sql::params::where_equality_literal_is_param(&tokens);
        let dummy_tokens = crate::sql::params::substitute_dummy(&tokens);
        let dummy_parsed = self.parse_tokens(dummy_tokens)?;
        Ok(PreparedSql {
            tokens,
            param_count,
            dummy_parsed,
            order_by_distance_literal_is_param,
            where_equality_dummy_flags,
        })
    }

    /// Bind（拡張クエリプロトコルの 'B' 種別。Issue #935・WIRE-12・TASK-217）:
    /// [`Self::parse_sql_prepared`] が返したテンプレートへ、実値（テキスト形式。
    /// バイナリ形式パラメータは wire 層が `0A000` で拒否済みの前提）を束縛し、
    /// [`Self::parse_sql`] が SQL テキストから直接返すのと**完全に同一**の
    /// [`ParsedSql`] を返す（第 2 の実行器を作らない設計。実行・Describe は
    /// 以降すべて既存の [`Self::execute_parsed_in_session`]／
    /// [`Self::describe_parsed_in_session`] を通る）。
    ///
    /// `values` は `$1` から順に対応する実値（NULL は `None`。本バージョンでは
    /// スコープ外として `22000` で拒否する。詳細は `sql::params` モジュール
    /// ドキュメント参照）。個数が [`PreparedSql::param_count`] と一致しない場合は
    /// `22000`（wire 層は Bind メッセージ自体の値件数検証で `08P01` を先に返す
    /// 契約のため、ここでの不一致検出は防御的な保険）。
    pub fn bind_prepared(
        &self,
        prepared: &PreparedSql,
        values: &[Option<Vec<u8>>],
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        if values.len() != prepared.param_count as usize {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                format!(
                    "expected {} bind value(s), got {}",
                    prepared.param_count,
                    values.len()
                ),
            ));
        }
        let decoded = crate::sql::params::decode_bind_values(&prepared.tokens, values)?;
        let substituted = crate::sql::params::substitute_values(&prepared.tokens, &decoded)?;
        self.parse_tokens(substituted)
    }

    /// Describe(statement)（拡張クエリプロトコルの 'D' 種別 S。Issue #935・
    /// WIRE-12・TASK-217）: [`Self::parse_sql_prepared`] が Parse 時点で構築した
    /// ダミー値束縛済みの [`ParsedSql`] をそのまま
    /// [`Self::describe_parsed_in_session`] へ渡す。値が未確定な段階（Bind 前）
    /// でも呼べる契約を維持するため、実際のパラメータ値は一切参照しない
    /// （投影列の形はどの `$n` 値を束縛しても変わらない——列名・型は列挙型
    /// `Statement`/`ValidatedInsert` 等の構造にのみ依存し、リテラル値には
    /// 依存しないため、ダミー値での導出結果は実値束縛後と常に一致する）。
    pub fn describe_prepared_in_session(
        &self,
        session: &crate::sql::mode::SessionState,
        prepared: &PreparedSql,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        // `prepared.dummy_parsed` は Parse 時点（値未確定）の構造検証のため
        // 全 `$n` を固定ダミー値へ置換した `ParsedSql` である。`ORDER BY`
        // ベクトルリテラルの実パースを省略してよいのは、そのリテラルが
        // 実際に `$n` に由来するダミー値である場合（`prepared.
        // order_by_distance_literal_is_param == true`）に限る。`$n` を含まない
        // 文では `substitute_dummy` が当該位置を変更しないため `dummy_parsed`
        // にも元の実リテラルがそのまま残っており、この場合は通常の
        // `describe_parsed_in_session` と同じく必ず検証する（PR #1012
        // レビュー指摘対応: 以前は文中の `$n` の位置に関係なく常に検証を
        // 省略していたため、パラメータ化されていない実ベクトルリテラルの
        // 不正値まで Describe をすり抜けて Execute まで遅延していた）。
        //
        // 同じ理由で `WHERE` 等価述語（`prepared.where_equality_dummy_flags`）
        // も `$n` 由来の位置に限り ENUM 列の語彙照合を省略する（PR #1012
        // Cursor Bugbot 指摘対応: `describe_parsed_in_session_impl` 内で
        // `bind_aggregate_with_dummy_flags`／`bind_projection_for_describe`／
        // `bind_scan_with_dummy_flags` の
        // いずれが呼ばれても同じ配列を共有する）。
        self.describe_parsed_in_session_impl(
            session,
            &prepared.dummy_parsed,
            prepared.order_by_distance_literal_is_param,
            &prepared.where_equality_dummy_flags,
        )
    }

    /// [`Self::parse_sql`] が返した [`ParsedSql`] を実行する（Issue #933・
    /// TASK-71・WIRE-11。拡張クエリプロトコルの Execute（#934）がそのまま使う
    /// 単一の実行入口——SQL テキスト経由・束縛済み計画経由で第 2 の実行器を
    /// 作らない設計を踏襲する）。各分岐は [`Self::execute_sql_in_session`] が
    /// 従来インライン実行していた本体をそのまま移設したもので、実行順序・
    /// エラー契約は不変。
    pub fn execute_parsed_in_session(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        match parsed {
            ParsedSql::Insert(stmt) => {
                // Issue #933: Parse 時点のスキーマキャッシュ（`InsertSchemaLookup`）
                // は Execute まで持ち越さない——Parse／Execute が時間的に分離しうる
                // 拡張クエリプロトコル（#934 の Execute）では、Execute 時点の
                // 最新スキーマを使う契約に揃える（`execute_sql_in_session` の
                // 単発合成でも実質的に同じタイミングで新規構築するため挙動は不変）。
                let lookup = InsertSchemaLookup::new(&self.storage);
                // `RETURNING`（Issue #873・SQL-21）はセッション経由の実行経路
                // 専用（`execute_insert_sql`／`execute_insert_sql_batch` は
                // 検証直後に `42601` で拒否する。`Self` モジュールドキュメント
                // 参照）。
                if stmt.returning.is_some() {
                    let outcome = self.execute_insert_returning_form(ctx, stmt, &lookup)?;
                    return Ok(crate::sql::SqlOutcome::Returning(outcome));
                }
                let outcome = self.execute_insert_form(ctx, stmt, &lookup)?;
                Ok(crate::sql::SqlOutcome::Insert(outcome))
            }
            ParsedSql::Truncate(stmt) => {
                let outcome = self.execute_truncate_form(ctx, stmt)?;
                Ok(crate::sql::SqlOutcome::Truncate(outcome))
            }
            ParsedSql::Delete(stmt) => match stmt {
                // 単一行形（[`crate::sql::allowlist::ValidatedDelete`]）は
                // `RETURNING`（Issue #873・SQL-21）を保持しうるためここで
                // 分岐する（`execute_delete_sql` は検証直後に `42601` で拒否
                // する非セッション経路。`Self` モジュールドキュメント参照）。
                // 述語形（[`crate::sql::allowlist::ValidatedPredicateDelete`]）
                // は構造検証段（`validate_delete_statement_tokens`）で
                // `RETURNING` 併用を既に `42601` 拒否済みのため常に
                // `execute_predicate_delete_form` へ委譲する（Issue #871）。
                crate::sql::allowlist::DeleteStatement::SingleRow(v) => {
                    if v.returning.is_some() {
                        let outcome = self.execute_delete_returning_form(ctx, v)?;
                        return Ok(crate::sql::SqlOutcome::Returning(outcome));
                    }
                    let outcome = self.execute_delete_form(ctx, v)?;
                    Ok(crate::sql::SqlOutcome::Delete(outcome))
                }
                crate::sql::allowlist::DeleteStatement::Predicate(v) => {
                    let outcome = self.execute_predicate_delete_form(ctx, session, v)?;
                    Ok(crate::sql::SqlOutcome::Delete(outcome))
                }
            },
            ParsedSql::Update(stmt) => {
                let outcome = self.execute_predicate_update_form(ctx, session, stmt)?;
                Ok(crate::sql::SqlOutcome::Update(outcome))
            }
            // SQL-23・TASK-203（Issue #902）: DDL 実行権限ゲート
            // （`sql::ddl::require_ddl_permission`）を、カタログ照会（対象
            // テーブルの存在確認）を含む `execute_drop_table` より必ず先に
            // 通す（`SqlSurfaceError::InsufficientPrivilege` ドキュメント
            // 参照）。`ctx`（テナント境界）は `Storage::drop_table` が取らない
            // ため未使用のまま——`DROP TABLE` はテナントスコープの操作ではない。
            ParsedSql::DropTable(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_drop_table(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::DropTable(outcome))
            }
            // SQL-23・TASK-202（Issue #899）: `DropTable` と同じ単一の DDL 実行
            // 権限ゲート（`sql::ddl::require_ddl_permission`）を、カタログ照会
            // （テーブル名の重複判定）を含む `execute_create_table` より必ず先に
            // 通す。`ctx`（テナント境界）は `Storage::create_table` が取らない
            // ため未使用のまま——`CREATE TABLE` はテナントスコープの操作ではない。
            ParsedSql::CreateTable(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_create_table(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::CreateTable(outcome))
            }
            // SQL-23・TASK-202（Issue #900）: `DropTable`／`CreateTable` と同じ
            // 単一の DDL 実行権限ゲート（`sql::ddl::require_ddl_permission`）を、
            // カタログ照会（ENUM 型名・テーブル・列の存在確認）を含む
            // `execute_alter_table_add_column` より必ず先に通す（権限の無い
            // セッションへテーブル・列の存在有無を返さない）。`ctx`（テナント
            // 境界）は `Storage::alter_table_add_column` が取らないため未使用。
            ParsedSql::AlterTable(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_alter_table_add_column(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::AlterTable(outcome))
            }
            // TABLE-18・SQL-23・TASK-205（Issue #909）: `DropTable` と同じ判定
            // 順序（DDL 実行権限ゲート → カタログ照会を含む実行本体）。
            ParsedSql::CreateView(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_create_view(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::CreateView(outcome))
            }
            ParsedSql::DropView(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_drop_view(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::DropView(outcome))
            }
            // TASK-206・INDEX-7・SQL-23（Issue #908）: `DropTable`・`CreateView` と
            // 同じ判定順序（DDL 実行権限ゲート → カタログ照会を含む実行本体）。
            // 権限を持たない主体には対象テーブル・索引の有無を問わず常に `42501`
            // のみを返す。`ctx` は取らない（索引宣言はテナントスコープの操作では
            // なく、索引の物理表現は既存の `(table, PolicyContext)` 可視
            // スナップショット由来キャッシュのまま）。
            ParsedSql::CreateIndex(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_create_index(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::CreateIndex(outcome))
            }
            ParsedSql::DropIndex(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_drop_index(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::DropIndex(outcome))
            }
            ParsedSql::Statement(stmt) => {
                self.execute_validated_in_session(ctx, session, stmt.clone())
            }
            // トランザクション文脈を持たない本エントリポイントは `BEGIN`／
            // `COMMIT`／`ROLLBACK` を受理しない（SQL-31・TASK-221。
            // トランザクション対応の実行入口は [`Self::execute_sql_in_txn`]）。
            // PR #1041 レビュー指摘（P1）: `parse_tokens` が `BEGIN`／`COMMIT`／
            // `ROLLBACK` を新たに `ParsedSql::Transaction` として受理するように
            // なった結果、本メソッド（`execute_sql_in_session` の実行本体）が
            // これらの文に対して以前（許可リスト外として `UnsupportedSyntax`
            // ＝ `42601`）と異なる `TransactionFeatureNotSupported`（`0A000`）を
            // 返すようになっていた。トランザクション文脈を持たない本 API の
            // エラー契約を変えないため、ここでは意図的に `unsupported`
            // （`42601`）へ写像し直す ―― `TransactionFeatureNotSupported`
            // （`0A000`）は明示トランザクション対応の実行入口
            // （[`Self::execute_parsed_in_txn`]・`Active` 中に未対応の文を
            // 拒否する場合）専用のまま維持する。
            ParsedSql::Transaction(_) => Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "transaction control statements are not supported by this entry point",
            )),
            // WIRE-15・TASK-218: カーソルは明示トランザクション内でのみ有効
            // （`sql::cursor` モジュールドキュメント参照）。トランザクション
            // 文脈を持たない本エントリポイントでは、`DECLARE` は「トランザク
            // ション外」（`25P01`。`COMMIT`/`ROLLBACK` を `Idle` で送った場合と
            // 同じ分類）、`FETCH`／`CLOSE` は対象カーソルが存在し得ないため
            // `34000` で拒否する。
            ParsedSql::Cursor(stmt) => match stmt {
                crate::sql::cursor::CursorStatement::Declare { .. } => {
                    Err(crate::sql::allowlist::SqlSurfaceError::NoActiveSqlTransaction)
                }
                crate::sql::cursor::CursorStatement::Fetch { .. }
                | crate::sql::cursor::CursorStatement::Close { .. } => {
                    Err(crate::sql::allowlist::SqlSurfaceError::invalid_cursor_name())
                }
            },
        }
    }

    /// SQL テキストを検証・束縛・実行まで単発で行う従来の入口（TASK-73・WIRE-1。
    /// 簡易クエリプロトコルから呼ばれる）。[`Self::parse_sql`]（検証のみ）と
    /// [`Self::execute_parsed_in_session`]（実行のみ）の合成であり、Issue #933
    /// による分割の前後でこの関数自体の判定順序・エラー契約・実行結果は不変
    /// （両メソッドの移設元コメント参照）。
    ///
    /// `CREATE TABLE`（SQL-23・TASK-85・TASK-202、Issue #899）・`DROP TABLE`
    /// （SQL-23・TASK-203、Issue #902）は [`Self::parse_tokens`] が
    /// [`ParsedSql::CreateTable`]／[`ParsedSql::DropTable`] へ構造検証のみ
    /// （カタログ照会なし）で束縛し、DDL 実行権限ゲート
    /// （[`crate::sql::ddl::require_ddl_permission`]）はカタログ照会を含む
    /// 実行本体より必ず先に [`Self::execute_parsed_in_session`] が適用する
    /// （fail-closed。構造検証はカタログを一切参照しないため許可形状に一致
    /// しない構文は権限の有無に関わらず `42601` になるが〔構文の正誤自体は
    /// テナント・カタログの存在情報ではないためオラクルにならない〕、許可
    /// 形状に一致した以降は未許可の主体が対象テーブルの有無を問わず常に
    /// `InsufficientPrivilege`〔`42501`〕のみを受け取り、カタログの状態を
    /// 一切観測できない。`sql::ddl` モジュールドキュメント参照）。
    pub fn execute_sql_in_session(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let tokens = crate::sql::lexer::tokenize(sql)?;
        let parsed = self.parse_tokens(tokens)?;
        self.execute_parsed_in_session(ctx, session, &parsed)
    }

    /// [`Self::execute_sql_in_session`] のトランザクション対応版（SQL-31・
    /// TASK-221）。`txn` が `Idle` の間は [`Self::execute_parsed_in_session`]
    /// （既存 autocommit 経路）とビット同一に振る舞う。`wire-server` の簡易・
    /// 拡張クエリ両経路が、接続（セッション）ごとに 1 つ保持する
    /// `SessionTransaction` を渡してここへ委譲する唯一の入口（第 2 の実行器を
    /// 作らない設計。`Self::execute_sql_in_session` と同じ判定順序を踏襲する）。
    pub fn execute_sql_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        sql: &str,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // `Failed` 中は `ROLLBACK` 以外を parse より前に `25P02` で拒否する
        // （PR #1041 レビュー指摘: parse を先に行うと、不正な SQL に対して
        // `42601` 等が返り `Failed` であることがクライアントへ伝わらない）。
        // `ROLLBACK` かどうかは先頭トークンだけで判定し、`ROLLBACK` の規範形の
        // 検証は通常どおり parse に任せる。字句解析自体に失敗した入力は
        // `ROLLBACK` ではないものとして `25P02` に倒す。
        if txn.status() == crate::sql::transaction::TransactionStatus::Failed
            && !crate::sql::transaction::is_rollback_statement(sql)
        {
            return Err(txn.take_failed_error());
        }
        // 構文・許可リスト検証のエラーも、明示トランザクション中なら種類を問わず
        // `Failed` へ遷移させる（PostgreSQL と同じ。`Active` のまま残すと後続の
        // `COMMIT` が先行する書き込みを永続化してしまう。PR #1041 レビュー指摘）。
        // `fail` は `Active` 以外では何もしないため、`Idle` の autocommit 経路・
        // 既に `Failed` の経路の挙動は不変。
        let parsed = match self.parse_sql(sql) {
            Ok(parsed) => parsed,
            Err(e) => {
                txn.fail();
                return Err(e);
            }
        };
        self.execute_parsed_in_txn(ctx, session, txn, &parsed)
    }

    /// [`Self::execute_parsed_in_session`] のトランザクション対応版（SQL-31・
    /// TASK-221）。`parsed` が `ParsedSql::Transaction` なら `txn` の状態機械を
    /// 直接遷移させる。それ以外は `txn.status()` に応じて分岐する:
    /// - `Idle`: 既存の [`Self::execute_parsed_in_session`]（autocommit）へ
    ///   そのまま委譲（挙動は不変）。
    /// - `Failed`: `ROLLBACK` 以外は一切実行せず `25P02` を返す
    ///   （`ParsedSql::Transaction(Rollback)` は上の分岐で既に処理済み）。
    /// - `Active`: 上限検査・`operation_id` 再利用検査（[`crate::sql::
    ///   transaction::SessionTransaction::check_and_register_statement`]）を
    ///   経てから、明示トランザクション内で対応する書き込み系（`INSERT` 単一行
    ///   形・`TRUNCATE`）・読み取り系（未書き込みテーブルの `SELECT`／
    ///   `Aggregate`／`Scan`）文のみを実行する。対応外の文
    ///   （複数行/ファイル形/`UPSERT` の `INSERT`・`UPDATE`・`DELETE`・
    ///   `UPSERT`・COPY・書き込み済みテーブルへの読み取り等）は `0A000` で
    ///   拒否する。文の実行中にエラーが起きた場合は必ずトランザクションを
    ///   `Failed` へ遷移させ（[`crate::sql::transaction::SessionTransaction::
    ///   fail`]）、元のエラーをそのまま返す（部分書き込みを残さない
    ///   fail-closed 契約。`docs/design/explicit-transaction.md` 参照）。
    pub fn execute_parsed_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::transaction::{TransactionStatus, TxnControl};

        if let ParsedSql::Transaction(ctrl) = parsed {
            return match ctrl {
                TxnControl::Begin => {
                    txn.begin(&self.storage, session)?;
                    Ok(crate::sql::SqlOutcome::Begin)
                }
                TxnControl::Commit => {
                    txn.commit(session)?;
                    Ok(crate::sql::SqlOutcome::Commit)
                }
                TxnControl::Rollback => {
                    txn.rollback(session)?;
                    Ok(crate::sql::SqlOutcome::Rollback)
                }
            };
        }

        match txn.status() {
            TransactionStatus::Idle => self.execute_parsed_in_session(ctx, session, parsed),
            TransactionStatus::Failed => Err(txn.take_failed_error()),
            TransactionStatus::InTransaction => {
                txn.check_and_register_statement(parsed_operation_id(parsed))?;
                let result = self.execute_in_active_txn(ctx, session, txn, parsed);
                if result.is_err() {
                    txn.fail();
                }
                result
            }
        }
    }

    /// [`Self::execute_parsed_in_txn`] の `Active` 分岐本体（SQL-31・TASK-221。
    /// エラー時のトランザクション遷移は呼び出し元が一括して行う）。
    fn execute_in_active_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::{SqlSurfaceError, Statement};

        match parsed {
            ParsedSql::Insert(stmt) if stmt.returning.is_none() => {
                let schema =
                    self.storage
                        .get_table_schema(&stmt.table_name)
                        .map_err(|e| match e {
                            CatalogError::TableNotFound(name) => {
                                SqlSurfaceError::UndefinedTable { name }
                            }
                            _ => SqlSurfaceError::Internal {
                                detail: "failed to load table schema".to_string(),
                            },
                        })?;
                let bound = crate::sql::parser::bind_insert_form(stmt, &schema)?;
                match bound {
                    crate::sql::parser::BoundInsertForm::Row(bound) => {
                        let write_txn =
                            txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "internal error".to_string(),
                            })?;
                        let outcome = crate::sql::exec::execute_insert_with_schema_in(
                            crate::tenant::WriteTarget::InTxn(write_txn),
                            ctx,
                            &bound,
                            self.ledger_mode,
                            Some(&schema),
                        )?;
                        txn.mark_written(&stmt.table_name);
                        Ok(crate::sql::SqlOutcome::Insert(outcome))
                    }
                    // 複数行 `VALUES`・ファイル形・`ON CONFLICT`（UPSERT）は
                    // 明示トランザクション内では未対応（対象外。
                    // `docs/design/explicit-transaction.md` 参照）。
                    _ => Err(SqlSurfaceError::transaction_feature_not_supported(
                        "this INSERT form is not supported inside an explicit transaction",
                    )),
                }
            }
            ParsedSql::Truncate(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let outcome = crate::sql::exec::execute_truncate_in(
                    crate::tenant::WriteTarget::InTxn(write_txn),
                    ctx,
                    stmt,
                    self.ledger_mode,
                )?;
                txn.mark_written(&stmt.table_name);
                Ok(crate::sql::SqlOutcome::Truncate(outcome))
            }
            ParsedSql::Statement(
                stmt @ (Statement::SetSearchMode { .. } | Statement::CreateFunction { .. }),
            ) => self.execute_validated_in_session(ctx, session, stmt.clone()),
            ParsedSql::Statement(stmt @ Statement::Select(v)) => {
                self.read_only_in_active_txn(ctx, session, txn, &v.table_name, stmt.clone())
            }
            ParsedSql::Statement(stmt @ Statement::Aggregate(v)) => {
                let table = v.table_name().to_string();
                self.read_only_in_active_txn(ctx, session, txn, &table, stmt.clone())
            }
            ParsedSql::Statement(stmt @ Statement::Scan(v)) => {
                let table = v.table_name().to_string();
                self.read_only_in_active_txn(ctx, session, txn, &table, stmt.clone())
            }
            ParsedSql::Statement(stmt @ Statement::Explain(v)) => {
                let table = v.table_name().to_string();
                self.read_only_in_active_txn(ctx, session, txn, &table, stmt.clone())
            }
            // WIRE-15・TASK-218: カーソルは `Active` なトランザクション内でのみ
            // 意味を持つ（`sql::cursor` モジュールドキュメント参照）。
            ParsedSql::Cursor(stmt) => self.execute_cursor_in_active_txn(ctx, session, txn, stmt),
            // 複数行 INSERT・ファイル形 INSERT・UPSERT・`UPDATE`・`DELETE`・
            // COPY 等、明示トランザクション内での対応外の文（対象外。
            // `docs/design/explicit-transaction.md` 参照）。
            _ => Err(SqlSurfaceError::transaction_feature_not_supported(
                "this statement is not supported inside an explicit transaction",
            )),
        }
    }

    /// `DECLARE` の内側 SELECT（`sql::cursor::validate_declare_tokens` により
    /// `Statement::Aggregate`／`Statement::Scan` のいずれかであることが構造
    /// 検証段で保証済み）専用の実行入口（PR #1049 レビュー指摘 P1 対応）。
    ///
    /// `Statement::Scan` は [`crate::sql::scan::execute_scan_with_budget`] を、
    /// `Statement::Aggregate` は [`Self::run_aggregate_plan`] を、いずれも
    /// [`crate::sql::cursor::MAX_CURSOR_BYTES_PER_SESSION`]（16 MiB）の結果
    /// バイト予算で直接呼び、行生成中の累積判定を [`Self::
    /// execute_validated_in_session`] 経由の既定予算（`Statement::Scan` の
    /// `sql::scan::MAX_SCAN_RESULT_BYTES`＝1 GiB、`Statement::Aggregate` の
    /// [`crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES`]＝1 GiB）より遥かに
    /// 手前で打ち切る——`CursorRegistry::declare` のバイト上限判定（16 MiB）は
    /// 本来カーソルが保持できる上限を表すにも関わらず、既定予算のまま実行する
    /// とその判定に到達する前に大きな結果をメモリ上に確定させてしまう
    /// （ネットワーク入力によるメモリ確保量の増幅。security.md「不安全な
    /// 設計」対応。PR #1049 レビュー指摘 P0 対応: `Statement::Aggregate` は
    /// `GROUP BY` のグループキー累計バイト数上限（`sql::group_by::
    /// MAX_GROUP_KEY_TOTAL_BYTES`）・`TEXT` 集計状態の累計バイト数上限が
    /// それぞれ独立に 16 MiB で頭打ちになるのみで、両者の合計や `GROUP BY`
    /// なしの単一行経路の `MIN`/`MAX(<TEXT 列>)` 累計（最大
    /// [`crate::sql::allowlist::MAX_AGGREGATE_ITEMS`] 項目分）を完成した
    /// [`crate::sql::exec::QueryResult`] 全体の容量として制限していなかった
    /// ため、既定予算のまま `run_aggregate_plan` へ委譲すると `declare` の
    /// 16 MiB 判定に到達する前に遥かに大きな結果を確定させてしまう。`run_aggregate_plan`
    /// 自体に `max_result_bytes` を明示的に渡すことで、`sql::aggregate::
    /// execute_aggregate_with_cache`／`sql::group_by::execute_grouped_aggregate`
    /// の行生成ループが 16 MiB 予算を生成中に検査するようにする）。それ以外の
    /// `Statement` 種別への到達は構造検証段の保証により起き得ないが、防御的に
    /// 通常の [`Self::execute_validated_in_session`] へ委譲する。
    ///
    /// `max_result_bytes` は呼び出し元（[`Self::execute_cursor_in_active_txn`]）が
    /// 実行前に求めたセッションのカーソル残容量（[`crate::sql::cursor::
    /// CursorRegistry::remaining_bytes`]＝16 MiB − 既存カーソルの保持量）。
    /// 上記の 16 MiB は既存カーソルが無い場合の上限であり、既存カーソルが
    /// 保持している分を差し引いた残容量を生成予算にすることで、`declare` の
    /// 合計上限判定より前に「既存保持量＋新規結果」が上限を超えて確保される
    /// ことを防ぐ（PR #1049 レビュー指摘 codex P1 対応）。
    fn execute_cursor_inner_query(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        inner: &crate::sql::allowlist::Statement,
        max_result_bytes: usize,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        if let crate::sql::allowlist::Statement::Scan(validated) = inner {
            let (read_txn, schema) = self.read_txn_with_schema(&validated.table_name)?;
            let bound = crate::sql::parser::bind_scan(validated, &schema, session.udfs())?;
            let result = crate::sql::scan::execute_scan_with_budget(
                &read_txn,
                ctx,
                &schema,
                &bound,
                max_result_bytes,
            )?;
            return Ok(crate::sql::SqlOutcome::Query(result));
        }
        if let crate::sql::allowlist::Statement::Aggregate(validated) = inner {
            let (read_txn, schema) = self.read_txn_with_schema(&validated.table_name)?;
            let bound = crate::sql::parser::bind_aggregate(validated, &schema, session.udfs())?;
            let result =
                self.run_aggregate_plan(&read_txn, ctx, &schema, &bound, max_result_bytes)?;
            return Ok(crate::sql::SqlOutcome::Query(result));
        }
        self.execute_validated_in_session(ctx, session, inner.clone())
    }

    /// [`Self::execute_in_active_txn`] の `ParsedSql::Cursor` 分岐本体
    /// （WIRE-15・TASK-218）。
    ///
    /// - `DECLARE`: 同一トランザクション内で既に書き込み済みのテーブルへの
    ///   読み取りは `0A000`（[`Self::read_only_in_active_txn`] と同じ既知の
    ///   逸脱）。カーソル数上限・重複名は内側 SELECT を実行する**前**に判定し
    ///   （[`crate::sql::cursor::CursorRegistry::ensure_capacity_for_declare`]）、
    ///   内側 SELECT 自体の実行は [`Self::execute_cursor_inner_query`] へ
    ///   委譲し、実行後に確定した [`crate::sql::exec::QueryResult`] をバイト
    ///   上限判定込みで登録する（[`crate::sql::cursor::CursorRegistry::declare`]）。
    /// - `FETCH`／`CLOSE`: `CursorRegistry` への委譲のみ。検索本体は再実行
    ///   しない。
    fn execute_cursor_in_active_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        stmt: &crate::sql::cursor::CursorStatement,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::SqlSurfaceError;
        use crate::sql::cursor::CursorStatement;

        match stmt {
            CursorStatement::Declare { name, query } => {
                // 構文解析段（`parse_tokens`）は構造検証のみでカタログを参照
                // しないため、ここで内側 SELECT を実カタログに対して再検証する
                // （PR #1049 レビュー指摘 codex P1 対応）。通常の広域取得
                // `SELECT` と同じ `validate_sql_tokens` を通すことで、FROM が
                // ビューを指す場合も同じ `sql::view::resolve_from` による展開
                // （基底テーブル＋ビュー由来述語。RLS は後段の実行経路が参照
                // セッション自身の `ctx` で暗黙適用する）が適用される。
                // `table` はビュー展開後の基底テーブル名。
                let (inner, table) =
                    crate::sql::cursor::validate_declare_inner(query, &self.storage)?;
                if txn.table_already_written(&table) {
                    return Err(SqlSurfaceError::transaction_feature_not_supported(
                        "reading a table already written in the same transaction is not supported",
                    ));
                }
                let registry = txn.cursors_mut().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                registry.ensure_capacity_for_declare(name)?;
                // 既存カーソルの保持量を差し引いた残容量を内側 SELECT の生成予算に
                // する（PR #1049 レビュー指摘 codex P1 対応。
                // `execute_cursor_inner_query` のドキュメント参照）。
                let remaining_bytes = registry.remaining_bytes();
                let outcome =
                    self.execute_cursor_inner_query(ctx, session, &inner, remaining_bytes)?;
                let result = match outcome {
                    crate::sql::SqlOutcome::Query(result) => result,
                    // `inner` は構造検証段で `Statement::Aggregate`／
                    // `Statement::Scan` のいずれかであることを保証済み
                    // （`sql::cursor::validate_declare_tokens` 参照）。いずれも
                    // `execute_validated_in_session` は必ず `SqlOutcome::Query`
                    // を返すため、この分岐は公開 API の誤用時のみの防御的経路。
                    _ => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "internal error".to_string(),
                        })
                    }
                };
                txn.cursors_mut()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?
                    .declare(name.clone(), result)?;
                Ok(crate::sql::SqlOutcome::DeclareCursor)
            }
            CursorStatement::Fetch { name, count } => {
                let result = txn
                    .cursors_mut()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?
                    .fetch(name, *count)?;
                Ok(crate::sql::SqlOutcome::Fetch(result))
            }
            CursorStatement::Close { name } => {
                txn.cursors_mut()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?
                    .close(name)?;
                Ok(crate::sql::SqlOutcome::CloseCursor)
            }
        }
    }

    /// 明示トランザクション内の読み取り系文（`Active` のときのみ呼ばれる）。
    /// 同一トランザクション内で既に書き込み済みのテーブルへの読み取りは
    /// `0A000` で拒否する（自トランザクションの未 commit 変更の可視化は対象外。
    /// `sql::transaction` モジュールドキュメント「読み取りの既知の逸脱」参照）。
    /// それ以外は BEGIN 時点のスナップショットに対する通常の読み取り経路
    /// （[`Self::execute_validated_in_session`]）へそのまま委譲する。
    fn read_only_in_active_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &crate::sql::transaction::SessionTransaction<'e>,
        table: &str,
        stmt: crate::sql::allowlist::Statement,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        if txn.table_already_written(table) {
            return Err(
                crate::sql::allowlist::SqlSurfaceError::transaction_feature_not_supported(
                    "reading a table already written in the same transaction is not supported",
                ),
            );
        }
        self.execute_validated_in_session(ctx, session, stmt)
    }

    /// Describe（拡張クエリプロトコルの 'D' 種別 S。Issue #933・TASK-71・
    /// WIRE-11）が返す結果列メタデータを、[`Self::parse_sql`] の結果から検索・
    /// 集計・広域取得の本体を一切実行せずに導出する。`None` は「結果列を持たない
    /// 文」（`CommandComplete` のみを返す DML・`SET`・`CREATE FUNCTION`）を表し、
    /// wire 層はこれを `NoData` へ写像する契約。
    ///
    /// 行・台帳・世代のいずれにも触れない（`INSERT`／`UPDATE`／`DELETE`／
    /// `TRUNCATE` は `RETURNING` の投影束縛のみを行い、実際の書き込みは行わない）。
    /// `USING PLAN` 経由の `SELECT` は投影列の束縛のみを行い、`plan_query`・
    /// `Embedder::embed_batch` 等の LLM／埋め込み I/O は一切呼ばない
    /// （Describe を LLM コスト増幅の DoS 経路にしないための契約）。
    pub fn describe_parsed_in_session(
        &self,
        session: &crate::sql::mode::SessionState,
        parsed: &ParsedSql,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        // 実リテラルを持つ通常の呼び出し（wire-server の Describe・単体テスト
        // 等）は常に値検証を行う（`skip_vector_literal_validation = false`・
        // `dummy_equality_flags = &[]`）。ダミー値専用の縮退は
        // `describe_prepared_in_session` のみが使う（PR #1012 レビュー指摘
        // 対応。詳細は同メソッドのコメント参照）。
        self.describe_parsed_in_session_impl(session, parsed, false, &[])
    }

    /// [`Self::describe_parsed_in_session`] のトランザクション対応版
    /// （WIRE-15・TASK-218。`wire-server::extended_query::handle_describe`／
    /// `handle_bind` が接続単位の `SessionTransaction` を渡して呼ぶ）。
    /// `ParsedSql::Cursor(CursorStatement::Fetch { name, .. })` に限り、`txn` が
    /// `Active` で `name` のカーソルが実在する場合はその結果列メタデータを
    /// 返し、実在しない場合は `34000`（検索本体を実行せずに判定できる—— カーソル
    /// 実行時に確定済みの `QueryResult::columns` を読むだけで済むため）。
    /// `Declare`／`Close` は結果列を持たない（`CommandComplete` のみ）。
    /// それ以外の `ParsedSql` は [`Self::describe_parsed_in_session`] と完全に
    /// 同一の判定へ委譲する。
    pub fn describe_parsed_in_txn(
        &self,
        session: &crate::sql::mode::SessionState,
        txn: &crate::sql::transaction::SessionTransaction<'_>,
        parsed: &ParsedSql,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        if let ParsedSql::Cursor(stmt) = parsed {
            return match stmt {
                crate::sql::cursor::CursorStatement::Declare { .. }
                | crate::sql::cursor::CursorStatement::Close { .. } => Ok(None),
                crate::sql::cursor::CursorStatement::Fetch { name, .. } => {
                    // `Failed` 中はカーソルが既に破棄されているが、Execute と同じく
                    // `25P02` を返す（`34000` にしない。PR #1049 レビュー指摘
                    // Cursor Bugbot Low 対応）。`txn` は読み取り専用のため、期限切れ
                    // 由来の `54000` の 1 回限り報告（`SessionTransaction::
                    // take_failed_error`）は後続の Execute に委ね、ここでは状態を
                    // 変えない。
                    if txn.status() == crate::sql::transaction::TransactionStatus::Failed {
                        return Err(crate::sql::allowlist::SqlSurfaceError::InFailedSqlTransaction);
                    }
                    match txn.cursor_columns(name) {
                        Some(columns) => Ok(Some(columns)),
                        None => Err(crate::sql::allowlist::SqlSurfaceError::invalid_cursor_name()),
                    }
                }
            };
        }
        self.describe_parsed_in_session(session, parsed)
    }

    /// [`Self::describe_parsed_in_session`]／[`Self::describe_prepared_in_session`]
    /// が共有する実装本体。`skip_vector_literal_validation` が `true` のときのみ
    /// `ORDER BY` のベクトルリテラルの実パース（`parse_vector_literal` による
    /// 形式・次元・非有限値・64 KiB 上限検証）を省略する——`prepared.dummy_parsed`
    /// のような実パラメータを持たない構造検証専用の `ParsedSql` を Describe する
    /// 場合に限る（Issue #935・WIRE-12・TASK-217。PR #1012 レビュー指摘対応）。
    ///
    /// `dummy_equality_flags`（PR #1012 Cursor Bugbot 指摘対応）は
    /// `sql::parser::bind_where_predicates`（crate 内限定の
    /// `bind_aggregate_with_dummy_flags`・`bind_projection_for_describe`・
    /// `bind_scan_with_dummy_flags` 経由。公開 API の `bind_aggregate`／
    /// `bind_scan` はフラグを受け取らず常に全値検証する）へそのまま渡し、`$n`
    /// 由来のダミー値へ置換された `WHERE` 等価述語に限り ENUM 列の語彙照合
    /// （`declarative_filter`）を Describe 時点では省略する。`$n` を含まない
    /// 通常の Describe（`describe_parsed_in_session` 経由）では常に空スライスに
    /// なり、実リテラルは従来どおり必ず検証される。
    fn describe_parsed_in_session_impl(
        &self,
        session: &crate::sql::mode::SessionState,
        parsed: &ParsedSql,
        skip_vector_literal_validation: bool,
        dummy_equality_flags: &[bool],
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        use crate::sql::allowlist::{DeleteStatement, Statement, ValidatedUpdateForm};

        match parsed {
            ParsedSql::Insert(stmt) => {
                let (_read_txn, schema) = self.read_txn_with_schema(&stmt.table_name)?;
                match crate::sql::parser::bind_returning(stmt.returning.as_ref(), &schema)? {
                    Some(projection) => Ok(Some(crate::sql::returning::column_meta(
                        &projection,
                        &schema,
                    )?)),
                    None => Ok(None),
                }
            }
            ParsedSql::Truncate(_) => Ok(None),
            // `DROP TABLE`（Issue #902）は `CommandComplete` のみを返す DDL の
            // ため、`Truncate` と同じく結果列を持たない。DDL 実行権限判定・
            // 実際の削除は一切行わない（Describe は本体を実行しない契約）。
            ParsedSql::DropTable(_) => Ok(None),
            // `CREATE TABLE`（Issue #899）も同じく `CommandComplete` のみを返す
            // DDL のため結果列を持たない。DDL 実行権限判定・カタログへの反映は
            // 一切行わない（Describe は本体を実行しない契約）。
            ParsedSql::CreateTable(_) => Ok(None),
            // `ALTER TABLE ADD COLUMN`（TASK-202・SQL-23。Issue #900）も
            // `CommandComplete` のみを返す DDL のため結果列を持たない。DDL 実行
            // 権限判定・カタログへの反映は一切行わない（Describe は本体を実行
            // しない契約。テーブル・列の存在有無も確認しない）。
            ParsedSql::AlterTable(_) => Ok(None),
            // TABLE-18・SQL-23・TASK-205（Issue #909）: DDL につき結果列を持たない
            // （`DropTable` と同じ扱い）。
            ParsedSql::CreateView(_) => Ok(None),
            ParsedSql::DropView(_) => Ok(None),
            // TASK-206・INDEX-7（Issue #908）: 索引 DDL も結果列を持たない。
            ParsedSql::CreateIndex(_) => Ok(None),
            ParsedSql::DropIndex(_) => Ok(None),
            ParsedSql::Delete(DeleteStatement::SingleRow(v)) => {
                let (_read_txn, schema) = self.read_txn_with_schema(&v.table_name)?;
                match crate::sql::parser::bind_returning(v.returning.as_ref(), &schema)? {
                    Some(projection) => Ok(Some(crate::sql::returning::column_meta(
                        &projection,
                        &schema,
                    )?)),
                    None => Ok(None),
                }
            }
            // 述語形 `DELETE ... WHERE` は構造検証段で `RETURNING` 併用を常に
            // `42601` 拒否済みのため（`ValidatedPredicateDelete` は `returning`
            // フィールド自体を持たない）、結果列は常に持たない。
            ParsedSql::Delete(DeleteStatement::Predicate(_)) => Ok(None),
            // `UPDATE` は `RETURNING` の実行結線が未着手のため、構造検証段
            // （`validate_update_form_tokens`）が単一行・述語形のいずれでも
            // `RETURNING` 併用を常に `42601` 拒否する（`ValidatedUpdate::
            // returning` ドキュメント参照）。結果列は常に持たない。
            ParsedSql::Update(ValidatedUpdateForm::Single(_))
            | ParsedSql::Update(ValidatedUpdateForm::Predicate(_)) => Ok(None),
            ParsedSql::Statement(Statement::SetSearchMode { .. }) => Ok(None),
            ParsedSql::Statement(Statement::CreateFunction { .. }) => Ok(None),
            // `EXPLAIN` は常に単一の `Computed` 列（`QUERY PLAN`）を返す
            // （`sql::explain::build_explain_result` と同一の列。中身の生成には
            // 検索本体の実行が必要だが、列自体は入力文の内容によらず固定のため
            // Describe はプラン実行を一切行わずに列だけを返せる）。
            ParsedSql::Statement(Statement::Explain(_)) => {
                Ok(Some(vec![crate::sql::exec::ColumnMeta::Computed {
                    name: crate::sql::explain::QUERY_PLAN_COLUMN.to_string(),
                }]))
            }
            ParsedSql::Statement(Statement::Aggregate(validated)) => {
                let (_read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
                let bound = crate::sql::parser::bind_aggregate_with_dummy_flags(
                    validated,
                    &schema,
                    session.udfs(),
                    dummy_equality_flags,
                )?;
                Ok(Some(crate::sql::describe::aggregate_columns(
                    &bound.projection,
                )))
            }
            ParsedSql::Statement(Statement::Scan(validated)) => {
                let (_read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
                let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                    validated,
                    &schema,
                    session.udfs(),
                    dummy_equality_flags,
                )?;
                Ok(Some(crate::sql::describe::projected_columns(
                    bound.projection(),
                    &schema,
                )))
            }
            ParsedSql::Statement(Statement::Select(validated)) => {
                let (_read_txn, schema) = self.read_txn_with_schema(&validated.table_name)?;
                if validated.using_plan().is_some() {
                    // `USING PLAN`: LLM 展開・再埋め込み（`plan_query`・
                    // `Embedder::embed_batch`）は行わず、投影列の束縛のみを行う
                    // （`sql::using_plan::pre_check_bindable` が I/O 前に行う
                    // 検証の一部——`bind_projection` 呼び出し——と同じ経路。
                    // 投影列自体は LLM 展開結果に依存しないため、この検証だけで
                    // Describe の結果列を確定できる）。
                    if let Some(literal) = validated.search_mode() {
                        crate::sql::mode::SearchMode::parse_literal(literal)?;
                    }
                    crate::sql::parser::vector_column(&schema)?;
                    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
                    let projection = crate::sql::parser::bind_projection(
                        validated.projection(),
                        &schema,
                        session.udfs(),
                        &mut node_budget,
                    )?;
                    Ok(Some(crate::sql::describe::projected_columns(
                        &projection,
                        &schema,
                    )))
                } else {
                    // `USING MODE` の解決（クエリ句 > セッション変数）は結果列に
                    // 影響しないため Describe では不要——ただし `bind_in_session`
                    // が行うリテラル形式検証はここでは行わない代わりに
                    // `bind_projection_for_describe` が同じ検証を担う。
                    // `skip_vector_literal_validation` が `true`（ダミー値専用の
                    // `describe_prepared_in_session` 経由）の場合に限り、`ORDER BY`
                    // のベクトルリテラルは構造検証に留め実パースを省略する。
                    // 実リテラルを持つ通常の Describe（`skip_vector_literal_validation
                    // == false`）は従来どおり `parse_vector_literal` による形式・
                    // 次元・非有限値・64 KiB 上限検証を行う（PR #1012 レビュー指摘
                    // 対応。詳細は `sql::parser::bind_projection_for_describe` 参照）。
                    let projection = crate::sql::parser::bind_projection_for_describe(
                        validated,
                        &schema,
                        session.udfs(),
                        !skip_vector_literal_validation,
                        dummy_equality_flags,
                    )?;
                    Ok(Some(crate::sql::describe::projected_columns(
                        &projection,
                        &schema,
                    )))
                }
            }
            // `BEGIN`／`COMMIT`／`ROLLBACK`（SQL-31・TASK-221）は結果列を持たない
            // （`CommandComplete` のみ）。
            ParsedSql::Transaction(_) => Ok(None),
            // WIRE-15・TASK-218: `DECLARE`／`CLOSE` は `CommandComplete` のみを
            // 返すため結果列を持たない。`FETCH` は本来カーソルの列メタデータを
            // 持つが、本メソッド（トランザクション文脈を持たない Describe。
            // [`Self::describe_parsed_in_session`]・[`Self::
            // describe_prepared_in_session`] が使う）はどのカーソルが開いて
            // いるかを知り得ないため `None` を返す（fail-safe。実際のカーソル
            // Describe は [`Self::describe_parsed_in_txn`] が担う）。
            ParsedSql::Cursor(_) => Ok(None),
        }
    }

    /// [`Self::execute_sql_in_session`] の実行本体。`validate_sql` 済みの
    /// [`crate::sql::allowlist::Statement`] を受け取ることで、呼び出し元
    /// （[`Self::execute_sql`] の `Select`／`Aggregate` アーム）が既に構文解析
    /// 済みの `Statement` を持つ場合に同一 SQL 文字列の再パースを避けられる
    /// （Issue #314・SQL-1・TASK-83 条件7: SQL 表層 C1 経路の固定コスト削減。
    /// `execute_sql_in_session` 自身（`sql: &str` を受け取る公開 API）は本メソッド
    /// より前に `INSERT` 判定・`validate_sql` を済ませてから委譲するため、挙動・
    /// エラー契約は分割前と不変）。
    fn execute_validated_in_session(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        stmt: crate::sql::allowlist::Statement,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        match stmt {
            crate::sql::allowlist::Statement::SetSearchMode { value } => {
                let mode = crate::sql::mode::SearchMode::parse_literal(&value)?;
                session.set_search_mode(mode);
                Ok(crate::sql::SqlOutcome::SetSearchMode(mode))
            }
            // TASK-79（SQL-9）: `CREATE FUNCTION` の検証・登録は
            // `sql::udf_call::define_function` に委譲する（検証→登録の順を守り、
            // 失敗時は `session.udfs()` を一切変更しない。`SET search_mode` と同じ
            // 「部分更新＝黙った既定化を防ぐ」方針）。
            crate::sql::allowlist::Statement::CreateFunction { name, params, body } => {
                crate::sql::udf_call::define_function(session.udfs_mut(), &name, &params, &body)?;
                Ok(crate::sql::SqlOutcome::CreateFunction { name })
            }
            crate::sql::allowlist::Statement::Select(validated) => {
                // TASK-77（SQL-5）: `USING PLAN('<query>')` は `ORDER BY` の代替
                // （相互排他）のため、`validated.using_plan()` の有無で束縛経路を
                // 分岐する。この経路は `sql::parser::bind_in_session` を呼ばない
                // （`bind_ranking` が `OrderByForm::UsingPlan` を防御的に拒否する
                // ことと対になる：正規の分岐は必ずここで行われる）。
                //
                // `USING PLAN` 経路は `plan_using_plan_expansion` 内で LLM 呼び出し・
                // 再埋め込み（`plan_query`・`Embedder::embed_batch`）という
                // 長時間の I/O を行う。この I/O の間、`execute_statement` 用の
                // `read_txn` を保持し続けると、redb のスナップショットが I/O 完了
                // まで固定され続けページ回収不能になるうえ、`plan_query` が内部で
                // 取得する `dictionary_snapshot` が後続のハイブリッドスキャンと
                // 異なる世代のデータを読む可能性がある（Cursor Bugbot Medium
                // 指摘対応）。そのため I/O 自体（`plan_using_plan_expansion`）は
                // スキーマに依存しない形（展開結果と再埋め込み済み query vector の
                // 生成のみ）で `read_txn` を開かずに完了させ、I/O 完了後に
                // `execute_statement` 用の `read_txn` とスキーマを新規に取得し、
                // その同一スナップショット上で `sql::using_plan::bind_expansion`
                // （列インデックス解決を含む束縛）を行ってから `execute_statement`
                // へ渡す（codex-review P1 指摘対応: 以前は I/O 前に取得した旧
                // スキーマで列インデックスを含む `BoundStatement` を確定し、I/O 後に
                // 取得し直した最新スキーマとの一致検証なしに旧束縛を
                // `execute_statement` へ渡していたため、I/O 中に同名テーブルの
                // `DROP TABLE`→再作成等でレイアウトが変わると、束縛済みの列
                // インデックスが別の列を指す状態になり得た。他経路と同じく
                // 列インデックス解決を含む束縛〜`execute_statement` は単一
                // スナップショットへ閉じ込める）。
                //
                // 不変条件の防御的検証（codex-review P1 指摘対応、PR #266）:
                // `order_by` が [`crate::sql::allowlist::OrderByForm::UsingPlan`] で
                // あることと `using_plan()` が `Some` であることは本来一対一で
                // 対応する必要がある（`sql::allowlist::ValidatedStatement::
                // with_using_plan` のドキュメント参照）。`validate_sql` の内部
                // パーサーは常に両者を揃えて構築するが、`ValidatedStatement::new`
                // （`order_by` を無検証で受け取る公開 constructor）経由で外部から
                // `order_by == UsingPlan` かつ `using_plan() == None` の矛盾した
                // 値を構築される可能性を完全には排除できないため、分岐前に
                // ここで検証し、矛盾していれば内部エラーとして拒否する
                // （fail-closed。到達は公開 API の誤用時のみの防御的経路）。
                let order_by_is_using_plan = matches!(
                    validated.order_by(),
                    crate::sql::allowlist::OrderByForm::UsingPlan
                );
                if order_by_is_using_plan != validated.using_plan().is_some() {
                    return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "inconsistent USING PLAN state: order_by and using_plan disagree"
                            .to_string(),
                    });
                }
                if let Some(question) = validated.using_plan() {
                    // `USING MODE` リテラルの解析は `run_using_plan_select` に
                    // 委譲し、ここでは行わない（cursor[bot] 指摘対応・PR #827）。
                    // 以前はここで `SearchMode::parse_literal` を先に解析して
                    // いたため、テーブル未存在（`42P01` 相当）と `USING MODE`
                    // 値の不正（`22000`）が同時に成立する要求で、テーブル解決
                    // より先に mode 解析エラーが確定してしまっていた。
                    // `run_using_plan_select` はテーブル解決（`read_txn_with_
                    // schema`）を終えた後の `pre_check`（`sql::using_plan::
                    // pre_check_bindable` が同一リテラルを再検証する）で初めて
                    // mode リテラルを解析するため、生リテラルをそのまま渡す
                    // ことで両表層（SQL テキスト経由・束縛済み計画経由）が
                    // 共有する fail-closed 順序〔LIMIT → テーブル解決 → 事前
                    // 束縛検証（mode 解析含む） → 辞書列検証 → I/O（LLM 展開・
                    // 再埋め込み） → 世代照合 → 再検証 → 束縛〕を保つ
                    // （辞書列検証を事前束縛検証より後に行う順序は
                    // codex-review P1 指摘対応・PR #828）。詳細は
                    // [`Self::run_using_plan_select`] のドキュメント参照）。
                    let result = self.run_using_plan_select(
                        ctx,
                        session,
                        &validated.table_name,
                        question,
                        validated.search_mode(),
                        validated.limit(),
                        |schema, udfs| {
                            crate::sql::using_plan::pre_check_bindable(&validated, schema, udfs)
                                .map(|_| ())
                        },
                        |schema, udfs, planned| {
                            crate::sql::using_plan::bind_expansion(
                                &validated,
                                schema,
                                question,
                                &planned.expansion,
                                planned.query_vector,
                                udfs,
                                planned.resolved_mode,
                            )
                        },
                    )?;
                    Ok(crate::sql::SqlOutcome::Query(result))
                } else {
                    let (read_txn, schema) = self.read_txn_with_schema(&validated.table_name)?;
                    let bound = crate::sql::parser::bind_in_session(
                        &validated,
                        &schema,
                        session.search_mode(),
                        session.udfs(),
                    )?;
                    let result = self.run_select_plan(&read_txn, ctx, &schema, &bound)?;
                    Ok(crate::sql::SqlOutcome::Query(result))
                }
            }
            // TASK-166（SQL-13）: 集計 SELECT はスキーマ取得（`bind_aggregate` 用）・
            // 行走査（`sql::aggregate::execute_aggregate`）を、既存の検索 SELECT
            // （`Statement::Select` アーム）と同じく単一の `read_txn`（同一
            // スナップショット）上で行う（Issue #56 レビュー指摘対応の踏襲。上記
            // `Statement::Select` アームのドキュメント参照）。トランザクション・
            // スキーマ取得は [`Self::read_txn_with_schema`] を、実行本体は
            // [`Self::run_aggregate_plan`] を共有する（TASK-186・NOSQL-4・NOSQL-5:
            // [`Self::execute_bound_aggregate_in_session`] が同じ実行本体を束縛済み
            // 計画向けに再利用する）。
            crate::sql::allowlist::Statement::Aggregate(validated) => {
                let (read_txn, schema) = self.read_txn_with_schema(&validated.table_name)?;
                let bound =
                    crate::sql::parser::bind_aggregate(&validated, &schema, session.udfs())?;
                let result = self.run_aggregate_plan(
                    &read_txn,
                    ctx,
                    &schema,
                    &bound,
                    crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES,
                )?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // Issue #454: 広域取得（ソートなしのフィルタ取得）は `Statement::Aggregate`
            // アームと同じく、スキーマ取得（`bind_scan` 用）・行走査
            // （`sql::scan::execute_scan`）を単一の `read_txn`（同一スナップショット）
            // 上で行う。`VectorArena`（既存の検索 SELECT 実行経路）は経由しない
            // （`VECTOR` 列を持たないテーブルでも動作させるため。`sql::scan`
            // モジュールドキュメント参照）。トランザクション・スキーマ取得は
            // [`Self::read_txn_with_schema`] を、実行本体は [`Self::run_scan_plan`]
            // を共有する（TASK-186・NOSQL-3: [`Self::execute_bound_scan_in_session`]
            // が同じ実行本体を束縛済み計画向けに再利用する）。
            crate::sql::allowlist::Statement::Scan(validated) => {
                let (read_txn, schema) = self.read_txn_with_schema(&validated.table_name)?;
                let bound = crate::sql::parser::bind_scan(&validated, &schema, session.udfs())?;
                let result = self.run_scan_plan(&read_txn, ctx, &schema, &bound)?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // TASK-78（SQL-6）・Issue #922（SQL-27）: `EXPLAIN` は対象文（検索・
            // 集計・広域取得のいずれも）の本体（ハイブリッド実行・行走査・
            // キャッシュ消費）を一切実行しない。`ExplainTarget` の variant ごとに
            // 異なる静的分類・応答整形へ振り分ける（[`crate::sql::explain`]
            // モジュールドキュメント参照）。
            crate::sql::allowlist::Statement::Explain(target) => match target {
                // `USING PLAN` を伴う検索 SELECT（TASK-78・SQL-6）は既存の
                // LLM クエリ展開・モード解決を可視化する経路をそのまま維持する
                // （行の形式・fail-closed 順序は不変。詳細手順は
                // [`Self::run_explain_plan`] のドキュメント参照）。行うのは
                // LIMIT 範囲検証 → テーブル解決 → `VECTOR` 列・投影列／`WHERE`
                // 述語の事前束縛検証（[`crate::sql::using_plan::
                // pre_check_bindable`]。PR #267 の是正対応）→ `question`
                // （`USING PLAN` 本文）の空文字・長さ上限検証（束縛検証より後。
                // codex-review P1 指摘対応・PR #828）→ 辞書必須列（`path`/
                // `body`）の事前スキーマ検証（`question` 検証より後。同 PR）→
                // `USING MODE` リテラルの解析（辞書必須列検証より後）→ LLM
                // クエリ展開・モード解決（`Self::plan_query_with_mode`）まで
                // で、すべての拒否を LLM I/O 開始前に完結させる
                // （`Statement::Select` アームの `USING PLAN` 経路〔PR #266・
                // #267 の是正方針〕を踏襲。security.md「不安全な設計」対応）。
                // 再埋め込み（`Embedder`）は応答に不要なため呼ばない
                // （`embedder` 未注入でも `EXPLAIN` 可能）。
                crate::sql::allowlist::ExplainTarget::Search(validated)
                    if validated.using_plan().is_some() =>
                {
                    // `using_plan().is_some()` を確認済みのため `ok_or_else` は
                    // 到達しない防御的経路（構文段の [`ExplainTarget::Search`]
                    // 構築規則が破られない限り）。
                    let question = validated.using_plan().ok_or_else(|| {
                        crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "EXPLAIN statement missing USING PLAN question".to_string(),
                        }
                    })?;

                    crate::sql::parser::validate_search_limit(validated.limit())?;

                    // `USING MODE` リテラルの解析は `run_explain_plan` に委譲し、
                    // ここでは行わない（cursor[bot] Bugbot 指摘対応・Issue #765
                    // 後続。`Statement::Select` アームの `USING PLAN` 経路
                    // 〔`run_using_plan_select`。PR #827〕と同じ理由: ここで先に
                    // 解析すると、テーブル未存在（`42P01` 相当）と `USING MODE`
                    // 値の不正（`22000`）が同時に成立する要求で、テーブル解決
                    // より先に mode 解析エラーが確定してしまう。
                    // `run_explain_plan` はテーブル解決（`read_txn_with_schema`）
                    // を終えた後で初めて mode リテラルを解析するため、生
                    // リテラルをそのまま渡すことで両表層（SQL テキスト経由・
                    // 束縛済み計画経由）が共有する fail-closed 順序〔LIMIT →
                    // テーブル解決 → 束縛検証（`plan` 欠落判定を含む） →
                    // `question` 検証 → 辞書必須列検証 → mode 解析 →
                    // I/O（LLM 展開・再埋め込み）→ 世代照合 → 辞書必須列の
                    // 再検証〕を保つ。詳細は [`Self::run_explain_plan`] の
                    // ドキュメント参照）。
                    //
                    // 手順本体（テーブル世代の事前記録 → 束縛検証（`plan`
                    // 欠落判定を含む） → `question` 検証 → 辞書必須列検証 →
                    // mode 解析 → LLM クエリ展開・モード解決 → 世代の事後照合 →
                    // 辞書必須列の再検証 → 使用エンジン・ANN／SCALAR 静的判定 →
                    // `QUERY PLAN` 整形）は [`Self::run_explain_plan`] が
                    // [`Self::explain_bound_plan_in_session`]（TASK-186・
                    // NOSQL-10・Issue #765）と共有する（第 2 の実装を持たない）。
                    let result = self.run_explain_plan(
                        ctx,
                        session,
                        validated.table_name(),
                        question,
                        validated.search_mode(),
                        |schema, udfs| {
                            crate::sql::using_plan::pre_check_bindable(&validated, schema, udfs)
                        },
                    )?;
                    Ok(crate::sql::SqlOutcome::Explain(result))
                }
                // Issue #922（SQL-27）: `USING PLAN` を伴わない検索 SELECT
                // （`ORDER BY <=>`・`HYBRID`）。LLM I/O を一切行わず、
                // `Statement::Select` アームの非 `USING PLAN` 経路と同じ
                // `bind_in_session` の束縛結果だけから ANN／SCALAR の静的判定を
                // 報告する（[`Self::run_search_explain`] 参照）。
                crate::sql::allowlist::ExplainTarget::Search(validated) => {
                    self.run_search_explain(&validated, session)
                }
                // Issue #922（SQL-27）: 集計 SELECT（`GROUP BY`・`DISTINCT` の
                // 脱糖形いずれも）。行走査・索引消費を一切行わず、`bind_aggregate`
                // の束縛結果だけから走査方式を報告する
                // （[`Self::run_relational_explain_aggregate`] 参照）。
                crate::sql::allowlist::ExplainTarget::Aggregate(validated) => {
                    self.run_relational_explain_aggregate(&validated, session)
                }
                // Issue #922（SQL-27）: 広域取得（ビュー展開後の形・`OFFSET` を
                // 含む）。`sql::scan` はランキング段・索引を持たないため、
                // `scalar_plan: plain_scan`／`access_path: full_scan` に固定
                // （[`Self::run_relational_explain_scan`] 参照）。
                crate::sql::allowlist::ExplainTarget::Scan(validated) => {
                    self.run_relational_explain_scan(&validated, session)
                }
            },
        }
    }

    /// [`Self::execute_validated_in_session`] の `Statement::Select` アーム
    /// （`USING PLAN` 経由を含む）・[`Self::execute_bound_search_in_session`]・
    /// [`Self::run_using_plan_select`] が共有する実行本体（TASK-186・
    /// NOSQL-2。Issue #764）。`read_txn`・`schema` は呼び出し元が同一
    /// スナップショットから取得済みのものをそのまま渡す。SQL 経路専用の
    /// キャッシュ（`SparseIndexCache`〔Issue #357〕・`SqlArenaCache`
    /// 〔Issue #363〕・`HnswIndexCache`〔Issue #408〕・`ScalarIndexCache`
    /// 〔Issue #473〕）を配線した `execute_statement_with_cache` を経由する
    /// ことで、束縛済み計画エントリも SQL テキスト経由と同一のキャッシュ
    /// 最適化・ANN opt-in・precision fail-closed 契約（SEARCH-9）を受ける。
    fn run_select_plan(
        &self,
        read_txn: &redb::ReadTransaction,
        ctx: &PolicyContext,
        schema: &crate::catalog::TableSchema,
        bound: &crate::sql::parser::BoundStatement,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::exec::execute_statement_with_cache(
            read_txn,
            self.provider.as_ref(),
            ctx,
            schema,
            bound,
            &self.precision_policy,
            Some(crate::sql::sparse_cache::SparseCacheAccess {
                storage: &self.storage,
                cache: &self.sparse_index_cache,
            }),
            Some(crate::sql::arena_cache::ArenaCacheAccess {
                storage: &self.storage,
                cache: &self.sql_arena_cache,
            }),
            // Issue #408: `SearchEngineKind::Hnsw` opt-in 構築時のみ
            // `Some`（`hnsw_state`）。フィルタなし `Ranking::Distance` の
            // DISTANCE 段を索引済みノード＋未索引分 brute-force の併用へ
            // 載せ替える（詳細は `sql::hnsw_cache` のドキュメント参照）。
            self.hnsw_state
                .as_ref()
                .map(|s| crate::sql::hnsw_cache::HnswCacheAccess {
                    storage: &self.storage,
                    cache: &s.cache,
                    provider: s.provider,
                }),
            // Issue #473: スカラー列二次索引の gated 構築（応答には未使用。
            // 詳細は `sql::scalar_index` のドキュメント参照）。
            Some(crate::sql::scalar_index::ScalarCacheAccess {
                storage: &self.storage,
                cache: &self.scalar_index_cache,
            }),
        )
    }

    /// `USING PLAN('<query>')`（TASK-77・SQL-5）経路の共有骨格（TASK-186・
    /// NOSQL-2。Issue #764）。[`Self::execute_validated_in_session`] の
    /// `Statement::Select` アーム（SQL テキスト経由）・
    /// [`Self::execute_bound_plan_search_in_session`]（NoSQL 表層の束縛済み
    /// `plan` 検索。`wire-server::http::query::search`）の双方が、この
    /// メソッドへ委譲することで fail-closed 判定順序（`LIMIT` 範囲検証 →
    /// 計画開始時のテーブル世代記録 → `pre_check`（呼び出し元提供のスキーマ
    /// 依存事前束縛検証）→ 辞書必須列 `path`/`body` の事前スキーマ検証
    /// （`pre_check` より後。codex-review P1 指摘対応・PR #828。[`Self::
    /// run_explain_plan`] と同一の優先順位）→ I/O（`plan_using_plan_
    /// expansion`。LLM クエリ展開・再埋め込み）→ I/O 完了後の世代照合 →
    /// 辞書必須列の再検証 → `bind`（呼び出し元提供の最終束縛）→ [`Self::
    /// run_select_plan`] による実行、という一連の手順を複製しない（第 2 の
    /// 実行器を作らない設計。`docs/design/bound-plan-session-entry.md`
    /// 参照）。
    ///
    /// `pre_check` は I/O（LLM 呼び出し・再埋め込み）より前にスキーマ依存の
    /// 検証（`USING MODE` リテラル・`VECTOR` 列の存在・投影列／`WHERE` 述語の
    /// 事前束縛可能性）を行い、戻り値（`Result<(), _>`）は捨てる（呼び出し元
    /// が必要な形状情報を自分で保持する。SQL アームは
    /// [`crate::sql::using_plan::pre_check_bindable`] をそのまま委譲する）。
    /// `bind` は I/O 完了後に取得し直した最新スキーマ上で最終
    /// [`crate::sql::parser::BoundStatement`] を組み立てる（SQL アームは
    /// [`crate::sql::using_plan::bind_expansion`] を委譲する）。
    ///
    /// `pre_check`・`bind` はいずれも `read_txn` が開いている間（`pre_check`
    /// は計画開始時のスキーマ取得直後、`bind` は I/O 完了後の再取得直後）に
    /// 呼ばれる契約であり、[`Self::execute_bound_scan_in_session`] と同じ
    /// 理由で重い処理（I/O・LLM 呼び出し）を closure 内で行わないこと。
    #[allow(clippy::too_many_arguments)]
    fn run_using_plan_select<Pre, Bind>(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        question: &str,
        mode_literal: Option<&str>,
        limit: u32,
        pre_check: Pre,
        bind: Bind,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError>
    where
        Pre: FnOnce(
            &crate::catalog::TableSchema,
            &crate::sql::udf_call::UdfRegistry,
        ) -> Result<(), crate::sql::allowlist::SqlSurfaceError>,
        Bind: FnOnce(
            &crate::catalog::TableSchema,
            &crate::sql::udf_call::UdfRegistry,
            UsingPlanExpansionResult,
        ) -> Result<
            crate::sql::parser::BoundStatement,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        // `LIMIT` の範囲検証（codex-review P1 指摘対応、PR #266 の踏襲）:
        // 高コスト処理・DB I/O より前に完結させる（untrusted 入力による
        // リソース増幅の防止。`bind` 側の検証は多層防御として残る）。
        crate::sql::parser::validate_search_limit(limit)?;

        // I/O（LLM 呼び出し）前のスキーマ取得。計画開始時のテーブル世代も
        // ここで記録する（対象テーブル限定化の理由は
        // [`Self::plan_using_plan_expansion`] 呼び出し元の既存ドキュメント
        // 〔`docs/design/table-generation-rejection-granularity.md`〕参照）。
        let (pre_check_schema, planning_generation) = {
            let (pre_check_txn, schema) = self.read_txn_with_schema(table)?;
            let generation = crate::catalog::table_generation_in_txn(&pre_check_txn, table)
                .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: format!("failed to read table generation: {e}"),
                })?;
            drop(pre_check_txn);
            (schema, generation)
        };

        // 呼び出し元提供のスキーマ依存事前束縛検証（`USING MODE` リテラル・
        // `VECTOR` 列・投影列／`WHERE` 述語）を辞書必須列検証より先に行う
        // （codex-review P1 指摘対応・PR #828。[`Self::run_explain_plan`]
        // と同一の理由・同一の優先順位——`pre_check` が返し得る `42601`／
        // `22000`／`54000` 系エラーが、辞書必須列を欠くテーブルに対しても
        // `Self::run_explain_plan` 側と一致した優先順位で確定するよう揃える。
        // 詳細は [`Self::run_explain_plan`] のドキュメント参照）。I/O より
        // 前に完結させる。
        pre_check(&pre_check_schema, session.udfs())?;

        // 辞書必須列（`path`/`body`）の検証は `pre_check`（上記）より後に
        // 行う（codex-review P1 指摘対応・PR #828）。
        dictionary_required_columns(&pre_check_schema)
            .map_err(crate::sql::allowlist::SqlSurfaceError::invalid_input)?;

        // `USING MODE` リテラルの解析はテーブル解決（上記 `read_txn_with_
        // schema`）より後で行う（cursor[bot] 指摘対応・PR #827。テーブル
        // 未存在＋ mode リテラル不正の要求で `42P01` が `22000` より優先
        // される既存の fail-closed 順序契約を維持する）。SQL テキスト経由
        // では `pre_check`（`sql::using_plan::pre_check_bindable`）が同一
        // リテラルを既に検証済みのため、ここでの解析結果は成功している
        // ことが保証される。
        let query_mode = match mode_literal {
            Some(literal) => Some(crate::sql::mode::SearchMode::parse_literal(literal)?),
            None => None,
        };

        let planned = self.plan_using_plan_expansion(ctx, session, table, query_mode, question)?;
        let (read_txn, schema) = self.read_txn_with_schema(table)?;

        // I/O 完了後の世代照合（対象テーブル限定化の理由は上記・
        // [`Self::plan_using_plan_expansion`] 呼び出し元の既存ドキュメント参照）。
        let current_generation = crate::catalog::table_generation_in_txn(&read_txn, table)
            .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!("failed to read table generation: {e}"),
            })?;
        if current_generation != planning_generation {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "table generation changed during USING PLAN query \
                         expansion; rejecting stale plan"
                    .to_string(),
            });
        }

        // I/O 完了後の最新スキーマにも辞書必須列の検証を再適用する（多層防御。
        // 理由は上記呼び出し元の既存ドキュメント参照）。
        dictionary_required_columns(&schema)
            .map_err(crate::sql::allowlist::SqlSurfaceError::invalid_input)?;

        let bound = bind(&schema, session.udfs(), planned)?;
        self.run_select_plan(&read_txn, ctx, &schema, &bound)
    }

    /// 束縛済み検索計画（[`crate::sql::parser::BoundStatement`]）を単一
    /// スナップショット上で実行する（TASK-186・NOSQL-2。Issue #764）。SQL
    /// テキストを経由せず束縛済み計画を直接実行したい呼び出し元（`wire-server`
    /// の NoSQL 表層 `search` op の `vector` 指定。TASK-175）向けのセッション
    /// 対応エントリで、単一の `Storage` を `EngineCore` が所有したまま
    /// [`Self::execute_sql_in_session`] の `Statement::Select` アーム（`ORDER
    /// BY` 経由。`USING PLAN` は対象外——[`Self::execute_bound_plan_search_in_session`]
    /// が別に担う）と同一のトランザクション・スキーマ・実行本体
    /// （[`Self::run_select_plan`]）を共有する（第 2 の実行器を作らない設計。
    /// `docs/design/bound-plan-session-entry.md` 参照）。
    ///
    /// 処理順は [`Self::execute_bound_scan_in_session`] と同一（(1) スキーマ
    /// 取得、(2) `bind(&schema, udfs)` で束縛、(3) 対象テーブル一致検証、
    /// (4) 実行）。`bind` は `read_txn` が開いている間に呼ばれるため、
    /// closure は純粋・軽量に保つこと。`mode`（`precision`／`recall`）は
    /// `bind` が返す [`crate::sql::parser::BoundStatement::mode`] にすでに
    /// 解決済みの値として含まれている必要がある（NoSQL 表層には `SET
    /// search_mode` 相当が無いため、呼び出し元が `mode::resolve_mode` 等で
    /// 解決してから束縛する。`wire-server::http::query::search::bind_search`
    /// 参照）。RLS 暗黙適用（RLS-7）は [`Self::run_select_plan`] が経由する
    /// `execute_statement_with_cache` 側で行われ、本メソッド自身はテナント
    /// 判定を一切行わない。
    pub fn execute_bound_search_in_session<F>(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        bind: F,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
            &crate::sql::udf_call::UdfRegistry,
        ) -> Result<
            crate::sql::parser::BoundStatement,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let bound = bind(&schema, session.udfs())?;
        if bound.table() != table {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "bound search plan targets a different table than requested",
            ));
        }
        self.run_select_plan(&read_txn, ctx, &schema, &bound)
    }

    /// 束縛済み `plan` 検索（NoSQL 表層 `search` op の `plan` 指定。TASK-175・
    /// NOSQL-2。Issue #764）を、SQL 表層の `USING PLAN('<query>')`
    /// （TASK-77・SQL-5）と同一の fail-closed 判定順序・LLM 展開・再埋め込み・
    /// RLS 暗黙適用（RLS-7）・`precision` fail-closed 契約（SEARCH-9）で実行
    /// する。[`Self::run_using_plan_select`] へ委譲し、`bind` closure が
    /// [`PlanSearchBinding`]（投影・フィルタのみ）を返す点だけが
    /// [`crate::sql::using_plan::bind_expansion`] と異なる——`Ranking::
    /// Hybrid`（再埋め込みベクトル・本文列インデックス・展開後クエリ文字列）
    /// ・`mode` は本メソッドが `planned`（[`UsingPlanExpansionResult`]）から
    /// 直接組み立て、NoSQL 表層からは差し替えられない。
    ///
    /// `bind` は `Fn`（`FnOnce` ではない）: [`Self::run_using_plan_select`]
    /// の `pre_check`（I/O 前）・`bind`（I/O 後）の双方から同一の束縛ロジック
    /// を 2 回呼ぶため（`pre_check` は投影・フィルタが対象スキーマ上で
    /// 束縛可能かどうかだけを確認し、戻り値は捨てる。I/O 前拒否のための
    /// 多層防御。`bind`〔本体〕はその戻り値を実際に使う）。`bind` が I/O
    /// 前後で異なる結果を返す（非決定的）場合でも、I/O 完了後の呼び出し
    /// 結果のみが実行に使われる契約（`pre_check` 側の戻り値は捨てるため）。
    ///
    /// `body_column_index`（本文列規約）は
    /// [`crate::sql::using_plan::body_column_index`] を共有する
    /// （`sql::using_plan::bind_expansion` と同一の列インデックス解決。
    /// 第 2 の実装を作らない）。
    #[allow(clippy::too_many_arguments)]
    pub fn execute_bound_plan_search_in_session<F>(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        question: &str,
        mode_literal: Option<&str>,
        limit: u32,
        bind: F,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError>
    where
        F: Fn(
            &crate::catalog::TableSchema,
            &crate::sql::udf_call::UdfRegistry,
        ) -> Result<PlanSearchBinding, crate::sql::allowlist::SqlSurfaceError>,
    {
        // `LIMIT`・`question` の多層防御（`Self::run_using_plan_select` 側の
        // 検証に加え、engine 側でも独立に通す。NoSQL 表層の `search.rs::
        // bind_search` が既に同じ検証を通しているが、本メソッドが engine の
        // 公開 API として単独呼び出しされる場合の fail-closed 契約を維持）。
        // `mode_literal`（`USING MODE` 相当の生リテラル）はここでは解析しない
        // （cursor[bot] 指摘対応・PR #827）。`Self::run_using_plan_select` が
        // テーブル解決後に初めて解析することで、テーブル未存在＋ mode 値不正の
        // 要求で `42P01` が優先される SQL テキスト経由と同一の fail-closed
        // 順序を保つ。
        let validated_limit = crate::sql::parser::validate_search_limit(limit)?;
        crate::sql::allowlist::validate_using_plan_question(question)?;

        self.run_using_plan_select(
            ctx,
            session,
            table,
            question,
            mode_literal,
            limit,
            |schema, udfs| {
                // `VECTOR` 列の存在は LLM 展開・再埋め込み（高コスト I/O）より
                // 前に確定させる（`sql::using_plan::pre_check_bindable` と同じ
                // fail-closed 順序。codex-review 指摘対応・PR #827。`VECTOR`
                // 列を持たないテーブルへの `vector` 指定検索が、プランナー・
                // 埋め込みのコストを消費したうえで拒否される増幅を防ぐ）。
                crate::sql::parser::vector_column(schema)?;
                bind(schema, udfs).map(|_| ())
            },
            |schema, udfs, planned| {
                let parts = bind(schema, udfs)?;
                let text_column_index = crate::sql::using_plan::body_column_index(schema)?;
                let query_text =
                    crate::sql::using_plan::expanded_query_text(question, &planned.expansion);
                let (_, vec_dim) = crate::sql::parser::vector_column(schema)?;
                let got_dim = u32::try_from(planned.query_vector.len()).map_err(|_| {
                    crate::sql::allowlist::SqlSurfaceError::invalid_input(format!(
                        "USING PLAN re-embedded vector length {} exceeds representable range",
                        planned.query_vector.len()
                    ))
                })?;
                if got_dim != vec_dim {
                    return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(format!(
                        "USING PLAN re-embedded vector dimension mismatch: expected {vec_dim}, got {got_dim}"
                    )));
                }
                if planned.query_vector.iter().any(|v| !v.is_finite()) {
                    return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                        "USING PLAN re-embedded vector must not contain NaN/Inf".to_string(),
                    ));
                }
                Ok(crate::sql::parser::BoundStatement {
                    table: table.to_string(),
                    projection: parts.projection,
                    metadata_filters: parts.metadata_filters,
                    rls_predicate_present: false,
                    expr_filters: Vec::new(),
                    expr_filter_programs: Vec::new(),
                    // NoSQL 表層の `plan` 検索経路は `OR` 未対応
                    // （TASK-208・Issue #912。計画§「対象外」参照）。
                    or_filters: Vec::new(),
                    ranking: crate::sql::parser::Ranking::Hybrid {
                        query: planned.query_vector,
                        text_column_index,
                        query_text,
                    },
                    limit: validated_limit,
                    mode: planned.resolved_mode,
                    evaluation_order: crate::sql::plan::EvaluationOrder::DEFAULT,
                })
            },
        )
    }

    /// [`Self::execute_validated_in_session`] の `Statement::Scan` アーム・
    /// [`Self::execute_bound_scan_in_session`] が共有する実行本体（TASK-186・
    /// NOSQL-3）。`read_txn`・`schema` は呼び出し元が同一スナップショットから
    /// 取得済みのものをそのまま渡す（`sql::scan::execute_scan` への単純な委譲。
    /// `VectorArena` を経由しない redb 直接走査であることは `sql::scan`
    /// モジュールドキュメント参照）。
    fn run_scan_plan(
        &self,
        read_txn: &redb::ReadTransaction,
        ctx: &PolicyContext,
        schema: &crate::catalog::TableSchema,
        bound: &crate::sql::parser::BoundScan,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::scan::execute_scan(read_txn, ctx, schema, bound)
    }

    /// [`Self::execute_validated_in_session`] の `Statement::Aggregate` アーム・
    /// [`Self::execute_bound_aggregate_in_session`] が共有する実行本体
    /// （TASK-186・NOSQL-4・NOSQL-5）。`read_txn`・`schema` は呼び出し元が同一
    /// スナップショットから取得済みのものをそのまま渡す。SQL 経路専用の
    /// キャッシュ（`VisibleBitmapCache`〔Issue #478〕・`SqlArenaCache`
    /// 〔Issue #363〕・`ScalarIndexCache`〔Issue #473〕）を配線した
    /// `execute_aggregate_with_cache` を経由することで、束縛済み計画エントリ
    /// （[`Self::execute_bound_aggregate_in_session`]）も SQL テキスト経由と
    /// 同一のキャッシュ最適化を受ける（`sql::aggregate::execute_aggregate`
    /// 〔公開ラッパー・キャッシュ非経由〕とは意図的に差別化する）。
    ///
    /// `max_result_bytes`（PR #1049 レビュー指摘 P0 対応）: 通常の（カーソル
    /// 非経由の）呼び出し元は [`crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES`]
    /// （既存挙動を変えない大きな既定値）を渡す。[`Self::execute_cursor_inner_query`]
    /// だけが `DECLARE` の内側実行専用により小さい
    /// [`crate::sql::cursor::MAX_CURSOR_BYTES_PER_SESSION`] を渡す
    /// （[`crate::sql::scan::execute_scan_with_budget`] と同じ設計判断）。
    fn run_aggregate_plan(
        &self,
        read_txn: &redb::ReadTransaction,
        ctx: &PolicyContext,
        schema: &crate::catalog::TableSchema,
        bound: &crate::sql::parser::BoundAggregate,
        max_result_bytes: usize,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::aggregate::execute_aggregate_with_cache(
            read_txn,
            ctx,
            schema,
            bound,
            max_result_bytes,
            Some(crate::sql::visible_cache::VisibleCacheAccess {
                storage: &self.storage,
                cache: &self.visible_bitmap_cache,
            }),
            Some(crate::sql::arena_cache::ArenaCacheAccess {
                storage: &self.storage,
                cache: &self.sql_arena_cache,
            }),
            Some(crate::sql::scalar_index::ScalarCacheAccess {
                storage: &self.storage,
                cache: &self.scalar_index_cache,
            }),
        )
    }

    /// `EXPLAIN SELECT ... USING PLAN(...)`（SQL-6・TASK-78）の `Statement::
    /// Explain` アームと、束縛済み計画向けセッション対応エントリ
    /// [`Self::explain_bound_plan_in_session`]（TASK-186・NOSQL-10・
    /// Issue #765）が共有する実行本体。fail-closed の手順順序（テーブル
    /// 世代の事前記録 → 束縛検証（`plan` 欠落判定を含む。LLM I/O より前）→
    /// `question`（`USING PLAN` 本文）の空文字・長さ上限検証 → 辞書必須列
    /// 検証 → LLM クエリ展開・モード解決 → 世代の事後照合 → 辞書必須列の
    /// 再検証 → 使用エンジン・ANN／SCALAR 静的判定 → `QUERY PLAN` 整形。
    /// 束縛検証を辞書必須列検証より先に行う順序は codex-review P1 指摘対応・
    /// PR #828、`question` 検証を束縛検証の直後・辞書必須列検証より前に置く
    /// 順序は同 PR の追加 P1 指摘対応）はどちらの呼び出し元でも
    /// ビット同一に保たれる（第 2 の実装を持たない）。
    /// 検索本体（`hnsw_state` の `lookup`／`prepare_*`・
    /// `SearchProvider::search`）はいずれの経路でも呼ばれない（`EXPLAIN`
    /// は検索を実行しない契約。[`crate::sql::explain`] モジュールドキュメント
    /// 参照）。
    ///
    /// `bind` は LLM I/O（`plan_query_with_mode` 内）より前、テーブル世代の
    /// 事前記録直後に一度だけ呼ばれる。返す [`crate::sql::explain::
    /// ExplainShape`] は `WHERE` 述語の構造のみから決まる値であり、以降の
    /// LLM I/O・世代照合の影響を受けないため最後まで使い回してよい
    /// （[`crate::sql::using_plan::pre_check_bindable`] の既存契約と同じ）。
    ///
    /// `mode_literal`（`USING MODE` 相当の生リテラル）はテーブル解決
    /// （最初の `read_txn_with_schema`）・`bind` 呼び出しより後で初めて
    /// 解析する（cursor[bot] Bugbot 指摘対応・Issue #765 後続）。判定順序は
    /// 「未知テーブル（`42P01`）が `mode` 値不正（`22000`）より優先される」
    /// ことが契約であり、[`Self::run_using_plan_select`]（PR #827）と同一の
    /// 順序を `EXPLAIN` 経路でも保つ。加えて `bind` を `mode_literal` 解析
    /// より先に呼ぶことで、`plan` 欠落等 `bind` 自身が返す `42601` 系エラーが
    /// `mode` 値不正（`22000`）より優先される（`vector`／`plan` の排他判定を
    /// `mode` 解析より先に行う `search::bind_search` と同一の優先順位。
    /// Cursor Bugbot 指摘対応）。呼び出し元（SQL `Statement::Explain` アーム・
    /// [`Self::explain_bound_plan_in_session`]）はいずれも解析前の生
    /// リテラルをそのまま渡すこと。
    ///
    /// エラー型 `E` は `E: From<SqlSurfaceError>` の境界のみを課す
    /// （呼び出し元ごとに異なるエラー型を返せるようにするため。SQL アーム
    /// 自身は blanket `impl<T> From<T> for T` により `E = SqlSurfaceError`
    /// のまま変更なく動く）。
    fn run_explain_plan<F, E>(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        question: &str,
        mode_literal: Option<&str>,
        bind: F,
    ) -> Result<crate::sql::exec::QueryResult, E>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
            &crate::sql::udf_call::UdfRegistry,
        ) -> Result<crate::sql::explain::ExplainShape, E>,
        E: From<crate::sql::allowlist::SqlSurfaceError>,
    {
        // 計画開始時の対象テーブル世代を記録する（`Statement::Select` アーム
        // の `USING PLAN` 経路と同じ理由: `plan_query_with_mode` 内の辞書
        // スナップショット構築・LLM クエリ展開の間に対象テーブルへの DDL・
        // 行書き込みが起きると、古いテーブル世代を前提にした偽の計画を返して
        // しまう。security.md「不安全な設計」対応）。
        let (pre_check_schema, planning_generation) = {
            let (pre_check_txn, schema) = self.read_txn_with_schema(table)?;
            let generation = crate::catalog::table_generation_in_txn(&pre_check_txn, table)
                .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: format!("failed to read table generation: {e}"),
                })?;
            drop(pre_check_txn);
            (schema, generation)
        };

        // `VECTOR` 列の存在・投影列／`WHERE` 述語の事前束縛検証（`plan`
        // 欠落判定を含む）を辞書必須列検証・LLM I/O（`plan_query_with_mode`）
        // より前に完結させる（`Statement::Select` アームの `USING PLAN`
        // 経路と同じ理由。加えて codex-review P1 指摘対応・PR #828:
        // `bind`（[`crate::http::query::explain::execute`] の closure）が
        // 返す `plan` 欠落エラー〔`42601`〕を辞書必須列検証〔`22000`〕より
        // 先に確定させることで、`table` が存在し辞書必須列を欠くテーブルに
        // 対して `plan` を省略した要求が、通常検索〔`search::bind_search`
        // の `VectorAndPlanBothMissing`＝`42601`〕と同じ `wire_code` を
        // 返す。`mode_literal` の解析（下記）より先にここで `bind` を呼ぶ
        // ことで、既存テーブル・`plan` 欠落・`mode` 値不正が同時に揃う要求
        // でも「`plan` 欠落（`42601`）が `mode` 値不正（`22000`）より優先
        // される」順序——`vector`／`plan` の排他判定を `mode` 解析より先に
        // 行う `search::bind_search`（`vector` 指定検索）と同一の優先順位
        // ——を保つ）。
        let explain_shape = bind(&pre_check_schema, session.udfs())?;

        // `question`（`USING PLAN` 相当の自然言語クエリ本文）の空文字・
        // 長さ上限検証は `bind`（`plan` 欠落判定を含む）より後、辞書必須列
        // 検証・LLM I/O（`plan_query_with_mode`）より前に行う（codex-review
        // P1 指摘対応・PR #828 継続。[`Self::execute_bound_plan_search_in_
        // session`] は呼び出し元（NoSQL 表層の `search.rs::execute`）が
        // 常に `plan` 実在確認後にのみ呼ぶため engine 側で無条件に検証できる
        // のに対し、[`Self::explain_bound_plan_in_session`] は `plan` 欠落時
        // にも呼び出し元（`http/query/explain.rs::execute`）がプレースホルダ
        // の空文字列を渡す設計（`bind` closure がテーブル解決後に初めて
        // `plan` 欠落を判定する）ため、`bind` を先に呼ぶことで両者を区別
        // する: `bind` が失敗する場合（`plan` 欠落＝プレースホルダ）は
        // ここへ到達せず既存の `42601` 優先順位を保ったまま、`bind` が成功
        // する場合（`plan` が実在——SQL `EXPLAIN` 経路は構文上常にこちら）
        // に限り、その実在する `question` を検証する。これにより
        // `explain_bound_plan_in_session` を NoSQL 表層の事前検証
        // （`http/query/explain.rs::execute` の `validate_using_plan_question`
        // 呼び出し）を経由せず直接呼ぶ経路でも、空文字列・64 KiB 超の
        // `question` で LLM 呼び出しまで進んでしまうことを防ぐ（engine 公開
        // API 単独でのエラー契約を通常検索〔`execute_bound_plan_search_in_
        // session`〕と揃える）。SQL `EXPLAIN` 経路は字句解析時点
        // （`Parser::parse_using_plan_clause`）で既に同じ検証を通過済みの
        // `question` を渡すため、本呼び出しは常に成功し挙動を変えない。
        crate::sql::allowlist::validate_using_plan_question(question)?;

        // 辞書必須列（`path`/`body`）の検証は `bind`（`plan` 欠落判定を含む）
        // より後に行う（codex-review P1 指摘対応・PR #828。SQL `EXPLAIN`
        // 経路〔`Statement::Explain` アーム〕は `question` が構文上必ず
        // `Some` のため `bind` が `plan` 欠落で失敗することはなく、本順序
        // 変更はその経路の挙動を変えない）。
        dictionary_required_columns(&pre_check_schema)
            .map_err(crate::sql::allowlist::SqlSurfaceError::invalid_input)?;

        // `mode_literal` の解析はテーブル解決（上記 `read_txn_with_schema`）
        // ・`bind`（上記）より後で行う（cursor[bot] Bugbot 指摘対応。上記
        // ドキュメント参照）。
        let query_mode = match mode_literal {
            Some(literal) => Some(crate::sql::mode::SearchMode::parse_literal(literal)?),
            None => None,
        };

        let planned = self
            .plan_query_with_mode(ctx, table, question, query_mode, session.search_mode())
            .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!("EXPLAIN query expansion failed: {e}"),
            })?;

        // I/O 完了後の世代照合: `planning_generation` と現在の対象テーブル
        // 世代が一致しなければ、計画時に使った辞書スナップショット・展開
        // 結果が現在のテーブル世代に対して有効である保証がないため
        // fail-closed に拒否する（クライアントへは `Internal::
        // client_message()` の固定の一般化メッセージのみを返し、他テナント・
        // 他クエリの書き込み有無という存在情報を漏らさない）。
        let (post_check_txn, post_check_schema) = self.read_txn_with_schema(table)?;
        let current_generation = crate::catalog::table_generation_in_txn(&post_check_txn, table)
            .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!("failed to read table generation: {e}"),
            })?;
        drop(post_check_txn);
        if current_generation != planning_generation {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "table generation changed during EXPLAIN USING PLAN query \
                         expansion; rejecting stale plan"
                    .to_string(),
            }
            .into());
        }

        // I/O 完了後の最新スキーマにも辞書必須列の検証を再適用する（多層防御。
        // `Statement::Select` アームの `USING PLAN` 経路と同じ理由）。
        dictionary_required_columns(&post_check_schema)
            .map_err(crate::sql::allowlist::SqlSurfaceError::invalid_input)?;

        // Issue #411: `engine:`／`hnsw_params:`／`ann_plan:` 行の入力を
        // 組み立てる。`EXPLAIN` は `USING PLAN` 専用（束縛結果は常に
        // `Ranking::Hybrid`）のため `is_hybrid` は常に `true`。`USING PLAN` は
        // `HINT ORDER` を受理しない（SQL-5・許可リスト層）ため評価順序は常に
        // 既定（`EvaluationOrder::DEFAULT`）であり、`scalar_prefilter` は
        // ここで固定的に導出できる。`explain_engine_for`（Issue #922・SQL-27
        // で `USING PLAN` なし検索 EXPLAIN と共有するために抽出）へ委譲する。
        let explain_engine = self.explain_engine_for(
            true,
            planned.mode().mode() == crate::sql::mode::SearchMode::Precision,
            explain_shape.filters_empty(),
            crate::sql::plan::ExecutionPlan::from_evaluation_order(
                crate::sql::plan::EvaluationOrder::DEFAULT,
            )
            .scalar_prefilter,
            // Issue #474: `bind` が構文段のみから確定させた静的判定（LLM
            // I/O・世代照合の影響を受けない。上記コメントと同じ理由）。
            explain_shape.scalar_plan(),
        );
        Ok(crate::sql::explain::build_explain_result(
            &planned,
            &explain_engine,
        ))
    }

    /// [`Self::run_explain_plan`]（`USING PLAN` 付き検索 EXPLAIN。TASK-78・
    /// SQL-6）と [`Self::run_search_explain`]（`USING PLAN` なし検索 EXPLAIN。
    /// Issue #922・SQL-27）が共有する `ExplainEngine` の組み立て（Issue #411・
    /// Issue #474）。使用エンジン種別（`self.search_engine_kind()`）・ANN 静的
    /// 判定（[`crate::sql::hnsw_cache::classify_ann_plan`]）を単一箇所へ
    /// 集約し、`hnsw_state` の `lookup`／`prepare_*`（索引構築・統計加算という
    /// 副作用を持つ）を一切呼ばず `is_some()` の有無だけを見る（検索本体を
    /// 実行しない契約はどちらの呼び出し元でも不変）。
    fn explain_engine_for(
        &self,
        is_hybrid: bool,
        is_precision: bool,
        filters_empty: bool,
        scalar_prefilter: bool,
        scalar_plan: crate::sql::scalar_plan::ScalarPlan,
    ) -> crate::sql::explain::ExplainEngine {
        let ann_plan =
            crate::sql::hnsw_cache::classify_ann_plan(crate::sql::hnsw_cache::AnnShapeInput {
                hnsw_enabled: self.hnsw_state.is_some(),
                engine_kind_unknown: self.search_engine_kind().is_none(),
                is_hybrid,
                is_precision,
                filters_empty,
                scalar_prefilter,
            });
        crate::sql::explain::ExplainEngine::new(self.search_engine_kind(), ann_plan, scalar_plan)
    }

    /// `EXPLAIN SELECT ...`（`USING PLAN` を伴わない検索 SELECT。`ORDER BY
    /// <=>`・`HYBRID`。Issue #922・SQL-27）。LLM I/O・検索本体（ハイブリッド
    /// 実行）のいずれも行わず、`Statement::Select` アームの非 `USING PLAN`
    /// 経路（`bind_in_session` → `run_select_plan`）と同じ束縛
    /// （`run_select_plan` は呼ばない）だけから ANN／SCALAR の静的判定を報告
    /// する。束縛（`bind_in_session`）を共有するため、テーブル未存在・型不整合・
    /// LIMIT／`USING MODE` 不正等のエラー分類は EXPLAIN なしの同じ文と同一
    /// （`wire_code` のパリティ）。
    fn run_search_explain(
        &self,
        validated: &crate::sql::allowlist::ValidatedStatement,
        session: &crate::sql::mode::SessionState,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let (_read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
        let bound = crate::sql::parser::bind_in_session(
            validated,
            &schema,
            session.search_mode(),
            session.udfs(),
        )?;
        let is_hybrid = matches!(bound.ranking(), crate::sql::parser::Ranking::Hybrid { .. });
        let is_precision = bound.mode().mode() == crate::sql::mode::SearchMode::Precision;
        let filters_empty = !bound.has_where_filters();
        // HINT ORDER（TASK-76・SQL-7）で DISTANCE 先行に切り替わっている場合、
        // executor（`sql::exec::execute_statement_with_cache`）は SCALAR 段を
        // 事後フィルタへ回す。`ExecutionPlan::from_evaluation_order` は executor
        // と同じ単一情報源（D6・core.rs モジュールドキュメント同様の方針）。
        let scalar_prefilter =
            crate::sql::plan::ExecutionPlan::from_evaluation_order(bound.evaluation_order())
                .scalar_prefilter;
        let scalar_plan = crate::sql::scalar_plan::classify_scalar_plan(
            &crate::sql::scalar_plan::ScalarShapeInput {
                scalar_prefilter,
                metadata_filters: bound.metadata_filters(),
                expr_filters: bound.expr_filters(),
                or_filters: bound.or_filters(),
            },
        );
        let engine = self.explain_engine_for(
            is_hybrid,
            is_precision,
            filters_empty,
            scalar_prefilter,
            scalar_plan,
        );
        Ok(crate::sql::SqlOutcome::Explain(
            crate::sql::explain::build_search_explain_result(bound.mode(), &engine),
        ))
    }

    /// `EXPLAIN SELECT <集計>`（`GROUP BY`・`SELECT DISTINCT` の脱糖形いずれも。
    /// Issue #922・SQL-27）。行走査・キャッシュ消費（`sql::aggregate::
    /// execute_aggregate_with_cache`・`sql::group_by::execute_grouped_aggregate`）
    /// を一切行わず、`bind_aggregate` の束縛結果だけから走査方式
    /// （[`crate::sql::explain::AccessPath`]）を静的に分類する
    /// （[`crate::sql::aggregate::classify_aggregate_access`] が executor の
    /// ゲートと同じ判定式〔`select_decode_tier`・`classify_scalar_plan`・
    /// `has_text_min_max_aggregate`〕を共有する単一情報源）。`bind_aggregate`
    /// を共有するためエラー分類は EXPLAIN なしの同じ文と同一。
    fn run_relational_explain_aggregate(
        &self,
        validated: &crate::sql::allowlist::ValidatedAggregate,
        session: &crate::sql::mode::SessionState,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let (_read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
        let bound = crate::sql::parser::bind_aggregate(validated, &schema, session.udfs())?;
        let (scalar_plan, access_path) =
            crate::sql::aggregate::classify_aggregate_access(&schema, &bound);
        Ok(crate::sql::SqlOutcome::Explain(
            crate::sql::explain::build_relational_explain_result(scalar_plan, access_path),
        ))
    }

    /// `EXPLAIN SELECT ... LIMIT n [OFFSET m]`（広域取得。ビュー展開後の形を
    /// 含む。Issue #922・SQL-27）。`sql::scan` はランキング段・索引・キャッシュ
    /// のいずれも消費しない（`sql::scan` モジュールドキュメント参照）ため、
    /// `scalar_plan: plain_scan`／`access_path: full_scan` に固定する
    /// （executor の実経路が常にこの 2 値である事実どおり。D5「矛盾出力の
    /// 防止」）。`bind_scan` を呼ぶのは LIMIT／`OFFSET`・投影列／`WHERE` 述語の
    /// 妥当性検証を EXPLAIN なしの同じ文と共有するため（エラー分類のパリティ）。
    fn run_relational_explain_scan(
        &self,
        validated: &crate::sql::allowlist::ValidatedScan,
        session: &crate::sql::mode::SessionState,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let (_read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
        let _bound = crate::sql::parser::bind_scan(validated, &schema, session.udfs())?;
        Ok(crate::sql::SqlOutcome::Explain(
            crate::sql::explain::build_relational_explain_result(
                crate::sql::scalar_plan::ScalarPlan::PlainScan,
                crate::sql::explain::AccessPath::FullScan,
            ),
        ))
    }

    /// 束縛済み `USING PLAN` 検索計画の EXPLAIN（TASK-186・NOSQL-10。
    /// Issue #765）。SQL の `Statement::Explain` アームと同一の私的ヘルパー
    /// （[`Self::run_explain_plan`]）を共有し、検索本体は実行しない。SQL
    /// テキストを経由せず束縛済み計画を実行したい呼び出し元（`wire-server`
    /// の NoSQL 表層。TASK-175 のポインタ）向けのセッション対応エントリで、
    /// [`Self::execute_bound_scan_in_session`]・[`Self::
    /// execute_bound_aggregate_in_session`]（Issue #728）と同型の設計
    /// （`docs/design/bound-plan-session-entry.md` 参照）。
    ///
    /// `bind` のエラー型 `E` は上記 2 エントリと異なり
    /// `E: From<crate::sql::allowlist::SqlSurfaceError>` というジェネリック
    /// 境界を持つ——呼び出し元（wire-server）の binder が SQL 表層の束縛
    /// ヘルパー（`sql::parser`）を経由して独自のエラー型（`search::
    /// SearchError` 等）を返すため。
    ///
    /// `mode_literal`（`USING MODE` 相当の生リテラル）はここでは解析しない
    /// （cursor[bot] Bugbot 指摘対応。[`Self::run_explain_plan`] がテーブル
    /// 解決後に初めて解析することで、未知テーブル＋ `mode` 値不正の要求で
    /// `42P01` が `22000` より優先される SQL テキスト経由と同一の
    /// fail-closed 順序を保つ。[`Self::execute_bound_plan_search_in_session`]
    /// と同じ設計）。
    pub fn explain_bound_plan_in_session<F, E>(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        question: &str,
        mode_literal: Option<&str>,
        bind: F,
    ) -> Result<crate::sql::exec::QueryResult, E>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
            &crate::sql::udf_call::UdfRegistry,
        ) -> Result<crate::sql::explain::ExplainShape, E>,
        E: From<crate::sql::allowlist::SqlSurfaceError>,
    {
        self.run_explain_plan(ctx, session, table, question, mode_literal, bind)
    }

    /// 束縛済み広域取得計画（[`crate::sql::parser::BoundScan`]）を単一
    /// スナップショット上で実行する（TASK-186・NOSQL-3。Issue #728）。SQL
    /// テキストを経由せず束縛済み計画を直接実行したい呼び出し元（`wire-server`
    /// の NoSQL 表層。TASK-175／TASK-177 のポインタ）向けのセッション対応
    /// エントリで、単一の `Storage` を `EngineCore` が所有したまま
    /// [`Self::execute_sql_in_session`] の `Statement::Scan` アームと同一の
    /// トランザクション・スキーマ・実行本体（[`Self::run_scan_plan`]）を共有する
    /// （`&redb::ReadTransaction` を要求する [`crate::sql::scan::execute_scan`] へ
    /// クレート外から直接到達する手段が無いための第 2 の実行器を作らない設計。
    /// `docs/design/bound-plan-session-entry.md` 参照）。
    ///
    /// 処理順は (1) `table` のスキーマを取得（`bind` はまだ呼ばない）、
    /// (2) `bind(&schema, session.udfs())` で束縛済み計画を得る、(3) 得られた
    /// 計画の対象テーブルが `table` と一致するか検証する、(4) 実行——の順。
    /// `bind` の戻り値が `Err` の場合はそのまま伝播する（engine 側で分類を
    /// 変えない）。テーブル不存在は [`Self::read_txn_with_schema`] と同じ
    /// `UndefinedTable` へ丸め込む（SQL 経路と同一のエラー分類・露出範囲）。
    ///
    /// `bind` は `read_txn` が開いている間に呼ばれる（RLS 実装のための単一
    /// スナップショット契約〔Issue #56〕を守るため、スキーマ取得と束縛の間に
    /// 別トランザクションを挟めない設計）。したがって呼び出し元の `bind`
    /// closure は純粋・軽量に保つこと（I/O・LLM 呼び出し等の重い処理を
    /// closure 内で行わない）。
    pub fn execute_bound_scan_in_session<F>(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        bind: F,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
            &crate::sql::udf_call::UdfRegistry,
        )
            -> Result<crate::sql::parser::BoundScan, crate::sql::allowlist::SqlSurfaceError>,
    {
        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let bound = bind(&schema, session.udfs())?;
        if bound.table() != table {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "bound scan plan targets a different table than requested",
            ));
        }
        self.run_scan_plan(&read_txn, ctx, &schema, &bound)
    }

    /// 束縛済み集計計画（[`crate::sql::parser::BoundAggregate`]。`GROUP BY`
    /// の有無を問わない）を単一スナップショット上で実行する（TASK-186・
    /// NOSQL-4・NOSQL-5。Issue #728）。契約・設計判断は
    /// [`Self::execute_bound_scan_in_session`] と同一（`docs/design/
    /// bound-plan-session-entry.md` 参照）。実行本体は [`Self::run_aggregate_plan`]
    /// を共有するため、SQL テキスト経由と同じキャッシュ最適化
    /// （`VisibleBitmapCache`／`SqlArenaCache`／`ScalarIndexCache`）を受ける。
    pub fn execute_bound_aggregate_in_session<F>(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        bind: F,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
            &crate::sql::udf_call::UdfRegistry,
        ) -> Result<
            crate::sql::parser::BoundAggregate,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let bound = bind(&schema, session.udfs())?;
        if bound.table() != table {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "bound aggregate plan targets a different table than requested",
            ));
        }
        self.run_aggregate_plan(
            &read_txn,
            ctx,
            &schema,
            &bound,
            crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES,
        )
    }

    /// 束縛済み複数行 `INSERT` 計画（[`crate::sql::parser::BoundInsert`] の列）を
    /// 単一スナップショット上で実行する（Issue #771・TASK-178・NOSQL-6）。
    /// SQL テキストを経由せず、NoSQL 表層（`wire-server::http::query::insert`）が
    /// `rows` 配列全体を 1 回の要求として書き込むためのセッション対応エントリで、
    /// 設計判断は [`Self::execute_bound_scan_in_session`]・
    /// [`Self::execute_bound_aggregate_in_session`]（Issue #728）と同型
    /// （`docs/design/bound-plan-session-entry.md` 参照）。異なる点は書き込み系で
    /// あるため binder closure が `read_txn` の下でスキーマのみを見て束縛し、
    /// 実際の書き込みは束縛結果を使って別途 [`crate::sql::exec::execute_insert_batch`]
    /// （独自の write トランザクション）へ委譲すること。
    ///
    /// 判定順序（fail-closed。この順が契約）:
    ///
    /// 1. `operation_id` 必須化ガード（TASK-92・RECOVER-1）: `self.ledger_mode` が
    ///    `operation_id` を要求する構成で `operation_id` が `None` なら
    ///    [`crate::sql::allowlist::SqlSurfaceError::MissingOperationId`]（`23502`）。
    ///    スキーマ取得・束縛より**前**に判定する（SQL 表層の
    ///    `sql::allowlist::validate_insert` が構造検証段階で同じ順序を守るのと同じ
    ///    理由。`sql/allowlist.rs::SqlSurfaceError::MissingOperationId` ドキュメント
    ///    参照）。
    /// 2. `row_count == 0` → [`crate::sql::allowlist::SqlSurfaceError::invalid_input`]
    ///    （`22000`。`EngineCore::execute_insert_sql_batch` の「空バッチ拒否」と同じ
    ///    契約）。
    /// 3. ①（バッチあたり最大ファイル数を「1 要求あたり最大行数」に読み替える。
    ///    `batch_limits.rs` INDEX-4 ドキュメント参照）: `row_count` が
    ///    `self.batch_limits.max_files_per_batch` を超えるなら
    ///    [`crate::sql::allowlist::SqlSurfaceError::payload_too_large`]（`54000`）。
    ///    カタログ参照・束縛より前に判定する（`row_count` だけで判定できるため。
    ///    coding-rust.md「不安全な設計 / DoS」対応）。
    /// 4. [`Self::read_txn_with_schema`] で `table` のスキーマを取得（`bind` はまだ
    ///    呼ばない）。テーブル不存在は `UndefinedTable`（`42P01`）。
    /// 5. `bind(&schema)` で束縛済み `Vec<BoundInsert>` を得る（`read_txn` が開いて
    ///    いる間に呼ぶ。[`Self::execute_bound_scan_in_session`] と同じ単一
    ///    スナップショット契約）。戻り値の `table` が引数 `table` と一致し、
    ///    `len() == row_count` であることを検証する（不一致は `22000`）。あわせて
    ///    各 `BoundInsert.operation_id` が判定 1 で検査した引数 `operation_id` と
    ///    一致することも検証する（不一致は同じく `22000`。判定 7 の実書き込みが
    ///    `bounds[0].operation_id` を台帳キーとして再解決するため、判定 1 の
    ///    早期ガードと実書き込みが異なる `operation_id` を使う fail-closed でない
    ///    経路を閉じる）。
    /// 6. ②③④（`batch_limits.rs`）: 各行のバイト量
    ///    `Σ Text.len() + Vector.len() × size_of::<f32>()`（`checked_add`。`Null` は
    ///    0）を `(0, row_bytes)` として `batch_limits::validate_batch_shape` へ渡し
    ///    （行形には「パス」が無いため `path_len` は常に 0）、
    ///    `batch_limits::validate_chunk_total(row_count, ..)`（1 行 = 1 チャンクと
    ///    みなす。INDEX-4 の「生成チャンク数」上限をここでは「行数」上限に読み替える）
    ///    で判定する。いずれも `payload_too_large`（`54000`）。
    /// 7. `read_txn` を drop してから
    ///    [`crate::sql::exec::execute_insert_batch`]（独自の write トランザクション。
    ///    `operation_id` の再解決を含む。単一行なら `execute_insert` へ委譲し
    ///    SQL-10 と同一の台帳ハッシュ空間で再送判定される）を呼ぶ。
    pub fn execute_bound_insert_in_session<F>(
        &self,
        ctx: &PolicyContext,
        table: &str,
        row_count: usize,
        operation_id: Option<&crate::recovery::required_op_id::OperationId>,
        bind: F,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
        ) -> Result<
            Vec<crate::sql::parser::BoundInsert>,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        // 判定 1: `operation_id` 必須化ガード（スキーマ取得より前）。
        self.ledger_mode
            .resolve(operation_id)
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;

        // 判定 2: 空バッチ拒否。
        if row_count == 0 {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "INSERT batch must contain at least one row",
            ));
        }

        // 判定 3（①）: 件数上限。カタログ参照・束縛より前。
        if row_count > self.batch_limits.max_files_per_batch {
            return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                crate::batch_limits::BatchLimitsError::TooManyFiles {
                    count: row_count,
                    max: self.batch_limits.max_files_per_batch,
                }
                .to_string(),
            ));
        }

        // 判定 4・5: スキーマ取得 → 束縛（同一 read_txn 下）。
        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let bounds = bind(&schema)?;
        if bounds.len() != row_count
            || bounds
                .iter()
                .any(|b| b.table != table || b.operation_id.as_ref() != operation_id)
        {
            // `bounds[*].operation_id` は判定 1 の早期ガードが検査した引数
            // `operation_id` と独立に `bind` closure（呼び出し元）が構築するため、
            // 一致検証なしでは判定 1 が通過させた値と実書き込み（判定 7・
            // `sql::exec::execute_insert_batch` が `bounds[0].operation_id` を
            // 台帳キーとして再解決する）が異なる `operation_id` を使い得る
            // （PR #823 Bugbot 指摘。`execute_insert_batch` 自身の
            // 「全要素で `operation_id` が一致」検証だけでは、判定 1 の引数との
            // 食い違いまでは検出できない）。
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "bound insert plan does not match the requested table, row count, or operation_id",
            ));
        }
        drop(read_txn);

        // 判定 6（②③④）: バイト量・チャンク数上限。SQL 表層の複数行 VALUES
        // （`execute_insert_form` の `RowBatch` 分岐。SQL-16・TASK-190）と判定本体を
        // 共有する [`Self::validate_insert_batch_byte_and_chunk_limits`] へ委譲する
        // （Issue #860 SQL/NoSQL 機能パリティ。計算内容・エラー写像は本 Issue 導入前
        // と不変）。
        self.validate_insert_batch_byte_and_chunk_limits(&bounds)?;

        // 判定 7: 実書き込み（独自の write トランザクション）。`Some(&schema)`
        // （判定 4・5 で取得した束縛時点のスキーマ）を渡すことで、実書き込みが
        // write トランザクション内で改めて取得するスキーマとの不一致を fail-closed
        // に検出する（codex-review P1 指摘・PR #823。`tenant::insert_typed_row_unchecked`
        // のドキュメント参照。束縛後・書き込み前にテーブルが再定義され列順が
        // 入れ替わっても、値が誤った列へ保存されるのを防ぐ）。
        crate::sql::exec::execute_insert_batch_with_schema(
            &self.storage,
            ctx,
            &bounds,
            self.ledger_mode,
            Some(&schema),
        )
    }

    /// `COPY` 文（Issue #939・WIRE-17・TASK-220）を構造検証し、`FROM STDIN` 形は
    /// ストリーミング取り込みセッション（[`crate::sql::copy::CopyInSession`]）を、
    /// `TO STDOUT` 形は広域取得と同じ実行本体（[`Self::run_scan_plan`]。RLS
    /// 暗黙適用・`WHERE`／`LIMIT` を含む）を即座に走らせた結果集合を返す
    /// （`wire-server::copy` から呼ばれる唯一の入口）。`FROM STDIN` 形はこの
    /// 時点では副作用を一切持たない（実際の commit は [`Self::commit_copy_in`]
    /// が CopyDone 到達後に行う）。
    ///
    /// `FROM STDIN` 形のスキーマ取得は [`Self::read_txn_with_schema`]（他の
    /// 書き込み系 SQL 文と同じ経路）を使うが、`read_txn` はスキーマのスナップ
    /// ショットを取るためだけに開き、CopyData の受信中（wire I/O 待ち）には
    /// 保持しない（`USING PLAN` 経路が LLM I/O の間 `read_txn` を保持しないの
    /// と同じ理由。`Self` モジュールドキュメント参照）。CopyData 受信中に
    /// テーブルが再定義された場合は [`Self::commit_copy_in`] が commit 直前に
    /// 検出し fail-closed に拒否する。
    pub fn begin_copy(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<CopyPlan, crate::sql::allowlist::SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_copy(sql, &self.storage, self.ledger_mode)?;
        match stmt {
            crate::sql::allowlist::CopyStatement::From(v) => {
                let (_read_txn, schema) = self.read_txn_with_schema(&v.table_name)?;
                Ok(CopyPlan::From(crate::sql::copy::CopyInSession::new(
                    v.table_name,
                    v.columns,
                    v.format,
                    v.operation_id,
                    schema,
                    self.batch_limits,
                )?))
            }
            crate::sql::allowlist::CopyStatement::To(v) => {
                let (read_txn, schema) = self.read_txn_with_schema(v.inner.table_name())?;
                let bound = crate::sql::parser::bind_scan(&v.inner, &schema, session.udfs())?;
                let result = self.run_scan_plan(&read_txn, ctx, &schema, &bound)?;
                Ok(CopyPlan::To(v.format, result))
            }
        }
    }

    /// [`Self::begin_copy`] が返した `CopyPlan::From` セッションへ、CopyDone
    /// 到達後に確定したバッチを渡し、SQL-16 の複数行 `INSERT` と同一の実行器
    /// （[`Self::execute_bound_insert_in_session`]）へ委譲して commit する
    /// （第 2 の書き込み経路を作らない設計。`operation_id` 台帳のハッシュ空間・
    /// INDEX-4 ①③④ の最終判定・RLS-9・TABLE-12 の契約はすべてそこから
    /// 継承する）。`bind` closure は束縛時点（[`Self::begin_copy`]）に取得した
    /// スキーマが commit 時点のスキーマと一致することを検証してから
    /// （不一致は fail-closed `22000`。CopyData 受信中の DDL によるスキーマ
    /// 食い違いを防ぐ）束縛済み行をそのまま返す。
    pub fn commit_copy_in(
        &self,
        ctx: &PolicyContext,
        batch: crate::sql::copy::CopyInBatch,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let crate::sql::copy::CopyInBatch {
            table,
            row_count,
            operation_id,
            bounds,
            schema: bound_schema,
        } = batch;
        self.execute_bound_insert_in_session(
            ctx,
            &table,
            row_count,
            operation_id.as_ref(),
            |schema| {
                if schema != &bound_schema {
                    return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                        "table schema changed while COPY data was being received",
                    ));
                }
                Ok(bounds)
            },
        )
    }

    /// NoSQL 表層 `update` op（束縛済み計画経由。TASK-186・NOSQL-12。Issue #876・
    /// 対象ビヘイビア NOSQL-6・SQL-17・RECOVER-1／10）を SQL 表層
    /// `UPDATE ... USING OPERATION_ID`（SQL-17・TASK-191）と**同一の実行器**
    /// （[`crate::sql::exec::execute_update_with_schema`]）へ、SQL テキストを
    /// 組み立てずに到達させるセッション入口（第 2 の実行器を作らない設計。
    /// [`Self::execute_bound_insert_in_session`] と同型）。呼び出し元
    /// （`wire-server::http::query::update`）は `bind` closure の内側で JSON
    /// `set` を [`crate::sql::allowlist::ValidatedUpdate`] へ写像したうえで
    /// [`crate::sql::parser::bind_update`] を呼び、禁止列・重複列・未知列・
    /// 型不一致の検査を engine 側の既存契約へ委譲する。
    ///
    /// 判定順序（fail-closed。契約の一部）:
    /// 1. `operation_id` 必須化ガード（TASK-92・RECOVER-1）: `self.ledger_mode`
    ///    が要求する構成で `operation_id` が `None` なら
    ///    [`crate::sql::allowlist::SqlSurfaceError::MissingOperationId`]
    ///    （`23502`）。スキーマ取得より前に判定する
    ///    （[`Self::execute_bound_insert_in_session`] 判定 1 と同じ理由）。
    /// 2. [`Self::read_txn_with_schema`] で `table` のスキーマを取得する
    ///    （`bind` はまだ呼ばない）。テーブル不存在は `UndefinedTable`
    ///    （`42P01`）。
    /// 3. `bind(&schema)` で束縛済み `BoundUpdate` を得る（`read_txn` が開いて
    ///    いる間に呼ぶ）。戻り値の `table`／`operation_id` が引数と一致する
    ///    ことを検証する（不一致は `22000`）。これは
    ///    [`Self::execute_bound_insert_in_session`] 判定 5 と同じ多層防御で、
    ///    判定 1 の早期ガードと判定 4 の実書き込みが異なる `operation_id` を
    ///    使う fail-closed でない経路を閉じる（PR #823 Bugbot 指摘と同種）。
    /// 4. `read_txn` を drop してから
    ///    [`crate::sql::exec::execute_update_with_schema`]（独自の write
    ///    トランザクション。束縛時点のスキーマ `Some(&schema)` を渡し、実行時の
    ///    再定義競合を fail-closed に検出する。`execute_update_form` と同じ
    ///    設計）を呼ぶ。
    pub fn execute_bound_update_in_session<F>(
        &self,
        ctx: &PolicyContext,
        table: &str,
        operation_id: Option<&crate::recovery::required_op_id::OperationId>,
        bind: F,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
        ) -> Result<
            crate::sql::parser::BoundUpdate,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        // 判定 1: `operation_id` 必須化ガード（スキーマ取得より前）。
        self.ledger_mode
            .resolve(operation_id)
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;

        // 判定 2・3: スキーマ取得 → 束縛（同一 read_txn 下）。
        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let bound = bind(&schema)?;
        if bound.table != table || bound.operation_id.as_ref() != operation_id {
            // `bound.operation_id` は判定 1 の早期ガードと独立に `bind` closure
            // （呼び出し元）が構築するため、一致検証なしでは判定 1 が通過させた
            // 値と実書き込み（判定 4）が異なる `operation_id` を使い得る
            // （`execute_bound_insert_in_session` 判定 5 と同じ理由）。
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "bound update plan does not match the requested table or operation_id",
            ));
        }
        drop(read_txn);

        // 判定 4: 実書き込み（独自の write トランザクション）。
        crate::sql::exec::execute_update_with_schema(
            &self.storage,
            ctx,
            &bound,
            self.ledger_mode,
            Some(&schema),
        )
    }

    /// NoSQL 表層 `delete` op（束縛済み計画経由。TASK-186・NOSQL-12。Issue #876・
    /// 対象ビヘイビア NOSQL-6・SQL-18・RECOVER-1／10）を SQL 表層
    /// `DELETE ... USING OPERATION_ID`（SQL-18・TASK-191）と**同一の実行器**
    /// （[`crate::sql::exec::execute_delete`]）へ薄く委譲するセッション入口
    /// （第 2 の実行器を作らない設計）。`DELETE` は `id` 疑似列以外の列を参照
    /// しないため、`insert`／`update` のようなスキーマ突き合わせを伴う `bind`
    /// closure は不要で、呼び出し元（`wire-server::http::query::delete`）が
    /// JSON から直接構築した [`crate::sql::parser::BoundDelete`] をそのまま
    /// 受け取る（[`Self::execute_delete_form`] と同じ設計）。
    ///
    /// `operation_id` 必須化ガード（TASK-92・RECOVER-1）は
    /// [`crate::sql::exec::execute_delete`] 自身が先頭で行う（`23502`）。
    /// テーブル不存在は [`crate::tenant::delete_row_ledgered_unchecked`] →
    /// `map_write_error` 経由で `UndefinedTable`（`42P01`）へ写像される。
    pub fn execute_bound_delete_in_session(
        &self,
        ctx: &PolicyContext,
        bound: &crate::sql::parser::BoundDelete,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::exec::execute_delete(&self.storage, ctx, bound, self.ledger_mode)
    }

    /// SQL 表層の複数行 `VALUES`（①行数上限＋②③④バイト量・チャンク総量）
    /// 上限検証本体。`execute_insert_form`・`execute_insert_returning_form`
    /// （`RETURNING` 付き。Issue #873・SQL-21）の `RowBatch` 分岐がいずれも
    /// 束縛済み `bounds` に対して同一の判定（①は `self.batch_limits.
    /// max_files_per_batch` との比較、②③④は [`Self::
    /// validate_insert_batch_byte_and_chunk_limits`] への委譲）を行っていた
    /// 重複を解消する（codex-review Low 指摘・PR #873）。判定内容・エラー写像は
    /// 集約前と完全に同一。
    fn validate_insert_row_batch_limits(
        &self,
        bounds: &[crate::sql::parser::BoundInsert],
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        if bounds.len() > self.batch_limits.max_files_per_batch {
            return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                crate::batch_limits::BatchLimitsError::TooManyFiles {
                    count: bounds.len(),
                    max: self.batch_limits.max_files_per_batch,
                }
                .to_string(),
            ));
        }
        self.validate_insert_batch_byte_and_chunk_limits(bounds)
    }

    /// バッチ INSERT の②③④上限（1 行あたり・バッチ合計のバイト量／チャンク総量。
    /// `batch_limits.rs`・INDEX-4）を検証する。行内容自体（テキスト・ベクトル）は
    /// 複製せず長さのみを積算する（coding-rust.md「不安全な設計 / DoS」対応）。
    ///
    /// [`Self::execute_bound_insert_in_session`]（NoSQL 表層 `rows[]`・NOSQL-6・
    /// TASK-178・判定6）と [`Self::validate_insert_row_batch_limits`]（SQL 表層の
    /// 複数行 VALUES・SQL-16・TASK-190・`RETURNING` 付き経路を含む）が同一の
    /// 判定本体を共有し、表層間で上限を迂回できないようにする（Issue #860
    /// SQL/NoSQL 機能パリティ）。①（行数上限。`self.batch_limits.
    /// max_files_per_batch`）は各呼び出し元が個別に行う（NoSQL 表層は
    /// スキーマ取得・束縛より前に判定する契約のため、束縛済み `bounds` だけを
    /// 受け取る本関数には含められない）。
    fn validate_insert_batch_byte_and_chunk_limits(
        &self,
        bounds: &[crate::sql::parser::BoundInsert],
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        let mut shapes: Vec<(usize, usize)> = Vec::new();
        shapes.try_reserve_exact(bounds.len()).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "failed to reserve batch shape buffer".to_string(),
            }
        })?;
        for bound in bounds {
            let mut row_bytes: usize = 0;
            for value in &bound.values {
                let value_len = match value {
                    crate::row_codec::Value::Null => 0,
                    crate::row_codec::Value::Text(s) => s.len(),
                    crate::row_codec::Value::Vector(v) => {
                        v.len().saturating_mul(std::mem::size_of::<f32>())
                    }
                    crate::row_codec::Value::Integer(_) => std::mem::size_of::<i32>(),
                    crate::row_codec::Value::BigInt(_) => std::mem::size_of::<i64>(),
                    // REAL/DOUBLE は固定長ペイロード（row_codec の
                    // SCALAR_REAL_ENTRY_LEN/SCALAR_DOUBLE_ENTRY_LEN と同じ本体幅）。
                    crate::row_codec::Value::Real(_) => std::mem::size_of::<f32>(),
                    crate::row_codec::Value::Double(_) => std::mem::size_of::<f64>(),
                    // BOOLEAN 値は行コーデック上 1 バイト固定（Issue #883・D-a）。
                    crate::row_codec::Value::Bool(_) => 1,
                    // DATE／TIMESTAMP 値は行コーデック上それぞれ 4／8 バイト固定
                    // （Issue #884・D-3）。
                    crate::row_codec::Value::Date(_) => 4,
                    crate::row_codec::Value::Timestamp(_) => 8,
                    // 配列値（Issue #888）は実際の行コーデック（`row_codec::
                    // scalar_array_entry_len`）が書き込むフレーム込みの全
                    // エンコード長をそのまま使う（presence(1)・flags(1)・
                    // count(4)・payload_len(4) のフレームヘッダ＋各要素の
                    // フレーミング。TEXT 要素は 4 バイトの長さプレフィックス
                    // を含む）。要素本文のみを積算すると、例えば空文字列
                    // 1,024 個の配列がここでは 0 バイトとして扱われる一方、
                    // 実エンコードでは要素長フィールドだけで 4 KiB 超を消費し
                    // INDEX-4 の行別・バッチ合計バイト上限を迂回できてしまう
                    // （codex-review P1 指摘・PR #1011）。
                    crate::row_codec::Value::Array(array_value) => {
                        let entry_len = crate::row_codec::scalar_array_entry_len(
                            array_value.elem(),
                            array_value,
                        )
                        .map_err(|e| {
                            crate::sql::allowlist::SqlSurfaceError::payload_too_large(e.to_string())
                        })?;
                        usize::try_from(entry_len).map_err(|_| {
                            crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                                "INSERT batch array entry length overflow",
                            )
                        })?
                    }
                    // BYTEA は TEXT と同じく本体長のみを数える（フレーミングの
                    // オーバーヘッドは他の値種別も同様に含めていないため対称、Issue #886）。
                    crate::row_codec::Value::Bytes(b) => b.len(),
                    // JSON／JSONB も TEXT と同じく本体長のみを数える（Issue #889）。
                    crate::row_codec::Value::Json(s) => s.len(),
                    // ENUM 値はラベル文字列の本体長のみを数える（TEXT と同じ
                    // フレーミング。Issue #890）。
                    crate::row_codec::Value::Enum(s) => s.len(),
                    // NUMERIC 値は presence を除く `unscaled`（i128 LE）分の
                    // 16 バイト固定（TABLE-13〔検討中〕・TASK-197、Issue #885）。
                    crate::row_codec::Value::Numeric(_) => 16,
                    // UUID 値は presence を除く 16 バイト固定生値
                    // （TABLE-13〔検討中〕・TASK-197、Issue #887）。
                    crate::row_codec::Value::Uuid(_) => 16,
                };
                row_bytes = row_bytes.checked_add(value_len).ok_or_else(|| {
                    crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                        "INSERT batch row byte size overflow",
                    )
                })?;
            }
            shapes.push((0, row_bytes));
        }
        crate::batch_limits::validate_batch_shape(&shapes, &self.batch_limits).map_err(|e| {
            crate::sql::allowlist::SqlSurfaceError::payload_too_large(e.to_string())
        })?;
        crate::batch_limits::validate_chunk_total(bounds.len(), &self.batch_limits).map_err(
            |e| crate::sql::allowlist::SqlSurfaceError::payload_too_large(e.to_string()),
        )?;
        Ok(())
    }

    /// `USING PLAN('<query>')`（TASK-77・SQL-5）経路のうち、スキーマに依存しない
    /// I/O 部分（LLM によるクエリ展開・再埋め込み）だけを行う。呼び出し元は
    /// [`Self::run_using_plan_select`]（SQL テキスト経由の `Statement::Select`
    /// アーム・束縛済み `plan` 検索〔`Self::execute_bound_plan_search_in_session`。
    /// TASK-186・NOSQL-2・Issue #764〕の双方が共有する骨格）のみ。`self.embedder`／
    /// `self.query_planner` はいずれも private フィールドで `sql::using_plan`
    /// （束縛の純粋なロジックのみを持つ）からは不可視なため、これらへアクセスする
    /// 処理（LLM 展開・再埋め込みの実行そのもの）は本メソッドに置く。
    ///
    /// `query_mode`（`USING MODE`／`search.mode` リテラルの解決済み値）は
    /// 呼び出し元が事前に解析して渡す（SQL テキスト経由は `ValidatedStatement::
    /// search_mode()` を、NoSQL 表層は `PlanSearch::query_mode()` を、
    /// いずれも呼び出し元が本メソッド呼び出し前に解析済み。以前は本メソッド内
    /// （末尾。I/O 完了後）で `ValidatedStatement` から直接解析していたが、
    /// `ValidatedStatement` を要求しない形へ一般化するため呼び出し元へ委譲した。
    /// 呼び出し元はいずれも I/O 前の事前束縛検証で同一リテラルを既に検証済みの
    /// ため、ここでの解析結果は変わらない）。
    ///
    /// 列インデックス解決（`sql::using_plan::bind_expansion`）は本メソッドに含めない
    /// （codex-review P1 指摘対応。呼び出し元が本メソッドの結果を使って I/O 完了後に
    /// 取得し直した最新スキーマ・`read_txn` の下で `bind_expansion` を呼ぶことで、
    /// I/O 中の DDL によるスキーマ食い違いを避ける。上記呼び出し元アームの
    /// ドキュメント参照）。
    ///
    /// fail-closed: プランナー未注入・埋め込み未注入・展開失敗・再埋め込み失敗は
    /// いずれも [`crate::sql::allowlist::SqlSurfaceError::Internal`]（`XX000`。ERR-2
    /// の既存分類。新規分類は追加しない）で拒否する。detail には [`CoreError`]／
    /// [`crate::embedding::EmbedError`] の固定文言（プロンプト本文・LLM 応答本文を
    /// 含まない、`query_planner.rs`・`embedding.rs` の P0 方針）のみを使う。
    fn plan_using_plan_expansion(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        query_mode: Option<crate::sql::mode::SearchMode>,
        question: &str,
    ) -> Result<UsingPlanExpansionResult, crate::sql::allowlist::SqlSurfaceError> {
        let embedder = self.embedder.as_deref().ok_or_else(|| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "no embedder configured for USING PLAN".to_string(),
            }
        })?;

        // `plan_query`（辞書スナップショット構築＋ LLM 呼び出し、高コスト I/O）より
        // 前に、展開前の原質問（`query_planner::sanitize_question` を通した後）だけで
        // 疎側の入力上限（`sparse::validate_query_bounds`）を満たせるか検証する
        // （codex-review P1 + Cursor Bugbot Medium 指摘対応、PR #266）。展開は検索語を
        // 追加するだけで既存の語を減らさないため、原質問の時点で `MAX_QUERY_BYTES`・
        // `MAX_QUERY_TERMS` を超える場合、展開後の検証（本メソッド末尾。多層防御として
        // 残す）まで待っても絶対に成功しえない。特に `MAX_QUESTION_CHARS` 文字以内の
        // CJK 質問は、`sparse::tokenize` が CJK 文字ごとに unigram／隣接文字との
        // bigram を生成するため、バイト長は上限内でも一意語数だけが
        // `MAX_QUERY_TERMS` を超えうる。ここで前倒し拒否しないと、成功不能な入力の
        // ためだけに辞書スナップショット構築・LLM 呼び出しという I/O を消費してから
        // 22000 で拒否することになり、untrusted 入力によるリソース増幅になる。
        let sanitized_question = crate::query_planner::sanitize_question(question);
        crate::sparse::validate_query_bounds(&sanitized_question).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "hybrid query text exceeds allowed length",
            )
        })?;

        let expansion = self.plan_query(ctx, table, question).map_err(|e| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!("USING PLAN query expansion failed: {e}"),
            }
        })?;

        // PLAN-10 ポインタ: 密側（再埋め込み）と疎側（`hybrid_search` の全文検索側）は
        // 別々のテキストを使う（codex-review P1 指摘対応、PR #266）。密側は
        // `query_planner::render_reembedding_text` が課す既存の再埋め込み規則
        // （固定接頭辞 `search_query: `・`MAX_SEARCH_TERMS`/`MAX_TERM_LEN` による
        // 検索語の防御的上限）に必ず従わせる。`sql::using_plan::expanded_query_text`
        // （接頭辞なし・上限なしの単純結合）をそのまま `embed_batch` へ渡すと、既存の
        // 増分索引・クエリ展開受け入れ検証（TASK-114・PLAN-10）が前提とする再埋め込み
        // 入力と異なるベクトルになり、両経路の Recall 特性が食い違う。疎側の検索
        // テキストは従来どおり `expanded_query_text`（`sql::using_plan::bind_expansion`
        // が疎側へ渡す）を使う。
        let sparse_query_text = crate::sql::using_plan::expanded_query_text(question, &expansion);

        // 疎側 `hybrid_search`（`sparse::SparseIndex::search`/`search_within`）が
        // 課すクエリ入力検証（`MAX_QUERY_BYTES`・`MAX_QUERY_TERMS`）を、密側の
        // 再埋め込み（`embedder.embed_batch`、高コスト I/O）より前に行う
        // （codex-review P1 指摘対応、PR #266）。`expanded_query_text` は原質問
        // （最大 `MAX_QUESTION_CHARS` 文字）＋展開検索語（最大 `MAX_SEARCH_TERMS` 件 ×
        // `MAX_TERM_LEN` 文字）を無条件に連結するため、CJK のような多バイト文字を
        // 多用する展開結果では文字数上限内でも結合後のバイト長が `MAX_QUERY_BYTES` を
        // 超えうる。検証を後段の `hybrid_search` 呼び出し時
        // （`sql::exec::map_hybrid_error`）にのみ委ねると、再埋め込みという高コスト
        // I/O を消費してから拒否することになり、untrusted 入力によるリソース増幅に
        // なる。fail-closed（`22000`。`map_hybrid_error` が `hybrid_search` 経由で
        // 課す既存のエラー契約と同一の `wire_code`・文言）でここで前倒し拒否する
        // （`map_hybrid_error` 側の検証は多層防御として残る）。
        crate::sparse::validate_query_bounds(&sparse_query_text).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "hybrid query text exceeds allowed length",
            )
        })?;

        // 密側の再埋め込み対象は `query_planner::render_reembedding_text`
        // （TASK-114・PLAN-10）が課す既存の再埋め込み規則（固定接頭辞・検索語の
        // 防御的上限）に従わせる。次元検証・非有限値検証は本メソッドでは行わず、
        // 呼び出し元 `execute_sql_in_session` が I/O 完了後に呼ぶ
        // `sql::using_plan::bind_expansion` に一本化する（TASK-77 が当初から採る
        // 既存の役割分担。両検証の実装は `bind_expansion` 内、`vector_column` に
        // よる次元突き合わせ・非有限値拒否の箇所を参照）。
        let dense_query_text = crate::query_planner::render_reembedding_text(question, &expansion);
        let embedded = embedder
            .embed_batch(&[dense_query_text.as_str()])
            .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!("USING PLAN re-embedding failed: {e}"),
            })?;
        // 要求は 1 件（`dense_query_text` の 1 要素スライス）のため、応答も
        // 厳密に 1 件でなければ untrusted な `Embedder` 実装の契約違反として
        // fail-closed に拒否する（codex-review P1 指摘対応、PR #266）。
        // 以前は `into_iter().next()` で先頭 1 件のみを黙って採用しており、
        // 複数ベクトルを返す契約違反応答を成功として扱っていた。
        // `query_planner::reembed_expansion` が課す同種の検証
        // （`vectors.len() != 1` を `EmbedError::InvalidResponse` で拒否）と
        // 揃えつつ、本メソッドの既存エラー契約（`SqlSurfaceError::Internal`・
        // `XX000`）は変えない最小差分とする。
        if embedded.len() != 1 {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "embedder returned unexpected vector count for USING PLAN query"
                    .to_string(),
            });
        }
        let query_vector = embedded.into_iter().next().ok_or_else(|| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "embedder returned no vector for USING PLAN query".to_string(),
            }
        })?;

        // `USING MODE`／`SET search_mode`（もしくは NoSQL 表層の `search.mode`）の
        // 優先順位解決は既存の検索 SELECT 経路（`sql::parser::bind_in_session`）と
        // 同一の規則（クエリ句 > セッション変数 > 既定）を踏襲する。`query_mode`
        // は呼び出し元が既に解析済み（本メソッドのドキュメント参照）。
        // TASK-164（PLAN-11）: プランナー推定（`expansion.mode_hint`）も優先順位
        // 解決へ含める（明示指定〔クエリ句・セッション変数〕> プランナー推定 >
        // 既定。codex-review P1 指摘対応: 従来はここで `resolve_mode` を使い
        // `expansion.mode_hint` を素通しで捨てていたため、明示指定もセッション
        // 設定もない `USING PLAN` クエリでプランナーが `precision` と推定しても
        // 常に既定の `recall` になっていた）。
        let resolved_mode = crate::sql::mode::resolve_mode_with_planner(
            query_mode,
            session.search_mode(),
            expansion.mode_hint,
        );

        Ok(UsingPlanExpansionResult {
            expansion,
            query_vector,
            resolved_mode,
        })
    }

    /// `table` へ新規行を 1 件挿入する（TASK-95・対象ビヘイビア: RECOVER-4）。
    /// `crate::tenant::insert_row`（テナント境界付き書き込みガード）への薄い委譲のみで、
    /// テナント比較・所有権判定のロジックは本メソッドに書かない。`VectorCore` trait
    /// へは昇格しない固有メソッド（`execute_sql` と同じ理由。`core-api-check` の対象外。
    /// wire 層が DML を行う際の入口はこのメソッド経由を想定し、SQL `INSERT` 表層
    /// （TASK-80/81/120）はこの経路の上に載せる前提）。
    ///
    /// TASK-92（対象ビヘイビア: RECOVER-1）: `crate::tenant::insert_row` へ委譲する前に
    /// `self.ledger_mode.require(operation_id)` を通す（詳細は
    /// `recovery::required_op_id` モジュールドキュメント参照）。
    pub fn insert_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        id: u64,
        row: &crate::storage::RowInput<'_>,
        operation_id: Option<&OperationId>,
    ) -> Result<(), crate::tenant::TenantWriteError> {
        let ledger_write = self.ledger_mode.resolve(operation_id)?;
        crate::tenant::insert_row_unchecked(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            table,
            ctx,
            id,
            row,
            ledger_write,
        )
    }

    /// `table` の既存行を 1 件更新する（TASK-95・対象ビヘイビア: RECOVER-4）。
    /// [`Self::insert_row`] と同じく `crate::tenant::update_row` への薄い委譲だが、
    /// 委譲前に同じ `operation_id` 必須化ガードを通す（TASK-92・RECOVER-1）。
    pub fn update_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        id: u64,
        row: &crate::storage::RowInput<'_>,
        operation_id: Option<&OperationId>,
    ) -> Result<(), crate::tenant::TenantWriteError> {
        let ledger_write = self.ledger_mode.resolve(operation_id)?;
        crate::tenant::update_row_unchecked(&self.storage, table, ctx, id, row, ledger_write)
    }

    /// `table` の既存行を 1 件削除する（TASK-95・対象ビヘイビア: RECOVER-4）。
    /// [`Self::insert_row`] と同じく `crate::tenant::delete_row` への薄い委譲だが、
    /// 委譲前に同じ `operation_id` 必須化ガードを通す（TASK-92・RECOVER-1）。
    pub fn delete_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        id: u64,
        operation_id: Option<&OperationId>,
    ) -> Result<(), crate::tenant::TenantWriteError> {
        let ledger_write = self.ledger_mode.resolve(operation_id)?;
        crate::tenant::delete_row_unchecked(&self.storage, table, ctx, id, ledger_write)
    }

    /// `table` に `op_id` が台帳記録済みかを照会する（TASK-93、対象ビヘイビア:
    /// RECOVER-2）。`crate::tenant::operation_recorded` への薄い委譲のみ。
    ///
    /// `LedgerMode::CompareOnlyWithoutLedger`（台帳を持たない構成）では台帳テーブルへ
    /// 一切触れず [`LedgerLookup::NoLedger`] を返す（「未記録」と誤認させない
    /// fail-closed な区別。`recovery::ledger` モジュールドキュメント参照）。
    /// `VectorCore` trait へは昇格しない（`execute_insert_sql` と同じ理由。
    /// `core-api-check` の対象外）。
    pub fn operation_recorded(
        &self,
        ctx: &PolicyContext,
        table: &str,
        op_id: &OperationId,
    ) -> Result<crate::recovery::ledger::LedgerLookup, crate::tenant::TenantWriteError> {
        use crate::recovery::ledger::LedgerLookup;
        if matches!(self.ledger_mode, LedgerMode::CompareOnlyWithoutLedger) {
            return Ok(LedgerLookup::NoLedger);
        }
        let recorded = crate::tenant::operation_recorded(&self.storage, table, ctx, op_id)?;
        Ok(if recorded {
            LedgerLookup::Recorded
        } else {
            LedgerLookup::NotRecorded
        })
    }

    /// `table` の最終 commit 済み `operation_id` を照会する（TASK-98、対象ビヘイビア:
    /// RECOVER-7。契約の詳細は spec 参照）。`crate::tenant::last_operation` への
    /// 薄い委譲のみ。
    ///
    /// `LedgerMode::CompareOnlyWithoutLedger`（台帳を持たない構成）では
    /// [`Self::operation_recorded`] と同じく台帳テーブルへ一切触れず
    /// [`LastOperationLookup::NoLedger`] を返す（`NotFound` へ丸めない）。
    /// `LastOperationLookup::Unavailable` を返すべきケースについては
    /// `recovery::ledger` モジュールドキュメント参照（codex-review P1 指摘対応）。
    /// `VectorCore` trait へは昇格しない（`operation_recorded` と同じ理由。
    /// `core-api-check` の対象外）。
    pub fn last_operation_id(
        &self,
        ctx: &PolicyContext,
        table: &str,
    ) -> Result<crate::recovery::ledger::LastOperationLookup, crate::tenant::TenantWriteError> {
        use crate::recovery::ledger::{LastOperationLookup, LastOperationRaw};
        if matches!(self.ledger_mode, LedgerMode::CompareOnlyWithoutLedger) {
            return Ok(LastOperationLookup::NoLedger);
        }
        let found = crate::tenant::last_operation(&self.storage, table, ctx)?;
        Ok(match found {
            LastOperationRaw::Found(op_id) => LastOperationLookup::Committed(op_id),
            LastOperationRaw::NotFound => LastOperationLookup::NotFound,
            // `last_op` テーブル導入（TASK-98）前の DB に旧 `op_ledger` 記録だけが
            // 残っているケース。正確な最終 `operation_id` を復元できないため
            // `NotFound` へ丸めず fail-closed に区別する（codex-review P1 指摘対応。
            // `LastOperationLookup::Unavailable` のドキュメント参照）。
            LastOperationRaw::LegacyLedgerWithoutLastOp => LastOperationLookup::Unavailable,
        })
    }

    /// 新規構築した `PrefilterSnapshot` を [`PrefilterCache::insert`] へ渡し、その
    /// 結果に応じて検索経路を分岐する（`VectorCore::search` のキャッシュミス経路
    /// からのみ呼ぶ。テスト（[`Self::search_with_built_snapshot`] を直接呼ぶ判別
    /// テスト）から呼びやすいよう `VectorCore::search` 本体から切り出した。
    /// Issue #280）。`insert` が `Some` を返せば挿入対象は現在世代と整合済みなので
    /// そのまま [`Self::search_with_snapshot`] で検索する。`None`（挿入対象自身が
    /// 構築完了から挿入までの間に stale になった、またはロック毒化・世代読み取り
    /// 失敗で判定不能）の場合はキャッシュへ一切触れず [`Self::search_uncached`] へ
    /// 1 回だけ縮退する。この縮退は「`search_with` が事後に `IndexStale` を検出して
    /// `search_uncached` へ縮退する」既存経路と観測可能な結果が同値である（世代は
    /// 単調増加のため、stale なスナップショットの `search_with` は必ず
    /// `IndexStale` を返す。無駄な `search_with` 試行を 1 回省くだけで、結果側の
    /// fail-closed 契約自体は変えない）。
    fn search_with_built_snapshot(
        &self,
        table: &str,
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
        snapshot: PrefilterSnapshot,
    ) -> Result<Vec<SearchHit>, CoreError> {
        match self
            .prefilter_cache
            .insert(&self.storage, table, ctx, snapshot)
        {
            Some(snapshot) => self.search_with_snapshot(table, ctx, query, k, snapshot),
            None => self.search_uncached(ctx, table, query, k),
        }
    }

    /// キャッシュ済み（または挿入直後の）`PrefilterSnapshot` に対して検索する
    /// （TASK-169）。`snapshot.search_with` が `IndexStale`/`ContextMismatch` を返した
    /// 場合は [`PrefilterCache::evict`] で該当エントリを破棄し、非キャッシュ経路
    /// （[`Self::search_uncached`]）で 1 回だけ検索し直す（stale なインデックスの結果を
    /// 返す経路を作らない。再構築の無限ループも発生しない。`core.rs` モジュール
    /// ドキュメント・[`PrefilterCache`] のドキュメント参照）。
    fn search_with_snapshot(
        &self,
        table: &str,
        ctx: &PolicyContext,
        query: &[f32],
        k: usize,
        snapshot: Arc<PrefilterSnapshot>,
    ) -> Result<Vec<SearchHit>, CoreError> {
        // Issue #409: `hnsw_state`（`SearchEngineKind::Hnsw` opt-in 構築時のみ
        // `Some`。TASK-131・CORE-9）が張られている場合、`sql::exec` のフィルタ
        // なし DISTANCE クエリ（`FullVisible` 形状）と同じ HNSW 世代整合キャッシュ
        // 結線を Rust API（`VectorCore::search`）経由の検索へも適用する
        // （`rls.rs::PrefilterSnapshot::search_with_hnsw` のドキュメント参照。
        // テナント境界・fail-closed 契約は `search_with` と同一のまま不変）。
        // 契約変更の経緯（旧: Rust API は常にキャッシュを迂回していた）は
        // `tests/hnsw_cache.rs::rust_api_search_bypasses_cache_and_matches_default_engine_via_fallback`
        // の docstring・`docs/design/hnsw-rls-cardinality-switch.md` 参照。
        let result = match &self.hnsw_state {
            Some(state) => {
                let access = crate::sql::hnsw_cache::HnswCacheAccess {
                    storage: &self.storage,
                    cache: &state.cache,
                    provider: state.provider,
                };
                snapshot.search_with_hnsw(
                    &self.storage,
                    ctx,
                    &access,
                    self.provider.as_ref(),
                    query,
                    k,
                )
            }
            None => snapshot.search_with(&self.storage, ctx, self.provider.as_ref(), query, k),
        };
        match result {
            Ok(hits) => Ok(hits),
            Err(RlsError::IndexStale) | Err(RlsError::ContextMismatch) => {
                self.prefilter_cache.evict(table, &snapshot);
                self.search_uncached(ctx, table, query, k)
            }
            Err(e) => Err(map_rls_error(e)),
        }
    }

    /// キャッシュを使わずアリーナを都度構築して検索する（TASK-169 以前の
    /// `EngineCore::search` 本体と同一の実装。[`Self::search_with_snapshot`] が
    /// stale 縮退時に 1 回だけ呼ぶ経路として切り出した）。呼び出し元
    /// （[`VectorCore::search`]）が `k`・次元・有限性の早期検証を既に済ませている前提で、
    /// アリーナ構築〜provider 呼び出し〜Top-k 契約検証までを行う。
    fn search_uncached(
        &self,
        ctx: &PolicyContext,
        table: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, CoreError> {
        // `VectorArena::build_filtered` へ `ctx.is_visible` をそのまま述語として渡し、
        // 不可視行（他テナント行を含む）をアリーナ構築時点で確保しない
        // （codex P0/P2・Issue #137 対応。以前はアリーナ全行を構築してから可視行だけを
        // 別バッファへ再確保・全コピーしており、1 検索あたりのピークメモリが最大で
        // 2 倍になっていた。構築時フィルタにより単一確保で完結する。`arena.rs` の
        // ドキュメント参照）。構築後のアリーナは `ctx` の下で可視な行だけを保持する。
        let arena = match VectorArena::build_filtered(&self.storage, table, |tenant, visibility| {
            ctx.is_visible(tenant, visibility)
        }) {
            Ok(arena) => arena,
            Err(ArenaError::Catalog(CatalogError::TableNotFound(_))) => {
                return Err(CoreError::NotFound)
            }
            Err(e) => return Err(e.into()),
        };

        // アリーナは構築時点で可視行だけへ絞り込み済みのため、`ids`/`vectors` を
        // そのまま `SearchInput` として provider へ渡せる（不可視データはそもそも
        // provider のアドレス空間へ渡らない。`SearchInput` に `is_visible` フィールドは
        // 存在しない。`kernel.rs` のドキュメント参照）。
        // provider へ渡す識別子はスロット番号（[`slot_ids_for`] のドキュメント参照。
        // 行 `id` は 1 つの可視集合内で一意とは限らないため識別子に使えない。TABLE-12）。
        let slot_ids = slot_ids_for(&arena)?;
        let input = SearchInput {
            ids: &slot_ids,
            vectors: arena.vectors(),
            dim: arena.dim(),
            query,
            k,
        };
        let hits = self.provider.search(input)?;

        // provider が返した結果が Top-k の契約を満たすことをコア側で検証する
        // （codex P1・Issue #137 対応）。可視集合所属だけでは「他テナントの行は
        // 混入していないが、件数超過・id 重複・非有限スコア・順序違反」を見逃す
        // ため、共有ヘルパ `provider_result_is_valid`（`rls.rs::PrefilterSnapshot::
        // search_with` と共通、TASK-133）で単一走査を確認し、1 件でも違反すれば結果を
        // 一切返さず fail-closed に拒否する（部分的なフィルタリング・並べ替えはしない）。
        //
        // 検証対象はスロット識別子の集合（スロットは重複しないため各件数は 1）。
        let visible_id_counts = visible_id_counts(&slot_ids);
        if !provider_result_is_valid(&hits, k, &visible_id_counts) {
            return Err(CoreError::ProviderResultRejected);
        }
        // 検証を通過した候補だけをテナント修飾済みヒットへ解決する（TABLE-12・RLS-9）。
        resolve_slot_hits(&arena, &hits).ok_or(CoreError::ProviderResultRejected)
    }

    /// SQL 表層の単一 INSERT 文実行エントリポイント（TASK-80、対象ビヘイビア:
    /// SQL-10）。`execute_sql`（TASK-75、SELECT 専用）とは独立した固有メソッドと
    /// する。`VectorCore` trait への昇格は行わない（`crates/engine/api/
    /// core_api.snapshot` が対象とするのは `VectorCore` trait 本体のみのため、
    /// 本メソッドの追加はコア API シグネチャ安定性チェックに影響しない。
    /// `execute_sql` と同じ理由）。
    ///
    /// `sql::allowlist::validate_insert`（構造検証。文末専用句
    /// `USING OPERATION_ID '<id>'` の省略（明示 `NULL` を含む）は、`self.ledger_mode`
    /// が `LedgerMode::Ledgered`（既定）である限りこの段階で `23502` として拒否され、
    /// 書き込みトランザクションは一切開始されない。TASK-92・対象ビヘイビア:
    /// RECOVER-1）→ `Storage::get_table_schema`（スキーマ取得）→
    /// `sql::parser::bind_insert`（意味論検証・束縛）→ `sql::exec::execute_insert`
    /// （単一 write トランザクションでの実行）の順に呼ぶ。
    pub fn execute_insert_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // Issue #485: `validate_insert` に `InsertSchemaLookup` を渡し、
        // `table_exists` が取得したスキーマを `take_schema` で再利用する
        // ことで、1 文あたり 2 回（read txn・decode とも）行っていたスキーマ
        // 取得を 1 回へ減らす（`InsertSchemaLookup` のドキュメント参照）。
        // 束縛・実行本体は [`Self::execute_insert_form`] へ委譲する（`core.rs::
        // execute_sql_in_session` の INSERT 分岐と共有し二重実装を避けるため）。
        let lookup = InsertSchemaLookup::new(&self.storage);
        let stmt = crate::sql::allowlist::validate_insert(sql, &lookup, self.ledger_mode)?;
        // `RETURNING`（Issue #873・SQL-21）はセッション経由の実行経路
        // （[`Self::execute_sql_in_session`]）専用。本エントリポイントは
        // `SqlOutcome` を持たず戻り値型が `InsertOutcome` 固定のため、
        // `RowDescription`／結果行を運べない——構文検証直後・書き込み
        // トランザクション開始前（`execute_insert_form` 呼び出し前）に
        // `42601` で拒否し、台帳を一切消費しない（fail-closed）。
        if stmt.returning.is_some() {
            return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "RETURNING requires the session-aware entry point",
            ));
        }
        self.execute_insert_form(ctx, &stmt, &lookup)
    }

    /// [`Self::execute_insert_sql`]・[`Self::execute_sql_in_session`] の
    /// INSERT 分岐が共有する束縛〜実行本体（Issue #485）。`validate_insert`／
    /// `validate_insert_tokens` が返した `stmt` と、その検証時に使った
    /// `lookup`（`table_exists` 呼び出しでスキーマをキャッシュ済み）を受け取り、
    /// [`InsertSchemaLookup::take_schema`] でスキーマを再取得できればそれを
    /// 使い、できなければ（名前不一致・未保持の防御的経路）`get_table_schema`
    /// 単独呼び出しへ fail-closed にフォールバックする——いずれの経路でも
    /// エラー写像・`wire_code` は本 Issue 導入前と完全に一致する。
    fn execute_insert_form(
        &self,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedInsert,
        lookup: &InsertSchemaLookup<'_>,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let schema = match lookup.take_schema(&stmt.table_name) {
            Some(schema) => schema,
            None => self
                .storage
                .get_table_schema(&stmt.table_name)
                .map_err(|e| match e {
                    CatalogError::TableNotFound(name) => {
                        crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                    }
                    // `TableNotFound` 以外（`CorruptSchema` の格納済みカタログ断片・
                    // `Backend` の redb I/O 情報等）は detail へ一切展開しない固定文言に
                    // 丸める（codex-review P0 指摘・PR #189。`CatalogError::CorruptSchema`
                    // 自身の「wire クライアントへは detail を渡さない」契約と
                    // security.md P0「エラー経由で内部情報・存在情報を漏らさない」対応）。
                    _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "failed to load table schema".to_string(),
                    },
                })?,
        };
        let bound = crate::sql::parser::bind_insert_form(stmt, &schema)?;
        match bound {
            crate::sql::parser::BoundInsertForm::Row(bound) => {
                crate::sql::exec::execute_insert(&self.storage, ctx, &bound, self.ledger_mode)
            }
            // 複数行 `VALUES`（SQL-16、TASK-190）。NoSQL 表層の `rows[]`
            // （NOSQL-6・TASK-178・`Self::execute_bound_insert_in_session`）と同じ
            // 「同一テーブル・同一 operation_id」の 1 write トランザクション・
            // 1 台帳エントリ実行を共有する（第 2 の書き込み経路を作らない設計。
            // `sql::exec::execute_insert_batch_with_schema` ドキュメント参照）。
            //
            // INDEX-4 上限も NoSQL 表層と揃える（Issue #860 SQL/NoSQL 機能
            // パリティ。SQL 表層の複数行 VALUES が `self.batch_limits` を迂回して
            // NoSQL 表層より緩い上限で受理されないようにする）。①（行数上限）・
            // ②③④（バイト量・チャンク総量）はいずれも [`Self::
            // validate_insert_row_batch_limits`]（`execute_insert_returning_form`
            // の `RowBatch` 分岐と共有）へ委譲する。
            // `sql::parser::MAX_INSERT_ROWS_PER_STATEMENT`（構文解析段階の
            // 1 文あたり行数上限）とは独立な、運用者が調整可能な上限
            // （`self.batch_limits`）である点に注意。
            crate::sql::parser::BoundInsertForm::RowBatch(bounds) => {
                self.validate_insert_row_batch_limits(&bounds)?;
                crate::sql::exec::execute_insert_batch_with_schema(
                    &self.storage,
                    ctx,
                    &bounds,
                    self.ledger_mode,
                    Some(&schema),
                )
            }
            crate::sql::parser::BoundInsertForm::File(bound) => {
                crate::sql::exec::execute_file_insert(
                    &self.storage,
                    ctx,
                    self.embedder.as_deref(),
                    &self.incremental_config,
                    &bound,
                    self.ledger_mode,
                )
            }
            // `ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...`（SQL-20・
            // TASK-193、Issue #872）。複数行 `VALUES` を伴う UPSERT にも
            // `RowBatch` 分岐と同じ INDEX-4 上限（①行数・②③④バイト量・
            // チャンク総量）を適用する（SQL/NoSQL 機能パリティの方針を UPSERT
            // にも揃える）。
            crate::sql::parser::BoundInsertForm::Upsert(bound) => {
                if bound.rows.len() > self.batch_limits.max_files_per_batch {
                    return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                        crate::batch_limits::BatchLimitsError::TooManyFiles {
                            count: bound.rows.len(),
                            max: self.batch_limits.max_files_per_batch,
                        }
                        .to_string(),
                    ));
                }
                self.validate_insert_batch_byte_and_chunk_limits(&bound.rows)?;
                crate::sql::exec::execute_upsert(
                    &self.storage,
                    ctx,
                    &bound,
                    self.ledger_mode,
                    &schema,
                )
            }
        }
    }

    /// [`Self::execute_sql_in_session`] の `RETURNING` 付き `INSERT`（Issue #873・
    /// SQL-21）分岐が呼ぶ実行本体。`execute_insert_form` と同じスキーマ取得
    /// （`InsertSchemaLookup::take_schema` 優先・フォールバック `get_table_schema`）
    /// を経てから `sql::parser::bind_returning`・`bind_insert_form` を束縛する。
    /// ファイル形（`BoundInsertForm::File`）は呼び出し元がサーバー側チャンク化
    /// 行を返す応答形を持たないため `42601` で拒否する（fail-closed。行形
    /// （単一行・複数行 `VALUES`）のみ [`crate::sql::exec::execute_insert_returning`]
    /// へ委譲する）。`stmt.returning` は呼び出し元が `Some` であることを検証
    /// 済みの前提（`is_none()` は `Internal` として拒否）。
    fn execute_insert_returning_form(
        &self,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedInsert,
        lookup: &InsertSchemaLookup<'_>,
    ) -> Result<crate::sql::exec::ReturningOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let schema = match lookup.take_schema(&stmt.table_name) {
            Some(schema) => schema,
            None => self
                .storage
                .get_table_schema(&stmt.table_name)
                .map_err(|e| match e {
                    CatalogError::TableNotFound(name) => {
                        crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                    }
                    _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "failed to load table schema".to_string(),
                    },
                })?,
        };
        let returning = crate::sql::parser::bind_returning(stmt.returning.as_ref(), &schema)?
            .ok_or_else(|| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "execute_insert_returning_form called without RETURNING".to_string(),
            })?;
        let bound = crate::sql::parser::bind_insert_form(stmt, &schema)?;
        match bound {
            crate::sql::parser::BoundInsertForm::Row(bound) => {
                crate::sql::exec::execute_insert_returning(
                    &self.storage,
                    ctx,
                    std::slice::from_ref(&bound),
                    self.ledger_mode,
                    &returning,
                    &schema,
                )
            }
            crate::sql::parser::BoundInsertForm::RowBatch(bounds) => {
                self.validate_insert_row_batch_limits(&bounds)?;
                crate::sql::exec::execute_insert_returning(
                    &self.storage,
                    ctx,
                    &bounds,
                    self.ledger_mode,
                    &returning,
                    &schema,
                )
            }
            crate::sql::parser::BoundInsertForm::File(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "RETURNING is not supported for file-form INSERT (path/body columns)",
                ))
            }
            // `RETURNING`（Issue #873・SQL-21）と `ON CONFLICT`（Issue #872・
            // SQL-20）の併用は実行結線未着手（`docs/design/sql-returning.md`
            // 「UPSERT（#872）との併用」節・`docs/design/sql-upsert.md`「対象外・
            // 申し送り」節。RETURNING の文法上の位置＝`USING OPERATION_ID` の
            // 直前という契約〔許可リスト段〕は維持しつつ、実行本体
            // `sql::exec::execute_upsert` は `stmt.returning` を一切参照しない
            // ため、ここで結線せず fail-closed に拒否する。第 2 の投影実装を
            // 作らない設計判断はそのまま維持する）。
            crate::sql::parser::BoundInsertForm::Upsert(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "RETURNING is not supported for INSERT ... ON CONFLICT ...",
                ))
            }
        }
    }

    /// SQL 表層の単一 TRUNCATE 文実行エントリポイント（TASK-195、対象ビヘイビア:
    /// SQL-22）。`execute_insert_sql` と同じ構造（`execute_sql`（TASK-75、SELECT
    /// 専用）とは独立した固有メソッド。`VectorCore` trait への昇格は行わない）。
    ///
    /// `sql::allowlist::validate_truncate`（構造検証。文末専用句
    /// `USING OPERATION_ID '<id>'` の省略（明示 `NULL` を含む）は、`self.ledger_mode`
    /// が `LedgerMode::Ledgered`（既定）である限りこの段階で `23502` として拒否され、
    /// 書き込みトランザクションは一切開始されない。TASK-92・対象ビヘイビア:
    /// RECOVER-1）→ [`Self::execute_truncate_form`]（実行本体）の順に呼ぶ。
    pub fn execute_truncate_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::TruncateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_truncate(sql, &self.storage, self.ledger_mode)?;
        self.execute_truncate_form(ctx, &stmt)
    }

    /// [`Self::execute_truncate_sql`]・[`Self::execute_sql_in_session`] の
    /// TRUNCATE 分岐が共有する実行本体（`execute_insert_form` と同じ設計）。
    /// TRUNCATE はスキーマを参照しない（列単位の意味論検証を持たない）ため、
    /// `execute_insert_form` の `InsertSchemaLookup` キャッシュに相当する仕組みは
    /// 不要で、`sql::exec::execute_truncate` を直接呼ぶだけの薄い委譲になる。
    fn execute_truncate_form(
        &self,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedTruncate,
    ) -> Result<crate::sql::exec::TruncateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::exec::execute_truncate(&self.storage, ctx, stmt, self.ledger_mode)
    }

    /// SQL 表層の単一 DELETE 文実行エントリポイント（SQL-18、TASK-191、#867）。
    /// `execute_truncate_sql` と同じ構造（`execute_sql`（TASK-75、SELECT
    /// 専用）とは独立した固有メソッド。`VectorCore` trait への昇格は行わない）。
    ///
    /// `sql::allowlist::validate_delete`（構造検証。文末専用句
    /// `USING OPERATION_ID '<id>'` の省略（明示 `NULL` を含む）は、`self.ledger_mode`
    /// が `LedgerMode::Ledgered`（既定）である限りこの段階で `23502` として拒否され、
    /// 書き込みトランザクションは一切開始されない。TASK-92・対象ビヘイビア:
    /// RECOVER-1）→ [`Self::execute_delete_form`]（実行本体）の順に呼ぶ。
    pub fn execute_delete_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let stmt = crate::sql::allowlist::validate_delete(sql, &self.storage, self.ledger_mode)?;
        // `RETURNING`（Issue #873・SQL-21）はセッション経由の実行経路専用
        // （`execute_insert_sql` と同じ理由・同じ判定順序）。
        if stmt.returning.is_some() {
            return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "RETURNING requires the session-aware entry point",
            ));
        }
        self.execute_delete_form(ctx, &stmt)
    }

    /// [`Self::execute_delete_sql`]・[`Self::execute_sql_in_session`] の
    /// DELETE 分岐が共有する実行本体（`execute_truncate_form` と同じ設計）。
    /// DELETE は `id` 疑似列以外の列を参照しない（`sql::parser::bind_delete`
    /// ドキュメント参照）ため、`execute_insert_form` の `InsertSchemaLookup`
    /// キャッシュに相当する仕組みは不要で、`sql::parser::bind_delete` →
    /// `sql::exec::execute_delete` の薄い委譲になる。
    fn execute_delete_form(
        &self,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedDelete,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let bound = crate::sql::parser::bind_delete(stmt)?;
        crate::sql::exec::execute_delete(&self.storage, ctx, &bound, self.ledger_mode)
    }

    /// [`Self::execute_sql_in_session`] の `DELETE`（述語形。SQL-19・TASK-192、
    /// Issue #871）分岐が呼ぶ実行本体。テーブルスキーマを取得したうえで
    /// [`crate::sql::parser::bind_predicate_delete`] で束縛し、内容照合ハッシュ
    /// （[`crate::recovery::content_hash::for_delete_where`]。RECOVER-11）を
    /// 束縛済み構文形〔`stmt.where_predicates()`〕から計算してから
    /// [`crate::sql::exec::execute_predicate_delete`] へ委譲する。
    ///
    /// エラー優先順位（`docs/design/predicate-dml-exec.md` 参照）: 構造検証・
    /// `operation_id` 必須化・カタログ存在確認は `validate_delete_statement_tokens`
    /// （`sql::allowlist`）が呼び出し元で既に適用済み。本メソッドは
    /// スキーマ取得（`42P01`。並行 `DROP TABLE` の防御的経路） → 束縛
    /// （`22000`。`sql/scan.rs` と同じ失敗点） → 内容照合ハッシュ計算
    /// （WASM UDF 呼び出しの拒否・`42601`。ADR
    /// `docs/design/multi-row-dml-operation-id.md` §4.4.1） → 実行本体（台帳照合
    /// `23505`／`22023`・上限超過 `54000`）の順で失敗しうる。
    fn execute_predicate_delete_form(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let schema = self
            .storage
            .get_table_schema(stmt.table_name())
            .map_err(|e| match e {
                CatalogError::TableNotFound(name) => {
                    crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                }
                _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "failed to load table schema".to_string(),
                },
            })?;
        let bound = crate::sql::parser::bind_predicate_delete(stmt, &schema, session.udfs())?;
        let content_hash_value = crate::recovery::content_hash::for_delete_where(
            stmt.table_name(),
            stmt.where_predicates(),
            session.udfs(),
        )?;
        crate::sql::exec::execute_predicate_delete(
            &self.storage,
            ctx,
            &bound,
            self.ledger_mode,
            &schema,
            &content_hash_value,
        )
    }

    /// [`Self::execute_sql_in_session`] の `RETURNING` 付き単一行 `DELETE`
    /// （Issue #873・SQL-21）分岐が呼ぶ実行本体。DELETE は元々スキーマ非依存
    /// （`id` 疑似列以外を参照しない。`execute_delete_form` ドキュメント参照）
    /// だが、`RETURNING` の投影束縛・削除前捕捉行の TOCTOU 対策のためここで
    /// 初めてスキーマを取得する。`stmt.returning` は呼び出し元が `Some` で
    /// あることを検証済みの前提（`is_none()` は `Internal` として拒否）。
    fn execute_delete_returning_form(
        &self,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedDelete,
    ) -> Result<crate::sql::exec::ReturningOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let schema = self
            .storage
            .get_table_schema(&stmt.table_name)
            .map_err(|e| match e {
                CatalogError::TableNotFound(name) => {
                    crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                }
                _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "failed to load table schema".to_string(),
                },
            })?;
        let returning = crate::sql::parser::bind_returning(stmt.returning.as_ref(), &schema)?
            .ok_or_else(|| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "execute_delete_returning_form called without RETURNING".to_string(),
            })?;
        let bound = crate::sql::parser::bind_delete(stmt)?;
        crate::sql::exec::execute_delete_returning(
            &self.storage,
            ctx,
            &bound,
            self.ledger_mode,
            &returning,
            &schema,
        )
    }

    /// SQL 表層の単一 UPDATE 文実行エントリポイント（Issue #865、対象ビヘイビア:
    /// SQL-17・TASK-191）。`execute_insert_sql` と同じ構造（`execute_sql`（TASK-75、
    /// SELECT 専用）とは独立した固有メソッド。`VectorCore` trait への昇格は行わない）。
    ///
    /// `sql::allowlist::validate_update`（構造検証。文末専用句
    /// `USING OPERATION_ID '<id>'` の省略（明示 `NULL` を含む）は、`self.ledger_mode`
    /// が `LedgerMode::Ledgered`（既定）である限りこの段階で `23502` として拒否され、
    /// 書き込みトランザクションは一切開始されない。TASK-92・対象ビヘイビア:
    /// RECOVER-1）→ [`Self::execute_update_form`]（束縛・実行本体）の順に呼ぶ。
    ///
    /// このエントリポイントは単一行・`id` 完全一致形専用のまま残す
    /// （`sql::allowlist::validate_update`・[`crate::sql::allowlist::ValidatedUpdate`]
    /// を経由）。述語形（SQL-19）を含む両形式のディスパッチは
    /// [`Self::execute_sql_in_session`] の `UPDATE` 分岐・
    /// [`Self::execute_predicate_update_form`] が担う。
    pub fn execute_update_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // `execute_insert_sql` と同じく `InsertSchemaLookup` を `validate_update` へ
        // 渡し、`table_exists` が取得したスキーマを `take_schema` で再利用する
        // ことで、1 文あたりのスキーマ取得を 1 回へ減らす。
        let lookup = InsertSchemaLookup::new(&self.storage);
        let stmt = crate::sql::allowlist::validate_update(sql, &lookup, self.ledger_mode)?;
        self.execute_update_form(ctx, &stmt, &lookup)
    }

    /// [`Self::execute_update_sql`] の単一行 UPDATE 分岐が使う束縛〜実行本体
    /// （`execute_insert_form` と同じ設計）。`validate_update`／
    /// `validate_update_tokens` が返した `stmt` と、その検証時に使った `lookup`
    /// （`table_exists` 呼び出しでスキーマをキャッシュ済み）を受け取り、
    /// [`InsertSchemaLookup::take_schema`] でスキーマを再取得できればそれを使い、
    /// できなければ `get_table_schema` 単独呼び出しへ fail-closed にフォール
    /// バックする。
    ///
    /// [`Self::execute_sql_in_session`] 経由（`validate_update_form_tokens` が
    /// 単一行・述語形の双方を受理する）の単一行 UPDATE は、スキーマを
    /// `InsertSchemaLookup` ではなく [`Self::execute_predicate_update_form`] が
    /// 直接取得するため本メソッドを経由しない
    /// （`crate::sql::exec::execute_update_with_schema` を直接呼ぶ）。
    fn execute_update_form(
        &self,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedUpdate,
        lookup: &InsertSchemaLookup<'_>,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let schema = match lookup.take_schema(&stmt.table_name) {
            Some(schema) => schema,
            None => self
                .storage
                .get_table_schema(&stmt.table_name)
                .map_err(|e| match e {
                    CatalogError::TableNotFound(name) => {
                        crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                    }
                    _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "failed to load table schema".to_string(),
                    },
                })?,
        };
        let bound = crate::sql::parser::bind_update(stmt, &schema)?;
        crate::sql::exec::execute_update_with_schema(
            &self.storage,
            ctx,
            &bound,
            self.ledger_mode,
            Some(&schema),
        )
    }

    /// [`Self::execute_sql_in_session`] の `UPDATE` 分岐が呼ぶ実行本体
    /// （SQL-17・TASK-191、Issue #865 の単一行 `id` 完全一致形と、SQL-19・
    /// TASK-192、Issue #871 の述語形の両方を扱う）。
    /// `sql::allowlist::validate_update_form` 自体は単一行・述語形の双方を
    /// 構造的に受理するため、本メソッドが [`crate::sql::parser::BoundUpdateForm`]
    /// の variant で実行本体を振り分ける（`Single` 腕は
    /// [`crate::sql::exec::execute_update_with_schema`]（PR #989 の単一行実行
    /// 結線と同一の実行本体。`docs/design/predicate-dml-exec.md`「PR #989 との
    /// 整合ルール」参照）、`Predicate` 腕は本メソッド内で内容照合ハッシュ
    /// （RECOVER-11）を計算したうえで [`crate::sql::exec::execute_predicate_update`]
    /// へ委譲する。[`Self::execute_predicate_delete_form`] と同じ手順
    /// （スキーマ取得 → 束縛 → （述語形のみ）内容照合ハッシュ計算 → 実行本体）
    /// で実行する。
    fn execute_predicate_update_form(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedUpdateForm,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::ValidatedUpdateForm;

        let table_name = match stmt {
            ValidatedUpdateForm::Single(v) => v.table_name.as_str(),
            ValidatedUpdateForm::Predicate(v) => v.table_name(),
        };
        let schema = self
            .storage
            .get_table_schema(table_name)
            .map_err(|e| match e {
                CatalogError::TableNotFound(name) => {
                    crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                }
                _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "failed to load table schema".to_string(),
                },
            })?;
        let bound_form = crate::sql::parser::bind_update_form(stmt, &schema, session.udfs())?;
        let predicate = match bound_form {
            crate::sql::parser::BoundUpdateForm::Single(bound) => {
                return crate::sql::exec::execute_update_with_schema(
                    &self.storage,
                    ctx,
                    &bound,
                    self.ledger_mode,
                    Some(&schema),
                );
            }
            crate::sql::parser::BoundUpdateForm::Predicate(bound) => bound,
        };

        let ValidatedUpdateForm::Predicate(validated) = stmt else {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "internal: bound predicate UPDATE from a non-predicate statement"
                    .to_string(),
            });
        };
        let assignment_refs: Vec<(&str, &crate::sql::allowlist::InsertLiteral)> = validated
            .assignments()
            .iter()
            .map(|(name, literal)| (name.as_str(), literal))
            .collect();
        let content_hash_value = crate::recovery::content_hash::for_update_where(
            validated.table_name(),
            &assignment_refs,
            validated.where_predicates(),
            session.udfs(),
        )?;
        crate::sql::exec::execute_predicate_update(
            &self.storage,
            ctx,
            &predicate,
            self.ledger_mode,
            &schema,
            &content_hash_value,
        )
    }

    /// SQL 表層のバッチ INSERT 実行エントリポイント（TASK-122、対象ビヘイビア:
    /// INDEX-4）。[`Self::execute_insert_sql`] の複数ファイル版で、複数ファイルを
    /// 1 バッチとして受け取る engine ローカル API の入口（1 文 = 1 ファイルの
    /// 検証済み `INSERT` 文の列（`sqls`）を 1 バッチとして受け取る。行形の
    /// 複数行 `VALUES`（SQL-16、TASK-190）はこの API とは別経路——`sql::parser::
    /// BoundInsertForm::RowBatch` を経由し、ファイル形が混在した場合と同じく
    /// 本メソッドは行形を一切受理しない）。`VectorCore` trait への昇格は行わない
    /// （`execute_insert_sql` と同じ理由）。
    ///
    /// 手順:
    /// 1. `sqls` が空なら `22000`（invalid input）で拒否する。
    /// 2. 各文を `sql::allowlist::validate_insert`（`operation_id` 必須化ガード
    ///    （TASK-92・RECOVER-1）を含む）→ `sql::parser::bind_insert_form` で束縛する。
    ///    **全文がファイル形であることを要求**し、行形が 1 件でも混在したら `22000`
    ///    で拒否する（黙って別セマンティクスへ丸めない）。
    /// 3. 束縛結果から `operation_id` を `self.ledger_mode.resolve` で台帳書き込み
    ///    指示へ解決し（TASK-93・RECOVER-2。行形・単一ファイル形と同じ契約）、
    ///    `incremental::index_file_batch` へまとめて委譲する。一括投入 4 上限
    ///    （`self.batch_limits`。TASK-122）の判定自体は `index_file_batch` が
    ///    埋め込み・write トランザクションのいずれよりも前に行う契約
    ///    （`batch_limits.rs`・`incremental.rs` モジュールドキュメント参照）。
    ///
    /// 上限超過時は redb・インメモリ索引・`operation_id` 台帳のいずれも変更されない
    /// （`incremental::index_file_batch` の副作用ゼロ契約）。上限非起因の途中失敗
    /// （例: 2 ファイル目の埋め込み失敗）は文単位セマンティクスとなり、既に処理済みの
    /// 先行ファイルはそのまま索引化された状態で残る（`incremental::index_file_batch`
    /// ドキュメント参照）。
    pub fn execute_insert_sql_batch(
        &self,
        ctx: &PolicyContext,
        sqls: &[&str],
    ) -> Result<Vec<crate::sql::exec::InsertOutcome>, crate::sql::allowlist::SqlSurfaceError> {
        if sqls.is_empty() {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                "INSERT batch must contain at least one statement",
            ));
        }

        // ①（バッチあたり最大ファイル数）はここで先に判定できる（`sqls.len()` は
        // 束縛前から既知）。①だけは束縛（`bind_insert_form` の `path`/`body`
        // 文字列複製を伴う）より前に切ることで、ファイル数超過時の無駄な確保・
        // 解析を避ける（coding-rust.md「不安全な設計 / DoS」対応。`batch_limits.rs`
        // 側の判定と重複するが、同じ上限値（`self.batch_limits.max_files_per_batch`）
        // に対する早期リジェクトであり結果は変わらない）。②③（本文サイズ・バッチ
        // 合計サイズ）は各文の束縛（`path`/`body` の `String` 複製を伴う）を経ない
        // と判定できないため、以下の束縛ループ内で束縛直後に逐次判定する
        // （`incremental::index_file_batch` 側の `validate_batch_shape` はその
        // 最終防衛線であり、ここでの早期判定が主防御となる）。
        if sqls.len() > self.batch_limits.max_files_per_batch {
            return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                crate::batch_limits::BatchLimitsError::TooManyFiles {
                    count: sqls.len(),
                    max: self.batch_limits.max_files_per_batch,
                }
                .to_string(),
            ));
        }

        // 束縛結果（`BoundFileInsert`）の所有権をここで保持する。後段で構築する
        // `BoundFileIndexInput`／`LedgerWrite` はこの `Vec` の要素を借用するため、
        // 束縛と `items` 構築を 2 パスへ分ける（`sql::parser::BoundFileInsert` の
        // 生存期間を `index_file_batch` 呼び出しまで維持するため）。
        let mut file_binds: Vec<crate::sql::parser::BoundFileInsert> = Vec::new();
        file_binds.try_reserve_exact(sqls.len()).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "failed to reserve batch bind buffer".to_string(),
            }
        })?;

        // ⓪（束縛前の生 SQL テキスト長に対する粗い早期リジェクト。
        // `batch_limits::validate_raw_sql_len`）を各文の束縛（`validate_insert` の
        // `lexer::tokenize`・`bind_insert_form` の `path`/`body` 文字列複製）より
        // 前に判定する（codex-review P1 指摘・PR #242 対応。②③の逐次判定は
        // decode 済みの `bound.path`/`bound.body` 長にしか作用せず、束縛処理自体
        // （構文木の構築・文字列複製）は判定前に必ず入力サイズへ比例して走って
        // しまうため、単一の巨大 SQL 文だけでファイル数上限（①）を経ずに
        // メモリ・CPU を消費させる DoS 経路が残っていた。⓪は生テキスト長のみを
        // 見る保守的な予算判定であり、正確な上限は引き続き②③（束縛後）が担う
        // ため置き換えではない。`batch_limits.rs` の `validate_raw_sql_len` ドキュ
        // メント参照）。
        //
        // ②③（1 ファイルあたり最大本文サイズ・バッチ合計最大サイズ）を束縛直後
        // ここで逐次判定する（`batch_limits.rs` の `validate_batch_shape` は
        // `index_file_batch` 側で全ファイルの束縛完了後に改めて呼ばれる最終防衛線
        // だが、束縛（`bind_insert_form` による `path`/`body` の `String` 複製）を
        // 全文について終えてからでは判定が遅く、`batch_limits.rs` モジュール
        // ドキュメントが掲げる「本文・パスの複製や追加確保を行わない」DoS 対策の
        // 意図に反する。ここでは束縛済みの `bound.path`/`bound.body` の長さのみを
        // 見て、違反を検出した時点のファイルで打ち切ることで、複製の増幅を
        // 「違反ファイルまで」に抑える）。
        let mut running_total_bytes: usize = 0;
        let mut running_raw_sql_bytes: usize = 0;
        for (index, sql) in sqls.iter().enumerate() {
            running_raw_sql_bytes = crate::batch_limits::validate_raw_sql_len(
                index,
                sql.len(),
                running_raw_sql_bytes,
                &self.batch_limits,
            )
            .map_err(|e| {
                crate::sql::allowlist::SqlSurfaceError::payload_too_large(e.to_string())
            })?;

            // Issue #485: 単文経路（`execute_insert_sql`）と同じく
            // `InsertSchemaLookup` で `validate_insert`（`table_exists`）と
            // 束縛のスキーマ取得を 1 read txn・1 decode へ一本化する
            // （1 ループ = 1 文につき新規インスタンス。他文とスキーマを
            // 共有しない一時オブジェクトのため文をまたいだ取り違えは
            // 起こらない）。
            let lookup = InsertSchemaLookup::new(&self.storage);
            let stmt = crate::sql::allowlist::validate_insert(sql, &lookup, self.ledger_mode)?;
            // `RETURNING`（Issue #873・SQL-21）はセッション経由の実行経路専用
            // （`execute_insert_sql` と同じ理由）。本エントリポイントはファイル形
            // 専用のバッチ投入であり、`RETURNING` はいずれにせよファイル形では
            // 未対応（サーバー側チャンク化行を返す応答形が未定義）のため、
            // 束縛より前に一律 `42601` で拒否する。
            if stmt.returning.is_some() {
                return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "RETURNING requires the session-aware entry point",
                ));
            }
            let schema = match lookup.take_schema(&stmt.table_name) {
                Some(schema) => schema,
                None => self
                    .storage
                    .get_table_schema(&stmt.table_name)
                    .map_err(|e| match e {
                        CatalogError::TableNotFound(name) => {
                            crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                        }
                        _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "failed to load table schema".to_string(),
                        },
                    })?,
            };
            let bound = crate::sql::parser::bind_insert_form(&stmt, &schema)?;
            match bound {
                crate::sql::parser::BoundInsertForm::File(file_bound) => {
                    if file_bound.body.len() > self.batch_limits.max_file_body_bytes {
                        return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                            crate::batch_limits::BatchLimitsError::FileBodyTooLarge {
                                index: file_binds.len(),
                                len: file_bound.body.len(),
                                max: self.batch_limits.max_file_body_bytes,
                            }
                            .to_string(),
                        ));
                    }
                    let next_total_bytes = file_bound
                        .path
                        .len()
                        .checked_add(file_bound.body.len())
                        .and_then(|t| running_total_bytes.checked_add(t))
                        .ok_or_else(|| {
                            crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                                crate::batch_limits::BatchLimitsError::BatchTotalTooLarge {
                                    total: usize::MAX,
                                    max: self.batch_limits.max_batch_total_bytes,
                                }
                                .to_string(),
                            )
                        })?;
                    if next_total_bytes > self.batch_limits.max_batch_total_bytes {
                        return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                            crate::batch_limits::BatchLimitsError::BatchTotalTooLarge {
                                total: next_total_bytes,
                                max: self.batch_limits.max_batch_total_bytes,
                            }
                            .to_string(),
                        ));
                    }
                    running_total_bytes = next_total_bytes;
                    file_binds.push(file_bound);
                }
                // 行形が 1 件でも混在した場合は黙って行形として処理せず拒否する
                // （「複数ファイルのバッチ投入」という本メソッドの契約を維持する。
                // 単一行・複数行 VALUES〔SQL-16、TASK-190〕・UPSERT〔SQL-20・
                // TASK-193、Issue #872。`bind_insert_form` がファイル形との
                // 併用〔`ON CONFLICT` かつ `path`/`body` 列指定〕を既に `42601`
                // で拒否しているため、ここへ到達する `Upsert` は必ず行形相当〕の
                // いずれも同じ扱い）。
                crate::sql::parser::BoundInsertForm::Row(_)
                | crate::sql::parser::BoundInsertForm::RowBatch(_)
                | crate::sql::parser::BoundInsertForm::Upsert(_) => {
                    return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                        "INSERT batch requires every statement to be file-form (path/body columns)",
                    ));
                }
            }
        }

        // embedder 未構成エラーは①②③（ファイル数・本文サイズ・バッチ合計サイズ）の
        // 上限判定をすべて終えた後に返す（codex-review 指摘 P1 対応）。ここより前で
        // 返すと、上限超過バッチを embedder 未構成の EngineCore に渡した際に
        // `54000`（payload too large）ではなく embedder 未構成由来の `Internal`
        // （`XX000` 相当）が先に返ってしまい、「上限超過は常に `54000`」という
        // エラー契約（本メソッドドキュメント参照）に反する。
        let embedder = self.embedder.as_deref().ok_or_else(|| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "no embedder configured for file-form insert".to_string(),
            }
        })?;

        let mut items: Vec<crate::incremental::BatchFileIndexItem<'_>> = Vec::new();
        items.try_reserve_exact(file_binds.len()).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "failed to reserve batch item buffer".to_string(),
            }
        })?;
        for bound in &file_binds {
            let ledger_write = self
                .ledger_mode
                .resolve(bound.operation_id.as_ref())
                .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;
            let input = crate::incremental::BoundFileIndexInput {
                table: &bound.table,
                path: &bound.path,
                body: &bound.body,
                template_values: &bound.template_values,
                path_column_index: bound.path_column_index,
                body_column_index: bound.body_column_index,
                vector_column_index: bound.vector_column_index,
            };
            items.push(crate::incremental::BatchFileIndexItem {
                input,
                ledger_write,
            });
        }

        let outcomes = crate::incremental::index_file_batch(
            &self.storage,
            ctx,
            embedder,
            &self.incremental_config,
            &self.batch_limits,
            items,
        )
        .map_err(crate::sql::exec::map_batch_incremental_error)?;

        Ok(outcomes
            .into_iter()
            .map(|outcome| crate::sql::exec::InsertOutcome {
                rows_affected: outcome.rows_replaced as u64,
                incremental: Some(outcome),
            })
            .collect())
    }
}

impl VectorCore for EngineCore {
    fn search(
        &self,
        ctx: &PolicyContext,
        table: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, CoreError> {
        validate_search_k(k).map_err(|k| CoreError::InvalidK { k })?;

        // `query` の次元をカタログ照会だけで早期検証する（`VectorArena::build` へ進む前）。
        // `VectorArena::build` は対象テーブル全行（最大 `MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES`）
        // をデコード・確保してから初めて provider（`kernel.rs::SearchProvider` 実装。既定は
        // `parallel_search.rs::ParallelSearchProvider`）が次元不一致を検出する構造だと、次元不一致
        // という軽量に判定できる入力であっても
        // 全行デコード分のコスト（リソース増幅）を強いられてしまう
        // （security.md「不安全な設計｜無制限リソース確保（DoS）」対応。Issue #32 レビュー
        // 指摘）。ここでの早期拒否はエラー契約を変えず、`kernel.rs` が返すのと同じ
        // `KernelError::DimMismatch` を用いる。
        //
        // テーブル不存在は [`Self::get_row`] と対称に [`CoreError::NotFound`] へ丸め込む
        // （存在情報を漏らさない。security.md「アクセス制御の不備」）。それ以外のカタログ
        // エラー（データ破損等）はそのまま伝播する。
        let schema = match self.storage.get_table_schema(table) {
            Ok(schema) => schema,
            Err(CatalogError::TableNotFound(_)) => return Err(CoreError::NotFound),
            Err(e) => return Err(CoreError::Catalog(e)),
        };
        let expected_dim = match schema.vector_dim() {
            Some(dim) if dim != 0 && dim <= crate::storage::MAX_EMBEDDING_DIM => dim,
            _ => return Err(CoreError::Arena(ArenaError::InvalidDim)),
        };
        if query.len() != expected_dim as usize {
            return Err(CoreError::Kernel(KernelError::DimMismatch {
                expected: expected_dim,
                found: query.len(),
            }));
        }
        // 次元検証と同じ理由で、非有限（NaN・Inf）query も `VectorArena::build` へ進む前に
        // 早期拒否する（Cursor Bugbot Medium 指摘・Issue #32 #137）。`query` は wire 経路
        // からの untrusted 入力であり得るため、正しい次元であっても
        // provider 側の検証（`KernelError::NonFiniteQuery`。`kernel.rs::CpuScalarProvider`・
        // `parallel_search.rs::ParallelSearchProvider` 共通の契約）だけに委ねると、次元一致・値だけ
        // 不正なクエリで同種のリソース増幅（全行デコード後に
        // 拒否）が残る。ここでの早期拒否はエラー契約を変えず、`kernel.rs` が返すのと同じ
        // `KernelError::NonFiniteQuery` を用いる。
        if query.iter().any(|v| !v.is_finite()) {
            return Err(CoreError::Kernel(KernelError::NonFiniteQuery));
        }

        // 実行経路の決定（TASK-155・対象ビヘイビア: CORE-11, CORE-12）。単発クエリ経路は
        // 待機キューを持たない（`pending_after_pop` は常に `false`）ため決定表は常に
        // `ExecutionPath::CpuSimd` を返す。`DispatchInput::for_single_query` は GPU
        // capability を引数に取らない設計のため `ExecutionPath::Gpu` は構造的に
        // 到達しないが、決定表が返しうる全 variant を網羅させることで、将来
        // 単発クエリ経路へ GPU capability を持ち込む変更が発生した際にコンパイル
        // エラーで検出できるようにする（`dispatch.rs` モジュールドキュメント参照）。
        let dispatch_input = DispatchInput::for_single_query(expected_dim as usize, false)?;
        match dispatch::select_execution_path(dispatch_input)? {
            ExecutionPath::CpuSimd { .. } => {}
            ExecutionPath::Gpu => return Err(CoreError::GpuPathUnavailable),
        }

        // 次元・有限性検証を通過した後にのみキャッシュ照合・アリーナ構築へ進む
        // （TASK-169）。まず [`PrefilterCache::lookup`] で `(table, ctx)` に一致し
        // 世代整合が取れているスナップショットを探す。ヒットすればそのまま
        // `PrefilterSnapshot::search_with` を呼ぶ（`core.rs` モジュールドキュメント・
        // [`PrefilterCache`] のドキュメント参照）。
        if let Some(snapshot) = self.prefilter_cache.lookup(&self.storage, table, ctx) {
            return self.search_with_snapshot(table, ctx, query, k, snapshot);
        }

        // キャッシュミス: `PrefilterSnapshot::build` で新規構築する。ここでの
        // `RlsError::NotFound` は上記の早期照会と同一スナップショットではない
        // （別トランザクション）ため、直前の照会成立後にテーブルが削除された場合の
        // 理論的な競合窓のみで発生しうる。その場合も同様に存在情報を漏らさず
        // `NotFound` へ丸め込む。
        //
        // `PrefilterSnapshot::build` は内部で `VectorArena::build_filtered` へ
        // `ctx.is_visible` をそのまま述語として渡し、不可視行（他テナント行を含む）を
        // アリーナ構築時点で確保しない（codex P0/P2・Issue #137 対応の構築時フィルタを
        // 引き継ぐ。`arena.rs`・`rls.rs` のドキュメント参照）。
        let snapshot = match PrefilterSnapshot::build(&self.storage, table, ctx) {
            Ok(snapshot) => snapshot,
            Err(RlsError::NotFound) => return Err(CoreError::NotFound),
            Err(e) => return Err(map_rls_error(e)),
        };
        self.search_with_built_snapshot(table, ctx, query, k, snapshot)
    }

    fn get_row(
        &self,
        ctx: &PolicyContext,
        table: &str,
        tenant_id: &str,
        id: u64,
    ) -> Result<Row, CoreError> {
        // 行 `id` の一意性スコープはテナント内（対象ビヘイビア: TABLE-12）のため、
        // 点取得のキーは `(tenant_id, id)`。`tenant_id` は検索結果
        // （[`crate::kernel::SearchHit::tenant_id`]）等から呼び出し元が渡す行の帰属で、
        // 認可には一切使わない（認可・可視性判定は下の `is_visible` の単一照合パスのみ）。
        // 存在しない行・`ctx` から不可視な行（他テナントの `Private` 行を含む）は
        // 区別せず `NotFound` に統一するため、他テナント行の存在探査には使えない
        // （fail-closed。RLS-9・security.md P0）。
        let row = match self.storage.get_row_from_table(table, tenant_id, id) {
            Ok(row) => row,
            // テーブル不存在・行不存在はいずれも「不可視と不存在を区別しない」契約に
            // 合流させる。それ以外（デコード不正等のデータ破損・バックエンドエラー）は
            // `NotFound` に丸め込まず `CoreError::Catalog` としてそのまま伝播する
            // （アクセス不可とデータ破損を区別する）。`search` 経路もテーブル不存在
            // （`ArenaError::Catalog(CatalogError::TableNotFound)`）を同じく `NotFound` へ
            // 丸め込んでおり、両経路は対称（Issue #32 レビュー指摘対応）。
            Err(CatalogError::TableNotFound(_) | CatalogError::RowNotFound(_)) => {
                return Err(CoreError::NotFound)
            }
            Err(e) => return Err(CoreError::Catalog(e)),
        };
        if !ImplicitRlsHook::new(ctx).is_visible(&row.tenant_id, row.visibility) {
            return Err(CoreError::NotFound);
        }
        Ok(row)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::storage::{RowInput, Visibility};

    fn schema_for(table_name: &str, dim: u32) -> TableSchema {
        TableSchema::new(
            table_name,
            vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
        )
    }

    fn new_core(dir: &std::path::Path) -> EngineCore {
        EngineCore::open(dir.join("db.redb")).expect("open engine core")
    }

    // 対象ビヘイビア: CORE-1。object-safety の固定（プロトコルアダプタは `&dyn VectorCore`
    // のみを介して呼び出せる）。
    fn _assert_object_safe(_: &dyn VectorCore) {}

    #[test]
    fn search_rejects_k_zero_and_over_limit() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        assert!(matches!(
            core.search(&ctx, "docs", &[1.0, 0.0], 0),
            Err(CoreError::InvalidK { k: 0 })
        ));
        assert!(matches!(
            core.search(&ctx, "docs", &[1.0, 0.0], MAX_SEARCH_K + 1),
            Err(CoreError::InvalidK { .. })
        ));
    }

    #[test]
    fn search_on_empty_table_returns_empty() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let hits = core
            .search(&ctx, "docs", &[1.0, 0.0], 5)
            .expect("search ok");
        assert!(hits.is_empty());
    }

    #[test]
    fn get_row_not_found_and_invisible_are_indistinguishable() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let not_found = core.get_row(&ctx, "docs", "tenant-a", 999);
        let invisible = core.get_row(&ctx, "docs", "tenant-a", 1);
        assert!(matches!(not_found, Err(CoreError::NotFound)));
        assert!(matches!(invisible, Err(CoreError::NotFound)));
    }

    // 対象ビヘイビア: RECOVER-4。`EngineCore::update_row`/`delete_row` は
    // `crate::tenant` の書き込みガードへ委譲するだけであることの結合確認
    // （テナント境界の実質的な検証は `tests/tenant_breach.rs` 側。ここでは
    // `EngineCore` からの委譲経路そのものが `NotFound` を返すことだけを確認する）。
    #[test]
    fn update_and_delete_row_reject_other_tenant_as_not_found() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        let attacker = PolicyContext::new("tenant-a").expect("valid tenant");
        let update_input = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[0.0, 1.0],
            metadata: &[],
        };
        let op_id = OperationId::parse("op-update-delete-not-found").expect("valid operation_id");
        assert!(matches!(
            core.update_row(&attacker, "docs", 1, &update_input, Some(&op_id)),
            Err(crate::tenant::TenantWriteError::NotFound)
        ));
        assert!(matches!(
            core.delete_row(&attacker, "docs", 1, Some(&op_id)),
            Err(crate::tenant::TenantWriteError::NotFound)
        ));

        // データが不変であることを確認する（拒否が実際に永続化を止めていること）。
        let row = core
            .storage
            .get_row_from_table("docs", "tenant-b", 1)
            .expect("row still present");
        assert_eq!(row.tenant_id, "tenant-b");
    }

    // 対象ビヘイビア: RLS-1〜4（TASK-169）。同一 `(table, ctx)` での 2 回目の検索は
    // キャッシュヒットし、結果は 1 回目と同一になる。
    #[test]
    fn search_second_call_with_same_table_and_ctx_hits_the_cache() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let first = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("first search ok");
        let second = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("second search ok");

        let stats = core.prefilter_cache_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(first.len(), second.len());
        assert_eq!(first[0].id, second[0].id);
    }

    // 対象ビヘイビア: RLS-1〜4（TASK-169）。キャッシュ温存中に書き込みが発生すると
    // （世代が進むと）、以後の検索は新しい行を含む結果を返す（stale なインデックスの
    // 結果を返す経路が存在しないことの確認）。
    #[test]
    fn search_reflects_a_write_committed_after_the_cache_was_populated() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let first = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("first search ok");
        assert_eq!(first.len(), 1);

        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        let second = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("second search ok");
        assert_eq!(second.len(), 2);

        let stats = core.prefilter_cache_stats();
        assert!(stats.stale_evictions >= 1);
    }

    // 対象ビヘイビア: RLS-1〜4（TASK-169）。別テーブルへの書き込みでも世代（単一
    // カウンタ）が進むため、無関係なテーブルのキャッシュも失効する
    // （過剰拒否＝fail-closed 方向。過小拒否ではない）。
    #[test]
    fn search_cache_is_invalidated_by_a_write_to_a_different_table() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .create_table(&schema_for("other", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        core.search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("first search ok");

        core.storage
            .insert_row_into_table(
                "other",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row into other table");

        core.search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("second search ok");

        let stats = core.prefilter_cache_stats();
        assert!(stats.stale_evictions >= 1);
        assert_eq!(stats.misses, 2);
    }

    // Issue #179: `drop_table` → 同名・同次元での再作成をまたぐと、drop 前に充填した
    // `PrefilterCache` エントリは世代不一致で破棄され、再作成後は新規行のみを返す
    // （旧行が混入する経路がないことの確認）。
    #[test]
    fn search_cache_is_invalidated_by_drop_table_and_recreate() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let first = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("first search ok");
        assert_eq!(first.len(), 1);

        core.storage.drop_table("docs").expect("drop table");
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("recreate table");
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row after recreate");

        let second = core
            .search(&ctx, "docs", &[1.0, 0.0], 10)
            .expect("second search ok after drop and recreate");
        let ids: Vec<u64> = second.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![2]);

        let stats = core.prefilter_cache_stats();
        assert!(stats.stale_evictions >= 1);
        assert_eq!(stats.misses, 2);
    }

    // Issue #179: drop 後に再作成しないテーブルへの検索は、既存の
    // 「存在情報を漏らさない」契約どおり `CoreError::NotFound` に丸め込まれる。
    #[test]
    fn search_returns_not_found_after_drop_without_recreate() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        core.storage.drop_table("docs").expect("drop table");

        let result = core.search(&ctx, "docs", &[1.0, 0.0], 10);
        assert!(matches!(result, Err(CoreError::NotFound)));
    }

    // 対象ビヘイビア: RLS-1（TASK-169）。異なる `PolicyContext`（別テナント）は別の
    // キャッシュエントリになり、互いの検索結果に混入しない。
    #[test]
    fn search_with_different_tenants_uses_separate_cache_entries_without_leakage() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        // `Private` は同一テナントの行のみ可視（`Public` は `policy.rs::is_visible` の
        // 契約上テナント跨ぎで可視になるため、テナント境界だけを分離要因にするには
        // `Private` を使い、`with_visibilities` で明示的に許可する必要がある）。
        let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        let ctx_b = PolicyContext::with_visibilities("tenant-b", [Visibility::Private])
            .expect("valid tenant");

        let hits_a = core
            .search(&ctx_a, "docs", &[1.0, 0.0], 10)
            .expect("search ok for tenant-a");
        let hits_b = core
            .search(&ctx_b, "docs", &[1.0, 0.0], 10)
            .expect("search ok for tenant-b");

        assert_eq!(hits_a.len(), 1);
        assert_eq!(hits_a[0].id, 1);
        assert_eq!(hits_b.len(), 1);
        assert_eq!(hits_b[0].id, 2);

        let stats = core.prefilter_cache_stats();
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.entries, 2);
    }

    // 対象ビヘイビア: TASK-169。テーブル不存在は非キャッシュ経路と同じく
    // `CoreError::NotFound` へ丸め込まれる（キャッシュ経路でも存在情報を漏らさない）。
    #[test]
    fn search_not_found_is_preserved_through_the_cache_path() {
        let dir = tempdir();
        let core = new_core(dir.path());
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        assert!(matches!(
            core.search(&ctx, "missing", &[1.0, 0.0], 10),
            Err(CoreError::NotFound)
        ));
    }

    // 対象ビヘイビア: TASK-169（security.md「不安全な設計｜無制限リソース確保
    // （DoS）」対応）。異なる `(table, ctx)` の組を上限超過分だけ検索しても、
    // エントリ数は `MAX_PREFILTER_CACHE_ENTRIES` を超えず、超過分は LRU で追い出される。
    #[test]
    fn search_cache_entries_stay_within_the_configured_capacity() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        for i in 0..(MAX_PREFILTER_CACHE_ENTRIES + 1) {
            let tenant = format!("tenant-{i}");
            let ctx = PolicyContext::new(&tenant).expect("valid tenant");
            core.search(&ctx, "docs", &[1.0, 0.0], 10)
                .expect("search ok");
        }

        let stats = core.prefilter_cache_stats();
        assert!(stats.entries <= MAX_PREFILTER_CACHE_ENTRIES);
        assert!(stats.capacity_evictions >= 1);
    }

    // 対象ビヘイビア: TASK-169（Cursor Bugbot 指摘の回帰テスト）。`PrefilterCache::insert`
    // が挿入対象スナップショット自身の世代を「現在世代」の代用にすると、並行書き込みで
    // 既に古くなったスナップショットを挿入した際、真に新しい既存エントリまで
    // 世代不一致として誤って全破棄してしまう（キャッシュが不当に空になる）。
    // `storage.current_generation()` を正として世代整合を判定していれば、後から書き込み
    // 前提で構築された（結果的に古い）スナップショットを挿入しても、既存の新しい
    // エントリは破棄されず残る。加えて（後続の Cursor Bugbot 指摘・PR #191）、
    // 挿入対象自身が現在世代と不一致（= stale）の場合はそもそもキャッシュへ反映しない
    // （型ドキュメント (0) 参照）。これにより、並行構築中の古いスナップショットの
    // 挿入が「同一キーの既存エントリを一旦取り除いてから push する」経路を経由して
    // 別スレッドが直前に挿入した現在世代の有効エントリを上書き・削除する不具合を防ぐ。
    #[test]
    fn search_insert_does_not_evict_a_newer_entry_using_a_stale_snapshots_own_generation() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // 世代 G0 時点で 2 つの異なるテナント向けスナップショットを構築しておく
        // （まだキャッシュへは挿入しない）。
        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let stale_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx_a).expect("build snapshot a");

        // tenant-b 側は通常の検索経路でキャッシュへ挿入し、世代 G0 のエントリとして
        // 常駐させる。
        core.search(&ctx_b, "docs", &[1.0, 0.0], 10)
            .expect("search ok for tenant-b");
        assert_eq!(core.prefilter_cache_stats().entries, 1);

        // 書き込みで世代を G0 → G1 へ進める（`stale_snapshot` は G0 のまま古くなる）。
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // tenant-b のエントリは既に世代 G1 で再構築済み（`search` がキャッシュミスから
        // 再挿入する）とみなし、まず現在の状態を G1 に揃える。
        core.search(&ctx_b, "docs", &[1.0, 0.0], 10)
            .expect("search ok for tenant-b after write");
        assert_eq!(
            core.prefilter_cache_stats().entries,
            1,
            "tenant-b の再構築後エントリが 1 件残っていること"
        );

        // 古い（G0 のままの）`stale_snapshot` を挿入しても、既存の G1 エントリ
        // （tenant-b）が誤って破棄されないこと（Cursor Bugbot 指摘の再現条件）。
        // かつ、挿入対象自身が stale なため tenant-a のエントリとしても追加されない
        // こと（後続の Cursor Bugbot 指摘の修正: stale な挿入はキャッシュへ一切
        // 反映しない）。
        let inserted = core
            .prefilter_cache
            .insert(&core.storage, "docs", &ctx_a, stale_snapshot);
        assert!(
            inserted.is_none(),
            "挿入対象自身が stale な場合は呼び出し元へも渡さない（Issue #280）"
        );
        let stats = core.prefilter_cache_stats();
        assert_eq!(
            stats.entries, 1,
            "stale な挿入は既存の新しいエントリを失わせてはならず、自身も追加されない"
        );
    }

    // 対象ビヘイビア: TASK-169（Cursor Bugbot 指摘の回帰テスト）。`PrefilterCache::insert`
    // は同一 `(table, ctx)` キーへの挿入で既存エントリを置換し、重複エントリを
    // 積み上げない（[`PrefilterCache::lookup`] は先頭一致のみ参照するため、重複が
    // 残ると無駄に `MAX_PREFILTER_CACHE_ENTRIES` を消費し続ける）。
    #[test]
    fn search_insert_replaces_an_existing_entry_for_the_same_key_instead_of_duplicating() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 同一 (table, ctx) キーへ複数回挿入する（両者とも現在世代 G0 と一致するため
        // `insert` に受理される。重複判定の再現には世代不一致は不要）。
        let snapshot_1 =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot 1");
        let snapshot_2 =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot 2");

        core.prefilter_cache
            .insert(&core.storage, "docs", &ctx, snapshot_1)
            .expect("current generation の snapshot は受理される");
        core.prefilter_cache
            .insert(&core.storage, "docs", &ctx, snapshot_2)
            .expect("current generation の snapshot は受理される");

        assert_eq!(
            core.prefilter_cache_stats().entries,
            1,
            "同一キーへの再挿入は既存エントリを置換し、重複登録してはならない"
        );
    }

    // 対象ビヘイビア: TASK-169（PR #191 codex-review 指摘の回帰テスト）。同一
    // `(table, ctx)` キーに対する並行構築時、旧世代（stale）のスナップショットの
    // 挿入が、既に挿入済みの新しい世代の有効エントリを「同一キーだから」という理由で
    // 上書き・削除してはならない。`search_with` 側の世代検証で stale 結果が漏れる
    // ことはないが、有効なキャッシュを失うと以降の検索が非キャッシュ経路へ縮退する
    // （型ドキュメント (0) 参照）。
    #[test]
    fn search_insert_does_not_overwrite_a_fresher_entry_for_the_same_key_with_a_stale_one() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 世代 G0 でスナップショットを構築しておく（並行スレッドの遅延構築を模す。
        // まだキャッシュへは挿入しない）。
        let stale_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g0");

        // 書き込みで世代を G0 → G1 へ進める。
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // 別スレッド（を模した経路）が同一キーで G1 のスナップショットを先に
        // キャッシュへ挿入済みとする。
        let fresh_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g1");
        let fresh_arc = core
            .prefilter_cache
            .insert(&core.storage, "docs", &ctx, fresh_snapshot)
            .expect("current generation の snapshot は受理される");
        assert_eq!(core.prefilter_cache_stats().entries, 1);

        // 遅れて到着した G0 の stale スナップショットを同一キーへ挿入しても、
        // 既にキャッシュされている G1 の有効エントリが上書き・削除されないこと。
        // 挿入対象自身は stale なので呼び出し元へも渡されない（Issue #280）。
        let stale_inserted =
            core.prefilter_cache
                .insert(&core.storage, "docs", &ctx, stale_snapshot);
        assert!(
            stale_inserted.is_none(),
            "stale な挿入は呼び出し元へも渡さない"
        );

        let cached = core
            .prefilter_cache
            .lookup(&core.storage, "docs", &ctx)
            .expect("同一キーの G1 エントリがキャッシュに残っていること");
        assert!(
            Arc::ptr_eq(&cached, &fresh_arc),
            "stale な挿入によって G1 の有効エントリが差し替えられてはならない"
        );
    }

    // 対象ビヘイビア: TASK-169・RLS-1〜4・Issue #280（判別テスト。`DictionaryCache::insert`
    //〔TASK-109・PR #249〕と同じ「スキャン中に世代が進んだ場合は stale な挿入を `None` で
    // 拒否する」契約を `PrefilterCache::insert` にも適用したことの確認）。世代 G0 で構築した
    // スナップショットを、書き込みで G0 → G1 へ進めた後に挿入すると `None` が返り、
    // キャッシュへも反映されないこと。対照として、G1 で構築し直したスナップショットの
    // 挿入は `Some` を返し、`lookup` で同一 `Arc` が引けること（拒否が過剰でないことの
    // 確認）。
    #[test]
    fn prefilter_cache_insert_rejects_a_stale_snapshot_when_generation_advances_during_build() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 世代 G0 でスナップショットを構築する（読み取り中に並行書き込みが割り込む
        // 状況を模す。まだキャッシュへは挿入しない）。
        let stale_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g0");

        // 書き込みで世代を G0 → G1 へ進める（構築完了〜挿入の間に世代が進む競合）。
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // G0 のまま stale になったスナップショットの挿入は拒否される（`None`）。
        let rejected = core
            .prefilter_cache
            .insert(&core.storage, "docs", &ctx, stale_snapshot);
        assert!(
            rejected.is_none(),
            "世代不一致の挿入対象は呼び出し元へも渡さない（DictionaryCache::insert と同契約）"
        );
        assert_eq!(
            core.prefilter_cache_stats().entries,
            0,
            "stale な挿入はキャッシュへ反映されない"
        );

        // 対照: G1 で構築し直したスナップショットは受理され、`lookup` で同一 `Arc`
        // が引けること（拒否が過剰でないことの確認）。
        let fresh_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g1");
        let fresh_arc = core
            .prefilter_cache
            .insert(&core.storage, "docs", &ctx, fresh_snapshot)
            .expect("current generation の snapshot は受理される");
        assert_eq!(core.prefilter_cache_stats().entries, 1);
        let cached = core
            .prefilter_cache
            .lookup(&core.storage, "docs", &ctx)
            .expect("G1 のエントリがキャッシュに残っていること");
        assert!(Arc::ptr_eq(&cached, &fresh_arc));
    }

    // 対象ビヘイビア: TASK-169・RLS-1〜4・Issue #280（受け入れ条件3: 結果側の
    // fail-closed が変わらないことの確認）。`PrefilterSnapshot::build` 完了後、
    // `PrefilterCache::insert` へ渡すまでの間に別の書き込みが世代を進め、挿入対象の
    // スナップショットが stale になったケースを、`EngineCore::search_with_built_snapshot`
    // を直接呼ぶことで決定的に再現する。`insert` が `None` を返し、`search_uncached`
    // へ 1 回だけ縮退することで、stale なスナップショットではなく最新状態で検索
    // される（クエリに最も近い、書き込み後に追加された行が結果へ含まれる）ことを
    // 確認する。
    #[test]
    fn search_falls_back_to_uncached_when_a_freshly_built_snapshot_turns_stale_before_insert() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 世代 G0（行 id=1 のみ）でスナップショットを構築する（まだ挿入しない）。
        let stale_snapshot =
            PrefilterSnapshot::build(&core.storage, "docs", &ctx).expect("build snapshot g0");

        // クエリに最も近いベクトルを持つ行 id=2 を書き込み、世代を G0 → G1 へ進める。
        // `stale_snapshot` は行 id=2 を含まないアリーナのまま古くなる。
        core.storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.0, 1.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // 構築済みの G0 スナップショットをそのまま渡す（`VectorCore::search` の
        // キャッシュミス経路が「構築 → 挿入」の間で競合に遭った状況を模す）。
        let hits = core
            .search_with_built_snapshot("docs", &ctx, &[0.0, 1.0], 10, stale_snapshot)
            .expect("search ok");

        assert!(
            hits.iter().any(|h| h.id == 2),
            "stale スナップショットではなく最新状態（行 id=2 を含む）で検索されること"
        );
        assert_eq!(
            core.prefilter_cache_stats().entries,
            0,
            "stale なスナップショットはキャッシュへ常駐しない"
        );
    }

    // 対象ビヘイビア: TASK-169（Issue #137 系の provider 検証をキャッシュ経路でも
    // 維持することの確認）。不正な結果を返す provider は、キャッシュヒット経路でも
    // `ProviderResultRejected` で拒否される。
    #[test]
    fn search_rejects_a_rogue_provider_result_even_on_a_cache_hit() {
        struct RogueProvider;
        impl SearchProvider for RogueProvider {
            fn search(&self, _input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
                // アリーナ外の id を返す不正 provider。
                Ok(vec![CandidateHit {
                    id: 999,
                    score: 1.0,
                }])
            }
        }

        let dir = tempdir();
        let core = EngineCore::with_provider(dir.path().join("db.redb"), Box::new(RogueProvider))
            .expect("open engine core with rogue provider");
        core.storage
            .create_table(&schema_for("docs", 2))
            .expect("create table");
        core.storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 1 回目（キャッシュミス経由）・2 回目（キャッシュヒット経由）のいずれも拒否する。
        assert!(matches!(
            core.search(&ctx, "docs", &[1.0, 0.0], 10),
            Err(CoreError::ProviderResultRejected)
        ));
        assert!(matches!(
            core.search(&ctx, "docs", &[1.0, 0.0], 10),
            Err(CoreError::ProviderResultRejected)
        ));
    }

    fn documents_schema() -> TableSchema {
        TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    // 対象ビヘイビア: TASK-109・PLAN-5（security.md「不安全な設計｜無制限リソース
    // 確保（DoS）」対応。`search_cache_entries_stay_within_the_configured_capacity`
    // 〔TASK-169〕と同じ意図・手法）。異なる `(table, ctx)` の組を上限超過分だけ
    // 構築しても、`DictionaryCache` のエントリ数は `MAX_DICTIONARY_CACHE_ENTRIES` を
    // 超えず、超過分は `last_used` 最小の LRU エントリから追い出される
    // （`DictionaryCache::insert` の追い出し分岐の回帰テスト）。
    #[test]
    fn dictionary_cache_entries_stay_within_the_configured_capacity() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&documents_schema())
            .expect("create table");

        for i in 0..(MAX_DICTIONARY_CACHE_ENTRIES + 1) {
            let tenant = format!("tenant-{i}");
            let ctx = PolicyContext::new(&tenant).expect("valid tenant");
            core.dictionary_snapshot(&ctx, "documents")
                .expect("dictionary snapshot ok");
        }

        let guard = core
            .dictionary_cache
            .state
            .read()
            .expect("cache lock not poisoned");
        // 上限ちょうどまで縮退していること（`<=` では「1 件も追い出されない」実装への
        // 退行を検知できないため `==` で固定する）。
        assert_eq!(guard.entries.len(), MAX_DICTIONARY_CACHE_ENTRIES);
        // 挿入順が `last_used` 昇順と一致するため、最古（tenant-0）が LRU 追い出しの
        // 対象になり、最新（tenant-{MAX}）は生存しているはずである
        // （どのエントリが追い出されたかまで検証し、件数一致だけの弱い保証を補う）。
        assert!(
            !guard
                .entries
                .iter()
                .any(|e| e.ctx.tenant_id() == "tenant-0"),
            "oldest entry (tenant-0) must be evicted by LRU"
        );
        let newest_tenant = format!("tenant-{MAX_DICTIONARY_CACHE_ENTRIES}");
        assert!(
            guard
                .entries
                .iter()
                .any(|e| e.ctx.tenant_id() == newest_tenant),
            "newest entry must survive LRU eviction"
        );
    }

    // 対象ビヘイビア: TASK-109・PLAN-5（`PrefilterCache::insert` の同種修正・
    // `search_insert_does_not_evict_a_newer_entry_using_a_stale_snapshots_own_generation`
    // 〔TASK-169〕と同じ意図・手法）。`DictionaryCache::insert` は挿入対象自身の
    // `built_generation` が `storage.current_generation()` と不一致（＝並行書き込みで
    // 既に古くなった）場合、既存の新しいエントリを破棄せず・自身も追加しない
    // （`DictionaryCache::insert` の世代不一致分岐の回帰テスト）。
    #[test]
    fn dictionary_cache_insert_does_not_evict_a_newer_entry_using_a_stale_generation() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&documents_schema())
            .expect("create table");

        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

        // 世代 G0 時点の辞書を構築しておく（まだキャッシュへは挿入しない）。
        let stale_generation = core.storage.current_generation().expect("read generation");
        let stale_dictionary =
            crate::dictionary::DictionaryBuilder::new(core.dictionary_config.clone()).finish();
        let stale_bytes = stale_dictionary.approx_heap_bytes();

        // tenant-b 側は通常経路でキャッシュへ挿入し、世代 G0 のエントリとして常駐させる。
        core.dictionary_snapshot(&ctx_b, "documents")
            .expect("dictionary snapshot ok for tenant-b");
        assert_eq!(
            core.dictionary_cache
                .state
                .read()
                .expect("cache lock not poisoned")
                .entries
                .len(),
            1
        );

        // 書き込みで世代を G0 → G1 へ進める（`stale_dictionary`/`stale_generation` は
        // G0 のまま古くなる）。
        core.storage
            .insert_row_into_table(
                "documents",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        // 古い（G0 のままの）辞書を挿入しても、既存の G0 エントリ（tenant-b）が
        // 誤って破棄されないこと。かつ、挿入対象自身が stale なため tenant-a の
        // エントリとしても追加されないこと。戻り値も `None`（PR #249
        // codex-review P1 指摘対応: 呼び出し元へ書き込み未反映のスナップショットを
        // 渡さないための契約。従来は `Arc` をそのまま返しており、
        // `EngineCore::dictionary_snapshot` がこれを正常結果として扱っていた）。
        let result = core.dictionary_cache.insert(
            &core.storage,
            "documents",
            &ctx_a,
            stale_dictionary,
            stale_generation,
            stale_bytes,
        );
        assert!(
            result.is_none(),
            "insert of an already-stale dictionary must return None, not the stale Arc"
        );
        let entries = core
            .dictionary_cache
            .state
            .read()
            .expect("cache lock not poisoned");
        assert_eq!(
            entries.entries.len(),
            1,
            "stale な挿入は既存の新しいエントリを失わせてはならず、自身も追加されない"
        );
        assert_eq!(entries.entries[0].ctx, ctx_b);
    }

    // 対象ビヘイビア: TASK-109・PLAN-5（PR #249 codex-review P1 指摘の回帰テスト）。
    // 可視行の `metadata`（スカラー列ペイロード）が `path`/`body`（非 nullable Text）
    // をデコードできないほど破損している場合、その行を黙ってスキップして内容を
    // 欠いた `Dictionary` を `truncated: false` のまま正常なキャッシュエントリと
    // して保存してはならない。`dictionary_snapshot` がエラーを返し、かつ
    // `DictionaryCache` にエントリが残らないことを確認する。
    #[test]
    fn dictionary_snapshot_fails_closed_on_corrupted_row_and_does_not_cache_it() {
        let dir = tempdir();
        let core = new_core(dir.path());
        core.storage
            .create_table(&documents_schema())
            .expect("create table");
        // `path`/`body` の presence タグすら読めない空のスカラーペイロード
        // （`row_codec::scan_scalar_columns` は非 nullable 列の途中で打ち切られた
        // バッファを `RowCodecError::Invalid` として拒否する）。
        core.storage
            .insert_row_into_table(
                "documents",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert corrupted row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let err = core
            .dictionary_snapshot(&ctx, "documents")
            .expect_err("corrupted visible row must fail dictionary snapshot construction");
        assert!(matches!(err, CoreError::Catalog(CatalogError::Invalid(_))));

        let guard = core
            .dictionary_cache
            .state
            .read()
            .expect("cache lock not poisoned");
        assert_eq!(
            guard.entries.len(),
            0,
            "a failed build must not leave a cache entry behind"
        );
    }

    // 対象ビヘイビア: TASK-109・PLAN-5（PR #249 codex-review P1 指摘の回帰テスト）。
    // `with_dictionary_config` は `dictionary_config` を差し替えるだけでは足りず、
    // 既構築の `dictionary_cache` も再初期化しなければならない。`(table, ctx)` と
    // `storage.current_generation()` のみをキーにするキャッシュは設定値を含まない
    // ため、世代が変わらない限り旧設定で構築した `Arc<Dictionary>` を返し続けて
    // しまう（設定変更が効かない見えないバグ）。
    #[test]
    fn with_dictionary_config_invalidates_stale_cache_entries() {
        let dir = tempdir();
        let core = new_core(dir.path());
        let schema = documents_schema();
        core.storage.create_table(&schema).expect("create table");
        let values = vec![
            crate::row_codec::Value::Null,
            crate::row_codec::Value::Text("src/example.rs".to_string()),
            crate::row_codec::Value::Text("fn one() {}\nfn two() {}\n".to_string()),
        ];
        let metadata =
            crate::row_codec::encode_scalar_columns(&schema, &values).expect("encode metadata");
        core.storage
            .insert_row_into_table(
                "documents",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &metadata,
                },
            )
            .expect("insert row");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        // 既定設定（ファイルツリーを有効）で一度構築し、キャッシュへ載せておく。
        let dict_before = core
            .dictionary_snapshot(&ctx, "documents")
            .expect("dictionary snapshot ok before config change");
        assert!(!dict_before.file_tree.paths.is_empty());
        assert_eq!(
            core.dictionary_cache
                .state
                .read()
                .expect("cache lock not poisoned")
                .entries
                .len(),
            1
        );

        // ファイルツリーを無効化する設定へ差し替える。世代は変わっていないため、
        // キャッシュを再初期化していなければ次の呼び出しが旧設定の
        // `Arc<Dictionary>` をそのまま返してしまう。
        let core = core.with_dictionary_config(crate::dictionary::DictionaryConfig {
            enable_file_tree: false,
            enable_term_index: false,
            ..crate::dictionary::DictionaryConfig::default()
        });
        assert_eq!(
            core.dictionary_cache
                .state
                .read()
                .expect("cache lock not poisoned")
                .entries
                .len(),
            0,
            "with_dictionary_config must invalidate the existing dictionary cache"
        );

        let dict_after = core
            .dictionary_snapshot(&ctx, "documents")
            .expect("dictionary snapshot ok after config change");
        assert!(
            dict_after.file_tree.paths.is_empty(),
            "the new config (file tree disabled) must take effect, not a stale cached dictionary"
        );
    }

    // codex-review P1 回帰（PR #266）: `USING PLAN` の I/O フェーズ
    // （`plan_using_plan_expansion`）はスキーマに依存せず、列インデックス解決
    // （`sql::using_plan::bind_expansion`）は I/O 完了後に取得し直した最新スキーマ
    // に対して行う。以前は I/O 前に取得した旧スキーマで列インデックスを含む
    // `BoundStatement` を確定していたため、I/O 中に同名テーブルの
    // `DROP TABLE`→再作成でレイアウトが変わると、束縛済みの列インデックスが
    // 別の列を指す状態になり得た（`execute_sql_in_session` の `Statement::Select`
    // アーム・[`EngineCore::plan_using_plan_expansion`] のドキュメント参照）。
    // ここでは `execute_sql_in_session` と同じ 2 段階（I/O → 束縛）を直接呼び出し、
    // 段階間に DDL を挟んで、列インデックスが post-I/O スキーマを反映することを
    // 固定する。
    #[test]
    fn using_plan_binds_column_indices_against_the_schema_fetched_after_io_not_before() {
        struct StubLlmClient;
        impl crate::query_planner::LlmClient for StubLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }))
            .with_query_planner(Box::new(StubLlmClient));

        // 旧レイアウト: [embedding, path, body]（body index = 2）。
        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table (old layout)");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id_old = OperationId::parse("using-plan-race-old").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("old-path".to_string()),
                crate::row_codec::Value::Text("old-body".to_string()),
            ],
            &op_id_old,
        )
        .expect("insert row (old layout)");

        let session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let stmt = crate::sql::allowlist::validate_sql(sql, &core.storage).expect("valid sql");
        let validated = match stmt {
            crate::sql::allowlist::Statement::Select(v) => v,
            other => panic!("expected Select statement, got {other:?}"),
        };
        let question = validated.using_plan().expect("USING PLAN present");

        // I/O フェーズ（スキーマに依存しない）。
        let planned = core
            .plan_using_plan_expansion(&ctx, &session, validated.table_name(), None, question)
            .expect("plan_using_plan_expansion should succeed");

        // I/O 完了後・束縛前に DDL が挟まる（同名テーブルの列順を入れ替えて再作成）。
        core.storage.drop_table("docs").expect("drop table");
        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                    ColumnDef::new("path", ColumnType::Text, false),
                ],
            ))
            .expect("create table (new layout)");
        let op_id_new = OperationId::parse("using-plan-race-new").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            2,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("new-body".to_string()),
                crate::row_codec::Value::Text("new-path".to_string()),
            ],
            &op_id_new,
        )
        .expect("insert row (new layout)");

        // I/O 完了後に取得し直した最新スキーマで束縛する（本テストの検証対象。
        // 呼び出し元 `execute_sql_in_session` の `Statement::Select` アームと同じ手順）。
        let (read_txn, schema) = core
            .read_txn_with_schema("docs")
            .expect("read_txn_with_schema should succeed");
        let bound = crate::sql::using_plan::bind_expansion(
            &validated,
            &schema,
            question,
            &planned.expansion,
            planned.query_vector,
            session.udfs(),
            planned.resolved_mode,
        )
        .expect("bind_expansion should succeed against the post-I/O schema");

        match bound.ranking() {
            crate::sql::parser::Ranking::Hybrid {
                text_column_index, ..
            } => assert_eq!(
                *text_column_index, 1,
                "text_column_index must reflect the post-I/O schema (body at index 1), not the \
                 pre-I/O one (index 2)"
            ),
            other => panic!("expected Ranking::Hybrid, got {other:?}"),
        }

        let result = crate::sql::exec::execute_statement_with_cache(
            &read_txn,
            core.provider.as_ref(),
            &ctx,
            &schema,
            &bound,
            &core.precision_policy,
            Some(crate::sql::sparse_cache::SparseCacheAccess {
                storage: &core.storage,
                cache: &core.sparse_index_cache,
            }),
            Some(crate::sql::arena_cache::ArenaCacheAccess {
                storage: &core.storage,
                cache: &core.sql_arena_cache,
            }),
            None,
            None,
        )
        .expect("execute_statement should succeed");
        let row = result
            .rows
            .iter()
            .find(|r| r.id == 2)
            .expect("row inserted under the new layout should be visible");
        assert_eq!(
            row.cells,
            vec![crate::sql::exec::Cell::Text("new-path".to_string())],
            "projected `path` must resolve against the post-I/O schema, not stale pre-I/O indices"
        );
    }

    // codex-review P1 回帰（PR #266）: `USING PLAN` の I/O フェーズ
    // （`plan_using_plan_expansion`）は計画開始時のテーブル世代を保持せず、I/O
    // 完了後は新しい `read_txn`・スキーマを取得するだけで、計画時との世代一致を
    // 照合していなかった。この間に対象テーブルの `DROP TABLE`→同名再作成が挟まると、
    // 旧テーブルの語彙（辞書スナップショット）による展開・再埋め込みベクトルが
    // 新テーブルへ適用され、テーブルの同一性を跨いだ不整合な結果になり得た。
    // 上記の `using_plan_binds_column_indices_against_the_schema_fetched_after_io_not_before`
    // は列インデックスが post-I/O スキーマを正しく反映することを固定するが、
    // 「世代不一致そのものを検出して拒否する」契約は別に固定する必要がある。
    // ここでは `execute_sql_in_session` を直接駆動し（`Statement::Select` アームの
    // 2 段階〔I/O → 束縛〕を経由する唯一の呼び出し元）、`LlmClient::complete`
    // コールバック（I/O フェーズの内部）から対象テーブルを `DROP TABLE`→再作成して
    // 世代を進め、fail-closed に `SqlSurfaceError::Internal` で拒否されることを
    // 検証する（修正前は `read_txn_with_schema` が新テーブルのスキーマ・
    // `read_txn` を黙って返し、クエリが成功していた）。
    #[test]
    fn execute_sql_in_session_rejects_using_plan_when_table_generation_changes_during_io() {
        // `LlmClient::complete`（I/O フェーズの内部）から対象テーブルを
        // `DROP TABLE`→再作成するには、構築中の `EngineCore` 自身（の
        // `storage` フィールド）へアクセスする必要がある。`Box<dyn LlmClient>`
        // は `'static` 境界を要求するため借用では持ち回せず、`unsafe`（生
        // ポインタ）も使わない（`tests/isa.rs::
        // unsafe_is_confined_to_isa_module_with_safety_comments` が
        // `crates/engine/src/**/*.rs` 中 `isa.rs` 以外の `unsafe` を禁止する）。
        // そこで `OnceLock<Weak<EngineCore>>` による安全な二段階初期化
        // （`EngineCore` は wire-server で実際に `Arc<EngineCore>` として
        // 共有される・`server.rs` 参照＝`Send + Sync` 済み）を使い、
        // `complete` 呼び出し時点で必ず設定済みの `Weak` を `upgrade` して
        // 同一 `EngineCore`（同一 `storage`）へアクセスする。
        struct GenerationBumpingLlmClient {
            core: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>>,
        }
        impl crate::query_planner::LlmClient for GenerationBumpingLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                let core = self
                    .core
                    .get()
                    .expect("core must be registered before execute_sql_in_session runs")
                    .upgrade()
                    .expect("core must still be alive during complete()");
                core.storage.drop_table("docs").expect("drop table mid-io");
                core.storage
                    .create_table(&TableSchema::new(
                        "docs",
                        vec![
                            ColumnDef::new("embedding", ColumnType::Vector(2), false),
                            ColumnDef::new("body", ColumnType::Text, false),
                            ColumnDef::new("path", ColumnType::Text, false),
                        ],
                    ))
                    .expect("create table mid-io (new layout)");
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }));

        // 旧レイアウト: [embedding, path, body]。
        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table (old layout)");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id = OperationId::parse("using-plan-gen-race").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("old-path".to_string()),
                crate::row_codec::Value::Text("old-body".to_string()),
            ],
            &op_id,
        )
        .expect("insert row (old layout)");

        // `core_cell` を `GenerationBumpingLlmClient` と外側スコープの双方で
        // `Arc::clone` して共有する（`Box<dyn LlmClient>` へ包んだ後は具体型
        // へ戻す公開経路が無いため、共有元は `with_query_planner` 呼び出し前に
        // 確保しておく）。`Arc<EngineCore>` は構築完了後にしか作れないため、
        // 二段階初期化（セル確保 → `EngineCore` 構築 → `Arc` 化 → `Weak` 登録）
        // にする。
        let core_cell: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let llm_client = Box::new(GenerationBumpingLlmClient {
            core: std::sync::Arc::clone(&core_cell),
        });
        let core = std::sync::Arc::new(core.with_query_planner(llm_client));
        core_cell
            .set(std::sync::Arc::downgrade(&core))
            .unwrap_or_else(|_| panic!("core_cell must be set exactly once"));

        let mut session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let result = core.execute_sql_in_session(&ctx, &mut session, sql);

        match result {
            Err(crate::sql::allowlist::SqlSurfaceError::Internal { detail }) => {
                assert!(
                    detail.contains("generation changed"),
                    "expected a generation-mismatch rejection, got: {detail}"
                );
            }
            other => panic!(
                "expected SqlSurfaceError::Internal (generation mismatch) rejection, got: \
                 {other:?}"
            ),
        }
    }

    // codex-review P1 再指摘（PR #266）: 上記
    // `execute_sql_in_session_rejects_using_plan_when_table_generation_changes_during_io`
    // が固定した世代照合は、当初ストレージ全体の単調増加世代
    // （`crate::storage::current_generation_in_txn`）と比較していたため、対象テーブル
    // （`docs`）自身は無変化でも、I/O（LLM 呼び出し）の間に**無関係な別テーブル**へ
    // 通常の書き込みが 1 回でも完了すると `USING PLAN` が `XX000` で拒否されていた
    // （書き込みが継続する運用では恒常的に失敗し、書き込み可能な利用者が無関係
    // テナントの `USING PLAN` 検索を事実上妨害できる可用性問題）。対象テーブル固有の
    // 世代（`crate::catalog::table_generation_in_txn`）へ切り替えた後は、無関係な
    // 他テーブルへの書き込みが `docs` の世代に影響しないため、本テストは
    // `Ok` を期待する（修正前は本テストが `fail` していたことを確認済み）。
    #[test]
    fn execute_sql_in_session_using_plan_succeeds_when_an_unrelated_table_is_written_during_io() {
        // `GenerationBumpingLlmClient` と同じ二段階初期化パターン（上記テストの
        // ドキュメントコメント参照）。ここでは `docs`（対象テーブル）ではなく
        // `other_tenant_docs`（無関係な別テーブル・別テナント名義）を
        // `LlmClient::complete` から書き込む。
        struct UnrelatedTableWritingLlmClient {
            core: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>>,
        }
        impl crate::query_planner::LlmClient for UnrelatedTableWritingLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                let core = self
                    .core
                    .get()
                    .expect("core must be registered before execute_sql_in_session runs")
                    .upgrade()
                    .expect("core must still be alive during complete()");
                // 対象テーブル（`docs`）とは別名の、無関係なテーブルへの通常の書き込み
                // （別テナント名義。TenantWriteError 経由の RLS 境界付き書き込み API を
                // 使い、テスト対象の書き込み経路〔tenant.rs〕を実際に通す）。
                core.storage
                    .create_table(&TableSchema::new(
                        "other_docs",
                        vec![
                            ColumnDef::new("embedding", ColumnType::Vector(2), false),
                            ColumnDef::new("path", ColumnType::Text, false),
                            ColumnDef::new("body", ColumnType::Text, false),
                        ],
                    ))
                    .expect("create unrelated table mid-io");
                let other_ctx =
                    PolicyContext::new("tenant-b").expect("valid tenant for unrelated table");
                let other_op_id =
                    OperationId::parse("unrelated-write-mid-io").expect("valid operation_id");
                crate::tenant::insert_typed_row(
                    &core.storage,
                    "other_docs",
                    &other_ctx,
                    1,
                    Visibility::Public,
                    &[
                        crate::row_codec::Value::Vector(vec![0.5, 0.5]),
                        crate::row_codec::Value::Text("other-path".to_string()),
                        crate::row_codec::Value::Text("other-body".to_string()),
                    ],
                    &other_op_id,
                )
                .expect("insert row into unrelated table mid-io");
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }));

        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table docs");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id = OperationId::parse("using-plan-unrelated-write").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("doc-path".to_string()),
                crate::row_codec::Value::Text("doc-body".to_string()),
            ],
            &op_id,
        )
        .expect("insert row into docs");

        let core_cell: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let llm_client = Box::new(UnrelatedTableWritingLlmClient {
            core: std::sync::Arc::clone(&core_cell),
        });
        let core = std::sync::Arc::new(core.with_query_planner(llm_client));
        core_cell
            .set(std::sync::Arc::downgrade(&core))
            .unwrap_or_else(|_| panic!("core_cell must be set exactly once"));

        let mut session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let result = core.execute_sql_in_session(&ctx, &mut session, sql);

        match result {
            Ok(crate::sql::SqlOutcome::Query(_)) => {}
            other => panic!(
                "expected USING PLAN to succeed despite an unrelated table being written \
                 during I/O, got: {other:?}"
            ),
        }
    }

    // Issue #285（テーブル単位世代カウンタの拒否精度の設計判断。
    // `docs/design/table-generation-rejection-granularity.md` 参照）: 同 ADR は
    // 「対象テーブルへの他テナントの書き込みは、可視性（`Public`/`Private`）を
    // 問わず拒否する」という現行の意図的な契約（テナント単位・可視性境界単位への
    // 細分化はしない設計判断）を現状維持として確定した。本テストはその契約のうち
    // `Visibility::Public` 側（要求元テナントの辞書内容に実際に影響しうる書き込み。
    // TASK-137・RLS-6, RLS-7 の可視性判定を経由）を固定する。将来 ADR の移行
    // トリガーが成立し可視性境界単位（選択肢 C）へ切り替える場合、本テストは
    // その設計変更に合わせて書き換えが必要になる。
    #[test]
    fn execute_sql_in_session_rejects_using_plan_when_other_tenant_writes_public_row_to_same_table_during_io(
    ) {
        // `GenerationBumpingLlmClient`（本モジュール上部）と同じ二段階初期化
        // パターンを使い、対象テーブル（`docs`）自身へ他テナント（`tenant-b`）
        // 名義で `Visibility::Public` 行を `LlmClient::complete`（I/O フェーズの
        // 内部）から書き込む。
        struct OtherTenantPublicWriteLlmClient {
            core: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>>,
        }
        impl crate::query_planner::LlmClient for OtherTenantPublicWriteLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                let core = self
                    .core
                    .get()
                    .expect("core must be registered before execute_sql_in_session runs")
                    .upgrade()
                    .expect("core must still be alive during complete()");
                let other_ctx =
                    PolicyContext::new("tenant-b").expect("valid tenant for other-tenant write");
                let other_op_id = OperationId::parse("other-tenant-public-write-mid-io")
                    .expect("valid operation_id");
                crate::tenant::insert_typed_row(
                    &core.storage,
                    "docs",
                    &other_ctx,
                    2,
                    Visibility::Public,
                    &[
                        crate::row_codec::Value::Vector(vec![0.2, 0.3]),
                        crate::row_codec::Value::Text("other-tenant-public-path".to_string()),
                        crate::row_codec::Value::Text("other-tenant-public-body".to_string()),
                    ],
                    &other_op_id,
                )
                .expect("insert Public row into the target table mid-io");
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }));

        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table docs");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id =
            OperationId::parse("using-plan-other-tenant-public").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("doc-path".to_string()),
                crate::row_codec::Value::Text("doc-body".to_string()),
            ],
            &op_id,
        )
        .expect("insert row into docs");

        let core_cell: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let llm_client = Box::new(OtherTenantPublicWriteLlmClient {
            core: std::sync::Arc::clone(&core_cell),
        });
        let core = std::sync::Arc::new(core.with_query_planner(llm_client));
        core_cell
            .set(std::sync::Arc::downgrade(&core))
            .unwrap_or_else(|_| panic!("core_cell must be set exactly once"));

        let mut session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let result = core.execute_sql_in_session(&ctx, &mut session, sql);

        match result {
            Err(crate::sql::allowlist::SqlSurfaceError::Internal { detail }) => {
                assert!(
                    detail.contains("generation changed"),
                    "expected a generation-mismatch rejection, got: {detail}"
                );
                assert!(
                    !detail.contains("tenant-b"),
                    "rejection detail must not leak the other tenant's identity: {detail}"
                );
            }
            other => panic!(
                "expected SqlSurfaceError::Internal (generation mismatch) rejection when \
                 another tenant writes a Public row to the same table during I/O, got: {other:?}"
            ),
        }
    }

    // Issue #285（`docs/design/table-generation-rejection-granularity.md` 参照）:
    // 上記テストの `Visibility::Private` 側。要求元テナント（`tenant-a`）からは
    // 不可視な他テナントの `Private` 行書き込みであっても、テーブル単位世代照合は
    // 拒否する（過剰検知を意図的に許容する設計判断。選択肢 C・可視性境界単位への
    // 細分化を採用しない限りこの契約は変わらない）。
    #[test]
    fn execute_sql_in_session_rejects_using_plan_when_other_tenant_writes_private_row_to_same_table_during_io(
    ) {
        struct OtherTenantPrivateWriteLlmClient {
            core: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>>,
        }
        impl crate::query_planner::LlmClient for OtherTenantPrivateWriteLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                let core = self
                    .core
                    .get()
                    .expect("core must be registered before execute_sql_in_session runs")
                    .upgrade()
                    .expect("core must still be alive during complete()");
                let other_ctx =
                    PolicyContext::new("tenant-b").expect("valid tenant for other-tenant write");
                let other_op_id = OperationId::parse("other-tenant-private-write-mid-io")
                    .expect("valid operation_id");
                crate::tenant::insert_typed_row(
                    &core.storage,
                    "docs",
                    &other_ctx,
                    2,
                    Visibility::Private,
                    &[
                        crate::row_codec::Value::Vector(vec![0.2, 0.3]),
                        crate::row_codec::Value::Text("other-tenant-private-path".to_string()),
                        crate::row_codec::Value::Text("other-tenant-private-body".to_string()),
                    ],
                    &other_op_id,
                )
                .expect("insert Private row into the target table mid-io");
                Ok(
                    r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        struct StubEmbedder {
            dim: u32,
        }
        impl crate::embedding::Embedder for StubEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(StubEmbedder { dim: 2 }));

        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table docs");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id =
            OperationId::parse("using-plan-other-tenant-private").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &core.storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[
                crate::row_codec::Value::Vector(vec![0.1, 0.2]),
                crate::row_codec::Value::Text("doc-path".to_string()),
                crate::row_codec::Value::Text("doc-body".to_string()),
            ],
            &op_id,
        )
        .expect("insert row into docs");

        let core_cell: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<EngineCore>>> =
            std::sync::Arc::new(std::sync::OnceLock::new());
        let llm_client = Box::new(OtherTenantPrivateWriteLlmClient {
            core: std::sync::Arc::clone(&core_cell),
        });
        let core = std::sync::Arc::new(core.with_query_planner(llm_client));
        core_cell
            .set(std::sync::Arc::downgrade(&core))
            .unwrap_or_else(|_| panic!("core_cell must be set exactly once"));

        let mut session = crate::sql::mode::SessionState::default();
        let sql = "SELECT path FROM docs USING PLAN('q') LIMIT 5";
        let result = core.execute_sql_in_session(&ctx, &mut session, sql);

        match result {
            Err(crate::sql::allowlist::SqlSurfaceError::Internal { detail }) => {
                assert!(
                    detail.contains("generation changed"),
                    "expected a generation-mismatch rejection, got: {detail}"
                );
                assert!(
                    !detail.contains("tenant-b"),
                    "rejection detail must not leak the other tenant's identity: {detail}"
                );
            }
            other => panic!(
                "expected SqlSurfaceError::Internal (generation mismatch) rejection when \
                 another tenant writes a Private row to the same table during I/O (current \
                 design: over-rejection is intentional per Issue #285), got: {other:?}"
            ),
        }
    }

    // codex-review P1 回帰（PR #266）: `plan_using_plan_expansion` が
    // `Embedder::embed_batch` へ渡すテキストは、`query_planner::
    // render_reembedding_text`（固定接頭辞 `search_query: `・検索語の防御的上限
    // つき）の出力と一致しなければならない。以前は `sql::using_plan::
    // expanded_query_text`（接頭辞なし・上限なしの単純結合）をそのまま渡していた
    // ため、`USING PLAN` 経路の再埋め込みベクトルが既存の増分索引・クエリ展開
    // 受け入れ検証（TASK-114・PLAN-10）が前提とするベクトルと食い違っていた。
    #[test]
    fn plan_using_plan_expansion_embeds_render_reembedding_text_not_expanded_query_text() {
        struct StubLlmClient;
        impl crate::query_planner::LlmClient for StubLlmClient {
            fn complete(&self, _prompt: &str) -> Result<String, crate::query_planner::PlanError> {
                Ok(
                    r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#
                        .to_string(),
                )
            }
        }

        // 呼び出し元が `embed_batch` へ渡した入力テキストをそのまま記録するスパイ
        // 実装（`Mutex` で束ねて `Sync` を満たす。`Embedder` は `&self` のみで
        // 呼ばれるため内部可変性が必要）。
        struct SpyEmbedder {
            dim: u32,
            seen: std::sync::Mutex<Vec<String>>,
        }
        impl crate::embedding::Embedder for SpyEmbedder {
            fn dim(&self) -> u32 {
                self.dim
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                self.seen
                    .lock()
                    .expect("spy lock not poisoned")
                    .extend(texts.iter().map(|t| t.to_string()));
                Ok(texts.iter().map(|_| vec![0.1; self.dim as usize]).collect())
            }
        }

        let dir = tempdir();
        let spy = std::sync::Arc::new(SpyEmbedder {
            dim: 2,
            seen: std::sync::Mutex::new(Vec::new()),
        });

        // `EngineCore::with_embedder` は `Box<dyn Embedder>` を要求するため、
        // テスト側から観測を続けられるよう `Arc` を経由する薄いラッパーで包む
        // （スパイ本体の所有権は `spy` 側にも残す）。
        struct ArcEmbedder(std::sync::Arc<SpyEmbedder>);
        impl crate::embedding::Embedder for ArcEmbedder {
            fn dim(&self) -> u32 {
                self.0.dim()
            }
            fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbedError> {
                self.0.embed_batch(texts)
            }
        }

        let core = EngineCore::open(dir.path().join("db.redb"))
            .expect("open engine core")
            .with_embedder(Box::new(ArcEmbedder(spy.clone())))
            .with_query_planner(Box::new(StubLlmClient));

        core.storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let session = crate::sql::mode::SessionState::default();
        let sql = "SELECT body FROM docs USING PLAN('find auth') LIMIT 5";
        let stmt = crate::sql::allowlist::validate_sql(sql, &core.storage).expect("valid sql");
        let validated = match stmt {
            crate::sql::allowlist::Statement::Select(v) => v,
            other => panic!("expected Select statement, got {other:?}"),
        };
        let question = validated.using_plan().expect("USING PLAN present");

        let planned = core
            .plan_using_plan_expansion(&ctx, &session, validated.table_name(), None, question)
            .expect("plan_using_plan_expansion should succeed");

        let seen = spy.seen.lock().expect("spy lock not poisoned");
        assert_eq!(
            seen.len(),
            1,
            "embed_batch must be called exactly once for USING PLAN re-embedding"
        );
        let expected = crate::query_planner::render_reembedding_text(question, &planned.expansion);
        assert_eq!(
            seen[0], expected,
            "embed_batch input must match query_planner::render_reembedding_text output \
             (fixed prefix + bounded search terms), not the unprefixed sparse query_text"
        );
        assert!(
            seen[0].starts_with(crate::query_planner::SEARCH_QUERY_PREFIX),
            "embed_batch input must carry the fixed re-embedding prefix"
        );
    }

    // 一時ディレクトリ（`TempDir` / `tempdir()`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    use crate::test_util::temp_db::tempdir;

    // `OpenWithEngineError::source()` が両 variant で内包エラーを返すことの固定
    // （codex-review P2 指摘・PR #433 追記。空の `impl Error` により `source()` が
    // 常に `None` を返し、`std::error::Error` のエラーチェーンを辿れなかった不備の
    // 回帰防止）。
    #[test]
    fn open_with_engine_error_source_returns_storage_error() {
        use std::error::Error as _;
        let inner = StorageError::NotFound(42);
        let err = OpenWithEngineError::Storage(inner);
        let source = err.source().expect("Storage variant must expose a source");
        assert_eq!(
            source.to_string(),
            StorageError::NotFound(42).to_string(),
            "source() must return the wrapped StorageError"
        );
    }

    #[test]
    fn open_with_engine_error_source_returns_search_engine_error() {
        use std::error::Error as _;
        let inner = crate::search_engine::SearchEngineError::InvalidHnswParams(
            crate::hnsw::HnswError::InvalidParams {
                reason: "test reason",
            },
        );
        let err = OpenWithEngineError::SearchEngine(inner);
        let source = err
            .source()
            .expect("SearchEngine variant must expose a source");
        assert_eq!(
            source.to_string(),
            crate::search_engine::SearchEngineError::InvalidHnswParams(
                crate::hnsw::HnswError::InvalidParams {
                    reason: "test reason",
                }
            )
            .to_string(),
            "source() must return the wrapped SearchEngineError"
        );
    }

    /// Issue #411: `Statement::Explain` アームは `self.hnsw_state.is_some()` を
    /// `EXPLAIN` の `ann_plan:` 判定（`AnnShapeInput::hnsw_enabled`）の源泉に
    /// 使う。この値が `search_engine_kind() == Some(SearchEngineKind::Hnsw(_))`
    /// と常に同値であること（`Self::assemble` の構築規則。`hnsw_state` は
    /// private フィールドのためこの in-module テストでのみ検証できる）を
    /// 4 経路（`open`／`with_provider`／`open_with_engine`／`from_storage`／
    /// `from_storage_with_engine`）全てで固定する。
    #[test]
    fn hnsw_state_presence_matches_hnsw_search_engine_kind() {
        let dir = tempdir();

        let default_core = new_core(dir.path());
        assert_eq!(
            default_core.hnsw_state.is_some(),
            matches!(
                default_core.search_engine_kind(),
                Some(crate::search_engine::SearchEngineKind::Hnsw(_))
            )
        );

        let provider_core = EngineCore::with_provider(
            dir.path().join("provider.redb"),
            Box::new(crate::kernel::CpuScalarProvider),
        )
        .expect("open with_provider core");
        assert!(provider_core.hnsw_state.is_none());
        assert!(!matches!(
            provider_core.search_engine_kind(),
            Some(crate::search_engine::SearchEngineKind::Hnsw(_))
        ));

        let hnsw_params = crate::hnsw::ValidatedHnswParams::new(crate::hnsw::HnswParams::default())
            .expect("valid hnsw params");
        let hnsw_kind = crate::search_engine::SearchEngineKind::Hnsw(hnsw_params);
        let hnsw_core = EngineCore::open_with_engine(dir.path().join("hnsw.redb"), hnsw_kind)
            .expect("open_with_engine core");
        assert!(hnsw_core.hnsw_state.is_some());
        assert!(matches!(
            hnsw_core.search_engine_kind(),
            Some(crate::search_engine::SearchEngineKind::Hnsw(_))
        ));

        let storage = crate::storage::Storage::open(dir.path().join("from-storage.redb"))
            .expect("open storage");
        let from_storage_core =
            EngineCore::from_storage(storage, Box::new(crate::kernel::CpuScalarProvider));
        assert!(from_storage_core.hnsw_state.is_none());
        assert!(from_storage_core.search_engine_kind().is_none());

        let storage2 = crate::storage::Storage::open(dir.path().join("from-storage-hnsw.redb"))
            .expect("open storage");
        let from_storage_hnsw_core = EngineCore::from_storage_with_engine(storage2, hnsw_kind);
        assert!(from_storage_hnsw_core.hnsw_state.is_some());
        assert!(matches!(
            from_storage_hnsw_core.search_engine_kind(),
            Some(crate::search_engine::SearchEngineKind::Hnsw(_))
        ));
    }

    // 対象指摘: codex-review P1（PR #1011）。`validate_insert_batch_byte_and_chunk_limits`
    // が配列値のフレーミング（presence(1)・flags(1)・count(4)・payload_len(4)・
    // 要素ごとの長さプレフィックス(4)）を積算せず要素本文のみを数えていたため、
    // 空文字列を大量に持つ配列で INDEX-4 の行別バイト上限（②）を迂回できた。
    // 空文字列 1,024 個の配列は要素本文としては 0 バイトだが、実エンコード
    // （`row_codec::scalar_array_entry_len`）ではフレームだけで 10 + 1,024*4 =
    // 4,106 バイトを消費する。②を「フレーム込みで 100 バイト」まで絞った
    // `BatchLimits` の下でこの行が拒否されることを固定する。
    #[test]
    fn insert_batch_array_byte_limit_counts_full_frame_not_just_element_payload() {
        let dir = tempdir();
        let mut core = new_core(dir.path());
        core.batch_limits = crate::batch_limits::BatchLimits {
            max_files_per_batch: 64,
            max_file_body_bytes: 100,
            max_batch_total_bytes: 1_000_000,
            max_batch_chunks: 1_000_000,
        };

        let empty_strings: Vec<String> = std::iter::repeat_with(String::new).take(1024).collect();
        let bound = crate::sql::parser::BoundInsert {
            table: "docs".to_string(),
            id: 1,
            values: vec![crate::row_codec::Value::Array(
                crate::row_codec::ArrayValue::Text(empty_strings),
            )],
            operation_id: Some(
                crate::sql::using_operation_id::OperationId::parse("op-array-batch-limit")
                    .expect("valid operation id"),
            ),
        };

        let result = core.validate_insert_row_batch_limits(std::slice::from_ref(&bound));
        assert!(
            result.is_err(),
            "array framing overhead must count toward the per-row byte limit (INDEX-4 ②)"
        );
    }

    /// 書き込み系文の対象名がテーブルとして見つからない（`42P01`）場合に行う
    /// ビュー定義の読み直し（`reclassify_write_to_view_error`）が失敗したとき、
    /// その失敗を「ビューではない」と同一視して `42P01` を返してはならない
    /// （codex-review P1・Cursor Bugbot・PR #1048。TABLE-18・SQL-23・TASK-205）。
    /// 破損したビュー定義を `VIEWS_TABLE` へ直接書き込み、`INSERT`／`TRUNCATE`
    /// のいずれも内部エラー（`XX000`）へ倒れること、ビューとしても存在しない
    /// 名前は従来どおり `42P01` のままであることを固定する。
    #[test]
    fn write_to_corrupt_view_definition_is_internal_error_not_undefined_table() {
        let dir = tempdir();
        let storage =
            crate::storage::Storage::open(dir.path().join("corrupt-view.redb")).expect("open");
        // テスト専用のセットアップ書き込みのため `commit_boundary` を経由せず
        // `commit_raw_for_test` で commit する（`catalog.rs` の破損カタログ
        // テストと同じ流儀。`table_generation_bump_coverage.rs` の走査対象外）。
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let mut table = write_txn
                .open_table(crate::catalog::VIEWS_TABLE)
                .expect("open views table");
            table
                .insert("v", [0xff_u8, 0xfe, 0xfd].as_slice())
                .expect("insert corrupt view definition");
        }
        write_txn
            .commit_raw_for_test()
            .expect("commit corrupt view definition");
        let core = EngineCore::from_storage(storage, Box::new(crate::kernel::CpuScalarProvider));

        for sql in [
            "INSERT INTO v (id, embedding) VALUES (1, '[1,1]') USING OPERATION_ID 'op-corrupt-1'",
            "TRUNCATE TABLE v USING OPERATION_ID 'op-corrupt-2'",
        ] {
            let err = core
                .parse_sql(sql)
                .err()
                .unwrap_or_else(|| panic!("write to a corrupt view must fail: {sql}"));
            assert_eq!(
                err.wire_code(),
                "XX000",
                "corrupt view definition must not be reported as 42P01/42809: {sql} -> {err:?}"
            );
        }

        // ビューとしても存在しない名前（`Ok(None)` 経路）は従来どおり `42P01`。
        let err = core
            .parse_sql("TRUNCATE TABLE nosuch USING OPERATION_ID 'op-corrupt-3'")
            .err()
            .unwrap_or_else(|| panic!("truncate of an unknown relation must fail"));
        assert_eq!(err.wire_code(), "42P01");
    }
}
