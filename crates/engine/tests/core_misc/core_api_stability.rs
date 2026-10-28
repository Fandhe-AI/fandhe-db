//! コア API（`VectorCore` / `SearchProvider`）シグネチャの安定性チェック
//! （TASK-125・対象ビヘイビア: CORE-1）。
//!
//! `crates/engine/src/core.rs` が明記するとおり、`wire-server`（および将来の他
//! プロトコル実装）は本クレートの [`VectorCore`] trait のみに依存する。プロトコル層の
//! 追加・変更がコア API のシグネチャ変更を伴わないことを、ここでは「関数ポインタ型への
//! 強制代入」でコンパイル時に固定する（シグネチャが変われば本ファイルがコンパイルエラーに
//! なる）。トレイトの object-safe 性（`Box<dyn VectorCore>` / `Box<dyn SearchProvider>`
//! として構築できること）も併せて固定する。
//!
//! `&dyn VectorCore` のみを介したプロトコルアダプタ経由の呼び出し・テナント境界
//! （不可視行の非混入・NotFound 統一）の結合テストは `tests/vector_core.rs`
//! （TASK-124）が既にカバーしているため、本ファイルでは重複させない。

use engine::core::{CoreError, EngineCore, VectorCore};
use engine::kernel::{
    CandidateHit, CpuScalarProvider, KernelError, SearchHit, SearchInput, SearchProvider,
};
use engine::policy::PolicyContext;
use engine::storage::Row;

// `VectorCore::search` のシグネチャをコンパイル時に固定する。引数・戻り値の型や
// 個数が変わると本代入自体がコンパイルエラーになる（trait メソッドのシグネチャ変更を
// CI が機械的に検知する）。シグネチャ固定という目的上、引数の多さそのものが検査対象
// であり `type` エイリアスへ分割する意味がないため、本行のみ clippy::type_complexity
// を許容する。
#[allow(clippy::type_complexity)]
const _SEARCH_SIGNATURE: fn(
    &EngineCore,
    &PolicyContext,
    &str,
    &[f32],
    usize,
) -> Result<Vec<SearchHit>, CoreError> = <EngineCore as VectorCore>::search;

// `VectorCore::get_row` のシグネチャをコンパイル時に固定する。行 `id` の一意性スコープが
// テナント内（対象ビヘイビア: TABLE-12）のため、点取得のキーは `(tenant_id, id)`。
#[allow(clippy::type_complexity)]
const _GET_ROW_SIGNATURE: fn(
    &EngineCore,
    &PolicyContext,
    &str,
    &str,
    u64,
) -> Result<Row, CoreError> = <EngineCore as VectorCore>::get_row;

// `SearchProvider::search` は `SearchInput<'_>` を借用するため、任意のライフタイムで
// 成立すること（HRTB）まで含めてシグネチャを固定する。
// provider が返すのは候補ヒット（`CandidateHit`。識別子は呼び出し元定義のスロット番号）で、
// テナント修飾済みの `SearchHit` は `VectorCore::search` の戻り値としてコア側が構築する
// （TABLE-12・RLS-9。`kernel.rs` の型ドキュメント参照）。
const _PROVIDER_SEARCH_SIGNATURE: for<'a> fn(
    &CpuScalarProvider,
    SearchInput<'a>,
) -> Result<Vec<CandidateHit>, KernelError> = <CpuScalarProvider as SearchProvider>::search;

// object-safe 性の固定（CORE-1）: `VectorCore` / `SearchProvider` はいずれも
// `Box<dyn _>` として構築できなければならない（ジェネリクスを持ち込むとコンパイル
// エラーになる）。関数として定義するだけで型検査が働く。
#[allow(dead_code)]
fn _assert_vector_core_is_object_safe(core: EngineCore) -> Box<dyn VectorCore> {
    Box::new(core)
}

#[allow(dead_code)]
fn _assert_search_provider_is_object_safe(provider: CpuScalarProvider) -> Box<dyn SearchProvider> {
    Box::new(provider)
}
