//! `POST /v1/query`（`op: aggregate`）を SQL 表層の集計実行計画（TASK-166・
//! SQL-13・TASK-167・SQL-14）へ**SQL テキストを経由せずに**写像し実行する
//! モジュール（Issue #768・#769。対象ビヘイビア TASK-177・NOSQL-4・NOSQL-5。
//! ポインタ: `docs/spec/05-tasks.md` TASK-177・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-4・NOSQL-5・
//! `docs/spec/04-behavior/sql-surface.md` SQL-13・SQL-14）。
//!
//! 責務境界: JSON → SQL 文字列の組み立ては行わない（`.claude/rules/
//! coding-rust.md`「SQL / プラン文字列の組み立てに未検証入力を連結しない」・
//! spec「第 2 の実行器を作らない」方針）。`engine::sql::parser::
//! BoundAggregateItem::bind`／`BoundAggregate::new`／`BoundAggregate::
//! new_grouped`（Issue #768・#769・TASK-186・NOSQL-4・NOSQL-5 で公開）を
//! 直接呼び、列名解決・型不整合判定（`resolve_aggregate_input`・
//! `resolve_group_by_column`・`check_having_target_is_numeric`）を SQL テキスト
//! 経由の `bind_aggregate` と完全に共有する。
//!
//! `explain: true` は SQL-6 の「`EXPLAIN` は `USING PLAN` 付き検索 `SELECT`
//! 専用」契約の写像として `42601`（[`AggregateError::ExplainNotSupported`]）
//! で拒否し実行しない（黙って無視すると `explain` なしの通常実行へ fail-open
//! に縮退してしまうため。Issue #765・NOSQL-10）。`group_by`／`having` は
//! Issue #769 で SQL-14（`sql::group_by::execute_grouped_aggregate`）へ
//! 写像する。
//!
//! `table`／`aggregates[].column`／`group_by[0]`／`having[].column`
//! （`*` を除く）は [`super::ident::check_identifier`] で識別子形状を検査
//! してから使う（SQL 表層の字句解析段階の拒否と同じ `42601` 分類。untrusted
//! 文字列は echo しない）。`filter` 配列は [`super::filter::bind_filter`]
//! （Issue #761・NOSQL-7）へ委譲する。
//!
//! `group_by`／`having` の応答コード確定（Issue #769）:
//! - グループ数上限（`engine::sql::group_by::MAX_GROUPS`）・グループキー
//!   累計バイト・TEXT 集計状態累計バイトの各予算超過 → `54000`
//!   （[`engine::sql::allowlist::SqlSurfaceError::payload_too_large`] 経由）
//! - `having` 述語数上限超過 → `54000`
//!   （[`engine::sql::allowlist::check_having_predicate_count`]）
//! - `group_by` 要素数 ≠ 1・`having` のみ（`group_by` なし）・語彙外
//!   `op`／`fn`・非有限 `value`・識別子形状不正 → `42601`
//! - `aggregates` が空リスト → `group_by`／`having` の有無・形状によらず
//!   一律 `42601`（`having` の参照解決より必ず先に検査する。PR #824
//!   codex-review 指摘。参照解決を先に走らせると `group_by` あり経路のみ
//!   `22000`（未知の HAVING 参照）へ落ち、`group_by` なし経路（`42601`）
//!   と非対称になる）
//! - `group_by` 列が `TEXT` 列でない・`having` が `MIN`/`MAX(<TEXT 列>)` を
//!   参照・参照先が `aggregates` に存在しない／曖昧 → `22000`
//!
//! RLS: `PolicyContext` は呼び出し元が渡す
//! [`crate::http::session::middleware::SessionPrincipal::policy_context`]
//! のみから導出する（本モジュールはヘッダ・JSON からテナントを読む経路を
//! シグネチャ上持たない）。`EngineCore::execute_bound_aggregate_in_session`
//! （Issue #728）が `ctx` から RLS を暗黙適用する既存契約をそのまま使う。

use engine::catalog::TableSchema;
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::sql::allowlist::{self, SqlSurfaceError};
use engine::sql::mode::SessionState;
use engine::sql::parser::{
    AggregateTarget, BoundAggregate, BoundAggregateItem, HavingOp, HavingSpec,
};

use super::filter::{self, FilterError};
use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated, HAVING_ITEM_SCHEMA};

use crate::http::session::middleware::SessionPrincipal;

/// `explain: true` を伴う `aggregate` 要求に返す固定文言（Issue #765・
/// TASK-186・NOSQL-10。SQL-6 の `EXPLAIN` が `USING PLAN` 付き検索 `SELECT`
/// 専用で、集計 `SELECT` への `EXPLAIN` 前置を `42601` で拒否する契約
/// （`sql::allowlist`）の写像。`scan.rs::ScanError::ExplainNotSupported` と
/// 同型の判断）。
pub const EXPLAIN_NOT_SUPPORTED_MESSAGE: &str = "explain is not supported for aggregate";

