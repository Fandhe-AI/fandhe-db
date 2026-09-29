//! `POST /v1/query` の JSON クエリオブジェクトに対する意味的検証ヘルパー
//! （Issue #760。対象ビヘイビア TASK-175・NOSQL-8・HTTP-7。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-2〜NOSQL-10）。
//!
//! 責務境界: `engine::json::parse_json` は構文解析（untrusted テキスト →
//! [`engine::json::JsonValue`]）までが責務で、「意味的な検証は呼び出し元が
//! 行う」契約になっている（`json.rs` モジュールドキュメント参照）。本モジュール
//! はその呼び出し元側の共通部品で、op ごとに宣言した [`ObjectSchema`] に照らし
//! 「必須キー欠落」「未知キー」「型不一致（`null` を含む）」の 3 類型を検査する。
//! いずれも [`SchemaError`] として `42601`（`UnsupportedSqlSyntax`）へ fail-closed
//! に写像する（`engine::json::JsonError` と同じ分類。構文エラー・意味エラーが
//! クライアントには同じ `wire_code` として収束する）。
//!
//! op 名の許可リスト・未知 op の `0A000` 判定は [`super::op::Op`]
//! （Issue #759・TASK-179・NOSQL-1・NOSQL-9）が担い、[`schema_for`] は
//! `super::op::Op::parse` へ委譲する薄い表引きに留まる。
//!
//! 対象外（後続 Issue の担当。二重実装しない）:
//! - フィールド値の**語彙・範囲**検査（`filter[].op` の `eq`／`prefix`、
//!   `aggregates[].fn`／`having[].fn`／`having[].op` の関数名・演算子語彙、
//!   `limit` の非負性等。#761・#763・#766・#768。`group_by`／`having` の
//!   意味検証は [`super::aggregate`]（#769）が担う。`scan` の `sort[].dir`
//!   語彙〔`"asc"`／`"desc"` 以外の拒否〕は [`super::scan`]（Issue #946・
//!   NOSQL-15）が担う）
//!
//! `insert` op の `operation_id` 欠落／`null`／空文字 → `23502`・`rows[*]` の
//! 列検証・engine への束縛・実行は [`super::insert`]（Issue #771・NOSQL-6）が担う。
//!
//! `update`／`delete` op（Issue #875・NOSQL-12）の `set` 禁止列（`id`／
//! `tenant_id`／`visibility`）検査・`where`／`filter` の排他判定・列名解決・
//! `operation_id` 欠落／`null`／空文字 → `23502`・engine への束縛・実行は
//! [`super::update`]・[`super::delete`]・[`super::dml_target`]（Issue
//! #876・TASK-186・NOSQL-6・NOSQL-12）が担う。本モジュールでは
//! `UPDATE_SCHEMA`（`set` を任意キーのオブジェクトとして型のみ検査する
//! [`FieldType::AnyObject`]）・`DELETE_SCHEMA` の宣言に留める。
//!
//! 未知キーは無視せず拒否する（NOSQL-8 の一般則。クライアント自己申告の
//! `tenant_id`（HTTP-7）・`HINT ORDER`／`SET search_mode` 相当フィールド
//! （NOSQL-9）・`scan` への `vector`／`plan`／`mode`／`hybrid` 付与拒否
//! （NOSQL-3）は、いずれもこの一般則で自然に成立する）。

use std::collections::BTreeMap;

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;

/// [`ObjectSchema::validate`] の失敗を表す 3 類型。いずれも
/// [`ErrorClass::UnsupportedSqlSyntax`]（`42601`）へ写像する。
///
/// - `key` はスキーマ表由来の `&'static str`（本モジュールのソースコードに
///   埋め込まれた定数のみ）であり、untrusted なクライアント入力のキー名を
///   保持しない。特に [`SchemaError::UnknownKey`] はキー名を一切保持しない
///   （JSON 文字列はバックスラッシュ `\u0000` エスケープ経由で NUL を含み
///   得るため、untrusted キー名をエラー文言へ埋め込むと
///   `error_response::encode` の NUL 拒否で `XX000` へ縮退しうる。
///   security.md の「エラー・ログ経由で他テナントのデータ・存在情報を
///   漏らさない」方針にも沿い、固定文言のみとする）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaError {
    /// 必須フィールドが欠落している。
    MissingRequired { key: &'static str },
    /// スキーマに存在しないフィールドが含まれている。
    UnknownKey,
    /// フィールドの JSON 型が期待と一致しない（`null` を含む）。
    TypeMismatch { key: &'static str },
}

impl SchemaError {
    /// SQLSTATE 風 `wire_code`（`.claude/rules/coding-rust.md`）。
    /// [`ClassifiedError`] へ委譲する（`engine::json::JsonError::wire_code` と
    /// 同じパターン。実装型ごとに再定義しない）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }
}

impl ClassifiedError for SchemaError {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::UnsupportedSqlSyntax
    }

    fn client_message(&self) -> String {
        match self {
            SchemaError::MissingRequired { key } => format!("missing required field: {key}"),
            SchemaError::UnknownKey => "unknown field in request object".to_string(),
            SchemaError::TypeMismatch { key } => format!("type mismatch for field: {key}"),
        }
    }
}

/// フィールドの必須／任意を表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Required,
    Optional,
}

