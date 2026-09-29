//! `POST /v1/query` の `update` op を SQL 表層の
//! `UPDATE ... WHERE id = <n> USING OPERATION_ID`（SQL-17）・述語形
//! `UPDATE ... WHERE <述語> USING OPERATION_ID`（SQL-19・TASK-192）と
//! **同一の実行器**（`engine::sql::exec::execute_update_with_schema`・
//! `execute_predicate_update`）へ、SQL テキストを組み立てずに束縛済み計画で
//! 到達させるモジュール（Issue #876・#1062・TASK-186・対象ビヘイビア
//! NOSQL-6・NOSQL-12。ポインタ: `docs/spec/05-tasks.md` TASK-178・TASK-186・
//! TASK-192・`docs/spec/04-behavior/nosql-surface.md` NOSQL-6・NOSQL-12・
//! `docs/spec/04-behavior/sql-surface.md` SQL-17・SQL-19）。
//!
//! 責務境界: [`super::schema::UPDATE_SCHEMA`] が形を検証済みの JSON
//! オブジェクトから `table`／`set`／`where`（または `filter`）／
//! `operation_id` を取り出し、[`map_set_assignments`]（純関数・engine 非依存。
//! JSON `set` を `engine::sql::allowlist::InsertLiteral` 列へ写像する）と
//! [`super::dml_target::bind_target_form`]（`where`／`filter` の排他判定。
//! 単一行形は [`super::dml_target::TargetForm::RowId`]、述語形は
//! [`super::dml_target::TargetForm::Predicate`]）で束縛したうえで、
//! [`execute`] が単一行形は
//! `engine::core::EngineCore::execute_bound_update_in_session`
//! （[`super::insert`] と同型のセッション対応エントリ）、述語形は
//! `execute_bound_predicate_update_in_session`（Issue #1062）へ委譲する。
//! 単一行形は `engine::sql::parser::bind_update`、述語形は
//! [`super::filter::bind_filter_where_predicates`]（`WHERE` 述語列を SQL 表層
//! と同一の `WherePredicate` へ写像する。`content_hash` 一致のため必須）が
//! SET 対象の禁止列（`id`／`tenant_id`／`visibility`）・重複列・未知列・
//! 型不一致の検査を担い、いずれも第 2 の実行器は作らない。
//!
//! テナントは `principal`（唯一の入口）からのみ導出する（`security.md` P0）。
//!
//! `set` の JSON → `InsertLiteral` 写像は **配列形のみ**を `VECTOR` 列の値と
//! して受理する（`insert` op と同じ「文字列形のベクトルリテラルは受理しない」
//! 非対称。`docs/design/nosql-update-delete-mapping.md` 参照）。`VECTOR` 列
//! 要素の `f32` 変換は `engine::json::JsonNumber::as_f32`（`insert.rs::
//! bind_row` と同一の単一丸め経路）を経由し、`NegInt(0)`（JSON `-0`）は
//! 明示的に `"-0"` として直列化する——SQL・NoSQL を跨いだ同一 `operation_id`
//! 再送時の `content_hash` 一致を保証するため
//! （`insert.rs::bind_row` のドキュメント参照）。
//!
//! `set` の各キーは engine へ渡す前に [`super::ident::check_identifier`]
//! （`42601`）を通す（`table` に対する既存の同判断・PR #823 と同じ理由。
//! untrusted な列名が engine 側のエラー文言へ埋め込まれて `XX000` へ縮退する
//! 経路を塞ぐ）。`set` が空オブジェクトの場合も `42601`（SQL 文法は SET 項目
//! ≥ 1 を要求する）。
//!
//! `operation_id` の欠落・`null`・空文字はいずれも `23502`
//! （`engine::sql::using_operation_id::OperationId::parse` の契約をそのまま
//! 透過する）。同一 `operation_id` への再送は台帳照合（TASK-101・
//! RECOVER-10）により内容一致なら `23505`・不一致なら `22023` へ収束する
//! （SQL 表層と同じ台帳キー空間 `(tenant, table, operation_id)` を共有する。
//! `crates/engine/tests/sql_update_delete_session_public_api.rs` で固定）。
//! 本モジュールの `set`（[`Validated::optional_object`] 由来の `BTreeMap`）は
//! キーがアルファベット順へ正規化されるが、内容一致判定に使う
//! `content_hash::for_update_columns` はスキーマ列順（`tenant::
//! update_row_columns_unchecked` が呼び出し前に正規化する）で計算されるため、
//! SQL 表層が非アルファベット順の宣言で書いた `UPDATE` と同一の
//! `operation_id`・同一内容で本 op から再送しても列の記述順の違いだけで
//! `22023` に誤判定されることはない（Issue #876 レビュー指摘の是正。
//! `crates/wire-server/tests/nosql12_update_delete.rs::
//! cross_surface_multi_column_set_declared_out_of_alphabetical_order_is_treated_as_duplicate`
//! で固定）。
//!
//! [`execute`] は成功時 [`UpdateSuccess`] を返し、[`handle`] が
//! [`encode_success_body`]（`{"updated":<n>,"operation_id":"<escaped>"}`。
//! キー順固定・空白なし）を経由して `gate.rs` の `Op::Update` アームへ
//! ディスパッチ可能な応答バイト列へ写像する（`insert::encode_success_body`
//! と同型）。他テナント所有 id・未存在 id はいずれも `updated:0`・`200`
//! （RLS-9。`execute_update_with_schema` のドキュメント参照）。
//!
//! 対象外: `RETURNING`（Issue #873）・`filter` の `INTEGER`／`BIGINT`／
//! `REAL`／`DOUBLE PRECISION` 列への `eq`（Issue #945。`filter.rs` と同じ
//! `0A000`）。

