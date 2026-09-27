//! `POST /v1/query` の `filter` 配列（`search`／`scan`／`aggregate` 共通。
//! [`super::schema::FILTER_ITEM_SCHEMA`] で形の検証済み）を
//! `engine::declarative_filter::DeclarativeFilter` へ写像し、
//! `engine::declarative_filter::bind_all` を通して束縛済み `MetadataFilter` 列を
//! 得るモジュール（Issue #761・TASK-175・対象ビヘイビア NOSQL-7。Issue #896・
//! NOSQL-17 で列型別の値レーンへ拡張。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-7・EXT-3・SQL-2）。
//!
//! 責務境界: 受理する `op` は `eq`・`prefix` の 2 語彙のみで、複数要素は常に
//! AND 結合として扱う（`or`・否定・範囲比較の構文はそもそも受理形に存在
//! しない）。`value` は文字列・数値・真偽値のいずれか（[`super::schema::
//! FieldType::Scalar`]）を受理し、対象列の型に応じて [`bind_filter`] が
//! レーンを振り分ける（列型は `schema` が届くまで確定しないため、`op`・
//! `column`（RLS 述語名）・件数上限のみを [`map_filter_items`] で
//! スキーマ非依存に検査し、値の型別解釈は [`bind_filter`]（`schema` 必須）
//! に集約する）。
//!
//! 型別レーン（`eq`。`prefix` は従来どおり `TEXT` 列限定のまま変更なし）:
//! - `TEXT`（値が非文字列は `42601`。本 Issue 導入前からの既存契約を維持
//!   ——insert/update の「TEXT は旧来型 = `22000`」非対称は filter には
//!   適用しない）・`ENUM`（値が非文字列は `42601`。語彙外ラベルは
//!   `engine::declarative_filter::bind` が
//!   `22P02` で判定する）→ [`DeclarativeFilter::equals`]
//! - `BOOLEAN`（値が非真偽値は `42601`）→ [`DeclarativeFilter::bool_equals`]
//! - `DATE`／`TIMESTAMP`／`UUID`（値が非文字列は `42601`。形式・範囲検証は
//!   engine 側〔`bind_datetime_literal`／`bind_uuid_literal`〕へ委譲）→
//!   [`DeclarativeFilter::compare`]（`CompareOp::Eq`）
//! - `BYTEA`（値が base64 の JSON string でない、または不正な base64 は
//!   `42601`。長すぎは `54000`。復号後は
//!   [`super::typed_json::bytea_literal_text`] で hex テキストへ再エンコード
//!   してから engine の `\x` hex 解析経路へ渡す）→ `compare`
//! - `NUMERIC`（数値または数値文字列。他は `42601`。桁あふれ・形式不正は
//!   engine 側へ委譲）→ `compare`／[`DeclarativeFilter::compare_numeric_literal`]
//! - `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION`（`eq`）は対象外
//!   （SQL 表層でも式レーン〔`udf_call::bind_expr`〕経由でしか扱えず、
//!   `BoundStatement`／`PlanSearchBinding` に `expr_filters` を渡す入口が
//!   無いため。`0A000` で拒否し Issue #945 へ申し送る）
//! - `VECTOR`／`ARRAY`／`JSON`／`JSONB` への `eq`／`prefix` は SQL 表層にも
//!   eq レーンが無いため、従来どおり `engine::declarative_filter::bind` の
//!   「`TEXT` 列でない」判定（`22000`）へ委譲する
//!
//! 未知列は列名だけで完結する判定のため値の種類に関わらず
//! `DeclarativeFilter::equals(column, "")` を engine へ渡し「unknown
//! column」（`22000`）を得る（`bind_impl` が値の解釈より先に列解決を行う
//! 契約に依拠）。
//!
//! SQL 表層（`sql::allowlist::Parser::parse_where`・
//! `sql::parser::bind_where_predicates`）と**同一の** `declarative_filter` 束縛へ
//! 委譲する第 2 の実行器は作らない。
//!
//! 呼び出し文脈: `scan`／`search`／`aggregate` の各 op ハンドラが `schema` が
//! 届く束縛 closure の内側で [`bind_filter`] を呼ぶ（`scan.rs` は Issue #896
//! で二段階構成〔schema 非依存の宣言 → 後段で `bind_all`〕から、`search.rs`・
//! `aggregate.rs` と同じ単一段構成〔closure 内で `bind_filter` を直接呼ぶ〕へ
//! 揃えた。型別レーンの振り分けに `schema` が必須のため）。
//!
//! 対象外: op 別ハンドラからの呼び出し結線・実行そのもの、HTTP 応答へのエラー
//! 射影（`http::status`／`http::error_body` が別途 `ErrorClass` から写像する）。