/// `fn`（[`engine::sql::allowlist::AggregateFunc`]）の受理語彙。`Op::parse`
/// （Issue #759）と同じ方針で**小文字完全一致のみ**を受理する（大文字小文字
/// 読み替え・trim なし）。`engine::sql::allowlist::AggregateFunc::from_name`
/// は大文字化して比較する private ヘルパーのため、ここでは独立に判定する。
pub fn parse_function(raw: &str) -> Option<engine::sql::allowlist::AggregateFunc> {
    use engine::sql::allowlist::AggregateFunc;
    match raw {
        "count" => Some(AggregateFunc::Count),
        "sum" => Some(AggregateFunc::Sum),
        "avg" => Some(AggregateFunc::Avg),
        "min" => Some(AggregateFunc::Min),
        "max" => Some(AggregateFunc::Max),
        _ => None,
    }
}

/// `having[].op`（[`HavingOp`]）の受理語彙（TASK-186・NOSQL-5）。
/// `engine::sql::allowlist::Parser::expect_cmp_op` と同じ 5 記号の
/// **完全一致のみ**を受理する（`gt`／`GE`／空白付き等の読み替えなし。
/// [`parse_function`] と同じ方針）。
pub fn parse_having_op(raw: &str) -> Option<HavingOp> {
    match raw {
        "=" => Some(HavingOp::Eq),
        "<" => Some(HavingOp::Lt),
        "<=" => Some(HavingOp::Le),
        ">" => Some(HavingOp::Gt),
        ">=" => Some(HavingOp::Ge),
        _ => None,
    }
}

/// [`bind`]／[`execute`] の失敗を表す。いずれも [`ClassifiedError`] を実装し
/// HTTP エラー応答へ射影される。
#[derive(Debug, Clone)]
pub enum AggregateError {
    /// `aggregates` 配列要素が [`super::schema::AGGREGATE_ITEM_SCHEMA`] の形
    /// （必須キー・型）に適合しない。
    Shape(SchemaError),
    /// `fn` が `count`／`sum`／`avg`／`min`／`max` の小文字完全一致に一致しない。
    /// untrusted な `fn` 文字列は保持しない（[`super::op::UnsupportedOp`] と
    /// 同じ判断）。
    UnsupportedFunction,
    /// `table`／`aggregates[].column` が識別子として意味を持ちうる形状
    /// （[`super::ident::check_identifier`]）を満たさない。
    InvalidIdentifier,
    /// `filter` 配列の写像・束縛エラー（Issue #761・NOSQL-7）。
    Filter(FilterError),
    /// `engine::sql::parser::BoundAggregateItem::bind`／`BoundAggregate::new`／
    /// `BoundAggregate::new_grouped`／`EngineCore::
    /// execute_bound_aggregate_in_session` の束縛・実行エラー（型不整合
    /// `22000`・オーバーフロー `22003`・テーブル不存在 `42P01`・集計項目数／
    /// `HAVING` 述語数・グループ数上限超過 `54000`・`having` 参照先なし／
    /// 曖昧 `22000` 等）をそのまま透過する（TASK-186・NOSQL-5）。
    Engine(SqlSurfaceError),
    /// `explain: true` を伴う要求（SQL-6 の `EXPLAIN` が `USING PLAN` 付き
    /// 検索 `SELECT` 専用で、集計 `SELECT` への適用を拒否する契約の写像。
    /// `42601`。Issue #765・NOSQL-10）。
    ExplainNotSupported,
    /// `group_by`／`having` の形の逸脱（TASK-186・NOSQL-5）: `group_by`
    /// 要素数が 1 でない、または `having`（空配列を含む）が `group_by` なしに
    /// 単独で指定されている。黙って無視すると `GROUP BY` なしの単一行集計
    /// 〔TASK-166・SQL-13〕として fail-open に実行してしまうため拒否する。
    /// `group_by` 配列形（複数列）の受理は本 Issue の対象外（NOSQL-16 (b)。
    /// 別 Issue）。engine 側は `BoundAggregate::new_grouped_by_columns`・
    /// `allowlist::check_group_by_column_count`（SQL-25 (d)）を公開済みのため、
    /// 本モジュールはそれらへ写像するだけで拡張できる。
    GroupByShape,
    /// `having[].op` が [`parse_having_op`] の 5 記号（`=`／`<`／`<=`／`>`／
    /// `>=`）に完全一致しない。
    UnsupportedHavingOperator,
    /// `having[].value` が非有限（`NaN`／`±Infinity`）。JSON 数値リテラルは
    /// 構文上非有限を直接表現できないが、`engine::json` は `Infinity` 等の
    /// 語彙を持たないため実際には到達しにくい——多層防御として明示的に拒否
    /// する（SQL 側の数値リテラル構文が非有限値を表現できないのと同じ分類）。
    NonFiniteHavingLiteral,
}

impl From<SchemaError> for AggregateError {
    fn from(err: SchemaError) -> Self {
        AggregateError::Shape(err)
    }
}