use std::fmt::Write as _;

use engine::catalog::TableSchema;
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::{InsertLiteral, SqlSurfaceError, ValidatedUpdate};
use engine::sql::exec::UpdateOutcome;
use engine::sql::parser::bind_update;

use crate::http::error_body::escape_json_string_into;
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

use super::dml_target::{bind_target_form, DmlTargetError, TargetForm};
use super::filter;
use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated};
use super::typed_json::{self, TypedJsonError};

/// [`map_set_assignments`]・[`execute`] の失敗を表す。いずれも
/// [`ClassifiedError`] を実装する（`insert.rs::InsertError` と同じ設計）。
#[derive(Debug, Clone)]
pub enum UpdateError {
    /// [`Validated`] アクセサの型・キー不整合（多層防御）。
    Shape(SchemaError),
    /// `table`／`set` のキーが識別子として意味を持ちうる形状を満たさない。
    InvalidIdentifier,
    /// `where`／`filter` の排他判定・述語形未対応（[`super::dml_target`]）。
    Target(DmlTargetError),
    /// `set` が空オブジェクト（`42601`。SQL 文法が SET 項目 ≥ 1 を要求する
    /// ことと同じ判断）。
    EmptySet,
    /// `set` の値・型不一致（`42601`。禁止列・未知列は wire 側で判定せず
    /// `bind_update`（`Engine` アーム）へ委譲する）。`insert.rs::InsertError`
    /// と同じく [`TypedJsonError`]（NOSQL-17。Issue #896）を単一情報源とする。
    Set(TypedJsonError),
    /// `engine::sql::parser::bind_update`（禁止列・未知列・型不一致）・
    /// `EngineCore::execute_bound_update_in_session`（`operation_id` 必須化・
    /// 台帳照合・テーブル不存在）のエラーをそのまま透過する。
    Engine(SqlSurfaceError),
}

impl From<SchemaError> for UpdateError {
    fn from(err: SchemaError) -> Self {
        UpdateError::Shape(err)
    }
}

impl From<InvalidIdentifier> for UpdateError {
    fn from(_err: InvalidIdentifier) -> Self {
        UpdateError::InvalidIdentifier
    }
}

impl From<DmlTargetError> for UpdateError {
    fn from(err: DmlTargetError) -> Self {
        UpdateError::Target(err)
    }
}

impl From<SqlSurfaceError> for UpdateError {
    fn from(err: SqlSurfaceError) -> Self {
        UpdateError::Engine(err)
    }
}

