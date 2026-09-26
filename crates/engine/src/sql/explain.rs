//! `EXPLAIN`（TASK-78・SQL-6）応答の構築。`core.rs::EngineCore::
//! execute_sql_in_session` の `Statement::Explain` アームが LLM クエリ展開・
//! モード解決（`EngineCore::plan_query_with_mode`、TASK-164・PLAN-11）した結果
//! （[`PlannedQuery`]）と、検索エンジン種別・ANN 静的適用判定
//! （[`ExplainEngine`]、Issue #411）を受け取り、クライアントが確認できる
//! `QUERY PLAN` 単一列の [`QueryResult`] へ決定的に整形するところまでを担う。
//!
//! 責務境界: 本モジュールは純粋な整形ロジックのみを持つ（DB I/O・LLM 呼び出しは
//! 行わない。呼び出し元 `core.rs` が LLM 展開・モード解決・[`ExplainEngine`] の
//! 組み立てを完了させたうえで [`build_explain_result`] を呼ぶ）。`EXPLAIN` は
//! 検索本体（ハイブリッド実行）を実行しないため、行の `id`/`score` は実在行を
//! 持たない疑似値（`0`）とする。`engine:`／`ann_plan:` 行も実行時の縮退結果では
//! なく、クエリ形状とエンジン設定から決まる**静的判定**
//! （`sql::hnsw_cache::classify_ann_plan`）をそのまま報告する（実行時
//! fail-closed 縮退・hybrid 再取得ラウンド数は対象外。可視カーディナリティ・
//! 閾値・行数等のテナント存在情報に繋がる数値は一切含めない。security.md
//! 「テナント境界」対応）。
//!
//! 行内容は SQL-6・SQL-12（TASK-161・PLAN-11）が要求する「展開後の検索語・
//! ソフトヒント・解決済み実効モードと指定元」に、Issue #411 で「使用エンジン・
//! ANN パラメータ・適用判定」を追記したもの。決定的順序・英語表記（プログラム
//! 出力文字列は英語）で並べる。一度出した行の形式・順序は**安定契約**として
//! 今後変更しない（`sql::mode::ModeSource::as_str` のドキュメントコメントと
//! 同じ方針）。既存 6 行（`search_terms[i]`…`mode_source`）は不変、新規行は
//! `mode_source` の直後へ追記のみで既定エンジン時の出力は変更前と後方互換
//! （TASK-164 で `mode_source` を追加した前例と同じ方針）。security.md P0:
//! LLM プロンプト本文・生応答本文は含めず、厳格パース済みの構造化フィールド
//! （[`crate::query_planner::QueryExpansion`]）のみを使う。
//!
//! `docs/design/explain-search-engine-exposure.md` に露出する行・語彙・
//! 露出しない値と理由をまとめる。
//!
//! TASK-186・NOSQL-10 の前提として Issue #730 で [`ExplainEngine`]・
//! [`build_explain_result`] を公開 API へ昇格した（`BoundScan`／`BoundAggregate`
//! と同じ「同作法」）。外部から [`PlannedQuery`] を得るには
//! `EngineCore::plan_query_with_mode`（`pub`）を使う（`PlannedQuery::new` 自体は
//! `pub(crate)` のまま。対象外）。[`ExplainEngine`] は `Self::new` で構築し、
//! `EXPLAIN` が検索本体を実行しない契約（本ファイル冒頭「責務境界」参照）は
//! 呼び出し元にも求められる。

use crate::declarative_filter::MetadataFilter;
use crate::query_planner::PlannedQuery;
use crate::search_engine::SearchEngineKind;
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::hnsw_cache::AnnPlan;
use crate::sql::mode::ResolvedMode;
use crate::sql::scalar_plan::{classify_scalar_plan, ScalarPlan, ScalarShapeInput};
use crate::sql::udf_call::BoundExpr;

/// `EXPLAIN` 応答の列名（安定契約。一度出したら変えない）。
/// `EXPLAIN` の唯一の結果列名。`core.rs::EngineCore::describe_parsed_in_session`
/// （Issue #933・TASK-71・WIRE-11）が Describe（'D' 種別 S）の応答をプラン本体を
/// 実行せずに組み立てるため `pub(crate)` へ昇格した（実行結果に依存しない固定値
/// のため、Describe 側は本定数を直接参照するだけで済む）。
pub(crate) const QUERY_PLAN_COLUMN: &str = "QUERY PLAN";