use engine::catalog::{ColumnType, TableSchema};
use engine::declarative_filter::{self, CompareOp, DeclarativeFilter, MetadataFilter};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::sql::allowlist::{is_allowed_where_predicate_name, SqlSurfaceError, WherePredicate};

use super::schema::{SchemaError, Validated, FILTER_ITEM_SCHEMA};
use super::typed_json::{self, TypedJsonError};

/// [`map_filter_items`]／[`bind_filter`] の失敗を表す。いずれも
/// [`ClassifiedError`] を実装し、`wire_code`／`client_message` を経由して
/// HTTP エラー応答へ射影される（呼び出し元が `to_string()` を使わない契約は
/// `SqlSurfaceError::client_message` の既存ドキュメントと同じ）。
#[derive(Debug, Clone)]
pub enum FilterError {
    /// `op` が許可語彙（`eq`／`prefix`）に厳密一致しない（大文字小文字読み替え
    /// なし。`or`・否定・範囲比較を含む語彙外の値はすべてここに落ちる）。
    /// untrusted な `op` 文字列は文言へ含めない固定文言（`security.md`
    /// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」対応。
    /// [`super::schema::SchemaError`] が `UnknownKey` でキー名を保持しないのと
    /// 同じ判断）。
    UnsupportedOperator,
    /// `column` が RLS 述語名（`is_allowed_where_predicate_name` が真。
    /// 例: `visible`／`visible()`、大文字小文字非区別）と一致した。RLS は
    /// サーバー側暗黙適用のみであり、`filter` 経由でクライアントが RLS 述語を
    /// 指定・解除できる経路を作らない（`security.md` P0「テナント境界」）。
    RlsPredicateNotAllowed,
    /// `filter` 要素が [`FILTER_ITEM_SCHEMA`] の形（必須キー・型）に適合しない。
    Shape(SchemaError),
    /// `engine::declarative_filter` 側の束縛エラー（件数上限超過 `54000`・
    /// 未知列／`TEXT` 列でない／空 prefix／ENUM 語彙外／`DATE`/`TIMESTAMP`/
    /// `UUID`/`NUMERIC` の形式・範囲エラー等）をそのまま透過する。
    Bind(SqlSurfaceError),
    /// `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への `eq`
    /// （`0A000`。SQL 表層にも式レーン以外の入口が無く本 Issue の対象外。
    /// Issue #945 へ申し送り）。
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
            FilterError::UnsupportedOperator | FilterError::RlsPredicateNotAllowed => {
                ErrorClass::UnsupportedSqlSyntax
            }
            FilterError::Shape(err) => err.error_class(),
            FilterError::Bind(err) => err.error_class(),
            FilterError::NumericFilterNotSupported => ErrorClass::FeatureNotSupported,
            FilterError::Value(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            FilterError::UnsupportedOperator => {
                "unsupported filter operator (only \"eq\" and \"prefix\" are allowed)".to_string()
            }
            FilterError::RlsPredicateNotAllowed => {
                "filter column must not reference an RLS predicate name".to_string()
            }
            FilterError::Shape(err) => err.client_message(),
            FilterError::Bind(err) => err.client_message(),
            FilterError::NumericFilterNotSupported => {
                "eq filter on INTEGER/BIGINT/REAL/DOUBLE PRECISION columns is not supported yet"
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
            // `UnsupportedOperator`／`RlsPredicateNotAllowed`／
            // `TypeMismatch`／`InvalidBytea` はいずれもここに到達する。
            _ => SqlSurfaceError::UnsupportedSyntax { detail },
        }
    }
}