impl ClassifiedError for UpdateError {
    fn error_class(&self) -> ErrorClass {
        match self {
            UpdateError::Shape(err) => err.error_class(),
            UpdateError::InvalidIdentifier => ErrorClass::UnsupportedSqlSyntax,
            UpdateError::Target(err) => err.error_class(),
            UpdateError::EmptySet => ErrorClass::UnsupportedSqlSyntax,
            UpdateError::Set(err) => err.error_class(),
            UpdateError::Engine(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            UpdateError::Shape(err) => err.client_message(),
            UpdateError::InvalidIdentifier => "invalid identifier".to_string(),
            UpdateError::Target(err) => err.client_message(),
            UpdateError::EmptySet => "SET clause must specify at least one column".to_string(),
            UpdateError::Set(err) => err.client_message(),
            UpdateError::Engine(err) => err.client_message(),
        }
    }
}
/// `set`（[`Validated::optional_object`]`("set")` が返す `BTreeMap`）を
/// `schema` の列型で分岐しながら `Vec<(String, InsertLiteral)>` へ写像する
/// （判定順序: 禁止列 → 列名解決 → 型。値・型の判定本体は
/// [`super::typed_json::map_json_to_literal`]（NOSQL-17。Issue #896）が
/// `insert.rs` と共有する単一情報源）。
///
/// `set` の各キーは呼び出し元（[`execute`]）が [`ident::check_identifier`]
/// で先に形状検査済みであることを前提とする（本関数自身は形状を再検査
/// しない。`engine::sql::parser::bind_update` が改めて禁止列・未知列を
/// 検査するため、多層防御は保たれる）。
fn map_set_assignments(
    set: &std::collections::BTreeMap<String, JsonValue>,
    schema: &TableSchema,
) -> Result<Vec<(String, InsertLiteral)>, UpdateError> {
    if set.is_empty() {
        return Err(UpdateError::EmptySet);
    }
    let mut assignments: Vec<(String, InsertLiteral)> = Vec::with_capacity(set.len());
    for (key, raw) in set.iter() {
        // 禁止列（`id`／`tenant_id`／`visibility`）は `bind_update` が `42601`
        // で拒否する契約のため、ここでは列名解決を先に試み、`schema` に無い
        // 列名（禁止列を含む）はそのまま `bind_update` へ委譲する
        // （wire 側で先取りして `22000` に丸めない。禁止列専用の判定は
        // engine 側の単一情報源に保つ）。
        let column = schema.columns.iter().find(|c| &c.name == key);
        let Some(column) = column else {
            // 未知列（禁止列も含む可能性がある）は `bind_update` へそのまま
            // 委譲する必要があるため、ここでは型を特定できない。プレース
            // ホルダーの `InsertLiteral::String` を渡し、実際の判定は
            // `bind_update` に一任する（列名だけで完結する判定のため値の
            // 種類は結果に影響しない）。
            assignments.push((key.clone(), InsertLiteral::String(String::new())));
            continue;
        };
        let literal = typed_json::map_json_to_literal(column, raw).map_err(UpdateError::Set)?;
        assignments.push((key.clone(), literal));
    }
    Ok(assignments)
}

/// `update` op 成功時の応答材料（[`encode_success_body`] の唯一の情報源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSuccess {
    pub updated: u64,
    pub operation_id: OperationId,
}

/// [`UpdateSuccess`] を成功応答本文（JSON）へ写像する（infallible）。
/// 出力形 `{"updated":<n>,"operation_id":"<escaped>"}`（`insert::
/// encode_success_body` と同型のキー順固定・空白なし規約）。
pub fn encode_success_body(success: &UpdateSuccess) -> String {
    let mut out = String::with_capacity(success.operation_id.as_str().len() + 64);
    out.push_str("{\"updated\":");
    let _ = write!(out, "{}", success.updated);
    out.push_str(",\"operation_id\":\"");
    escape_json_string_into(&mut out, success.operation_id.as_str());
    out.push_str("\"}");
    out
}

/// [`map_set_assignments`] の結果を、束縛 closure（`Result<_, SqlSurfaceError>`
/// を要求する `EngineCore::execute_bound_update_in_session`／
/// `execute_bound_predicate_update_in_session` の共通契約）が要求する形へ
/// 写像する（[`execute`] の単一行・述語形の両分岐で共有する。重複実装を
/// 避けるための抽出。Issue #1062）。
fn map_set_assignments_for_engine(
    set: &std::collections::BTreeMap<String, JsonValue>,
    schema: &TableSchema,
) -> Result<Vec<(String, InsertLiteral)>, SqlSurfaceError> {
    map_set_assignments(set, schema).map_err(|e| match e {
        UpdateError::Engine(err) => err,
        // `TypedJsonError`（NOSQL-17。Issue #896）の分類は単一の
        // `into_sql_surface_error` 変換点に集約する（`insert.rs` と共有）。
        UpdateError::Set(err) => err.into_sql_surface_error(),
        UpdateError::EmptySet => SqlSurfaceError::UnsupportedSyntax {
            detail: "SET clause must specify at least one column".to_string(),
        },
        // `bind_target_form`／`Shape`／`InvalidIdentifier` はこの
        // closure より前に確定済みのため到達不能だが、fail-closed の
        // まま網羅する。
        UpdateError::Shape(_) | UpdateError::InvalidIdentifier | UpdateError::Target(_) => {
            SqlSurfaceError::Internal {
                detail: "unexpected error during UPDATE SET binding".to_string(),
            }
        }
    })
}

