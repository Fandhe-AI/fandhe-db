//! `POST /v1/query` の `insert` op を SQL 表層の `INSERT ... USING OPERATION_ID`
//! （SQL-10）と同じ engine 書き込み契約へ写像するモジュール（Issue #771・
//! TASK-178・対象ビヘイビア NOSQL-6。ポインタ: `docs/spec/05-tasks.md` TASK-178・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-6）。
//!
//! 責務境界: [`super::schema::INSERT_SCHEMA`] が形（必須キー・型）を検証済みの
//! JSON オブジェクトから `table`／`rows`／`operation_id` を取り出し、
//! [`bind_rows`] が行ごとに [`super::typed_json::map_json_to_literal`]
//! （NOSQL-17。Issue #896。JSON 値 → `InsertLiteral` の型別写像）で列値を
//! `engine::sql::allowlist::ValidatedInsert`（1 行分）へ組み立て、
//! `engine::sql::parser::bind_insert`（SQL 表層 `INSERT` と共有する公開束縛
//! 関数。整数のオーバーフロー・`NUMERIC` の桁あふれ・`DATE`／`TIMESTAMP` の
//! 暦上妥当性・`UUID` の文法等はすべてここへ委譲する）で `BoundInsert` へ
//! 束縛する。[`execute`] がその結果を
//! `engine::core::EngineCore::execute_bound_insert_in_session`（Issue #728 と同型の
//! セッション対応エントリ）へ委譲する。SQL テキストを一切生成せず、第 2 の実行器も
//! 作らない（`filter.rs`・`docs/design/scalar-index-mask-search.md` と同じ設計方針）。
//!
//! テナントは `principal`（[`crate::http::session::middleware::SessionPrincipal`]。
//! 唯一の入口）からのみ導出し、JSON からテナント相当の値を読む経路をシグネチャ上
//! 持たない（`security.md` P0）。可視性は engine 側 [`engine::sql::exec::execute_insert_batch`]
//! が常に `Private` 固定で書き込む（SQL-10 `execute_insert` と同じ fail-closed 判断）。
//!
//! `operation_id` の欠落・`null`・空文字はいずれも `23502`
//! （[`engine::sql::using_operation_id::OperationId::parse`] が空文字を句の省略と
//! 同等に扱う契約をそのまま透過する）。同一 `operation_id` への再送は台帳照合
//! （TASK-101・RECOVER-10）により内容一致なら `23505`・不一致なら `22023` へ収束する
//! （`execute_insert_batch`・`insert_typed_rows_unchecked` のドキュメント参照）。
//! INDEX-4 の 4 上限（バッチ相当の判定を「1 要求の `rows` 行数」に読み替えたもの）は
//! `EngineCore::execute_bound_insert_in_session` が判定する。
//!
//! `table` は兄弟 op（`search`／`scan`／`aggregate`）と同じ識別子形状検査
//! （[`super::ident::check_identifier`]）を engine へ渡す前に適用する（`42601`。
//! Cursor Bugbot 指摘・PR #823）。
//!
//! [`execute`] は成功時 [`InsertSuccess`] を返し、[`handle`] が
//! [`encode_success_body`]（`{"inserted":<n>,"operation_id":"<escaped>"}`。
//! キー順固定・空白なしのコンパクト形）を経由して `gate.rs` の `Op::Insert`
//! アームへディスパッチ可能な応答バイト列へ写像する（Issue #772・TASK-178。
//! `scan::handle`／`aggregate::handle` と同一シグネチャ形）。`operation_id` は
//! クライアント要求から検証済みの値をそのまま echo するため
//! [`crate::http::error_body::escape_json_string_into`] を通し（`"`／`\` の
//! 混入がありうる。制御文字は `OperationId::parse` が既に拒否済みだが
//! 多層防御として統一する）、`InsertOutcome::incremental` は行形では常に
//! `None`（ファイル形専用フィールド）のため成功本文へ含めない。
//!
//! 対象外: `EXPLAIN` フィールド（NOSQL-10・#765）・全契約の層 A テスト群
//! （SQL 経由との seed 一致を含む・#773）。

use std::fmt::Write as _;

use engine::catalog::{ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::{SqlSurfaceError, ValidatedInsert};
use engine::sql::exec::InsertOutcome;
use engine::sql::parser::{bind_insert, BoundInsert};

use crate::http::error_body::escape_json_string_into;
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated};
use super::typed_json::{self, TypedJsonError};

/// [`bind_row`] が返す `InsertError::Bind` 用の `SqlSurfaceError::InvalidInput`
/// 構築ヘルパー。`SqlSurfaceError::invalid_input`（切り詰め処理を持つコンストラクタ）
/// は `pub(crate)`（engine クレート内限定）のため、wire-server からは列挙子の
/// フィールドを直接構築する（`InvalidInput` variant は enum の可視性に従い pub）。
/// 固定文言（`&'static str` リテラル）のみを渡す前提のため切り詰めは行わない。
fn invalid_input_error(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::InvalidInput {
        detail: detail.to_string(),
    }
}

