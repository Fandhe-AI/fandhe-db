//! `op: search` の JSON クエリオブジェクトを `engine::sql::parser::
//! BoundStatement` へ束縛するモジュール（Issue #763・TASK-175。対象
//! ビヘイビア NOSQL-2・NOSQL-7・SQL-1〜5・SQL-12。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-2・`docs/spec/04-behavior/sql-surface.md` SQL-1〜5・SQL-12）。
//!
//! 責務境界: [`super::gate`] が認証済み要求（`SEARCH_SCHEMA` による形の検証
//! 済み [`super::schema::Validated`]）を得たあと呼び出す入口が [`bind_search`]。
//! SQL 表層の検索 `SELECT` が `sql::parser::bind_in_session` で得るのと
//! **同一形の `BoundStatement`** を、SQL テキストを一切生成せず
//! `BoundStatement::new` で直接構築する（第 2 の実行器を作らない方針。
//! `filter.rs`（Issue #761）が `declarative_filter::bind_all` を共有するのと
//! 同じ判断）。投影列解決・ベクトル値検証・本文列規約・`LIMIT` 範囲・
//! `mode` 解決は `engine::sql::parser`／`engine::sql::mode` の既存公開関数へ
//! 委譲し、束縛規則を wire-server 側に複製しない。
//!
//! `vector`／`plan` は排他（SQL の `ORDER BY`／`USING PLAN` が構文上
//! 相互排他なのと対応）。`plan` 指定は LLM 展開（`EngineCore` の private
//! フィールド `embedder`／`query_planner` 経由の I/O）が必要なため、本
//! モジュールでは束縛可能な部品（投影・フィルタ・`limit`・未解決 `mode`・
//! 原質問）を [`PlanSearch`] として返すに留める。`BoundStatement` までの
//! 完成・実行は [`execute`]／[`handle`] が [`EngineCore::
//! execute_bound_search_in_session`]（`vector` 指定）・[`EngineCore::
//! execute_bound_plan_search_in_session`]（`plan` 指定。TASK-186・
//! NOSQL-2・Issue #764）へ委譲する（第 2 の実行器を作らない設計。
//! `docs/design/bound-plan-session-entry.md` 参照）。
//!
//! RLS はサーバー側の `PolicyContext` 暗黙適用のみで、クライアントは述語を
//! 書けない（`security.md` P0「テナント境界」）。[`bind_search`] は
//! `PolicyContext` を一切受け取らず、`BoundStatement::new` の
//! `rls_predicate_present` は常に `false` で構築する。`vector`／`plan` いずれ
//! でも RLS 暗黙適用は [`EngineCore`] 側（`execute_statement_with_cache` の
//! `ImplicitRlsHook`。RLS-7）が担い、本モジュールはテナント判定を一切行わ
//! ない。
//!
//! `explain: true` の要求は通常 [`super::gate::handle`] が [`execute`] より
//! 前に [`super::explain::execute`] へ振り分ける（`vector` 指定は Issue
//! #948・NOSQL-16・SQL-27、`plan` 指定は NOSQL-10・Issue #765）。[`execute`]
//! 内の [`SearchError::ExplainRequiresPlan`]／[`SearchError::
//! ExplainNotSupported`] 拒否は、ゲートを迂回して直接呼ばれた場合に備える
//! 多層防御としてのみ残る（黙って無視して fail-open にしない）。

use std::time::SystemTime;

use engine::catalog::TableSchema;
use engine::core::{EngineCore, PlanSearchBinding};
use engine::declarative_filter::MetadataFilter;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::sql::allowlist::{validate_using_plan_question, SqlSurfaceError};
use engine::sql::mode::{self, SearchMode, SessionState};
use engine::sql::parser::{
    bind_body_text_column, bind_column_projection, bind_vector_values, require_vector_column,
    validate_search_limit, BoundStatement, ProjectedColumn, Ranking,
};
use engine::sql::plan::EvaluationOrder;

use super::filter::{bind_filter, FilterError};
use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated};
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

/// `plan` 指定の束縛結果（Issue #763 時点での中間形）。LLM 展開・再埋め込みは
/// engine 内部 I/O（`EngineCore` の private フィールド経由）を要するため、
/// 本モジュールでは `BoundStatement` まで到達させない。#764 が
/// `crate::sql::using_plan::bind_expansion` 相当の処理を経て `BoundStatement`
/// を完成させる際、ここで束縛済みの部品（投影・フィルタ・`limit`）と
/// 原質問文字列をそのまま使う。
///
/// `query_mode`（`search.mode`）は**未解決のまま**保持する。SQL の
/// `USING PLAN` 経路（`core.rs::EngineCore::execute_sql_in_session`）は
/// `resolve_mode_with_planner(query, session, planner_hint)` で
/// `PlannerEstimate` 由来の推定とあわせて解決するため、ここで
/// `resolve_mode(query_mode, None)` へ確定してしまうとプランナー推定を
/// 採用する経路を失う（TASK-164・PLAN-11）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PlanSearch {
    table: String,
    projection: Vec<ProjectedColumn>,
    /// `filter` の束縛結果（メタデータフィルタ・式述語・`OR` 群。Issue #945・
    /// NOSQL-14）。`engine::sql::declarative_predicate::BoundWhereFilters` は
    /// `pub mod` の `pub` 型のためクレート外（本モジュール）から名指しできる
    /// （`crate::sql::where_tree` が `pub(crate) mod` のため個々のフィールド型
    /// までは名指しできないが、この不透明な入れ物のまま持ち回せる）。
    filters: engine::sql::declarative_predicate::BoundWhereFilters,
    limit: usize,
    question: String,
    query_mode: Option<SearchMode>,
    explain: bool,
}

