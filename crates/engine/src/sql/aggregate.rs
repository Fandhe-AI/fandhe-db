//! 集計 SELECT（`GROUP BY` なし・単一行結果、TASK-166・SQL-13）の実行本体。
//! `GROUP BY` ありの複数行実行は [`crate::sql::group_by::execute_grouped_aggregate`]
//! （TASK-167・SQL-14）が担い、[`execute_aggregate_with_cache`] は
//! `BoundAggregate::group_by` の有無で振り分けるだけの薄い分岐を持つ。
//! [`execute_aggregate`] は engine クレート外向けの公開ラッパー（TASK-186・
//! NOSQL-4・NOSQL-5）で `execute_aggregate_with_cache` へキャッシュ `None` で
//! 委譲するのみ。
//!
//! 責務境界: [`crate::sql::parser::bind_aggregate`] が返す
//! [`crate::sql::parser::BoundAggregate`] を受け取り、対象テーブルの行テーブル
//! （`user_rows/{table}`）を**ストリーミングで**（可視行の embedding・metadata を
//! 一度に確保せず、行ごとに判定・集約・破棄する O(1) メモリで）走査して単一行の
//! [`crate::sql::exec::QueryResult`] を組み立てる。`core.rs::EngineCore::execute_sql_in_session`
//! の `Statement::Aggregate` アームから呼ばれる（[`sql`](crate::sql) モジュール
//! ドキュメント参照）。
//!
//! [`crate::arena::VectorArena`]（既存の検索 SELECT 実行経路）は使わない: アリーナは
//! スキーマに `VECTOR` 列が必須（`validated_vector_dim_in_txn`）で、かつ可視行の
//! embedding を全件バッファへ確保するため、`VECTOR` 列を持たないテーブルの集計や
//! 大規模テーブルの `COUNT(*)` には過剰（メモリ）かつ非対応（対象ビヘイビア:
//! SQL-13 は `VECTOR` 列なしテーブルでの集計を要求する）。
//!
//! RLS 適用順序は [`crate::arena`] モジュールの走査ループと同一の規約に揃える
//! （`.claude/rules/security.md`「テナント境界（P0）」）: 行ヘッダから
//! `tenant_id`・`visibility`・本体デコード再開オフセットを取り出し
//! （[`crate::storage::decode_row_header`]）、[`crate::policy::PolicyContext::is_visible`]
//! が `false` を返す行は embedding・metadata を一切デコードせずスキップする。
//! 可視行はまず物理キー側 `tenant_id`（`key_tenant`）とヘッダ側 `tenant_id` の
//! 整合検査（TABLE-12。従来 `storage::decode_row_for_key` が担っていた検査を
//! 呼び出し元の明示比較へ移設。`storage::verify_row_key_tenant`）を`tier` に
//! 関わらず必ず経てから、実際にクエリが参照するデータだけを段階的にデコードして
//! SCALAR 段（`WHERE`）・集計へ進む（[`ReferencedColumns`]・[`DecodeTier`] 参照。
//! Issue #350: embedding 非参照集計のデコードスキップと必要列限定デコード。
//! `DecodeTier::Embedding` はヘッダのオフセットを引き継いで本体のみをデコードし
//! （[`crate::storage::decode_row_body_into`]。スクラッチ再利用によりヘッダの
//! 二重デコード・行ごとの `Vec<f32>` 新規確保を避ける。Issue #349・Issue #314
//! の横展開）、`DecodeTier::Fast`・`DecodeTier::DimAndScalar` は
//! `storage::decode_row_dim_and_metadata_borrowed`（ヒープ確保を伴わない構造検証・
//! embedding 自体は `Vec<f32>` へ確保しない）を必ず通す（codex-review P1 指摘・
//! PR #369: `DecodeTier::Fast` が構造検証を省略すると破損した可視行を集計結果へ
//! 含めてしまう fail-open な契約変更になるため）。
//! `COUNT` 等の集計値から他テナント行の存在・件数を推測できないという契約
//! （RLS-7・RLS-8）は、この「不可視行は集約対象に一切現れない」という走査自体の
//! 構造で担保し、デコード段階の縮小はこの構造に一切手を加えない（可視性判定・
//! TABLE-12 の整合検査・metadata の構造検証は全 tier で常に行う）。

use crate::catalog::{self, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::allowlist::{AggregateFunc, SqlSurfaceError};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::expr_program::StackValue;
use crate::sql::parser::{AggregateInput, BoundAggregate};
use crate::sql::udf_call::{self, BoundExpr, ExprValue};
use crate::storage::{self, StorageError};
use redb::ReadableTable;

/// 型不整合・NULL 契約違反等の意味論的問題ではなく、`bind_aggregate` の型検査を
/// 通過したはずの `(AggregateFunc, AggregateInput)` の組み合わせが
/// [`Accumulator::new`] の網羅から漏れていた場合に返す（実装バグの検出用。
/// untrusted 入力起因ではないため `wire_code` は `XX000`）。
pub(crate) fn accumulator_bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("aggregate accumulator/input type mismatch: {detail}"),
    }
}

/// 単一行集計（`GROUP BY` なし）の既定の結果バイト予算（PR #1049 レビュー指摘
/// P0 対応）。`crate::sql::scan::MAX_SCAN_RESULT_BYTES`・
/// `crate::sql::cursor::MAX_CURSOR_BYTES_PER_SESSION` と同方針で、通常の
/// （カーソル非経由の）集計呼び出しでは既存の実挙動を変えない十分大きな値を既定
/// とし、[`crate::sql::cursor`] の `DECLARE` 経由の呼び出しだけがより小さい予算
/// （[`crate::sql::cursor::MAX_CURSOR_BYTES_PER_SESSION`]）を明示的に渡す。
pub(crate) const MAX_AGGREGATE_RESULT_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// [`accumulate_text_budget`] の予算超過 detail 文言（[`is_aggregate_text_budget_error`]
/// が照合する）。
const AGGREGATE_TEXT_BUDGET_EXCEEDED_DETAIL: &str = "aggregate result exceeds capacity";
/// 同上。`checked_add` のオーバーフロー側の detail 文言。
const AGGREGATE_TEXT_BUDGET_OVERFLOW_DETAIL: &str = "aggregate text budget accounting overflowed";

/// `err` が [`accumulate_text_budget`] の予算超過（処理順序に依存しうる一時的な
/// 超過）かを判定する（PR #1049 レビュー指摘 codex P1 対応）。
///
/// `MIN`/`MAX(<TEXT 列>)` の保持文字列は後続行でより短い極値へ更新されると縮小する
/// ため、途中の最大値は行の処理順序に依存する。索引経路の候補順と全走査の物理行順が
/// 異なると、同じ最終結果でも索引経路だけが途中で予算を超えて失敗しうる——索引選択
/// によってクエリの成否が変わってはならない（`sql::group_by::
/// is_text_accumulator_budget_error` と同じ理由・同じ設計）ため、索引経路は
/// この超過を検出したら結果を破棄して全走査（処理順序に依存しない基準実装）へ
/// 退避する。
fn is_aggregate_text_budget_error(err: &SqlSurfaceError) -> bool {
    matches!(
        err,
        SqlSurfaceError::PayloadTooLarge { detail }
            if detail == AGGREGATE_TEXT_BUDGET_EXCEEDED_DETAIL
                || detail == AGGREGATE_TEXT_BUDGET_OVERFLOW_DETAIL
    )
}

/// `MIN`/`MAX(<TEXT 列>)` 集計項目（[`Accumulator::TextMin`]/[`Accumulator::TextMax`]）
/// がクエリ全体で保持する文字列の累計バイト数を、呼び出し元が指定する
/// `max_result_bytes` 予算で頭打ちにする（PR #1049 レビュー指摘 P0 対応）。
/// `GROUP BY` なしの単一行集計は項目ごとに高々 1 本の `String` しか保持しないが
/// （[`Accumulator::text_len`] ドキュメント参照）、最大
/// [`crate::sql::allowlist::MAX_AGGREGATE_ITEMS`]（32）項目がそれぞれ独立した
/// `TEXT` 列（1 列あたり最大 [`crate::row_codec::MAX_TEXT_FIELD_LEN`]＝4 MiB）の
/// 極値を同時に保持しうるため、項目数だけでは有界にならない
/// （`sql::group_by::accumulate_row` の `MAX_TEXT_ACCUMULATOR_TOTAL_BYTES` と同じ
/// 「クエリ全体での累計」という考え方を、`GROUP BY` なしの単一行経路にも適用する）。
/// `before`／`after` の比較により、より短い極値への更新（保持量の縮小）を正しく
/// 減算し、正常なクエリを誤って予算超過にしない（`sql::group_by::accumulate_row`
/// と同方針）。
///
/// 超過時の detail は [`AGGREGATE_TEXT_BUDGET_EXCEEDED_DETAIL`]／
/// [`AGGREGATE_TEXT_BUDGET_OVERFLOW_DETAIL`]。より短い極値への更新で縮小しうる
/// ため途中の最大値は処理順序に依存し、索引経路（[`try_scalar_index_aggregate`]）は
/// この超過を検出したら全走査へ退避する（[`is_aggregate_text_budget_error`] 参照）。
fn accumulate_text_budget(
    before: usize,
    after: usize,
    total_text_accumulator_bytes: &mut usize,
    max_result_bytes: usize,
) -> Result<(), SqlSurfaceError> {
    if after > before {
        let delta = after - before;
        *total_text_accumulator_bytes = total_text_accumulator_bytes
            .checked_add(delta)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(AGGREGATE_TEXT_BUDGET_OVERFLOW_DETAIL)
            })?;
        if *total_text_accumulator_bytes > max_result_bytes {
            return Err(SqlSurfaceError::payload_too_large(
                AGGREGATE_TEXT_BUDGET_EXCEEDED_DETAIL,
            ));
        }
    } else if after < before {
        let delta = before - after;
        *total_text_accumulator_bytes = total_text_accumulator_bytes
            .checked_sub(delta)
            .ok_or_else(|| accumulator_bug("aggregate text budget accounting underflowed"))?;
    }
    Ok(())
}

/// `Accumulator::observe` 1 回分の呼び出しと、`MIN`/`MAX(<TEXT 列>)`・
/// `COUNT(DISTINCT)`（SQL-25 (c)・TASK-209）の両方の累計予算判定をまとめた
/// 薄いヘルパ。`sql::aggregate` 内の 3 つの observe 呼び出し箇所
/// （`execute_aggregate_with_cache` の Fast 経路・全走査、
/// `observe_candidate_slots`）が共有する（`sql::group_by::accumulate_row` は
/// 独自のグループキー累計・`ResultBudget` 判定を持つため別実装のまま）。
/// `COUNT(DISTINCT)` の増分は [`Accumulator::distinct_footprint`] の
/// before/after 差分から求め、新規キー 1 件の確定時のみ
/// [`crate::sql::distinct::DistinctBudget::charge`] を呼ぶ（既存キーへの
/// 再ヒットでは計上しない）。
#[allow(clippy::too_many_arguments)]
fn observe_with_budgets(
    accumulator: &mut Accumulator,
    input: &AggregateInput,
    id: u64,
    vector: &RowVector<'_>,
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    scratch: &mut Vec<StackValue>,
    total_text_accumulator_bytes: &mut usize,
    max_result_bytes: usize,
    distinct_budget: &mut crate::sql::distinct::DistinctBudget,
) -> Result<(), SqlSurfaceError> {
    let text_before = accumulator.text_len();
    let (entries_before, bytes_before) = accumulator.distinct_footprint();
    accumulator.observe(input, id, vector, scanned, scratch)?;
    let text_after = accumulator.text_len();
    accumulate_text_budget(
        text_before,
        text_after,
        total_text_accumulator_bytes,
        max_result_bytes,
    )?;
    let (entries_after, bytes_after) = accumulator.distinct_footprint();
    if entries_after > entries_before {
        let delta = bytes_after.checked_sub(bytes_before).ok_or_else(|| {
            accumulator_bug("COUNT(DISTINCT) footprint bytes decreased unexpectedly")
        })?;
        distinct_budget.charge(delta)?;
    }
    Ok(())
}

/// 可視行 1 件から取り出した `VECTOR` 列の値のビュー（Issue #350）。
/// [`ReferencedColumns::needs_embedding`] に応じて呼び出し元（`execute_aggregate`・
/// `sql::group_by::execute_grouped_aggregate`）が `values` を実際にデコード済みの
/// embedding（`Some`）か、デコードを省略した状態（`None`。`dim` のみ既知）かで
/// 埋め分ける。`dim > 0` は「`VECTOR` 列が NULL でない」を表す（
/// `storage::Row::embedding` が空 = 未設定という既存契約の dim 版）。ファストパス
/// （[`DecodeTier::Fast`]）でも `dim` は `decode_row_dim_and_metadata_borrowed`
/// による構造検証込みの実値を持つ（PR #369 codex-review P1 対応。破損行を fail-open
/// で見逃さないための変更で、値自体はファストパス到達条件
/// （`VectorColumnPresence`/`ScalarExpr` 系項目を含まない）により集計結果へは
/// 影響しない）。
pub(crate) struct RowVector<'a> {
    pub(crate) dim: u32,
    pub(crate) values: Option<&'a [f32]>,
}

/// [`BoundAggregate`] が実際に参照する列集合（Issue #350）。行走査ループが
/// 可視行 1 件ごとにどこまでデコードすべきか（[`DecodeTier`]）を決めるために
/// 束縛結果から一度だけ導出する。
///
/// `embedding` へのアクセスは束縛段で [`BoundExpr::VectorRef`] に一元化されている
/// （`sql::udf_call::eval` 参照）ため、`references_embedding` による式木の再帰走査は
/// 過小評価が起きない精密判定になる。判定に自信が持てない新 variant が将来
/// `BoundExpr` へ追加された場合は、`references_embedding` 側の非網羅 `match`（`_`
/// パターン禁止）がコンパイルエラーとして検出する設計にしてあり、ここで黙って
/// 「デコード不要」側に倒れることはない。
pub(crate) struct ReferencedColumns {
    /// `schema.columns` と同じ長さ。[`row_codec::scan_scalar_columns_masked`] へ
    /// そのまま渡すマスク。
    scalar_mask: Vec<bool>,
    /// `ScalarExpr` 項目または `expr_filters`（`WHERE`）の式木が embedding へ
    /// 到達する場合に `true`。
    needs_embedding: bool,
    /// `VectorColumnPresence`（`COUNT(<VECTOR 列>)`）項目がある場合に `true`
    /// （embedding 自体は不要・dim のみ必要）。
    needs_vector_presence: bool,
    /// `TextColumn` 項目・`metadata_filters`・`GROUP BY` キー列のいずれかが
    /// スカラー列を参照する場合に `true`。`scalar_mask` への反映（`get_mut`）が
    /// 範囲外インデックスで黙って無視された場合でも、この値は参照の有無を
    /// 直接記録するため `any_scalar_column_referenced()`（`scalar_mask` 経由の
    /// 間接判定）だけに依存せず `DecodeTier::Fast` の誤選択を防ぐ
    /// （codex-review 指摘対応）。
    has_scalar_reference: bool,
}

impl ReferencedColumns {
    /// `items`・`metadata_filters`・`expr_filters` に加え、`GROUP BY` キー列
    /// （`extra_scalar_indices`。SQL-25 (d) で複数列 `GROUP BY` に対応するため
    /// `Option<usize>` からスライスへ拡張した。`GROUP BY` なしの単一行集計では
    /// 空スライス）から参照列集合を導出する。
    pub(crate) fn derive(
        schema: &TableSchema,
        items: &[crate::sql::parser::BoundAggregateItem],
        metadata_filters: &[declarative_filter::MetadataFilter],
        expr_filters: &[BoundExpr],
        or_filters: &[crate::sql::where_tree::BoundOrGroup],
        extra_scalar_indices: &[usize],
    ) -> Self {
        let mut scalar_mask = vec![false; schema.columns.len()];
        let mut needs_embedding = false;
        let mut needs_vector_presence = false;
        let mut has_scalar_reference = false;

        for item in items {
            match &item.input {
                AggregateInput::TextColumn(index)
                | AggregateInput::BooleanColumn(index)
                | AggregateInput::DateColumn(index)
                | AggregateInput::TimestampColumn(index)
                | AggregateInput::ArrayColumn(index)
                | AggregateInput::ByteaColumn(index)
                | AggregateInput::JsonColumn(index)
                | AggregateInput::EnumColumn(index)
                | AggregateInput::UuidColumn(index)
                | AggregateInput::IntegerColumn(index)
                | AggregateInput::BigIntColumn(index)
                | AggregateInput::RealColumn(index)
                | AggregateInput::DoubleColumn(index) => {
                    has_scalar_reference = true;
                    if let Some(slot) = scalar_mask.get_mut(*index) {
                        *slot = true;
                    }
                }
                AggregateInput::NumericColumn { index, .. } => {
                    has_scalar_reference = true;
                    if let Some(slot) = scalar_mask.get_mut(*index) {
                        *slot = true;
                    }
                }
                AggregateInput::ScalarExpr { source, .. } => {
                    if udf_call::references_embedding(source) {
                        needs_embedding = true;
                    }
                }
                AggregateInput::VectorColumnPresence => needs_vector_presence = true,
                AggregateInput::AllVisible | AggregateInput::IdU64 => {}
            }
        }
        if !metadata_filters.is_empty() {
            has_scalar_reference = true;
        }
        for filter in metadata_filters {
            if let Some(slot) = scalar_mask.get_mut(filter.column_index()) {
                *slot = true;
            }
        }
        for expr in expr_filters {
            if udf_call::references_embedding(expr) {
                needs_embedding = true;
            }
        }
        if !extra_scalar_indices.is_empty() {
            has_scalar_reference = true;
        }
        for &index in extra_scalar_indices {
            if let Some(slot) = scalar_mask.get_mut(index) {
                *slot = true;
            }
        }
        // TASK-208・SQL-24（Issue #912）: `WHERE` の OR 群が参照する列・embedding
        // も同様に反映する（`metadata_filters`／`expr_filters` と同じ理由。
        // security.md「不安全な設計」対応）。
        if !or_filters.is_empty() {
            has_scalar_reference = true;
        }
        for group in or_filters {
            group.visit_column_indices(&mut |idx| {
                if let Some(slot) = scalar_mask.get_mut(idx) {
                    *slot = true;
                }
            });
            if group.references_embedding() {
                needs_embedding = true;
            }
        }

        ReferencedColumns {
            scalar_mask,
            needs_embedding,
            needs_vector_presence,
            has_scalar_reference,
        }
    }

    /// `DecodeTier::Fast` 選択可否の判定に使う。`scalar_mask`（`get_mut` が
    /// 範囲外インデックスで無視された場合に値が反映されない可能性がある）
    /// ではなく `has_scalar_reference`（参照の有無を直接記録した値）を主とし、
    /// 両者いずれかが立っていれば `true` を返す fail-closed 判定にする。
    fn any_scalar_column_referenced(&self) -> bool {
        self.has_scalar_reference || self.scalar_mask.iter().any(|&wanted| wanted)
    }

    pub(crate) fn scalar_mask(&self) -> &[bool] {
        &self.scalar_mask
    }

    pub(crate) fn needs_embedding(&self) -> bool {
        self.needs_embedding
    }
}

/// 可視行 1 件のデコード段階（Issue #350: embedding 非参照集計のデコードスキップと
/// 必要列限定デコード）。段階が進むほどデコードコストが増える（`Fast` <
/// `DimAndScalar` < `Embedding`）。可視性判定・TABLE-12 のキー/ヘッダ tenant 整合
/// 検査はどの段階でも必ず行う（この列挙自体はデコード「範囲」だけを制御し、RLS 段の
/// 順序・全行実行には関与しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeTier {
    /// `COUNT(*)`・`COUNT(id)`・`SUM`/`AVG`/`MIN`/`MAX(id)` のみ・`WHERE` 述語なし・
    /// `GROUP BY` なしの場合に限り選択する（受入条件 3。単一行集計
    /// 〔`execute_aggregate`〕専用で、`GROUP BY` はグループキー列を必ず読む必要が
    /// あるため到達しない）。dim・metadata の構造検証は `DimAndScalar` と同じ
    /// `decode_row_dim_and_metadata_borrowed` を通す（PR #369 codex-review P1
    /// 対応。破損した可視行を fail-open で集計へ含めない）。`DimAndScalar` との
    /// 違いは metadata の `&str` 化・保持（`scan_scalar_columns_masked`）を
    /// 省略する点のみ。
    Fast,
    /// dim・metadata（マスク済み `scan_scalar_columns_masked`）まで。embedding は
    /// 構造検証のみで `Vec<f32>` へ確保しない
    /// （[`storage::decode_row_dim_and_metadata_borrowed`]）。
    DimAndScalar,
    /// embedding を含む完全デコード（[`storage::decode_row_embedding_and_metadata_into`]）。
    Embedding,
}