/// ソフトヒント未指定時の固定表記（安定契約）。
const NONE_LABEL: &str = "(none)";

/// `search_engine_kind()` が `None`（provider を直接注入する `with_provider`／
/// `from_storage` 経由。`kind` との対応を構造的に検証できない）の場合の固定表記
/// （安定契約）。ヒント未指定の [`NONE_LABEL`] と意味が異なるため区別する。
const CUSTOM_PROVIDER_LABEL: &str = "(custom_provider)";

/// `EXPLAIN` の `engine:`／`hnsw_params:`／`ann_plan:` 行（Issue #411）を組み立てる
/// ための入力。呼び出し元 `core.rs::EngineCore::execute_sql_in_session` の
/// `Statement::Explain` アームが、実行時に executor（`sql::exec`）が使うのと同じ
/// 源泉（`EngineCore::search_engine_kind()`・`sql::hnsw_cache::classify_ann_plan`）
/// から組み立てる。
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ExplainEngine {
    /// [`crate::core::EngineCore::search_engine_kind`] の戻り値そのまま。
    pub(crate) kind: Option<SearchEngineKind>,
    /// [`crate::sql::hnsw_cache::classify_ann_plan`] の判定結果（静的判定）。
    pub(crate) ann_plan: AnnPlan,
    /// [`crate::sql::scalar_plan::classify_scalar_plan`] の判定結果
    /// （静的判定。Issue #474）。
    pub(crate) scalar_plan: ScalarPlan,
}

impl ExplainEngine {
    /// [`build_explain_result`] の入力を組み立てる（TASK-186・NOSQL-10 の前提。
    /// Issue #730）。`kind`・`ann_plan`・`scalar_plan` はいずれも
    /// `core.rs::EngineCore::execute_sql_in_session` の `Statement::Explain` アーム
    /// が SQL `EXPLAIN` 経路で使うのと同じ源泉（`EngineCore::search_engine_kind()`・
    /// [`crate::sql::hnsw_cache::classify_ann_plan`]・
    /// [`crate::sql::scalar_plan::classify_scalar_plan`]）から組み立てる想定
    /// （呼び出し元がこの単一情報源を経由しない値を渡した場合、`EXPLAIN` の
    /// 出力と一致しなくなる）。
    pub fn new(kind: Option<SearchEngineKind>, ann_plan: AnnPlan, scalar_plan: ScalarPlan) -> Self {
        Self {
            kind,
            ann_plan,
            scalar_plan,
        }
    }

    /// [`crate::core::EngineCore::search_engine_kind`] の戻り値そのまま。
    pub fn kind(&self) -> Option<SearchEngineKind> {
        self.kind
    }

    /// ANN（HNSW）経路の静的適用判定（Issue #411）。
    pub fn ann_plan(&self) -> AnnPlan {
        self.ann_plan
    }

    /// SCALAR 索引の静的適用判定（Issue #474）。
    pub fn scalar_plan(&self) -> ScalarPlan {
        self.scalar_plan
    }
}

/// [`crate::sql::using_plan::pre_check_bindable`]（`Statement::Explain`
/// アーム・[`crate::core::EngineCore::explain_bound_plan_in_session`] が共有
/// する私的ヘルパー `run_explain_plan` が LLM I/O より前に一度だけ呼ぶ binder
/// closure）の戻り値。`WHERE` 述語の構造のみから決まる、束縛の副産物である
/// 形状情報のみを運ぶ（旧 `sql::using_plan::PreCheckShape` を TASK-186・
/// NOSQL-10 の前提として `sql::explain` の公開型へ昇格したもの。Issue #765）。
///
/// `filters_empty`（[`AnnShapeInput::filters_empty`](crate::sql::hnsw_cache::AnnShapeInput)
/// が要求する形状）は `metadata_filters`／`expr_filters` の両方が空である
/// ことを指す。`WHERE visible()`
/// （[`crate::sql::parser::WherePredicate::PredicateCall`]）はこの 2 つの列を
/// 増やさず RLS フラグのみを立てるため、`WHERE` 句自体が非空でも
/// `filters_empty` が `true` になりうる（`sql::exec` の
/// `bound.metadata_filters.is_empty() && bound.expr_filters.is_empty()` と
/// 同じ定義）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExplainShape {
    filters_empty: bool,
    scalar_plan: ScalarPlan,
}