impl PlanSearch {
    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn projection(&self) -> &[ProjectedColumn] {
        &self.projection
    }

    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        self.filters.metadata_filters()
    }

    /// `filter` の束縛結果一式（メタデータフィルタ・式述語・`OR` 群）。
    /// [`PlanSearchBinding::with_where_filters`] へそのまま渡す入口
    /// （Issue #945・NOSQL-14）。
    pub fn filters(&self) -> &engine::sql::declarative_predicate::BoundWhereFilters {
        &self.filters
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// `USING PLAN('<query>')` に渡す原質問文字列。sanitize は engine 側
    /// （`query_planner.rs`）の既存契約に委ねる。
    pub fn question(&self) -> &str {
        &self.question
    }

    /// 未解決のクエリ句モード指定（`search.mode`）。`None` は指定なし。
    pub fn query_mode(&self) -> Option<SearchMode> {
        self.query_mode
    }

    pub fn explain(&self) -> bool {
        self.explain
    }
}

/// `search` op の束縛結果。`vector` 指定は SQL の `bind_in_session` と
/// 同一形の [`BoundStatement`]、`plan` 指定は束縛済み部品のみの
/// [`PlanSearch`]（完成は #764）。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundSearch {
    Vector(BoundStatement),
    Plan(PlanSearch),
}

/// [`bind_search`] の失敗を表す。いずれも [`ClassifiedError`] を実装し
/// `wire_code`／`client_message` を経由して HTTP エラー応答へ射影される。
#[derive(Debug, Clone)]
pub enum SearchError {
    /// [`super::schema::Validated`] アクセサの型・キー不一致（`42601`）。
    Shape(SchemaError),
    /// `vector` と `plan` が両方指定されている（SQL の `ORDER BY`／
    /// `USING PLAN` が構文上相互排他なのと対応。`42601`）。
    VectorAndPlanBothPresent,
    /// `vector`・`plan` のいずれも指定されていない（`search` はいずれか
    /// 必須。`42601`）。
    VectorAndPlanBothMissing,
    /// `plan` と `hybrid` が同時に指定されている（SQL-5: `USING PLAN` は
    /// `ORDER BY` と相互排他であり、`hybrid` は `ORDER BY HYBRID(...)` 形の
    /// 一部のため両立しない。`42601`）。
    PlanWithHybrid,
    /// `columns: []`（空配列）。SQL 表層に等価形（空の SELECT リスト）が
    /// 存在しないため fail-closed に拒否する（`42601`）。
    EmptyColumns,
    /// `table`／`columns` の要素が識別子として意味を持ちうる形状
    /// （[`super::ident::check_identifier`]）を満たさない。SQL 表層の
    /// 字句解析段階の拒否と同じ `42601` 分類へ揃える（`aggregate.rs`
    /// `AggregateError::InvalidIdentifier` と同じ判断）。
    InvalidIdentifier,
    /// `filter` 配列の写像・束縛エラー（[`FilterError`] をそのまま透過）。
    Filter(FilterError),
    /// engine の束縛ヘルパー（投影・ベクトル値・本文列・`LIMIT`・`mode`）・
    /// [`EngineCore::execute_bound_search_in_session`]／[`EngineCore::
    /// execute_bound_plan_search_in_session`] の実行エラーをそのまま透過する
    /// （`22000`／`54000`／`42P01`／`XX000` 等）。
    Bind(SqlSurfaceError),
    /// `vector` 指定に `explain: true` を伴う要求（`42601`）。通常は
    /// [`super::gate::handle`] が [`super::explain::execute`]（Issue #948・
    /// NOSQL-16・SQL-27）へ振り分けるため到達しない。ゲートを迂回した場合の
    /// 多層防御としてのみ残る。
    ExplainRequiresPlan,
    /// `plan` 指定に `explain: true` を伴う要求（`0A000`）。通常は
    /// [`super::gate::handle`] が [`super::explain::execute`]（NOSQL-10・
    /// Issue #765）へ振り分けるため到達しない。ゲートを迂回した場合の
    /// 多層防御としてのみ残る。
    ExplainNotSupported,
}

impl From<SchemaError> for SearchError {
    fn from(err: SchemaError) -> Self {
        SearchError::Shape(err)
    }
}

impl From<FilterError> for SearchError {
    fn from(err: FilterError) -> Self {
        SearchError::Filter(err)
    }
}

impl From<InvalidIdentifier> for SearchError {
    fn from(_err: InvalidIdentifier) -> Self {
        SearchError::InvalidIdentifier
    }
}

impl From<SqlSurfaceError> for SearchError {
    fn from(err: SqlSurfaceError) -> Self {
        SearchError::Bind(err)
    }
}

