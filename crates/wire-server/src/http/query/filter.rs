//! `POST /v1/query` の `filter` 配列（`search`／`scan`／`aggregate` 共通）を
//! `engine::sql::declarative_predicate::DeclarativePredicate` 木へ写像し、
//! `engine::sql::declarative_predicate::bind_declarative_predicates` を通して
//! SQL 表層と同型の束縛結果（`MetadataFilter`・`BoundExpr`・`OR` 群）を得る
//! モジュール（Issue #761・TASK-175・対象ビヘイビア NOSQL-7。Issue #896・
//! NOSQL-17 で列型別の値レーンへ拡張。Issue #945・NOSQL-14 で範囲比較・`IN`・
//! `OR` へ拡張。ポインタ: `docs/spec/05-tasks.md` TASK-175・TASK-147・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-7・NOSQL-14・EXT-3・
//! `sql-surface.md` SQL-24）。
//!
//! 責務境界: 受理する `op` は `eq`・`prefix`・`lt`・`le`・`lte`・`gt`・`ge`・
//! `gte`・`in` の 9 語彙（`le`/`lte` と `ge`/`gte` はそれぞれ完全一致の同義語。
//! Issue #945 の受け入れ条件と対象ビヘイビア NOSQL-14 とで表記が食い違うため、
//! 両方を許可リストへ入れて安全側に倒す判断——表記の一本化は spec 側の
//! オーナー判断事項）。要素は「葉」（`{"column","op","value"}`）または
//! 「グループ」（`{"or": [<要素>, ...]}`。2 分岐以上・各分岐は葉または入れ子の
//! グループ 1 つ）のいずれかの形を取り、`filter` 配列自体・グループ内の分岐は
//! いずれも暗黙に `AND` 結合として扱う（`or` キー以外での `OR` 表現は存在しない）。
//! `value` は文字列・数値・真偽値（`eq`／`prefix`／範囲比較）または配列
//! （`in`。要素は文字列・数値・真偽値）のいずれかを受理し、対象列の型に応じて
//! [`bind_filter`] がレーンを振り分ける。
//!
//! 型別レーン:
//! - `eq`: `TEXT`／`ENUM`（文字列。`prefix` と同じ）・`BOOLEAN`（真偽値）・
//!   `DATE`／`TIMESTAMP`／`UUID`（文字列）・`BYTEA`（base64 文字列）・
//!   `NUMERIC`（数値または数値文字列）
//! - `lt`／`le`／`lte`／`gt`／`ge`／`gte`: `DATE`／`TIMESTAMP`／`UUID`（文字列）・
//!   `NUMERIC`（数値または数値文字列）・`BYTEA`（base64 文字列）。
//!   `TEXT`／`ENUM`／`BOOLEAN`／`VECTOR`／`ARRAY`／`JSON`／`JSONB` は
//!   engine 側の「範囲比較非対応列」判定（`22000`）へ委譲する
//! - `in`: `TEXT`／`ENUM`（文字列配列）・`DATE`／`TIMESTAMP`／`UUID`（文字列配列）・
//!   `NUMERIC`（数値または数値文字列の配列）・`BYTEA`（base64 文字列配列）。
//!   他の列型は engine 側の「IN 非対応列」判定（`22000`）へ委譲する
//! - `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `eq`・範囲比較は
//!   **対象外**（`0A000`）。`udf_call::bind_expr` の列参照解決
//!   （`crate::sql::udf_call` の `Ident` 分岐）が現時点でこれらの列型を式内で
//!   受理しないため（別 Issue #891 の担当）。Issue #945 の計画時点ではこの
//!   列型を式レーンへ渡す想定だったが、実装時に engine 側の未対応を確認し、
//!   対象外へ縮小した（既存の `eq` と同じ挙動を範囲比較へも揃える）
//! - `VECTOR`／`ARRAY`／`JSON`／`JSONB` への `eq`／`prefix` は SQL 表層にも
//!   レーンが無いため、従来どおり engine の「`TEXT` 列でない」判定（`22000`）へ
//!   委譲する
//!
//! 未知列は列名だけで完結する判定のため値の種類に関わらず
//! `DeclarativeFilter::equals(column, "")` を engine へ渡し「unknown
//! column」（`22000`）を得る（`bind_impl` が値の解釈より先に列解決を行う
//! 契約に依拠）。
//!
//! `or` グループ内で RLS 述語名（`visible`／`visible()`）を指定することは
//! トップレベルと同じく `42601` で拒否する（再帰的に検査する。RLS はサーバー側
//! 暗黙適用のみで、`filter` の形に関わらずクライアントが解除できる経路を
//! 作らない。`.claude/rules/security.md` P0「テナント境界」）。
//!
//! 上限（`Vec` 確保・借用より前に JSON を走査して検査する。多層防御として
//! engine 側 `declarative_predicate::bind_declarative_predicates` も同じ上限を
//! 再検査する）:
//! - 葉（`Leaf`）の総数: [`engine::declarative_filter::MAX_METADATA_FILTERS`]
//!   （256）超過は `54000`
//! - `or` のネスト深さ: [`engine::sql::udf_call::MAX_EXPR_DEPTH`]（32）超過は
//!   `54000`
//! - `in` の要素数: [`engine::declarative_filter::MAX_IN_LIST_ITEMS`]（256）
//!   超過は `54000`。空の `in` は `42601`
//!
//! SQL 表層（`sql::allowlist::Parser::parse_where`・
//! `sql::parser::bind_where_predicates`）と**同一の** `declarative_filter`／
//! `declarative_predicate` 束縛へ委譲する第 2 の実行器は作らない。
//!
//! 呼び出し文脈: `scan`／`search`／`aggregate` の各 op ハンドラが `schema`・
//! `udfs`（UDF レジストリ。`INTEGER` 系列への式レーン用に受け取るが、本 Issue の
//! 対象外化により現時点では葉の束縛でのみ使う）が届く束縛 closure の内側で
//! [`bind_filter`] を呼ぶ。
//!
//! 対象外: op 別ハンドラからの呼び出し結線・実行そのもの、HTTP 応答へのエラー
//! 射影（`http::status`／`http::error_body` が別途 `ErrorClass` から写像する）。
//! `update`／`delete` op の `filter`（述語形）は本モジュールの
//! [`bind_filter_where_predicates`]（新設・Issue #1062）を経由するが、
//! [`bind_filter`] とは独立に `eq`／`prefix` の 2 語彙・`or` グループ非対応の
//! まま据え置く（範囲比較・`in`・`or` への拡張は Issue #1118 が明示的に
//! 対象外とした。`FILTER_ITEM_SCHEMA` は葉形のみを要求する契約のまま変更
//! していない）。

use std::collections::BTreeMap;

use engine::catalog::{ColumnType, TableSchema};
use engine::declarative_filter::{self, CompareOp, DeclarativeFilter};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::sql::allowlist::{is_allowed_where_predicate_name, SqlSurfaceError, WherePredicate};
use engine::sql::declarative_predicate::{self, DeclarativePredicate};
use engine::sql::udf_call::{self, MAX_EXPR_DEPTH};

use super::schema::SchemaError;
use super::typed_json::{self, TypedJsonError};