/// [`bind_rows`]・[`execute`] の失敗を表す。いずれも [`ClassifiedError`] を実装し
/// `wire_code`／`client_message` を経由して HTTP エラー応答へ射影される
/// （`filter.rs::FilterError` と同じ設計）。
#[derive(Debug, Clone)]
pub enum InsertError {
    /// `rows` の形（`schema.rs` の意味的検証を通過した後の、行要素単位の検証）が
    /// 不正。`SchemaError` は untrusted な値・キー名を保持しない固定文言。
    Shape(SchemaError),
    /// `table` が識別子として意味を持ちうる形状（[`super::ident::check_identifier`]）
    /// を満たさない。兄弟 op（`search`／`scan`／`aggregate`）と同じ検査を `table` を
    /// `EngineCore::execute_bound_insert_in_session` へ渡す前に適用する（`42601`）。
    /// この検査がないと、NUL 等の制御文字を含む `table` 文字列が engine 側の
    /// `validate_identifier`（`22000`／`42P01`／`XX000` のいずれかに丸められる）まで
    /// 素通りし、兄弟 op と異なる `wire_code` として露出しうる（Cursor Bugbot 指摘・
    /// PR #823）。
    InvalidIdentifier,
    /// `engine::sql::parser::bind_insert` への束縛時のエラー（未知列・型不一致・
    /// 次元不一致・非 nullable 列の欠落・整数のオーバーフロー・`NUMERIC` の
    /// 桁あふれ・`DATE`／`TIMESTAMP` の暦上妥当性・`UUID` の文法等。SQL 表層
    /// `INSERT` と同一の分類）。
    Bind(SqlSurfaceError),
    /// `EngineCore::execute_bound_insert_in_session` 側のエラー（`operation_id`
    /// 必須化・台帳照合・INDEX-4 上限・テーブル不存在等）をそのまま透過する。
    Exec(SqlSurfaceError),
    /// JSON 値 → `InsertLiteral` 写像時の型不一致・wire 固有の符号化エラー
    /// （NOSQL-17。Issue #896。[`super::typed_json::TypedJsonError`] を
    /// `update.rs::UpdateError::Set` と共有する単一情報源とする）。
    Set(TypedJsonError),
}

impl From<SchemaError> for InsertError {
    fn from(err: SchemaError) -> Self {
        InsertError::Shape(err)
    }
}

impl From<InvalidIdentifier> for InsertError {
    fn from(_err: InvalidIdentifier) -> Self {
        InsertError::InvalidIdentifier
    }
}

