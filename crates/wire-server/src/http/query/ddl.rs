//! `POST /v1/query` の `create_table`／`alter_table`／`drop_table` op を
//! SQL 表層の DDL（SQL-23）と**同一の実行器**
//! （`engine::core::EngineCore::execute_parsed_in_session`）へ、SQL テキストを
//! 組み立てずトークン列として到達させるモジュール（Issue #910・NOSQL-13・
//! TASK-207。ポインタ: `docs/spec/05-tasks.md` TASK-207・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-13・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23）。
//!
//! ## 設計（第 2 の DDL 実行器・第 2 の権限判定を作らない）
//!
//! JSON の各フィールドを `engine::sql::lexer::Token` へ直接写像し、
//! `engine::sql::allowlist::{validate_create_table_tokens,
//! validate_alter_table_tokens, validate_drop_table_tokens}`（crate 外部公開。
//! Issue #910 で pub 化）へそのまま渡す。これは `EngineCore::
//! parse_sql_prepared`／`bind_prepared`（拡張クエリプロトコルの `$n` を実値の
//! `Token::StringLiteral` へ置換したトークン列を「SQL テキストへ戻さず」
//! パーサーへ渡す設計）と同じ前例に倣う——文字列連結ではなく構造化トークン列
//! を渡すことで、SQL 表層が適用する構造検証（予約列名・列名重複・列数／
//! 制約数上限・PRIMARY KEY／UNIQUE／FOREIGN KEY の finalize 処理等）を
//! すべてそのまま享受し、複製しない。
//!
//! `validate_*_tokens` はいずれもカタログ照会を行わない契約（SQL 表層と
//! 同じ）。DDL 実行権限ゲート（`engine::sql::ddl::require_ddl_permission`）・
//! カタログ照会を含む実行本体は、[`execute_create_table`]・
//! [`execute_alter_table`]・[`execute_drop_table`] が `ParsedSql` を包んで渡す
//! `EngineCore::execute_parsed_in_session` 単独が担う。本モジュールに第 2 の
//! 権限判定は存在しない。
//!
//! ## セッションへの DDL 実行権限の受け渡し
//!
//! `principal.ddl_allowed()`（`http::session::store::SessionStore` が
//! セッション発行時に `UserStore::is_ddl_allowed` から確定させた値。
//! `http::session::issue` モジュール doc 参照）が真のときのみ
//! `SessionState::allow_ddl()` を呼ぶ。権限を持たない主体には、対象テーブル
//! の有無にかかわらず常に `42501`（`require_ddl_permission` がカタログ照会
//! より前に判定するため。`ParsedSql::CreateTable` 等のドキュメント参照）が
//! 返る——存在オラクルにならない。
//!
//! ## `alter_table.drop_column`（Issue #1167）
//!
//! SQL 表層の `ALTER TABLE ... DROP COLUMN` と同じ入口（トークン列 →
//! `validate_alter_table_tokens` → `run_ddl`）へ結線する。権限判定・依存検査・
//! エラー写像は engine 側に一本化され、NoSQL 側に第 2 の判定を持たない。
//! `ALTER COLUMN TYPE` 相当の op は未提供（別論点）。
//!
//! ## `create_table.constraints[].kind == "check"`（Issue #1199）
//!
//! `{"kind":"check","name":"<任意>","predicate":[<filter 葉形>, ...]}` を
//! 表制約 `[CONSTRAINT <name>] CHECK (<葉> AND <葉> ...)` のトークン列へ写像し、
//! SQL 表層の CHECK（TABLE-16・TASK-204）と同じ `validate_create_table_tokens`
//! → `run_ddl` へ合流させる。意味検証（列型との整合・既定名の確定・件数上限）と
//! 書き込み時の検査（違反は `23514`）は engine 側の単一の実装が担い、wire 側に
//! 型検査・評価器を持たない。
//!
//! - `predicate` の葉は `FILTER_ITEM_SCHEMA` 形（`column`／`op`／`value`）で、
//!   `op` は `eq`／`prefix`／`lt`／`le`(`lte`)／`gt`／`ge`(`gte`) のみ。`in`・`or`
//!   グループは engine も CHECK 内で拒否するため wire で先に `42601` とする。
//! - CREATE TABLE 時点ではスキーマが無いため、値トークンの種類は JSON 値の型で
//!   決める（`prefix` は `LIKE '<escaped>%'`、真偽値は `eq` のみ）。
//! - 表制約としてのみ生成するため既定名は `<table>_check` 系となる（SQL の列制約
//!   形の `<table>_<col>_check` とは既定名だけが異なる。意図した差分）。
//! - `kind` と矛盾するフィールド（check への `columns`／`references`、他 kind への
//!   `name`）は黙って無視せず `42601` で拒否する（fail-closed）。
//!
//! ## untrusted 入力の取り扱い
//! - 識別子（テーブル名・列名・制約参照列・参照先テーブル・ENUM 型名）は
//!   [`super::ident::check_identifier`]（長さ・文字種の事前フィルタ）を通した
//!   うえで、`engine::sql::lexer::tokenize` の結果が単一の `Token::Ident`
//!   （かつ原文と完全一致）であることを要求する（[`ident_token`]）。
//!   `Token::Ident` を直接組み立てない——lexer がキーワード化する語
//!   （`select`／`from` 等）を NoSQL だけで作れないようにするため。
//! - 数値（`dim`／`precision`／`scale`）は `Validated::required_u32` で取得
//!   した非負整数を 10 進テキストへ変換して `Token::Number` にする。
//! - `DEFAULT` の数値は [`super::typed_json::number_literal_text`] を再利用
//!   して正準テキスト化する（`parse_sql_prepared` の前例と同じ、独自の
//!   float 変換を作らない）。
//! - `references.on_delete`／`on_update`（Issue #1148・NOSQL-13）は
//!   [`referential_action_tokens`] の固定語彙 `match` だけで扱い、JSON 文字列
//!   値を `Token::Ident` へ直接流用しない。語彙外は
//!   [`DdlError::InvalidRequest`] で拒否する。
//! - エラー文言は固定英語文言のみとし、untrusted 値を echo しない
//!   （`super::op::UnsupportedOp` と同じ方針）。

use engine::core::{EngineCore, ParsedSql};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::sql::allowlist::{
    validate_alter_table_tokens, validate_create_table_tokens, validate_drop_table_tokens,
    SqlSurfaceError,
};
use engine::sql::lexer::{tokenize, Keyword, Token};
use engine::sql::mode::SessionState;

use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

use super::filter::{is_rls_predicate_column, like_escape};
use super::ident::{self, InvalidIdentifier};
use super::schema::{
    SchemaError, Validated, DDL_ADD_COLUMN_SCHEMA, DDL_COLUMN_SCHEMA, DDL_CONSTRAINT_SCHEMA,
    DDL_DROP_COLUMN_SCHEMA, DDL_REFERENCES_SCHEMA, FILTER_ITEM_SCHEMA,
};

/// 旧 `create_table` の CHECK 未実装応答文言。Issue #1199 で CHECK を実装した
/// ため、本クレート内では使用しない（公開 API 互換のため残置）。
#[deprecated(note = "CHECK constraints are supported via the NoSQL DDL surface (Issue #1199)")]
pub const CHECK_CONSTRAINT_UNAVAILABLE_MESSAGE: &str =
    "CHECK constraints are not available via the NoSQL DDL surface yet";