/// `column` が RLS 述語呼び出し形の許可名（現状 `visible`）に一致するかを、
/// 末尾の `()` の有無を問わず大文字小文字非区別で判定する。
/// `engine::sql::allowlist::is_allowed_where_predicate_name` は述語名そのもの
/// （`()` を含まない）を受け取る契約のため、NoSQL 表層の `column` 文字列に
/// クライアントが `"visible()"` のように書いた場合も同じ判定へ正規化する。
fn is_rls_predicate_column(column: &str) -> bool {
    let name = column.strip_suffix("()").unwrap_or(column);
    is_allowed_where_predicate_name(name)
}

/// `filter` 配列の要素 1 件。列型に依存する値の解釈は `schema` が届くまで
/// 決定できないため、形（`column`／`op`／`value` の型・RLS 述語名・`op`
/// 語彙）だけをここで確定し、値そのもの（`&'a JsonValue`）は借用のまま
/// [`bind_filter`] へ引き継ぐ。
/// フィールドは非公開のまま構造体自体を `pub` にする（層 A 結合テスト
/// `nosql7_filter_mapping.rs` が [`map_filter_items`] を「`op` 語彙外は
/// `bind_filter` を待たず拒否する」ことの検証にのみ使う。中身の検査は
/// 提供しない——Rust の「非公開型を公開 API に露出しない」制約〔E0446〕を
/// 満たしつつ、値の取り出しは [`bind_filter`] 経由に限定する設計）。
#[derive(Debug)]
pub struct FilterItem<'a> {
    column: &'a str,
    op: &'a str,
    value: &'a JsonValue,
}

/// `filter` 配列の要素 1 件を [`FilterItem`] へ写像する。`op` は厳密一致
/// （trim・大文字小文字読み替えなし）で判定し、`eq`／`prefix` 以外はすべて
/// [`FilterError::UnsupportedOperator`] とする。
fn map_filter_item(item: &JsonValue) -> Result<FilterItem<'_>, FilterError> {
    // 上位スキーマ（呼び出し元の `optional_array` 経由）の検証結果を信頼し
    // きらず、要素単位でも再検証する（多層防御。`schema.rs` の
    // `check_element_type` が `ElementType::Object` 経由で既に検証している
    // 経路とは独立に、本モジュール単体で呼ばれても安全な契約にする）。
    let validated: Validated<'_> = FILTER_ITEM_SCHEMA.validate(item)?;
    let column = validated.required_str("column")?;
    let op = validated.required_str("op")?;
    let value = validated.required_scalar("value")?;

    if is_rls_predicate_column(column) {
        return Err(FilterError::RlsPredicateNotAllowed);
    }
    if op != "eq" && op != "prefix" {
        return Err(FilterError::UnsupportedOperator);
    }

    Ok(FilterItem { column, op, value })
}

/// `items`（`filter` 配列。[`Validated::optional_array`]`("filter")` が返す形）を
/// スキーマ非依存に検証済みの [`FilterItem`] 列へ写像する。
///
/// 件数検査（[`declarative_filter::check_filter_count`]、`54000`）を `Vec`
/// 確保・借用より**前**に行う（`.claude/rules/security.md`
/// 「不安全な設計｜無制限リソース確保（DoS）」対応。`items` は JSON パーサの
/// コンテナ要素数上限〔65,536〕までメモリ上に存在しうるため、写像前の件数
/// 検査は必須）。
pub fn map_filter_items(items: &[JsonValue]) -> Result<Vec<FilterItem<'_>>, FilterError> {
    declarative_filter::check_filter_count(items.len())?;
    let mut mapped = Vec::with_capacity(items.len());
    for item in items {
        mapped.push(map_filter_item(item)?);
    }
    Ok(mapped)
}