/// 単一行集計（`GROUP BY` なし）の [`DecodeTier`] 選択本体（Issue #894 で
/// `execute_aggregate_with_cache` のインライン if 連鎖から抽出。判定式・分岐順序は
/// 抽出前と完全に同一で挙動はビット同一のまま変わらない）。`ReferencedColumns` が
/// 新スカラー型（INTEGER〜ENUM。TABLE-13・TASK-199）の列参照も `scalar_mask`／
/// `has_scalar_reference` へ一律に反映するため、本関数は型ごとの分岐を持たず
/// `DimAndScalar` に収まる（embedding をデコードしない）。
pub(crate) fn select_decode_tier(
    referenced: &ReferencedColumns,
    has_expr_filters: bool,
) -> DecodeTier {
    if referenced.needs_embedding {
        DecodeTier::Embedding
    } else if referenced.needs_vector_presence
        || referenced.any_scalar_column_referenced()
        || has_expr_filters
    {
        DecodeTier::DimAndScalar
    } else {
        // `WHERE`（`metadata_filters`・`expr_filters` いずれも空）・`VECTOR` 列
        // 参照・スカラー列参照のいずれも無い場合のみ（`COUNT(*)`・`COUNT(id)`・
        // `SUM`/`AVG`/`MIN`/`MAX(id)` の組み合わせ）。RLS 可視判定＋TABLE-12 の
        // キー/ヘッダ tenant 整合検査だけで完結する（受入条件 3）。
        DecodeTier::Fast
    }
}

/// `EXPLAIN`（Issue #922・SQL-27）の集計文向け `scalar_plan`／`access_path` を
/// 決定する。executor（本ファイルの [`execute_aggregate_with_cache`]・
/// [`crate::sql::group_by::execute_grouped_aggregate`]）の実行時ゲートと
/// **同じ判定式**（[`select_decode_tier`]・
/// [`crate::sql::scalar_plan::classify_scalar_plan`]・[`crate::sql::
/// group_by::has_text_min_max_aggregate`]）から静的に導出する単一情報源
/// （D5「矛盾出力の防止」。呼び出し元 `core.rs::EngineCore::
/// run_relational_explain_aggregate` がこの分類だけを使い、行走査・キャッシュ
/// 消費は一切行わない）。`arena_cache`／`scalar_cache`（キャッシュの構築可否・
/// ヒット/ミスという実行時の縮退結果）はテナント存在情報に繋がるため入力に
/// 含めない（`schema`・`bound` の構文的形状のみで決まる）。
pub(crate) fn classify_aggregate_access(
    schema: &TableSchema,
    bound: &BoundAggregate,
) -> (
    crate::sql::scalar_plan::ScalarPlan,
    crate::sql::explain::AccessPath,
) {
    use crate::sql::explain::AccessPath;
    use crate::sql::scalar_plan::{classify_scalar_plan, ScalarPlan, ScalarShapeInput};

    let has_vector = schema.vector_dim().is_some();
    let where_less = !bound.has_where_filters();
    let scalar_shape = ScalarShapeInput {
        scalar_prefilter: true,
        metadata_filters: bound.metadata_filters(),
        expr_filters: bound.expr_filters(),
        or_filters: bound.or_filters(),
    };
    // Issue #475 の「索引対応述語のみで構成される」判定と同一
    // （`execute_aggregate_with_cache`・`execute_grouped_aggregate` の
    // `candidate_walk`／索引経路ゲートと同じ呼び出し）。`VECTOR` 列を持たない
    // テーブルはこの索引の構築材料を持てないため `has_vector` で先にゲートする
    // （D5: 矛盾出力〔`scalar_plan: index_equality` と `access_path: full_scan`
    // の同時出力〕を防ぐ）。
    let index_candidate_eligible =
        has_vector && classify_scalar_plan(&scalar_shape) != ScalarPlan::PlainScan;
    let scalar_plan = if has_vector {
        classify_scalar_plan(&scalar_shape)
    } else {
        ScalarPlan::PlainScan
    };

    if bound.has_group_by() {
        // SQL-25 (d)・Issue #1099: `execute_grouped_aggregate` は複数列
        // `GROUP BY`（`key_count != 1`）を索引経路（列挙形・候補走査形）の
        // 対象外とし常に全走査へ一本化する（`ScalarIndex::column_groups` が
        // 単一キー専用のため）。ここも同じ `key_count == 1` ゲートを掛けないと
        // 複数列 `GROUP BY` で `access_path: full_scan`（実行時の実態）と
        // 異なる索引経路を EXPLAIN が返す D5 矛盾出力になる。
        let single_key_group_by = bound
            .group_by
            .as_ref()
            .is_some_and(|g| g.column_indices.len() == 1);
        let text_min_max_blocks = crate::sql::group_by::has_text_min_max_aggregate(bound.items());
        let access_path = if !single_key_group_by {
            AccessPath::FullScan
        } else if has_vector && where_less && !text_min_max_blocks {
            AccessPath::ScalarIndexGroupEnumeration
        } else if has_vector && !where_less && index_candidate_eligible {
            AccessPath::ScalarIndexCandidates
        } else {
            AccessPath::FullScan
        };
        // codex-review P1（PR #1102）: `execute_grouped_aggregate` は複数列
        // `GROUP BY` を常に全走査へ倒す（`ScalarIndex::column_groups` が単一
        // キー専用のため、上の `single_key_group_by` ゲートで `access_path` は
        // 既に `FullScan` に一本化されている）。`scalar_plan` はこのゲートより
        // 前に `has_vector` のみで計算されるため、複数列 `GROUP BY` かつ
        // 索引対応述語（`WHERE`）を伴う場合に `scalar_plan: index_equality`
        // 等と `access_path: full_scan` が同時に出て D5「矛盾出力の防止」に
        // 反していた。実行経路（全走査で述語索引を一切使わない）に合わせ、
        // 複数列 `GROUP BY` では `scalar_plan` も `PlainScan` へ揃える。
        let scalar_plan = if single_key_group_by {
            scalar_plan
        } else {
            ScalarPlan::PlainScan
        };
        (scalar_plan, access_path)
    } else {
        // `GROUP BY` なし単一行集計の `DecodeTier::Fast` 判定
        // （[`select_decode_tier`]）は `VECTOR` 列の有無を問わない
        // （`COUNT(*)`・`id` 系のみなら可視ビットマップキャッシュを使える）。
        let referenced = ReferencedColumns::derive(
            schema,
            bound.items(),
            bound.metadata_filters(),
            bound.expr_filters(),
            bound.or_filters(),
            &[],
        );
        let fast_tier = select_decode_tier(
            &referenced,
            !bound.expr_filters().is_empty() || !bound.or_filters().is_empty(),
        ) == DecodeTier::Fast;
        let access_path = if fast_tier {
            AccessPath::VisibleBitmapCache
        } else if index_candidate_eligible {
            AccessPath::ScalarIndexCandidates
        } else {
            AccessPath::FullScan
        };
        (scalar_plan, access_path)
    }
}

/// 集計項目 1 つの実行時アキュムレータ（TASK-166・SQL-13）。すべて O(1) 状態
/// （`TextMin`/`TextMax` のみ、新しい極値を更新するたびに高々 1 本の `String` を
/// 保持し直す。`.claude/rules/security.md`「不安全な設計｜無制限リソース確保
/// （DoS）」対応）。
///
/// `Clone`（SQL-30・TASK-214、Issue #930）: `sql::window::execute_window_scan` が
/// ORDER BY ありのウィンドウ集計で、peer グループ境界ごとに `clone().finish()`
/// でスナップショットを取りつつ元のアキュムレータへ観測を積み増し続けるために
/// 使う（`finish` は `self` を消費するため、走査を止めずに中間結果だけ確定する
/// 手段が必要）。既存の集計経路（`aggregate.rs`・`group_by.rs`）は複製せず単一の
/// 所有権のまま走査末尾で `finish` するため、この派生追加で挙動は変わらない。
#[derive(Clone)]
pub(crate) enum Accumulator {
    /// `COUNT(*)`・`COUNT(id)`・`COUNT(<VECTOR 列>)`・`COUNT(<Scalar 式>)`。
    Count(u64),
    /// `SUM(id)`。`checked_add` で正確に演算し、超過は [`SqlSurfaceError::numeric_out_of_range`]。
    IdSum(Option<u64>),
    /// `AVG(id)`。合計は `u64` で正確に保持し、`finish` で `f64` 化して除算する。
    IdAvg {
        sum: Option<u64>,
        count: u64,
    },
    IdMin(Option<u64>),
    IdMax(Option<u64>),
    /// `SUM(<Scalar 式>)`。加算のたびに `is_finite()` を検査する。
    FloatSum(Option<f64>),
    FloatAvg {
        sum: Option<f64>,
        count: u64,
    },
    /// `MIN(<Scalar 式>)`。`f64` は全順序を持たないため `total_cmp` で比較する。
    FloatMin(Option<f64>),
    FloatMax(Option<f64>),
    /// `MIN(<TEXT 列>)`。バイト順比較（`str::lt`/`str::gt`）。
    TextMin(Option<String>),
    TextMax(Option<String>),
    /// `SUM(<INTEGER>/<BIGINT>)`（Issue #892・D2）。`i128` の `checked_add` で
    /// 正確に累積し、`finish` で `i64` 範囲を検査する（部分和が一時的に
    /// `i64` を超えても最終値が収まれば成功する契約のため、確定時に検査する）。
    IntSum(Option<i128>),
    /// `AVG(<INTEGER>/<BIGINT>)`（D3）。合計は `i128` で正確に保持し、`finish`
    /// で `f64` 化して除算する（結果は `Cell::Float`。DOUBLE PRECISION 相当）。
    IntAvg {
        sum: Option<i128>,
        count: u64,
    },
    /// `MIN`/`MAX(<INTEGER>/<BIGINT>)`。`i64` の全順序で比較する。
    IntMin(Option<i64>),
    IntMax(Option<i64>),
    /// `SUM(<NUMERIC(p,s)>)`（D5）。`unscaled` を `i128`（`checked_add`）で
    /// 累積し、列の `scale` をそのまま結果の scale とする。`finish` で
    /// `Decimal::fits_precision(MAX_PRECISION)` を検査する。
    NumericSum {
        sum: Option<i128>,
        scale: u8,
    },
    /// `AVG(<NUMERIC(p,s)>)`（D6）。結果 scale は
    /// `max(s, min(16, 38 - (p - s)))`（`finish` で算出）。除算は
    /// [`crate::numeric::avg_unscaled`] による長除算（half away from zero）。
    NumericAvg {
        sum: Option<i128>,
        count: u64,
        precision: u8,
        scale: u8,
    },
    /// `MIN`/`MAX(<NUMERIC(p,s)>)`。同一 scale の unscaled 値を比較する
    /// （列の scale と一致しない値は `Accumulator::observe` 側で実装バグとして
    /// 拒否済みのため、ここでは常に一致している前提）。
    NumericMin {
        value: Option<i128>,
        scale: u8,
    },
    NumericMax {
        value: Option<i128>,
        scale: u8,
    },
    /// `MIN`/`MAX(<DATE>)`（D7）。1970-01-01 起点の日数（`i32`）の全順序比較。
    DateMin(Option<i32>),
    DateMax(Option<i32>),
    /// `MIN`/`MAX(<TIMESTAMP>)`。1970-01-01 起点のマイクロ秒（`i64`）の
    /// 全順序比較。
    TimestampMin(Option<i64>),
    TimestampMax(Option<i64>),
    /// `COUNT(DISTINCT <expr>)`（SQL-25 (c)・TASK-209）。`seen` は正準化済み
    /// キー（[`Accumulator::observe_distinct`]）の集合、`bytes` はそのキーの
    /// 累計バイト数（[`Accumulator::distinct_footprint`] が呼び出し元へ公開する
    /// before/after 差分の元。`sql::group_by::accumulate_row`／
    /// `sql::aggregate` の各 observe 呼び出し箇所が、この差分を
    /// [`crate::sql::distinct::DistinctBudget`] へ計上する）。`HashSet` は
    /// 標準の SipHash（ランダム鍵）で構築されるため hash flooding に強い
    /// （`.claude/rules/security.md`「不安全な設計」対応。追加の依存を要しない）。
    CountDistinct {
        seen: std::collections::HashSet<Vec<u8>>,
        bytes: usize,
    },
}

impl Accumulator {
    /// [`crate::sql::parser::BoundAggregateItem`] からアキュムレータを作る
    /// （SQL-25 (c)・TASK-209）。`item.distinct` が `true` なら
    /// [`Accumulator::CountDistinct`] を、それ以外は既存の [`Self::new`] へ
    /// 委譲する。`sql::aggregate::execute_aggregate_with_cache`・
    /// `sql::aggregate::try_scalar_index_aggregate`・
    /// `sql::group_by::new_accumulators` の 3 箇所（`Accumulator::new` の全直接
    /// 呼び出し箇所）がこの入口へ切り替わる。
    pub(crate) fn for_item(
        item: &crate::sql::parser::BoundAggregateItem,
    ) -> Result<Self, SqlSurfaceError> {
        if item.distinct {
            // 構文層（`Parser::parse_aggregate_item`）・束縛層
            // （`resolve_count_distinct_input`）が `COUNT` 以外での `distinct`
            // を構造的に拒否済みのため、ここへ到達する `item.func` は常に
            // `Count`。念のため多層防御として確認する（`accumulator_bug` は
            // untrusted 入力起因ではなく実装バグ検出用）。
            if item.func != AggregateFunc::Count {
                return Err(accumulator_bug(
                    "distinct flag set on a non-COUNT aggregate item",
                ));
            }
            return Ok(Accumulator::CountDistinct {
                seen: std::collections::HashSet::new(),
                bytes: 0,
            });
        }
        Self::new(item.func, &item.input)
    }

    /// `CountDistinct` が現在保持しているエントリ数・累計バイト数（それ以外の
    /// 集計種別は常に `(0, 0)`）。呼び出し元が `observe` 前後でこの値を比較し、
    /// 新規に確定した増分だけを [`crate::sql::distinct::DistinctBudget::charge`]
    /// へ計上する（`Self::text_len` の before/after パターンと同じ設計）。
    pub(crate) fn distinct_footprint(&self) -> (usize, usize) {
        match self {
            Accumulator::CountDistinct { seen, bytes } => (seen.len(), *bytes),
            _ => (0, 0),
        }
    }

    /// `bind_aggregate`（[`crate::sql::parser::resolve_aggregate_input`]）が型検査
    /// 済みの `(func, input)` から初期状態を作る。ここで到達しない組み合わせが
    /// あれば `bind_aggregate` 側のバグであり、[`accumulator_bug`] で fail-closed に
    /// 拒否する（黙って `Count(0)` 等へ縮退しない）。
    pub(crate) fn new(
        func: AggregateFunc,
        input: &AggregateInput,
    ) -> Result<Self, SqlSurfaceError> {
        use AggregateFunc::{Avg, Count, Max, Min, Sum};
        Ok(match (func, input) {
            (Count, _) => Accumulator::Count(0),
            (Sum, AggregateInput::IdU64) => Accumulator::IdSum(None),
            (Avg, AggregateInput::IdU64) => Accumulator::IdAvg {
                sum: None,
                count: 0,
            },
            (Min, AggregateInput::IdU64) => Accumulator::IdMin(None),
            (Max, AggregateInput::IdU64) => Accumulator::IdMax(None),
            (Sum, AggregateInput::ScalarExpr { .. }) => Accumulator::FloatSum(None),
            (Avg, AggregateInput::ScalarExpr { .. }) => Accumulator::FloatAvg {
                sum: None,
                count: 0,
            },
            (Min, AggregateInput::ScalarExpr { .. }) => Accumulator::FloatMin(None),
            (Max, AggregateInput::ScalarExpr { .. }) => Accumulator::FloatMax(None),
            (Min, AggregateInput::TextColumn(_)) => Accumulator::TextMin(None),
            (Max, AggregateInput::TextColumn(_)) => Accumulator::TextMax(None),
            // Issue #892（D2）: `INTEGER`/`BIGINT` 列。`SUM`/`AVG` は `i128` で
            // 正確に累積し、確定時（`finish`）に `i64` 範囲を検査する。
            (Sum, AggregateInput::IntegerColumn(_) | AggregateInput::BigIntColumn(_)) => {
                Accumulator::IntSum(None)
            }
            (Avg, AggregateInput::IntegerColumn(_) | AggregateInput::BigIntColumn(_)) => {
                Accumulator::IntAvg {
                    sum: None,
                    count: 0,
                }
            }
            (Min, AggregateInput::IntegerColumn(_) | AggregateInput::BigIntColumn(_)) => {
                Accumulator::IntMin(None)
            }
            (Max, AggregateInput::IntegerColumn(_) | AggregateInput::BigIntColumn(_)) => {
                Accumulator::IntMax(None)
            }
            // Issue #892（D4）: `REAL`/`DOUBLE PRECISION` 列。`ScalarExpr` と
            // 同じ `f64` 累積アキュムレータを共有する（結果はどちらも
            // `Cell::Float`）。
            (Sum, AggregateInput::RealColumn(_) | AggregateInput::DoubleColumn(_)) => {
                Accumulator::FloatSum(None)
            }
            (Avg, AggregateInput::RealColumn(_) | AggregateInput::DoubleColumn(_)) => {
                Accumulator::FloatAvg {
                    sum: None,
                    count: 0,
                }
            }
            (Min, AggregateInput::RealColumn(_) | AggregateInput::DoubleColumn(_)) => {
                Accumulator::FloatMin(None)
            }
            (Max, AggregateInput::RealColumn(_) | AggregateInput::DoubleColumn(_)) => {
                Accumulator::FloatMax(None)
            }
            // Issue #892（D5・D6）: `NUMERIC(p, s)` 列。
            (Sum, AggregateInput::NumericColumn { scale, .. }) => Accumulator::NumericSum {
                sum: None,
                scale: *scale,
            },
            (
                Avg,
                AggregateInput::NumericColumn {
                    precision, scale, ..
                },
            ) => Accumulator::NumericAvg {
                sum: None,
                count: 0,
                precision: *precision,
                scale: *scale,
            },
            (Min, AggregateInput::NumericColumn { scale, .. }) => Accumulator::NumericMin {
                value: None,
                scale: *scale,
            },
            (Max, AggregateInput::NumericColumn { scale, .. }) => Accumulator::NumericMax {
                value: None,
                scale: *scale,
            },
            // Issue #892（D7）: `DATE`/`TIMESTAMP` 列は `MIN`/`MAX` のみ
            // （`SUM`/`AVG` は `resolve_aggregate_input` が既に拒否済み）。
            (Min, AggregateInput::DateColumn(_)) => Accumulator::DateMin(None),
            (Max, AggregateInput::DateColumn(_)) => Accumulator::DateMax(None),
            (Min, AggregateInput::TimestampColumn(_)) => Accumulator::TimestampMin(None),
            (Max, AggregateInput::TimestampColumn(_)) => Accumulator::TimestampMax(None),
            _ => return Err(accumulator_bug("unsupported (func, input) combination")),
        })
    }