impl ClassifiedError for InsertError {
    fn error_class(&self) -> ErrorClass {
        match self {
            InsertError::Shape(err) => err.error_class(),
            InsertError::InvalidIdentifier => ErrorClass::UnsupportedSqlSyntax,
            InsertError::Bind(err) | InsertError::Exec(err) => err.error_class(),
            InsertError::Set(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            InsertError::Shape(err) => err.client_message(),
            InsertError::InvalidIdentifier => "invalid identifier".to_string(),
            InsertError::Bind(err) | InsertError::Exec(err) => err.client_message(),
            InsertError::Set(err) => err.client_message(),
        }
    }
}

/// `rows` 配列の要素 1 件（`JsonValue::Object` 前提。非オブジェクトは
/// `InsertError::Shape`）から `schema` の列順に対応する `Vec<Value>` と行キー
/// `id` を取り出す。`sql::parser::bind_insert` の束縛規則を JSON 入力向けに
/// 焼き直したもので、束縛結果の意味（未知列・型不一致は `22000`、非 nullable
/// 列欠落は `23502`。`VECTOR` 列のみ `nullable` の値に関わらず必須という
/// NoSQL 表層固有の制約が上乗せされ、`nullable = false` なら `23502`・
/// `nullable = true` なら `22000` に分岐する。TABLE-16・TASK-204、
/// Issue #904 レビュー指摘）は SQL 表層と同一に保つ。
///
/// untrusted な列名・値をエラー文言へ埋め込まない（JSON 文字列はバックスラッシュ
/// `\u0000` エスケープ経由で NUL を含み得るため、`error_response::encode` の NUL
/// 拒否で `XX000` へ縮退しうる。`schema.rs::SchemaError::UnknownKey`・
/// `filter.rs::FilterError::UnsupportedOperator` と同じ判断）。
fn bind_row(
    item: &JsonValue,
    table: &str,
    operation_id: Option<&OperationId>,
    schema: &TableSchema,
) -> Result<BoundInsert, InsertError> {
    let JsonValue::Object(map) = item else {
        return Err(InsertError::Bind(invalid_input_error(
            "INSERT row must be a JSON object",
        )));
    };

    // `id` 疑似列: `engine::json::JsonNumber::PosInt`（小数点・指数部を含まない
    // 非負整数リテラルで、パース時点で `u64` として無損失にパース済みの表現）
    // のときのみ受理する。SQL 表層 `sql::parser::bind_insert` が SQL リテラルの
    // 生テキストから `u64` を `.parse()` するのと同じ「テキスト → 整数」の
    // 精度を持たせるための判定であり、`u64` 全域を許容範囲とする（SQL 表層との
    // パリティ。`sql::parser::bind_insert` 参照）。
    //
    // 以前は `JsonValue::Number(f64)` の丸め後の値を `fract() == 0.0` 等で
    // 事後判定していたが、`f64` 変換自体が精度を失うため
    // `9007199254740993`（2^53+1）のような safe range 境界直上の整数が
    // 最近傍の偶数 `9007199254740992` へ丸められ、上限検査をすり抜けて
    // 別の行 id へ書き込まれ得た（PR #823 レビュー指摘）。`JsonNumber` は
    // [`engine::json::JsonParser::parse_number`] が `f64` へ変換する前に
    // 整数リテラルを分類するため、この丸めが発生しない。
    //
    // `NegInt`（負の整数）・`Float`（小数点／指数部を含むリテラル。整数値に
    // 丸められる小数——例: `1.0`——を含む）はいずれも `id` として拒否する
    // （`id` は非負整数のみが妥当なため。文字列形の `id` も受理しない）。
    let id: u64 = match map.get("id") {
        Some(JsonValue::Number(n)) => match n.as_exact_u64() {
            Some(id) => id,
            None => {
                return Err(InsertError::Bind(invalid_input_error(
                    "INSERT row id must be a non-negative integer JSON number",
                )))
            }
        },
        _ => {
            return Err(InsertError::Bind(invalid_input_error(
                "INSERT row id must be a non-negative integer JSON number",
            )))
        }
    };

    // `columns`／`literals` は宣言順に対応する 2 本の Vec として組み立て、
    // `engine::sql::parser::bind_insert`（SQL 表層 `INSERT` と共有する公開
    // 束縛関数）へそのまま渡す。値の解析・範囲検証（整数のオーバーフロー・
    // `NUMERIC` の桁あふれ・暦上妥当性・`UUID` の文法等）は一切ここでは行わず、
    // `bind_insert` 側の単一情報源に委譲する（NOSQL-17。Issue #896）。
    let mut columns: Vec<String> = Vec::with_capacity(map.len());
    let mut literals: Vec<engine::sql::allowlist::InsertLiteral> = Vec::with_capacity(map.len());
    columns.push("id".to_string());
    literals.push(engine::sql::allowlist::InsertLiteral::Number(
        id.to_string(),
    ));

    for (key, raw) in map.iter() {
        if key == "id" {
            continue;
        }
        // untrusted な列名が engine 側のエラー文言へそのまま埋め込まれて
        // `XX000` へ縮退する経路を塞ぐ（`table`・`update.rs` の `set` キーと
        // 同じ判断・PR #823）。
        ident::check_identifier(key)?;
        let Some(column) = schema.columns.iter().find(|c| &c.name == key) else {
            return Err(InsertError::Bind(invalid_input_error(
                "INSERT row references an unknown column",
            )));
        };
        // JSON `null` の扱い（TABLE-16・TASK-204、Issue #904 D5）: `VECTOR` 列は
        // 引き続き列を省略する契約のまま維持する（`VECTOR` は
        // `tenant::insert_typed_rows_unchecked` の既存契約により `nullable` の
        // 値に関わらず常に必須で、下の「VECTOR 列は常に必須」ループが
        // `nullable = false` なら `23502`・`nullable = true` なら `22000` で
        // 拒否する〔レビュー指摘・Issue #904: SQL 表層〔`fill_omitted_columns`〕
        // との `wire_code` パリティのため非 nullable 側のみ `23502` へ分岐〕。
        // TABLE-16 の `DEFAULT`／`NOT NULL` は `VECTOR` に適用できないため
        // 対象外）。それ以外の列は `InsertLiteral::Null` を
        // そのまま `bind_insert` へ渡し、「省略」（`DEFAULT` 適用対象）と
        // 「明示 NULL」（`DEFAULT` を適用せず nullable なら NULL・非 nullable
        // なら `23502`）を区別する（TABLE-16 の確定契約。NOSQL-17 束縛表
        // 「null の扱い」節参照）。
        if matches!(raw, JsonValue::Null) {
            if matches!(column.ty, ColumnType::Vector(_)) {
                continue;
            }
            columns.push(key.clone());
            literals.push(engine::sql::allowlist::InsertLiteral::Null);
            continue;
        }
        let literal = typed_json::map_json_to_literal(column, raw).map_err(InsertError::Set)?;
        columns.push(key.clone());
        literals.push(literal);
    }

    // `VECTOR` 列は `nullable` の値に関わらず常に必須として扱う
    // （`tenant::insert_typed_rows_unchecked` の既存契約。上の JSON `null` 省略
    // 経路と合流させることで、明示 `null`・値の丸ごと省略のいずれも同じ
    // 「値が提供されていない」拒否になる。PR #823 レビュー指摘の非対称解消を
    // 引き続き維持する）。
    //
    // `wire_code` の分岐（TABLE-16・TASK-204、Issue #904 レビュー指摘）: 対象列が
    // `nullable == false` の場合、SQL 表層の同一操作（`fill_omitted_columns`
    // 経由。`ColumnType::Vector` に `DEFAULT` は付けられないため常に
    // `column.default.is_none()`）は `23502`（`NotNullViolation`）を返す。
    // NoSQL 表層だけが `22000` のまま据え置くと表層間で `wire_code` が食い違う
    // ため、非 nullable な VECTOR 列に限り SQL 表層と同じ `NotNullViolation`
    // へ写像する。`nullable == true` な VECTOR 列の省略拒否（PR #823 が導入した
    // 「VECTOR は nullable でも常に必須」という NoSQL 表層固有の制約。NOT NULL
    // 違反ではない）は従来どおり `22000` のまま変更しない。
    for column in &schema.columns {
        if matches!(column.ty, ColumnType::Vector(_)) && !columns.iter().any(|c| c == &column.name)
        {
            if !column.nullable {
                return Err(InsertError::Bind(SqlSurfaceError::NotNullViolation {
                    column: column.name.clone(),
                }));
            }
            return Err(InsertError::Bind(invalid_input_error(
                "INSERT row is missing a value for a required column",
            )));
        }
    }

    let validated = ValidatedInsert {
        table_name: table.to_string(),
        columns,
        rows: vec![literals],
        operation_id: operation_id.cloned(),
        // NoSQL 表層 `insert` op は `RETURNING`／`ON CONFLICT` を公開しない
        // （Issue #896 のスコープ外）。
        returning: None,
        on_conflict: None,
    };
    bind_insert(&validated, schema).map_err(InsertError::Bind)
}

/// `rows`（[`Validated::required_array`]`("rows")` が返す形。要素の型は
/// [`super::schema::INSERT_SCHEMA`] では `Any` としてしか検証されていないため
/// 各要素をここで検証する）を `table`／`operation_id` と合わせて
/// `Vec<BoundInsert>` へ束縛する。件数（`rows.len()`）は呼び出し元
/// （[`execute`]）が `EngineCore::execute_bound_insert_in_session` の row_count
/// 引数として先に渡し、①（件数上限）判定を通過した後にのみ本関数が呼ばれる。
pub fn bind_rows(
    items: &[JsonValue],
    table: &str,
    operation_id: Option<&OperationId>,
    schema: &TableSchema,
) -> Result<Vec<BoundInsert>, InsertError> {
    let mut bounds: Vec<BoundInsert> = Vec::new();
    bounds.try_reserve_exact(items.len()).map_err(|_| {
        InsertError::Bind(SqlSurfaceError::Internal {
            detail: "failed to reserve INSERT row buffer".to_string(),
        })
    })?;
    for item in items {
        bounds.push(bind_row(item, table, operation_id, schema)?);
    }
    Ok(bounds)
}

/// `insert` op 成功時の応答材料（[`encode_success_body`] の唯一の情報源）。
/// `operation_id` は `execute` 内で 1 回だけ `OperationId::parse` した値を
/// そのまま持ち回る（`validated` から `handle` が再抽出する二重パースは
/// 行わない。情報源を単一に保つ設計判断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertSuccess {
    /// [`InsertOutcome::rows_affected`]（書き込んだ行数）。
    pub inserted: u64,
    /// 要求で検証済みの `operation_id`。
    pub operation_id: OperationId,
}