impl ExplainShape {
    /// `metadata_filters`／`expr_filters`（`USING PLAN` の `WHERE` 束縛結果。
    /// [`crate::sql::parser::bind_where_predicates`] の戻り値の一部）から
    /// [`ExplainShape`] を組み立てる。`USING PLAN` は `HINT ORDER` を受理
    /// しない（SQL-5・許可リスト層）ため SCALAR 段は常に DISTANCE 段より先に
    /// 評価される契約に基づき、`scalar_prefilter: true` 固定で
    /// [`classify_scalar_plan`] を呼ぶ（[`crate::sql::using_plan::
    /// pre_check_bindable`] の既存契約をそのまま引き継ぐ）。
    ///
    /// `or_filters`（TASK-208・SQL-24、Issue #912）: `WHERE` の `OR` 群。
    /// **BREAKING CHANGE**: 引数を追加した（OR を持たない既存呼び出しは
    /// `&[]` を渡す）。`filters_empty` の判定に含めないと、OR だけの
    /// `WHERE`（例: `WHERE a OR b`）が `USING PLAN` の ANN 適用条件判定
    /// （`AnnShapeInput::filters_empty`）で「フィルタなし」と誤認され、
    /// OR 条件が黙って無視される fail-open のバグになる（security.md
    /// 「不安全な設計」対応）。
    pub fn from_filters(
        metadata_filters: &[MetadataFilter],
        expr_filters: &[BoundExpr],
        or_filters: &[crate::sql::where_tree::BoundOrGroup],
    ) -> Self {
        let scalar_plan = classify_scalar_plan(&ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters,
            expr_filters,
            or_filters,
        });
        Self {
            filters_empty: metadata_filters.is_empty()
                && expr_filters.is_empty()
                && or_filters.is_empty(),
            scalar_plan,
        }
    }

    /// `ann_plan:` 行（Issue #411・[`crate::sql::hnsw_cache::classify_ann_plan`]）
    /// が要求する形状情報。
    pub fn filters_empty(&self) -> bool {
        self.filters_empty
    }

    /// `scalar_plan:` 行（Issue #474）が要求する静的判定。
    pub fn scalar_plan(&self) -> ScalarPlan {
        self.scalar_plan
    }
}

/// [`ExplainEngine::kind`] を `engine:` 行の値（閉じた語彙・snake_case）へ変換する。
/// [`SearchEngineKind`] の [`std::fmt::Display`] 実装は `full_scan_ratio` を含む
/// 診断・ログ向けの表現であり、テナント存在情報に繋がらない値のみを露出する
/// `EXPLAIN` の契約とは別に保つため、ここで専用の網羅 `match` を持つ
/// （`SearchEngineKind` は本クレート内 `#[non_exhaustive]` の影響を受けない）。
fn engine_token(kind: Option<SearchEngineKind>) -> &'static str {
    match kind {
        None => CUSTOM_PROVIDER_LABEL,
        Some(SearchEngineKind::CpuScalarBruteForce) => "cpu_scalar_brute_force",
        Some(SearchEngineKind::ParallelBruteForce) => "parallel_brute_force",
        Some(SearchEngineKind::Hnsw(_)) => "hnsw",
        // `SearchEngineKind` は `#[non_exhaustive]` だが本クレート内なので
        // 網羅チェックは効く。将来 variant が追加された場合はコンパイルエラーで
        // ここへの追記を強制する（fail-closed。未知エンジンを偽装しない）。
    }
}

/// [`AnnPlan`] を `ann_plan:` 行の値（閉じた語彙・snake_case）へ変換する。
fn ann_plan_token(plan: AnnPlan) -> &'static str {
    match plan {
        AnnPlan::PlainScanEngine => "plain_scan_engine",
        AnnPlan::PlainScanPrecision => "plain_scan_precision",
        AnnPlan::HnswFullVisible => "hnsw_full_visible",
        AnnPlan::HnswSubset => "hnsw_subset",
        // codex-review P1 指摘対応（PR #437）: `engine: (custom_provider)`
        // （`kind == None`）のときに限り到達する。実際に ANN か brute-force
        // かを `EngineCore` 側から判別できない旨を明示し、`plain_scan_engine`
        // （厳密 brute-force と確定）と区別する。
        AnnPlan::UnknownCustomProvider => "unknown_custom_provider",
    }
}