/// [`map_filter_items`]／[`bind_filter`] の失敗を表す。いずれも
/// [`ClassifiedError`] を実装し、`wire_code`／`client_message` を経由して
/// HTTP エラー応答へ射影される（呼び出し元が `to_string()` を使わない契約は
/// `SqlSurfaceError::client_message` の既存ドキュメントと同じ）。
#[derive(Debug, Clone)]
pub enum FilterError {
    /// `op` が許可語彙（`eq`／`prefix`／範囲比較 6 語彙／`in`）に厳密一致しない
    /// （大文字小文字読み替えなし。否定・`between` 等の語彙外の値はすべてここに
    /// 落ちる）。untrusted な `op` 文字列は文言へ含めない固定文言（security.md
    /// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」対応）。
    UnsupportedOperator,
    /// 述語形 `update`／`delete` の `filter`（[`map_predicate_dml_item`]）で
    /// `op` が `eq`／`prefix` のいずれでもない（大文字小文字読み替えなし）。
    /// [`UnsupportedOperator`]（`search`／`scan`／`aggregate` 用。範囲比較・
    /// `in` を許可語彙に含む文言）を共用すると、述語形 DML では実際には
    /// 拒否される `lt`／`in` 等まで許可済みと誤案内するため独立させる
    /// （codex-review P2 指摘対応、PR #1121）。untrusted な `op` 文字列は
    /// 文言へ含めない固定文言（security.md 同上）。
    ///
    /// [`UnsupportedOperator`]: FilterError::UnsupportedOperator
    UnsupportedOperatorForPredicateDml,
    /// `column` が RLS 述語名（`is_allowed_where_predicate_name` が真。
    /// 例: `visible`／`visible()`、大文字小文字非区別）と一致した（`or` 分岐の
    /// 内側を含め再帰的に検査する）。
    RlsPredicateNotAllowed,
    /// `filter` 要素・`or` グループが期待する形（必須キー・型）に適合しない。
    Shape(SchemaError),
    /// `or` グループの形が不正（キーが `or` 以外を含む、`or` の値が配列でない、
    /// 空配列）。`42601`。
    GroupShape,
    /// `or` のネスト深さが [`MAX_EXPR_DEPTH`] を超える（`54000`）。
    DepthExceeded,
    /// 葉（`Leaf`）の総数が [`declarative_filter::MAX_METADATA_FILTERS`] を
    /// 超える（`54000`）。
    LeafCountExceeded,
    /// `in` の要素配列が空（`42601`）。
    InEmpty,
    /// `in` の要素数が [`declarative_filter::MAX_IN_LIST_ITEMS`] を超える
    /// （`54000`）。
    InTooMany,
    /// `engine::declarative_predicate` 側の束縛エラー（件数上限超過 `54000`・
    /// 未知列／`TEXT` 列でない／空 prefix／ENUM 語彙外／`DATE`/`TIMESTAMP`/
    /// `UUID`/`NUMERIC` の形式・範囲エラー等）をそのまま透過する。
    Bind(SqlSurfaceError),
    /// `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `eq`・範囲比較
    /// （`0A000`。`udf_call::bind_expr` がこれらの列型の式内参照を現時点で
    /// 受理しないため対象外。Issue #891 へ申し送り）。
    NumericFilterNotSupported,
    /// `value` の JSON 種別が対象列型と噛み合わない・wire 固有の符号化
    /// エラー（NOSQL-17。Issue #896。BYTEA の base64 decode を含む。
    /// [`super::typed_json::TypedJsonError`] を insert・update と共有する）。
    Value(TypedJsonError),
}

impl From<SchemaError> for FilterError {
    fn from(err: SchemaError) -> Self {
        FilterError::Shape(err)
    }
}

impl From<SqlSurfaceError> for FilterError {
    fn from(err: SqlSurfaceError) -> Self {
        FilterError::Bind(err)
    }
}