/// [`InsertSuccess`] を成功応答本文（JSON）へ写像する（infallible）。
///
/// 出力形 `{"inserted":<n>,"operation_id":"<escaped>"}`。キー順固定・空白
/// なし・0x20 未満のバイトを含まない（[`crate::http::error_body::encode`]・
/// `super::response::encode` と同じ不変条件。`Content-Length` 算出対象を
/// 安定させるため）。`operation_id` は [`escape_json_string_into`] を通す
/// （`"`／`\` の混入がありうるため。`OperationId::parse` は制御文字を既に
/// 拒否済みだが多層防御として統一する）。`InsertOutcome::incremental` は
/// 行形では常に `None`（ファイル形専用）のため本文へは含めない。
pub fn encode_success_body(success: &InsertSuccess) -> String {
    // 固定オーバーヘッド + `operation_id`（≤ 256 バイト。`OperationId::parse`
    // が検証済み）+ u64 の最大桁数の概算のみを事前確保する（`inserted` の
    // 具体的な桁数は `write!` が可変長で埋める）。
    let mut out = String::with_capacity(success.operation_id.as_str().len() + 64);
    out.push_str("{\"inserted\":");
    // `u64` の `Display` 実装は infallible（`write!` への `String` 追記も
    // アロケーション失敗以外で失敗しない）ため戻り値は捨ててよい。
    let _ = write!(out, "{}", success.inserted);
    out.push_str(",\"operation_id\":\"");
    escape_json_string_into(&mut out, success.operation_id.as_str());
    out.push_str("\"}");
    out
}

