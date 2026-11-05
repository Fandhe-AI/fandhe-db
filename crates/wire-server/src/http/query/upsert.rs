//! `POST /v1/query` の `upsert` op を SQL 表層の `INSERT ... ON CONFLICT`（SQL-20・SQL-49）と
//! 同じ束縛関数・同じ実行器へ写像するモジュール（Issue #2087・NOSQL-21 ポインタ。親 #2085。
//! ポインタ: `docs/spec/04-behavior/nosql-surface.md` NOSQL-21）。
//!
//! 呼び出し文脈: `gate.rs` の `Op::Upsert` アーム（engine 接続済み時）が [`handle`] を呼び、
//! [`execute`] が `EngineCore::execute_bound_upsert_in_session` へ委譲する。束縛 closure の中で
//! `engine::sql::parser::bind_insert_form`（SQL 表層と同一の束縛関数。文内の id 重複・対象キー重複・
//! 衝突対象の解決〔`42P10`〕・`SET` 列の検査はすべてここに委ねる）を呼び、実行は engine の
//! `sql::exec::execute_upsert_in`（SQL 表層と同一の実行器）が行う。SQL テキストを一切生成せず、
//! 第 2 の実行器・独自の衝突判定は作らない。
//!
//! 責務境界と判断（自動運転で確定した安全側の解釈。nosql-api.md の本格記述は Issue #2088 の担当）:
//!
//! - `rows[*]` は `insert` と同じ写像（[`super::insert::row_literals`]）で列値へ変換する。
//!   `ValidatedInsert` の列リストは全行共有のため、**全行のキー集合（`id` を含む）が一致すること**を
//!   要求する（不一致は固定文言の `22000`）。行ごとに個別束縛する方式は、文内の重複検出を
//!   迂回するため採らない。
//! - `on_conflict.do` は `"nothing"`／`"update"` の完全一致のみ受理する（それ以外は `42601`）。
//!   `"nothing"` に `set` は付けられず（`42601`）、`"update"` の `set` は各列を `EXCLUDED.<col>` で更新する
//!   列名の配列（空配列は `42601`）。
//! - `"update"` で `set` を省略した場合は「`rows` のキー集合 − `on_conflict.columns` − 疑似列 `id`」を
//!   `EXCLUDED` で更新する。表の全列は採らない（要求に含まれない列を既定値／NULL で上書きする
//!   破壊的な解釈を避けるため）。結果が空なら `42601`。列順は `rows` のキー順で決定的。
//! - `returning` は付けられない（スキーマ外キーとして `42601`。NOSQL-22 は対象外）。
//!
//! テナントは `principal` からのみ導出し、JSON から読む経路を持たない（`security.md` P0）。
//! 衝突判定は engine のテナント名前空間のキーで行われるため、他テナントの行との衝突は
//! 観測できない（RLS-9）。列名・`do` の値等の未信頼な値はエラー文言へ埋め込まない（固定文言のみ）。
//!
//! `operation_id` の供給・省略・台帳照合は `insert` と同一（HTTP-14・RECOVER-1・RECOVER-11）。
//! 応答は `{"upserted":<n>[,"operation_id":"<escaped>"]}`（`n` は SQL の `INSERT 0 n` と同じ値で、
//! 挿入件数＋更新件数。`DO NOTHING` で飛ばした行は数えない）。

use std::fmt::Write as _;

use engine::catalog::TableSchema;
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::{
    ConflictTargetSpec, InsertLiteral, OnConflictAction, OnConflictClause, SqlSurfaceError,
    UpsertValue, ValidatedInsert,
};
use engine::sql::parser::{bind_insert_form, BoundInsertForm, BoundUpsert};

use crate::http::error_body::escape_json_string_into;
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

use super::ident::{self, InvalidIdentifier};
use super::insert::{invalid_input_error, row_literals, InsertError};
use super::operation_id_source::{resolve_operation_id, OperationIdSourceError};
use super::schema::{SchemaError, Validated};
use super::typed_json::TypedJsonError;