    /// 可視行 1 件を観測して状態を更新する。`scanned` は同じ行の
    /// `row_codec::scan_scalar_columns_masked` 結果（借用のまま。`vector` が
    /// `values: None` を持つ場合は embedding 未デコードのため、そこへ到達する
    /// `ScalarExpr` は [`accumulator_bug`] で fail-closed に拒否する——
    /// `ReferencedColumns` の導出漏れという実装バグの検出用であり、黙って空
    /// embedding として評価しない）。
    /// `scratch` は [`crate::sql::expr_program::ExprProgram::eval`] が使う
    /// スクラッチスタック（呼び出し元の行ループの外で確保し使い回す。Issue #353
    /// で行ごとの再帰評価をなくすために導入）。[`StackValue`] は行 `embedding` へ
    /// の借用を保持しないため（PR #373 codex-review 指摘対応）、`embedding` が
    /// 行ごとに異なるライフタイムを持つ呼び出し元（`embedding_scratch` を毎行
    /// `&mut` 上書きデコードする `execute_aggregate`）でも `scratch` を行ループの
    /// 外から使い回せる。`ScalarExpr` 以外の入力では参照されない。
    pub(crate) fn observe(
        &mut self,
        input: &AggregateInput,
        id: u64,
        vector: &RowVector<'_>,
        scanned: &[Option<row_codec::ScalarRef<'_>>],
        scratch: &mut Vec<StackValue>,
    ) -> Result<(), SqlSurfaceError> {
        // SQL-25 (c)・TASK-209: `CountDistinct` は「存在のみを数える」既存の
        // `observe_present` 系ではなく、実値を正準化したキーの集合で異なり数を
        // 判定する独立経路（[`Self::observe_distinct`]）を使う。予算計上
        // （[`crate::sql::distinct::DistinctBudget`]）は呼び出し元が
        // [`Self::distinct_footprint`] の before/after 差分で行う（本メソッドは
        // 予算超過を判定しない）。
        if matches!(self, Accumulator::CountDistinct { .. }) {
            return self.observe_distinct(input, id, vector, scanned, scratch);
        }
        match input {
            AggregateInput::AllVisible => self.observe_present(),
            // nullable な `VECTOR` 列（TABLE-5 の `ALTER TABLE ADD COLUMN` で追加
            // された列を含む）の裸の列参照。`row.embedding` が空 = 未設定（NULL）
            // という `storage::Row` の既存契約に従い、NULL 行は数えない
            // （PR #229 codex-review 指摘対応）。embedding 自体は不要なため
            // `dim`（`storage::decode_row_dim_and_metadata_borrowed` 由来）だけで
            // 判定する。
            AggregateInput::VectorColumnPresence => {
                if vector.dim == 0 {
                    Ok(())
                } else {
                    self.observe_present()
                }
            }
            AggregateInput::IdU64 => self.observe_id(id),
            AggregateInput::TextColumn(index) => {
                let value = scanned
                    .get(*index)
                    .copied()
                    .flatten()
                    .and_then(|v| v.as_text());
                self.observe_text(value)
            }
            // BOOLEAN 列の裸参照は COUNT（非 NULL 行数）専用（`resolve_aggregate_input`
            // が SUM/AVG/MIN/MAX を型不整合として拒否済み。Issue #883）。値の真偽は
            // 問わず「NULL でない」ことだけを数える。
            AggregateInput::BooleanColumn(index) => {
                if scanned.get(*index).copied().flatten().is_some() {
                    self.observe_present()
                } else {
                    Ok(())
                }
            }
            // `DATE`／`TIMESTAMP` 列の裸参照は `COUNT`・`MIN`/`MAX` を受理する
            // （TABLE-13・TASK-197、Issue #884・#892）。`resolve_aggregate_input`
            // が `SUM`/`AVG` を型不整合として既に拒否済みのため、`observe_date`/
            // `observe_timestamp` が到達する `Accumulator` は `Count`／
            // `DateMin`/`DateMax`（`TimestampMin`/`TimestampMax`）のいずれか。
            AggregateInput::DateColumn(index) => {
                let value = match scanned.get(*index).copied().flatten() {
                    Some(v) => Some(v.as_date().ok_or_else(|| {
                        accumulator_bug("DateColumn observed a ScalarRef that is not Date")
                    })?),
                    None => None,
                };
                self.observe_date(value)
            }
            AggregateInput::TimestampColumn(index) => {
                let value = match scanned.get(*index).copied().flatten() {
                    Some(v) => Some(v.as_timestamp().ok_or_else(|| {
                        accumulator_bug(
                            "TimestampColumn observed a ScalarRef that is not Timestamp",
                        )
                    })?),
                    None => None,
                };
                self.observe_timestamp(value)
            }
            // `INTEGER`／`BIGINT` 列は `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` の
            // すべてを受理する（TABLE-13・TASK-196、Issue #881・#892）。`SUM`/
            // `AVG` は `i128` で累積し、確定時（`finish`）に `i64` 範囲を
            // 検査する（D2）。
            AggregateInput::IntegerColumn(index) => {
                let value = match scanned.get(*index).copied().flatten() {
                    Some(row_codec::ScalarRef::Integer(v)) => Some(i64::from(v)),
                    Some(_) => {
                        return Err(accumulator_bug(
                            "IntegerColumn observed a ScalarRef that is not Integer",
                        ))
                    }
                    None => None,
                };
                self.observe_int(value)
            }
            AggregateInput::BigIntColumn(index) => {
                let value = match scanned.get(*index).copied().flatten() {
                    Some(row_codec::ScalarRef::BigInt(v)) => Some(v),
                    Some(_) => {
                        return Err(accumulator_bug(
                            "BigIntColumn observed a ScalarRef that is not BigInt",
                        ))
                    }
                    None => None,
                };
                self.observe_int(value)
            }
            // `REAL`／`DOUBLE PRECISION` 列も `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`
            // のすべてを受理する（Issue #892・D4）。既存の `ScalarExpr` と同じ
            // `f64` 累積アキュムレータへ観測する（`observe_float` は
            // `is_finite()` 検査込みで累積・非有限化は `22003`）。
            AggregateInput::RealColumn(index) => {
                let value = match scanned.get(*index).copied().flatten() {
                    Some(row_codec::ScalarRef::Real(v)) => Some(f64::from(v)),
                    Some(_) => {
                        return Err(accumulator_bug(
                            "RealColumn observed a ScalarRef that is not Real",
                        ))
                    }
                    None => None,
                };
                match value {
                    Some(v) => self.observe_float(v),
                    None => Ok(()),
                }
            }
            AggregateInput::DoubleColumn(index) => {
                let value = match scanned.get(*index).copied().flatten() {
                    Some(row_codec::ScalarRef::Double(v)) => Some(v),
                    Some(_) => {
                        return Err(accumulator_bug(
                            "DoubleColumn observed a ScalarRef that is not Double",
                        ))
                    }
                    None => None,
                };
                match value {
                    Some(v) => self.observe_float(v),
                    None => Ok(()),
                }
            }
            // ARRAY 列の裸参照も BOOLEAN と同じく COUNT（非 NULL 行数）専用
            // （TABLE-14・Issue #888・D-A8）。要素の中身は問わず、列自体が
            // NULL でないことだけを数える。
            AggregateInput::ArrayColumn(index) => {
                if scanned.get(*index).copied().flatten().is_some() {
                    self.observe_present()
                } else {
                    Ok(())
                }
            }
            // BYTEA 列の裸参照も COUNT（非 NULL 行数）専用（`resolve_aggregate_input`
            // が SUM/AVG/MIN/MAX を型不整合として拒否済み。Issue #886）。
            AggregateInput::ByteaColumn(index) => {
                if scanned.get(*index).copied().flatten().is_some() {
                    self.observe_present()
                } else {
                    Ok(())
                }
            }
            // JSON／JSONB 列の裸参照も COUNT（非 NULL 行数）専用
            // （`resolve_aggregate_input` が SUM/AVG/MIN/MAX を型不整合として
            // 拒否済み。TABLE-14・Issue #889）。
            AggregateInput::JsonColumn(index) => {
                if scanned.get(*index).copied().flatten().is_some() {
                    self.observe_present()
                } else {
                    Ok(())
                }
            }
            // ENUM 列の裸参照も COUNT（非 NULL 行数）専用（`resolve_aggregate_input`
            // が SUM/AVG/MIN/MAX を型不整合として拒否済み。TABLE-14・TASK-198、
            // Issue #890）。
            AggregateInput::EnumColumn(index) => {
                if scanned.get(*index).copied().flatten().is_some() {
                    self.observe_present()
                } else {
                    Ok(())
                }
            }
            // `NUMERIC(p, s)` 列は `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` のすべてを
            // 受理する（TABLE-13〔検討中〕・TASK-197、Issue #885・#892）。
            // 格納された `Decimal` の scale は常に列の scale と一致する契約
            // （`row_codec`/`catalog` の TABLE-13 検証）のため、不一致は
            // デコード側の実装バグとして fail-closed に拒否する。
            AggregateInput::NumericColumn { index, scale, .. } => {
                let value = match scanned.get(*index).copied().flatten() {
                    Some(row_codec::ScalarRef::Numeric(d)) => {
                        if d.scale() != *scale {
                            return Err(accumulator_bug(
                                "NumericColumn observed a Decimal whose scale does not match the column",
                            ));
                        }
                        Some(d.unscaled())
                    }
                    Some(_) => {
                        return Err(accumulator_bug(
                            "NumericColumn observed a ScalarRef that is not Numeric",
                        ))
                    }
                    None => None,
                };
                self.observe_numeric(value)
            }
            // UUID 列の裸参照も COUNT（非 NULL 行数）専用（`resolve_aggregate_input`
            // が SUM/AVG/MIN/MAX を型不整合として拒否済み。TABLE-13〔検討中〕・
            // TASK-197、Issue #887）。
            AggregateInput::UuidColumn(index) => {
                if scanned.get(*index).copied().flatten().is_some() {
                    self.observe_present()
                } else {
                    Ok(())
                }
            }
            AggregateInput::ScalarExpr { source, program } => {
                // 式木が実際に embedding へ到達する場合のみ `vector.values` を
                // 要求する（`references_embedding` は `ReferencedColumns::derive`
                // が `needs_embedding` を立てる際と同じ判定関数。式木に
                // `VectorRef` が無ければ `eval` は embedding を一切参照しないため
                // 空スライスで安全に評価できる）。`None` に到達したら
                // `ReferencedColumns` の導出漏れという実装バグとして fail-closed
                // に拒否する（黙って空 embedding へ縮退しない）。
                let embedding: &[f32] = if udf_call::references_embedding(source) {
                    match vector.values {
                        Some(v) => v,
                        None => {
                            return Err(accumulator_bug(
                                "ScalarExpr references the VECTOR column but the row's embedding was not decoded",
                            ))
                        }
                    }
                } else {
                    &[]
                };
                match program.eval(id, embedding, scratch)? {
                    ExprValue::Scalar(v) => self.observe_float(v),
                    // `resolve_aggregate_input` が `ExprType::Scalar` のみを
                    // `ScalarExpr` として束縛するため到達しない（束縛段の型検査と
                    // 評価結果の型が食い違う実装バグの検出用）。
                    _ => Err(accumulator_bug(
                        "scalar-typed BoundExpr evaluated to a non-scalar value",
                    )),
                }
            }
        }
    }

    /// [`Accumulator::CountDistinct`] 専用の観測経路（SQL-25 (c)・TASK-209）。
    /// `input` の型ごとに実値を正準化した `Vec<u8>` キー（NULL は `None`。
    /// `COUNT(DISTINCT)` は NULL を除外する PostgreSQL 互換契約）を作り、集合へ
    /// 挿入する。予算判定はここでは行わず、挿入後のサイズ・累計バイト数の増分
    /// （[`Self::distinct_footprint`]）を呼び出し元
    /// （`sql::aggregate`／`sql::group_by::accumulate_row`）が
    /// [`crate::sql::distinct::DistinctBudget::charge`] へ計上する。
    fn observe_distinct(
        &mut self,
        input: &AggregateInput,
        id: u64,
        vector: &RowVector<'_>,
        scanned: &[Option<row_codec::ScalarRef<'_>>],
        scratch: &mut Vec<StackValue>,
    ) -> Result<(), SqlSurfaceError> {
        let key: Option<Vec<u8>> = match input {
            AggregateInput::IdU64 => Some(id.to_be_bytes().to_vec()),
            AggregateInput::TextColumn(index) => scanned
                .get(*index)
                .copied()
                .flatten()
                .and_then(|v| v.as_text())
                .map(|s| s.as_bytes().to_vec()),
            AggregateInput::IntegerColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::Integer(v)) => Some(v.to_be_bytes().to_vec()),
                Some(_) => {
                    return Err(accumulator_bug(
                        "IntegerColumn observed a ScalarRef that is not Integer (distinct)",
                    ))
                }
                None => None,
            },
            AggregateInput::BigIntColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::BigInt(v)) => Some(v.to_be_bytes().to_vec()),
                Some(_) => {
                    return Err(accumulator_bug(
                        "BigIntColumn observed a ScalarRef that is not BigInt (distinct)",
                    ))
                }
                None => None,
            },
            AggregateInput::RealColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::Real(v)) => {
                    Some(crate::sql::distinct::canon_f64(f64::from(v)).to_vec())
                }
                Some(_) => {
                    return Err(accumulator_bug(
                        "RealColumn observed a ScalarRef that is not Real (distinct)",
                    ))
                }
                None => None,
            },
            AggregateInput::DoubleColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::Double(v)) => {
                    Some(crate::sql::distinct::canon_f64(v).to_vec())
                }
                Some(_) => {
                    return Err(accumulator_bug(
                        "DoubleColumn observed a ScalarRef that is not Double (distinct)",
                    ))
                }
                None => None,
            },
            AggregateInput::BooleanColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::Bool(b)) => Some(vec![u8::from(b)]),
                Some(_) => {
                    return Err(accumulator_bug(
                        "BooleanColumn observed a ScalarRef that is not Bool (distinct)",
                    ))
                }
                None => None,
            },
            AggregateInput::DateColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(v) => Some(
                    v.as_date()
                        .ok_or_else(|| {
                            accumulator_bug(
                                "DateColumn observed a ScalarRef that is not Date (distinct)",
                            )
                        })?
                        .to_be_bytes()
                        .to_vec(),
                ),
                None => None,
            },
            AggregateInput::TimestampColumn(index) => match scanned.get(*index).copied().flatten()
            {
                Some(v) => Some(
                    v.as_timestamp()
                        .ok_or_else(|| {
                            accumulator_bug(
                                "TimestampColumn observed a ScalarRef that is not Timestamp (distinct)",
                            )
                        })?
                        .to_be_bytes()
                        .to_vec(),
                ),
                None => None,
            },
            AggregateInput::ByteaColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::Bytes(b)) => Some(b.to_vec()),
                Some(_) => {
                    return Err(accumulator_bug(
                        "ByteaColumn observed a ScalarRef that is not Bytes (distinct)",
                    ))
                }
                None => None,
            },
            AggregateInput::EnumColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::Enum(s)) => Some(s.as_bytes().to_vec()),
                Some(_) => {
                    return Err(accumulator_bug(
                        "EnumColumn observed a ScalarRef that is not Enum (distinct)",
                    ))
                }
                None => None,
            },
            AggregateInput::UuidColumn(index) => match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::Uuid(u)) => Some(u.as_bytes().to_vec()),
                Some(_) => {
                    return Err(accumulator_bug(
                        "UuidColumn observed a ScalarRef that is not Uuid (distinct)",
                    ))
                }
                None => None,
            },
            AggregateInput::NumericColumn { index, scale, .. } => {
                match scanned.get(*index).copied().flatten() {
                    Some(row_codec::ScalarRef::Numeric(d)) => {
                        if d.scale() != *scale {
                            return Err(accumulator_bug(
                                "NumericColumn observed a Decimal whose scale does not match the column (distinct)",
                            ));
                        }
                        Some(d.unscaled().to_be_bytes().to_vec())
                    }
                    Some(_) => {
                        return Err(accumulator_bug(
                            "NumericColumn observed a ScalarRef that is not Numeric (distinct)",
                        ))
                    }
                    None => None,
                }
            }
            AggregateInput::ScalarExpr { source, program } => {
                let embedding: &[f32] = if udf_call::references_embedding(source) {
                    match vector.values {
                        Some(v) => v,
                        None => {
                            return Err(accumulator_bug(
                                "ScalarExpr references the VECTOR column but the row's embedding was not decoded (distinct)",
                            ))
                        }
                    }
                } else {
                    &[]
                };
                match program.eval(id, embedding, scratch)? {
                    ExprValue::Scalar(v) => Some(crate::sql::distinct::canon_f64(v).to_vec()),
                    _ => {
                        return Err(accumulator_bug(
                            "scalar-typed BoundExpr evaluated to a non-scalar value (distinct)",
                        ))
                    }
                }
            }
            // `resolve_count_distinct_input` は `AllVisible`／
            // `VectorColumnPresence`／`ArrayColumn`／`JsonColumn` をいずれも
            // 生成しない（`VECTOR`／`ARRAY`／`JSON` 列は `22000` で束縛段階で
            // 拒否し、`COUNT(DISTINCT *)` は構文段階で `42601` で拒否済み）。
            // 到達したら束縛層の実装バグ。
            AggregateInput::AllVisible
            | AggregateInput::VectorColumnPresence
            | AggregateInput::ArrayColumn(_)
            | AggregateInput::JsonColumn(_) => {
                return Err(accumulator_bug(
                    "COUNT(DISTINCT) accumulator observed an input type that bind-time resolution should have rejected",
                ))
            }
        };

        let (seen, bytes) = match self {
            Accumulator::CountDistinct { seen, bytes } => (seen, bytes),
            _ => {
                return Err(accumulator_bug(
                    "observe_distinct called on a non-CountDistinct accumulator",
                ))
            }
        };
        if let Some(key) = key {
            if seen.insert(key.clone()) {
                *bytes = bytes.checked_add(key.len()).ok_or_else(|| {
                    accumulator_bug("COUNT(DISTINCT) accumulated byte count overflowed")
                })?;
            }
        }
        Ok(())
    }

    /// NULL・非存在の概念を持たない入力（`*`・`id`・`Scalar` 式）を数えるだけの
    /// 経路。`VECTOR` 列は nullable のため別経路
    /// （[`AggregateInput::VectorColumnPresence`]）を使う。`COUNT` 以外がこの
    /// 経路に来ることはない（[`Accumulator::new`] の型検査）。
    fn observe_present(&mut self) -> Result<(), SqlSurfaceError> {
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_present on a non-Count accumulator",
            )),
        }
    }

    /// `n` 件の可視行を「存在のみ」でまとめて観測する（`COUNT(*)` 専用の
    /// 一括版）。`sql::scalar_index::ScalarIndex` の候補列・値グループが
    /// 「その述語／グループに属する可視行の**正確な**集合」であると静的に
    /// 判定できた場合に限り、行ごとのデコード・述語再適用を省いて件数だけを
    /// 加算する経路（[`count_star_only`] のドキュメント参照）から呼ぶ。
    /// `Count` 以外のアキュムレータへ到達したら実装バグとして fail-closed に
    /// 拒否する（[`Self::observe_present`] と同じ方針）。
    pub(crate) fn observe_present_n(&mut self, n: u64) -> Result<(), SqlSurfaceError> {
        match self {
            Accumulator::Count(current) => {
                *current = current.checked_add(n).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_present_n on a non-Count accumulator",
            )),
        }
    }

    fn observe_id(&mut self, id: u64) -> Result<(), SqlSurfaceError> {
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::IdSum(sum) => {
                let next = match sum {
                    None => id,
                    Some(cur) => cur.checked_add(id).ok_or_else(|| {
                        SqlSurfaceError::numeric_out_of_range("SUM(id) exceeds u64 range")
                    })?,
                };
                *sum = Some(next);
                Ok(())
            }
            Accumulator::IdAvg { sum, count } => {
                let next = match sum {
                    None => id,
                    Some(cur) => cur.checked_add(id).ok_or_else(|| {
                        SqlSurfaceError::numeric_out_of_range("AVG(id) exceeds u64 range")
                    })?,
                };
                *sum = Some(next);
                *count = count.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("AVG(id) row count exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::IdMin(m) => {
                *m = Some(match m {
                    None => id,
                    Some(cur) => id.min(*cur),
                });
                Ok(())
            }
            Accumulator::IdMax(m) => {
                *m = Some(match m {
                    None => id,
                    Some(cur) => id.max(*cur),
                });
                Ok(())
            }
            _ => Err(accumulator_bug("observe_id on an incompatible accumulator")),
        }
    }

    /// `value` が `None`（`TEXT` 列 NULL）の行は `COUNT`・`MIN`・`MAX` いずれも
    /// 無視する（PostgreSQL 互換の NULL 契約。ポインタ: TASK-166・SQL-13）。
    fn observe_text(&mut self, value: Option<&str>) -> Result<(), SqlSurfaceError> {
        let Some(value) = value else {
            return Ok(());
        };
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::TextMin(m) => {
                let should_update = match m {
                    None => true,
                    Some(cur) => value < cur.as_str(),
                };
                if should_update {
                    *m = Some(try_clone_str(value)?);
                }
                Ok(())
            }
            Accumulator::TextMax(m) => {
                let should_update = match m {
                    None => true,
                    Some(cur) => value > cur.as_str(),
                };
                if should_update {
                    *m = Some(try_clone_str(value)?);
                }
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_text on an incompatible accumulator",
            )),
        }
    }

    /// `sql::udf_call::eval` は各行単独の評価結果が有限であることを保証するが
    /// （`finite_scalar`／`apply_scalar_op` 参照）、複数行にわたる**累積**は
    /// 個々が有限でもオーバーフローで非有限化しうる。ここで加算・平均の分子ごとに
    /// `is_finite()` を検査し、超過は [`SqlSurfaceError::numeric_out_of_range`]
    /// （`22003`）で fail-closed に拒否する（黙って `inf`/`NaN` を返さない）。
    fn observe_float(&mut self, v: f64) -> Result<(), SqlSurfaceError> {
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::FloatSum(s) => {
                let next = s.unwrap_or(0.0) + v;
                if !next.is_finite() {
                    return Err(SqlSurfaceError::numeric_out_of_range(
                        "SUM overflowed to a non-finite value",
                    ));
                }
                *s = Some(next);
                Ok(())
            }
            Accumulator::FloatAvg { sum, count } => {
                let next = sum.unwrap_or(0.0) + v;
                if !next.is_finite() {
                    return Err(SqlSurfaceError::numeric_out_of_range(
                        "AVG overflowed to a non-finite value",
                    ));
                }
                *sum = Some(next);
                *count = count.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("AVG row count exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::FloatMin(m) => {
                *m = Some(match m {
                    None => v,
                    Some(cur) if v.total_cmp(cur) == std::cmp::Ordering::Less => v,
                    Some(cur) => *cur,
                });
                Ok(())
            }
            Accumulator::FloatMax(m) => {
                *m = Some(match m {
                    None => v,
                    Some(cur) if v.total_cmp(cur) == std::cmp::Ordering::Greater => v,
                    Some(cur) => *cur,
                });
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_float on an incompatible accumulator",
            )),
        }
    }

    /// `value` が `None`（`INTEGER`/`BIGINT` 列 NULL）の行は無視する
    /// （Issue #892・D2）。`SUM`/`AVG` は `i128` の `checked_add` で正確に
    /// 累積し、`i64` 範囲の検査は `finish` の確定時まで遅延させる（部分和が
    /// 一時的に `i64` を超えても最終値が収まれば成功する契約のため）。
    /// `MIN`/`MAX` は `i64` のまま全順序で比較する。
    fn observe_int(&mut self, value: Option<i64>) -> Result<(), SqlSurfaceError> {
        let Some(value) = value else {
            return Ok(());
        };
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::IntSum(s) => {
                let widened = i128::from(value);
                let next = match s {
                    None => widened,
                    Some(cur) => cur.checked_add(widened).ok_or_else(|| {
                        SqlSurfaceError::numeric_out_of_range(
                            "SUM(INTEGER/BIGINT) intermediate accumulation overflowed",
                        )
                    })?,
                };
                *s = Some(next);
                Ok(())
            }
            Accumulator::IntAvg { sum, count } => {
                let widened = i128::from(value);
                let next = match sum {
                    None => widened,
                    Some(cur) => cur.checked_add(widened).ok_or_else(|| {
                        SqlSurfaceError::numeric_out_of_range(
                            "AVG(INTEGER/BIGINT) intermediate accumulation overflowed",
                        )
                    })?,
                };
                *sum = Some(next);
                *count = count.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range(
                        "AVG(INTEGER/BIGINT) row count exceeds u64 range",
                    )
                })?;
                Ok(())
            }
            Accumulator::IntMin(m) => {
                *m = Some(match m {
                    None => value,
                    Some(cur) => value.min(*cur),
                });
                Ok(())
            }
            Accumulator::IntMax(m) => {
                *m = Some(match m {
                    None => value,
                    Some(cur) => value.max(*cur),
                });
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_int on an incompatible accumulator",
            )),
        }
    }

    /// `value` が `None`（`NUMERIC` 列 NULL）の行は無視する（Issue #892・
    /// D5・D6）。`value` は呼び出し元（[`Self::observe`]）が列の scale と
    /// 一致することを検証済みの unscaled 値。`SUM`/`AVG` は `i128` の
    /// `checked_add` で累積し、桁あふれ（38 桁超過）の検査は `finish` の
    /// 確定時まで遅延させる。`MIN`/`MAX` は同一 scale の unscaled 値を直接
    /// 比較する（scale が同一な限り数値としての大小関係と一致する）。
    fn observe_numeric(&mut self, value: Option<i128>) -> Result<(), SqlSurfaceError> {
        let Some(value) = value else {
            return Ok(());
        };
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::NumericSum { sum, .. } => {
                let next = match sum {
                    None => value,
                    Some(cur) => cur.checked_add(value).ok_or_else(|| {
                        SqlSurfaceError::numeric_out_of_range(
                            "SUM(NUMERIC) intermediate accumulation overflowed",
                        )
                    })?,
                };
                *sum = Some(next);
                Ok(())
            }
            Accumulator::NumericAvg { sum, count, .. } => {
                let next = match sum {
                    None => value,
                    Some(cur) => cur.checked_add(value).ok_or_else(|| {
                        SqlSurfaceError::numeric_out_of_range(
                            "AVG(NUMERIC) intermediate accumulation overflowed",
                        )
                    })?,
                };
                *sum = Some(next);
                *count = count.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range(
                        "AVG(NUMERIC) row count exceeds u64 range",
                    )
                })?;
                Ok(())
            }
            Accumulator::NumericMin { value: m, .. } => {
                *m = Some(match m {
                    None => value,
                    Some(cur) => value.min(*cur),
                });
                Ok(())
            }
            Accumulator::NumericMax { value: m, .. } => {
                *m = Some(match m {
                    None => value,
                    Some(cur) => value.max(*cur),
                });
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_numeric on an incompatible accumulator",
            )),
        }
    }

    /// `value` が `None`（`DATE` 列 NULL）の行は無視する（Issue #892・D7）。
    /// `resolve_aggregate_input` が `SUM`/`AVG` を既に拒否済みのため、ここで
    /// 到達するのは `Count`／`DateMin`/`DateMax` のいずれか。
    fn observe_date(&mut self, value: Option<i32>) -> Result<(), SqlSurfaceError> {
        let Some(value) = value else {
            return Ok(());
        };
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::DateMin(m) => {
                *m = Some(match m {
                    None => value,
                    Some(cur) => value.min(*cur),
                });
                Ok(())
            }
            Accumulator::DateMax(m) => {
                *m = Some(match m {
                    None => value,
                    Some(cur) => value.max(*cur),
                });
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_date on an incompatible accumulator",
            )),
        }
    }

    /// `TIMESTAMP` 版の [`Self::observe_date`]（同じ契約）。
    fn observe_timestamp(&mut self, value: Option<i64>) -> Result<(), SqlSurfaceError> {
        let Some(value) = value else {
            return Ok(());
        };
        match self {
            Accumulator::Count(n) => {
                *n = n.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::numeric_out_of_range("COUNT exceeds u64 range")
                })?;
                Ok(())
            }
            Accumulator::TimestampMin(m) => {
                *m = Some(match m {
                    None => value,
                    Some(cur) => value.min(*cur),
                });
                Ok(())
            }
            Accumulator::TimestampMax(m) => {
                *m = Some(match m {
                    None => value,
                    Some(cur) => value.max(*cur),
                });
                Ok(())
            }
            _ => Err(accumulator_bug(
                "observe_timestamp on an incompatible accumulator",
            )),
        }
    }

    /// `TextMin`/`TextMax` が現在保持している文字列のバイト数（未保持なら 0、
    /// それ以外の集計種別は常に 0）。呼び出し元（`sql::group_by`）が `observe`
    /// 前後でこの値を比較し、`GROUP BY` 全体での TEXT アキュムレータ累計バイト数を
    /// 予算管理するために公開する（PR #230 codex-review 指摘対応: `GROUP BY` は
    /// グループ数に比例して `TextMin`/`TextMax` インスタンスが増えるため、
    /// 単一行集計〔TASK-166・SQL-13〕の「項目ごとに高々 1 本」という有界性だけでは
    /// 不十分）。
    pub(crate) fn text_len(&self) -> usize {
        match self {
            Accumulator::TextMin(m) | Accumulator::TextMax(m) => {
                m.as_ref().map(String::len).unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// 空集合契約: `COUNT` は `0`、それ以外は `NULL`（PostgreSQL 互換。TASK-166・
    /// SQL-13）。`AVG` は合計を保持したまま `finish` の時点で除算する。
    ///
    /// Issue #892（D2・D5・D6）: `SUM(INTEGER/BIGINT)` の `i64` 範囲検査、
    /// `SUM`/`AVG(NUMERIC)` の 38 桁上限検査をここで確定的に行うため
    /// `Result` を返す（`22003`＝[`SqlSurfaceError::numeric_out_of_range`]）。
    pub(crate) fn finish(self) -> Result<Cell, SqlSurfaceError> {
        Ok(match self {
            Accumulator::Count(n) => Cell::Integer(n),
            Accumulator::IdSum(s) => s.map(Cell::Integer).unwrap_or(Cell::Null),
            Accumulator::IdAvg { sum, count } => match sum {
                None => Cell::Null,
                Some(s) => Cell::Float(s as f64 / count as f64),
            },
            Accumulator::IdMin(m) => m.map(Cell::Integer).unwrap_or(Cell::Null),
            Accumulator::IdMax(m) => m.map(Cell::Integer).unwrap_or(Cell::Null),
            Accumulator::FloatSum(s) => s.map(Cell::Float).unwrap_or(Cell::Null),
            Accumulator::FloatAvg { sum, count } => match sum {
                None => Cell::Null,
                Some(s) => Cell::Float(s / count as f64),
            },
            Accumulator::FloatMin(m) => m.map(Cell::Float).unwrap_or(Cell::Null),
            Accumulator::FloatMax(m) => m.map(Cell::Float).unwrap_or(Cell::Null),
            Accumulator::TextMin(m) => m.map(Cell::Text).unwrap_or(Cell::Null),
            Accumulator::TextMax(m) => m.map(Cell::Text).unwrap_or(Cell::Null),
            // Issue #892（D2）: 部分和は `i128` で正確に保持し、ここで初めて
            // `i64`（BIGINT）範囲を検査する（走査順に依存しない確定的判定）。
            Accumulator::IntSum(s) => match s {
                None => Cell::Null,
                Some(sum) => {
                    let narrowed = i64::try_from(sum).map_err(|_| {
                        SqlSurfaceError::numeric_out_of_range(
                            "SUM(INTEGER/BIGINT) exceeds BIGINT range",
                        )
                    })?;
                    Cell::SignedInteger(narrowed)
                }
            },
            Accumulator::IntAvg { sum, count } => match sum {
                None => Cell::Null,
                Some(s) => Cell::Float(s as f64 / count as f64),
            },
            Accumulator::IntMin(m) => m.map(Cell::SignedInteger).unwrap_or(Cell::Null),
            Accumulator::IntMax(m) => m.map(Cell::SignedInteger).unwrap_or(Cell::Null),
            // Issue #892（D5）: `unscaled` は列の scale のまま保持しているため
            // `Decimal::from_parts` の scale 検証は必ず通る（列の scale は
            // カタログ検証済みで常に `<= MAX_PRECISION`）。桁あふれ（38 桁超過）
            // は `fits_precision` で確定的に検査する。
            Accumulator::NumericSum { sum, scale } => match sum {
                None => Cell::Null,
                Some(unscaled) => {
                    let decimal = crate::numeric::Decimal::from_parts(unscaled, scale)
                        .map_err(|_| accumulator_bug("SUM(NUMERIC) produced an invalid scale"))?;
                    if !decimal.fits_precision(crate::numeric::MAX_PRECISION) {
                        return Err(SqlSurfaceError::numeric_out_of_range(
                            "SUM(NUMERIC) exceeds the maximum supported precision",
                        ));
                    }
                    Cell::Numeric(decimal)
                }
            },
            // Issue #892（D6）: 結果 scale は列の `precision`/`scale` から
            // `numeric_avg_result_scale` で決め、除算は
            // `crate::numeric::avg_unscaled`（長除算・half away from zero）に
            // 委譲する。丸め後の桁あふれは防御的に再検査する。
            Accumulator::NumericAvg {
                sum,
                count,
                precision,
                scale,
            } => match sum {
                None => Cell::Null,
                Some(unscaled_sum) => {
                    let result_scale = numeric_avg_result_scale(precision, scale);
                    let avg =
                        crate::numeric::avg_unscaled(unscaled_sum, count, scale, result_scale)
                            .map_err(|_| {
                                SqlSurfaceError::numeric_out_of_range(
                                    "AVG(NUMERIC) exceeds the maximum supported precision",
                                )
                            })?;
                    let decimal = crate::numeric::Decimal::from_parts(avg, result_scale)
                        .map_err(|_| accumulator_bug("AVG(NUMERIC) produced an invalid scale"))?;
                    if !decimal.fits_precision(crate::numeric::MAX_PRECISION) {
                        return Err(SqlSurfaceError::numeric_out_of_range(
                            "AVG(NUMERIC) exceeds the maximum supported precision",
                        ));
                    }
                    Cell::Numeric(decimal)
                }
            },
            // `MIN`/`MAX` は observe 時点で列の scale と一致する値のみを保持
            // するため、`from_parts` は常に成功する想定（不一致は実装バグとして
            // `accumulator_bug` で fail-closed に拒否する）。
            Accumulator::NumericMin { value, scale } => match value {
                None => Cell::Null,
                Some(unscaled) => Cell::Numeric(
                    crate::numeric::Decimal::from_parts(unscaled, scale).map_err(|_| {
                        accumulator_bug("MIN(NUMERIC) holds a value with an invalid scale")
                    })?,
                ),
            },
            Accumulator::NumericMax { value, scale } => match value {
                None => Cell::Null,
                Some(unscaled) => Cell::Numeric(
                    crate::numeric::Decimal::from_parts(unscaled, scale).map_err(|_| {
                        accumulator_bug("MAX(NUMERIC) holds a value with an invalid scale")
                    })?,
                ),
            },
            Accumulator::DateMin(m) => m.map(Cell::Date).unwrap_or(Cell::Null),
            Accumulator::DateMax(m) => m.map(Cell::Date).unwrap_or(Cell::Null),
            Accumulator::TimestampMin(m) => m.map(Cell::Timestamp).unwrap_or(Cell::Null),
            Accumulator::TimestampMax(m) => m.map(Cell::Timestamp).unwrap_or(Cell::Null),
            // SQL-25 (c)・TASK-209: 空集合・全 NULL はいずれも `0`
            // （PostgreSQL の `COUNT(DISTINCT ...)` と同じ契約。`COUNT` 一般の
            // 「空集合で NULL ではなく 0」という既存契約とも一致する）。
            Accumulator::CountDistinct { seen, .. } => Cell::Integer(seen.len() as u64),
        })
    }
}

/// `AVG(<NUMERIC(p,s)>)` の結果 scale（Issue #892・D6）。列の scale `s` を
/// 下限とし、整数部（`p - s` 桁）と合わせて `crate::numeric::MAX_PRECISION`
/// （38 桁）以内に収まるよう最大 16 桁まで拡張する
/// （`max(s, min(16, MAX_PRECISION - (p - s)))`）。`p >= s` は列宣言時の
/// カタログ検証済み契約。
fn numeric_avg_result_scale(precision: u8, scale: u8) -> u8 {
    let integer_digits = precision.saturating_sub(scale);
    let budget = crate::numeric::MAX_PRECISION.saturating_sub(integer_digits);
    scale.max(budget.min(16))
}

/// `TextMin`/`TextMax` が新しい極値を保持し直す際にのみ呼ぶ（1 項目あたり高々
/// 1 本。`.claude/rules/security.md`「不安全な設計」対応で `try_reserve_exact` を
/// 使い、確保失敗を abort ではなく `54000` として返す）。
pub(crate) fn try_clone_str(value: &str) -> Result<String, SqlSurfaceError> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("aggregate text value exceeds available memory")
    })?;
    owned.push_str(value);
    Ok(owned)
}