/// `POST /v1/query` の `insert` op を実行する。`validated` は
/// [`super::schema::ObjectSchema::validate`]（`super::schema::INSERT_SCHEMA` に対する呼び出し）
/// を通過済みの JSON オブジェクト。
///
/// `operation_id` の欠落・`null`・空文字はいずれも `OperationId::parse` により
/// `23502` へ収束する（`optional_str` は欠落・`null` の双方を `None` として返す
/// 契約のため、`None` を空文字と同じ扱いで `OperationId::parse("")` へ通す）。
pub fn execute(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<InsertSuccess, InsertError> {
    let table = validated
        .required_str("table")
        .map_err(InsertError::Shape)?;
    ident::check_identifier(table)?;
    let rows = validated
        .required_array("rows")
        .map_err(InsertError::Shape)?;
    let operation_id_raw = validated
        .optional_str("operation_id")
        .map_err(InsertError::Shape)?
        .unwrap_or("");
    let operation_id = OperationId::parse(operation_id_raw).map_err(InsertError::Exec)?;

    let outcome: InsertOutcome = core
        .execute_bound_insert_in_session(
            principal.policy_context(),
            table,
            rows.len(),
            Some(&operation_id),
            |schema| {
                bind_rows(rows, table, Some(&operation_id), schema).map_err(|e| match e {
                    InsertError::Bind(err) | InsertError::Exec(err) => err,
                    // `bind_rows`（`bind_row` を行ごとに呼ぶ）は列キーの識別子形状検査
                    // （`ident::check_identifier`。NUL・制御文字・63 文字上限等）を
                    // 行うため `InsertError::InvalidIdentifier` を実際に構築しうる
                    // （Cursor Bugbot 指摘。かつては「本関数冒頭の `table` 検査でしか
                    // 構築されない」という誤った前提で `Internal`〔`XX000`〕へ丸めて
                    // いたため、不正な列キーを含む insert が `42601` ではなく内部
                    // エラー相当のコードで返っていた）。`Shape` は `schema.rs` の
                    // 意味的検証専用で `bind_rows` からは構築されないため、`Internal`
                    // への丸め込みを維持する。
                    InsertError::InvalidIdentifier => SqlSurfaceError::UnsupportedSyntax {
                        detail: "invalid identifier".to_string(),
                    },
                    InsertError::Shape(_) => SqlSurfaceError::Internal {
                        detail: "unexpected shape error during INSERT row binding".to_string(),
                    },
                    // `TypedJsonError`（NOSQL-17。Issue #896）の分類は単一の
                    // `into_sql_surface_error` 変換点に集約する（`update.rs` と共有）。
                    InsertError::Set(err) => err.into_sql_surface_error(),
                })
            },
        )
        .map_err(InsertError::Exec)?;

    // Issue #829（テスト専用・feature `fault-injection` 限定）: この直前の
    // `execute_bound_insert_in_session` が commit まで成功した直後（＝
    // `crate::http::conn::build_outcome` の `ResponseBoundaryGuard` が
    // 保護している区間の内側）にだけ検査する。これより後ろへ移動すると
    // 呼び出し元（`handle`）の応答整形（`encode_success_body`）まで通過して
    // しまい「commit 成功後の panic」を再現できなくなる。feature 無効時は
    // この呼び出しごとコンパイルされず、既定ビルドの挙動・コード生成は
    // 完全に不変（`crate::simple_query` の同型コメント参照）。
    #[cfg(feature = "fault-injection")]
    crate::fault_injection::maybe_panic_after_http_insert_commit();

    Ok(InsertSuccess {
        inserted: outcome.rows_affected,
        operation_id,
    })
}

/// `POST /v1/query` の `insert` op を処理し応答バイト列を返す
/// （`gate.rs` 手順 5 から `engine` 接続済み時のみ呼ばれる。`scan::handle`／
/// `aggregate::handle` と同一シグネチャ形）。成功時は [`encode_success_body`]
/// を `200` で、失敗時は [`InsertError`] の分類を `http_response::encode_error`
/// でそれぞれ応答へ写像する。
pub fn handle(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute(core, principal, validated) {
        Ok(success) => http_response::encode_ok(&encode_success_body(&success), now_wall),
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ColumnDef, TableSchema};
    use engine::json::parse_json;
    use engine::kernel::CpuScalarProvider;
    use engine::policy::PolicyContext;
    use engine::row_codec::Value;
    use engine::storage::Storage;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        )
    }

    fn rows_from(json: &str) -> Vec<JsonValue> {
        let JsonValue::Array(items) = parse_json(json).expect("valid JSON") else {
            panic!("expected array");
        };
        items
    }

    #[test]
    fn bind_rows_maps_id_and_columns() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let bounds = bind_rows(&items, "docs", None, &schema()).expect("bind ok");
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].id, 1);
        assert_eq!(bounds[0].table, "docs");
        assert_eq!(
            bounds[0].values,
            vec![
                Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ]
        );
    }

    #[test]
    fn bind_rows_rejects_non_object_row() {
        let items = rows_from(r#"["not-an-object"]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert!(matches!(err, InsertError::Bind(_)));
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_missing_id() {
        let items = rows_from(r#"[{"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_string_id() {
        let items = rows_from(r#"[{"id":"1","embedding":[1,0,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_unknown_column() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"lang":"ja","nope":"x"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_vector_dim_mismatch() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_non_finite_vector_element() {
        // JSON は NaN／Infinity を表現できないため（`engine::json::parse_json` が
        // 構文段階で拒否する）、ここでは非有限になり得る値そのものを渡せない。
        // 代わりに型不一致（文字列要素）で fail-closed 経路を固定する。
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,"x"],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    /// Issue #896 レビュー指摘（PR #1038）: `insert` op が `VECTOR` 列を
    /// `InsertLiteral::String`（`[f1,f2,...]` 形のテキストリテラル）経由で
    /// `engine::sql::parser::bind_insert` へ渡していたため、テキスト表現が
    /// 64 KiB（`MAX_VECTOR_LITERAL_BYTES`）を超える宣言次元の妥当なベクトルが
    /// `54000` で誤って拒否されていた。`InsertLiteral::Vector`（JSON 配列 →
    /// `f32` の直接構築。テキスト長上限を経由しない）への変更後は、
    /// スキーマ上合法な高次元ベクトルが JSON 配列の要素数上限の範囲内であれば
    /// 受理されることを固定する。
    #[test]
    fn bind_rows_accepts_high_dimension_vector_exceeding_text_literal_length_limit() {
        const DIM: usize = 20_000;
        let high_dim_schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "embedding",
                ColumnType::Vector(DIM as u32),
                false,
            )],
        );

        let mut embedding_json = String::from("[");
        for i in 0..DIM {
            if i > 0 {
                embedding_json.push(',');
            }
            embedding_json.push_str("0.1");
        }
        embedding_json.push(']');
        // テキスト表現が旧経路の 64 KiB 上限（`MAX_VECTOR_LITERAL_BYTES`）を
        // 超えることを確認する（超えなければ本テストは修正前の退行を検出できない）。
        assert!(embedding_json.len() > 64 * 1024);

        let body = format!(r#"[{{"id":1,"embedding":{embedding_json}}}]"#);
        let items = rows_from(&body);
        let bounds = bind_rows(&items, "docs", None, &high_dim_schema).expect("bind ok");
        assert_eq!(bounds.len(), 1);
        let Value::Vector(values) = &bounds[0].values[0] else {
            panic!("expected Value::Vector");
        };
        assert_eq!(values.len(), DIM);
        assert!(values.iter().all(|v| (*v - 0.1_f32).abs() < 1e-6));
    }

    #[test]
    fn bind_rows_rejects_missing_non_nullable_column() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0]}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        // TABLE-16・TASK-204、Issue #904: NOT NULL 違反は `23502`
        // （`NotNullViolation`）へ写像する（旧 `22000` から契約変更）。
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    // Issue #904 レビュー指摘: 非 nullable `VECTOR` 列（`schema()` の
    // `embedding`）を省略した INSERT が SQL 表層（`execute_insert_sql` 経由の
    // `crates/engine/tests/sql_create_table.rs::create_table_succeeds_with_vector_and_text_columns`）
    // と同じ `23502`（`NotNullViolation`）を返すことを固定する。`VECTOR` 列は
    // `fill_omitted_columns` を経由しない専用ループ（上記）で判定されるため、
    // SQL 表層とパリティが崩れやすい箇所として個別に検証する。
    fn bind_rows_rejects_omitted_non_nullable_vector_column() {
        let items = rows_from(r#"[{"id":1,"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    // Issue #904 レビュー指摘: 上のコメントは「明示 `null`・値の丸ごと省略の
    // いずれも同じ「値が提供されていない」拒否になる」と述べる。
    // `bind_rows_rejects_explicit_null_vector_column` は nullable スキーマ
    // （`22000`）のみを固定していたため、非 nullable スキーマ（`schema()`）
    // に対する明示 `null` も同じ `23502` になることを併せて固定する。
    fn bind_rows_rejects_explicit_null_on_non_nullable_vector_column() {
        let items = rows_from(r#"[{"id":1,"embedding":null,"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "23502");
    }

    // id 精度（PR #823 レビュー指摘対応）: `JsonNumber::PosInt` は `f64` へ丸める
    // 前の整数リテラルをそのまま `u64` として保持するため、2^53 を超える整数でも
    // 別の値へエイリアスせず正確に束縛されることを固定する。
    #[test]
    fn bind_rows_preserves_id_precision_beyond_f64_safe_integer_range() {
        // 2^53 + 1。旧実装（`f64` の丸め後に safe range 上限で判定）では
        // 最近傍の偶数 `9007199254740992`（2^53）へ丸められて上限検査を通過し、
        // 別の id の行として束縛され得た。
        let items = rows_from(r#"[{"id":9007199254740993,"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let bounds = bind_rows(&items, "docs", None, &schema()).expect("bind ok");
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].id, 9_007_199_254_740_993);
    }

    #[test]
    fn bind_rows_preserves_id_precision_at_u64_max() {
        let body = format!(
            r#"[{{"id":{},"embedding":[1,0,0,0],"lang":"ja"}}]"#,
            u64::MAX
        );
        let items = rows_from(&body);
        let bounds = bind_rows(&items, "docs", None, &schema()).expect("bind ok");
        assert_eq!(bounds[0].id, u64::MAX);
    }

    #[test]
    fn bind_rows_rejects_negative_id() {
        let items = rows_from(r#"[{"id":-1,"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_decimal_id_even_when_numerically_integral() {
        // `1.0` は数値としては整数だが、リテラルの構文上は小数点付き（`Float`
        // variant）であり、`id` は `PosInt`（整数リテラル）のみを受理する。
        let items = rows_from(r#"[{"id":1.0,"embedding":[1,0,0,0],"lang":"ja"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    // P2（nullable VECTOR 列省略時の bind/exec 非対称。PR #823 レビュー指摘対応）:
    // 明示 `null` は既存の `match` で拒否されるが、省略時も同じく `22000` で
    // 拒否されることを固定する（`tenant::insert_typed_rows_unchecked` が
    // VECTOR 列を無条件必須として扱う実行層の契約に bind 層を合わせる）。
    fn schema_with_nullable_vector() -> TableSchema {
        TableSchema::new(
            "docs_nullable_vec",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), true),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        )
    }

    #[test]
    fn bind_rows_rejects_omitted_nullable_vector_column() {
        let items = rows_from(r#"[{"id":1,"lang":"ja"}]"#);
        let err = bind_rows(
            &items,
            "docs_nullable_vec",
            None,
            &schema_with_nullable_vector(),
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_rejects_explicit_null_vector_column() {
        let items = rows_from(r#"[{"id":1,"embedding":null,"lang":"ja"}]"#);
        let err = bind_rows(
            &items,
            "docs_nullable_vec",
            None,
            &schema_with_nullable_vector(),
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rows_carries_operation_id_to_every_row() {
        let items = rows_from(
            r#"[{"id":1,"embedding":[1,0,0,0],"lang":"ja"},{"id":2,"embedding":[0,1,0,0],"lang":"en"}]"#,
        );
        let op_id = OperationId::parse("op-1").expect("valid operation_id");
        let bounds = bind_rows(&items, "docs", Some(&op_id), &schema()).expect("bind ok");
        assert_eq!(bounds.len(), 2);
        for b in &bounds {
            assert_eq!(b.operation_id, Some(op_id.clone()));
        }
    }

    #[test]
    fn encode_success_body_is_compact_with_fixed_key_order() {
        let success = InsertSuccess {
            inserted: 2,
            operation_id: OperationId::parse("op-1").expect("valid operation_id"),
        };
        assert_eq!(
            encode_success_body(&success),
            r#"{"inserted":2,"operation_id":"op-1"}"#
        );
    }

    #[test]
    fn encode_success_body_escapes_quotes_and_backslashes_in_operation_id() {
        let success = InsertSuccess {
            inserted: 1,
            operation_id: OperationId::parse("a\"b\\c").expect("valid operation_id"),
        };
        assert_eq!(
            encode_success_body(&success),
            r#"{"inserted":1,"operation_id":"a\"b\\c"}"#
        );
    }

    fn open_core() -> (EngineCore, std::path::PathBuf) {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "wire-insert-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        (
            EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
            path,
        )
    }

    fn principal(tenant: &str) -> SessionPrincipal {
        use crate::http::headers::{parse_headers, HeaderParse};
        use crate::http::session::middleware;
        use crate::http::session::store::SessionStore;
        use std::time::Instant;

        let sessions = SessionStore::new();
        let now = Instant::now();
        let ctx = PolicyContext::new(tenant).expect("valid ctx");
        let token = sessions.issue(ctx, now).expect("issue");
        let raw = format!(
            "Authorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
            token.encoded()
        );
        let leaked: &'static [u8] = Box::leak(raw.into_bytes().into_boxed_slice());
        let headers = match parse_headers(leaked).expect("header parse") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        };
        middleware::authenticate(&sessions, &headers, move || now).expect("authenticate")
    }

    #[test]
    fn execute_writes_rows_via_engine_core() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let success = execute(&core, &principal, &validated).expect("insert ok");
        assert_eq!(success.inserted, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_returns_operation_id_that_was_validated() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-echo"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let success = execute(&core, &principal, &validated).expect("insert ok");
        assert_eq!(success.operation_id.as_str(), "op-echo");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn handle_returns_200_with_success_body() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-handle"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(0);
        let response = handle(&core, &principal, &validated, now);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 200 "), "got: {text}");
        assert!(
            text.ends_with(r#"{"inserted":1,"operation_id":"op-handle"}"#),
            "got: {text}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn handle_maps_engine_error_to_error_response() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body =
            r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}]}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(0);
        let response = handle(&core, &principal, &validated, now);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("23502"), "got: {text}");
        let _ = std::fs::remove_file(&path);
    }

    // Cursor Bugbot 指摘（PR #823）: `table` が識別子形状検査
    // （`ident::check_identifier`）を通らずに engine へ渡ると、NUL 等の制御文字を
    // 含む値が `22000`／`42P01`／`XX000` のいずれかへ丸められ、兄弟 op（`search`／
    // `scan`／`aggregate`）と異なる `wire_code` として露出しうる。JSON 文字列は
    // `\u0000` エスケープ経由で NUL を表現できるため、この経路で `42601` が
    // 返ることを固定する。
    #[test]
    fn execute_rejects_table_name_containing_nul_as_invalid_identifier() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs\u0000","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "42601");
        let _ = std::fs::remove_file(&path);
    }

    // 列名（`id` 以外の JSON キー）に NUL を含む場合、`ident::check_identifier`
    // が engine へ渡す前に `42601` で拒否する（`execute_rejects_table_name_
    // containing_nul_as_invalid_identifier` と同じ判断を `rows[*]` のキーへも
    // 適用する。Issue #896 で `bind_row` が列ごとに `ident::check_identifier`
    // を呼ぶよう変更した結果——旧実装はこの検査を持たず、NUL を含む列名は
    // engine 側の「未知列」判定〔`22000`〕まで素通りしていた）。
    #[test]
    fn bind_rows_rejects_column_key_containing_nul_as_invalid_identifier() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"lang\u0000":"x"}]"#);
        let err = bind_rows(&items, "docs", None, &schema()).expect_err("must reject");
        assert_eq!(err.wire_code(), "42601");
    }

    /// `bind_rows_rejects_column_key_containing_nul_as_invalid_identifier` は
    /// `bind_rows` を直接呼ぶ単体テストであり、`execute` の束縛 closure が
    /// `InsertError::InvalidIdentifier` を `SqlSurfaceError::Internal`
    /// （`XX000`）へ丸め込んでいたバグ（Cursor Bugbot 指摘）は検出できて
    /// いなかった。本テストは本番経路（`execute`）を実際に通し、`42601` の
    /// まま到達することを固定する。
    #[test]
    fn execute_rejects_column_key_containing_nul_as_invalid_identifier() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang\u0000":"x"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "42601");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_missing_operation_id_is_23502() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body =
            r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}]}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "23502");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_null_operation_id_is_23502() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":null}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "23502");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_empty_operation_id_is_23502() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":""}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        assert_eq!(err.wire_code(), "23502");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_resend_same_operation_id_same_content_is_23505() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");
        execute(&core, &principal, &validated).expect("first insert ok");

        let err = execute(&core, &principal, &validated).expect_err("resend must fail");
        assert_eq!(err.wire_code(), "23505");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_resend_same_operation_id_different_content_is_22023() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let first = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"}],"operation_id":"op-1"}"#;
        let first_value = parse_json(first).expect("valid json");
        let first_validated = super::super::schema::INSERT_SCHEMA
            .validate(&first_value)
            .expect("schema ok");
        execute(&core, &principal, &first_validated).expect("first insert ok");

        let second = r#"{"op":"insert","table":"docs","rows":[{"id":2,"embedding":[0,1,0,0],"lang":"en"}],"operation_id":"op-1"}"#;
        let second_value = parse_json(second).expect("valid json");
        let second_validated = super::super::schema::INSERT_SCHEMA
            .validate(&second_value)
            .expect("schema ok");
        let err =
            execute(&core, &principal, &second_validated).expect_err("mismatched resend must fail");
        assert_eq!(err.wire_code(), "22023");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn execute_multi_row_batch_shares_one_operation_id() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        let body = r#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0,0],"lang":"ja"},{"id":2,"embedding":[0,1,0,0],"lang":"en"}],"operation_id":"op-1"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let success = execute(&core, &principal, &validated).expect("insert ok");
        assert_eq!(success.inserted, 2);
        let _ = std::fs::remove_file(&path);
    }

    // 表層横断の content_hash 一貫性（Issue #771 レビュー指摘対応）: SQL 表層
    // `execute_sql_in_session`（`sql::parser::parse_vector_literal`。`str -> f32`
    // 単一丸め）と NoSQL 表層 `execute`（`bind_row`。かつては
    // `JsonNumber::as_f64() as f32` の `str -> f64 -> f32` 2 回丸めだった）は
    // 同一のベクトルリテラル文字列を異なる丸めで `f32` へ変換しうるため、
    // 同一テナント・同一テーブル・同一 `id`・同一 `operation_id` で表層を跨いで
    // 再送しても `content_hash`（`recovery::content_hash::push_vector`）が
    // 一致せず「同一内容の再送」（`23505`）ではなく「内容不一致」（`22023`）と
    // 誤判定されうる。`1.0000000596046448` は 2 回丸め経路だと `1.0` になる一方
    // 単一丸め（`str -> f32` 直接パース）では異なるビットパターンになる値
    // （`json.rs::as_f32_matches_direct_str_parse_and_differs_from_f64_roundtrip`
    // と同じ数値）を使い、SQL → NoSQL・NoSQL → SQL の双方向で `23505` になる
    // ことを固定する。
    const DOUBLE_ROUNDING_LITERAL: &str = "1.0000000596046448";

    #[test]
    fn cross_surface_resend_same_vector_literal_is_23505_sql_then_nosql() {
        let (core, path) = open_core();
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let mut session = engine::sql::mode::SessionState::default();

        core.execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO docs (id, embedding, lang) VALUES (1, '[{DOUBLE_ROUNDING_LITERAL},0.2,0.3,0.4]', 'ja') USING OPERATION_ID 'op-cross-1'"
            ),
        )
        .expect("SQL insert ok");

        let http_principal = principal("tenant-a");
        let body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":1,"embedding":[{DOUBLE_ROUNDING_LITERAL},0.2,0.3,0.4],"lang":"ja"}}],"operation_id":"op-cross-1"}}"#
        );
        let value = parse_json(&body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &http_principal, &validated)
            .expect_err("resend of identical content across surfaces must be detected");
        assert_eq!(
            err.wire_code(),
            "23505",
            "SQL insert followed by identical NoSQL resend must be recognized as same-content"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cross_surface_resend_same_vector_literal_is_23505_nosql_then_sql() {
        let (core, path) = open_core();
        let http_principal = principal("tenant-a");
        let body = format!(
            r#"{{"op":"insert","table":"docs","rows":[{{"id":1,"embedding":[{DOUBLE_ROUNDING_LITERAL},0.2,0.3,0.4],"lang":"ja"}}],"operation_id":"op-cross-2"}}"#
        );
        let value = parse_json(&body).expect("valid json");
        let validated = super::super::schema::INSERT_SCHEMA
            .validate(&value)
            .expect("schema ok");
        execute(&core, &http_principal, &validated).expect("NoSQL insert ok");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let mut session = engine::sql::mode::SessionState::default();
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!(
                    "INSERT INTO docs (id, embedding, lang) VALUES (1, '[{DOUBLE_ROUNDING_LITERAL},0.2,0.3,0.4]', 'ja') USING OPERATION_ID 'op-cross-2'"
                ),
            )
            .expect_err("resend of identical content across surfaces must be detected");
        assert_eq!(
            err.wire_code(),
            "23505",
            "NoSQL insert followed by identical SQL resend must be recognized as same-content"
        );
        let _ = std::fs::remove_file(&path);
    }

    // --- BYTEA 列（Issue #886）の base64 束縛 ---------------------------------

    fn bytea_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("blob", ColumnType::Bytea, true),
            ],
        )
    }

    #[test]
    fn bind_rows_decodes_base64_bytea_column() {
        // "3q2+7w==" は [0xde, 0xad, 0xbe, 0xef] の標準 base64 表現。
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"blob":"3q2+7w=="}]"#);
        let bounds = bind_rows(&items, "docs", None, &bytea_schema()).expect("bind ok");
        assert_eq!(
            bounds[0].values,
            vec![
                Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
                Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            ]
        );
    }

    #[test]
    fn bind_rows_rejects_non_string_bytea_column() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"blob":123}]"#);
        let err = bind_rows(&items, "docs", None, &bytea_schema()).expect_err("must reject");
        assert!(matches!(
            err,
            InsertError::Set(TypedJsonError::TypeMismatch(_))
        ));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_rows_rejects_malformed_base64_bytea_column() {
        for bad in ["3q2+7w=", "3q2+7w=a", "!!!!"] {
            let items = rows_from(&format!(
                r#"[{{"id":1,"embedding":[1,0,0,0],"blob":"{bad}"}}]"#
            ));
            let err = bind_rows(&items, "docs", None, &bytea_schema()).expect_err("must reject");
            assert!(
                matches!(err, InsertError::Set(TypedJsonError::InvalidByteaText(_))),
                "input: {bad}"
            );
            assert_eq!(err.wire_code(), "22P02", "input: {bad}");
        }
    }

    #[test]
    fn bind_rows_accepts_null_bytea_column() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"blob":null}]"#);
        let bounds = bind_rows(&items, "docs", None, &bytea_schema()).expect("bind ok");
        assert_eq!(bounds[0].values[1], Value::Null);
    }

    // --- 新型（Issue #896）の INSERT 束縛 --------------------------------------

    fn typed_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("count", ColumnType::Integer, true),
                ColumnDef::new("score", ColumnType::Real, true),
                ColumnDef::new("active", ColumnType::Boolean, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
            ],
        )
    }

    #[test]
    fn bind_rows_maps_integer_real_boolean_numeric_columns() {
        let items = rows_from(
            r#"[{"id":1,"embedding":[1,0,0,0],"count":42,"score":1.5,"active":true,"amount":12.34}]"#,
        );
        let bounds = bind_rows(&items, "docs", None, &typed_schema()).expect("bind ok");
        assert_eq!(
            bounds[0].values,
            vec![
                Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
                Value::Integer(42),
                Value::Real(1.5),
                Value::Bool(true),
                engine::numeric::parse_for_column("12.34", 5, 2)
                    .map(Value::Numeric)
                    .expect("valid numeric"),
            ]
        );
    }

    #[test]
    fn bind_rows_rejects_float_for_integer_column() {
        // 小数は engine の整数束縛が `22P02` で拒否する（Issue #1187）。
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"count":1.5}]"#);
        let err = bind_rows(&items, "docs", None, &typed_schema()).expect_err("must reject");
        assert!(matches!(err, InsertError::Bind(_)));
        assert_eq!(err.wire_code(), "22P02");
    }

    #[test]
    fn bind_rows_rejects_integer_overflow_via_bind_insert() {
        // 範囲検証は `bind_insert`（engine 側の単一情報源）の責務。
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"count":2147483648}]"#);
        let err = bind_rows(&items, "docs", None, &typed_schema()).expect_err("must reject");
        assert!(matches!(err, InsertError::Bind(_)));
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn bind_rows_omits_null_integer_column() {
        let items = rows_from(r#"[{"id":1,"embedding":[1,0,0,0],"count":null}]"#);
        let bounds = bind_rows(&items, "docs", None, &typed_schema()).expect("bind ok");
        assert_eq!(bounds[0].values[1], Value::Null);
    }
}
