//! `POST /v1/query` の `insert`／`update`／`filter` が共有する
//! 「JSON 値 → `engine::sql::allowlist::InsertLiteral`」写像を集約するモジュール
//! （Issue #896・対象ビヘイビア NOSQL-17。ポインタ: `docs/spec/05-tasks.md`
//! TASK-175〜TASK-178・`docs/spec/04-behavior/nosql-surface.md` NOSQL-17）。
//!
//! 責務境界: JSON の種別（数値・文字列・真偽値・配列・オブジェクト）と列型の
//! 対応判定、および wire 表層固有の符号化（BYTEA の base64 ⇄ hex テキスト、
//! `ARRAY` 列の JSON 配列 ⇄ `{...}` テキスト、`JSON`／`JSONB` 列の正規化）
//! だけを担う。値そのものの解析・範囲検証（整数のオーバーフロー・
//! `NUMERIC` の桁あふれ・`DATE`／`TIMESTAMP` の暦上妥当性・`UUID` の文法等）は
//! 一切行わず、`InsertLiteral` として `engine::sql::parser::bind_insert`／
//! `bind_update`（SQL 表層と同一の束縛経路。`docs/design/
//! nosql-typed-json-binding.md` 参照）へ委譲する。第 2 の実行器を作らない
//! 設計方針は `insert.rs`／`update.rs` と同じ。
//!
//! 数値は `f64` を経由しない（[`engine::json::JsonNumber::PosInt`]／`NegInt` は
//! `to_string()`、`Float` は保持済みの生テキストをそのまま使う）。SQL 表層の
//! リテラルパーサーと同一の「テキストから直接解釈する」経路に載せることで、
//! 表層を跨いだ `content_hash`（TASK-101・RECOVER-10）の一致を保つ
//! （`update.rs::vector_literal_text` が既に確立していた設計をここへ集約する）。
//!
//! `TypedJsonError` の文言は untrusted な値・列名を埋め込まない固定文言
//! （`security.md` P0「情報漏えい」対応）。唯一の例外は ENUM 語彙外ラベル
//! （クライアント自身が送った値そのものであり、他テナントの情報を含まない。
//! `insert.rs::InsertError::InvalidEnumLabel` の既存判断を踏襲）。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::{JsonNumber, JsonValue};
use engine::sql::allowlist::{InsertLiteral, SqlSurfaceError};