impl From<FilterError> for AggregateError {
    fn from(err: FilterError) -> Self {
        AggregateError::Filter(err)
    }
}

impl From<SqlSurfaceError> for AggregateError {
    fn from(err: SqlSurfaceError) -> Self {
        AggregateError::Engine(err)
    }
}

impl From<InvalidIdentifier> for AggregateError {
    fn from(_err: InvalidIdentifier) -> Self {
        AggregateError::InvalidIdentifier
    }
}

impl ClassifiedError for AggregateError {
    fn error_class(&self) -> ErrorClass {
        match self {
            AggregateError::Shape(err) => err.error_class(),
            AggregateError::UnsupportedFunction
            | AggregateError::InvalidIdentifier
            | AggregateError::GroupByShape
            | AggregateError::UnsupportedHavingOperator
            | AggregateError::NonFiniteHavingLiteral => ErrorClass::UnsupportedSqlSyntax,
            AggregateError::Filter(err) => err.error_class(),
            AggregateError::Engine(err) => err.error_class(),
            AggregateError::ExplainNotSupported => ErrorClass::UnsupportedSqlSyntax,
        }
    }

    fn client_message(&self) -> String {
        match self {
            AggregateError::Shape(err) => err.client_message(),
            AggregateError::UnsupportedFunction => {
                "unsupported aggregate function (only \"count\", \"sum\", \"avg\", \"min\", \
                 \"max\" are allowed)"
                    .to_string()
            }
            AggregateError::InvalidIdentifier => "invalid identifier".to_string(),
            AggregateError::Filter(err) => err.client_message(),
            AggregateError::Engine(err) => err.client_message(),
            AggregateError::ExplainNotSupported => EXPLAIN_NOT_SUPPORTED_MESSAGE.to_string(),
            AggregateError::GroupByShape => {
                "group_by must have exactly one element, and having requires group_by".to_string()
            }
            AggregateError::UnsupportedHavingOperator => {
                "unsupported having operator (only \"=\", \"<\", \"<=\", \">\", \">=\" are \
                 allowed)"
                    .to_string()
            }
            AggregateError::NonFiniteHavingLiteral => {
                "having value must be a finite number".to_string()
            }
        }
    }
}

/// `validated`（[`super::schema::AGGREGATE_SCHEMA`] を通過済みの `aggregate`
/// 要求本文）が `explain: true` を伴うかを判定する（SQL-6 の「`EXPLAIN` は
/// `USING PLAN` 付き検索 `SELECT` 専用」契約の写像。Issue #765・NOSQL-10）。
/// 伴う場合は `Err(AggregateError::ExplainNotSupported)` を返し、呼び出し元は
/// 束縛・実行を一切行わない（黙って無視すると `explain` なしの通常実行へ
/// fail-open に縮退してしまうため）。`group_by`／`having` は [`bind`] が
/// 直接処理する（TASK-186・NOSQL-5・Issue #769）。
fn reject_explain(validated: &Validated<'_>) -> Result<(), AggregateError> {
    if validated.optional_bool("explain")? == Some(true) {
        return Err(AggregateError::ExplainNotSupported);
    }
    Ok(())
}

/// `aggregates` 配列の要素 1 件を写像した結果。`func`／`target` は
/// `having[].{fn,column}` の参照解決（[`resolve_having_item_index`]）が
/// `bound`（[`BoundAggregateItem`]）のフィールドを直接読めない
/// （`pub(crate)`）ため並行して保持する。
#[derive(Debug)]
struct ParsedAggregateItem {
    bound: BoundAggregateItem,
    func: engine::sql::allowlist::AggregateFunc,
    target: AggregateTarget,
}

/// `aggregates` 配列の要素 1 件を [`BoundAggregateItem`] へ写像する。
fn bind_item(
    item: &engine::json::JsonValue,
    schema: &TableSchema,
) -> Result<ParsedAggregateItem, AggregateError> {
    let validated = super::schema::AGGREGATE_ITEM_SCHEMA.validate(item)?;
    let raw_fn = validated.required_str("fn")?;
    let column = validated.required_str("column")?;

    let func = parse_function(raw_fn).ok_or(AggregateError::UnsupportedFunction)?;
    let target = if column == "*" {
        AggregateTarget::Star
    } else {
        ident::check_identifier(column)?;
        AggregateTarget::Column(column.to_string())
    };

    let bound = BoundAggregateItem::bind(func, target.clone(), schema)?;
    Ok(ParsedAggregateItem {
        bound,
        func,
        target,
    })
}