/// 配列要素に期待する JSON 型（[`FieldType::Array`] が使う）。
#[derive(Debug, Clone, Copy)]
pub enum ElementType {
    /// 要素の型を検査しない（`insert` の `rows[*]` のように任意列名・任意型を
    /// 持つ要素向け）。
    Any,
    String,
    Number,
    /// 要素が [`ObjectSchema`] に従うオブジェクトであることを再帰検証する。
    Object(&'static ObjectSchema),
}

/// フィールドに期待する JSON 型（値の範囲・語彙は検査しない。型のみ）。
#[derive(Debug, Clone, Copy)]
pub enum FieldType {
    Bool,
    Number,
    String,
    /// 文字列・数値・真偽値のいずれか（`null` は含まない。`nullable` で
    /// 別途扱う）。`filter[].value`（Issue #896・NOSQL-17）のように列型に
    /// 応じて種別を後段で判定するフィールド向け（`insert`／`update` の
    /// 個々の列値のような、値の種類ごとに異なる意味を持つ場所とは異なり、
    /// 単一フィールドが複数種別を許容する契約であることを型で表す）。
    Scalar,
    Array(ElementType),
    /// 単一の文字列、または要素型付き配列のいずれか（Issue #1198・NOSQL-16 (b)。
    /// `aggregate.group_by` の単一文字列形〔`"lang"`〕と配列形〔`["lang"]`〕の
    /// 後方互換受理向け。文字列形は要素型が [`ElementType::String`] のときのみ
    /// 受理する）。読み出しは [`Validated::optional_string_or_array`]。
    StringOrArray(ElementType),
    /// 値が [`ObjectSchema`] に従うオブジェクトであることを再帰検証する。
    Object(&'static ObjectSchema),
    /// 値が JSON オブジェクトであることのみを検証する（固定フィールド集合を
    /// 持たない。`update` op の `set`〔列名をキーに持つ部分更新オブジェクト〕
    /// のように、キー集合がクライアント指定の列名に依存し静的に宣言できない
    /// フィールド向け。[`ElementType::Any`]〔`insert` の `rows[*]`〕と同じ
    /// 思想。禁止列・値の型検査は本モジュールの対象外〔Issue #876〕）。
    AnyObject,
}

/// スキーマ 1 フィールドの宣言。
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    pub key: &'static str,
    pub presence: Presence,
    pub ty: FieldType,
    /// `true` のとき `null` を受理する（既定 `false` = `null` は
    /// [`SchemaError::TypeMismatch`]。fail-closed をデフォルトとするための
    /// 明示的な opt-in）。
    pub nullable: bool,
}

/// 1 つの JSON オブジェクトが満たすべきスキーマ（op トップレベル、または
/// ネストしたサブオブジェクト双方に使う）。
#[derive(Debug, Clone, Copy)]
pub struct ObjectSchema {
    /// 診断・エラー文言（ルート非オブジェクト時の [`SchemaError::TypeMismatch`]
    /// の `key`）に使う名前。
    pub name: &'static str,
    pub fields: &'static [FieldSpec],
}

/// フィールド 1 つの値を型検査する（[`ObjectSchema::validate`] から呼ばれる
/// 内部ヘルパー）。`null` は `nullable` を見て個別に処理するため、ここへは
/// 非 `Null` の値のみが渡ってくる前提。
fn check_field_type(
    value: &JsonValue,
    ty: &FieldType,
    key: &'static str,
) -> Result<(), SchemaError> {
    match (ty, value) {
        (FieldType::Bool, JsonValue::Bool(_)) => Ok(()),
        (FieldType::Number, JsonValue::Number(_)) => Ok(()),
        (FieldType::String, JsonValue::String(_)) => Ok(()),
        (FieldType::Scalar, JsonValue::String(_) | JsonValue::Number(_) | JsonValue::Bool(_)) => {
            Ok(())
        }
        (FieldType::Array(elem_ty), JsonValue::Array(items)) => {
            for item in items {
                check_element_type(item, elem_ty, key)?;
            }
            Ok(())
        }
        (FieldType::StringOrArray(ElementType::String), JsonValue::String(_)) => Ok(()),
        (FieldType::StringOrArray(elem_ty), JsonValue::Array(items)) => {
            for item in items {
                check_element_type(item, elem_ty, key)?;
            }
            Ok(())
        }
        (FieldType::Object(schema), JsonValue::Object(_)) => {
            schema.validate(value)?;
            Ok(())
        }
        (FieldType::AnyObject, JsonValue::Object(_)) => Ok(()),
        _ => Err(SchemaError::TypeMismatch { key }),
    }
}

/// 配列要素 1 つを型検査する（[`check_field_type`] の `Array` 分岐から呼ばれる）。
/// 要素の型不一致も、包含するフィールドの `key`（配列全体のキー名）で
/// 報告する（要素の添字までは公開しない。固定文言の方針に沿う）。
fn check_element_type(
    value: &JsonValue,
    elem_ty: &ElementType,
    key: &'static str,
) -> Result<(), SchemaError> {
    match (elem_ty, value) {
        (ElementType::Any, _) => Ok(()),
        (ElementType::String, JsonValue::String(_)) => Ok(()),
        (ElementType::Number, JsonValue::Number(_)) => Ok(()),
        (ElementType::Object(schema), JsonValue::Object(_)) => {
            schema.validate(value)?;
            Ok(())
        }
        _ => Err(SchemaError::TypeMismatch { key }),
    }
}

impl ObjectSchema {
    /// `value` がこのスキーマに従うことを検証する。判定順序は
    /// 「必須欠落 → 型不一致 → 未知キー」に固定し、同一入力には常に同一の
    /// 単一エラーを返す（決定的）。
    ///
    /// - ルートが `Object` でなければ `TypeMismatch { key: self.name }`。
    /// - 各 [`FieldSpec`] について、キー欠落かつ `Required` なら
    ///   `MissingRequired`。値が `Null` なら `nullable` を見て通過／
    ///   `TypeMismatch`。それ以外は [`check_field_type`] で型照合する
    ///   （`Object`／`Array(Object(_))` は再帰的に本メソッドへ委譲する。
    ///   ネスト先でも未知キー拒否が成立する）。
    /// - 最後に、スキーマに存在しないキーが 1 つでもあれば `UnknownKey`
    ///   （`fields` は高々十数個の静的スライスのため線形探索で十分。
    ///   `BTreeSet` 等の追加アロケーションは行わない）。
    pub fn validate<'a>(&'static self, value: &'a JsonValue) -> Result<Validated<'a>, SchemaError> {
        let JsonValue::Object(map) = value else {
            return Err(SchemaError::TypeMismatch { key: self.name });
        };

        for field in self.fields {
            match map.get(field.key) {
                None => {
                    if field.presence == Presence::Required {
                        return Err(SchemaError::MissingRequired { key: field.key });
                    }
                }
                Some(JsonValue::Null) => {
                    if !field.nullable {
                        return Err(SchemaError::TypeMismatch { key: field.key });
                    }
                }
                Some(v) => {
                    check_field_type(v, &field.ty, field.key)?;
                }
            }
        }

        for key in map.keys() {
            if !self.fields.iter().any(|f| f.key == key.as_str()) {
                return Err(SchemaError::UnknownKey);
            }
        }

        Ok(Validated { map, schema: self })
    }
}

/// [`FieldType::StringOrArray`] フィールドの値（Issue #1198）。
#[derive(Debug, Clone, Copy)]
pub enum StringOrArray<'a> {
    /// 単一文字列形。
    Single(&'a str),
    /// 配列形（要素の型は [`ObjectSchema::validate`] が検査済み）。
    Many(&'a [JsonValue]),
}

/// [`ObjectSchema::validate`] を通過したオブジェクトへの型付きアクセサ。
/// 借用のみでアロケーションを行わない。各アクセサは要求されたキー・型を
/// スキーマに照らして再検査し、スキーマ外のキーや型不一致の要求は `Err`
/// を返す（呼び出し元の誤用に対する多層防御。`validate` の検査結果を信頼
/// しきらない）。
#[derive(Debug, Clone, Copy)]
pub struct Validated<'a> {
    map: &'a BTreeMap<String, JsonValue>,
    schema: &'static ObjectSchema,
}

impl<'a> Validated<'a> {
    /// この値の検証元スキーマ。
    pub fn schema(&self) -> &'static ObjectSchema {
        self.schema
    }