impl ClassifiedError for FilterError {
    fn error_class(&self) -> ErrorClass {
        match self {
            FilterError::UnsupportedOperator
            | FilterError::UnsupportedOperatorForPredicateDml
            | FilterError::RlsPredicateNotAllowed
            | FilterError::GroupShape
            | FilterError::InEmpty => ErrorClass::UnsupportedSqlSyntax,
            FilterError::Shape(err) => err.error_class(),
            FilterError::DepthExceeded
            | FilterError::LeafCountExceeded
            | FilterError::InTooMany => ErrorClass::PayloadTooLarge,
            FilterError::Bind(err) => err.error_class(),
            FilterError::NumericFilterNotSupported => ErrorClass::FeatureNotSupported,
            FilterError::Value(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            FilterError::UnsupportedOperator => {
                "unsupported filter operator (only \"eq\", \"prefix\", \"lt\", \"le\", \"lte\", \"gt\", \"ge\", \"gte\" and \"in\" are allowed)".to_string()
            }
            FilterError::UnsupportedOperatorForPredicateDml => {
                "unsupported filter operator for predicate-form update/delete (only \"eq\" and \"prefix\" are allowed)".to_string()
            }
            FilterError::RlsPredicateNotAllowed => {
                "filter column must not reference an RLS predicate name".to_string()
            }
            FilterError::Shape(err) => err.client_message(),
            FilterError::GroupShape => {
                "filter \"or\" group must have exactly one \"or\" key with 1 or more branches"
                    .to_string()
            }
            FilterError::DepthExceeded => {
                "filter \"or\" nesting exceeds the allowed depth".to_string()
            }
            FilterError::LeafCountExceeded => {
                "filter leaf count exceeds the allowed limit".to_string()
            }
            FilterError::InEmpty => "filter \"in\" value must not be an empty array".to_string(),
            FilterError::InTooMany => {
                "filter \"in\" value exceeds the allowed number of elements".to_string()
            }
            FilterError::Bind(err) => err.client_message(),
            FilterError::NumericFilterNotSupported => {
                "eq/range filter on INTEGER/BIGINT/REAL/DOUBLE PRECISION columns is not supported yet"
                    .to_string()
            }
            FilterError::Value(err) => err.client_message(),
        }
    }
}

impl FilterError {
    /// `scan`／`search`／`aggregate` の束縛 closure（`Result<_,
    /// SqlSurfaceError>` を要求する `EngineCore::execute_bound_*_in_session`）
    /// が要求する形へ写像する。`Bind`（既に `SqlSurfaceError`）はそのまま
    /// 透過し、それ以外は [`ClassifiedError::error_class`] から `wire_code`
    /// を保ったまま `SqlSurfaceError` を構築する（`err.client_message()` を
    /// 埋め込むだけの雑な `UnsupportedSyntax` 丸めをしない。`54000`／`0A000`
    /// 等の分類を保つため。`typed_json::TypedJsonError::
    /// into_sql_surface_error` と同じ判断）。
    pub fn into_sql_surface_error(self) -> SqlSurfaceError {
        if let FilterError::Bind(inner) = self {
            return inner;
        }
        let detail = self.client_message();
        match self.error_class() {
            ErrorClass::PayloadTooLarge => SqlSurfaceError::PayloadTooLarge { detail },
            ErrorClass::InvalidTextRepresentation => {
                SqlSurfaceError::InvalidTextRepresentation { detail }
            }
            ErrorClass::InvalidInput => SqlSurfaceError::InvalidInput { detail },
            ErrorClass::FeatureNotSupported => SqlSurfaceError::FeatureNotSupported { detail },
            // `UnsupportedOperator`／`RlsPredicateNotAllowed`／`GroupShape`／
            // `InEmpty`／`TypeMismatch` はいずれもここに到達する。
            _ => SqlSurfaceError::UnsupportedSyntax { detail },
        }
    }
}

/// `column` が RLS 述語呼び出し形の許可名（現状 `visible`）に一致するかを、
/// 末尾の `()` の有無を問わず大文字小文字非区別で判定する。
fn is_rls_predicate_column(column: &str) -> bool {
    let name = column.strip_suffix("()").unwrap_or(column);
    is_allowed_where_predicate_name(name)
}

/// `op` が許可語彙（`le`/`lte`・`ge`/`gte` の同義語を含む）かどうか。
fn is_known_op(op: &str) -> bool {
    matches!(
        op,
        "eq" | "prefix" | "lt" | "le" | "lte" | "gt" | "ge" | "gte" | "in"
    )
}

/// `op` を [`CompareOp`] へ写像する（範囲比較 6 語彙のみ。`eq`／`prefix`／`in`
/// は `None`）。
fn range_op(op: &str) -> Option<CompareOp> {
    match op {
        "lt" => Some(CompareOp::Lt),
        "le" | "lte" => Some(CompareOp::Le),
        "gt" => Some(CompareOp::Gt),
        "ge" | "gte" => Some(CompareOp::Ge),
        _ => None,
    }
}

/// `filter` 配列要素・`or` グループの中間形（形・語彙・RLS・上限は検査済みで、
/// 列型に依存する値の解釈は `schema` が届くまで確定できないため未解決のまま
/// 借用で保持する）。
#[derive(Debug)]
enum FilterNode<'a> {
    Leaf {
        column: &'a str,
        op: &'a str,
        value: &'a JsonValue,
    },
    Or(Vec<FilterNode<'a>>),
}

/// `map`（`filter` 要素の JSON オブジェクト）から `column`／`op`／`value` を
/// 取り出す。判定順序は「必須欠落 → 型不一致 → 未知キー」に固定する
/// （`super::schema::ObjectSchema::validate` と同じ決定的な順序。`value` は
/// `null`／オブジェクトを除く JSON 型〔文字列・数値・真偽値・配列〕を受理し、
/// `in` かどうかによる配列可否の判定は呼び出し元〔[`validate_leaf_value_shape`]〕
/// に委ねる）。
fn extract_leaf_fields(
    map: &BTreeMap<String, JsonValue>,
) -> Result<(&str, &str, &JsonValue), FilterError> {
    let column = match map.get("column") {
        Some(JsonValue::String(s)) => s.as_str(),
        Some(_) => {
            return Err(FilterError::Shape(SchemaError::TypeMismatch {
                key: "column",
            }))
        }
        None => {
            return Err(FilterError::Shape(SchemaError::MissingRequired {
                key: "column",
            }))
        }
    };
    let op = match map.get("op") {
        Some(JsonValue::String(s)) => s.as_str(),
        Some(_) => return Err(FilterError::Shape(SchemaError::TypeMismatch { key: "op" })),
        None => {
            return Err(FilterError::Shape(SchemaError::MissingRequired {
                key: "op",
            }))
        }
    };
    let value = match map.get("value") {
        Some(
            v @ (JsonValue::String(_)
            | JsonValue::Number(_)
            | JsonValue::Bool(_)
            | JsonValue::Array(_)),
        ) => v,
        Some(_) => {
            return Err(FilterError::Shape(SchemaError::TypeMismatch {
                key: "value",
            }))
        }
        None => {
            return Err(FilterError::Shape(SchemaError::MissingRequired {
                key: "value",
            }))
        }
    };
    for key in map.keys() {
        if key != "column" && key != "op" && key != "value" {
            return Err(FilterError::Shape(SchemaError::UnknownKey));
        }
    }
    Ok((column, op, value))
}

/// `op`・`value` の形（配列可否・要素数・要素の JSON 型）を検査する
/// （`Vec` 確保・借用より前の多層防御の一部。`in` の要素数上限
/// （[`declarative_filter::MAX_IN_LIST_ITEMS`]）はここで検査する——列型に
/// 依存しないため schema 到達を待たずに検査できる）。
fn validate_leaf_value_shape(op: &str, value: &JsonValue) -> Result<(), FilterError> {
    if op == "in" {
        let JsonValue::Array(items) = value else {
            return Err(FilterError::Shape(SchemaError::TypeMismatch {
                key: "value",
            }));
        };
        if items.is_empty() {
            return Err(FilterError::InEmpty);
        }
        if items.len() > declarative_filter::MAX_IN_LIST_ITEMS {
            return Err(FilterError::InTooMany);
        }
        for item in items {
            if !matches!(
                item,
                JsonValue::String(_) | JsonValue::Number(_) | JsonValue::Bool(_)
            ) {
                return Err(FilterError::Shape(SchemaError::TypeMismatch {
                    key: "value",
                }));
            }
        }
        Ok(())
    } else if matches!(value, JsonValue::Array(_)) {
        Err(FilterError::Shape(SchemaError::TypeMismatch {
            key: "value",
        }))
    } else {
        Ok(())
    }
}

/// `item`（`filter` 配列要素、または `or` グループの 1 分岐）を再帰的に
/// [`FilterNode`] へ写像する。葉の総数（`leaves`）・ネスト深さ（`depth`）は
/// `Vec` 確保・借用より前にここで検査する（[`declarative_predicate::
/// bind_declarative_predicates`] 側の検査は多層防御の二次防御）。
fn map_element<'a>(
    item: &'a JsonValue,
    depth: usize,
    leaves: &mut usize,
) -> Result<FilterNode<'a>, FilterError> {
    if depth > MAX_EXPR_DEPTH {
        return Err(FilterError::DepthExceeded);
    }
    let JsonValue::Object(map) = item else {
        return Err(FilterError::Shape(SchemaError::TypeMismatch {
            key: "filter_item",
        }));
    };

    if map.contains_key("or") {
        if map.len() != 1 {
            return Err(FilterError::GroupShape);
        }
        let Some(JsonValue::Array(branches_json)) = map.get("or") else {
            return Err(FilterError::GroupShape);
        };
        if branches_json.is_empty() {
            return Err(FilterError::GroupShape);
        }
        // トップレベルと同じ件数上限を分岐配列にも適用する（`Vec::with_capacity`
        // より前の多層防御。真の上限は葉の総数〔256〕で足りるため、この検査は
        // 「分岐配列そのものの見かけ上のサイズ」を JSON パーサのコンテナ上限
        // 〔65,536〕未満で早期に打ち切るためだけの補助的な検査）。
        declarative_filter::check_filter_count(branches_json.len()).map_err(FilterError::Bind)?;
        let mut branches = Vec::with_capacity(branches_json.len());
        for branch_json in branches_json {
            branches.push(map_element(branch_json, depth + 1, leaves)?);
        }
        if branches.len() == 1 {
            // 単一分岐の `or` は親の AND 列へ平坦化する（`sql::allowlist::
            // Parser::parse_where_or` と同じ判断。`BoundOrGroup` の「分岐は
            // 2 個以上」という不変条件に合わせる）。
            return Ok(branches.remove(0));
        }
        return Ok(FilterNode::Or(branches));
    }

    let (column, op, value) = extract_leaf_fields(map)?;
    if is_rls_predicate_column(column) {
        return Err(FilterError::RlsPredicateNotAllowed);
    }
    if !is_known_op(op) {
        return Err(FilterError::UnsupportedOperator);
    }
    validate_leaf_value_shape(op, value)?;

    let next = leaves
        .checked_add(1)
        .ok_or(FilterError::LeafCountExceeded)?;
    if next > declarative_filter::MAX_METADATA_FILTERS {
        return Err(FilterError::LeafCountExceeded);
    }
    *leaves = next;

    Ok(FilterNode::Leaf { column, op, value })
}

/// `items`（`filter` 配列。[`super::schema::Validated::optional_array`]`("filter")`
/// が返す形）を、形・語彙・RLS・上限を検査済みの [`FilterNode`] 列（`AND` 結合）
/// へ写像する。件数検査（[`declarative_filter::check_filter_count`]、`54000`）を
/// `Vec` 確保・借用より**前**に行う（`.claude/rules/security.md`「不安全な設計｜
/// 無制限リソース確保（DoS）」対応）。
fn map_filter_items(items: &[JsonValue]) -> Result<Vec<FilterNode<'_>>, FilterError> {
    declarative_filter::check_filter_count(items.len()).map_err(FilterError::Bind)?;
    let mut leaves = 0usize;
    let mut mapped = Vec::with_capacity(items.len());
    for item in items {
        mapped.push(map_element(item, 0, &mut leaves)?);
    }
    Ok(mapped)
}