pub(crate) fn storage_internal(e: impl Into<StorageError>) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("aggregate row scan failed: {}", e.into()),
    }
}

/// [`BoundAggregate`] を実行する（TASK-186・NOSQL-4・NOSQL-5 の公開 API）。従来の
/// 5 引数シグネチャ（`execute_aggregate_with_cache`）を維持する薄いラッパー
/// （Issue #357・#363 レビュー指摘対応を踏襲した [`crate::sql::exec::execute_statement`]
/// と同じ設計判断: `visible_cache`・`arena_cache`・`scalar_cache` の各型が属する
/// `sql::visible_cache`／`sql::arena_cache`／`sql::scalar_index` モジュールが
/// いずれも `pub(crate)` のため crate 外から構築不能で、本関数はすべてに `None`
/// を渡しキャッシュ最適化を経由しない新規走査のみの経路として動作する）。crate
/// 内部のキャッシュ経路（`core.rs::EngineCore::execute_sql_in_session` の
/// `Statement::Aggregate` アーム）は引き続き [`execute_aggregate_with_cache`] を
/// 直接呼ぶ。
pub fn execute_aggregate(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundAggregate,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_aggregate_with_cache(
        read_txn,
        ctx,
        schema,
        bound,
        MAX_AGGREGATE_RESULT_BYTES,
        None,
        None,
        None,
    )
}