    fn field_spec(&self, key: &'static str) -> Option<&'static FieldSpec> {
        self.schema.fields.iter().find(|f| f.key == key)
    }

    pub fn required_str(&self, key: &'static str) -> Result<&'a str, SchemaError> {
        self.optional_str(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_str(&self, key: &'static str) -> Result<Option<&'a str>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::String) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::String(s)) => Ok(Some(s.as_str())),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    pub fn required_number(&self, key: &'static str) -> Result<f64, SchemaError> {
        self.optional_number(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_number(&self, key: &'static str) -> Result<Option<f64>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::Number) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::Number(n)) => Ok(Some(n.as_f64())),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    /// `required_number` の結果を `u32` として再検査する（Issue #763・#766・
    /// TASK-175）。SQL 表層の `LIMIT` 許可リスト検査（`sql::allowlist`）が
    /// `LIMIT 1.5`／`LIMIT -1`／`u32` 超過をいずれも構文段階の `42601` で
    /// 拒否するのと対応させ、NoSQL 表層でも同じ 3 条件（整数値・非負・
    /// `u32::MAX` 以下）を JSON 数値から `u32` へ変換する時点で検査する
    /// （範囲検証そのものは呼び出し元が
    /// `engine::sql::parser::validate_search_limit` 等へ委譲する）。
    pub fn required_u32(&self, key: &'static str) -> Result<u32, SchemaError> {
        self.optional_u32(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_u32(&self, key: &'static str) -> Result<Option<u32>, SchemaError> {
        let Some(raw) = self.optional_number(key)? else {
            return Ok(None);
        };
        if !raw.is_finite() || raw.fract() != 0.0 || raw < 0.0 || raw > f64::from(u32::MAX) {
            return Err(SchemaError::TypeMismatch { key });
        }
        // 直上の範囲検査（`0.0..=f64::from(u32::MAX)`・整数値）により
        // `raw as u32` は必ず可逆な変換になる。
        Ok(Some(raw as u32))
    }

    pub fn required_bool(&self, key: &'static str) -> Result<bool, SchemaError> {
        self.optional_bool(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_bool(&self, key: &'static str) -> Result<Option<bool>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::Bool) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::Bool(b)) => Ok(Some(*b)),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    /// `FieldType::Scalar` フィールドの生 JSON 値（文字列・数値・真偽値の
    /// いずれか）への参照を返す（Issue #896・NOSQL-17。`filter[].value`
    /// 専用。他のアクセサと異なり単一の Rust 型へ変換しない——呼び出し元
    /// が列型に応じて種別を判定する契約のため）。
    pub fn required_scalar(&self, key: &'static str) -> Result<&'a JsonValue, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::Scalar) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            Some(v @ (JsonValue::String(_) | JsonValue::Number(_) | JsonValue::Bool(_))) => Ok(v),
            None => Err(SchemaError::MissingRequired { key }),
            // 他のアクセサ（`optional_str`／`optional_number`／`optional_bool`
            // 等）と同じく、キーは存在するが値の型が噛み合わない場合は
            // `TypeMismatch` を返す（`Null`／`Array`／`Object` はここに落ちる。
            // `ObjectSchema::validate` が既に拒否済みのため到達しないはずだが、
            // 多層防御として他アクセサと分類を揃える）。
            Some(_) => Err(SchemaError::TypeMismatch { key }),
        }
    }

    pub fn required_array(&self, key: &'static str) -> Result<&'a [JsonValue], SchemaError> {
        self.optional_array(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_array(
        &self,
        key: &'static str,
    ) -> Result<Option<&'a [JsonValue]>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::Array(_)) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::Array(items)) => Ok(Some(items.as_slice())),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    /// `FieldType::StringOrArray` フィールドを読む（Issue #1198・NOSQL-16 (b)）。
    /// 他アクセサと同じく spec 型を再検査する（多層防御）。
    pub fn optional_string_or_array(
        &self,
        key: &'static str,
    ) -> Result<Option<StringOrArray<'a>>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        if !matches!(spec.ty, FieldType::StringOrArray(_)) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::String(s)) => Ok(Some(StringOrArray::Single(s.as_str()))),
            Some(JsonValue::Array(items)) => Ok(Some(StringOrArray::Many(items.as_slice()))),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }

    pub fn required_object(
        &self,
        key: &'static str,
    ) -> Result<&'a BTreeMap<String, JsonValue>, SchemaError> {
        self.optional_object(key)?
            .ok_or(SchemaError::MissingRequired { key })
    }

    pub fn optional_object(
        &self,
        key: &'static str,
    ) -> Result<Option<&'a BTreeMap<String, JsonValue>>, SchemaError> {
        let Some(spec) = self.field_spec(key) else {
            return Err(SchemaError::UnknownKey);
        };
        // `Object(_)`（固定フィールド集合）・`AnyObject`（`update` の `set`
        // のように任意列名を持つオブジェクト。Issue #875）のいずれも
        // このアクセサで読める（値としては両方 `JsonValue::Object` のため）。
        if !matches!(spec.ty, FieldType::Object(_) | FieldType::AnyObject) {
            return Err(SchemaError::TypeMismatch { key });
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(JsonValue::Null) if spec.nullable => Ok(None),
            Some(JsonValue::Object(m)) => Ok(Some(m)),
            _ => Err(SchemaError::TypeMismatch { key }),
        }
    }
}

/// `hybrid` フィールドのサブスキーマ（`search` op）。
pub static HYBRID_SCHEMA: ObjectSchema = ObjectSchema {
    name: "hybrid",
    fields: &[FieldSpec {
        key: "text",
        presence: Presence::Required,
        ty: FieldType::String,
        nullable: false,
    }],
};

/// `aggregates` 配列要素のサブスキーマ（`aggregate` op）。
pub static AGGREGATE_ITEM_SCHEMA: ObjectSchema = ObjectSchema {
    name: "aggregate_item",
    fields: &[
        FieldSpec {
            key: "fn",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "column",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
    ],
};

/// `having` 配列要素のサブスキーマ（`aggregate` op）。
pub static HAVING_ITEM_SCHEMA: ObjectSchema = ObjectSchema {
    name: "having_item",
    fields: &[
        FieldSpec {
            key: "fn",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "column",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "value",
            presence: Presence::Required,
            ty: FieldType::Number,
            nullable: false,
        },
    ],
};

/// `sort` 配列要素のサブスキーマ（`scan`・`aggregate` op。Issue #946・NOSQL-15・SQL-25
/// (a)・TASK-224）。`dir` は語彙・大文字小文字を型検査段では見ず
/// （`"asc"`／`"desc"` 以外・大文字混じりの拒否は [`super::scan::build_sort`]
/// が `42601` へ写像する）、値が文字列であることのみ検査する。
pub static SORT_ITEM_SCHEMA: ObjectSchema = ObjectSchema {
    name: "sort_item",
    fields: &[
        FieldSpec {
            key: "column",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "dir",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
    ],
};

/// `search` op のトップレベルスキーマ（NOSQL-2・NOSQL-3・NOSQL-9・NOSQL-10
/// ポインタ）。`vector`／`plan` の排他は #763 が担う（本ヘルパーは型のみ
/// 検査するため両方同時に指定した入力も通過し得る）。`sort`（スカラー列
/// `ORDER BY` 相当。NOSQL-15・SQL-25 (a)・Issue #946）は本スキーマに宣言
/// しないため未知キー `42601` で拒否される——ベクトル順位付け（距離・
/// `hybrid`）との相互排他をスキーマの一般則だけで成立させる判断。
pub static SEARCH_SCHEMA: ObjectSchema = ObjectSchema {
    name: "search",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "limit",
            presence: Presence::Required,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "vector",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Number),
            nullable: false,
        },
        FieldSpec {
            key: "plan",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "columns",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "filter",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Any),
            nullable: false,
        },
        FieldSpec {
            key: "hybrid",
            presence: Presence::Optional,
            ty: FieldType::Object(&HYBRID_SCHEMA),
            nullable: false,
        },
        FieldSpec {
            key: "mode",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "explain",
            presence: Presence::Optional,
            ty: FieldType::Bool,
            nullable: false,
        },
    ],
};

/// `scan` op のトップレベルスキーマ（NOSQL-3・NOSQL-15 ポインタ）。`vector`／
/// `plan`／`mode`／`hybrid` は未知キーとして `42601` で拒否される（スキーマに
/// 宣言しないことで一般則から自然に成立する。個別の除外ロジックは持たない）。
/// `offset`（Issue #947・NOSQL-15・TASK-224）は SQL 表層の広域取得
/// `LIMIT n OFFSET m` と同じ実行計画（[`engine::sql::parser::BoundScan::
/// with_offset`]）へ写像する（[`super::scan::execute`] 参照）。`sort`
/// （Issue #946・NOSQL-15・SQL-25 (a)・TASK-224）は末尾に追加した独立
/// フィールドで、要素の語彙検査・列名解決・上限判定は
/// [`super::scan::build_sort`] が担う（本モジュールは形のみ検査する）。
/// `aggregate` op の `offset`／`sort` は [`AGGREGATE_SCHEMA`] が別途宣言する
/// （Issue #1198。`group_by` 必須。`super::aggregate` 参照）。
pub static SCAN_SCHEMA: ObjectSchema = ObjectSchema {
    name: "scan",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "limit",
            presence: Presence::Required,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "offset",
            presence: Presence::Optional,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "filter",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Any),
            nullable: false,
        },
        FieldSpec {
            key: "columns",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "explain",
            presence: Presence::Optional,
            ty: FieldType::Bool,
            nullable: false,
        },
        FieldSpec {
            key: "sort",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Object(&SORT_ITEM_SCHEMA)),
            nullable: false,
        },
    ],
};

/// `aggregate` op のトップレベルスキーマ（NOSQL-4〜NOSQL-7 ポインタ）。
/// `group_by`／`having` の意味検証（語彙・列名解決・上限判定）は
/// [`super::aggregate`]（#769）が担う。`group_by` は単一文字列形と配列形の
/// 両方を受理し（Issue #1198・NOSQL-16 (b)）、`sort`／`offset`（Issue #1198・
/// NOSQL-15。`group_by` 必須）は形のみをここで検査する。
pub static AGGREGATE_SCHEMA: ObjectSchema = ObjectSchema {
    name: "aggregate",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "aggregates",
            presence: Presence::Required,
            ty: FieldType::Array(ElementType::Object(&AGGREGATE_ITEM_SCHEMA)),
            nullable: false,
        },
        FieldSpec {
            key: "filter",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Any),
            nullable: false,
        },
        FieldSpec {
            key: "group_by",
            presence: Presence::Optional,
            ty: FieldType::StringOrArray(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "having",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Object(&HAVING_ITEM_SCHEMA)),
            nullable: false,
        },
        FieldSpec {
            key: "explain",
            presence: Presence::Optional,
            ty: FieldType::Bool,
            nullable: false,
        },
        FieldSpec {
            key: "offset",
            presence: Presence::Optional,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "sort",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Object(&SORT_ITEM_SCHEMA)),
            nullable: false,
        },
    ],
};