/// `having[].{fn,column}` が指す `aggregates` 項目の添字を `item_specs`
/// （[`bind_item`] が返した `(func, target)` の宣言順一覧）から一意に解決
/// する（TASK-186・NOSQL-5）。SQL テキスト経由の `bind_group_by_clause` に
/// おける「HAVING 対象名の一意解決」と同じ判断（一致 0 件は unknown・
/// 2 件以上は ambiguous、いずれも `22000`）。`raw_column == "*"` は
/// [`AggregateTarget::Star`] とのみ一致し得る（`AggregateTarget::Column`
/// はユーザーが `*` という列名を指定できないため——[`bind_item`] が
/// `column == "*"` を常に `Star` へ写像する）。
fn resolve_having_item_index(
    item_specs: &[(engine::sql::allowlist::AggregateFunc, AggregateTarget)],
    func: engine::sql::allowlist::AggregateFunc,
    raw_column: &str,
) -> Result<usize, AggregateError> {
    let matches: Vec<usize> = item_specs
        .iter()
        .enumerate()
        .filter(|(_, (item_func, target))| {
            *item_func == func
                && match target {
                    AggregateTarget::Star => raw_column == "*",
                    AggregateTarget::Column(name) => name == raw_column,
                }
        })
        .map(|(idx, _)| idx)
        .collect();
    match matches.as_slice() {
        [idx] => Ok(*idx),
        // `SqlSurfaceError::invalid_input`（`pub(crate)` コンストラクタ）は
        // engine クレート外から呼べないため、`InvalidInput` variant 自身を
        // 直接構築する（`SqlSurfaceError` は `#[non_exhaustive]` を持たない
        // ため、`sql::exec::to_sql_surface_error` と同じ手段が使える）。
        [] => Err(AggregateError::Engine(SqlSurfaceError::InvalidInput {
            detail: "unknown HAVING reference".to_string(),
        })),
        _ => Err(AggregateError::Engine(SqlSurfaceError::InvalidInput {
            detail: "ambiguous HAVING reference".to_string(),
        })),
    }
}

/// `having` 配列要素 1 件を [`HavingSpec`] へ写像する（TASK-186・NOSQL-5）。
/// 形（[`HAVING_ITEM_SCHEMA`]）は [`super::schema::AGGREGATE_SCHEMA`] が
/// `having` フィールドの検証時に再帰的に確認済みだが、[`bind_item`]・
/// `super::filter::map_filter_item` と同じ多層防御として本関数内でも
/// 再検証する。
fn bind_having_item(
    item: &engine::json::JsonValue,
    item_specs: &[(engine::sql::allowlist::AggregateFunc, AggregateTarget)],
) -> Result<HavingSpec, AggregateError> {
    let validated = HAVING_ITEM_SCHEMA.validate(item)?;
    let raw_fn = validated.required_str("fn")?;
    let raw_column = validated.required_str("column")?;
    let raw_op = validated.required_str("op")?;
    let value = validated.required_number("value")?;

    let func = parse_function(raw_fn).ok_or(AggregateError::UnsupportedFunction)?;
    let op = parse_having_op(raw_op).ok_or(AggregateError::UnsupportedHavingOperator)?;
    if !value.is_finite() {
        return Err(AggregateError::NonFiniteHavingLiteral);
    }
    if raw_column != "*" {
        ident::check_identifier(raw_column)?;
    }
    let item_index = resolve_having_item_index(item_specs, func, raw_column)?;

    Ok(HavingSpec {
        item_index,
        op,
        literal: value,
    })
}