/// [`ScalarPlan`] を `scalar_plan:` 行の値（閉じた語彙・snake_case）へ変換する
/// （Issue #474）。
fn scalar_plan_token(plan: ScalarPlan) -> &'static str {
    match plan {
        ScalarPlan::PlainScan => "plain_scan",
        ScalarPlan::IndexEquality => "index_equality",
        ScalarPlan::IndexPrefix => "index_prefix",
        ScalarPlan::IndexIdRange => "index_id_range",
        ScalarPlan::IndexTypedRange => "index_typed_range",
        ScalarPlan::IndexConjunction => "index_conjunction",
    }
}

/// 集計・広域取得の走査方式（Issue #922・SQL-27）。executor の実行時ゲート
/// （`sql::aggregate::execute_aggregate_with_cache`・`sql::group_by::
/// execute_grouped_aggregate`・`sql::scan`）と 1 対 1 対応する**静的判定**の
/// みを表す（可視カーディナリティ・索引の構築可否・キャッシュのヒット/ミス
/// 等の実行時縮退の結果はいずれも含まない。security.md「テナント境界」
/// 対応）。`access_path:` 行の値になる閉じた語彙（`&'static str`・
/// snake_case）。executor の実経路は 4 種（列挙形を候補削減と別に数える）
/// あるため 4 variant を持つ（SQL-27 確定時に spec 側と語彙数を擦り合わせる
/// 事項。`docs/design/explain-search-engine-exposure.md` 参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum AccessPath {
    /// `GROUP BY` なし集計で `DecodeTier::Fast` 相当（`WHERE` なし、
    /// `COUNT(*)`／`id` 系のみ）。可視ビットマップキャッシュを使える形。
    VisibleBitmapCache,
    /// `VECTOR` 列ありのテーブルで、索引対応述語のみで構成される `WHERE` を
    /// 持つ集計（`GROUP BY` の有無いずれも。索引経由の候補削減が使える形）。
    ScalarIndexCandidates,
    /// `GROUP BY` あり・`WHERE` なし・`VECTOR` 列あり・`TEXT` 列の `MIN`／`MAX`
    /// を含まない集計（索引によるグループ列挙形）。
    ScalarIndexGroupEnumeration,
    /// 上記のいずれにも該当しない（`VECTOR` 列なしテーブルの集計・非索引の
    /// 式述語を含む集計・広域取得は常にこれ）。
    FullScan,
}

fn access_path_token(path: AccessPath) -> &'static str {
    match path {
        AccessPath::VisibleBitmapCache => "visible_bitmap_cache",
        AccessPath::ScalarIndexCandidates => "scalar_index_candidates",
        AccessPath::ScalarIndexGroupEnumeration => "scalar_index_group_enumeration",
        AccessPath::FullScan => "full_scan",
    }
}

/// `mode:` から `scalar_plan:` までの末尾行を組み立てる（TASK-78・SQL-6 の
/// [`build_explain_result`]〔`USING PLAN` 付き検索〕と Issue #922・SQL-27 の
/// [`build_search_explain_result`]〔`USING PLAN` なし検索〕が共有する。
/// 書式の発散を構造的に防ぐ）。
fn push_engine_tail_rows(lines: &mut Vec<String>, mode: ResolvedMode, engine: &ExplainEngine) {
    lines.push(format!("mode: {}", mode.mode().as_str()));
    lines.push(format!("mode_source: {}", mode.source().as_str()));
    lines.push(format!("engine: {}", engine_token(engine.kind)));
    if let Some(SearchEngineKind::Hnsw(params)) = engine.kind {
        // `ValidatedHnswParams::get()` は検証済み `m`／`ef_construction`／
        // `ef_search` のみを返す（構築時の静的設定値。`full_scan_ratio` や
        // 実行時の可視カーディナリティ・索引ノード数はここでは露出しない）。
        // `resident=`（Issue #514）も構築時の静的設定値（要求精度）のみで、
        // 実行時の自動縮退結果（`HnswIndex::resident_precision` の実効値）は
        // 露出しない（#411 の「実行時縮退結果は非露出」契約を踏襲）。
        // `sparse_visited_max=`（Issue #497）も同じ区分——構築時の静的閾値
        // （opt-in・既定 0）のみを露出し、実行時にどちらの visited 実装が
        // 選ばれたか・可視候補数・索引ノード数は非露出のまま
        // （`docs/design/explain-search-engine-exposure.md` 参照）。
        // `acorn_max_visible_ratio`（ACORN-1 の 2-hop 展開切替閾値。Issue #501）は
        // `full_scan_ratio` と同じ「切替閾値」区分のため、本行では意図的に
        // 露出しない（実行時のレジーム選択・`acorn_searches`／
        // `acorn_expansions` も同様。§`docs/design/explain-search-engine-
        // exposure.md`「露出しない値」節参照）。
        let p = params.get();
        lines.push(format!(
            "hnsw_params: m={},ef_construction={},ef_search={},resident={},sparse_visited_max={}",
            p.m,
            p.ef_construction,
            p.ef_search,
            params.resident_precision(),
            params.sparse_visited_max()
        ));
    }
    lines.push(format!("ann_plan: {}", ann_plan_token(engine.ann_plan)));
    lines.push(format!(
        "scalar_plan: {}",
        scalar_plan_token(engine.scalar_plan)
    ));
}