/// 形状は妥当だが意味的に受理できない DDL 要求への固定応答文言
/// （untrusted 値を含まない）。
pub const INVALID_DDL_REQUEST_MESSAGE: &str = "invalid DDL request";

/// [`execute_create_table`]・[`execute_alter_table`]・[`execute_drop_table`]
/// の失敗を表す。いずれも [`ClassifiedError`] を実装する（`delete.rs::
/// DeleteError` と同じ設計）。
#[derive(Debug, Clone)]
pub enum DdlError {
    /// [`Validated`] アクセサの型・キー不整合（多層防御）。
    Shape(SchemaError),
    /// 識別子として意味を持ちうる形状を満たさない。
    InvalidIdentifier,
    /// 形状は妥当だが意味的に受理できない要求（未知の型名・`vector` 列への
    /// `nullable: true`／`dim` 欠落・`DEFAULT` に SQL で表現できない値
    /// （`bool`／`null`／配列／オブジェクト）・`references.on_delete`／
    /// `on_update` の語彙外の値〔Issue #1148〕等）。
    InvalidRequest,
    /// 未実装形のための予約変種（現状、構築箇所なし。公開 API 互換のため残置）。
    FeatureNotSupported(&'static str),
    /// `engine::sql::allowlist::validate_*_tokens`／
    /// `EngineCore::execute_parsed_in_session` のエラー（DDL 実行権限不足
    /// `42501`・カタログ照会由来の `42P07`／`42P01`／`42701` 等）をそのまま
    /// 透過する。
    Engine(SqlSurfaceError),
}

impl From<SchemaError> for DdlError {
    fn from(err: SchemaError) -> Self {
        DdlError::Shape(err)
    }
}

impl From<InvalidIdentifier> for DdlError {
    fn from(_err: InvalidIdentifier) -> Self {
        DdlError::InvalidIdentifier
    }
}

impl From<SqlSurfaceError> for DdlError {
    fn from(err: SqlSurfaceError) -> Self {
        DdlError::Engine(err)
    }
}

impl ClassifiedError for DdlError {
    fn error_class(&self) -> ErrorClass {
        match self {
            DdlError::Shape(err) => err.error_class(),
            DdlError::InvalidIdentifier | DdlError::InvalidRequest => {
                ErrorClass::UnsupportedSqlSyntax
            }
            DdlError::FeatureNotSupported(_) => ErrorClass::FeatureNotSupported,
            DdlError::Engine(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            DdlError::Shape(err) => err.client_message(),
            DdlError::InvalidIdentifier => "invalid identifier".to_string(),
            DdlError::InvalidRequest => INVALID_DDL_REQUEST_MESSAGE.to_string(),
            DdlError::FeatureNotSupported(msg) => (*msg).to_string(),
            DdlError::Engine(err) => err.client_message(),
        }
    }
}

/// 固定の成功応答本文（3 op 共通。行数・件数を返さない。モジュール doc 参照）。
const OK_BODY: &str = "{\"ok\":true}";

/// `raw` を識別子として検証し、単一の `Token::Ident` として持ち出す
/// （モジュール doc「untrusted 入力の取り扱い」参照）。`Token::Ident` を
/// 直接組み立てない唯一の入口。
fn ident_token(raw: &str) -> Result<Token, DdlError> {
    ident::check_identifier(raw).map_err(DdlError::from)?;
    let tokens = tokenize(raw).map_err(|_| DdlError::InvalidRequest)?;
    match tokens.as_slice() {
        [Token::Ident(s)] if s == raw => Ok(Token::Ident(raw.to_string())),
        _ => Err(DdlError::InvalidRequest),
    }
}

/// 非負整数（`Validated::required_u32` 済み）を `Token::Number` へ変換する。
fn number_token(n: u32) -> Token {
    Token::Number(n.to_string())
}

/// `DEFAULT` に指定できる値（文字列・数値のみ。`bool`／`null`／配列／
/// オブジェクトは SQL の CREATE TABLE で表現できないため [`DdlError::
/// InvalidRequest`]）をトークン列へ写像する。負数は SQL の字句解析が
/// 符号を数値トークンに含めないため `Punct('-')` を独立して積む
/// （`sql::allowlist::Parser::expect_literal` と同じ設計）。
fn default_literal_tokens(value: &JsonValue) -> Result<Vec<Token>, DdlError> {
    match value {
        JsonValue::String(s) => {
            // lexer の文字列リテラル規則が拒否する制御文字（NUL 等）を含む
            // 値は往復不能なため、DEFAULT 未指定と区別できるよう明示的に拒否
            // する（`sql::allowlist::parse_check_predicate_text` と同じ
            // fail-closed 判断）。
            if s.chars().any(|c| c.is_control()) {
                return Err(DdlError::InvalidRequest);
            }
            Ok(vec![Token::StringLiteral(s.clone())])
        }
        JsonValue::Number(n) => {
            let text = super::typed_json::number_literal_text(n);
            if text.contains(['e', 'E']) {
                // 指数表記は `sql::lexer::lex_number` が受理しない形状のため
                // 構造的に拒否する（独自の指数展開は行わない）。
                return Err(DdlError::InvalidRequest);
            }
            match text.strip_prefix('-') {
                Some(rest) => Ok(vec![Token::Punct('-'), Token::Number(rest.to_string())]),
                None => Ok(vec![Token::Number(text)]),
            }
        }
        JsonValue::Bool(_) | JsonValue::Null | JsonValue::Array(_) | JsonValue::Object(_) => {
            Err(DdlError::InvalidRequest)
        }
    }
}

/// `Validated::required_scalar` の「欠落」を `None` として読み替える
/// （`default` フィールドの Optional 性を `Validated` の型付きアクセサへ
/// 素直に対応させるための局所ヘルパー。`schema.rs::Validated` 自体は変更
/// しない——`FieldType::Scalar` の Optional フィールドは本モジュールにしか
/// 現れないため）。
fn optional_scalar<'a>(
    v: &Validated<'a>,
    key: &'static str,
) -> Result<Option<&'a JsonValue>, SchemaError> {
    match v.required_scalar(key) {
        Ok(val) => Ok(Some(val)),
        Err(SchemaError::MissingRequired { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `create_table.columns[*]` の受理型名（SQL 表層の CREATE TABLE と同じ
/// 集合。モジュール doc 参照）を型トークン列へ写像する。戻り値の `bool` は
/// `VECTOR` 列か（`UNIQUE`／`DEFAULT`／`nullable: true` が使えない）を表す。
/// `build_add_column_type_tokens`（`alter_table.add_column`）と同じく
/// ASCII 大文字小文字を無視して判定する——`create_table` だけ大文字小文字を
/// 区別すると `alter_table` で通る型名（例: `INTEGER`／`Text`）が
/// `create_table` では拒否される非対称が生じ、SQL 表層との型名受理集合の
/// パリティが崩れるため。
fn create_table_type_tokens(ty: &str, dim: Option<u32>) -> Result<(Vec<Token>, bool), DdlError> {
    let ty = ty.to_ascii_lowercase();
    match ty.as_str() {
        "text" => {
            if dim.is_some() {
                return Err(DdlError::InvalidRequest);
            }
            Ok((vec![Token::Ident("TEXT".to_string())], false))
        }
        "integer" => {
            if dim.is_some() {
                return Err(DdlError::InvalidRequest);
            }
            Ok((vec![Token::Ident("INTEGER".to_string())], false))
        }
        "bigint" => {
            if dim.is_some() {
                return Err(DdlError::InvalidRequest);
            }
            Ok((vec![Token::Ident("BIGINT".to_string())], false))
        }
        "vector" => {
            let dim = dim.ok_or(DdlError::InvalidRequest)?;
            Ok((
                vec![
                    Token::Ident("VECTOR".to_string()),
                    Token::Punct('('),
                    number_token(dim),
                    Token::Punct(')'),
                ],
                true,
            ))
        }
        _ => Err(DdlError::InvalidRequest),
    }
}

/// `create_table.columns[*]` 1 件をトークン列へ写像する（`<col> <type>
/// [NOT NULL] [DEFAULT <lit>]` の順。`sql::allowlist::Parser::
/// parse_column_constraints` は順序非依存だがこの順に固定して積む）。
fn build_column_tokens(item: &JsonValue) -> Result<Vec<Token>, DdlError> {
    let v = DDL_COLUMN_SCHEMA.validate(item).map_err(DdlError::from)?;
    let name = v.required_str("name").map_err(DdlError::from)?;
    let name_token = ident_token(name)?;
    let ty = v.required_str("type").map_err(DdlError::from)?;
    let dim = v.optional_u32("dim").map_err(DdlError::from)?;
    let nullable = v.optional_bool("nullable").map_err(DdlError::from)?;
    let default = optional_scalar(&v, "default").map_err(DdlError::from)?;

    let (type_tokens, is_vector) = create_table_type_tokens(ty, dim)?;

    let mut tokens = vec![name_token];
    tokens.extend(type_tokens);

    match nullable {
        Some(true) if is_vector => return Err(DdlError::InvalidRequest),
        Some(false) => {
            tokens.push(Token::Ident("NOT".to_string()));
            tokens.push(Token::Ident("NULL".to_string()));
        }
        _ => {}
    }

    if let Some(default_value) = default {
        if is_vector {
            return Err(DdlError::InvalidRequest);
        }
        let mut lit_tokens = default_literal_tokens(default_value)?;
        tokens.push(Token::Ident("DEFAULT".to_string()));
        tokens.append(&mut lit_tokens);
    }

    Ok(tokens)
}

/// `columns`（非空。`ObjectSchema` は空配列も型としては受理するため、ここで
/// 構造的な空を拒否する。`sql::allowlist::Parser::parse_unique_table_
/// constraint` 等と同じ「表制約の列リストは 1 件以上」規約）を
/// `(<col>[, <col>]*)` へ写像する。
fn push_column_list(tokens: &mut Vec<Token>, columns: &[JsonValue]) -> Result<(), DdlError> {
    if columns.is_empty() {
        return Err(DdlError::InvalidRequest);
    }
    tokens.push(Token::Punct('('));
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            tokens.push(Token::Punct(','));
        }
        let JsonValue::String(name) = col else {
            return Err(DdlError::InvalidRequest);
        };
        tokens.push(ident_token(name)?);
    }
    tokens.push(Token::Punct(')'));
    Ok(())
}

/// `references.on_delete`／`on_update` の固定語彙を `ON DELETE`／`ON UPDATE`
/// 句のトークン列（アクション部分のみ）へ写像する（Issue #1148・NOSQL-13。
/// SQL 表層の `REFERENCES ... ON DELETE ...` と同一のカタログ表現・連鎖適用
/// （`engine::constraint::propagate_referential_actions`）に合流させるための
/// NoSQL 側入口）。
///
/// untrusted な JSON 文字列値をそのまま `Token::Ident` へ転用せず、固定語彙の
/// `match` で固定のトークン列だけを積む（インジェクション防止。モジュール
/// doc の「untrusted 入力の取り扱い」参照）。大文字小文字の揺れは正規化せず
/// 完全一致のみ受理し、語彙外は [`DdlError::InvalidRequest`]（`42601`）で
/// 拒否する。
fn referential_action_tokens(raw: &str) -> Result<Vec<Token>, DdlError> {
    match raw {
        "no_action" => Ok(vec![
            Token::Ident("NO".to_string()),
            Token::Ident("ACTION".to_string()),
        ]),
        "restrict" => Ok(vec![Token::Ident("RESTRICT".to_string())]),
        "cascade" => Ok(vec![Token::Ident("CASCADE".to_string())]),
        "set_null" => Ok(vec![
            Token::Ident("SET".to_string()),
            Token::Ident("NULL".to_string()),
        ]),
        "set_default" => Ok(vec![
            Token::Ident("SET".to_string()),
            Token::Ident("DEFAULT".to_string()),
        ]),
        _ => Err(DdlError::InvalidRequest),
    }
}

/// CHECK 述語の比較演算子（`=`／`<`／`<=`／`>`／`>=`）の固定語彙写像。
/// `prefix`・`in` は本関数の対象外（呼び出し元が分岐する）。語彙外は
/// [`DdlError::InvalidRequest`]。JSON 文字列を `Token::Ident` へ流用しない。
fn check_compare_op_token(op: &str) -> Result<Token, DdlError> {
    match op {
        "eq" => Ok(Token::Punct('=')),
        "lt" => Ok(Token::Punct('<')),
        "gt" => Ok(Token::Punct('>')),
        "le" | "lte" => Ok(Token::Le),
        "ge" | "gte" => Ok(Token::Ge),
        _ => Err(DdlError::InvalidRequest),
    }
}

/// CHECK 述語の葉 1 件（`{column, op, value}`）をトークン列へ写像する
/// （[`build_check_tokens`] 専用。モジュール doc「CHECK」節参照）。
fn check_leaf_tokens(item: &JsonValue) -> Result<Vec<Token>, DdlError> {
    let v = FILTER_ITEM_SCHEMA.validate(item).map_err(DdlError::from)?;
    let column = v.required_str("column").map_err(DdlError::from)?;
    let op = v.required_str("op").map_err(DdlError::from)?;
    let value = v.required_scalar("value").map_err(DdlError::from)?;
    // `visible()` 等の RLS 述語名は CHECK の列にできない（多層防御。engine も拒否）。
    if is_rls_predicate_column(column) {
        return Err(DdlError::InvalidRequest);
    }
    let mut tokens = vec![ident_token(column)?];
    if op == "prefix" {
        let JsonValue::String(raw) = value else {
            return Err(DdlError::InvalidRequest);
        };
        if raw.chars().any(|c| c.is_control()) {
            return Err(DdlError::InvalidRequest);
        }
        tokens.push(Token::Ident("LIKE".to_string()));
        tokens.push(Token::StringLiteral(format!("{}%", like_escape(raw))));
        return Ok(tokens);
    }
    let op_token = check_compare_op_token(op)?;
    match value {
        JsonValue::Bool(b) => {
            // 真偽値は等価比較のみ（BoolEquality 形）。
            if op != "eq" {
                return Err(DdlError::InvalidRequest);
            }
            tokens.push(op_token);
            tokens.push(Token::Ident(if *b { "true" } else { "false" }.to_string()));
        }
        other => {
            tokens.push(op_token);
            tokens.extend(default_literal_tokens(other)?);
        }
    }
    Ok(tokens)
}

/// `kind == "check"` の制約をトークン列へ写像する（Issue #1199・NOSQL-13・
/// TABLE-16）。`columns`／`references` の混入と、空・欠落の `predicate` は
/// fail-closed で拒否する。
fn build_check_tokens(
    v: &Validated<'_>,
    map: &std::collections::BTreeMap<String, JsonValue>,
) -> Result<Vec<Token>, DdlError> {
    if map.contains_key("columns") || map.contains_key("references") {
        return Err(DdlError::InvalidRequest);
    }
    let predicate = v
        .optional_array("predicate")
        .map_err(DdlError::from)?
        .ok_or(DdlError::InvalidRequest)?;
    if predicate.is_empty() {
        return Err(DdlError::InvalidRequest);
    }
    // 確保前に件数上限を検査する（DoS 対策。`54000`）。
    engine::declarative_filter::check_filter_count(predicate.len()).map_err(DdlError::from)?;
    let name = v.optional_str("name").map_err(DdlError::from)?;

    let mut tokens = Vec::new();
    if let Some(name) = name {
        tokens.push(Token::Ident("CONSTRAINT".to_string()));
        tokens.push(ident_token(name)?);
    }
    tokens.push(Token::Ident("CHECK".to_string()));
    tokens.push(Token::Punct('('));
    for (i, leaf) in predicate.iter().enumerate() {
        if i > 0 {
            tokens.push(Token::Keyword(Keyword::And));
        }
        tokens.extend(check_leaf_tokens(leaf)?);
    }
    tokens.push(Token::Punct(')'));
    Ok(tokens)
}

/// `create_table.constraints[*]` 1 件をトークン列へ写像する（`primary_key`／
/// `unique`／`foreign_key`／`check`）。
fn build_constraint_tokens(item: &JsonValue) -> Result<Vec<Token>, DdlError> {
    let v = DDL_CONSTRAINT_SCHEMA
        .validate(item)
        .map_err(DdlError::from)?;
    let kind = v.required_str("kind").map_err(DdlError::from)?;
    let JsonValue::Object(map) = item else {
        // `DDL_CONSTRAINT_SCHEMA.validate` がルートオブジェクトであることを
        // 既に保証済みのため到達しない（多層防御）。
        return Err(DdlError::InvalidRequest);
    };

    match kind {
        "primary_key" | "unique" => {
            // `name`／`predicate` は `check` 専用（engine は `CONSTRAINT <name>` を
            // CHECK の前置としてのみ受理する）。黙って捨てず先に拒否する。
            if map.contains_key("name") || map.contains_key("predicate") {
                return Err(DdlError::InvalidRequest);
            }
            // `references` は `foreign_key` 専用フィールドだが
            // `DDL_CONSTRAINT_SCHEMA` は `kind` に関わらず形状として許容する
            // （意味検証は本関数が担う）。`kind` と矛盾する `references` を
            // 黙って無視すると、書き手が意図した外部キー制約が静かに
            // primary_key／unique として登録されてしまうため、fail-closed
            // 方針（曖昧な入力は拒否側に倒す）に従い明示的に拒否する。
            if map.contains_key("references") {
                return Err(DdlError::InvalidRequest);
            }
            let columns = v.required_array("columns").map_err(DdlError::from)?;
            let mut tokens = if kind == "primary_key" {
                vec![
                    Token::Ident("PRIMARY".to_string()),
                    Token::Ident("KEY".to_string()),
                ]
            } else {
                vec![Token::Ident("UNIQUE".to_string())]
            };
            push_column_list(&mut tokens, columns)?;
            Ok(tokens)
        }
        "foreign_key" => {
            if map.contains_key("name") || map.contains_key("predicate") {
                return Err(DdlError::InvalidRequest);
            }
            let columns = v.required_array("columns").map_err(DdlError::from)?;
            let references_raw = map.get("references").ok_or(DdlError::InvalidRequest)?;
            let refs_v = DDL_REFERENCES_SCHEMA
                .validate(references_raw)
                .map_err(DdlError::from)?;
            let parent_table = refs_v.required_str("table").map_err(DdlError::from)?;
            let parent_columns = refs_v.optional_array("columns").map_err(DdlError::from)?;
            let on_delete = refs_v.optional_str("on_delete").map_err(DdlError::from)?;
            let on_update = refs_v.optional_str("on_update").map_err(DdlError::from)?;

            let mut tokens = vec![
                Token::Ident("FOREIGN".to_string()),
                Token::Ident("KEY".to_string()),
            ];
            push_column_list(&mut tokens, columns)?;
            tokens.push(Token::Ident("REFERENCES".to_string()));
            tokens.push(ident_token(parent_table)?);
            if let Some(parent_columns) = parent_columns {
                push_column_list(&mut tokens, parent_columns)?;
            }
            // 生成順は `ON DELETE` → `ON UPDATE` に固定する（SQL パーサは
            // 順不同で受理するが、NoSQL からの写像は決定的にする。Issue
            // #1148）。省略時はトークンを生成せず、engine 既定の `NO ACTION`
            // に委ねる（現行挙動と完全互換）。
            if let Some(action) = on_delete {
                tokens.push(Token::Ident("ON".to_string()));
                tokens.push(Token::Ident("DELETE".to_string()));
                tokens.extend(referential_action_tokens(action)?);
            }
            if let Some(action) = on_update {
                tokens.push(Token::Ident("ON".to_string()));
                tokens.push(Token::Ident("UPDATE".to_string()));
                tokens.extend(referential_action_tokens(action)?);
            }
            Ok(tokens)
        }
        "check" => build_check_tokens(&v, map),
        _ => Err(DdlError::InvalidRequest),
    }
}

/// `create_table` op を実行する。`validated` は
/// [`super::schema::CREATE_TABLE_SCHEMA::validate`] を通過済みの JSON。
pub fn execute_create_table(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<(), DdlError> {
    let table = validated.required_str("table").map_err(DdlError::from)?;
    let table_token = ident_token(table)?;
    let columns = validated
        .required_array("columns")
        .map_err(DdlError::from)?;
    let constraints = validated
        .optional_array("constraints")
        .map_err(DdlError::from)?
        .unwrap_or(&[]);

    let mut tokens = vec![
        Token::Ident("CREATE".to_string()),
        Token::Ident("TABLE".to_string()),
        table_token,
        Token::Punct('('),
    ];
    for (i, column) in columns.iter().enumerate() {
        if i > 0 {
            tokens.push(Token::Punct(','));
        }
        tokens.extend(build_column_tokens(column)?);
    }
    for constraint in constraints {
        tokens.push(Token::Punct(','));
        tokens.extend(build_constraint_tokens(constraint)?);
    }
    tokens.push(Token::Punct(')'));

    let stmt = validate_create_table_tokens(&tokens)?;
    run_ddl(core, principal, ParsedSql::CreateTable(stmt))
}

/// `alter_table.add_column` 1 件をトークン列（`<type-name>` 部分のみ）へ
/// 写像する（SQL 表層 `ALTER TABLE ADD COLUMN` と同じ受理集合。モジュール
/// doc 参照）。
fn build_add_column_type_tokens(v: &Validated<'_>) -> Result<Vec<Token>, DdlError> {
    let ty = v
        .required_str("type")
        .map_err(DdlError::from)?
        .to_ascii_lowercase();
    let dim = v.optional_u32("dim").map_err(DdlError::from)?;
    let precision = v.optional_u32("precision").map_err(DdlError::from)?;
    let scale = v.optional_u32("scale").map_err(DdlError::from)?;
    let enum_type = v.optional_str("enum_type").map_err(DdlError::from)?;

    // ほとんどの型は `dim`／`precision`／`scale`／`enum_type` のいずれも
    // 取らない（`vector`／`numeric`／`enum` のみの専用パラメータ）。
    let no_extra = |ok: bool| -> Result<(), DdlError> {
        if dim.is_some() || precision.is_some() || scale.is_some() || enum_type.is_some() {
            return Err(DdlError::InvalidRequest);
        }
        if !ok {
            return Err(DdlError::InvalidRequest);
        }
        Ok(())
    };

    match ty.as_str() {
        "text" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("TEXT".to_string())])
        }
        "integer" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("INTEGER".to_string())])
        }
        "bigint" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("BIGINT".to_string())])
        }
        "real" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("REAL".to_string())])
        }
        "double" => {
            no_extra(true)?;
            Ok(vec![
                Token::Ident("DOUBLE".to_string()),
                Token::Ident("PRECISION".to_string()),
            ])
        }
        "boolean" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("BOOLEAN".to_string())])
        }
        "date" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("DATE".to_string())])
        }
        "timestamp" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("TIMESTAMP".to_string())])
        }
        "bytea" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("BYTEA".to_string())])
        }
        "json" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("JSON".to_string())])
        }
        "jsonb" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("JSONB".to_string())])
        }
        "uuid" => {
            no_extra(true)?;
            Ok(vec![Token::Ident("UUID".to_string())])
        }
        "vector" => {
            if precision.is_some() || scale.is_some() || enum_type.is_some() {
                return Err(DdlError::InvalidRequest);
            }
            let dim = dim.ok_or(DdlError::InvalidRequest)?;
            Ok(vec![
                Token::Ident("VECTOR".to_string()),
                Token::Punct('('),
                number_token(dim),
                Token::Punct(')'),
            ])
        }
        "numeric" => {
            if dim.is_some() || enum_type.is_some() {
                return Err(DdlError::InvalidRequest);
            }
            let precision = precision.ok_or(DdlError::InvalidRequest)?;
            let scale = scale.ok_or(DdlError::InvalidRequest)?;
            if precision > u32::from(u8::MAX) || scale > u32::from(u8::MAX) {
                return Err(DdlError::InvalidRequest);
            }
            Ok(vec![
                Token::Ident("NUMERIC".to_string()),
                Token::Punct('('),
                number_token(precision),
                Token::Punct(','),
                number_token(scale),
                Token::Punct(')'),
            ])
        }
        "enum" => {
            if dim.is_some() || precision.is_some() || scale.is_some() {
                return Err(DdlError::InvalidRequest);
            }
            let enum_type = enum_type.ok_or(DdlError::InvalidRequest)?;
            if is_reserved_type_keyword(enum_type) {
                return Err(DdlError::InvalidRequest);
            }
            Ok(vec![ident_token(enum_type)?])
        }
        _ => Err(DdlError::InvalidRequest),
    }
}