/// [`FilterNode`] を `schema` と照合して [`DeclarativePredicate`] へ写像する
/// （値の型別レーン振り分け本体。モジュール doc の型別レーン一覧を参照）。
fn declare_node(
    node: &FilterNode<'_>,
    schema: &TableSchema,
) -> Result<DeclarativePredicate, FilterError> {
    match node {
        FilterNode::Leaf { column, op, value } => declare_leaf(column, op, value, schema),
        FilterNode::Or(branches) => {
            let mut bound_branches = Vec::with_capacity(branches.len());
            for branch in branches {
                bound_branches.push(vec![declare_node(branch, schema)?]);
            }
            Ok(DeclarativePredicate::Or(bound_branches))
        }
    }
}

fn declare_leaf(
    column: &str,
    op: &str,
    value: &JsonValue,
    schema: &TableSchema,
) -> Result<DeclarativePredicate, FilterError> {
    let Some(col) = schema.columns.iter().find(|c| c.name == column) else {
        return Ok(DeclarativePredicate::Leaf(DeclarativeFilter::equals(
            column, "",
        )));
    };

    if op == "prefix" {
        // `prefix` は従来どおり `TEXT` 列限定。値の型不一致は `42601`
        // （insert/update の「TEXT は旧来型 = 22000」非対称は filter には
        // 適用しない——filter の既存契約はそもそも `42601` だったため）。
        return match value {
            JsonValue::String(s) => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::starts_with(
                column,
                s.as_str(),
            ))),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "prefix filter value must be a JSON string",
            ))),
        };
    }

    if op == "in" {
        let JsonValue::Array(items) = value else {
            // 到達しないはず（`validate_leaf_value_shape` が事前に検査済み）。
            // 多層防御として明示的に拒否する。
            return Err(FilterError::Shape(SchemaError::TypeMismatch {
                key: "value",
            }));
        };
        return declare_in(column, &col.ty, items);
    }

    if let Some(cmp) = range_op(op) {
        return declare_range(column, &col.ty, cmp, value);
    }

    // ここへ到達するのは `op == "eq"` のみ（`is_known_op` が他の語彙を
    // 事前に拒否済み）。
    declare_eq(column, &col.ty, value)
}

fn declare_eq(
    column: &str,
    ty: &ColumnType,
    value: &JsonValue,
) -> Result<DeclarativePredicate, FilterError> {
    match ty {
        ColumnType::Text => match value {
            JsonValue::String(s) => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::equals(
                column,
                s.as_str(),
            ))),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a TEXT column must be a JSON string",
            ))),
        },
        ColumnType::Enum(_) => match value {
            JsonValue::String(s) => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::equals(
                column,
                s.as_str(),
            ))),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for an ENUM column must be a JSON string",
            ))),
        },
        ColumnType::Boolean => match value {
            JsonValue::Bool(b) => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::bool_equals(
                column, *b,
            ))),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a BOOLEAN column must be a JSON boolean",
            ))),
        },
        ColumnType::Date | ColumnType::Timestamp | ColumnType::Uuid => match value {
            JsonValue::String(s) => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::compare(
                column,
                CompareOp::Eq,
                s.as_str(),
            ))),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a DATE/TIMESTAMP/UUID column must be a JSON string",
            ))),
        },
        ColumnType::Bytea => match value {
            JsonValue::String(s) => {
                let hex = typed_json::bytea_literal_text(s).map_err(FilterError::Value)?;
                Ok(DeclarativePredicate::Leaf(DeclarativeFilter::compare(
                    column,
                    CompareOp::Eq,
                    hex,
                )))
            }
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a BYTEA column must be a base64 JSON string",
            ))),
        },
        ColumnType::Numeric { .. } => match value {
            JsonValue::Number(n) => Ok(DeclarativePredicate::Leaf(
                DeclarativeFilter::compare_numeric_literal(
                    column,
                    CompareOp::Eq,
                    typed_json::number_literal_text(n),
                ),
            )),
            JsonValue::String(s) => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::compare(
                column,
                CompareOp::Eq,
                s.as_str(),
            ))),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a NUMERIC column must be a JSON number or numeric string",
            ))),
        },
        // 式レーン（`udf_call::bind_expr`）がこれらの列型の式内参照を現時点で
        // 受理しないため対象外（モジュール doc 参照。別 Issue #891）。
        ColumnType::Integer | ColumnType::BigInt | ColumnType::Real | ColumnType::Double => {
            Err(FilterError::NumericFilterNotSupported)
        }
        // SQL 表層にも eq レーンが無い列型は、従来どおり engine 側の
        // 「`TEXT` 列でない」判定（`22000`）へ委譲する（legacy 互換）。
        ColumnType::Vector(_) | ColumnType::Array(_) | ColumnType::Json | ColumnType::Jsonb => Ok(
            DeclarativePredicate::Leaf(DeclarativeFilter::equals(column, "")),
        ),
    }
}

fn declare_range(
    column: &str,
    ty: &ColumnType,
    cmp: CompareOp,
    value: &JsonValue,
) -> Result<DeclarativePredicate, FilterError> {
    match ty {
        ColumnType::Date | ColumnType::Timestamp | ColumnType::Uuid => match value {
            JsonValue::String(s) => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::compare(
                column,
                cmp,
                s.as_str(),
            ))),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "range filter value for a DATE/TIMESTAMP/UUID column must be a JSON string",
            ))),
        },
        ColumnType::Bytea => match value {
            JsonValue::String(s) => {
                let hex = typed_json::bytea_literal_text(s).map_err(FilterError::Value)?;
                Ok(DeclarativePredicate::Leaf(DeclarativeFilter::compare(
                    column, cmp, hex,
                )))
            }
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "range filter value for a BYTEA column must be a base64 JSON string",
            ))),
        },
        ColumnType::Numeric { .. } => match value {
            JsonValue::Number(n) => Ok(DeclarativePredicate::Leaf(
                DeclarativeFilter::compare_numeric_literal(
                    column,
                    cmp,
                    typed_json::number_literal_text(n),
                ),
            )),
            JsonValue::String(s) => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::compare(
                column,
                cmp,
                s.as_str(),
            ))),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "range filter value for a NUMERIC column must be a JSON number or numeric string",
            ))),
        },
        // 式レーンが現時点で受理しないため対象外（`declare_eq` と同じ理由）。
        ColumnType::Integer | ColumnType::BigInt | ColumnType::Real | ColumnType::Double => {
            Err(FilterError::NumericFilterNotSupported)
        }
        // TEXT/ENUM/BOOLEAN/VECTOR/ARRAY/JSON/JSONB: 比較不能。engine 側の
        // 「範囲比較非対応列」判定（`22000`）へ委譲する（SQL の `lang > 'x'` と
        // 同じ結果になる）。
        ColumnType::Text
        | ColumnType::Enum(_)
        | ColumnType::Boolean
        | ColumnType::Vector(_)
        | ColumnType::Array(_)
        | ColumnType::Json
        | ColumnType::Jsonb => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::compare(
            column, cmp, "",
        ))),
    }
}