/// `lines` の各要素を `QUERY PLAN` 単一列の 1 行（`id`/`score` は実在行を持た
/// ない疑似値 `0`）へ変換する（[`build_explain_result`]・
/// [`build_search_explain_result`]・[`build_relational_explain_result`] が
/// 共有する終端処理）。
fn lines_to_query_result(lines: Vec<String>) -> QueryResult {
    let rows = lines
        .into_iter()
        .map(|text| ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::Text(text)],
        })
        .collect();

    QueryResult {
        columns: vec![ColumnMeta::Computed {
            name: QUERY_PLAN_COLUMN.to_string(),
        }],
        rows,
    }
}

/// [`PlannedQuery`]（LLM 展開結果＋解決済み実効モード）と [`ExplainEngine`]
/// （使用エンジン・ANN 静的判定〔Issue #411〕・SCALAR 索引静的判定
/// 〔Issue #474〕）から `EXPLAIN` の [`QueryResult`] を決定的に構築する
/// （副作用なし。同一入力には常に同一の行を返す）。
/// 行順序: `search_terms[i]`（展開結果の件数分）→ `path_hint` → `kind_hint` →
/// `mode` → `mode_source` → `engine` → （`engine: hnsw` のときのみ）
/// `hnsw_params` → `ann_plan` → `scalar_plan`。
pub fn build_explain_result(planned: &PlannedQuery, engine: &ExplainEngine) -> QueryResult {
    let expansion = planned.expansion();
    let resolved = planned.mode();

    let mut lines: Vec<String> = Vec::with_capacity(expansion.search_terms.len() + 7);
    for (i, term) in expansion.search_terms.iter().enumerate() {
        lines.push(format!("search_terms[{i}]: {term}"));
    }
    lines.push(format!(
        "path_hint: {}",
        expansion.path_hint.as_deref().unwrap_or(NONE_LABEL)
    ));
    lines.push(format!(
        "kind_hint: {}",
        expansion.kind_hint.as_deref().unwrap_or(NONE_LABEL)
    ));
    push_engine_tail_rows(&mut lines, resolved, engine);

    lines_to_query_result(lines)
}

/// `EXPLAIN SELECT ...`（`USING PLAN` を伴わない検索 SELECT。`ORDER BY <=>`・
/// `HYBRID`。Issue #922・SQL-27）の [`QueryResult`] を構築する。行順序:
/// `mode` → `mode_source` → `engine` → （`engine: hnsw` のときのみ）
/// `hnsw_params` → `ann_plan` → `scalar_plan`（[`build_explain_result`] の
/// 末尾部分から LLM 由来の `search_terms[i]`／`path_hint`／`kind_hint` を
/// 除いたもの。[`push_engine_tail_rows`] を共有するため書式は発散しない）。
pub(crate) fn build_search_explain_result(
    mode: ResolvedMode,
    engine: &ExplainEngine,
) -> QueryResult {
    let mut lines: Vec<String> = Vec::with_capacity(6);
    push_engine_tail_rows(&mut lines, mode, engine);
    lines_to_query_result(lines)
}