/// `enum_type` が予約型キーワード（大小無視）と一致するかを判定する
/// （`sql::ddl_column_type::parse_column_type_name` が同じ語をすべて予約型
/// として読むため、ENUM 型名候補として別型に解釈されるのを防ぐ）。
fn is_reserved_type_keyword(raw: &str) -> bool {
    const RESERVED: [&str; 15] = [
        "TEXT",
        "INTEGER",
        "BIGINT",
        "REAL",
        "DOUBLE",
        "BOOLEAN",
        "DATE",
        "TIMESTAMP",
        "BYTEA",
        "JSON",
        "JSONB",
        "UUID",
        "VECTOR",
        "NUMERIC",
        "DECIMAL",
    ];
    RESERVED.iter().any(|kw| raw.eq_ignore_ascii_case(kw))
}

/// `alter_table` op を実行する。`add_column`／`drop_column` はどちらか
/// 一方のみ必須（両方指定・両方欠落は [`DdlError::InvalidRequest`]）。
pub fn execute_alter_table(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<(), DdlError> {
    let table = validated.required_str("table").map_err(DdlError::from)?;
    let table_token = ident_token(table)?;
    let add_column = validated
        .optional_object("add_column")
        .map_err(DdlError::from)?;
    let drop_column = validated
        .optional_object("drop_column")
        .map_err(DdlError::from)?;

    match (add_column, drop_column) {
        (Some(_), Some(_)) | (None, None) => Err(DdlError::InvalidRequest),
        (None, Some(drop_column)) => {
            // `add_column` と同じく `DDL_DROP_COLUMN_SCHEMA` で再度包んで型付き
            // アクセサを使う（多層防御。トップレベル検証で既に再帰検証済み）。
            let wrapped = JsonValue::Object(drop_column.clone());
            let drop_v = DDL_DROP_COLUMN_SCHEMA
                .validate(&wrapped)
                .map_err(DdlError::from)?;
            let column_name = drop_v.required_str("name").map_err(DdlError::from)?;
            let column_name_token = ident_token(column_name)?;
            let tokens = vec![
                Token::Ident("ALTER".to_string()),
                Token::Ident("TABLE".to_string()),
                table_token,
                Token::Ident("DROP".to_string()),
                Token::Ident("COLUMN".to_string()),
                column_name_token,
            ];
            let stmt = validate_alter_table_tokens(&tokens)?;
            run_ddl(core, principal, ParsedSql::AlterTable(stmt))
        }
        (Some(add_column), None) => {
            // ネストしたオブジェクトへ型付きアクセサ（`required_str`／
            // `optional_u32` 等）を使うため、`DDL_ADD_COLUMN_SCHEMA` で
            // 再度包む（トップレベルの `ALTER_TABLE_SCHEMA::validate` が
            // 既に同じスキーマで再帰検証済みのため、この呼び出し自体は
            // 失敗しない。多層防御として `?` は残す）。
            let wrapped = JsonValue::Object(add_column.clone());
            let add_v = DDL_ADD_COLUMN_SCHEMA
                .validate(&wrapped)
                .map_err(DdlError::from)?;
            let column_name = add_v.required_str("name").map_err(DdlError::from)?;
            let column_name_token = ident_token(column_name)?;
            let type_tokens = build_add_column_type_tokens(&add_v)?;

            let mut tokens = vec![
                Token::Ident("ALTER".to_string()),
                Token::Ident("TABLE".to_string()),
                table_token,
                Token::Ident("ADD".to_string()),
                Token::Ident("COLUMN".to_string()),
                column_name_token,
            ];
            tokens.extend(type_tokens);

            let stmt = validate_alter_table_tokens(&tokens)?;
            run_ddl(core, principal, ParsedSql::AlterTable(stmt))
        }
    }
}

/// `drop_table` op を実行する。
pub fn execute_drop_table(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<(), DdlError> {
    let table = validated.required_str("table").map_err(DdlError::from)?;
    let table_token = ident_token(table)?;
    let tokens = vec![
        Token::Ident("DROP".to_string()),
        Token::Ident("TABLE".to_string()),
        table_token,
    ];
    let stmt = validate_drop_table_tokens(&tokens)?;
    run_ddl(core, principal, ParsedSql::DropTable(stmt))
}

/// [`execute_create_table`]・[`execute_alter_table`]・[`execute_drop_table`]
/// が共有する実行本体。`principal.ddl_allowed()` に応じて
/// `SessionState::allow_ddl()` を呼んだうえで、SQL 表層の DDL と単一の
/// 実行入口である `EngineCore::execute_parsed_in_session` へ委譲する
/// （モジュール doc 参照。第 2 の権限判定を作らない）。
fn run_ddl(
    core: &EngineCore,
    principal: &SessionPrincipal,
    parsed: ParsedSql,
) -> Result<(), DdlError> {
    let mut session = SessionState::default();
    if principal.ddl_allowed() {
        session.allow_ddl();
    }
    core.execute_parsed_in_session(principal.policy_context(), &mut session, &parsed)?;
    Ok(())
}

/// `POST /v1/query` の `create_table` op を処理し応答バイト列を返す。
pub fn handle_create_table(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute_create_table(core, principal, validated) {
        Ok(()) => http_response::encode_ok(OK_BODY, now_wall),
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

/// `POST /v1/query` の `alter_table` op を処理し応答バイト列を返す。
pub fn handle_alter_table(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute_alter_table(core, principal, validated) {
        Ok(()) => http_response::encode_ok(OK_BODY, now_wall),
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

/// `POST /v1/query` の `drop_table` op を処理し応答バイト列を返す。
pub fn handle_drop_table(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute_drop_table(core, principal, validated) {
        Ok(()) => http_response::encode_ok(OK_BODY, now_wall),
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

#[cfg(test)]
mod tests {
    //! トークン列写像の境界値を固定する unit tests（モジュール doc
    //! 「untrusted 入力の取り扱い」参照）。HTTP フレーミング越しの結合検証は
    //! `crates/wire-server/tests/nosql13_ddl.rs` が担う（同ファイル冒頭の
    //! コメント参照）。

    use super::*;
    use engine::json::parse_json;

    fn obj(json: &str) -> JsonValue {
        parse_json(json).expect("test fixture must be valid JSON")
    }

    // --- default_literal_tokens ------------------------------------------

    #[test]
    fn default_literal_tokens_accepts_plain_string() {
        let value = obj("\"hello\"");
        let tokens = default_literal_tokens(&value).expect("string DEFAULT must succeed");
        assert_eq!(tokens, vec![Token::StringLiteral("hello".to_string())]);
    }

    #[test]
    fn default_literal_tokens_rejects_control_characters() {
        let value = JsonValue::String("a\u{0000}b".to_string());
        let err = default_literal_tokens(&value).expect_err("control char must be rejected");
        assert!(matches!(err, DdlError::InvalidRequest));
    }

    #[test]
    fn default_literal_tokens_splits_negative_number_into_punct_and_number() {
        let value = obj("-3");
        let tokens = default_literal_tokens(&value).expect("negative number must succeed");
        assert_eq!(
            tokens,
            vec![Token::Punct('-'), Token::Number("3".to_string())]
        );
    }

    #[test]
    fn default_literal_tokens_accepts_positive_number_without_punct() {
        let value = obj("3");
        let tokens = default_literal_tokens(&value).expect("positive number must succeed");
        assert_eq!(tokens, vec![Token::Number("3".to_string())]);
    }

    #[test]
    fn default_literal_tokens_rejects_exponent_notation() {
        let value = obj("1e10");
        let err = default_literal_tokens(&value).expect_err("exponent form must be rejected");
        assert!(matches!(err, DdlError::InvalidRequest));
    }

    #[test]
    fn default_literal_tokens_rejects_non_scalar_json_values() {
        for json in ["true", "null", "[1]", "{}"] {
            let value = obj(json);
            let err = default_literal_tokens(&value)
                .expect_err("bool/null/array/object DEFAULT must be rejected");
            assert!(matches!(err, DdlError::InvalidRequest), "input: {json}");
        }
    }

    // --- build_add_column_type_tokens（numeric precision/scale 上限） ------

    fn add_column_validated(json: &str) -> JsonValue {
        obj(json)
    }

    #[test]
    fn add_column_numeric_accepts_boundary_precision_and_scale() {
        let value =
            add_column_validated(r#"{"name":"n","type":"numeric","precision":255,"scale":255}"#);
        let v = DDL_ADD_COLUMN_SCHEMA.validate(&value).expect("valid shape");
        let tokens = build_add_column_type_tokens(&v).expect("precision/scale 255 must succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("NUMERIC".to_string()),
                Token::Punct('('),
                Token::Number("255".to_string()),
                Token::Punct(','),
                Token::Number("255".to_string()),
                Token::Punct(')'),
            ]
        );
    }

    #[test]
    fn add_column_numeric_rejects_precision_above_upper_bound() {
        let value =
            add_column_validated(r#"{"name":"n","type":"numeric","precision":256,"scale":0}"#);
        let v = DDL_ADD_COLUMN_SCHEMA.validate(&value).expect("valid shape");
        let err = build_add_column_type_tokens(&v).expect_err("precision 256 must be rejected");
        assert!(matches!(err, DdlError::InvalidRequest));
    }

    #[test]
    fn add_column_numeric_rejects_scale_above_upper_bound() {
        let value =
            add_column_validated(r#"{"name":"n","type":"numeric","precision":0,"scale":256}"#);
        let v = DDL_ADD_COLUMN_SCHEMA.validate(&value).expect("valid shape");
        let err = build_add_column_type_tokens(&v).expect_err("scale 256 must be rejected");
        assert!(matches!(err, DdlError::InvalidRequest));
    }

    // --- create_table_type_tokens（alter_table とのケース正規化パリティ） --

    #[test]
    fn create_table_type_tokens_accepts_uppercase_and_mixed_case_type_names() {
        // `build_add_column_type_tokens`（alter_table.add_column）が ASCII
        // 小文字化してから型名を判定するのに対し、本関数がケース区別のまま
        // 完全一致していると `INTEGER`／`Text` が create_table では拒否・
        // alter_table では受理される非対称が生じる（レビュー指摘の回帰）。
        let (tokens, is_vector) =
            create_table_type_tokens("INTEGER", None).expect("uppercase type name must succeed");
        assert_eq!(tokens, vec![Token::Ident("INTEGER".to_string())]);
        assert!(!is_vector);

        let (tokens, is_vector) =
            create_table_type_tokens("Text", None).expect("mixed-case type name must succeed");
        assert_eq!(tokens, vec![Token::Ident("TEXT".to_string())]);
        assert!(!is_vector);

        let (tokens, is_vector) =
            create_table_type_tokens("Vector", Some(3)).expect("mixed-case VECTOR must succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("VECTOR".to_string()),
                Token::Punct('('),
                Token::Number("3".to_string()),
                Token::Punct(')'),
            ]
        );
        assert!(is_vector);
    }

    #[test]
    fn create_table_type_tokens_still_rejects_unknown_type_name() {
        let err = create_table_type_tokens("bogus", None)
            .expect_err("unknown type name must be rejected regardless of case");
        assert!(matches!(err, DdlError::InvalidRequest));
    }

    // --- build_add_column_type_tokens（enum の予約キーワード判定） ---------

    #[test]
    fn is_reserved_type_keyword_matches_case_insensitively() {
        assert!(is_reserved_type_keyword("vector"));
        assert!(is_reserved_type_keyword("VECTOR"));
        assert!(is_reserved_type_keyword("Numeric"));
        assert!(!is_reserved_type_keyword("mood"));
    }

    #[test]
    fn add_column_enum_rejects_reserved_type_keyword() {
        let value = add_column_validated(r#"{"name":"n","type":"enum","enum_type":"TEXT"}"#);
        let v = DDL_ADD_COLUMN_SCHEMA.validate(&value).expect("valid shape");
        let err =
            build_add_column_type_tokens(&v).expect_err("reserved keyword enum_type must fail");
        assert!(matches!(err, DdlError::InvalidRequest));
    }

    #[test]
    fn add_column_enum_accepts_non_reserved_type_name() {
        let value = add_column_validated(r#"{"name":"n","type":"enum","enum_type":"mood"}"#);
        let v = DDL_ADD_COLUMN_SCHEMA.validate(&value).expect("valid shape");
        let tokens = build_add_column_type_tokens(&v).expect("non-reserved enum_type must succeed");
        assert_eq!(tokens, vec![Token::Ident("mood".to_string())]);
    }

    // --- build_constraint_tokens（foreign_key 以外への references 混入） ---

    #[test]
    fn build_constraint_tokens_foreign_key_maps_references() {
        let value = obj(r#"{"kind":"foreign_key","columns":["parent_id"],
                "references":{"table":"parents","columns":["id"]}}"#);
        let tokens = build_constraint_tokens(&value).expect("foreign_key must succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("FOREIGN".to_string()),
                Token::Ident("KEY".to_string()),
                Token::Punct('('),
                Token::Ident("parent_id".to_string()),
                Token::Punct(')'),
                Token::Ident("REFERENCES".to_string()),
                Token::Ident("parents".to_string()),
                Token::Punct('('),
                Token::Ident("id".to_string()),
                Token::Punct(')'),
            ]
        );
    }

    #[test]
    fn build_constraint_tokens_primary_key_rejects_stray_references() {
        let value = obj(r#"{"kind":"primary_key","columns":["id"],
                "references":{"table":"parents"}}"#);
        let err = build_constraint_tokens(&value)
            .expect_err("primary_key with references must be rejected (fail-closed)");
        assert!(matches!(err, DdlError::InvalidRequest));
    }

    #[test]
    fn build_constraint_tokens_unique_rejects_stray_references() {
        let value = obj(r#"{"kind":"unique","columns":["email"],
                "references":{"table":"parents"}}"#);
        let err = build_constraint_tokens(&value)
            .expect_err("unique with references must be rejected (fail-closed)");
        assert!(matches!(err, DdlError::InvalidRequest));
    }

    #[test]
    fn build_constraint_tokens_primary_key_without_references_still_succeeds() {
        let value = obj(r#"{"kind":"primary_key","columns":["id"]}"#);
        let tokens = build_constraint_tokens(&value).expect("plain primary_key must succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("PRIMARY".to_string()),
                Token::Ident("KEY".to_string()),
                Token::Punct('('),
                Token::Ident("id".to_string()),
                Token::Punct(')'),
            ]
        );
    }

    // --- referential_action_tokens（Issue #1148・NOSQL-13） ----------------

    #[test]
    fn referential_action_tokens_maps_fixed_vocabulary() {
        assert_eq!(
            referential_action_tokens("no_action").expect("no_action must succeed"),
            vec![
                Token::Ident("NO".to_string()),
                Token::Ident("ACTION".to_string())
            ]
        );
        assert_eq!(
            referential_action_tokens("restrict").expect("restrict must succeed"),
            vec![Token::Ident("RESTRICT".to_string())]
        );
        assert_eq!(
            referential_action_tokens("cascade").expect("cascade must succeed"),
            vec![Token::Ident("CASCADE".to_string())]
        );
        assert_eq!(
            referential_action_tokens("set_null").expect("set_null must succeed"),
            vec![
                Token::Ident("SET".to_string()),
                Token::Ident("NULL".to_string())
            ]
        );
        assert_eq!(
            referential_action_tokens("set_default").expect("set_default must succeed"),
            vec![
                Token::Ident("SET".to_string()),
                Token::Ident("DEFAULT".to_string())
            ]
        );
    }

    #[test]
    fn referential_action_tokens_rejects_out_of_vocabulary() {
        for raw in ["bogus", "CASCADE", "set null", ""] {
            let err = referential_action_tokens(raw)
                .expect_err("out-of-vocabulary or non-exact-case value must be rejected");
            assert!(matches!(err, DdlError::InvalidRequest));
        }
    }

    #[test]
    fn build_constraint_tokens_foreign_key_maps_referential_actions() {
        let value = obj(r#"{"kind":"foreign_key","columns":["parent_id"],
                "references":{"table":"parents","columns":["id"],
                "on_delete":"cascade","on_update":"set_null"}}"#);
        let tokens =
            build_constraint_tokens(&value).expect("foreign_key with actions must succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("FOREIGN".to_string()),
                Token::Ident("KEY".to_string()),
                Token::Punct('('),
                Token::Ident("parent_id".to_string()),
                Token::Punct(')'),
                Token::Ident("REFERENCES".to_string()),
                Token::Ident("parents".to_string()),
                Token::Punct('('),
                Token::Ident("id".to_string()),
                Token::Punct(')'),
                Token::Ident("ON".to_string()),
                Token::Ident("DELETE".to_string()),
                Token::Ident("CASCADE".to_string()),
                Token::Ident("ON".to_string()),
                Token::Ident("UPDATE".to_string()),
                Token::Ident("SET".to_string()),
                Token::Ident("NULL".to_string()),
            ]
        );
    }

    #[test]
    fn build_constraint_tokens_foreign_key_rejects_out_of_vocabulary_action() {
        for bad in [
            r#"{"kind":"foreign_key","columns":["p"],
                "references":{"table":"t","on_delete":"bogus"}}"#,
            r#"{"kind":"foreign_key","columns":["p"],
                "references":{"table":"t","on_delete":"CASCADE"}}"#,
            r#"{"kind":"foreign_key","columns":["p"],
                "references":{"table":"t","on_update":"set null"}}"#,
        ] {
            let value = obj(bad);
            let err = build_constraint_tokens(&value)
                .expect_err("out-of-vocabulary referential action must be rejected");
            assert!(matches!(err, DdlError::InvalidRequest));
        }
    }

    #[test]
    fn build_constraint_tokens_foreign_key_rejects_non_string_action() {
        for bad in [
            r#"{"kind":"foreign_key","columns":["p"],
                "references":{"table":"t","on_delete":1}}"#,
            r#"{"kind":"foreign_key","columns":["p"],
                "references":{"table":"t","on_delete":null}}"#,
        ] {
            let value = obj(bad);
            let err = build_constraint_tokens(&value)
                .expect_err("non-string referential action must be rejected");
            assert!(matches!(err, DdlError::Shape(_)));
        }
    }

    #[test]
    fn build_constraint_tokens_foreign_key_referential_actions_match_sql_surface_parity() {
        // NoSQL の `references.on_delete`／`on_update` から生成したトークン列を
        // `validate_create_table_tokens` に通した結果が、同じ宣言を表す SQL
        // テキストの結果と構造的に一致することを固定する（Issue #1148 の
        // パリティ要件）。`restrict` は SQL 表層と同じく `no_action` と同一の
        // 正規化結果になることも併せて確認する。
        let vocab: &[(&str, &str)] = &[
            ("no_action", "NO ACTION"),
            ("restrict", "RESTRICT"),
            ("cascade", "CASCADE"),
            ("set_null", "SET NULL"),
            ("set_default", "SET DEFAULT"),
        ];
        for (json_value, sql_action) in vocab {
            let nosql_value = obj(&format!(
                r#"{{"kind":"foreign_key","columns":["parent_id"],
                    "references":{{"table":"parents","columns":["id"],
                    "on_delete":"{json_value}","on_update":"{json_value}"}}}}"#,
            ));
            let nosql_tokens =
                build_constraint_tokens(&nosql_value).expect("nosql mapping must succeed");
            let mut full_tokens = vec![
                Token::Ident("CREATE".to_string()),
                Token::Ident("TABLE".to_string()),
                Token::Ident("child".to_string()),
                Token::Punct('('),
                Token::Ident("parent_id".to_string()),
                Token::Ident("BIGINT".to_string()),
                Token::Punct(','),
            ];
            full_tokens.extend(nosql_tokens);
            full_tokens.push(Token::Punct(')'));
            let nosql_validated = validate_create_table_tokens(&full_tokens)
                .expect("nosql-derived tokens must validate");

            let sql_text = format!(
                "CREATE TABLE child (parent_id BIGINT, FOREIGN KEY (parent_id) \
                 REFERENCES parents (id) ON DELETE {sql_action} ON UPDATE {sql_action})"
            );
            let sql_tokens = tokenize(&sql_text).expect("sql fixture must tokenize");
            let sql_validated = validate_create_table_tokens(&sql_tokens)
                .expect("sql-derived tokens must validate");

            assert_eq!(
                nosql_validated, sql_validated,
                "NoSQL and SQL surfaces must produce the same catalog representation for {json_value}"
            );
        }

        // 省略時（`on_delete`／`on_update` を付けない）も、明示的な `no_action`
        // と同じ結果になることを確認する（現行挙動との完全互換）。
        let omitted = obj(r#"{"kind":"foreign_key","columns":["parent_id"],
                "references":{"table":"parents","columns":["id"]}}"#);
        let omitted_tokens = build_constraint_tokens(&omitted).expect("omitted case must succeed");
        let mut full_tokens = vec![
            Token::Ident("CREATE".to_string()),
            Token::Ident("TABLE".to_string()),
            Token::Ident("child".to_string()),
            Token::Punct('('),
            Token::Ident("parent_id".to_string()),
            Token::Ident("BIGINT".to_string()),
            Token::Punct(','),
        ];
        full_tokens.extend(omitted_tokens);
        full_tokens.push(Token::Punct(')'));
        let omitted_validated =
            validate_create_table_tokens(&full_tokens).expect("omitted tokens must validate");

        let explicit_no_action = obj(r#"{"kind":"foreign_key","columns":["parent_id"],
                "references":{"table":"parents","columns":["id"],
                "on_delete":"no_action","on_update":"no_action"}}"#);
        let explicit_tokens =
            build_constraint_tokens(&explicit_no_action).expect("explicit no_action must succeed");
        let mut full_tokens = vec![
            Token::Ident("CREATE".to_string()),
            Token::Ident("TABLE".to_string()),
            Token::Ident("child".to_string()),
            Token::Punct('('),
            Token::Ident("parent_id".to_string()),
            Token::Ident("BIGINT".to_string()),
            Token::Punct(','),
        ];
        full_tokens.extend(explicit_tokens);
        full_tokens.push(Token::Punct(')'));
        let explicit_validated =
            validate_create_table_tokens(&full_tokens).expect("explicit tokens must validate");

        assert_eq!(omitted_validated, explicit_validated);
    }

    fn check_table_tokens(constraint_tokens: Vec<Token>) -> Vec<Token> {
        let mut t = tokenize("CREATE TABLE t (qty INTEGER, kind TEXT,").expect("lex");
        t.extend(constraint_tokens);
        t.push(Token::Punct(')'));
        t
    }

    #[test]
    fn build_constraint_tokens_check_matches_sql_surface_parity() {
        let value = obj(r#"{"kind":"check","name":"c1","predicate":[
                {"column":"qty","op":"gt","value":0},
                {"column":"kind","op":"prefix","value":"a%_\\"}]}"#);
        let tokens = build_check_case(&value);
        let nosql = validate_create_table_tokens(&check_table_tokens(tokens)).expect("nosql");
        let sql_tokens = tokenize(
            "CREATE TABLE t (qty INTEGER, kind TEXT, CONSTRAINT c1 CHECK (qty > 0 AND kind LIKE 'a\\%\\_\\\\%'))",
        )
        .expect("lex");
        let sql = validate_create_table_tokens(&sql_tokens).expect("sql");
        assert_eq!(nosql, sql);
    }

    fn build_check_case(value: &JsonValue) -> Vec<Token> {
        build_constraint_tokens(value).expect("check must map")
    }

    #[test]
    fn build_constraint_tokens_check_unnamed_and_range_synonyms() {
        let value = obj(r#"{"kind":"check","predicate":[
                {"column":"qty","op":"lte","value":9},
                {"column":"qty","op":"ge","value":1},
                {"column":"kind","op":"eq","value":"x"}]}"#);
        let nosql = validate_create_table_tokens(&check_table_tokens(build_check_case(&value)))
            .expect("nosql");
        let sql_tokens = tokenize(
            "CREATE TABLE t (qty INTEGER, kind TEXT, CHECK (qty <= 9 AND qty >= 1 AND kind = 'x'))",
        )
        .expect("lex");
        let sql = validate_create_table_tokens(&sql_tokens).expect("sql");
        assert_eq!(nosql, sql);
    }

    #[test]
    fn build_constraint_tokens_check_bool_equality_maps_to_ident() {
        let value =
            obj(r#"{"kind":"check","predicate":[{"column":"flag","op":"eq","value":true}]}"#);
        let tokens = build_check_case(&value);
        assert!(tokens.contains(&Token::Ident("true".to_string())));
    }

    #[test]
    fn build_constraint_tokens_check_negative_number_splits_sign() {
        let value = obj(r#"{"kind":"check","predicate":[{"column":"qty","op":"gt","value":-1}]}"#);
        let tokens = build_check_case(&value);
        assert!(tokens.contains(&Token::Punct('-')));
        assert!(tokens.contains(&Token::Number("1".to_string())));
    }

    #[test]
    fn build_constraint_tokens_check_rejects_malformed_forms() {
        let cases = [
            r#"{"kind":"check"}"#,
            r#"{"kind":"check","predicate":[]}"#,
            r#"{"kind":"check","columns":["a"],"predicate":[{"column":"a","op":"gt","value":0}]}"#,
            r#"{"kind":"check","predicate":[{"column":"a","op":"in","value":[1]}]}"#,
            r#"{"kind":"check","predicate":[{"column":"a","op":"neq","value":1}]}"#,
            r#"{"kind":"check","predicate":[{"column":"visible","op":"eq","value":true}]}"#,
            r#"{"kind":"check","predicate":[{"column":"a","op":"prefix","value":"\u0001"}]}"#,
            r#"{"kind":"check","predicate":[{"column":"a","op":"lt","value":false}]}"#,
            r#"{"kind":"primary_key","columns":["a"],"name":"n"}"#,
        ];
        for c in cases {
            let err = build_constraint_tokens(&obj(c)).expect_err(c);
            assert!(
                matches!(
                    err,
                    DdlError::InvalidRequest | DdlError::InvalidIdentifier | DdlError::Shape(_)
                ),
                "case {c}: {err:?}"
            );
        }
    }

    #[test]
    fn build_constraint_tokens_check_leaf_count_over_limit_is_engine_error() {
        let leaf = r#"{"column":"a","op":"gt","value":0}"#;
        let leaves = vec![leaf; 257].join(",");
        let value = obj(&format!(r#"{{"kind":"check","predicate":[{leaves}]}}"#));
        let err = build_constraint_tokens(&value).expect_err("over limit");
        assert!(matches!(err, DdlError::Engine(_)), "{err:?}");
    }
}