/// `item`（列型未確定の [`FilterItem`]）を `schema` 上の列型に応じた
/// [`DeclarativeFilter`] へ写像する（値の型別レーン振り分け本体。
/// モジュール doc の型別レーン一覧を参照）。未知列は列名だけで完結する
/// 判定のため、値の種類に関わらず `equals(column, "")` を経由して engine
/// 側の「unknown column」（`22000`）へ委譲する。
fn declare_one(
    item: &FilterItem<'_>,
    schema: &TableSchema,
) -> Result<DeclarativeFilter, FilterError> {
    let Some(column) = schema.columns.iter().find(|c| c.name == item.column) else {
        return Ok(DeclarativeFilter::equals(item.column, ""));
    };

    if item.op == "prefix" {
        // `prefix` は従来どおり `TEXT` 列限定。値の型不一致は本 Issue
        // （#896）導入前から `FILTER_ITEM_SCHEMA.value`（旧 `FieldType::
        // String`）が形状検証段階で `42601` として拒否していた契約と同じ
        // 分類を維持する（`TypeMismatch`。`value` を `Scalar` へ広げた後も
        // ここで判定するため wire_code は変わらない。insert/update の
        // 「TEXT は旧来型 = 22000」非対称は filter には適用しない——filter の
        // 既存契約はそもそも `42601` だったため）。他の列型は engine 側の
        // 「`TEXT` 列でない」判定（`22000`）へ委譲する（legacy 互換）。
        return match item.value {
            JsonValue::String(s) => Ok(DeclarativeFilter::starts_with(item.column, s.as_str())),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "prefix filter value must be a JSON string",
            ))),
        };
    }

    // ここから `op == "eq"`。
    match &column.ty {
        // `value` の型不一致は本 Issue（#896）導入前から `42601` だった
        // 契約を維持する（`prefix` と同じ理由。insert/update の
        // 「TEXT は旧来型 = 22000」非対称は filter には適用しない）。
        ColumnType::Text => match item.value {
            JsonValue::String(s) => Ok(DeclarativeFilter::equals(item.column, s.as_str())),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a TEXT column must be a JSON string",
            ))),
        },
        // ENUM の語彙照合は engine 側（`bind_impl`）が `22P02` で行う。
        ColumnType::Enum(_) => match item.value {
            JsonValue::String(s) => Ok(DeclarativeFilter::equals(item.column, s.as_str())),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for an ENUM column must be a JSON string",
            ))),
        },
        ColumnType::Boolean => match item.value {
            JsonValue::Bool(b) => Ok(DeclarativeFilter::bool_equals(item.column, *b)),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a BOOLEAN column must be a JSON boolean",
            ))),
        },
        ColumnType::Date | ColumnType::Timestamp | ColumnType::Uuid => match item.value {
            JsonValue::String(s) => Ok(DeclarativeFilter::compare(
                item.column,
                CompareOp::Eq,
                s.as_str(),
            )),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a DATE/TIMESTAMP/UUID column must be a JSON string",
            ))),
        },
        ColumnType::Bytea => match item.value {
            JsonValue::String(s) => {
                let hex = typed_json::bytea_literal_text(s).map_err(FilterError::Value)?;
                Ok(DeclarativeFilter::compare(item.column, CompareOp::Eq, hex))
            }
            _ => Err(FilterError::Value(TypedJsonError::InvalidBytea(
                "eq filter value for a BYTEA column must be a base64 JSON string",
            ))),
        },
        ColumnType::Numeric { .. } => match item.value {
            JsonValue::Number(n) => Ok(DeclarativeFilter::compare_numeric_literal(
                item.column,
                CompareOp::Eq,
                typed_json::number_literal_text(n),
            )),
            JsonValue::String(s) => Ok(DeclarativeFilter::compare(
                item.column,
                CompareOp::Eq,
                s.as_str(),
            )),
            _ => Err(FilterError::Value(TypedJsonError::TypeMismatch(
                "eq filter value for a NUMERIC column must be a JSON number or numeric string",
            ))),
        },
        // 式レーン（`udf_call::bind_expr`）の入口が `BoundStatement`／
        // `PlanSearchBinding` に無いため対象外（Issue #945）。
        ColumnType::Integer | ColumnType::BigInt | ColumnType::Real | ColumnType::Double => {
            Err(FilterError::NumericFilterNotSupported)
        }
        // SQL 表層にも eq レーンが無い列型は、従来どおり engine 側の
        // 「`TEXT` 列でない」判定（`22000`）へ委譲する（legacy 互換）。
        ColumnType::Vector(_) | ColumnType::Array(_) | ColumnType::Json | ColumnType::Jsonb => {
            Ok(DeclarativeFilter::equals(item.column, ""))
        }
    }
}