/// `insert` op のトップレベルスキーマ（NOSQL-6 ポインタ）。`operation_id` の
/// 欠落／`null`／空文字 → `23502`、`rows[*]` の列検証は [`super::insert`]
/// （Issue #771）が担う（本ヘルパーでは `operation_id` を Required にせず、`rows`
/// は任意列名を持つため `Array(Any)` として型のみ検査する）。
pub static INSERT_SCHEMA: ObjectSchema = ObjectSchema {
    name: "insert",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "rows",
            presence: Presence::Required,
            ty: FieldType::Array(ElementType::Any),
            nullable: false,
        },
        FieldSpec {
            key: "operation_id",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: true,
        },
    ],
};

/// `where` フィールドのサブスキーマ（`update`／`delete` op。単一行・`id`
/// 指定形。Issue #875・NOSQL-12）。`where`／`filter` の排他・双方欠落の
/// 意味検証はここでは行わず（両方 `Optional`）、後続 Issue（#876）の
/// バインダが担う（`search` op の `vector`／`plan` 排他判定を #763 の
/// バインダへ置いた前例と同じ設計）。
pub static WHERE_ID_SCHEMA: ObjectSchema = ObjectSchema {
    name: "where",
    fields: &[FieldSpec {
        key: "id",
        presence: Presence::Required,
        ty: FieldType::Number,
        nullable: false,
    }],
};

/// `update` op のトップレベルスキーマ（NOSQL-12 ポインタ。Issue #875）。
/// `set` の禁止列（`id`／`tenant_id`／`visibility`）検査・列名解決・
/// `where`／`filter` の排他判定・`operation_id` 欠落／`null`／空文字 →
/// `23502`・engine への束縛・実行は [`super::update`]・[`super::dml_target`]
/// （Issue #876）が担う（本スキーマは型のみを検査する）。
pub static UPDATE_SCHEMA: ObjectSchema = ObjectSchema {
    name: "update",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "set",
            presence: Presence::Required,
            ty: FieldType::AnyObject,
            nullable: false,
        },
        FieldSpec {
            key: "where",
            presence: Presence::Optional,
            ty: FieldType::Object(&WHERE_ID_SCHEMA),
            nullable: false,
        },
        FieldSpec {
            key: "filter",
            presence: Presence::Optional,
            // 述語形 DML の要素の形（葉・`not` グループ・値なしの `is_null` 等）は
            // `http::query::filter` へ集約している（Issue #1197。二重実装しない）。
            ty: FieldType::Array(ElementType::Any),
            nullable: false,
        },
        FieldSpec {
            key: "operation_id",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: true,
        },
    ],
};

/// `delete` op のトップレベルスキーマ（NOSQL-12 ポインタ。Issue #875）。
/// `where`／`filter` の排他判定・`operation_id` 欠落／`null`／空文字 →
/// `23502`・engine への束縛・実行は [`super::delete`]・[`super::dml_target`]
/// （Issue #876）が担う。
pub static DELETE_SCHEMA: ObjectSchema = ObjectSchema {
    name: "delete",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "where",
            presence: Presence::Optional,
            ty: FieldType::Object(&WHERE_ID_SCHEMA),
            nullable: false,
        },
        FieldSpec {
            key: "filter",
            presence: Presence::Optional,
            // 述語形 DML の要素の形（葉・`not` グループ・値なしの `is_null` 等）は
            // `http::query::filter` へ集約している（Issue #1197。二重実装しない）。
            ty: FieldType::Array(ElementType::Any),
            nullable: false,
        },
        FieldSpec {
            key: "operation_id",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: true,
        },
    ],
};

/// `create_table.columns[*]` のサブスキーマ（NOSQL-13・TASK-207、Issue #910）。
/// 受理する `type` の語彙は SQL 表層の `CREATE TABLE` と同じ 3 種
/// （`text`／`vector`／`integer`／`bigint`）に限る（意味検証は
/// [`super::ddl`] が engine のトークン入口
/// （`engine::sql::allowlist::validate_create_table_tokens`）へ渡す前に行う。
/// 本スキーマは型のみを検査する）。
pub static DDL_COLUMN_SCHEMA: ObjectSchema = ObjectSchema {
    name: "ddl_column",
    fields: &[
        FieldSpec {
            key: "name",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "type",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "dim",
            presence: Presence::Optional,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "nullable",
            presence: Presence::Optional,
            ty: FieldType::Bool,
            nullable: false,
        },
        FieldSpec {
            key: "default",
            presence: Presence::Optional,
            // 文字列・数値・真偽値のいずれも型としては受理し、`bool`／`null`
            // 相当（SQL の CREATE TABLE で表現できない DEFAULT）の拒否は
            // [`super::ddl`] の意味検証が担う（`filter` の `value` と
            // 同じく、形の検証段階では型を絞らない設計）。
            ty: FieldType::Scalar,
            nullable: false,
        },
    ],
};

/// `create_table.constraints[*].references` のサブスキーマ（`foreign_key`
/// 制約専用。NOSQL-13・TASK-207、Issue #910）。
pub static DDL_REFERENCES_SCHEMA: ObjectSchema = ObjectSchema {
    name: "ddl_references",
    fields: &[
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "columns",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        // 参照アクション（Issue #1148・NOSQL-13）。語彙の意味検証
        // （固定語彙外の拒否）は本スキーマではなく [`super::ddl`] が担う
        // （`super::ddl::referential_action_tokens` 参照）。省略時は
        // engine 既定の `NO ACTION` になる。
        FieldSpec {
            key: "on_delete",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "on_update",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: false,
        },
    ],
};

/// `create_table.constraints[*]` のサブスキーマ（NOSQL-13・TASK-207、
/// Issue #910）。`kind` の語彙（`primary_key`／`unique`／`foreign_key`／
/// `check`）・`columns` の必須性は [`super::ddl`] の意味検証が判定する
/// （`check` は `columns` を要求しないため、本スキーマでは `columns` を
/// `Optional` として宣言し、`kind` ごとの必須性は呼び出し元が
/// `Validated::required_array` で判定する。`WHERE_ID_SCHEMA` と同じ
/// 「スキーマは型のみ、意味は呼び出し元」設計）。
pub static DDL_CONSTRAINT_SCHEMA: ObjectSchema = ObjectSchema {
    name: "ddl_constraint",
    fields: &[
        FieldSpec {
            key: "kind",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "columns",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::String),
            nullable: false,
        },
        FieldSpec {
            key: "references",
            presence: Presence::Optional,
            ty: FieldType::Object(&DDL_REFERENCES_SCHEMA),
            nullable: false,
        },
    ],
};

/// `create_table` op のトップレベルスキーマ（NOSQL-13・TASK-207、Issue #910）。
/// `engine::sql::allowlist::validate_create_table_tokens` と同一の実行器へ
/// トークン列として渡す入口は [`super::ddl`] が担う。
pub static CREATE_TABLE_SCHEMA: ObjectSchema = ObjectSchema {
    name: "create_table",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "columns",
            presence: Presence::Required,
            ty: FieldType::Array(ElementType::Object(&DDL_COLUMN_SCHEMA)),
            nullable: false,
        },
        FieldSpec {
            key: "constraints",
            presence: Presence::Optional,
            ty: FieldType::Array(ElementType::Object(&DDL_CONSTRAINT_SCHEMA)),
            nullable: false,
        },
    ],
};