/// [`crate::sql::parser::BoundAggregate`] を実行し、単一行の [`QueryResult`] を返す
/// （TASK-166・SQL-13）。`read_txn` は呼び出し元（`core.rs::EngineCore::execute_sql_in_session`）
/// が `schema` 取得と同一のトランザクションから渡す（既存の検索 SELECT 実行経路
/// `sql::exec::execute_statement` と同じ「単一スナップショット」契約。Issue #56
/// レビュー指摘対応の踏襲）。
/// `Statement::Aggregate` の唯一の生産用エントリポイント（`core.rs`）は本関数を
/// 直接使う（キャッシュを渡さない場合は `None`）。engine クレート外からは
/// [`execute_aggregate`]（本モジュールの公開ラッパー）を使う。
/// Issue #478: `GROUP BY` なし・`WHERE` なしの
/// `DecodeTier::Fast`（`COUNT(*)`・`COUNT(id)`・`SUM`/`AVG`/`MIN`/`MAX(id)`）に
/// 限り、`visible_cache`（`crate::sql::visible_cache::VisibleBitmapCache`）が
/// 同一テーブル世代でヒットすれば `user_rows/{table}` を一切開かずに
/// `visible_ids` を反復して集計する（A1〜A5 全段の省略。`core.rs::EngineCore`
/// の `Statement::Aggregate` アームから渡される）。ミス時は従来どおり全行走査
/// するが、その走査に相乗りしてスナップショットを構築し `insert` する
/// （`DecodeTier::Fast` は PR #369 の契約によりこの走査で dim・metadata の構造
/// 検証を毎行実施済みのため、構築コストは実質ゼロ）。`GROUP BY`（`sql::group_by`
/// への分岐）・`DimAndScalar`/`Embedding` tier はこのキャッシュの対象外
/// （詳細・スコープ判断は `docs/design/visible-bitmap-cache.md` 参照）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_aggregate_with_cache(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundAggregate,
    // PR #1049 レビュー指摘 P0 対応: 集計結果（`MIN`/`MAX(<TEXT 列>)` の累計
    // バイト数。`GROUP BY` ありは `sql::group_by::execute_grouped_aggregate` の
    // グループキー・TEXT 集計状態の累計）が、完成した [`QueryResult`] を
    // 呼び出し元へ返す前ではなく生成中に頭打ちになるようにする上限。通常の
    // （カーソル非経由の）呼び出しは [`MAX_AGGREGATE_RESULT_BYTES`]（既存挙動を
    // 変えない大きな既定値）を渡し、`sql::cursor::CursorStatement::Declare` の
    // 内側実行（`core.rs::EngineCore::execute_cursor_inner_query`）だけがより
    // 小さい [`crate::sql::cursor::MAX_CURSOR_BYTES_PER_SESSION`] を明示的に渡す
    // （`sql::scan::execute_scan_with_budget` と同じ設計判断）。
    max_result_bytes: usize,
    visible_cache: Option<crate::sql::visible_cache::VisibleCacheAccess<'_>>,
    // Issue #475: スカラー列二次索引（`sql::scalar_index::ScalarIndex`）経由の
    // 候補削減。索引対応述語（`TEXT` 列の等価・前方一致・`id` の単純比較）を
    // 持つ `WHERE` 付き集計に限り、`arena_cache`（RLS 段適用済みスナップ
    // ショット）・`scalar_cache`（索引本体）の双方が揃っている場合に消費する。
    // `GROUP BY` ありは [`crate::sql::group_by::execute_grouped_aggregate`] へ
    // そのまま引き継ぐ（列挙形・候補走査形の判断はそちら側の責務）。
    arena_cache: Option<crate::sql::arena_cache::ArenaCacheAccess<'_>>,
    scalar_cache: Option<crate::sql::scalar_index::ScalarCacheAccess<'_>>,
) -> Result<QueryResult, SqlSurfaceError> {
    // TASK-167（SQL-14）: `GROUP BY` ありは複数行結果を返すため
    // `sql::group_by::execute_grouped_aggregate` へ分岐する（グループ表の有界化・
    // `HAVING`/`ORDER BY`/`LIMIT` はそちらの責務）。`GROUP BY` なしは以下の
    // TASK-166・SQL-13 の単一行経路を維持する（既存挙動は変更しない）。
    if bound.group_by.is_some() {
        return crate::sql::group_by::execute_grouped_aggregate(
            read_txn,
            ctx,
            schema,
            bound,
            max_result_bytes,
            arena_cache,
            scalar_cache,
        );
    }

    let mut accumulators = Vec::with_capacity(bound.items.len());
    for item in &bound.items {
        accumulators.push(Accumulator::for_item(item)?);
    }
    // SQL-25 (c)・TASK-209: `COUNT(DISTINCT)` の中間状態はクエリ全体
    // （`GROUP BY` なしの単一行集計なので `accumulators` の全項目の合計）で
    // 1 つ。Fast 経路（`visible_ids` のみ走査）・全走査のどちらから observe
    // されても同じ予算を共有する（[`observe_with_budgets`] 参照）。
    let mut distinct_budget = crate::sql::distinct::DistinctBudget::new();
    // `VECTOR` 列を宣言するスキーマのみ次元検証を行う（`VECTOR` 列を持たない
    // テーブルは `row_codec`/`storage` 側で常に埋め込み次元 0 として符号化される
    // ため検証対象がない）。`arena.rs::validated_vector_dim_in_txn` と異なり
    // 「`VECTOR` 列が存在しないと失敗する」制約を持たない実装にする（SQL-13 は
    // `VECTOR` 列なしテーブルでの集計を要求するため、本関数は `arena` を経由しない
    // 独自の走査を持つ）。
    let expected_dim = schema.vector_dim();

    // Issue #350: クエリが実際に参照する列集合から、可視行 1 件あたりの最小限の
    // デコード段階（[`DecodeTier`]）を一度だけ決める（`GROUP BY` なしなので
    // `extra_scalar_index` は無し）。新スカラー型（INTEGER〜ENUM。TABLE-13・
    // TASK-199）の列参照も `ReferencedColumns::derive` が `scalar_mask` へ
    // 反映済みのため、以下の判定式は型を問わず「スカラー列参照の有無」だけを見る
    // （新型ごとの分岐は増えない。#894 で判定式自体の網羅性を確認済み）。
    let referenced = ReferencedColumns::derive(
        schema,
        &bound.items,
        &bound.metadata_filters,
        &bound.expr_filters,
        &bound.or_filters,
        &[],
    );
    let tier = select_decode_tier(
        &referenced,
        !bound.expr_filters.is_empty() || !bound.or_filters.is_empty(),
    );

    // Issue #475: `WHERE` が索引対応述語のみ（`classify_scalar_plan` が
    // `PlainScan` 以外）で構成される場合、`user_rows/{table}` の全行走査
    // （O(N)）ではなく `ScalarIndex::resolve_candidates` が絞った候補
    // （概ね O(|hits|)）だけを走査する。`VECTOR` 列を持たないテーブル
    // （SQL-13）はこの索引の構築材料（`SqlArenaSnapshot`／`VectorArena`）を
    // 持てないため対象外。`Fast` tier（`WHERE` 自体が無い）はこの gate に
    // 到達しない（`classify_scalar_plan` は `metadata_filters`・`expr_filters`
    // が両方空なら常に `PlainScan`）。索引が使えない・構築できない・選択度が
    // 悪いなど、あらゆる縮退はこのブロックの外（後続の既存全走査ループ）へ
    // フォールバックするだけでクエリの正しさに影響しない（fail-closed。
    // モジュール「Issue #475」節参照）。
    if let (Some(expected_dim), Some(arena_access), Some(scalar_access)) = (
        schema.vector_dim(),
        arena_cache.as_ref(),
        scalar_cache.as_ref(),
    ) {
        let scalar_shape = crate::sql::scalar_plan::ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &bound.metadata_filters,
            expr_filters: &bound.expr_filters,
            or_filters: &bound.or_filters,
        };
        if crate::sql::scalar_plan::classify_scalar_plan(&scalar_shape)
            != crate::sql::scalar_plan::ScalarPlan::PlainScan
        {
            if let Some(result) = try_scalar_index_aggregate(
                read_txn,
                ctx,
                schema,
                bound,
                &referenced,
                expected_dim,
                arena_access,
                scalar_access,
                max_result_bytes,
            )? {
                return Ok(result);
            }
        }
    }

    // Issue #478: `DecodeTier::Fast` に限り、同一テーブル世代で構築済みの可視
    // `id` 集合がキャッシュにあれば `user_rows/{table}` を一切開かずに集計する
    // （A1〜A5 全段の省略。モジュールドキュメント参照）。`expr_scratch` は
    // `Accumulator::observe` の必須引数だが `Fast` 到達時の入力
    // （`AllVisible`/`IdU64`）は `ScalarExpr` を持たないため未使用のまま渡す。
    if tier == DecodeTier::Fast {
        if let Some(access) = &visible_cache {
            if let Some(snapshot) = access
                .cache
                .lookup(access.storage, read_txn, &bound.table, ctx)
            {
                let mut expr_scratch: Vec<StackValue> = Vec::new();
                let empty_vector = RowVector {
                    dim: 0,
                    values: None,
                };
                // `TextMin`/`TextMax` はこの経路（`AllVisible`/`IdU64`/
                // `COUNT(DISTINCT id)` のみ到達）には現れないため、`TEXT` 予算の
                // 累計はこの分岐専用のローカル変数で十分（後続の全走査分岐とは
                // 独立）。`COUNT(DISTINCT)` の予算（`distinct_budget`）は関数
                // 全体で共有する。
                let mut fast_path_text_accumulator_bytes: usize = 0;
                for &id in snapshot.visible_ids() {
                    for (accumulator, item) in accumulators.iter_mut().zip(&bound.items) {
                        observe_with_budgets(
                            accumulator,
                            &item.input,
                            id,
                            &empty_vector,
                            &[],
                            &mut expr_scratch,
                            &mut fast_path_text_accumulator_bytes,
                            max_result_bytes,
                            &mut distinct_budget,
                        )?;
                    }
                }
                return finish_aggregate_result(accumulators, bound);
            }
        }
    }

    let row_table_name = catalog::user_rows_table_name(&bound.table);
    let table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
        Ok(t) => Some(t),
        // 対象テーブルの行が 1 件も書き込まれていない（行テーブル自体が未作成）は
        // 空集合として扱う（既存の検索 SELECT 実行経路
        // `arena::build_filtered_with_rows_in_txn` と同じ契約）。
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => {
            return Err(SqlSurfaceError::Internal {
                detail: format!(
                    "aggregate row scan failed: {}",
                    catalog::map_row_table_error(e)
                ),
            })
        }
    };

    // 可視行ごとの embedding デコード先スクラッチバッファ（Issue #349・Issue #314
    // 横展開）。行ごとに新しい `Vec<f32>` を確保する代わりにループ外で 1 本だけ
    // 確保し、`storage::decode_row_body_into` が `clear()` 後に書き込む。同一
    // テーブルは全行同一次元のため、初回のみ確保が発生し 2 行目以降は再確保なしで
    // 使い回せる（`arena.rs::build_filtered_with_rows_and_limits_in_txn` と同じ方針）。
    let mut embedding_scratch: Vec<f32> = Vec::new();

    // Issue #353・PR #373 codex-review 指摘対応: `ExprProgram::eval` の明示
    // スタック。[`StackValue`] は行 `embedding` への借用を保持しないため、
    // `embedding_scratch` を毎行 `&mut` で上書きデコードするこのループの外でも
    // 1 回だけ確保し使い回せる（以前は `Vec<ExprValue<'a>>` を直接積んでおり、
    // 借用ライフタイムが行ごとに変わることと両立しないため行ごとに新規確保して
    // いた）。
    let mut expr_scratch: Vec<StackValue> = Vec::new();

    // PR #1049 レビュー指摘 P0 対応: `MIN`/`MAX(<TEXT 列>)` の累計バイト数予算
    // （[`accumulate_text_budget`] ドキュメント参照）。
    let mut total_text_accumulator_bytes: usize = 0;

    // Issue #478: `DecodeTier::Fast` のミス時（上のキャッシュヒット判定で
    // 早期リターンしなかった場合）は、この既存走査に相乗りして可視行の `id` を
    // 記録する（同じ走査の中で `verify_row_key_tenant` 済みの行のみ記録するため
    // 追加コストは実質ゼロ）。`Fast` 以外の tier ではキャッシュを構築しない
    // （スコープ判断は `docs/design/visible-bitmap-cache.md` 参照）。
    let mut visible_builder = if tier == DecodeTier::Fast && visible_cache.is_some() {
        Some(crate::sql::visible_cache::VisibleSnapshotBuilder::new())
    } else {
        None
    };

    if let Some(table) = table {
        'rows: for entry in table.iter().map_err(storage_internal)? {
            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();

            // RLS 段（無条件・デコード前）: `arena.rs` の走査ループと同一の順序
            // （ヘッダのみ読み `predicate` 相当を通過しない行は embedding・
            // metadata を一切デコードしない）。不可視行の破損状態が対象テナントの
            // クエリ可用性へ干渉しない設計を踏襲する（codex P0 対応・Issue #137、
            // security.md P0「テナント境界」）。`offset` は本体デコードの再開位置
            // （[`storage::decode_row_body_into`] へそのまま渡す。Issue #349:
            // ここで一度ヘッダを読んだ後に本体デコード関数の内部でヘッダを
            // もう一度読む二重デコードを、`DecodeTier::Embedding` 到達時は
            // 避ける）。
            let (tenant_id, visibility, offset) =
                storage::decode_row_header(buf).map_err(storage_internal)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // 可視行・常に: 複合キー側の `key_tenant` とヘッダ側 `tenant_id` の
            // 不一致（内部バグ・raw redb 書き込みによる異常）を fail-closed に
            // 拒否する（対象ビヘイビア: TABLE-12。codex P0 指摘対応・PR #229の
            // 踏襲）。以降の embedding・metadata デコードは `tier`（Issue #350）に
            // 応じて必要な範囲のみ行う。
            storage::verify_row_key_tenant(key_tenant, tenant_id).map_err(storage_internal)?;

            // Issue #478: 可視行・TABLE-12 検査済みの `id` をキャッシュ構築用に
            // 記録する（`visible_builder` は `DecodeTier::Fast` かつキャッシュ
            // アクセスが渡された場合のみ `Some`）。
            if let Some(builder) = visible_builder.as_mut() {
                builder.mark_visible(id);
            }

            // 可視行・必要時のみ（Issue #350: embedding 非参照集計のデコード
            // スキップと必要列限定デコード）: `tier` が要求する範囲だけ dim・
            // metadata・embedding をデコードする。`DecodeTier::Fast` は
            // embedding・metadata を一切デコードしないため、`dim` は次元検証の
            // 対象を持たない `0` で表す（ファストパス到達条件は `VECTOR` 列・
            // スカラー列のいずれも参照しないため、この値が誤って観測に使われる
            // ことはない）。
            // `DecodeTier::Fast` も `DecodeTier::DimAndScalar` と同じ
            // `decode_row_dim_and_metadata_borrowed`（ヒープ確保を伴わない構造検証
            // のみ・embedding は境界検証のみで `Vec<f32>` へ確保しない）を必ず通す
            // （codex-review P1 指摘・PR #369: `DecodeTier::Fast` が embedding/
            // metadata セクションの切断・長さ不整合・次元不一致を検査せず破損した
            // 可視行を集計結果へ含めてしまう fail-open な契約変更になっていたため。
            // `Fast` が省略するのは metadata の `&str` 化・保持
            // （`scan_scalar_columns_masked`。呼び出し側で
            // `any_scalar_column_referenced()` が偽の場合は既に省略済み）と
            // embedding の `f32` 変換・確保（`decode_row_embedding_and_metadata_into`）
            // だけであり、破損検知（`XX000`）は全 tier で維持する）。
            let (dim, metadata): (u32, &[u8]) = match tier {
                DecodeTier::Fast | DecodeTier::DimAndScalar => {
                    storage::decode_row_dim_and_metadata_borrowed(buf).map_err(storage_internal)?
                }
                DecodeTier::Embedding => {
                    // 上で読み済みの `offset`（ヘッダ直後の再開位置）を引き継ぎ、
                    // ヘッダの二重デコードを避ける（Issue #349）。
                    storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                        .map_err(storage_internal)?
                }
            };
            // `dim == 0` = `VECTOR` 列が未設定（NULL、TABLE-5 の
            // `ALTER TABLE ADD COLUMN` で追加された nullable 列を含む）という
            // `storage::Row` の既存契約に従い、次元検証は値が実際に存在する行に
            // だけ行う。空を無条件に次元不一致として拒否すると、`COUNT(*)` 等
            // `VECTOR` 値を参照しない集計まで nullable 列の NULL 行で `XX000`
            // 失敗する（PR #229 codex-review 指摘対応）。`DecodeTier::Fast` も
            // dim を実際に読むため（PR #369 codex-review P1 対応）この検証を
            // 同じく経る。
            if let Some(expected) = expected_dim {
                if dim != 0 && dim != expected {
                    return Err(SqlSurfaceError::Internal {
                        detail: "aggregate row scan failed: embedding dimension mismatch"
                            .to_string(),
                    });
                }
            }

            // マスク外の列は presence・長さ・バッファ境界・UTF-8 妥当性の構造検証
            // だけを行い `&str` 化・保持を省略する。`referenced.scalar_mask()` が
            // 全列 `false`（どの列も参照しない。`COUNT(*)`・`SUM(id)` 等）でも
            // 構造検証自体は省略しない（codex-review P1 指摘・PR #369: 検証を丸ごと
            // 省略すると metadata の presence・列長・UTF-8・余剰バイトの構造検証が
            // 全く行われず、破損した可視行を検出できないまま集計が成功してしまう
            // fail-open な契約変更になるため）。
            //
            // `DecodeTier::Fast` は `docs/design/aggregate-decode-skip.md` の tier
            // 契約どおり [`row_codec::validate_scalar_columns`]（検証専用・`Vec`
            // 確保なし）を使う（codex-review P1 指摘・PR #369: 全 tier で
            // `scan_scalar_columns_masked` を呼び毎行 `Vec<Option<&str>>` を確保
            // していたのは、`Fast` がこの確保自体を省略するという tier 契約に反する
            // 実装バグだった）。`Fast` 到達時は `scalar_mask` が全列 `false` かつ
            // `metadata_filters`・`expr_filters` が空（tier 決定条件）であるため、
            // 空スライスで安全に代用できる（`matches_all` は無条件で真、
            // `Accumulator::observe` の `TextColumn` はこの tier では出現しない）。
            let scanned: Vec<Option<row_codec::ScalarRef<'_>>> = match tier {
                DecodeTier::Fast => {
                    row_codec::validate_scalar_columns(schema, metadata)?;
                    Vec::new()
                }
                DecodeTier::DimAndScalar | DecodeTier::Embedding => {
                    row_codec::scan_scalar_columns_masked(
                        schema,
                        metadata,
                        Some(referenced.scalar_mask()),
                    )?
                }
            };

            // SCALAR 段（WHERE）: 既存の検索 SELECT 実行経路（`sql::exec`）と同じ
            // 意味論（等価・前方一致条件 → 式述語の順）で適用する。
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                continue;
            }
            // `expr_filters` が embedding を参照する場合のみ実データを渡す
            // （`references_embedding` は `tier` 決定時と同じ判定関数）。参照しない
            // 式は `id` のみで評価できるため空スライスで安全に評価できる。評価自体は
            // 束縛時にステップ列コンパイル済みの `expr_filter_programs`（`expr_filters`
            // と要素数・評価順が 1 対 1 対応。Issue #353）を使う。
            for (expr, program) in bound.expr_filters.iter().zip(&bound.expr_filter_programs) {
                let embedding: &[f32] = if udf_call::references_embedding(expr) {
                    match tier {
                        DecodeTier::Embedding => embedding_scratch.as_slice(),
                        // `tier` 決定は `expr_filters` の embedding 参照有無も
                        // 見ているため到達しない（実装バグの検出用）。
                        DecodeTier::Fast | DecodeTier::DimAndScalar => {
                            return Err(accumulator_bug(
                                "WHERE expression references the VECTOR column but tier did not decode it",
                            ))
                        }
                    }
                } else {
                    &[]
                };
                match program.eval(id, embedding, &mut expr_scratch)? {
                    ExprValue::Bool(true) => {}
                    ExprValue::Bool(false) => continue 'rows,
                    // 束縛段（`sql::parser::bind_where_predicates`）が `WHERE` 式
                    // 述語の型を `Bool` に限定済みのため到達しない。
                    _ => {
                        return Err(SqlSurfaceError::invalid_input(
                            "WHERE expression did not evaluate to a boolean",
                        ))
                    }
                }
            }
            // TASK-208・SQL-24（Issue #912）: `WHERE` の OR 群を、既存のメタデータ
            // フィルタ・式述語と同じ SCALAR 段の一部として適用する。
            for group in &bound.or_filters {
                let group_embedding: &[f32] = match tier {
                    DecodeTier::Embedding => embedding_scratch.as_slice(),
                    DecodeTier::Fast | DecodeTier::DimAndScalar => &[],
                };
                if !group.matches(
                    &scanned,
                    id,
                    group_embedding,
                    dim as usize,
                    &mut expr_scratch,
                )? {
                    continue 'rows;
                }
            }

            // defense-in-depth（RlsSafetyNet と同趣旨）: 上のヘッダ由来判定と
            // 独立した検査ではなく、ヘッダから取り出した同じ `tenant_id`・
            // `visibility` に対して `PolicyContext::is_visible` を再適用するだけの
            // 重ね掛け。デコード前判定が唯一の防御線にならないようにする
            // （security.md P0）。
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // AGG 段。
            let vector = RowVector {
                dim,
                values: match tier {
                    DecodeTier::Embedding => Some(embedding_scratch.as_slice()),
                    DecodeTier::Fast | DecodeTier::DimAndScalar => None,
                },
            };
            for (accumulator, item) in accumulators.iter_mut().zip(&bound.items) {
                observe_with_budgets(
                    accumulator,
                    &item.input,
                    id,
                    &vector,
                    &scanned,
                    &mut expr_scratch,
                    &mut total_text_accumulator_bytes,
                    max_result_bytes,
                    &mut distinct_budget,
                )?;
            }
        }
    }

    // Issue #478: 走査が正常に完了した場合のみ、採取した可視 `id` 集合を
    // キャッシュへ反映する（構築中にエラーで抜けた場合はここへ到達しないため
    // 非登録のまま——キャッシュは常に検証済みの行だけを保持する）。世代の
    // 読み取りに失敗した場合もキャッシュへ登録しないだけで、クエリ応答自体は
    // 確定済みの集計結果をそのまま返す（fail-closed: 判定できないなら常駐
    // させない。`sql/exec.rs` の `SqlArenaCache` 挿入と同じ方針）。
    if let (Some(builder), Some(access)) = (visible_builder, &visible_cache) {
        if let Ok(generation) = crate::catalog::table_generation_in_txn(read_txn, &bound.table) {
            if let Some(snapshot) = builder.finish(ctx.clone(), generation) {
                access
                    .cache
                    .insert(access.storage, &bound.table, ctx, snapshot);
            }
        }
    }

    finish_aggregate_result(accumulators, bound)
}

/// 確定した [`Accumulator`] 群から単一行の [`QueryResult`] を組み立てる
/// （キャッシュヒット経路・従来の走査経路の両方が共有する終端処理。Issue #478）。
/// Issue #892: `Accumulator::finish` が `Result` を返すようになった
/// （`SUM(INTEGER/BIGINT)` の `i64` 範囲検査・`SUM`/`AVG(NUMERIC)` の 38 桁上限
/// 検査を確定時に行うため）ため、本関数も `Result` を返す。
fn finish_aggregate_result(
    accumulators: Vec<Accumulator>,
    bound: &BoundAggregate,
) -> Result<QueryResult, SqlSurfaceError> {
    let mut columns = Vec::with_capacity(bound.items.len());
    let mut cells = Vec::with_capacity(bound.items.len());
    for (accumulator, item) in accumulators.into_iter().zip(&bound.items) {
        columns.push(ColumnMeta::Computed {
            name: item.name.clone(),
        });
        cells.push(accumulator.finish()?);
    }

    Ok(QueryResult {
        columns,
        rows: vec![ResultRow {
            id: 0,
            score: 0.0,
            cells,
        }],
    })
}

/// Issue #475: `GROUP BY` なし・索引対応述語のみの `WHERE` を持つ集計を、
/// Issue #660 系（索引経路の残存コスト削減）: 集計項目がすべて `COUNT(*)`
/// （[`AggregateInput::AllVisible`]）だけで構成されるかを判定する。
///
/// 真のとき、集計結果は「対象行の**件数**」だけで確定し、行の内容
/// （スカラー列値・embedding・`id`）を一切参照しない。このため
/// `sql::scalar_index::ScalarIndex` が返す候補スロット列・値グループが
/// 「対象行の正確な集合」であると静的に保証できる場合に限り、行ごとの
/// デコード・述語再適用（`observe_candidate_slots`／
/// `sql::group_by::observe_group_slots`）をまるごと省いて
/// [`Accumulator::observe_present_n`] で件数のみを加算できる。
///
/// **信頼してよい範囲（維持すべき不変条件）**: 索引由来の集合をそのまま
/// 件数として使えるのは次をすべて満たす場合のみで、1 つでも崩れたら従来の
/// 候補再適用・全走査経路へ落とす（索引は「絞る」ことしかできないという
/// Issue #474 の多層防御は、この 3 条件下でのみ「索引で完全被覆された
/// 述語に限って信頼する」形へ緩和する）:
///
/// 1. 本関数が `true`（`COUNT(*)` のみ。`COUNT(<列>)` の NULL 意味論は対象外）。
/// 2. `WHERE` が索引で完全被覆されている——`sql::scalar_plan::
///    classify_scalar_plan` が `PlainScan` 以外を返し、かつ
///    `ScalarIndex::resolve_candidates` が `Use` を返した場合。
///    `classify_scalar_plan` は索引非対応の残余述語が 1 つでもあれば
///    `PlainScan` を返す契約（`sql::scalar_plan` モジュールドキュメント）
///    のため、`PlainScan` 以外＝残余述語なしが静的に保証される。
///    `GROUP BY` 列挙形では `WHERE` そのものが空であることが前提。
/// 3. 索引↔スナップショット同一性ガード（行数・構築世代の一致。
///    `ensure_scalar_index_snapshot`／`sql::exec` の SELECT 経路と同じ検査）を
///    通過している——索引の母集合が当該 `(table, ctx)` の可視集合そのもの
///    であることの担保。世代不一致時はこの経路へ入らない。
pub(crate) fn count_star_only(items: &[crate::sql::parser::BoundAggregateItem]) -> bool {
    !items.is_empty()
        && items.iter().all(|item| {
            item.func == crate::sql::allowlist::AggregateFunc::Count
                && matches!(item.input, AggregateInput::AllVisible)
                // SQL-25 (c)・TASK-209: `COUNT(DISTINCT *)` は構文段
                // （`Parser::parse_aggregate_item`）で既に `42601` として拒否
                // 済みのため `AggregateInput::AllVisible` と `distinct: true` が
                // 両立することはないはずだが、多層防御としてここでも明示的に
                // 除外する（索引経由の候補数カウントは異なり値の判定を行わない
                // ため、`distinct` 項目を誤って通すと不正な結果になる）。
                && !item.distinct
        })
}