/// `POST /v1/query` の `update` op を実行する。`validated` は
/// [`super::schema::UPDATE_SCHEMA::validate`] を通過済みの JSON オブジェクト。
pub fn execute(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<UpdateSuccess, UpdateError> {
    let table = validated
        .required_str("table")
        .map_err(UpdateError::Shape)?;
    ident::check_identifier(table)?;

    let target = bind_target_form(validated)?;

    let set = validated
        .required_object("set")
        .map_err(UpdateError::Shape)?;
    for key in set.keys() {
        ident::check_identifier(key)?;
    }

    let operation_id_raw = validated
        .optional_str("operation_id")
        .map_err(UpdateError::Shape)?
        .unwrap_or("");
    let operation_id = OperationId::parse(operation_id_raw)?;

    let outcome: UpdateOutcome = match target {
        TargetForm::RowId(id) => core.execute_bound_update_in_session(
            principal.policy_context(),
            table,
            Some(&operation_id),
            |schema| {
                let assignments = map_set_assignments_for_engine(set, schema)?;
                let stmt = ValidatedUpdate {
                    table_name: table.to_string(),
                    assignments,
                    id_literal: id.to_string(),
                    operation_id: Some(operation_id.clone()),
                    // NoSQL 表層 `op: update` は `RETURNING` を公開しない
                    // （Issue #876 のスコープ外。SQL 表層の `RETURNING`
                    // 実行結線は Issue #873・SQL-21 が別途担当し、
                    // `ValidatedUpdate::returning` を `Some` にする経路は
                    // ここには無い）。
                    returning: None,
                };
                bind_update(&stmt, schema)
            },
        )?,
        // 述語形（`filter`。TASK-186・NOSQL-12、Issue #1062）: SET 束縛は
        // 単一行形と同一（`map_set_assignments_for_engine`）を共有し、
        // `WHERE` 述語列は `filter::bind_filter_where_predicates`
        // （SQL 表層の述語形 `UPDATE` が使うのと同一の `WherePredicate` を
        // 生成する。`content_hash` 一致のため必須）で得る。実行本体
        // （台帳照合・影響行数上限・RLS 適用）は
        // `EngineCore::execute_bound_predicate_update_in_session` に一任し、
        // 第 2 の実行器は作らない。
        TargetForm::Predicate(items) => core.execute_bound_predicate_update_in_session(
            principal.policy_context(),
            table,
            Some(&operation_id),
            |schema| {
                let assignments = map_set_assignments_for_engine(set, schema)?;
                let predicates = filter::bind_filter_where_predicates(items, schema)
                    .map_err(filter::FilterError::into_sql_surface_error)?;
                Ok((assignments, predicates))
            },
        )?,
    };

    Ok(UpdateSuccess {
        updated: outcome.rows_affected,
        operation_id,
    })
}

/// `POST /v1/query` の `update` op を処理し応答バイト列を返す（`gate.rs` から
/// `engine` 接続済み時のみ呼ばれる。`insert::handle` と同一シグネチャ形）。
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
    use engine::catalog::{ColumnDef, ColumnType};
    use engine::json::parse_json;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        )
    }

    fn set_map(json: &str) -> std::collections::BTreeMap<String, JsonValue> {
        let JsonValue::Object(m) = parse_json(json).expect("valid json") else {
            panic!("expected object");
        };
        m
    }

    #[test]
    fn map_set_assignments_maps_text_column() {
        let set = set_map(r#"{"lang":"en"}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok");
        assert_eq!(
            bound,
            vec![("lang".to_string(), InsertLiteral::String("en".to_string()))]
        );
    }

    #[test]
    fn map_set_assignments_maps_vector_array() {
        // Issue #896 レビュー指摘（PR #1038）以降、`VECTOR` 列は
        // `InsertLiteral::Vector`（テキストリテラルの 64 KiB 上限を経由しない
        // 直接構築の `f32` 列）へ束縛される（`typed_json::vector_literal_values`
        // 参照）。
        let set = set_map(r#"{"embedding":[1,2,3]}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "embedding".to_string(),
                InsertLiteral::Vector(vec![1.0, 2.0, 3.0])
            )]
        );
    }

    #[test]
    fn map_set_assignments_preserves_negative_zero() {
        let set = set_map(r#"{"embedding":[-0,1,2]}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "embedding".to_string(),
                InsertLiteral::Vector(vec![-0.0, 1.0, 2.0])
            )]
        );
        // `-0.0` はビットパターンまで一致することを確認する（符号ビットの
        // 保持は `engine::json::JsonNumber::as_f32` の契約。テキスト直列化を
        // 経由しなくなった後も同じ保証を維持する）。
        let InsertLiteral::Vector(values) = &bound[0].1 else {
            panic!("expected InsertLiteral::Vector");
        };
        assert_eq!(values[0].to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn map_set_assignments_preserves_float_text() {
        let set = set_map(r#"{"embedding":[1.5,2.25,3.0]}"#);
        let bound = map_set_assignments(&set, &schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "embedding".to_string(),
                InsertLiteral::Vector(vec![1.5, 2.25, 3.0])
            )]
        );
    }

    #[test]
    fn map_set_assignments_rejects_string_for_vector_column() {
        let set = set_map(r#"{"embedding":"[1,2,3]"}"#);
        assert!(matches!(
            map_set_assignments(&set, &schema()),
            Err(UpdateError::Set(_))
        ));
    }

    #[test]
    fn map_set_assignments_rejects_number_for_text_column() {
        let set = set_map(r#"{"lang":1}"#);
        assert!(matches!(
            map_set_assignments(&set, &schema()),
            Err(UpdateError::Set(_))
        ));
    }

    // Issue #896 で JSON `null` は列型を問わず `InsertLiteral::Null` へ写像
    // する契約へ一般化しかけたが、`TEXT`／`ENUM` 列については Issue #896
    // 以前の `update` op が `nullable` 属性に関わらず `null` を一律拒否して
    // いた契約を維持する（PR #1038 レビュー指摘。`typed_json::
    // map_json_to_literal` が `TEXT`／`ENUM` 列の `null` を列型固有の
    // `TypedJsonError` で拒否する。`docs/design/nosql-typed-json-binding.md`
    // 「null の扱い」節参照）。`TEXT`／`ENUM` 以外の列（`bind_update` の
    // nullable 判定へ委譲する設計自体）は不変で、非 nullable 列への
    // `NULL` 拒否は引き続き `bind_update` の責務
    // （`execute_rejects_null_for_non_nullable_column_via_engine` で
    // end-to-end に固定する）。
    #[test]
    fn map_set_assignments_rejects_null_for_nullable_text_column() {
        let set = set_map(r#"{"lang":null}"#);
        assert!(matches!(
            map_set_assignments(&set, &schema()),
            Err(UpdateError::Set(TypedJsonError::LegacyMismatch(_)))
        ));
    }

    #[test]
    fn map_set_assignments_rejects_bool_and_object() {
        for json in [r#"{"lang":true}"#, r#"{"lang":{}}"#] {
            let set = set_map(json);
            assert!(matches!(
                map_set_assignments(&set, &schema()),
                Err(UpdateError::Set(_))
            ));
        }
    }

    #[test]
    fn map_set_assignments_rejects_empty_set() {
        let set = set_map("{}");
        assert!(matches!(
            map_set_assignments(&set, &schema()),
            Err(UpdateError::EmptySet)
        ));
    }

    #[test]
    fn map_set_assignments_rejects_vector_dimension_mismatch_via_bind_update() {
        // Issue #896 レビュー指摘（PR #1038）以降、次元検証は
        // `typed_json::vector_literal_values`（本関数が呼び出す JSON → `f32`
        // 直接束縛）がアロケーション前に行うため、本関数自身が `22000` で
        // 拒否する（`bind_update` 側での遅延検証ではなくなった）。
        let set = set_map(r#"{"embedding":[1,2]}"#);
        let err = map_set_assignments(&set, &schema()).expect_err("dimension mismatch rejected");
        assert!(matches!(
            err,
            UpdateError::Set(TypedJsonError::LegacyMismatch(_))
        ));
    }

    #[test]
    fn update_error_wire_codes_match_expected_classes() {
        assert_eq!(UpdateError::InvalidIdentifier.wire_code(), "42601");
        assert_eq!(UpdateError::EmptySet.wire_code(), "42601");
        assert_eq!(
            UpdateError::Set(TypedJsonError::LegacyMismatch("x")).wire_code(),
            "22000"
        );
        assert_eq!(
            UpdateError::Set(TypedJsonError::InvalidByteaText("x")).wire_code(),
            "22P02"
        );
        assert_eq!(
            UpdateError::Set(TypedJsonError::ByteaTooLarge).wire_code(),
            "54000"
        );
    }

    // --- BYTEA 列（Issue #886）の base64 → 正準 hex 再エンコード ---------------

    fn bytea_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("blob", ColumnType::Bytea, true),
            ],
        )
    }

    #[test]
    fn map_set_assignments_reencodes_base64_bytea_to_canonical_hex() {
        // "3q2+7w==" は [0xde, 0xad, 0xbe, 0xef] の標準 base64 表現。
        let set = set_map(r#"{"blob":"3q2+7w=="}"#);
        let bound = map_set_assignments(&set, &bytea_schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "blob".to_string(),
                InsertLiteral::String("\\xdeadbeef".to_string())
            )]
        );
    }

    #[test]
    fn map_set_assignments_rejects_non_string_bytea() {
        let set = set_map(r#"{"blob":true}"#);
        let err = map_set_assignments(&set, &bytea_schema()).expect_err("must reject");
        assert!(matches!(
            err,
            UpdateError::Set(TypedJsonError::TypeMismatch(_))
        ));
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn map_set_assignments_rejects_malformed_base64_bytea() {
        let set = set_map(r#"{"blob":"3q2+7w=a"}"#);
        let err = map_set_assignments(&set, &bytea_schema()).expect_err("must reject");
        assert!(matches!(
            err,
            UpdateError::Set(TypedJsonError::InvalidByteaText(_))
        ));
        assert_eq!(err.wire_code(), "22P02");
    }

    // --- JSON／JSONB 列（Issue #889）の null 分岐（PR #1014 レビュー指摘対応） ---

    fn json_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("doc", ColumnType::Json, true),
                ColumnDef::new("docb", ColumnType::Jsonb, false),
            ],
        )
    }

    #[test]
    fn map_set_assignments_maps_json_null_on_nullable_column_to_insert_literal_null() {
        let set = set_map(r#"{"doc":null}"#);
        let bound = map_set_assignments(&set, &json_schema()).expect("ok");
        assert_eq!(bound, vec![("doc".to_string(), InsertLiteral::Null)]);
    }

    // Issue #896 で null 判定を一般化した結果、`map_set_assignments` 単体では
    // 非 nullable 列への `null` も `InsertLiteral::Null` として通過する
    // （nullable 判定は `bind_update` の責務。`execute_rejects_null_for_non_nullable_column_via_engine`
    // で end-to-end に `22000` を固定する）。
    #[test]
    fn map_set_assignments_maps_json_null_on_non_nullable_column_to_insert_literal_null() {
        let set = set_map(r#"{"docb":null}"#);
        let bound = map_set_assignments(&set, &json_schema()).expect("ok");
        assert_eq!(bound, vec![("docb".to_string(), InsertLiteral::Null)]);
    }

    #[test]
    fn map_set_assignments_maps_json_object_column() {
        let set = set_map(r#"{"doc":{"a":1}}"#);
        let bound = map_set_assignments(&set, &json_schema()).expect("ok");
        assert_eq!(
            bound,
            vec![(
                "doc".to_string(),
                InsertLiteral::String(r#"{"a":1}"#.to_string())
            )]
        );
    }

    // --- 新型（Issue #896）の SET 束縛と end-to-end 実行 -----------------------

    #[test]
    fn map_set_assignments_maps_integer_and_boolean() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("count", ColumnType::Integer, true),
                ColumnDef::new("active", ColumnType::Boolean, true),
            ],
        );
        let set = set_map(r#"{"count":42,"active":true}"#);
        let bound = map_set_assignments(&set, &schema).expect("ok");
        assert!(bound.contains(&("count".to_string(), InsertLiteral::Number("42".to_string()))));
        assert!(bound.contains(&("active".to_string(), InsertLiteral::Bool(true))));
    }

    #[test]
    fn map_set_assignments_passes_float_for_integer_column_to_engine_binding() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("count", ColumnType::Integer, true),
            ],
        );
        let set = set_map(r#"{"count":1.5}"#);
        // 小数の拒否（`22P02`）は engine の整数束縛が担う（Issue #1187）。
        let bound = map_set_assignments(&set, &schema).expect("mapping passes through");
        assert!(bound.contains(&(
            "count".to_string(),
            InsertLiteral::Number("1.5".to_string())
        )));
    }

    fn open_core() -> (EngineCore, std::path::PathBuf) {
        use engine::kernel::CpuScalarProvider;
        use engine::storage::Storage;
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "wire-update-typed-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(3), false),
                    ColumnDef::new("count", ColumnType::Integer, true),
                    ColumnDef::new("doc", ColumnType::Json, false),
                ],
            ))
            .expect("create table");
        (
            EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
            path,
        )
    }

    fn principal(tenant: &str) -> SessionPrincipal {
        use crate::http::headers::{parse_headers, HeaderParse};
        use crate::http::session::middleware;
        use crate::http::session::store::SessionStore;
        use engine::policy::PolicyContext;
        use engine::storage::Visibility;
        use std::time::Instant;

        let sessions = SessionStore::new();
        let now = Instant::now();
        // 実運用の `auth::verify` は `Public` ＋ 自テナントの `Private` を
        // 許可する `PolicyContext` を返す（RLS-11・TASK-195「read-your-writes」）。
        // `PolicyContext::new`（`Public` のみ）だと SQL/NoSQL の既定書き込み
        // 可視性（常に `Private`）を自テナントでも読み戻せず、update の対象
        // 行探索が「不可視」として `updated:0` に丸まってしまう。
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid ctx");
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

    fn insert_seed_row(core: &EngineCore, principal: &SessionPrincipal) {
        let mut session = engine::sql::mode::SessionState::default();
        core.execute_sql_in_session(
            principal.policy_context(),
            &mut session,
            "INSERT INTO docs (id, embedding, doc) VALUES (1, '[1,0,0]', '{}') USING OPERATION_ID 'seed-1'",
        )
        .expect("seed insert ok");
    }

    /// `map_set_assignments`（wire 層）は非 nullable 列への JSON `null` も
    /// `InsertLiteral::Null` として通過させるが、`bind_update`（engine 側の
    /// 単一情報源）が最終的に `22000` で拒否する契約を end-to-end で固定する
    /// （`map_set_assignments_maps_json_null_on_non_nullable_column_to_insert_literal_null`
    /// と対になるテスト）。
    #[test]
    fn execute_rejects_null_for_non_nullable_column_via_engine() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        insert_seed_row(&core, &principal);

        let body = r#"{"op":"update","table":"docs","where":{"id":1},"set":{"doc":null},"operation_id":"op-null"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::UPDATE_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let err = execute(&core, &principal, &validated).expect_err("must reject");
        // TABLE-16・TASK-204、Issue #904: NOT NULL 違反は `23502`
        // （`NotNullViolation`）へ写像する（旧 `22000` から契約変更）。
        assert_eq!(err.wire_code(), "23502");
        let _ = std::fs::remove_file(&path);
    }

    /// 新型（`INTEGER`）の SET 束縛が engine 経由で実際に成功することを
    /// end-to-end で固定する（NOSQL-17。Issue #896）。
    #[test]
    fn execute_updates_integer_column_via_engine() {
        let (core, path) = open_core();
        let principal = principal("tenant-a");
        insert_seed_row(&core, &principal);

        let body = r#"{"op":"update","table":"docs","where":{"id":1},"set":{"count":7},"operation_id":"op-int"}"#;
        let value = parse_json(body).expect("valid json");
        let validated = super::super::schema::UPDATE_SCHEMA
            .validate(&value)
            .expect("schema ok");

        let success = execute(&core, &principal, &validated).expect("update ok");
        assert_eq!(success.updated, 1);
        let _ = std::fs::remove_file(&path);
    }
}