/// [`map_json_to_literal`] 系関数の失敗を表す。[`ClassifiedError`] を実装し、
/// `insert.rs::InsertError`／`update.rs::UpdateError` の対応する variant へ
/// 1 対 1 で写像される（呼び出し元は `match` で個別 variant へ変換するのみで、
/// 分類判定自体はここに集約する）。
#[derive(Debug, Clone)]
pub enum TypedJsonError {
    /// JSON の種別が列型と噛み合わない（`42601`）。
    TypeMismatch(&'static str),
    /// `TEXT`／`VECTOR` 列（Issue #896 以前から対応済みの「旧来型」）の値・
    /// 型不一致（`22000`）。両列型は本 Issue 導入前から `nosql-api.md` が
    /// `22000` と明記済みのため、新型（`42601`）とは意図的に異なる分類を
    /// 維持する（`docs/design/nosql-typed-json-binding.md`「TEXT/VECTOR の
    /// 22000 非対称」節参照。表層内の非対称はオーナー確認事項）。
    LegacyMismatch(&'static str),
    /// `BYTEA` 列の値が base64 として不正（`22P02`。Issue #1187）。JSON string
    /// 以外が渡された種別不一致は [`TypedJsonError::TypeMismatch`]（`42601`）。
    InvalidByteaText(&'static str),
    /// `BYTEA` 列の base64 値が復号後 [`engine::bytea::MAX_BYTEA_FIELD_LEN`] を
    /// 超える（`54000`）。
    ByteaTooLarge,
    /// `JSON`／`JSONB` 列の値が JSON オブジェクト／配列として不整合（`42601`）。
    InvalidJson(&'static str),
    /// `JSON`／`JSONB` 列の値が正規化後 [`engine::json::MAX_JSON_FIELD_LEN`] を
    /// 超える（`54000`）。
    JsonTooLarge,
    /// `ARRAY` 列の要素数が宣言済み上限を超える（`54000`）。
    ArrayTooLarge,
    /// `ENUM` 列の値が語彙外のラベル（`22P02`）。
    InvalidEnumLabel(String),
}

impl ClassifiedError for TypedJsonError {
    fn error_class(&self) -> ErrorClass {
        match self {
            TypedJsonError::TypeMismatch(_) => ErrorClass::UnsupportedSqlSyntax,
            TypedJsonError::LegacyMismatch(_) => ErrorClass::InvalidInput,
            TypedJsonError::InvalidByteaText(_) => ErrorClass::InvalidTextRepresentation,
            TypedJsonError::ByteaTooLarge => ErrorClass::PayloadTooLarge,
            TypedJsonError::InvalidJson(_) => ErrorClass::UnsupportedSqlSyntax,
            TypedJsonError::JsonTooLarge => ErrorClass::PayloadTooLarge,
            TypedJsonError::ArrayTooLarge => ErrorClass::PayloadTooLarge,
            TypedJsonError::InvalidEnumLabel(_) => ErrorClass::InvalidTextRepresentation,
        }
    }

    fn client_message(&self) -> String {
        match self {
            TypedJsonError::TypeMismatch(detail) => detail.to_string(),
            TypedJsonError::LegacyMismatch(detail) => detail.to_string(),
            TypedJsonError::InvalidByteaText(detail) => detail.to_string(),
            TypedJsonError::ByteaTooLarge => "BYTEA value exceeds the length limit".to_string(),
            TypedJsonError::InvalidJson(detail) => detail.to_string(),
            TypedJsonError::JsonTooLarge => "JSON value exceeds the length limit".to_string(),
            TypedJsonError::ArrayTooLarge => {
                "ARRAY value exceeds the element count limit".to_string()
            }
            TypedJsonError::InvalidEnumLabel(detail) => detail.clone(),
        }
    }
}

impl TypedJsonError {
    /// `EngineCore::execute_bound_insert_in_session`／
    /// `execute_bound_update_in_session` の束縛 closure が要求する
    /// `Result<_, SqlSurfaceError>` へ写像する（`insert.rs`／`update.rs` の
    /// 束縛 closure が共有する。分類は [`ClassifiedError::error_class`] と
    /// 同一の判定点から導出し、`wire_code` の二重管理を避ける）。
    pub fn into_sql_surface_error(self) -> SqlSurfaceError {
        let detail = self.client_message();
        match self.error_class() {
            ErrorClass::PayloadTooLarge => SqlSurfaceError::PayloadTooLarge { detail },
            ErrorClass::InvalidTextRepresentation => {
                SqlSurfaceError::InvalidTextRepresentation { detail }
            }
            ErrorClass::InvalidInput => SqlSurfaceError::InvalidInput { detail },
            // `UnsupportedSqlSyntax`（`TypeMismatch`／
            // `InvalidJson`）以外の分類はここには到達しない
            // （[`TypedJsonError::error_class`] の網羅から明らか）が、
            // fail-closed に保つため既定は `UnsupportedSyntax` とする。
            _ => SqlSurfaceError::UnsupportedSyntax { detail },
        }
    }
}

/// JSON 数値リテラルの生テキスト化（`f64` を経由しない）。`NegInt(0)`（JSON
/// `-0`）は `"-0"` として直列化する（SQL 表層の `-0.0` 保持契約との整合。
/// `update.rs::vector_literal_text` の既存規則を数値列全般へ一般化する）。
pub fn number_literal_text(n: &JsonNumber) -> String {
    match n {
        JsonNumber::PosInt(v) => v.to_string(),
        JsonNumber::NegInt(0) => "-0".to_string(),
        JsonNumber::NegInt(v) => v.to_string(),
        JsonNumber::Float { text, .. } => text.to_string(),
    }
}

/// `VECTOR` 列向け配列 → `f32` 要素列（`update.rs::vector_literal_text` の
/// 移設・Issue #896 レビュー指摘〔PR #1038〕による再設計）。旧実装は
/// `[f1,f2,...]` 形のテキストへ直列化し `InsertLiteral::String` として
/// `engine::sql::parser::parse_vector_literal`（64 KiB のテキスト長上限）を
/// 経由していたため、宣言次元が大きく JSON 配列自体は妥当でもテキスト表現が
/// 64 KiB を超える正当なベクトルを誤って拒否していた（`54000`）。本関数は
/// 旧経路（Issue #896 以前の `insert.rs::bind_row`）と同じ直接構築方式へ
/// 戻し、次元一致を**アロケーション前**に検査してから（`security.md`
/// 「不安全な設計」対応。`dim` は `catalog::validate_vector_dim` で
/// `MAX_VECTOR_DIM` 以下と CREATE TABLE 時点で保証済みのため安全な上限として
/// 使える）`Vec<f32>` を構築する。各要素は [`JsonNumber::as_f32`] が有限値と
/// して解釈できることを要求する（SQL 表層 `parse_vector_literal` の
/// `str -> f32` 単一丸めと同一実装を経由するため表層横断で `content_hash` が
/// 一致する。Issue #771 レビュー指摘対応の設計を踏襲）。`VECTOR` は旧来型の
/// ため要素の型不一致・非有限・次元不一致のいずれも `LegacyMismatch`
/// （`22000`）で統一する（`insert.rs::bind_rows_rejects_non_finite_vector_element`
/// の既存契約）。
pub fn vector_literal_values(items: &[JsonValue], dim: u32) -> Result<Vec<f32>, TypedJsonError> {
    if items.len() != dim as usize {
        return Err(TypedJsonError::LegacyMismatch(
            "VECTOR column length does not match the table dimension",
        ));
    }
    let mut values: Vec<f32> = Vec::with_capacity(items.len());
    for item in items {
        let JsonValue::Number(n) = item else {
            return Err(TypedJsonError::LegacyMismatch(
                "VECTOR column element must be a JSON number",
            ));
        };
        let Some(f) = n.as_f32() else {
            return Err(TypedJsonError::LegacyMismatch(
                "VECTOR column element must be finite",
            ));
        };
        values.push(f);
    }
    Ok(values)
}

/// `{...}` 形の配列リテラル要素 1 個を組み立てる。`quote` が `true` のときは
/// `"`／`\` をエスケープしたうえで二重引用符で囲む（`engine::sql::parser::
/// parse_array_literal` の引用要素解釈と対称）。`quote` が `false` のときは
/// 未加工のまま出力する（JSON `null` 要素専用。D-A6・`parse_array_literal` の
/// 「配列リテラルは NULL 要素を受理しない」契約に照合させ `22000` として拒否
/// させるため）。
fn push_array_element(out: &mut String, text: &str, quote: bool) {
    if !quote {
        out.push_str(text);
        return;
    }
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

/// `ARRAY` 列（`TEXT[]`／`BOOLEAN[]`）向け配列直列化。要素種別を
/// `array_ty.elem()` に応じて検証したうえで `{...}` テキストを組み立てる。
/// 要素数が [`ArrayType::max_len`] を超える場合はテキストを組み立てる**前**に
/// 拒否する（`54000`。アロケーション前の上限検査。`security.md`「不安全な
/// 設計」対応）。JSON `null` 要素は引用なしの `NULL` として出力し、判定自体は
/// `engine::sql::parser::parse_array_literal`（`22000`）へ委譲する。
pub fn array_literal_text(
    items: &[JsonValue],
    array_ty: ArrayType,
) -> Result<String, TypedJsonError> {
    if items.len() as u64 > array_ty.max_len() as u64 {
        return Err(TypedJsonError::ArrayTooLarge);
    }
    let mut out = String::from("{");
    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        match (array_ty.elem(), item) {
            (_, JsonValue::Null) => push_array_element(&mut out, "NULL", false),
            (ArrayElemType::Text, JsonValue::String(s)) => push_array_element(&mut out, s, true),
            (ArrayElemType::Bool, JsonValue::Bool(b)) => {
                push_array_element(&mut out, if *b { "true" } else { "false" }, true)
            }
            (ArrayElemType::Text, _) => {
                return Err(TypedJsonError::TypeMismatch(
                    "ARRAY column element must be a JSON string",
                ))
            }
            (ArrayElemType::Bool, _) => {
                return Err(TypedJsonError::TypeMismatch(
                    "ARRAY column element must be a JSON boolean",
                ))
            }
        }
    }
    out.push('}');
    Ok(out)
}

/// `BYTEA` 列向け: base64 の JSON string を復号し、`engine::bytea::
/// format_hex_text`（`\x` ＋ 小文字 hex）へ再エンコードする（`insert.rs`・
/// `update.rs` の既存 BYTEA 分岐と同一の判断。engine 側の束縛経路を hex 解析
/// 1 本に保つ）。
pub fn bytea_literal_text(s: &str) -> Result<String, TypedJsonError> {
    let decoded = super::base64_std::decode_base64_std(s, engine::bytea::MAX_BYTEA_FIELD_LEN)
        .map_err(|e| match e {
            super::base64_std::Base64StdError::TooLong => TypedJsonError::ByteaTooLarge,
            _ => TypedJsonError::InvalidByteaText("invalid input syntax for type bytea (base64)"),
        })?;
    Ok(engine::bytea::format_hex_text(&decoded))
}

/// `JSON`／`JSONB` 列向け: JSON オブジェクト／配列を正規化テキストへ写像する
/// （`insert.rs`・`update.rs` の既存 JSON 分岐と同一の判断。スカラー JSON は
/// 呼び出し元が事前に拒否する）。
pub fn json_literal_text(raw: &JsonValue) -> Result<String, TypedJsonError> {
    let mut canonical = String::new();
    engine::json::write_canonical(raw, &mut canonical);
    if canonical.len() > engine::json::MAX_JSON_FIELD_LEN {
        return Err(TypedJsonError::JsonTooLarge);
    }
    Ok(canonical)
}

/// `raw`（JSON 値）を `column` の列型に応じて `InsertLiteral` へ写像する
/// （NOSQL-17 の束縛表そのもの）。呼び出し元の責務:
///
/// - `raw` が `JsonValue::Null` の場合の扱いは呼び出し元が決める（`insert` op
///   は列を省略する。`update` op はそのまま [`InsertLiteral::Null`] を engine
///   （`bind_set_assignments`）の nullable 判定へ委ねる）ため、本関数は
///   `JsonValue::Null` を `InsertLiteral::Null` へ写像するのみで nullable
///   検査は行わない——ただし `TEXT`／`ENUM` 列は例外で、Issue #896 以前の
///   `update` op が列の `nullable` 属性に関わらず一律拒否していた契約を
///   維持するため、`null` は列型を問わず（nullable 列でも）ここで拒否する
///   （PR #1038 レビュー指摘。契約変更が必要ならオーナー承認・spec 改訂を
///   別途経る）。
/// - 列名の識別子形状検査（[`super::ident::check_identifier`]）は呼び出し元が
///   先に済ませておくこと（本関数はエラー文言に列名を含めないため直接には
///   影響しないが、多層防御の判定順序は呼び出し元の責務）。
pub fn map_json_to_literal(
    column: &ColumnDef,
    raw: &JsonValue,
) -> Result<InsertLiteral, TypedJsonError> {
    // TEXT／ENUM 列は Issue #896 以前の `update` op が `null` を列の
    // `nullable` 属性に関わらず一律拒否していた契約を維持する（PR #1038
    // レビュー指摘。`map_json_to_literal` が `null` を列型を問わず一律
    // `InsertLiteral::Null` へ写像し `bind_update` の nullable 判定へ
    // 委譲する一般化〔`docs/design/nosql-typed-json-binding.md`「null の
    // 扱い」節〕は、nullable な TEXT／ENUM 列への `null` 受理拡大という
    // 契約変更を伴い、同 doc 自身も未確定事項〔オーナー確認要〕と記載して
    // いた。契約確定までは安全側として従来の拒否を維持し、他の型
    // （nullable 判定を `bind_update` へ委譲する設計自体）には影響しない。
    match (&column.ty, raw) {
        (ColumnType::Text, JsonValue::Null) => {
            return Err(TypedJsonError::LegacyMismatch(
                "TEXT column value must be a JSON string",
            ))
        }
        (ColumnType::Enum(_), JsonValue::Null) => {
            return Err(TypedJsonError::TypeMismatch(
                "ENUM column value must be a JSON string",
            ))
        }
        _ => {}
    }
    if matches!(raw, JsonValue::Null) {
        return Ok(InsertLiteral::Null);
    }
    match &column.ty {
        // TEXT は Issue #896 以前から対応済みの旧来型のため、値・型不一致は
        // `LegacyMismatch`（`22000`）のまま維持する（`nosql-api.md` の既存契約）。
        ColumnType::Text => match raw {
            JsonValue::String(s) => Ok(InsertLiteral::String(s.clone())),
            _ => Err(TypedJsonError::LegacyMismatch(
                "TEXT column value must be a JSON string",
            )),
        },
        ColumnType::Boolean => match raw {
            JsonValue::Bool(b) => Ok(InsertLiteral::Bool(*b)),
            _ => Err(TypedJsonError::TypeMismatch(
                "BOOLEAN column value must be a JSON boolean",
            )),
        },
        ColumnType::Integer | ColumnType::BigInt => match raw {
            JsonValue::Number(n @ (JsonNumber::PosInt(_) | JsonNumber::NegInt(_))) => {
                Ok(InsertLiteral::Number(number_literal_text(n)))
            }
            // `u64`／`i64` に収まらない整数リテラル（小数点・指数部を含まない）は
            // `engine::json::parse_number` が `JsonNumber::Float` へフォールバック
            // する。小数・指数表記（`1.5`・`1e3`）を含め、Float はすべて生テキストの
            // まま engine の整数束縛（`bind_integer_literal`）へ委譲する: 範囲外は
            // `22003`、整数として解析できない小数・指数表記は `22P02`（Issue #1187・
            // NOSQL-17）。判定点を engine に一本化し、SQL 表層と分類を揃える。
            JsonValue::Number(n @ JsonNumber::Float { .. }) => {
                Ok(InsertLiteral::Number(number_literal_text(n)))
            }
            _ => Err(TypedJsonError::TypeMismatch(
                "INTEGER/BIGINT column value must be a JSON number",
            )),
        },
        ColumnType::Real | ColumnType::Double => match raw {
            JsonValue::Number(n) => Ok(InsertLiteral::Number(number_literal_text(n))),
            _ => Err(TypedJsonError::TypeMismatch(
                "REAL/DOUBLE PRECISION column value must be a JSON number",
            )),
        },
        ColumnType::Numeric { .. } => match raw {
            JsonValue::Number(n) => Ok(InsertLiteral::Number(number_literal_text(n))),
            JsonValue::String(s) => Ok(InsertLiteral::String(s.clone())),
            _ => Err(TypedJsonError::TypeMismatch(
                "NUMERIC column value must be a JSON number or numeric string",
            )),
        },
        ColumnType::Date => match raw {
            JsonValue::String(s) => Ok(InsertLiteral::String(s.clone())),
            _ => Err(TypedJsonError::TypeMismatch(
                "DATE column value must be a JSON string",
            )),
        },
        ColumnType::Timestamp => match raw {
            JsonValue::String(s) => Ok(InsertLiteral::String(s.clone())),
            _ => Err(TypedJsonError::TypeMismatch(
                "TIMESTAMP column value must be a JSON string",
            )),
        },
        ColumnType::Uuid => match raw {
            JsonValue::String(s) => Ok(InsertLiteral::String(s.clone())),
            _ => Err(TypedJsonError::TypeMismatch(
                "UUID column value must be a JSON string",
            )),
        },
        ColumnType::Enum(def) => match raw {
            JsonValue::String(s) => {
                if def.validate_label(s).is_err() {
                    return Err(TypedJsonError::InvalidEnumLabel(format!(
                        "column value {s:?} is not a member of enum type {:?}",
                        def.name()
                    )));
                }
                Ok(InsertLiteral::String(s.clone()))
            }
            _ => Err(TypedJsonError::TypeMismatch(
                "ENUM column value must be a JSON string",
            )),
        },
        // VECTOR も同じく旧来型のため `LegacyMismatch`（`22000`）を維持する。
        // 配列要素自体の型不一致・次元不一致は [`vector_literal_values`] が
        // 同じ `LegacyMismatch` で返すため、ここでは列トップレベルの型不一致
        // （配列でない）のみを対象とする。`InsertLiteral::Vector`（Issue #896
        // レビュー指摘・PR #1038）として直接構築し、テキストリテラル経由の
        // 64 KiB 上限を経由しない（`vector_literal_values` のドキュメント
        // コメント参照）。
        ColumnType::Vector(dim) => match raw {
            JsonValue::Array(items) => {
                Ok(InsertLiteral::Vector(vector_literal_values(items, *dim)?))
            }
            _ => Err(TypedJsonError::LegacyMismatch(
                "VECTOR column value must be a JSON array of numbers",
            )),
        },
        ColumnType::Array(array_ty) => match raw {
            JsonValue::Array(items) => {
                Ok(InsertLiteral::String(array_literal_text(items, *array_ty)?))
            }
            _ => Err(TypedJsonError::TypeMismatch(
                "ARRAY column value must be a JSON array",
            )),
        },
        ColumnType::Bytea => match raw {
            JsonValue::String(s) => Ok(InsertLiteral::String(bytea_literal_text(s)?)),
            _ => Err(TypedJsonError::TypeMismatch(
                "BYTEA column value must be a base64 JSON string",
            )),
        },
        ColumnType::Json | ColumnType::Jsonb => match raw {
            JsonValue::Object(_) | JsonValue::Array(_) => {
                Ok(InsertLiteral::String(json_literal_text(raw)?))
            }
            _ => Err(TypedJsonError::InvalidJson(
                "JSON column value must be a JSON object or array",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType};
    use engine::json::parse_json;

    fn col(ty: ColumnType) -> ColumnDef {
        ColumnDef::new("c", ty, true)
    }

    fn num(json: &str) -> JsonValue {
        parse_json(json).expect("valid json")
    }

    #[test]
    fn maps_integer_from_json_number() {
        let lit = map_json_to_literal(&col(ColumnType::Integer), &num("42")).expect("ok");
        assert_eq!(lit, InsertLiteral::Number("42".to_string()));
    }

    #[test]
    fn passes_float_for_integer_column_to_engine_binding() {
        // 小数・指数表記の拒否（`22P02`）は engine の `bind_integer_literal` が担う
        // （Issue #1187。判定点の一本化）。
        for raw in ["1.5", "1e3"] {
            let lit = map_json_to_literal(&col(ColumnType::Integer), &num(raw)).expect("ok");
            assert_eq!(lit, InsertLiteral::Number(raw.to_string()));
        }
    }

    #[test]
    fn maps_real_preserving_raw_text() {
        let lit = map_json_to_literal(&col(ColumnType::Real), &num("1.5")).expect("ok");
        assert_eq!(lit, InsertLiteral::Number("1.5".to_string()));
    }

    #[test]
    fn maps_numeric_from_number_or_string() {
        let lit = map_json_to_literal(
            &col(ColumnType::Numeric {
                precision: 5,
                scale: 2,
            }),
            &num("12.34"),
        )
        .expect("ok");
        assert_eq!(lit, InsertLiteral::Number("12.34".to_string()));
        let lit = map_json_to_literal(
            &col(ColumnType::Numeric {
                precision: 5,
                scale: 2,
            }),
            &JsonValue::String("12.34".to_string()),
        )
        .expect("ok");
        assert_eq!(lit, InsertLiteral::String("12.34".to_string()));
    }

    #[test]
    fn maps_boolean() {
        let lit =
            map_json_to_literal(&col(ColumnType::Boolean), &JsonValue::Bool(true)).expect("ok");
        assert_eq!(lit, InsertLiteral::Bool(true));
    }

    #[test]
    fn maps_null_to_insert_literal_null_regardless_of_column_type() {
        let lit = map_json_to_literal(&col(ColumnType::Integer), &JsonValue::Null).expect("ok");
        assert_eq!(lit, InsertLiteral::Null);
    }

    // PR #1038 レビュー指摘: `TEXT`／`ENUM` 列は Issue #896 以前の `update` op
    // が `null` を `nullable` 属性に関わらず一律拒否していた契約を維持する
    // （nullable 列でも拒否する。上記の「他の列型は列型を問わず
    // `InsertLiteral::Null` へ写像する」一般化からの意図的な例外）。
    #[test]
    fn rejects_null_for_text_column_even_when_nullable() {
        let err =
            map_json_to_literal(&col(ColumnType::Text), &JsonValue::Null).expect_err("reject");
        assert!(matches!(err, TypedJsonError::LegacyMismatch(_)));
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_null_for_enum_column_even_when_nullable() {
        // `EnumTypeDef` はフィールドが private で `Storage::create_enum_type`
        // 経由でのみ構築できる（`result_encoder.rs` の既存テストと同じ判断）。
        let db_path = std::env::temp_dir().join(format!(
            "typed-json-enum-null-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let storage = engine::storage::Storage::open(&db_path).expect("open throwaway storage");
        let enum_def = storage
            .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
            .expect("create enum type");
        let err = map_json_to_literal(&col(ColumnType::Enum(enum_def)), &JsonValue::Null)
            .expect_err("reject");
        drop(storage);
        let _ = std::fs::remove_file(&db_path);
        assert!(matches!(err, TypedJsonError::TypeMismatch(_)));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn array_literal_text_quotes_and_escapes_text_elements() {
        let array_ty = ArrayType::new(ArrayElemType::Text, 8).expect("valid array type");
        let items = vec![
            JsonValue::String("a\"b\\c".to_string()),
            JsonValue::String("plain".to_string()),
        ];
        let text = array_literal_text(&items, array_ty).expect("ok");
        assert_eq!(text, r#"{"a\"b\\c","plain"}"#);
    }

    #[test]
    fn array_literal_text_rejects_element_type_mismatch() {
        let array_ty = ArrayType::new(ArrayElemType::Bool, 8).expect("valid array type");
        let items = vec![JsonValue::String("true".to_string())];
        let err = array_literal_text(&items, array_ty).expect_err("reject");
        assert!(matches!(err, TypedJsonError::TypeMismatch(_)));
    }

    #[test]
    fn array_literal_text_rejects_over_max_len_before_building_text() {
        let array_ty = ArrayType::new(ArrayElemType::Text, 1).expect("valid array type");
        let items = vec![
            JsonValue::String("a".to_string()),
            JsonValue::String("b".to_string()),
        ];
        let err = array_literal_text(&items, array_ty).expect_err("reject");
        assert!(matches!(err, TypedJsonError::ArrayTooLarge));
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn array_literal_text_emits_unquoted_null_for_json_null_element() {
        let array_ty = ArrayType::new(ArrayElemType::Text, 8).expect("valid array type");
        let items = vec![JsonValue::Null, JsonValue::String("a".to_string())];
        let text = array_literal_text(&items, array_ty).expect("ok");
        assert_eq!(text, r#"{NULL,"a"}"#);
    }

    #[test]
    fn number_literal_text_preserves_negative_zero() {
        assert_eq!(number_literal_text(&JsonNumber::NegInt(0)), "-0");
    }
}