/// `sql::scalar_index::ScalarIndex::resolve_candidates` の候補削減経由で実行
/// できるか試みる。`Ok(Some(_))` は索引経由で確定した応答（呼び出し元はこれを
/// そのまま返す）、`Ok(None)` は索引が使えない・構築できない・選択度が悪い等の
/// 理由で従来の全走査へ委ねるべきことを示す（fail-closed。索引経路の一切の
/// 縮退はここで吸収し、呼び出し元の全走査ループは無変更のまま安全側へ倒れる）。
/// `Err` は実装バグ相当の内部矛盾（[`accumulator_bug`] 等）のみを返す。
#[allow(clippy::too_many_arguments)]
fn try_scalar_index_aggregate(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundAggregate,
    referenced: &ReferencedColumns,
    expected_dim: u32,
    arena_access: &crate::sql::arena_cache::ArenaCacheAccess<'_>,
    scalar_access: &crate::sql::scalar_index::ScalarCacheAccess<'_>,
    // PR #1049 レビュー指摘 P0 対応: [`observe_candidate_slots`] へそのまま渡す
    // 結果バイト予算（[`execute_aggregate_with_cache`] ドキュメント参照）。
    max_result_bytes: usize,
) -> Result<Option<QueryResult>, SqlSurfaceError> {
    // Cursor Bugbot Medium 指摘（PR #603）: キャッシュ照会・再利用ロジックを
    // [`ensure_scalar_index_snapshot`] へ一本化した（`sql::group_by` の
    // 候補走査形と同じヘルパを共有する）。以前はここで同種のロジックを独自に
    // 複製しており、arena ヒット・スカラー索引ミス時に温かいスナップショットを
    // 使わず `capture_scalar_index_snapshot` で `user_rows` を再走査していた
    // （同関数のドキュメント「Cursor Bugbot Medium 指摘」節参照）。
    let (snapshot, index) = match ensure_scalar_index_snapshot(
        read_txn,
        ctx,
        schema,
        &bound.table,
        expected_dim,
        arena_access,
        scalar_access,
    ) {
        Some(pair) => pair,
        None => {
            scalar_access.cache.record_aggregate_plain_scan_fallback();
            return Ok(None);
        }
    };

    let id_preds: Vec<crate::sql::scalar_plan::IdPredicate> = bound
        .expr_filters
        .iter()
        .filter_map(crate::sql::scalar_plan::id_predicate_from_expr)
        .collect();
    // 数値・日時・`NUMERIC`・`UUID` 列の範囲述語（`FilterOp::TypedCompare`。
    // Issue #891・TASK-199 で production 結線済み）は `bound.metadata_filters`
    // に混在したまま渡り、`ScalarIndex::candidates_for` が内部で振り分ける
    // （Issue #893 production 接続）。
    let slots = match index.resolve_candidates(&bound.metadata_filters, &id_preds) {
        crate::sql::scalar_index::CandidateResolution::Use(slots) => slots,
        crate::sql::scalar_index::CandidateResolution::FallbackNoIndex
        | crate::sql::scalar_index::CandidateResolution::FallbackSelectivity => {
            scalar_access.cache.record_aggregate_plain_scan_fallback();
            return Ok(None);
        }
    };

    let mut accumulators = Vec::with_capacity(bound.items.len());
    for item in &bound.items {
        accumulators.push(Accumulator::for_item(item)?);
    }
    if count_star_only(&bound.items) {
        // 索引で完全被覆された述語のみ（`classify_scalar_plan` が `PlainScan`
        // 以外・`resolve_candidates` が `Use`）かつ `COUNT(*)` だけの集計は、
        // 候補スロット列がそのまま対象行の正確な集合になる（[`count_star_only`]
        // の不変条件 1〜3 参照。同一性ガードは `ensure_scalar_index_snapshot`
        // が通過済み）。行ごとの `scan_scalar_columns_masked`＋`matches_all`＋
        // 式述語の再適用を省き件数のみを加算する。
        let hits = u64::try_from(slots.len())
            .map_err(|_| accumulator_bug("candidate slot count does not fit in u64"))?;
        for acc in &mut accumulators {
            acc.observe_present_n(hits)?;
        }
    } else {
        match observe_candidate_slots(
            &snapshot,
            &slots,
            schema,
            bound,
            referenced,
            &mut accumulators,
            max_result_bytes,
        ) {
            Ok(()) => {}
            // PR #1049 レビュー指摘 codex P1 対応: 候補順に依存しうる一時的な
            // `TEXT` 予算超過は、索引経路の途中結果（`accumulators`）を破棄して
            // 全走査へ退避する（[`is_aggregate_text_budget_error`] 参照）。全走査
            // でも超過するなら、そこで同じ `54000` が返る。
            Err(err) if is_aggregate_text_budget_error(&err) => {
                scalar_access.cache.record_aggregate_plain_scan_fallback();
                return Ok(None);
            }
            Err(err) => return Err(err),
        }
    }
    scalar_access.cache.record_aggregate_index_scan();
    Ok(Some(finish_aggregate_result(accumulators, bound)?))
}

/// Issue #475: `WHERE` なし・索引対応述語のみの `WHERE` を持つ `GROUP BY` の
/// 候補走査形が使う、`ScalarIndex` 由来のスナップショット・索引を用意する
/// （キャッシュヒットならそのまま、ミスなら [`capture_scalar_index_snapshot`]
/// で構築して両キャッシュへ登録する）。`aggregate.rs`（`GROUP BY` なし。
/// [`try_scalar_index_aggregate`] から）と `sql::group_by`（`GROUP BY` あり）が
/// 共有する（crate 内公開）。
///
/// Cursor Bugbot Medium 指摘（PR #603）: `SqlArenaCache` はヒットしたが
/// `ScalarIndexCache` はミスした場合（または索引↔スナップショット同一性
/// ガード不一致）、以前はここで `capture_scalar_index_snapshot` により
/// `user_rows` を無条件に再走査していた。ヒット済みの温かい `SqlArenaSnapshot`
/// が既にあるにもかかわらず全量デコードを繰り返すうえ、再走査が失敗すると
/// （`mark_capture_unbuildable` により）同一世代の以降の呼び出しまで索引経路
/// 自体が丸ごと使えなくなっていた。`sql::exec::execute_statement_with_cache`
/// の SELECT 経路〔Issue #473〕が既に行っている方式と同じく、arena ヒット時は
/// ヒット済みスナップショットから [`crate::sql::scalar_index::ScalarIndex::build`]
/// のみを実行する。両キャッシュがともにミスした場合のみ、従来どおり
/// [`capture_scalar_index_snapshot`] による全行再走査（`is_capture_known_
/// unbuildable` ゲート・`mark_capture_unbuildable` 記録を含む）へフォールバック
/// する。
#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_scalar_index_snapshot(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    table: &str,
    expected_dim: u32,
    arena_access: &crate::sql::arena_cache::ArenaCacheAccess<'_>,
    scalar_access: &crate::sql::scalar_index::ScalarCacheAccess<'_>,
) -> Option<(
    std::sync::Arc<crate::sql::arena_cache::SqlArenaSnapshot>,
    std::sync::Arc<crate::sql::scalar_index::ScalarIndex>,
)> {
    let arena_hit = arena_access
        .cache
        .lookup(arena_access.storage, read_txn, table, ctx);
    let scalar_hit = scalar_access
        .cache
        .lookup(scalar_access.storage, read_txn, table, ctx);

    if let (Some(snapshot), Some(index)) = (&arena_hit, &scalar_hit) {
        // 索引↔スナップショット同一性ガード（`sql::exec` の SELECT 経路と
        // 同じ検査。`ScalarIndexCache`／`SqlArenaCache` はそれぞれ独立に
        // `(table, ctx)` × 世代でキャッシュされるため、両者が同時に
        // ヒットしても由来スナップショットが一致する保証がない）。
        if index.row_count() == snapshot.arena().len()
            && index.built_table_generation() == snapshot.built_table_generation_for_index()
        {
            return Some((
                std::sync::Arc::clone(snapshot),
                std::sync::Arc::clone(index),
            ));
        }
    }

    if let Some(snapshot) = arena_hit {
        // Bugbot Medium 指摘: arena はヒット・スカラー索引はミス（または
        // 同一性ガード不一致）。`user_rows` を再走査せず、ヒット済み
        // スナップショットから索引だけを構築する。
        //
        // codex-review P2 指摘（PR #603）: この構築が容量超過等で失敗した
        // 場合、[`crate::sql::scalar_index::ScalarIndexCache::mark_build_
        // unbuildable`] で記録しないと、`SqlArenaCache` がヒットし続ける限り
        // 同一世代の集計・`GROUP BY` クエリのたびにこの（確定的に失敗する）
        // 構築——スナップショット全体のデコード・文字列複製——を繰り返して
        // から全走査へ縮退してしまう。まず `is_capture_known_unbuildable`
        // （起点を問わず判定する。同メソッドのドキュメント参照）で既知の
        // 失敗を確認し、既知なら構築自体を試みない。
        if scalar_access
            .cache
            .is_capture_known_unbuildable(read_txn, table, ctx)
        {
            return None;
        }
        return match crate::sql::scalar_index::ScalarIndex::build(schema, &snapshot) {
            Ok(index) => scalar_access
                .cache
                .insert(scalar_access.storage, table, ctx, index)
                .map(|index| (snapshot, index)),
            Err(_) => {
                // このスナップショット（＝この世代）に対して確定的に構築
                // 不能。採取失敗（`capture_scalar_index_snapshot` が `None`
                // を返した場合）とは区別し、`mark_build_unbuildable` で
                // 記録する。
                scalar_access.cache.record_build_failure();
                scalar_access
                    .cache
                    .mark_build_unbuildable(read_txn, table, ctx);
                None
            }
        };
    }

    // 両キャッシュともミス: 従来どおり `capture_scalar_index_snapshot` に
    // よる全走査 piggyback 採取（`is_capture_known_unbuildable` ゲート・
    // `mark_capture_unbuildable` 記録を含む）へフォールバックする。
    if scalar_access
        .cache
        .is_capture_known_unbuildable(read_txn, table, ctx)
    {
        return None;
    }
    let (snapshot, index) =
        match capture_scalar_index_snapshot(read_txn, ctx, schema, table, expected_dim) {
            Some(pair) => pair,
            None => {
                scalar_access
                    .cache
                    .mark_capture_unbuildable(read_txn, table, ctx);
                return None;
            }
        };
    // `index` は挿入前の `snapshot` から構築済み（`try_scalar_index_aggregate`
    // と同じ理由で再構築不要。同関数のドキュメント参照）。
    let inserted_snapshot = arena_access
        .cache
        .insert(arena_access.storage, table, ctx, snapshot);
    let inserted_index = scalar_access
        .cache
        .insert(scalar_access.storage, table, ctx, index)?;
    Some((inserted_snapshot, inserted_index))
}

/// Issue #475: `ScalarIndex::resolve_candidates` が絞った候補スロット
/// （`snapshot` 上の添字）を走査し、可視行と同じ SCALAR 段・集計判定を適用する。
/// 索引は候補を「絞る」ことしかできないため、ここで `matches_all`・式述語
/// （残余は許されない——`classify_scalar_plan` の gate により `expr_filters` は
/// 常に `id` 単純比較のみ）を候補行にも再適用する（`sql::exec` の SELECT 経路と
/// 同じ多層防御。モジュールドキュメント「Issue #475」節）。
pub(crate) fn observe_candidate_slots(
    snapshot: &crate::sql::arena_cache::SqlArenaSnapshot,
    slots: &[u32],
    schema: &TableSchema,
    bound: &BoundAggregate,
    referenced: &ReferencedColumns,
    accumulators: &mut [Accumulator],
    // PR #1049 レビュー指摘 P0 対応: `MIN`/`MAX(<TEXT 列>)` の累計バイト数予算
    // （[`accumulate_text_budget`] ドキュメント参照）。索引経由の候補走査は
    // 通常の全走査ループ（本モジュール下部）と別の実行経路のため、同じ予算
    // 検査を独立して適用する。
    max_result_bytes: usize,
) -> Result<(), SqlSurfaceError> {
    let arena = snapshot.arena();
    let mut expr_scratch: Vec<StackValue> = Vec::new();
    let mut total_text_accumulator_bytes: usize = 0;
    // SQL-25 (c)・TASK-209: `COUNT(DISTINCT)` の中間状態はクエリ全体
    // （`accumulators` の全項目の合計）で 1 つ。索引経路が超過した場合は
    // `is_aggregate_text_budget_error` に一致しない別文言のため呼び出し元
    // （`try_scalar_index_aggregate`）が全走査へフォールバックせず、そのまま
    // `54000` を返す（異なり値の集合は処理順序に依存しない単調増加のため、
    // 索引経路と全走査で成否が変わってはならない）。
    let mut distinct_budget = crate::sql::distinct::DistinctBudget::new();
    'candidates: for &slot in slots {
        let slot_idx = usize::try_from(slot)
            .map_err(|_| accumulator_bug("candidate slot does not fit in usize"))?;
        let &id = arena
            .ids()
            .get(slot_idx)
            .ok_or_else(|| accumulator_bug("candidate slot out of bounds (ids)"))?;
        let metadata = snapshot
            .metadata()
            .get(slot_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let scanned = row_codec::scan_scalar_columns_masked(
            schema,
            metadata,
            Some(referenced.scalar_mask()),
        )?;

        if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
            continue;
        }
        for (expr, program) in bound.expr_filters.iter().zip(&bound.expr_filter_programs) {
            let embedding: &[f32] = if udf_call::references_embedding(expr) {
                arena
                    .vector(slot_idx)
                    .ok_or_else(|| accumulator_bug("candidate slot out of bounds (vector)"))?
            } else {
                &[]
            };
            match program.eval(id, embedding, &mut expr_scratch)? {
                ExprValue::Bool(true) => {}
                ExprValue::Bool(false) => continue 'candidates,
                _ => {
                    return Err(SqlSurfaceError::invalid_input(
                        "WHERE expression did not evaluate to a boolean",
                    ))
                }
            }
        }
        // TASK-208・Issue #912: `classify_scalar_plan` は OR 群を含む述語を常に
        // `PlainScan` へ縮退させるため、この索引経由の候補走査へは通常到達しない
        // （`bound.or_filters` は空のはず）。多層防御として、万一到達しても
        // OR を黙って無視しない（fail-open 防止）。
        for group in &bound.or_filters {
            let group_embedding: &[f32] = if group.references_embedding() {
                arena
                    .vector(slot_idx)
                    .ok_or_else(|| accumulator_bug("candidate slot out of bounds (vector)"))?
            } else {
                &[]
            };
            if !group.matches(
                &scanned,
                id,
                group_embedding,
                arena.dim() as usize,
                &mut expr_scratch,
            )? {
                continue 'candidates;
            }
        }

        let vector = RowVector {
            dim: arena.dim(),
            values: if referenced.needs_embedding() {
                Some(
                    arena
                        .vector(slot_idx)
                        .ok_or_else(|| accumulator_bug("candidate slot out of bounds (vector)"))?,
                )
            } else {
                None
            },
        };
        for (accumulator, item) in accumulators.iter_mut().zip(&bound.items) {
            observe_with_budgets(
                accumulator,
                &item.input,
                id,
                &vector,
                &scanned,
                &mut expr_scratch,
                &mut total_text_accumulator_bytes,
                max_result_bytes,
                &mut distinct_budget,
            )?;
        }
    }
    Ok(())
}