/// [`bind`]・[`execute`] の失敗を表す。いずれも [`ClassifiedError`] を実装し、`wire_code`／
/// `client_message` 経由で HTTP エラー応答へ射影される（`insert::InsertError` と同じ設計）。
#[derive(Debug, Clone)]
pub enum UpsertError {
    /// スキーマ通過後のアクセサでの再検査失敗（多層防御）。
    Shape(SchemaError),
    /// `table`・列名が識別子の形状検査を満たさない（`42601`）。
    InvalidIdentifier,
    /// `on_conflict` の値レベルの不正（`do` の語彙外・`set` の不整合・`columns` の空／重複）。
    /// 固定文言のみを保持する（`42601`）。
    OnConflict(&'static str),
    /// 束縛段のエラー（`bind_insert_form` 由来を含む。SQL 表層と同一の分類）。
    Bind(SqlSurfaceError),
    /// engine 入口側のエラー（`operation_id` 必須化・台帳照合・上限・権限・テーブル不存在等）。
    Exec(SqlSurfaceError),
    /// JSON 値 → `InsertLiteral` 写像時の型不一致（NOSQL-17）。
    Set(TypedJsonError),
}

impl From<SchemaError> for UpsertError {
    fn from(err: SchemaError) -> Self {
        UpsertError::Shape(err)
    }
}

impl From<InvalidIdentifier> for UpsertError {
    fn from(_err: InvalidIdentifier) -> Self {
        UpsertError::InvalidIdentifier
    }
}

impl From<InsertError> for UpsertError {
    fn from(err: InsertError) -> Self {
        match err {
            InsertError::Shape(e) => UpsertError::Shape(e),
            InsertError::InvalidIdentifier => UpsertError::InvalidIdentifier,
            InsertError::Bind(e) => UpsertError::Bind(e),
            InsertError::Exec(e) => UpsertError::Exec(e),
            InsertError::Set(e) => UpsertError::Set(e),
        }
    }
}

impl ClassifiedError for UpsertError {
    fn error_class(&self) -> ErrorClass {
        match self {
            UpsertError::Shape(err) => err.error_class(),
            UpsertError::InvalidIdentifier | UpsertError::OnConflict(_) => {
                ErrorClass::UnsupportedSqlSyntax
            }
            UpsertError::Bind(err) | UpsertError::Exec(err) => err.error_class(),
            UpsertError::Set(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            UpsertError::Shape(err) => err.client_message(),
            UpsertError::InvalidIdentifier => "invalid identifier".to_string(),
            UpsertError::OnConflict(msg) => (*msg).to_string(),
            UpsertError::Bind(err) | UpsertError::Exec(err) => err.client_message(),
            UpsertError::Set(err) => err.client_message(),
        }
    }
}

impl UpsertError {
    /// engine の束縛 closure が要求する `SqlSurfaceError` へ写像する。`Shape` は `bind` からは
    /// 構築されない（`schema.rs` の意味的検証専用）ため `Internal` に丸める（fail-closed）。
    fn into_surface_error(self) -> SqlSurfaceError {
        match self {
            UpsertError::Bind(err) | UpsertError::Exec(err) => err,
            UpsertError::InvalidIdentifier => SqlSurfaceError::UnsupportedSyntax {
                detail: "invalid identifier".to_string(),
            },
            UpsertError::OnConflict(msg) => SqlSurfaceError::UnsupportedSyntax {
                detail: msg.to_string(),
            },
            UpsertError::Shape(_) => SqlSurfaceError::Internal {
                detail: "unexpected shape error during UPSERT row binding".to_string(),
            },
            UpsertError::Set(err) => err.into_sql_surface_error(),
        }
    }
}

/// `on_conflict.do` の解釈結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoKind {
    /// `"nothing"`。
    Nothing,
    /// `"update"`。`Some` は明示した `set` 列、`None` は省略（[`bind`] が既定を解決する）。
    Update(Option<Vec<String>>),
}

/// 識別子の配列を読む。要素が文字列でない／識別子の形状検査に失敗する場合は拒否する。
fn identifier_list(items: &[JsonValue], key: &'static str) -> Result<Vec<String>, UpsertError> {
    let mut out: Vec<String> = Vec::new();
    out.try_reserve_exact(items.len()).map_err(|_| {
        UpsertError::Bind(SqlSurfaceError::Internal {
            detail: "failed to reserve UPSERT column buffer".to_string(),
        })
    })?;
    for item in items {
        let JsonValue::String(name) = item else {
            return Err(UpsertError::Shape(SchemaError::TypeMismatch { key }));
        };
        ident::check_identifier(name)?;
        if out.iter().any(|c| c == name) {
            return Err(UpsertError::OnConflict("duplicate column in on_conflict"));
        }
        out.push(name.clone());
    }
    Ok(out)
}

/// `on_conflict` の値レベルの静的検査（カタログを参照しない段。engine 呼び出しの前）。
/// 衝突対象の列名の解決（主キー・UNIQUE 制約との一致は `42P10`、`(id)` は行キー）は engine の
/// 束縛（`bind_insert_form`）に委ねる。
pub fn parse_on_conflict(
    on_conflict: &Validated<'_>,
) -> Result<(Vec<String>, DoKind), UpsertError> {
    let columns = identifier_list(on_conflict.required_array("columns")?, "columns")?;
    if columns.is_empty() {
        return Err(UpsertError::OnConflict(
            "on_conflict.columns must not be empty",
        ));
    }
    let set = match on_conflict.optional_array("set")? {
        Some(items) => Some(identifier_list(items, "set")?),
        None => None,
    };
    let kind = match on_conflict.required_str("do")? {
        "nothing" => {
            if set.is_some() {
                return Err(UpsertError::OnConflict(
                    "on_conflict.set is not allowed with do nothing",
                ));
            }
            DoKind::Nothing
        }
        "update" => {
            if set.as_ref().is_some_and(Vec::is_empty) {
                return Err(UpsertError::OnConflict("on_conflict.set must not be empty"));
            }
            DoKind::Update(set)
        }
        _ => {
            return Err(UpsertError::OnConflict(
                "on_conflict.do must be \"nothing\" or \"update\"",
            ))
        }
    };
    Ok((columns, kind))
}

/// `rows` を `table`／`operation_id`／`on_conflict` と合わせて `BoundUpsert` へ束縛する。
/// 全行のキー集合の一致を検査し、`set` を解決して `ValidatedInsert`（`ON CONFLICT` 付き）を組み、
/// SQL 表層と同一の `bind_insert_form` へ渡す。`Upsert` 以外の束縛結果は `Internal`（fail-closed）。
pub fn bind(
    items: &[JsonValue],
    table: &str,
    operation_id: Option<&OperationId>,
    conflict_columns: &[String],
    kind: &DoKind,
    schema: &TableSchema,
) -> Result<BoundUpsert, UpsertError> {
    let mut columns: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<InsertLiteral>> = Vec::new();
    rows.try_reserve_exact(items.len()).map_err(|_| {
        UpsertError::Bind(SqlSurfaceError::Internal {
            detail: "failed to reserve UPSERT row buffer".to_string(),
        })
    })?;
    for item in items {
        let (cols, literals) = row_literals(item, schema)?;
        match &columns {
            None => columns = Some(cols),
            Some(first) if *first == cols => {}
            Some(_) => {
                return Err(UpsertError::Bind(invalid_input_error(
                    "UPSERT rows must all have the same keys",
                )))
            }
        }
        rows.push(literals);
    }
    let Some(columns) = columns else {
        return Err(UpsertError::Bind(invalid_input_error(
            "UPSERT batch must contain at least one row",
        )));
    };

    let action = match kind {
        DoKind::Nothing => OnConflictAction::DoNothing,
        DoKind::Update(set) => {
            let set_columns: Vec<String> = match set {
                Some(list) => list.clone(),
                None => columns
                    .iter()
                    .filter(|c| c.as_str() != "id" && !conflict_columns.contains(c))
                    .cloned()
                    .collect(),
            };
            if set_columns.is_empty() {
                return Err(UpsertError::OnConflict(
                    "on_conflict.set resolves to no columns",
                ));
            }
            OnConflictAction::DoUpdate {
                assignments: set_columns
                    .into_iter()
                    .map(|c| (c.clone(), UpsertValue::Excluded(c)))
                    .collect(),
                condition: Vec::new(),
            }
        }
    };

    let validated = ValidatedInsert {
        table_name: table.to_string(),
        columns,
        columns_omitted: false,
        rows,
        operation_id: operation_id.cloned(),
        returning: None,
        on_conflict: Some(OnConflictClause {
            target: ConflictTargetSpec::Columns(conflict_columns.to_vec()),
            action,
        }),
    };
    match bind_insert_form(&validated, schema).map_err(UpsertError::Bind)? {
        BoundInsertForm::Upsert(bound) => Ok(bound),
        _ => Err(UpsertError::Bind(SqlSurfaceError::Internal {
            detail: "unexpected bound form for UPSERT".to_string(),
        })),
    }
}

/// `upsert` op 成功時の応答材料（[`encode_success_body`] の唯一の情報源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertSuccess {
    /// 挿入件数＋更新件数（SQL の `INSERT 0 <n>` の `n` と同じ値）。
    pub upserted: u64,
    /// 要求で検証済みの `operation_id`。省略時は `None`（応答本文に含めない）。
    pub operation_id: Option<OperationId>,
}