/// [`map_filter_items`] の結果を `schema` へ束縛し、SQL 表層の
/// `WHERE <col> = <lit> AND <col> LIKE '<prefix>%'` と同一の
/// `Vec<MetadataFilter>` を得る（宣言順を保った AND 結合）。
pub fn bind_filter(
    items: &[JsonValue],
    schema: &TableSchema,
) -> Result<Vec<MetadataFilter>, FilterError> {
    let mapped = map_filter_items(items)?;
    let mut bound = Vec::with_capacity(mapped.len());
    for item in &mapped {
        let declared = declare_one(item, schema)?;
        bound.push(declared.bind(schema)?);
    }
    Ok(bound)
}

/// `LIKE` パターンのメタ文字（`\`・`%`・`_`。[`engine::declarative_filter::
/// parse_like_pattern`] が解釈する 3 文字）をエスケープし、`raw` を無加工の
/// 前方一致対象文字列として扱えるようにする（[`bind_filter_where_predicates`]
/// の `prefix` レーン専用。`declare_one` が `DeclarativeFilter::starts_with`
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

/// `item`（列型未確定の [`FilterItem`]）を、SQL 表層の述語形 `UPDATE`／
/// `DELETE`（[`engine::sql::allowlist::WherePredicate`]）が使うのと同一の
/// 構文形へ写像する（[`bind_filter_where_predicates`] 専用）。呼び出し元が
/// 先に `declare_one(item, schema)?.bind(schema)?`（[`bind_filter`] と同じ
/// 検証経路）を通していることを前提とし、本関数自身は値の型検査をしない
/// （検証済みの `(op, 列型)` の組み合わせのみが渡る契約。想定外の組み合わせは
/// `Internal` で fail-closed に落とす）。
fn where_predicate_for(
    item: &FilterItem<'_>,
    column: &engine::catalog::ColumnDef,
) -> Result<WherePredicate, FilterError> {
    if item.op == "prefix" {
        // `declare_one` が `JsonValue::String` であることを既に検証済み
        // （呼び出し元契約。想定外の値種別は多層防御として fail-closed に拒否する）。
        let raw = match item.value {
            JsonValue::String(s) => s.as_str(),
            _ => {
                return Err(FilterError::Bind(SqlSurfaceError::Internal {
                    detail: "unexpected non-string value for a validated prefix filter".to_string(),
                }))
            }
        };
        return Ok(WherePredicate::Prefix {
            column: item.column.to_string(),
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
    match &column.ty {
        ColumnType::Boolean => match item.value {
            JsonValue::Bool(b) => Ok(WherePredicate::BoolEquality {
                column: item.column.to_string(),
                value: *b,
            }),
            _ => Err(internal()),
        },
        ColumnType::Bytea => match item.value {
            JsonValue::String(s) => {
                let text = typed_json::bytea_literal_text(s).map_err(FilterError::Value)?;
                Ok(WherePredicate::Equality {
                    column: item.column.to_string(),
                    value: text,
                })
            }
            _ => Err(internal()),
        },
        ColumnType::Numeric { .. } => match item.value {
            JsonValue::Number(n) => Ok(WherePredicate::Equality {
                column: item.column.to_string(),
                value: typed_json::number_literal_text(n),
            }),
            JsonValue::String(s) => Ok(WherePredicate::Equality {
                column: item.column.to_string(),
                value: s.clone(),
            }),
            _ => Err(internal()),
        },
        // TEXT／ENUM／DATE／TIMESTAMP／UUID はいずれも文字列リテラル形
        // （`declare_one` が検証済み）。
        _ => match item.value {
            JsonValue::String(s) => Ok(WherePredicate::Equality {
                column: item.column.to_string(),
                value: s.clone(),
            }),
            _ => Err(internal()),
        },
    }
}

/// [`map_filter_items`] の結果を `schema` へ束縛し、SQL 表層の述語形
/// `UPDATE`／`DELETE`（`WHERE <col> = <lit> AND <col> LIKE '<prefix>%'`）が
/// 使うのと同一の `Vec<WherePredicate>` を得る（NoSQL 表層 `update`／`delete`
/// op の `filter`。TASK-186・NOSQL-12、Issue #1062。宣言順を保った `AND`
/// 結合）。[`bind_filter`]（`MetadataFilter` を返す。`scan`／`search`／
/// `aggregate` 専用）とは戻り値の型が異なる別 API——述語形 DML の
/// `content_hash`（[`engine::recovery::content_hash::for_update_where`]・
/// `for_delete_where`）は束縛前の構文形 `WherePredicate` をハッシュ源にする
/// ため、SQL 表層と同一のバイト列を再現するには `WherePredicate` そのものが
/// 必要（`MetadataFilter` へ変換してしまうと SQL⇄NoSQL 間の台帳照合
/// （`23505`／`22023`、RECOVER-10）が成立しなくなる）。
///
/// 各要素は [`declare_one`]`(item, schema)?.bind(schema)?`（[`bind_filter`]
/// と完全に同じ検証経路。未知列・非 TEXT 列への `prefix`・ENUM 語彙外・
/// `INTEGER` 系列への `eq`・値の型不一致・件数上限はすべて `bind_filter` と
/// 同じ分類になる）を通してから [`where_predicate_for`] で構文形へ変換する
/// （検証してから変換する不変条件。変換器は値の型検査をしない）。
pub fn bind_filter_where_predicates(
    items: &[JsonValue],
    schema: &TableSchema,
) -> Result<Vec<WherePredicate>, FilterError> {
    let mapped = map_filter_items(items)?;
    let mut predicates = Vec::with_capacity(mapped.len());
    for item in &mapped {
        let declared = declare_one(item, schema)?;
        // `bind_filter` と同一の検証（未知列・型不一致・件数上限等）を通す。
        // 束縛結果（`MetadataFilter`）自体は使わない——構文形への変換は
        // 独立した `where_predicate_for` が担う。
        let _ = declared.bind(schema)?;
        let Some(column) = schema.columns.iter().find(|c| c.name == item.column) else {
            // 未知列は `declare_one`／`bind` が先に `22000` で拒否済みのため
            // 到達しない（fail-closed フォールバック）。
            return Err(FilterError::Bind(SqlSurfaceError::Internal {
                detail: "unexpected unknown column after successful filter bind".to_string(),
            }));
        };
        predicates.push(where_predicate_for(item, column)?);
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

    fn filter_items(json: &str) -> Vec<JsonValue> {
        let JsonValue::Array(items) = parse_json(json).expect("valid JSON") else {
            panic!("expected array");
        };
        items
    }

    #[test]
    fn eq_maps_to_declarative_equals() {
        let items = filter_items(r#"[{"column":"lang","op":"eq","value":"ja"}]"#);
        let bound = bind_filter(&items, &schema()).expect("bind ok");
        assert_eq!(
            bound,
            declarative_filter::bind_all(&[DeclarativeFilter::equals("lang", "ja")], &schema())
                .expect("bind ok")
        );
    }

    #[test]
    fn prefix_maps_to_declarative_starts_with() {
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":"src/"}]"#);
        let bound = bind_filter(&items, &schema()).expect("bind ok");
        assert_eq!(
            bound,
            declarative_filter::bind_all(
                &[DeclarativeFilter::starts_with("path", "src/")],
                &schema()
            )
            .expect("bind ok")
        );
    }

    #[test]
    fn prefix_value_wildcards_are_kept_literal() {
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":"a%b_c"}]"#);
        let bound = bind_filter(&items, &schema()).expect("bind ok");
        assert_eq!(
            bound,
            declarative_filter::bind_all(
                &[DeclarativeFilter::starts_with("path", "a%b_c")],
                &schema()
            )
            .expect("bind ok")
        );
    }

    #[test]
    fn multiple_items_preserve_order_as_and() {
        let items = filter_items(
            r#"[{"column":"lang","op":"eq","value":"ja"},{"column":"path","op":"prefix","value":"src/"}]"#,
        );
        let bound = bind_filter(&items, &schema()).expect("bind ok");
        assert_eq!(
            bound,
            declarative_filter::bind_all(
                &[
                    DeclarativeFilter::equals("lang", "ja"),
                    DeclarativeFilter::starts_with("path", "src/"),
                ],
                &schema()
            )
            .expect("bind ok")
        );
    }

    #[test]
    fn empty_array_binds_to_empty_vec() {
        let items = filter_items("[]");
        assert_eq!(bind_filter(&items, &schema()).expect("bind ok"), vec![]);
    }

    #[test]
    fn unsupported_operators_are_rejected() {
        for op in [
            "or", "not", "neq", "gt", "lt", "gte", "lte", "between", "range", "in", "EQ", "Prefix",
            "visible",
        ] {
            let items = filter_items(&format!(r#"[{{"column":"lang","op":"{op}","value":"x"}}]"#));
            let err = bind_filter(&items, &schema()).expect_err("must reject");
            assert!(matches!(err, FilterError::UnsupportedOperator));
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn rls_predicate_columns_are_rejected() {
        for column in ["visible", "VISIBLE", "visible()", "Visible()"] {
            let items = filter_items(&format!(
                r#"[{{"column":"{column}","op":"eq","value":"x"}}]"#
            ));
            let err = bind_filter(&items, &schema()).expect_err("must reject");
            assert!(matches!(err, FilterError::RlsPredicateNotAllowed));
            assert_eq!(err.wire_code(), "42601");
        }
    }

    #[test]
    fn malformed_shape_is_rejected() {
        // 非オブジェクト要素。
        let items = filter_items(r#"["not-an-object"]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));
        assert_eq!(err.wire_code(), "42601");

        // 欠落キー。
        let items = filter_items(r#"[{"column":"lang","op":"eq"}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));

        // 未知キー。
        let items = filter_items(r#"[{"column":"lang","op":"eq","value":"ja","extra":"x"}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));

        // 型不一致（`value` がオブジェクト。Scalar は object/array/null を
        // 受理しない）。
        let items = filter_items(r#"[{"column":"lang","op":"eq","value":{}}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert!(matches!(err, FilterError::Shape(_)));
    }

    // `value` が Scalar（文字列・数値・真偽値）の形状検証は通過するが、
    // 対象列（TEXT）の型と噛み合わない場合は本 Issue（#896）導入前からの
    // 既存契約どおり `42601` のまま（insert/update の「TEXT は旧来型 =
    // `22000`」非対称は filter には適用しない。`FilterError::Value(
    // TypedJsonError::TypeMismatch)` 経由）。
    #[test]
    fn eq_rejects_non_string_value_for_text_column_with_42601() {
        let items = filter_items(r#"[{"column":"lang","op":"eq","value":1}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert!(matches!(
            err,
            FilterError::Value(TypedJsonError::TypeMismatch(_))
        ));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn prefix_rejects_non_string_value_with_42601() {
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":1}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert!(matches!(
            err,
            FilterError::Value(TypedJsonError::TypeMismatch(_))
        ));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn over_limit_count_is_rejected() {
        let items: Vec<JsonValue> = (0..=declarative_filter::MAX_METADATA_FILTERS)
            .map(|i| {
                let mut map = std::collections::BTreeMap::new();
                map.insert("column".to_string(), JsonValue::String("lang".to_string()));
                map.insert("op".to_string(), JsonValue::String("eq".to_string()));
                map.insert("value".to_string(), JsonValue::String(i.to_string()));
                JsonValue::Object(map)
            })
            .collect();
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn at_limit_count_is_accepted() {
        let items: Vec<JsonValue> = (0..declarative_filter::MAX_METADATA_FILTERS)
            .map(|i| {
                let mut map = std::collections::BTreeMap::new();
                map.insert("column".to_string(), JsonValue::String("lang".to_string()));
                map.insert("op".to_string(), JsonValue::String("eq".to_string()));
                map.insert("value".to_string(), JsonValue::String(i.to_string()));
                JsonValue::Object(map)
            })
            .collect();
        assert_eq!(
            bind_filter(&items, &schema()).expect("bind ok").len(),
            declarative_filter::MAX_METADATA_FILTERS
        );
    }

    #[test]
    fn bind_filter_rejects_unknown_column() {
        let items = filter_items(r#"[{"column":"nope","op":"eq","value":"x"}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_filter_rejects_vector_column() {
        let items = filter_items(r#"[{"column":"embedding","op":"eq","value":"x"}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_filter_rejects_empty_prefix() {
        let items = filter_items(r#"[{"column":"path","op":"prefix","value":""}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    // --- 新型（Issue #896）の eq レーン ------------------------------------

    #[test]
    fn eq_maps_boolean_column_to_bool_equals() {
        let items = filter_items(r#"[{"column":"active","op":"eq","value":true}]"#);
        let bound = bind_filter(&items, &schema()).expect("bind ok");
        assert_eq!(
            bound,
            declarative_filter::bind_all(
                &[DeclarativeFilter::bool_equals("active", true)],
                &schema()
            )
            .expect("bind ok")
        );
    }

    #[test]
    fn eq_rejects_non_boolean_value_for_boolean_column() {
        let items = filter_items(r#"[{"column":"active","op":"eq","value":"true"}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert!(matches!(
            err,
            FilterError::Value(TypedJsonError::TypeMismatch(_))
        ));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn eq_maps_date_column_via_compare() {
        let items = filter_items(r#"[{"column":"created","op":"eq","value":"2024-01-02"}]"#);
        let bound = bind_filter(&items, &schema()).expect("bind ok");
        assert_eq!(
            bound,
            declarative_filter::bind_all(
                &[DeclarativeFilter::compare(
                    "created",
                    CompareOp::Eq,
                    "2024-01-02"
                )],
                &schema()
            )
            .expect("bind ok")
        );
    }

    #[test]
    fn eq_maps_numeric_column_from_number_and_string() {
        let items = filter_items(r#"[{"column":"amount","op":"eq","value":12.34}]"#);
        let bound = bind_filter(&items, &schema()).expect("bind ok");
        let via_string = filter_items(r#"[{"column":"amount","op":"eq","value":"12.34"}]"#);
        let bound_via_string = bind_filter(&via_string, &schema()).expect("bind ok");
        assert_eq!(bound, bound_via_string);
    }

    #[test]
    fn eq_on_integer_column_is_feature_not_supported() {
        let items = filter_items(r#"[{"column":"count","op":"eq","value":1}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        assert!(matches!(err, FilterError::NumericFilterNotSupported));
        assert_eq!(err.wire_code(), "0A000");
    }

    /// `scan`／`search`／`aggregate` の束縛 closure（`Result<_, SqlSurfaceError>`）
    /// が要求する形へ写像したあとも `0A000` が保たれることを固定する
    /// （codex レビュー指摘。`SqlSurfaceError::FeatureNotSupported` variant
    /// 追加前は closure 境界を通る際に `42601` へ縮退していた）。
    #[test]
    fn eq_on_integer_column_stays_feature_not_supported_after_sql_surface_conversion() {
        let items = filter_items(r#"[{"column":"count","op":"eq","value":1}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
        let sql_err = err.into_sql_surface_error();
        assert_eq!(sql_err.wire_code(), "0A000");
    }

    #[test]
    fn prefix_on_boolean_column_is_rejected_as_not_a_text_column() {
        let items = filter_items(r#"[{"column":"active","op":"prefix","value":"t"}]"#);
        let err = bind_filter(&items, &schema()).expect_err("must reject");
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
}