impl ClassifiedError for SearchError {
    fn error_class(&self) -> ErrorClass {
        match self {
            SearchError::Shape(err) => err.error_class(),
            SearchError::VectorAndPlanBothPresent
            | SearchError::VectorAndPlanBothMissing
            | SearchError::PlanWithHybrid
            | SearchError::EmptyColumns
            | SearchError::InvalidIdentifier
            | SearchError::ExplainRequiresPlan => ErrorClass::UnsupportedSqlSyntax,
            SearchError::ExplainNotSupported => ErrorClass::FeatureNotSupported,
            SearchError::Filter(err) => err.error_class(),
            SearchError::Bind(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            SearchError::Shape(err) => err.client_message(),
            SearchError::VectorAndPlanBothPresent => {
                "search request must not specify both \"vector\" and \"plan\"".to_string()
            }
            SearchError::VectorAndPlanBothMissing => {
                "search request must specify exactly one of \"vector\" or \"plan\"".to_string()
            }
            SearchError::PlanWithHybrid => {
                "search request must not combine \"plan\" with \"hybrid\"".to_string()
            }
            SearchError::EmptyColumns => {
                "search request \"columns\" must not be an empty array".to_string()
            }
            SearchError::InvalidIdentifier => "invalid identifier".to_string(),
            SearchError::ExplainRequiresPlan => {
                "explain is not supported for a vector search".to_string()
            }
            SearchError::ExplainNotSupported => EXPLAIN_NOT_YET_SUPPORTED_MESSAGE.to_string(),
            SearchError::Filter(err) => err.client_message(),
            SearchError::Bind(err) => err.client_message(),
        }
    }
}

/// `vector`／`plan` の排他判定（[`bind_search`]）が、判定と同時に以降の
/// 分岐が使う値を確定するための内部区分。受信データ経路で
/// `Option::expect` により「到達しないはずの分岐」を後段に残さないための
/// 構造（`.claude/rules/coding-rust.md`「受信データ経路では `unwrap`/
/// `expect`... を禁止する」）。
enum VectorOrPlan<'a> {
    Vector(&'a [JsonValue]),
    Plan(&'a str),
}

/// `columns` フィールド（`Option<&[JsonValue]>`。要素はスキーマ検証済みの
/// `String` のはずだが多層防御として再検査する）を `Vec<String>` へ写像する。
fn columns_as_strings(items: &[JsonValue]) -> Result<Vec<String>, SearchError> {
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        match item {
            JsonValue::String(s) => {
                ident::check_identifier(s)?;
                names.push(s.clone())
            }
            _ => {
                return Err(SearchError::Shape(SchemaError::TypeMismatch {
                    key: "columns",
                }))
            }
        }
    }
    Ok(names)
}

/// `vector` フィールド（`&[JsonValue]`。要素はスキーマ検証済みの `Number`
/// のはずだが多層防御として再検査する）を `Vec<f32>` へ写像する。
///
/// [`engine::json::JsonNumber::as_f32`] を使う（`str -> f64 -> f32` の 2 回
/// 丸めになる `as_f64() as f32` ではなく、保持した生リテラル文字列を SQL 表層
/// `sql::parser::parse_vector_literal` と同一の `str -> f32` 単一丸めで変換
/// する。`insert.rs::bind_row` の VECTOR 列要素判定と同じ理由——表層横断で
/// 同一リテラルが同一の `f32` になることを保証するため。Issue #771 レビュー
/// 指摘対応）。非有限（`as_f32` が `None` を返す）は本関数の時点で
/// `SearchError::Bind` へ拒否し、`bind_vector_values`（呼び出し元）側でも
/// 多層防御として再検査される。
fn vector_as_f32(items: &[JsonValue]) -> Result<Vec<f32>, SearchError> {
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        match item {
            JsonValue::Number(n) => match n.as_f32() {
                Some(v) => values.push(v),
                None => {
                    // `SqlSurfaceError::invalid_input` コンストラクタは
                    // `pub(crate)`（engine クレート内限定）のため、wire-server
                    // からは列挙子のフィールドを直接構築する（`insert.rs::
                    // invalid_input_error` と同じ判断）。
                    return Err(SearchError::Bind(SqlSurfaceError::InvalidInput {
                        detail: "vector element must be finite (NaN/Inf are not allowed)"
                            .to_string(),
                    }));
                }
            },
            _ => {
                return Err(SearchError::Shape(SchemaError::TypeMismatch {
                    key: "vector",
                }))
            }
        }
    }
    Ok(values)
}