/// `alter_table.add_column` のサブスキーマ（NOSQL-13・TASK-207、Issue #910）。
/// 受理する `type` の語彙は SQL 表層 `ALTER TABLE ADD COLUMN`
/// （`engine::sql::ddl_column_type::SqlColumnTypeName`）と同じ集合。
pub static DDL_ADD_COLUMN_SCHEMA: ObjectSchema = ObjectSchema {
    name: "ddl_add_column",
    fields: &[
        FieldSpec {
            key: "name",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "type",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "dim",
            presence: Presence::Optional,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "precision",
            presence: Presence::Optional,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "scale",
            presence: Presence::Optional,
            ty: FieldType::Number,
            nullable: false,
        },
        FieldSpec {
            key: "enum_type",
            presence: Presence::Optional,
            ty: FieldType::String,
            nullable: false,
        },
    ],
};

/// `alter_table.drop_column` のサブスキーマ（NOSQL-13・TASK-207、Issue #910）。
/// SQL 表層の `ALTER TABLE ... DROP COLUMN` と同じ入口へ結線される
/// （Issue #1167。[`super::ddl`] モジュール doc 参照）。
pub static DDL_DROP_COLUMN_SCHEMA: ObjectSchema = ObjectSchema {
    name: "ddl_drop_column",
    fields: &[FieldSpec {
        key: "name",
        presence: Presence::Required,
        ty: FieldType::String,
        nullable: false,
    }],
};

/// `alter_table` op のトップレベルスキーマ（NOSQL-13・TASK-207、Issue #910）。
/// `add_column`／`drop_column` は排他必須（[`super::ddl`] が意味検証する）。
pub static ALTER_TABLE_SCHEMA: ObjectSchema = ObjectSchema {
    name: "alter_table",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "add_column",
            presence: Presence::Optional,
            ty: FieldType::Object(&DDL_ADD_COLUMN_SCHEMA),
            nullable: false,
        },
        FieldSpec {
            key: "drop_column",
            presence: Presence::Optional,
            ty: FieldType::Object(&DDL_DROP_COLUMN_SCHEMA),
            nullable: false,
        },
    ],
};

/// `drop_table` op のトップレベルスキーマ（NOSQL-13・TASK-207、Issue #910）。
pub static DROP_TABLE_SCHEMA: ObjectSchema = ObjectSchema {
    name: "drop_table",
    fields: &[
        FieldSpec {
            key: "op",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "table",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
    ],
};

/// op 名（厳密一致。trim・大文字小文字の読み替えはしない）→ [`ObjectSchema`]
/// の対応表。`schema_for` の実体であり、単一情報源として扱う。
pub const OP_SCHEMAS: [(&str, &ObjectSchema); 9] = [
    ("search", &SEARCH_SCHEMA),
    ("scan", &SCAN_SCHEMA),
    ("aggregate", &AGGREGATE_SCHEMA),
    ("insert", &INSERT_SCHEMA),
    ("update", &UPDATE_SCHEMA),
    ("delete", &DELETE_SCHEMA),
    ("create_table", &CREATE_TABLE_SCHEMA),
    ("alter_table", &ALTER_TABLE_SCHEMA),
    ("drop_table", &DROP_TABLE_SCHEMA),
];

/// `op` 名からスキーマを引く。語彙外は `None`（`0A000` への写像・応答は
/// [`super::op::classify_op`] の呼び出し元が行う。本モジュールは表引きの
/// みで、実体は [`super::op::Op::parse`] への委譲）。
pub fn schema_for(op: &str) -> Option<&'static ObjectSchema> {
    super::op::Op::parse(op).map(super::op::Op::schema)
}