/// `validated`（`aggregate` op のスキーマ検証済み要求本文）を `schema` へ
/// 束縛し、[`BoundAggregate`] を得る（TASK-186・NOSQL-4・NOSQL-5。SQL
/// テキストを一切組み立てない）。`explain: true` の拒否は
/// [`reject_explain`] を呼び出し元（[`execute`]）が先に行う契約
/// のため、ここでは繰り返さない。`group_by`（要素数 1 限定）が指定されて
/// いれば [`BoundAggregate::new_grouped`]（SQL-14）へ、指定されていなければ
/// 従来どおり [`BoundAggregate::new`]（SQL-13）へ振り分ける（Issue #769）。
pub fn bind(
    validated: &Validated<'_>,
    schema: &TableSchema,
) -> Result<BoundAggregate, AggregateError> {
    let items_json = validated.required_array("aggregates")?;
    // `Vec` 確保・`String` 複製より前に件数上限を検査する
    // （`engine::sql::allowlist::check_aggregate_item_count`。
    // `.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」
    // 対応。`BoundAggregate::new`／`new_grouped` も同じ検査を行うが、
    // `items` を組み立てる前に打ち切るための多層防御）。
    allowlist::check_aggregate_item_count(items_json.len()).map_err(AggregateError::Engine)?;

    let mut items = Vec::with_capacity(items_json.len());
    let mut item_specs: Vec<(engine::sql::allowlist::AggregateFunc, AggregateTarget)> =
        Vec::with_capacity(items_json.len());
    for item in items_json {
        let parsed = bind_item(item, schema)?;
        item_specs.push((parsed.func, parsed.target));
        items.push(parsed.bound);
    }

    // `aggregates` が空リストなら、`group_by`／`having` の有無によらず一律
    // `42601` で拒否する。`BoundAggregate::new`（`group_by` なし経路）・
    // `new_grouped`（`group_by` あり経路）はいずれも同じ検査を持つが、
    // `group_by` あり経路では `having` を `item_specs`（この時点で空）へ
    // 照合する `bind_having_item` がこの検査より先に走ってしまうと、
    // `having` 側の「未知の HAVING 参照」（`22000`）が先に確定してしまい
    // `group_by` なし経路（`42601`）と異なる `wire_code` を返す非対称が
    // 生じる（codex-review 指摘。`crates/engine/src/sql/parser.rs` の
    // `BoundAggregate::new_grouped` 冒頭の同検査を参照）。ここで先に検査
    // することで両経路の `wire_code` を揃える。
    if items.is_empty() {
        return Err(AggregateError::Engine(SqlSurfaceError::UnsupportedSyntax {
            detail: "aggregate SELECT list must have at least one item".to_string(),
        }));
    }

    let metadata_filters = match validated.optional_array("filter")? {
        Some(filter_items) => filter::bind_filter(filter_items, schema)?,
        None => Vec::new(),
    };

    let group_by_json = validated.optional_array("group_by")?;
    let having_json = validated.optional_array("having")?;

    let group_by_column = match group_by_json {
        None => {
            // `having` は `group_by` なしに単独で指定できない（黙って無視
            // すると単一行集計〔SQL-13〕として fail-open に実行してしまう）。
            if having_json.is_some() {
                return Err(AggregateError::GroupByShape);
            }
            None
        }
        Some(cols) => {
            // `GROUP BY` は単一の裸識別子のみ受理する（SQL 表層の
            // `Parser::parse_group_by_clause` と同じ構造的制約）。
            let [engine::json::JsonValue::String(col)] = cols else {
                return Err(AggregateError::GroupByShape);
            };
            ident::check_identifier(col)?;
            Some(col.as_str())
        }
    };

    let Some(group_by_column) = group_by_column else {
        let bound = BoundAggregate::new(schema.name.clone(), items, metadata_filters, Vec::new())?;
        return Ok(bound);
    };

    let having_json = having_json.unwrap_or(&[]);
    // `Vec` 確保・要素の複製より前に件数上限を検査する
    // （`engine::sql::allowlist::check_having_predicate_count`。
    // `check_aggregate_item_count` と同じ設計判断）。
    allowlist::check_having_predicate_count(having_json.len()).map_err(AggregateError::Engine)?;

    let mut having = Vec::with_capacity(having_json.len());
    for item in having_json {
        having.push(bind_having_item(item, &item_specs)?);
    }

    let bound = BoundAggregate::new_grouped(
        schema.name.clone(),
        items,
        metadata_filters,
        Vec::new(),
        group_by_column,
        having,
        schema,
    )?;
    Ok(bound)
}