/// Issue #475: `ScalarIndex` 経由の候補削減・`GROUP BY` キー列挙が使う
/// [`crate::sql::arena_cache::SqlArenaSnapshot`] を、この集計クエリ自身の走査
/// から piggyback で構築する（`sql::exec` の SELECT 経路が構築した
/// `SqlArenaCache` エントリに依存すると、集計クエリ単独では索引が永遠に
/// 構築されず候補削減が非 vacuous にならないため）。
///
/// [`crate::arena::SqlArenaCaptureBuilder::push`] は `embedding.len() ==
/// expected_dim` を検証しない（`GrowableArenaBuffers::push_row` が
/// `extend_from_slice` するだけ）ため、呼び出し元がここで dim を検証してから
/// 渡す必要がある。nullable `VECTOR` 列の `NULL` 行（`dim == 0`）を含む世代は
/// アリーナのレイアウト不変条件（`vectors.len() == ids.len() * dim`）を守れず
/// 索引を構築できないため、1 行でも次元不一致を検出した時点で採取全体を
/// 断念する（soft-fail。呼び出し元は `None` を受け取り、この集計クエリ自体は
/// 従来の全走査へフォールバックするだけで失敗しない）。
fn capture_scalar_index_snapshot(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    table: &str,
    expected_dim: u32,
) -> Option<(
    crate::sql::arena_cache::SqlArenaSnapshot,
    crate::sql::scalar_index::ScalarIndex,
)> {
    let row_table_name = catalog::user_rows_table_name(table);
    let row_table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
        Ok(t) => t,
        // 行テーブル未作成（1 行も書き込まれていない）は空の索引を構築する
        // 意味がないため採取を断念する（呼び出し元は全走査へフォールバック。
        // 空テーブルは全走査も O(1) で軽量なため実害はない）。
        Err(_) => return None,
    };

    let mut capture = crate::arena::SqlArenaCaptureBuilder::new(
        expected_dim,
        crate::arena::MAX_ARENA_ROWS,
        crate::arena::MAX_ARENA_TOTAL_BYTES,
        crate::arena::MAX_ARENA_TOTAL_BYTES,
    );
    let mut embedding_scratch: Vec<f32> = Vec::new();
    let mut capture_ok = true;

    for entry in row_table.iter().ok()? {
        let (k, v) = entry.ok()?;
        let (key_tenant, id) = k.value();
        let buf = v.value();
        let (tenant_id, visibility, offset) = storage::decode_row_header(buf).ok()?;
        if !ctx.is_visible(tenant_id, visibility) {
            continue;
        }
        // TABLE-12: 物理キー側 `tenant_id` とヘッダ側 `tenant_id` の不一致は
        // このクエリ本体（呼び出し元の全走査）が独立に検出し `XX000` を返す
        // 契約のため、ここ（piggyback な派生キャッシュの採取）では採取だけを
        // 諦め、クエリ応答の成否には関与しない。
        if storage::verify_row_key_tenant(key_tenant, tenant_id).is_err() {
            capture_ok = false;
            continue;
        }
        if !capture_ok {
            continue;
        }
        let (dim, metadata) =
            match storage::decode_row_body_into(buf, offset, &mut embedding_scratch) {
                Ok(v) => v,
                Err(_) => {
                    capture_ok = false;
                    continue;
                }
            };
        if dim != expected_dim {
            // nullable VECTOR の NULL 行（dim 0）を含む世代はこの索引の対象外
            // （型ドキュメント参照）。
            capture_ok = false;
            continue;
        }
        capture.push(id, tenant_id, visibility, &embedding_scratch, metadata, 0);
        // Issue #475 codex-review P2 対応: `capture` が容量超過（`MAX_ARENA_
        // TOTAL_BYTES` 等）で `failed` へ遷移すると、以降の `push` は no-op に
        // 縮退し `finish` は確定で `None` を返す。それにもかかわらずこの
        // ループを最後まで回すと、残り行すべてについて意味のない
        // `decode_row_body_into`（embedding 全量デコード）を続けてしまう
        // （容量超過は行数・バイト量に比例するため、テーブル規模が大きいほど
        // 無駄なデコード・確保コストも比例して増える）。`finish` を待たず
        // ここで打ち切り、呼び出し元を全走査フォールバックへ即座に委ねる。
        if capture.failed() {
            capture_ok = false;
            break;
        }
    }

    if !capture_ok {
        return None;
    }
    let (arena, metadata) = capture.finish(table)?;
    let generation = crate::catalog::table_generation_in_txn(read_txn, table).ok()?;
    let snapshot =
        crate::sql::arena_cache::SqlArenaSnapshot::new(arena, metadata, ctx.clone(), generation);
    let index = crate::sql::scalar_index::ScalarIndex::build(schema, &snapshot).ok()?;
    Some((snapshot, index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::sql::allowlist::AggregateFunc;
    use crate::sql::parser::{BoundAggregate, BoundAggregateItem};
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    /// nullable な `VECTOR` 列を宣言するテーブルへ、`storage::encode_row`
    /// （低レベル API）で直接行を書き込む。TABLE-5 が想定する「既存行が対象列を
    /// 未設定のまま持つ」状態（`row.embedding` が空）を、現行の公開 INSERT 経路
    /// （`tenant::insert_row` 系。`TableSchema::validate_embedding_dim` を介し常に
    /// 次元一致を要求する）を経由せず直接再現するための検証専用ヘルパ（PR #229
    /// codex-review 指摘対応）。
    fn write_row_direct(
        storage: &Storage,
        table_name: &str,
        tenant_id: &str,
        id: u64,
        embedding: &[f32],
    ) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name(table_name),
                ))
                .expect("open row table");
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            })
            .expect("encode row");
            table
                .insert((tenant_id, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// 物理キー側 `tenant_id`（`key_tenant`）とヘッダ側 `tenant_id`
    /// （`header_tenant`）を意図的にずらして raw redb 書き込みする（TABLE-12 の
    /// 整合検査を検証するための専用ヘルパ。Issue #349: 旧 `decode_row_for_key`
    /// 内部検査から `execute_aggregate`/`execute_grouped_aggregate` 側の明示比較へ
    /// 移設した後も、この不一致が fail-closed に拒否されることを固定する）。
    fn write_row_with_mismatched_key_tenant(
        storage: &Storage,
        table_name: &str,
        key_tenant: &str,
        header_tenant: &str,
        id: u64,
        embedding: &[f32],
    ) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name(table_name),
                ))
                .expect("open row table");
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id: header_tenant,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            })
            .expect("encode row");
            table
                .insert((key_tenant, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// nullable な `VECTOR` 列（TABLE-5 想定）を単一列として持つテーブルの
    /// スキーマ・空 `BoundAggregate` を組み立てる共通部。
    fn nullable_vector_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), true)],
        )
    }

    fn bound_single(func: AggregateFunc, input: AggregateInput) -> BoundAggregate {
        BoundAggregate {
            table: "docs".to_string(),
            items: vec![BoundAggregateItem {
                func,
                input,
                name: "result".to_string(),
                distinct: false,
            }],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            rls_predicate_present: false,
            projection: vec![crate::sql::parser::ProjectionColumn::Aggregate {
                item_index: 0,
                name: "result".to_string(),
            }],
            group_by: None,
        }
    }

    /// PR #1049 レビュー指摘 P0 対応の回帰テスト用: 同一 `(func, input)` の組を
    /// `count` 個並べた `BoundAggregate`（`GROUP BY` なし）を組み立てる。
    /// `MIN`/`MAX(<TEXT 列>)` を複数項目束ねることで、単一行集計が
    /// [`Accumulator::text_len`] の累計をどれだけ保持しうるかを検証する。
    fn bound_multi(func: AggregateFunc, input: AggregateInput, count: usize) -> BoundAggregate {
        let items: Vec<BoundAggregateItem> = (0..count)
            .map(|i| BoundAggregateItem {
                func,
                input: input.clone(),
                name: format!("result_{i}"),
                distinct: false,
            })
            .collect();
        let projection = (0..count)
            .map(|i| crate::sql::parser::ProjectionColumn::Aggregate {
                item_index: i,
                name: format!("result_{i}"),
            })
            .collect();
        BoundAggregate {
            table: "docs".to_string(),
            items,
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            rls_predicate_present: false,
            projection,
            group_by: None,
        }
    }

    /// 検証専用: `encode_row` で組み立てた妥当な行バイト列を、そのまま行テーブルへ
    /// 直接書き込む（`storage::encode_row` の検証をバイパスして任意バイト列を
    /// 挿入する `write_row_direct` とは異なり、ここでは呼び出し元が事前に破損させた
    /// `buf` をそのまま書き込む）。
    fn write_row_raw(storage: &Storage, table_name: &str, tenant_id: &str, id: u64, buf: &[u8]) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name(table_name),
                ))
                .expect("open row table");
            table.insert((tenant_id, id), buf).expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// `encode_row` が組み立てた行バイト列の `dim` フィールドを
    /// `MAX_EMBEDDING_DIM` 超過の値へ書き換える（embedding/metadata セクション
    /// 全体の以降のデコードを構造的に破綻させる検証専用ヘルパ）。オフセット計算は
    /// `storage.rs` の行フォーマット（version 1B + tenant_len 2B + tenant_id +
    /// visibility 1B + dim 4B ...）に対応する（`storage.rs` 側の同種テスト
    /// `decode_row_dim_and_metadata_borrowed_rejects_same_corruptions_as_decode_row`
    /// 等と同じオフセット計算）。
    fn corrupt_dim_field(mut buf: Vec<u8>, tenant_id: &str) -> Vec<u8> {
        let dim_offset = 1 + 2 + tenant_id.len() + 1;
        buf[dim_offset..dim_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        buf
    }

    #[test]
    fn count_vector_column_skips_null_rows_but_count_star_does_not() {
        let path = unique_db_path("agg-nullable-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        // id=1: VECTOR 値あり、id=2: nullable 列が未設定（embedding 空 = NULL）。
        write_row_direct(&storage, "docs", "tenant-a", 1, &[1.0, 2.0, 3.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        // COUNT(embedding) は NULL 行（id=2）を数えない。
        let bound_vec = bound_single(AggregateFunc::Count, AggregateInput::VectorColumnPresence);
        let result = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_vec,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect("COUNT(embedding) should succeed even with a NULL row present");
        assert_eq!(result.rows[0].cells[0], Cell::Integer(1));

        // COUNT(*) は VECTOR 値を参照しないため、nullable 列の NULL 行があっても
        // 次元不一致（旧 XX000）を返さず両方の可視行を数える（本 PR の中心的指摘）。
        let bound_star = bound_single(AggregateFunc::Count, AggregateInput::AllVisible);
        let result = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_star,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect("COUNT(*) must not fail on a nullable VECTOR column's NULL row");
        assert_eq!(result.rows[0].cells[0], Cell::Integer(2));
    }

    // Issue #349: TABLE-12 の整合検査（物理キー側 `tenant_id` とヘッダ側
    // `tenant_id` の不一致）が、`decode_row_for_key` 呼び出しをやめた後の
    // 明示比較でも従来どおり fail-closed（`XX000`・`SqlSurfaceError::Internal`）に
    // 拒否されることを固定する（旧 `decode_row_for_key` 内部検査への暗黙依存を
    // 解消するための回帰）。
    #[test]
    fn key_tenant_header_tenant_mismatch_is_rejected_fail_closed() {
        let path = unique_db_path("agg-table12-mismatch");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        // key 側は tenant-a、ヘッダ側は tenant-b（tenant-a から見て可視な
        // Public 行だが、key/ヘッダ不一致という異常な行）。
        write_row_with_mismatched_key_tenant(
            &storage,
            "docs",
            "tenant-a",
            "tenant-b",
            1,
            &[1.0, 2.0, 3.0],
        );

        let ctx = PolicyContext::new("tenant-b").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound_star = bound_single(AggregateFunc::Count, AggregateInput::AllVisible);
        let err = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_star,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect_err("key/header tenant mismatch must be rejected fail-closed");
        assert_eq!(err.wire_code(), "XX000");
    }

    // --- DecodeTier::Fast の破損検知（Issue #350・PR #369 codex-review P1 対応） ---

    #[test]
    fn count_star_fastpath_still_fails_closed_on_corrupted_embedding_section() {
        // `COUNT(*)`（`WHERE` なし・`VECTOR`/スカラー列参照なし）は
        // `DecodeTier::Fast` に到達するが、`decode_row_dim_and_metadata_borrowed`
        // による構造検証（ヒープ確保は伴わない）を `DecodeTier::DimAndScalar` と
        // 共有するため、embedding セクションの `dim` フィールドを構造的に
        // 破綻させた行は fail-closed（`XX000`）で拒否される（PR #369
        // codex-review P1 指摘対応: 従来案のように破損可視行を集計結果へ
        // 黙って含めない）。
        let path = unique_db_path("agg-fastpath-corrupt");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        let good = crate::storage::encode_row(&RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0, 3.0],
            metadata: &[],
        })
        .expect("encode row");
        let corrupted = corrupt_dim_field(good, "tenant-a");
        write_row_raw(&storage, "docs", "tenant-a", 1, &corrupted);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound_star = bound_single(AggregateFunc::Count, AggregateInput::AllVisible);
        let err = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_star,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect_err("COUNT(*) fast path must fail closed on a corrupted embedding section");
        assert_eq!(err.wire_code(), "XX000");
    }

    #[test]
    fn count_vector_column_on_same_corrupted_row_still_fails_closed() {
        // 同じ破損行に対し、`VECTOR` 列を参照する `COUNT(embedding)`
        // （`DecodeTier::DimAndScalar`）も同じく dim フィールドを実際に読むため、
        // 破損を検出して `Err`（`XX000`）になる——`Fast`/`DimAndScalar` 間で
        // 検証水準に差が無いことの対照テスト。
        let path = unique_db_path("agg-dimscalar-corrupt");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        let good = crate::storage::encode_row(&RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0, 3.0],
            metadata: &[],
        })
        .expect("encode row");
        let corrupted = corrupt_dim_field(good, "tenant-a");
        write_row_raw(&storage, "docs", "tenant-a", 1, &corrupted);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound_vec = bound_single(AggregateFunc::Count, AggregateInput::VectorColumnPresence);
        let err = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_vec,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "XX000");
    }

    #[test]
    fn count_star_still_fails_closed_on_corrupted_metadata_with_no_scalar_reference() {
        // `COUNT(*)`（`VECTOR`/スカラー列いずれも非参照）で `referenced.scalar_mask()`
        // が全 `false` になっても、`scan_scalar_columns_masked` の呼び出し自体は
        // 省略しない（codex-review P1 指摘・PR #369: 呼び出しごと省略すると
        // metadata の presence・列長・UTF-8・余剰バイトの構造検証が全く行われず、
        // 破損した可視行（不正 UTF-8 を含む `Text` 列）が集計結果へ黙って含まれて
        // しまう fail-open な契約変更になっていたための回帰テスト）。
        let path = unique_db_path("agg-metadata-corrupt-no-scalar-ref");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), true),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        );
        storage.create_table(&schema).expect("create table");

        let mut metadata = row_codec::encode_scalar_columns(
            &schema,
            &[
                row_codec::Value::Null,
                row_codec::Value::Text("hello".to_string()),
            ],
        )
        .expect("encode scalar columns");
        // 妥当な `Text` 値のバイト列を、presence タグ・長さフィールドはそのまま
        // 不正 UTF-8（単独継続バイト）へ書き換える。
        let corrupt_offset = metadata.len() - 1;
        metadata[corrupt_offset] = 0x80;

        let buf = crate::storage::encode_row(&RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0, 3.0],
            metadata: &metadata,
        })
        .expect("encode row");
        write_row_raw(&storage, "docs", "tenant-a", 1, &buf);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound_star = bound_single(AggregateFunc::Count, AggregateInput::AllVisible);
        let err = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_star,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect_err(
            "COUNT(*) must fail closed on corrupted metadata even with no scalar column referenced",
        );
        assert_eq!(err.wire_code(), "XX000");
    }

    fn count_acc() -> Accumulator {
        Accumulator::new(AggregateFunc::Count, &AggregateInput::AllVisible).unwrap()
    }

    #[test]
    fn count_empty_set_is_zero() {
        let acc = count_acc();
        assert_eq!(acc.finish().unwrap(), Cell::Integer(0));
    }

    #[test]
    fn sum_id_empty_set_is_null() {
        let acc = Accumulator::new(AggregateFunc::Sum, &AggregateInput::IdU64).unwrap();
        assert_eq!(acc.finish().unwrap(), Cell::Null);
    }

    #[test]
    fn sum_id_overflow_is_rejected_as_numeric_out_of_range() {
        let mut acc = Accumulator::new(AggregateFunc::Sum, &AggregateInput::IdU64).unwrap();
        acc.observe_id(u64::MAX).unwrap();
        let err = acc.observe_id(1).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn avg_id_divides_sum_by_count() {
        let mut acc = Accumulator::new(AggregateFunc::Avg, &AggregateInput::IdU64).unwrap();
        acc.observe_id(2).unwrap();
        acc.observe_id(4).unwrap();
        assert_eq!(acc.finish().unwrap(), Cell::Float(3.0));
    }

    #[test]
    fn float_sum_overflow_is_rejected_as_numeric_out_of_range() {
        let mut acc = Accumulator::FloatSum(None);
        acc.observe_float(f64::MAX).unwrap();
        let err = acc.observe_float(f64::MAX).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn text_min_max_skip_null_and_compare_by_byte_order() {
        let mut min = Accumulator::TextMin(None);
        let mut max = Accumulator::TextMax(None);
        for v in [Some("banana"), None, Some("apple"), Some("cherry")] {
            min.observe_text(v).unwrap();
            max.observe_text(v).unwrap();
        }
        assert_eq!(min.finish().unwrap(), Cell::Text("apple".to_string()));
        assert_eq!(max.finish().unwrap(), Cell::Text("cherry".to_string()));
    }

    #[test]
    fn text_min_max_empty_set_is_null() {
        let min = Accumulator::TextMin(None);
        assert_eq!(min.finish().unwrap(), Cell::Null);
    }

    /// PR #1049 レビュー指摘 P0 対応の回帰テスト: `GROUP BY` なしの単一行集計が
    /// `MIN`/`MAX(<TEXT 列>)` を複数項目束ねた場合、[`accumulate_text_budget`]
    /// が呼び出し元の指定するバイト予算（`sql::cursor::
    /// MAX_CURSOR_BYTES_PER_SESSION` 相当の小さい値を模した `1`）で行生成中に
    /// 打ち切ることを固定する。[`crate::sql::scan::execute_scan_with_budget_
    /// honors_caller_supplied_cap`] と同じ検証形（既定予算では成功・極端に
    /// 小さい予算では `54000` で早期失敗）。
    #[test]
    fn execute_aggregate_with_cache_honors_caller_supplied_result_byte_budget_for_text_min_max() {
        let path = unique_db_path("agg-text-budget-cap");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), true),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        );
        storage.create_table(&schema).expect("create table");

        let metadata = row_codec::encode_scalar_columns(
            &schema,
            &[
                row_codec::Value::Null,
                row_codec::Value::Text("hello world".to_string()),
            ],
        )
        .expect("encode scalar columns");
        let buf = crate::storage::encode_row(&RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0, 3.0],
            metadata: &metadata,
        })
        .expect("encode row");
        write_row_raw(&storage, "docs", "tenant-a", 1, &buf);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        // 5 項目（`MIN`/`MAX(body)` を交互に束ねる）の単一行集計。既定予算
        // （[`MAX_AGGREGATE_RESULT_BYTES`]）では成功する。
        let bound = bound_multi(AggregateFunc::Min, AggregateInput::TextColumn(1), 5);
        execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect("default budget should succeed");

        // 同じデータ・同じクエリでも、呼び出し元が極端に小さい予算を渡せば
        // 行生成中に打ち切られる（`sql::cursor::CursorStatement::Declare` の
        // 内側実行が `MAX_CURSOR_BYTES_PER_SESSION` を渡す経路の回帰）。
        let err =
            execute_aggregate_with_cache(&read_txn, &ctx, &schema, &bound, 1, None, None, None)
                .expect_err("tiny caller-supplied budget must reject before default cap");
        assert_eq!(err.wire_code(), "54000");
    }

    /// PR #1049 レビュー指摘 codex P1 の回帰テスト: 単一行集計の索引経路
    /// （候補走査）で、候補順に依存する一時的な `MIN(<TEXT 列>)` 予算超過が
    /// 起きても `54000` で失敗させず、全走査（物理行順）へ退避して全走査と同じ
    /// 結果を返す。
    ///
    /// 構成: redb 上は `id` 昇順（1:`a`、2:`bbbbbbbbbb`）だが、温めた
    /// `SqlArenaSnapshot` には `id` 降順で積むため、候補走査は 2 → 1 の順に処理する。
    /// `MIN(v)` の保持量のピークは物理行順なら 1 バイト、候補順なら 10 バイトに
    /// なるため、予算 5 バイトでは候補順だけが途中で超過する。
    #[test]
    fn index_path_text_budget_overflow_falls_back_to_full_scan() {
        let path = unique_db_path("agg-index-text-budget-fallback");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("kind", ColumnType::Text, false),
                ColumnDef::new("v", ColumnType::Text, false),
            ],
        );
        storage.create_table(&schema).expect("create table");

        let rows: [(u64, &str, &str); 5] = [
            (1, "x", "a"),
            (2, "x", "bbbbbbbbbb"),
            (3, "y", "z"),
            (4, "y", "z"),
            (5, "y", "z"),
        ];
        let metadata_of = |kind: &str, v: &str| {
            row_codec::encode_scalar_columns(
                &schema,
                &[
                    row_codec::Value::Null,
                    row_codec::Value::Text(kind.to_string()),
                    row_codec::Value::Text(v.to_string()),
                ],
            )
            .expect("encode scalar columns")
        };
        for (id, kind, v) in rows {
            let metadata = metadata_of(kind, v);
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0, 3.0],
                metadata: &metadata,
            })
            .expect("encode row");
            write_row_raw(&storage, "docs", "tenant-a", id, &buf);
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let expected_dim = schema.vector_dim().expect("vector dim");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");
        let generation = crate::catalog::table_generation_in_txn(&read_txn, "docs")
            .expect("read table generation");

        // 物理行順と逆（`id` 降順）のスナップショットを温める。
        let mut capture = crate::arena::SqlArenaCaptureBuilder::new(
            expected_dim,
            crate::arena::MAX_ARENA_ROWS,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
        );
        for (id, kind, v) in rows.iter().rev() {
            capture.push(
                *id,
                "tenant-a",
                Visibility::Public,
                &[1.0, 2.0, 3.0],
                &metadata_of(kind, v),
                0,
            );
        }
        let (arena, metadata) = capture.finish("docs").expect("capture");
        let snapshot = crate::sql::arena_cache::SqlArenaSnapshot::new(
            arena,
            metadata,
            ctx.clone(),
            generation,
        );
        let arena_cache = crate::sql::arena_cache::SqlArenaCache::new();
        let scalar_cache = crate::sql::scalar_index::ScalarIndexCache::new();
        arena_cache.insert(&storage, "docs", &ctx, snapshot);

        let crate::sql::allowlist::Statement::Aggregate(validated) =
            crate::sql::allowlist::validate_sql(
                "SELECT MIN(v) AS mn FROM docs WHERE kind = 'x'",
                &storage,
            )
            .expect("validate")
        else {
            panic!("expected aggregate statement");
        };
        let bound = crate::sql::parser::bind_aggregate(
            &validated,
            &schema,
            &crate::sql::udf_call::UdfRegistry::default(),
        )
        .expect("bind");

        let run = |budget: usize| {
            execute_aggregate_with_cache(
                &read_txn,
                &ctx,
                &schema,
                &bound,
                budget,
                None,
                Some(crate::sql::arena_cache::ArenaCacheAccess {
                    storage: &storage,
                    cache: &arena_cache,
                }),
                Some(crate::sql::scalar_index::ScalarCacheAccess {
                    storage: &storage,
                    cache: &scalar_cache,
                }),
            )
        };

        // 既定予算では索引経路をそのまま使う（非 vacuous 性の確認）。
        let before = scalar_cache.stats();
        let ok = run(MAX_AGGREGATE_RESULT_BYTES).expect("default budget succeeds");
        assert_eq!(ok.rows[0].cells, vec![Cell::Text("a".to_string())]);
        let after = scalar_cache.stats();
        assert_eq!(
            after.aggregate_index_scans,
            before.aggregate_index_scans + 1,
            "the candidate-walk index path must be taken"
        );

        // 予算 5 バイト: 候補順では途中で超過するが、全走査へ退避して成功する。
        let before = scalar_cache.stats();
        let result = run(5).expect("order-dependent overflow must fall back, not fail");
        assert_eq!(result.rows[0].cells, vec![Cell::Text("a".to_string())]);
        let after = scalar_cache.stats();
        assert_eq!(
            after.aggregate_plain_scan_fallbacks,
            before.aggregate_plain_scan_fallbacks + 1,
            "the index path must record a fallback to the full scan"
        );
        assert_eq!(after.aggregate_index_scans, before.aggregate_index_scans);

        // 全走査でも超過する予算では、従来どおり `54000` で失敗する。
        let err = run(0).expect_err("overflow on the full scan must still fail");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn float_min_max_use_total_cmp() {
        let mut min = Accumulator::FloatMin(None);
        let mut max = Accumulator::FloatMax(None);
        for v in [3.0, -1.5, 42.0] {
            min.observe_float(v).unwrap();
            max.observe_float(v).unwrap();
        }
        assert_eq!(min.finish().unwrap(), Cell::Float(-1.5));
        assert_eq!(max.finish().unwrap(), Cell::Float(42.0));
    }

    // --- Issue #892: 新スカラー型のアキュムレータ単体テスト ------------------

    #[test]
    fn int_sum_finish_rejects_values_exceeding_i64_range() {
        let mut acc = Accumulator::IntSum(None);
        acc.observe_int(Some(i64::MAX)).unwrap();
        acc.observe_int(Some(1)).unwrap();
        let err = acc.finish().unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn int_sum_finish_succeeds_when_partial_overflow_cancels_out() {
        // 部分和が一時的に `i64` を超えても最終値が収まれば成功する（D2）。
        let mut acc = Accumulator::IntSum(None);
        acc.observe_int(Some(i64::MAX)).unwrap();
        acc.observe_int(Some(1)).unwrap();
        acc.observe_int(Some(-1)).unwrap();
        assert_eq!(acc.finish().unwrap(), Cell::SignedInteger(i64::MAX));
    }

    #[test]
    fn int_min_max_null_rows_are_ignored() {
        let mut min = Accumulator::IntMin(None);
        let mut max = Accumulator::IntMax(None);
        min.observe_int(None).unwrap();
        max.observe_int(None).unwrap();
        min.observe_int(Some(-3)).unwrap();
        max.observe_int(Some(-3)).unwrap();
        min.observe_int(Some(5)).unwrap();
        max.observe_int(Some(5)).unwrap();
        assert_eq!(min.finish().unwrap(), Cell::SignedInteger(-3));
        assert_eq!(max.finish().unwrap(), Cell::SignedInteger(5));
    }

    #[test]
    fn numeric_sum_finish_rejects_values_exceeding_38_digits() {
        let near_max = 99_999_999_999_999_999_999_999_999_999_999_999_999i128;
        let mut acc = Accumulator::NumericSum {
            sum: None,
            scale: 0,
        };
        acc.observe_numeric(Some(near_max)).unwrap();
        acc.observe_numeric(Some(1)).unwrap();
        let err = acc.finish().unwrap_err();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn numeric_min_max_compare_unscaled_values_at_the_same_scale() {
        let mut min = Accumulator::NumericMin {
            value: None,
            scale: 2,
        };
        let mut max = Accumulator::NumericMax {
            value: None,
            scale: 2,
        };
        for v in [150i128, -50, 300] {
            min.observe_numeric(Some(v)).unwrap();
            max.observe_numeric(Some(v)).unwrap();
        }
        assert_eq!(
            min.finish().unwrap(),
            Cell::Numeric(crate::numeric::Decimal::from_parts(-50, 2).unwrap())
        );
        assert_eq!(
            max.finish().unwrap(),
            Cell::Numeric(crate::numeric::Decimal::from_parts(300, 2).unwrap())
        );
    }

    #[test]
    fn date_and_timestamp_min_max_ignore_null_rows() {
        let mut day_min = Accumulator::DateMin(None);
        let mut day_max = Accumulator::DateMax(None);
        day_min.observe_date(None).unwrap();
        day_max.observe_date(None).unwrap();
        for v in [100i32, -50, 300] {
            day_min.observe_date(Some(v)).unwrap();
            day_max.observe_date(Some(v)).unwrap();
        }
        assert_eq!(day_min.finish().unwrap(), Cell::Date(-50));
        assert_eq!(day_max.finish().unwrap(), Cell::Date(300));

        let mut ts_min = Accumulator::TimestampMin(None);
        let mut ts_max = Accumulator::TimestampMax(None);
        for v in [1_000i64, -500, 3_000] {
            ts_min.observe_timestamp(Some(v)).unwrap();
            ts_max.observe_timestamp(Some(v)).unwrap();
        }
        assert_eq!(ts_min.finish().unwrap(), Cell::Timestamp(-500));
        assert_eq!(ts_max.finish().unwrap(), Cell::Timestamp(3_000));
    }

    #[test]
    fn numeric_avg_result_scale_expands_scale_up_to_16_within_the_38_digit_budget() {
        // `NUMERIC(5, 2)`: 整数部 3 桁 → budget = 38-3 = 35 → min(16,35)=16。
        assert_eq!(numeric_avg_result_scale(5, 2), 16);
        // `NUMERIC(38, 2)`: 整数部 36 桁 → budget = 38-36 = 2 → 列の scale のまま。
        assert_eq!(numeric_avg_result_scale(38, 2), 2);
        // `NUMERIC(38, 38)`: 整数部 0 桁 → budget = 38 → min(16,38)=16 だが
        // 列の scale（38）自体が下限のため、拡張されず 38 のまま。
        assert_eq!(numeric_avg_result_scale(38, 38), 38);
    }

    // --- Issue #475: piggyback 索引採取の soft-fail 契約 ------------------

    /// nullable な `VECTOR` 列（TABLE-5 想定）の `NULL` 行（`dim == 0`）を含む
    /// 世代では、[`capture_scalar_index_snapshot`] が採取全体を断念して `None`
    /// を返すこと（型ドキュメント参照）を固定する。`ScalarIndex` 経由の
    /// 索引化を試みず、呼び出し元（`execute_aggregate_with_cache`）は従来の
    /// 全走査へフォールバックするだけでクエリ自体は失敗しない契約を、この
    /// レイヤ単体のオラクルとして検証する（`tests/scalar_index_aggregate.rs`
    /// の結合テストは現行の公開 INSERT 経路が nullable `VECTOR` 列の NULL 行を
    /// 書き込めない制約——`write_row_direct` 参照——のため代替できない）。
    #[test]
    fn capture_scalar_index_snapshot_soft_fails_on_nullable_vector_null_row() {
        let path = unique_db_path("agg-scalar-index-capture-null-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        // id=1: VECTOR 値あり（dim==expected_dim）、id=2: nullable 列が未設定
        // （embedding 空 = NULL・dim==0）。
        write_row_direct(&storage, "docs", "tenant-a", 1, &[1.0, 2.0, 3.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");
        let expected_dim = schema.vector_dim().expect("vector dim");

        assert!(
            capture_scalar_index_snapshot(&read_txn, &ctx, &schema, "docs", expected_dim).is_none(),
            "a generation containing a nullable-VECTOR NULL row must not be captured"
        );
    }

    /// 上記と対照的に、`NULL` 行を含まない世代では通常どおり採取・構築できる
    /// ことを固定する（soft-fail が「常に None」の縮退ではないことの確認）。
    #[test]
    fn capture_scalar_index_snapshot_succeeds_without_null_rows() {
        let path = unique_db_path("agg-scalar-index-capture-ok");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        write_row_direct(&storage, "docs", "tenant-a", 1, &[1.0, 2.0, 3.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[4.0, 5.0, 6.0]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");
        let expected_dim = schema.vector_dim().expect("vector dim");

        let (snapshot, index) =
            capture_scalar_index_snapshot(&read_txn, &ctx, &schema, "docs", expected_dim)
                .expect("capture should succeed when no row is NULL");
        assert_eq!(snapshot.arena().len(), 2);
        assert_eq!(index.row_count(), 2);
    }

    /// Cursor Bugbot Medium 指摘（PR #603）の回帰: `SqlArenaCache` はヒット
    /// （温かい `SqlArenaSnapshot` が既にある）だが `ScalarIndexCache` はミスの
    /// 場合、[`ensure_scalar_index_snapshot`] が `user_rows/{table}` の再走査
    /// （[`capture_scalar_index_snapshot`]）に頼らず、温かいスナップショットから
    /// 索引だけを構築できることを固定する。
    ///
    /// 検証手段: arena キャッシュへ手動でスナップショットを温めた**後**に、
    /// テーブル世代カウンタ（`catalog::table_generation_in_txn` が参照する
    /// `TABLE_GENERATION_TABLE`）を変えないまま行テーブル（`user_rows/docs`）を
    /// 削除する。世代が変わらないため温めた arena キャッシュエントリは
    /// 引き続きヒットし続けるが、もし実装が（修正前のように）
    /// `capture_scalar_index_snapshot` へフォールバックすれば、行テーブル不在
    /// （`TableDoesNotExist`）により必ず `None` になる。したがって
    /// `Some(_)` が返ることは「温かいスナップショットを実際に再利用した」
    /// ことの直接証拠になる。
    #[test]
    fn ensure_scalar_index_snapshot_reuses_warm_arena_snapshot_without_row_table_rescan() {
        let path = unique_db_path("agg-scalar-index-reuse-warm-arena");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        write_row_direct(&storage, "docs", "tenant-a", 1, &[1.0, 2.0, 3.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[4.0, 5.0, 6.0]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let expected_dim = schema.vector_dim().expect("vector dim");

        use redb::ReadableDatabase;
        let arena_cache = crate::sql::arena_cache::SqlArenaCache::new();
        let scalar_cache = crate::sql::scalar_index::ScalarIndexCache::new();
        {
            // arena キャッシュだけを温める（スカラー索引キャッシュへは意図的に
            // 登録しない。ミス状態を維持する）。
            let read_txn = storage.db().begin_read().expect("begin_read");
            let (snapshot, _index) =
                capture_scalar_index_snapshot(&read_txn, &ctx, &schema, "docs", expected_dim)
                    .expect("initial capture should succeed");
            arena_cache.insert(&storage, "docs", &ctx, snapshot);
        }

        // 行テーブルを、世代カウンタ（`TABLE_GENERATION_TABLE`）を変えずに
        // 削除する（`Storage::drop_table` はカタログエントリごと削除し世代も
        // 進めてしまうため使わない。`write_row_direct` 等の
        // `bump_generation_and_commit` が進めるのは別カウンタ（`GENERATION_TABLE`。
        // `storage::current_generation_in_txn` 用）であり
        // `catalog::table_generation_in_txn` には影響しない）。
        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            write_txn
                .delete_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("delete row table");
            write_txn.commit().expect("commit row table deletion");
        }

        let arena_access = crate::sql::arena_cache::ArenaCacheAccess {
            storage: &storage,
            cache: &arena_cache,
        };
        let scalar_access = crate::sql::scalar_index::ScalarCacheAccess {
            storage: &storage,
            cache: &scalar_cache,
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let resolved = ensure_scalar_index_snapshot(
            &read_txn,
            &ctx,
            &schema,
            "docs",
            expected_dim,
            &arena_access,
            &scalar_access,
        );
        let (snapshot, index) = resolved
            .expect("arena キャッシュヒット時は行テーブルの再走査に頼らず索引を構築できるはず");
        assert_eq!(snapshot.arena().len(), 2);
        assert_eq!(index.row_count(), 2);
    }

    /// codex-review P2 指摘（PR #603）の回帰: `SqlArenaCache` ヒット・
    /// `ScalarIndexCache` ミスの経路で温かいスナップショットから
    /// `ScalarIndex::build` を実行した結果が失敗した場合、
    /// `mark_build_unbuildable` により同一世代の以降の呼び出しは
    /// `is_capture_known_unbuildable` に阻まれて再試行しないことを固定する。
    ///
    /// 検証手段: `SqlArenaCaptureBuilder` へ、TEXT 列として不正な presence
    /// バイト（`0xFF`）を含むメタデータを直接 `push` することで、通常の redb
    /// 行走査を経由せず `ScalarIndex::build` が確実に失敗する温かいスナップ
    /// ショットを手動で構築する。同一 `read_txn`（＝同一世代）で
    /// `ensure_scalar_index_snapshot` を 2 回呼び、`ScalarIndexCacheStats::
    /// build_failures` が 1 回目でのみ増加し 2 回目では増加しないことを
    /// 確認する（修正前は 2 回目も構築を再試行し `build_failures` がもう
    /// 1 回増えていた）。
    #[test]
    fn ensure_scalar_index_snapshot_does_not_retry_build_after_known_failure_in_same_generation() {
        let path = unique_db_path("agg-scalar-index-build-failure-memo");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("kind", ColumnType::Text, false),
            ],
        );
        storage.create_table(&schema).expect("create table");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let expected_dim = schema.vector_dim().expect("vector dim");

        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");
        let generation = crate::catalog::table_generation_in_txn(&read_txn, "docs")
            .expect("read table generation");

        let mut capture = crate::arena::SqlArenaCaptureBuilder::new(
            expected_dim,
            crate::arena::MAX_ARENA_ROWS,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
        );
        // `SqlArenaCaptureBuilder::push` はメタデータの内容を検証しない
        // （`ScalarIndex::build` 実行時に初めて `scan_scalar_columns` が
        // 検証する）ため、不正な presence バイトをそのまま仕込める。
        capture.push(
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 2.0, 3.0],
            &[0xFF],
            0,
        );
        let (arena, metadata) = capture
            .finish("docs")
            .expect("capture must succeed structurally");
        let snapshot = crate::sql::arena_cache::SqlArenaSnapshot::new(
            arena,
            metadata,
            ctx.clone(),
            generation,
        );

        let arena_cache = crate::sql::arena_cache::SqlArenaCache::new();
        let scalar_cache = crate::sql::scalar_index::ScalarIndexCache::new();
        arena_cache.insert(&storage, "docs", &ctx, snapshot);

        let arena_access = crate::sql::arena_cache::ArenaCacheAccess {
            storage: &storage,
            cache: &arena_cache,
        };
        let scalar_access = crate::sql::scalar_index::ScalarCacheAccess {
            storage: &storage,
            cache: &scalar_cache,
        };

        let failures_before = scalar_cache.stats().build_failures;
        let first = ensure_scalar_index_snapshot(
            &read_txn,
            &ctx,
            &schema,
            "docs",
            expected_dim,
            &arena_access,
            &scalar_access,
        );
        assert!(
            first.is_none(),
            "corrupted TEXT metadata must make ScalarIndex::build fail"
        );
        let failures_after_first = scalar_cache.stats().build_failures;
        assert_eq!(
            failures_after_first,
            failures_before + 1,
            "the first attempt must record exactly one build failure"
        );

        let second = ensure_scalar_index_snapshot(
            &read_txn,
            &ctx,
            &schema,
            "docs",
            expected_dim,
            &arena_access,
            &scalar_access,
        );
        assert!(second.is_none());
        let failures_after_second = scalar_cache.stats().build_failures;
        assert_eq!(
            failures_after_second, failures_after_first,
            "a generation already known to be unbuildable must not retry the build"
        );
    }

    // --- Issue #894: 新スカラー型（TABLE-13・TASK-199）の DecodeTier 選択 ------
    //
    // `select_decode_tier` は Issue #350 の判定式をインライン if 連鎖から抽出した
    // ものであり、`ReferencedColumns` が新型の列参照をどう反映するかにさえ依存
    // しない（`scalar_mask`／`has_scalar_reference` を見るだけで型を区別しない）。
    // ここでは (a) 関数単体を直接構成した `ReferencedColumns` で固定し、
    // (b) 新型 `AggregateInput` を実際に `ReferencedColumns::derive` へ通した
    // 経路でも `DimAndScalar` に収まり `needs_embedding() == false` になることを
    // 固定する（受入条件 3）。

    fn referenced_columns_for_test(
        scalar_mask: Vec<bool>,
        needs_embedding: bool,
        needs_vector_presence: bool,
        has_scalar_reference: bool,
    ) -> ReferencedColumns {
        ReferencedColumns {
            scalar_mask,
            needs_embedding,
            needs_vector_presence,
            has_scalar_reference,
        }
    }

    #[test]
    fn select_decode_tier_no_reference_is_fast() {
        let referenced =
            referenced_columns_for_test(vec![false, false, false], false, false, false);
        assert_eq!(select_decode_tier(&referenced, false), DecodeTier::Fast);
    }

    #[test]
    fn select_decode_tier_scalar_reference_is_dim_and_scalar() {
        let referenced = referenced_columns_for_test(vec![false, true, false], false, false, true);
        assert_eq!(
            select_decode_tier(&referenced, false),
            DecodeTier::DimAndScalar
        );
    }

    #[test]
    fn select_decode_tier_vector_presence_is_dim_and_scalar() {
        let referenced = referenced_columns_for_test(vec![false, false, false], false, true, false);
        assert_eq!(
            select_decode_tier(&referenced, false),
            DecodeTier::DimAndScalar
        );
    }

    #[test]
    fn select_decode_tier_expr_filter_is_dim_and_scalar() {
        let referenced =
            referenced_columns_for_test(vec![false, false, false], false, false, false);
        assert_eq!(
            select_decode_tier(&referenced, true),
            DecodeTier::DimAndScalar
        );
    }

    #[test]
    fn select_decode_tier_needs_embedding_wins_over_scalar_reference() {
        // `needs_embedding` は `scalar_mask`／`has_scalar_reference` の状態を問わず
        // 最優先で `Embedding` を選ぶ（判定式の分岐順序を固定する）。
        let referenced = referenced_columns_for_test(vec![true, true], true, true, true);
        assert_eq!(select_decode_tier(&referenced, true), DecodeTier::Embedding);
    }

    /// 新スカラー型（TABLE-13・TASK-199）を多数含むスキーマ。`ReferencedColumns::
    /// derive` が型ごとに分岐せず一律に `scalar_mask` を立てることを確認する
    /// ための固定 fixture（列順は本 Issue のスコープと無関係で任意）。
    fn many_new_scalar_types_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), true), // 0
                ColumnDef::new("u", ColumnType::Uuid, true),              // 1
                ColumnDef::new("bo", ColumnType::Boolean, true),          // 2
                ColumnDef::new(
                    "n",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ), // 3
                ColumnDef::new("dt", ColumnType::Date, true),             // 4
                ColumnDef::new("ts", ColumnType::Timestamp, true),        // 5
                ColumnDef::new("i", ColumnType::Integer, true),           // 6
                ColumnDef::new("bi", ColumnType::BigInt, true),           // 7
                ColumnDef::new("r", ColumnType::Real, true),              // 8
                ColumnDef::new("d", ColumnType::Double, true),            // 9
            ],
        )
    }

    #[test]
    fn derive_and_select_decode_tier_new_type_count_is_dim_and_scalar_without_embedding() {
        let schema = many_new_scalar_types_schema();
        for (input, label) in [
            (AggregateInput::UuidColumn(1), "uuid"),
            (AggregateInput::BooleanColumn(2), "boolean"),
            (
                AggregateInput::NumericColumn {
                    index: 3,
                    precision: 10,
                    scale: 2,
                },
                "numeric",
            ),
            (AggregateInput::DateColumn(4), "date"),
            (AggregateInput::TimestampColumn(5), "timestamp"),
            (AggregateInput::IntegerColumn(6), "integer"),
            (AggregateInput::BigIntColumn(7), "bigint"),
            (AggregateInput::RealColumn(8), "real"),
            (AggregateInput::DoubleColumn(9), "double"),
        ] {
            let items = vec![BoundAggregateItem {
                func: AggregateFunc::Count,
                input: input.clone(),
                name: "result".to_string(),
                distinct: false,
            }];
            let referenced = ReferencedColumns::derive(&schema, &items, &[], &[], &[], &[]);
            assert!(
                !referenced.needs_embedding(),
                "COUNT({label}) must not require embedding decode"
            );
            let tier = select_decode_tier(&referenced, false);
            assert_eq!(
                tier,
                DecodeTier::DimAndScalar,
                "COUNT({label}) must select DimAndScalar (TABLE-13・TASK-199)"
            );
            let col_index = match &input {
                AggregateInput::UuidColumn(i)
                | AggregateInput::BooleanColumn(i)
                | AggregateInput::DateColumn(i)
                | AggregateInput::TimestampColumn(i)
                | AggregateInput::IntegerColumn(i)
                | AggregateInput::BigIntColumn(i)
                | AggregateInput::RealColumn(i)
                | AggregateInput::DoubleColumn(i) => *i,
                AggregateInput::NumericColumn { index, .. } => *index,
                _ => unreachable!(),
            };
            assert!(
                referenced.scalar_mask()[col_index],
                "COUNT({label}) must mark its own column in scalar_mask"
            );
        }
    }

    #[test]
    fn derive_and_select_decode_tier_count_star_on_new_type_schema_is_fast() {
        // `WHERE`・スカラー列参照・`VECTOR` 列参照のいずれも無い `COUNT(*)` は、
        // 新型列を多数持つスキーマでも `Fast` のまま（受入条件 3）。
        let schema = many_new_scalar_types_schema();
        let items = vec![BoundAggregateItem {
            func: AggregateFunc::Count,
            input: AggregateInput::AllVisible,
            name: "result".to_string(),
            distinct: false,
        }];
        let referenced = ReferencedColumns::derive(&schema, &items, &[], &[], &[], &[]);
        assert_eq!(select_decode_tier(&referenced, false), DecodeTier::Fast);
    }

    #[test]
    fn derive_and_select_decode_tier_count_embedding_on_new_type_schema_is_dim_and_scalar() {
        // `COUNT(<VECTOR 列>)`（次元のみ必要・embedding 本体は不要）は、新型列を
        // 多数持つスキーマでも従来どおり `DimAndScalar`。
        let schema = many_new_scalar_types_schema();
        let items = vec![BoundAggregateItem {
            func: AggregateFunc::Count,
            input: AggregateInput::VectorColumnPresence,
            name: "result".to_string(),
            distinct: false,
        }];
        let referenced = ReferencedColumns::derive(&schema, &items, &[], &[], &[], &[]);
        assert!(!referenced.needs_embedding());
        assert_eq!(
            select_decode_tier(&referenced, false),
            DecodeTier::DimAndScalar
        );
    }

    /// 新型列を持つスキーマでの TABLE-12（キー側／ヘッダ側 tenant 不一致）が、
    /// `Fast`／`DimAndScalar` いずれの tier でも fail-closed（`XX000`）で
    /// 拒否されることを固定する（受入条件 2。`key_tenant_header_tenant_mismatch_
    /// is_rejected_fail_closed` の新型版）。
    #[test]
    fn key_tenant_header_tenant_mismatch_is_rejected_fail_closed_on_new_type_schema_fast_and_dim_and_scalar(
    ) {
        let path = unique_db_path("agg-table12-mismatch-new-types");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = many_new_scalar_types_schema();
        storage.create_table(&schema).expect("create table");

        write_row_with_mismatched_key_tenant(
            &storage,
            "docs",
            "tenant-a",
            "tenant-b",
            1,
            &[1.0, 2.0, 3.0],
        );

        let ctx = PolicyContext::new("tenant-b").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        // Fast tier: COUNT(*)
        let bound_star = bound_single(AggregateFunc::Count, AggregateInput::AllVisible);
        let err = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_star,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect_err("Fast tier must reject key/header tenant mismatch");
        assert_eq!(err.wire_code(), "XX000");

        // DimAndScalar tier: COUNT(<UUID 列>)（新型列参照）
        let bound_uuid = bound_single(AggregateFunc::Count, AggregateInput::UuidColumn(1));
        let err = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_uuid,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect_err("DimAndScalar tier (new-type column) must reject key/header tenant mismatch");
        assert_eq!(err.wire_code(), "XX000");
    }

    /// `Fast` tier（`COUNT(*)`・新型列は非参照）でも、可視行の新型スカラー列
    /// メタデータが破損していれば fail-closed に拒否する（受入条件 2。
    /// `count_star_still_fails_closed_on_corrupted_metadata_with_no_scalar_
    /// reference` の新型版。REAL 列に NaN のビットパターンを直接書き込む）。
    #[test]
    fn count_star_still_fails_closed_on_corrupted_new_type_metadata_with_no_scalar_reference() {
        let path = unique_db_path("agg-metadata-corrupt-new-type-no-scalar-ref");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), true),
                ColumnDef::new("score", ColumnType::Real, false),
            ],
        );
        storage.create_table(&schema).expect("create table");

        let mut metadata = row_codec::encode_scalar_columns(
            &schema,
            &[row_codec::Value::Null, row_codec::Value::Real(1.0)],
        )
        .expect("encode scalar columns");
        // presence タグはそのまま、REAL 値のバイト列だけを NaN へ書き換える
        // （末尾 4 バイトが REAL の値フィールド。`SCALAR_REAL_ENTRY_LEN` = 5 =
        // presence(1) + f32(4)）。
        let value_offset = metadata.len() - 4;
        metadata[value_offset..].copy_from_slice(&f32::NAN.to_le_bytes());

        let buf = crate::storage::encode_row(&RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0, 3.0],
            metadata: &metadata,
        })
        .expect("encode row");
        write_row_raw(&storage, "docs", "tenant-a", 1, &buf);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound_star = bound_single(AggregateFunc::Count, AggregateInput::AllVisible);
        let err = execute_aggregate_with_cache(
            &read_txn,
            &ctx,
            &schema,
            &bound_star,
            MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
            None,
        )
        .expect_err(
            "COUNT(*) must fail closed on corrupted REAL metadata even with no scalar column referenced",
        );
        assert_eq!(err.wire_code(), "XX000");
    }
}