/// `SEARCH_SCHEMA` 検証済みの `validated`（[`super::gate::handle`] の手順 4
/// を通過した要求）を `schema`（呼び出し元が同一 `read_txn` 内で
/// `Storage::get_table_schema` 等から供給する）へ束縛する。
///
/// 処理順序（fail-closed。順序自体が契約の一部）:
/// 1. `table`・`limit`・`explain` を読み取る（`limit` は
///    [`validate_search_limit`] で範囲検証、`table` は
///    [`super::ident::check_identifier`] で識別子形状を検証）。
/// 2. `vector`／`plan` の排他判定（4 ケース）。
/// 3. `plan` かつ `hybrid` 同時指定を拒否。
/// 4. `columns` を投影へ束縛（`Some([])` は拒否。各要素は
///    [`super::ident::check_identifier`] で識別子形状を検証してから使う）。
/// 5. `filter` を束縛。
/// 6. `mode` を検証（値は分岐によって確定／未解決のまま保持）。
/// 7. `vector`／`plan` いずれかへ分岐して最終形を組み立てる。
pub fn bind_search(
    validated: &Validated<'_>,
    schema: &TableSchema,
    udfs: &engine::sql::udf_call::UdfRegistry,
) -> Result<BoundSearch, SearchError> {
    let table = validated.required_str("table")?;
    ident::check_identifier(table)?;
    let limit_raw = validated.required_u32("limit")?;
    let limit = validate_search_limit(limit_raw)?;
    let explain = validated.optional_bool("explain")?.unwrap_or(false);

    let vector_items = validated.optional_array("vector")?;
    let plan = validated.optional_str("plan")?;
    let hybrid = validated.optional_object("hybrid")?;

    // 排他判定と同時に、以降の分岐が使う値をここで確定する（`.expect()` で
    // 「到達しないはず」の分岐を後段に残さない。受信データ経路では
    // `unwrap`/`expect` を使わない方針 `.claude/rules/coding-rust.md`）。
    let vector_or_plan = match (vector_items, plan) {
        (Some(items), None) => VectorOrPlan::Vector(items),
        (None, Some(question)) => VectorOrPlan::Plan(question),
        (Some(_), Some(_)) => return Err(SearchError::VectorAndPlanBothPresent),
        (None, None) => return Err(SearchError::VectorAndPlanBothMissing),
    };
    if plan.is_some() && hybrid.is_some() {
        return Err(SearchError::PlanWithHybrid);
    }

    let columns = match validated.optional_array("columns")? {
        None => None,
        Some([]) => return Err(SearchError::EmptyColumns),
        Some(items) => Some(columns_as_strings(items)?),
    };
    let projection = bind_column_projection(columns.as_deref(), schema)?;

    let filter_items = validated.optional_array("filter")?.unwrap_or(&[]);
    let filters = bind_filter(filter_items, schema, udfs)?;

    // `mode` は `table`／`columns` と同じ識別子形状検査（`ident::
    // check_identifier`）を先に通す（cursor[bot] 指摘・PR #820。`"recall"`／
    // `"precision"` はいずれもこの形状に収まるため受理側の挙動は不変。
    // `SearchMode::parse_literal` のエラーメッセージは不一致値をそのまま
    // 埋め込むため、事前に長さ上限（`MAX_IDENTIFIER_LEN`）・NUL／制御文字・
    // 非 ASCII を弾いておくことで、巨大な値や NUL を含む値がエラー応答本文の
    // 肥大化や `error_response::encode` の制御文字拒否による `XX000` への
    // 縮退を引き起こさないようにする）。
    let query_mode = match validated.optional_str("mode")? {
        Some(literal) => {
            ident::check_identifier(literal)?;
            Some(SearchMode::parse_literal(literal)?)
        }
        None => None,
    };

    if let VectorOrPlan::Vector(vector_items) = vector_or_plan {
        let values = vector_as_f32(vector_items)?;
        let query = bind_vector_values(&values, schema)?;
        let ranking = match hybrid {
            Some(hybrid_obj) => {
                let text_column_index = bind_body_text_column(schema)?;
                let query_text = match hybrid_obj.get("text") {
                    Some(JsonValue::String(s)) => s.clone(),
                    // `HYBRID_SCHEMA` が既に `text: Required String` を検証
                    // 済みだが、多層防御として再検査する（`filter.rs`
                    // `map_filter_item` と同じ判断）。
                    _ => {
                        return Err(SearchError::Shape(SchemaError::TypeMismatch {
                            key: "hybrid",
                        }))
                    }
                };
                Ranking::Hybrid {
                    query,
                    text_column_index,
                    query_text,
                }
            }
            None => Ranking::Distance { query },
        };
        let resolved_mode = mode::resolve_mode(query_mode, None);
        let (metadata_filters, expr_filters, or_filters) = filters.into_parts();
        let bound = BoundStatement::new(
            table.to_string(),
            projection,
            metadata_filters,
            false,
            ranking,
            limit,
            EvaluationOrder::DEFAULT,
        )
        .with_mode(resolved_mode)
        .with_where_filters(expr_filters, or_filters);
        return Ok(BoundSearch::Vector(bound));
    }

    // ここへ到達するのは `vector_or_plan` が `Plan` の場合のみ（直上の
    // `if let` が `Vector` を早期 return 済み）。
    let VectorOrPlan::Plan(question) = vector_or_plan else {
        return Err(SearchError::VectorAndPlanBothMissing);
    };
    validate_using_plan_question(question)?;
    // `USING PLAN`（SQL-5）は `VECTOR` 列必須・本文列必須の契約を束縛時に
    // 確認する（`sql::using_plan::pre_check_bindable`／`bind_expansion` と
    // 同じ判断。`VECTOR` 列なしテーブルへの `plan` 受理をここで塞がないと、
    // 失敗が本来のバインド契約より後——プラン検索の完了・実行時点、#764 で
    // LLM I/O が追加された後は高価な LLM 呼び出しの後——まで遅延する）。
    require_vector_column(schema)?;
    bind_body_text_column(schema)?;

    Ok(BoundSearch::Plan(PlanSearch {
        table: table.to_string(),
        projection,
        filters,
        limit,
        question: question.to_string(),
        query_mode,
        explain,
    }))
}

/// `explain: true` を伴う `plan` 検索に返す固定文言（[`super::gate::
/// PLACEHOLDER_MESSAGE`]・`aggregate.rs::EXPLAIN_NOT_YET_SUPPORTED_MESSAGE`
/// と同型の「未実装」扱い。NOSQL-10・Issue #765 の担当）。
pub const EXPLAIN_NOT_YET_SUPPORTED_MESSAGE: &str = "search explain is not yet available";

/// binder closure（`Fn(...) -> Result<_, SqlSurfaceError>`）の戻り値型に
/// [`SearchError`] をそのまま渡せないため、`Bind`（engine 側の分類をそのまま
/// 持つ）は中身の [`SqlSurfaceError`] をそのまま使う。`Filter` は
/// [`FilterError::into_sql_surface_error`]（NOSQL-17。Issue #896。`Bind` 以外の
/// 分類〔`54000`／`42601`／`22P02` 等〕も保ったまま `SqlSurfaceError` へ写像
/// する単一の変換点）へ委譲する。それ以外（形状・排他判定・識別子形状等）は
/// いずれも `SqlSurfaceError::UnsupportedSyntax` 自身と同じ分類（`42601`）の
/// ため、その variant として復元してよい（`aggregate.rs::to_sql_surface_error`
/// と同じ判断。[`EngineCore::execute_bound_search_in_session`]／[`EngineCore::
/// execute_bound_plan_search_in_session`] は binder のエラーをそのまま
/// 呼び出し元へ返す契約のため、[`execute`] 側の `From<SqlSurfaceError> for
/// SearchError` により最終的な `wire_code`／`client_message` はここでの分類
/// のまま保たれる）。
///
/// `FilterError::CompositeEqNotSupportedForPredicateDml` 等の `0A000` 分類は
/// `SqlSurfaceError::FeatureNotSupported` としてこの closure 境界をそのまま通過する
/// （`docs/design/nosql-typed-json-binding.md` 参照）。
fn to_sql_surface_error(err: SearchError) -> SqlSurfaceError {
    match err {
        SearchError::Bind(inner) => inner,
        SearchError::Filter(filter_err) => filter_err.into_sql_surface_error(),
        other => SqlSurfaceError::UnsupportedSyntax {
            detail: other.client_message(),
        },
    }
}