fn declare_in(
    column: &str,
    ty: &ColumnType,
    items: &[JsonValue],
) -> Result<DeclarativePredicate, FilterError> {
    match ty {
        ColumnType::Text | ColumnType::Enum(_) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    JsonValue::String(s) => values.push(s.clone()),
                    _ => return Err(FilterError::Value(TypedJsonError::TypeMismatch(
                        "in filter value for a TEXT/ENUM column must be an array of JSON strings",
                    ))),
                }
            }
            Ok(DeclarativePredicate::Leaf(DeclarativeFilter::in_list(
                column, values,
            )))
        }
        ColumnType::Date | ColumnType::Timestamp | ColumnType::Uuid => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    JsonValue::String(s) => values.push(s.clone()),
                    _ => {
                        return Err(FilterError::Value(TypedJsonError::TypeMismatch(
                            "in filter value for a DATE/TIMESTAMP/UUID column must be an array of JSON strings",
                        )))
                    }
                }
            }
            Ok(DeclarativePredicate::Leaf(DeclarativeFilter::in_list(
                column, values,
            )))
        }
        ColumnType::Bytea => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    JsonValue::String(s) => {
                        values.push(typed_json::bytea_literal_text(s).map_err(FilterError::Value)?)
                    }
                    _ => {
                        return Err(FilterError::Value(TypedJsonError::TypeMismatch(
                            "in filter value for a BYTEA column must be an array of base64 JSON strings",
                        )))
                    }
                }
            }
            Ok(DeclarativePredicate::Leaf(DeclarativeFilter::in_list(
                column, values,
            )))
        }
        ColumnType::Numeric { .. } => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    JsonValue::Number(n) => values.push(typed_json::number_literal_text(n)),
                    JsonValue::String(s) => values.push(s.clone()),
                    _ => {
                        return Err(FilterError::Value(TypedJsonError::TypeMismatch(
                            "in filter value for a NUMERIC column must be an array of JSON numbers or numeric strings",
                        )))
                    }
                }
            }
            Ok(DeclarativePredicate::Leaf(DeclarativeFilter::in_list(
                column, values,
            )))
        }
        // INTEGER 系・BOOLEAN・VECTOR・ARRAY・JSON・JSONB は engine 側の
        // 「IN 非対応列」判定（`22000`）へ委譲する（SQL の `col IN ('..')` を
        // 数値・真偽列へ書いた場合と同じ結果になる）。
        ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Boolean
        | ColumnType::Vector(_)
        | ColumnType::Array(_)
        | ColumnType::Json
        | ColumnType::Jsonb => Ok(DeclarativePredicate::Leaf(DeclarativeFilter::in_list(
            column,
            Vec::new(),
        ))),
    }
}

/// [`map_filter_items`]・[`declare_node`]・
/// [`declarative_predicate::bind_declarative_predicates`] を通しで実行し、
/// SQL 表層と同型の束縛結果を得る（`scan`／`search`／`aggregate` から呼ばれる
/// 唯一の公開入口）。
pub fn bind_filter(
    items: &[JsonValue],
    schema: &TableSchema,
    udfs: &udf_call::UdfRegistry,
) -> Result<declarative_predicate::BoundWhereFilters, FilterError> {
    let mapped = map_filter_items(items)?;
    let mut preds = Vec::with_capacity(mapped.len());
    for node in &mapped {
        preds.push(declare_node(node, schema)?);
    }
    declarative_predicate::bind_declarative_predicates(&preds, schema, udfs)
        .map_err(FilterError::Bind)
}