/// [`UpsertSuccess`] を成功応答本文へ写像する（infallible）。出力形
/// `{"upserted":<n>[,"operation_id":"<escaped>"]}`（キー順固定・空白なし）。
/// `operation_id` は [`escape_json_string_into`] を通す。
pub fn encode_success_body(success: &UpsertSuccess) -> String {
    let mut out = String::with_capacity(
        success
            .operation_id
            .as_ref()
            .map_or(0, |id| id.as_str().len())
            + 64,
    );
    out.push_str("{\"upserted\":");
    let _ = write!(out, "{}", success.upserted);
    if let Some(id) = &success.operation_id {
        out.push_str(",\"operation_id\":\"");
        escape_json_string_into(&mut out, id.as_str());
        out.push('"');
    }
    out.push('}');
    out
}

/// `POST /v1/query` の `upsert` op を実行する。`validated` は `UPSERT_SCHEMA` 検証を通過済み。
/// 判定順: `table` の形状 → `on_conflict` の静的検査 → `operation_id` の供給元解決 →
/// engine 入口（`23502` → 行数 → カタログ → 束縛 → 権限 → INDEX-4 上限 → 実行）。
pub fn execute(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    header_operation_id: Option<&OperationId>,
) -> Result<UpsertSuccess, UpsertError> {
    let table = validated.required_str("table")?;
    ident::check_identifier(table)?;
    let on_conflict = validated.required_nested("on_conflict")?;
    let (conflict_columns, kind) = parse_on_conflict(&on_conflict)?;
    let rows = validated.required_array("rows")?;
    let operation_id =
        resolve_operation_id(validated, header_operation_id).map_err(|e| match e {
            OperationIdSourceError::Shape(err) => UpsertError::Shape(err),
            other => UpsertError::Exec(other.into_sql_surface_error()),
        })?;

    let outcome = core
        .execute_bound_upsert_in_session(
            principal.policy_context(),
            &principal.session_state(),
            table,
            rows.len(),
            operation_id.as_ref(),
            |schema| {
                bind(
                    rows,
                    table,
                    operation_id.as_ref(),
                    &conflict_columns,
                    &kind,
                    schema,
                )
                .map_err(UpsertError::into_surface_error)
            },
        )
        .map_err(UpsertError::Exec)?;

    Ok(UpsertSuccess {
        upserted: outcome.rows_affected,
        operation_id,
    })
}