/// スキーマ本体（`op` 以外のキー）を検証する前段として、トップレベルから
/// `op` 文字列だけを取り出す入口。ルートが `Object` でない場合・`op` が
/// 欠落／非文字列の場合を区別して報告する。
pub fn extract_op(value: &JsonValue) -> Result<&str, SchemaError> {
    let JsonValue::Object(map) = value else {
        return Err(SchemaError::TypeMismatch { key: "$" });
    };
    match map.get("op") {
        None => Err(SchemaError::MissingRequired { key: "op" }),
        Some(JsonValue::String(s)) => Ok(s.as_str()),
        Some(_) => Err(SchemaError::TypeMismatch { key: "op" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::error_format::ErrorClass;
    use engine::json::parse_json;

    fn obj(json: &str) -> JsonValue {
        parse_json(json).expect("test fixture must be valid JSON")
    }

    // --- 表網羅 ---------------------------------------------------------

    #[test]
    fn op_schemas_cover_expected_ops_and_schema_for_matches() {
        let ops: Vec<&str> = OP_SCHEMAS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            ops,
            vec![
                "search",
                "scan",
                "aggregate",
                "insert",
                "update",
                "delete",
                "create_table",
                "alter_table",
                "drop_table",
            ]
        );
        for (name, schema) in OP_SCHEMAS.iter() {
            assert!(std::ptr::eq(schema_for(name).unwrap(), *schema));
        }
    }

    #[test]
    fn op_schemas_all_require_op_and_table() {
        for (name, schema) in OP_SCHEMAS.iter() {
            let op_spec = schema.fields.iter().find(|f| f.key == "op");
            let table_spec = schema.fields.iter().find(|f| f.key == "table");
            assert!(
                matches!(
                    op_spec,
                    Some(FieldSpec {
                        presence: Presence::Required,
                        ty: FieldType::String,
                        ..
                    })
                ),
                "op {name} must declare required string `op`"
            );
            assert!(
                matches!(
                    table_spec,
                    Some(FieldSpec {
                        presence: Presence::Required,
                        ty: FieldType::String,
                        ..
                    })
                ),
                "op {name} must declare required string `table`"
            );
        }
    }

    #[test]
    fn op_schemas_have_no_duplicate_field_keys() {
        for (name, schema) in OP_SCHEMAS.iter() {
            let mut seen: Vec<&str> = Vec::new();
            for field in schema.fields {
                assert!(
                    !seen.contains(&field.key),
                    "op {name} declares duplicate key {}",
                    field.key
                );
                seen.push(field.key);
            }
        }
    }

    #[test]
    fn schema_for_returns_none_for_unknown_vocabulary() {
        assert!(schema_for("select").is_none());
        assert!(schema_for("SEARCH").is_none());
        assert!(schema_for("").is_none());
        assert!(schema_for("searchx").is_none());
    }

    // --- 受理ケース -------------------------------------------------------

    #[test]
    fn search_accepts_full_valid_object() {
        let v = obj(
            r#"{"op":"search","table":"docs","limit":10,"vector":[0.1,0.2],
               "plan":"find x","columns":["id","body"],
               "filter":[{"column":"lang","op":"eq","value":"ja"}],
               "hybrid":{"text":"x"},"mode":"precision","explain":true}"#,
        );
        assert!(SEARCH_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn scan_accepts_full_valid_object() {
        let v = obj(r#"{"op":"scan","table":"docs","limit":500,"offset":20,
               "filter":[{"column":"lang","op":"eq","value":"ja"}],
               "columns":["id"],"explain":false}"#);
        assert!(SCAN_SCHEMA.validate(&v).is_ok());
    }

    /// `offset`（Issue #947・NOSQL-15）は任意キーで、省略しても受理される。
    #[test]
    fn scan_accepts_missing_offset() {
        let v = obj(r#"{"op":"scan","table":"docs","limit":10}"#);
        assert!(SCAN_SCHEMA.validate(&v).is_ok());
    }

    /// `offset` の型不一致（文字列・真偽値・`null`）は `TypeMismatch` になる
    /// （値の範囲・形状検査は [`super::scan::offset_to_u32`] が担う）。
    #[test]
    fn scan_offset_type_mismatches_are_rejected() {
        for offset_json in [r#""offset":"5""#, r#""offset":true"#, r#""offset":null"#] {
            let json = format!(r#"{{"op":"scan","table":"docs","limit":10,{offset_json}}}"#);
            let v = obj(&json);
            assert_eq!(
                SCAN_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::TypeMismatch { key: "offset" },
                "offset_json={offset_json}"
            );
        }
    }

    /// `offset` は `scan` op 専用のキーであり、`search`／`aggregate` へ
    /// 付与すると未知キーとして `42601` へ落ちる（NOSQL-15 のスコープ注記。
    /// `super::scan` モジュール doc の「対象は scan のみ」判断を固定する）。
    #[test]
    fn offset_is_unknown_key_for_search() {
        let search_v =
            obj(r#"{"op":"search","table":"docs","limit":10,"vector":[0.1],"offset":5}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&search_v).unwrap_err(),
            SchemaError::UnknownKey
        );
    }

    /// `aggregate` は `offset`／`sort` と文字列形 `group_by` を型検査段で通す
    /// （Issue #1198・NOSQL-15・NOSQL-16 (b)。意味検証は `super::aggregate`）。
    #[test]
    fn aggregate_accepts_offset_sort_and_string_group_by() {
        let v = obj(r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"*"}],
               "group_by":"lang","offset":5,
               "sort":[{"column":"count","dir":"desc"}]}"#);
        assert!(AGGREGATE_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn aggregate_group_by_rejects_non_string_shapes() {
        for gb in ["1", "{}", "null", r#"["a",1]"#, r#"[["a"]]"#, "true"] {
            let v = obj(&format!(
                r#"{{"op":"aggregate","table":"docs",
                   "aggregates":[{{"fn":"count","column":"*"}}],"group_by":{gb}}}"#
            ));
            assert!(
                matches!(
                    AGGREGATE_SCHEMA.validate(&v).unwrap_err(),
                    SchemaError::TypeMismatch { key: "group_by" }
                ),
                "group_by={gb}"
            );
        }
    }

    #[test]
    fn aggregate_accepts_full_valid_object() {
        let v = obj(r#"{"op":"aggregate","table":"docs",
               "aggregates":[{"fn":"count","column":"id"}],
               "filter":[{"column":"lang","op":"eq","value":"ja"}],
               "group_by":["lang"],
               "having":[{"fn":"count","column":"id","op":"gt","value":1}],
               "explain":true}"#);
        assert!(AGGREGATE_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn insert_accepts_full_valid_object() {
        let v = obj(
            r#"{"op":"insert","table":"docs","rows":[{"id":"1","body":"x"}],
               "operation_id":"op-1"}"#,
        );
        assert!(INSERT_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn insert_accepts_missing_and_null_operation_id() {
        let missing = obj(r#"{"op":"insert","table":"docs","rows":[]}"#);
        assert!(INSERT_SCHEMA.validate(&missing).is_ok());
        let null_id = obj(r#"{"op":"insert","table":"docs","rows":[],"operation_id":null}"#);
        assert!(INSERT_SCHEMA.validate(&null_id).is_ok());
    }

    #[test]
    fn update_accepts_full_valid_object() {
        // `where` 形。
        let v = obj(
            r#"{"op":"update","table":"docs","set":{"lang":"en","body":"x"},
               "where":{"id":1},"operation_id":"op-1"}"#,
        );
        assert!(UPDATE_SCHEMA.validate(&v).is_ok());
        // `filter` 形。
        let v2 = obj(r#"{"op":"update","table":"docs","set":{"lang":"en"},
               "filter":[{"column":"lang","op":"eq","value":"ja"}]}"#);
        assert!(UPDATE_SCHEMA.validate(&v2).is_ok());
    }

    #[test]
    fn delete_accepts_full_valid_object() {
        let v = obj(r#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":"op-1"}"#);
        assert!(DELETE_SCHEMA.validate(&v).is_ok());
        let v2 = obj(r#"{"op":"delete","table":"docs",
               "filter":[{"column":"lang","op":"eq","value":"ja"}]}"#);
        assert!(DELETE_SCHEMA.validate(&v2).is_ok());
    }

    #[test]
    fn update_and_delete_accept_missing_and_null_operation_id() {
        let update_missing = obj(r#"{"op":"update","table":"docs","set":{"lang":"en"}}"#);
        assert!(UPDATE_SCHEMA.validate(&update_missing).is_ok());
        let update_null =
            obj(r#"{"op":"update","table":"docs","set":{"lang":"en"},"operation_id":null}"#);
        assert!(UPDATE_SCHEMA.validate(&update_null).is_ok());

        let delete_missing = obj(r#"{"op":"delete","table":"docs","where":{"id":1}}"#);
        assert!(DELETE_SCHEMA.validate(&delete_missing).is_ok());
        let delete_null =
            obj(r#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":null}"#);
        assert!(DELETE_SCHEMA.validate(&delete_null).is_ok());
    }

    #[test]
    fn update_missing_set_is_rejected() {
        let v = obj(r#"{"op":"update","table":"docs","where":{"id":1}}"#);
        assert_eq!(
            UPDATE_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::MissingRequired { key: "set" }
        );
    }

    #[test]
    fn update_set_non_object_is_rejected() {
        for set_json in [r#""set":[]"#, r#""set":"x""#, r#""set":null"#, r#""set":1"#] {
            let json = format!(r#"{{"op":"update","table":"docs",{set_json}}}"#);
            let v = obj(&json);
            assert_eq!(
                UPDATE_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::TypeMismatch { key: "set" },
                "set_json={set_json}"
            );
        }
    }

    #[test]
    fn where_id_type_mismatch_is_rejected() {
        let v = obj(r#"{"op":"delete","table":"docs","where":{"id":"1"}}"#);
        assert_eq!(
            DELETE_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "id" }
        );
        let v2 = obj(r#"{"op":"update","table":"docs","set":{"lang":"en"},"where":"x"}"#);
        assert_eq!(
            UPDATE_SCHEMA.validate(&v2).unwrap_err(),
            SchemaError::TypeMismatch { key: "where" }
        );
    }

    #[test]
    fn nested_where_rejects_unknown_key() {
        let v = obj(r#"{"op":"delete","table":"docs","where":{"id":1,"extra":1}}"#);
        assert_eq!(
            DELETE_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::UnknownKey
        );
    }

    #[test]
    fn any_object_accessor_returns_map() {
        // `required_object` は `AnyObject`（`set`）も `Object(_)`（`where`）
        // と同様に読める（Issue #876 の `bind_set_assignments` 相当が
        // `required_object("set")` で列名マップを取得する前提）。
        let v = obj(r#"{"op":"update","table":"docs","set":{"lang":"en"}}"#);
        let validated = UPDATE_SCHEMA.validate(&v).unwrap();
        let set = validated.required_object("set").unwrap();
        assert_eq!(set.get("lang"), Some(&JsonValue::String("en".to_string())));
    }

    // --- 必須欠落 ---------------------------------------------------------

    #[test]
    fn search_missing_required_fields_are_rejected() {
        for missing in ["op", "table", "limit"] {
            let full = obj(r#"{"op":"search","table":"docs","limit":10}"#);
            let JsonValue::Object(mut map) = full else {
                unreachable!()
            };
            map.remove(missing);
            let v = JsonValue::Object(map);
            let err = SEARCH_SCHEMA.validate(&v).unwrap_err();
            assert_eq!(err, SchemaError::MissingRequired { key: missing });
            assert_eq!(err.wire_code(), "42601");
            assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
        }
    }

    #[test]
    fn scan_missing_required_fields_are_rejected() {
        for missing in ["op", "table", "limit"] {
            let full = obj(r#"{"op":"scan","table":"docs","limit":10}"#);
            let JsonValue::Object(mut map) = full else {
                unreachable!()
            };
            map.remove(missing);
            let v = JsonValue::Object(map);
            assert_eq!(
                SCAN_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::MissingRequired { key: missing }
            );
        }
    }

    #[test]
    fn aggregate_missing_required_fields_are_rejected() {
        for missing in ["op", "table", "aggregates"] {
            let full = obj(
                r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
            );
            let JsonValue::Object(mut map) = full else {
                unreachable!()
            };
            map.remove(missing);
            let v = JsonValue::Object(map);
            assert_eq!(
                AGGREGATE_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::MissingRequired { key: missing }
            );
        }
    }

    #[test]
    fn insert_missing_required_fields_are_rejected() {
        for missing in ["op", "table", "rows"] {
            let full = obj(r#"{"op":"insert","table":"docs","rows":[]}"#);
            let JsonValue::Object(mut map) = full else {
                unreachable!()
            };
            map.remove(missing);
            let v = JsonValue::Object(map);
            assert_eq!(
                INSERT_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::MissingRequired { key: missing }
            );
        }
    }

    // --- 未知キー ---------------------------------------------------------

    #[test]
    fn unknown_key_is_rejected_per_op() {
        let cases: [(&'static ObjectSchema, &str); 9] = [
            (
                &SEARCH_SCHEMA,
                r#"{"op":"search","table":"docs","limit":1,"bogus":1}"#,
            ),
            (
                &SCAN_SCHEMA,
                r#"{"op":"scan","table":"docs","limit":1,"bogus":1}"#,
            ),
            (
                &AGGREGATE_SCHEMA,
                r#"{"op":"aggregate","table":"docs","aggregates":[],"bogus":1}"#,
            ),
            (
                &INSERT_SCHEMA,
                r#"{"op":"insert","table":"docs","rows":[],"bogus":1}"#,
            ),
            (
                &UPDATE_SCHEMA,
                r#"{"op":"update","table":"docs","set":{"lang":"en"},"bogus":1}"#,
            ),
            (
                &DELETE_SCHEMA,
                r#"{"op":"delete","table":"docs","where":{"id":1},"bogus":1}"#,
            ),
            (
                &CREATE_TABLE_SCHEMA,
                r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text"}],"bogus":1}"#,
            ),
            (
                &ALTER_TABLE_SCHEMA,
                r#"{"op":"alter_table","table":"docs","add_column":{"name":"a","type":"text"},"bogus":1}"#,
            ),
            (
                &DROP_TABLE_SCHEMA,
                r#"{"op":"drop_table","table":"docs","bogus":1}"#,
            ),
        ];
        for (schema, json) in cases {
            let v = obj(json);
            assert_eq!(schema.validate(&v).unwrap_err(), SchemaError::UnknownKey);
        }
    }

    #[test]
    fn tenant_id_self_declaration_is_rejected_as_unknown_key() {
        // HTTP-7 ポインタ: クライアント自己申告の tenant_id はテナント文脈
        // (PolicyContext) を上書きする経路になってはならず、未知キーとして
        // 一律 42601 で拒否する。
        for (schema, json) in [
            (
                &SEARCH_SCHEMA,
                r#"{"op":"search","table":"docs","limit":1,"tenant_id":"evil"}"#,
            ),
            (
                &INSERT_SCHEMA,
                r#"{"op":"insert","table":"docs","rows":[],"tenant_id":"evil"}"#,
            ),
            (
                &UPDATE_SCHEMA,
                r#"{"op":"update","table":"docs","set":{"lang":"en"},"tenant_id":"evil"}"#,
            ),
            (
                &DELETE_SCHEMA,
                r#"{"op":"delete","table":"docs","where":{"id":1},"tenant_id":"evil"}"#,
            ),
            (
                &CREATE_TABLE_SCHEMA,
                r#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text"}],"tenant_id":"evil"}"#,
            ),
            (
                &DROP_TABLE_SCHEMA,
                r#"{"op":"drop_table","table":"docs","tenant_id":"evil"}"#,
            ),
        ] {
            let v = obj(json);
            assert_eq!(schema.validate(&v).unwrap_err(), SchemaError::UnknownKey);
        }
    }

    #[test]
    fn hint_order_and_search_mode_style_fields_are_rejected_as_unknown_keys() {
        // NOSQL-9 ポインタ: SQL 表層の HINT ORDER／SET search_mode に相当する
        // フィールドを JSON トップレベルへ自己申告しても未知キーとして拒否
        // される（`mode` は search 専用の既存フィールドとして受理されるが、
        // それ以外の別名は拒否）。
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"hint_order":"path"}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::UnknownKey
        );
        let v2 = obj(r#"{"op":"search","table":"docs","limit":1,"search_mode":"precision"}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v2).unwrap_err(),
            SchemaError::UnknownKey
        );
        let v3 = obj(r#"{"op":"update","table":"docs","set":{"lang":"en"},"hint_order":["path"]}"#);
        assert_eq!(
            UPDATE_SCHEMA.validate(&v3).unwrap_err(),
            SchemaError::UnknownKey
        );
        let v4 =
            obj(r#"{"op":"delete","table":"docs","where":{"id":1},"search_mode":"precision"}"#);
        assert_eq!(
            DELETE_SCHEMA.validate(&v4).unwrap_err(),
            SchemaError::UnknownKey
        );
    }

    #[test]
    fn update_and_delete_reject_explain_and_search_only_fields() {
        // NOSQL-12 ポインタ: update／delete は explain／vector／limit／mode を
        // 宣言しないため、未知キーとして 42601 で拒否される（一般則から自然に
        // 成立する。個別の除外ロジックは持たない）。
        for field_json in [
            r#""explain":true"#,
            r#""vector":[0.1]"#,
            r#""limit":10"#,
            r#""mode":"precision""#,
        ] {
            let update_json =
                format!(r#"{{"op":"update","table":"docs","set":{{"lang":"en"}},{field_json}}}"#);
            assert_eq!(
                UPDATE_SCHEMA.validate(&obj(&update_json)).unwrap_err(),
                SchemaError::UnknownKey,
                "update field_json={field_json}"
            );
            let delete_json =
                format!(r#"{{"op":"delete","table":"docs","where":{{"id":1}},{field_json}}}"#);
            assert_eq!(
                DELETE_SCHEMA.validate(&obj(&delete_json)).unwrap_err(),
                SchemaError::UnknownKey,
                "delete field_json={field_json}"
            );
        }
    }

    #[test]
    fn scan_rejects_search_only_fields() {
        // NOSQL-3 ポインタ: scan は vector/plan/mode/hybrid を受理しない。
        for field_json in [
            r#""vector":[0.1]"#,
            r#""plan":"x""#,
            r#""mode":"precision""#,
            r#""hybrid":{"text":"x"}"#,
        ] {
            let json = format!(r#"{{"op":"scan","table":"docs","limit":1,{field_json}}}"#);
            let v = obj(&json);
            assert_eq!(
                SCAN_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::UnknownKey
            );
        }
    }

    #[test]
    fn unknown_key_containing_escaped_nul_is_rejected_without_leaking_key_name() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"a\u0000b":1}"#);
        let err = SEARCH_SCHEMA.validate(&v).unwrap_err();
        assert_eq!(err, SchemaError::UnknownKey);
        let msg = err.client_message();
        assert!(
            !msg.contains('\u{0}'),
            "message must not leak untrusted key bytes: {msg:?}"
        );
        assert_eq!(msg, "unknown field in request object");
    }

    // --- 型不一致 ---------------------------------------------------------

    #[test]
    fn search_type_mismatches_are_rejected() {
        let cases = [
            (r#"{"op":"search","table":"docs","limit":"10"}"#, "limit"),
            (
                r#"{"op":"search","table":"docs","limit":1,"columns":"id"}"#,
                "columns",
            ),
            (
                r#"{"op":"search","table":"docs","limit":1,"hybrid":[]}"#,
                "hybrid",
            ),
            (
                r#"{"op":"search","table":"docs","limit":1,"explain":1}"#,
                "explain",
            ),
            (
                r#"{"op":"search","table":"docs","limit":1,"vector":["a"]}"#,
                "vector",
            ),
        ];
        for (json, key) in cases {
            let v = obj(json);
            assert_eq!(
                SEARCH_SCHEMA.validate(&v).unwrap_err(),
                SchemaError::TypeMismatch { key },
                "input={json}"
            );
        }
    }

    #[test]
    fn non_nullable_field_rejects_null() {
        let v = obj(r#"{"op":"search","table":"docs","limit":null}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "limit" }
        );
        let v2 = obj(r#"{"op":null,"table":"docs","limit":1}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v2).unwrap_err(),
            SchemaError::TypeMismatch { key: "op" }
        );
    }

    // --- ネスト -------------------------------------------------------

    #[test]
    fn nested_hybrid_rejects_unknown_key() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"hybrid":{"text":"x","extra":1}}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::UnknownKey
        );
    }

    #[test]
    fn filter_element_shape_is_deferred_to_filter_module() {
        // Issue #945・NOSQL-14: `filter` 要素は「葉」（`column`／`op`／`value`）と
        // 「グループ」（`or`）の 2 形を取りうるため、`SEARCH_SCHEMA`／
        // `SCAN_SCHEMA`／`AGGREGATE_SCHEMA` はもはや要素の形を検査しない
        // （`FieldType::Array(ElementType::Any)`）。形・語彙・上限の検査は
        // `http::query::filter` モジュールへ集約した（二重実装しない）。
        // `UPDATE_SCHEMA`／`DELETE_SCHEMA` も Issue #1197 で同様に要素の形を
        // 検査しなくなった（`not` グループ・値なしの `is_null` 等を表現できない
        // ため。述語形 DML の形検査は `filter::bind_filter_where_predicates`）。
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"filter":[{"column":"a"}]}"#);
        assert!(SEARCH_SCHEMA.validate(&v).is_ok());

        let v = obj(r#"{"op":"update","table":"docs","set":{},"filter":[{"column":"a"}]}"#);
        assert!(UPDATE_SCHEMA.validate(&v).is_ok());
    }

    #[test]
    fn nested_having_item_type_mismatch() {
        let v = obj(
            r#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}],
               "having":[{"fn":"count","column":"id","op":"gt","value":"1"}]}"#,
        );
        assert_eq!(
            AGGREGATE_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "value" }
        );
    }

    // --- ルート非オブジェクト ---------------------------------------------

    #[test]
    fn non_object_root_is_rejected() {
        for json in ["[]", "\"x\"", "1", "true", "null"] {
            let v = obj(json);
            let err = SEARCH_SCHEMA.validate(&v).unwrap_err();
            assert_eq!(err, SchemaError::TypeMismatch { key: "search" });
        }
    }

    // --- extract_op --------------------------------------------------------

    #[test]
    fn extract_op_returns_op_string() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1}"#);
        assert_eq!(extract_op(&v).unwrap(), "search");
    }

    #[test]
    fn extract_op_reports_missing_op() {
        let v = obj(r#"{"table":"docs"}"#);
        assert_eq!(
            extract_op(&v).unwrap_err(),
            SchemaError::MissingRequired { key: "op" }
        );
    }

    #[test]
    fn extract_op_reports_non_string_op() {
        let v = obj(r#"{"op":1,"table":"docs"}"#);
        assert_eq!(
            extract_op(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "op" }
        );
    }

    #[test]
    fn extract_op_reports_non_object_root() {
        let v = obj("[]");
        assert_eq!(
            extract_op(&v).unwrap_err(),
            SchemaError::TypeMismatch { key: "$" }
        );
    }

    // --- ClassifiedError ----------------------------------------------------

    #[test]
    fn all_schema_error_variants_classify_as_unsupported_sql_syntax() {
        let variants = [
            SchemaError::MissingRequired { key: "op" },
            SchemaError::UnknownKey,
            SchemaError::TypeMismatch { key: "limit" },
        ];
        for err in variants {
            assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
            assert_eq!(err.wire_code(), "42601");
        }
    }

    // --- Validated アクセサ ---------------------------------------------

    #[test]
    fn validated_accessors_read_present_and_absent_fields() {
        let v = obj(r#"{"op":"search","table":"docs","limit":10,"explain":true}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(validated.required_str("op").unwrap(), "search");
        assert_eq!(validated.required_str("table").unwrap(), "docs");
        assert_eq!(validated.required_number("limit").unwrap(), 10.0);
        assert_eq!(validated.optional_bool("explain").unwrap(), Some(true));
        assert_eq!(validated.optional_str("plan").unwrap(), None);
        assert_eq!(validated.optional_array("vector").unwrap(), None);
        assert!(std::ptr::eq(validated.schema(), &SEARCH_SCHEMA));
    }

    #[test]
    fn validated_accessors_reject_schema_foreign_keys() {
        let v = obj(r#"{"op":"search","table":"docs","limit":10}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(
            validated.optional_str("not_a_field").unwrap_err(),
            SchemaError::UnknownKey
        );
    }

    #[test]
    fn validated_accessors_reject_type_mismatched_requests() {
        let v = obj(r#"{"op":"search","table":"docs","limit":10}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        // `limit` はスキーマ上 Number なので required_str での取得は拒否する。
        assert_eq!(
            validated.required_str("limit").unwrap_err(),
            SchemaError::TypeMismatch { key: "limit" }
        );
    }

    #[test]
    fn validated_object_accessor_returns_nested_map() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1,"hybrid":{"text":"x"}}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        let hybrid = validated.required_object("hybrid").unwrap();
        assert_eq!(
            hybrid.get("text"),
            Some(&JsonValue::String("x".to_string()))
        );
    }

    // --- required_u32 / optional_u32（Issue #763・#766・TASK-175） ------------

    #[test]
    fn optional_u32_accepts_zero_and_u32_max() {
        let v = obj(r#"{"op":"search","table":"docs","limit":0}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(validated.required_u32("limit").unwrap(), 0);

        let v2 = obj(&format!(
            r#"{{"op":"search","table":"docs","limit":{}}}"#,
            u32::MAX
        ));
        let validated2 = SEARCH_SCHEMA.validate(&v2).unwrap();
        assert_eq!(validated2.required_u32("limit").unwrap(), u32::MAX);
    }

    #[test]
    fn optional_u32_rejects_fractional_value() {
        let v = obj(r#"{"op":"search","table":"docs","limit":1.5}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(
            validated.required_u32("limit").unwrap_err(),
            SchemaError::TypeMismatch { key: "limit" }
        );
    }

    #[test]
    fn optional_u32_rejects_negative_value() {
        let v = obj(r#"{"op":"search","table":"docs","limit":-1}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(
            validated.required_u32("limit").unwrap_err(),
            SchemaError::TypeMismatch { key: "limit" }
        );
    }

    #[test]
    fn optional_u32_rejects_value_exceeding_u32_max() {
        let v = obj(&format!(
            r#"{{"op":"search","table":"docs","limit":{}}}"#,
            u64::from(u32::MAX) + 1
        ));
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(
            validated.required_u32("limit").unwrap_err(),
            SchemaError::TypeMismatch { key: "limit" }
        );
    }

    #[test]
    fn optional_u32_rejects_non_number_field() {
        let v = obj(r#"{"op":"search","table":"docs","limit":10}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        // `explain` はスキーマ上 Bool なので Number 用アクセサでは取得しない
        // （`optional_u32` は非該当型のキーで `TypeMismatch` を返す）。
        assert_eq!(
            validated.optional_u32("explain").unwrap_err(),
            SchemaError::TypeMismatch { key: "explain" }
        );
    }

    #[test]
    fn required_u32_reports_unknown_key_for_schema_foreign_key() {
        let v = obj(r#"{"op":"search","table":"docs","limit":10}"#);
        let validated = SEARCH_SCHEMA.validate(&v).unwrap();
        assert_eq!(
            validated.required_u32("nonexistent_field_placeholder"),
            Err(SchemaError::UnknownKey)
        );
    }

    // --- 判定順序の決定性 ---------------------------------------------------

    #[test]
    fn missing_required_takes_precedence_over_unknown_key() {
        // table 欠落 + 未知キー bogus の両方を含む入力は、必須欠落が先に
        // 検出される（フィールド走査を先に行うため）。
        let v = obj(r#"{"op":"search","limit":1,"bogus":1}"#);
        assert_eq!(
            SEARCH_SCHEMA.validate(&v).unwrap_err(),
            SchemaError::MissingRequired { key: "table" }
        );
    }
}