/// `vector` 指定の `search` op を [`engine::sql::parser::BoundStatement`] へ
/// 束縛する binder closure 本体（Issue #948・NOSQL-16。`bind_search` の完全な
/// 束縛結果から `BoundSearch::Vector` のみを取り出す）。[`execute`]（通常
/// 実行。[`EngineCore::execute_bound_search_in_session`] へ渡す）と
/// [`super::explain::execute`]（`explain: true`。[`EngineCore::
/// explain_bound_search_in_session`] へ渡す）の両方が同一の closure を共有
/// することで、`42P01`（未知テーブル）が排他判定・型不整合より先に確定する
/// 等のエラー分類・優先順位が両経路でビット同一になる（第 2 の binder を
/// 作らない設計）。
pub(crate) fn bind_vector_statement(
    validated: &Validated<'_>,
    schema: &TableSchema,
    udfs: &engine::sql::udf_call::UdfRegistry,
) -> Result<BoundStatement, SqlSurfaceError> {
    match bind_search(validated, schema, udfs).map_err(to_sql_surface_error)? {
        BoundSearch::Vector(stmt) => Ok(stmt),
        // `execute`（通常実行）側の対称コメント参照: `explain::execute` は
        // `vector` 指定であることを呼び出し前に確定させているため、この
        // 分岐が実際に到達することはない。受信データ経路での `unwrap`/
        // `expect` を避けつつ fail-closed に拒否する
        // （`.claude/rules/coding-rust.md`）。
        BoundSearch::Plan(_) => Err(SqlSurfaceError::Internal {
            detail: "search binder returned a plan form for a vector request".to_string(),
        }),
    }
}