/// `validated`（`aggregate` op のスキーマ検証済み要求本文）を `engine` 上で
/// 実行する。`principal` の [`SessionPrincipal::policy_context`] のみから
/// RLS 境界（テナント）を導出し、`table`／`aggregates[].column` の識別子
/// 形状検査・`group_by`／`having`／`explain: true` の拒否をここで行う。
pub fn execute(
    engine: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<engine::sql::exec::QueryResult, AggregateError> {
    reject_explain(validated)?;

    let table = validated.required_str("table")?;
    ident::check_identifier(table)?;

    let session = SessionState::default();
    let result = engine.execute_bound_aggregate_in_session(
        principal.policy_context(),
        &session,
        table,
        |schema, _udfs| bind(validated, schema).map_err(to_sql_surface_error),
    )?;
    Ok(result)
}

/// binder closure（`FnOnce(...) -> Result<BoundAggregate, SqlSurfaceError>`）の
/// 戻り値型に [`AggregateError`] をそのまま渡せないため、`Engine`
/// （engine 側の分類をそのまま持つ）・`Filter(FilterError::Bind(_))`
/// （`declarative_filter::bind_all` 由来。`22000`／`54000` 等の分類を保つ）は
/// 中身の `SqlSurfaceError` をそのまま使う。それ以外（形状・関数語彙・
/// 識別子形状・`filter` の演算子語彙／RLS 述語名違反）はいずれも
/// `SqlSurfaceError::UnsupportedSyntax` 自身と同じ分類（`42601`）のため、
/// `SqlSurfaceError::UnsupportedSyntax { detail }` として復元してよい
/// （`SqlSurfaceError` は `pub enum`（`#[non_exhaustive]` 無し）のため
/// variant を engine クレート外から直接構築できる。`EngineCore::
/// execute_bound_aggregate_in_session` は binder のエラーをそのまま
/// 呼び出し元へ返す契約のため、[`execute`] 側の `From<SqlSurfaceError>
/// for AggregateError` により最終的な `wire_code`／`client_message` は
/// ここでの分類のまま保たれる）。
fn to_sql_surface_error(err: AggregateError) -> SqlSurfaceError {
    match err {
        AggregateError::Engine(inner) => inner,
        // `FilterError::into_sql_surface_error`（NOSQL-17。Issue #896）が
        // `Bind` 以外の分類（`54000`／`42601`／`22P02`／`0A000` 等）も保つ
        // 単一の変換点（`search.rs::to_sql_surface_error` のドキュメント参照）。
        AggregateError::Filter(filter_err) => filter_err.into_sql_surface_error(),
        other => SqlSurfaceError::UnsupportedSyntax {
            detail: other.client_message(),
        },
    }
}

/// `POST /v1/query`（`op: aggregate`）を実行し HTTP 応答バイト列を返す
/// （[`super::gate::handle`] の `Op::Aggregate` 分岐から呼ばれる唯一の入口）。
/// 成功時は [`super::response::encode`]（`QueryResult` → JSON 本文）を経て
/// `200 OK`、失敗時は [`AggregateError::error_class`]／[`AggregateError::
/// client_message`] を [`crate::http::response::encode_error`] へ渡す。
/// `response::encode` 自身の失敗（非有限値。`ResponseEncodeError`）は
/// `XX000` へ写像する（多層防御。engine 側は評価時に非有限を `22000` で
/// 拒否する契約だが、[`super::response`] モジュール自身の契約として
/// `ErrorClass::InternalError` を用いる）。
pub fn handle(
    engine: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute(engine, principal, validated) {
        Ok(result) => match super::response::encode(&result) {
            Ok(body) => crate::http::response::encode_ok(&body, now_wall),
            Err(encode_err) => crate::http::response::encode_error(
                encode_err.error_class(),
                &encode_err.client_message(),
                now_wall,
            ),
        },
        Err(err) => {
            crate::http::response::encode_error(err.error_class(), &err.client_message(), now_wall)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ColumnDef, ColumnType};
    use engine::json::{parse_json, JsonValue};

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        )
    }

    fn obj(json: &str) -> JsonValue {
        parse_json(json).expect("test fixture must be valid JSON")
    }

    // `Validated<'a>` は `value` への借用を保持するため、fixture の `JsonValue`
    // をテスト側で先に確保してから `AGGREGATE_SCHEMA.validate` を呼ぶ
    // （`schema.rs` の既存テストと同じ流儀）。
    macro_rules! validated_aggregate {
        ($name:ident, $json:expr) => {
            let value = obj($json);
            let $name = super::super::schema::AGGREGATE_SCHEMA
                .validate(&value)
                .expect("schema fixture must be valid");
        };
    }

    #[test]
    fn parse_function_accepts_five_lowercase_names() {
        use engine::sql::allowlist::AggregateFunc;
        assert_eq!(parse_function("count"), Some(AggregateFunc::Count));
        assert_eq!(parse_function("sum"), Some(AggregateFunc::Sum));
        assert_eq!(parse_function("avg"), Some(AggregateFunc::Avg));
        assert_eq!(parse_function("min"), Some(AggregateFunc::Min));
        assert_eq!(parse_function("max"), Some(AggregateFunc::Max));
    }

    #[test]
    fn parse_function_rejects_case_variants_and_unknown() {
        for raw in ["COUNT", "Count", " count", "count ", "total", ""] {
            assert_eq!(parse_function(raw), None, "raw={raw:?}");
        }
    }

    #[test]
    fn bind_accepts_star_count_and_column_forms() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"},{"fn":"min","column":"lang"}]}"#
        );
        let bound = bind(&v, &schema()).expect("bind should succeed");
        assert_eq!(bound.items().len(), 2);
    }

    #[test]
    fn bind_rejects_star_with_non_count_function() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"*"}]}"#
        );
        let err = bind(&v, &schema()).expect_err("SUM(*) must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_unknown_function_name() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"COUNT","column":"id"}]}"#
        );
        let err = bind(&v, &schema()).expect_err("unknown fn must be rejected");
        assert!(matches!(err, AggregateError::UnsupportedFunction));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_malformed_identifier_column() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"do cs"}]}"#
        );
        let err = bind(&v, &schema()).expect_err("malformed identifier must be rejected");
        assert!(matches!(err, AggregateError::InvalidIdentifier));
        assert_eq!(err.wire_code(), "42601");
        // untrusted な列名文字列を含まない固定文言であること。
        assert_eq!(err.client_message(), "invalid identifier");
    }

    #[test]
    fn bind_rejects_over_limit_item_count_before_binding() {
        let items: Vec<String> = (0..=engine::sql::allowlist::MAX_AGGREGATE_ITEMS)
            .map(|_| r#"{"fn":"count","column":"*"}"#.to_string())
            .collect();
        let json = format!(
            r#"{{"op":"aggregate","table":"docs","aggregates":[{}]}}"#,
            items.join(",")
        );
        validated_aggregate!(v, &json);
        let err = bind(&v, &schema()).expect_err("over-limit must be rejected");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn bind_item_rejects_malformed_shape_as_multi_layer_defense() {
        // `AGGREGATE_SCHEMA.validate` は `aggregates[]` を `AGGREGATE_ITEM_SCHEMA`
        // 経由で既に検証済みのため、`bind()` 経由では本パスに到達しない。
        // `bind_item` 単体が呼ばれた場合（多層防御。`filter.rs::map_filter_item`
        // と同じ判断）でも形状不正を独立に検出することを固定する。
        let malformed = obj(r#"{"fn":"count"}"#);
        let err = bind_item(&malformed, &schema()).expect_err("missing column must be rejected");
        assert!(matches!(err, AggregateError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_applies_filter_matching_declarative_filter_bind_all() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "filter":[{"column":"lang","op":"eq","value":"ja"}]}"#
        );
        let bound = bind(&v, &schema()).expect("bind should succeed");
        assert_eq!(bound.metadata_filters().len(), 1);
    }

    #[test]
    fn bind_rejects_unknown_column_with_22000() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"nope"}]}"#
        );
        let err = bind(&v, &schema()).expect_err("unknown column must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rejects_vector_column_for_sum_with_22000_but_accepts_count() {
        validated_aggregate!(
            v_sum,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"sum","column":"embedding"}]}"#
        );
        let err = bind(&v_sum, &schema()).expect_err("SUM(VECTOR) must be rejected");
        assert_eq!(err.wire_code(), "22000");

        validated_aggregate!(
            v_count,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"embedding"}]}"#
        );
        bind(&v_count, &schema()).expect("COUNT(VECTOR) must be accepted");
    }

    #[test]
    fn reject_explain_detects_only_explain_true() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[],"explain":true}"#
        );
        let err = reject_explain(&v).expect_err("explain:true must reject");
        assert!(matches!(err, AggregateError::ExplainNotSupported));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn reject_explain_allows_group_by_and_having() {
        // `group_by`／`having` は #769 で `reject_explain` の対象外に
        // なった（`bind` が直接処理する）。
        for json in [
            r#"{"op":"aggregate","table":"docs","aggregates":[],"group_by":["lang"]}"#,
            r#"{"op":"aggregate","table":"docs","aggregates":[],"group_by":[],"having":[]}"#,
        ] {
            validated_aggregate!(v, json);
            assert!(reject_explain(&v).is_ok(), "json={json}");
        }
    }

    // --- Issue #769・TASK-186・NOSQL-5: `parse_having_op` の語彙 -----------------

    #[test]
    fn parse_having_op_accepts_five_symbols() {
        assert_eq!(parse_having_op("="), Some(HavingOp::Eq));
        assert_eq!(parse_having_op("<"), Some(HavingOp::Lt));
        assert_eq!(parse_having_op("<="), Some(HavingOp::Le));
        assert_eq!(parse_having_op(">"), Some(HavingOp::Gt));
        assert_eq!(parse_having_op(">="), Some(HavingOp::Ge));
    }

    #[test]
    fn parse_having_op_rejects_word_forms_and_whitespace() {
        for raw in ["gt", "GE", " =", "= ", "==", "!=", ""] {
            assert_eq!(parse_having_op(raw), None, "raw={raw:?}");
        }
    }

    // --- Issue #769・TASK-186・NOSQL-5: `bind` の `group_by`／`having` 分岐 -----

    #[test]
    fn bind_dispatches_group_by_to_new_grouped() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "group_by":["lang"]}"#
        );
        let bound = bind(&v, &schema()).expect("group_by should bind");
        assert!(bound.has_group_by());
    }

    #[test]
    fn bind_rejects_group_by_with_more_than_one_element() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "group_by":["lang","lang"]}"#
        );
        let err = bind(&v, &schema()).expect_err("multi-column group_by must be rejected");
        assert!(matches!(err, AggregateError::GroupByShape));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_empty_group_by_array() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "group_by":[]}"#
        );
        let err = bind(&v, &schema()).expect_err("empty group_by must be rejected");
        assert!(matches!(err, AggregateError::GroupByShape));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_having_without_group_by() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "having":[{"fn":"count","column":"*","op":">=","value":1}]}"#
        );
        let err = bind(&v, &schema()).expect_err("having without group_by must be rejected");
        assert!(matches!(err, AggregateError::GroupByShape));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_having_without_group_by_even_when_having_is_empty() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "having":[]}"#
        );
        let err = bind(&v, &schema()).expect_err("having:[] without group_by must be rejected");
        assert!(matches!(err, AggregateError::GroupByShape));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_empty_aggregates_with_42601_regardless_of_group_by_shape() {
        // codex-review 指摘（PR #824）: `aggregates:[]` は `group_by`／`having`
        // の有無によらず一律 `42601` であるべき。`group_by` あり・`having` が
        // `aggregates` を参照する形（本来なら `item_specs` 未存在で `22000`
        // になり得た形）でも `42601` が先に確定することを固定する。
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[],
               "group_by":["lang"],
               "having":[{"fn":"count","column":"*","op":"=","value":1}]}"#
        );
        let err = bind(&v, &schema()).expect_err("aggregates:[] must be rejected");
        assert_eq!(err.wire_code(), "42601");

        // `group_by` なし経路（`BoundAggregate::new`）も同じ `42601` のまま
        // であることを対照として固定する。
        validated_aggregate!(
            v_ungrouped,
            r#"{"op":"aggregate","table":"docs","aggregates":[]}"#
        );
        let err_ungrouped =
            bind(&v_ungrouped, &schema()).expect_err("aggregates:[] must be rejected");
        assert_eq!(err_ungrouped.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_malformed_group_by_identifier_without_leaking_input() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "group_by":["do cs"]}"#
        );
        let err = bind(&v, &schema()).expect_err("malformed group_by identifier must be rejected");
        assert!(matches!(err, AggregateError::InvalidIdentifier));
        assert_eq!(err.wire_code(), "42601");
        assert!(!err.client_message().contains("do cs"));
    }

    #[test]
    fn bind_rejects_group_by_on_non_text_column_with_22000() {
        for column in ["embedding", "id", "nope"] {
            validated_aggregate!(
                v,
                &format!(
                    r#"{{"op":"aggregate","table":"docs",
                       "aggregates":[{{"fn":"count","column":"*"}}],
                       "group_by":["{column}"]}}"#
                )
            );
            let err = bind(&v, &schema()).expect_err("non-TEXT group_by column must be rejected");
            assert_eq!(err.wire_code(), "22000", "column={column}");
        }
    }

    #[test]
    fn bind_rejects_having_on_min_max_text_aggregate_with_22000() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"},{"fn":"min","column":"lang"}],
               "group_by":["lang"],
               "having":[{"fn":"min","column":"lang","op":">=","value":1}]}"#
        );
        let err = bind(&v, &schema()).expect_err("HAVING on MIN(TEXT) must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rejects_having_reference_absent_from_aggregates_with_22000() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "group_by":["lang"],
               "having":[{"fn":"sum","column":"id","op":">=","value":1}]}"#
        );
        let err = bind(&v, &schema()).expect_err("unknown HAVING reference must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rejects_ambiguous_having_reference_with_22000() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"},{"fn":"count","column":"*"}],
               "group_by":["lang"],
               "having":[{"fn":"count","column":"*","op":">=","value":1}]}"#
        );
        let err = bind(&v, &schema()).expect_err("ambiguous HAVING reference must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rejects_having_unsupported_operator_with_42601() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "group_by":["lang"],
               "having":[{"fn":"count","column":"*","op":"gt","value":1}]}"#
        );
        let err = bind(&v, &schema()).expect_err("unsupported HAVING operator must be rejected");
        assert!(matches!(err, AggregateError::UnsupportedHavingOperator));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rejects_non_finite_having_value_with_42601() {
        // `1e400` は f64 の表現範囲を超えるため、Rust の `str::parse::<f64>()`
        // （`engine::json::parse_json` が内部で使う）が `f64::INFINITY` を
        // 返す（JSON パーサ自身は構文エラーにしない）。
        let having_item = obj(r#"{"fn":"count","column":"*","op":">=","value":1e400}"#);
        if let engine::json::JsonValue::Object(map) = &having_item {
            // `engine::json::JsonNumber` は `is_finite()` を持たない（`PosInt`／
            // `NegInt` 整数リテラルは常に有限なため、非有限判定は `f64` へ変換した
            // `Float` variant にのみ意味を持つ。`as_f64()` は丸めを伴いうるが、
            // `1e400` は元より `f64::INFINITY` へしか丸まらないため本フィクスチャの
            // 検証には影響しない）。
            assert!(
                matches!(map.get("value"), Some(engine::json::JsonValue::Number(n)) if !n.as_f64().is_finite()),
                "fixture must actually be non-finite"
            );
        }
        let item_specs: Vec<(engine::sql::allowlist::AggregateFunc, AggregateTarget)> = vec![(
            engine::sql::allowlist::AggregateFunc::Count,
            AggregateTarget::Star,
        )];
        let err = bind_having_item(&having_item, &item_specs)
            .expect_err("non-finite value must be rejected");
        assert!(matches!(err, AggregateError::NonFiniteHavingLiteral));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_applies_group_by_with_having_and_filter_together() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "filter":[{"column":"lang","op":"eq","value":"ja"}],
               "group_by":["lang"],
               "having":[{"fn":"count","column":"*","op":">=","value":1}]}"#
        );
        let bound = bind(&v, &schema()).expect("group_by+having+filter should bind");
        assert!(bound.has_group_by());
        assert_eq!(bound.metadata_filters().len(), 1);
    }

    #[test]
    fn reject_explain_allows_plain_aggregate() {
        validated_aggregate!(
            v,
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],
               "explain":false}"#
        );
        assert!(reject_explain(&v).is_ok());
    }
}