/// `POST /v1/query` の `upsert` op を処理し応答バイト列を返す（`gate.rs` から engine 接続済み時のみ
/// 呼ばれる。`insert::handle` と同一シグネチャ形）。
pub fn handle(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    header_operation_id: Option<&OperationId>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute(core, principal, validated, header_operation_id) {
        Ok(success) => http_response::encode_ok(&encode_success_body(&success), now_wall),
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::{ColumnDef, ColumnType};
    use engine::json::parse_json;
    use engine::row_codec::Value;

    use crate::http::query::schema::UPSERT_SCHEMA;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        )
    }

    fn items(json: &str) -> Vec<JsonValue> {
        let JsonValue::Array(items) = parse_json(json).expect("valid JSON") else {
            panic!("expected array");
        };
        items
    }

    fn parse_oc(oc: &str) -> Result<(Vec<String>, DoKind), UpsertError> {
        let json = format!(r#"{{"op":"upsert","table":"docs","rows":[],"on_conflict":{oc}}}"#);
        let value = parse_json(&json).expect("valid JSON");
        let validated = UPSERT_SCHEMA.validate(&value).expect("schema ok");
        let nested = validated.required_nested("on_conflict").expect("nested");
        parse_on_conflict(&nested)
    }

    #[test]
    fn parse_on_conflict_accepts_nothing_and_update_forms() {
        let (cols, kind) = parse_oc(r#"{"columns":["id"],"do":"nothing"}"#).unwrap();
        assert_eq!(cols, vec!["id".to_string()]);
        assert_eq!(kind, DoKind::Nothing);
        let (_, kind) = parse_oc(r#"{"columns":["id"],"do":"update"}"#).unwrap();
        assert_eq!(kind, DoKind::Update(None));
        let (_, kind) = parse_oc(r#"{"columns":["id"],"do":"update","set":["lang"]}"#).unwrap();
        assert_eq!(kind, DoKind::Update(Some(vec!["lang".to_string()])));
    }

    #[test]
    fn parse_on_conflict_rejects_bad_values_with_42601() {
        for oc in [
            r#"{"columns":[],"do":"nothing"}"#,
            r#"{"columns":["id","id"],"do":"nothing"}"#,
            r#"{"columns":["id"],"do":"NOTHING"}"#,
            r#"{"columns":["id"],"do":"replace"}"#,
            r#"{"columns":["id"],"do":"nothing","set":["lang"]}"#,
            r#"{"columns":["id"],"do":"update","set":[]}"#,
            r#"{"columns":["id"],"do":"update","set":["lang","lang"]}"#,
            r#"{"columns":["bad name\u0000"],"do":"nothing"}"#,
            r#"{"columns":["id"],"do":"update","set":["bad\u0000"]}"#,
        ] {
            let err = parse_oc(oc).expect_err(oc);
            assert_eq!(err.wire_code(), "42601", "{oc}");
        }
    }

    #[test]
    fn bind_maps_rows_and_resolves_default_set() {
        let rows = items(
            r#"[{"id":1,"embedding":[1,0],"lang":"ja"},{"id":2,"embedding":[0,1],"lang":"en"}]"#,
        );
        let bound = bind(
            &rows,
            "docs",
            None,
            &["id".to_string()],
            &DoKind::Update(None),
            &schema(),
        )
        .expect("bind ok");
        assert_eq!(bound.rows.len(), 2);
        assert_eq!(bound.rows[1].id, 2);
        assert_eq!(
            bound.rows[0].values,
            vec![Value::Vector(vec![1.0, 0.0]), Value::Text("ja".to_string())]
        );
        // 既定の SET は rows のキー − 対象列 − id（embedding・lang の 2 列）。
        match &bound.action {
            engine::sql::parser::BoundConflictAction::DoUpdate { assignments, .. } => {
                assert_eq!(assignments.len(), 2);
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn bind_rejects_mismatched_key_sets_with_22000() {
        let rows = items(r#"[{"id":1,"embedding":[1,0],"lang":"ja"},{"id":2,"embedding":[0,1]}]"#);
        let err = bind(
            &rows,
            "docs",
            None,
            &["id".to_string()],
            &DoKind::Nothing,
            &schema(),
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
        assert!(err
            .client_message()
            .contains("UPSERT rows must all have the same keys"));
    }

    #[test]
    fn bind_rejects_default_set_that_resolves_to_nothing() {
        let rows = items(r#"[{"id":1}]"#);
        let err = bind(
            &rows,
            "docs",
            None,
            &["id".to_string()],
            &DoKind::Update(None),
            &schema(),
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_unknown_conflict_target_is_42p10() {
        let rows = items(r#"[{"id":1,"embedding":[1,0],"lang":"ja"}]"#);
        let err = bind(
            &rows,
            "docs",
            None,
            &["lang".to_string()],
            &DoKind::Nothing,
            &schema(),
        )
        .expect_err("must reject");
        assert_eq!(err.wire_code(), "42P10");
    }

    #[test]
    fn encode_success_body_is_compact_and_escapes_operation_id() {
        let plain = UpsertSuccess {
            upserted: 3,
            operation_id: None,
        };
        assert_eq!(encode_success_body(&plain), r#"{"upserted":3}"#);
        let with_id = UpsertSuccess {
            upserted: 0,
            operation_id: Some(OperationId::parse("op\"1\\x").expect("valid id")),
        };
        assert_eq!(
            encode_success_body(&with_id),
            r#"{"upserted":0,"operation_id":"op\"1\\x"}"#
        );
    }
}