/// `validated`（`search` op のスキーマ検証済み要求本文）を `engine` 上で
/// 実行する。`principal` の [`SessionPrincipal::policy_context`] のみから
/// RLS 境界（テナント）を導出する（RLS-7・本モジュールはテナント判定を
/// 一切行わない）。`explain: true` の拒否（多層防御。モジュール doc 参照）
/// もここで行う。
///
/// `vector`・`plan` の両方指定（排他違反）は、スキーマ解決を要さない
/// 構造的な契約違反として本関数の先頭で確定させ（[`SearchError::
/// VectorAndPlanBothPresent`]）、以降のディスパッチには進まない。よって
/// 以下のディスパッチ先（[`EngineCore::execute_bound_search_in_session`]／
/// [`EngineCore::execute_bound_plan_search_in_session`]）の呼び分けが実際に
/// 見るのは「ちょうど一方だけ指定」／「両方欠落」の 3 ケースで、これも
/// スキーマに依存しない `plan` フィールドの有無のみで決める（`plan` あり
/// なら `Plan` 側エントリ、`plan` なしなら `Vector` 側エントリを呼べば、
/// 両方欠落の場合も含め `bind_search` 自身が正しい分岐・エラーを返す）。
/// `bind_search` は schema 依存の束縛を担うため、実際の呼び分け確定は各
/// エントリの binder closure 内で行う。
pub fn execute(
    engine: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<engine::sql::exec::QueryResult, SearchError> {
    let table = validated.required_str("table")?;
    ident::check_identifier(table)?;

    let vector_present = validated.optional_array("vector")?.is_some();
    let plan_present = validated.optional_str("plan")?.is_some();

    // `vector`・`plan` の両方指定はスキーマ解決（テーブルの存在確認）を
    // 一切要さない、要求本文自身が抱える構造的な契約違反であるため、他の
    // どの検証よりも先にここで確定させる（codex-review P1 指摘・PR #827）。
    // 修正前は下の `plan_present` 分岐内でローカルに行う
    // `validate_using_plan_question`・`mode` 解析（いずれも engine 呼び出し
    // 前のローカル処理）が、`bind_search` の排他判定（engine 呼び出し内の
    // closure 経由。テーブル解決後に実行される）より手前に位置していたため、
    // `vector` も同時に指定された要求では排他違反（`42601`）より先に
    // `plan` の長さ超過（`54000`）や `mode` の解析失敗（`22000`）が
    // 返ってしまっていた——つまり修正前もこの組合せでは「テーブル解決が
    // 排他判定に先行する」優先順位は元々保証されていなかった（ローカル
    // 検証が先に確定するため）。本行はこの構造をそのまま踏襲し、両方指定
    // という契約違反自体をテーブル解決より前に確定させる。未知テーブル単体
    // （`vector`／`plan` のどちらか一方だけを指定）に対する `42P01` 優先
    // （`undefined_table_rejects_with_42p01_before_exclusivity_check`）は
    // この判定が `vector_present && plan_present` のときにしか発火しない
    // ため変わらない。
    if vector_present && plan_present {
        return Err(SearchError::VectorAndPlanBothPresent);
    }

    // `explain: true` の拒否は `vector`／`plan` のどちらか一方だけが指定
    // された「（排他契約上）有効な形の要求」に限って行う（cursor[bot]
    // 指摘・PR #827）。両方指定は上の排他判定で既に確定済みのためここへは
    // 到達しない。両方欠落のときにここで `explain` 専用の分類（`plan`
    // 指定時 `0A000`・`vector` 指定時 `42601`）へ先回りして拒否すると、
    // 未知テーブルが排他判定より先に `42P01` で拒否される既存の優先順位
    // 契約（`undefined_table_rejects_with_42p01_before_exclusivity_check`。
    // 本関数はテーブル存在確認の前にここへ到達するため、`table` なしで
    // 判定してしまうと `42P01` より先に応答が確定してしまう）を壊す。
    // `vector_present != plan_present`（ちょうど一方だけ true。上の早期
    // 判定により両方 true は既に排除済みなので実質 `||` と等価）のときのみ
    // 早期拒否し、それ以外（いずれも false）は通常のディスパッチへ進めて
    // `bind_search` 自身のテーブル存在確認の優先順位に委ねる（両方欠落かつ
    // `explain: true` の場合も、テーブルが存在すれば最終的に `bind_search`
    // が `VectorAndPlanBothMissing`＝`42601` を返すため、旧来の
    // `ExplainRequiresPlan`＝`42601` と wire_code は変わらない）。
    if validated.optional_bool("explain")?.unwrap_or(false) && vector_present != plan_present {
        return Err(if plan_present {
            SearchError::ExplainNotSupported
        } else {
            SearchError::ExplainRequiresPlan
        });
    }

    let session = SessionState::default();
    let ctx = principal.policy_context();

    if plan_present {
        let question = validated.required_str("plan")?;
        validate_using_plan_question(question)?;
        let limit_raw = validated.required_u32("limit")?;
        // `EngineCore::execute_bound_plan_search_in_session` は範囲検証前の
        // 生 `u32` を受け取り、内部で `validate_search_limit` を通す（多層
        // 防御としてここでも一度通し、範囲外を engine 呼び出し前に拒否する。
        // `bind_search`〔schema 依存の束縛〕側でも同じ検証が再度行われる）。
        validate_search_limit(limit_raw)?;
        // `mode` の識別子形状検査・`SearchMode::parse_literal` はここでは
        // 行わない（cursor[bot] 指摘対応・PR #827）。以前はここで先に解析
        // していたため、テーブル未存在＋ `mode` 値不正の要求で `42P01` より
        // 先に `22000` が確定してしまっていた（`vector` 指定検索は
        // `bind_search` が同じ解析をテーブル解決後〔`execute_bound_search_
        // in_session` の `read_txn_with_schema` 後〕に行うため、この問題が
        // 無かった）。生リテラルをそのまま渡し、
        // `EngineCore::run_using_plan_select` がテーブル解決後に初めて
        // 解析することで `vector` 指定検索と同一の fail-closed 順序を保つ。
        let mode_literal = validated.optional_str("mode")?;
        let result = engine.execute_bound_plan_search_in_session(
            ctx,
            &session,
            table,
            question,
            mode_literal,
            limit_raw,
            |schema, udfs| match bind_search(validated, schema, udfs)
                .map_err(to_sql_surface_error)?
            {
                BoundSearch::Plan(plan) => {
                    let (metadata_filters, expr_filters, or_filters) =
                        plan.filters().clone().into_parts();
                    Ok(
                        PlanSearchBinding::new(plan.projection().to_vec(), metadata_filters)
                            .with_where_filters(expr_filters, or_filters),
                    )
                }
                // `plan_present` が `true` の間は `bind_search` が
                // `BoundSearch::Vector` を返すことはない（両者は同一の
                // `validated` を見て同じ排他判定を行うため）。到達した場合は
                // 受信データ経路での `unwrap`/`expect` を避けつつ fail-closed
                // に拒否する（`.claude/rules/coding-rust.md`）。
                BoundSearch::Vector(_) => Err(SqlSurfaceError::Internal {
                    detail: "search binder returned a vector form for a plan request".to_string(),
                }),
            },
        )?;
        Ok(result)
    } else {
        let result =
            engine.execute_bound_search_in_session(ctx, &session, table, |schema, udfs| {
                bind_vector_statement(validated, schema, udfs)
            })?;
        Ok(result)
    }
}

/// `POST /v1/query`（`op: "search"`）を処理し応答バイト列を返す
/// （[`super::gate::handle`] から呼ばれる。認証・スキーマ検証済みの要求の
/// み）。`scan.rs::handle`／`aggregate.rs::handle` と同型: 成功時は
/// [`super::response::encode`] を経て `200 OK`、失敗時は [`SearchError::
/// error_class`]／[`SearchError::client_message`] をエラー応答へ写像する。
pub fn handle(
    engine: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: SystemTime,
) -> Vec<u8> {
    match execute(engine, principal, validated) {
        Ok(result) => match super::response::encode(&result) {
            Ok(body) => http_response::encode_ok(&body, now_wall),
            Err(err) => {
                http_response::encode_error(err.error_class(), &err.client_message(), now_wall)
            }
        },
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ColumnDef, ColumnType};
    use engine::json::{parse_json, JsonNumber};

    use crate::http::query::schema::SEARCH_SCHEMA;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    fn validate(text: &str) -> JsonValue {
        parse_json(text).expect("valid JSON fixture")
    }

    fn udfs() -> engine::sql::udf_call::UdfRegistry {
        engine::sql::udf_call::UdfRegistry::default()
    }

    fn bind(text: &str) -> Result<BoundSearch, SearchError> {
        let value = validate(text);
        let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
        bind_search(&validated, &schema(), &udfs())
    }

    #[test]
    fn vector_only_binds_to_ranking_distance() {
        let bound = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10}"#)
            .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        assert!(matches!(stmt.ranking(), Ranking::Distance { .. }));
        assert_eq!(stmt.limit(), 10);
        assert!(!stmt.rls_predicate_present());
        assert_eq!(stmt.evaluation_order(), EvaluationOrder::DEFAULT);
    }

    // 表層横断の丸め一貫性（Issue #771 レビュー指摘対応）: `vector` 要素の
    // JSON 数値リテラルは SQL 表層 `parse_vector_literal`（`str -> f32` 単一
    // 丸め）と同一のビットパターンへ変換される。`1.0000000596046448` は
    // `str -> f64 -> f32` の 2 回丸め経路だと `1.0` になる値
    // （`engine::json` の `as_f32_matches_direct_str_parse_and_differs_from_f64_roundtrip`
    // と同じ数値）。
    #[test]
    fn vector_element_matches_sql_surface_single_rounding() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[1.0000000596046448,0.2,0.3,0.4],"limit":10}"#,
        )
        .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        let Ranking::Distance { query } = stmt.ranking() else {
            panic!("expected Ranking::Distance");
        };
        let expected =
            engine::sql::parser::parse_vector_literal("[1.0000000596046448,0.2,0.3,0.4]", 4)
                .expect("SQL literal parses");
        assert_eq!(
            query[0].to_bits(),
            expected[0].to_bits(),
            "NoSQL vector element must match SQL parse_vector_literal bit-for-bit"
        );
    }

    #[test]
    fn plan_only_is_accepted_and_returns_plan_search() {
        let bound = bind(r#"{"op":"search","table":"docs","plan":"find something","limit":5}"#)
            .expect("bind ok");
        let BoundSearch::Plan(plan) = bound else {
            panic!("expected BoundSearch::Plan");
        };
        assert_eq!(plan.question(), "find something");
        assert_eq!(plan.limit(), 5);
        assert_eq!(plan.query_mode(), None);
        assert!(!plan.explain());
    }

    #[test]
    fn vector_and_plan_both_present_is_rejected() {
        let err = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"plan":"x","limit":5}"#,
        )
        .expect_err("must reject");
        assert!(matches!(err, SearchError::VectorAndPlanBothPresent));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn vector_and_plan_both_missing_is_rejected() {
        let err = bind(r#"{"op":"search","table":"docs","limit":5}"#).expect_err("must reject");
        assert!(matches!(err, SearchError::VectorAndPlanBothMissing));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn plan_with_hybrid_is_rejected() {
        let err =
            bind(r#"{"op":"search","table":"docs","plan":"x","hybrid":{"text":"y"},"limit":5}"#)
                .expect_err("must reject");
        assert!(matches!(err, SearchError::PlanWithHybrid));
        assert_eq!(err.wire_code(), "42601");
    }

    /// `VECTOR` 列を持たないテーブル（広域取得専用。SQL-15・Issue #454）の
    /// 束縛。`plan` 経路の `VECTOR` 列存在チェック（Issue #763 PR #820
    /// Bugbot 指摘）を単体で検証するための fixture。
    fn scan_only_schema() -> TableSchema {
        TableSchema::new(
            "notes",
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    #[test]
    fn plan_on_table_without_vector_column_is_rejected_at_bind_time() {
        let value =
            validate(r#"{"op":"search","table":"notes","plan":"find something","limit":5}"#);
        let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
        let err = bind_search(&validated, &scan_only_schema(), &udfs()).expect_err("must reject");
        assert!(matches!(err, SearchError::Bind(_)));
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn empty_columns_array_is_rejected() {
        let err = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"columns":[]}"#,
        )
        .expect_err("must reject");
        assert!(matches!(err, SearchError::EmptyColumns));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_malformed_identifier_table() {
        let err = bind(r#"{"op":"search","table":"do cs","vector":[0.1,0.2,0.3,0.4],"limit":5}"#)
            .expect_err("must reject");
        assert!(matches!(err, SearchError::InvalidIdentifier));
        assert_eq!(err.wire_code(), "42601");
        // untrusted なテーブル名文字列を含まない固定文言であること。
        assert_eq!(err.client_message(), "invalid identifier");
    }

    #[test]
    fn bind_rejects_malformed_identifier_column() {
        let err = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"columns":["do cs"]}"#,
        )
        .expect_err("must reject");
        assert!(matches!(err, SearchError::InvalidIdentifier));
        assert_eq!(err.wire_code(), "42601");
        // untrusted な列名文字列を含まない固定文言であること。
        assert_eq!(err.client_message(), "invalid identifier");
    }

    #[test]
    fn columns_selects_named_projection() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"columns":["id","lang"]}"#,
        )
        .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        assert_eq!(stmt.projection().len(), 2);
        assert_eq!(stmt.projection()[0], ProjectedColumn::Id);
        assert!(
            matches!(&stmt.projection()[1], ProjectedColumn::Column { name, .. } if name == "lang")
        );
    }

    #[test]
    fn mode_recall_and_precision_are_resolved_via_query_clause() {
        for (literal, expected) in [
            ("recall", SearchMode::Recall),
            ("precision", SearchMode::Precision),
        ] {
            let bound = bind(&format!(
                r#"{{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"mode":"{literal}"}}"#
            ))
            .expect("bind ok");
            let BoundSearch::Vector(stmt) = bound else {
                panic!("expected BoundSearch::Vector");
            };
            assert_eq!(stmt.mode().mode(), expected);
            assert_eq!(stmt.mode().source(), mode::ModeSource::QueryClause);
        }
    }

    #[test]
    fn mode_absent_resolves_to_default() {
        let bound = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5}"#)
            .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        assert_eq!(stmt.mode().source(), mode::ModeSource::Default);
    }

    #[test]
    fn plan_mode_is_left_unresolved() {
        let bound =
            bind(r#"{"op":"search","table":"docs","plan":"x","limit":5,"mode":"precision"}"#)
                .expect("bind ok");
        let BoundSearch::Plan(plan) = bound else {
            panic!("expected BoundSearch::Plan");
        };
        assert_eq!(plan.query_mode(), Some(SearchMode::Precision));
    }

    #[test]
    fn mode_invalid_literal_is_rejected() {
        let err = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"mode":"fast"}"#,
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    /// `mode` に巨大な値を与えても `table`／`columns` と同じ識別子形状検査
    /// （`ident::check_identifier`）で長さ上限超過として拒否され、
    /// `SearchMode::parse_literal` のエラーメッセージ（不一致値をそのまま
    /// 埋め込む）へは到達しないことを固定する（cursor[bot] 指摘・PR #820）。
    #[test]
    fn mode_oversized_literal_is_rejected_before_parse_literal() {
        let oversized = "a".repeat(10_000);
        let err = bind(&format!(
            r#"{{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"mode":"{oversized}"}}"#
        ))
        .expect_err("must reject");
        assert!(matches!(err, SearchError::InvalidIdentifier));
        assert_eq!(err.wire_code(), "42601");
        // untrusted な mode 文字列を含まない固定文言であること（エラー応答
        // 本文の肥大化を防ぐ）。
        assert_eq!(err.client_message(), "invalid identifier");
    }

    /// `mode` に NUL バイトを含む値を与えても識別子形状検査で拒否され、
    /// untrusted な生文字列がエラー文言へ埋め込まれないことを固定する
    /// （`error_response::encode` の制御文字拒否による `XX000` への縮退を
    /// 未然に防ぐ）。`engine::json::parse_json` は `\u0000` エスケープ経由の
    /// 制御文字も拒否するため、JSON テキストを経由せず `JsonValue` を直接
    /// 組み立てて schema 検証以降の経路だけを検証する。
    #[test]
    fn mode_literal_with_nul_byte_is_rejected() {
        let mut object = std::collections::BTreeMap::new();
        object.insert("op".to_string(), JsonValue::String("search".to_string()));
        object.insert("table".to_string(), JsonValue::String("docs".to_string()));
        object.insert(
            "vector".to_string(),
            JsonValue::Array(vec![
                JsonValue::Number(JsonNumber::Float {
                    value: 0.1,
                    text: Box::from("0.1"),
                }),
                JsonValue::Number(JsonNumber::Float {
                    value: 0.2,
                    text: Box::from("0.2"),
                }),
                JsonValue::Number(JsonNumber::Float {
                    value: 0.3,
                    text: Box::from("0.3"),
                }),
                JsonValue::Number(JsonNumber::Float {
                    value: 0.4,
                    text: Box::from("0.4"),
                }),
            ]),
        );
        object.insert(
            "limit".to_string(),
            JsonValue::Number(JsonNumber::PosInt(5)),
        );
        object.insert(
            "mode".to_string(),
            JsonValue::String("re\u{0}call".to_string()),
        );
        let value = JsonValue::Object(object);
        let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
        let err = bind_search(&validated, &schema(), &udfs()).expect_err("must reject");
        assert!(matches!(err, SearchError::InvalidIdentifier));
        assert_eq!(err.wire_code(), "42601");
        assert_eq!(err.client_message(), "invalid identifier");
    }

    #[test]
    fn hybrid_binds_to_ranking_hybrid_with_body_column() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"hybrid":{"text":"hello world"}}"#,
        )
        .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        let Ranking::Hybrid {
            text_column_index,
            query_text,
            ..
        } = stmt.ranking()
        else {
            panic!("expected Ranking::Hybrid");
        };
        assert_eq!(schema().columns[*text_column_index].name, "body");
        assert_eq!(query_text, "hello world");
    }

    #[test]
    fn hybrid_missing_body_column_is_rejected() {
        let schema_without_body = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        );
        let value = validate(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"hybrid":{"text":"x"}}"#,
        );
        let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
        let err = bind_search(&validated, &schema_without_body, &udfs()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn vector_dimension_mismatch_is_rejected() {
        let err = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2],"limit":5}"#)
            .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn limit_zero_is_rejected() {
        let err = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":0}"#)
            .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn limit_over_max_search_k_is_rejected() {
        // `engine::core::MAX_SEARCH_K` は `pub(crate)`（engine クレート内部）
        // のため、wire-server 側のテストからは直接参照できない。SQL 表層の
        // 既存テスト（例: `wire_scan.rs` の `LIMIT 10000`）と同じ 10_000 を
        // 実測値として直接ハードコードする（値は
        // `docs/spec/04-behavior/sql-surface.md` 由来ではなく本リポ実装の
        // 既定値。CLAUDE.md ステータス欄「TASK-166」等の記載でも公開済み）。
        let text = r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10001}"#;
        let err = bind(text).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn limit_fractional_is_rejected_at_schema_layer() {
        let err = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":1.5}"#)
            .expect_err("must reject");
        assert!(matches!(err, SearchError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn limit_negative_is_rejected_at_schema_layer() {
        let err = bind(r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":-1}"#)
            .expect_err("must reject");
        assert!(matches!(err, SearchError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn plan_oversized_question_is_rejected() {
        let huge = "x".repeat(64 * 1024 + 1);
        let text = format!(r#"{{"op":"search","table":"docs","plan":"{huge}","limit":5}}"#);
        let err = bind(&text).expect_err("must reject");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn filter_is_bound_for_vector_search() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        )
        .expect("bind ok");
        let BoundSearch::Vector(stmt) = bound else {
            panic!("expected BoundSearch::Vector");
        };
        let expected = engine::declarative_filter::bind_all(
            &[engine::declarative_filter::DeclarativeFilter::equals(
                "lang", "ja",
            )],
            &schema(),
        )
        .expect("bind ok");
        assert_eq!(stmt.metadata_filters(), expected.as_slice());
    }

    #[test]
    fn filter_is_bound_for_plan_search() {
        let bound = bind(
            r#"{"op":"search","table":"docs","plan":"x","limit":5,"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
        )
        .expect("bind ok");
        let BoundSearch::Plan(plan) = bound else {
            panic!("expected BoundSearch::Plan");
        };
        let expected = engine::declarative_filter::bind_all(
            &[engine::declarative_filter::DeclarativeFilter::equals(
                "lang", "ja",
            )],
            &schema(),
        )
        .expect("bind ok");
        assert_eq!(plan.metadata_filters(), expected.as_slice());
    }

    #[test]
    fn explain_flag_is_preserved() {
        let bound = bind(
            r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"explain":true}"#,
        )
        .expect("bind ok");
        // `explain` はプランナー実行系（#764）・`explain` op（#765）が使う
        // ため `BoundStatement` 自体には保持しない。`Vector` 経路では
        // 「拒否されないこと」だけを固定する（`Plan` 経路は `PlanSearch`
        // へ保持する。下の `plan_explain_flag_is_preserved` 参照）。
        assert!(matches!(bound, BoundSearch::Vector(_)));

        let plan_bound =
            bind(r#"{"op":"search","table":"docs","plan":"x","limit":5,"explain":true}"#)
                .expect("bind ok");
        let BoundSearch::Plan(plan) = plan_bound else {
            panic!("expected BoundSearch::Plan");
        };
        assert!(plan.explain());
    }
}
