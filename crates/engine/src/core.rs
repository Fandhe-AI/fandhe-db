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
use crate::storage::rowid::hit_id_for_response;
use crate::storage::{Row, Storage, StorageError};
use redb::ReadableDatabase;

// 一時表（TABLE-44・Issue #1684）の振り分けと実行。永続コアの私的な項目に触れるため子モジュールに
// 置く（`core.rs` の肥大化を避ける目的も兼ねる）。
mod temp_route;
pub use temp_route::SessionRoutedSql;

mod ctas_exec;
mod matview_exec;

// INSERT ... SELECT の実行本体（SQL-51・Issue #1733）。
mod insert_select_exec;

// データ変更 CTE（`WITH d AS (DELETE ... RETURNING ...) SELECT ...`）の実行本体（SQL-63・Issue #1876）。
mod dm_cte_exec;

// 書き込み・実体化の入力 SELECT の行数上限（SQL-34・Issue #2212）。
mod write_input_cap;

// UPDATE ... FROM／DELETE ... USING の束縛と実行腕（SQL-53 (b)・Issue #1754。実行は #1755 以降）。
mod dml_from_exec;

// 述語形 UPDATE／DELETE の WHERE 内サブクエリの実行（SQL-53・Issue #1751）。
mod dml_subquery_exec;
// 述語形 UPDATE ... FROM／DELETE ... USING の結合実行（SQL-53・Issue #1755）。
#[cfg(test)]
mod cte_join_budget_tests;
mod dml_join_exec;
#[cfg(test)]
mod dml_join_exec_tests;
#[cfg(test)]
mod dml_subquery_exec_tests;
// PL/pgSQL 本体の埋め込み SQL の定義時構文検証（Issue #2224・SQL-68。`plpgsql::embedded` の検証口）。
mod plpgsql_validate;
#[cfg(test)]
mod plpgsql_validate_tests;
// BEFORE ROW INSERT トリガの発火の結合テスト（TABLE-45・Issue #1723）。
#[cfg(test)]
mod trigger_insert_tests;
// AFTER ROW INSERT トリガの発火の結合テスト（TABLE-45・Issue #1724）。
#[cfg(test)]
mod trigger_after_insert_tests;
// BEFORE・AFTER ROW UPDATE／DELETE トリガの発火の結合テスト（TABLE-45・Issue #1725）。
#[cfg(test)]
mod trigger_update_delete_tests;
// 文レベルトリガと TRUNCATE トリガの発火の結合テスト（TABLE-45・Issue #1726）。
#[cfg(test)]
mod trigger_statement_tests;
// トリガの入れ子の発火と深さ上限（`54001`）の結合テスト（TABLE-45 (5)・Issue #1727）。
#[cfg(test)]
mod trigger_depth_tests;
// トリガ副作用と operation_id 台帳の整合（再送・越境・クラッシュ注入）のテスト（TABLE-45・Issue #1728）。
#[cfg(test)]
mod trigger_ledger_tests;

/// 検索 `k` の上限（直接 Rust API 用）。明示 `LIMIT` の既定値と同値だが、SQL／NoSQL の
/// `LIMIT` 範囲は起動時設定（`result_limits::ResultLimits`。Issue #1555）が決め、本定数は
/// `OFFSET`・集合演算の枝行数・サブクエリ内側行数など他の内部上限として固定のまま残す。
/// 上限検証前にアロケーションへ使わないための防御的定数
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応。`catalog.rs::MAX_LIST_TABLES`
/// と同程度の桁に揃える）。`rls.rs`（TASK-133）の `PrefilterIndex::search` も同一の上限を
/// 共有するため `pub(crate)`（二重定義しない）。
pub(crate) const MAX_SEARCH_K: usize = 10_000;

/// `INSERT INTO <表> ( <列> {, <列>} ) VALUES` の表名と列リストをトークン列から読む
/// （Issue #1546。`parse_sql_prepared` が構造検証 parse より前に型を引くために使う）。
/// 形が崩れている場合は `None`（型は未解決＝`Text` へ倒れ、構文エラーは後続の parse が返す）。
fn insert_target_from_tokens(tokens: &[crate::sql::lexer::Token]) -> Option<(String, Vec<String>)> {
    use crate::sql::lexer::Token;
    let is_word = |t: Option<&Token>, w: &str| matches!(t, Some(Token::Ident(x)) if x.eq_ignore_ascii_case(w));
    if !is_word(tokens.first(), "INSERT") || !is_word(tokens.get(1), "INTO") {
        return None;
    }
    let Some(Token::Ident(table) | Token::QuotedNull(table)) = tokens.get(2) else {
        return None;
    };
    if !matches!(tokens.get(3), Some(Token::Punct('('))) {
        return None;
    }
    let mut columns: Vec<String> = Vec::new();
    for t in tokens.iter().skip(4) {
        match t {
            Token::Ident(c) | Token::QuotedNull(c) => columns.push(c.clone()),
            Token::Punct(',') => {}
            _ => break,
        }
    }
    Some((table.clone(), columns))
}

/// `INSERT INTO <表> VALUES` 形（列リスト省略。SQL-50・Issue #1730）の表名を返す。
/// [`insert_target_from_tokens`] が `None`（列リスト無し）を返したときの補完用で、
/// 形が違えば `None`（型は未解決＝`Text`）。
fn insert_table_without_column_list(tokens: &[crate::sql::lexer::Token]) -> Option<String> {
    use crate::sql::lexer::Token;
    let is_word = |t: Option<&Token>, w: &str| matches!(t, Some(Token::Ident(x)) if x.eq_ignore_ascii_case(w));
    if !is_word(tokens.first(), "INSERT")
        || !is_word(tokens.get(1), "INTO")
        || !is_word(tokens.get(3), "VALUES")
    {
        return None;
    }
    match tokens.get(2) {
        Some(Token::Ident(table) | Token::QuotedNull(table)) => Some(table.clone()),
        _ => None,
    }
}

/// `INSERT INTO <表>` の表名だけを読む（Issue #2427。`DEFAULT VALUES ON CONFLICT ...` 形のように
/// 列リストも `VALUES` も無い INSERT でも、加算式の `$n` の型を表の列から引くための補完）。
fn insert_table_name_only(tokens: &[crate::sql::lexer::Token]) -> Option<String> {
    use crate::sql::lexer::Token;
    let is_word = |t: Option<&Token>, w: &str| matches!(t, Some(Token::Ident(x)) if x.eq_ignore_ascii_case(w));
    if !is_word(tokens.first(), "INSERT") || !is_word(tokens.get(1), "INTO") {
        return None;
    }
    match tokens.get(2) {
        Some(Token::Ident(table) | Token::QuotedNull(table)) => Some(table.clone()),
        _ => None,
    }
}

/// トークン列のうち括弧深さ 0 で最初の `FROM` の添字を返す（Issue #1188）。`EXTRACT(field
/// FROM src)` の括弧内 `FROM` をテーブル名の手掛かりと取り違えないための走査で、
/// prepared statement のパラメータ型推論（`EngineCore::prepare_*`）が呼ぶ。
fn first_top_level_from_index(tokens: &[crate::sql::lexer::Token]) -> Option<usize> {
    let mut depth: i32 = 0;
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            crate::sql::lexer::Token::Punct('(') => depth = depth.saturating_add(1),
            crate::sql::lexer::Token::Punct(')') => depth = depth.saturating_sub(1),
            crate::sql::lexer::Token::Keyword(crate::sql::lexer::Keyword::From) if depth == 0 => {
                return Some(i)
            }
            _ => {}
        }
    }
    None
}

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
    let len = arena.row_ids().len();
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
        // 応答 `id` は rowid → 境界関数 1 箇所を通す（Issue #2195・ADR D8）。
        let id = hit_id_for_response(arena.row_id(slot)?);
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

/// [`EngineCore::explain_index_name_scans`] の観測用統計（Issue #1066
/// codex-review P2 指摘対応・PR #1155 4 巡目）。テナント ID・索引名等の機微
/// 情報は一切含まない（カウンタのみ）。`VectorCore` trait には載せない固有 API
/// （`EngineCore::explain_index_name_scan_stats`）としてのみ公開する
/// （`core_api.snapshot` の対象外。`PrefilterCacheStats` と同じ方針）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ExplainIndexNameScanStats {
    /// `catalog::explain_index_names_in_txn` によるカタログ全件走査が
    /// [`EngineCore::read_explain_index_names_in_txn`] 経由で実際に呼ばれた
    /// 累計回数。索引名を最終的に表示しない `EXPLAIN` 呼び出しではこの値が
    /// 増えないことを `tests/explain_index_names.rs` で固定する。
    pub scans: u64,
}

/// [`EngineCore::materialized_cte_eval_stats`] の観測用統計（SQL-62・Issue #1861）。
/// CTE 本文を記憶域に対して評価した回数は結果から観測できない（結果は評価方式で変わらない）ため、
/// 「複数回参照しても本文の実行は 1 回」を検証する手段として回数だけを公開する。テナント ID・行数等の
/// 機微情報は含まない。`VectorCore` trait には載せない固有 API で、`core_api.snapshot` の対象外
/// （[`ExplainIndexNameScanStats`] と同じ方針。wire・HTTP へも出さない）。
#[derive(Debug, Clone, Copy, Default)]
pub struct MaterializedCteEvalStats {
    /// 具体化経路（`BufferedView` の `MaterializedCte`、JOIN 入力の CTE slot）が CTE 本文を
    /// 記憶域に対して実行した累計回数。記憶域を読まない CTE→CTE の導出は数えない。
    pub evaluations: u64,
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
    /// `WHERE` の式述語（Issue #945・NOSQL-14。NoSQL `plan` 検索の `filter` に
    /// 範囲比較・`IN`・`OR` を拡張した際、`INTEGER`／`BIGINT`／`REAL`／
    /// `DOUBLE PRECISION` 列への比較を式レーンへ渡す入口）。[`Self::new`] の
    /// 既存呼び出し元との互換を保つため既定は空（[`Self::with_where_filters`]
    /// で設定する）。
    expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
    /// `WHERE` の `OR` 群（Issue #945・NOSQL-14）。既定は空。
    or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
}

impl PlanSearchBinding {
    pub fn new(
        projection: Vec<crate::sql::parser::ProjectedColumn>,
        metadata_filters: Vec<crate::declarative_filter::MetadataFilter>,
    ) -> Self {
        Self {
            projection,
            metadata_filters,
            expr_filters: Vec::new(),
            or_filters: Vec::new(),
        }
    }

    /// `WHERE` の式述語・`OR` 群を設定したコピーを返すビルダー的メソッド
    /// （Issue #945・NOSQL-14。`sql::declarative_predicate::
    /// bind_declarative_predicates` の束縛結果〔`BoundWhereFilters::into_parts`〕
    /// をそのまま渡す入口。`BoundStatement::with_where_filters` と同じ設計）。
    #[must_use]
    pub fn with_where_filters(
        mut self,
        expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
        or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
    ) -> Self {
        self.expr_filters = expr_filters;
        self.or_filters = or_filters;
        self
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
    /// BEFORE ROW INSERT トリガの呼び出し口を保持する通過証（TABLE-45・Issue #1723）。発火に対応した
    /// INSERT 経路（SQL の `INSERT ... VALUES`・`INSERT ... SELECT`）だけが `tenant` へ渡す。既定は
    /// 未結線（トリガ関数の実行は `0A000`）で、実体への結線は Issue #2228。入れ子の深さ上限
    /// （Issue #1727）も保持し、`with_trigger_limits` が起動時に 1 回だけ差し替える。
    trigger_runtime: crate::trigger::TriggerRuntime,
    /// `WHERE` 省略の `UPDATE`／`DELETE` を `42601` で拒否する opt-in のサーバー側構成
    /// （Issue #2210・SQL-36 ポインタ。既定 `false`）。クエリ・セッション変数・HTTP から
    /// 到達できる経路を持たない（差し替えは [`Self::with_require_where_for_dml`] のみ）。
    require_where_for_dml: bool,
    /// 一括投入（複数ファイルのバッチ投入）に対する 4 種の処理量上限（TASK-122・
    /// 対象ビヘイビア: INDEX-4）。差し替えは [`Self::with_batch_limits`] のみ。
    /// `crate::batch_limits` モジュールドキュメント参照。
    batch_limits: crate::batch_limits::BatchLimits,
    /// 明示トランザクション（SQL-31・TASK-221）の持続時間・文数の上限。差し替えは
    /// [`Self::with_transaction_limits`] のみ。
    transaction_limits: crate::sql::transaction::TransactionLimits,
    /// 述語形 UPDATE/DELETE の 1 文あたり影響行数上限・複数行 `VALUES` の
    /// 1 文あたり行数上限（Issue #997。オーナー判断 2026-09-27: プロセス全体を
    /// 起動時 CLI フラグで設定可能にする。セッション・テナント単位は対象外）。
    /// 差し替えは [`Self::with_dml_limits`] のみ。`crate::sql::parser::DmlLimits`
    /// モジュールドキュメント参照。
    dml_limits: crate::sql::parser::DmlLimits,
    /// スキーマ定義系 3 上限（列数・配列要素数・ENUM ラベル数。Issue #1548）。
    /// 差し替えは [`Self::with_schema_limits`] のみ（起動時 1 回）。定義が増える DDL の
    /// 実行段（`sql::ddl`）にだけ効き、永続化済みデータの読み出しには効かない。
    schema_limits: crate::schema_limits::SchemaLimits,
    /// 1 文あたりの総走査行数上限（Issue #1549。起動時 CLI `--max-scanned-rows`）。
    /// 差し替えは [`Self::with_scan_limits`] のみ（起動時 1 回）。原子経路の述語形
    /// UPDATE／DELETE の候補列挙・辞書スナップショット構築・ファイル形 INSERT の
    /// 置換走査に効く。分割実行 DML の走査予算とは連動しない。
    scan_limits: crate::scan_limits::ScanLimits,
    /// `GROUP BY` の列数・グループ数の上限（Issue #1550）。差し替えは
    /// [`Self::with_group_by_limits`] のみ（起動時 1 回）。集計の実行段
    /// （単一表・集合演算枝・サブクエリ内側・ビュー外側・JOIN）にだけ効く。
    group_by_limits: crate::group_by_limits::GroupByLimits,
    /// 一時表のセッションあたりの件数・総バイト数の上限（TABLE-44・Issue #1683）。差し替えは
    /// [`Self::with_temp_table_limits`] のみ（起動時 1 回）。`CREATE TEMP TABLE` の実行段にだけ効く。
    temp_table_limits: crate::temp_table_limits::TempTableLimits,
    /// jsonpath 評価のステップ数・再帰深さの上限（Issue #2184・TABLE-25 ポインタ）。
    /// 差し替えは [`Self::with_jsonpath_limits`] のみ（起動時 1 回）。SQL 表層の `jsonb_path_*`
    /// （Issue #2185）は、文実行入口が張る `JsonPathLimitsScope` 経由で束縛時にこの値を焼き込む。
    jsonpath_limits: crate::jsonpath_limits::JsonPathLimits,
    /// 正規表現マッチャのパターン長・入力長・コンパイル後サイズの上限（Issue #1786・SQL-55
    /// ポインタ）。差し替えは [`Self::with_regex_limits`] のみ（起動時 1 回）。SQL 表層の
    /// `~` 系（Issue #1787）等が束縛時に [`Self::regex_limits`] を取り出して
    /// [`crate::regex_match::CompiledRegex::compile`] へ渡す。
    regex_limits: crate::regex_limits::RegexLimits,
    /// サブクエリ深さ・実行回数・CTE 定義数・CTE 連鎖深さの上限（Issue #1552）。
    /// 差し替えは [`Self::with_nesting_limits`] のみ（起動時 1 回）。構文解析段と
    /// サブクエリ解決段にだけ効き、格納済みビュー本文の再パースはハード上限のまま。
    nesting_limits: crate::nesting_limits::NestingLimits,
    /// 窓の `PARTITION BY`／`ORDER BY` キー数の上限（Issue #1900）。差し替えは
    /// [`Self::with_window_limits`] のみ（起動時 1 回）。構文解析段にだけ効き、値は
    /// `nesting_limits` の運搬フィールドへ合成される（入れ子パーサーまで届けるため）。
    window_limits: crate::window_limits::WindowLimits,
    /// JOIN のリレーション数・入出力行数の上限（Issue #1551）。差し替えは
    /// [`Self::with_join_limits`] のみ（起動時 1 回）。JOIN の実行段（`sql::join`）にだけ効く。
    join_limits: crate::join_limits::JoinSizeLimits,
    /// 相関サブクエリの再評価回数上限（Issue #1847）。[`Self::with_correlated_limits`] のみ
    /// （起動時 1 回）。`sql::correlated_exists::CorrelatedCtx` の予算へ渡る。
    correlated_limits: crate::correlated_limits::CorrelatedLimits,
    /// 再帰 CTE の反復回数・累積行数の上限（Issue #1871・SQL-35 (3)）。
    /// [`Self::with_recursive_cte_limits`] のみが起動時に 1 回だけ差し替え、
    /// `sql::recursive_eval` の実行段だけに効く。
    recursive_cte_limits: crate::recursive_cte_limits::RecursiveCteLimits,
    /// PL/pgSQL の文数・反復・入れ子・本体長・呼び出し深さの上限（Issue #2225・SQL-68）。
    /// [`Self::with_plpgsql_limits`] のみが起動時に 1 回だけ差し替える。消費者（`LANGUAGE plpgsql`
    /// の受理〔#2217〕・実行の結線〔#2226〕）が入るまでは getter の呼び出し元は無い。
    plpgsql_limits: crate::plpgsql_limits::PlpgsqlLimits,
    /// 明示 `LIMIT` の範囲上限（Issue #1555）。差し替えは [`Self::with_result_limits`]
    /// のみ（起動時 1 回）。文の `LIMIT`・`FETCH n`・集合演算／JOIN 文の `LIMIT` の
    /// 範囲判定にだけ効き、`OFFSET`・枝行数・サブクエリ内側行数の上限とは連動しない。
    result_limits: crate::result_limits::ResultLimits,
    /// MATERIALIZED VIEW の初回実体化の対象文脈（TABLE-42・Issue #1664）。`None` は未設定で、
    /// 作成したセッションの文脈だけを実体化する（安全側。それ以外の文脈からの読み取りは `55000`）。
    /// 差し替えは [`Self::with_materialization_contexts`] のみ（起動時 1 回）。クライアントの
    /// 申告値は使わず、認証構成から導出した文脈だけを注入する。
    materialization_contexts: Option<std::sync::Arc<Vec<PolicyContext>>>,
    /// 述語形 UPDATE／DELETE の分割実行（Issue #1128・ADR `docs/design/partitioned-dml.md`
    /// §15.2）のチャンク幅・走査予算・writer 保持時間。差し替えは
    /// [`Self::with_partitioned_dml_limits`] のみ（起動時に 1 回）。
    partitioned_dml_limits: crate::sql::parser::PartitionedDmlLimits,
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
    /// `catalog::hnsw_targeted_in_txn` のカタログ全件走査結果を索引カタログ専用
    /// 世代単位で再利用するキャッシュ（codex-review P2 対応・Issue #1065
    /// PR #1124。Issue #1154 でキーをストレージ全体世代から専用世代へ切り替え、
    /// 索引宣言と無関係な行 DML では無効化されないようにした）。`hnsw_state`
    /// の有無に関わらず常時構築する（`hnsw_targeted_in_txn`
    /// 自身が `hnsw_available == false` を最初に見て素通しするため、未使用でも
    /// 空のキャッシュを持つだけで副作用はない）。[`Self::explain_engine_for`]・
    /// [`Self::search_with_snapshot`] が直接、`sql::exec` は
    /// `sql::hnsw_cache::HnswCacheAccess::index_gate_cache` 経由で共有する。
    /// 詳細は `catalog::IndexCatalogGateCache` のドキュメント参照。
    index_catalog_gate_cache: crate::catalog::IndexCatalogGateCache,
    /// HNSW opt-in 時に HNSW 経路を使うテーブルの範囲（Issue #1065。既定
    /// `HnswScope::All`＝全テーブル。[`Self::with_hnsw_scope`] でのみ差し替える）。
    /// `hnsw_state` が `None`（opt-in なし）の構築では参照されない。
    hnsw_scope: crate::search_engine::HnswScope,
    /// [`Self::read_explain_index_names_in_txn`]（`catalog::explain_index_names_
    /// in_txn` の唯一の呼び出し経路。[`Self::explain_engine_for`]・
    /// [`Self::aggregate_explain_from_bound`] の両方が経由する）が実際にカタログ
    /// 全件走査を行った累計回数（Issue #1066 codex-review P2 指摘対応・
    /// PR #1155 4 巡目）。索引名を最終的に表示しない呼び出し（述語なし・
    /// `HnswScope::All`・`scalar_plan` が `PlainScan` 等）で走査が省略されて
    /// いることをテストから観測できるようにするための observability 専用
    /// カウンタで、検索結果・`EXPLAIN` 出力には影響しない。
    explain_index_name_scans: std::sync::atomic::AtomicU64,
    /// 具体化経路が CTE 本文を記憶域に対して実行した累計回数（SQL-62・Issue #1861）。
    /// 観測専用で、結果・エラーには影響しない（[`MaterializedCteEvalStats`]）。
    materialized_cte_evals: std::sync::atomic::AtomicU64,
    /// CTE を入力に持つ JOIN の文全体バイト予算の上限を差し替えるテスト専用の入口（既定 `None`＝本番の
    /// 上限）。1 GiB の実データを作らずに予算の事前計上を固定するためで、本番ビルドには含めない。
    #[cfg(test)]
    cte_join_budget_cap_for_test: Option<usize>,
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
/// [`EngineCore::execute_bound_predicate_update_in_session`]・[`EngineCore::
/// execute_bound_predicate_delete_in_session`]（TASK-186・NOSQL-12、
/// Issue #1062・#1356）が `bind` closure の戻り値へ課す多層防御。NoSQL 表層の
/// `filter` から構築される `WherePredicate` は
/// `wire-server::http::query::filter::bind_filter_where_predicates` が SQL の述語形
/// DML（SQL-19）と同じ語彙（等価・前方一致・範囲比較・`IN`・`BETWEEN`・`IS NULL`・
/// 数値列の式比較・それらの `NOT`・`OR`）だけを生成する契約だが、`bind` closure 自体は
/// engine の外（wire-server）にあるため、契約が壊れた場合に備えてここでも
/// fail-closed に拒否する。
///
/// - 空列（`WHERE` 句省略に相当）は全行対象の DML になりうるため `42601` で拒否する
///   （SQL 表層は SQL-36・Issue #1743 で省略を受理するが、NoSQL 表層は SQL-36 (5) により空列を拒否し続ける。意図的な非対称。
///   束縛段は空列（省略）を通すため、
///   NoSQL 経路はこの関数が唯一の防御になる）。
/// - 葉として `Equality`／`BoolEquality`／`Prefix`／`Between`／`IsNull`／`Compare`／
///   `InList` を許可し、`Not` はそれらを**直接**包む場合に限り許可する（再帰しない。
///   SQL の `NOT <葉>` と同じ構文形）。
/// - `Expression` は「列識別子 × 数値リテラルの比較（`= < <= > >=`）」の形に**限り**
///   許可する（数値 4 型の列向け式レーン。`Expr::Call`＝UDF 呼び出しは持ち込めない
///   ので既定の UDF レジストリで足りる）。`Not(Expression)` は構文段が演算子反転で
///   押し下げるため生成されず拒否する。
/// - `Or` は分岐 2 個以上・各分岐が空でないものに限り許可し、分岐の中身を再帰検査する
///   （空の分岐は恒真になり fail-open なので拒否。再帰は [`MAX_PREDICATE_DML_DEPTH`]
///   で打ち切る）。`Not(Or)` は構文段が押し下げるため拒否する。
/// - それ以外（`PredicateCall`〔RLS 述語 `visible()`。`Or` の内側を含む〕・
///   `BoolColumn`・`Not(Not)`・許可形以外を包む `Not`・`InSubquery`／`Exists`／
///   `ScalarSubqueryCompare`／`QuantifiedSubqueryCompare`〔NoSQL にサブクエリ構文が無い〕）はすべて `42601` で
///   拒否する。網羅的 `match` にし、将来 variant 追加時に無言で穴が開かないようにする。
fn reject_unsupported_predicate_dml_forms(
    predicates: &[crate::sql::allowlist::WherePredicate],
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    if predicates.is_empty() {
        return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
            "predicate-form update/delete requires at least one filter predicate",
        ));
    }
    reject_unsupported_predicate_dml_conjunction(predicates, 0)
}

/// 述語形 DML の再帰検査の深さ上限（`Or` の入れ子。構文段の上限と同値）。
const MAX_PREDICATE_DML_DEPTH: usize = crate::sql::udf_call::MAX_EXPR_DEPTH;

/// [`reject_unsupported_predicate_dml_forms`] の再帰本体（`AND` 列 1 本分）。
fn reject_unsupported_predicate_dml_conjunction(
    predicates: &[crate::sql::allowlist::WherePredicate],
    depth: usize,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::{SqlSurfaceError, WherePredicate};
    use crate::sql::udf_call::{BinOp, Expr};

    let unsupported = || {
        SqlSurfaceError::unsupported(
            "predicate form is not supported for NoSQL update/delete filter",
        )
    };
    if depth > MAX_PREDICATE_DML_DEPTH {
        return Err(SqlSurfaceError::payload_too_large(
            "predicate nesting exceeds the allowed depth",
        ));
    }
    for predicate in predicates {
        match predicate {
            WherePredicate::Equality { .. }
            | WherePredicate::BoolEquality { .. }
            | WherePredicate::Between { .. }
            | WherePredicate::IsNull { .. }
            | WherePredicate::Compare { .. }
            | WherePredicate::InList { .. } => {}
            // SQL-54（Issue #1781）: 左辺が裸の列の LIKE／ILIKE だけを許可する（式左辺は拒否）。
            WherePredicate::Prefix { lhs, .. } if lhs.as_bare_column().is_some() => {}
            // `Not` は内側が許可葉のいずれかである場合に限り通す（再帰しない）。
            WherePredicate::Not(inner) => match inner.as_ref() {
                WherePredicate::Equality { .. }
                | WherePredicate::BoolEquality { .. }
                | WherePredicate::Between { .. }
                | WherePredicate::IsNull { .. }
                | WherePredicate::Compare { .. }
                | WherePredicate::InList { .. } => {}
                WherePredicate::Prefix { lhs, .. } if lhs.as_bare_column().is_some() => {}
                WherePredicate::Prefix { .. }
                | WherePredicate::PredicateCall { .. }
                | WherePredicate::Expression(_)
                | WherePredicate::BoolColumn { .. }
                | WherePredicate::Not(_)
                | WherePredicate::Or(_)
                | WherePredicate::InSubquery { .. }
                | WherePredicate::Exists { .. }
                | WherePredicate::ScalarSubqueryCompare { .. }
                | WherePredicate::QuantifiedSubqueryCompare { .. }
                | WherePredicate::IdCompare { .. }
                | WherePredicate::CorrelatedExists(_) => return Err(unsupported()),
            },
            // 列 × 数値リテラルの比較の形に限る（`Call` 等は持ち込ませない）。
            WherePredicate::Expression(expr) => match expr {
                Expr::Binary { op, lhs, rhs }
                    if matches!(
                        op,
                        BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                    ) && matches!(lhs.as_ref(), Expr::Ident(_))
                        && matches!(rhs.as_ref(), Expr::Number(_)) => {}
                _ => return Err(unsupported()),
            },
            WherePredicate::Or(branches) => {
                if branches.len() < 2 {
                    return Err(unsupported());
                }
                for branch in branches {
                    if branch.is_empty() {
                        return Err(unsupported());
                    }
                    reject_unsupported_predicate_dml_conjunction(branch, depth.saturating_add(1))?;
                }
            }
            WherePredicate::Prefix { .. }
            | WherePredicate::PredicateCall { .. }
            | WherePredicate::BoolColumn { .. }
            | WherePredicate::InSubquery { .. }
            | WherePredicate::Exists { .. }
            | WherePredicate::ScalarSubqueryCompare { .. }
            | WherePredicate::QuantifiedSubqueryCompare { .. }
            | WherePredicate::IdCompare { .. }
            | WherePredicate::CorrelatedExists(_) => return Err(unsupported()),
        }
    }
    Ok(())
}

fn dictionary_required_columns(
    schema: &crate::catalog::TableSchema,
) -> Result<(usize, usize), String> {
    // `path`／`body` は documents テーブルの構造契約（TASK-109・PLAN-5）のため TEXT 限定。
    // VARCHAR は意図的に非対象（Issue #2305）。
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
    // Issue #1765（WIRE-22・TASK-294）: 第 3 要素に解決済みの COPY オプションを
    // 載せる（破壊的変更。wire-server の出力経路が HEADER・DELIMITER・NULL・
    // QUOTE・ESCAPE を適用するために参照する）。
    To(
        crate::sql::allowlist::CopyFormat,
        crate::sql::exec::QueryResult,
        crate::sql::copy_options::CopyOptions,
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
    /// `INSERT INTO <表> (<列>...) <SELECT> [RETURNING ...] [USING OPERATION_ID ...]`
    /// （SQL-51・Issue #1732）。内側 SELECT は既存の SELECT 検証器を通過済みで、構文・内側の検証・
    /// 列数／型の束縛までを担う。実行腕は束縛の後に
    /// `execute_parsed_insert_select`（Issue #1733）へ結線済みで、内側 SELECT の RLS 適用・挿入・台帳記録
    /// （`for_insert_select` のハッシュ。Issue #1734）まで行う。ON CONFLICT 併用・ビュー・一時表・`$n`・
    /// `USING PLAN` は実行前に `0A000` で拒否する（fail-closed）。
    ///
    /// **BREAKING CHANGE**（Issue #1732）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチするクレート外コードは追随が必要。
    InsertSelect(crate::sql::allowlist::ValidatedInsertSelect),
    /// データ変更 CTE を含む最上位の `WITH` 文（`WITH d AS (DELETE ... RETURNING ...) SELECT ... FROM d`。
    /// SQL-63・Issue #1876・#1877）。構文・名前・`RETURNING` 公開列・実行形のゲート（実行する形は副文 1 つ
    /// 以上と、副文を読む主クエリまたは実表等を読む主クエリ）までを通過した実行計画で、実行腕は全副文と
    /// 文末の `USING OPERATION_ID` 台帳記録を 1 つの書き込みトランザクションで行う。全副文と主クエリは文の
    /// 開始時点の状態を読み、副文を読む主クエリには副文の `RETURNING` 行へ外側処理を適用する
    /// （`core/dm_cte_exec.rs`）。読み取り専用の [`ParsedSql::Statement`] へ載せないのは、書き込みを伴う
    /// 文を wire 側が非書き込みと誤判定しないため。
    ///
    /// **BREAKING CHANGE**（Issue #1876）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチするクレート外コードは追随が必要（`#[non_exhaustive]`）。
    DataModifyingCte(Box<crate::sql::allowlist::ValidatedDataModifyingCte>),
    /// 自動更新可能なビューへの `INSERT`（TABLE-43・Issue #1671）。送られた文の形のまま保持し、
    /// 基表への書き換えは Execute／Describe 時に現在のカタログで行う（Parse と Execute の間の
    /// `CREATE OR REPLACE VIEW`／`DROP VIEW` を取り込む）。自動更新不可のビューは Parse で `55000`。
    ViewInsert(crate::sql::allowlist::ValidatedViewInsert),
    /// 自動更新可能なビューへの `DELETE`（TABLE-43・Issue #1672）。[`ParsedSql::ViewInsert`] と同じく
    /// 送られた文の形のまま保持し、基表への書き換えは Execute／Describe 時に現在のカタログで行う。
    ViewDelete(crate::sql::allowlist::ValidatedViewDelete),
    /// 自動更新可能なビューへの `UPDATE`（TABLE-43・Issue #1672）。[`ParsedSql::ViewDelete`] と同じ扱い。
    ViewUpdate(crate::sql::allowlist::ValidatedViewUpdate),
    /// `TRUNCATE TABLE ... USING OPERATION_ID '<id>'`（SQL-22）。
    Truncate(crate::sql::allowlist::ValidatedTruncate),
    /// `DELETE`（単一行・`id` 完全一致形／述語形。SQL-18・SQL-19）。
    Delete(crate::sql::allowlist::DeleteStatement),
    /// `UPDATE`（単一行・`id` 完全一致形／述語形。SQL-17・SQL-19。`RETURNING` は
    /// セッション経路（autocommit・明示/暗黙トランザクション内。Issue #1272）のみ。SQL-21）。
    Update(crate::sql::allowlist::ValidatedUpdateForm),
    /// `UPDATE <表> [[AS] <別名>] SET ... FROM <表の並び> [WHERE ...]`（SQL-53 (b)・Issue #1754）。
    /// 構文と表の存在確認までを担い、列の解決・型照合は束縛（`sql::dml_from::bind_update_from`）が
    /// 実行腕・Describe 腕で行う。実行腕は束縛の後に `0A000` で止める（副作用ゼロ。結合の実行は
    /// #1755、RLS と `RETURNING` は #1756、台帳の content_hash は #1757 で構文段に実装済み）。既存の [`Self::Update`] に
    /// フィールドを足さず別 variant にしているのは、FROM を黙って無視する実行経路（fail-open）を
    /// 作らず、網羅 `match` の追随漏れをコンパイラに検出させるため。
    ///
    /// **BREAKING CHANGE**（Issue #1754）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチするクレート外コードは追随が必要。
    UpdateFrom(Box<crate::sql::allowlist::ValidatedUpdateFrom>),
    /// `DELETE FROM <表> [[AS] <別名>] USING <表の並び> [WHERE ...]`（SQL-53 (b)・Issue #1754）。
    /// 契約は [`Self::UpdateFrom`] と同じ。
    ///
    /// **BREAKING CHANGE**（Issue #1754）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチするクレート外コードは追随が必要。
    DeleteUsing(Box<crate::sql::allowlist::ValidatedDeleteUsing>),
    /// `BEGIN`／`COMMIT`／`ROLLBACK`（SQL-31・TASK-221）。トランザクション文脈を
    /// 持たない [`EngineCore::execute_parsed_in_session`] はこの variant を
    /// `0A000` で拒否する（トランザクション対応の実行入口は
    /// [`EngineCore::execute_sql_in_txn`]／[`EngineCore::execute_parsed_in_txn`]）。
    Transaction(crate::sql::transaction::TxnControl),
    /// `DROP TABLE [IF EXISTS] <table> [, <table>]*`（SQL-23・TASK-203、Issue #902。複数指定は
    /// TABLE-37 (1)・Issue #1639）。DDL 実行権限ゲート
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
    /// `CREATE [GLOBAL|LOCAL] {TEMP|TEMPORARY} TABLE ... [ON COMMIT ...]`
    /// （TABLE-44・Issue #1683）。永続表の [`CreateTable`](Self::CreateTable) とは別 variant
    /// にして、確認漏れが「一時表の定義が全テナント共有の永続表になる」fail-open にならないよう、
    /// 網羅 `match` をコンパイル時に強制する。DDL 実行権限ゲートの対象外（セッション内で閉じる）。
    /// 実行は `EngineCore::execute_parsed_in_session` の本 variant の分岐が担う。
    CreateTempTable(crate::sql::allowlist::ValidatedCreateTempTable),
    /// 一時表だけを参照する文を、セッション専用の一時コアで実行する形（TABLE-44・Issue #1684）。
    /// `INSERT`／`UPDATE`／`DELETE`／`TRUNCATE`／読み取り系／索引 DDL を包む。永続コアの実行器へ
    /// 渡すと同名の永続表へ作用してしまうため、永続側の [`ParsedSql`] とは別 variant にして、
    /// 網羅 `match` の確認漏れをコンパイル時に検出する（`CreateTempTable` と同じ理由）。
    /// 一時カタログの世代を持ち、一時表の作成・削除・索引 DDL の後に実行すると `0A000` で拒否する。
    SessionRouted(Box<SessionRoutedSql>),
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
    /// 拡張クエリプロトコルの Parse が検証済みの `COPY` 文（Issue #1759・WIRE-21 (1)・ERR-6 の
    /// ポインタ）。Parse／Bind／Describe だけを受理し、行・台帳・世代には一切触れない。
    /// Execute は wire-server が横取りして CopyIn／CopyOut を実行する（Issue #1760）ため、
    /// [`EngineCore::execute_parsed_in_session`]・[`EngineCore::execute_parsed_in_txn`] の
    /// 本 variant の腕は他の呼び出し元に対する防御的な fail-closed（`0A000`）として残す。
    /// 簡易クエリ（`Q`）の `COPY` は本 variant を経由せず `begin_copy*` へ直行する。
    /// [`EngineCore::parse_sql`] は `COPY` を受理しない（拡張クエリの Parse だけが生成する）。
    ///
    /// **BREAKING CHANGE**（Issue #1759）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチするクレート外コードは追随が必要。
    Copy(crate::sql::allowlist::ValidatedCopy),
    /// 分割実行 DML の制御文・説明文（Issue #1129・SQL-19）: `SHOW PARTITIONED DML`・
    /// `CANCEL PARTITIONED DML`・`EXPLAIN UPDATE|DELETE ... PARTITIONED`。分割実行そのもの
    /// （`UPDATE`／`DELETE ... PARTITIONED`）は [`Self::Update`]／[`Self::Delete`] の述語形が
    /// 修飾を保持する。`SHOW`・`EXPLAIN` は読み取りのみ、`CANCEL` は autocommit 専用の書き込み。
    ///
    /// **BREAKING CHANGE**（Issue #1129）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチするクレート外コードは追随が必要。
    Partitioned(crate::sql::partitioned::PartitionedControl),
    /// `ALTER TABLE <table> ADD COLUMN <column> <type>`（SQL-23・TASK-202、
    /// Issue #900）・`ALTER TABLE <table> ADD [CONSTRAINT <name>] UNIQUE
    /// (<col>[, ...])`／`DROP CONSTRAINT <name>`（TABLE-16・TASK-204、
    /// Issue #1067）・`ALTER TABLE <table> ADD [CONSTRAINT <name>] CHECK (<述語>)`
    /// （TABLE-16・TASK-204、Issue #1068）の 4 形状の和。DDL 実行権限ゲート
    /// （`sql::ddl::require_ddl_permission`）の判定は
    /// [`DropTable`](Self::DropTable)／[`CreateTable`](Self::CreateTable) と
    /// 同じく `EngineCore::execute_parsed_in_session` が担い、
    /// `validate_alter_table_tokens` 自体はカタログ照会（テーブル・列・ENUM 型の
    /// 存在確認）を一切行わない（[`crate::sql::allowlist::ValidatedAlterTable`]
    /// ドキュメント参照）。
    ///
    /// **BREAKING CHANGE**（Issue #900・#1067）: 本 variant の追加、および中身の
    /// `ValidatedAlterTableAddColumn` から `ValidatedAlterTable`（4 形状の和）への
    /// 変更により `ParsedSql` を網羅的にマッチする既存コード
    /// （`crate::core::EngineCore`）はすべて更新済み。クレート外で `ParsedSql` を
    /// 網羅的にマッチするコードがあれば追随が必要。
    ///
    /// **BREAKING CHANGE**（Issue #1068）: `ValidatedAlterTable::AddCheck` variant
    /// の追加により、`ValidatedAlterTable` を網羅的にマッチするクレート外の
    /// コードは追随が必要。
    ///
    /// **BREAKING CHANGE**（Issue #1167）: `ValidatedAlterTable::DropColumn`・
    /// `AlterColumnType` variant（`DROP COLUMN`／`ALTER COLUMN ... TYPE`）を追加した。
    /// 権限ゲート・Describe・明示トランザクション内 DDL の拒否は本 variant 単位で
    /// 判定するため、新形状も既存形状と同じ扱いになる。
    ///
    /// **BREAKING CHANGE**（Issue #1632）: `ValidatedAlterTable::AlterColumnDropNotNull`
    /// variant（`ALTER COLUMN ... DROP NOT NULL`）を追加した。
    ///
    /// **BREAKING CHANGE**（Issue #1633）: `ValidatedAlterTable::Multiple`（複数アクション）
    /// variant と `ValidatedAlterTableAddColumn::if_not_exists` を追加した。権限ゲート・
    /// 明示トランザクション内 DDL の拒否は `AlterTable` 単位で判定するため新形状も同じ扱い。
    AlterTable(crate::sql::allowlist::ValidatedAlterTable),
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
    /// `DROP MATERIALIZED VIEW`（TABLE-42 (5)・TABLE-37、Issue #1667）。構造検証のみで
    /// カタログを照会しない（`DropView` と同じ設計）。
    DropMaterializedView(crate::sql::allowlist::ValidatedDropMaterializedView),
    /// `CREATE MATERIALIZED VIEW [IF NOT EXISTS] <name> [(<alias>, ...)] AS <body>
    /// [WITH [NO] DATA]`（TABLE-42・Issue #1664）。DDL 実行権限ゲートの判定と初回実体化は
    /// `EngineCore::execute_parsed_in_session` の同名腕が担い、構文検証
    /// （`validate_create_materialized_view_tokens`）はカタログを一切照会しない（`CreateView` と
    /// 同じ設計）。
    ///
    /// **BREAKING CHANGE**（Issue #1664）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチする既存コードはすべて更新済み。
    CreateMaterializedView(crate::sql::allowlist::ValidatedCreateMaterializedView),
    /// `CREATE TABLE [IF NOT EXISTS] <name> [(<col>, ...)] AS <SELECT> [WITH [NO] DATA]`
    /// （CTAS。SQL-52・Issue #1678）。構文検証（`validate_create_table_as_tokens`）はカタログを一切
    /// 照会しない。DDL 権限ゲートの後、`EngineCore::execute_create_table_as`
    /// （`core/ctas_exec.rs`）が 1 つの write txn で表の作成と行の投入を行う。明示トランザクション内は
    /// 汎用フォールバックで `0A000`（DDL と同じ）。
    ///
    /// **BREAKING CHANGE**（Issue #1678）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチする既存コードはすべて更新済み。
    CreateTableAs(crate::sql::allowlist::ValidatedCreateTableAs),
    /// `REFRESH MATERIALIZED VIEW [CONCURRENTLY] <name> [WITH [NO] DATA]`（TABLE-42・Issue #1665）。
    /// DDL 権限ゲートと再実体化は `execute_parsed_in_session` の同名腕が担い、構文検証は
    /// カタログを照会しない。明示トランザクション内は汎用フォールバックで `0A000`
    /// （`CreateMaterializedView` と同じ）。
    ///
    /// **BREAKING CHANGE**（Issue #1665）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチする既存コードはすべて更新済み。
    RefreshMaterializedView(crate::sql::allowlist::ValidatedRefreshMaterializedView),
    /// `CREATE INDEX <name> ON <table> [USING hnsw] (<col>[, ...])`（TASK-206・
    /// INDEX-7・SQL-23、Issue #908）。DDL 実行権限ゲート
    /// （`sql::ddl::require_ddl_permission`）の判定は `EngineCore::
    /// execute_parsed_in_session` が担い、`validate_create_index_tokens` 自体は
    /// カタログ照会を一切行わない（`CreateView` と同じ設計）。
    CreateIndex(crate::sql::allowlist::ValidatedCreateIndex),
    /// `DROP INDEX <name>`（TASK-206・INDEX-7・SQL-23、Issue #908）。
    DropIndex(crate::sql::allowlist::ValidatedDropIndex),
    /// `CREATE TYPE <name> AS ENUM (...)`（TABLE-14・SQL-23・TASK-198、
    /// Issue #1194）。権限ゲートは `execute_parsed_in_session` が担う。
    CreateType(crate::sql::allowlist::ValidatedCreateType),
    /// `DROP TYPE <name>`（TABLE-14・SQL-23・TASK-198、Issue #1194）。
    DropType(crate::sql::allowlist::ValidatedDropType),
    /// `CREATE SEQUENCE`（TABLE-24、Issue #1688）。
    CreateSequence(crate::sql::allowlist::ValidatedCreateSequence),
    /// `DROP SEQUENCE`（TABLE-24、Issue #1688）。
    DropSequence(crate::sql::allowlist::ValidatedDropSequence),
    /// `DROP FUNCTION <name> [(<型>, ...)]`（SQL-66・TASK-309、Issue #1920）。
    ///
    /// **BREAKING CHANGE**（Issue #1920）: `ParsedSql` への variant 追加。
    DropFunction(crate::sql::allowlist::ValidatedDropFunction),
    /// `CREATE [OR REPLACE] TRIGGER`（TABLE-45、Issue #1722）。
    ///
    /// **BREAKING CHANGE**（Issue #1722）: `ParsedSql` への variant 追加。
    CreateTrigger(crate::sql::allowlist::ValidatedCreateTrigger),
    /// `DROP TRIGGER [IF EXISTS] <名前> ON <表>`（TABLE-45、Issue #1722）。
    DropTrigger(crate::sql::allowlist::ValidatedDropTrigger),
    /// `CREATE SCHEMA [IF NOT EXISTS] <name>`（TABLE-27、Issue #1713）。
    ///
    /// **BREAKING CHANGE**（Issue #1713）: `ParsedSql` への variant 追加。
    CreateSchema(crate::sql::allowlist::ValidatedCreateSchema),
    /// `DROP SCHEMA [IF EXISTS] <name> [RESTRICT]`（TABLE-27、Issue #1713）。
    DropSchema(crate::sql::allowlist::ValidatedDropSchema),
    /// `ALTER SCHEMA <name> RENAME TO <new>`（TABLE-27、Issue #1716）。**BREAKING CHANGE**: variant 追加。
    AlterSchema(crate::sql::allowlist::ValidatedAlterSchema),
    /// `ALTER SEQUENCE`（TABLE-24、Issue #1689）。
    AlterSequence(crate::sql::allowlist::ValidatedAlterSequence),
    /// `SELECT setval('<名>', <n>[, <is_called>])`／`SELECT lastval()` の単独形（TABLE-24・
    /// ERR-6、Issue #1691）。書き込み txn を伴うため、ストレージに触れない `ConstSelect`（#1531）
    /// には乗せない。結果は 1 行 1 列の `bigint`（`SqlOutcome::Query`）。
    ///
    /// **BREAKING CHANGE**（Issue #1691）: 本 variant の追加により `ParsedSql` を網羅的に
    /// マッチする既存コードはすべて更新済み。
    SequenceFunction(crate::sql::allowlist::ValidatedSequenceFunction),
}

/// 読み取り文（[`ParsedSql::Statement`]）と書き込み文（INSERT／UPDATE／DELETE の原子版）が
/// 現在時刻関数を受理する束縛スコープを開く（SQL-37・Issue #1586・#2188。許可リスト型の
/// fail-closed）。`txn_start_micros` は `Active` トランザクション中のトランザクション開始時刻
/// （`None` は autocommit・Describe で、文の開始時刻と同値）。書き込み文で時刻関数が評価
/// されると、台帳エントリは v3 で評価時刻を記録する（`recovery::ledger`）。分割実行
/// （`Partitioned`）は固定時刻の専用スコープ（`TxnFixedOnly`）を別に開く。COPY／DDL／
/// カーソル等は `None` を返し、束縛が時刻関数を `0A000`（定数式は `42601`）で拒否する。
/// `ParsedSql` を網羅 `match` にして variant 追加時に判定漏れをコンパイルエラーで検出する。
/// `SHOW TimeZone` の結果列（列名 `TimeZone`・text の 1 列。実行と Describe が共有する）。
fn show_time_zone_columns() -> Vec<crate::sql::exec::ColumnMeta> {
    vec![crate::sql::exec::ColumnMeta::Computed {
        name: "TimeZone".to_string(),
        ty: Some(crate::catalog::ColumnType::Text),
    }]
}

/// `SHOW search_path` の結果列（列名 `search_path`・text の 1 列。実行と Describe が共有する。
/// TABLE-27・Issue #1715）。
fn show_search_path_columns() -> Vec<crate::sql::exec::ColumnMeta> {
    vec![crate::sql::exec::ColumnMeta::Computed {
        name: "search_path".to_string(),
        ty: Some(crate::catalog::ColumnType::Text),
    }]
}

impl ParsedSql {
    /// セッションの一時カタログに対する振り分けの包み（[`ParsedSql::SessionRouted`]）を外した、
    /// 文そのものの構文形を返す。包まれていなければ自分自身。wire-server が、実行した文の種類
    /// （書き込みか・カーソルの `FETCH` か等）を判定するために使う。包みを外した形を永続コアの
    /// 実行入口へ渡してはならない（一時表の文は一時コアでしか実行できない）。
    pub fn peel_session_route(&self) -> &ParsedSql {
        match self {
            ParsedSql::SessionRouted(routed) => routed.inner(),
            other => other,
        }
    }

    /// 振り分けの包みを持つ文について、一時カタログの世代・`search_path` の世代が準備時から
    /// 変わっていないかを検査する（陳腐化なら `0A000`。包みの無い文は常に `Ok`）。
    /// `execute_parsed_in_txn` の粗いゲートと同じ検査で、wire-server が包みを外して
    /// 独自に実行する経路（COPY の委譲。Issue #1760）が、ゲートを迂回しないために呼ぶ。
    pub fn require_session_current(
        &self,
        session: &crate::sql::mode::SessionState,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        match self {
            ParsedSql::SessionRouted(routed) => routed.require_current(session),
            _ => Ok(()),
        }
    }
}

/// セッション TimeZone を現在の文の束縛区間へ張る（`execute_parsed_in_session` 等の
/// セッション入口が呼ぶ。TZ を持たない `TIMESTAMPTZ` リテラルの解釈とDDL既定値の正規化が
/// 読む。TABLE-33・Issue #1575）。
fn session_time_zone_scope(
    session: &crate::sql::mode::SessionState,
) -> crate::datetime::SessionTimeZoneScope {
    crate::datetime::SessionTimeZoneScope::enter(session.time_zone().clone())
}

fn statement_clock_scope(
    parsed: &ParsedSql,
    txn_start_micros: Option<i64>,
) -> Result<Option<crate::sql::time_fn::ClockScope>, crate::sql::allowlist::SqlSurfaceError> {
    let is_read = match parsed {
        // 一時表の文は、中身の文種で判定する（時刻の確定は永続側と同じ規則）。
        ParsedSql::SessionRouted(routed) => {
            return statement_clock_scope(routed.inner(), txn_start_micros)
        }
        ParsedSql::Statement(_)
        | ParsedSql::Insert(_)
        | ParsedSql::InsertSelect(_)
        // SQL-63・Issue #1876: 副文と主クエリは同じ文の開始時刻を共有する。
        | ParsedSql::DataModifyingCte(_)
        | ParsedSql::ViewInsert(_)
        | ParsedSql::ViewDelete(_)
        | ParsedSql::ViewUpdate(_)
        | ParsedSql::Delete(_)
        | ParsedSql::Update(_)
        | ParsedSql::UpdateFrom(_)
        | ParsedSql::DeleteUsing(_) => true,
        ParsedSql::Truncate(_)
        | ParsedSql::Transaction(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::CreateTable(_)
        | ParsedSql::CreateTableAs(_)
        | ParsedSql::CreateTempTable(_)
        | ParsedSql::Cursor(_)
        | ParsedSql::Partitioned(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateView(_)
        | ParsedSql::DropView(_)
        | ParsedSql::DropMaterializedView(_)
        // TABLE-42・Issue #1664: `CreateView` と同じく本文の時刻関数は束縛段で拒否する（fail-closed）。
        | ParsedSql::CreateMaterializedView(_)
        | ParsedSql::RefreshMaterializedView(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_)
        | ParsedSql::CreateType(_)
        | ParsedSql::DropType(_)
        | ParsedSql::CreateSequence(_)
        | ParsedSql::DropSequence(_)
        | ParsedSql::DropFunction(_)
        | ParsedSql::CreateTrigger(_)
        | ParsedSql::DropTrigger(_)
        | ParsedSql::CreateSchema(_)
        | ParsedSql::DropSchema(_)
        | ParsedSql::AlterSchema(_)
        | ParsedSql::AlterSequence(_)
        | ParsedSql::SequenceFunction(_)
        | ParsedSql::Copy(_) => false,
    };
    if !is_read {
        return Ok(None);
    }
    let stmt_start_micros = crate::sql::time_fn::now_micros()?;
    Ok(Some(crate::sql::time_fn::ClockScope::enter(
        crate::sql::time_fn::StatementClock {
            txn_start_micros: txn_start_micros.unwrap_or(stmt_start_micros),
            stmt_start_micros,
        },
    )))
}

/// INSERT 文だけが SEQUENCE の採番文脈（アロケータとセッションの `currval` 状態）を持つ
/// スコープを開く（TABLE-24・Issue #1690。許可リスト型の fail-closed）。`tenant` の INSERT 書き込みが
/// 台帳照合の後に読む（`sql::sequence_scope`）。autocommit・明示トランザクションの両入口が呼ぶ。
/// `ParsedSql` を網羅 `match` にして variant 追加時の判定漏れをコンパイルエラーで検出する。
fn insert_sequence_scope(
    storage: &Storage,
    session: &crate::sql::mode::SessionState,
    parsed: &ParsedSql,
) -> Option<crate::sql::sequence_scope::SequenceScope> {
    let is_insert = match parsed {
        ParsedSql::SessionRouted(routed) => {
            return insert_sequence_scope(storage, session, routed.inner())
        }
        ParsedSql::Insert(_) | ParsedSql::InsertSelect(_) | ParsedSql::ViewInsert(_) => true,
        // SQL-63・Issue #1876: 副文に `INSERT` を含むときだけ採番・既定時刻の文脈を張る。
        ParsedSql::DataModifyingCte(v) => v
            .dm_ctes
            .iter()
            .any(|d| d.command == crate::sql::returning::DmlCommand::Insert),
        ParsedSql::Statement(_)
        | ParsedSql::Truncate(_)
        | ParsedSql::Delete(_)
        | ParsedSql::Update(_)
        | ParsedSql::UpdateFrom(_)
        | ParsedSql::DeleteUsing(_)
        | ParsedSql::ViewDelete(_)
        | ParsedSql::ViewUpdate(_)
        | ParsedSql::Transaction(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::CreateTable(_)
        | ParsedSql::CreateTableAs(_)
        | ParsedSql::CreateTempTable(_)
        | ParsedSql::Cursor(_)
        | ParsedSql::Partitioned(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateView(_)
        | ParsedSql::DropView(_)
        | ParsedSql::DropMaterializedView(_)
        | ParsedSql::CreateMaterializedView(_)
        | ParsedSql::RefreshMaterializedView(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_)
        | ParsedSql::CreateType(_)
        | ParsedSql::DropType(_)
        | ParsedSql::CreateSequence(_)
        | ParsedSql::DropSequence(_)
        | ParsedSql::DropFunction(_)
        | ParsedSql::CreateTrigger(_)
        | ParsedSql::DropTrigger(_)
        | ParsedSql::CreateSchema(_)
        | ParsedSql::DropSchema(_)
        | ParsedSql::AlterSchema(_)
        | ParsedSql::AlterSequence(_)
        | ParsedSql::SequenceFunction(_)
        | ParsedSql::Copy(_) => false,
    };
    is_insert.then(|| {
        crate::sql::sequence_scope::SequenceScope::enter(
            std::sync::Arc::clone(storage.sequence_allocator()),
            session.sequences_handle(),
        )
    })
}

/// INSERT 文だけが列 `DEFAULT now()` 系の評価時刻を持つ束縛スコープを開く（SQL-37・
/// TABLE-16・Issue #1587。許可リスト型の fail-closed）。`txn_start_micros` は `Active`
/// トランザクション中のトランザクション開始時刻（`None` は autocommit で、文の開始時刻と
/// 同値）。式束縛用の [`crate::sql::time_fn::ClockScope`] とは別スロットで、`INSERT VALUES`
/// 内の時刻関数は受理しない（#2188 の担当）。`ParsedSql` を網羅 `match` にして variant 追加時の
/// 判定漏れをコンパイルエラーで検出する。
fn insert_default_clock_scope(
    parsed: &ParsedSql,
    txn_start_micros: Option<i64>,
) -> Result<Option<crate::sql::time_fn::DefaultClockScope>, crate::sql::allowlist::SqlSurfaceError>
{
    let is_insert = match parsed {
        ParsedSql::SessionRouted(routed) => {
            return insert_default_clock_scope(routed.inner(), txn_start_micros)
        }
        ParsedSql::Insert(_) | ParsedSql::InsertSelect(_) | ParsedSql::ViewInsert(_) => true,
        // SQL-63・Issue #1876: 副文に `INSERT` を含むときだけ採番・既定時刻の文脈を張る。
        ParsedSql::DataModifyingCte(v) => v
            .dm_ctes
            .iter()
            .any(|d| d.command == crate::sql::returning::DmlCommand::Insert),
        ParsedSql::Statement(_)
        | ParsedSql::Truncate(_)
        | ParsedSql::Delete(_)
        | ParsedSql::Update(_)
        | ParsedSql::UpdateFrom(_)
        | ParsedSql::DeleteUsing(_)
        | ParsedSql::ViewDelete(_)
        | ParsedSql::ViewUpdate(_)
        | ParsedSql::Transaction(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::CreateTable(_)
        | ParsedSql::CreateTableAs(_)
        | ParsedSql::CreateTempTable(_)
        | ParsedSql::Cursor(_)
        | ParsedSql::Partitioned(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateView(_)
        | ParsedSql::DropView(_)
        | ParsedSql::DropMaterializedView(_)
        | ParsedSql::CreateMaterializedView(_)
        | ParsedSql::RefreshMaterializedView(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_)
        | ParsedSql::CreateType(_)
        | ParsedSql::DropType(_)
        | ParsedSql::CreateSequence(_)
        | ParsedSql::DropSequence(_)
        | ParsedSql::DropFunction(_)
        | ParsedSql::CreateTrigger(_)
        | ParsedSql::DropTrigger(_)
        | ParsedSql::CreateSchema(_)
        | ParsedSql::DropSchema(_)
        | ParsedSql::AlterSchema(_)
        | ParsedSql::AlterSequence(_)
        | ParsedSql::SequenceFunction(_)
        | ParsedSql::Copy(_) => false,
    };
    if !is_insert {
        return Ok(None);
    }
    // 式スコープ（`statement_clock_scope`）が既に確定した時刻があればそれを共有する。
    // `VALUES` の `now()` と省略列の `DEFAULT now()` が同一文で別の値にならず、台帳 v3 の
    // 評価時刻とも一致させるため（Issue #2188）。
    if let Some(clock) = crate::sql::time_fn::current_clock() {
        return Ok(Some(crate::sql::time_fn::DefaultClockScope::enter(clock)));
    }
    let stmt_start_micros = crate::sql::time_fn::now_micros()?;
    Ok(Some(crate::sql::time_fn::DefaultClockScope::enter(
        crate::sql::time_fn::StatementClock {
            txn_start_micros: txn_start_micros.unwrap_or(stmt_start_micros),
            stmt_start_micros,
        },
    )))
}

/// `name` が採番方式（暗黙の `id` を持たない）の表かを返す（TABLE-23・Issue #2199）。単一行形
/// `WHERE id = <n>` を述語形へ戻すかの判定にだけ使う。引けない場合は従来方式（`false`）として扱う:
/// 採番方式の表を従来方式と誤判定しても、id 指定の書き込みゲート（`0A000`）が fail-closed に拒否する。
fn allocated_table(lookup: &impl crate::sql::allowlist::TableLookup, name: &str) -> bool {
    crate::sql::allowlist::TableLookup::table_is_allocated(lookup, name).unwrap_or(false)
}

/// `read_txn`（確定済みスナップショットまたは明示トランザクションの書き込み
/// トランザクション。[`crate::storage::read_source::ReadSource`]）から `table_name` の
/// スキーマを取得する（[`EngineCore::read_txn_with_schema`] とエラー分類を共有する
/// 単一の写像。Issue #1179）。
fn table_schema_in(
    read_txn: &impl crate::storage::read_source::ReadSource,
    table_name: &str,
) -> Result<crate::catalog::TableSchema, crate::sql::allowlist::SqlSurfaceError> {
    crate::catalog::get_table_schema_in_txn(read_txn, table_name).map_err(|e| match e {
        CatalogError::TableNotFound(name) => {
            crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
        }
        // `CatalogError::Invalid`（識別子形式不正）は `catalog::table_lookup_error`
        // （`impl TableLookup for Storage` と共有する単一の写像本体）へ委譲し、
        // SQL 表層と同じ `42601`（unsupported syntax）へ分類する（Issue #728 PR #788
        // レビュー指摘）。`Invalid` 以外（内部破損系）は `Internal` に丸め込まれる。
        other => crate::catalog::table_lookup_error(other),
    })
}

/// 評価後射影形ビュー本文が参照するテーブル名を集める（入れ子の評価後射影形ビューは本文を辿る）。
fn buffered_body_tables(stmt: &crate::sql::allowlist::Statement, out: &mut Vec<String>) {
    use crate::sql::allowlist::Statement;
    match stmt {
        Statement::Select(v) => out.push(v.table_name().to_string()),
        Statement::Aggregate(v) => out.push(v.table_name().to_string()),
        Statement::Scan(v) => out.push(v.table_name().to_string()),
        Statement::Join(v) => out.extend(crate::sql::join::collect_join_tables(v)),
        Statement::SetOperation(v) => crate::sql::set_op::collect_branch_tables(&v.tree, out),
        Statement::BufferedView(v) => buffered_body_tables(&v.body, out),
        Statement::RecursiveCte(v) => recursive_cte_tables(v, out),
        _ => {}
    }
}

/// 再帰 CTE（SQL-35・Issue #1870）が実際に読む基底表の名前を集める。再帰項が読む CTE 名自身は
/// 含めない（同名の実表をストレージから引いてしまうと、CTE が実表に隠れる規則に反する）。
fn recursive_cte_tables(v: &crate::sql::allowlist::ValidatedRecursiveCte, out: &mut Vec<String>) {
    buffered_body_tables(&v.anchor, out);
    let mut recursive = Vec::new();
    buffered_body_tables(&v.recursive, &mut recursive);
    out.extend(
        recursive
            .into_iter()
            .filter(|t| !t.eq_ignore_ascii_case(&v.name)),
    );
}

/// 評価後射影形ビューの本文が非バイト列順の照合（ICU）の列を持つテーブルを参照し、かつ外側が
/// 順序・比較に依存するときは `0A000` で拒否する（照合が運ばれずバイト列順で黙って
/// 比較される意味論の fail-open を作らない。TABLE-28・Issue #2190）。
fn check_buffered_view_collations(
    read_txn: &impl crate::storage::read_source::ReadSource,
    view: &crate::sql::allowlist::ValidatedBufferedView,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    if !view.outer_depends_on_order() {
        return Ok(());
    }
    let mut names = Vec::new();
    buffered_body_tables(&view.body, &mut names);
    let schemas = table_schemas_in(read_txn, &names)?;
    let collated = schemas.values().any(|schema| {
        schema
            .columns
            .iter()
            .any(|c| c.collation.is_some_and(|coll| !coll.is_byte_order()))
    });
    if collated {
        return Err(
            crate::sql::allowlist::SqlSurfaceError::FeatureNotSupported {
                detail: "this collation is not supported in this expression".to_string(),
            },
        );
    }
    Ok(())
}

/// [`table_schema_in`] の複数テーブル版（重複名は 1 回だけ解決する）。
fn table_schemas_in(
    read_txn: &impl crate::storage::read_source::ReadSource,
    table_names: &[String],
) -> Result<
    std::collections::HashMap<String, crate::catalog::TableSchema>,
    crate::sql::allowlist::SqlSurfaceError,
> {
    let mut schemas = std::collections::HashMap::with_capacity(table_names.len());
    for name in table_names {
        if schemas.contains_key(name) {
            continue;
        }
        schemas.insert(name.clone(), table_schema_in(read_txn, name)?);
    }
    Ok(schemas)
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
        ParsedSql::InsertSelect(stmt) => stmt.operation_id().map(|id| id.as_str()),
        // SQL-63・Issue #1876: 文末の 1 つだけ（副文は持たない）。
        ParsedSql::DataModifyingCte(stmt) => stmt.operation_id.as_ref().map(|id| id.as_str()),
        ParsedSql::ViewInsert(stmt) => stmt.operation_id().map(|id| id.as_str()),
        ParsedSql::ViewDelete(stmt) => stmt.operation_id().map(|id| id.as_str()),
        ParsedSql::ViewUpdate(stmt) => stmt.operation_id().map(|id| id.as_str()),
        ParsedSql::Truncate(stmt) => stmt.operation_id.as_ref().map(|id| id.as_str()),
        ParsedSql::Delete(crate::sql::allowlist::DeleteStatement::SingleRow(v)) => {
            v.operation_id.as_ref().map(|id| id.as_str())
        }
        ParsedSql::Delete(crate::sql::allowlist::DeleteStatement::Predicate(v)) => {
            v.operation_id().map(|id| id.as_str())
        }
        ParsedSql::Update(crate::sql::allowlist::ValidatedUpdateForm::Single(v)) => {
            v.operation_id.as_ref().map(|id| id.as_str())
        }
        ParsedSql::Update(crate::sql::allowlist::ValidatedUpdateForm::Predicate(v)) => {
            v.operation_id().map(|id| id.as_str())
        }
        // SQL-53 (b)・Issue #1754: 実行腕が束縛後に必ず `0A000` で拒否する間は、同一トランザクションの
        // operation_id 登録を変えない（拒否される文が再利用検査の状態を汚さない）。実行を結線する
        // SQL 経路の dispatch 結線時にここを見直す（#1755・#1757 は実行入口と台帳ハッシュまで）。
        ParsedSql::UpdateFrom(_) | ParsedSql::DeleteUsing(_) => None,
        // 永続側の文を包んだ形は中身の `operation_id` を使う（一時表の文は `operation_id` を持たない）。
        ParsedSql::SessionRouted(routed) if routed.is_persistent() => {
            parsed_operation_id(routed.inner())
        }
        _ => None,
    }
}

/// `parsed` が分割実行に属する書き込み（`UPDATE`／`DELETE ... PARTITIONED`・`CANCEL
/// PARTITIONED DML`）か。明示・暗黙トランザクション内では黙って原子実行へ切り替えず
/// `25001` で拒否するための判定（Issue #1129）。
fn is_partitioned_write(parsed: &ParsedSql) -> bool {
    match parsed {
        ParsedSql::Delete(crate::sql::allowlist::DeleteStatement::Predicate(v)) => {
            v.partitioned().is_some()
        }
        ParsedSql::Update(crate::sql::allowlist::ValidatedUpdateForm::Predicate(v)) => {
            v.partitioned().is_some()
        }
        ParsedSql::Partitioned(control) => control.writes(),
        ParsedSql::SessionRouted(routed) if routed.is_persistent() => {
            is_partitioned_write(routed.inner())
        }
        _ => false,
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
/// Bind 時に型付きリテラルトークン（文字列は `StringLiteral`、`id`・`INTEGER`・
/// `BIGINT` は `Number`、`BOOLEAN` は `true`/`false` 識別子。Issue #1342）へ
/// 置換されるまでは `Token::Param` として保持され、SQL テキストへは
/// 一度も戻らない（インジェクション経路を作らない設計。`sql::params`
/// モジュールドキュメント参照）。
#[derive(Debug, Clone)]
pub struct PreparedSql {
    /// Parse 時に SCHEMA 修飾を解決したテナント（[`EngineCore::parse_sql_prepared_for_tenant`]。
    /// Bind のエラー文言で内部キーをそのテナントの SCHEMA 名へ戻すために使う。TABLE-27・Issue #1717）。
    resolution_tenant: Option<Box<str>>,
    tokens: Vec<crate::sql::lexer::Token>,
    param_count: u16,
    dummy_parsed: ParsedSql,
    /// 一時表（TABLE-44・Issue #1684）の経路。セッションを持たない Parse は常に `Plain`。
    /// セッション対応の Parse が一時表だけを参照する文を一時コアで構造検証したときは `Temp`
    /// （`dummy_parsed` も一時コア向けの形で包む。`core/temp_route.rs`）。
    route: temp_route::PreparedRoute,
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
    /// `$n` ごとの型公告（`param_types().len() == param_count`。WIRE-12）。
    /// Parse 時点のカタログ（スキーマ）だけから導出し、行には触れない。
    param_types: Vec<crate::sql::params::PreparedParamType>,
    /// Parse で宣言された型 OID から写した `$n` ごとの宣言型（`declared.len() ==
    /// param_count`。未宣言・0・unknown・持たない型は `None`。Issue #2436・WIRE-24 ポインタ）。
    /// 推論型（`param_types`）を上書きせず並べて保持する（変換可否の判定が両方を見るため）。
    /// Bind は値を宣言型として検証する（[`EngineCore::bind_prepared`]）。
    declared: Vec<Option<crate::sql::params::DeclaredParamType>>,
    /// Parse 時の構造検証 parse がセッション TimeZone を読んだか（ゾーン無し `TIMESTAMPTZ`
    /// リテラル・`AT LOCAL` の定数畳み込み。Issue #1576）。`true` の文は `dummy_parsed` を
    /// 実行へ再利用せず、Bind ごとにセッション TimeZone の下で再 parse する。
    session_tz_dependent: bool,
    /// Parse 時に `public.` 修飾を外した表名（TABLE-27・Issue #1714）。Parse 後に同名の一時表が
    /// 作られると未修飾名が一時表へ解決されるため、Bind・Describe の段で一時カタログと再照合する。
    public_qualified: Vec<String>,
    /// Parse 時に `public.` 修飾を外したトークンの添字（TABLE-27・Issue #1715）。`search_path` から
    /// `public` が外れたセッションの未修飾名ゲートが、これらの位置を除いて `tokens` を走査する
    /// （置換は 1 対 1 のため位置は保たれる）。
    qualified_positions: Vec<usize>,
    /// `CREATE TABLE` の作成対象名が `public.` で明示修飾されていたか（TABLE-27・Issue #1715）。
    /// 再 parse した `CREATE TABLE` へ戻し、作成先の判定（`search_path`）を飛ばすために使う。
    create_public: bool,
}

/// 解決済みトークンから再 parse した `DROP TABLE` に、Parse 時に外した `public.` 修飾の
/// 表名を戻す（TABLE-27・Issue #1714）。実行時の粗いゲートが同名の一時表との衝突を拒否する。
/// `CREATE TABLE`・`CREATE [MATERIALIZED] VIEW`・`CREATE SEQUENCE` には作成対象名の `public.` 修飾の
/// 有無を戻す（Issue #1715）。
fn attach_public_qualified(
    parsed: &mut ParsedSql,
    public_qualified: &[String],
    create_public: bool,
) {
    match parsed {
        ParsedSql::DropTable(stmt) => stmt.public_qualified = public_qualified.to_vec(),
        ParsedSql::CreateTable(stmt) => stmt.public_qualified = create_public,
        ParsedSql::CreateTableAs(stmt) => stmt.public_qualified = create_public,
        ParsedSql::CreateView(stmt) => stmt.public_qualified = create_public,
        ParsedSql::CreateMaterializedView(stmt) => stmt.public_qualified = create_public,
        ParsedSql::CreateSequence(stmt) => stmt.public_qualified = create_public,
        _ => {}
    }
}

impl PreparedSql {
    /// `$n` ごとの宣言型（`param_count()` と同じ長さ。宣言なしは `None`）。
    pub fn declared_param_types(&self) -> &[Option<crate::sql::params::DeclaredParamType>] {
        &self.declared
    }

    /// `$n` ごとの型公告（`param_count()` と同じ長さ。番号 1 始まりの `n` が
    /// `n - 1` 番目）。wire-server の `ParameterDescription` の OID・バイナリ
    /// 受理可否の判定に使う（WIRE-12）。値の宣言型としての検証は
    /// [`Self::declared_param_types`] と [`EngineCore::bind_prepared`] が担う（WIRE-24）。
    pub fn param_types(&self) -> &[crate::sql::params::PreparedParamType] {
        &self.param_types
    }

    /// `param_count() == 0`（`$n` を含まない文）のときに限り、構造検証済みの
    /// [`ParsedSql`] を返す。`$n` が無い文では `substitute_dummy` が恒等変換の
    /// ため、これは [`EngineCore::parse_sql`] の結果と構造的に同一である
    /// （wire-server が 0 パラメータ文を従来の Parsed 経路のまま保持するため）。
    /// ただし Parse 時の畳み込みがセッション TimeZone を読んだ文は `None` を返し、呼び出し元は
    /// Bind ごとに [`EngineCore::bind_prepared`] で再 parse する（Issue #1576）。
    pub fn parsed_if_unparameterized(&self) -> Option<&ParsedSql> {
        (self.param_count == 0 && !self.session_tz_dependent).then_some(&self.dummy_parsed)
    }

    /// Bind 後に portal が実際に保持する束縛値の総バイト数（同一 `$n` の出現
    /// ごとに値が複製されるため、参照回数を掛けた展開後の量。未参照の値は
    /// 保持されないため数えない）。wire-server が接続単位の保持量上限
    /// （`MAX_BOUND_PARAM_BYTES_PER_SESSION`）を束縛後の実量で判定するために
    /// 使う。`None`（NULL）は 0 として数える（別途 `bind_prepared` が拒否する）。
    pub fn expanded_bound_bytes(&self, values: &[Option<Vec<u8>>]) -> usize {
        self.tokens.iter().fold(0usize, |acc, t| match t {
            crate::sql::lexer::Token::Param(n) => {
                let len = usize::from(*n)
                    .checked_sub(1)
                    .and_then(|i| values.get(i))
                    .and_then(|v| v.as_ref())
                    .map_or(0, Vec::len);
                acc.saturating_add(len)
            }
            _ => acc,
        })
    }

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

    /// 呼び出し元が取得済みのスキーマ（ビュー書き込みの同一スナップショット由来。TABLE-43）を
    /// 最初から保持する。`take_schema` が基表名一致のときだけ返す点は [`Self::new`] と同じ。
    fn with_schema(storage: &'a Storage, table_name: &str, schema: TableSchema) -> Self {
        Self {
            storage,
            cached: std::cell::RefCell::new(Some((table_name.to_string(), schema))),
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

/// `metadata_filters` が参照する列インデックス
/// （[`crate::declarative_filter::MetadataFilter::column_index`]）を、束縛に
/// 使った `schema` で列名へ写像する（Issue #1066）。`EXPLAIN` の
/// `scalar_plan:` 行への使用索引名注記——`EngineCore::explain_engine_for` の
/// 被覆判定——専用のヘルパーで、`sql::exec` の実行経路（索引消費の判定は
/// `MetadataFilter` の値そのものを使い、列名へは写像しない）には影響しない。
/// 添字が `schema.columns` の範囲外（構造上到達しないはずだが多層防御）の
/// 列が 1 つでもあれば `None`（呼び出し元は索引名を出さない側へ fail-closed
/// に倒す）。`metadata_filters` が空なら `Some(Vec::new())`（「フィルタなし」
/// と「列名を確定できない」を区別する）。
fn metadata_filter_column_names<'a>(
    schema: &'a crate::catalog::TableSchema,
    metadata_filters: &[crate::declarative_filter::MetadataFilter],
) -> Option<Vec<&'a str>> {
    let mut names = Vec::with_capacity(metadata_filters.len());
    for f in metadata_filters {
        let col = schema.columns.get(f.column_index())?;
        names.push(col.name.as_str());
    }
    Some(names)
}

/// [`metadata_filter_column_names`] に式述語の数値列（Issue #1413・INDEX-7）を
/// 加えた版。数値列述語は `expr_filters` 側にあり `metadata_filters` には現れない
/// ため、索引名の被覆判定へ入れるには別途列を集める必要がある。`id` 述語は
/// `numeric_predicate_columns` が返さず対象外のまま（既存契約）。範囲外添字が
/// あれば `None`（fail-closed で名前なし）。
fn scalar_filter_column_names_with_numeric<'a>(
    schema: &'a crate::catalog::TableSchema,
    metadata_filters: &[crate::declarative_filter::MetadataFilter],
    expr_filters: &[crate::sql::udf_call::BoundExpr],
) -> Option<Vec<&'a str>> {
    let mut names = metadata_filter_column_names(schema, metadata_filters)?;
    for idx in crate::sql::scalar_plan::numeric_predicate_columns(expr_filters) {
        names.push(schema.columns.get(idx)?.name.as_str());
    }
    Some(names)
}

/// `column_names` の**全列**がいずれかのスカラー索引宣言（`(名前, 対象列)`。
/// [`crate::catalog::TableIndexDecls::scalar`]）で被覆されている場合に限り、
/// 被覆に用いる宣言の名前（貪欲法選択・グローバルな最小性は保証しない。
/// 昇順・重複排除は呼び出し元 [`crate::sql::explain::ExplainIndexNames::new`]
/// に委ねる）を返す（Issue #1066）。被覆されない列が 1 つでもあれば空 `Vec`。
///
/// **契約は「静的な宣言上の候補」であり「実行時に実際に使われた索引」の
/// 保証ではない**（2 巡目 codex-review P1 指摘対応・Issue #1066 PR #1155）。
/// 実行側 `sql::scalar_index::ScalarIndex::resolve_candidates` は候補
/// スロット比率が閾値を超える場合（`CandidateResolution::FallbackSelectivity`）
/// や候補取得不能な場合（`FallbackNoIndex`）に索引経由を諦め全走査へ縮退
/// するが、この縮退は可視行数・カーディナリティに依存する実行時値であり、
/// `EXPLAIN` はテーブル内容に一切依存しない静的判定のみを報告する既存契約
/// （`docs/design/explain-search-engine-exposure.md`「決定 1」節）のため
/// ここでは反映しない。反映するには `EXPLAIN` 自体が索引を構築し行データに
/// 依存する選択度を観測する必要があり、実行の副作用禁止契約に反すると同時に
/// テナントのカーディナリティを索引名の有無という副チャネルで漏らしうる
/// （同 doc「露出しない値」節の「実行時縮退結果」と同区分）。詳細は同 doc
/// 「Issue #1066」節参照。
fn scalar_index_names_for_columns(
    scalar_decls: &[(String, Vec<String>)],
    column_names: &[&str],
) -> Vec<String> {
    if column_names.is_empty() {
        return Vec::new();
    }
    let all_covered = column_names.iter().all(|col| {
        scalar_decls
            .iter()
            .any(|(_, cols)| cols.iter().any(|c| c == col))
    });
    if !all_covered {
        return Vec::new();
    }
    // codex-review P1 指摘対応（Issue #1066 PR #1155）: 実行側
    // `catalog::declared_index_targets_in_txn` は宣言名を捨て、対象テーブルの
    // 全スカラー宣言の列を和集合した単一の `ScalarIndex` を構築する（宣言別に
    // 個別の実行経路は存在しない）。そのため「述語列と 1 列でも交差する宣言を
    // 全て返す」だけでは、同じ列を複数宣言が重複して覆う場合に実行側で個別に
    // 使われない宣言まで「使用索引名」として表示してしまう。ここでは表示を
    // 「述語列を被覆する宣言集合」へ縮退させ、貪欲法（残り未被覆列を最も多く
    // 覆う宣言から、同数なら名前の昇順で）で選ぶ。
    //
    // 2 巡目 codex-review P1 指摘対応（Issue #1066 PR #1155）: 貪欲法は
    // 「被覆に必要な宣言数が最小」であることを保証しない（反例: 列
    // a..h、宣言 idx_a=(a,b,c,d)・idx_b=(a,b,e,f)・idx_c=(c,d,g,h) に対し
    // 述語列が a..h 全列のとき、貪欲法は最初に idx_a（4 列被覆）を選び
    // idx_a+idx_b+idx_c の 3 件を表示するが、idx_b+idx_c の 2 件でも
    // a..h 全列を被覆できる）。真の最小被覆集合の算出は NP-hard な
    // 厳密集合被覆問題であり、EXPLAIN はクエリ形状から決まる `WHERE` 述語列
    // （攻撃者が調整可能な入力）を被覆元に使うため、厳密解を指数時間で
    // 求める実装は非信頼入力に対する計算量 DoS 経路になり得る
    // （`coding-rust.md`「untrusted 入力の扱い」）。そのためここでは貪欲法を
    // 意図的に維持し、契約を「述語列を被覆するために必要な最小の宣言集合」
    // ではなく「述語列を被覆する宣言集合（貪欲法による選択。グローバルな
    // 最小性は保証しない）」と定義し直す（`docs/design/
    // explain-search-engine-exposure.md`「Issue #1066」節に追記）。
    // 列被覆が重複しない既存ケース（単一列・列ごとに別宣言）では従来と同じ
    // 結果になる。
    let mut remaining: Vec<&str> = column_names.to_vec();
    remaining.sort_unstable();
    remaining.dedup();
    let mut chosen: Vec<String> = Vec::new();
    while !remaining.is_empty() {
        let Some((name, covers)) = scalar_decls
            .iter()
            .map(|(name, cols)| {
                let covers: Vec<&str> = remaining
                    .iter()
                    .copied()
                    .filter(|col| cols.iter().any(|c| c == col))
                    .collect();
                (name, covers)
            })
            .filter(|(_, covers)| !covers.is_empty())
            .max_by(|(name_a, covers_a), (name_b, covers_b)| {
                covers_a
                    .len()
                    .cmp(&covers_b.len())
                    .then_with(|| name_b.cmp(name_a))
            })
        else {
            break;
        };
        chosen.push(name.clone());
        remaining.retain(|col| !covers.contains(col));
    }
    chosen
}

/// `scalar_plan:` の `index=` 注記へ出すスカラー索引名（INDEX-10、Issue #1650）。列述語の被覆名
/// （[`scalar_index_names_for_columns`]）に、WHERE の式述語が一致した式索引名を合流させる。
///
/// - 列述語が無い（`column_names` が空）: 式索引名だけ。
/// - 列述語がある: 全列が宣言で被覆されている場合に限り、被覆名と式索引名の和集合。被覆されない
///   列が 1 つでもある・宣言を読めていない（`scalar_decls == None`）なら空（実行時は全走査へ縮退
///   するため索引使用を主張しない）。昇順・重複排除は呼び出し元 `ExplainIndexNames::new` に委ねる。
fn scalar_index_names_with_expressions(
    scalar_decls: Option<&[(String, Vec<String>)]>,
    column_names: &[&str],
    expression_names: &[String],
) -> Vec<String> {
    if column_names.is_empty() {
        return expression_names.to_vec();
    }
    let Some(decls) = scalar_decls else {
        return Vec::new();
    };
    let mut names = scalar_index_names_for_columns(decls, column_names);
    if names.is_empty() {
        return Vec::new();
    }
    names.extend(expression_names.iter().cloned());
    names
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
            trigger_runtime: crate::trigger::TriggerRuntime::unwired(),
            require_where_for_dml: false,
            batch_limits: crate::batch_limits::BatchLimits::default(),
            transaction_limits: crate::sql::transaction::TransactionLimits::default(),
            dml_limits: crate::sql::parser::DmlLimits::default(),
            schema_limits: crate::schema_limits::SchemaLimits::default(),
            scan_limits: crate::scan_limits::ScanLimits::default(),
            group_by_limits: crate::group_by_limits::GroupByLimits::default(),
            temp_table_limits: crate::temp_table_limits::TempTableLimits::default(),
            jsonpath_limits: crate::jsonpath_limits::JsonPathLimits::default(),
            regex_limits: crate::regex_limits::RegexLimits::default(),
            nesting_limits: crate::nesting_limits::NestingLimits::default(),
            window_limits: crate::window_limits::WindowLimits::default(),
            join_limits: crate::join_limits::JoinSizeLimits::default(),
            correlated_limits: crate::correlated_limits::CorrelatedLimits::default(),
            recursive_cte_limits: crate::recursive_cte_limits::RecursiveCteLimits::default(),
            plpgsql_limits: crate::plpgsql_limits::PlpgsqlLimits::default(),
            result_limits: crate::result_limits::ResultLimits::default(),
            materialization_contexts: None,
            partitioned_dml_limits: crate::sql::parser::PartitionedDmlLimits::default(),
            dictionary_cache: DictionaryCache::new(),
            dictionary_config: crate::dictionary::DictionaryConfig::default(),
            sparse_index_cache: crate::sql::sparse_cache::SparseIndexCache::new(),
            sql_arena_cache: crate::sql::arena_cache::SqlArenaCache::new(),
            scalar_index_cache: crate::sql::scalar_index::ScalarIndexCache::new(),
            visible_bitmap_cache: crate::sql::visible_cache::VisibleBitmapCache::new(),
            search_engine_kind,
            hnsw_state,
            index_catalog_gate_cache: crate::catalog::IndexCatalogGateCache::new(),
            hnsw_scope: crate::search_engine::HnswScope::default(),
            explain_index_name_scans: std::sync::atomic::AtomicU64::new(0),
            materialized_cte_evals: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            cte_join_budget_cap_for_test: None,
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

    /// `catalog::IndexCatalogGateCache` の現在の統計を返す（codex-review Low
    /// 指摘対応・Issue #1065。テスト・運用観測用）。HNSW 適格性ゲート
    /// （`catalog::hnsw_targeted_in_txn`）がカタログ読み取り失敗で brute-force へ
    /// fail-closed 縮退した回数（`gate_read_failures`）と、キャッシュがミスし
    /// 索引カタログを実走査した回数（`catalog_scans`。Issue #1154）のみを持ち、
    /// テナント ID・行データ等の機微情報は含まない。`VectorCore` trait には
    /// 載せない固有メソッド（`core_api.snapshot` の対象外。`hnsw_index_cache_stats`
    /// と同じ方針）。
    pub fn index_catalog_gate_cache_stats(&self) -> crate::catalog::IndexCatalogGateCacheStats {
        self.index_catalog_gate_cache.stats()
    }

    /// [`Self::explain_index_name_scans`] の現在の統計を返す（Issue #1066
    /// codex-review P2 指摘対応・PR #1155 4 巡目。テスト・運用観測用）。索引名を
    /// 最終的に表示しない `EXPLAIN` 呼び出しでカタログ全件走査
    /// （`catalog::explain_index_names_in_txn`）が省略されていることを
    /// `tests/explain_index_names.rs` から固定するために使う。テナント ID・
    /// 索引名等の機微情報は含まない。`VectorCore` trait には載せない固有
    /// メソッド（`core_api.snapshot` の対象外。`index_catalog_gate_cache_stats`
    /// と同じ方針）。
    pub fn explain_index_name_scan_stats(&self) -> ExplainIndexNameScanStats {
        ExplainIndexNameScanStats {
            scans: self
                .explain_index_name_scans
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// CTE を入力に持つ JOIN の文全体バイト予算を作る（Issue #1861）。テストビルドでは上限を差し替えられる。
    fn cte_join_budget(&self) -> crate::sql::join::JoinBudget {
        #[cfg(test)]
        if let Some(cap) = self.cte_join_budget_cap_for_test {
            return crate::sql::join::JoinBudget::with_cap(cap);
        }
        crate::sql::join::JoinBudget::for_statement()
    }

    /// 具体化経路が CTE 本文を記憶域に対して実行した累計回数（SQL-62・Issue #1861）。
    /// `WITH c AS MATERIALIZED (...)` を複数回参照しても本文の実行が 1 回であることを
    /// `tests/sql62_cte_join_input.rs` から固定するために使う。`VectorCore` trait には
    /// 載せない固有メソッド（`core_api.snapshot` の対象外）。
    pub fn materialized_cte_eval_stats(&self) -> MaterializedCteEvalStats {
        MaterializedCteEvalStats {
            evaluations: self
                .materialized_cte_evals
                .load(std::sync::atomic::Ordering::Relaxed),
        }
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

    /// テスト専用: トリガ関数の呼び出し口を差し替える（トリガ関数の保存形式・PL/pgSQL 結線〔Issue #2228〕が
    /// 未整備のため、発火側の意味論を注入した呼び出し口で固定する。本番の結線は #2228 が行う）。
    #[cfg(test)]
    pub(crate) fn with_trigger_invoker_for_test(
        mut self,
        invoker: std::sync::Arc<dyn crate::trigger::call::TriggerFunctionInvoker>,
    ) -> Self {
        // 深さ上限は `with_trigger_limits` との呼び出し順に依存しないよう保つ。
        let limits = self.trigger_runtime.limits();
        self.trigger_runtime =
            crate::trigger::TriggerRuntime::with_invoker(invoker).with_limits(limits);
        self
    }

    /// トリガ呼び出しの入れ子深さ上限（[`crate::trigger_limits::TriggerLimits`]）を差し替えた
    /// ビルダーを返す（Issue #1727。TABLE-45 (5)）。`wire-server` の起動時 CLI フラグ
    /// `--max-trigger-depth` からのみ呼ぶ（クエリ・セッション変数・HTTP から到達する経路は無い）。
    /// 未呼び出しならハード上限と同値の既定。超過は `54001` で文全体を拒否する。
    pub fn with_trigger_limits(mut self, limits: crate::trigger_limits::TriggerLimits) -> Self {
        self.trigger_runtime = self.trigger_runtime.with_limits(limits);
        self
    }

    /// 現在のトリガ深さ上限（Issue #1727）。
    pub fn trigger_limits(&self) -> crate::trigger_limits::TriggerLimits {
        self.trigger_runtime.limits()
    }

    /// `WHERE` 省略の SQL `UPDATE`／`DELETE` を変更開始前に `42601` で拒否するか（Issue #2210・
    /// SQL-36 ポインタ。MySQL の `sql_safe_updates` 相当の opt-in）を差し替えたビルダーを返す。
    /// [`Self::with_ledger_mode`] と同型の所有権消費ビルダーで、`wire-server` が起動時に 1 回だけ
    /// 呼ぶ。明示した `WHERE`（`WHERE TRUE` を含む）・NoSQL 表層・`TRUNCATE` は影響を受けない。
    pub fn with_require_where_for_dml(mut self, enabled: bool) -> Self {
        self.require_where_for_dml = enabled;
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
    /// 設定する（`Storage` 構築側、または [`Self::with_write_lock_wait`]）。
    /// wire-server の起動時 CLI
    /// （`--max-transaction-duration-ms`・`--max-transaction-statements`。Issue #1553）
    /// からは、範囲検証済みの値が 1 回だけ注入される。
    pub fn with_transaction_limits(
        mut self,
        limits: crate::sql::transaction::TransactionLimits,
    ) -> Self {
        self.transaction_limits = limits;
        self
    }

    /// 書き込みロック待機上限（超過は `55P03`）を差し替えたビルダーを返す（Issue #1554・
    /// SQL-46 ポインタ）。`wire-server` の起動時 CLI `--write-lock-wait-ms` からプロセス
    /// 全体で 1 回だけ設定する契約。範囲検証は `storage::validate_write_lock_wait_ms`。
    /// セッション・テナント単位で差し替える経路は設けない。
    pub fn with_write_lock_wait(mut self, wait: std::time::Duration) -> Self {
        self.storage.set_write_lock_wait(wait);
        self
    }

    /// 現在の書き込みロック待機上限（読み取り専用。`Storage` は外へ出さない）。
    pub fn write_lock_wait(&self) -> std::time::Duration {
        self.storage.write_lock_wait()
    }

    /// 述語形 UPDATE/DELETE の 1 文あたり影響行数上限・複数行 `VALUES` の
    /// 1 文あたり行数上限（[`crate::sql::parser::DmlLimits`]）を差し替えた
    /// ビルダーを返す（[`Self::with_batch_limits`] と同じ流儀。未呼び出しなら
    /// `DmlLimits::default()`＝既定値 1,000・現行挙動を維持）。Issue #997・
    /// オーナー判断 2026-09-27: `wire-server` の起動時 CLI フラグ（プロセス全体・
    /// 1 回限り）から本メソッドを経由して設定する契約で、セッション・テナント
    /// 単位で差し替える経路は設けない。
    pub fn with_dml_limits(mut self, limits: crate::sql::parser::DmlLimits) -> Self {
        self.dml_limits = limits;
        self
    }

    /// スキーマ定義系 3 上限（[`crate::schema_limits::SchemaLimits`]）を差し替えた
    /// ビルダーを返す（Issue #1548）。`wire-server` の起動時 CLI フラグ
    /// （`--max-table-columns`・`--max-array-elements`・`--max-enum-labels`）から
    /// のみ呼ぶ。未呼び出しならハード上限と同値の既定（現行挙動）。
    pub fn with_schema_limits(mut self, limits: crate::schema_limits::SchemaLimits) -> Self {
        self.schema_limits = limits;
        self
    }

    /// 現在のスキーマ定義系 3 上限（Issue #1548）。
    pub fn schema_limits(&self) -> crate::schema_limits::SchemaLimits {
        self.schema_limits
    }

    /// 総走査行数上限（[`crate::scan_limits::ScanLimits`]）を差し替えたビルダーを返す
    /// （Issue #1549）。`wire-server` の起動時 CLI フラグ `--max-scanned-rows` からのみ
    /// 呼ぶ。未呼び出しならハード上限と同値の既定（現行挙動）。
    pub fn with_scan_limits(mut self, limits: crate::scan_limits::ScanLimits) -> Self {
        self.scan_limits = limits;
        self
    }

    /// `table` が採番方式（暗黙の `id` を持たない。TABLE-23・Issue #2199）の表かを返す。NoSQL 表層が
    /// 該当する表への `aggregate`／`search` を実行前に `0A000` で拒否する判定と、`update`／`delete` の
    /// 経路選択（疑似列 `id` の単一行形か PRIMARY KEY の述語形か。Issue #2201）に使う。表が無い・スキーマを
    /// 読めない場合は `false`（通常の実行経路が未定義表・内部エラーとして分類する。存在の有無を
    /// この判定の応答に出さない）。
    pub fn table_uses_allocated_row_identity(&self, table: &str) -> bool {
        self.storage
            .get_table_schema(table)
            .map(|schema| schema.is_allocated())
            .unwrap_or(false)
    }

    /// 現在の総走査行数上限（Issue #1549）。
    pub fn scan_limits(&self) -> crate::scan_limits::ScanLimits {
        self.scan_limits
    }

    /// `GROUP BY` の列数・グループ数上限（[`crate::group_by_limits::GroupByLimits`]）を
    /// 差し替えたビルダーを返す（Issue #1550）。`wire-server` の起動時 CLI フラグ
    /// （`--max-group-by-columns`・`--max-groups`）からのみ呼ぶ。未呼び出しなら
    /// ハード上限と同値の既定（現行挙動）。
    pub fn with_group_by_limits(mut self, limits: crate::group_by_limits::GroupByLimits) -> Self {
        self.group_by_limits = limits;
        self
    }

    /// 現在の `GROUP BY` 上限（Issue #1550）。
    pub fn group_by_limits(&self) -> crate::group_by_limits::GroupByLimits {
        self.group_by_limits
    }

    /// 一時表の件数・総バイト数上限（[`crate::temp_table_limits::TempTableLimits`]）を
    /// 差し替えたビルダーを返す（TABLE-44・Issue #1683）。`wire-server` の起動時 CLI フラグ
    /// （`--max-temp-tables`・`--max-temp-bytes`）からのみ呼ぶ。未呼び出しならハード上限と
    /// 同値の既定（現行挙動）。
    pub fn with_temp_table_limits(
        mut self,
        limits: crate::temp_table_limits::TempTableLimits,
    ) -> Self {
        self.temp_table_limits = limits;
        self
    }

    /// 現在の一時表上限（TABLE-44・Issue #1683）。
    pub fn temp_table_limits(&self) -> crate::temp_table_limits::TempTableLimits {
        self.temp_table_limits
    }

    /// jsonpath 評価のステップ数・再帰深さ上限（[`crate::jsonpath_limits::JsonPathLimits`]）を
    /// 差し替えたビルダーを返す（Issue #2184）。`wire-server` の起動時 CLI フラグ
    /// （`--max-jsonpath-steps`・`--max-jsonpath-depth`）からのみ呼ぶ。未呼び出しなら
    /// ハード上限と同値の既定。
    pub fn with_jsonpath_limits(mut self, limits: crate::jsonpath_limits::JsonPathLimits) -> Self {
        self.jsonpath_limits = limits;
        self
    }

    /// 現在の jsonpath 評価上限（Issue #2184）。
    pub fn jsonpath_limits(&self) -> crate::jsonpath_limits::JsonPathLimits {
        self.jsonpath_limits
    }

    /// 正規表現マッチャの上限（[`crate::regex_limits::RegexLimits`]）を差し替えたビルダーを返す
    /// （Issue #1786）。`wire-server` の起動時 CLI フラグ（`--max-regex-pattern-len`・
    /// `--max-regex-input-len`・`--max-regex-compiled-size`）からのみ呼ぶ。未呼び出しなら
    /// ハード上限と同値の既定。
    pub fn with_regex_limits(mut self, limits: crate::regex_limits::RegexLimits) -> Self {
        self.regex_limits = limits;
        self
    }

    /// 現在の正規表現マッチャ上限（Issue #1786）。
    pub fn regex_limits(&self) -> crate::regex_limits::RegexLimits {
        self.regex_limits
    }

    /// サブクエリ深さ・実行回数・CTE 定義数・CTE 連鎖深さの上限
    /// （[`crate::nesting_limits::NestingLimits`]）を差し替えたビルダーを返す（Issue #1552）。
    /// `wire-server` の起動時 CLI フラグ（`--max-subquery-depth` 等）からのみ呼ぶ。
    /// 未呼び出しならハード上限と同値の既定（現行挙動）。
    pub fn with_nesting_limits(mut self, limits: crate::nesting_limits::NestingLimits) -> Self {
        // 窓キー数は別設定（[`Self::with_window_limits`]）が正。呼び出し順に依存しないよう再合成する。
        self.nesting_limits = limits.with_max_window_keys(self.window_limits.max_window_keys());
        self
    }

    /// 窓の `PARTITION BY`／`ORDER BY` キー数上限（[`crate::window_limits::WindowLimits`]）を
    /// 差し替えたビルダーを返す（Issue #1900）。`wire-server` の起動時 CLI フラグ
    /// `--max-window-keys` からのみ呼ぶ。未呼び出しならハード上限と同値の既定（現行挙動）。
    pub fn with_window_limits(mut self, limits: crate::window_limits::WindowLimits) -> Self {
        self.window_limits = limits;
        self.nesting_limits = self
            .nesting_limits
            .with_max_window_keys(limits.max_window_keys());
        self
    }

    /// 現在の窓キー数上限（Issue #1900）。
    pub fn window_limits(&self) -> crate::window_limits::WindowLimits {
        self.window_limits
    }

    /// 現在の入れ子系上限（Issue #1552）。
    pub fn nesting_limits(&self) -> crate::nesting_limits::NestingLimits {
        self.nesting_limits
    }

    /// JOIN のリレーション数・入出力行数上限（[`crate::join_limits::JoinSizeLimits`]）を
    /// 差し替えたビルダーを返す（Issue #1551）。`wire-server` の起動時 CLI フラグ
    /// （`--max-join-relations`・`--max-join-input-rows`・`--max-join-output-rows`）からのみ
    /// 呼ぶ。未呼び出しならハード上限と同値の既定（現行挙動）。
    pub fn with_join_limits(mut self, limits: crate::join_limits::JoinSizeLimits) -> Self {
        self.join_limits = limits;
        self
    }

    /// 現在の JOIN 上限（Issue #1551）。
    pub fn join_limits(&self) -> crate::join_limits::JoinSizeLimits {
        self.join_limits
    }

    /// 相関サブクエリの再評価回数上限（[`crate::correlated_limits::CorrelatedLimits`]）を
    /// 差し替えたビルダーを返す（Issue #1847）。`wire-server` の起動時 CLI フラグ
    /// （`--max-correlated-reevaluations`）からのみ呼ぶ。未呼び出しならハード上限と同値の既定
    /// （現行挙動）。
    pub fn with_correlated_limits(
        mut self,
        limits: crate::correlated_limits::CorrelatedLimits,
    ) -> Self {
        self.correlated_limits = limits;
        self
    }

    /// 現在の相関サブクエリ上限（Issue #1847）。
    pub fn correlated_limits(&self) -> crate::correlated_limits::CorrelatedLimits {
        self.correlated_limits
    }

    /// 再帰 CTE の反復回数・累積行数の上限
    /// （[`crate::recursive_cte_limits::RecursiveCteLimits`]）を差し替える。wire-server の
    /// 起動時 CLI フラグからだけ呼ぶ。呼ばなければハード上限と同値の既定値（Issue #1871）。
    pub fn with_recursive_cte_limits(
        mut self,
        limits: crate::recursive_cte_limits::RecursiveCteLimits,
    ) -> Self {
        self.recursive_cte_limits = limits;
        self
    }

    /// 現在の再帰 CTE 上限設定。
    pub fn recursive_cte_limits(&self) -> crate::recursive_cte_limits::RecursiveCteLimits {
        self.recursive_cte_limits
    }

    /// PL/pgSQL の実行上限（[`crate::plpgsql_limits::PlpgsqlLimits`]）を差し替える。wire-server の
    /// 起動時 CLI フラグからだけ呼ぶ。呼ばなければハード上限と同値の既定値（Issue #2225）。
    pub fn with_plpgsql_limits(mut self, limits: crate::plpgsql_limits::PlpgsqlLimits) -> Self {
        self.plpgsql_limits = limits;
        self
    }

    /// 現在の PL/pgSQL 上限設定。#2218 の関数呼び出し深さもここの `max_call_depth` を読む。
    pub fn plpgsql_limits(&self) -> crate::plpgsql_limits::PlpgsqlLimits {
        self.plpgsql_limits
    }

    /// 束縛済み計画の `LIMIT` を設定値の範囲で再検査する（Issue #1555 の多層防御）。
    /// NoSQL 表層などが既定値（10,000）で束縛した計画を渡した場合でも、設定を下げた構成で
    /// 既定の範囲を通す fail-open を防ぐ。エラー分類は束縛段と同じ `22000`。
    fn check_bound_limit(
        &self,
        limit: usize,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        let max = self.result_limits.max_result_rows();
        if limit > max {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                format!("LIMIT {limit} out of range (must be 1..={max})"),
            ));
        }
        Ok(())
    }

    /// `LIMIT` 省略・`LIMIT ALL` の結果が結果行数上限 `cap` を超えたら `54000` で文全体を拒否する
    /// （Issue #1796・SQL-34）。黙って切り詰めない。文言は固定の英語で、件数・テナント・行の
    /// 内容を含めない。JOIN・GROUP BY・集合演算・評価後射影形ビューの外側が共有する。
    fn enforce_result_cap(
        result: crate::sql::exec::QueryResult,
        cap: usize,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        if result.rows.len() > cap {
            return Err(crate::sql::scan::row_limit_exceeded());
        }
        Ok(result)
    }

    /// `GROUP BY`／`SELECT DISTINCT` 集計の結果へ暗黙の結果行数上限を適用する（Issue #1796）。
    /// `LIMIT` が省略・`ALL`（`limit: None`）の `GROUP BY` 文だけが対象で、HAVING と OFFSET を
    /// 適用した後の件数で判定する。単一行の集計（`GROUP BY` なし）は対象外。
    /// `limits` は呼び出し段の上限（ビュー本文の評価では本文用の既定値、外側では呼び出し段の
    /// 設定値。本文と外側の上限を分ける契約を保つため `self.result_limits` は参照しない）。
    fn enforce_group_result_cap(
        validated: &crate::sql::allowlist::ValidatedAggregate,
        result: crate::sql::exec::QueryResult,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        match &validated.group_by {
            // `WITH TIES` は同値グループの追加で `LIMIT` を超えうるため、明示件数でも設定上限を
            // 適用する（超過は黙って切り詰めず `54000`。Issue #1793）。
            Some(g) if g.limit.is_none() || g.with_ties => {
                Self::enforce_result_cap(result, limits.max_result_rows())
            }
            _ => Ok(result),
        }
    }

    /// 束縛済み広域取得計画の取得上限を設定値で再検査する（Issue #1796・SQL-34 の多層防御）。
    /// 暗黙上限の印（`result_cap`）付きの計画は「`cap` が設定値と一致し、取得上限が
    /// `cap + 1` 以下」のときだけ通す（印を偽装して巨大な取得上限を通す経路を作らない）。
    /// それ以外（明示 `LIMIT`・NoSQL 表層の直接構築）は従来どおり [`Self::check_bound_limit`]。
    fn check_bound_scan_limit(
        &self,
        bound: &crate::sql::parser::BoundScan,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        let Some(cap) = bound.result_cap() else {
            return self.check_bound_limit(bound.limit());
        };
        let max = self.result_limits.max_result_rows();
        if cap != max || bound.limit() > max.saturating_add(1) {
            return Err(crate::sql::allowlist::SqlSurfaceError::invalid_input(
                format!("LIMIT {} out of range (must be 1..={max})", bound.limit()),
            ));
        }
        Ok(())
    }

    /// 評価後射影形ビューの外側 `LIMIT`（行形のみ）を設定値の範囲で再検査する
    /// （Issue #1555）。構文段（`sql::allowlist`）は設定値を持たないため範囲検査をここへ
    /// 一任する。`limits` は呼び出し段の上限（ビュー本文の再帰先では既定値）。
    fn check_buffered_outer_limit(
        validated: &crate::sql::allowlist::ValidatedBufferedView,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        // 利用者が明示した `LIMIT` は値が `u32::MAX` でも必ず検査する。`limit_explicit`
        // が false のものはビュー連鎖の素通し外側（`LIMIT` なし＝無制限）で検査対象外。
        if let crate::sql::allowlist::BufferedOuter::Rows(rows) = &validated.outer {
            if rows.limit_explicit {
                crate::sql::parser::validate_search_limit_with(rows.limit, limits)?;
            }
        }
        Ok(())
    }

    /// 明示 `LIMIT` の範囲上限（[`crate::result_limits::ResultLimits`]）を差し替えた
    /// ビルダーを返す（Issue #1555）。`wire-server` の起動時 CLI フラグ
    /// （`--max-result-rows`）からのみ呼ぶ。未呼び出しなら従来定数と同値の既定
    /// （現行挙動）。
    pub fn with_result_limits(mut self, limits: crate::result_limits::ResultLimits) -> Self {
        self.result_limits = limits;
        self
    }

    /// MATERIALIZED VIEW の初回実体化の対象文脈を差し替えたビルダーを返す（TABLE-42・Issue #1664）。
    /// `wire-server` が起動時に認証構成（UserStore）のテナント集合から導出した文脈を 1 回だけ
    /// 注入する。文脈は [`crate::catalog::materialized::ContextKey`] で重複を除き、決定的な順序に
    /// 並べる。未呼び出し（Rust API の埋め込み・テスト）なら作成セッションの文脈だけを実体化する。
    pub fn with_materialization_contexts(mut self, contexts: Vec<PolicyContext>) -> Self {
        let mut keyed: Vec<(crate::catalog::materialized::ContextKey, PolicyContext)> = contexts
            .into_iter()
            .map(|c| {
                (
                    crate::catalog::materialized::ContextKey::from_context(&c),
                    c,
                )
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        keyed.dedup_by(|a, b| a.0 == b.0);
        self.materialization_contexts = Some(std::sync::Arc::new(
            keyed.into_iter().map(|(_, c)| c).collect(),
        ));
        self
    }

    /// 現在の結果行数系上限（Issue #1555）。NoSQL 表層の `limit` 検証や、後続の
    /// 結果行数上限の適用が参照する。
    pub fn result_limits(&self) -> crate::result_limits::ResultLimits {
        self.result_limits
    }

    /// 分割実行 DML の設定（[`crate::sql::parser::PartitionedDmlLimits`]）を差し替えた
    /// ビルダーを返す。`wire-server` の起動時 CLI フラグ（プロセス全体・1 回限り）から
    /// のみ呼ぶ。未呼び出しなら既定値（ADR §15.2）。
    pub fn with_partitioned_dml_limits(
        mut self,
        limits: crate::sql::parser::PartitionedDmlLimits,
    ) -> Self {
        self.partitioned_dml_limits = limits;
        self
    }

    /// 結合ではなくモジュール内テストが登録簿などの内部状態へ届くための入口
    /// （Issue #1129 の同時実行数上限のテスト用。本番ビルドには含めない）。
    #[cfg(test)]
    pub(crate) fn storage_for_test(&self) -> &Storage {
        &self.storage
    }

    /// 現在の分割実行 DML の設定。
    pub fn partitioned_dml_limits(&self) -> crate::sql::parser::PartitionedDmlLimits {
        self.partitioned_dml_limits
    }

    /// HNSW opt-in 時に HNSW 経路を使うテーブルの範囲
    /// （[`crate::search_engine::HnswScope`]）を差し替えたビルダーを返す
    /// （[`Self::with_dml_limits`] と同じ流儀。未呼び出しなら `HnswScope::All`
    /// ＝全テーブル HNSW・宣言導入前の挙動を維持）。Issue #1065・オーナー判断
    /// 2026-09-28: `wire-server` の起動時 CLI フラグ `--hnsw-scope`（プロセス
    /// 全体・1 回限り）から本メソッドを経由して設定する契約。SQL 表層・Rust API・
    /// `EXPLAIN` の全経路が同じ値で `catalog::hnsw_targeted_in_txn` を呼ぶ。
    /// HNSW opt-in なし（`hnsw_state` が `None`）の構築では効果を持たない
    /// （全テーブル厳密のまま）。
    pub fn with_hnsw_scope(mut self, scope: crate::search_engine::HnswScope) -> Self {
        self.hnsw_scope = scope;
        self
    }

    /// [`Self::with_hnsw_scope`] で設定した HNSW の適用範囲（未設定なら
    /// `HnswScope::All`）。
    pub fn hnsw_scope(&self) -> crate::search_engine::HnswScope {
        self.hnsw_scope
    }

    /// 新規の [`crate::sql::transaction::SessionTransaction`]（`Idle`）を作る。
    /// `wire-server` が接続（セッション）ごとに 1 つ保持し、
    /// [`Self::execute_sql_in_txn`]／[`Self::execute_parsed_in_txn`] へ `&mut` で
    /// 渡す想定（SQL-31・TASK-221）。
    pub fn new_session_transaction(&self) -> crate::sql::transaction::SessionTransaction<'_> {
        crate::sql::transaction::SessionTransaction::new(self.transaction_limits)
    }

    /// `BEGIN` を含まない複数文メッセージを原子的に実行するための暗黙トランザクションを
    /// 開始する（Issue #1175・WIRE-16。[`crate::sql::statement_splitter::
    /// plan_multi_statement`] が `ImplicitTransaction` を返したときに、
    /// `wire-server` がメッセージの先頭で 1 回だけ呼ぶ）。
    ///
    /// `wire-server` は `Storage` を持たないため、書き込みゲートの取得を伴う開始を
    /// この入口へ集約する。`txn` が `Idle` でなければ `XX000`、ゲート待機上限超過は
    /// `55P03`（[`crate::sql::transaction::SessionTransaction::begin_implicit`]）。
    /// 以降の文は通常どおり [`Self::execute_sql_in_txn`] へ渡し、最後の文の成功後に
    /// `SessionTransaction::commit_implicit` で確定する。
    pub fn begin_implicit_transaction<'e>(
        &'e self,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        session: &crate::sql::mode::SessionState,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        txn.begin_implicit(&self.storage, session)
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

            let rows = crate::tenant::visible_rows_any_identity(
                &self.storage,
                table,
                ctx,
                self.scan_limits,
            )
            .map_err(|e| match e {
                crate::tenant::TenantError::Catalog(e) => CoreError::from(e),
                // 走査量上限超過は他テナントの存在情報を含まない固定メッセージへ
                // 丸める（`TenantError` 自体が既にその契約を満たすが、
                // `CoreError` 側の型に昇格し直す。security.md「エラー・ログ
                // 経由で他テナントのデータ・存在情報を漏らさない」）。
                crate::tenant::TenantError::TooManyVisibleRows { .. }
                | crate::tenant::TenantError::TooManyRowsScanned { .. } => CoreError::from(
                    CatalogError::Invalid("too many rows to build dictionary snapshot".to_string()),
                ),
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

    /// 明示トランザクション内（Issue #1353）で、自トランザクションの未 commit の変更を含む
    /// 辞書スナップショットと正規化索引を `write_txn` から直接構築する。
    ///
    /// 呼び出し元は `expand_query`（`USING PLAN` の SELECT・`EXPLAIN` が、書き込み済みテーブルを
    /// 含む明示トランザクションの中で LLM 展開する経路）のみ。結果は
    /// [`DictionaryCache`] に **lookup も insert もしない**（P0）。未 commit の行から作った辞書を
    /// 世代キーのキャッシュへ載せると、ROLLBACK 後にテーブル世代が再利用された際に別セッション・
    /// 別テナントへ ROLLBACK 済みの語が漏れる。スキーマは `write_txn` 自身から読む（行と同一
    /// スナップショット）。可視判定・走査上限・デコード失敗時の fail-closed は
    /// [`Self::dictionary_snapshot_with_index`] と同一。
    fn dictionary_from_write_txn(
        &self,
        ctx: &PolicyContext,
        table: &str,
        write_txn: &redb::WriteTransaction,
    ) -> Result<
        (
            Arc<crate::dictionary::Dictionary>,
            Arc<crate::tiering::NormalizedDictionaryIndex>,
        ),
        CoreError,
    > {
        let schema = crate::catalog::get_table_schema_in_txn(write_txn, table)?;
        let (path_idx, body_idx) = dictionary_required_columns(&schema)
            .map_err(|msg| CoreError::from(CatalogError::Invalid(msg)))?;

        let row_table_name = crate::catalog::user_rows_table_name(table);
        // `WriteTransaction::open_table` は未存在テーブルを作成してしまうため、読み取り経路では
        // `list_tables` で存在確認してから開く（空の user_rows が COMMIT で永続化されるのを防ぐ）。
        // 未存在は「行なし」＝空辞書として扱う。
        let row_table_exists = {
            use redb::TableHandle;
            write_txn
                .list_tables()
                .map_err(CatalogError::from)?
                .any(|handle| handle.name() == row_table_name)
        };
        let row_table = if row_table_exists {
            match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name)) {
                Ok(t) => Some(t),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(CoreError::from(crate::catalog::map_row_table_error(e))),
            }
        } else {
            None
        };

        let mut builder = crate::dictionary::DictionaryBuilder::new(self.dictionary_config.clone());
        if let Some(row_table) = row_table {
            use redb::ReadableTable;
            let invalid = |msg: &str| CoreError::from(CatalogError::Invalid(msg.to_string()));
            const DECODE_FAILED: &str =
                "failed to decode a visible row while building dictionary snapshot";
            let mut scanned: usize = 0;
            let mut visible: usize = 0;
            let iter = row_table
                .range::<(&str, u64)>(..)
                .map_err(CatalogError::from)?;
            for entry in iter {
                let (k, v) = entry.map_err(CatalogError::from)?;
                let (key_tenant, id) = k.value();
                let raw = v.value();
                scanned = scanned.saturating_add(1);
                if scanned > self.scan_limits.max_scanned_rows() {
                    return Err(invalid("too many rows to build dictionary snapshot"));
                }
                let (row_tenant, visibility) =
                    crate::storage::decode_row_tenant_and_visibility(raw)
                        .map_err(|_| invalid(DECODE_FAILED))?;
                if !ctx.is_visible(row_tenant, visibility) {
                    continue;
                }
                visible = visible.saturating_add(1);
                if visible > crate::tenant::MAX_VISIBLE_ROWS {
                    return Err(invalid("too many rows to build dictionary snapshot"));
                }
                let row = crate::storage::decode_row_for_key(
                    key_tenant,
                    crate::storage::rowid::RowId::from_legacy_id(id),
                    raw,
                )
                .map_err(|_| invalid(DECODE_FAILED))?;
                let values = crate::row_codec::decode_scalar_columns(&schema, &row.metadata)
                    .map_err(|_| invalid(DECODE_FAILED))?;
                let (
                    Some(crate::row_codec::Value::Text(path)),
                    Some(crate::row_codec::Value::Text(body)),
                ) = (values.get(path_idx), values.get(body_idx))
                else {
                    return Err(invalid(
                        "path or body column value was unexpectedly not text while building \
                         dictionary snapshot",
                    ));
                };
                builder.ingest(path.as_str(), body.as_str());
            }
        }
        let dictionary = Arc::new(builder.finish());
        let normalized_index = Arc::new(crate::tiering::NormalizedDictionaryIndex::build(
            &dictionary,
        ));
        Ok((dictionary, normalized_index))
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
        let (expansion, _classification) = self.expand_query(ctx, table, question, None)?;
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
        self.expand_query(ctx, table, question, None)
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
        self.plan_query_with_mode_in(ctx, table, question, query_mode, session_mode, None)
    }

    /// [`Self::plan_query_with_mode`] の本体。`dict_txn` が `Some` のとき辞書を共有 write
    /// トランザクションから構築する（明示トランザクション内の `EXPLAIN ... USING PLAN`。
    /// Issue #1353。[`Self::expand_query`] 参照）。公開 API のシグネチャは変えない。
    fn plan_query_with_mode_in(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
        query_mode: Option<crate::sql::mode::SearchMode>,
        session_mode: Option<crate::sql::mode::SearchMode>,
        dict_txn: Option<&redb::WriteTransaction>,
    ) -> Result<crate::query_planner::PlannedQuery, CoreError> {
        let (expansion, _classification) = self.expand_query(ctx, table, question, dict_txn)?;
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
    ///
    /// `dict_txn`（Issue #1353）が `Some` のときは、辞書を [`DictionaryCache`] を経由せず
    /// 共有 write トランザクションから構築する（[`Self::dictionary_from_write_txn`]。
    /// 明示トランザクション内で自トランザクションの未 commit の行を辞書へ反映し、
    /// キャッシュには一切載せない）。`None` は従来どおりキャッシュ経由の確定済み辞書。
    fn expand_query(
        &self,
        ctx: &PolicyContext,
        table: &str,
        question: &str,
        dict_txn: Option<&redb::WriteTransaction>,
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
        let (dictionary, normalized_index) = match dict_txn {
            Some(write_txn) => self.dictionary_from_write_txn(ctx, table, write_txn)?,
            None => self.dictionary_snapshot_with_index(ctx, table)?,
        };

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
    fn execute_sql_unscrubbed(
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
        let resolver = self.schema_resolver(ctx);
        let resolved_tokens =
            crate::sql::relation_name::tokenize_resolved_with(sql, false, Some(&resolver))?;
        match crate::sql::allowlist::validate_session_sql_tokens(
            &resolved_tokens,
            &self.storage,
            0,
            self.nesting_limits,
        )? {
            crate::sql::allowlist::Statement::SetSearchMode { .. } => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "SET search_mode requires a session-aware entry point",
                ))
            }
            // TABLE-33・Issue #1575: セッションを持たない入口では `SET TIME ZONE`／`SHOW` も
            // 値の妥当性に関わらず一律 `42601`（`SET search_mode` と同じ決定的な契約）。
            crate::sql::allowlist::Statement::SetTimeZone { .. }
            | crate::sql::allowlist::Statement::ShowTimeZone => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "SET/SHOW TimeZone requires a session-aware entry point",
                ))
            }
            // TABLE-27・Issue #1715: `search_path` もセッション変数のため、セッションを持たない入口では
            // 値の妥当性に関わらず一律 `42601`（TimeZone と同じ決定的な契約）。
            crate::sql::allowlist::Statement::SetSearchPath { .. }
            | crate::sql::allowlist::Statement::ShowSearchPath => {
                Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                    "SET/SHOW search_path requires a session-aware entry point",
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
                    | crate::sql::SqlOutcome::SetTimeZone
                    | crate::sql::SqlOutcome::SetSearchPath { .. }
                    | crate::sql::SqlOutcome::Show(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_)
                    | crate::sql::SqlOutcome::Truncate(_)
                    | crate::sql::SqlOutcome::Delete(_)
                    | crate::sql::SqlOutcome::Returning(_)
                    | crate::sql::SqlOutcome::Update(_)
                    | crate::sql::SqlOutcome::CreateTable(_)
                    | crate::sql::SqlOutcome::CreateTableAs(_)
                    | crate::sql::SqlOutcome::DropTable(_)
                    | crate::sql::SqlOutcome::AlterTable(_)
                    | crate::sql::SqlOutcome::CreateView(_)
                    | crate::sql::SqlOutcome::CreateMaterializedView(_)
                    | crate::sql::SqlOutcome::RefreshMaterializedView(_)
                    | crate::sql::SqlOutcome::DropView(_)
                    | crate::sql::SqlOutcome::DropMaterializedView(_)
                    | crate::sql::SqlOutcome::CreateIndex(_)
                    | crate::sql::SqlOutcome::DropIndex(_)
                    | crate::sql::SqlOutcome::CreateType(_)
                    | crate::sql::SqlOutcome::DropType(_)
                    | crate::sql::SqlOutcome::CreateSequence(_)
                    | crate::sql::SqlOutcome::DropSequence(_)
                    | crate::sql::SqlOutcome::DropFunction(_)
                    | crate::sql::SqlOutcome::CreateTrigger(_)
                    | crate::sql::SqlOutcome::DropTrigger(_)
                    | crate::sql::SqlOutcome::CreateSchema(_)
                    | crate::sql::SqlOutcome::DropSchema(_)
                    | crate::sql::SqlOutcome::AlterSchema(_)
                    | crate::sql::SqlOutcome::AlterSequence(_)
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
                    | crate::sql::SqlOutcome::SetTimeZone
                    | crate::sql::SqlOutcome::SetSearchPath { .. }
                    | crate::sql::SqlOutcome::Show(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_)
                    | crate::sql::SqlOutcome::Truncate(_)
                    | crate::sql::SqlOutcome::Delete(_)
                    | crate::sql::SqlOutcome::Returning(_)
                    | crate::sql::SqlOutcome::Update(_)
                    | crate::sql::SqlOutcome::CreateTable(_)
                    | crate::sql::SqlOutcome::CreateTableAs(_)
                    | crate::sql::SqlOutcome::DropTable(_)
                    | crate::sql::SqlOutcome::AlterTable(_)
                    | crate::sql::SqlOutcome::CreateView(_)
                    | crate::sql::SqlOutcome::CreateMaterializedView(_)
                    | crate::sql::SqlOutcome::RefreshMaterializedView(_)
                    | crate::sql::SqlOutcome::DropView(_)
                    | crate::sql::SqlOutcome::DropMaterializedView(_)
                    | crate::sql::SqlOutcome::CreateIndex(_)
                    | crate::sql::SqlOutcome::DropIndex(_)
                    | crate::sql::SqlOutcome::CreateType(_)
                    | crate::sql::SqlOutcome::DropType(_)
                    | crate::sql::SqlOutcome::CreateSequence(_)
                    | crate::sql::SqlOutcome::DropSequence(_)
                    | crate::sql::SqlOutcome::DropFunction(_)
                    | crate::sql::SqlOutcome::CreateTrigger(_)
                    | crate::sql::SqlOutcome::DropTrigger(_)
                    | crate::sql::SqlOutcome::CreateSchema(_)
                    | crate::sql::SqlOutcome::DropSchema(_)
                    | crate::sql::SqlOutcome::AlterSchema(_)
                    | crate::sql::SqlOutcome::AlterSequence(_)
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
                    | crate::sql::SqlOutcome::SetTimeZone
                    | crate::sql::SqlOutcome::SetSearchPath { .. }
                    | crate::sql::SqlOutcome::Show(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_)
                    | crate::sql::SqlOutcome::Truncate(_)
                    | crate::sql::SqlOutcome::Delete(_)
                    | crate::sql::SqlOutcome::Returning(_)
                    | crate::sql::SqlOutcome::Update(_)
                    | crate::sql::SqlOutcome::CreateTable(_)
                    | crate::sql::SqlOutcome::CreateTableAs(_)
                    | crate::sql::SqlOutcome::DropTable(_)
                    | crate::sql::SqlOutcome::AlterTable(_)
                    | crate::sql::SqlOutcome::CreateView(_)
                    | crate::sql::SqlOutcome::CreateMaterializedView(_)
                    | crate::sql::SqlOutcome::RefreshMaterializedView(_)
                    | crate::sql::SqlOutcome::DropView(_)
                    | crate::sql::SqlOutcome::DropMaterializedView(_)
                    | crate::sql::SqlOutcome::CreateIndex(_)
                    | crate::sql::SqlOutcome::DropIndex(_)
                    | crate::sql::SqlOutcome::CreateType(_)
                    | crate::sql::SqlOutcome::DropType(_)
                    | crate::sql::SqlOutcome::CreateSequence(_)
                    | crate::sql::SqlOutcome::DropSequence(_)
                    | crate::sql::SqlOutcome::DropFunction(_)
                    | crate::sql::SqlOutcome::CreateTrigger(_)
                    | crate::sql::SqlOutcome::DropTrigger(_)
                    | crate::sql::SqlOutcome::CreateSchema(_)
                    | crate::sql::SqlOutcome::DropSchema(_)
                    | crate::sql::SqlOutcome::AlterSchema(_)
                    | crate::sql::SqlOutcome::AlterSequence(_)
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
            // Issue #929（SQL-29 (c)・TASK-213）: 集合演算の枝は `Scan` と同じく
            // セッション UDF レジストリを参照しうるため、`Scan` と同じくセッション
            // を要する実行本体（`execute_validated_in_session`）へ委譲する。
            stmt @ crate::sql::allowlist::Statement::SetOperation(_) => {
                let mut session = crate::sql::mode::SessionState::default();
                match self.execute_validated_in_session(ctx, &mut session, stmt)? {
                    crate::sql::SqlOutcome::Query(result) => Ok(result),
                    crate::sql::SqlOutcome::SetSearchMode(_)
                    | crate::sql::SqlOutcome::SetTimeZone
                    | crate::sql::SqlOutcome::SetSearchPath { .. }
                    | crate::sql::SqlOutcome::Show(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_)
                    | crate::sql::SqlOutcome::Truncate(_)
                    | crate::sql::SqlOutcome::Delete(_)
                    | crate::sql::SqlOutcome::Returning(_)
                    | crate::sql::SqlOutcome::Update(_)
                    | crate::sql::SqlOutcome::CreateTable(_)
                    | crate::sql::SqlOutcome::CreateTableAs(_)
                    | crate::sql::SqlOutcome::DropTable(_)
                    | crate::sql::SqlOutcome::AlterTable(_)
                    | crate::sql::SqlOutcome::CreateView(_)
                    | crate::sql::SqlOutcome::CreateMaterializedView(_)
                    | crate::sql::SqlOutcome::RefreshMaterializedView(_)
                    | crate::sql::SqlOutcome::DropView(_)
                    | crate::sql::SqlOutcome::DropMaterializedView(_)
                    | crate::sql::SqlOutcome::CreateIndex(_)
                    | crate::sql::SqlOutcome::DropIndex(_)
                    | crate::sql::SqlOutcome::CreateType(_)
                    | crate::sql::SqlOutcome::DropType(_)
                    | crate::sql::SqlOutcome::CreateSequence(_)
                    | crate::sql::SqlOutcome::DropSequence(_)
                    | crate::sql::SqlOutcome::DropFunction(_)
                    | crate::sql::SqlOutcome::CreateTrigger(_)
                    | crate::sql::SqlOutcome::DropTrigger(_)
                    | crate::sql::SqlOutcome::CreateSchema(_)
                    | crate::sql::SqlOutcome::DropSchema(_)
                    | crate::sql::SqlOutcome::AlterSchema(_)
                    | crate::sql::SqlOutcome::AlterSequence(_)
                    | crate::sql::SqlOutcome::Begin
                    | crate::sql::SqlOutcome::Commit
                    | crate::sql::SqlOutcome::Rollback
                    | crate::sql::SqlOutcome::DeclareCursor
                    | crate::sql::SqlOutcome::Fetch(_)
                    | crate::sql::SqlOutcome::CloseCursor => {
                        Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "unexpected non-Query outcome for a statement already classified as SetOperation"
                                .to_string(),
                        })
                    }
                }
            }
            // Issue #925（SQL-28・RLS-10、TASK-212）: `INNER JOIN` は `SetOperation`
            // と同じくセッション UDF レジストリを参照しうる束縛経路
            // （`sql::parser::bind_scan`）を通るため、セッションを要する実行本体
            // （`execute_validated_in_session`）へ委譲する。
            // Issue #1192（TABLE-18）: 評価後射影形ビューへの参照も本文（`Scan`／
            // `Aggregate`／`Join`）がセッション UDF レジストリを参照しうる束縛
            // 経路を通るため、同じくセッションを要する実行本体へ委譲する。
            // SQL-43（Issue #1531）: `FROM` なし定数式 SELECT もセッション UDF
            // レジストリを参照しうるため、同じくセッション付き実行本体へ委譲する。
            // TABLE-42・Issue #1664: matview の葉は `BufferedView` の本文としてのみ現れ、
            // 単独の文としては生成されない（防御的に fail-closed）。
            crate::sql::allowlist::Statement::MaterializedView(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "unexpected materialized view statement".to_string(),
                })
            }
            // SQL-35・Issue #1870: 再帰 CTE は `BufferedView` の本文としてのみ現れ、単独の文としては
            // 生成されない（防御的に fail-closed）。
            crate::sql::allowlist::Statement::RecursiveCte(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "unexpected recursive CTE statement".to_string(),
                })
            }
            stmt @ (crate::sql::allowlist::Statement::Join(_)
            | crate::sql::allowlist::Statement::BufferedView(_)
            | crate::sql::allowlist::Statement::ConstSelect(_)) => {
                let mut session = crate::sql::mode::SessionState::default();
                match self.execute_validated_in_session(ctx, &mut session, stmt)? {
                    crate::sql::SqlOutcome::Query(result) => Ok(result),
                    crate::sql::SqlOutcome::SetSearchMode(_)
                    | crate::sql::SqlOutcome::SetTimeZone
                    | crate::sql::SqlOutcome::SetSearchPath { .. }
                    | crate::sql::SqlOutcome::Show(_)
                    | crate::sql::SqlOutcome::CreateFunction { .. }
                    | crate::sql::SqlOutcome::Explain(_)
                    | crate::sql::SqlOutcome::Insert(_)
                    | crate::sql::SqlOutcome::Truncate(_)
                    | crate::sql::SqlOutcome::Delete(_)
                    | crate::sql::SqlOutcome::Returning(_)
                    | crate::sql::SqlOutcome::Update(_)
                    | crate::sql::SqlOutcome::CreateTable(_)
                    | crate::sql::SqlOutcome::CreateTableAs(_)
                    | crate::sql::SqlOutcome::DropTable(_)
                    | crate::sql::SqlOutcome::AlterTable(_)
                    | crate::sql::SqlOutcome::CreateView(_)
                    | crate::sql::SqlOutcome::CreateMaterializedView(_)
                    | crate::sql::SqlOutcome::RefreshMaterializedView(_)
                    | crate::sql::SqlOutcome::DropView(_)
                    | crate::sql::SqlOutcome::DropMaterializedView(_)
                    | crate::sql::SqlOutcome::CreateIndex(_)
                    | crate::sql::SqlOutcome::DropIndex(_)
                    | crate::sql::SqlOutcome::CreateType(_)
                    | crate::sql::SqlOutcome::DropType(_)
                    | crate::sql::SqlOutcome::CreateSequence(_)
                    | crate::sql::SqlOutcome::DropSequence(_)
                    | crate::sql::SqlOutcome::DropFunction(_)
                    | crate::sql::SqlOutcome::CreateTrigger(_)
                    | crate::sql::SqlOutcome::DropTrigger(_)
                    | crate::sql::SqlOutcome::CreateSchema(_)
                    | crate::sql::SqlOutcome::DropSchema(_)
                    | crate::sql::SqlOutcome::AlterSchema(_)
                    | crate::sql::SqlOutcome::AlterSequence(_)
                    | crate::sql::SqlOutcome::Begin
                    | crate::sql::SqlOutcome::Commit
                    | crate::sql::SqlOutcome::Rollback
                    | crate::sql::SqlOutcome::DeclareCursor
                    | crate::sql::SqlOutcome::Fetch(_)
                    | crate::sql::SqlOutcome::CloseCursor => {
                        Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "unexpected non-Query outcome for a statement already classified as Join"
                                .to_string(),
                        })
                    }
                }
            }
        }
    }

    /// `execute_sql_unscrubbed` の公開入口。判定順序・エラー契約・実行結果は同関数と同一で、
    /// 出口でだけ SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を利用者向けの
    /// `<schema>.<name>` へ戻す（TABLE-27・Issue #1717、ADR D2。内部 ID を文言へ出さない）。
    pub fn execute_sql(
        &self,
        ctx: &PolicyContext,
        sql: &str,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        self.execute_sql_unscrubbed(ctx, sql)
            .map_err(|e| self.scrub_error(ctx, e))
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
        let read_txn = self.begin_read_txn()?;
        let schema = table_schema_in(&read_txn, table_name)?;
        Ok((read_txn, schema))
    }

    /// 確定済みスナップショットの read トランザクションを開始する
    /// （[`Self::read_txn_with_schema`]・[`Self::read_txn_with_schemas`]・
    /// [`Self::execute_validated_in_session`] の読み取りアームが共有する入口）。
    fn begin_read_txn(
        &self,
    ) -> Result<redb::ReadTransaction, crate::sql::allowlist::SqlSurfaceError> {
        self.storage.db().begin_read().map_err(|e| {
            crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!(
                    "failed to begin read transaction: {}",
                    StorageError::from(e)
                ),
            }
        })
    }

    /// [`Self::read_txn_with_schema`] の複数テーブル版（SQL-29 (c)・RLS-10 (b)・
    /// TASK-213）。集合演算文（`Statement::SetOperation`）が参照する全テーブルの
    /// スキーマを、単一の `read_txn`（同一スナップショット）上でまとめて解決する
    /// ——`storage.db().begin_read()` を枝ごとに開き直さない（`sql::set_op`
    /// モジュールドキュメント参照）。テーブル不存在は [`Self::read_txn_with_schema`]
    /// と同じ写像（`UndefinedTable`／`table_lookup_error`）を適用する。
    fn read_txn_with_schemas(
        &self,
        table_names: &[String],
    ) -> Result<
        (
            redb::ReadTransaction,
            std::collections::HashMap<String, crate::catalog::TableSchema>,
        ),
        crate::sql::allowlist::SqlSurfaceError,
    > {
        let read_txn = self.begin_read_txn()?;
        let schemas = table_schemas_in(&read_txn, table_names)?;
        Ok((read_txn, schemas))
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
        let tokens = crate::sql::lexer::tokenize_marked(sql)?;
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
        self.reclassify_write_to_view_error_for(None, e)
    }

    /// 接続に束縛された解決テナント（[`crate::sql::mode::SessionState::bind_resolution_tenant`]）と
    /// 実行時の `ctx` のテナントが食い違う呼び出しを拒否する（TABLE-27・Issue #1717、ADR D6）。
    /// wire-server は handshake で `ctx` と同じテナントを束縛するため通常は一致する。組み込み側が
    /// 別テナントの `ctx` で束縛済みセッションを使い回し、Parse 時に解決した SCHEMA 修飾を別テナントの
    /// 文脈で実行する経路を作らない（fail-closed）。未束縛のセッションは検査しない。
    fn check_resolution_tenant(
        session: &crate::sql::mode::SessionState,
        ctx: &PolicyContext,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        let mismatch = || crate::sql::allowlist::SqlSurfaceError::Internal {
            detail: "session tenant binding mismatch".to_string(),
        };
        if session
            .resolution_tenant()
            .is_some_and(|bound| bound != ctx.tenant_id())
        {
            return Err(mismatch());
        }
        // カタログ由来の UDF をロード済みのセッションは、ロードしたテナントの ctx でしか実行できない
        // （別テナントの ctx での使い回しで UDF 本体が漏れる経路を作らない。Issue #1921・P0）。
        // 解決テナント未束縛のまま UDF を保持するセッションも fail-closed で拒否する。
        if let Some(scope) = session.udfs().catalog_scope() {
            if scope != ctx.tenant_id() || session.resolution_tenant().is_none() {
                return Err(mismatch());
            }
        }
        Ok(())
    }

    /// エラー文言に現れる SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を、呼び出し元テナントの
    /// SCHEMA 名で `<schema>.<name>` へ戻す（TABLE-27・Issue #1717、ADR D2）。`#` を含む文言が
    /// 無ければ追加コストを払わない。逆引きに失敗した場合は `#<id>.` を落とす（`#` を出さない）。
    fn scrub_error(
        &self,
        ctx: &PolicyContext,
        e: crate::sql::allowlist::SqlSurfaceError,
    ) -> crate::sql::allowlist::SqlSurfaceError {
        self.scrub_error_for_tenant(ctx.tenant_id(), e)
    }

    /// [`Self::scrub_error`] のテナント指定版（認証コンテキストを持たない拡張クエリの Parse が使う）。
    fn scrub_error_for_tenant(
        &self,
        tenant_id: &str,
        e: crate::sql::allowlist::SqlSurfaceError,
    ) -> crate::sql::allowlist::SqlSurfaceError {
        let found = std::cell::Cell::new(false);
        let _ = e.clone().map_text(&|t| {
            if t.contains('#') {
                found.set(true);
            }
            t.to_string()
        });
        if !found.get() {
            return e;
        }
        let names = self
            .storage
            .schema_names_for_tenant(tenant_id)
            .unwrap_or_default();
        e.map_text(&|t| crate::sql::relation_name::display_relation_keys(t, &names))
    }

    /// 成功応答のうち、利用者向けの文字列（EXPLAIN の計画テキスト）に内部キーが現れうるものを表示用へ戻す。
    fn scrub_outcome(
        &self,
        ctx: &PolicyContext,
        outcome: crate::sql::SqlOutcome,
    ) -> crate::sql::SqlOutcome {
        use crate::sql::exec::Cell;
        let crate::sql::SqlOutcome::Explain(mut result) = outcome else {
            return outcome;
        };
        let has_key = result.rows.iter().any(|r| {
            r.cells
                .iter()
                .any(|c| matches!(c, Cell::Text(t) if t.contains('#')))
        });
        if has_key {
            let names = self.schema_display_names(ctx);
            for row in &mut result.rows {
                for cell in &mut row.cells {
                    if let Cell::Text(t) = cell {
                        *t = crate::sql::relation_name::display_relation_keys(t, &names);
                    }
                }
            }
        }
        crate::sql::SqlOutcome::Explain(result)
    }

    /// 呼び出し元テナントの SCHEMA `内部 ID → 名前`（表示変換用。失敗時は空 = `#<id>.` を落とす）。
    fn schema_display_names(&self, ctx: &PolicyContext) -> std::collections::BTreeMap<u64, String> {
        self.storage
            .schema_names_for_tenant(ctx.tenant_id())
            .unwrap_or_default()
    }

    /// 呼び出し元テナントの SCHEMA 解決器（TABLE-27・Issue #1714・#1717、ADR D2・D4）。`schema.name` の
    /// 修飾を `(ctx.tenant_id, schema)` の get 1 回で内部 ID へ解決する。他テナントの SCHEMA と
    /// 存在しない名前は同じ `None`（`3F000`。存在オラクルにしない）。テナントは ctx からだけ取り、
    /// SQL 文からは変えられない。
    fn schema_resolver<'a>(
        &'a self,
        ctx: &'a PolicyContext,
    ) -> impl Fn(&str) -> Result<Option<u64>, crate::sql::allowlist::SqlSurfaceError> + 'a {
        move |schema: &str| {
            self.storage
                .schema_id(ctx.tenant_id(), schema)
                .map_err(crate::catalog::table_lookup_error)
        }
    }

    /// 永続オブジェクトを作る DDL の作成先を `search_path` から判定する（TABLE-27・Issue #1715）。
    /// DDL 権限ゲートの直後・既存名の判定より前に呼ぶ（`42501` → `3F000`／`0A000` → `42P07` の順）。
    /// `explicit_public` は `public.` で明示修飾された作成（判定を飛ばして `public` へ作る）。
    /// SCHEMA の照会は `(呼び出し元テナント, 名前)` の get だけで、他テナントの SCHEMA は存在しない名前と
    /// 同じに扱われる。
    fn require_creation_target(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        explicit_public: bool,
        created_name: &str,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        // `schema.name` で明示修飾された作成（解決済みの内部キー `#<id>.<name>`。キーを作れるのは
        // 名前解決器だけ）は作成先が文中で確定しているので、`search_path` による作成先判定を飛ばす。
        if explicit_public || created_name.starts_with('#') {
            return Ok(());
        }
        let probe = |schema: &str| {
            self.storage
                .schema_id(ctx.tenant_id(), schema)
                .map(|id| id.is_some())
                .map_err(crate::catalog::table_lookup_error)
        };
        crate::sql::search_path::creation_target(session, &probe).map(|_| ())
    }

    /// [`Self::reclassify_write_to_view_error`] の文種別つき版（TABLE-43 (3)・Issue #1670）。
    /// `op` が `Some`（`INSERT`／`UPDATE`／`DELETE`）で対象がビューのとき、自動更新可能性を
    /// `sql::view_updatable` で判定し、自動更新不可なら `55000`
    /// （`ViewNotAutoUpdatable`）で拒否する。自動更新可能なビューへの INSERT／UPDATE／DELETE は
    /// `parse_tokens` が先に `ParsedSql::ViewInsert`／`ViewUpdate`／`ViewDelete` として受理する
    /// （Issue #1671・#1672）ため通常は本関数には来ない（並行 DDL でビューが現れた競合時だけ
    /// 従来どおり `42809`〔fail-closed〕）。`op` が `None`（`TRUNCATE`・`EXPLAIN` 系）は常に従来の `42809`。判定自体の失敗（破損 `XX000`・深さ超過
    /// `54000` 等）はそのまま返し、「ビューではない」側へは倒さない。
    fn reclassify_write_to_view_error_for(
        &self,
        op: Option<crate::sql::allowlist::ViewWriteOp>,
        e: crate::sql::allowlist::SqlSurfaceError,
    ) -> crate::sql::allowlist::SqlSurfaceError {
        if let crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name } = &e {
            // 索引名（TASK-206・INDEX-7、Issue #908）もビューと同じくテーブルと
            // relation 名前空間を共有する行を持たないオブジェクトのため、書き込み
            // 系 DML の対象にされた場合は `42809` へ揃える。
            let wrong_kind = match self.storage.view_definition(name) {
                Ok(Some(_)) => {
                    if let Some(op) = op {
                        match crate::sql::view_updatable::view_updatability(&self.storage, name) {
                            Ok(crate::sql::view_updatable::ViewUpdatability::NotAutoUpdatable(
                                reason,
                            )) => {
                                return crate::sql::allowlist::SqlSurfaceError::ViewNotAutoUpdatable {
                                    op,
                                    reason,
                                };
                            }
                            Ok(crate::sql::view_updatable::ViewUpdatability::AutoUpdatable) => {}
                            Err(judge_err) => return judge_err,
                        }
                    }
                    true
                }
                Ok(None) => match self.storage.index_exists(name) {
                    Ok(true) => true,
                    // matview（TABLE-42・Issue #1664）も行を直接書けないオブジェクト。表・ビュー・
                    // 索引と名前空間を共有するため、書き込み対象にされたら `42809` へ揃える。
                    // 照会の失敗は「存在しない」と同一視せず、同じ写像で `XX000` にする。
                    Ok(false) => match self.storage.materialized_view_definition(name) {
                        Ok(def) => def.is_some(),
                        Err(lookup_err) => return crate::catalog::table_lookup_error(lookup_err),
                    },
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

    /// `COPY FROM` の対象が matview のとき `42809` へ読み替える（TABLE-42・Issue #1664。行を直接書けない
    /// オブジェクトへの書き込みは、`UndefinedTable` ではなく種別違いで報告する）。ビュー・索引の読み替えは
    /// 従来どおりこの経路では行わない（既存挙動を変えない）。照会の失敗は「存在しない」と同一視せず
    /// `table_lookup_error` で写す（fail-closed）。
    fn reclassify_matview_write_error(
        &self,
        e: crate::sql::allowlist::SqlSurfaceError,
    ) -> crate::sql::allowlist::SqlSurfaceError {
        if let crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name } = &e {
            match self.storage.materialized_view_definition(name) {
                Ok(Some(_)) => {
                    return crate::sql::allowlist::SqlSurfaceError::WrongObjectType {
                        name: name.clone(),
                    }
                }
                Ok(None) => {}
                Err(lookup_err) => return crate::catalog::table_lookup_error(lookup_err),
            }
        }
        e
    }

    /// `CHUNK n` がサーバー設定のチャンク幅以下であることを検査する（Issue #1129。
    /// 解析時と実行時の 2 回呼ぶ多層防御。`PARTITIONED` なしは何もしない）。
    fn check_partitioned_chunk(
        &self,
        clause: Option<&crate::sql::partitioned::PartitionedClause>,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        if let Some(clause) = clause {
            crate::sql::partitioned::apply_chunk_override(clause, &self.partitioned_dml_limits)?;
        }
        Ok(())
    }

    /// `EXPLAIN UPDATE|DELETE ... PARTITIONED [CHUNK n]` の構造検証（Issue #1129）。先頭の
    /// `EXPLAIN` を除いた残りを既存の述語形検証へ渡し、`PARTITIONED` 付きの述語形だけを
    /// 受理する（それ以外は `42601`）。実行は束縛までで、書き込み・登録簿・ジョブ表には触れない。
    fn parse_explain_partitioned_dml(
        &self,
        tokens: &[crate::sql::lexer::Token],
        lookup: &impl crate::sql::allowlist::TableLookup,
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::{DeleteStatement, SqlSurfaceError, ValidatedUpdateForm};
        use crate::sql::partitioned::PartitionedControl;

        let reject = || {
            SqlSurfaceError::unsupported(
                "EXPLAIN requires a SELECT or partitioned UPDATE/DELETE statement",
            )
        };
        let rest = tokens.get(1..).ok_or_else(reject)?;
        let is_delete = matches!(
            rest.first(),
            Some(crate::sql::lexer::Token::Ident(w)) if w.eq_ignore_ascii_case("DELETE")
        );
        if is_delete {
            // SQL-36（Issue #2210）: 構造解析 → WHERE 省略ガード → カタログ検証の順。通常 DML 経路と
            // 同じく、ガードを表の存在確認より前に置き、不存在表と既存表で応答を分けない。
            let shape = crate::sql::allowlist::parse_delete_statement_shape_tokens_with_limits(
                rest,
                self.ledger_mode,
                self.nesting_limits,
            )?;
            if let DeleteStatement::Predicate(v) = &shape {
                crate::sql::allowlist::reject_omitted_where_for_dml(
                    self.require_where_for_dml,
                    v.where_predicates(),
                )?;
            }
            let stmt = crate::sql::allowlist::validate_delete_statement_tokens_with_limits(
                rest,
                lookup,
                self.ledger_mode,
                self.nesting_limits,
            )
            .map_err(|e| self.reclassify_write_to_view_error(e))?;
            return match stmt {
                DeleteStatement::Predicate(v) if v.partitioned().is_some() => {
                    self.check_partitioned_chunk(v.partitioned())?;
                    Ok(ParsedSql::Partitioned(PartitionedControl::ExplainDelete(v)))
                }
                _ => Err(reject()),
            };
        }
        // SQL-36（Issue #2210）: DELETE と同じ順序（構造解析 → WHERE 省略ガード → カタログ検証）。
        let shape = crate::sql::allowlist::parse_update_form_shape_tokens_with_limits(
            rest,
            self.ledger_mode,
            self.nesting_limits,
        )?;
        if let ValidatedUpdateForm::Predicate(v) = &shape {
            crate::sql::allowlist::reject_omitted_where_for_dml(
                self.require_where_for_dml,
                v.where_predicates(),
            )?;
        }
        let stmt = crate::sql::allowlist::validate_update_form_tokens_with_limits(
            rest,
            lookup,
            self.ledger_mode,
            self.nesting_limits,
        )
        .map_err(|e| self.reclassify_write_to_view_error(e))?;
        match stmt {
            ValidatedUpdateForm::Predicate(v) if v.partitioned().is_some() => {
                self.check_partitioned_chunk(v.partitioned())?;
                Ok(ParsedSql::Partitioned(PartitionedControl::ExplainUpdate(v)))
            }
            _ => Err(reject()),
        }
    }

    /// セッション一時カタログ対応の構文検証（TABLE-44・Issue #1681。一時表への振り分けは Issue #1684）。
    ///
    /// 一時カタログが空なら [`Self::parse_tokens`] と同一。空でなければ、トランザクション制御文と
    /// `CREATE [TEMP] TABLE`／`DROP TABLE`（関係を読み書きせず、実行段が自セッションの一時カタログで
    /// 名前を解決する）を除き、トークン列に一時表の名前と ASCII 大小無視で一致する識別子が無ければ
    /// 通常の parse、あれば [`Self::route_temp_statement`]（一時コアへの振り分け。混在・対象外の
    /// 文種は `0A000`）に回す。トークン走査は保守的（一致しすぎても拒否側＝fail-closed）で、
    /// カタログを引かない文（`ALTER`・ビュー・カーソル等）や永続ビュー本文の再検証も覆う。
    fn parse_tokens_gated(
        &self,
        tokens: Vec<crate::sql::lexer::Token>,
        session: &crate::sql::mode::SessionState,
        resolver: Option<crate::sql::relation_name::SchemaResolver<'_>>,
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        // 標準 SQL の行数制限句の正規化は、参照ゲート・一時カタログ走査より前に行う（Issue #1793）。
        // 正規化が `0A000`（入れ子・非 SELECT の `WITH TIES`）で失敗した場合も、`search_path` が
        // `public` を隠すセッションでは参照ゲートの `42P01` を優先する（`42601` → `42P01` → その他）。
        let original = session.search_path_hides_public().then(|| tokens.clone());
        let tokens = match Self::normalize_row_count_clauses(tokens) {
            Ok(t) => t,
            Err(e) => {
                if let Some(orig) = original {
                    if e.wire_code() != "42601" {
                        crate::sql::search_path::check_unqualified_refs(session, &orig, &[])?;
                    }
                }
                return Err(e);
            }
        };
        // 実効 `search_path` に `public` が含まれる既定のセッションは何も足さない（追加コストなし）。
        if !session.search_path_hides_public() {
            return self.parse_tokens_gated_inner(tokens, session, resolver);
        }
        // TABLE-27・Issue #1715: `public` が見えないセッションでは、一時表以外の未修飾のテーブル参照を
        // `42P01` にする。判定順は構文エラー `42601` → 参照ゲート `42P01` → その他の検証エラー。
        let gate = crate::sql::search_path::check_unqualified_refs(session, &tokens, &[]);
        let parsed = self.parse_tokens_gated_inner(tokens, session, resolver);
        match (parsed, gate) {
            (Err(e), _) if e.wire_code() == "42601" => Err(e),
            (_, Err(gate_err)) => Err(gate_err),
            (parsed, Ok(())) => parsed,
        }
    }

    /// [`Self::parse_tokens_gated`] の一時カタログ対応の本体（`search_path` の参照ゲートを持たない）。
    fn parse_tokens_gated_inner(
        &self,
        tokens: Vec<crate::sql::lexer::Token>,
        session: &crate::sql::mode::SessionState,
        resolver: Option<crate::sql::relation_name::SchemaResolver<'_>>,
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        let temp = session.temp_tables();
        if temp.is_empty() {
            return self.parse_tokens_r(tokens, resolver);
        }
        // `public.<一時表の名前>` は永続側の関係を指すため、裸の名前へ解決すると一時表に化ける。
        // 解決より前に拒否する（TABLE-27・Issue #1714。fail-closed）。
        crate::sql::temp_catalog::reject_public_qualified_temp(&tokens, temp)?;
        let txn_control_head = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name))
                if name.eq_ignore_ascii_case("BEGIN")
                    || name.eq_ignore_ascii_case("COMMIT")
                    || name.eq_ignore_ascii_case("ROLLBACK")
        );
        // TABLE-44・Issue #1683: 次の 3 種は一時表の行を読み書きしないためトークン走査を飛ばす。
        // `CREATE [TEMP] TABLE` は列定義だけで（FOREIGN KEY の参照先が一時表の名前なら実行段が
        // `42P16`／`0A000`）、`DROP TABLE` は実行段が自セッションの一時カタログで名前を解決する。
        let is_drop_table = matches!(tokens.first(), Some(crate::sql::lexer::Token::Ident(a)) if a.eq_ignore_ascii_case("DROP"))
            && matches!(tokens.get(1), Some(crate::sql::lexer::Token::Ident(b)) if b.eq_ignore_ascii_case("TABLE"));
        let scan_exempt = is_drop_table
            // CTAS は内側の問い合わせが一時表を読みうるため走査を免除しない（一時表は `0A000`）。
            || (crate::sql::allowlist::is_create_table_statement(&tokens)
                && !crate::sql::allowlist::is_create_table_as_statement(&tokens))
            || crate::sql::allowlist::is_create_temp_table_statement(&tokens)
            // SCHEMA 名は関係名と別の名前空間で一時表を読み書きしないため走査対象外（TABLE-27・Issue #1713）。
            || crate::sql::allowlist::is_create_schema_statement(&tokens)
            || crate::sql::allowlist::is_drop_schema_statement(&tokens)
            || crate::sql::allowlist::is_alter_schema_statement(&tokens);
        if txn_control_head || scan_exempt {
            let hits = crate::sql::temp_catalog::RelationHits::default();
            let parsed = self.parse_tokens_in(tokens, temp, &hits, self.ledger_mode, resolver)?;
            if hits.temp_hit() {
                return Err(crate::sql::temp_catalog::temp_table_unsupported());
            }
            return Ok(parsed);
        }
        // `DROP INDEX <名前>` は索引名しか持たず、一時表の名前がトークンに現れない。一時表の索引かどうかは
        // 一時 DB のカタログで決まる（一時表の索引は永続側の DDL 権限ゲートの対象外）。
        if crate::sql::allowlist::is_drop_index_statement(&tokens) {
            return self.route_drop_index(tokens, session, resolver);
        }
        if !crate::sql::temp_catalog::tokens_reference_temp(&tokens, temp) {
            return self.parse_tokens_r(tokens, resolver);
        }
        self.route_temp_statement(tokens, session, resolver)
    }

    /// 粗いゲート（TABLE-44・Issue #1681。#1684 で振り分け済みの文の世代検査を足した）。既に parse
    /// 済みの [`ParsedSql`] を受ける入口（`execute_parsed_*`・`describe_*`）では、セッションを持たない
    /// parse で作られた文（名前が一時表かを判定できていない）を、一時カタログが空でないセッションで
    /// （トランザクション制御・`CREATE [TEMP] TABLE`・`DROP TABLE` 以外）一律 `0A000` で拒否する。
    /// セッション対応の parse／Bind が振り分けた文（[`ParsedSql::SessionRouted`]）は、一時カタログの
    /// 世代が振り分け時から変わっていなければ通す。一時カタログが空なら何もしない。細かい振り分けを
    /// 済ませた SQL テキスト入口は本ゲートを持たない内部版（`*_inner`）を呼ぶ。
    ///
    /// | 入口 | ゲート |
    /// | ---- | ------ |
    /// | `execute_sql_in_session`・`execute_sql_in_txn` | 細かい（`parse_tokens_gated` の振り分け） |
    /// | `begin_copy`・`begin_copy_in_txn` | 細かい（`begin_copy_temp` の振り分け） |
    /// | `execute_parsed_in_session`・`execute_parsed_in_txn`・`describe_*` | 粗い（世代検査つき） |
    fn coarse_temp_gate(
        session: &crate::sql::mode::SessionState,
        parsed: &ParsedSql,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        // 振り分け済みの文（一時表向け・永続側）は、一時カタログの世代・参照する一時表が振り分け時から
        // 変わっていなければ通す。変わっていれば陳腐化として `0A000`（TABLE-44・Issue #1684）。
        if let ParsedSql::SessionRouted(routed) = parsed {
            return routed.require_current(session);
        }
        // TABLE-27・Issue #1715: `public` が見えない `search_path` のセッションでは、セッション対応の
        // parse／Bind を通っていない（名前の解決結果が現在の path のものと分からない）文を
        // 一律 `0A000` で拒否する（関係名を引かない免除文を除く。fail-closed）。既定の path では何もしない。
        if session.search_path_hides_public() && !temp_route::is_search_path_exempt(parsed) {
            return Err(temp_route::search_path_stale());
        }
        // TABLE-44・Issue #1683: トランザクション制御に加え、一時表の行を読み書きしない
        // `CREATE [TEMP] TABLE`・`DROP TABLE` を通す（実行段がセッションの一時カタログで
        // 名前を解決する）。これらを通さないと、一時表を作った直後のセッションが拡張クエリで
        // 一時表の `DROP` も永続表の DDL も実行できなくなる。
        // `public.` 修飾つきの `DROP TABLE` は、同名の一時表があれば一時表へ化けるため拒否する
        // （`parse_sql` 経由で修飾情報を持つ文が、後から一時表を持つセッションで実行される経路。
        // TABLE-27・Issue #1714。`execute_parsed_in_session`・`execute_parsed_in_txn` 共通）。
        if let ParsedSql::DropTable(stmt) = parsed {
            crate::sql::temp_catalog::reject_public_names_temp(
                &stmt.public_qualified,
                session.temp_tables(),
            )?;
        }
        if session.temp_tables().is_empty()
            || matches!(
                parsed,
                ParsedSql::Transaction(_)
                    | ParsedSql::CreateTable(_)
                    | ParsedSql::CreateTempTable(_)
                    | ParsedSql::DropTable(_)
                    | ParsedSql::CreateSchema(_)
                    | ParsedSql::DropSchema(_)
                    | ParsedSql::AlterSchema(_)
            )
        {
            return Ok(());
        }
        Err(crate::sql::temp_catalog::temp_table_unsupported())
    }

    /// [`Self::execute_parsed_in_session_inner`] の公開入口。一時カタログが空でないセッションでは
    /// 粗いゲート（[`Self::coarse_temp_gate`]）を通してから実行する。空なら従来と同一。
    fn execute_parsed_in_session_unscrubbed(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        Self::coarse_temp_gate(session, parsed)?;
        self.execute_parsed_in_session_inner(ctx, session, parsed)
    }

    /// `execute_parsed_in_session_unscrubbed` の公開入口。判定順序・エラー契約・実行結果は同関数と同一で、
    /// 出口でだけ SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を利用者向けの
    /// `<schema>.<name>` へ戻す（TABLE-27・Issue #1717、ADR D2。内部 ID を文言へ出さない）。
    pub fn execute_parsed_in_session(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        Self::check_resolution_tenant(session, ctx)?;
        self.execute_parsed_in_session_unscrubbed(ctx, session, parsed)
            .map_err(|e| self.scrub_error(ctx, e))
            .map(|o| self.scrub_outcome(ctx, o))
    }

    /// [`Self::execute_parsed_in_txn_inner`] の公開入口。`Failed` 中は従来どおり `25P02` を優先し、
    /// それ以外で粗いゲートに当たった場合は明示トランザクションを `Failed` へ遷移させる
    /// （parse 失敗と同じ扱い）。
    fn execute_parsed_in_txn_unscrubbed<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        if txn.status() != crate::sql::transaction::TransactionStatus::Failed {
            if let Err(e) = Self::coarse_temp_gate(session, parsed) {
                txn.fail();
                return Err(e);
            }
        }
        self.execute_parsed_in_txn_inner(ctx, session, txn, parsed)
    }

    /// `execute_parsed_in_txn_unscrubbed` の公開入口。判定順序・エラー契約・実行結果は同関数と同一で、
    /// 出口でだけ SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を利用者向けの
    /// `<schema>.<name>` へ戻す（TABLE-27・Issue #1717、ADR D2。内部 ID を文言へ出さない）。
    pub fn execute_parsed_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        Self::check_resolution_tenant(session, ctx)?;
        self.execute_parsed_in_txn_unscrubbed(ctx, session, txn, parsed)
            .map_err(|e| self.scrub_error(ctx, e))
            .map(|o| self.scrub_outcome(ctx, o))
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
        let tokens = Self::normalize_row_count_clauses(tokens)?;
        self.parse_tokens_r(tokens, None)
    }

    /// 標準 SQL の `FETCH FIRST n ROWS ONLY`／`OFFSET n ROWS`／`WITH TIES` をトークン段で
    /// 正規形（`LIMIT n`／`OFFSET n`、`WITH TIES` は専用の正規形）へ書き換える（SQL-56 (3)・
    /// TASK-298 のポインタ。Issue #1793。実体は `sql::fetch_clause`）。構文検証・名前解決・
    /// 一時カタログ走査のいずれより前に、各パース入口が呼ぶ。冪等で、`FETCH`／`OFFSET` を
    /// 含まない文は何も変えない。`WITH TIES` を解釈できるのは「`SELECT` で始まる最上位の問い合わせ」
    /// だけなので、それ以外の文種・括弧内の使用は黙って `ONLY` 扱いにせず `0A000` で拒否する。
    fn normalize_row_count_clauses(
        tokens: Vec<crate::sql::lexer::Token>,
    ) -> Result<Vec<crate::sql::lexer::Token>, crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::fetch_clause::normalize_statement_tokens(tokens)
    }

    /// [`Self::parse_tokens`] のテナント SCHEMA 解決つき版（`resolver` は ctx を持つ入口だけが渡す。
    /// TABLE-27・Issue #1717）。`None` ならテナント SCHEMA の修飾は `3F000`。
    fn parse_tokens_r(
        &self,
        tokens: Vec<crate::sql::lexer::Token>,
        resolver: Option<crate::sql::relation_name::SchemaResolver<'_>>,
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        // セッションを持たない入口（`parse_sql`・拡張クエリの Parse 等）は空の一時カタログで
        // 解決する（挙動は一時カタログ導入前と同一。TABLE-44・Issue #1681）。
        let temp = crate::sql::temp_catalog::SessionTempCatalog::default();
        let hits = crate::sql::temp_catalog::RelationHits::default();
        self.parse_tokens_in(tokens, &temp, &hits, self.ledger_mode, resolver)
    }

    /// [`Self::parse_tokens`] のセッション一時カタログ対応版（TABLE-44・Issue #1681）。
    /// カタログを引く検証器へ、一時カタログ優先の
    /// [`crate::sql::temp_catalog::SessionRelationLookup`] を渡す。一時表に当たると
    /// `hits` の一時側が、永続の関係（表・ビュー）に当たると永続側が立つ（呼び出し元の振り分け
    /// [`Self::route_temp_statement`] が使う）。`temp` が空なら永続カタログ（`Storage`）だけを引き、
    /// `parse_tokens` と完全に同一に振る舞う。`ledger_mode` は書き込み文の `operation_id` 必須化の
    /// 判定に使う台帳モードで、通常は `self.ledger_mode`。一時表への書き込みの分類 parse だけが
    /// 緩めたモードを渡す。
    fn parse_tokens_in(
        &self,
        tokens: Vec<crate::sql::lexer::Token>,
        temp: &crate::sql::temp_catalog::SessionTempCatalog,
        hits: &crate::sql::temp_catalog::RelationHits,
        ledger_mode: LedgerMode,
        resolver: Option<crate::sql::relation_name::SchemaResolver<'_>>,
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        // 各入口で正規化済みでも冪等のため、ここでも最終的な保証として適用する（Issue #1793）。
        let mut tokens = Self::normalize_row_count_clauses(tokens)?;
        // `schema.table` のテーブル参照を解決して未修飾の名前（public）または内部キー
        // （テナントの SCHEMA 配下。`#<id>.<name>`）へ置き換える（TABLE-27・Issue #1714・#1717）。
        // 以降の検証は 1 個の名前として扱うため、修飾の有無で経路・エラー契約が変わらない。
        // 冪等（解決済みのトークン列には何もしない）。`resolver` が `None`（ctx を持たない入口）の
        // テナント SCHEMA の修飾は `3F000`。
        let rewrite =
            crate::sql::relation_name::rewrite_relation_names_full(&mut tokens, resolver)?;
        let public_names = rewrite.names;
        let create_public = rewrite.create_public;
        let storage_lookup = crate::sql::temp_catalog::SessionRelationLookup::new(
            &self.storage,
            temp,
            hits.temp_cell(),
        )
        .with_persistent_hit(hits.persistent_cell());
        // 引用付き `NULL`（`Token::QuotedNull`）を値位置で区別する必要があるのは
        // INSERT／UPDATE／CREATE／ALTER（`WITH` 先頭の DML 含む）の許可リスト検証だけ
        // （Issue #1524）。それ以外の文種は従来どおり `Ident` として扱う（識別子位置の
        // `"null"` を受理し続けるため）。
        // `EXPLAIN UPDATE ...` も実行時の UPDATE と同じ検証（`expect_literal`）へ渡すため、
        // 対象が UPDATE のときは先頭が `EXPLAIN` でも保持する（EXPLAIN と実行でエラー契約を
        // 食い違わせない）。
        let literal_sensitive_head = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name))
                if ["INSERT", "UPDATE", "CREATE", "ALTER", "WITH"]
                    .iter()
                    .any(|kw| name.eq_ignore_ascii_case(kw))
        ) || matches!(
            (tokens.first(), tokens.get(1)),
            (
                Some(crate::sql::lexer::Token::Ident(head)),
                Some(crate::sql::lexer::Token::Ident(second)),
            ) if head.eq_ignore_ascii_case("EXPLAIN") && second.eq_ignore_ascii_case("UPDATE")
        );
        // FROM なし SELECT の `WHERE "NULL"` は NULL リテラルではなく列参照として扱う
        // 必要がある（Issue #2329）ため、引用情報をこの文種に限り保持する。
        let from_less_select_head = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Keyword(
                crate::sql::lexer::Keyword::Select
            ))
        ) && crate::sql::allowlist::is_from_less_select(&tokens);
        let tokens = if literal_sensitive_head || from_less_select_head {
            tokens
        } else {
            crate::sql::lexer::demote_quoted_null(tokens)
        };
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

        // TABLE-24・Issue #1691: `SELECT setval(...)`／`SELECT lastval()` は専用の検証器で受理するか
        // 拒否する。`ConstSelect` の一般経路へ流すと書き込みを伴う関数が式として評価されうるため、
        // 先頭がこの形なら必ずここで結論を出す（fail-closed）。
        if crate::sql::allowlist::is_sequence_function_select(&tokens) {
            let stmt = crate::sql::allowlist::validate_sequence_function_tokens(&tokens)?;
            return Ok(ParsedSql::SequenceFunction(stmt));
        }

        // TABLE-33・Issue #1575: `SHOW timezone`／`SHOW TIME ZONE` は分割実行 DML の `SHOW` より
        // 先に厳密一致で取り出す（それ以外の `SHOW` の挙動は変えない）。
        if crate::sql::allowlist::is_show_time_zone(&tokens) {
            return Ok(ParsedSql::Statement(
                crate::sql::allowlist::Statement::ShowTimeZone,
            ));
        }
        // TABLE-27・Issue #1715: `SHOW search_path` も同じ理由で厳密一致のみ先に取り出す。
        if crate::sql::allowlist::is_show_search_path(&tokens) {
            return Ok(ParsedSql::Statement(
                crate::sql::allowlist::Statement::ShowSearchPath,
            ));
        }

        // Issue #1129: `SHOW`／`CANCEL PARTITIONED DML '<id>' ON <table>`。カタログは引かない
        // （テーブルの存在・可視性を応答に表さない。RLS-9）。
        let is_partitioned_control = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name))
                if name.eq_ignore_ascii_case("SHOW") || name.eq_ignore_ascii_case("CANCEL")
        );
        if is_partitioned_control {
            let control = crate::sql::allowlist::validate_partitioned_control_tokens(&tokens)?;
            return Ok(ParsedSql::Partitioned(control));
        }

        // Issue #1129: `EXPLAIN UPDATE|DELETE ... PARTITIONED`。分割実行でない DML の
        // `EXPLAIN` は従来どおり許可リスト外（`42601`）。
        if let (
            Some(crate::sql::lexer::Token::Ident(head)),
            Some(crate::sql::lexer::Token::Ident(second)),
        ) = (tokens.first(), tokens.get(1))
        {
            if head.eq_ignore_ascii_case("EXPLAIN")
                && (second.eq_ignore_ascii_case("DELETE") || second.eq_ignore_ascii_case("UPDATE"))
                && crate::sql::partitioned::tokens_have_partitioned_clause(&tokens)
            {
                return self.parse_explain_partitioned_dml(&tokens, &storage_lookup);
            }
        }

        let is_insert_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("INSERT")
        );
        if is_insert_statement {
            let insert_inner = InsertSchemaLookup::new(&self.storage);
            let lookup = crate::sql::temp_catalog::SessionRelationLookup::new(
                &insert_inner,
                temp,
                hits.temp_cell(),
            )
            .with_persistent_hit(hits.persistent_cell());
            // 構造検証 → 存在確認の順序は `validate_insert_tokens_with_limit` と同じ。INTO の名前が
            // 表でなくビューなら、自動更新可能なものに限りビューへの INSERT として受理する
            // （TABLE-43・Issue #1671。書き換えは Execute 時）。
            let parsed_insert = crate::sql::allowlist::parse_insert_any_shape_tokens_with_limit(
                &tokens,
                ledger_mode,
                self.dml_limits.max_insert_rows_per_statement,
            )?;
            let stmt = match parsed_insert {
                crate::sql::allowlist::ParsedInsertTokens::Values(stmt) => stmt,
                // `INSERT ... SELECT`（SQL-51・Issue #1732）。内側 SELECT の検証は SELECT 経路と同じ
                // `storage_lookup`（一時カタログ・ビューの解決込み）で行う。
                crate::sql::allowlist::ParsedInsertTokens::Select(shape) => {
                    return self.parse_insert_select(shape, &tokens, &lookup, &storage_lookup);
                }
            };
            if crate::sql::allowlist::TableLookup::table_exists(&lookup, &stmt.table_name)? {
                return Ok(ParsedSql::Insert(stmt));
            }
            if matches!(self.storage.view_definition(&stmt.table_name), Ok(Some(_))) {
                let view_stmt = crate::sql::allowlist::ValidatedViewInsert::new(stmt);
                // 自動更新不可は `55000`、判定・解決の失敗はそのまま返す（fail-closed）。
                self.storage
                    .with_view_write_snapshot(view_stmt.view_name(), |snap, _| {
                        crate::sql::view_insert::check_view_insert(snap, &view_stmt)
                    })
                    .map_err(crate::catalog::table_lookup_error)??;
                return Ok(ParsedSql::ViewInsert(view_stmt));
            }
            // 表でもビューでもない名前は従来どおり（索引・matview は `42809`、その他は `42P01`）。
            // 照会の失敗は再照会する `reclassify_write_to_view_error_for` が `XX000` へ写す。
            return Err(self.reclassify_write_to_view_error_for(
                Some(crate::sql::allowlist::ViewWriteOp::Insert),
                crate::sql::allowlist::SqlSurfaceError::undefined_table(stmt.table_name),
            ));
        }

        let is_truncate_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("TRUNCATE")
        );
        if is_truncate_statement {
            let stmt = crate::sql::allowlist::validate_truncate_tokens(
                &tokens,
                &storage_lookup,
                ledger_mode,
            )
            .map_err(|e| self.reclassify_write_to_view_error(e))?;
            return Ok(ParsedSql::Truncate(stmt));
        }

        // SQL-53 (b)・Issue #1754: `DELETE ... USING <表の並び>`／`UPDATE ... FROM <表の並び>` は既存の
        // 単一表 DML の解析器より前に専用の解析器へ回す（既存の解析器は FROM／USING 形を `42601` で
        // 拒否したままにし、受理範囲を広げない）。
        if crate::sql::allowlist::looks_like_delete_using(&tokens) {
            let stmt = crate::sql::allowlist::validate_delete_using_tokens_with_limits(
                &tokens,
                ledger_mode,
                self.require_where_for_dml,
                self.nesting_limits,
                &storage_lookup,
            )?;
            return Ok(ParsedSql::DeleteUsing(Box::new(stmt)));
        }
        if crate::sql::allowlist::looks_like_update_from(&tokens) {
            let stmt = crate::sql::allowlist::validate_update_from_tokens_with_limits(
                &tokens,
                ledger_mode,
                self.require_where_for_dml,
                self.nesting_limits,
                &storage_lookup,
            )?;
            return Ok(ParsedSql::UpdateFrom(Box::new(stmt)));
        }

        let is_delete_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("DELETE")
        );
        if is_delete_statement {
            // 構造検証 → 存在確認の順序は `validate_delete_statement_tokens` と同じ。FROM の名前が表で
            // なくビューなら、自動更新可能なものに限りビューへの DELETE として受理する（TABLE-43・
            // Issue #1672。書き換えは Execute 時）。
            let stmt = crate::sql::allowlist::parse_delete_statement_shape_tokens_with_limits(
                &tokens,
                ledger_mode,
                self.nesting_limits,
            )?;
            // SQL-36（Issue #2210）: opt-in 有効時は `WHERE` 省略を表の存在確認より前に拒否する
            // （ビュー・PARTITIONED・一時表の分類 parse も本経路を通る）。
            if let crate::sql::allowlist::DeleteStatement::Predicate(v) = &stmt {
                crate::sql::allowlist::reject_omitted_where_for_dml(
                    self.require_where_for_dml,
                    v.where_predicates(),
                )?;
            }
            let table_name = match &stmt {
                crate::sql::allowlist::DeleteStatement::SingleRow(v) => v.table_name.clone(),
                crate::sql::allowlist::DeleteStatement::Predicate(v) => v.table_name().to_string(),
            };
            if crate::sql::allowlist::TableLookup::table_exists(&storage_lookup, &table_name)? {
                // 採番方式の表（TABLE-23・Issue #2199）では `WHERE id = <n>` は物理キーの直接指定ではなく
                // 通常の述語（`id` は宣言した列）なので、述語形 DML へ戻す。
                let stmt = match stmt {
                    crate::sql::allowlist::DeleteStatement::SingleRow(v)
                        if allocated_table(&storage_lookup, &table_name) =>
                    {
                        crate::sql::allowlist::DeleteStatement::Predicate(v.into_predicate_form()?)
                    }
                    other => other,
                };
                if let crate::sql::allowlist::DeleteStatement::Predicate(v) = &stmt {
                    crate::sql::allowlist::reject_partitioned_single_row_form(
                        &storage_lookup,
                        &table_name,
                        v.partitioned(),
                        v.where_predicates(),
                    )?;
                    self.check_partitioned_chunk(v.partitioned())?;
                }
                return Ok(ParsedSql::Delete(stmt));
            }
            if matches!(self.storage.view_definition(&table_name), Ok(Some(_))) {
                let view_stmt = crate::sql::allowlist::ValidatedViewDelete::new(stmt);
                // 自動更新不可は `55000`、判定・解決の失敗はそのまま返す（fail-closed）。
                self.storage
                    .with_view_write_snapshot(view_stmt.view_name(), |snap, _| {
                        crate::sql::view_dml::check_view_delete(snap, &view_stmt)
                    })
                    .map_err(crate::catalog::table_lookup_error)??;
                return Ok(ParsedSql::ViewDelete(view_stmt));
            }
            return Err(self.reclassify_write_to_view_error_for(
                Some(crate::sql::allowlist::ViewWriteOp::Delete),
                crate::sql::allowlist::SqlSurfaceError::undefined_table(table_name),
            ));
        }

        let is_update_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("UPDATE")
        );
        if is_update_statement {
            // 構造検証 → 存在確認の順序は `validate_update_form_tokens` と同じ（TABLE-43・Issue #1672）。
            let stmt = crate::sql::allowlist::parse_update_form_shape_tokens_with_limits(
                &tokens,
                ledger_mode,
                self.nesting_limits,
            )?;
            // SQL-36（Issue #2210）: DELETE と同じ位置・同じ理由で `WHERE` 省略を拒否する。
            if let crate::sql::allowlist::ValidatedUpdateForm::Predicate(v) = &stmt {
                crate::sql::allowlist::reject_omitted_where_for_dml(
                    self.require_where_for_dml,
                    v.where_predicates(),
                )?;
            }
            let table_name = match &stmt {
                crate::sql::allowlist::ValidatedUpdateForm::Single(v) => v.table_name.clone(),
                crate::sql::allowlist::ValidatedUpdateForm::Predicate(v) => {
                    v.table_name().to_string()
                }
            };
            if crate::sql::allowlist::TableLookup::table_exists(&storage_lookup, &table_name)? {
                // 採番方式の表では単一行形 `WHERE id = <n>` を述語形へ戻す（DELETE と同じ。Issue #2199）。
                let stmt = match stmt {
                    crate::sql::allowlist::ValidatedUpdateForm::Single(v)
                        if allocated_table(&storage_lookup, &table_name) =>
                    {
                        crate::sql::allowlist::ValidatedUpdateForm::Predicate(
                            v.into_predicate_form()?,
                        )
                    }
                    other => other,
                };
                if let crate::sql::allowlist::ValidatedUpdateForm::Predicate(v) = &stmt {
                    crate::sql::allowlist::reject_partitioned_single_row_form(
                        &storage_lookup,
                        &table_name,
                        v.partitioned(),
                        v.where_predicates(),
                    )?;
                    self.check_partitioned_chunk(v.partitioned())?;
                }
                return Ok(ParsedSql::Update(stmt));
            }
            if matches!(self.storage.view_definition(&table_name), Ok(Some(_))) {
                let view_stmt = crate::sql::allowlist::ValidatedViewUpdate::new(stmt);
                self.storage
                    .with_view_write_snapshot(view_stmt.view_name(), |snap, _| {
                        crate::sql::view_dml::check_view_update(snap, &view_stmt)
                    })
                    .map_err(crate::catalog::table_lookup_error)??;
                return Ok(ParsedSql::ViewUpdate(view_stmt));
            }
            return Err(self.reclassify_write_to_view_error_for(
                Some(crate::sql::allowlist::ViewWriteOp::Update),
                crate::sql::allowlist::SqlSurfaceError::undefined_table(table_name),
            ));
        }

        let is_declare_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("DECLARE")
        );
        if is_declare_statement {
            // カタログ照会（テーブル存在確認）は意図的にここでは行わない
            // （`sql::cursor::validate_declare_tokens_structure_only` 参照。
            // PR #1049 レビュー指摘 P1 対応）。
            let stmt = crate::sql::cursor::validate_declare_tokens_structure_only(
                &tokens,
                self.nesting_limits,
            )?;
            return Ok(ParsedSql::Cursor(stmt));
        }

        let is_fetch_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("FETCH")
        );
        if is_fetch_statement {
            let stmt = crate::sql::cursor::validate_fetch_tokens(&tokens, &self.result_limits)?;
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
        // Issue #1194: `DROP TYPE` も汎用 `DROP`（`DROP TABLE` 扱い）より前に分岐する。
        // Issue #1688: `DROP SEQUENCE` も汎用 `DROP` より前に分岐する（カタログ照会なし）。
        // Issue #1713: `DROP SCHEMA` も汎用 `DROP` より前に分岐する（カタログ照会なし）。
        if crate::sql::allowlist::is_drop_schema_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_drop_schema_tokens(&tokens)?;
            return Ok(ParsedSql::DropSchema(stmt));
        }
        // Issue #1716: `ALTER SCHEMA` も汎用 `ALTER`（`ALTER TABLE` 扱い）より前に分岐する。
        if crate::sql::allowlist::is_alter_schema_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_alter_schema_tokens(&tokens)?;
            return Ok(ParsedSql::AlterSchema(stmt));
        }
        // Issue #1920: `DROP FUNCTION` も汎用 `DROP` より前に分岐する（カタログ照会なし）。
        if crate::sql::allowlist::is_drop_function_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_drop_function_tokens(&tokens)?;
            return Ok(ParsedSql::DropFunction(stmt));
        }
        // Issue #1722: `DROP TRIGGER` も汎用 `DROP` より前に分岐する（カタログ照会なし）。
        if crate::sql::allowlist::is_drop_trigger_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_drop_trigger_tokens(&tokens)?;
            return Ok(ParsedSql::DropTrigger(stmt));
        }
        if crate::sql::allowlist::is_drop_sequence_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_drop_sequence_tokens(&tokens)?;
            return Ok(ParsedSql::DropSequence(stmt));
        }
        if crate::sql::allowlist::is_drop_type_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_drop_type_tokens(&tokens)?;
            return Ok(ParsedSql::DropType(stmt));
        }
        if crate::sql::allowlist::is_drop_index_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_drop_index_tokens(&tokens)?;
            return Ok(ParsedSql::DropIndex(stmt));
        }

        // TABLE-42 (5)・Issue #1667: `DROP MATERIALIZED VIEW` も汎用 `DROP`（`DROP TABLE` 扱い）より
        // 前に分岐する。構造検証のみでカタログを照会しない（`DropView` と同じ設計）。
        if crate::sql::allowlist::is_drop_materialized_view_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_drop_materialized_view_tokens(&tokens)?;
            return Ok(ParsedSql::DropMaterializedView(stmt));
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
            let mut stmt = crate::sql::allowlist::validate_drop_table_tokens(&tokens)?;
            // `public.` 修飾の情報を保持し、セッション付き実行の粗いゲートが一時表との衝突を拒否する。
            stmt.public_qualified = public_names;
            return Ok(ParsedSql::DropTable(stmt));
        }

        let ident_at = |i: usize, word: &str| {
            matches!(
                tokens.get(i),
                Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case(word)
            )
        };
        // TABLE-42・Issue #1664: `CREATE MATERIALIZED VIEW` は `CREATE VIEW` の判定より前に分岐する。
        // 構造検証のみでカタログを照会しない（`CreateView` と同じ設計）。`CREATE OR REPLACE
        // MATERIALIZED` は下の `CREATE VIEW` 経路が `OR REPLACE` の後の `VIEW` を要求して `42601`。
        if crate::sql::allowlist::is_create_materialized_view_statement(&tokens) {
            let mut stmt =
                crate::sql::allowlist::validate_create_materialized_view_tokens(&tokens)?;
            stmt.public_qualified = create_public;
            return Ok(ParsedSql::CreateMaterializedView(stmt));
        }
        // TABLE-42・Issue #1665: `REFRESH MATERIALIZED VIEW`。構造検証のみでカタログを照会しない。
        if crate::sql::allowlist::is_refresh_materialized_view_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_refresh_materialized_view_tokens(&tokens)?;
            return Ok(ParsedSql::RefreshMaterializedView(stmt));
        }
        // `CREATE VIEW` に加え `CREATE OR REPLACE VIEW`（TABLE-41 (2)・Issue #1660）も同じ経路へ
        // 入れる。`CREATE OR REPLACE` の後が `VIEW` 以外（`TABLE`・`INDEX` 等）は従来どおり汎用経路で
        // `42601` になる。
        let is_create_view_statement = ident_at(0, "CREATE")
            && (ident_at(1, "VIEW")
                || (ident_at(1, "OR") && ident_at(2, "REPLACE") && ident_at(3, "VIEW")));
        if is_create_view_statement {
            // TABLE-18・SQL-23・TASK-205（Issue #909）: `validate_create_view_tokens`
            // もカタログ照会を一切行わない（`DropTable`・`DropView` と同じ設計。
            // `is_create_function_statement`〔`sql::allowlist::validate_sql_tokens`
            // 内〕より前にここで分岐するため、`CREATE VIEW` のトークン列が
            // `parse_create_function` へ渡ることはない）。
            let mut stmt = crate::sql::allowlist::validate_create_view_tokens(&tokens)?;
            stmt.public_qualified = create_public;
            return Ok(ParsedSql::CreateView(stmt));
        }

        // TASK-206・INDEX-7・SQL-23（Issue #908）: `CREATE INDEX` も
        // `CreateView` と同じくカタログ照会を一切行わない構造検証のみで束縛し、
        // `sql::allowlist::validate_sql_tokens` 内の `CREATE FUNCTION` 分岐へ
        // 渡さない。
        // Issue #1194: `CREATE TYPE` も `CREATE FUNCTION` 分岐へ渡さない。
        // Issue #1688: `CREATE [TEMP|UNLOGGED] SEQUENCE` も `CREATE TABLE`／`FUNCTION` 分岐へ渡さない。
        // Issue #1713: `CREATE SCHEMA` も `CREATE TABLE`／`FUNCTION` 分岐へ渡さない（カタログ照会なし）。
        // Issue #1722: `CREATE [OR REPLACE] TRIGGER` も `CREATE TABLE`／`FUNCTION` 分岐へ渡さない（カタログ照会なし）。
        if crate::sql::allowlist::is_create_trigger_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_create_trigger_tokens(&tokens)?;
            return Ok(ParsedSql::CreateTrigger(stmt));
        }
        if crate::sql::allowlist::is_create_schema_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_create_schema_tokens(&tokens)?;
            return Ok(ParsedSql::CreateSchema(stmt));
        }
        if crate::sql::allowlist::is_create_sequence_statement(&tokens) {
            let mut stmt = crate::sql::allowlist::validate_create_sequence_tokens(&tokens)?;
            stmt.public_qualified = create_public;
            return Ok(ParsedSql::CreateSequence(stmt));
        }
        if crate::sql::allowlist::is_create_type_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_create_type_tokens(&tokens)?;
            return Ok(ParsedSql::CreateType(stmt));
        }
        if crate::sql::allowlist::is_create_index_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_create_index_tokens(&tokens)?;
            return Ok(ParsedSql::CreateIndex(stmt));
        }

        // `DropTable` と同じ設計（Issue #899・#902 で DDL 実行権限ゲートの判定
        // 順序を統一）: `validate_create_table_tokens` はカタログ照会（テーブル名
        // の重複判定）を一切行わない。権限判定・実行本体は
        // `execute_parsed_in_session` の `CreateTable` 分岐が担う。
        // SQL-52・Issue #1678: `CREATE TABLE ... AS`（CTAS）は通常の `CREATE TABLE` より前に分岐する。
        // 構造検証のみでカタログを照会しない（権限の無い主体へ元表の存在を観測させない）。
        if crate::sql::allowlist::is_create_table_as_statement(&tokens) {
            let mut stmt = crate::sql::allowlist::validate_create_table_as_tokens(&tokens)?;
            stmt.public_qualified = create_public;
            return Ok(ParsedSql::CreateTableAs(stmt));
        }
        if crate::sql::allowlist::is_create_table_statement(&tokens) {
            let mut stmt = crate::sql::allowlist::validate_create_table_tokens(&tokens)?;
            stmt.public_qualified = create_public;
            return Ok(ParsedSql::CreateTable(stmt));
        }
        // TABLE-44・Issue #1683: `CREATE [GLOBAL|LOCAL] TEMP|TEMPORARY TABLE` は永続表の
        // `CreateTable` とは別 variant へ束縛する（カタログ照会なしの構造検証のみ）。
        if crate::sql::allowlist::is_create_temp_table_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_create_temp_table_tokens(&tokens)?;
            return Ok(ParsedSql::CreateTempTable(stmt));
        }

        // TASK-202（SQL-23。Issue #900）: `ALTER TABLE ... ADD COLUMN ...` は
        // DDL であり `USING OPERATION_ID` を取らないため `LedgerMode`・
        // `TableLookup` のいずれも渡さない（`ValidatedAlterTableAddColumn` の
        // ドキュメント参照。カタログ照会は権限ゲート通過後の実行段が担う）。
        // Issue #1689: `ALTER SEQUENCE` は「先頭が ALTER なら ALTER TABLE」の汎用分岐より前に束縛する。
        if crate::sql::allowlist::is_alter_sequence_statement(&tokens) {
            let stmt = crate::sql::allowlist::validate_alter_sequence_tokens(&tokens)?;
            return Ok(ParsedSql::AlterSequence(stmt));
        }
        let is_alter_table_statement = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("ALTER")
        );
        if is_alter_table_statement {
            let stmt = crate::sql::allowlist::validate_alter_table_tokens(&tokens)?;
            return Ok(ParsedSql::AlterTable(stmt));
        }

        // SQL-63・Issue #1876: 最上位の `WITH` にデータ変更 CTE を含む文だけが実行計画を持つ
        // （検出は `parse_with_clause` の結果。それ以外は従来どおり読み取りの `Statement`）。
        match crate::sql::allowlist::validate_session_with_tokens(
            &tokens,
            &storage_lookup,
            0,
            self.nesting_limits,
            ledger_mode,
        )? {
            crate::sql::allowlist::SessionWith::Statement(stmt) => Ok(ParsedSql::Statement(stmt)),
            crate::sql::allowlist::SessionWith::DataModifying(v) => {
                Ok(ParsedSql::DataModifyingCte(v))
            }
        }
    }

    /// `INSERT ... SELECT` の挿入先の確認と内側 SELECT の検証（SQL-51・Issue #1732）。
    ///
    /// 挿入先が表でなくビューなら `0A000`（ビューへの `INSERT ... SELECT` は未対応）、表でもビューでも
    /// なければ既存の書き込み文と同じ `42P01`／`42809`。内側 SELECT は既存の SELECT 検証器
    /// （`validate_sql_tokens_with_subquery_ctx_limits`）へトークン列のまま渡し、文字列を組み立て直さない。
    /// 受理するのは読み取りの文種だけで、`EXPLAIN`・`SET`・`SHOW`・`CREATE FUNCTION` は `42601`、
    /// `USING PLAN`（LLM I/O を起こす）は `0A000`。`Statement` の網羅 `match` により、将来の variant 追加は
    /// コンパイルエラーで判断を強制される。
    fn parse_insert_select(
        &self,
        shape: crate::sql::allowlist::InsertSelectShape,
        statement_tokens: &[crate::sql::lexer::Token],
        lookup: &impl crate::sql::allowlist::TableLookup,
        storage_lookup: &impl crate::sql::allowlist::TableLookup,
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::SqlSurfaceError;
        if !crate::sql::allowlist::TableLookup::table_exists(lookup, &shape.table_name)? {
            if matches!(self.storage.view_definition(&shape.table_name), Ok(Some(_))) {
                return Err(SqlSurfaceError::feature_not_supported(
                    "INSERT ... SELECT into a view is not supported",
                ));
            }
            return Err(self.reclassify_write_to_view_error_for(
                Some(crate::sql::allowlist::ViewWriteOp::Insert),
                SqlSurfaceError::undefined_table(shape.table_name),
            ));
        }
        // `INSERT` 先頭のため `QuotedNull` が残っている。単独の SELECT と同じ扱いに揃える
        // （FROM なし SELECT は引用情報を保持する。CTAS と共有）。
        let inner_tokens =
            crate::sql::allowlist::normalize_inner_query_tokens(shape.query_tokens.clone());
        // 内側 SELECT の `LIMIT` 省略（Issue #2615）。まず送られた形のまま検証し、受理されればそのまま使う
        // （集計・定数 SELECT など `LIMIT` 不要の形。行数上限は実行側の `rows.len() > cap` 判定が担う）。
        // 拒否され、かつ括弧深さ 0 に `LIMIT` が無いときだけ、末尾へ `LIMIT 行数上限+1` を付けて再検証する。
        // `cap` ちょうどで切ると黙って切り詰める fail-open になるため、必ず `+1` で超過（`54000`）を検出する。
        // 再検証も拒否なら元の拒否理由を返す（`OFFSET` のみ・`USING MODE` 等は従来の `42601` のまま）。
        let validate_inner = |tokens: &[crate::sql::lexer::Token]| {
            crate::sql::allowlist::validate_sql_tokens_with_subquery_ctx_limits(
                tokens,
                storage_lookup,
                0,
                self.nesting_limits,
            )
        };
        let mut implicit_limit: Option<u32> = None;
        let query = match validate_inner(&inner_tokens) {
            Ok(q) => q,
            Err(first_err) => {
                if crate::sql::allowlist::has_top_level_limit(&inner_tokens) {
                    return Err(first_err);
                }
                let fetch = u32::try_from(self.write_input_cap_injected()?.fetch)
                    .ok()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?;
                let mut limited = inner_tokens.clone();
                limited.push(crate::sql::lexer::Token::Keyword(
                    crate::sql::lexer::Keyword::Limit,
                ));
                limited.push(crate::sql::lexer::Token::Number(fetch.to_string()));
                match validate_inner(&limited) {
                    Ok(q) => {
                        implicit_limit = Some(fetch);
                        q
                    }
                    Err(_) => return Err(first_err),
                }
            }
        };
        crate::sql::allowlist::require_row_query_statement(&query, "INSERT ... SELECT")?;
        // 台帳の内容照合ハッシュは送られた文のトークン列（`USING OPERATION_ID` 句・末尾 `;` を除く）から
        // 算出する（Issue #1734）。未束縛の `$n` が残る形は上流で拒否済みだが、残っていればここでも
        // 固定文言で拒否する（fail-closed）。
        let statement_hash = crate::recovery::content_hash::for_insert_select(
            crate::recovery::content_hash::strip_operation_id_clause(statement_tokens),
        )
        .map_err(|_| SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        })?;
        let mut validated =
            crate::sql::allowlist::ValidatedInsertSelect::new(shape, query, statement_hash);
        if let Some(fetch) = implicit_limit {
            validated = validated.with_implicit_limit(fetch);
        }
        Ok(ParsedSql::InsertSelect(validated))
    }

    /// `INSERT ... SELECT` の束縛（SQL-51・Issue #1732）。挿入先スキーマと内側 SELECT の結果列メタ
    /// （既存の Describe で導出）を突き合わせ、列数・型の不整合を拒否する。実行腕・Describe 腕が呼ぶ。
    /// 内側とのスナップショット一致は要求しない（実行時の再検証は後続 Issue が write txn 内で行う）。
    fn bind_insert_select(
        &self,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedInsertSelect,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<
        crate::sql::insert_select::BoundInsertSelectTarget,
        crate::sql::allowlist::SqlSurfaceError,
    > {
        // `LIMIT` を暗黙付与した文（Issue #2615）は、付与値（行数上限 + 1）が Describe の `LIMIT` 範囲検査で
        // `22000` にならないよう、その値を上限とする。明示 `LIMIT` の文は従来の `limits` のまま。
        let implicit_limits = stmt
            .implicit_limit
            .and_then(|n| usize::try_from(n).ok())
            .map(crate::result_limits::ResultLimits::new);
        let limits = implicit_limits.as_ref().unwrap_or(limits);
        let source = self
            .describe_parsed_in_session_impl(
                session,
                &ParsedSql::Statement((*stmt.query).clone()),
                false,
                &[],
                limits,
            )?
            .ok_or_else(|| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        let (_read_txn, schema) = self.read_txn_with_schema(&stmt.table_name)?;
        let bound = crate::sql::insert_select::bind(stmt, &schema, &source)?;
        // `ON CONFLICT` 併用（Issue #2614）の静的な誤りは Describe 段・内側 SELECT の評価前に返す
        // （`42P10`／`42704`／`42809` 等）。結果は捨て、実行本体が自分で取得したスキーマで再束縛する
        // （スキーマ取得と書き込みの間の差を TOCTOU として扱わないため）。
        if let Some(clause) = stmt.on_conflict.as_ref() {
            self.reject_index_derived_conflict_constraint(&stmt.table_name, Some(clause))?;
            crate::sql::parser::bind_conflict_clause(clause, &schema)?;
            // 移行済み表で `id` 省略の UPSERT は実行本体と同じ条件で bind 時にも拒否する
            // （Describe 成功・Execute 必ず拒否の食い違いを作らない）。
            let targets =
                crate::sql::insert_select::resolve_target_columns(&schema, Some(&bound.columns))?;
            crate::sql::insert_select::reject_omitted_id_upsert(&schema, &targets)?;
        }
        Ok(bound)
    }

    /// Issue #927・SQL-29 (a)・TASK-213: `Self::parse_sql_prepared` が拡張
    /// クエリプロトコル経由のサブクエリを一律拒否するための、安価なテキスト
    /// 走査（`Parser::require_subquery_depth` を経由せず、`sql::allowlist::
    /// Parser` と同じ「直後が `SELECT` の `IN (`／`EXISTS (`」という判定条件
    /// だけをトークン列全体に対して行う）。誤検出（列名 `in`／`exists` の
    /// 通常参照）があっても安全側（拒否）に倒れるだけで、見逃し
    /// （実際にサブクエリを含むのに検出しない）は無い判定条件そのもの
    /// （`Parser::parse_where_leaf` の受理条件と同一。Issue #1191 でスカラー比較形を追加）。
    fn contains_subquery_syntax(tokens: &[crate::sql::lexer::Token]) -> bool {
        use crate::sql::lexer::{Keyword, Token};
        for i in 0..tokens.len() {
            let is_exists = matches!(tokens.get(i), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("EXISTS"))
                && matches!(tokens.get(i + 1), Some(Token::Punct('(')))
                && matches!(tokens.get(i + 2), Some(Token::Keyword(Keyword::Select)));
            let is_in = matches!(tokens.get(i + 1), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("IN"))
                && matches!(tokens.get(i + 2), Some(Token::Punct('(')))
                && matches!(tokens.get(i + 3), Some(Token::Keyword(Keyword::Select)));
            // スカラーサブクエリ `<col> <op> (SELECT ...)`（Issue #1191）: 比較演算子
            // トークン（`= < > <= >=`。`<>` は `< >` の 2 トークン）の直後の
            // `( SELECT` だけを検出する。任意の `( SELECT`（括弧付き集合演算の枝等）へ
            // 一般化しない（`$n` との併用が従来から受理される形を変えないため）。
            let is_scalar = matches!(tokens.get(i), Some(Token::Punct('(')))
                && matches!(tokens.get(i + 1), Some(Token::Keyword(Keyword::Select)))
                && i > 0
                && matches!(
                    tokens.get(i - 1),
                    Some(Token::Punct('=' | '<' | '>') | Token::Le | Token::Ge)
                );
            // 投影位置のスカラーサブクエリ（Issue #1352）: 直前が `SELECT` または `,` の
            // `( SELECT`。内側の `$n` が外側の `where_equality_dummy_flags` の添字を
            // ずらしうるため、`$n` との併用は Parse 段で拒否する。CTE 本体（直前が
            // `AS`）・集合演算の枝（直前が演算子キーワード）は対象にしない。
            let is_projection_scalar = matches!(tokens.get(i), Some(Token::Punct('(')))
                && matches!(tokens.get(i + 1), Some(Token::Keyword(Keyword::Select)))
                && i > 0
                && matches!(
                    tokens.get(i - 1),
                    Some(Token::Keyword(Keyword::Select) | Token::Punct(','))
                );
            // 量化比較のサブクエリ形 `<col> <op> ANY|SOME|ALL (SELECT ...)`（Issue #1855・
            // SQL-61 (3)）: 内側の `$n` が外側の `where_equality_dummy_flags` の添字を
            // ずらしうるため、`$n` との併用は Parse 段で拒否する（P0）。誤検出しても
            // 拒否側に倒れるだけ。
            let is_quantified = matches!(
                tokens.get(i),
                Some(Token::Ident(w))
                    if w.eq_ignore_ascii_case("ANY")
                        || w.eq_ignore_ascii_case("SOME")
                        || w.eq_ignore_ascii_case("ALL")
            ) && matches!(tokens.get(i + 1), Some(Token::Punct('(')))
                && matches!(tokens.get(i + 2), Some(Token::Keyword(Keyword::Select)))
                && i > 0
                && matches!(
                    tokens.get(i - 1),
                    Some(Token::Punct('=' | '<' | '>') | Token::Le | Token::Ge)
                );
            if is_exists || is_in || is_scalar || is_projection_scalar || is_quantified {
                return true;
            }
        }
        false
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
        self.parse_sql_prepared_with_declared(sql, &[])
    }

    /// [`Self::parse_sql_prepared`] に Parse で宣言された型（wire-server が型 OID から
    /// 写した `$n` ごとの宣言型。番号 1 始まりの `n` が `n - 1` 番目、`None` は未宣言）を
    /// 渡す版（Issue #2436・WIRE-24 ポインタ）。宣言型は推論段で反映する: 推論できない
    /// 位置・未参照番号は宣言型をスロット型とし（`42P18` にしない）、推論型がある位置は
    /// 推論型のまま宣言型を並べて保持する。`declared` が `param_count` より長い余剰分は
    /// ここでは無視する（過剰宣言の `08P01` は wire-server が判定する）。
    pub fn parse_sql_prepared_with_declared(
        &self,
        sql: &str,
        declared: &[Option<crate::sql::params::DeclaredParamType>],
    ) -> Result<PreparedSql, crate::sql::allowlist::SqlSurfaceError> {
        self.parse_sql_prepared_for_tenant(sql, declared, None)
    }

    /// [`Self::parse_sql_prepared_with_declared`] の本体。`tenant` が `Some` なら、`schema.name` の
    /// 修飾をそのテナントの SCHEMA として解決する（拡張クエリの Parse。認証済みテナントは
    /// [`crate::sql::mode::SessionState::bind_resolution_tenant`] が接続に 1 回だけ束縛した値。
    /// TABLE-27・Issue #1717）。`None` ならテナント SCHEMA の修飾は `3F000`。エラー文言の内部キーは
    /// テナントの SCHEMA 名へ戻す。
    pub(crate) fn parse_sql_prepared_for_tenant(
        &self,
        sql: &str,
        declared: &[Option<crate::sql::params::DeclaredParamType>],
        tenant: Option<&str>,
    ) -> Result<PreparedSql, crate::sql::allowlist::SqlSurfaceError> {
        self.parse_sql_prepared_for_tenant_impl(sql, declared, tenant)
            .map(|mut prepared| {
                prepared.resolution_tenant = tenant.map(Into::into);
                prepared
            })
            .map_err(|e| match tenant {
                Some(t) => self.scrub_error_for_tenant(t, e),
                None => e,
            })
    }

    fn parse_sql_prepared_for_tenant_impl(
        &self,
        sql: &str,
        declared: &[Option<crate::sql::params::DeclaredParamType>],
        tenant: Option<&str>,
    ) -> Result<PreparedSql, crate::sql::allowlist::SqlSurfaceError> {
        let mut tokens =
            Self::normalize_row_count_clauses(crate::sql::lexer::tokenize_with_params(sql)?)?;
        // `schema.table` のテーブル参照を解決して未修飾の名前（public）または内部キー（テナントの
        // SCHEMA 配下）へ置き換える（TABLE-27・Issue #1714・#1717）。位置推論・型推論・Bind の再
        // parse が同じ（解決済みの）トークン列を見るよう、最初に行う。
        let probe = |schema: &str| match tenant {
            Some(t) => self
                .storage
                .schema_id(t, schema)
                .map_err(crate::catalog::table_lookup_error),
            None => Ok(None),
        };
        let resolver: Option<crate::sql::relation_name::SchemaResolver<'_>> =
            tenant.map(|_| &probe as crate::sql::relation_name::SchemaResolver<'_>);
        let rewrite =
            crate::sql::relation_name::rewrite_relation_names_full(&mut tokens, resolver)?;
        let public_qualified = rewrite.names;
        let qualified_positions = rewrite.positions;
        let create_public = rewrite.create_public;
        // TABLE-18・SQL-23・TASK-205（Issue #909）: `CREATE VIEW`／`DROP VIEW`
        // は DDL であり、`sql::params` が受理する `$n` の許可位置一覧
        // （WHERE 等号右辺・VALUES 節等）はいずれも DML 向けの規範形のため、
        // これらの DDL トークン列に紛れ込んだ `$n` は位置に関わらず一律で
        // 拒否する（PostgreSQL の DDL がバインドパラメータを受け付けない
        // のと同じ方針。`sql::params::validate_param_positions` の汎用位置
        // 判定へ DDL 分岐を混在させない設計）。
        // Issue #1194: `CREATE TYPE`／`DROP TYPE` も同じ理由で `$n` を一律拒否する。
        let is_create_or_drop_view = matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name))
                if name.eq_ignore_ascii_case("CREATE") || name.eq_ignore_ascii_case("DROP")
        ) && matches!(
            tokens.get(1),
            Some(crate::sql::lexer::Token::Ident(name))
                if name.eq_ignore_ascii_case("VIEW") || name.eq_ignore_ascii_case("TYPE")
        );
        if is_create_or_drop_view
            && tokens
                .iter()
                .any(|t| matches!(t, crate::sql::lexer::Token::Param(_)))
        {
            return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "bind parameters are not supported in CREATE VIEW / DROP VIEW / CREATE TYPE / DROP TYPE",
            ));
        }
        // TABLE-24・Issue #1691: `setval`／`lastval` は実値を要する構文形のため `$n` を一律 `0A000` で
        // 拒否する（ダミー置換より前に判定する。位置検査の `42601` より優先）。
        if crate::sql::allowlist::is_sequence_function_select(&tokens)
            && tokens
                .iter()
                .any(|t| matches!(t, crate::sql::lexer::Token::Param(_)))
        {
            return Err(
                crate::sql::allowlist::SqlSurfaceError::feature_not_supported(
                    "bind parameters are not supported in setval / lastval",
                ),
            );
        }
        // SQL-51・Issue #1732: `INSERT ... SELECT` の内側 SELECT は `$n` の位置規則（`VALUES` 前提）の
        // 対象外のため、`$n` を含む形は位置検査より前に `0A000` で拒否する（fail-closed）。
        if matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("INSERT")
        ) && tokens.iter().any(|t| {
            matches!(
                t,
                crate::sql::lexer::Token::Keyword(crate::sql::lexer::Keyword::Select)
            )
        }) && tokens
            .iter()
            .any(|t| matches!(t, crate::sql::lexer::Token::Param(_)))
        {
            return Err(
                crate::sql::allowlist::SqlSurfaceError::feature_not_supported(
                    "bind parameters are not supported in INSERT ... SELECT",
                ),
            );
        }
        // SQL-53 (b)・Issue #1754: `UPDATE ... FROM`／`DELETE ... USING` の `$n` は位置規則の対象外のため、
        // Parse 段で `42601` として拒否する（fail-closed。`#1750` の WHERE 内サブクエリと同じ扱い）。
        if (crate::sql::allowlist::looks_like_update_from(&tokens)
            || crate::sql::allowlist::looks_like_delete_using(&tokens))
            && tokens
                .iter()
                .any(|t| matches!(t, crate::sql::lexer::Token::Param(_)))
        {
            return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "bind parameters are not supported in UPDATE ... FROM / DELETE ... USING",
            ));
        }
        // SQL-52・Issue #1678: `CREATE TABLE ... AS` も DDL であり内側の問い合わせは `$n` の位置規則の
        // 対象外のため、`$n` を含む形は位置検査より前に `0A000` で拒否する（fail-closed）。
        if crate::sql::allowlist::is_create_table_as_statement(&tokens)
            && tokens
                .iter()
                .any(|t| matches!(t, crate::sql::lexer::Token::Param(_)))
        {
            return Err(
                crate::sql::allowlist::SqlSurfaceError::feature_not_supported(
                    "bind parameters are not supported in CREATE TABLE AS",
                ),
            );
        }
        // Issue #1759・WIRE-21 (1): `COPY` は拡張クエリでも Parse／Bind／Describe まで受理する。
        // `$n` を含む形は位置検査（`42601`）より前に `42P02` で拒否する（fail-closed。`COPY` に
        // バインドパラメータは無い）。`$n` が無ければ簡易クエリの `COPY` と同一の許可リスト検証
        // （`validate_copy_tokens`）を Parse 時に行い、検証済みの文を保持する。
        if matches!(
            tokens.first(),
            Some(crate::sql::lexer::Token::Ident(name)) if name.eq_ignore_ascii_case("COPY")
        ) {
            if let Some(param) = tokens.iter().find_map(|t| match t {
                crate::sql::lexer::Token::Param(n) => Some(*n),
                _ => None,
            }) {
                return Err(crate::sql::allowlist::SqlSurfaceError::UndefinedParameter { param });
            }
            // 簡易クエリの `begin_copy`（印なしの `tokenize`）と同じ字句に揃えるため、引用付きの
            // `NULL`／`ARRAY`／SQL 値関数予約語（`QuotedNull`）もすべて `Ident` へ戻す
            // （`demote_quoted_null` は引用付き `ARRAY` を残し、表名・列名 `"array"` が 42601 になる）。
            let copy_tokens: Vec<crate::sql::lexer::Token> = tokens
                .iter()
                .cloned()
                .map(|t| match t {
                    crate::sql::lexer::Token::QuotedNull(s) => crate::sql::lexer::Token::Ident(s),
                    other => other,
                })
                .collect();
            let stmt = crate::sql::allowlist::validate_copy_tokens(
                &copy_tokens,
                &self.storage,
                self.ledger_mode,
            )
            .map_err(|e| self.reclassify_matview_write_error(e))?;
            return Ok(PreparedSql {
                resolution_tenant: None,
                tokens,
                param_count: 0,
                dummy_parsed: ParsedSql::Copy(crate::sql::allowlist::ValidatedCopy::new(
                    stmt,
                    sql.to_string(),
                )),
                order_by_distance_literal_is_param: false,
                where_equality_dummy_flags: Vec::new(),
                param_types: Vec::new(),
                declared: Vec::new(),
                session_tz_dependent: false,
                public_qualified,
                qualified_positions,
                create_public,
                route: temp_route::PreparedRoute::Plain,
            });
        }
        let param_count = crate::sql::params::validate_param_positions(&tokens)?;
        // Issue #927・SQL-29 (a)・TASK-213（拡張クエリプロトコルの注意点）:
        // `sql::params::where_equality_literal_is_param` は元トークン列全体を
        // 位置で走査し `Ident '=' $n` を数えて `equality_ordinal` 用の
        // ダミーフラグ配列を組み立てる。本 Issue のサブクエリ解決
        // （`sql::subquery::resolve_where_predicates`）は束縛前に新たな
        // `WherePredicate::Equality` 葉を追加しうるため、これを許すと
        // フラグ配列の添字と実際の束縛順序がずれ得る（`$n` を含まない
        // 通常の等価条件の enum ダミー値検証スキップ判定が誤る恐れがある）。
        // 拡張クエリプロトコル（Parse/Bind）経由のサブクエリは対象外とし、
        // ダミー置換・束縛より前にここで検出して一律 `42601` で拒否する
        // （fail-closed。簡易クエリプロトコル〔`Self::parse_sql`〕はこの
        // 経路を通らないため影響しない）。
        // `$n` を含まない文はダミー置換が恒等でフラグ配列も全 `false` のため
        // ずれが生じない。従来の `parse_sql` で受理されていた文を拡張クエリ
        // から実行できなくならないよう、拒否は `$n` を含む文に限定する
        // （PR #1217 レビュー指摘）。
        if param_count > 0 && Self::contains_subquery_syntax(&tokens) {
            return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "subquery is not supported over the extended query protocol (Parse/Bind)",
            ));
        }
        // ダミー置換前の元トークン列から判定する（置換後は `$n` 自体が
        // 消えるため、置換後トークン列からは判定できない）。
        let order_by_distance_literal_is_param =
            crate::sql::params::order_by_distance_literal_is_param(&tokens);
        // Issue #1342・#1546: 型推論（カタログとトークン列だけで決まる）を先に行い、
        // 確定した置換リテラル種別の型付きダミーで 1 回だけ構造検証 parse する。
        // 式内 `$n`（`1 + $1` 等）は文字列ダミーでは定数畳み込み／束縛の型検査に落ちるため、
        // 推論を parse より前に置く。推論不能（`42P18`）のときだけ、構文エラー（`42601`）を
        // 先に返す（PostgreSQL と同じ優先順位）ため文字列ダミーで parse を試す。
        let (param_types, param_checks) =
            match self.infer_param_types(&tokens, param_count, declared) {
                Ok(inferred) => inferred,
                Err(e @ crate::sql::allowlist::SqlSurfaceError::IndeterminateDatatype { .. }) => {
                    // 文字列ダミーの定数畳み込みで型エラーになっても途中終了しないよう、
                    // 構文エラー以外を無視して文全体を走査する。
                    let _guard = crate::sql::const_expr::defer_all_non_syntax_errors();
                    if let Err(syntax) =
                        self.parse_tokens(crate::sql::params::substitute_dummy(&tokens))
                    {
                        if syntax.wire_code() == "42601" {
                            return Err(syntax);
                        }
                    }
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
        let kinds: Vec<crate::sql::params::ParamLiteralKind> =
            param_types.iter().map(|t| t.literal_kind()).collect();
        // 等価述語のダミーフラグは種別確定後に求める（非 String 種別は
        // `WherePredicate::Equality` にならず序数に数えない）。
        let where_equality_dummy_flags =
            crate::sql::params::where_equality_literal_is_param_typed(&tokens, &kinds);
        // 仮値での定数畳み込みは値依存エラー（0 除算等）を Bind まで遅延する
        // （`VALUES (1, 10 / $1)` の実値を Bind で受けるため。Parse は構造と型だけ検証）。
        let dummy_tokens = crate::sql::params::substitute_dummy_typed(&tokens, &kinds);
        let dummy_parsed = {
            let positions: Vec<usize> = tokens
                .iter()
                .enumerate()
                .filter(|(_, t)| matches!(t, crate::sql::lexer::Token::Param(_)))
                .map(|(i, _)| i)
                .collect();
            let _guard = crate::sql::const_expr::defer_param_value_errors(positions);
            crate::datetime::reset_session_time_zone_read();
            let mut parsed = self.parse_tokens(dummy_tokens)?;
            // 解決済みトークンの再 parse は `public.` 修飾の情報を持たない。`$n` を含まない文の
            // 実行は `dummy_parsed` を複製するため、ここで保持して実行時の衝突検査を通す。
            attach_public_qualified(&mut parsed, &public_qualified, create_public);
            parsed
        };
        // Issue #2437・WIRE-24 (1)(5): 宣言型と文脈の型の変換可否は、構造検証 parse の成功後に
        // 判定する（構文 `42601`・名前解決 `42P01`／`42703` が型エラーより先に返る）。
        // 宣言が無い番号は判定しない（従来と完全に同一）。出現順は番号の昇順で決定的。
        for check in &param_checks {
            let decl = usize::from(check.param)
                .checked_sub(1)
                .and_then(|i| declared.get(i))
                .copied()
                .flatten();
            if let Some(d) = decl {
                crate::sql::param_coerce::check_declared(d, check)?;
            }
        }
        let session_tz_dependent = crate::datetime::take_session_time_zone_read();
        Ok(PreparedSql {
            resolution_tenant: None,
            tokens,
            param_count,
            dummy_parsed,
            order_by_distance_literal_is_param,
            where_equality_dummy_flags,
            param_types,
            declared: (0..usize::from(param_count))
                .map(|i| declared.get(i).copied().flatten())
                .collect(),
            session_tz_dependent,
            public_qualified,
            qualified_positions,
            create_public,
            route: temp_route::PreparedRoute::Plain,
        })
    }

    /// `$n` ごとの型公告を導出する（WIRE-12。[`Self::parse_sql_prepared`] 専用）。
    /// カタログ（スキーマ）だけを参照し行には触れない。スキーマを引けない場合
    /// （ビュー・解決不能な表記等）は常に `Text` へ倒す（公告のみの問題で、
    /// 値の検証には影響しない）。同じ番号が複数位置に現れて食い違う場合は、
    /// 1 つでもベクトル位置があれば `VectorText`（バイナリ不可側へ倒す）、
    /// それ以外は `Text`。
    ///
    /// Issue #2427: `INSERT ... ON CONFLICT ... DO UPDATE SET <列> = <列> + $n` の `$n` は
    /// INSERT 対象表の代入先列の型（整数・浮動小数）をスロット型にする（パターン 8）。
    ///
    /// Issue #1546: `UPDATE ... SET` 右辺・`INSERT ... VALUES` 要素の式内 `$n` は
    /// `sql::param_infer` が演算の相手・関数引数・兄弟枝から推論する。`INSERT` の
    /// 表名・列リストは検証済み構文木ではなくトークン列から読む（推論を構造検証 parse の
    /// 前に行うため）。型が決められない `$n`（全出現が推論不能）は
    /// [`crate::sql::allowlist::SqlSurfaceError::IndeterminateDatatype`]（`42P18`）。
    /// 判定は構文の形だけで決まり、表・列の存在には依存しない（存在オラクルを作らない）。
    ///
    /// Issue #2430・#2431・SQL-78: `SELECT`／`UPDATE`／`DELETE` の `WHERE` の非等価比較・`BETWEEN` の `$n` は比較相手の列の型
    /// （引けなければ `Text`）。`$n op $m`（両辺とも `$n`）は推論不能の出現として扱う。
    fn infer_param_types(
        &self,
        tokens: &[crate::sql::lexer::Token],
        param_count: u16,
        declared: &[Option<crate::sql::params::DeclaredParamType>],
    ) -> Result<
        (
            Vec<crate::sql::params::PreparedParamType>,
            Vec<crate::sql::param_coerce::ParamCheck>,
        ),
        crate::sql::allowlist::SqlSurfaceError,
    > {
        use crate::sql::allowlist::SqlSurfaceError;
        use crate::sql::exec::ColumnMeta;
        use crate::sql::lexer::Token;
        use crate::sql::param_coerce::{context_kind, ParamCheck, ParamContextKind};
        use crate::sql::param_infer::ParamInference;
        use crate::sql::params::{ExprTarget, ParamPosition, PreparedParamType};

        // WHERE 等価位置の列を引くテーブル名（トークン列から）。
        let where_table: Option<String> = match tokens.first() {
            Some(Token::Ident(w)) if w.eq_ignore_ascii_case("UPDATE") => match tokens.get(1) {
                Some(Token::Ident(name) | Token::QuotedNull(name)) => Some(name.clone()),
                _ => None,
            },
            // Issue #1188: `EXTRACT(field FROM src)` は括弧内に `FROM` を持つため、テーブル名を
            // 引く `FROM` は括弧深さ 0 の最初のものに限る。
            _ => first_top_level_from_index(tokens).and_then(|i| match tokens.get(i + 1) {
                Some(Token::Ident(name) | Token::QuotedNull(name)) => Some(name.clone()),
                _ => None,
            }),
        };
        // `INSERT INTO <表> ( <列> {, <列>} ) VALUES`（構文は `Parser::parse_insert` が
        // 保証する形。ここでは型の引き当てだけに使い、崩れていれば未解決として扱う）。
        // 列リスト省略（`INSERT INTO <表> VALUES (...)`。SQL-50・Issue #1730）は、表のスキーマから
        // `SELECT *` と同じ宣言順の列（疑似 `id` が先頭）を序数へ当てる。表を引けない（ビュー等）場合は
        // 従来どおり未解決（`Text`）。構文と既存の列型照会だけで決まり、存在オラクルを増やさない。
        let insert_target: Option<(String, Vec<String>)> = insert_target_from_tokens(tokens)
            .or_else(|| {
                let table = insert_table_without_column_list(tokens)?;
                let schema = self.storage.get_table_schema(&table).ok()?;
                Some((table, crate::catalog::lookup_star_columns_of(&schema)))
            });
        let column_type = |table: &str, column: &str| -> Option<PreparedParamType> {
            let id_type = || Some(PreparedParamType::Column(ColumnMeta::Id));
            let (schema, column) = match self.storage.get_table_schema(table) {
                Ok(schema) if column == "id" && schema.has_pseudo_id() => return id_type(),
                Ok(schema) => (schema, column.to_string()),
                // 表でなければ、ビューの公開列を基表の列へ解決して型を引く（書き込み文・読み取り文とも。
                // TABLE-43・Issue #1671・#1672・#2199）。公開名 `id` でも物理列へ解決してから疑似列かを
                // 判定する（TEXT 列を `id` として公開するビューで `$n` を整数と誤推論しない）。
                // 解決できなければ、`id` は従来どおり疑似列、他は未解決（`Text`）。
                Err(_) => {
                    let resolved = self
                        .storage
                        .with_view_write_snapshot(table, |snap, _| {
                            let (base, physical) = crate::sql::view_insert::resolve_insert_column(
                                snap, table, column,
                            )?;
                            snap.table_schema(&base).ok().map(|s| (s, physical))
                        })
                        .ok()
                        .flatten();
                    match resolved {
                        Some(r) => r,
                        None if column == "id" => return id_type(),
                        None => return None,
                    }
                }
            };
            if column == "id" && schema.has_pseudo_id() {
                return id_type();
            }
            let def = schema.columns.iter().find(|c| c.name == column)?;
            Some(PreparedParamType::Column(ColumnMeta::Scalar {
                name: def.name.clone(),
                ty: def.ty.clone(),
            }))
        };

        // 式内 `$n`（パターン 7）の推論結果を、番号ごとに集める。
        let segments = crate::sql::params::expr_param_segments(tokens);
        let mut expr_results: Vec<Vec<(ParamInference, Option<ParamContextKind>)>> =
            vec![Vec::new(); usize::from(param_count)];
        for seg in &segments {
            let (table, root_col, for_set): (Option<&str>, Option<&str>, bool) = match &seg.target {
                ExprTarget::Set(col) => (where_table.as_deref(), Some(col.as_str()), true),
                ExprTarget::Insert(k) => (
                    insert_target.as_ref().map(|(t, _)| t.as_str()),
                    insert_target
                        .as_ref()
                        .and_then(|(_, cols)| cols.get(*k))
                        .map(String::as_str),
                    false,
                ),
            };
            let root_type = table.zip(root_col).and_then(|(t, c)| column_type(t, c));
            let lookup = |name: &str| table.and_then(|t| column_type(t, name));
            for (n, inferred, kind) in
                crate::sql::param_infer::infer_segment(tokens, seg, for_set, root_type, &lookup)?
            {
                if let Some(slot) = usize::from(n)
                    .checked_sub(1)
                    .and_then(|k| expr_results.get_mut(k))
                {
                    slot.push((inferred, kind));
                }
            }
        }

        let mut out: Vec<PreparedParamType> = Vec::with_capacity(usize::from(param_count));
        // Issue #2437: 宣言型の変換可否判定の入力（出現ごとの位置種別と文脈の型）。
        let mut checks: Vec<ParamCheck> = Vec::new();
        for (idx, positions) in crate::sql::params::infer_param_positions(tokens, param_count)
            .into_iter()
            .enumerate()
        {
            let mut resolved: Vec<PreparedParamType> = Vec::new();
            let param_no = u16::try_from(idx.saturating_add(1)).unwrap_or(u16::MAX);
            // Issue #2430・#2431・SQL-78: `SELECT`／`UPDATE`／`DELETE` の `WHERE` の `$n op $m` の出現（型を決められない）。
            let mut where_indeterminate = false;
            for pos in &positions {
                // 列が引けた位置だけ変換可否判定の対象にする（引けない位置は従来どおり
                // `Text` へ倒し、判定せず後段の既存エラーに委ねる）。
                let (resolved_ty, kind): (Option<PreparedParamType>, Option<ParamContextKind>) =
                    match pos {
                        ParamPosition::Vector => (
                            Some(PreparedParamType::VectorText),
                            Some(ParamContextKind::Operator),
                        ),
                        // `USING PLAN`／`USING OPERATION_ID` は本 DB 固有の句で判定対象外。
                        ParamPosition::Text => (Some(PreparedParamType::Text), None),
                        ParamPosition::WhereColumn(col) => (
                            where_table.as_deref().and_then(|t| column_type(t, col)),
                            Some(ParamContextKind::Operator),
                        ),
                        ParamPosition::SetColumn(col) => (
                            where_table.as_deref().and_then(|t| column_type(t, col)),
                            Some(ParamContextKind::Assignment),
                        ),
                        ParamPosition::InsertValue(k) => (
                            insert_target.as_ref().and_then(|(table, cols)| {
                                cols.get(*k).and_then(|col| column_type(table, col))
                            }),
                            Some(ParamContextKind::Assignment),
                        ),
                        // `ON CONFLICT DO UPDATE SET <列> = <列> + $n`（Issue #2427・SQL-49）。
                        // 型は INSERT 対象表の代入先列。数値列以外は、列が引けた場合に限り
                        // リテラル経路が束縛段で返すのと同じ `42883`／`0A000` を Parse で返す
                        // （静的文言のみ・値を含めない）。引けない場合は `Text`（存在を観測させない）。
                        ParamPosition::UpsertAddColumn(col) => {
                            let upsert_table = insert_target
                                .as_ref()
                                .map(|(t, _)| t.clone())
                                .or_else(|| insert_table_name_only(tokens));
                            let ty = upsert_table.as_deref().and_then(|t| column_type(t, col));
                            if let Some(PreparedParamType::Column(ColumnMeta::Scalar {
                                ty: col_ty,
                                ..
                            })) = &ty
                            {
                                crate::sql::upsert_add::check_target_type(col_ty)?;
                            }
                            (ty, Some(ParamContextKind::AddOperand))
                        }
                        // 式内の出現は下で `expr_results` から合流させる。
                        ParamPosition::InExpr(_) => continue,
                        // 推論不能の出現は `resolved` に積まない。同じ番号の別の出現で型が決まれば
                        // それを使い、全出現が推論不能なら宣言型、それも無ければ `42P18`。
                        ParamPosition::WhereIndeterminate => {
                            where_indeterminate = true;
                            continue;
                        }
                    };
                if let (Some(t), Some(kind)) = (&resolved_ty, kind) {
                    if let Some(ctx) = context_kind(t) {
                        checks.push(ParamCheck {
                            param: param_no,
                            kind,
                            ctx,
                        });
                    }
                }
                resolved.push(resolved_ty.unwrap_or(PreparedParamType::Text));
            }
            // 式内の出現。推論不能の出現は無視し、全出現が推論不能のときだけ `42P18`。
            let mut expr_indeterminate = false;
            for (inferred, kind) in expr_results.get(idx).map(Vec::as_slice).unwrap_or_default() {
                match inferred {
                    ParamInference::Known(t) => {
                        if let (Some(kind), Some(ctx)) = (*kind, context_kind(t)) {
                            checks.push(ParamCheck {
                                param: param_no,
                                kind,
                                ctx,
                            });
                        }
                        resolved.push(t.clone());
                    }
                    ParamInference::Unresolved => resolved.push(PreparedParamType::Text),
                    ParamInference::Indeterminate => expr_indeterminate = true,
                }
            }
            // Issue #2436・WIRE-24: 推論型が無い（全出現が推論不能、または未参照）番号は、
            // 宣言型があればそれをスロット型にする（`42P18` にしない）。式の中で扱えない
            // 宣言型は推論時と同じく `0A000`。推論型がある位置は宣言型で上書きしない。
            if resolved.is_empty() {
                if let Some(d) = declared.get(idx).copied().flatten() {
                    if expr_indeterminate && !d.is_supported_in_expr() {
                        let param = u16::try_from(idx.saturating_add(1)).unwrap_or(u16::MAX);
                        return Err(SqlSurfaceError::FeatureNotSupported {
                            detail: format!(
                                "parameter ${param} of this data type is not supported inside an expression"
                            ),
                        });
                    }
                    out.push(PreparedParamType::Declared(d));
                    continue;
                }
            }
            if resolved.is_empty() && (expr_indeterminate || where_indeterminate) {
                let param = u16::try_from(idx.saturating_add(1)).unwrap_or(u16::MAX);
                return Err(SqlSurfaceError::IndeterminateDatatype { param });
            }
            out.push(match resolved.split_first() {
                None => PreparedParamType::Text,
                Some((first, rest)) => {
                    // 同一種別（Integer／Float／Boolean）の数値・真偽値スロットは列名・
                    // 型が異なっても統合する（`n = $1 AND m = $1` で Text へ
                    // 後退させない。PR #1372 レビュー指摘）。
                    let first_kind = first.literal_kind();
                    if rest.iter().all(|t| t == first)
                        || (first_kind != crate::sql::params::ParamLiteralKind::String
                            && rest.iter().all(|t| t.literal_kind() == first_kind))
                    {
                        first.clone()
                    } else if resolved.contains(&PreparedParamType::VectorText) {
                        PreparedParamType::VectorText
                    } else {
                        PreparedParamType::Text
                    }
                }
            });
        }
        Ok((out, checks))
    }

    /// Bind（拡張クエリプロトコルの 'B' 種別。Issue #935・WIRE-12・TASK-217）:
    /// [`Self::parse_sql_prepared`] が返したテンプレートへ、実値（バイト列。
    /// バイナリ形式の値は wire 層が受信形式を正規テキストへ復号済み、復号できない
    /// スロットは wire 層が `0A000` で拒否済みの前提。Issue #1345）を束縛し、
    /// [`Self::parse_sql`] が SQL テキストから直接返すのと**完全に同一**の
    /// [`ParsedSql`] を返す（第 2 の実行器を作らない設計。実行・Describe は
    /// 以降すべて既存の [`Self::execute_parsed_in_session`]／
    /// [`Self::describe_parsed_in_session`] を通る）。
    ///
    /// `values` は `$1` から順に対応する実値。NULL（`None`）は裸の `NULL` トークンへ
    /// 置換され、位置ごとの受理／拒否（`wire_code`）はリテラル `NULL` と同一になる
    /// （Issue #2439・WIRE-24。詳細は `sql::params` モジュールドキュメント参照）。
    /// 個数が [`PreparedSql::param_count`] と一致しない場合は `22000`
    /// （wire 層は Bind メッセージ自体の値件数検証で `08P01` を先に返す契約のため、
    /// ここでの不一致検出は防御的な保険）。
    fn bind_prepared_unscrubbed(
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
        // Issue #1759: `COPY` は `$n` を持たず、Parse で検証済みの文をそのまま返す
        // （`parse_tokens` は `COPY` を受理しないため再 parse できない）。
        if let ParsedSql::Copy(_) = &prepared.dummy_parsed {
            return Ok(prepared.dummy_parsed.clone());
        }
        // Issue #2436・WIRE-24: 宣言型がある値は、置換の前に宣言型として検証する。
        // NULL は検証対象外で SQL の NULL として置換される。非 UTF-8 は後段の復号が拒否する。
        // float4 は浮動小数スロットに限り丸めた正規値で Bind 値を差し替え、受信形式によらず同じ結果にする。
        // 差し替えは復号（総バイト数上限 MAX_INPUT_LEN の検証）より前に行い、正規化で
        // 長くなった値も参照回数込みで上限検査の対象にする。
        let mut canonical_values: Vec<(usize, Vec<u8>)> = Vec::new();
        for (i, decl) in prepared.declared.iter().enumerate() {
            let (Some(ty), Some(Some(raw))) = (decl, values.get(i)) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(raw) else {
                continue;
            };
            let n = u16::try_from(i.saturating_add(1)).unwrap_or(u16::MAX);
            let canonical = crate::sql::params::validate_declared_value(n, *ty, text)?;
            // 正規値への差し替えは、置換先が浮動小数リテラルのスロットに限る。整数・文字列・
            // 真偽値スロットでは元の値を据え置き（`'2'` が `'2.0'` になる等を避ける）、
            // 宣言型としての検証だけを行う。
            let is_float_slot = prepared
                .param_types
                .get(i)
                .is_some_and(|t| t.literal_kind() == crate::sql::params::ParamLiteralKind::Float);
            if let (Some(canonical), true) = (canonical, is_float_slot) {
                canonical_values.push((i, canonical.into_bytes()));
            }
        }
        let effective: std::borrow::Cow<'_, [Option<Vec<u8>>]> = if canonical_values.is_empty() {
            std::borrow::Cow::Borrowed(values)
        } else {
            let mut v = values.to_vec();
            for (i, bytes) in canonical_values {
                if let Some(slot) = v.get_mut(i) {
                    *slot = Some(bytes);
                }
            }
            std::borrow::Cow::Owned(v)
        };
        let decoded =
            crate::sql::params::decode_bind_values_nullable(&prepared.tokens, &effective)?;
        let kinds: Vec<crate::sql::params::ParamLiteralKind> = prepared
            .param_types
            .iter()
            .map(|t| t.literal_kind())
            .collect();
        let substituted =
            crate::sql::params::substitute_values_typed(&prepared.tokens, &decoded, &kinds)?;
        let mut parsed = self.parse_tokens(substituted)?;
        attach_public_qualified(
            &mut parsed,
            &prepared.public_qualified,
            prepared.create_public,
        );
        Ok(parsed)
    }

    /// `bind_prepared_unscrubbed` の公開入口。判定順序・エラー契約・結果は同関数と同一で、出口でだけ
    /// エラー文言の内部キー（`#<id>.<name>`）をテナントの SCHEMA 名へ戻す（TABLE-27・Issue #1717、
    /// ADR D2）。
    pub fn bind_prepared(
        &self,
        prepared: &PreparedSql,
        values: &[Option<Vec<u8>>],
    ) -> Result<ParsedSql, crate::sql::allowlist::SqlSurfaceError> {
        self.bind_prepared_unscrubbed(prepared, values)
            .map_err(|e| match prepared.resolution_tenant.as_deref() {
                Some(t) => self.scrub_error_for_tenant(t, e),
                None => e,
            })
    }

    /// Describe(statement)（拡張クエリプロトコルの 'D' 種別 S。Issue #935・
    /// WIRE-12・TASK-217）: [`Self::parse_sql_prepared`] が Parse 時点で構築した
    /// ダミー値束縛済みの [`ParsedSql`] をそのまま
    /// [`Self::describe_parsed_in_session`] へ渡す。値が未確定な段階（Bind 前）
    /// でも呼べる契約を維持するため、実際のパラメータ値は一切参照しない
    /// （投影列の形はどの `$n` 値を束縛しても変わらない——列名・型は列挙型
    /// `Statement`/`ValidatedInsert` 等の構造にのみ依存し、リテラル値には
    /// 依存しないため、ダミー値での導出結果は実値束縛後と常に一致する）。
    fn describe_prepared_in_session_unscrubbed(
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
        Self::check_prepared_for_session(session, prepared)?;
        self.describe_parsed_in_session_impl(
            session,
            &prepared.dummy_parsed,
            prepared.order_by_distance_literal_is_param,
            &prepared.where_equality_dummy_flags,
            &self.result_limits,
        )
    }

    /// `describe_prepared_in_session_unscrubbed` の公開入口。判定順序・エラー契約・結果は同関数と同一で、出口でだけ
    /// エラー文言の内部キー（`#<id>.<name>`）をテナントの SCHEMA 名へ戻す（TABLE-27・Issue #1717、
    /// ADR D2）。
    pub fn describe_prepared_in_session(
        &self,
        session: &crate::sql::mode::SessionState,
        prepared: &PreparedSql,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        self.describe_prepared_in_session_unscrubbed(session, prepared)
            .map_err(|e| match session.resolution_tenant() {
                Some(t) => self.scrub_error_for_tenant(t, e),
                None => e,
            })
    }

    /// [`Self::parse_sql`] が返した [`ParsedSql`] を実行する（Issue #933・
    /// TASK-71・WIRE-11。拡張クエリプロトコルの Execute（#934）がそのまま使う
    /// 単一の実行入口——SQL テキスト経由・束縛済み計画経由で第 2 の実行器を
    /// 作らない設計を踏襲する）。各分岐は [`Self::execute_sql_in_session`] が
    /// 従来インライン実行していた本体をそのまま移設したもので、実行順序・
    /// エラー契約は不変。
    fn execute_parsed_in_session_inner(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // 永続側の文を包んだ形（世代の検査は公開入口の粗いゲートが済ませている）は中身を実行する。
        if let ParsedSql::SessionRouted(routed) = parsed {
            if routed.is_persistent() {
                return self.execute_parsed_in_session_inner(ctx, session, routed.inner());
            }
        }
        // SQL-53（Issue #1751）: サブクエリつき述語形 DML は文単位の暗黙トランザクションで実行する
        // （内側の評価と書き込みを同じ write txn に乗せる）。`ON COMMIT` の適用は commit 側が担う。
        // SQL-63・Issue #1876: データ変更 CTE も同じ暗黙トランザクションで実行する（全副文・台帳記録・
        // 主クエリが 1 つの write txn に乗り、commit 時に遅延 FK の検査を通る。副文ごとの autocommit
        // では原子的にならない）。
        if Self::is_subquery_predicate_dml(parsed)
            || matches!(parsed, ParsedSql::DataModifyingCte(_))
        {
            return self.execute_subquery_dml_autocommit(ctx, session, parsed);
        }
        let outcome = self.execute_parsed_statement(ctx, session, parsed)?;
        // 自動コミットの文が成功した直後に、一時表の `ON COMMIT` 動作を適用する
        // （TABLE-44・Issue #1683。失敗した文では適用しない。明示・暗黙トランザクションの
        // COMMIT は `sql::transaction::commit_active` が適用する）。
        if !matches!(parsed, ParsedSql::Transaction(_)) {
            session.apply_temp_on_commit()?;
        }
        Ok(outcome)
    }

    /// [`Self::execute_parsed_in_session_inner`] の本体（文の種類ごとの実行。`ON COMMIT` の
    /// 適用は呼び出し元が行う）。
    fn execute_parsed_statement(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // SQL-37（Issue #1586）: autocommit の読み取り文は文の開始時刻を `now()` 系の
        // 値として確定する（`now()` と `statement_timestamp()` は同値）。
        let _clock_scope = statement_clock_scope(parsed, None)?;
        // Issue #2185: `jsonb_path_*` の束縛が起動時 CLI の評価上限を焼き込めるよう、文の束縛区間に
        // 限って `jsonpath_limits` を現スレッドへ張る（`ClockScope` と同じ RAII。大域状態ではない）。
        let _jsonpath_limits_scope =
            crate::sql::jsonpath_fn::JsonPathLimitsScope::enter(self.jsonpath_limits);
        // Issue #1787: `~` 系の束縛が起動時 CLI の正規表現上限を焼き込めるよう同じ区間に張る。
        let _regex_limits_scope = crate::sql::regex_op::RegexLimitsScope::enter(self.regex_limits);
        // TABLE-33・Issue #1575: TZ 無しの TIMESTAMPTZ リテラル束縛・DDL 既定値の正規化が読む
        // セッション TimeZone を文ごとに張る（`SET TIME ZONE; INSERT` の後続文が新しい値を見る）。
        let _session_tz_scope = session_time_zone_scope(session);
        // SQL-37・Issue #1587: INSERT の列 `DEFAULT now()` 系の評価時刻（autocommit は文の開始時刻）。
        let _default_clock_scope = insert_default_clock_scope(parsed, None)?;
        // TABLE-24・Issue #1690: INSERT の `nextval`／`currval`（VALUES・列 DEFAULT）の採番文脈。
        let _sequence_scope = insert_sequence_scope(&self.storage, session, parsed);
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
                    let outcome = self.execute_insert_returning_form(
                        crate::tenant::WriteTarget::Autocommit(&self.storage),
                        ctx,
                        stmt,
                        &lookup,
                    )?;
                    return Ok(crate::sql::SqlOutcome::Returning(outcome));
                }
                let outcome = self.execute_insert_form(
                    crate::tenant::WriteTarget::Autocommit(&self.storage),
                    ctx,
                    stmt,
                    &lookup,
                )?;
                Ok(crate::sql::SqlOutcome::Insert(outcome))
            }
            // SQL-51・Issue #1732・#1733: 束縛（列数・型。不一致は挿入より前に拒否）の後、内側 SELECT を
            // 書き込むセッションの `PolicyContext` で評価して挿入する（`core/insert_select_exec.rs`）。
            ParsedSql::InsertSelect(stmt) => self.execute_parsed_insert_select(
                crate::core::insert_select_exec::InsertSelectTxn::Autocommit,
                ctx,
                session,
                stmt,
            ),
            // SQL-63・Issue #1876: 入口（`execute_parsed_in_session_inner`）が暗黙トランザクションへ
            // 振り分けるため、ここへ到達するのは構造上の誤りだけ（副文ごとの autocommit で原子性を
            // 失わないよう、実行せず fail-closed）。
            ParsedSql::DataModifyingCte(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })
            }
            ParsedSql::ViewInsert(stmt) => self
                .execute_view_insert(
                    || crate::tenant::WriteTarget::Autocommit(&self.storage),
                    ctx,
                    stmt,
                )
                .map(|(outcome, _base_table)| outcome),
            // ビューへの DELETE／UPDATE（TABLE-43・Issue #1672）。基表形へ書き換えて既存の基表 DML 経路へ流す。
            ParsedSql::ViewDelete(stmt) => self
                .execute_view_delete(
                    || crate::tenant::WriteTarget::Autocommit(&self.storage),
                    ctx,
                    session,
                    stmt,
                )
                .map(|(outcome, _base_table)| outcome),
            ParsedSql::ViewUpdate(stmt) => self
                .execute_view_update(
                    || crate::tenant::WriteTarget::Autocommit(&self.storage),
                    ctx,
                    session,
                    stmt,
                )
                .map(|(outcome, _base_table)| outcome),
            ParsedSql::Truncate(stmt) => {
                let outcome = self.execute_truncate_form(ctx, stmt)?;
                Ok(crate::sql::SqlOutcome::Truncate(outcome))
            }
            ParsedSql::Delete(stmt) => match stmt {
                // 単一行形（[`crate::sql::allowlist::ValidatedDelete`]）は
                // `RETURNING`（Issue #873・SQL-21）を保持しうるためここで
                // 分岐する（`execute_delete_sql` は検証直後に `42601` で拒否
                // する非セッション経路。`Self` モジュールドキュメント参照）。
                // 述語形（[`crate::sql::allowlist::ValidatedPredicateDelete`]）も
                // Issue #1182 で `RETURNING` を保持しうるため同じく分岐する
                // （`execute_predicate_delete_returning_form`）。`RETURNING` なしは
                // 従来どおり `execute_predicate_delete_form`（Issue #871）。
                crate::sql::allowlist::DeleteStatement::SingleRow(v) => {
                    if v.returning.is_some() {
                        let outcome = self.execute_delete_returning_form(
                            crate::tenant::WriteTarget::Autocommit(&self.storage),
                            ctx,
                            v,
                        )?;
                        return Ok(crate::sql::SqlOutcome::Returning(outcome));
                    }
                    let outcome = self.execute_delete_form(
                        crate::tenant::WriteTarget::Autocommit(&self.storage),
                        ctx,
                        v,
                    )?;
                    Ok(crate::sql::SqlOutcome::Delete(outcome))
                }
                crate::sql::allowlist::DeleteStatement::Predicate(v) => {
                    // Issue #1129: `PARTITIONED` 付きは分割実行（非原子・autocommit 専用）。
                    if v.partitioned().is_some() {
                        let outcome = self.execute_partitioned_delete_form(ctx, session, v)?;
                        return Ok(crate::sql::SqlOutcome::Delete(outcome));
                    }
                    if v.returning().is_some() {
                        let outcome = self.execute_predicate_delete_returning_form(
                            crate::tenant::WriteTarget::Autocommit(&self.storage),
                            ctx,
                            session,
                            v,
                        )?;
                        return Ok(crate::sql::SqlOutcome::Returning(outcome));
                    }
                    let outcome = self.execute_predicate_delete_form(
                        crate::tenant::WriteTarget::Autocommit(&self.storage),
                        ctx,
                        session,
                        v,
                    )?;
                    Ok(crate::sql::SqlOutcome::Delete(outcome))
                }
            },
            // SQL-53 (b)・Issue #1754: 束縛（列解決・型照合）を通した後に `0A000` で止める。実行は #1755 以降。
            ParsedSql::UpdateFrom(stmt) => self.execute_update_from_stub(stmt),
            ParsedSql::DeleteUsing(stmt) => self.execute_delete_using_stub(stmt),
            ParsedSql::Update(stmt) => {
                // Issue #1129: `PARTITIONED` 付きは分割実行（非原子・autocommit 専用）。
                if let crate::sql::allowlist::ValidatedUpdateForm::Predicate(v) = stmt {
                    if v.partitioned().is_some() {
                        let outcome = self.execute_partitioned_update_form(ctx, session, v)?;
                        return Ok(crate::sql::SqlOutcome::Update(outcome));
                    }
                }
                // `RETURNING`（Issue #1182・SQL-21）は単一行・述語形いずれも
                // セッション経路（本分岐）でのみ実行結線する（非セッション入口
                // `execute_update_sql` は `validate_update` が `42601` 拒否する）。
                let has_returning = match stmt {
                    crate::sql::allowlist::ValidatedUpdateForm::Single(v) => v.returning.is_some(),
                    crate::sql::allowlist::ValidatedUpdateForm::Predicate(v) => {
                        v.returning().is_some()
                    }
                };
                if has_returning {
                    let outcome = self.execute_update_returning_form(
                        crate::tenant::WriteTarget::Autocommit(&self.storage),
                        ctx,
                        session,
                        stmt,
                    )?;
                    return Ok(crate::sql::SqlOutcome::Returning(outcome));
                }
                let outcome = self.execute_predicate_update_form(
                    crate::tenant::WriteTarget::Autocommit(&self.storage),
                    ctx,
                    session,
                    stmt,
                )?;
                Ok(crate::sql::SqlOutcome::Update(outcome))
            }
            ParsedSql::Partitioned(control) => {
                self.execute_partitioned_control(ctx, session, control)
            }
            // TABLE-44・Issue #1684: 一時表だけを参照する文は一時コアで実行する
            // （`core/temp_route.rs`）。永続コアの実行器へは渡さない。
            ParsedSql::SessionRouted(routed) if routed.is_persistent() => {
                self.execute_parsed_statement(ctx, session, routed.inner())
            }
            ParsedSql::SessionRouted(routed) => {
                self.execute_temp_routed_autocommit(ctx, session, routed)
            }
            // SQL-23・TASK-203（Issue #902）: DDL 実行権限ゲート
            // （`sql::ddl::require_ddl_permission`）を、カタログ照会（対象
            // テーブルの存在確認）を含む `execute_drop_table` より必ず先に
            // 通す（`SqlSurfaceError::InsufficientPrivilege` ドキュメント
            // 参照）。`ctx`（テナント境界）は `Storage::drop_table` が取らない
            // ため未使用のまま——`DROP TABLE` はテナントスコープの操作ではない。
            ParsedSql::DropTable(stmt) => {
                // TABLE-44・Issue #1683: 名前がすべて自セッションの一時表なら、DDL 権限ゲートの
                // 対象外で一時表を削除する。一時表と永続表の混在は、2 つのストアにまたがる原子的な
                // 削除ができないため `0A000`（fail-closed）。判定は自セッションの一時カタログだけ
                // で決まり、永続表の存否は観測できない。
                match crate::sql::ddl::classify_drop_targets(session, &stmt.table_names) {
                    Some(true) => {
                        let outcome = crate::sql::ddl::execute_drop_temp_tables(session, stmt)?;
                        return Ok(crate::sql::SqlOutcome::DropTable(outcome));
                    }
                    Some(false) => {}
                    None => {
                        return Err(
                            crate::sql::allowlist::SqlSurfaceError::feature_not_supported(
                                "DROP TABLE cannot mix temporary and permanent tables",
                            ),
                        );
                    }
                }
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
                self.require_creation_target(
                    ctx,
                    session,
                    stmt.public_qualified,
                    &stmt.table_name,
                )?;
                // TABLE-44・Issue #1683: 永続表の FOREIGN KEY がこのセッションの一時表の名前に
                // 解決される場合は `42P16`（永続表は一時表を参照できない）。同名の永続表を
                // 黙って参照する fail-open を避ける。
                // `IF NOT EXISTS` で同名の関係が既にあるときは、`execute_create_table` が定義の
                // 意味検証より前に no-op 成功を返す契約のため、この検査も行わない（権限確認後の
                // 既存名判定を FK 検証より先に維持する）。実際に新規作成する場合だけ拒否する。
                let existing_noop = stmt.if_not_exists
                    && self
                        .storage
                        .relation_name_exists(&stmt.table_name)
                        .map_err(|_| crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "internal error".to_string(),
                        })?;
                // ADR D7（TABLE-27・Issue #1717）の検査は `execute_create_table` 内で、既存名 no-op の
                // 判定を通過して実作成へ進む経路すべてに対して行う（ここで省略判定を持つと、判定後に
                // 他セッションが同名を DROP した競合で未検査の定義が作られる）。
                if !existing_noop
                    && stmt
                        .foreign_keys
                        .iter()
                        .any(|fk| session.temp_tables().contains(fk.parent_table()))
                {
                    return Err(
                        crate::sql::allowlist::SqlSurfaceError::invalid_table_definition(
                            stmt.table_name.clone(),
                        ),
                    );
                }
                let outcome = crate::sql::ddl::execute_create_table(
                    &self.storage,
                    stmt,
                    &self.schema_limits,
                )?;
                Ok(crate::sql::SqlOutcome::CreateTable(outcome))
            }
            // SQL-52・Issue #1678: CTAS。DDL 権限ゲートを名前解決・カタログ照会（内側の問い合わせの
            // 実カタログ検証を含む）より先に置く（権限の無い主体へ元表・作成先の有無を返さない）。
            ParsedSql::CreateTableAs(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                self.require_creation_target(
                    ctx,
                    session,
                    stmt.public_qualified,
                    stmt.table_name(),
                )?;
                let outcome = self.execute_create_table_as(ctx, session, stmt)?;
                Ok(crate::sql::SqlOutcome::CreateTableAs(outcome))
            }
            // TABLE-44・Issue #1683: 一時表の作成は DDL 実行権限ゲートの対象外（セッション内で
            // 閉じる）。永続カタログには触れず、セッションの一時カタログへ登録する。
            ParsedSql::CreateTempTable(stmt) => {
                let outcome = crate::sql::ddl::execute_create_temp_table(
                    session,
                    ctx.tenant_id(),
                    stmt,
                    &self.schema_limits,
                    self.temp_table_limits,
                    self,
                )?;
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
                crate::sql::schema_dependency::check_alter_table(stmt)?;
                let outcome = crate::sql::ddl::execute_alter_table(
                    &self.storage,
                    ctx.tenant_id(),
                    stmt,
                    &self.schema_limits,
                )?;
                Ok(crate::sql::SqlOutcome::AlterTable(outcome))
            }
            // TABLE-18・SQL-23・TASK-205（Issue #909）: `DropTable` と同じ判定
            // 順序（DDL 実行権限ゲート → カタログ照会を含む実行本体）。
            ParsedSql::CreateView(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                crate::sql::schema_dependency::check_view_body(stmt.name(), stmt.body_sql())?;
                // TABLE-41 (1)・Issue #1659: 列別名リストと本文の出力列数の突合は、カタログを
                // 照会するため DDL 権限ゲートの後に行う（権限の無い主体へ列構成を返さない）。
                self.check_create_view_column_aliases(session, stmt)?;
                // TABLE-43 (2)・Issue #1673: `WITH CHECK OPTION` の宣言先が自動更新できる形か・検査
                // できる述語かを、カタログ書き込みの前に判定する（宣言なしは何もしない）。
                // 新定義による循環（`42P17`）は、新定義を重ねて連鎖を辿る宣言検査より先に判定する
                // （CHECK OPTION の有無でエラー契約が割れて `XX000` になるのを防ぐ）。
                if stmt.or_replace()
                    && self
                        .storage
                        .view_definition(stmt.name())
                        .map_err(crate::sql::ddl::map_replace_view_error)?
                        .is_some()
                    && self
                        .storage
                        .view_body_reaches_self(stmt.name(), stmt.body_sql())
                        .map_err(crate::sql::ddl::map_replace_view_error)?
                {
                    return Err(crate::sql::ddl::map_replace_view_error(
                        crate::catalog::CatalogError::ViewReferenceCycle(stmt.name().to_string()),
                    ));
                }
                crate::sql::view_check::check_declaration(
                    &self.storage,
                    stmt.name(),
                    crate::catalog::ViewDef {
                        base_relation: stmt.base_relation().to_string(),
                        body_sql: stmt.body_sql().to_string(),
                        column_aliases: stmt.column_aliases().to_vec(),
                        check_option: stmt.check_option(),
                    },
                )?;
                // TABLE-41 (2)・Issue #1660: `OR REPLACE` は互換性検査付きの置換経路へ。
                let outcome = if stmt.or_replace() {
                    self.create_or_replace_view_in_session(session, stmt)?
                } else {
                    crate::sql::ddl::execute_create_view(&self.storage, stmt)?
                };
                Ok(crate::sql::SqlOutcome::CreateView(outcome))
            }
            // TABLE-42・Issue #1664: DDL 権限ゲートを名前解決・カタログ照会より先に置く
            // （権限の無い主体へ relation の有無・列構成を返さない）。以降は write txn 内で
            // 列の確定・本文の評価・定義と実体の原子的な作成を行う。
            ParsedSql::CreateMaterializedView(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                crate::sql::schema_dependency::check_view_body(stmt.name(), stmt.body_sql())?;
                let outcome = self.execute_create_materialized_view(ctx, session, stmt)?;
                Ok(crate::sql::SqlOutcome::CreateMaterializedView(outcome))
            }
            // TABLE-42・Issue #1665: 権限ゲートを名前解決より先に置く（`CREATE` と同じ）。
            ParsedSql::RefreshMaterializedView(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = self.execute_refresh_materialized_view(ctx, session, stmt)?;
                Ok(crate::sql::SqlOutcome::RefreshMaterializedView(outcome))
            }
            ParsedSql::DropView(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_drop_view(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::DropView(outcome))
            }
            // TABLE-42 (5)・Issue #1667: `DropView` と同じ判定順序（DDL 権限ゲート → カタログ照会を
            // 含む実行本体）。権限の無い主体には対象の有無を問わず常に `42501` のみを返す。
            ParsedSql::DropMaterializedView(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_drop_materialized_view(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::DropMaterializedView(outcome))
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
            // TABLE-14・SQL-23・TASK-198（Issue #1194）: 他の DDL と同じ判定順序
            // （DDL 実行権限ゲート → 実行本体）。ENUM 型は全テナント共有カタログで
            // `ctx` は取らない。権限の無い主体には型の有無を問わず常に `42501`。
            ParsedSql::CreateType(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                self.require_creation_target(ctx, session, false, stmt.name())?;
                let outcome =
                    crate::sql::ddl::execute_create_type(&self.storage, stmt, &self.schema_limits)?;
                Ok(crate::sql::SqlOutcome::CreateType(outcome))
            }
            ParsedSql::DropType(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_drop_type(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::DropType(outcome))
            }
            // TABLE-24・SQL-23（Issue #1688）: 他の DDL と同じ判定順序（DDL 実行権限ゲート →
            // 実行本体）。SEQUENCE の定義は全テナント共有カタログで `ctx` は取らない。
            ParsedSql::CreateSequence(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                self.require_creation_target(ctx, session, stmt.public_qualified, stmt.name())?;
                let outcome = crate::sql::ddl::execute_create_sequence(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::CreateSequence(outcome))
            }
            ParsedSql::DropSequence(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::ddl::execute_drop_sequence(&self.storage, stmt)?;
                Ok(crate::sql::SqlOutcome::DropSequence(outcome))
            }
            // SQL-66（Issue #1920）: 権限ゲートは置かない（権限の無いセッションはセッション内の定義だけを
            // 消せる。CREATE FUNCTION と対称。42501 化は #1922）。カタログは DDL 権限ありのときだけ触る。
            ParsedSql::DropFunction(stmt) => {
                let outcome = crate::sql::udf_ddl::execute_drop_function(
                    &self.storage,
                    ctx.tenant_id(),
                    session,
                    stmt,
                )?;
                Ok(crate::sql::SqlOutcome::DropFunction(outcome))
            }
            // TABLE-45（Issue #1722）: DDL 実行権限ゲート → 実行本体。権限の無い主体には対象表・トリガの
            // 有無を問わず常に `42501`。TRIGGER 定義はテナント単位のため `ctx.tenant_id()` を渡す。
            ParsedSql::CreateTrigger(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::trigger_ddl::execute_create_trigger(
                    &self.storage,
                    ctx.tenant_id(),
                    stmt,
                )?;
                Ok(crate::sql::SqlOutcome::CreateTrigger(outcome))
            }
            ParsedSql::DropTrigger(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome = crate::sql::trigger_ddl::execute_drop_trigger(
                    &self.storage,
                    ctx.tenant_id(),
                    stmt,
                )?;
                Ok(crate::sql::SqlOutcome::DropTrigger(outcome))
            }
            // TABLE-27（Issue #1713）: DDL 実行権限ゲート → 実行本体。SCHEMA はテナント単位のカタログ
            // のため `ctx.tenant_id()` を渡す（SEQUENCE との違い）。権限の無い主体には対象の有無を
            // 問わず常に `42501`。
            ParsedSql::CreateSchema(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome =
                    crate::sql::ddl::execute_create_schema(&self.storage, ctx.tenant_id(), stmt)?;
                Ok(crate::sql::SqlOutcome::CreateSchema(outcome))
            }
            ParsedSql::DropSchema(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome =
                    crate::sql::ddl::execute_drop_schema(&self.storage, ctx.tenant_id(), stmt)?;
                Ok(crate::sql::SqlOutcome::DropSchema(outcome))
            }
            // TABLE-27（Issue #1716）: DDL 実行権限ゲート → 実行本体（`ALTER SCHEMA ... RENAME TO`）。
            ParsedSql::AlterSchema(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome =
                    crate::sql::ddl::execute_alter_schema(&self.storage, ctx.tenant_id(), stmt)?;
                Ok(crate::sql::SqlOutcome::AlterSchema(outcome))
            }
            // TABLE-24・SQL-23（Issue #1689）: DDL 実行権限ゲート → 実行本体。`ctx.tenant_id()` は
            // RESTART なしの範囲検査で呼び出し元テナントの現在値を読むためだけに渡す。
            ParsedSql::AlterSequence(stmt) => {
                crate::sql::ddl::require_ddl_permission(session)?;
                let outcome =
                    crate::sql::ddl::execute_alter_sequence(&self.storage, ctx.tenant_id(), stmt)?;
                Ok(crate::sql::SqlOutcome::AlterSequence(outcome))
            }
            // TABLE-24・Issue #1691: `setval`／`lastval`。DDL 権限ゲートはかけない（作用範囲が呼び出し元
            // テナントに閉じるため。設計記録 §10）。
            ParsedSql::SequenceFunction(stmt) => {
                let result = crate::sql::sequence_fn::execute_autocommit(
                    &self.storage,
                    session,
                    ctx.tenant_id(),
                    stmt,
                )?;
                Ok(crate::sql::SqlOutcome::Query(result))
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
            // Issue #1759・WIRE-21 (1): 拡張クエリの `COPY` は Parse／Bind／Describe まで受理し、
            // Execute は wire-server が先に横取りして `begin_copy*` へ結線する（Issue #1760）ため
            // 通常は到達しない。他の呼び出し元への防御的な fail-closed（`0A000`・副作用なし。
            // 行・台帳・世代には触れない）。
            ParsedSql::Copy(_) => Err(
                crate::sql::allowlist::SqlSurfaceError::feature_not_supported(
                    "COPY via the extended query protocol is not supported yet",
                ),
            ),
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
    fn execute_sql_in_session_unscrubbed(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let tokens = crate::sql::lexer::tokenize_marked(sql)?;
        // 定数式の畳み込み（INSERT VALUES・DEFAULT）は parse 中に行われるため、セッション
        // TimeZone を parse 区間へも張る（ゾーン無し `TIMESTAMPTZ` リテラル・`AT LOCAL`。Issue #1576）。
        let parsed = {
            let _session_tz_scope = session_time_zone_scope(session);
            let resolver = self.schema_resolver(ctx);
            self.parse_tokens_gated(tokens, session, Some(&resolver))?
        };
        // 一時カタログに対する細かいゲートは上の parse で通過済みのため、粗いゲートを持たない
        // 内部版を呼ぶ（TABLE-44・Issue #1681）。
        self.execute_parsed_in_session_inner(ctx, session, &parsed)
    }

    /// 認証済みテナントの UDF カタログをセッションの UDF レジストリへロードする（SQL-66・Issue #1921）。
    /// wire-server のハンドシェイクが認証成功後・ReadyForQuery の前に 1 回呼ぶ。DDL 権限の有無を
    /// 問わず、呼び出しは誰でもできる。カタログ全体が読めなければ `Err`（`XX000`・固定文言）で、
    /// 呼び出し側は接続を拒否する（fail-closed）。個別の定義が登録できない場合はその定義だけ skip する。
    pub fn load_session_udfs(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        // 解決テナントを ctx のテナントへ束縛する（別テナントへ束縛済みなら fail-closed）。実行入口の
        // `check_resolution_tenant` がロード元テナントと異なる ctx での実行を拒否する前提になる。
        if !session.bind_resolution_tenant(ctx.tenant_id()) {
            session.udfs_mut().remove_catalog_entries();
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "could not load function catalog".to_string(),
            });
        }
        crate::sql::udf_load::load(&self.storage, ctx.tenant_id(), session.udfs_mut())
    }

    /// 他セッションの `CREATE`／`DROP FUNCTION` を反映する（SQL-66・Issue #1921）。wire-server が
    /// Query／Parse の実行前に呼ぶ。UDF カタログの世代が変わっていなければ何もしない。失敗したら
    /// カタログ由来の登録を全部消して次回再試行する（stale な定義で実行し続けない）。未ロードの
    /// セッションと、ロード時と異なるテナントの ctx では何もロードしない。
    pub fn refresh_session_udfs(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
    ) {
        crate::sql::udf_load::refresh(&self.storage, ctx.tenant_id(), session.udfs_mut());
    }

    /// `execute_sql_in_session_unscrubbed` の公開入口。判定順序・エラー契約・実行結果は同関数と同一で、
    /// 出口でだけ SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を利用者向けの
    /// `<schema>.<name>` へ戻す（TABLE-27・Issue #1717、ADR D2。内部 ID を文言へ出さない）。
    pub fn execute_sql_in_session(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        Self::check_resolution_tenant(session, ctx)?;
        self.execute_sql_in_session_unscrubbed(ctx, session, sql)
            .map_err(|e| self.scrub_error(ctx, e))
            .map(|o| self.scrub_outcome(ctx, o))
    }

    /// [`Self::execute_sql_in_session`] のトランザクション対応版（SQL-31・
    /// TASK-221）。`txn` が `Idle` の間は [`Self::execute_parsed_in_session`]
    /// （既存 autocommit 経路）とビット同一に振る舞う。`wire-server` の簡易・
    /// 拡張クエリ両経路が、接続（セッション）ごとに 1 つ保持する
    /// `SessionTransaction` を渡してここへ委譲する唯一の入口（第 2 の実行器を
    /// 作らない設計。`Self::execute_sql_in_session` と同じ判定順序を踏襲する）。
    fn execute_sql_in_txn_unscrubbed<'e>(
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
        let parsed = {
            // parse 中の定数式畳み込みがセッション TimeZone を読む（Issue #1576）。
            let _session_tz_scope = session_time_zone_scope(session);
            // `parse_sql` と同じ字句解析 → 構文検証に、一時カタログの細かいゲートを足したもの。
            let parsed = (|| {
                let tokens = crate::sql::lexer::tokenize_marked(sql)?;
                let resolver = self.schema_resolver(ctx);
                self.parse_tokens_gated(tokens, session, Some(&resolver))
            })();
            match parsed {
                Ok(parsed) => parsed,
                Err(e) => {
                    txn.fail();
                    return Err(e);
                }
            }
        };
        self.execute_parsed_in_txn_inner(ctx, session, txn, &parsed)
    }

    /// `execute_sql_in_txn_unscrubbed` の公開入口。判定順序・エラー契約・実行結果は同関数と同一で、
    /// 出口でだけ SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を利用者向けの
    /// `<schema>.<name>` へ戻す（TABLE-27・Issue #1717、ADR D2。内部 ID を文言へ出さない）。
    pub fn execute_sql_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        sql: &str,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        Self::check_resolution_tenant(session, ctx)?;
        self.execute_sql_in_txn_unscrubbed(ctx, session, txn, sql)
            .map_err(|e| self.scrub_error(ctx, e))
            .map(|o| self.scrub_outcome(ctx, o))
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
    ///   経てから、明示トランザクション内で対応する書き込み系（`INSERT`・
    ///   `UPDATE`・`DELETE`・`TRUNCATE`。全 DML の `RETURNING` に対応。
    ///   Issue #1272・#1273。ファイル形 `INSERT` は Issue #1353）・読み取り系
    ///   （`SELECT`／`Aggregate`／`Scan`／`EXPLAIN`／`USING PLAN`。書き込み済み
    ///   テーブルは未 commit の変更を反映する）文のみを実行する。対応外の文
    ///   （COPY・DDL 等）は `0A000` で拒否する。文の実行中にエラーが起きた場合は必ずトランザクションを
    ///   `Failed` へ遷移させ（[`crate::sql::transaction::SessionTransaction::
    ///   fail`]）、元のエラーをそのまま返す（部分書き込みを残さない
    ///   fail-closed 契約。`docs/design/explicit-transaction.md` 参照）。
    fn execute_parsed_in_txn_inner<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        parsed: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::transaction::{TransactionStatus, TxnControl};

        // 永続側の文を包んだ形（世代の検査は公開入口の粗いゲートが済ませている）は中身を実行する。
        if let ParsedSql::SessionRouted(routed) = parsed {
            if routed.is_persistent() {
                return self.execute_parsed_in_txn_inner(ctx, session, txn, routed.inner());
            }
        }
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
            TransactionStatus::Idle => self.execute_parsed_in_session_inner(ctx, session, parsed),
            TransactionStatus::Failed => Err(txn.take_failed_error()),
            TransactionStatus::InTransaction => {
                // Issue #1129: 分割実行（`PARTITIONED` 付き `UPDATE`／`DELETE`・`CANCEL`）は
                // 黙って原子実行へ切り替えず `25001` で拒否する（トランザクションは `Failed`）。
                if is_partitioned_write(parsed) {
                    txn.fail();
                    return Err(
                        crate::sql::allowlist::SqlSurfaceError::PartitionedDmlInTransaction,
                    );
                }
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

        // SQL-37（Issue #1586）: 読み取り文は文の開始時刻とトランザクション開始時刻を確定する
        // （`Active` なのに開始時刻が無い場合は fail-closed の `Internal`）。
        let _clock_scope = statement_clock_scope(
            parsed,
            Some(
                txn.txn_start_micros()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?,
            ),
        )?;
        // Issue #2185: `jsonb_path_*` の束縛が起動時 CLI の評価上限を焼き込めるよう、文の束縛区間に
        // 限って `jsonpath_limits` を現スレッドへ張る（`ClockScope` と同じ RAII。大域状態ではない）。
        let _jsonpath_limits_scope =
            crate::sql::jsonpath_fn::JsonPathLimitsScope::enter(self.jsonpath_limits);
        // Issue #1787: `~` 系の束縛が起動時 CLI の正規表現上限を焼き込めるよう同じ区間に張る。
        let _regex_limits_scope = crate::sql::regex_op::RegexLimitsScope::enter(self.regex_limits);
        // TABLE-33・Issue #1575: TZ 無しの TIMESTAMPTZ リテラル束縛・DDL 既定値の正規化が読む
        // セッション TimeZone を文ごとに張る（`SET TIME ZONE; INSERT` の後続文が新しい値を見る）。
        let _session_tz_scope = session_time_zone_scope(session);

        // SQL-37・Issue #1587: INSERT の列 `DEFAULT now()` 系はトランザクション開始時刻で評価する。
        let _default_clock_scope = insert_default_clock_scope(
            parsed,
            Some(
                txn.txn_start_micros()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?,
            ),
        )?;
        // TABLE-24・Issue #1690: INSERT の `nextval`／`currval`（VALUES・列 DEFAULT）の採番文脈。
        let _sequence_scope = insert_sequence_scope(&self.storage, session, parsed);

        // 注意（Issue #1182・#1272）: 明示トランザクション内の `RETURNING` は `INSERT`
        // （単一行・複数行）・`DELETE`・`UPDATE`（単一行・述語形）・UPSERT に対応する
        // （#1273）。DML 腕を追加・変更する際も `RETURNING` を黙って落とさない分岐を必ず持たせること。
        match parsed {
            // Issue #1179: 単一行・複数行 `VALUES`・`ON CONFLICT`（UPSERT）を
            // 受理する（`RETURNING` は単一行・複数行・UPSERT とも受理。Issue #1273。
            // ファイル形は `RETURNING` なしなら受理（Issue #1353。埋め込み I/O は保持済みの
            // 単一ライタの下で行う）、ありなら `42601`（`execute_insert_returning_form`））。
            // autocommit と同じ
            // フォーム関数（INDEX-4 上限・束縛・台帳順序を共有。第 2 の書き込み
            // 経路を作らない）へ `InTxn` を渡す。
            ParsedSql::Insert(stmt) => {
                let lookup = InsertSchemaLookup::new(&self.storage);
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let target = crate::tenant::WriteTarget::InTxn(write_txn);
                // AFTER ROW トリガが書いた表も COMMIT 時の遅延 FK 検査の対象にする（Issue #1724）。
                let trigger_scope =
                    crate::tenant::trigger_writes::TriggerWrittenTablesScope::enter();
                let outcome = if stmt.returning.is_some() {
                    crate::sql::SqlOutcome::Returning(
                        self.execute_insert_returning_form(target, ctx, stmt, &lookup)?,
                    )
                } else {
                    crate::sql::SqlOutcome::Insert(
                        self.execute_insert_form(target, ctx, stmt, &lookup)?,
                    )
                };
                txn.mark_written(ctx.tenant_id(), &stmt.table_name);
                for table in trigger_scope.take() {
                    txn.mark_written(ctx.tenant_id(), &table);
                }
                Ok(outcome)
            }
            // ビューへの INSERT（TABLE-43・Issue #1671）。基表形へ書き換えて同じフォーム関数へ
            // `InTxn` を渡す。台帳・`mark_written` は基表の単位（ビュー名ではない）。
            ParsedSql::ViewInsert(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let (outcome, base_table) = self.execute_view_insert(
                    || crate::tenant::WriteTarget::InTxn(write_txn),
                    ctx,
                    stmt,
                )?;
                txn.mark_written(ctx.tenant_id(), &base_table);
                Ok(outcome)
            }
            // ビューへの DELETE／UPDATE（TABLE-43・Issue #1672）。INSERT と同じく台帳・`mark_written` は
            // 基表の単位（ビュー名ではない）。
            ParsedSql::ViewDelete(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let (outcome, base_table) = self.execute_view_delete(
                    || crate::tenant::WriteTarget::InTxn(write_txn),
                    ctx,
                    session,
                    stmt,
                )?;
                txn.mark_written(ctx.tenant_id(), &base_table);
                Ok(outcome)
            }
            ParsedSql::ViewUpdate(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let (outcome, base_table) = self.execute_view_update(
                    || crate::tenant::WriteTarget::InTxn(write_txn),
                    ctx,
                    session,
                    stmt,
                )?;
                txn.mark_written(ctx.tenant_id(), &base_table);
                Ok(outcome)
            }
            ParsedSql::Delete(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let target = crate::tenant::WriteTarget::InTxn(write_txn);
                // トリガ（BEFORE・AFTER）が書いた表も COMMIT 時の遅延 FK 検査の対象にする（Issue #1725）。
                let trigger_scope =
                    crate::tenant::trigger_writes::TriggerWrittenTablesScope::enter();
                let (outcome, table) = match stmt {
                    crate::sql::allowlist::DeleteStatement::SingleRow(v) => {
                        let outcome = if v.returning.is_some() {
                            crate::sql::SqlOutcome::Returning(
                                self.execute_delete_returning_form(target, ctx, v)?,
                            )
                        } else {
                            crate::sql::SqlOutcome::Delete(
                                self.execute_delete_form(target, ctx, v)?,
                            )
                        };
                        (outcome, v.table_name.as_str())
                    }
                    // Issue #1272: 述語形 `RETURNING` も autocommit と同じフォーム関数へ
                    // `InTxn` を渡して受理する（`RETURNING` を黙って落とさない）。
                    crate::sql::allowlist::DeleteStatement::Predicate(v) => {
                        let outcome = if v.returning().is_some() {
                            crate::sql::SqlOutcome::Returning(
                                self.execute_predicate_delete_returning_form(
                                    target, ctx, session, v,
                                )?,
                            )
                        } else {
                            crate::sql::SqlOutcome::Delete(
                                self.execute_predicate_delete_form(target, ctx, session, v)?,
                            )
                        };
                        (outcome, v.table_name())
                    }
                };
                txn.mark_written(ctx.tenant_id(), table);
                for written in trigger_scope.take() {
                    txn.mark_written(ctx.tenant_id(), &written);
                }
                Ok(outcome)
            }
            ParsedSql::Update(stmt) => {
                // Issue #1272: `UPDATE ... RETURNING`（単一行・述語形）も autocommit と
                // 同じフォーム関数へ `InTxn` を渡して受理する（SQL-31・SQL-21）。
                let has_returning = match stmt {
                    crate::sql::allowlist::ValidatedUpdateForm::Single(v) => v.returning.is_some(),
                    crate::sql::allowlist::ValidatedUpdateForm::Predicate(v) => {
                        v.returning().is_some()
                    }
                };
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let target = crate::tenant::WriteTarget::InTxn(write_txn);
                // トリガ（BEFORE・AFTER）が書いた表も COMMIT 時の遅延 FK 検査の対象にする（Issue #1725）。
                let trigger_scope =
                    crate::tenant::trigger_writes::TriggerWrittenTablesScope::enter();
                let outcome = if has_returning {
                    crate::sql::SqlOutcome::Returning(
                        self.execute_update_returning_form(target, ctx, session, stmt)?,
                    )
                } else {
                    crate::sql::SqlOutcome::Update(
                        self.execute_predicate_update_form(target, ctx, session, stmt)?,
                    )
                };
                let table = match stmt {
                    crate::sql::allowlist::ValidatedUpdateForm::Single(v) => v.table_name.as_str(),
                    crate::sql::allowlist::ValidatedUpdateForm::Predicate(v) => v.table_name(),
                };
                txn.mark_written(ctx.tenant_id(), table);
                for written in trigger_scope.take() {
                    txn.mark_written(ctx.tenant_id(), &written);
                }
                Ok(outcome)
            }
            ParsedSql::Truncate(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                // TRUNCATE トリガが書いた表も COMMIT 時の遅延 FK 検査の対象にする（Issue #1726）。
                let trigger_scope =
                    crate::tenant::trigger_writes::TriggerWrittenTablesScope::enter();
                let outcome = crate::sql::exec::execute_truncate_in(
                    crate::tenant::WriteTarget::InTxn(write_txn),
                    ctx,
                    stmt,
                    self.ledger_mode,
                    Some(&self.trigger_runtime),
                )?;
                // 全表に印を付ける（漏れると遅延 FK の COMMIT 時検査が取りこぼす。P0）。
                // CASCADE（Issue #1644）で連鎖して空にした表も書き込み済みとして扱う。
                for table in stmt.table_names.iter().chain(outcome.cascaded()) {
                    txn.mark_written(ctx.tenant_id(), table);
                }
                for table in trigger_scope.take() {
                    txn.mark_written(ctx.tenant_id(), &table);
                }
                Ok(crate::sql::SqlOutcome::Truncate(outcome))
            }
            ParsedSql::Statement(
                stmt @ (Statement::SetSearchMode { .. }
                | Statement::SetTimeZone { .. }
                | Statement::ShowTimeZone
                | Statement::SetSearchPath { .. }
                | Statement::ShowSearchPath),
            ) => self.execute_validated_in_session(ctx, session, stmt.clone()),
            // SQL-66（Issue #1920）: DDL 権限のあるセッションの `CREATE FUNCTION` は UDF カタログへ書く
            // 永続 DDL のため、ROLLBACK で取り消せない書き込みを明示トランザクションの中で行わない
            // （他の DDL と同じ `0A000`）。権限の無いセッションはセッション内だけの登録で、ROLLBACK で
            // `session_at_begin` へ戻るため従来どおり通す。
            ParsedSql::Statement(stmt @ Statement::CreateFunction { .. }) => {
                if session.ddl_allowed() {
                    Err(SqlSurfaceError::feature_not_supported(
                        "CREATE [OR REPLACE] FUNCTION is not supported inside a transaction block",
                    ))
                } else {
                    self.execute_validated_in_session(ctx, session, stmt.clone())
                }
            }
            ParsedSql::DropFunction(_) if session.ddl_allowed() => {
                Err(SqlSurfaceError::feature_not_supported(
                    "DROP FUNCTION is not supported inside a transaction block",
                ))
            }
            ParsedSql::DropFunction(stmt) => {
                let outcome = crate::sql::udf_ddl::execute_drop_function(
                    &self.storage,
                    ctx.tenant_id(),
                    session,
                    stmt,
                )?;
                Ok(crate::sql::SqlOutcome::DropFunction(outcome))
            }
            // 検索・集計・広域取得・`EXPLAIN`・JOIN・評価後射影形ビュー・集合演算はすべて
            // 同じ読み取り経路へ入る（Issue #1179・#1192・#1353）。dirty テーブルの有無・
            // `USING PLAN`／`EXPLAIN` の読み取り源の切り替えは `read_only_in_active_txn`
            // が文全体に対して一括で判定する（文ごとの対象テーブルは判定に使わない）。
            ParsedSql::Statement(
                stmt @ (Statement::Select(_)
                | Statement::Aggregate(_)
                | Statement::Scan(_)
                | Statement::Explain(_)
                | Statement::Join(_)
                | Statement::BufferedView(_)
                | Statement::ConstSelect(_)
                | Statement::SetOperation(_)),
            ) => self.read_only_in_active_txn(ctx, session, txn, stmt.clone()),
            // WIRE-15・TASK-218: カーソルは `Active` なトランザクション内でのみ
            // 意味を持つ（`sql::cursor` モジュールドキュメント参照）。
            // Issue #1129: `SHOW`・`EXPLAIN` は書き込みも gate も伴わないためトランザクション内でも
            // autocommit と同じ処理で許可する。`CANCEL` は独立に commit し自セッションの writer と
            // 衝突するため拒否する（`execute_parsed_in_txn` が先に拒否済みの多層防御）。
            ParsedSql::Partitioned(control) if control.writes() => {
                Err(SqlSurfaceError::PartitionedDmlInTransaction)
            }
            ParsedSql::Partitioned(control) => {
                self.execute_partitioned_control(ctx, session, control)
            }
            ParsedSql::Cursor(stmt) => self.execute_cursor_in_active_txn(ctx, session, txn, stmt),
            // TABLE-44・Issue #1684: 一時表の文は、一時側の書き込みトランザクション（最初の一時表の
            // 文で開く）の上で一時コアが実行する。永続側の write txn・dirty 判定には触れない。
            ParsedSql::SessionRouted(routed) if routed.is_persistent() => {
                self.execute_in_active_txn(ctx, session, txn, routed.inner())
            }
            ParsedSql::SessionRouted(routed) => {
                self.execute_temp_routed_in_txn(ctx, session, txn, routed)
            }
            // TABLE-24・Issue #1691: 共有 write txn へ状態行を書く（commit は COMMIT 時）。表を書かないため
            // `mark_written` の相手がなく、`has_writes` だけを立てる（立てないと COMMIT が write txn を
            // 破棄して setval の永続化が落ちる）。`lastval` は読み取りのみで印を付けない。
            ParsedSql::SequenceFunction(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let (result, wrote) = crate::sql::sequence_fn::execute_in_txn(
                    &self.storage,
                    write_txn,
                    session,
                    ctx.tenant_id(),
                    stmt,
                )?;
                if wrote {
                    txn.mark_sequence_written();
                }
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // SQL-53 (b)・Issue #1754: 書き込みトランザクションに触れず、束縛の後に `0A000` で止める。
            // 呼び出し元が明示トランザクションを `Failed` へ遷移させる（他の失敗文と同じ契約）。
            ParsedSql::UpdateFrom(stmt) => self.execute_update_from_stub(stmt),
            ParsedSql::DeleteUsing(stmt) => self.execute_delete_using_stub(stmt),
            // SQL-51・Issue #1732・#1733: autocommit と同じ実行本体へ共有 write txn を渡す
            // （読み取り源にも書き込み先にもなる）。台帳・世代の単位は挿入先の表。
            // SQL-63・Issue #1876: 全副文・台帳記録・主クエリを共有 write txn で実行する。書いた全表へ
            // `mark_written` する（漏れると遅延 FK の COMMIT 時検査が取りこぼす。P0）。
            ParsedSql::DataModifyingCte(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                let (outcome, written) =
                    self.execute_data_modifying_cte(write_txn, ctx, session, stmt)?;
                for table in &written {
                    txn.mark_written(ctx.tenant_id(), table);
                }
                Ok(outcome)
            }
            ParsedSql::InsertSelect(stmt) => {
                let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                // AFTER ROW トリガが書いた表も COMMIT 時の遅延 FK 検査の対象にする（Issue #1724）。
                let trigger_scope =
                    crate::tenant::trigger_writes::TriggerWrittenTablesScope::enter();
                let outcome = self.execute_parsed_insert_select(
                    crate::core::insert_select_exec::InsertSelectTxn::InTxn(write_txn),
                    ctx,
                    session,
                    stmt,
                )?;
                txn.mark_written(ctx.tenant_id(), &stmt.table_name);
                for table in trigger_scope.take() {
                    txn.mark_written(ctx.tenant_id(), &table);
                }
                Ok(outcome)
            }
            // 拡張クエリの `COPY` は wire-server が先に横取りする（Issue #1760）ため通常は到達しない。
            // 他の呼び出し元への防御的な fail-closed（副作用なし。呼び出し元がトランザクションを
            // `Failed` へ遷移させる）。
            ParsedSql::Copy(_) => Err(SqlSurfaceError::feature_not_supported(
                "COPY via the extended query protocol is not supported yet",
            )),
            // COPY・DDL 等、明示トランザクション内での対応外の文（対象外。
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
    fn execute_cursor_inner_query<R: crate::storage::read_source::ReadSource>(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        inner: &crate::sql::allowlist::Statement,
        max_result_bytes: usize,
        read_txn: &R,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        if let crate::sql::allowlist::Statement::Scan(validated) = inner {
            let schema = table_schema_in(read_txn, &validated.table_name)?;
            let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                validated,
                &schema,
                session.udfs(),
                &[],
                &self.result_limits,
            )?;
            let result = crate::sql::scan::execute_scan_with_budget(
                read_txn,
                ctx,
                &schema,
                &bound,
                max_result_bytes,
            )?;
            return Ok(crate::sql::SqlOutcome::Query(result));
        }
        if let crate::sql::allowlist::Statement::Aggregate(validated) = inner {
            let schema = table_schema_in(read_txn, &validated.table_name)?;
            let bound = crate::sql::parser::bind_aggregate(validated, &schema, session.udfs())?
                .with_result_cap(self.result_limits.max_result_rows());
            let result =
                self.run_aggregate_plan(read_txn, ctx, &schema, &bound, max_result_bytes)?;
            let result = Self::enforce_group_result_cap(validated, result, &self.result_limits)?;
            return Ok(crate::sql::SqlOutcome::Query(result));
        }
        self.execute_read_statement(ctx, session, inner.clone(), read_txn)
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
                let (inner, _table) = crate::sql::cursor::validate_declare_inner(
                    query,
                    &self.storage,
                    self.nesting_limits,
                )?;
                let registry = txn.cursors_mut().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
                registry.ensure_capacity_for_declare(name)?;
                // 既存カーソルの保持量を差し引いた残容量を内側 SELECT の生成予算に
                // する（PR #1049 レビュー指摘 codex P1 対応。
                // `execute_cursor_inner_query` のドキュメント参照）。
                let remaining_bytes = registry.remaining_bytes();
                // 自トランザクションが未 commit の変更を持つ間（dirty テーブルあり）は
                // 共有書き込みトランザクションを読み取り源にして内側 SELECT を実行し、
                // 未 commit の変更を反映する（Issue #1179。キャッシュは使われない）。
                // 変更がなければ従来どおり確定済みスナップショットで実行する。
                let outcome = if txn.dirty_tables()?.is_empty() {
                    let read_txn = self.begin_read_txn()?;
                    self.execute_cursor_inner_query(
                        ctx,
                        session,
                        &inner,
                        remaining_bytes,
                        &read_txn,
                    )?
                } else {
                    let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?;
                    self.execute_cursor_inner_query(
                        ctx,
                        session,
                        &inner,
                        remaining_bytes,
                        write_txn,
                    )?
                };
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
    ///
    /// 自トランザクションが未 commit の変更を持たない（dirty テーブルなし）間は、
    /// BEGIN 時点の確定済みスナップショットに対する通常の読み取り経路
    /// （[`Self::execute_validated_in_session`]。キャッシュ利用可。単一ライタにより
    /// スナップショットは BEGIN 時点と一致する）へ委譲する。変更を持つ間は、
    /// 共有書き込みトランザクションを読み取り源（[`crate::storage::read_source::ReadSource`]）
    /// にして同じ実行本体（[`Self::execute_read_statement`]。RLS・束縛・実行は
    /// autocommit と共通）を呼び、自トランザクションの未 commit の変更を反映する
    /// （キャッシュは使われない brute-force 経路。Issue #1179）。dirty は文が直接
    /// 書き込んだテーブルに加え、参照アクションの連鎖で書き換わった子テーブルを含む
    /// （[`crate::sql::transaction::SessionTransaction::dirty_tables`]）。サブクエリ・
    /// JOIN・集合演算が別テーブルの未 commit 変更を読む場合も、文全体が同じ読み取り源
    /// を使うため反映される。
    ///
    /// `USING PLAN` を伴う検索 SELECT と `EXPLAIN` も dirty のとき受け付ける（Issue #1353）。
    /// カタログ・スキーマ・テーブル世代は確定済みスナップショットから読み（トランザクション内は
    /// DDL 不可・単一ライタ保持のため BEGIN 以降不変）、辞書構築と SELECT 本体の行の読みだけを
    /// 共有 write トランザクションへ切り替える
    /// （[`Self::execute_validated_with_dict`]・[`Self::dictionary_from_write_txn`]）。
    /// 辞書は [`DictionaryCache`] に載せない。行を読まない `EXPLAIN`（`USING PLAN` なしの
    /// 3 variant）の出力はスキーマ・索引宣言・束縛結果だけで決まるため確定済みスナップショットの
    /// ままでよい。想定外の文種別は fail-closed に `Internal` で拒否する。
    fn read_only_in_active_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        txn: &crate::sql::transaction::SessionTransaction<'e>,
        stmt: crate::sql::allowlist::Statement,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::Statement;

        // SQL-43（Issue #1531）: 定数式 SELECT はテーブルを読まないため、dirty
        // テーブルの有無に関わらず autocommit と同じ実行本体へ直接委譲する。
        if matches!(stmt, Statement::ConstSelect(_)) {
            return self.execute_validated_in_session(ctx, session, stmt);
        }
        let dirty = txn.dirty_tables()?;
        if dirty.is_empty() {
            return self.execute_validated_in_session(ctx, session, stmt);
        }
        let write_txn =
            txn.write_txn()
                .ok_or_else(|| crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?;
        match stmt {
            Statement::Select(ref v) if v.using_plan().is_some() => {
                self.execute_validated_with_dict(ctx, session, stmt, Some(write_txn))
            }
            Statement::Explain(_) => {
                self.execute_validated_with_dict(ctx, session, stmt, Some(write_txn))
            }
            Statement::Select(_)
            | Statement::Aggregate(_)
            | Statement::Scan(_)
            | Statement::SetOperation(_)
            | Statement::Join(_)
            | Statement::BufferedView(_) => {
                self.execute_read_statement(ctx, session, stmt, write_txn)
            }
            Statement::SetSearchMode { .. }
            | Statement::SetTimeZone { .. }
            | Statement::ShowTimeZone
            | Statement::SetSearchPath { .. }
            | Statement::ShowSearchPath
            | Statement::CreateFunction { .. }
            | Statement::MaterializedView(_)
            | Statement::RecursiveCte(_)
            | Statement::ConstSelect(_) => Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "unexpected statement in transactional read path".to_string(),
            }),
        }
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
    fn describe_parsed_in_session_unscrubbed(
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
        Self::coarse_temp_gate(session, parsed)?;
        self.describe_parsed_in_session_impl(session, parsed, false, &[], &self.result_limits)
    }

    /// `describe_parsed_in_session_unscrubbed` の公開入口。判定順序・エラー契約・結果は同関数と同一で、出口でだけ
    /// エラー文言の内部キー（`#<id>.<name>`）をテナントの SCHEMA 名へ戻す（TABLE-27・Issue #1717、
    /// ADR D2）。
    pub fn describe_parsed_in_session(
        &self,
        session: &crate::sql::mode::SessionState,
        parsed: &ParsedSql,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        self.describe_parsed_in_session_unscrubbed(session, parsed)
            .map_err(|e| match session.resolution_tenant() {
                Some(t) => self.scrub_error_for_tenant(t, e),
                None => e,
            })
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
    fn describe_parsed_in_txn_unscrubbed(
        &self,
        session: &crate::sql::mode::SessionState,
        txn: &crate::sql::transaction::SessionTransaction<'_>,
        parsed: &ParsedSql,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        // 永続側の文を包んだ形（拡張クエリの Bind が一時カタログが空でないセッションで作る）の中身が
        // カーソル文なら、包みを外した文として扱う（世代の検査は先に済ませる）。
        let cursor = match parsed {
            ParsedSql::Cursor(stmt) => Some(stmt),
            ParsedSql::SessionRouted(routed) if routed.is_persistent() => {
                routed.require_current(session)?;
                match routed.inner() {
                    ParsedSql::Cursor(stmt) => Some(stmt),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(stmt) = cursor {
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

    /// `describe_parsed_in_txn_unscrubbed` の公開入口。判定順序・エラー契約・結果は同関数と同一で、出口でだけ
    /// エラー文言の内部キー（`#<id>.<name>`）をテナントの SCHEMA 名へ戻す（TABLE-27・Issue #1717、
    /// ADR D2）。
    pub fn describe_parsed_in_txn(
        &self,
        session: &crate::sql::mode::SessionState,
        txn: &crate::sql::transaction::SessionTransaction<'_>,
        parsed: &ParsedSql,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        self.describe_parsed_in_txn_unscrubbed(session, txn, parsed)
            .map_err(|e| match session.resolution_tenant() {
                Some(t) => self.scrub_error_for_tenant(t, e),
                None => e,
            })
    }

    /// `CREATE OR REPLACE VIEW`（TABLE-41 (2)・Issue #1660）の実行本体。`execute_sql_in_session` の
    /// `CreateView` 腕が DDL 権限ゲートと列別名の突合の後に呼ぶ。同名ビューがあれば、旧定義と新定義の
    /// 出力列（名前・型・順序）を [`Self::view_output_columns`] で同じ規則により導出して
    /// `sql::view_replace::check_replace_compatible`（非互換は `42P16`）に通し、通ったときだけ
    /// `Storage::create_or_replace_view_checked` が旧定義・参照スキーマ指紋との compare-and-swap で
    /// 置換する。自己参照・循環（`42P17`）は互換性検査より先に判定する。検査後に旧定義または参照先が
    /// 変わっていた場合は検査からやり直す（有界。超過は `55P03`）。同名がテーブル・索引なら
    /// カタログ側が `42809` を返す。
    fn create_or_replace_view_in_session(
        &self,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedCreateView,
    ) -> Result<crate::sql::ddl::CreateViewOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::catalog::CatalogError;
        use crate::sql::allowlist::SqlSurfaceError;
        // 検査と置換の間の競合による再試行の上限（無制限ループにしない）。
        const MAX_ATTEMPTS: usize = 3;
        for _ in 0..MAX_ATTEMPTS {
            let old = self.storage.view_definition(stmt.name()).map_err(|_| {
                SqlSurfaceError::Internal {
                    detail: "CREATE VIEW failed".to_string(),
                }
            })?;
            let mut fingerprint = None;
            let mut appends_columns = false;
            if let Some(old) = &old {
                // 自己参照・循環（`42P17`）は列互換性（`42P16`）より先に判定する。
                let reaches_self = self
                    .storage
                    .view_body_reaches_self(stmt.name(), stmt.body_sql())
                    .map_err(crate::sql::ddl::map_replace_view_error)?;
                if reaches_self {
                    return Err(crate::sql::ddl::map_replace_view_error(
                        CatalogError::ViewReferenceCycle(stmt.name().to_string()),
                    ));
                }
                // 互換性検査が前提にする参照スキーマの指紋は、列の導出より前に取る。
                // 置換時に同一 write txn 内の指紋と照合し、検査後の参照先変更を検出する。
                fingerprint = Some(
                    self.storage
                        .replace_view_fingerprint(
                            stmt.name(),
                            stmt.base_relation(),
                            stmt.body_sql(),
                            Some(old),
                        )
                        .map_err(crate::sql::ddl::map_replace_view_error)?,
                );
                let old_cols =
                    self.view_output_columns(session, &old.body_sql, &old.column_aliases)?;
                let new_cols =
                    self.view_output_columns(session, stmt.body_sql(), stmt.column_aliases())?;
                crate::sql::view_replace::check_replace_compatible(&old_cols, &new_cols)?;
                // 互換検査通過後の列数増は末尾追加。依存ビューの `*` 展開への影響を
                // カタログ層（同一 write txn）が保守的に検査する（Issue #1661）。
                appends_columns = new_cols.len() > old_cols.len();
            }
            match crate::sql::ddl::execute_create_or_replace_view(
                &self.storage,
                stmt,
                old.as_ref(),
                fingerprint.as_deref(),
                appends_columns,
            ) {
                Ok(outcome) => return Ok(outcome),
                Err(CatalogError::ViewDefinitionChanged(_)) => continue,
                Err(e) => return Err(crate::sql::ddl::map_replace_view_error(e)),
            }
        }
        Err(SqlSurfaceError::LockNotAvailable)
    }

    /// ビュー本文 `body_sql`（格納形）と列別名 `column_aliases` から、出力列（名前と静的な型）を
    /// 導出する。`CREATE OR REPLACE VIEW` の旧定義・新定義の両方に同じ関数を使い、比較の非対称を
    /// 作らない（Issue #1660）。本文は実行せず Describe で型を得る。単純形本文は
    /// `sql::view::body_output` の名前と Describe の名前が一致することを確認し、食い違えば
    /// fail-closed で `XX000`。`SELECT *` は参照時の展開結果（置換時点）で比較する。
    fn view_output_columns(
        &self,
        session: &crate::sql::mode::SessionState,
        body_sql: &str,
        column_aliases: &[String],
    ) -> Result<Vec<crate::sql::view_replace::ViewColumn>, crate::sql::allowlist::SqlSurfaceError>
    {
        use crate::sql::allowlist::SqlSurfaceError;
        use crate::sql::exec::ColumnMeta;
        use crate::sql::view_replace::ViewColumn;
        let invalid_body = || SqlSurfaceError::Internal {
            detail: "invalid view definition".to_string(),
        };
        let tokens =
            crate::sql::lexer::tokenize_stored_view_body(body_sql).map_err(|_| invalid_body())?;
        let kind = crate::sql::allowlist::classify_view_body(
            &tokens,
            &self.storage,
            crate::sql::allowlist::ViewBodyCheck::Resolved,
        )?;
        let (statement, expected_names) = match crate::sql::view::body_output(&self.storage, kind)?
        {
            crate::sql::view::BodyOutput::Statement(body) => (*body, None),
            // 単純形本文は `LIMIT` を持たず、読み取り文として検証するには `LIMIT` が要る。
            // Describe は本文を実行しないため、検証用に `LIMIT 1` を足した写しで型を得る。
            crate::sql::view::BodyOutput::Names(names) => {
                let mut limited = tokens.clone();
                limited.push(crate::sql::lexer::Token::Keyword(
                    crate::sql::lexer::Keyword::Limit,
                ));
                limited.push(crate::sql::lexer::Token::Number("1".to_string()));
                (
                    crate::sql::allowlist::validate_sql_tokens(&limited, &self.storage)?,
                    Some(names),
                )
            }
        };
        let metas = self
            .describe_parsed_in_session_impl(
                session,
                &ParsedSql::Statement(statement),
                false,
                &[],
                &crate::result_limits::ResultLimits::default(),
            )?
            .ok_or_else(invalid_body)?;
        let mut columns: Vec<ViewColumn> = metas
            .into_iter()
            .map(|m| match m {
                ColumnMeta::Id => ViewColumn {
                    name: "id".to_string(),
                    ty: Some(crate::sql::exec::ALIASED_ID_TYPE.clone()),
                },
                ColumnMeta::AliasedId { name } => ViewColumn {
                    name,
                    ty: Some(crate::sql::exec::ALIASED_ID_TYPE.clone()),
                },
                ColumnMeta::Scalar { name, ty } => ViewColumn { name, ty: Some(ty) },
                ColumnMeta::Computed { name, ty } => ViewColumn { name, ty },
                // 再帰 CTE の SEARCH 順序列（record／record[]）は表・ビューの列型にできない。型なしとして
                // 渡し、下流の「型なし列は 0A000」検査で拒否させる（fail-closed。SQL-35・Issue #2213）。
                ColumnMeta::Record { name, .. } => ViewColumn { name, ty: None },
            })
            .collect();
        if let Some(names) = expected_names {
            let described: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
            if described != names.iter().map(String::as_str).collect::<Vec<_>>() {
                return Err(invalid_body());
            }
        }
        if column_aliases.len() > columns.len() {
            return Err(SqlSurfaceError::unsupported(
                "view column alias list is longer than the view's output columns",
            ));
        }
        for (col, alias) in columns.iter_mut().zip(column_aliases.iter()) {
            col.name = alias.clone();
        }
        Ok(columns)
    }

    /// `CREATE VIEW v (a, b) AS ...`（TABLE-41 (1)・Issue #1659）の列別名リストを、本文の出力列に
    /// 突き合わせる。別名が出力列より多ければ `42601`、別名を重ねた後の最終的な列名（別名＋残りの
    /// 導出名）に重複があれば `42701`。列別名が無い `CREATE VIEW` は何もしない（従来の挙動を変えない）。
    /// `execute_sql_in_session` の `CreateView` 腕が DDL 権限ゲートの後に呼ぶ。本文の出力列は
    /// `sql::view::body_output` が参照時の別名適用と同じ規則で導出する。ここと `create_view` の
    /// write txn の間で基底表の列が変わる競合は、参照時の再検査（`42601`／`42702`）が fail-closed で塞ぐ。
    fn check_create_view_column_aliases(
        &self,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedCreateView,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::SqlSurfaceError;
        use crate::sql::exec::ColumnMeta;
        let aliases = stmt.column_aliases();
        if aliases.is_empty() {
            return Ok(());
        }
        let invalid_body = || SqlSurfaceError::Internal {
            detail: "invalid view definition".to_string(),
        };
        let tokens = crate::sql::lexer::tokenize_stored_view_body(stmt.body_sql())
            .map_err(|_| invalid_body())?;
        let kind = crate::sql::allowlist::classify_view_body(
            &tokens,
            &self.storage,
            crate::sql::allowlist::ViewBodyCheck::Resolved,
        )?;
        let names: Vec<String> = match crate::sql::view::body_output(&self.storage, kind)? {
            crate::sql::view::BodyOutput::Names(names) => names,
            crate::sql::view::BodyOutput::Statement(body) => {
                let metas = self
                    .describe_parsed_in_session_impl(
                        session,
                        &ParsedSql::Statement(*body),
                        false,
                        &[],
                        &crate::result_limits::ResultLimits::default(),
                    )?
                    .ok_or_else(invalid_body)?;
                metas
                    .into_iter()
                    .map(|m| match m {
                        ColumnMeta::Id => "id".to_string(),
                        ColumnMeta::AliasedId { name }
                        | ColumnMeta::Scalar { name, .. }
                        | ColumnMeta::Computed { name, .. }
                        | ColumnMeta::Record { name, .. } => name,
                    })
                    .collect()
            }
        };
        if aliases.len() > names.len() {
            return Err(SqlSurfaceError::unsupported(
                "view column alias list is longer than the view's output columns",
            ));
        }
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let finals = aliases
            .iter()
            .map(String::as_str)
            .chain(names.iter().skip(aliases.len()).map(String::as_str));
        for name in finals {
            if !seen.insert(name) {
                return Err(SqlSurfaceError::duplicate_column(name));
            }
        }
        Ok(())
    }

    /// 再帰 CTE（SQL-35・Issue #1870）の静的検証と結果列の導出（行は読まない）。アンカー項の結果列を
    /// 導出し、再帰項の束縛・列整合・型・照合の検査を [`crate::sql::recursive_eval::describe`] で行う。
    /// Describe の結果列導出と、Execute のアンカー実行前の事前検証が共有する唯一の実装で、
    /// 静的に決まるエラー（`42804`・`0A000` 等）がデータ依存の実行エラー（アンカー本文の除算エラー等）より
    /// 先に決まり、両経路で同じエラーになる。
    fn describe_recursive_cte(
        &self,
        session: &crate::sql::mode::SessionState,
        validated: &crate::sql::allowlist::ValidatedRecursiveCte,
        skip_vector_literal_validation: bool,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<Vec<crate::sql::exec::ColumnMeta>, crate::sql::allowlist::SqlSurfaceError> {
        // 定数アンカーの `current_schema()` 系は、通常の定数 SELECT が張る検索パスのスコープ外で実行
        // されるため未対応（Execute の実行本体の前提）。Describe と Execute が同じ `0A000` になるよう
        // 共有経路で拒否する。
        if let crate::sql::allowlist::Statement::ConstSelect(c) = &*validated.anchor {
            if crate::sql::const_select::uses_schema_fn(c) {
                return Err(
                    crate::sql::allowlist::SqlSurfaceError::feature_not_supported(
                        "current_schema() in the anchor term of WITH RECURSIVE is not supported",
                    ),
                );
            }
        }
        let anchor_parsed = ParsedSql::Statement((*validated.anchor).clone());
        let anchor_columns = self
            .describe_parsed_in_session_impl(
                session,
                &anchor_parsed,
                skip_vector_literal_validation,
                &[],
                limits,
            )?
            .ok_or_else(|| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        let mut table_names = Vec::new();
        recursive_cte_tables(validated, &mut table_names);
        let (_read_txn, schemas) = self.read_txn_with_schemas(&table_names)?;
        let join_limits = crate::sql::join::JoinLimits::from_settings(
            self.group_by_limits,
            self.join_limits,
            *limits,
        );
        crate::sql::recursive_eval::describe(
            validated,
            &anchor_columns,
            &schemas,
            session.udfs(),
            &join_limits,
        )
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
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, crate::sql::allowlist::SqlSurfaceError>
    {
        use crate::sql::allowlist::{DeleteStatement, Statement, ValidatedUpdateForm};

        // SQL-37（Issue #1586）: Describe も読み取り文の束縛を通る（値は型決定にしか使わない）。
        let _clock_scope = statement_clock_scope(parsed, None)?;
        // Issue #2185: `jsonb_path_*` の束縛が起動時 CLI の評価上限を焼き込めるよう、文の束縛区間に
        // 限って `jsonpath_limits` を現スレッドへ張る（`ClockScope` と同じ RAII。大域状態ではない）。
        let _jsonpath_limits_scope =
            crate::sql::jsonpath_fn::JsonPathLimitsScope::enter(self.jsonpath_limits);
        // Issue #1787: `~` 系の束縛が起動時 CLI の正規表現上限を焼き込めるよう同じ区間に張る。
        let _regex_limits_scope = crate::sql::regex_op::RegexLimitsScope::enter(self.regex_limits);
        // TABLE-33・Issue #1575: TZ 無しの TIMESTAMPTZ リテラル束縛・DDL 既定値の正規化が読む
        // セッション TimeZone を文ごとに張る（`SET TIME ZONE; INSERT` の後続文が新しい値を見る）。
        let _session_tz_scope = session_time_zone_scope(session);

        match parsed {
            // TABLE-44・Issue #1684: 一時表の文は一時コアが結果列を導出する。
            ParsedSql::SessionRouted(routed) if routed.is_persistent() => {
                routed.require_current(session)?;
                self.describe_parsed_in_session_impl(
                    session,
                    routed.inner(),
                    skip_vector_literal_validation,
                    dummy_equality_flags,
                    limits,
                )
            }
            ParsedSql::SessionRouted(routed) => self.describe_temp_routed(
                session,
                routed,
                skip_vector_literal_validation,
                dummy_equality_flags,
                limits,
            ),
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
            // SQL-63・Issue #1876: 書き込みは行わず、参照される副文の `RETURNING` 列に主クエリの外側処理を
            // 適用した列メタを返す（Execute と同じ `plan_outer_parts` を通るため列名・型・束縛エラーが一致）。
            ParsedSql::DataModifyingCte(stmt) => self
                .describe_data_modifying_cte(session, stmt, limits)
                .map(Some),
            // SQL-51・Issue #1732: 束縛（列数・型）を通した上で、`RETURNING` の列メタだけを返す。
            ParsedSql::InsertSelect(stmt) => {
                let bound = self.bind_insert_select(session, stmt, limits)?;
                match bound.returning {
                    Some(projection) => {
                        let (_read_txn, schema) = self.read_txn_with_schema(&stmt.table_name)?;
                        Ok(Some(crate::sql::returning::column_meta(
                            &projection,
                            &schema,
                        )?))
                    }
                    None => Ok(None),
                }
            }
            // ビューへの INSERT（TABLE-43・Issue #1671）: 現在のカタログで書き換え、`RETURNING` の
            // 列メタは基表の型で導き、列名だけビューの公開名へ上書きする。
            ParsedSql::ViewInsert(stmt) => {
                // 書き換えと基表スキーマは同一スナップショットから取る（A→B→A の置換で別の表を
                // 引かないため）。
                let (rewritten, schema) = self
                    .storage
                    .with_view_write_snapshot(stmt.view_name(), |snap, _| {
                        let rewritten = crate::sql::view_insert::rewrite_view_insert(snap, stmt)?;
                        let schema = snap.table_schema(&rewritten.insert.table_name).map_err(
                            |e| match e {
                                CatalogError::TableNotFound(name) => {
                                    crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                                }
                                other => crate::catalog::table_lookup_error(other),
                            },
                        )?;
                        Ok::<_, crate::sql::allowlist::SqlSurfaceError>((rewritten, schema))
                    })
                    .map_err(crate::catalog::table_lookup_error)??;
                match crate::sql::parser::bind_returning(
                    rewritten.insert.returning.as_ref(),
                    &schema,
                )? {
                    Some(projection) => {
                        let mut columns = crate::sql::returning::column_meta(&projection, &schema)?;
                        crate::sql::view_insert::apply_public_names(
                            &mut columns,
                            rewritten.public_names.as_deref(),
                        );
                        Ok(Some(columns))
                    }
                    None => Ok(None),
                }
            }
            // ビューへの DELETE／UPDATE（TABLE-43・Issue #1672）: 現在のカタログで書き換え、`RETURNING` の
            // 列メタは基表の型で導き、列名だけビューの公開名へ上書きする（INSERT と同じ）。
            ParsedSql::ViewDelete(stmt) => {
                let (rewritten, schema) = self
                    .storage
                    .with_view_write_snapshot(stmt.view_name(), |snap, _| {
                        let rewritten = crate::sql::view_dml::rewrite_view_delete(snap, stmt)?;
                        let table = match &rewritten.delete {
                            DeleteStatement::SingleRow(v) => v.table_name.as_str(),
                            DeleteStatement::Predicate(v) => v.table_name(),
                        };
                        let schema = snap.table_schema(table).map_err(|e| match e {
                            CatalogError::TableNotFound(name) => {
                                crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                            }
                            other => crate::catalog::table_lookup_error(other),
                        })?;
                        Ok::<_, crate::sql::allowlist::SqlSurfaceError>((rewritten, schema))
                    })
                    .map_err(crate::catalog::table_lookup_error)??;
                let returning = match &rewritten.delete {
                    DeleteStatement::SingleRow(v) => v.returning.as_ref(),
                    DeleteStatement::Predicate(v) => v.returning(),
                };
                match crate::sql::parser::bind_returning(returning, &schema)? {
                    Some(projection) => {
                        let mut columns = crate::sql::returning::column_meta(&projection, &schema)?;
                        crate::sql::view_insert::apply_public_names(
                            &mut columns,
                            rewritten.public_names.as_deref(),
                        );
                        Ok(Some(columns))
                    }
                    None => Ok(None),
                }
            }
            ParsedSql::ViewUpdate(stmt) => {
                let (rewritten, schema) = self
                    .storage
                    .with_view_write_snapshot(stmt.view_name(), |snap, _| {
                        let rewritten = crate::sql::view_dml::rewrite_view_update(snap, stmt)?;
                        let table = match &rewritten.update {
                            ValidatedUpdateForm::Single(v) => v.table_name.as_str(),
                            ValidatedUpdateForm::Predicate(v) => v.table_name(),
                        };
                        let schema = snap.table_schema(table).map_err(|e| match e {
                            CatalogError::TableNotFound(name) => {
                                crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
                            }
                            other => crate::catalog::table_lookup_error(other),
                        })?;
                        Ok::<_, crate::sql::allowlist::SqlSurfaceError>((rewritten, schema))
                    })
                    .map_err(crate::catalog::table_lookup_error)??;
                let returning = match &rewritten.update {
                    ValidatedUpdateForm::Single(v) => v.returning.as_ref(),
                    ValidatedUpdateForm::Predicate(v) => v.returning(),
                };
                match crate::sql::parser::bind_returning(returning, &schema)? {
                    Some(projection) => {
                        let mut columns = crate::sql::returning::column_meta(&projection, &schema)?;
                        crate::sql::view_insert::apply_public_names(
                            &mut columns,
                            rewritten.public_names.as_deref(),
                        );
                        Ok(Some(columns))
                    }
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
            ParsedSql::CreateTable(_) | ParsedSql::CreateTempTable(_) => Ok(None),
            // SQL-52・Issue #1678: CTAS も DDL（`CommandComplete` のみ）で結果列を持たない（Describe は実行しない）。
            ParsedSql::CreateTableAs(_) => Ok(None),
            // `ALTER TABLE ADD COLUMN`（TASK-202・SQL-23。Issue #900）も
            // `CommandComplete` のみを返す DDL のため結果列を持たない。DDL 実行
            // 権限判定・カタログへの反映は一切行わない（Describe は本体を実行
            // しない契約。テーブル・列の存在有無も確認しない）。
            ParsedSql::AlterTable(_) => Ok(None),
            // TABLE-18・SQL-23・TASK-205（Issue #909）: DDL につき結果列を持たない
            // （`DropTable` と同じ扱い）。
            ParsedSql::CreateView(_) => Ok(None),
            // TABLE-42・Issue #1664: DDL につき結果列を持たない（Describe は本体を実行しない）。
            ParsedSql::CreateMaterializedView(_) => Ok(None),
            // TABLE-42・Issue #1665: REFRESH も結果列を持たない。
            ParsedSql::RefreshMaterializedView(_) => Ok(None),
            ParsedSql::DropView(_) => Ok(None),
            ParsedSql::DropMaterializedView(_) => Ok(None),
            // TASK-206・INDEX-7（Issue #908）: 索引 DDL も結果列を持たない。
            ParsedSql::CreateIndex(_) => Ok(None),
            ParsedSql::DropIndex(_) => Ok(None),
            // Issue #1194: 型 DDL も結果列を持たない。
            ParsedSql::CreateType(_) => Ok(None),
            ParsedSql::DropType(_) => Ok(None),
            // Issue #1688: SEQUENCE DDL も結果列を持たない。
            ParsedSql::CreateSequence(_) => Ok(None),
            ParsedSql::DropSequence(_) => Ok(None),
            ParsedSql::DropFunction(_) => Ok(None),
            ParsedSql::CreateTrigger(_) => Ok(None),
            ParsedSql::DropTrigger(_) => Ok(None),
            ParsedSql::CreateSchema(_) => Ok(None),
            ParsedSql::DropSchema(_) => Ok(None),
            ParsedSql::AlterSchema(_) => Ok(None),
            ParsedSql::AlterSequence(_) => Ok(None),
            // Issue #1691: 結果列（1 列・bigint）だけを返し、`setval` の書き込みは実行しない。
            ParsedSql::SequenceFunction(stmt) => Ok(Some(crate::sql::sequence_fn::columns(stmt))),
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
            // 述語形 `DELETE ... WHERE`・`UPDATE`（単一行・述語形）も Issue #1182
            // で `RETURNING` を実行結線済みのため、`INSERT`・単一行 `DELETE` と
            // 同じ規則で結果列を返す（`RETURNING` なしは `None`）。
            ParsedSql::Delete(DeleteStatement::Predicate(v)) => {
                let (_read_txn, schema) = self.read_txn_with_schema(v.table_name())?;
                match crate::sql::parser::bind_returning(v.returning(), &schema)? {
                    Some(projection) => Ok(Some(crate::sql::returning::column_meta(
                        &projection,
                        &schema,
                    )?)),
                    None => Ok(None),
                }
            }
            ParsedSql::Update(ValidatedUpdateForm::Single(v)) => {
                let (_read_txn, schema) = self.read_txn_with_schema(&v.table_name)?;
                match crate::sql::parser::bind_returning(v.returning.as_ref(), &schema)? {
                    Some(projection) => Ok(Some(crate::sql::returning::column_meta(
                        &projection,
                        &schema,
                    )?)),
                    None => Ok(None),
                }
            }
            ParsedSql::Update(ValidatedUpdateForm::Predicate(v)) => {
                let (_read_txn, schema) = self.read_txn_with_schema(v.table_name())?;
                match crate::sql::parser::bind_returning(v.returning(), &schema)? {
                    Some(projection) => Ok(Some(crate::sql::returning::column_meta(
                        &projection,
                        &schema,
                    )?)),
                    None => Ok(None),
                }
            }
            // SQL-53 (b)・Issue #1754: 束縛エラーは Execute と同じコードで返す。`RETURNING` は未対応
            // （Parse で `0A000`）のため結果列は無い。
            ParsedSql::UpdateFrom(stmt) => {
                self.bind_update_from_stmt(stmt)?;
                Ok(None)
            }
            ParsedSql::DeleteUsing(stmt) => {
                self.bind_delete_using_stmt(stmt)?;
                Ok(None)
            }
            // Issue #1129: `SHOW`／`CANCEL` は `status`・`rows` の 2 列、`EXPLAIN` は
            // `QUERY PLAN` 単一列（いずれも入力文の内容によらず固定のため実行しない）。
            ParsedSql::Partitioned(control) => {
                Ok(Some(crate::sql::partitioned::describe_columns(control)))
            }
            ParsedSql::Statement(Statement::SetSearchMode { .. }) => Ok(None),
            ParsedSql::Statement(Statement::SetTimeZone { .. }) => Ok(None),
            // `SHOW TimeZone` は列名 `TimeZone`（text）の 1 列固定（TABLE-33・Issue #1575）。
            ParsedSql::Statement(Statement::ShowTimeZone) => Ok(Some(show_time_zone_columns())),
            // TABLE-27・Issue #1715: `SET search_path`／`RESET` は結果列なし、`SHOW search_path` は
            // 列名 `search_path`（text）の 1 列固定。
            ParsedSql::Statement(Statement::SetSearchPath { .. }) => Ok(None),
            ParsedSql::Statement(Statement::ShowSearchPath) => Ok(Some(show_search_path_columns())),
            ParsedSql::Statement(Statement::CreateFunction { .. }) => Ok(None),
            // `EXPLAIN` は常に単一の `Computed` 列（`QUERY PLAN`）を返す
            // （`sql::explain::build_explain_result` と同一の列。中身の生成には
            // 検索本体の実行が必要だが、列自体は入力文の内容によらず固定のため
            // Describe はプラン実行を一切行わずに列だけを返せる）。
            ParsedSql::Statement(Statement::Explain(_)) => {
                Ok(Some(vec![crate::sql::exec::ColumnMeta::Computed {
                    name: crate::sql::explain::QUERY_PLAN_COLUMN.to_string(),
                    ty: Some(crate::catalog::ColumnType::Text),
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
                    &bound, &schema,
                )?))
            }
            ParsedSql::Statement(Statement::Scan(validated)) => {
                let (_read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
                let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                    validated,
                    &schema,
                    session.udfs(),
                    dummy_equality_flags,
                    limits,
                )?;
                // SQL-30・TASK-214（Issue #930）: ウィンドウ項目を含む広域取得の
                // Describe 列は `scan_columns` が実行時（`sql::window::
                // execute_window_scan`）と同じ写像で組み立てる。
                Ok(Some(crate::sql::describe::scan_columns(&bound, &schema)?))
            }
            // Issue #929（SQL-29 (c)・TASK-213）: 検索本体（各枝の行走査）は実行せず、
            // 全枝を束縛して型整合検証（§2.2）を行った結果列（左端の枝の列）だけを
            // 返す（Describe は本体を実行しない契約。`Statement::Aggregate`／
            // `Statement::Scan` アームと同じ方針）。
            ParsedSql::Statement(Statement::SetOperation(validated)) => {
                let mut table_names = Vec::new();
                crate::sql::set_op::collect_branch_tables(&validated.tree, &mut table_names);
                let (_read_txn, schemas) = self.read_txn_with_schemas(&table_names)?;
                let columns = crate::sql::set_op::describe_columns(
                    &schemas,
                    &validated.tree,
                    session.udfs(),
                    dummy_equality_flags,
                    validated.limit,
                    validated.offset,
                    limits,
                )?;
                Ok(Some(columns))
            }
            // Issue #925（SQL-28・RLS-10、TASK-212）: 検索本体（両辺の行走査・
            // ハッシュ結合）は実行せず、束縛結果の列（`sql::join::bind_join`）
            // だけを返す（Describe は本体を実行しない契約。`Statement::SetOperation`
            // アームと同じ方針）。
            ParsedSql::Statement(Statement::Join(validated)) => {
                let table_names = crate::sql::join::collect_join_tables(validated);
                let (_read_txn, mut schemas) = self.read_txn_with_schemas(&table_names)?;
                let mut cte_ids = crate::sql::join::CteIdColumns::new();
                // SQL-62・Issue #1861: CTE 入力は本文を実行せず、結果列だけを導出して Execute と同じ
                // 関数で合成スキーマにする（Describe は本体を実行しない契約）。
                if let Some(cte) = validated.cte_inputs.as_deref() {
                    for (rel, slot) in validated.relations.iter().zip(cte.relation_slot.iter()) {
                        let Some(k) = slot else { continue };
                        let cols = self.describe_cte_slot(
                            session,
                            cte,
                            *k,
                            skip_vector_literal_validation,
                            limits,
                        )?;
                        let (schema, id_columns) =
                            crate::sql::join::cte_schema(rel.table(), &cols)?;
                        schemas.insert(rel.table().to_string(), schema);
                        cte_ids.insert(rel.table().to_string(), id_columns);
                    }
                }
                let columns = crate::sql::join::describe_columns(
                    &schemas,
                    &cte_ids,
                    validated,
                    session.udfs(),
                    limits,
                )?;
                Ok(Some(columns))
            }
            // TABLE-42・Issue #1664: matview の葉。列は参照時に束縛した定義の列で、実体の有無は
            // Describe では判定しない（本体を実行しない契約）。
            ParsedSql::Statement(Statement::MaterializedView(v)) => {
                Ok(Some(crate::sql::matview::describe_columns(v)))
            }
            // SQL-35・Issue #1870: 再帰 CTE。アンカー項の結果列を（本文は実行せずに）導出し、
            // 再帰項の束縛・列整合の検査だけを行う（Execute と同じ `recursive_eval` の計画を通る
            // ため、束縛エラーが両経路で一致する）。
            ParsedSql::Statement(Statement::RecursiveCte(validated)) => {
                let columns = self.describe_recursive_cte(
                    session,
                    validated,
                    skip_vector_literal_validation,
                    limits,
                )?;
                Ok(Some(columns))
            }
            // Issue #1192（TABLE-18）: 評価後射影形ビュー。本文の結果列を
            // （本文は実行せずに）導出し、外側の射影だけを適用する。
            ParsedSql::Statement(Statement::BufferedView(validated)) => {
                Self::check_buffered_outer_limit(validated, limits)?;
                check_buffered_view_collations(&self.begin_read_txn()?, validated)?;
                let body_parsed = ParsedSql::Statement((*validated.body).clone());
                // 本文は永続化済みで `$n` を含まないため、外側の `$n` 置換に
                // 位置対応するダミーフラグは渡さない（本文の ENUM 検証を省略しない）。
                let body_columns = self.describe_parsed_in_session_impl(
                    session,
                    &body_parsed,
                    skip_vector_literal_validation,
                    &[],
                    // 本文の `LIMIT` は外側の設定値と連動させず独立した既定値で検証する
                    // （result-limits-cli.md の「内側 LIMIT は連動しない」）。具体化 CTE
                    // （Issue #1860）は実行側と同じく呼び出し段の `limits` を渡す。
                    &match validated.origin {
                        crate::sql::allowlist::BufferedOrigin::MaterializedCte
                        | crate::sql::allowlist::BufferedOrigin::GroupedWindow => *limits,
                        crate::sql::allowlist::BufferedOrigin::View => {
                            crate::result_limits::ResultLimits::default()
                        }
                    },
                )?;
                match body_columns {
                    Some(cols) => {
                        let plan = crate::sql::view_buffered::plan_outer(
                            &cols,
                            validated,
                            session.udfs(),
                        )?;
                        Ok(Some(plan.columns().to_vec()))
                    }
                    None => Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    }),
                }
            }
            // SQL-43（Issue #1531）: `FROM` なし定数式 SELECT。評価は行わず束縛結果の
            // 列だけを返す（ストレージにも触れない）。
            ParsedSql::Statement(Statement::ConstSelect(validated)) => {
                // `current_schema` 系は型だけ公告する（Describe は `PolicyContext` を持たず、
                // SCHEMA の照会をしない。TABLE-27 (7)・Issue #1718）。
                let _schema_fn = crate::sql::search_path::SchemaFnScope::enter(
                    crate::sql::search_path::SchemaFnMode::TypesOnly,
                );
                let bound = crate::sql::const_select::bind(validated, session.udfs())?;
                Ok(Some(bound.columns()))
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
                        limits,
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
            // Issue #1759: `COPY` は結果列を持たない（Describe は NoData。CopyIn/CopyOut の
            // 開始は Execute 側の責務で Issue #1760）。
            ParsedSql::Copy(_) => Ok(None),
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
        self.execute_validated_with_dict(ctx, session, stmt, None)
    }

    /// [`Self::execute_validated_in_session`] の本体。`dict_txn`（Issue #1353）が `Some` の
    /// とき、`USING PLAN` を伴う SELECT・`EXPLAIN ... USING PLAN` だけが辞書構築（と SELECT の
    /// 検索本体の行の読み）を共有 write トランザクションへ切り替える（明示トランザクション内で
    /// 自トランザクションの未 commit の変更を反映する。キャッシュは使わない）。それ以外の
    /// 文は `dict_txn` を参照しない。`None` は autocommit と同一の挙動。
    fn execute_validated_with_dict(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        stmt: crate::sql::allowlist::Statement,
        dict_txn: Option<&redb::WriteTransaction>,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // Issue #2185: 検証済み文の直接実行（`execute_sql` 等）でも `jsonb_path_*` の束縛が
        // 起動時の評価上限を使えるようにする（`execute_parsed_in_session` 等と同じ RAII）。
        let _jsonpath_limits_scope =
            crate::sql::jsonpath_fn::JsonPathLimitsScope::enter(self.jsonpath_limits);
        // Issue #1787: `~` 系の束縛が起動時 CLI の正規表現上限を焼き込めるよう同じ区間に張る。
        let _regex_limits_scope = crate::sql::regex_op::RegexLimitsScope::enter(self.regex_limits);
        // TABLE-33・Issue #1575: TZ 無しの TIMESTAMPTZ リテラル束縛・DDL 既定値の正規化が読む
        // セッション TimeZone を文ごとに張る（`SET TIME ZONE; INSERT` の後続文が新しい値を見る）。
        let _session_tz_scope = session_time_zone_scope(session);
        match stmt {
            crate::sql::allowlist::Statement::SetSearchMode { value } => {
                let mode = crate::sql::mode::SearchMode::parse_literal(&value)?;
                session.set_search_mode(mode);
                Ok(crate::sql::SqlOutcome::SetSearchMode(mode))
            }
            // TABLE-33・Issue #1575: 検証（解決）に成功した場合に限りセッションへ代入する
            // （失敗した `SET` は値を変えない）。`DEFAULT`／`LOCAL` は reset 値へ戻す。
            // 失敗は固定文言の `22023`（入力値を反射しない）。
            crate::sql::allowlist::Statement::SetTimeZone { value } => {
                match value {
                    crate::sql::allowlist::TimeZoneSetting::Default => session.reset_time_zone(),
                    crate::sql::allowlist::TimeZoneSetting::Value(text) => {
                        let tz = crate::datetime::SessionTimeZone::parse_setting(&text).map_err(
                            |_| crate::sql::allowlist::SqlSurfaceError::InvalidParameterValue {
                                detail: crate::datetime::INVALID_TIME_ZONE_DETAIL,
                            },
                        )?;
                        session.set_time_zone(tz);
                    }
                }
                Ok(crate::sql::SqlOutcome::SetTimeZone)
            }
            // TABLE-27・Issue #1715: 検証（要素数・要素長・`pg_temp` の順序）に成功した場合に限り
            // セッションへ代入する（失敗した `SET` は値を変えない）。`RESET`／`DEFAULT` は既定値へ戻す。
            // SCHEMA の存在はここでは検査しない（存在しない名前は解決時に飛ばす。ADR D5）。
            crate::sql::allowlist::Statement::SetSearchPath { value, reset } => {
                match value {
                    crate::sql::search_path::SearchPathSetting::Default => {
                        session.reset_search_path()?
                    }
                    crate::sql::search_path::SearchPathSetting::List(elems) => {
                        let path = crate::sql::search_path::SearchPath::new_for_user(
                            elems,
                            session.session_user(),
                        )?;
                        session.set_search_path(path)?;
                    }
                }
                Ok(crate::sql::SqlOutcome::SetSearchPath { reset })
            }
            crate::sql::allowlist::Statement::ShowSearchPath => Ok(crate::sql::SqlOutcome::Show(
                crate::sql::exec::QueryResult {
                    columns: show_search_path_columns(),
                    rows: vec![crate::sql::exec::ResultRow {
                        id: 0,
                        score: 0.0,
                        cells: vec![crate::sql::exec::Cell::Text(
                            session.search_path().display(),
                        )],
                    }],
                },
            )),
            crate::sql::allowlist::Statement::ShowTimeZone => Ok(crate::sql::SqlOutcome::Show(
                crate::sql::exec::QueryResult {
                    columns: show_time_zone_columns(),
                    rows: vec![crate::sql::exec::ResultRow {
                        id: 0,
                        score: 0.0,
                        cells: vec![crate::sql::exec::Cell::Text(
                            session.time_zone().display().to_string(),
                        )],
                    }],
                },
            )),
            // TASK-79（SQL-9）: `CREATE FUNCTION` の検証・登録は
            // `sql::udf_call::define_function` に委譲する（検証→登録の順を守り、
            // 失敗時は `session.udfs()` を一切変更しない。`SET search_mode` と同じ
            // 「部分更新＝黙った既定化を防ぐ」方針）。
            //
            // Issue #1920（SQL-66）: DDL 実行権限のあるセッションでは、検証 → UDF カタログへの書き込み
            // （単一 write txn・1 回の commit）→ レジストリへの登録の順で永続化する。権限の無いセッションは
            // 従来どおりセッション内だけに登録し、カタログには触れない（42501 ゲートは #1922）。
            //
            // Issue #1915（SQL-66 (1)）: 型注釈つきの定義は、カタログへの型の永続化とロード側の
            // 復元（`udf_load` は型注釈つきの行を読み飛ばす）が揃うまで、永続化経路では `0A000` で
            // 拒否する（セッションにだけ残り再起動で消える黙った不整合を作らない・fail-closed）。
            //
            // Issue #1917（SQL-66 (2)）: `CREATE OR REPLACE FUNCTION` は同じ分岐で `or_replace` により
            // 置換経路（`prepare_function_replacement`）へ切り替える。DDL セッションの型付き・RETURNS つきの
            // `0A000`・`42P13` の順序は `CREATE` と同じ。
            crate::sql::allowlist::Statement::CreateFunction {
                or_replace,
                name,
                params,
                param_types,
                return_type,
                body,
                body_source,
            } => {
                if session.ddl_allowed() {
                    // Issue #1916: RETURNS 付きは永続化の前に、非 DDL セッションと同じ検証（本体の型検査）を
                    // レジストリの複製に対して行う。型の不一致は権限によらず `42P13` になり、
                    // 検証を通った定義だけが下の `0A000`（未対応の永続化）に到達する。
                    if return_type.is_some() {
                        let mut probe = session.udfs().clone();
                        if or_replace {
                            crate::sql::udf_call::define_or_replace_function_with_signature(
                                &mut probe,
                                &name,
                                &params,
                                &param_types,
                                return_type.as_ref(),
                                &body,
                            )?;
                        } else {
                            crate::sql::udf_call::define_function_with_signature(
                                &mut probe,
                                &name,
                                &params,
                                &param_types,
                                return_type.as_ref(),
                                &body,
                            )?;
                        }
                        return Err(
                            crate::sql::allowlist::SqlSurfaceError::feature_not_supported(
                                "function return types cannot be persisted yet",
                            ),
                        );
                    }
                    if param_types.iter().any(Option::is_some) {
                        return Err(
                            crate::sql::allowlist::SqlSurfaceError::feature_not_supported(
                                "typed function parameters cannot be persisted yet",
                            ),
                        );
                    }
                    if or_replace {
                        crate::sql::udf_ddl::execute_create_or_replace_function_persistent(
                            &self.storage,
                            ctx.tenant_id(),
                            session,
                            &name,
                            &params,
                            &body,
                            &body_source,
                        )?;
                    } else {
                        crate::sql::udf_ddl::execute_create_function_persistent(
                            &self.storage,
                            ctx.tenant_id(),
                            session,
                            &name,
                            &params,
                            &body,
                            &body_source,
                        )?;
                    }
                } else if or_replace {
                    crate::sql::udf_call::define_or_replace_function_with_signature(
                        session.udfs_mut(),
                        &name,
                        &params,
                        &param_types,
                        return_type.as_ref(),
                        &body,
                    )?;
                } else {
                    crate::sql::udf_call::define_function_with_signature(
                        session.udfs_mut(),
                        &name,
                        &params,
                        &param_types,
                        return_type.as_ref(),
                        &body,
                    )?;
                }
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
                        dict_txn,
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
                                &self.result_limits,
                            )
                        },
                    )?;
                    Ok(crate::sql::SqlOutcome::Query(result))
                } else {
                    let read_txn = self.begin_read_txn()?;
                    self.execute_read_statement(
                        ctx,
                        session,
                        crate::sql::allowlist::Statement::Select(validated),
                        &read_txn,
                    )
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
                let read_txn = self.begin_read_txn()?;
                self.execute_read_statement(
                    ctx,
                    session,
                    crate::sql::allowlist::Statement::Aggregate(validated),
                    &read_txn,
                )
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
                let read_txn = self.begin_read_txn()?;
                self.execute_read_statement(
                    ctx,
                    session,
                    crate::sql::allowlist::Statement::Scan(validated),
                    &read_txn,
                )
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

                    crate::sql::parser::validate_search_limit_with(
                        validated.limit(),
                        &self.result_limits,
                    )?;

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
                        dict_txn,
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
            // Issue #929（SQL-29 (c)・RLS-10 (b)・TASK-213）: 集合演算は複数枝
            // （各枝は独立した単一テーブル広域取得）を単一スナップショット上で
            // 評価する必要があるため、`Statement::Scan` アームと異なり
            // `read_txn_with_schemas`（複数テーブル版）で全枝のスキーマを
            // まとめて解決する。実行本体（束縛・型整合検証・重複除去・RLS 独立
            // 適用）は `sql::set_op::execute` が担う（第 2 の実行器を作らない
            // 方針は `Statement::Scan` と同じ）。
            crate::sql::allowlist::Statement::SetOperation(validated) => {
                let read_txn = self.begin_read_txn()?;
                self.execute_read_statement(
                    ctx,
                    session,
                    crate::sql::allowlist::Statement::SetOperation(validated),
                    &read_txn,
                )
            }
            // Issue #925（SQL-28・RLS-10、TASK-212）: JOIN も両辺を単一
            // スナップショット上で評価する必要があるため、`Statement::SetOperation`
            // と同じく `read_txn_with_schemas`（複数テーブル版）で両辺のスキーマを
            // まとめて解決する。実行本体（束縛・型検証・ハッシュ結合・RLS 独立
            // 適用）は `sql::join::execute` が担う（第 2 の実行器を作らない）。
            crate::sql::allowlist::Statement::Join(validated) => {
                let read_txn = self.begin_read_txn()?;
                self.execute_read_statement(
                    ctx,
                    session,
                    crate::sql::allowlist::Statement::Join(validated),
                    &read_txn,
                )
            }
            // Issue #1192（TABLE-18）: 評価後射影形ビューは本文を同一スナップ
            // ショット上で実行してから外側の射影を適用する
            // （[`Self::execute_read_statement`] の同名アーム参照）。
            crate::sql::allowlist::Statement::BufferedView(validated) => {
                let read_txn = self.begin_read_txn()?;
                self.execute_read_statement(
                    ctx,
                    session,
                    crate::sql::allowlist::Statement::BufferedView(validated),
                    &read_txn,
                )
            }
            // SQL-43（Issue #1531）: `FROM` なし定数式 SELECT。テーブル・スナップ
            // ショットに触れず、`PolicyContext` を要する本入口からのみ到達する。
            // `execute_read_statement` には渡さない（読み取り源を持たない）。
            crate::sql::allowlist::Statement::ConstSelect(validated) => {
                // `current_schema` 系を含む文だけ、呼び出し元テナントの SCHEMA 実在を path の要素数
                // 以下の get で確かめてからスコープを張る（含まない文は照会コストゼロ。
                // 他テナントの SCHEMA は不在と同じ扱い。TABLE-27 (7)・Issue #1718）。
                let _schema_fn = if crate::sql::const_select::uses_schema_fn(&validated) {
                    let probe = |schema: &str| {
                        self.storage
                            .schema_id(ctx.tenant_id(), schema)
                            .map(|id| id.is_some())
                            .map_err(crate::catalog::table_lookup_error)
                    };
                    let values =
                        crate::sql::search_path::resolve_schema_functions(session, &probe)?;
                    Some(crate::sql::search_path::SchemaFnScope::enter(
                        crate::sql::search_path::SchemaFnMode::Values(values),
                    ))
                } else {
                    None
                };
                let result = crate::sql::const_select::run(&validated, session.udfs())?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // TABLE-42・Issue #1664: matview の葉は `BufferedView` の本文としてのみ現れ、単独の
            // 文としては生成されない（防御的に fail-closed）。
            crate::sql::allowlist::Statement::MaterializedView(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "unexpected materialized view statement".to_string(),
                })
            }
            // SQL-35・Issue #1870: 再帰 CTE も `BufferedView` の本文としてのみ現れる（防御的に fail-closed）。
            crate::sql::allowlist::Statement::RecursiveCte(_) => {
                Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "unexpected recursive CTE statement".to_string(),
                })
            }
        }
    }

    /// `Statement::Select`（`USING PLAN` なし）・`Aggregate`・`Scan`・`SetOperation`・
    /// `Join` の実行本体（読み取り源 `read_txn` を引数に取る。Issue #1179）。
    ///
    /// [`Self::execute_validated_in_session`]（autocommit。確定済みスナップショットを
    /// 開いて渡す。従来と同一の挙動）と、明示トランザクション内で自トランザクションの
    /// 未 commit 変更を読む経路（[`Self::execute_in_active_txn`]。共有書き込み
    /// トランザクションを渡す。キャッシュは `read_txn.snapshot()` が `None` のため
    /// 使われない）が共有する（第 2 の実行器を作らない）。スキーマ取得・サブクエリ解決・
    /// 実行はすべて同一の `read_txn` 上で行う（単一スナップショット契約）。
    fn execute_read_statement<R: crate::storage::read_source::ReadSource>(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        stmt: crate::sql::allowlist::Statement,
        read_txn: &R,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.execute_read_statement_with_limits(ctx, session, stmt, read_txn, self.result_limits)
    }

    /// [`Self::execute_read_statement`] の本体。明示 `LIMIT` の範囲上限を引数で受ける
    /// （Issue #1555）。評価後射影形ビューの本文は外側の設定値と連動させず、
    /// 独立した既定値で実行するために分離している。
    fn execute_read_statement_with_limits<R: crate::storage::read_source::ReadSource>(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        stmt: crate::sql::allowlist::Statement,
        read_txn: &R,
        limits: crate::result_limits::ResultLimits,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // TABLE-15・Issue #1363: スナップショット確定後・スキーマ解決前の同期点
        // （feature 限定。既定ビルドでは消える）。
        #[cfg(feature = "test-sync-points")]
        self.storage
            .sync_point(crate::test_sync::SyncPoint::ReadStatementSnapshotAcquired);
        match stmt {
            crate::sql::allowlist::Statement::Select(validated) => {
                let schema = table_schema_in(read_txn, &validated.table_name)?;
                let bound = crate::sql::parser::bind_in_session_with_limits(
                    &validated,
                    &schema,
                    session.search_mode(),
                    session.udfs(),
                    &limits,
                )?;
                let result = self.run_select_plan(read_txn, ctx, &schema, &bound)?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            crate::sql::allowlist::Statement::Aggregate(mut validated) => {
                let schema = table_schema_in(read_txn, &validated.table_name)?;
                // Issue #927・SQL-29 (a)・RLS-10 (b)・TASK-213: 束縛
                // （`bind_aggregate`）の前に WHERE 中のサブクエリ
                // （`IN (SELECT ...)`／`EXISTS (SELECT ...)`）を、外側と同じ
                // `PolicyContext`・同じ `read_txn`（同一スナップショット）で
                // 解決する（`sql::subquery` モジュールドキュメント参照）。
                let mut subquery_budget = self.nesting_limits.max_subquery_executions();
                let mut subquery_in_value_budget = crate::sql::subquery::MAX_SUBQUERY_IN_VALUES;
                // Issue #1842・SQL-61: 肯定形の相関 `EXISTS` を半結合・行ごとの再評価で実行する。
                // 再評価・比較の予算は 1 文ごとに作り、入れ子を含む文全体で共有する。
                let correlated = crate::sql::correlated_exists::CorrelatedCtx::enabled(
                    self.join_limits,
                    self.correlated_limits,
                );
                validated.where_predicates = crate::sql::subquery::resolve_where_predicates(
                    validated.where_predicates,
                    &[&schema],
                    read_txn,
                    ctx,
                    &self.storage,
                    session.udfs(),
                    self.group_by_limits,
                    self.nesting_limits,
                    limits,
                    &mut subquery_budget,
                    &mut subquery_in_value_budget,
                    &correlated,
                )?;
                // SQL-57・Issue #1806: 集計項目の `FILTER (WHERE ...)` 中の部分クエリも
                // 同じ文脈・同じ予算で解決する。
                crate::sql::subquery::resolve_aggregate_item_filters(
                    &mut validated.items,
                    validated
                        .group_by
                        .as_mut()
                        .map_or(&mut [][..], |g| g.having_aggregates.as_mut_slice()),
                    &[&schema],
                    read_txn,
                    ctx,
                    &self.storage,
                    session.udfs(),
                    self.group_by_limits,
                    self.nesting_limits,
                    limits,
                    &mut subquery_budget,
                    &mut subquery_in_value_budget,
                )?;
                let bound =
                    crate::sql::parser::bind_aggregate(&validated, &schema, session.udfs())?
                        .with_result_cap(limits.max_result_rows());
                let result = self.run_aggregate_plan(
                    read_txn,
                    ctx,
                    &schema,
                    &bound,
                    crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES,
                )?;
                let result = Self::enforce_group_result_cap(&validated, result, &limits)?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            crate::sql::allowlist::Statement::Scan(mut validated) => {
                let schema = table_schema_in(read_txn, &validated.table_name)?;
                // `Statement::Aggregate` と同じ理由・同じ経路でサブクエリを解決する。
                let mut subquery_budget = self.nesting_limits.max_subquery_executions();
                let mut subquery_in_value_budget = crate::sql::subquery::MAX_SUBQUERY_IN_VALUES;
                // Issue #1842・SQL-61: 肯定形の相関 `EXISTS` を半結合・行ごとの再評価で実行する。
                // 再評価・比較の予算は 1 文ごとに作り、入れ子を含む文全体で共有する。
                let correlated = crate::sql::correlated_exists::CorrelatedCtx::enabled(
                    self.join_limits,
                    self.correlated_limits,
                );
                validated.where_predicates = crate::sql::subquery::resolve_where_predicates(
                    validated.where_predicates,
                    &[&schema],
                    read_txn,
                    ctx,
                    &self.storage,
                    session.udfs(),
                    self.group_by_limits,
                    self.nesting_limits,
                    limits,
                    &mut subquery_budget,
                    &mut subquery_in_value_budget,
                    &correlated,
                )?;
                // Issue #1848・SQL-61: 式の内側に埋め込まれた相関スカラーサブクエリも WHERE 側と同じ
                // 予算・同じ `read_txn`・同じ `ctx`（RLS 暗黙適用）で内側を 1 回だけ実行し、解決済みの
                // 値の葉を埋めてから外側を束縛する（WHERE 側の解決が束縛前に走る既存の順序と同じ）。
                if !validated.embedded_scalar_subqueries.is_empty() {
                    let embedded = std::mem::take(&mut validated.embedded_scalar_subqueries);
                    let leaves = crate::sql::subquery::resolve_embedded_scalar_subqueries(
                        &embedded,
                        &[&schema],
                        read_txn,
                        ctx,
                        &self.storage,
                        session.udfs(),
                        self.group_by_limits,
                        self.nesting_limits,
                        limits,
                        &mut subquery_budget,
                        &mut subquery_in_value_budget,
                        &correlated,
                    )?;
                    validated.embedded_scalar_subqueries = embedded
                        .into_iter()
                        .zip(leaves)
                        .map(|(mut e, leaf)| {
                            e.resolved = Some(leaf);
                            e
                        })
                        .collect();
                }
                // Issue #1352: 投影位置のスカラーサブクエリは WHERE 側と同じ予算・同じ
                // `read_txn`・同じ `ctx`（RLS 暗黙適用）で解決し、束縛へは項目を空にして
                // 渡す。解決結果は外側の走査結果へ SELECT リスト上の位置で合流する。
                let projection_items = std::mem::take(&mut validated.scalar_subquery_items);
                // 外側の静的な束縛・検証を内側の実行より先に行う（外側の誤りが内側の
                // 可視データに依存するエラーで覆われないようにする）。
                let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                    &validated,
                    &schema,
                    session.udfs(),
                    &[],
                    &limits,
                )?;
                let resolved_items = crate::sql::subquery::resolve_scalar_projection_items(
                    &projection_items,
                    &[&schema],
                    read_txn,
                    ctx,
                    &self.storage,
                    session.udfs(),
                    self.group_by_limits,
                    self.nesting_limits,
                    limits,
                    &mut subquery_budget,
                    &mut subquery_in_value_budget,
                    &correlated,
                )?;
                // Issue #1846・SQL-61: 集計形の相関スカラーは外側の行ごとに相関キーで引き当てるため、
                // 外側の投影の末尾へ隠しキー列を足した写しで走査する（元の投影位置は変えない。
                // 隠し列は合流時に取り除く）。相関項目が無ければ従来どおり元の束縛で走査する。
                let hidden = crate::sql::subquery::hidden_key_columns(&resolved_items);
                let result = if hidden.is_empty() {
                    self.run_scan_plan(read_txn, ctx, &schema, &bound)?
                } else {
                    let mut with_keys = validated.clone();
                    crate::sql::subquery::append_hidden_key_columns(&mut with_keys, &hidden)?;
                    let bound_with_keys = crate::sql::parser::bind_scan_with_dummy_flags(
                        &with_keys,
                        &schema,
                        session.udfs(),
                        &[],
                        &limits,
                    )?;
                    self.run_scan_plan(read_txn, ctx, &schema, &bound_with_keys)?
                };
                let result = crate::sql::subquery::merge_scalar_projection_items(
                    result,
                    resolved_items,
                    &hidden,
                )?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // 集合演算・JOIN は複数枝／両辺を単一スナップショット上で評価する必要が
            // あるため、全テーブルのスキーマをまとめて解決する（Issue #929・#925）。
            crate::sql::allowlist::Statement::SetOperation(validated) => {
                let mut table_names = Vec::new();
                crate::sql::set_op::collect_branch_tables(&validated.tree, &mut table_names);
                let schemas = table_schemas_in(read_txn, &table_names)?;
                let result = crate::sql::set_op::execute(
                    read_txn,
                    ctx,
                    &schemas,
                    &validated.tree,
                    session.udfs(),
                    validated.limit,
                    validated.offset,
                    self.group_by_limits,
                    limits,
                )?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            crate::sql::allowlist::Statement::Join(validated) => {
                let table_names = crate::sql::join::collect_join_tables(&validated);
                let mut schemas = table_schemas_in(read_txn, &table_names)?;
                let result = match validated.cte_inputs.as_deref() {
                    None => crate::sql::join::execute(
                        read_txn,
                        ctx,
                        &schemas,
                        &validated,
                        session.udfs(),
                        self.group_by_limits,
                        self.join_limits,
                        limits,
                    )?,
                    // SQL-62・Issue #1861: CTE を入力に持つ JOIN。slot（CTE 本文）を参照者自身の
                    // `ctx`・同一の `read_txn` で評価し、評価済みの行を側入力として渡す。
                    Some(cte) => {
                        // CTE 本文の評価・共有キャッシュ・参照ごとの複製と、続く JOIN の走査・結合・
                        // 出力生成は、1 つの文全体のバイト予算を共有する（確保の前に計上する）。
                        let mut budget = self.cte_join_budget();
                        let (inputs, cte_ids) = self.evaluate_join_cte_inputs(
                            ctx,
                            session,
                            read_txn,
                            &validated,
                            cte,
                            limits,
                            &mut schemas,
                            &mut budget,
                        )?;
                        crate::sql::join::execute_with_cte_inputs(
                            read_txn,
                            ctx,
                            &schemas,
                            &validated,
                            session.udfs(),
                            self.group_by_limits,
                            self.join_limits,
                            limits,
                            &cte_ids,
                            inputs,
                            budget,
                        )?
                    }
                };
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // Issue #1192（TABLE-18・RLS-10 (b)）: 評価後射影形ビュー。本文
            // （`Scan`／`Aggregate`／`Join`。参照時に再検証済み）を、参照した
            // セッション自身の `ctx`・同一の `read_txn`（単一スナップショット）で
            // 既存の実行経路により評価し、結果へ外側の列射影と `LIMIT`／`OFFSET`
            // だけを適用する。作成者の可視性は引き継がれない（`ctx` は参照者の
            // もの）。第 2 の実行器は作らない。
            crate::sql::allowlist::Statement::BufferedView(validated) => {
                Self::check_buffered_outer_limit(&validated, &limits)?;
                check_buffered_view_collations(read_txn, &validated)?;
                let body = (*validated.body).clone();
                // 具体化 CTE（Issue #1860・SQL-62 (5)）の本文は呼び出し段の上限で実行する
                // （可視行が `--max-result-rows` を超えたら黙って切り詰めず `54000`）。ビュー本文は
                // 従来どおり既定値（内側 LIMIT は外側の設定値と連動させない）。
                let body_limits = match validated.origin {
                    crate::sql::allowlist::BufferedOrigin::MaterializedCte
                    | crate::sql::allowlist::BufferedOrigin::GroupedWindow => limits,
                    crate::sql::allowlist::BufferedOrigin::View => {
                        crate::result_limits::ResultLimits::default()
                    }
                };
                if validated.origin == crate::sql::allowlist::BufferedOrigin::MaterializedCte {
                    self.count_materialized_cte_eval();
                }
                match self.execute_read_statement_with_limits(
                    ctx,
                    session,
                    body,
                    read_txn,
                    body_limits,
                )? {
                    crate::sql::SqlOutcome::Query(result) => {
                        // Issue #1360: 外側の WHERE・ORDER BY・射影・LIMIT/OFFSET は
                        // 評価済みの本文結果に対する後処理（Describe と同じ
                        // `plan_outer` で束縛する）。
                        let plan = crate::sql::view_buffered::plan_outer(
                            &result.columns,
                            &validated,
                            session.udfs(),
                        )?;
                        let processed = crate::sql::view_buffered::apply_outer(
                            result,
                            &plan,
                            self.group_by_limits,
                        )?;
                        // Issue #1796・SQL-34: 外側の `LIMIT` 省略・`LIMIT ALL` は後処理後の
                        // 件数が結果行数上限を超えたら `54000`（連鎖の素通しは対象外）。
                        let processed = match &validated.outer {
                            crate::sql::allowlist::BufferedOuter::Rows(r) if r.limit_implicit => {
                                Self::enforce_result_cap(processed, limits.max_result_rows())?
                            }
                            // 外側の `GROUP BY` も同様（`limit: None` のときだけ HAVING・OFFSET
                            // 適用後の件数を呼び出し段の上限で検査する）。
                            crate::sql::allowlist::BufferedOuter::Aggregate(agg) => {
                                Self::enforce_group_result_cap(agg, processed, &limits)?
                            }
                            _ => processed,
                        };
                        Ok(crate::sql::SqlOutcome::Query(processed))
                    }
                    _ => Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    }),
                }
            }
            // SQL-35・Issue #1870: 再帰 CTE。アンカー項を実行して最初の作業表とし、再帰項を
            // 作業表が空になるまで反復する（`sql::recursive_eval`）。基底表の読み取りは参照者自身の
            // `ctx`・同一の `read_txn`（単一スナップショット）で既存の走査経路を通る（RLS 暗黙適用）。
            // 外側の WHERE・ORDER BY・射影・集計・LIMIT は呼び出し元の `BufferedView` 腕が適用する。
            crate::sql::allowlist::Statement::RecursiveCte(validated) => {
                // 静的に決まるエラー（アンカーの束縛・再帰項の束縛・列数・型・照合・未対応型）は、アンカーを
                // 実行する前に Describe と同じ経路で検査する（データ依存の実行エラーに覆われない。
                // Describe／Execute で同じエラー）。
                self.describe_recursive_cte(&*session, &validated, false, &limits)?;
                // 重複除去版（`UNION`）はアンカー候補へ結果行数上限を適用しない（捨てる重複は行数に
                // 数えない契約。上限は重複除去後に `recursive_eval::execute` が判定する）。候補生成は
                // ハード上限と各走査の入力上限で抑えられる。
                let anchor_limits = if validated.union_all {
                    limits
                } else {
                    crate::result_limits::ResultLimits::new(
                        crate::result_limits::MAX_RESULT_ROWS_HARD_CAP,
                    )
                };
                let anchor = match &*validated.anchor {
                    // `FROM` なし定数式はセッション UDF レジストリを要する専用の実行本体を持つ
                    // （`execute_read_statement_with_limits` の対象外）。
                    crate::sql::allowlist::Statement::ConstSelect(c) => {
                        crate::sql::const_select::run(c, session.udfs())?
                    }
                    other => match self.execute_read_statement_with_limits(
                        ctx,
                        session,
                        other.clone(),
                        read_txn,
                        anchor_limits,
                    )? {
                        crate::sql::SqlOutcome::Query(result) => result,
                        _ => {
                            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                                detail: "internal error".to_string(),
                            })
                        }
                    },
                };
                let mut table_names = Vec::new();
                recursive_cte_tables(&validated, &mut table_names);
                let schemas = table_schemas_in(read_txn, &table_names)?;
                let join_limits = crate::sql::join::JoinLimits::from_settings(
                    self.group_by_limits,
                    self.join_limits,
                    limits,
                );
                let result = crate::sql::recursive_eval::execute(
                    read_txn,
                    ctx,
                    &validated,
                    anchor,
                    &schemas,
                    session.udfs(),
                    &join_limits,
                    crate::sql::recursive_eval::RecursiveLimits::from_settings(
                        self.recursive_cte_limits,
                        limits.max_result_rows(),
                    ),
                )?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            // TABLE-42・Issue #1664: matview の葉。読み手の `ctx`（RLS 暗黙適用と同じ文脈）に
            // 完全一致する文脈の実体だけを、同一の `read_txn`（単一スナップショット）から読む。
            // 外側の WHERE・ORDER BY・射影・LIMIT は呼び出し元の `BufferedView` 腕が適用する。
            crate::sql::allowlist::Statement::MaterializedView(validated) => {
                let result = crate::sql::matview::execute_read(read_txn, ctx, &validated)?;
                Ok(crate::sql::SqlOutcome::Query(result))
            }
            _ => Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            }),
        }
    }

    /// 具体化経路が CTE 本文を記憶域に対して 1 回実行したことを数える（観測専用。
    /// [`Self::materialized_cte_eval_stats`]）。
    fn count_materialized_cte_eval(&self) {
        self.materialized_cte_evals
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// JOIN の CTE 入力（SQL-62・Issue #1861）を評価し、リレーションごとの評価済みの行
    /// （CTE でないリレーションは `None`）と、CTE の行 `id` を運ぶ列の表を返す。あわせて CTE の結果列から
    /// 合成したスキーマを `schemas` へ CTE 名（リレーションの綴りのまま）で加える。
    ///
    /// slot は定義順（依存順）に、参照者自身の `ctx`・呼び出し元と同一の `read_txn`（単一
    /// スナップショット。RLS-10 (b)）で評価する。共有する slot（`MATERIALIZED`、または指定なしで
    /// 2 回以上参照）は文のローカルな `cache` に 1 回だけ保持して参照ごとに複製し、共有しない
    /// slot は参照のたびに評価する。保持した行は本関数の戻りで破棄され、他の文・他セッションとは
    /// 共有しない（SQL-62 (6)）。`Scan` 起点の本文は暗黙 `LIMIT` つきの広域取得で、可視行が
    /// `--max-result-rows` を超えたら黙って切り詰めず `54000`（SQL-62 (5)・SQL-34）。
    ///
    /// 評価結果・キャッシュ・複製の保持バイトはすべて呼び出し元の `budget`（後続の JOIN と共有する
    /// 文全体の 1 つの予算）へ、確保・複製の前に計上する。超過は `54000`（部分結果は返さない）。
    #[allow(clippy::too_many_arguments)]
    fn evaluate_join_cte_inputs<R: crate::storage::read_source::ReadSource>(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        read_txn: &R,
        validated: &crate::sql::allowlist::ValidatedJoin,
        cte: &crate::sql::allowlist::JoinCteInputs,
        limits: crate::result_limits::ResultLimits,
        schemas: &mut std::collections::HashMap<String, crate::catalog::TableSchema>,
        budget: &mut crate::sql::join::JoinBudget,
    ) -> Result<
        (
            Vec<Option<crate::sql::exec::QueryResult>>,
            crate::sql::join::CteIdColumns,
        ),
        crate::sql::allowlist::SqlSurfaceError,
    > {
        let mut cache: Vec<Option<crate::sql::exec::QueryResult>> = vec![None; cte.slots.len()];
        let mut inputs = Vec::with_capacity(validated.relations.len());
        let mut cte_ids = crate::sql::join::CteIdColumns::new();
        for (rel, slot) in validated.relations.iter().zip(cte.relation_slot.iter()) {
            let Some(k) = slot else {
                inputs.push(None);
                continue;
            };
            let result = self
                .evaluate_cte_slot(ctx, session, read_txn, cte, *k, limits, &mut cache, budget)?;
            let (schema, id_columns) = crate::sql::join::cte_schema(rel.table(), &result.columns)?;
            schemas.insert(rel.table().to_string(), schema);
            cte_ids.insert(rel.table().to_string(), id_columns);
            inputs.push(Some(result));
        }
        Ok((inputs, cte_ids))
    }

    /// CTE slot `k` の評価済みの行（列名リスト適用後）を返す（[`Self::evaluate_join_cte_inputs`]
    /// の再帰本体）。`Derived` は親 slot の評価済みの行へ層を重ねて導出し、記憶域を読まない
    /// （数えない）。生成・保持・複製のたびに `budget` へ計上する（`Scan` は走査へ残予算を上限として
    /// 渡し、生成中に打ち切らせる。キャッシュ格納・ヒット時の複製・層の適用は借用のまま見積もって
    /// から行う）。
    #[allow(clippy::too_many_arguments)]
    fn evaluate_cte_slot<R: crate::storage::read_source::ReadSource>(
        &self,
        ctx: &PolicyContext,
        session: &mut crate::sql::mode::SessionState,
        read_txn: &R,
        cte: &crate::sql::allowlist::JoinCteInputs,
        k: usize,
        limits: crate::result_limits::ResultLimits,
        cache: &mut Vec<Option<crate::sql::exec::QueryResult>>,
        budget: &mut crate::sql::join::JoinBudget,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::{CteSlotSource, SqlSurfaceError};
        let slot = cte.slots.get(k).ok_or_else(|| SqlSurfaceError::Internal {
            detail: "CTE slot out of range".to_string(),
        })?;
        if slot.shared {
            if let Some(hit) = cache.get(k).and_then(Option::as_ref) {
                budget.charge_result(hit)?;
                return Ok(hit.clone());
            }
        }
        let raw = match &slot.source {
            CteSlotSource::Scan(scan) => {
                self.count_materialized_cte_eval();
                let schema = table_schema_in(read_txn, scan.table_name())?;
                let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                    scan,
                    &schema,
                    session.udfs(),
                    &[],
                    &limits,
                )?;
                let result = crate::sql::scan::execute_scan_with_budget(
                    read_txn,
                    ctx,
                    &schema,
                    &bound,
                    budget.remaining(),
                )?;
                budget.charge_result(&result)?;
                result
            }
            CteSlotSource::Derived { from, layer } => {
                // tail CTE の `LIMIT` 省略（`limit_implicit`）は `OFFSET` 適用後の件数で上限を判定し、
                // 明示 `LIMIT`（`limit_explicit`）は `ORDER BY`／`LIMIT` 適用後の件数（設定値の範囲で
                // 検査済みの `LIMIT` 以下）で判定する。親を上限つきで評価すると層の適用前の件数で
                // `54000` になるため（単一表経路は inline 化で成功する形との不整合）、親が共有されない
                // （他の読み手へ上限なしの行を渡さない）ときだけ親の暗黙上限を hard cap まで緩める。
                // 緩めた分の行数は JoinBudget が引き続き抑え、層の適用後に呼び出し段の上限を検査する
                // （無制限の具体化は許さない。fail-closed）。
                let parent_limits = if (layer.limit_implicit || layer.limit_explicit)
                    && !cte.slots.get(*from).is_some_and(|p| p.shared)
                {
                    crate::result_limits::ResultLimits::new(
                        crate::result_limits::MAX_RESULT_ROWS_HARD_CAP,
                    )
                } else {
                    limits
                };
                let parent = self.evaluate_cte_slot(
                    ctx,
                    session,
                    read_txn,
                    cte,
                    *from,
                    parent_limits,
                    cache,
                    budget,
                )?;
                // tail CTE（Issue #1862）の明示 `LIMIT` は設定値の範囲で検査する（構文段は
                // 設定値を持たない。Scan 側の `bind_scan_with_dummy_flags` と同じ契約）。
                if layer.limit_explicit {
                    crate::sql::parser::validate_search_limit_with(layer.limit, &limits)?;
                }
                let plan = crate::sql::view_buffered::plan_rows_layer(
                    &parent.columns,
                    &slot.name,
                    layer,
                    session.udfs(),
                )?;
                // 層は親の行から射影のセルを複製して新しい行集合を作る。同じ列の複数回選択で親より
                // 大きくなりうるため、出力の見積もり（上界）を適用の **前** に計上する。
                budget.charge_layer_output(&parent, &plan)?;
                let layered = crate::sql::view_buffered::apply_rows_layer(parent, &plan)?;
                if layer.limit_implicit {
                    Self::enforce_result_cap(layered, limits.max_result_rows())?
                } else {
                    layered
                }
            }
        };
        let result = if slot.column_aliases.is_empty() {
            raw
        } else {
            crate::sql::exec::QueryResult {
                columns: crate::sql::view_buffered::rename_cte_columns(
                    &raw.columns,
                    &slot.column_aliases,
                )?,
                rows: raw.rows,
            }
        };
        if slot.shared {
            // キャッシュに残す複製の分を、複製の前に計上する（返す値とは別に 1 部保持される）。
            budget.charge_result(&result)?;
            if let Some(entry) = cache.get_mut(k) {
                *entry = Some(result.clone());
            }
        }
        Ok(result)
    }

    /// CTE slot `k` の結果列（列名リスト適用後）を、本文を実行せずに導出する（Describe 用。
    /// [`Self::evaluate_cte_slot`] の結果列と一致する）。
    fn describe_cte_slot(
        &self,
        session: &crate::sql::mode::SessionState,
        cte: &crate::sql::allowlist::JoinCteInputs,
        k: usize,
        skip_vector_literal_validation: bool,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<Vec<crate::sql::exec::ColumnMeta>, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::{CteSlotSource, SqlSurfaceError, Statement};
        let slot = cte.slots.get(k).ok_or_else(|| SqlSurfaceError::Internal {
            detail: "CTE slot out of range".to_string(),
        })?;
        let raw = match &slot.source {
            CteSlotSource::Scan(scan) => self
                .describe_parsed_in_session_impl(
                    session,
                    &ParsedSql::Statement(Statement::Scan((**scan).clone())),
                    skip_vector_literal_validation,
                    &[],
                    limits,
                )?
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                })?,
            CteSlotSource::Derived { from, layer } => {
                let parent = self.describe_cte_slot(
                    session,
                    cte,
                    *from,
                    skip_vector_literal_validation,
                    limits,
                )?;
                // Execute（`evaluate_cte_slot`）と同じ明示 `LIMIT` の範囲検査を Describe でも行い、
                // Describe 成功・Execute だけ上限違反になる乖離を作らない（Issue #1862）。
                if layer.limit_explicit {
                    crate::sql::parser::validate_search_limit_with(layer.limit, limits)?;
                }
                crate::sql::view_buffered::plan_rows_layer(
                    &parent,
                    &slot.name,
                    layer,
                    session.udfs(),
                )?
                .columns()
                .to_vec()
            }
        };
        if slot.column_aliases.is_empty() {
            Ok(raw)
        } else {
            crate::sql::view_buffered::rename_cte_columns(&raw, &slot.column_aliases)
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
        read_txn: &impl crate::storage::read_source::ReadSource,
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
                    index_gate_cache: &self.index_catalog_gate_cache,
                    hnsw_scope: self.hnsw_scope,
                }),
            // Issue #473: スカラー列二次索引の gated 構築（応答には未使用。
            // 詳細は `sql::scalar_index` のドキュメント参照）。
            Some(crate::sql::scalar_index::ScalarCacheAccess {
                storage: &self.storage,
                cache: &self.scalar_index_cache,
                declarations_enabled: self.hnsw_state.is_some(),
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
    /// `dict_txn`（Issue #1353）が `Some` のとき、辞書構築と検索本体の行の読みを共有
    /// write トランザクションへ切り替える（明示トランザクション内で自トランザクションの
    /// 未 commit の変更を反映する）。カタログ・スキーマ・テーブル世代の照合は常に確定済み
    /// スナップショットから行う（トランザクション内は DDL 不可・単一ライタ保持のため
    /// BEGIN 以降不変で、世代は write_txn 側だけが進むため write_txn から読むと必ず不一致
    /// になる）。`None` は従来どおり。
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
        dict_txn: Option<&redb::WriteTransaction>,
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
        crate::sql::parser::validate_search_limit_with(limit, &self.result_limits)?;

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

        let planned =
            self.plan_using_plan_expansion(ctx, session, table, query_mode, question, dict_txn)?;
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
        match dict_txn {
            // 明示トランザクション内（Issue #1353）: 検索本体の行の読みだけを共有 write
            // トランザクションへ切り替える。`snapshot()` が `None` のためキャッシュ
            // （sparse・arena・hnsw・scalar）は構造的に使われない。
            Some(write_txn) => self.run_select_plan(write_txn, ctx, &schema, &bound),
            None => self.run_select_plan(&read_txn, ctx, &schema, &bound),
        }
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
        self.check_bound_limit(bound.limit())?;
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
        let validated_limit =
            crate::sql::parser::validate_search_limit_with(limit, &self.result_limits)?;
        crate::sql::allowlist::validate_using_plan_question(question)?;

        self.run_using_plan_select(
            ctx,
            session,
            table,
            question,
            mode_literal,
            limit,
            None,
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
                // Issue #945・NOSQL-14: NoSQL 表層の `plan` 検索経路も `filter` の
                // 範囲比較・IN・OR 拡張を受ける（`bind` closure が
                // `PlanSearchBinding::with_where_filters` 経由で設定した式述語・
                // OR 群をそのまま素通しする。第 2 のコンパイル経路を作らない）。
                let expr_filter_programs =
                    crate::sql::parser::compile_expr_filter_programs(&parts.expr_filters);
                Ok(crate::sql::parser::BoundStatement {
                    table: table.to_string(),
                    projection: parts.projection,
                    metadata_filters: parts.metadata_filters,
                    rls_predicate_present: false,
                    expr_filters: parts.expr_filters,
                    expr_filter_programs,
                    or_filters: parts.or_filters,
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
        read_txn: &impl crate::storage::read_source::ReadSource,
        ctx: &PolicyContext,
        schema: &crate::catalog::TableSchema,
        bound: &crate::sql::parser::BoundScan,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::scan::execute_scan(read_txn, ctx, schema, bound)
    }

    /// `COPY ... TO STDOUT` の読み取り（束縛 → 走査）を担う単一の入口（Issue #1771・WIRE-22 (5)・
    /// SQL-34 のポインタ）。`begin_copy_validated`・`begin_copy_active`（明示 txn の dirty 分岐）・
    /// `core/temp_route.rs::begin_copy_temp` の 3 経路がここを通る（行数上限の判定を 1 か所に
    /// 集約し、経路の増減でテーブル形が上限なしになる fail-open を防ぐ）。
    ///
    /// - 括弧形: 内側 `SELECT` の明示 `LIMIT` を従来どおり束縛する（挙動不変）。
    /// - テーブル形: `LIMIT` を行数上限 + 1 に差し替えて取得し、上限を超えたら黙って切り詰めず
    ///   `54000` で拒否する（`CopyOutResponse` より前に確定するため行は 1 件も送られない）。
    ///   エラー文言は固定で、行の内容・テナント ID を含めない。
    pub(super) fn run_copy_to_scan(
        &self,
        source: &impl crate::storage::read_source::ReadSource,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        schema: &crate::catalog::TableSchema,
        v: &crate::sql::allowlist::ValidatedCopyTo,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::SqlSurfaceError;
        if !v.is_table_form() {
            let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                &v.inner,
                schema,
                session.udfs(),
                &[],
                &self.result_limits,
            )?;
            return self.run_scan_plan(source, ctx, schema, &bound);
        }
        let internal = || SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        };
        // 上限は設定値そのもの（`--max-result-rows`）。INSERT ... SELECT 用の丸め
        // （ハード上限 - 1）は使わない（設定値ちょうどの行数を許容するため）。
        let cap = self.result_limits.max_result_rows();
        let fetch = cap.checked_add(1).ok_or_else(internal)?;
        let mut scan = (*v.inner).clone();
        scan.limit = u32::try_from(cap).map_err(|_| internal())?;
        let mut bound = crate::sql::parser::bind_scan_with_dummy_flags(
            &scan,
            schema,
            session.udfs(),
            &[],
            &self.result_limits,
        )?;
        // 束縛は上限 `cap` までしか LIMIT を受理しない（ハード上限での飽和）ため、超過検出用の
        // `+1` 行は束縛後に差し替える（束縛済み値の差し替えなので検証は迂回しない）。
        bound.limit = fetch;
        let result = self.run_scan_plan(source, ctx, schema, &bound)?;
        if result.rows.len() > cap {
            return Err(SqlSurfaceError::payload_too_large(
                "COPY result exceeds the row limit",
            ));
        }
        Ok(result)
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
        read_txn: &impl crate::storage::read_source::ReadSource,
        ctx: &PolicyContext,
        schema: &crate::catalog::TableSchema,
        bound: &crate::sql::parser::BoundAggregate,
        max_result_bytes: usize,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::aggregate::execute_aggregate_with_group_limits(
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
                declarations_enabled: self.hnsw_state.is_some(),
            }),
            self.group_by_limits,
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
    #[allow(clippy::too_many_arguments)]
    fn run_explain_plan<F, E>(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        table: &str,
        question: &str,
        mode_literal: Option<&str>,
        dict_txn: Option<&redb::WriteTransaction>,
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
            .plan_query_with_mode_in(
                ctx,
                table,
                question,
                query_mode,
                session.search_mode(),
                dict_txn,
            )
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
        // Issue #1066 PR #1155 codex-review P1 指摘・3 巡目対応: `post_check_txn`
        // はここで drop せず [`Self::explain_engine_for`] へ渡すまで保持する
        // （Issue #1153 の `resolve_scalar_index_target_in_txn` 呼び出しも同じ
        // `post_check_txn` を使い、その後 `drop` するのは `explain_engine_for`
        // 呼び出し後まで遅らせる）。世代照合済みの `post_check_schema` と同一
        // スナップショットで索引宣言を読むことで、世代照合からここまでの間に
        // 新たな `DROP INDEX`／`CREATE INDEX` がコミットされても、束縛に
        // 使ったスキーマと異なる世代の索引宣言を注記しない（[`Self::
        // explain_engine_for`] のドキュメンテーションコメント参照）。
        if current_generation != planning_generation {
            drop(post_check_txn);
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "table generation changed during EXPLAIN USING PLAN query \
                         expansion; rejecting stale plan"
                    .to_string(),
            }
            .into());
        }

        // Issue #1153・TASK-206・INDEX-7: 索引宣言を、世代照合を通過した
        // 直後の `post_check_txn`（世代一致＝`pre_check_schema` と同じ状態の
        // カタログ）から解決する。`CREATE INDEX`／`DROP INDEX` は対象テーブルの
        // 世代を進める（`catalog::create_index`／`drop_index`）ため、世代一致は
        // 「この読み取りの宣言が `bind` 時点のスキーマと矛盾しない」ことを
        // 保証する。`post_check_txn` はここでは drop しない（Issue #1066
        // PR #1155 3 巡目対応で [`Self::explain_engine_for`] へも同じ
        // スナップショットを渡す契約のため、そこまで生かし続ける。下記
        // `explain_engine_for` 呼び出し直後にまとめて drop する）。
        let target = crate::sql::scalar_index::resolve_scalar_index_target_in_txn(
            &post_check_txn,
            table,
            self.hnsw_state.is_some(),
        );

        // INDEX-10（Issue #1650）: 式索引の式一致を、世代照合を通過した `post_check_txn`（`bind` 時と
        // 同じ世代のカタログ）から確定する。式候補が無いクエリはカタログを読まない。opt-in なし
        // （`declarations_enabled == false`）は空のマッチャで、実行時と同じく式候補は全走査になる。
        let (explain_scalar_plan, expression_index_names) =
            if explain_shape.needs_expression_matcher() {
                let matcher = if self.hnsw_state.is_some() {
                    crate::sql::scalar_index::resolve_expression_matcher_for_table_in_txn(
                        &post_check_txn,
                        table,
                        &post_check_schema,
                    )
                } else {
                    crate::sql::expression_key_index::ExpressionIndexMatcher::empty()
                };
                explain_shape.resolve_expressions(&matcher)
            } else {
                (explain_shape.scalar_plan(), Vec::new())
            };

        // I/O 完了後の最新スキーマにも辞書必須列の検証を再適用する（多層防御。
        // `Statement::Select` アームの `USING PLAN` 経路と同じ理由）。
        dictionary_required_columns(&post_check_schema)
            .map_err(crate::sql::allowlist::SqlSurfaceError::invalid_input)?;

        // Issue #1066: `scalar_plan:` 行へ使用索引名を注記するため、
        // `explain_shape`（`bind` が構文段のみから確定させた形状。`WHERE`
        // 束縛結果の一部）が記録した `metadata_filters` 参照列を、世代照合
        // 済みの `post_check_schema`（`bind` 時と同一世代であることを上で
        // 検証済み）で列名へ写像する。写像に失敗する列（overflow・
        // インデックス範囲外）が 1 つでもあれば全体を `None` へ fail-closed に
        // 倒す（未検証・不整合な列名を索引名注記の被覆判定に使わない）。
        let scalar_filter_column_names: Option<Vec<&str>> =
            if explain_shape.filter_columns_overflowed() {
                None
            } else {
                let mut names = Vec::new();
                let mut ok = true;
                for idx in explain_shape.filter_columns() {
                    match post_check_schema.columns.get(idx) {
                        Some(col) => names.push(col.name.as_str()),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    Some(names)
                } else {
                    None
                }
            };

        // Issue #411: `engine:`／`hnsw_params:`／`ann_plan:` 行の入力を
        // 組み立てる。`EXPLAIN` は `USING PLAN` 専用（束縛結果は常に
        // `Ranking::Hybrid`）のため `is_hybrid` は常に `true`。`USING PLAN` は
        // `HINT ORDER` を受理しない（SQL-5・許可リスト層）ため評価順序は常に
        // 既定（`EvaluationOrder::DEFAULT`）であり、`scalar_prefilter` は
        // ここで固定的に導出できる。`explain_engine_for`（Issue #922・SQL-27
        // で `USING PLAN` なし検索 EXPLAIN と共有するために抽出。Issue #1066
        // で使用索引名の組み立ても統合）へ委譲する。
        // Issue #2191: 照合込みの使用可否と縮退理由（実行時の `candidates_for` と同じ判定関数）。
        let scalar_plan_with_reason =
            crate::sql::scalar_index::scalar_plan_under_target_with_reason(
                explain_scalar_plan,
                explain_shape.metadata_filter_columns(),
                &explain_shape.text_uses(),
                &post_check_schema,
                &target,
            );
        // INDEX-11（Issue #1656）: 通常の分類（式索引の一致後・宣言補正の前）が `PlainScan` のとき、
        // 実行時（`sql::exec`）と同じ選択関数で部分索引の含意を判定する。含意されれば `index_partial`。
        let partial_index_names = if self.hnsw_state.is_some()
            && explain_scalar_plan == crate::sql::scalar_plan::ScalarPlan::PlainScan
        {
            explain_shape.implied_partial_indexes(&post_check_txn, table, &post_check_schema)
        } else {
            Vec::new()
        };
        let scalar_plan_with_reason = if partial_index_names.is_empty() {
            scalar_plan_with_reason
        } else {
            (crate::sql::scalar_plan::ScalarPlan::IndexPartial, None)
        };
        let (explain_engine, index_names) = self.explain_engine_for(
            &post_check_txn,
            table,
            true,
            planned.mode().mode() == crate::sql::mode::SearchMode::Precision,
            explain_shape.filters_empty(),
            crate::sql::plan::ExecutionPlan::from_evaluation_order(
                crate::sql::plan::EvaluationOrder::DEFAULT,
            )
            .scalar_prefilter,
            // Issue #474: `bind` が構文段のみから確定させた静的判定（LLM
            // I/O・世代照合の影響を受けない。上記コメントと同じ理由）に、
            // Issue #1153 で索引宣言による対象列の絞り込みを反映する
            // （`scalar_plan_under_target` が `PlainScan` へ降格させた場合、
            // 直後の `explain_engine_for` の `scalar_names_eligible` 判定
            // 〔`scalar_plan != PlainScan`〕も連動して索引名注記を出さなく
            // なる。Issue #1066 PR #1155 codex-review P2 対応と矛盾しない）。
            scalar_plan_with_reason.0,
            scalar_plan_with_reason.1,
            scalar_filter_column_names.as_deref(),
            &expression_index_names,
            &partial_index_names,
        );
        drop(post_check_txn);
        Ok(crate::sql::explain::build_explain_result_with_indexes(
            &planned,
            &explain_engine,
            &index_names,
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
    /// 戻り値の [`crate::sql::explain::ExplainIndexNames`]（Issue #1066・
    /// TASK-206・INDEX-7）は `ann_plan:`／`scalar_plan:` 行への使用索引名注記。
    /// HNSW ゲート判定（[`crate::catalog::hnsw_targeted_in_txn`]）と索引宣言
    /// 読み取り（[`crate::catalog::explain_index_names_in_txn`]）を同一の
    /// `read_txn`（同一スナップショット）から行い、トークンと索引名の食い違い
    /// を作らない。`scalar_filter_columns`（`None` は「列名を確定できない」
    /// ——`ExplainShape` の overflow・写像失敗いずれか——ことを表し、fail-closed
    /// にスカラー索引名を出さない）は `metadata_filters` が参照する列の名前
    /// （呼び出し元が束縛時スキーマで解決済み）。付与条件（対象外は空 `Vec`
    /// のまま）:
    /// - ann: `ann_plan` が `HnswFullVisible`／`HnswSubset` かつ
    ///   `self.hnsw_scope == HnswScope::Declared` のときのみ、対象テーブルの
    ///   `USING hnsw` 宣言すべて（`--hnsw-scope all` では適格性
    ///   〔`catalog::hnsw_targeted_in_txn`〕が宣言の有無を見ず常に真になり、
    ///   経路・Top-k が宣言の有無で変わらないのに `index=` 表示だけ変わる
    ///   契約矛盾を避けるため、宣言があっても付けない。codex-review P1
    ///   指摘対応・Issue #1066 PR #1155 2 巡目）
    /// - scalar: 宣言の有効化スイッチ `self.hnsw_state.is_some()` が真・
    ///   `scalar_plan` が `PlainScan` 以外・`scalar_filter_columns` が
    ///   `Some` かつ非空・その全列がいずれかのスカラー宣言で被覆されている
    ///   場合のみ（被覆されない列が 1 つでもあれば実行時は
    ///   `FallbackNoIndex` で全走査に落ちるため索引使用を主張しない。
    ///   トークン自体の実行時整合は #1153 の管轄）
    ///
    /// `read_txn`（Issue #1066 PR #1155 codex-review P1 指摘・3 巡目対応）は
    /// 呼び出し元が `bind`（束縛）に使ったスキーマと同一の `read_txn_with_
    /// schema` 由来のスナップショットをそのまま渡す（新規に `begin_read()`
    /// し直さない）。索引宣言をここで独自に新しい読み取りトランザクションから
    /// 読むと、束縛時と索引宣言取得時の間に `DROP INDEX`／`CREATE INDEX`／
    /// テーブル再作成がコミットされた場合、旧スキーマで束縛した述語に新しい
    /// 索引宣言の名前が付き実際の実行計画と食い違う（[`Self::run_explain_
    /// plan`] の世代照合は `post_check_txn` とは別の txn を開き直す実装だと
    /// この窓を塞がない）。呼び出し元はいずれも `bind` 用に開いた
    /// `read_txn` を保持し続け、ここへ同じ参照を渡すことで束縛・索引宣言
    /// 読み取りを単一スナップショットへ統一する。
    /// `catalog::explain_index_names_in_txn`（索引カタログ全件走査）を呼ぶ
    /// 唯一の経路（Issue #1066 codex-review P2 指摘対応・PR #1155 4 巡目）。
    /// [`Self::explain_engine_for`]・[`Self::aggregate_explain_from_bound`] の
    /// 両方がこれを経由することで、`explain_index_name_scans` カウンタが
    /// 「索引名注記のためのカタログ走査が実際に行われた回数」を漏れなく計上
    /// する（呼び出し元は事前に索引名が最終的に表示されるかを判定し、表示
    /// しない場合はこのメソッド自体を呼ばない契約——判定式は各呼び出し元の
    /// ドキュメンテーションコメント参照）。カウンタは observability 専用で
    /// あり、戻り値・fail-closed 挙動は `catalog::explain_index_names_in_txn`
    /// をそのまま透過する。
    fn read_explain_index_names_in_txn(
        &self,
        read_txn: &redb::ReadTransaction,
        table: &str,
    ) -> crate::catalog::Result<crate::catalog::TableIndexDecls> {
        self.explain_index_name_scans
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::catalog::explain_index_names_in_txn(read_txn, table)
    }

    #[allow(clippy::too_many_arguments)] // Issue #1066 で `scalar_filter_columns`・`read_txn` 追加により 9 引数。hnsw.rs と同じ方針で許容する。
    fn explain_engine_for(
        &self,
        read_txn: &redb::ReadTransaction,
        table: &str,
        is_hybrid: bool,
        is_precision: bool,
        filters_empty: bool,
        scalar_prefilter: bool,
        scalar_plan: crate::sql::scalar_plan::ScalarPlan,
        scalar_fallback: Option<crate::sql::scalar_index::ScalarFallbackReason>,
        scalar_filter_columns: Option<&[&str]>,
        expression_index_names: &[String],
        partial_index_names: &[String],
    ) -> (
        crate::sql::explain::ExplainEngine,
        crate::sql::explain::ExplainIndexNames,
    ) {
        let declarations_enabled = self.hnsw_state.is_some();
        // Issue #1065: `ann_plan:` 表示は実行時判定（`sql::exec` の
        // `hnsw_enabled`）と同じゲート（`catalog::hnsw_targeted_in_txn`）を
        // 通す（実行時判定と `EXPLAIN` 表示の乖離を作らない。`hnsw_state` の
        // `lookup`／`prepare_*` は呼ばない副作用なしの読み取り専用判定）。
        // `declarations_enabled == false`（opt-in なしの既定エンジン）では
        // `hnsw_enabled` は常に `false`・索引名も常に空になるため、旧実装
        // （`self.hnsw_state.is_some() && match begin_read() {..}` の短絡評価）
        // と同じくカタログに一切触れない（advisor 指摘対応・Issue #1066:
        // 索引名の組み立てを追加した際に txn を無条件で開くよう誤って変更
        // すると、既定エンジンの EXPLAIN 毎に不要な索引カタログ全件走査
        // （最大 `MAX_INDEX_COUNT` 件のデコード）という新規 I/O 副作用を生む）。
        // 読み取りに使う txn は呼び出し元が `bind` に使った `read_txn`
        // 引数（同一スナップショット。上記ドキュメンテーションコメント・
        // Issue #1066 PR #1155 codex-review P1 指摘・3 巡目対応）をそのまま
        // 使う（新規に `begin_read()` しない）。カタログ読み取り自体の失敗
        // （走査上限超過・デコード失敗）は fail-closed に `false`
        // （brute-force 表示）・索引名なしへ倒す。
        let hnsw_enabled = if declarations_enabled {
            crate::catalog::hnsw_targeted_in_txn(
                read_txn,
                &self.index_catalog_gate_cache,
                table,
                self.hnsw_scope,
                true,
            )
        } else {
            false
        };
        let ann_plan =
            crate::sql::hnsw_cache::classify_ann_plan(crate::sql::hnsw_cache::AnnShapeInput {
                hnsw_enabled,
                engine_kind_unknown: self.search_engine_kind().is_none(),
                is_hybrid,
                is_precision,
                filters_empty,
                scalar_prefilter,
            });

        // codex-review P1 指摘対応（Issue #1066 PR #1155）: `HnswScope::All` は
        // `hnsw_targeted_in_txn` が宣言の有無を見ず常に `true` を返す（適格性が
        // 宣言に依存しない）ため、宣言名を注記すると「宣言の追加・削除で実行
        // 経路が変わらないのに使用索引名表示だけが変わる」という「使用索引を
        // 示す」契約との矛盾を生む。`HnswScope::Declared` はテーブル単位の
        // 適格性そのものが宣言の有無で決まる（catalog::hnsw_targeted_in_txn）
        // ため、この scope に限り宣言名を「使用索引名」として注記する。
        let ann_names_eligible = matches!(
            ann_plan,
            crate::sql::hnsw_cache::AnnPlan::HnswFullVisible
                | crate::sql::hnsw_cache::AnnPlan::HnswSubset
        ) && self.hnsw_scope == crate::search_engine::HnswScope::Declared;
        // Issue #1066 codex-review P2 指摘対応: `scalar_plan` の静的分類・列
        // 被覆の前提条件（下記 `scalar_names_eligible`）は `index_decls` を
        // 読まなくても確定する。索引名を最終的に一切表示しない呼び出し
        // （述語なし・`HnswScope::All`・`PlainScan` 等）でもカタログ全件走査
        // （[`crate::catalog::explain_index_names_in_txn`]）を無条件に行うと、
        // 既存の HNSW ゲート（[`crate::catalog::hnsw_targeted_in_txn`]。上記
        // `index_catalog_gate_cache` による世代別キャッシュ対象）とは別の
        // 未キャッシュな走査が `EXPLAIN` の呼び出しごとに発生してしまう。
        // 両条件のいずれかが真の場合に限りカタログを読む（走査自体は
        // キャッシュしないが、不要な呼び出しでは行わない）。
        //
        // INDEX-10（Issue #1650）: 式索引の名前は呼び出し元が一致判定で確定済みなので、式述語だけの
        // WHERE（列述語なし）でも名前を出せる。列名を確定できない（`scalar_filter_columns == None`）
        // ときは従来どおり名前を出さない。索引カタログの読み取り（列被覆の判定用）が要るのは
        // 列述語があるときだけ。
        let scalar_columns_nonempty = scalar_filter_columns.is_some_and(|cols| !cols.is_empty());
        // 部分索引（`IndexPartial`。Issue #1656）の名前は呼び出し元が含意判定で確定済み。列被覆の
        // 判定（`scalar_index_names_with_expressions`）は通さない（部分索引は被覆の候補から除外）。
        let scalar_names_eligible = declarations_enabled
            && !matches!(
                scalar_plan,
                crate::sql::scalar_plan::ScalarPlan::PlainScan
                    | crate::sql::scalar_plan::ScalarPlan::IndexPartial
            )
            && scalar_filter_columns.is_some()
            && (scalar_columns_nonempty || !expression_index_names.is_empty());
        let index_decls = if declarations_enabled
            && (ann_names_eligible || (scalar_names_eligible && scalar_columns_nonempty))
        {
            // Issue #1066: `Err`（走査上限超過・デコード失敗）は「名前を
            // 出さない」へ fail-closed に倒す（`EXPLAIN` 自体は失敗させ
            // ない。トークン〔`hnsw_enabled`〕は上記判定のまま変えない）。
            self.read_explain_index_names_in_txn(read_txn, table).ok()
        } else {
            None
        };
        let ann_names = match &index_decls {
            Some(decls) if ann_names_eligible => decls.hnsw.clone(),
            _ => Vec::new(),
        };
        // 2 巡目 codex-review P1 指摘対応（Issue #1066 PR #1155）: ここで付ける
        // 名前は `scalar_plan` の静的分類（`ScalarPlan::PlainScan` 以外）と
        // カタログ宣言による列被覆だけで決まる「宣言上の候補索引名」であり、
        // 実行側 `ScalarIndex::resolve_candidates` が選択度超過・候補取得
        // 不能で全走査へ縮退した場合でもそのまま付く（`scalar_index_names_
        // for_columns` のドキュメンテーションコメント参照）。EXPLAIN は
        // テーブル内容・行数に依存する実行時縮退を観測しない既存契約
        // （`docs/design/explain-search-engine-exposure.md`「決定 1」節）を
        // 優先し、意図的に静的判定のみを報告する。
        let scalar_names = match scalar_filter_columns {
            _ if declarations_enabled
                && scalar_plan == crate::sql::scalar_plan::ScalarPlan::IndexPartial =>
            {
                partial_index_names.to_vec()
            }
            Some(cols) if scalar_names_eligible => scalar_index_names_with_expressions(
                index_decls.as_ref().map(|decls| decls.scalar.as_slice()),
                cols,
                expression_index_names,
            ),
            _ => Vec::new(),
        };

        (
            crate::sql::explain::ExplainEngine::new(
                self.search_engine_kind(),
                ann_plan,
                scalar_plan,
            )
            .with_scalar_fallback(scalar_fallback),
            crate::sql::explain::ExplainIndexNames::new(ann_names, scalar_names),
        )
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
        let (read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
        let bound = crate::sql::parser::bind_in_session_with_limits(
            validated,
            &schema,
            session.search_mode(),
            session.udfs(),
            &self.result_limits,
        )?;
        Ok(crate::sql::SqlOutcome::Explain(
            self.search_explain_from_bound(&read_txn, &schema, &bound),
        ))
    }

    /// 束縛済み検索計画（[`crate::sql::parser::BoundStatement`]）から
    /// `QUERY PLAN` 応答を組み立てる（Issue #948・NOSQL-16・SQL-27・
    /// TASK-186）。[`Self::run_search_explain`]（SQL `EXPLAIN SELECT ...`
    /// テキスト経由）と [`Self::explain_bound_search_in_session`]（NoSQL 表層の
    /// `vector` 指定 `explain: true`。wire-server から呼ばれる）の両方が
    /// この私的ヘルパーを共有することで、同じ意味の要求が SQL 経由・NoSQL
    /// 経由のどちらでもビット同一の行を返す契約を構造として保証する
    /// （第 2 の実装を作らない設計）。検索本体（`hnsw_state` の
    /// `lookup`／`prepare_*`・`SearchProvider::search`）は呼ばない。`schema`
    /// （Issue #1066）は `bound` を束縛したのと同一スキーマで、
    /// `metadata_filters` の `column_index()` を索引名注記の被覆判定用の
    /// 列名へ写像するために使う（`bound.table()` と同一テーブルであることは
    /// 呼び出し元がいずれも `bind` 直後に呼ぶ契約により保証される）。
    /// `read_txn`（Issue #1066 PR #1155 codex-review P1 指摘・3 巡目対応）は
    /// `schema` を取得したのと同一の `read_txn_with_schema` 由来のスナップ
    /// ショットで、[`Self::explain_engine_for`] へそのまま渡し索引宣言も
    /// 同一スナップショットから読む（束縛時スキーマと索引宣言取得時の間に
    /// `DROP INDEX`／`CREATE INDEX` がコミットされて食い違う窓を作らない）。
    ///
    /// 同じ `read_txn`／`schema`（Issue #1153・TASK-206・INDEX-7）から索引宣言
    /// （[`crate::sql::scalar_index::resolve_scalar_index_target_in_txn`]）も
    /// 解決し、`scalar_plan:` 表示を実行時の索引構築対象選択と一致させる
    /// （[`crate::sql::scalar_index::scalar_plan_under_target`]。宣言で対象外に
    /// なった列への述語は `PlainScan` へ補正され、直後の `explain_engine_for`
    /// の索引名注記も連動して出なくなる）。
    fn search_explain_from_bound(
        &self,
        read_txn: &redb::ReadTransaction,
        schema: &crate::catalog::TableSchema,
        bound: &crate::sql::parser::BoundStatement,
    ) -> crate::sql::exec::QueryResult {
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
        // INDEX-10（Issue #1650）: 実行時（`sql::exec`）と同じ式索引の一致判定を同一の `read_txn` から
        // 解決して分類へ加える（式候補が無い・opt-in なしのクエリは空のマッチャ＝従来どおり）。
        let expr_matcher = if scalar_prefilter {
            crate::sql::scalar_index::resolve_expression_matcher_in_txn(
                read_txn,
                bound.table(),
                schema,
                self.hnsw_state.is_some(),
                bound.expr_filters(),
            )
        } else {
            crate::sql::expression_key_index::ExpressionIndexMatcher::empty()
        };
        let expr_finder = |e: &crate::sql::udf_call::BoundExpr,
                           ty: crate::sql::udf_call::ExprType| {
            expr_matcher.find(e, ty)
        };
        let scalar_plan_before_target = crate::sql::scalar_plan::classify_scalar_plan_with(
            &crate::sql::scalar_plan::ScalarShapeInput {
                scalar_prefilter,
                metadata_filters: bound.metadata_filters(),
                expr_filters: bound.expr_filters(),
                or_filters: bound.or_filters(),
            },
            &expr_finder,
        );
        let expression_index_names = crate::sql::scalar_plan::matched_expression_index_names(
            bound.expr_filters(),
            &expr_finder,
        )
        .unwrap_or_default();
        // Issue #1066: `metadata_filters` が参照する列を同一 `schema` で
        // 列名へ写像する（添字は `MetadataFilter::column_index` のドキュメント
        // どおり `schema.columns` と同一空間）。写像に失敗する列があれば
        // fail-closed に `None`（索引名なし）へ倒す。
        let scalar_filter_column_names = scalar_filter_column_names_with_numeric(
            schema,
            bound.metadata_filters(),
            bound.expr_filters(),
        );
        // Issue #1153: `sql::exec` が索引構築対象選択に使うのと同じ単一
        // 情報源から `target` を解決し、宣言で対象外にした列への述語を
        // `scalar_plan_under_target` で `PlainScan` へ補正する。
        let target = crate::sql::scalar_index::resolve_scalar_index_target_in_txn(
            read_txn,
            bound.table(),
            self.hnsw_state.is_some(),
        );
        // 式述語の数値列（Issue #1359）も `Declared` の宣言列照合へ加える。
        let metadata_filter_columns: Vec<Option<usize>> = bound
            .metadata_filters()
            .iter()
            .map(|f| Some(f.column_index()))
            .chain(
                crate::sql::scalar_plan::numeric_predicate_columns(bound.expr_filters()).map(Some),
            )
            .collect();
        // Issue #2191: 照合込みの使用可否と縮退理由（実行時の `candidates_for` と同じ判定関数）。
        let text_uses = crate::sql::explain::text_predicate_uses(
            bound.metadata_filters(),
            bound.expr_filters(),
        );
        let (scalar_plan, scalar_fallback) =
            crate::sql::scalar_index::scalar_plan_under_target_with_reason(
                scalar_plan_before_target,
                metadata_filter_columns,
                &text_uses,
                schema,
                &target,
            );
        // INDEX-11（Issue #1656）: 通常の分類（宣言補正の前）が `PlainScan` で `WHERE` が部分索引の述語を
        // 含意するとき、`sql::exec` と同じ選択関数の結果を `index_partial` として表示する。
        let partial_index_names = if self.hnsw_state.is_some()
            && scalar_prefilter
            && bound.has_where_filters()
            && scalar_plan_before_target == crate::sql::scalar_plan::ScalarPlan::PlainScan
        {
            crate::sql::scalar_index::resolve_implied_partial_indexes_in_txn(
                read_txn,
                bound.table(),
                schema,
                true,
                (
                    bound.metadata_filters(),
                    bound.expr_filters(),
                    bound.or_filters(),
                ),
            )
        } else {
            Vec::new()
        };
        let (scalar_plan, scalar_fallback) = if partial_index_names.is_empty() {
            (scalar_plan, scalar_fallback)
        } else {
            (crate::sql::scalar_plan::ScalarPlan::IndexPartial, None)
        };
        let (engine, index_names) = self.explain_engine_for(
            read_txn,
            bound.table(),
            is_hybrid,
            is_precision,
            filters_empty,
            scalar_prefilter,
            scalar_plan,
            scalar_fallback,
            scalar_filter_column_names.as_deref(),
            &expression_index_names,
            &partial_index_names,
        );
        crate::sql::explain::build_search_explain_result(bound.mode(), &engine, &index_names)
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
        let (read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
        let bound = crate::sql::parser::bind_aggregate(validated, &schema, session.udfs())?;
        Ok(crate::sql::SqlOutcome::Explain(
            self.aggregate_explain_from_bound(&read_txn, &schema, &bound),
        ))
    }

    /// 束縛済み集計計画（[`crate::sql::parser::BoundAggregate`]）から
    /// `QUERY PLAN` 応答を組み立てる（Issue #948・NOSQL-16・SQL-27・
    /// TASK-186）。[`Self::run_relational_explain_aggregate`]（SQL
    /// `EXPLAIN SELECT <集計>` テキスト経由）と
    /// [`Self::explain_bound_aggregate_in_session`]（NoSQL 表層の `aggregate`
    /// op の `explain: true`）の両方が共有する（第 2 の実装を作らない設計）。
    /// `classify_aggregate_access` 自体はテーブルスキーマと束縛結果・
    /// `target`（後述）だけから静的判定する純粋関数のままだが、Issue #1066 で
    /// `scalar_plan:` 行への使用索引名注記を追加したため、カタログ読み取り
    /// （`self.storage`）が必要になり `&self` メソッドへ変更した
    /// （`classify_aggregate_access` 自体の判定式は変えない）。`access_path` が
    /// `ScalarIndexCandidates`（索引経由の候補削減が使える形）のときに限り、
    /// `metadata_filters` の列がすべて対象テーブルのスカラー宣言で被覆されて
    /// いるかを `read_txn`（Issue #1066 PR #1155 codex-review P1 指摘・
    /// 3 巡目対応: `bind_aggregate` に使ったスキーマと同一の `read_txn_with_
    /// schema` 由来スナップショット。新規に `begin_read()` し直さない）で
    /// 判定する（ann は常に対象外——集計 EXPLAIN は `ann_plan:` 行を出さない）。
    /// カタログ読み取り失敗・列名写像失敗は fail-closed に名前なしへ倒す。
    /// ここで付ける名前も `search_explain_from_bound` の `scalar_names` と
    /// 同じく「宣言上の候補索引名」であり、`ScalarIndex::resolve_candidates`
    /// の実行時縮退（選択度超過・候補取得不能）は意図的に反映しない
    /// （`scalar_index_names_for_columns` のドキュメンテーションコメント・
    /// `docs/design/explain-search-engine-exposure.md`「決定 1」節参照）。
    ///
    /// `target`（Issue #1153・TASK-206・INDEX-7）: 呼び出し元が `bound` を
    /// 束縛したのと同一のスナップショット（`read_txn`）から索引宣言
    /// （[`crate::sql::scalar_index::resolve_scalar_index_target_in_txn`]）を
    /// 解決し、[`crate::sql::aggregate::classify_aggregate_access`] へ渡す
    /// （`ensure_scalar_index_snapshot` が実際に構築する索引の対象選択と
    /// 一致させるため。`self.hnsw_state.is_some()` を「起動時 opt-in」の
    /// 上位スイッチとして使うのは `sql::exec`／`sql::aggregate` と同じ）。
    /// `target` により `access_path` 自体が `PlainScan`／`FullScan` 側へ
    /// 補正され得るため、直後の索引名注記の可否判定（`ScalarIndexCandidates`
    /// か）は必ずこの補正後の `access_path` を見る。
    fn aggregate_explain_from_bound(
        &self,
        read_txn: &redb::ReadTransaction,
        schema: &crate::catalog::TableSchema,
        bound: &crate::sql::parser::BoundAggregate,
    ) -> crate::sql::exec::QueryResult {
        let target = crate::sql::scalar_index::resolve_scalar_index_target_in_txn(
            read_txn,
            &schema.name,
            self.hnsw_state.is_some(),
        );
        // INDEX-10（Issue #1650）: 実行時と同じ式索引の一致判定を同一の `read_txn` から解決する。
        let expr_matcher = crate::sql::scalar_index::resolve_expression_matcher_in_txn(
            read_txn,
            &schema.name,
            schema,
            self.hnsw_state.is_some(),
            bound.expr_filters(),
        );
        let expr_finder = |e: &crate::sql::udf_call::BoundExpr,
                           ty: crate::sql::udf_call::ExprType| {
            expr_matcher.find(e, ty)
        };
        let (scalar_plan, scalar_fallback, access_path) =
            crate::sql::aggregate::classify_aggregate_access(schema, bound, &target, &expr_finder);
        // Issue #1066: `access_path`（上記 `target` 補正後の値）が
        // `ScalarIndexCandidates` のときに限り、`metadata_filters` の列が
        // すべて対象テーブルのスカラー宣言で被覆されているかを確認して索引名を
        // 注記する。カタログ読み取り失敗・列名写像失敗は fail-closed に名前
        // なしへ倒す（`Self::aggregate_explain_from_bound` のドキュメンテー
        // ションコメント参照）。
        let index_names = if matches!(
            access_path,
            crate::sql::explain::AccessPath::ScalarIndexCandidates
        ) && self.hnsw_state.is_some()
        {
            let expression_names = crate::sql::scalar_plan::matched_expression_index_names(
                bound.expr_filters(),
                &expr_finder,
            )
            .unwrap_or_default();
            match scalar_filter_column_names_with_numeric(
                schema,
                bound.metadata_filters(),
                bound.expr_filters(),
            ) {
                Some(cols) if !cols.is_empty() => {
                    match self.read_explain_index_names_in_txn(read_txn, bound.table()) {
                        Ok(decls) => crate::sql::explain::ExplainIndexNames::new(
                            Vec::new(),
                            scalar_index_names_with_expressions(
                                Some(decls.scalar.as_slice()),
                                &cols,
                                &expression_names,
                            ),
                        ),
                        Err(_) => crate::sql::explain::ExplainIndexNames::default(),
                    }
                }
                // 列述語が無く式述語だけ: 索引カタログを読まず、一致済みの式索引名だけを出す。
                Some(_) => {
                    crate::sql::explain::ExplainIndexNames::new(Vec::new(), expression_names)
                }
                None => crate::sql::explain::ExplainIndexNames::default(),
            }
        } else {
            crate::sql::explain::ExplainIndexNames::default()
        };
        crate::sql::explain::build_relational_explain_result(
            scalar_plan,
            scalar_fallback,
            access_path,
            &index_names,
        )
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
        let (read_txn, schema) = self.read_txn_with_schema(validated.table_name())?;
        let bound = crate::sql::parser::bind_scan_with_dummy_flags(
            validated,
            &schema,
            session.udfs(),
            &[],
            &self.result_limits,
        )?;
        let gins = Self::gin_decls_for_explain(&read_txn, &bound)?;
        Ok(crate::sql::SqlOutcome::Explain(Self::scan_explain_result(
            &schema, &bound, &gins,
        )))
    }

    /// 広域取得（scan）の `EXPLAIN` が GIN 相当索引の分類に使う宣言を読む（INDEX-8、Issue #2208）。
    /// 索引カタログは GIN の対象になりうる形の述語がある文でだけ読む
    /// （[`crate::sql::gin_scan::may_use_gin`]。それ以外の文でカタログ読み取りを増やさない）。
    /// 実行側（`sql::scan`）と同じ判定を共有するため、2 行は実経路と矛盾しない。
    fn gin_decls_for_explain(
        read_txn: &redb::ReadTransaction,
        bound: &crate::sql::parser::BoundScan,
    ) -> Result<Vec<crate::catalog::DeclaredGinIndex>, crate::sql::allowlist::SqlSurfaceError> {
        if !crate::sql::gin_scan::may_use_gin(bound) {
            return Ok(Vec::new());
        }
        crate::catalog::table_expression_decls_in_source(read_txn, bound.table())
            .map(|decls| decls.gins)
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "gin index declaration lookup failed".to_string(),
            })
    }

    /// 広域取得（scan）の `QUERY PLAN` 応答を組み立てる（Issue #948・
    /// NOSQL-16・SQL-27・TASK-186）。`sql::scan` は既定では
    /// `scalar_plan: plain_scan`／`access_path: full_scan` の全行走査で、次の 2 経路だけが静的分類で
    /// 候補を絞る（実行側 `sql::scan` と同じ分類関数を呼ぶので、2 行は実経路と矛盾しない。D5「矛盾出力の
    /// 防止」）。優先順位は PK 点検索 → GIN → 全走査。実行時の縮退（テナントごとのマーカー欠落による
    /// 行範囲走査）は実行時の事実であり、ここには露出しない（可視カーディナリティ・テナントの存在情報に
    /// 繋がる値を出さない）。
    ///
    /// - 宣言 PRIMARY KEY の全構成列へのリテラル等価（Issue #2198・TABLE-23）:
    ///   `index_primary_key`／`primary_key_lookup`。
    /// - jsonb 列への包含・キー存在・jsonpath 演算子と GIN 相当索引（Issue #2208・INDEX-8）:
    ///   `index_gin index=<索引名>`／`gin_index_candidates`。
    ///
    /// [`Self::run_relational_explain_scan`]（SQL `EXPLAIN SELECT ... LIMIT n` テキスト経由）と
    /// [`Self::explain_bound_scan_in_session`]（NoSQL 表層の `scan` op の `explain: true`）の両方が
    /// 共有する。`gins` は [`Self::gin_decls_for_explain`] が読んだ対象表の GIN 宣言。
    fn scan_explain_result(
        schema: &crate::catalog::TableSchema,
        bound: &crate::sql::parser::BoundScan,
        gins: &[crate::catalog::DeclaredGinIndex],
    ) -> crate::sql::exec::QueryResult {
        use crate::sql::explain::{AccessPath, ExplainIndexNames};
        use crate::sql::scalar_plan::ScalarPlan;
        let gin_plan = crate::sql::gin_scan::classify_gin_scan(schema, bound, gins);
        let (scalar_plan, access_path, names) =
            if crate::sql::pk_point::classify_pk_point(schema, bound).is_point() {
                (
                    ScalarPlan::IndexPrimaryKey,
                    AccessPath::PrimaryKeyLookup,
                    ExplainIndexNames::default(),
                )
            } else if let Some(index_name) = gin_plan.index_name() {
                (
                    ScalarPlan::IndexGin,
                    AccessPath::GinIndexCandidates,
                    ExplainIndexNames::new(Vec::new(), vec![index_name.to_string()]),
                )
            } else {
                (
                    ScalarPlan::PlainScan,
                    AccessPath::FullScan,
                    ExplainIndexNames::default(),
                )
            };
        // Issue #1896・SQL-64 (6): 窓付きの文は `classify_pk_point`／`classify_gin_scan` が
        // 索引経路を返さず、実行側も `sql::window::execute_window_scan`（全行走査）へ回るため、
        // 上の分岐は自然に `plain_scan`／`full_scan` となり実経路と矛盾しない。窓の評価段は
        // 静的な情報（関数名・キー数）だけを追記する。NoSQL 表層の `BoundScan` は常に窓なし。
        let windows: Vec<crate::sql::explain::ExplainWindowStage> = bound
            .windows()
            .iter()
            .map(|w| crate::sql::explain::ExplainWindowStage {
                func: w.func.default_alias(),
                partition_keys: w.partition_by.len(),
                order_keys: w.order_by.len(),
            })
            .collect();
        crate::sql::explain::build_relational_explain_result_with_windows(
            scalar_plan,
            None,
            access_path,
            &names,
            &windows,
        )
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
        self.run_explain_plan(ctx, session, table, question, mode_literal, None, bind)
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
        self.check_bound_scan_limit(&bound)?;
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

    /// 束縛済み検索計画（[`crate::sql::parser::BoundStatement`]）の
    /// `EXPLAIN`（Issue #948・NOSQL-16・SQL-27・TASK-186）。SQL テキストを
    /// 経由せず束縛済み計画の `QUERY PLAN` を得たい呼び出し元（`wire-server`
    /// の NoSQL 表層 `search` op の `vector` 指定 `explain: true`）向けの
    /// セッション対応エントリで、[`Self::execute_bound_search_in_session`]
    /// と同型の設計（`docs/design/bound-plan-session-entry.md` 参照）。
    ///
    /// 処理順序は execute 系と同一: (1) `table` のスキーマを取得、(2) `bind`
    /// で束縛、(3) 得られた計画の対象テーブルが `table` と一致するか検証。
    /// ただしその後は [`Self::run_select_plan`]（実行本体）を呼ばず、
    /// [`Self::search_explain_from_bound`]（SQL `EXPLAIN` 経路と共有する
    /// 私的ヘルパー）だけを呼ぶ。検索本体（`hnsw_state` の
    /// `lookup`／`prepare_*`・`SearchProvider::search`）は実行しないため、
    /// 索引構築・キャッシュ加算・行走査のいずれも発生しない。
    ///
    /// `ctx`（[`PolicyContext`]）は [`Self::execute_bound_search_in_session`]
    /// とのシグネチャ対称性のために残す（呼び出し元に
    /// `SessionPrincipal::policy_context()` 由来のテナント文脈を必ず通させる
    /// ため）。`EXPLAIN` は行を読まないため、本メソッド自身は `ctx` を
    /// テナント判定に使わない。
    pub fn explain_bound_search_in_session<F>(
        &self,
        _ctx: &PolicyContext,
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
        self.check_bound_limit(bound.limit())?;
        Ok(self.search_explain_from_bound(&read_txn, &schema, &bound))
    }

    /// 束縛済み広域取得計画（[`crate::sql::parser::BoundScan`]）の
    /// `EXPLAIN`（Issue #948・NOSQL-16・SQL-27・TASK-186）。
    /// [`Self::execute_bound_scan_in_session`] と同型の設計・処理順序
    /// （`docs/design/bound-plan-session-entry.md` 参照）で、実行本体
    /// （[`Self::run_scan_plan`]）を呼ばず [`Self::scan_explain_result`]
    /// （静的分類だけ。SQL `EXPLAIN` 経路と共有）を呼ぶ。`ctx` を残す理由・
    /// テナント判定への不使用は [`Self::explain_bound_search_in_session`]
    /// と同じ。
    pub fn explain_bound_scan_in_session<F>(
        &self,
        _ctx: &PolicyContext,
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
        self.check_bound_scan_limit(&bound)?;
        let gins = Self::gin_decls_for_explain(&read_txn, &bound)?;
        Ok(Self::scan_explain_result(&schema, &bound, &gins))
    }

    /// 束縛済み集計計画（[`crate::sql::parser::BoundAggregate`]。`GROUP BY`
    /// の有無を問わない）の `EXPLAIN`（Issue #948・NOSQL-16・SQL-27・
    /// TASK-186）。[`Self::execute_bound_aggregate_in_session`] と同型の
    /// 設計・処理順序（`docs/design/bound-plan-session-entry.md` 参照）で、
    /// 実行本体（[`Self::run_aggregate_plan`]。`VisibleBitmapCache`／
    /// `SqlArenaCache`／`ScalarIndexCache` を消費する）を呼ばず
    /// [`Self::aggregate_explain_from_bound`]（SQL `EXPLAIN` 経路と共有）
    /// だけを呼ぶため、いずれのキャッシュも変化しない。`ctx` を残す理由・
    /// テナント判定への不使用は [`Self::explain_bound_search_in_session`]
    /// と同じ。
    pub fn explain_bound_aggregate_in_session<F>(
        &self,
        _ctx: &PolicyContext,
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
        Ok(self.aggregate_explain_from_bound(&read_txn, &schema, &bound))
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
        self.execute_bound_insert_with_target(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            ctx,
            table,
            row_count,
            operation_id,
            bind,
        )
    }

    /// [`Self::execute_bound_insert_in_session`] の本体（書き込み先を `target` で
    /// 選べる版。Issue #1179）。`InTxn` は明示トランザクション内の `COPY FROM STDIN`
    /// （[`Self::commit_copy_in_txn`]）が共有 `redb::WriteTransaction` へ書き込む
    /// ために使い、`Autocommit` は従来の 1 文 1 commit と同一。
    fn execute_bound_insert_with_target<F>(
        &self,
        target: crate::tenant::WriteTarget<'_>,
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
        // SQL-37・Issue #1587: 束縛クロージャ（NoSQL insert の `bind_insert`）が列 `DEFAULT now()`
        // 系を評価する時刻（autocommit 相当の文の開始時刻）。束縛区間だけ保持する。
        // 束縛スコープは実書き込み前に破棄されるため、時刻 DEFAULT を評価していれば台帳 v3 の
        // 評価時刻をここで取り出し、書き込み区間へ引き継ぐ（Issue #2188）。
        let (bounds, bound_evaluated_at) = {
            let now = crate::sql::time_fn::now_micros()?;
            let _default_clock_scope = crate::sql::time_fn::DefaultClockScope::enter(
                crate::sql::time_fn::StatementClock {
                    txn_start_micros: now,
                    stmt_start_micros: now,
                },
            );
            let bounds = bind(&schema)?;
            (bounds, crate::sql::time_fn::ledger_evaluated_at())
        };
        // COPY など呼び出し元が既に上書きを設定している場合は、束縛が評価していないときに
        // それを潰さない。
        let _ledger_clock_scope =
            bound_evaluated_at.map(|at| crate::sql::time_fn::LedgerClockScope::enter(Some(at)));
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
        // NoSQL 表層はトリガの発火に未対応（`None`。INSERT トリガのある表は `0A000`。Issue #1723）。
        crate::sql::exec::execute_insert_batch_with_schema_in(
            target,
            ctx,
            &bounds,
            self.ledger_mode,
            Some(&schema),
            None,
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
    fn begin_copy_unscrubbed(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<CopyPlan, crate::sql::allowlist::SqlSurfaceError> {
        Self::check_copy_search_path(session, sql)?;
        if let Some(plan) = self.begin_copy_temp(
            ctx,
            session,
            None::<&mut crate::sql::transaction::SessionTransaction<'_>>,
            sql,
        )? {
            return Ok(plan);
        }
        let resolver = self.schema_resolver(ctx);
        let copy_tokens =
            crate::sql::relation_name::tokenize_resolved_with(sql, false, Some(&resolver))?;
        let stmt = crate::sql::allowlist::validate_copy_tokens(
            &copy_tokens,
            &self.storage,
            self.ledger_mode,
        )
        .map_err(|e| self.reclassify_matview_write_error(e))?;
        self.begin_copy_validated(ctx, session, stmt, None)
    }

    /// `COPY` の未修飾テーブル参照に対する `search_path` の参照ゲート（TABLE-27・Issue #1715）。
    /// 実効 path に `public` が含まれる既定のセッションは字句解析もしない。字句解析に失敗する入力は
    /// 通常の検証へ任せる。
    fn check_copy_search_path(
        session: &crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        if !session.search_path_hides_public() {
            return Ok(());
        }
        let Ok(tokens) = crate::sql::lexer::tokenize_marked(sql) else {
            return Ok(());
        };
        crate::sql::search_path::check_unqualified_refs(session, &tokens, &[])
    }

    /// `begin_copy_unscrubbed` の公開入口。判定順序・エラー契約・実行結果は同関数と同一で、
    /// 出口でだけ SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を利用者向けの
    /// `<schema>.<name>` へ戻す（TABLE-27・Issue #1717、ADR D2。内部 ID を文言へ出さない）。
    pub fn begin_copy(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        sql: &str,
    ) -> Result<CopyPlan, crate::sql::allowlist::SqlSurfaceError> {
        Self::check_resolution_tenant(session, ctx)?;
        self.begin_copy_unscrubbed(ctx, session, sql)
            .map_err(|e| self.scrub_error(ctx, e))
    }

    /// [`Self::begin_copy`] の検証済み文を受け取る本体（[`Self::begin_copy_in_txn`] と共有）。
    fn begin_copy_validated(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: crate::sql::allowlist::CopyStatement,
        txn_start_micros: Option<i64>,
    ) -> Result<CopyPlan, crate::sql::allowlist::SqlSurfaceError> {
        match stmt {
            crate::sql::allowlist::CopyStatement::From(v) => {
                let (_read_txn, schema) = self
                    .read_txn_with_schema(&v.table_name)
                    .map_err(|e| self.reclassify_matview_write_error(e))?;
                // SQL-37・Issue #1587: COPY の行ごとの束縛は複数の wire メッセージにまたがるため、
                // 列 `DEFAULT now()` 系の評価時刻（文の開始時刻・明示トランザクションでは
                // トランザクション開始時刻）をここで確定してセッションへ持たせる。
                let stmt_start_micros = crate::sql::time_fn::now_micros()?;
                let clock = crate::sql::time_fn::StatementClock {
                    txn_start_micros: txn_start_micros.unwrap_or(stmt_start_micros),
                    stmt_start_micros,
                };
                Ok(CopyPlan::From(crate::sql::copy::CopyInSession::new(
                    v.table_name,
                    v.columns,
                    v.format,
                    v.options,
                    v.operation_id,
                    schema,
                    self.batch_limits,
                    clock,
                    session.time_zone().clone(),
                )?))
            }
            crate::sql::allowlist::CopyStatement::To(v) => {
                let (read_txn, schema) = self.read_txn_with_schema(v.inner.table_name())?;
                let result = self.run_copy_to_scan(&read_txn, ctx, session, &schema, &v)?;
                Ok(CopyPlan::To(v.format, result, v.options))
            }
        }
    }

    /// [`Self::begin_copy`] の明示トランザクション対応版（Issue #1179）。`wire-server` の
    /// `COPY` 入口（`copy::run`）が接続の [`crate::sql::transaction::SessionTransaction`]
    /// とともに呼ぶ。
    ///
    /// - `Idle`: [`Self::begin_copy`] と同一。
    /// - `Failed`: 実行せず `25P02` 相当のエラーを返す。
    /// - `InTransaction`: `COPY FROM STDIN` は受け入れ開始のみ（書き込みは
    ///   [`Self::commit_copy_in_txn`] が行う）。`COPY (...) TO STDOUT` は他の読み取り文と
    ///   同じく、自トランザクションが未 commit の変更を持つ間は共有書き込み
    ///   トランザクションを読み取り源にして走査し、未 commit の変更を反映する。
    ///   失敗時はトランザクションを `Failed` へ遷移させる。
    fn begin_copy_in_txn_unscrubbed<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        sql: &str,
    ) -> Result<CopyPlan, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::transaction::TransactionStatus;
        match txn.status() {
            TransactionStatus::Idle => self.begin_copy(ctx, session, sql),
            TransactionStatus::Failed => Err(txn.take_failed_error()),
            TransactionStatus::InTransaction => {
                let result = self.begin_copy_active(ctx, session, txn, sql);
                if result.is_err() {
                    txn.fail();
                }
                result
            }
        }
    }

    /// `begin_copy_in_txn_unscrubbed` の公開入口。判定順序・エラー契約・実行結果は同関数と同一で、
    /// 出口でだけ SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を利用者向けの
    /// `<schema>.<name>` へ戻す（TABLE-27・Issue #1717、ADR D2。内部 ID を文言へ出さない）。
    pub fn begin_copy_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        sql: &str,
    ) -> Result<CopyPlan, crate::sql::allowlist::SqlSurfaceError> {
        Self::check_resolution_tenant(session, ctx)?;
        self.begin_copy_in_txn_unscrubbed(ctx, session, txn, sql)
            .map_err(|e| self.scrub_error(ctx, e))
    }

    fn begin_copy_active<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        sql: &str,
    ) -> Result<CopyPlan, crate::sql::allowlist::SqlSurfaceError> {
        Self::check_copy_search_path(session, sql)?;
        if let Some(plan) = self.begin_copy_temp(ctx, session, Some(&mut *txn), sql)? {
            return Ok(plan);
        }
        let resolver = self.schema_resolver(ctx);
        let copy_tokens =
            crate::sql::relation_name::tokenize_resolved_with(sql, false, Some(&resolver))?;
        let stmt = crate::sql::allowlist::validate_copy_tokens(
            &copy_tokens,
            &self.storage,
            self.ledger_mode,
        )
        .map_err(|e| self.reclassify_matview_write_error(e))?;
        if let crate::sql::allowlist::CopyStatement::To(v) = &stmt {
            // 通常の読み取り文と同じ文実行前チェック（文数・持続時間の上限。
            // 超過時は `Failed` へ遷移して拒否する）。`COPY FROM` は
            // [`Self::commit_copy_in_txn`] が同じ検査を行う。
            txn.check_and_register_statement(None)?;
            // 自トランザクションが未 commit の変更を持つ間は、共有書き込み
            // トランザクションを読み取り源にして走査する（他の読み取り文と同じ
            // 方針。[`Self::read_only_in_active_txn`] 参照）。
            if !txn.dirty_tables()?.is_empty() {
                let write_txn = txn.write_txn().ok_or_else(|| {
                    crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    }
                })?;
                let schema = table_schema_in(write_txn, v.inner.table_name())?;
                let result = self.run_copy_to_scan(write_txn, ctx, session, &schema, v)?;
                return Ok(CopyPlan::To(v.format, result, v.options.clone()));
            }
        }
        self.begin_copy_validated(ctx, session, stmt, txn.txn_start_micros())
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
        // 一時表向けのバッチはセッションを要する `commit_copy_in_txn_in_session` だけが扱う
        // （`ON COMMIT` の適用に `SessionState` が要る。永続 DB へ書く経路には絶対に載せない）。
        if batch.temp.is_some() {
            return Err(crate::sql::temp_catalog::temp_table_unsupported());
        }
        // 出口で SCHEMA 配下の relation の内部キー（`#<id>.<name>`）を表示名へ戻す
        // （COPY 開始後に別接続が表を削除した場合の `UndefinedTable` 等。ADR D2・Issue #1717）。
        self.commit_copy_in_with_target(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            ctx,
            batch,
        )
        .map_err(|e| self.scrub_error(ctx, e))
    }

    /// [`Self::commit_copy_in`] の明示トランザクション対応版（Issue #1179）。
    /// `wire-server` の `COPY FROM STDIN` 完了処理（`copy::finish_copy_from`）が、
    /// 接続の [`crate::sql::transaction::SessionTransaction`] とともに呼ぶ。
    ///
    /// - `Idle`: 従来どおり autocommit で 1 文 1 commit（[`Self::commit_copy_in`] と同一）。
    /// - `InTransaction`: 他の書き込み文と同じ順序（文数・持続時間・`operation_id`
    ///   再利用の検査、共有 `write_txn` への複数行 INSERT、`mark_written`）で実行する。
    ///   失敗時はトランザクションを `Failed` へ遷移させる（commit は `COMMIT` 文のみ）。
    /// - `Failed`: 実行せず `25P02` 相当のエラーを返す。
    pub fn commit_copy_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        batch: crate::sql::copy::CopyInBatch,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.commit_copy_in_txn_unscrubbed(ctx, txn, batch)
            .map_err(|e| self.scrub_error(ctx, e))
    }

    /// [`Self::commit_copy_in_txn`] の本体。出口の内部キー表示変換（`scrub_error`）だけが公開入口と異なる。
    fn commit_copy_in_txn_unscrubbed<'e>(
        &'e self,
        ctx: &PolicyContext,
        txn: &mut crate::sql::transaction::SessionTransaction<'e>,
        batch: crate::sql::copy::CopyInBatch,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::transaction::TransactionStatus;
        if batch.temp.is_some() {
            return Err(crate::sql::temp_catalog::temp_table_unsupported());
        }
        match txn.status() {
            TransactionStatus::Idle => self.commit_copy_in(ctx, batch),
            TransactionStatus::Failed => Err(txn.take_failed_error()),
            TransactionStatus::InTransaction => {
                txn.check_and_register_statement(
                    batch.operation_id.as_ref().map(|id| id.as_str()),
                )?;
                let table = batch.table.clone();
                let result = match txn.write_txn() {
                    Some(write_txn) => self.commit_copy_in_with_target(
                        crate::tenant::WriteTarget::InTxn(write_txn),
                        ctx,
                        batch,
                    ),
                    None => Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    }),
                };
                match result {
                    Ok(outcome) => {
                        txn.mark_written(ctx.tenant_id(), &table);
                        Ok(outcome)
                    }
                    Err(e) => {
                        txn.fail();
                        Err(e)
                    }
                }
            }
        }
    }

    fn commit_copy_in_with_target(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        batch: crate::sql::copy::CopyInBatch,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let crate::sql::copy::CopyInBatch {
            table,
            row_count,
            operation_id,
            bounds,
            schema: bound_schema,
            evaluated_at,
            // 一時表向けの印は呼び出し元（`commit_copy_in*` の各入口）が振り分け済み。ここでは使わない。
            temp: _,
        } = batch;
        // COPY の束縛は wire メッセージをまたいで完了済みのため、台帳 v3 の評価時刻を
        // 書き込み区間へ明示的に渡す（Issue #2188。None は v2 の権威値）。
        let _ledger_clock_scope = crate::sql::time_fn::LedgerClockScope::enter(evaluated_at);
        self.execute_bound_insert_with_target(
            target,
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

    /// [`Self::execute_bound_delete_in_session`] の束縛時スキーマ検査付き版（Issue #2199）。
    /// `check` が受け取るスキーマは実行（write トランザクション内の再照合）と同一世代で、
    /// `check` が `Err` ならば書き込まずにそのまま返す。write 側は束縛時スキーマと現スキーマの
    /// 不一致を fail-closed で拒否するため、検査後に並行 DDL で表が差し替わっても素通りしない。
    /// `wire-server::http::query::delete` の単一行形が、採番方式の表を `id` 直指定で
    /// 触らせないために使う。`operation_id` 必須化ガードはスキーマ取得より前に判定する。
    pub fn execute_bound_delete_checked_in_session<F>(
        &self,
        ctx: &PolicyContext,
        bound: &crate::sql::parser::BoundDelete,
        check: F,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
        ) -> Result<(), crate::sql::allowlist::SqlSurfaceError>,
    {
        let ledger_write = self
            .ledger_mode
            .resolve(bound.operation_id.as_ref())
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;
        let (read_txn, schema) = self.read_txn_with_schema(&bound.table)?;
        check(&schema)?;
        drop(read_txn);
        match crate::tenant::delete_row_ledgered_capturing_unchecked(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            &bound.table,
            ctx,
            bound.id,
            ledger_write,
            Some(&schema),
            None,
        ) {
            Ok((crate::tenant::DeleteRowOutcome::Deleted, _)) => {
                Ok(crate::sql::exec::DeleteOutcome { rows_affected: 1 })
            }
            Ok((crate::tenant::DeleteRowOutcome::NotFound, _)) => {
                Ok(crate::sql::exec::DeleteOutcome { rows_affected: 0 })
            }
            Err(e) => Err(crate::sql::exec::map_write_error(e, "delete")),
        }
    }

    /// NoSQL 表層 `update` op の `filter`（述語形。TASK-186・NOSQL-12、
    /// Issue #1062・対象ビヘイビア NOSQL-6・NOSQL-12・SQL-19・RECOVER-1／10／11）
    /// を SQL 表層 `UPDATE ... WHERE <述語> USING OPERATION_ID`（SQL-19・
    /// TASK-192）と**同一の実行本体**（[`Self::run_predicate_update`]）へ
    /// 到達させるセッション対応の束縛済み入口（[`Self::
    /// execute_bound_update_in_session`]（単一行形）と同型の closure 方式。
    /// 第 2 の実行器を作らない設計）。
    ///
    /// 判定順序（fail-closed。呼び出し元 `wire-server::http::query::update`
    /// は本メソッドへ到達する前に `table`／`set` の識別子検査・
    /// `OperationId::parse`（`23502`）を済ませている前提）:
    /// 1. `self.ledger_mode.resolve(operation_id)` による必須化ガード
    ///    （`23502`。スキーマ取得より前）
    /// 2. スキーマ取得（`42P01`）
    /// 3. `bind` closure で SET 割当・`WHERE` 述語列を得る（呼び出し元は
    ///    `wire-server::http::query::update::map_set_assignments` と
    ///    `filter::bind_filter_where_predicates` を使う）
    /// 4. `bind` が返した `WherePredicate` 列を検査する（等価・前方一致・範囲比較・
    ///    `IN`・`BETWEEN`・`IS NULL`・列×数値リテラルの式比較・直接包む `Not`・
    ///    `Or` のみ許可し、空列を含むそれ以外〔`PredicateCall`／`BoolColumn`／
    ///    `Not(Not)` 等／`InSubquery`／`Exists`〕は `42601` で拒否。
    ///    [`reject_unsupported_predicate_dml_forms`] 参照）
    /// 5. ガード済みの `operation_id`（`bind` closure の外で確定済みの
    ///    引数そのもの）から [`crate::sql::allowlist::ValidatedPredicateUpdate`]
    ///    を engine 内部で構築する（`pub(crate)` フィールドへの struct
    ///    リテラルで構築するため公開コンストラクタは追加しない。
    ///    `bound.operation_id` との一致検証（[`Self::
    ///    execute_bound_update_in_session`] 判定 3 参照）は、本メソッド自身が
    ///    `operation_id` から直接 [`crate::sql::allowlist::
    ///    ValidatedPredicateUpdate::operation_id`] を組み立てるため不要
    ///    ——`bind` closure は `operation_id` を運ばない契約にする）
    /// 6. `read_txn` を drop してから共通本体（[`Self::run_predicate_update`]）
    ///    へ渡す
    ///
    /// UDF レジストリは [`crate::sql::udf_call::UdfRegistry::default()`] を
    /// 渡す（NoSQL 表層にはセッション UDF が無く、wire 層は列×数値リテラルの比較に
    /// 限って `Expression` 述語を生成する——判定 4 が `Expr::Call` を拒否することの
    /// 前提でもある）。
    pub fn execute_bound_predicate_update_in_session<F>(
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
            (
                Vec<(String, crate::sql::allowlist::InsertLiteral)>,
                Vec<crate::sql::allowlist::WherePredicate>,
            ),
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        // 判定 1: `operation_id` 必須化ガード（スキーマ取得より前）。
        self.ledger_mode
            .resolve(operation_id)
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;

        // 判定 2・3: スキーマ取得 → 束縛（同一 read_txn 下）。
        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let (assignments, where_predicates) = bind(&schema)?;
        // 判定 4: wire 表層が生成しない述語形への多層防御（空列を含む）。
        reject_unsupported_predicate_dml_forms(&where_predicates)?;
        drop(read_txn);

        // 判定 5: engine 内部でのみ検証済み `ValidatedPredicateUpdate` を構築する
        // （`pub(crate)` フィールドは同一クレート内の struct リテラルで構築可能。
        // `#[non_exhaustive]` はクレート外からの構築のみを禁じる）。
        // NoSQL 表層の SET 値は常に平易なリテラル（式は SQL 表層のみ。Issue #1544）。
        let assignments = assignments
            .into_iter()
            .map(|(name, literal)| (name, crate::sql::allowlist::SetValue::Literal(literal)))
            .collect();
        let validated = crate::sql::allowlist::ValidatedPredicateUpdate {
            table_name: table.to_string(),
            assignments,
            where_predicates,
            operation_id: operation_id.cloned(),
            returning: None,
            partitioned: None,
        };
        let udfs = crate::sql::udf_call::UdfRegistry::default();
        // 判定 6: 実書き込み（`Self::run_predicate_update` 内部の独自 write トランザクション）。
        self.run_predicate_update(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            ctx,
            &udfs,
            &validated,
            &schema,
        )
    }

    /// NoSQL 表層 `delete` op の `filter`（述語形。TASK-186・NOSQL-12、
    /// Issue #1062）を SQL 表層 `DELETE FROM ... WHERE <述語>
    /// USING OPERATION_ID`（SQL-19・TASK-192）と**同一の実行本体**
    /// （[`Self::run_predicate_delete`]）へ到達させるセッション対応の
    /// 束縛済み入口。[`Self::execute_bound_predicate_update_in_session`] と
    /// 同じ判定順序・設計（`bind` closure は `WHERE` 述語列のみを返す点のみ
    /// 異なる。`DELETE` は `SET` を持たない）。
    pub fn execute_bound_predicate_delete_in_session<F>(
        &self,
        ctx: &PolicyContext,
        table: &str,
        operation_id: Option<&crate::recovery::required_op_id::OperationId>,
        bind: F,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
        ) -> Result<
            Vec<crate::sql::allowlist::WherePredicate>,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        self.ledger_mode
            .resolve(operation_id)
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;

        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let where_predicates = bind(&schema)?;
        reject_unsupported_predicate_dml_forms(&where_predicates)?;
        drop(read_txn);

        let validated = crate::sql::allowlist::ValidatedPredicateDelete {
            table_name: table.to_string(),
            where_predicates,
            operation_id: operation_id.cloned(),
            returning: None,
            partitioned: None,
        };
        let udfs = crate::sql::udf_call::UdfRegistry::default();
        self.run_predicate_delete(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            ctx,
            &udfs,
            &validated,
            &schema,
        )
    }

    /// NoSQL 表層 `update` op の分割実行修飾（`mode: partitioned`。Issue #1130・
    /// NOSQL-12・SQL-19・RECOVER-11、ADR `docs/design/partitioned-dml.md` 10 節）。
    /// `wire-server::http::query::update` から呼ばれ、SQL 表層
    /// `UPDATE ... PARTITIONED [CHUNK n]` と**同一の共通本体**
    /// （[`Self::run_partitioned_update`]）へ到達させる（内容照合ハッシュ・実行器・上限・
    /// 失敗写像が構造的に同一。第 2 の実行器を作らない）。
    ///
    /// 判定順序（fail-closed）:
    /// 1. `ledger_mode.resolve`（`23502`）
    /// 2. `chunk_rows` のサーバー幅検査（`22000`。テーブル不存在より先）
    /// 3. スキーマ取得（`42P01`）
    /// 4. `bind` closure（SET 割当・述語列）
    /// 5. 非許可の述語形の拒否（`42601`。[`reject_unsupported_predicate_dml_forms`]）
    /// 6. `read_txn` を drop して共通本体へ
    pub fn execute_bound_partitioned_update_in_session<F>(
        &self,
        ctx: &PolicyContext,
        table: &str,
        operation_id: Option<&crate::recovery::required_op_id::OperationId>,
        chunk_rows: Option<std::num::NonZeroUsize>,
        bind: F,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
        ) -> Result<
            (
                Vec<(String, crate::sql::allowlist::InsertLiteral)>,
                Vec<crate::sql::allowlist::WherePredicate>,
            ),
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        // 分割実行は構成に関わらず operation_id 必須（Issue #1704・RECOVER-1）。
        crate::recovery::required_op_id::require_partitioned(operation_id)
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;
        let clause = crate::sql::partitioned::PartitionedClause { chunk_rows };
        let limits =
            crate::sql::partitioned::apply_chunk_override(&clause, &self.partitioned_dml_limits)?;

        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let (assignments, where_predicates) = bind(&schema)?;
        reject_unsupported_predicate_dml_forms(&where_predicates)?;
        drop(read_txn);

        // NoSQL 表層の SET 値は常に平易なリテラル（式は SQL 表層のみ。Issue #1544）。
        let assignments = assignments
            .into_iter()
            .map(|(name, literal)| (name, crate::sql::allowlist::SetValue::Literal(literal)))
            .collect();
        let validated = crate::sql::allowlist::ValidatedPredicateUpdate {
            table_name: table.to_string(),
            assignments,
            where_predicates,
            operation_id: operation_id.cloned(),
            returning: None,
            partitioned: Some(clause),
        };
        let udfs = crate::sql::udf_call::UdfRegistry::default();
        self.run_partitioned_update(ctx, &udfs, &validated, &schema, &limits)
    }

    /// NoSQL 表層 `delete` op の分割実行修飾（Issue #1130）。
    /// [`Self::execute_bound_partitioned_update_in_session`] と同じ判定順序・設計
    /// （`bind` は述語列のみを返す）。
    pub fn execute_bound_partitioned_delete_in_session<F>(
        &self,
        ctx: &PolicyContext,
        table: &str,
        operation_id: Option<&crate::recovery::required_op_id::OperationId>,
        chunk_rows: Option<std::num::NonZeroUsize>,
        bind: F,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError>
    where
        F: FnOnce(
            &crate::catalog::TableSchema,
        ) -> Result<
            Vec<crate::sql::allowlist::WherePredicate>,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    {
        // 分割実行は構成に関わらず operation_id 必須（Issue #1704・RECOVER-1）。
        crate::recovery::required_op_id::require_partitioned(operation_id)
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;
        let clause = crate::sql::partitioned::PartitionedClause { chunk_rows };
        let limits =
            crate::sql::partitioned::apply_chunk_override(&clause, &self.partitioned_dml_limits)?;

        let (read_txn, schema) = self.read_txn_with_schema(table)?;
        let where_predicates = bind(&schema)?;
        reject_unsupported_predicate_dml_forms(&where_predicates)?;
        drop(read_txn);

        let validated = crate::sql::allowlist::ValidatedPredicateDelete {
            table_name: table.to_string(),
            where_predicates,
            operation_id: operation_id.cloned(),
            returning: None,
            partitioned: Some(clause),
        };
        let udfs = crate::sql::udf_call::UdfRegistry::default();
        self.run_partitioned_delete(ctx, &udfs, &validated, &schema, &limits)
    }

    /// NoSQL 表層 `show_partitioned_dml` op（Issue #1130・RLS-9・RECOVER-11）。
    /// SQL `SHOW PARTITIONED DML '<id>' ON <table>` と同じ
    /// [`crate::sql::partitioned::execute_show`] を呼ぶ。テーブル名は識別子の構文検査のみで
    /// ユーザーテーブルのカタログは引かない（ジョブなし・他テナント・テーブルなし・
    /// 見えないテーブルは同一応答）。`wire-server::http::query::partitioned` から呼ばれる。
    pub fn show_partitioned_dml_in_session(
        &self,
        ctx: &PolicyContext,
        table: &str,
        operation_id: &crate::recovery::required_op_id::OperationId,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        let job = Self::partitioned_job_ref(table, operation_id)?;
        crate::sql::partitioned::execute_show(&self.storage, ctx, &job)
    }

    /// NoSQL 表層 `cancel_partitioned_dml` op（Issue #1130）。SQL `CANCEL PARTITIONED DML`
    /// と同じ [`crate::sql::partitioned::execute_cancel`] を呼ぶ（該当なしは常に同じ応答。RLS-9）。
    pub fn cancel_partitioned_dml_in_session(
        &self,
        ctx: &PolicyContext,
        table: &str,
        operation_id: &crate::recovery::required_op_id::OperationId,
    ) -> Result<crate::sql::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
        let job = Self::partitioned_job_ref(table, operation_id)?;
        crate::sql::partitioned::execute_cancel(&self.storage, ctx, &job)
    }

    /// show／cancel 共通: 識別子の構文検査（SQL 表層と同じ固定文言の `42601`）の後で
    /// [`crate::sql::partitioned::PartitionedJobRef`] を engine 内部で構築する。
    fn partitioned_job_ref(
        table: &str,
        operation_id: &crate::recovery::required_op_id::OperationId,
    ) -> Result<crate::sql::partitioned::PartitionedJobRef, crate::sql::allowlist::SqlSurfaceError>
    {
        crate::catalog::validate_identifier(table).map_err(|_| {
            crate::sql::allowlist::SqlSurfaceError::unsupported("invalid table identifier")
        })?;
        Ok(crate::sql::partitioned::PartitionedJobRef {
            table: table.to_string(),
            operation_id: operation_id.clone(),
        })
    }

    /// `--max-insert-rows`（`self.dml_limits.max_insert_rows_per_statement`）を、
    /// `stmt.rows.len()`（`ValidatedInsert` の行数。単一行形も `rows.len() == 1`
    /// として同じ形を持つ）に対して**実行時にも**検査する（codex-review P1
    /// 指摘・PR #1122）。
    ///
    /// 行数上限は本来 `sql::allowlist::Parser::parse_insert`（構文解析段）が
    /// 判定する契約だが、`validate_insert`（`pub fn`。`max_insert_rows: None`
    /// 固定で解析する）が返した `ValidatedInsert` を `ParsedSql::Insert` へ
    /// 包んで [`Self::execute_parsed_in_session`]（`pub fn`）へ渡す経路では、
    /// 解析時の上限判定を経由しない。設定済みの `EngineCore`（`--max-insert-
    /// rows` 指定済み）に対してこの経路で上限超過の文を渡すと、構文解析段の
    /// ゲートを迂回して素通りしてしまう（AGENTS.md の公開 API・エラー契約の
    /// 相互運用性、および設定した上限の契約に反する）。本メソッドを
    /// `execute_insert_form`・`execute_insert_returning_form` の冒頭
    /// （スキーマ取得・書き込みトランザクション開始より前）で呼ぶことで、
    /// 到達経路に関わらず設定済み上限を必ず適用する（超過は `54000`・
    /// 副作用ゼロ。メッセージは構文解析段と同一形式に揃える）。
    fn check_insert_row_count_limit(
        &self,
        row_count: usize,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        let Some(max_insert_rows) = self.dml_limits.max_insert_rows_per_statement else {
            return Ok(());
        };
        if row_count > max_insert_rows.get() {
            return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                format!("INSERT statement exceeds the allowed row count ({max_insert_rows})"),
            ));
        }
        Ok(())
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
                    // TIME 値は行コーデック上 8 バイト固定（Issue #1569）。
                    crate::row_codec::Value::Time(_) => 8,
                    crate::row_codec::Value::TimestampTz(_) => 8,
                    // INTERVAL 値は行コーデック上 16 バイト固定（Issue #1579）。
                    crate::row_codec::Value::Interval(_) => 16,
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
            // 採番の印（`nextval`／`currval`・`DEFAULT nextval`）の列は仮値 `Null`（0 バイト計上）で
            // 運ばれるが、書き込み時は整数 1 値が入る。採番せずに固定幅の上限（BIGINT 相当の 8 バイト。
            // SMALLINT／INTEGER 列でも上限側へ倒す fail-closed）を計上し、INDEX-4 の行別・バッチ合計
            // バイト上限を迂回できないようにする（Issue #1690・codex-review P1）。
            for volatile in &bound.volatile_default_columns {
                if matches!(
                    volatile,
                    crate::sql::parser::VolatileColumn::Sequence(..)
                        | crate::sql::parser::VolatileColumn::DefaultSequence(_)
                ) {
                    row_bytes = row_bytes
                        .checked_add(std::mem::size_of::<i64>())
                        .ok_or_else(|| {
                            crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                                "INSERT batch row byte size overflow",
                            )
                        })?;
                }
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
        dict_txn: Option<&redb::WriteTransaction>,
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

        let expansion = self
            .expand_query(ctx, table, question, dict_txn)
            .map(|(expansion, _classification)| expansion)
            .map_err(|e| crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: format!("USING PLAN query expansion failed: {e}"),
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
        // 利用者が渡す生の表名は裸の識別子だけを受理する（SCHEMA 配下の内部キー `#<id>.<name>` を直接
        // 指定して他テナントの SCHEMA の表を操作する経路を作らない。TABLE-27・Issue #1717、ADR D2）。
        crate::catalog::validate_identifier(table)
            .map_err(crate::tenant::TenantWriteError::from)?;
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
        // 生の表名は裸の識別子だけ（[`Self::insert_row`] と同じ理由）。
        crate::catalog::validate_identifier(table)
            .map_err(crate::tenant::TenantWriteError::from)?;
        let ledger_write = self.ledger_mode.resolve(operation_id)?;
        crate::tenant::update_row_unchecked(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            table,
            ctx,
            id,
            row,
            ledger_write,
        )
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
        // 生の表名は裸の識別子だけ（[`Self::insert_row`] と同じ理由）。
        crate::catalog::validate_identifier(table)
            .map_err(crate::tenant::TenantWriteError::from)?;
        let ledger_write = self.ledger_mode.resolve(operation_id)?;
        crate::tenant::delete_row_unchecked(&self.storage, table, ctx, id, ledger_write)
    }

    /// `table` の永続一意索引（`user_uniq/{table}`）が「完全に構築済み」という
    /// 不変条件を、**書き込みを一切行わずに**検証する（クラッシュ耐性検証専用の
    /// 診断 API。Issue #1070・PR #1123 レビュー対応。`examples/crash_tool_unique_index.rs`
    /// の `verify` から使う想定）。`insert_row`・`execute_insert_sql` 等の書き込み
    /// 経路が持つ遅延バックフィル（`constraint` モジュールドキュメント参照）を
    /// 経由しない——バックフィルは索引欠落を検出次第静かに再構築してしまうため、
    /// クラッシュ後の索引欠落そのものを検出する用途には使えない
    /// （[`crate::constraint::verify_unique_index_read_only`] 参照）。マーカー
    /// 不在・不一致、または生存行のいずれかが正引きエントリを欠く場合は `Err` を
    /// 返す。主キーも UNIQUE 制約も宣言しないテーブルは即座に成功する。
    pub fn verify_unique_index_read_only(
        &self,
        ctx: &PolicyContext,
        table: &str,
    ) -> Result<(), crate::tenant::TenantWriteError> {
        let snapshot = self.storage.begin_read()?;
        let read_txn = snapshot.raw_txn();
        let schema = crate::catalog::get_table_schema_in_txn(read_txn, table)?;
        crate::constraint::verify_unique_index_read_only(read_txn, table, &schema, ctx.tenant_id())
    }

    /// GIN 相当索引 `index_name`（INDEX-8、Issue #2207）の自テナントの索引内容を、**書き込みを一切行わずに**
    /// 全行走査の抽出結果と突き合わせて検証する診断 API。マーカーの一致・欠落ポスティング数（不変条件違反）・
    /// stale 数（衛生指標）を返す。再構築・遅延バックフィルは行わない（欠落を静かに直さず検出するため）。
    /// 索引が無い・GIN 索引でない場合は `Err`。他テナントの範囲は読まない。
    pub fn verify_gin_index_read_only(
        &self,
        ctx: &PolicyContext,
        index_name: &str,
    ) -> Result<crate::catalog::GinIndexAudit, crate::tenant::TenantWriteError> {
        let snapshot = self.storage.begin_read()?;
        let read_txn = snapshot.raw_txn();
        let Some((table, decl)) = crate::catalog::gin_decl_in_read_txn(read_txn, index_name)?
        else {
            return Err(crate::tenant::TenantWriteError::Catalog(
                crate::catalog::CatalogError::IndexNotFound(index_name.to_string()),
            ));
        };
        let schema = crate::catalog::get_table_schema_in_txn(read_txn, &table)?;
        crate::constraint::verify_gin_index_read_only(
            read_txn,
            &table,
            &schema,
            ctx.tenant_id(),
            &decl,
        )
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
        // Issue #1065: 起動時 opt-in（`hnsw_state.is_some()`）に加え、テーブル
        // 単位の適格性ゲート（`catalog::hnsw_targeted_in_txn`。`sql::exec` の
        // `hnsw_enabled` と同一の判定。`self.hnsw_scope` が `Declared` なら
        // `USING hnsw` 宣言テーブルのみ。他テーブルの宣言に影響されず、索引
        // カタログを読み取れない場合は `false`）も満たす場合のみ HNSW 経路へ進む。この判定専用に新規の
        // read txn を開く（検索本体の `search_with_hnsw`／`search_with` は
        // 内部で別途 txn を開くため、ここでの読み取りは判定用の一時的なもの）。
        // 読み取り失敗は fail-closed に「対象外」（brute-force）へ倒す。
        // `hnsw_state.is_none()`（起動時 opt-in なしの既定デプロイ）では
        // この read txn 自体を開かない――HNSW opt-in が無効な限り
        // `search_with_hnsw` 経路には絶対に進まないため、判定用の読み取りは
        // 無駄なホットパスコストにしかならない（既定デプロイの回帰防止）。
        let (hnsw_targeted, gate_generation) = if self.hnsw_state.is_none() {
            (false, None)
        } else {
            match self.storage.db().begin_read() {
                Ok(read_txn) => {
                    let targeted = crate::catalog::hnsw_targeted_in_txn(
                        &read_txn,
                        &self.index_catalog_gate_cache,
                        table,
                        self.hnsw_scope,
                        true,
                    );
                    let generation = crate::storage::current_generation_in_txn(&read_txn).ok();
                    (targeted, generation)
                }
                Err(_) => (false, None),
            }
        };
        // 上記ゲート用 read txn はここで既に閉じている。判定〜`search_with_hnsw`
        // 呼び出しの間に別の書き込み（索引カタログの変更を含む、テーブルを問わない
        // 任意のコミット）が挟まると、`hnsw_targeted` はもう最新のカタログ状態
        // （読み取り可否）を反映せず、古い判定 `true` のまま HNSW 経路（`search_with_hnsw` 内部の
        // `sql::hnsw_cache::search_or_fallback`）へ進み得る（codex-review P1
        // 指摘・PR #1124）。`search_with_hnsw` 自身の世代照合（`built_generation`
        // との比較）はスナップショット構築時点からの失効しか検出せず、この判定〜
        // 呼び出し間の失効を必ずしも捉えない。ここで判定時に読んだストレージ世代
        // （`gate_generation`）を呼び出し直前に再照合し、その間に何か 1 件でも
        // コミットされていれば `hnsw_targeted` を強制的に `false` へ倒して
        // brute-force 側（`snapshot.search_with`）へ縮退させる（fail-closed。
        // 縮退先は既存の非 HNSW 経路そのままで、近似ではなく厳密な結果になる）。
        // Issue #1154: `index_catalog_gate_cache` のキーは索引カタログ専用世代へ
        // 切り替えたが、この再照合はあえてストレージ全体世代
        // （`current_generation`）のまま据え置く。ここは「判定〜呼び出しの間に
        // "何か 1 件でも" commit されたら縮退する」ための安全弁であり、索引
        // カタログの変更だけを見る専用世代に緩めると、対象テーブルへの行 DML
        // が判定直後に commit された場合の失効検出を落としてしまう（`gate_generation`
        // は「索引カタログを読み取った瞬間の全体状態」のスナップショット照合を
        // 担っており、`IndexCatalogGateCache` のキャッシュキー選定とは別の関心
        // 事）。索引カタログ世代へ緩める性能改善自体は本 Issue の対象外。
        let hnsw_targeted = hnsw_targeted
            && gate_generation.is_some()
            && self.storage.current_generation().ok() == gate_generation;
        let result = match (hnsw_targeted, &self.hnsw_state) {
            (true, Some(state)) => {
                let access = crate::sql::hnsw_cache::HnswCacheAccess {
                    storage: &self.storage,
                    cache: &state.cache,
                    provider: state.provider,
                    index_gate_cache: &self.index_catalog_gate_cache,
                    hnsw_scope: self.hnsw_scope,
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
            _ => snapshot.search_with(&self.storage, ctx, self.provider.as_ref(), query, k),
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
    /// が `LedgerMode::Ledgered`（`--require-operation-id`）である限りこの段階で `23502` として拒否され、
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
        let stmt = crate::sql::allowlist::validate_insert_with_limit(
            sql,
            &lookup,
            self.ledger_mode,
            self.dml_limits.max_insert_rows_per_statement,
        )?;
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
        // SQL-37・Issue #1587: 列 `DEFAULT now()` 系の評価時刻（autocommit は文の開始時刻）。
        let now = crate::sql::time_fn::now_micros()?;
        let _default_clock_scope =
            crate::sql::time_fn::DefaultClockScope::enter(crate::sql::time_fn::StatementClock {
                txn_start_micros: now,
                stmt_start_micros: now,
            });
        self.execute_insert_form(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            ctx,
            &stmt,
            &lookup,
        )
    }

    /// ビューへの書き込み（`INSERT`／`UPDATE`／`DELETE`。TABLE-43・Issue #1671・#1672）が共有する
    /// 「スナップショット取得 → 書き換え → 指紋スコープ → 実行 → 再試行」の枠。autocommit 腕・
    /// 明示トランザクション腕の実行本体（[`Self::execute_view_insert`]・[`Self::execute_view_update`]・
    /// [`Self::execute_view_delete`]）から呼ばれる。
    ///
    /// `prepare` は同一 read スナップショット上で現在のカタログによりビューを基表形へ書き換える
    /// （書き換え結果 `P` を返す）。`execute` は書き換え結果を既存の基表 DML 経路（`execute_*_form`）へ
    /// 流す。書き換え後に並行 DDL でビュー連鎖・基表スキーマが変わる TOCTOU は、書き換えの**前に**取った
    /// 指紋（[`Storage::view_write_fingerprint`]）を [`crate::sql::view_write_scope`] 経由で書き込み
    /// txn の冒頭に照合して検出する（不一致は書き込み前に中断。`CREATE OR REPLACE VIEW` と同じ型の
    /// 上限つき再試行。上限到達は `55P03`）。明示トランザクションでも照合は書き込み前に失敗するため、
    /// 共有 write txn は汚れず再試行できる。戻り値の第 2 要素は書き換え後の基表名（`mark_written` 用）。
    fn run_view_write<P>(
        &self,
        view_name: &str,
        prepare: impl Fn(
            &crate::catalog::ViewWriteSnapshot<'_>,
        ) -> Result<P, crate::sql::allowlist::SqlSurfaceError>,
        base_table_of: impl Fn(&P) -> String,
        checks_of: impl Fn(&P) -> Vec<Vec<crate::sql::allowlist::WherePredicate>>,
        mut execute: impl FnMut(
            P,
        ) -> Result<
            crate::sql::SqlOutcome,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    ) -> Result<(crate::sql::SqlOutcome, String), crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::SqlSurfaceError;
        // 並行 DDL による再試行の上限（`CREATE OR REPLACE VIEW` の再試行と同じ小さな固定値の考え方）。
        const MAX_ATTEMPTS: usize = 8;
        for _ in 0..MAX_ATTEMPTS {
            // 指紋・ビュー解決・基表スキーマを同一 read スナップショットから取る（別々の読み取りだと、
            // ビューが A→B→A と置換されたとき指紋は一致するのに別の表へ書く状態を作れる）。
            let snapshot = self
                .storage
                .with_view_write_snapshot(view_name, |snap, fingerprint| {
                    prepare(snap).map(|prepared| (fingerprint, prepared))
                })
                .map_err(crate::catalog::table_lookup_error)?;
            // スナップショット内の解決エラーはその時点の状態に対する正当なエラー。
            let (fingerprint, prepared) = snapshot?;
            let base_table = base_table_of(&prepared);
            // `WITH CHECK OPTION`（TABLE-43 (2)・Issue #1673）の検査対象の述語は、INSERT・UPDATE の
            // 書き換え結果だけが持つ（DELETE は空）。検査点が書き込み txn 内で評価する。
            let check_predicates = checks_of(&prepared);
            let scope = crate::sql::view_write_scope::ViewWriteScope::enter_with_checks(
                view_name,
                fingerprint.clone(),
                &base_table,
                check_predicates,
            );
            let result = execute(prepared);
            if scope.mismatched() {
                continue;
            }
            // 書き込み txn 前に失敗した場合（`verify_in_txn` に到達しなかった）でも、その間に
            // 並行 DDL が入っていたなら失敗はスナップショット由来の陳腐化なので再試行する。
            // 指紋の再取得に失敗したときは元のエラーを返す（fail-closed）。
            if result.is_err() {
                if let Ok(current) = self.storage.view_write_fingerprint(view_name) {
                    if current != fingerprint {
                        continue;
                    }
                }
            }
            return result.map(|outcome| (outcome, base_table));
        }
        Err(SqlSurfaceError::LockNotAvailable)
    }

    /// ビューへの `INSERT`（TABLE-43・Issue #1671）の実行本体。autocommit 腕・明示トランザクション腕
    /// から呼ばれる。現在のカタログでビューを基表形の INSERT へ書き換え
    /// （[`crate::sql::view_insert::rewrite_view_insert`]）、既存の基表 INSERT 経路
    /// （[`Self::execute_insert_form`]／[`Self::execute_insert_returning_form`]）へ流す。制約・
    /// `--max-insert-rows`・INDEX-4 上限・テナント所有・台帳は基表への書き込みと同じ。
    /// 再試行・指紋照合は [`Self::run_view_write`]。`make_target` は試行ごとに書き込み先を作る。
    fn execute_view_insert<'t>(
        &self,
        make_target: impl Fn() -> crate::tenant::WriteTarget<'t>,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedViewInsert,
    ) -> Result<(crate::sql::SqlOutcome, String), crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::SqlSurfaceError;
        self.run_view_write(
            stmt.view_name(),
            |snap| {
                let rewritten = crate::sql::view_insert::rewrite_view_insert(snap, stmt)?;
                let schema =
                    snap.table_schema(&rewritten.insert.table_name)
                        .map_err(|e| match e {
                            CatalogError::TableNotFound(name) => {
                                SqlSurfaceError::UndefinedTable { name }
                            }
                            other => crate::catalog::table_lookup_error(other),
                        })?;
                Ok((rewritten, schema))
            },
            |(rewritten, _)| rewritten.insert.table_name.clone(),
            |(rewritten, _)| rewritten.check_predicates.clone(),
            |(rewritten, schema)| {
                // 基表の疑似列 `id` を省略するビュー経由の INSERT は、`WITH CHECK OPTION` の検査時点と
                // 採番値の関係が未検証のため受理せず `0A000` で拒否する（Issue #1699・ADR D9）。
                if crate::sql::parser::insert_omits_pseudo_id(&rewritten.insert, &schema) {
                    return Err(
                        crate::sql::allowlist::SqlSurfaceError::FeatureNotSupported {
                            detail: "omitting the id pseudo-column is not supported through a view"
                                .to_string(),
                        },
                    );
                }
                let lookup = InsertSchemaLookup::with_schema(
                    &self.storage,
                    &rewritten.insert.table_name,
                    schema,
                );
                if rewritten.insert.returning.is_some() {
                    self.execute_insert_returning_form(
                        make_target(),
                        ctx,
                        &rewritten.insert,
                        &lookup,
                    )
                    .map(|mut outcome| {
                        crate::sql::view_insert::apply_public_names(
                            &mut outcome.result.columns,
                            rewritten.public_names.as_deref(),
                        );
                        crate::sql::SqlOutcome::Returning(outcome)
                    })
                } else {
                    self.execute_insert_form(make_target(), ctx, &rewritten.insert, &lookup)
                        .map(crate::sql::SqlOutcome::Insert)
                }
            },
        )
    }

    /// ビューへの `DELETE`（TABLE-43・Issue #1672）の実行本体。現在のカタログでビューを基表形の
    /// DELETE（ビューの述語と利用者の述語を AND 合成）へ書き換え
    /// （[`crate::sql::view_dml::rewrite_view_delete`]）、既存の基表 DELETE 経路
    /// （[`Self::execute_delete_form`]・[`Self::execute_predicate_delete_form`] とその
    /// `RETURNING` 版）へ流す。影響行数の上限（`--max-dml-affected-rows`）・RLS・台帳は基表への
    /// 書き込みと同じ。`PARTITIONED` は書き換えの段で拒否済み（`0A000`）。
    fn execute_view_delete<'t>(
        &self,
        make_target: impl Fn() -> crate::tenant::WriteTarget<'t>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedViewDelete,
    ) -> Result<(crate::sql::SqlOutcome, String), crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::DeleteStatement;
        self.run_view_write(
            stmt.view_name(),
            |snap| crate::sql::view_dml::rewrite_view_delete(snap, stmt),
            |rewritten| match &rewritten.delete {
                DeleteStatement::SingleRow(v) => v.table_name.clone(),
                DeleteStatement::Predicate(v) => v.table_name().to_string(),
            },
            // DELETE は `WITH CHECK OPTION` の検査対象外（削除後の行は存在しない）。
            |_| Vec::new(),
            |rewritten| {
                let names = rewritten.public_names.as_deref();
                let returning = |mut outcome: crate::sql::exec::ReturningOutcome| {
                    crate::sql::view_insert::apply_public_names(&mut outcome.result.columns, names);
                    crate::sql::SqlOutcome::Returning(outcome)
                };
                match &rewritten.delete {
                    DeleteStatement::SingleRow(v) if v.returning.is_some() => self
                        .execute_delete_returning_form(make_target(), ctx, v)
                        .map(returning),
                    DeleteStatement::SingleRow(v) => self
                        .execute_delete_form(make_target(), ctx, v)
                        .map(crate::sql::SqlOutcome::Delete),
                    DeleteStatement::Predicate(v) if v.returning().is_some() => self
                        .execute_predicate_delete_returning_form(make_target(), ctx, session, v)
                        .map(returning),
                    DeleteStatement::Predicate(v) => self
                        .execute_predicate_delete_form(make_target(), ctx, session, v)
                        .map(crate::sql::SqlOutcome::Delete),
                }
            },
        )
    }

    /// ビューへの `UPDATE`（TABLE-43・Issue #1672）の実行本体。[`Self::execute_view_delete`] と同じ
    /// 構成で、基表形の UPDATE を [`Self::execute_predicate_update_form`]／
    /// [`Self::execute_update_returning_form`] へ流す。
    fn execute_view_update<'t>(
        &self,
        make_target: impl Fn() -> crate::tenant::WriteTarget<'t>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedViewUpdate,
    ) -> Result<(crate::sql::SqlOutcome, String), crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::ValidatedUpdateForm;
        self.run_view_write(
            stmt.view_name(),
            |snap| crate::sql::view_dml::rewrite_view_update(snap, stmt),
            |rewritten| match &rewritten.update {
                ValidatedUpdateForm::Single(v) => v.table_name.clone(),
                ValidatedUpdateForm::Predicate(v) => v.table_name().to_string(),
            },
            |rewritten| rewritten.check_predicates.clone(),
            |rewritten| {
                let has_returning = match &rewritten.update {
                    ValidatedUpdateForm::Single(v) => v.returning.is_some(),
                    ValidatedUpdateForm::Predicate(v) => v.returning().is_some(),
                };
                if has_returning {
                    self.execute_update_returning_form(
                        make_target(),
                        ctx,
                        session,
                        &rewritten.update,
                    )
                    .map(|mut outcome| {
                        crate::sql::view_insert::apply_public_names(
                            &mut outcome.result.columns,
                            rewritten.public_names.as_deref(),
                        );
                        crate::sql::SqlOutcome::Returning(outcome)
                    })
                } else {
                    self.execute_predicate_update_form(
                        make_target(),
                        ctx,
                        session,
                        &rewritten.update,
                    )
                    .map(crate::sql::SqlOutcome::Update)
                }
            },
        )
    }

    /// `ON CONFLICT ON CONSTRAINT <名>` の名前が `CREATE UNIQUE INDEX` 由来のとき `42704` で
    /// 拒否する（SQL-49・INDEX-9 (3)、Issue #1737）。`sql::parser::bind_insert_form` の名前解決は
    /// スキーマだけを見る純関数で索引カタログを参照できないため、束縛の前にここで判定する
    /// （`execute_insert_form`・`execute_insert_returning_form`・ファイル形バッチの 3 経路から呼ぶ）。
    /// スキーマ取得と本判定は別 txn だが、競合しても返るエラーコードが入れ替わるだけで、
    /// 書き込みは既存の世代検査で保護される。カタログ I/O 失敗は固定文言の `Internal` に丸める。
    ///
    /// `table_name`・`on_conflict` を直接受けるのは、VALUES 形（`ValidatedInsert`）と
    /// `INSERT ... SELECT`（`ValidatedInsertSelect`。Issue #2614）で共有するため。
    fn reject_index_derived_conflict_constraint(
        &self,
        table_name: &str,
        on_conflict: Option<&crate::sql::allowlist::OnConflictClause>,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        let Some(crate::sql::allowlist::OnConflictClause {
            target: crate::sql::allowlist::ConflictTargetSpec::Constraint(name),
            ..
        }) = on_conflict
        else {
            return Ok(());
        };
        match self.storage.is_unique_index_of_table(table_name, name) {
            Ok(false) => Ok(()),
            Ok(true) => {
                Err(crate::sql::allowlist::SqlSurfaceError::UndefinedObject { name: name.clone() })
            }
            Err(_) => Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "failed to load table schema".to_string(),
            }),
        }
    }

    /// [`Self::execute_insert_sql`]・[`Self::execute_sql_in_session`] の
    /// INSERT 分岐が共有する束縛〜実行本体（Issue #485）。
    /// `sql::allowlist::validate_insert_with_limit`／
    /// `validate_insert_tokens_with_limit`（Issue #997 で行数上限を設定可能に
    /// した現行版。呼び出し元は `self.dml_limits.max_insert_rows_per_statement`
    /// を渡す）が返した `stmt` と、その検証時に使った
    /// `lookup`（`table_exists` 呼び出しでスキーマをキャッシュ済み）を受け取り、
    /// [`InsertSchemaLookup::take_schema`] でスキーマを再取得できればそれを
    /// 使い、できなければ（名前不一致・未保持の防御的経路）`get_table_schema`
    /// 単独呼び出しへ fail-closed にフォールバックする——いずれの経路でも
    /// エラー写像・`wire_code` は本 Issue 導入前と完全に一致する。
    fn execute_insert_form(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedInsert,
        lookup: &InsertSchemaLookup<'_>,
    ) -> Result<crate::sql::exec::InsertOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.check_insert_row_count_limit(stmt.rows.len())?;
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
        self.reject_index_derived_conflict_constraint(&stmt.table_name, stmt.on_conflict.as_ref())?;
        let bound = crate::sql::parser::bind_insert_form(stmt, &schema)?;
        match bound {
            crate::sql::parser::BoundInsertForm::Row(bound) => {
                crate::sql::exec::execute_insert_with_schema_in(
                    target,
                    ctx,
                    &bound,
                    self.dml_ledger_mode(),
                    None,
                    Some(&self.trigger_runtime),
                )
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
                crate::sql::exec::execute_insert_batch_with_schema_in(
                    target,
                    ctx,
                    &bounds,
                    self.dml_ledger_mode(),
                    Some(&schema),
                    Some(&self.trigger_runtime),
                )
            }
            crate::sql::parser::BoundInsertForm::File(bound) => {
                // 明示トランザクション内でも受理する（Issue #1353）。埋め込み I/O は BEGIN
                // 時点で保持済みの単一ライタの下・セッションの `max_duration` の範囲で行う。
                crate::sql::exec::execute_file_insert(
                    target,
                    &self.storage,
                    ctx,
                    self.embedder.as_deref(),
                    &self.incremental_config,
                    &bound,
                    self.dml_ledger_mode(),
                    self.scan_limits.max_scanned_rows(),
                )
            }
            // `ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...`（SQL-20・
            // TASK-193、Issue #872）。複数行 `VALUES` を伴う UPSERT にも
            // `RowBatch` 分岐と同じ INDEX-4 上限（①行数・②③④バイト量・
            // チャンク総量）を適用する（SQL/NoSQL 機能パリティの方針を UPSERT
            // にも揃える）。
            crate::sql::parser::BoundInsertForm::Upsert(bound) => {
                self.validate_upsert_batch_limits(&bound.rows)?;
                crate::sql::exec::execute_upsert_in(
                    target,
                    ctx,
                    &bound,
                    self.dml_ledger_mode(),
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
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedInsert,
        lookup: &InsertSchemaLookup<'_>,
    ) -> Result<crate::sql::exec::ReturningOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.check_insert_row_count_limit(stmt.rows.len())?;
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
        self.reject_index_derived_conflict_constraint(&stmt.table_name, stmt.on_conflict.as_ref())?;
        let bound = crate::sql::parser::bind_insert_form(stmt, &schema)?;
        match bound {
            crate::sql::parser::BoundInsertForm::Row(bound) => {
                crate::sql::exec::execute_insert_returning_in(
                    target,
                    ctx,
                    std::slice::from_ref(&bound),
                    self.dml_ledger_mode(),
                    &returning,
                    &schema,
                )
            }
            crate::sql::parser::BoundInsertForm::RowBatch(bounds) => {
                self.validate_insert_row_batch_limits(&bounds)?;
                crate::sql::exec::execute_insert_returning_in(
                    target,
                    ctx,
                    &bounds,
                    self.dml_ledger_mode(),
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
            // SQL-20）の併用（Issue #1182 で実行結線）。`Upsert` 腕と同じ
            // INDEX-4 上限（[`Self::validate_upsert_batch_limits`]）を適用したうえで
            // [`crate::sql::exec::execute_upsert_returning`] へ委譲する。挿入行・
            // `DO UPDATE` 行のみ返し、`DO NOTHING` で衝突した行は返さない。
            crate::sql::parser::BoundInsertForm::Upsert(bound) => {
                // autocommit と [`Self::execute_in_active_txn`]（`InTxn`。Issue #1273）の
                // 双方から呼ばれる。衝突判定・不可視な衝突先の扱い・台帳・制約検査は
                // `tenant::upsert_typed_rows_impl` が `WriteTarget` 越しに共有する。
                self.validate_upsert_batch_limits(&bound.rows)?;
                crate::sql::exec::execute_upsert_returning_in(
                    target,
                    ctx,
                    &bound,
                    self.dml_ledger_mode(),
                    &returning,
                    &schema,
                )
            }
        }
    }

    /// UPSERT（`INSERT ... ON CONFLICT`）の INDEX-4 バッチ上限検査
    /// （①行数・②③④バイト量・チャンク総量。`RowBatch` と同じ SQL/NoSQL パリティ
    /// 方針）。[`Self::execute_insert_form`] と
    /// [`Self::execute_insert_returning_form`] の `Upsert` 腕が共有する。
    fn validate_upsert_batch_limits(
        &self,
        rows: &[crate::sql::parser::BoundInsert],
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        if rows.len() > self.batch_limits.max_files_per_batch {
            return Err(crate::sql::allowlist::SqlSurfaceError::payload_too_large(
                crate::batch_limits::BatchLimitsError::TooManyFiles {
                    count: rows.len(),
                    max: self.batch_limits.max_files_per_batch,
                }
                .to_string(),
            ));
        }
        self.validate_insert_batch_byte_and_chunk_limits(rows)
    }

    /// SQL 表層の単一 TRUNCATE 文実行エントリポイント（TASK-193、対象ビヘイビア:
    /// SQL-22）。`execute_insert_sql` と同じ構造（`execute_sql`（TASK-75、SELECT
    /// 専用）とは独立した固有メソッド。`VectorCore` trait への昇格は行わない）。
    ///
    /// `sql::allowlist::validate_truncate`（構造検証。文末専用句
    /// `USING OPERATION_ID '<id>'` の省略（明示 `NULL` を含む）は、`self.ledger_mode`
    /// が `LedgerMode::Ledgered`（`--require-operation-id`）である限りこの段階で `23502` として拒否され、
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
        crate::sql::exec::execute_truncate_in(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            ctx,
            stmt,
            self.ledger_mode,
            // TRUNCATE トリガ（Issue #1726）は通過証を渡す結線済み経路だけが発火する。
            Some(&self.trigger_runtime),
        )
    }

    /// SQL 表層の単一 DELETE 文実行エントリポイント（SQL-18、TASK-191、#867）。
    /// `execute_truncate_sql` と同じ構造（`execute_sql`（TASK-75、SELECT
    /// 専用）とは独立した固有メソッド。`VectorCore` trait への昇格は行わない）。
    ///
    /// `sql::allowlist::validate_delete`（構造検証。文末専用句
    /// `USING OPERATION_ID '<id>'` の省略（明示 `NULL` を含む）は、`self.ledger_mode`
    /// が `LedgerMode::Ledgered`（`--require-operation-id`）である限りこの段階で `23502` として拒否され、
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
        self.execute_delete_form(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            ctx,
            &stmt,
        )
    }

    /// [`Self::execute_delete_sql`]・[`Self::execute_sql_in_session`] の
    /// DELETE 分岐が共有する実行本体（`execute_truncate_form` と同じ設計）。
    /// DELETE は `id` 疑似列以外の列を参照しない（`sql::parser::bind_delete`
    /// ドキュメント参照）ため、`execute_insert_form` の `InsertSchemaLookup`
    /// キャッシュに相当する仕組みは不要で、`sql::parser::bind_delete` →
    /// `sql::exec::execute_delete` の薄い委譲になる。
    fn execute_delete_form(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        stmt: &crate::sql::allowlist::ValidatedDelete,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let bound = crate::sql::parser::bind_delete(stmt)?;
        crate::sql::exec::execute_delete_in(
            target,
            ctx,
            &bound,
            self.dml_ledger_mode(),
            Some(&self.trigger_runtime),
        )
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
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.execute_predicate_delete_form_pre(target, ctx, session, stmt, None)
    }

    /// 内側の実装。`pre` が `Some` のとき、`WHERE` のサブクエリは呼び出し元（データ変更 CTE の
    /// 段 A。Issue #1877）が書き込み前に解決済みで、その結果（サブクエリが無ければ `None`）をそのまま使う。
    /// 台帳ハッシュは解決前の文から出す契約（SQL-53 (5)）は変わらない。
    fn execute_predicate_delete_form_pre(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
        pre: Option<Option<crate::sql::allowlist::ValidatedPredicateDelete>>,
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
        // SQL-53（Issue #1751）: WHERE 内のサブクエリは束縛の前に書き込み txn のスナップショットで解決する。
        let resolved = match pre {
            Some(resolved) => resolved,
            None => {
                self.resolve_predicate_delete_subqueries(&target, ctx, session, stmt, &schema)?
            }
        };
        self.run_predicate_delete_resolved(
            target,
            ctx,
            session.udfs(),
            resolved.as_ref().unwrap_or(stmt),
            stmt,
            &schema,
            Some(&self.trigger_runtime),
        )
    }

    /// [`Self::execute_predicate_delete_form`]（SQL 表層。SQL-19・TASK-192、
    /// Issue #871）と [`Self::execute_bound_predicate_delete_in_session`]
    /// （NoSQL 表層 `delete` op の `filter`。TASK-186・NOSQL-12、Issue #1062）が
    /// スキーマ取得より後で共有する実行本体（第 2 の実行器を作らない設計。
    /// CLAUDE.md「委譲方針」）。抽出前と挙動が完全に同一であることは既存の
    /// engine テスト（`sql_predicate_dml_exec` 等）で回帰確認済み。
    ///
    /// `udfs` は呼び出し元が解決済みの UDF レジストリ（SQL 経路は
    /// `session.udfs()`、NoSQL 経路はセッション UDF を持たないため
    /// [`crate::sql::udf_call::UdfRegistry::default()`]）を渡す。
    fn run_predicate_delete(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        udfs: &crate::sql::udf_call::UdfRegistry,
        stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
        schema: &crate::catalog::TableSchema,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.run_predicate_delete_resolved(target, ctx, udfs, stmt, stmt, schema, None)
    }

    /// [`Self::run_predicate_delete`] の本体。`bind_stmt` は束縛に使う文（サブクエリ解決済み）、
    /// `hash_stmt` は台帳ハッシュに使う元の文（解決前。SQL-53 (5)・RECOVER-11）。
    /// サブクエリつきでも台帳ハッシュは解決前の `hash_stmt` から出す（Issue #2650）。
    #[allow(clippy::too_many_arguments)]
    fn run_predicate_delete_resolved(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        udfs: &crate::sql::udf_call::UdfRegistry,
        bind_stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
        hash_stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
        schema: &crate::catalog::TableSchema,
        triggers: Option<&crate::trigger::TriggerRuntime>,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // 多層防御（Issue #1129）: 分割実行修飾付きの文は原子実行器へ到達させない
        // （黙って原子実行へ切り替えない。fail-closed）。
        if bind_stmt.partitioned().is_some() {
            return Err(crate::sql::allowlist::SqlSurfaceError::PartitionedDmlInTransaction);
        }
        let (bound, content_hash_value) =
            Self::prepare_predicate_delete(bind_stmt, hash_stmt, schema, udfs)?;
        crate::sql::exec::execute_predicate_delete_in(
            target,
            ctx,
            &bound,
            self.dml_ledger_mode(),
            schema,
            &content_hash_value,
            self.dml_limits.max_affected_rows,
            self.scan_limits.max_scanned_rows(),
            triggers,
        )
    }

    /// 述語形 `DELETE ... PARTITIONED [CHUNK n]`（Issue #1129・SQL-19）の実行 form。
    /// スキーマ取得 → 束縛 → 分割実行用の内容照合ハッシュ（チャンク幅は含めない）→ 分割実行器、
    /// の順で、原子経路（[`Self::run_predicate_delete`]）の入口とは独立している。autocommit
    /// 専用（[`crate::tenant::WriteTarget`] を取らない）。`CHUNK n` は実行時にも再検査する。
    fn execute_partitioned_delete_form(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let clause =
            stmt.partitioned()
                .ok_or_else(|| crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "internal: partitioned DELETE without PARTITIONED clause".to_string(),
                })?;
        let limits =
            crate::sql::partitioned::apply_chunk_override(clause, &self.partitioned_dml_limits)?;
        let schema = self.load_table_schema_for_dml(stmt.table_name())?;
        self.run_partitioned_delete(ctx, session.udfs(), stmt, &schema, &limits)
    }

    /// 述語形 `UPDATE ... PARTITIONED [CHUNK n]`（Issue #1129・SQL-19）の実行 form。
    /// [`Self::execute_partitioned_delete_form`] と同じ構成で、UPDATE は分割実行用ハッシュだけを
    /// 使う（旧ハッシュ互換の `legacy_hashes` は持たない）。
    fn execute_partitioned_update_form(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedPredicateUpdate,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let clause =
            stmt.partitioned()
                .ok_or_else(|| crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "internal: partitioned UPDATE without PARTITIONED clause".to_string(),
                })?;
        let limits =
            crate::sql::partitioned::apply_chunk_override(clause, &self.partitioned_dml_limits)?;
        let schema = self.load_table_schema_for_dml(stmt.table_name())?;
        self.run_partitioned_update(ctx, session.udfs(), stmt, &schema, &limits)
    }

    /// 分割実行 DELETE の束縛以降の共通本体（Issue #1130）。SQL 表層
    /// （[`Self::execute_partitioned_delete_form`]）と NoSQL 表層
    /// （[`Self::execute_bound_partitioned_delete_in_session`]）の両方が呼ぶ。
    /// 述語束縛・内容照合ハッシュ（ドメイン共有。RECOVER-11）・分割実行器・上限を
    /// 1 箇所に集約し、表層を跨いだ同一 `operation_id` の再送判定を構造的に一致させる。
    /// `limits` は呼び出し元が `apply_chunk_override` 済みの値を渡す。
    fn run_partitioned_delete(
        &self,
        ctx: &PolicyContext,
        udfs: &crate::sql::udf_call::UdfRegistry,
        stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
        schema: &crate::catalog::TableSchema,
        limits: &crate::sql::parser::PartitionedDmlLimits,
    ) -> Result<crate::sql::exec::DeleteOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // 多層防御: 外部で組み立てた文でも構成に関わらず operation_id 必須（Issue #1704）。
        crate::recovery::required_op_id::require_partitioned(stmt.operation_id())
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;
        // 多層防御（SQL-53 (6)・Issue #1751）: 分割実行はサブクエリつき WHERE を解決しない。
        if dml_subquery_exec::where_has_subquery(stmt.where_predicates()) {
            return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "subquery is not supported for this statement shape",
            ));
        }
        // 時刻値関数を含む文は、ジョブ開始時に確定・永続化した固定時刻で全チャンクを束縛する
        // （再開時も同じ値。SQL-37・RECOVER-11・Issue #1587、設計 D6）。束縛 → ハッシュ計算の
        // 失敗順序は従来どおり。
        let (bound, content_hash_value, fixed_time) = self.bind_partitioned_with_fixed_time(
            ctx,
            stmt.table_name(),
            stmt.operation_id(),
            || crate::sql::parser::bind_predicate_delete(stmt, schema, udfs),
            || {
                crate::recovery::content_hash::for_delete_where_partitioned(
                    stmt.table_name(),
                    stmt.where_predicates(),
                    udfs,
                    schema,
                )
            },
        )?;
        crate::sql::exec::execute_predicate_delete_partitioned(
            &self.storage,
            ctx,
            &bound,
            self.ledger_mode,
            schema,
            &content_hash_value,
            limits,
            self.dml_limits.max_affected_rows,
            fixed_time,
        )
    }

    /// 分割実行 UPDATE の束縛以降の共通本体（Issue #1130）。
    /// [`Self::run_partitioned_delete`] と同じ位置付け（SQL・NoSQL 両表層の共有本体）。
    fn run_partitioned_update(
        &self,
        ctx: &PolicyContext,
        udfs: &crate::sql::udf_call::UdfRegistry,
        stmt: &crate::sql::allowlist::ValidatedPredicateUpdate,
        schema: &crate::catalog::TableSchema,
        limits: &crate::sql::parser::PartitionedDmlLimits,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // 多層防御: 外部で組み立てた文でも構成に関わらず operation_id 必須（Issue #1704）。
        crate::recovery::required_op_id::require_partitioned(stmt.operation_id())
            .map_err(|_| crate::sql::allowlist::SqlSurfaceError::MissingOperationId)?;
        // 多層防御（SQL-53 (6)・Issue #1751）: 分割実行はサブクエリつき WHERE を解決しない。
        if dml_subquery_exec::where_has_subquery(stmt.where_predicates()) {
            return Err(crate::sql::allowlist::SqlSurfaceError::unsupported(
                "subquery is not supported for this statement shape",
            ));
        }
        let assignment_refs: Vec<(&str, &crate::sql::allowlist::SetValue)> = stmt
            .assignments()
            .iter()
            .map(|(name, literal)| (name.as_str(), literal))
            .collect();
        // 時刻値関数を含む文の固定時刻（`run_partitioned_delete` と同じ。設計 D6）。
        let (bound, content_hash_value, fixed_time) = self.bind_partitioned_with_fixed_time(
            ctx,
            stmt.table_name(),
            stmt.operation_id(),
            || crate::sql::parser::bind_predicate_update(stmt, schema, udfs),
            || {
                crate::recovery::content_hash::for_update_where_partitioned(
                    stmt.table_name(),
                    &assignment_refs,
                    stmt.where_predicates(),
                    udfs,
                    schema,
                )
            },
        )?;
        crate::sql::exec::execute_predicate_update_partitioned(
            &self.storage,
            ctx,
            &bound,
            self.ledger_mode,
            schema,
            &content_hash_value,
            limits,
            self.dml_limits.max_affected_rows,
            fixed_time,
        )
    }

    /// 分割実行 DML の束縛を、トランザクション固定の時刻関数だけを受理するスコープで行い、
    /// 時刻値関数を含む文には**ジョブ開始時に確定した固定時刻**を返す（SQL-37・RECOVER-11・
    /// RECOVER-12・Issue #1587、設計 D6）。
    ///
    /// 1. 現在時刻を仮の固定時刻としてスコープを開き `bind` を呼ぶ。束縛が時刻を要求しなければ
    ///    （[`crate::sql::time_fn::ClockScope::was_used`]）時刻値関数を含まない文で、`None` を返す
    ///    （従来どおり。`statement_timestamp()`・`clock_timestamp()` はスコープが `0A000` にする）。
    /// 2. 含む文は、同じ `(tenant, table, operation_id)` の `Running` 記録が内容一致で固定時刻を
    ///    持っていればその値、なければ仮の固定時刻を採用する。記録の値が仮と異なる場合は、記録の
    ///    値で束縛し直す（全チャンクと再開後で同じ時刻にする）。固定時刻を持たない記録（v1）は
    ///    `Some(仮)` のまま実行器の再送判定が `22023` にする。
    ///
    /// 実行器（`tenant::partitioned_dml`）が最初のチャンクの write txn 内で記録の値との一致を
    /// 再検査し、事前読み込みとの間で記録が変わっていれば止める（TOCTOU 対策）。
    fn bind_partitioned_with_fixed_time<T>(
        &self,
        ctx: &PolicyContext,
        table: &str,
        op_id: Option<&crate::recovery::required_op_id::OperationId>,
        bind: impl Fn() -> Result<T, crate::sql::allowlist::SqlSurfaceError>,
        hash_fn: impl FnOnce() -> Result<
            crate::recovery::content_hash::ContentHash,
            crate::sql::allowlist::SqlSurfaceError,
        >,
    ) -> Result<
        (T, crate::recovery::content_hash::ContentHash, Option<i64>),
        crate::sql::allowlist::SqlSurfaceError,
    > {
        use crate::sql::time_fn::{now_micros, ClockScope, StatementClock};
        let provisional = now_micros()?;
        let scope = ClockScope::enter_txn_fixed_only(StatementClock {
            txn_start_micros: provisional,
            stmt_start_micros: provisional,
        });
        let bound = bind()?;
        let hash = hash_fn()?;
        if !scope.was_used() {
            return Ok((bound, hash, None));
        }
        let recorded = match op_id {
            Some(op_id) => {
                let key = crate::recovery::partitioned_job::JobKey::for_context(ctx, table, op_id);
                let read_txn = self.storage.db().begin_read().map_err(|_| {
                    crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    }
                })?;
                match crate::recovery::partitioned_job::lookup_in_read_txn(&read_txn, &key) {
                    Ok(Some(crate::recovery::partitioned_job::JobRecord::Running {
                        hash: stored,
                        fixed_time,
                        ..
                    })) if hash.matches(&stored) => fixed_time,
                    Ok(_) => None,
                    Err(_) => {
                        return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                            detail: "internal error".to_string(),
                        })
                    }
                }
            }
            None => None,
        };
        match recorded {
            Some(t) if t != provisional => {
                drop(scope);
                let _rebind_scope = ClockScope::enter_txn_fixed_only(StatementClock {
                    txn_start_micros: t,
                    stmt_start_micros: t,
                });
                let bound = bind()?;
                Ok((bound, hash, Some(t)))
            }
            Some(t) => Ok((bound, hash, Some(t))),
            None => Ok((bound, hash, Some(provisional))),
        }
    }

    /// `SHOW`／`CANCEL PARTITIONED DML`・`EXPLAIN ... PARTITIONED`（Issue #1129）の実行。
    /// `SHOW`・`CANCEL` はユーザーテーブルのカタログを引かず、自テナントのジョブ表だけを
    /// 見る（該当なしは常に同じ応答。RLS-9）。`EXPLAIN` は束縛（スキーマ取得・述語束縛・
    /// ハッシュ計算）までで、書き込み・登録簿・ジョブ表には触れない。
    fn execute_partitioned_control(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        control: &crate::sql::partitioned::PartitionedControl,
    ) -> Result<crate::sql::SqlOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::SqlSurfaceError;
        use crate::sql::partitioned::PartitionedControl;
        match control {
            PartitionedControl::Show(job) => Ok(crate::sql::SqlOutcome::Query(
                crate::sql::partitioned::execute_show(&self.storage, ctx, job)?,
            )),
            PartitionedControl::Cancel(job) => Ok(crate::sql::SqlOutcome::Query(
                crate::sql::partitioned::execute_cancel(&self.storage, ctx, job)?,
            )),
            PartitionedControl::ExplainDelete(stmt) => {
                let clause = stmt
                    .partitioned()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal: EXPLAIN DELETE without PARTITIONED clause".to_string(),
                    })?;
                let limits = crate::sql::partitioned::apply_chunk_override(
                    clause,
                    &self.partitioned_dml_limits,
                )?;
                let schema = self.load_table_schema_for_dml(stmt.table_name())?;
                let udfs = session.udfs();
                // EXPLAIN は束縛だけ行う（SQL-37・Issue #1587）。時刻値関数は実行時と同じく
                // トランザクション固定のものだけを受理する。
                let now = crate::sql::time_fn::now_micros()?;
                let _clock_scope = crate::sql::time_fn::ClockScope::enter_txn_fixed_only(
                    crate::sql::time_fn::StatementClock {
                        txn_start_micros: now,
                        stmt_start_micros: now,
                    },
                );
                crate::sql::parser::bind_predicate_delete(stmt, &schema, udfs)?;
                crate::recovery::content_hash::for_delete_where_partitioned(
                    stmt.table_name(),
                    stmt.where_predicates(),
                    udfs,
                    &schema,
                )?;
                Ok(crate::sql::SqlOutcome::Explain(
                    crate::sql::partitioned::explain_result(
                        "delete",
                        &self.partitioned_dml_limits,
                        limits.chunk_rows.get(),
                    ),
                ))
            }
            PartitionedControl::ExplainUpdate(stmt) => {
                let clause = stmt
                    .partitioned()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal: EXPLAIN UPDATE without PARTITIONED clause".to_string(),
                    })?;
                let limits = crate::sql::partitioned::apply_chunk_override(
                    clause,
                    &self.partitioned_dml_limits,
                )?;
                let schema = self.load_table_schema_for_dml(stmt.table_name())?;
                let udfs = session.udfs();
                let now = crate::sql::time_fn::now_micros()?;
                let _clock_scope = crate::sql::time_fn::ClockScope::enter_txn_fixed_only(
                    crate::sql::time_fn::StatementClock {
                        txn_start_micros: now,
                        stmt_start_micros: now,
                    },
                );
                crate::sql::parser::bind_predicate_update(stmt, &schema, udfs)?;
                let assignment_refs: Vec<(&str, &crate::sql::allowlist::SetValue)> = stmt
                    .assignments()
                    .iter()
                    .map(|(name, literal)| (name.as_str(), literal))
                    .collect();
                crate::recovery::content_hash::for_update_where_partitioned(
                    stmt.table_name(),
                    &assignment_refs,
                    stmt.where_predicates(),
                    udfs,
                    &schema,
                )?;
                Ok(crate::sql::SqlOutcome::Explain(
                    crate::sql::partitioned::explain_result(
                        "update",
                        &self.partitioned_dml_limits,
                        limits.chunk_rows.get(),
                    ),
                ))
            }
        }
    }
    /// 述語形 `DELETE` の束縛と内容照合ハッシュ計算（RECOVER-11）。`RETURNING` の
    /// 有無に関わらず**同一の入力**（`stmt.where_predicates()` のみ）からハッシュを
    /// 計算するため、[`Self::run_predicate_delete`] と
    /// [`Self::execute_predicate_delete_returning_form`] が共有する（`RETURNING`
    /// の有無で再送判定が変わらない契約〔Issue #1182〕をコード上でも 1 箇所に固定）。
    ///
    /// `bind_stmt` は束縛用（サブクエリ解決済み）、`hash_stmt` はハッシュ用の元の文
    /// （サブクエリのない文では同一。SQL-53 (5)）。サブクエリつきの文も解決**前**の `hash_stmt` から
    /// 正規化ハッシュを出す（Issue #2650。`dml_subquery_exec` の事前照合と同じ入力・同じ関数なので
    /// 事前照合と確定記録のハッシュは一致する）。
    fn prepare_predicate_delete(
        bind_stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
        hash_stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
        schema: &crate::catalog::TableSchema,
        udfs: &crate::sql::udf_call::UdfRegistry,
    ) -> Result<
        (
            crate::sql::parser::BoundPredicateDelete,
            crate::recovery::content_hash::ContentHash,
        ),
        crate::sql::allowlist::SqlSurfaceError,
    > {
        let bound = crate::sql::parser::bind_predicate_delete(bind_stmt, schema, udfs)?;
        let content_hash_value = crate::recovery::content_hash::for_delete_where(
            hash_stmt.table_name(),
            hash_stmt.where_predicates(),
            udfs,
        )?;
        Ok((bound, content_hash_value))
    }

    /// UPDATE の束縛区間（`bind_*` 呼び出しの範囲だけ）で NOT NULL 検査を後回しにするガードを返す。
    /// 対象表に BEFORE の UPDATE 行トリガ（NEW を書き換え得る）があるときだけで、AFTER のみ・文のみのトリガ・
    /// トリガ無しは従来どおり束縛時に `23502` を返す（`23502` と `UPDATE 0` の差で行の存在・可視性を
    /// 漏らさない。Issue #1725）。`table` は束縛に渡すスキーマの表名（表ごとに限定するため）。
    /// `UPDATE OF` 付きトリガは SET 列（`DEFAULT` 指定列を含む）と交差するときだけ「発火する」と数える
    /// （発火しない文で延期すると、行の有無で `UPDATE 0` と `23502` が分かれる。`trigger::plan` の
    /// `filter_update_of` と同じ規則）。
    fn defer_not_null_scope_for_update(
        &self,
        ctx: &PolicyContext,
        table: &str,
        set_columns: &[&str],
    ) -> Option<crate::sql::set_expr::DeferNotNullBindScope> {
        // 読み取りの失敗・トリガ無しは従来どおり束縛時に検査する（fail-closed。トリガ有無の誤判定でも
        // 最終検査が `validate_set_assignments` に残る）。
        let triggers = self.storage.list_triggers(ctx.tenant_id(), table).ok()?;
        triggers
            .iter()
            .any(|t| {
                t.timing == crate::catalog::trigger::TriggerTiming::Before
                    // 文トリガは NEW を書き換えられないので延期しない（延期すると `23502` と `UPDATE 0` の
                    // 差で行の存在を漏らす。Issue #1726）。
                    && t.level == crate::catalog::trigger::TriggerLevel::Row
                    && t.events
                        .contains(crate::catalog::trigger::TriggerEvents::UPDATE)
                    && (t.update_of.is_empty()
                        || t.update_of
                            .iter()
                            .any(|c| set_columns.contains(&c.as_str())))
            })
            .then(|| crate::sql::set_expr::DeferNotNullBindScope::enter(table))
    }

    /// 対象テーブルのスキーマを取得する（`RETURNING` 付き DML の各実行本体が
    /// 共有。`TableNotFound` は `42P01`、それ以外は詳細を含まない固定文言の
    /// `XX000`）。
    fn load_table_schema_for_dml(
        &self,
        table: &str,
    ) -> Result<crate::catalog::TableSchema, crate::sql::allowlist::SqlSurfaceError> {
        self.storage.get_table_schema(table).map_err(|e| match e {
            CatalogError::TableNotFound(name) => {
                crate::sql::allowlist::SqlSurfaceError::UndefinedTable { name }
            }
            _ => crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "failed to load table schema".to_string(),
            },
        })
    }

    /// [`Self::execute_sql_in_session`]（autocommit）と [`Self::execute_in_active_txn`]
    /// （`InTxn`。Issue #1272）の `RETURNING` 付き述語形 `DELETE`
    /// （Issue #1182・SQL-21）分岐が呼ぶ実行本体。[`Self::
    /// execute_predicate_delete_form`] と同じ手順（スキーマ取得 → 束縛 → 内容照合
    /// ハッシュ計算 → 実行）に `RETURNING` の投影束縛（書き込み前。未知列は
    /// `22000`・式項目は `42601`）を加える。削除前の値を返す。`stmt.returning()` は
    /// 呼び出し元が `Some` を確認済みの前提（`None` は `Internal`）。
    fn execute_predicate_delete_returning_form(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
    ) -> Result<crate::sql::exec::ReturningOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.execute_predicate_delete_returning_form_pre(target, ctx, session, stmt, None)
    }

    /// 内側の実装。`pre` が `Some` のとき、`WHERE` のサブクエリは呼び出し元（データ変更 CTE の
    /// 段 A。Issue #1877）が書き込み前に解決済みで、その結果（サブクエリが無ければ `None`）をそのまま使う。
    /// 台帳ハッシュは解決前の文から出す契約（SQL-53 (5)）は変わらない。
    fn execute_predicate_delete_returning_form_pre(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedPredicateDelete,
        pre: Option<Option<crate::sql::allowlist::ValidatedPredicateDelete>>,
    ) -> Result<crate::sql::exec::ReturningOutcome, crate::sql::allowlist::SqlSurfaceError> {
        let schema = self.load_table_schema_for_dml(stmt.table_name())?;
        let returning =
            crate::sql::parser::bind_returning(stmt.returning(), &schema)?.ok_or_else(|| {
                crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "execute_predicate_delete_returning_form called without RETURNING"
                        .to_string(),
                }
            })?;
        let resolved = match pre {
            Some(resolved) => resolved,
            None => {
                self.resolve_predicate_delete_subqueries(&target, ctx, session, stmt, &schema)?
            }
        };
        let (bound, content_hash_value) = Self::prepare_predicate_delete(
            resolved.as_ref().unwrap_or(stmt),
            stmt,
            &schema,
            session.udfs(),
        )?;
        crate::sql::exec::execute_predicate_delete_returning(
            target,
            ctx,
            &bound,
            self.dml_ledger_mode(),
            &schema,
            &content_hash_value,
            self.dml_limits.max_affected_rows,
            self.scan_limits.max_scanned_rows(),
            &returning,
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
        target: crate::tenant::WriteTarget<'_>,
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
        crate::sql::exec::execute_delete_returning_in(
            target,
            ctx,
            &bound,
            self.dml_ledger_mode(),
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
    /// が `LedgerMode::Ledgered`（`--require-operation-id`）である限りこの段階で `23502` として拒否され、
    /// 書き込みトランザクションは一切開始されない。TASK-92・対象ビヘイビア:
    /// RECOVER-1）→ [`Self::execute_update_form`]（束縛・実行本体）の順に呼ぶ。
    ///
    /// `RETURNING` 付き文は本エントリポイントでは `42601` で拒否する
    /// （`validate_update` が構造検証段で拒否。`RETURNING` はセッション経路
    /// [`Self::execute_sql_in_session`] 専用。Issue #1182）。
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
        self.execute_update_form(
            crate::tenant::WriteTarget::Autocommit(&self.storage),
            ctx,
            &stmt,
            &lookup,
        )
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
        target: crate::tenant::WriteTarget<'_>,
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
        let bound = {
            let set_columns: Vec<&str> = stmt.assignments.iter().map(|(n, _)| n.as_str()).collect();
            let _defer = self.defer_not_null_scope_for_update(ctx, &schema.name, &set_columns);
            crate::sql::parser::bind_update(stmt, &schema)?
        };
        crate::sql::exec::execute_update_with_schema_in(
            target,
            ctx,
            &bound,
            self.dml_ledger_mode(),
            Some(&schema),
            Some(&self.trigger_runtime),
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
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedUpdateForm,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.execute_predicate_update_form_pre(target, ctx, session, stmt, None)
    }

    /// 内側の実装。`pre` が `Some` のとき、`WHERE` のサブクエリは呼び出し元（データ変更 CTE の
    /// 段 A。Issue #1877）が書き込み前に解決済みで、その結果（サブクエリが無ければ `None`）をそのまま使う。
    /// 台帳ハッシュは解決前の文から出す契約（SQL-53 (5)）は変わらない。
    fn execute_predicate_update_form_pre(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedUpdateForm,
        pre: Option<Option<crate::sql::allowlist::ValidatedPredicateUpdate>>,
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
        let ValidatedUpdateForm::Predicate(validated) = stmt else {
            // `Single` は `bind_update_form` へ渡す前にここで直接処理する
            // （`Predicate` 分岐専用の `Self::run_predicate_update` へは
            // 渡さない。抽出前と挙動を完全に同一に保つ）。
            let bound = {
                // BEFORE の UPDATE トリガがあれば、束縛時の NOT NULL 検査を最終 NEW まで後回しにする。
                let set_columns: Vec<&str> = match stmt {
                    ValidatedUpdateForm::Single(s) => {
                        s.assignments.iter().map(|(n, _)| n.as_str()).collect()
                    }
                    ValidatedUpdateForm::Predicate(p) => {
                        p.assignments().iter().map(|(n, _)| n.as_str()).collect()
                    }
                };
                let _defer = self.defer_not_null_scope_for_update(ctx, &schema.name, &set_columns);
                crate::sql::parser::bind_update_form(stmt, &schema, session.udfs())?
            };
            let crate::sql::parser::BoundUpdateForm::Single(bound) = bound else {
                return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "internal: bound single-row UPDATE from a predicate-form statement"
                        .to_string(),
                });
            };
            return crate::sql::exec::execute_update_with_schema_in(
                target,
                ctx,
                &bound,
                self.dml_ledger_mode(),
                Some(&schema),
                Some(&self.trigger_runtime),
            );
        };
        // SQL-53（Issue #1751）: WHERE 内のサブクエリは束縛の前に書き込み txn のスナップショットで解決する。
        let resolved = match pre {
            Some(resolved) => resolved,
            None => {
                self.resolve_predicate_update_subqueries(&target, ctx, session, stmt, &schema)?
            }
        };
        self.run_predicate_update_resolved(
            target,
            ctx,
            session.udfs(),
            resolved.as_ref().unwrap_or(validated),
            validated,
            &schema,
            Some(&self.trigger_runtime),
        )
    }

    /// 述語形 `UPDATE` の束縛と内容照合ハッシュ計算（RECOVER-11）。戻り値の
    /// 第 3 要素は Issue #1061 の互換用 `legacy_hashes`（不要なら空）。`RETURNING`
    /// の有無に関わらず**同一の入力**（SET 割当・`WHERE` 述語のみ）から計算する
    /// ため、[`Self::run_predicate_update`] と [`Self::execute_update_returning_form`]
    /// が共有する（`RETURNING` の有無で再送判定が変わらない契約〔Issue #1182〕を
    /// コード上でも 1 箇所に固定）。
    ///
    /// `bind_stmt`／`validated`（ハッシュ用の元の文）の意味は
    /// [`Self::prepare_predicate_delete`] と同じ。サブクエリつきの文でも `legacy_hashes` は一律に計算する
    /// （サブクエリつきは従来 `0A000` で記録されず正準化前エントリは存在しないため判定は弱まらない）。
    fn prepare_predicate_update(
        bind_stmt: &crate::sql::allowlist::ValidatedPredicateUpdate,
        validated: &crate::sql::allowlist::ValidatedPredicateUpdate,
        schema: &crate::catalog::TableSchema,
        udfs: &crate::sql::udf_call::UdfRegistry,
    ) -> Result<
        (
            crate::sql::parser::BoundPredicateUpdate,
            crate::recovery::content_hash::ContentHash,
            Vec<crate::recovery::content_hash::ContentHash>,
        ),
        crate::sql::allowlist::SqlSurfaceError,
    > {
        let predicate = crate::sql::parser::bind_predicate_update(bind_stmt, schema, udfs)?;
        let assignment_refs: Vec<(&str, &crate::sql::allowlist::SetValue)> = validated
            .assignments()
            .iter()
            .map(|(name, literal)| (name.as_str(), literal))
            .collect();
        let content_hash_value = crate::recovery::content_hash::for_update_where(
            validated.table_name(),
            &assignment_refs,
            validated.where_predicates(),
            udfs,
            schema,
        )?;
        // Issue #1061: VECTOR 列への SET 割当を含む場合のみ、正準化前
        // （タグ 1・生テキスト）のレイアウトでもハッシュを計算し
        // `legacy_hashes` として渡す。正準化前に記録された台帳エントリを
        // 持つ再送を `22023`（内容不一致）へ誤判定しないための互換経路
        // （新規記録には常に正準ハッシュ `content_hash_value` を使うため
        // `23505`／`22023` の判定は弱まらない。`docs/design/
        // nosql-update-delete-mapping.md`「述語形 VECTOR 割当の表現統一と
        // 既存台帳エントリの互換性」参照）。
        let legacy_hash: Vec<crate::recovery::content_hash::ContentHash> =
            if crate::recovery::content_hash::needs_legacy_vector_hash(&assignment_refs, schema) {
                vec![
                    crate::recovery::content_hash::for_update_where_legacy_text_vector(
                        validated.table_name(),
                        &assignment_refs,
                        validated.where_predicates(),
                        udfs,
                    )?,
                ]
            } else {
                Vec::new()
            };
        Ok((predicate, content_hash_value, legacy_hash))
    }

    /// [`Self::execute_sql_in_session`]（autocommit）と [`Self::execute_in_active_txn`]
    /// （`InTxn`。Issue #1272）の `RETURNING` 付き `UPDATE`
    /// （Issue #1182・SQL-21。単一行 `id` 指定形・述語形の双方）分岐が呼ぶ実行
    /// 本体。[`Self::execute_predicate_update_form`] と同じ手順（スキーマ取得 →
    /// 束縛 → （述語形のみ）内容照合ハッシュ計算 → 実行）に `RETURNING` の投影束縛
    /// （書き込み前）を加える。更新後の値を返す。`returning` は呼び出し元が
    /// `Some` を確認済みの前提（`None` は `Internal`）。
    fn execute_update_returning_form(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedUpdateForm,
    ) -> Result<crate::sql::exec::ReturningOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.execute_update_returning_form_pre(target, ctx, session, stmt, None)
    }

    /// 内側の実装。`pre` が `Some` のとき、`WHERE` のサブクエリは呼び出し元（データ変更 CTE の
    /// 段 A。Issue #1877）が書き込み前に解決済みで、その結果（サブクエリが無ければ `None`）をそのまま使う。
    /// 台帳ハッシュは解決前の文から出す契約（SQL-53 (5)）は変わらない。
    fn execute_update_returning_form_pre(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        stmt: &crate::sql::allowlist::ValidatedUpdateForm,
        pre: Option<Option<crate::sql::allowlist::ValidatedPredicateUpdate>>,
    ) -> Result<crate::sql::exec::ReturningOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::ValidatedUpdateForm;

        let (table_name, projection) = match stmt {
            ValidatedUpdateForm::Single(v) => (v.table_name.as_str(), v.returning.as_ref()),
            ValidatedUpdateForm::Predicate(v) => (v.table_name(), v.returning()),
        };
        let schema = self.load_table_schema_for_dml(table_name)?;
        let returning =
            crate::sql::parser::bind_returning(projection, &schema)?.ok_or_else(|| {
                crate::sql::allowlist::SqlSurfaceError::Internal {
                    detail: "execute_update_returning_form called without RETURNING".to_string(),
                }
            })?;
        match stmt {
            ValidatedUpdateForm::Single(_) => {
                let bound = crate::sql::parser::bind_update_form(stmt, &schema, session.udfs())?;
                let crate::sql::parser::BoundUpdateForm::Single(bound) = bound else {
                    return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                        detail: "internal: bound predicate-form UPDATE from a single-row statement"
                            .to_string(),
                    });
                };
                crate::sql::exec::execute_update_returning_in(
                    target,
                    ctx,
                    &bound,
                    self.dml_ledger_mode(),
                    &returning,
                    &schema,
                )
            }
            ValidatedUpdateForm::Predicate(validated) => {
                let resolved = match pre {
                    Some(resolved) => resolved,
                    None => self.resolve_predicate_update_subqueries(
                        &target, ctx, session, stmt, &schema,
                    )?,
                };
                let (bound, content_hash_value, legacy_hashes) = Self::prepare_predicate_update(
                    resolved.as_ref().unwrap_or(validated),
                    validated,
                    &schema,
                    session.udfs(),
                )?;
                crate::sql::exec::execute_predicate_update_returning(
                    target,
                    ctx,
                    &bound,
                    self.dml_ledger_mode(),
                    &schema,
                    &content_hash_value,
                    &legacy_hashes,
                    self.dml_limits.max_affected_rows,
                    self.scan_limits.max_scanned_rows(),
                    &returning,
                )
            }
        }
    }

    /// [`Self::execute_predicate_update_form`]（SQL 表層。SQL-19・TASK-192、
    /// Issue #871）と [`Self::execute_bound_predicate_update_in_session`]
    /// （NoSQL 表層 `update` op の `filter`。TASK-186・NOSQL-12、Issue #1062）が
    /// スキーマ取得より後で共有する実行本体（[`Self::run_predicate_delete`]と
    /// 対になる。第 2 の実行器を作らない設計）。抽出前と挙動が完全に同一で
    /// あることは既存の engine テスト（`sql_predicate_dml_exec`・
    /// `sql_update_delete_session_public_api` 等）で回帰確認済み。
    ///
    /// `udfs` は呼び出し元が解決済みの UDF レジストリ（[`Self::
    /// run_predicate_delete`] と同じ契約）を渡す。
    fn run_predicate_update(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        udfs: &crate::sql::udf_call::UdfRegistry,
        validated: &crate::sql::allowlist::ValidatedPredicateUpdate,
        schema: &crate::catalog::TableSchema,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        self.run_predicate_update_resolved(target, ctx, udfs, validated, validated, schema, None)
    }

    /// [`Self::run_predicate_update`] の本体（`bind_stmt`／`hash_stmt` の意味は
    /// [`Self::run_predicate_delete_resolved`] と同じ）。
    #[allow(clippy::too_many_arguments)]
    fn run_predicate_update_resolved(
        &self,
        target: crate::tenant::WriteTarget<'_>,
        ctx: &PolicyContext,
        udfs: &crate::sql::udf_call::UdfRegistry,
        bind_stmt: &crate::sql::allowlist::ValidatedPredicateUpdate,
        hash_stmt: &crate::sql::allowlist::ValidatedPredicateUpdate,
        schema: &crate::catalog::TableSchema,
        triggers: Option<&crate::trigger::TriggerRuntime>,
    ) -> Result<crate::sql::exec::UpdateOutcome, crate::sql::allowlist::SqlSurfaceError> {
        // 多層防御（Issue #1129）: 分割実行修飾付きの文は原子実行器へ到達させない。
        if bind_stmt.partitioned().is_some() {
            return Err(crate::sql::allowlist::SqlSurfaceError::PartitionedDmlInTransaction);
        }
        let (predicate, content_hash_value, legacy_hash) = {
            // トリガ結線経路（SQL）だけ、BEFORE の UPDATE トリガがあれば束縛時の NOT NULL 検査を後回しにする。
            let set_columns: Vec<&str> = bind_stmt
                .assignments()
                .iter()
                .map(|(n, _)| n.as_str())
                .collect();
            let _defer = triggers.and_then(|_| {
                self.defer_not_null_scope_for_update(ctx, &schema.name, &set_columns)
            });
            Self::prepare_predicate_update(bind_stmt, hash_stmt, schema, udfs)?
        };
        let legacy_hashes: &[crate::recovery::content_hash::ContentHash] = legacy_hash.as_slice();
        crate::sql::exec::execute_predicate_update_in(
            target,
            ctx,
            &predicate,
            self.dml_ledger_mode(),
            schema,
            &content_hash_value,
            legacy_hashes,
            self.dml_limits.max_affected_rows,
            self.scan_limits.max_scanned_rows(),
            triggers,
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
            let stmt = crate::sql::allowlist::validate_insert_with_limit(
                sql,
                &lookup,
                self.ledger_mode,
                self.dml_limits.max_insert_rows_per_statement,
            )?;
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
            self.reject_index_derived_conflict_constraint(
                &stmt.table_name,
                stmt.on_conflict.as_ref(),
            )?;
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
                default_applied_columns: &bound.default_applied_columns,
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
            self.scan_limits.max_scanned_rows(),
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

    /// Issue #1921（P0）: テナント A でロードした UDF 入りセッションを B の ctx で実行すると拒否される。
    #[test]
    fn loaded_udf_session_rejects_other_tenant_ctx() {
        let dir = tempdir();
        let core = new_core(dir.path());
        let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
        let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
        let mut session = crate::sql::mode::SessionState::default();
        core.load_session_udfs(&ctx_a, &mut session)
            .expect("load a");
        assert_eq!(session.resolution_tenant(), Some("tenant-a"));
        let err = core
            .execute_sql_in_session(&ctx_b, &mut session, "SELECT 1")
            .expect_err("tenant mismatch must be rejected");
        assert_eq!(err.wire_code(), "XX000");
        // 別テナントへ束縛済みのセッションへの load も fail-closed。
        assert!(core.load_session_udfs(&ctx_b, &mut session).is_err());
    }

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
            .plan_using_plan_expansion(&ctx, &session, validated.table_name(), None, question, None)
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
            &crate::result_limits::ResultLimits::default(),
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
            .plan_using_plan_expansion(&ctx, &session, validated.table_name(), None, question, None)
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

        let empty_strings: Vec<Option<String>> = std::iter::repeat_with(|| Some(String::new()))
            .take(1024)
            .collect();
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
            volatile_default_columns: Vec::new(),
            id_omitted: false,
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

    // codex-review P2 指摘対応（PR #1121）・Issue #1356: `reject_unsupported_predicate_dml_forms`
    // の許可範囲（述語形 DML の SQL パリティ）を固定する。許可形は受理し、それ以外は
    // すべて `42601` で拒否する。
    #[test]
    fn reject_unsupported_predicate_dml_forms_allows_only_the_documented_variants() {
        use crate::sql::allowlist::{CompareOp, WherePredicate};
        use crate::sql::udf_call::{BinOp, Expr};

        let eq = |v: &str| WherePredicate::Equality {
            column: "lang".to_string(),
            value: v.to_string(),
        };
        let num = |op: BinOp, n: &str| {
            WherePredicate::Expression(Expr::Binary {
                op,
                lhs: Box::new(Expr::Ident("qty".to_string())),
                rhs: Box::new(Expr::Number(n.to_string())),
            })
        };
        let compare = WherePredicate::Compare {
            column: "amount".to_string(),
            op: CompareOp::Lt,
            value: "1".to_string(),
        };
        let in_list = WherePredicate::InList {
            column: "lang".to_string(),
            values: vec!["ja".to_string()],
        };

        let allowed = [
            eq("ja"),
            WherePredicate::BoolEquality {
                column: "flag".to_string(),
                value: true,
            },
            WherePredicate::Prefix {
                lhs: crate::sql::udf_call::Expr::Ident("path".to_string()),
                pattern: "src/%".to_string(),
                case_insensitive: false,
            },
            WherePredicate::Between {
                column: "amount".to_string(),
                low: "1".to_string(),
                high: "2".to_string(),
            },
            WherePredicate::IsNull {
                column: "flag".to_string(),
                negated: false,
            },
            WherePredicate::Not(Box::new(eq("ja"))),
            WherePredicate::Not(Box::new(WherePredicate::IsNull {
                column: "flag".to_string(),
                negated: false,
            })),
            compare.clone(),
            in_list.clone(),
            WherePredicate::Not(Box::new(compare.clone())),
            WherePredicate::Not(Box::new(in_list.clone())),
            num(BinOp::Eq, "5"),
            num(BinOp::Le, "5"),
            WherePredicate::Or(vec![vec![eq("ja")], vec![eq("en")]]),
            WherePredicate::Or(vec![
                vec![num(BinOp::Ge, "1"), num(BinOp::Le, "5")],
                vec![WherePredicate::Or(vec![
                    vec![eq("ja")],
                    vec![compare.clone()],
                ])],
            ]),
        ];
        for predicate in allowed {
            assert!(
                reject_unsupported_predicate_dml_forms(std::slice::from_ref(&predicate)).is_ok(),
                "documented predicate form must be accepted: {predicate:?}"
            );
        }

        let call_expr = WherePredicate::Expression(Expr::Binary {
            op: BinOp::Eq,
            lhs: Box::new(Expr::Call {
                name: "abs".to_string(),
                args: vec![Expr::Ident("qty".to_string())],
            }),
            rhs: Box::new(Expr::Number("1".to_string())),
        });
        let rejected = [
            WherePredicate::BoolColumn {
                column: "flag".to_string(),
            },
            WherePredicate::PredicateCall {
                name: "visible".to_string(),
            },
            // `Not` は許可葉を直接包む場合だけ通す。
            WherePredicate::Not(Box::new(WherePredicate::Not(Box::new(eq("ja"))))),
            WherePredicate::Not(Box::new(num(BinOp::Eq, "5"))),
            WherePredicate::Not(Box::new(WherePredicate::Or(vec![
                vec![eq("ja")],
                vec![eq("en")],
            ]))),
            // 式述語は「列 × 数値リテラルの比較」に限る。
            call_expr.clone(),
            WherePredicate::Expression(Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(Expr::Ident("qty".to_string())),
                rhs: Box::new(Expr::Number("1".to_string())),
            }),
            // `Or` は 2 分岐以上・空分岐なし・内側の RLS 述語／呼び出し式も拒否。
            WherePredicate::Or(vec![vec![eq("ja")]]),
            WherePredicate::Or(vec![vec![eq("ja")], vec![]]),
            WherePredicate::Or(vec![
                vec![eq("ja")],
                vec![WherePredicate::PredicateCall {
                    name: "visible".to_string(),
                }],
            ]),
            WherePredicate::Or(vec![vec![eq("ja")], vec![call_expr]]),
        ];
        for predicate in rejected {
            let err = reject_unsupported_predicate_dml_forms(std::slice::from_ref(&predicate))
                .expect_err("undocumented predicate form must be rejected");
            assert_eq!(
                err.wire_code(),
                "42601",
                "unexpected wire_code for {predicate:?}: {err:?}"
            );
        }
        assert_eq!(
            reject_unsupported_predicate_dml_forms(&[])
                .expect_err("empty predicate list must be rejected")
                .wire_code(),
            "42601"
        );
    }
}

/// NoSQL update/delete フィルタのゲートが式左辺の LIKE を拒否することの単体テスト
/// （SQL-54、Issue #1781）。
#[cfg(test)]
mod like_lhs_expr_gate_tests {
    use super::*;
    use crate::sql::allowlist::WherePredicate;
    use crate::sql::udf_call::Expr;

    fn like(lhs: Expr) -> WherePredicate {
        WherePredicate::Prefix {
            lhs,
            pattern: "a%".to_string(),
            case_insensitive: false,
        }
    }

    #[test]
    fn expression_lhs_is_rejected_and_bare_column_is_accepted() {
        let expr = Expr::Call {
            name: "lower".to_string(),
            args: vec![Expr::Ident("path".to_string())],
        };
        let col = Expr::Ident("path".to_string());
        assert!(reject_unsupported_predicate_dml_conjunction(&[like(col)], 0).is_ok());
        assert!(reject_unsupported_predicate_dml_conjunction(&[like(expr.clone())], 0).is_err());
        let negated = WherePredicate::Not(Box::new(like(expr)));
        assert!(reject_unsupported_predicate_dml_conjunction(&[negated], 0).is_err());
    }
}