/// `EXPLAIN SELECT <集計>`・`EXPLAIN SELECT ... LIMIT n`（集計・広域取得。
/// Issue #922・SQL-27）の [`QueryResult`] を構築する。行順序: `scalar_plan` →
/// `access_path`。`mode`・`mode_source`・`engine`・`ann_plan` は出さない
/// （広域取得は `search_mode` を参照せず、どちらの文にもランキング段が無い
/// ため `mode:` を出すと誤情報になる。SQL-27 確定時に spec 側と擦り合わせる
/// 事項）。
pub(crate) fn build_relational_explain_result(
    scalar_plan: ScalarPlan,
    access_path: AccessPath,
) -> QueryResult {
    let lines = vec![
        format!("scalar_plan: {}", scalar_plan_token(scalar_plan)),
        format!("access_path: {}", access_path_token(access_path)),
    ];
    lines_to_query_result(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hnsw::{HnswParams, ValidatedHnswParams};
    use crate::query_planner::QueryExpansion;
    use crate::sql::mode::{ModeSource, ResolvedMode, SearchMode};

    fn cell_text(result: &QueryResult, row: usize) -> &str {
        match &result.rows[row].cells[0] {
            Cell::Text(s) => s.as_str(),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    /// 既定エンジン（`ParallelBruteForce`・`ann_plan: plain_scan_engine`）を
    /// 表す `ExplainEngine`（多くのテストで共通に使う）。
    fn default_engine() -> ExplainEngine {
        ExplainEngine {
            kind: Some(SearchEngineKind::ParallelBruteForce),
            ann_plan: AnnPlan::PlainScanEngine,
            scalar_plan: ScalarPlan::PlainScan,
        }
    }

    #[test]
    fn build_explain_result_orders_search_terms_then_hints_then_mode() {
        let expansion = QueryExpansion {
            search_terms: vec!["alpha".to_string(), "beta".to_string()],
            path_hint: Some("src/lib.rs".to_string()),
            kind_hint: Some("fn".to_string()),
            ..QueryExpansion::default()
        };
        let planned = PlannedQuery::new(
            expansion,
            ResolvedMode::new(SearchMode::Precision, ModeSource::QueryClause),
        );

        let result = build_explain_result(&planned, &default_engine());

        assert_eq!(result.columns.len(), 1);
        assert_eq!(
            result.columns[0],
            ColumnMeta::Computed {
                name: QUERY_PLAN_COLUMN.to_string()
            }
        );
        // 既存 6 行（不変・後方互換）+ Issue #411 の `engine`／`ann_plan` 2 行 +
        // Issue #474 の `scalar_plan` 1 行（既定エンジンでは `hnsw_params` 行は
        // 出ない）。
        assert_eq!(result.rows.len(), 9);
        assert_eq!(cell_text(&result, 0), "search_terms[0]: alpha");
        assert_eq!(cell_text(&result, 1), "search_terms[1]: beta");
        assert_eq!(cell_text(&result, 2), "path_hint: src/lib.rs");
        assert_eq!(cell_text(&result, 3), "kind_hint: fn");
        assert_eq!(cell_text(&result, 4), "mode: precision");
        assert_eq!(cell_text(&result, 5), "mode_source: query_clause");
        assert_eq!(cell_text(&result, 6), "engine: parallel_brute_force");
        assert_eq!(cell_text(&result, 7), "ann_plan: plain_scan_engine");
        assert_eq!(cell_text(&result, 8), "scalar_plan: plain_scan");
    }

    #[test]
    fn build_explain_result_uses_none_label_for_absent_hints() {
        let expansion = QueryExpansion {
            search_terms: Vec::new(),
            path_hint: None,
            kind_hint: None,
            ..QueryExpansion::default()
        };
        let planned = PlannedQuery::new(
            expansion,
            ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
        );

        let result = build_explain_result(&planned, &default_engine());

        // 検索語 0 件のため行は path_hint/kind_hint/mode/mode_source/engine/
        // ann_plan/scalar_plan の 7 行。
        assert_eq!(result.rows.len(), 7);
        assert_eq!(cell_text(&result, 0), "path_hint: (none)");
        assert_eq!(cell_text(&result, 1), "kind_hint: (none)");
        assert_eq!(cell_text(&result, 2), "mode: recall");
        assert_eq!(cell_text(&result, 3), "mode_source: default");
        assert_eq!(cell_text(&result, 4), "engine: parallel_brute_force");
        assert_eq!(cell_text(&result, 5), "ann_plan: plain_scan_engine");
        assert_eq!(cell_text(&result, 6), "scalar_plan: plain_scan");
    }

    #[test]
    fn build_explain_result_reports_all_four_mode_sources() {
        for (mode, source, expected_source) in [
            (SearchMode::Recall, ModeSource::QueryClause, "query_clause"),
            (
                SearchMode::Precision,
                ModeSource::SessionVariable,
                "session_variable",
            ),
            (
                SearchMode::Recall,
                ModeSource::PlannerEstimate,
                "planner_estimate",
            ),
            (SearchMode::Recall, ModeSource::Default, "default"),
        ] {
            let planned =
                PlannedQuery::new(QueryExpansion::default(), ResolvedMode::new(mode, source));
            let result = build_explain_result(&planned, &default_engine());
            // `mode_source` は末尾から 4 番目（末尾 3 行が
            // `engine`／`ann_plan`／`scalar_plan`）。
            let mode_source_row = result.rows.len() - 4;
            assert_eq!(
                cell_text(&result, mode_source_row),
                format!("mode_source: {expected_source}")
            );
        }
    }

    #[test]
    fn build_explain_result_uses_custom_provider_label_when_kind_absent() {
        let planned = PlannedQuery::new(
            QueryExpansion::default(),
            ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
        );
        // codex-review P1 指摘対応（PR #437）: `kind == None` の実運用ペアリングは
        // `AnnPlan::UnknownCustomProvider`（`core.rs` の `EXPLAIN` アームが
        // `engine_kind_unknown: self.search_engine_kind().is_none()` を渡すことで
        // 到達する）。
        let engine = ExplainEngine {
            kind: None,
            ann_plan: AnnPlan::UnknownCustomProvider,
            scalar_plan: ScalarPlan::PlainScan,
        };

        let result = build_explain_result(&planned, &engine);

        let last = result.rows.len() - 1;
        assert_eq!(cell_text(&result, last), "scalar_plan: plain_scan");
        assert_eq!(
            cell_text(&result, last - 1),
            "ann_plan: unknown_custom_provider"
        );
        assert_eq!(cell_text(&result, last - 2), "engine: (custom_provider)");
    }

    #[test]
    fn build_explain_result_reports_hnsw_params_only_for_hnsw_engine() {
        let planned = PlannedQuery::new(
            QueryExpansion::default(),
            ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
        );
        let hnsw_params = ValidatedHnswParams::new(HnswParams::default())
            .expect("既定 HnswParams は常に検証を通過する");
        let engine = ExplainEngine {
            kind: Some(SearchEngineKind::Hnsw(hnsw_params)),
            ann_plan: AnnPlan::HnswFullVisible,
            scalar_plan: ScalarPlan::PlainScan,
        };

        let result = build_explain_result(&planned, &engine);

        // path_hint/kind_hint/mode/mode_source/engine/hnsw_params/ann_plan/
        // scalar_plan の 8 行（`hnsw_params` が挟まる分、既定エンジンより
        // 1 行多い）。
        assert_eq!(result.rows.len(), 8);
        assert_eq!(cell_text(&result, 4), "engine: hnsw");
        assert_eq!(
            cell_text(&result, 5),
            "hnsw_params: m=16,ef_construction=100,ef_search=64,resident=f32,sparse_visited_max=0"
        );
        assert_eq!(cell_text(&result, 6), "ann_plan: hnsw_full_visible");
        assert_eq!(cell_text(&result, 7), "scalar_plan: plain_scan");
    }

    #[test]
    fn build_explain_result_reports_all_five_ann_plan_tokens() {
        for (plan, expected) in [
            (AnnPlan::PlainScanEngine, "plain_scan_engine"),
            (AnnPlan::PlainScanPrecision, "plain_scan_precision"),
            (AnnPlan::HnswFullVisible, "hnsw_full_visible"),
            (AnnPlan::HnswSubset, "hnsw_subset"),
            (AnnPlan::UnknownCustomProvider, "unknown_custom_provider"),
        ] {
            let planned = PlannedQuery::new(
                QueryExpansion::default(),
                ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
            );
            let engine = ExplainEngine {
                kind: Some(SearchEngineKind::ParallelBruteForce),
                ann_plan: plan,
                scalar_plan: ScalarPlan::PlainScan,
            };
            let result = build_explain_result(&planned, &engine);
            // `ann_plan` は末尾から 2 番目（末尾行は Issue #474 の
            // `scalar_plan`）。
            let ann_plan_row = result.rows.len() - 2;
            assert_eq!(
                cell_text(&result, ann_plan_row),
                format!("ann_plan: {expected}")
            );
        }
    }

    /// Issue #411 の要件 3（テナント存在情報に繋がる数値の非露出）を
    /// 機械的に固定する: 新規行（`engine`／`hnsw_params`／`ann_plan`／
    /// `scalar_plan`〔Issue #474〕）の値がいずれも閉じた語彙集合の要素であり、
    /// 可視カーディナリティ・行数・索引ノード数等のデータ由来の数値を
    /// 含まないことを検証する。
    #[test]
    fn build_explain_result_new_rows_use_closed_vocabulary_only() {
        const ENGINE_TOKENS: &[&str] = &[
            "cpu_scalar_brute_force",
            "parallel_brute_force",
            "hnsw",
            "(custom_provider)",
        ];
        const ANN_PLAN_TOKENS: &[&str] = &[
            "plain_scan_engine",
            "plain_scan_precision",
            "hnsw_full_visible",
            "hnsw_subset",
            "unknown_custom_provider",
        ];

        let hnsw_params = ValidatedHnswParams::new(HnswParams::default())
            .expect("既定 HnswParams は常に検証を通過する");
        for (kind, ann_plan) in [
            (
                Some(SearchEngineKind::CpuScalarBruteForce),
                AnnPlan::PlainScanEngine,
            ),
            (
                Some(SearchEngineKind::ParallelBruteForce),
                AnnPlan::PlainScanEngine,
            ),
            (
                Some(SearchEngineKind::Hnsw(hnsw_params)),
                AnnPlan::HnswFullVisible,
            ),
            (
                Some(SearchEngineKind::Hnsw(hnsw_params)),
                AnnPlan::HnswSubset,
            ),
            (
                Some(SearchEngineKind::Hnsw(hnsw_params)),
                AnnPlan::PlainScanPrecision,
            ),
            // codex-review P1 指摘対応（PR #437）: `kind == None`（`with_provider`／
            // `from_storage` 経由）の実運用ペアリングは `AnnPlan::
            // UnknownCustomProvider`（`core.rs` の `EXPLAIN` アームが
            // `engine_kind_unknown: self.search_engine_kind().is_none()` を渡す
            // ことで到達する）。
            (None, AnnPlan::UnknownCustomProvider),
        ] {
            let planned = PlannedQuery::new(
                QueryExpansion::default(),
                ResolvedMode::new(SearchMode::Recall, ModeSource::Default),
            );
            let engine = ExplainEngine {
                kind,
                ann_plan,
                scalar_plan: ScalarPlan::PlainScan,
            };
            let result = build_explain_result(&planned, &engine);

            let engine_line = format!("engine: {}", engine_token(kind));
            assert!(
                ENGINE_TOKENS
                    .iter()
                    .any(|t| engine_line == format!("engine: {t}")),
                "unexpected engine token: {engine_line}"
            );
            let ann_plan_line = format!("ann_plan: {}", ann_plan_token(ann_plan));
            assert!(
                ANN_PLAN_TOKENS
                    .iter()
                    .any(|t| ann_plan_line == format!("ann_plan: {t}")),
                "unexpected ann_plan token: {ann_plan_line}"
            );
            if let Some(SearchEngineKind::Hnsw(params)) = kind {
                let p = params.get();
                let expected = format!(
                    "hnsw_params: m={},ef_construction={},ef_search={},resident={},sparse_visited_max={}",
                    p.m,
                    p.ef_construction,
                    p.ef_search,
                    params.resident_precision(),
                    params.sparse_visited_max()
                );
                assert!(
                    result
                        .rows
                        .iter()
                        .any(|row| matches!(&row.cells[0], Cell::Text(s) if *s == expected)),
                    "hnsw_params row missing or mismatched for {result:?}"
                );
            }
        }
    }
}