/// `LIKE` パターンのメタ文字（`\`・`%`・`_`。[`engine::declarative_filter::
/// parse_like_pattern`] が解釈する 3 文字）をエスケープし、`raw` を無加工の
/// 前方一致対象文字列として扱えるようにする（[`bind_filter_where_predicates`]
/// の `prefix` レーン専用。`declare_leaf` が `DeclarativeFilter::starts_with`
/// 経由で構築する `MetadataFilter::StartsWith`〔本モジュール内で完結し
/// `parse_like_pattern` を経由しない、`scan`／`search`／`aggregate` の
/// `filter` 専用表現〕はこのエスケープを必要としない。DML 経路のみ SQL 表層の
/// `WHERE <col> LIKE '<pattern>%'`〔`engine::sql::allowlist::
/// WherePredicate::Prefix`〕と同一の `content_hash` 入力を再現する必要が
/// あるため、`raw` に含まれるメタ文字を無害化してから `%` を付与する）。
fn like_escape(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c == '\\' || c == '%' || c == '_' {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// `(column, op, value)`（列型未確定の葉。[`map_predicate_dml_item`] が
/// 検証済み）を、SQL 表層の述語形 `UPDATE`／`DELETE`（[`engine::sql::
/// allowlist::WherePredicate`]）が使うのと同一の構文形へ写像する
/// （[`bind_filter_where_predicates`] 専用）。呼び出し元が先に
/// [`declare_leaf`]`(column, op, value, schema)?` の `Leaf` を
/// `DeclarativeFilter::bind` に通していることを前提とし、本関数自身は値の
/// 型検査をしない（検証済みの `(op, 列型)` の組み合わせのみが渡る契約。
/// 想定外の組み合わせは `Internal` で fail-closed に落とす）。
fn where_predicate_for(
    column: &str,
    op: &str,
    value: &JsonValue,
    col: &engine::catalog::ColumnDef,
) -> Result<WherePredicate, FilterError> {
    if op == "prefix" {
        // 呼び出し元が `JsonValue::String` であることを既に検証済み
        // （契約。想定外の値種別は多層防御として fail-closed に拒否する）。
        let raw = match value {
            JsonValue::String(s) => s.as_str(),
            _ => {
                return Err(FilterError::Bind(SqlSurfaceError::Internal {
                    detail: "unexpected non-string value for a validated prefix filter".to_string(),
                }))
            }
        };
        return Ok(WherePredicate::Prefix {
            column: column.to_string(),
            pattern: format!("{}%", like_escape(raw)),
        });
    }

    // ここから `op == "eq"`。BOOLEAN のみ `BoolEquality`、それ以外は
    // `Equality`（値は SQL リテラルテキスト形。`sql::parser::
    // declarative_leaf_to_filter` が列型に応じて `DeclarativeFilter::compare`
    // （DATE/TIMESTAMP/NUMERIC/UUID/BYTEA）／`equals`（TEXT/ENUM）へ
    // 振り分けるため、`WherePredicate` 構築段では列型を分岐する必要が無い）。
    let internal = || {
        FilterError::Bind(SqlSurfaceError::Internal {
            detail: "unexpected value shape for a validated eq filter".to_string(),
        })
    };
    match &col.ty {
        ColumnType::Boolean => match value {
            JsonValue::Bool(b) => Ok(WherePredicate::BoolEquality {
                column: column.to_string(),
                value: *b,
            }),
            _ => Err(internal()),
        },
        ColumnType::Bytea => match value {
            JsonValue::String(s) => {
                let text = typed_json::bytea_literal_text(s).map_err(FilterError::Value)?;
                Ok(WherePredicate::Equality {
                    column: column.to_string(),
                    value: text,
                })
            }
            _ => Err(internal()),
        },
        ColumnType::Numeric { .. } => match value {
            JsonValue::Number(n) => Ok(WherePredicate::Equality {
                column: column.to_string(),
                value: typed_json::number_literal_text(n),
            }),
            JsonValue::String(s) => Ok(WherePredicate::Equality {
                column: column.to_string(),
                value: s.clone(),
            }),
            _ => Err(internal()),
        },
        // TEXT／ENUM／DATE／TIMESTAMP／UUID はいずれも文字列リテラル形
        // （呼び出し元契約により検証済み）。
        _ => match value {
            JsonValue::String(s) => Ok(WherePredicate::Equality {
                column: column.to_string(),
                value: s.clone(),
            }),
            _ => Err(internal()),
        },
    }
}

/// `update`／`delete` op の `filter`（述語形）専用の葉検証。`filter` 配列は
/// `search`／`scan`／`aggregate` と共通の JSON 語彙（[`FilterNode`]。範囲
/// 比較・`in`・`or`、Issue #945・#1118）を持つが、述語形 DML への同拡張は
/// Issue #1118 で明示的に対象外とされた（`FILTER_ITEM_SCHEMA` は変更しない・
/// `UPDATE_SCHEMA`／`DELETE_SCHEMA` は葉形のみを要求する契約は
/// `schema.rs::filter_element_shape_is_deferred_to_filter_module` 参照）ため、
/// 本関数は [`map_element`]／[`FilterNode`] を経由せず `eq`／`prefix` の
/// 2 語彙・`or` グループ非対応のまま独立に検証する（#1062 の範囲）。
/// 語彙・RLS の検証順序・エラー分類は [`map_element`] の葉分岐と揃える。
fn map_predicate_dml_item(item: &JsonValue) -> Result<(&str, &str, &JsonValue), FilterError> {
    let JsonValue::Object(map) = item else {
        return Err(FilterError::Shape(SchemaError::TypeMismatch {
            key: "filter_item",
        }));
    };
    let (column, op, value) = extract_leaf_fields(map)?;
    if is_rls_predicate_column(column) {
        return Err(FilterError::RlsPredicateNotAllowed);
    }
    if op != "eq" && op != "prefix" {
        return Err(FilterError::UnsupportedOperatorForPredicateDml);
    }
    validate_leaf_value_shape(op, value)?;
    Ok((column, op, value))
}

/// `items`（`filter` 配列。述語形 DML 専用の狭い語彙）を検証済みの葉列へ
/// 写像する。件数検査を `Vec` 確保・借用より前に行う（[`map_filter_items`]
/// と同じ多層防御の判断）。
fn map_predicate_dml_items(
    items: &[JsonValue],
) -> Result<Vec<(&str, &str, &JsonValue)>, FilterError> {
    declarative_filter::check_filter_count(items.len()).map_err(FilterError::Bind)?;
    items.iter().map(map_predicate_dml_item).collect()
}

/// [`map_predicate_dml_items`] の結果を `schema` へ束縛し、SQL 表層の述語形
/// `UPDATE`／`DELETE`（`WHERE <col> = <lit> AND <col> LIKE '<prefix>%'`）が
/// 使うのと同一の `Vec<WherePredicate>` を得る（NoSQL 表層 `update`／`delete`
/// op の `filter`。TASK-186・NOSQL-12、Issue #1062。宣言順を保った `AND`
/// 結合）。[`bind_filter`]（`BoundWhereFilters` を返す。`scan`／`search`／
/// `aggregate` 専用で範囲比較・`in`・`or` に対応）とは戻り値の型・対応語彙が
/// 異なる別 API——述語形 DML の `content_hash`（[`engine::recovery::
/// content_hash::for_update_where`]・`for_delete_where`）は束縛前の構文形
/// `WherePredicate` をハッシュ源にするため、SQL 表層と同一のバイト列を
/// 再現するには `WherePredicate` そのものが必要（`MetadataFilter` へ変換
/// してしまうと SQL⇄NoSQL 間の台帳照合（`23505`／`22023`、RECOVER-10）が
/// 成立しなくなる）。
///
/// 各要素は [`declare_leaf`]`(column, op, value, schema)?` の `Leaf` を
/// `DeclarativeFilter::bind`（[`bind_filter`] の葉と同じ検証経路。未知列・
/// 非 TEXT 列への `prefix`・ENUM 語彙外・値の型不一致はすべて `bind_filter`
/// と同じ分類になる）に通してから [`where_predicate_for`] で構文形へ変換する
/// （検証してから変換する不変条件。変換器は値の型検査をしない）。
pub fn bind_filter_where_predicates(
    items: &[JsonValue],
    schema: &TableSchema,
) -> Result<Vec<WherePredicate>, FilterError> {
    let mapped = map_predicate_dml_items(items)?;
    let mut predicates = Vec::with_capacity(mapped.len());
    for (column, op, value) in mapped {
        let declared = declare_leaf(column, op, value, schema)?;
        let DeclarativePredicate::Leaf(leaf) = &declared else {
            // `declare_leaf` は `eq`／`prefix` に対して常に `Leaf` を返す
            // （`map_predicate_dml_item` が他の語彙を事前に拒否済み）。多層
            // 防御として明示的に拒否する。
            return Err(FilterError::Bind(SqlSurfaceError::Internal {
                detail: "unexpected non-leaf predicate for an eq/prefix filter".to_string(),
            }));
        };
        // `bind_filter` と同一の検証（未知列・型不一致等）を通す。束縛結果
        // （`MetadataFilter`）自体は使わない——構文形への変換は独立した
        // `where_predicate_for` が担う。
        leaf.bind(schema).map_err(FilterError::Bind)?;
        let Some(col) = schema.columns.iter().find(|c| c.name == column) else {
            // 未知列は直前の `bind` が先に `22000` で拒否済みのため到達しない
            // （fail-closed フォールバック）。
            return Err(FilterError::Bind(SqlSurfaceError::Internal {
                detail: "unexpected unknown column after successful filter bind".to_string(),
            }));
        };
        predicates.push(where_predicate_for(column, op, value, col)?);
    }
    Ok(predicates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::ColumnDef;
    use engine::json::parse_json;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("active", ColumnType::Boolean, true),
                ColumnDef::new("created", ColumnType::Date, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("count", ColumnType::Integer, true),
            ],
        )
    }

    fn udfs() -> udf_call::UdfRegistry {
        udf_call::UdfRegistry::default()
    }

    fn filter_items(json: &str) -> Vec<JsonValue> {
        let JsonValue::Array(items) = parse_json(json).expect("valid JSON") else {
            panic!("expected array");
        };
        items
    }

    fn bind(json: &str) -> Result<declarative_predicate::BoundWhereFilters, FilterError> {
        let items = filter_items(json);
        bind_filter(&items, &schema(), &udfs())
    }

    #[test]
    fn eq_maps_to_declarative_equals() {
        let bound = bind(r#"[{"column":"lang","op":"eq","value":"ja"}]"#).expect("bind ok");
        assert_eq!(bound.metadata_filters().len(), 1);
        assert!(bound.expr_filters().is_empty());
        assert!(bound.or_filters().is_empty());
    }

    #[test]
    fn prefix_maps_to_declarative_starts_with() {
        let bound = bind(r#"[{"column":"path","op":"prefix","value":"src/"}]"#).expect("bind ok");
        assert_eq!(bound.metadata_filters().len(), 1);
    }

    #[test]
    fn multiple_items_preserve_order_as_and() {
        let bound = bind(
            r#"[{"column":"lang","op":"eq","value":"ja"},{"column":"path","op":"prefix","value":"src/"}]"#,
        )
        .expect("bind ok");
        assert_eq!(bound.metadata_filters().len(), 2);
    }

    #[test]
    fn empty_array_binds_to_empty_vec() {
        let bound = bind("[]").expect("bind ok");
        assert!(bound.metadata_filters().is_empty());
    }

    #[test]
    fn unsupported_operators_are_rejected() {
        for op in [
            "or", "not", "neq", "between", "range", "EQ", "Prefix", "LT", "visible",
        ] {
            let err = bind(&format!(r#"[{{"column":"lang","op":"{op}","value":"x"}}]"#))
                .expect_err("must reject");
            assert!(matches!(err, FilterError::UnsupportedOperator));
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn range_op_synonyms_are_accepted() {
        for op in ["lt", "le", "lte", "gt", "ge", "gte"] {
            let bound = bind(&format!(
                r#"[{{"column":"created","op":"{op}","value":"2024-01-01"}}]"#
            ))
            .unwrap_or_else(|e| panic!("op {op} must bind: {e:?}"));
            assert_eq!(bound.metadata_filters().len(), 1);
        }
    }

    #[test]
    fn rls_predicate_columns_are_rejected() {
        for column in ["visible", "VISIBLE", "visible()", "Visible()"] {
            let err = bind(&format!(
                r#"[{{"column":"{column}","op":"eq","value":"x"}}]"#
            ))
            .expect_err("must reject");
            assert!(matches!(err, FilterError::RlsPredicateNotAllowed));
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn rls_predicate_columns_are_rejected_inside_or_branches() {
        let err = bind(
            r#"[{"or":[{"column":"visible","op":"eq","value":"x"},{"column":"lang","op":"eq","value":"ja"}]}]"#,
        )
        .expect_err("must reject");
        assert!(matches!(err, FilterError::RlsPredicateNotAllowed));
    }

    #[test]
    fn malformed_shape_is_rejected() {
        let err = bind(r#"["not-an-object"]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");

        let err = bind(r#"[{"column":"lang","op":"eq"}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));

        let err = bind(r#"[{"column":"lang","op":"eq","value":"ja","extra":"x"}]"#)
            .expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));

        let err = bind(r#"[{"column":"lang","op":"eq","value":{}}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));
    }

    #[test]
    fn or_group_shape_errors() {
        // 空の or。
        let err = bind(r#"[{"or":[]}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::GroupShape));
        assert_eq!(err.wire_code(), "42601");

        // or 以外のキーが混在。
        let err = bind(
            r#"[{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"eq","value":"en"}],"extra":1}]"#,
        )
        .expect_err("must reject");
        assert!(matches!(err, FilterError::GroupShape));

        // or の値が配列でない。
        let err = bind(r#"[{"or":"nope"}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::GroupShape));
    }

    #[test]
    fn single_branch_or_is_flattened() {
        let bound = bind(r#"[{"or":[{"column":"lang","op":"eq","value":"ja"}]}]"#)
            .expect("bind ok (flattened)");
        assert_eq!(bound.metadata_filters().len(), 1);
        assert!(bound.or_filters().is_empty());
    }

    #[test]
    fn two_branch_or_binds_to_or_group() {
        let bound = bind(
            r#"[{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"eq","value":"en"}]}]"#,
        )
        .expect("bind ok");
        assert!(bound.metadata_filters().is_empty());
        assert_eq!(bound.or_filters().len(), 1);
    }

    #[test]
    fn nested_or_is_accepted() {
        let bound = bind(
            r#"[{"or":[{"column":"lang","op":"eq","value":"ja"},{"or":[{"column":"lang","op":"eq","value":"en"},{"column":"lang","op":"eq","value":"fr"}]}]}]"#,
        )
        .expect("bind ok");
        assert_eq!(bound.or_filters().len(), 1);
    }

    fn leaf_json(value: &str) -> JsonValue {
        let mut map = BTreeMap::new();
        map.insert("column".to_string(), JsonValue::String("lang".to_string()));
        map.insert("op".to_string(), JsonValue::String("eq".to_string()));
        map.insert("value".to_string(), JsonValue::String(value.to_string()));
        JsonValue::Object(map)
    }

    #[test]
    fn or_depth_over_limit_is_rejected() {
        // JSON テキスト経由（`parse_json`）だと `engine::json` 自身の深さ上限
        // （NOSQL-8。`MAX_EXPR_DEPTH` より浅い）に先に抵触するため、
        // `JsonValue` を直接組み立てて本モジュールの深さ検査だけを対象にする
        // （計画で申し送った「HTTP 経由では JSON 深さ上限が先に効く」制約を
        // 迂回してテストする——本検査自体は engine API 直接呼び出し経路の
        // fail-closed のために存在する）。
        let mut node = leaf_json("ja");
        for _ in 0..=MAX_EXPR_DEPTH {
            let mut or_map = BTreeMap::new();
            or_map.insert(
                "or".to_string(),
                JsonValue::Array(vec![node, leaf_json("en")]),
            );
            node = JsonValue::Object(or_map);
        }
        let err = bind_filter(&[node], &schema(), &udfs()).expect_err("must reject");
        assert!(matches!(err, FilterError::DepthExceeded));
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn empty_array_input_is_ok_but_over_limit_count_is_rejected() {
        let items: Vec<JsonValue> = (0..=declarative_filter::MAX_METADATA_FILTERS)
            .map(|i| {
                let mut map = BTreeMap::new();
                map.insert("column".to_string(), JsonValue::String("lang".to_string()));
                map.insert("op".to_string(), JsonValue::String("eq".to_string()));
                map.insert("value".to_string(), JsonValue::String(i.to_string()));
                JsonValue::Object(map)
            })
            .collect();
        let err = bind_filter(&items, &schema(), &udfs()).expect_err("must reject");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn at_limit_count_is_accepted() {
        let items: Vec<JsonValue> = (0..declarative_filter::MAX_METADATA_FILTERS)
            .map(|i| {
                let mut map = BTreeMap::new();
                map.insert("column".to_string(), JsonValue::String("lang".to_string()));
                map.insert("op".to_string(), JsonValue::String("eq".to_string()));
                map.insert("value".to_string(), JsonValue::String(i.to_string()));
                JsonValue::Object(map)
            })
            .collect();
        assert_eq!(
            bind_filter(&items, &schema(), &udfs())
                .expect("bind ok")
                .metadata_filters()
                .len(),
            declarative_filter::MAX_METADATA_FILTERS
        );
    }

    #[test]
    fn in_rejects_empty_array() {
        let err = bind(r#"[{"column":"lang","op":"in","value":[]}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::InEmpty));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn in_rejects_over_limit_count() {
        let values: Vec<JsonValue> = (0..=declarative_filter::MAX_IN_LIST_ITEMS)
            .map(|i| JsonValue::String(i.to_string()))
            .collect();
        let items = vec![{
            let mut map = BTreeMap::new();
            map.insert("column".to_string(), JsonValue::String("lang".to_string()));
            map.insert("op".to_string(), JsonValue::String("in".to_string()));
            map.insert("value".to_string(), JsonValue::Array(values));
            JsonValue::Object(map)
        }];
        let err = bind_filter(&items, &schema(), &udfs()).expect_err("must reject");
        assert!(matches!(err, FilterError::InTooMany));
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn in_binds_text_column() {
        let bound = bind(r#"[{"column":"lang","op":"in","value":["ja","en"]}]"#).expect("bind ok");
        assert_eq!(bound.metadata_filters().len(), 1);
    }

    #[test]
    fn in_on_integer_column_delegates_to_engine_22000() {
        let err = bind(r#"[{"column":"count","op":"in","value":[1,2]}]"#).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn in_value_must_be_array() {
        let err = bind(r#"[{"column":"lang","op":"in","value":"ja"}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn non_in_op_rejects_array_value() {
        let err = bind(r#"[{"column":"lang","op":"eq","value":["ja"]}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_filter_rejects_unknown_column() {
        let err = bind(r#"[{"column":"nope","op":"eq","value":"x"}]"#).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_filter_rejects_vector_column() {
        let err =
            bind(r#"[{"column":"embedding","op":"eq","value":"x"}]"#).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_filter_rejects_empty_prefix() {
        let err = bind(r#"[{"column":"path","op":"prefix","value":""}]"#).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn eq_maps_boolean_column_to_bool_equals() {
        let bound = bind(r#"[{"column":"active","op":"eq","value":true}]"#).expect("bind ok");
        assert_eq!(bound.metadata_filters().len(), 1);
    }

    #[test]
    fn eq_rejects_non_boolean_value_for_boolean_column() {
        let err =
            bind(r#"[{"column":"active","op":"eq","value":"true"}]"#).expect_err("must reject");
        assert!(matches!(
            err,
            FilterError::Value(TypedJsonError::TypeMismatch(_))
        ));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn eq_maps_date_column_via_compare() {
        let bound =
            bind(r#"[{"column":"created","op":"eq","value":"2024-01-02"}]"#).expect("bind ok");
        assert_eq!(bound.metadata_filters().len(), 1);
    }

    #[test]
    fn eq_maps_numeric_column_from_number_and_string() {
        let bound = bind(r#"[{"column":"amount","op":"eq","value":12.34}]"#).expect("bind ok");
        let bound_via_string =
            bind(r#"[{"column":"amount","op":"eq","value":"12.34"}]"#).expect("bind ok");
        assert_eq!(
            bound.metadata_filters(),
            bound_via_string.metadata_filters()
        );
    }

    #[test]
    fn range_maps_numeric_column_from_number_and_string() {
        let bound = bind(r#"[{"column":"amount","op":"gt","value":1.5}]"#).expect("bind ok");
        let bound_via_string =
            bind(r#"[{"column":"amount","op":"gt","value":"1.5"}]"#).expect("bind ok");
        assert_eq!(
            bound.metadata_filters(),
            bound_via_string.metadata_filters()
        );
    }

    #[test]
    fn range_on_text_column_delegates_to_engine_22000() {
        let err = bind(r#"[{"column":"lang","op":"gt","value":"a"}]"#).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn eq_on_integer_column_is_feature_not_supported() {
        let err = bind(r#"[{"column":"count","op":"eq","value":1}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::NumericFilterNotSupported));
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn range_on_integer_column_is_feature_not_supported() {
        let err = bind(r#"[{"column":"count","op":"gt","value":1}]"#).expect_err("must reject");
        assert!(matches!(err, FilterError::NumericFilterNotSupported));
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn eq_on_integer_column_stays_feature_not_supported_after_sql_surface_conversion() {
        let err = bind(r#"[{"column":"count","op":"eq","value":1}]"#).expect_err("must reject");
        let sql_err = err.into_sql_surface_error();
        assert_eq!(sql_err.wire_code(), "0A000");
    }

    #[test]
    fn prefix_on_boolean_column_is_rejected_as_not_a_text_column() {
        let err =
            bind(r#"[{"column":"active","op":"prefix","value":"t"}]"#).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind_filter_where_predicates（述語形 DML への写像。Issue #1062） ---

    fn predicate_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("active", ColumnType::Boolean, true),
                ColumnDef::new("note", ColumnType::Bytea, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("count", ColumnType::Integer, true),
            ],
        )
    }

    #[test]
    fn like_escape_escapes_backslash_percent_and_underscore() {
        assert_eq!(like_escape(r"a%b_c\d"), r"a\%b\_c\\d");
    }

    #[test]
    fn like_escape_leaves_ordinary_characters_untouched() {
        assert_eq!(like_escape("src/"), "src/");
    }

    #[test]
    fn prefix_maps_to_where_predicate_prefix_with_escaped_literal_and_trailing_percent() {
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":"a%b_c\\d"}]"#);
        let bound = bind_filter_where_predicates(&items, &predicate_schema()).expect("bind ok");
        assert_eq!(
            bound,
            vec![WherePredicate::Prefix {
                column: "path".to_string(),
                pattern: r"a\%b\_c\\d%".to_string(),
            }]
        );
    }

    #[test]
    fn empty_prefix_is_rejected_instead_of_matching_all_rows() {
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":""}]"#);
        let err = bind_filter_where_predicates(&items, &predicate_schema()).expect_err("reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn eq_text_maps_to_where_predicate_equality() {
        let items = filter_items(r#"[{"column":"lang","op":"eq","value":"ja"}]"#);
        let bound = bind_filter_where_predicates(&items, &predicate_schema()).expect("bind ok");
        assert_eq!(
            bound,
            vec![WherePredicate::Equality {
                column: "lang".to_string(),
                value: "ja".to_string(),
            }]
        );
    }

    #[test]
    fn eq_boolean_maps_to_where_predicate_bool_equality() {
        let items = filter_items(r#"[{"column":"active","op":"eq","value":true}]"#);
        let bound = bind_filter_where_predicates(&items, &predicate_schema()).expect("bind ok");
        assert_eq!(
            bound,
            vec![WherePredicate::BoolEquality {
                column: "active".to_string(),
                value: true,
            }]
        );
    }

    #[test]
    fn eq_bytea_maps_to_where_predicate_equality_with_hex_text() {
        // base64("\x01\x02") == "AQI="
        let items = filter_items(r#"[{"column":"note","op":"eq","value":"AQI="}]"#);
        let bound = bind_filter_where_predicates(&items, &predicate_schema()).expect("bind ok");
        assert_eq!(
            bound,
            vec![WherePredicate::Equality {
                column: "note".to_string(),
                value: typed_json::bytea_literal_text("AQI=").expect("valid base64"),
            }]
        );
    }

    #[test]
    fn eq_numeric_json_number_maps_to_where_predicate_equality_with_literal_text() {
        let items = filter_items(r#"[{"column":"amount","op":"eq","value":1.5}]"#);
        let bound = bind_filter_where_predicates(&items, &predicate_schema()).expect("bind ok");
        assert_eq!(
            bound,
            vec![WherePredicate::Equality {
                column: "amount".to_string(),
                value: "1.5".to_string(),
            }]
        );
    }

    #[test]
    fn eq_on_integer_column_is_rejected_before_where_predicate_mapping() {
        let items = filter_items(r#"[{"column":"count","op":"eq","value":1}]"#);
        let err = bind_filter_where_predicates(&items, &predicate_schema()).expect_err("reject");
        assert!(matches!(err, FilterError::NumericFilterNotSupported));
    }

    #[test]
    fn unknown_column_is_rejected_before_where_predicate_mapping() {
        let items = filter_items(r#"[{"column":"missing","op":"eq","value":"x"}]"#);
        let err = bind_filter_where_predicates(&items, &predicate_schema()).expect_err("reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn multiple_elements_preserve_declaration_order_for_and_conjunction() {
        let items = filter_items(
            r#"[{"column":"lang","op":"eq","value":"ja"},{"column":"path","op":"prefix","value":"src/"}]"#,
        );
        let bound = bind_filter_where_predicates(&items, &predicate_schema()).expect("bind ok");
        assert_eq!(
            bound,
            vec![
                WherePredicate::Equality {
                    column: "lang".to_string(),
                    value: "ja".to_string(),
                },
                WherePredicate::Prefix {
                    column: "path".to_string(),
                    pattern: "src/%".to_string(),
                },
            ]
        );
    }

    #[test]
    fn unsupported_operator_for_predicate_dml_reports_narrower_message_than_general_filter() {
        // codex-review P2 指摘対応（PR #1121）: 述語形 DML は `eq`／`prefix` の
        // 2 語彙しか許可しないため、`FilterError::UnsupportedOperator`（`search`／
        // `scan`／`aggregate` 用。`lt`／`in` 等も許可語彙に含む文言）を誤って
        // 案内しないことを確認する。
        let items = filter_items(r#"[{"column":"lang","op":"lt","value":"ja"}]"#);
        let err = bind_filter_where_predicates(&items, &predicate_schema()).expect_err("reject");
        assert!(matches!(
            err,
            FilterError::UnsupportedOperatorForPredicateDml
        ));
        assert_eq!(err.wire_code(), "42601");
        let message = err.client_message();
        assert!(message.contains("eq") && message.contains("prefix"));
        assert!(!message.contains("\"lt\"") && !message.contains("\"in\""));
    }
}
