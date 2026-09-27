//! `POST /v1/query` の `scan` op を `engine::sql::parser::BoundScan` へ束縛し
//! `EngineCore::execute_bound_scan_in_session` で実行する写像本体（Issue #766・
//! TASK-176・対象ビヘイビア NOSQL-3。ポインタ: `docs/spec/05-tasks.md`
//! TASK-176・`docs/spec/04-behavior/nosql-surface.md` NOSQL-3・
//! `docs/spec/04-behavior/sql-surface.md` SQL-15）。
//!
//! 呼び出し文脈: [`super::gate::handle`] が op 許可リスト判定（[`super::op`]）・
//! スキーマ検証（[`super::schema::SCAN_SCHEMA`]）を通過させた
//! [`super::schema::Validated`] を [`handle`] へ渡す。テナント文脈は
//! [`crate::http::session::middleware::SessionPrincipal::policy_context`] の
//! みから導出し、本モジュールはヘッダ・JSON からテナントを読む経路を持たない。
//!
//! 実行意味論は SQL 表層の bare 広域取得形（`SELECT ... [WHERE ...] LIMIT n`。
//! SQL-15・`docs/design/wide-retrieval-scan.md`）と同一——順序保証なし・
//! `limit` 件到達で早期終了・取得モード非適用——であり、第 2 の実行器は作らない
//! （[`engine::core::EngineCore::execute_bound_scan_in_session`] へ束縛済み
//! [`engine::sql::parser::BoundScan`] を渡すのみ。TASK-186・NOSQL-3。
//! `docs/design/bound-plan-session-entry.md` 参照）。
//!
//! `vector`／`plan`／`mode`／`hybrid` の付与は [`super::schema::SCAN_SCHEMA`]
//! が宣言しないフィールドのため、スキーマ検証（[`super::gate::handle`] 手順 4）
//! の未知キー判定で本モジュールへ到達する前に `42601` へ落ちる（個別の除外
//! ロジックをここに持たない）。`explain: true` は `SCAN_SCHEMA` が型としては
//! 受理するが、SQL-15 の bare 形への `EXPLAIN` 前置は `42601`
//! （`docs/design/wide-retrieval-scan.md`）であるため、本モジュールが
//! fail-closed に拒否する（NOSQL-10〔`USING PLAN` の `EXPLAIN`〕とは別の判断。
//! 「spec 側への申し送り」は呼び出し元 PR 本文が担う）。
//!
//! `sort`（Issue #946・NOSQL-15・SQL-25 (a)・TASK-224）を指定すると
//! 決定的な順序（SQL 表層のスカラー `ORDER BY` と同一の並び・NULL 位置・
//! 同点解決規約）になる。省略時は従来どおり順序保証がない（NOSQL-3・
//! SQL-15）。`search` の距離順位付けとの相互排他は [`super::schema::
//! SEARCH_SCHEMA`] が `sort` を宣言しないことで、`aggregate` への `sort` も
//! 同様に [`super::schema::AGGREGATE_SCHEMA`] が宣言しないことで、いずれも
//! スキーマの未知キー一般則（`42601`）だけで成立する。[`build_sort`] が
//! JSON `sort[].dir`（`"asc"`／`"desc"`）の語彙検査・識別子形状検査を行い、
//! [`engine::sql::parser::BoundScan::with_order_by`]（engine と SQL 表層の
//! `ORDER BY` 束縛を共有する入口）へ列名解決・上限判定・型検査（未知列・
//! `VECTOR` 列は `22000`、上限超過は `54000`）を委譲する——第 2 の実行器・
//! 第 2 の並べ替え実装は持たない。
//!
//! 応答は `score` を一切含まない: `Projection::All`（`columns` 省略時）は
//! `schema.columns` の実列と疑似列 `id` のみを列挙し（`bind_projection` の
//! 既存契約）、`hybrid`／スカラー `ORDER BY`（`sort` 指定時を含む）を経由
//! しない広域取得には合成スコア列が構造上存在しない。

use std::time::SystemTime;

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonValue;
use engine::policy::PolicyContext;
use engine::sql::allowlist::{Projection, ScalarOrderKey, SqlSurfaceError, MAX_SCALAR_ORDER_KEYS};
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind_projection, validate_search_limit, BoundScan};
use engine::sql::udf_call::MAX_EXPR_NODES;

use super::filter::{bind_filter, FilterError};
use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated};
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

/// [`bind_request`]／[`execute`] の失敗を表す。いずれも [`ClassifiedError`]
/// を実装し、HTTP エラー応答へ射影される。
#[derive(Debug)]
pub enum ScanError {
    /// [`Validated`] アクセサの型・キー不整合（多層防御。通常は
    /// [`super::schema::SCAN_SCHEMA::validate`] 済みのため到達しない）。
    Shape(SchemaError),
    /// `filter` 配列の写像・束縛エラー（[`super::filter`] 参照）。
    Filter(FilterError),
    /// `limit` が有限の非負整数として `u32` の範囲に収まらない、または
    /// [`validate_search_limit`] の範囲（`1..=`
    /// [`engine::core::MAX_SEARCH_K`]）外（固定文言。SQL-15 と同じ分類 `42601`）。
    InvalidLimit,
    /// `table`／`columns` の要素が識別子形状検査（[`super::ident::
    /// check_identifier`]）を満たさない。`search`／`aggregate` の
    /// `InvalidIdentifier` と同じ判断（cursor[bot] 指摘。SQL レキサーが
    /// 拒否する形状の文字列を engine のスキーマ解決より前に `42601` へ
    /// 落とし、63 文字を超える長大文字列を schema 突き合わせより前に
    /// 打ち切る）。
    InvalidIdentifier,
    /// `columns` が空配列、または要素に空文字列を含む（固定文言。untrusted
    /// な列名文字列そのものは含めない）。
    InvalidColumns,
    /// `explain: true` の指定（SQL-15 の bare 形は `EXPLAIN` 前置を拒否する
    /// 契約と同じ判断を NoSQL 表層側で明示的に適用する）。
    ExplainNotSupported,
    /// `sort` の形が不正（空配列・非オブジェクト要素・`dir` が `"asc"`／
    /// `"desc"` 以外〔大文字混じりを含む〕。Issue #946・NOSQL-15）。固定文言で
    /// untrusted な `sort` の内容（列名・`dir` の生値）を一切含まない
    /// （`InvalidColumns` と同じ判断）。
    InvalidSort,
    /// engine 側の束縛・実行エラー（未知列・`VECTOR` 列・テーブル不存在・
    /// `sort` の上限超過〔`54000`〕・内部エラー等）をそのまま透過する。
    Engine(SqlSurfaceError),
}

impl From<SchemaError> for ScanError {
    fn from(err: SchemaError) -> Self {
        ScanError::Shape(err)
    }
}

impl From<FilterError> for ScanError {
    fn from(err: FilterError) -> Self {
        ScanError::Filter(err)
    }
}

impl From<InvalidIdentifier> for ScanError {
    fn from(_err: InvalidIdentifier) -> Self {
        ScanError::InvalidIdentifier
    }
}

impl From<SqlSurfaceError> for ScanError {
    fn from(err: SqlSurfaceError) -> Self {
        ScanError::Engine(err)
    }
}

impl ClassifiedError for ScanError {
    fn error_class(&self) -> ErrorClass {
        match self {
            ScanError::Shape(err) => err.error_class(),
            ScanError::Filter(err) => err.error_class(),
            ScanError::InvalidLimit
            | ScanError::InvalidIdentifier
            | ScanError::InvalidColumns
            | ScanError::ExplainNotSupported
            | ScanError::InvalidSort => ErrorClass::UnsupportedSqlSyntax,
            ScanError::Engine(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            ScanError::Shape(err) => err.client_message(),
            ScanError::Filter(err) => err.client_message(),
            ScanError::InvalidLimit => {
                "limit must be a positive integer within the supported range".to_string()
            }
            ScanError::InvalidIdentifier => "invalid identifier".to_string(),
            ScanError::InvalidColumns => {
                "columns must be a non-empty array of non-empty column name strings".to_string()
            }
            ScanError::ExplainNotSupported => "explain is not supported for scan".to_string(),
            ScanError::InvalidSort => {
                "sort must be a non-empty array of {column, dir} with dir \"asc\" or \"desc\""
                    .to_string()
            }
            ScanError::Engine(err) => err.client_message(),
        }
    }
}

/// JSON `limit`（`f64`。[`Validated::required_number`] が返す）が非負の
/// 有限整数として `u32` の範囲へ丸めなく収まることを検証する（確保前の
/// untrusted 数値検証。`.claude/rules/security.md` DoS 対応）。範囲は
/// [`validate_search_limit`] がさらに `1..=MAX_SEARCH_K` へ絞り込む。
fn limit_to_u32(raw: f64) -> Result<u32, ScanError> {
    if !raw.is_finite() || raw.fract() != 0.0 || raw < 0.0 || raw > f64::from(u32::MAX) {
        return Err(ScanError::InvalidLimit);
    }
    // 直前の範囲検査で `[0, u32::MAX]` の整数値であることを確定させたため、
    // `as` 変換は値の損失を伴わない（`u32::try_from` と同じ結果になる）。
    Ok(raw as u32)
}

/// `columns`（[`Validated::optional_array`]`("columns")` の結果）を
/// [`Projection`] へ写像する。未指定は `Projection::All`（`id` 疑似列＋
/// 実列を宣言順で列挙。`score` に相当する合成列は構造上存在しない）。
fn build_projection(validated: &Validated<'_>) -> Result<Projection, ScanError> {
    let Some(items) = validated.optional_array("columns")? else {
        return Ok(Projection::All);
    };
    if items.is_empty() {
        return Err(ScanError::InvalidColumns);
    }
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        // `SCAN_SCHEMA` が `Array(ElementType::String)` として型検査済みだが、
        // 本モジュール単体で呼ばれても安全なよう再検証する（多層防御。
        // `filter.rs::map_filter_item` と同じ方針）。
        let JsonValue::String(s) = item else {
            return Err(ScanError::InvalidColumns);
        };
        // `search`／`aggregate` と同じ識別子形状検査（SQL レキサーが `Ident`
        // として読む文字集合・63 文字上限）を先に通す（cursor[bot] 指摘）。
        ident::check_identifier(s)?;
        names.push(s.clone());
    }
    Ok(Projection::Columns(names))
}

/// `sort`（[`Validated::optional_array`]`("sort")` の結果）を
/// [`ScalarOrderKey`] 列へ写像する（Issue #946・NOSQL-15・SQL-25 (a)・
/// TASK-224）。未指定は空 `Vec`（`BoundScan::with_order_by` の no-op 契約と
/// 対応し、順序保証なしの既存挙動を保つ）。空配列は明示的な指定として
/// `InvalidSort` にする（省略と空配列を区別し、空配列を「意味のない
/// 指定」として拒否する——`columns` の空配列拒否と同じ判断）。
///
/// 件数上限（[`MAX_SCALAR_ORDER_KEYS`]）の `54000` 判定はここでは行わない
/// （engine 側の [`BoundScan::with_order_by`] に一元化し二重実装しない）。
/// ただし確保前に `MAX_SCALAR_ORDER_KEYS + 1` 件で打ち切ることで、untrusted
/// な要素数（JSON 配列は `engine::json::MAX_JSON_STRING_CHARS` の範囲内で
/// 任意個の要素を持ちうる）に応じた無制限な `Vec` 確保を避ける
/// （`.claude/rules/security.md` DoS 対応。`54000` の判定に十分な
/// `MAX_SCALAR_ORDER_KEYS + 1` 件が engine 側へ届けば足りる）。
fn build_sort(validated: &Validated<'_>) -> Result<Vec<ScalarOrderKey>, ScanError> {
    let Some(items) = validated.optional_array("sort")? else {
        return Ok(Vec::new());
    };
    if items.is_empty() {
        return Err(ScanError::InvalidSort);
    }
    let capacity = items.len().min(MAX_SCALAR_ORDER_KEYS + 1);
    let mut keys = Vec::with_capacity(capacity);
    for item in items {
        if keys.len() > MAX_SCALAR_ORDER_KEYS {
            // engine 側の `with_order_by` が `54000` を判定できるだけの
            // 件数（上限 + 1）を渡せば足りるため、ここで打ち切る
            // （確保量を上限付きに保ったまま、超過件数そのものは伝える）。
            break;
        }
        // `SORT_ITEM_SCHEMA` が `Array(ElementType::Object(&SORT_ITEM_SCHEMA))`
        // として型検査済みだが、本モジュール単体で呼ばれても安全なよう
        // 再検証する（多層防御。`build_projection` と同じ方針）。
        let JsonValue::Object(obj) = item else {
            return Err(ScanError::InvalidSort);
        };
        let Some(JsonValue::String(column)) = obj.get("column") else {
            return Err(ScanError::InvalidSort);
        };
        ident::check_identifier(column)?;
        let descending = match obj.get("dir") {
            Some(JsonValue::String(dir)) if dir == "asc" => false,
            Some(JsonValue::String(dir)) if dir == "desc" => true,
            _ => return Err(ScanError::InvalidSort),
        };
        keys.push(ScalarOrderKey {
            column: column.clone(),
            descending,
        });
    }
    Ok(keys)
}

/// [`execute`] の本体。`table` のスキーマ取得・束縛・実行を単一スナップショット
/// 上で行う（[`EngineCore::execute_bound_scan_in_session`] の契約）。
///
/// 手順: (1) `explain: true` の拒否、(2) `table`／`limit`／`columns`／`sort`
/// をスキーマに依存しない範囲で検証・写像（[`limit_to_u32`]・
/// [`validate_search_limit`]・[`build_projection`]・[`build_sort`]。いずれも
/// `TableSchema` を必要としないため、テーブル解決より前に完結させる——
/// 未知テーブルへの要求でも `limit`／`columns`／`sort` の構文エラーを先に
/// 確定させて構わない。SQL 表層の許可リスト検証段と同じ判定順序の思想）、
/// (3) [`EngineCore::execute_bound_scan_in_session`] の bind closure 内で
/// [`bind_projection`]・[`super::filter::bind_filter`]（いずれもスキーマ依存の
/// 検証。`filter` は列型ごとに値レーンを振り分けるため `schema` が届くまで
/// 束縛できない——Issue #896・NOSQL-17 で `search`／`aggregate` と同じ
/// 「schema 到達後に単一段で束縛する」構成へ揃えた。未知列・`VECTOR` 列は
/// `22000`）を適用して [`BoundScan::new`] を組み立て、続けて
/// [`engine::sql::parser::BoundScan::with_order_by`]（Issue #946・NOSQL-15）で
/// `sort` を束縛（未知列・`VECTOR` 列は `22000`、上限超過は `54000`）し、
/// (4) 実行する。
pub fn execute(
    core: &EngineCore,
    ctx: &PolicyContext,
    validated: &Validated<'_>,
) -> Result<QueryResult, ScanError> {
    if validated.optional_bool("explain")?.unwrap_or(false) {
        return Err(ScanError::ExplainNotSupported);
    }

    let table = validated.required_str("table")?;
    // `search`／`aggregate` と同じ識別子形状検査を engine のスキーマ解決
    // （`resolve_scan_input`）より前に適用する（cursor[bot] 指摘。SQL レキサー
    // が拒否する形状の `table` を `42P01`／`22000` ではなく `42601` へ揃え、
    // 63 文字を超える長大文字列を schema 走査より前に打ち切る）。
    ident::check_identifier(table)?;
    let raw_limit = validated.required_number("limit")?;
    let limit = validate_search_limit(limit_to_u32(raw_limit)?)?;
    let projection = build_projection(validated)?;
    let filter_items = validated.optional_array("filter")?.unwrap_or(&[]);
    let sort_keys = build_sort(validated)?;

    let session = SessionState::default();
    let result = core.execute_bound_scan_in_session(ctx, &session, table, |schema, udfs| {
        let mut node_budget = MAX_EXPR_NODES;
        let bound_projection = bind_projection(&projection, schema, udfs, &mut node_budget)?;
        let bound_filters =
            bind_filter(filter_items, schema).map_err(FilterError::into_sql_surface_error)?;
        BoundScan::new(
            table.to_string(),
            bound_projection,
            bound_filters,
            Vec::new(),
            limit,
        )
        .with_order_by(&sort_keys, schema)
    })?;
    Ok(result)
}

/// `POST /v1/query`（`op: "scan"`）を処理し応答バイト列を返す（[`super::gate`]
/// から呼ばれる。認証・スキーマ検証済みの要求のみ）。
pub fn handle(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: SystemTime,
) -> Vec<u8> {
    match execute(core, principal.policy_context(), validated) {
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

    #[test]
    fn limit_to_u32_accepts_boundary_integers() {
        assert_eq!(limit_to_u32(0.0).unwrap(), 0);
        assert_eq!(limit_to_u32(1.0).unwrap(), 1);
        assert_eq!(limit_to_u32(10_000.0).unwrap(), 10_000);
        assert_eq!(limit_to_u32(f64::from(u32::MAX)).unwrap(), u32::MAX);
    }

    #[test]
    fn limit_to_u32_rejects_non_integers_and_out_of_range_values() {
        for raw in [
            1.5,
            -1.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from(u32::MAX) + 1.0,
        ] {
            assert!(
                matches!(limit_to_u32(raw), Err(ScanError::InvalidLimit)),
                "raw={raw}"
            );
        }
    }

    #[test]
    fn scan_error_wire_codes_match_expected_classes() {
        assert_eq!(ScanError::InvalidLimit.wire_code(), "42601");
        assert_eq!(ScanError::InvalidIdentifier.wire_code(), "42601");
        assert_eq!(ScanError::InvalidColumns.wire_code(), "42601");
        assert_eq!(ScanError::ExplainNotSupported.wire_code(), "42601");
    }

    /// `ScanError::InvalidIdentifier` の応答文言は固定文言であり、
    /// untrusted な識別子文字列をそのまま含まない（`ident::
    /// check_identifier` の非漏えい契約を `ScanError` 側でも維持する）。
    #[test]
    fn invalid_identifier_client_message_is_fixed_and_does_not_leak_input() {
        assert_eq!(
            ScanError::InvalidIdentifier.client_message(),
            "invalid identifier"
        );
    }

    // --- `build_sort`（Issue #946・NOSQL-15・SQL-25 (a)・TASK-224） -------------

    /// `body`（`scan` op の JSON オブジェクトテキスト）を [`super::super::schema::
    /// SCAN_SCHEMA`] で検証した [`Validated`] を得る（`build_sort` 単体テスト用の
    /// 共通ヘルパー。本番経路〔`super::gate::handle`〕と同じ検証段を通す）。
    fn validated_scan_body(body: &str) -> engine::json::JsonValue {
        engine::json::parse_json(body).expect("valid JSON")
    }

    #[test]
    fn build_sort_accepts_asc_and_desc_and_multiple_keys() {
        let value = validated_scan_body(
            r#"{"op":"scan","table":"docs","limit":10,
                "sort":[{"column":"lang","dir":"desc"},{"column":"id","dir":"asc"}]}"#,
        );
        let validated = super::super::schema::SCAN_SCHEMA
            .validate(&value)
            .expect("schema validate ok");
        let keys = build_sort(&validated).expect("build_sort ok");
        assert_eq!(
            keys,
            vec![
                ScalarOrderKey {
                    column: "lang".to_string(),
                    descending: true,
                },
                ScalarOrderKey {
                    column: "id".to_string(),
                    descending: false,
                },
            ]
        );
    }

    #[test]
    fn build_sort_omitted_yields_empty_vec() {
        let value = validated_scan_body(r#"{"op":"scan","table":"docs","limit":10}"#);
        let validated = super::super::schema::SCAN_SCHEMA
            .validate(&value)
            .expect("schema validate ok");
        assert_eq!(build_sort(&validated).expect("build_sort ok"), Vec::new());
    }

    #[test]
    fn build_sort_rejects_empty_array_and_vocabulary_violations() {
        // `column`／`dir` 欠落は `SORT_ITEM_SCHEMA` が `Required` として宣言する
        // ため `build_sort` へ到達する前に `ObjectSchema::validate` 自体が
        // `SchemaError::MissingRequired`（`42601`）で拒否する（別テスト
        // `super::super::schema::tests` の担当）。本テストは schema 検証を
        // 通過した形（`column`／`dir` は文字列として存在する）が `build_sort`
        // 自身の語彙・空配列検査で `InvalidSort` になることを固定する。
        let cases = [
            r#"{"op":"scan","table":"docs","limit":10,"sort":[]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"sort":[{"column":"lang","dir":"ASC"}]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"sort":[{"column":"lang","dir":"ascending"}]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"sort":[{"column":"lang","dir":""}]}"#,
        ];
        for body in cases {
            let value = validated_scan_body(body);
            let validated = super::super::schema::SCAN_SCHEMA
                .validate(&value)
                .expect("schema validate ok");
            assert!(
                matches!(build_sort(&validated), Err(ScanError::InvalidSort)),
                "body={body}"
            );
        }
    }

    #[test]
    fn build_sort_missing_column_or_dir_is_rejected_at_schema_layer() {
        for body in [
            r#"{"op":"scan","table":"docs","limit":10,"sort":[{"column":"lang"}]}"#,
            r#"{"op":"scan","table":"docs","limit":10,"sort":[{"dir":"asc"}]}"#,
        ] {
            let value = validated_scan_body(body);
            assert!(
                super::super::schema::SCAN_SCHEMA.validate(&value).is_err(),
                "body={body}"
            );
        }
    }

    #[test]
    fn build_sort_rejects_invalid_identifier_column() {
        let value = validated_scan_body(
            r#"{"op":"scan","table":"docs","limit":10,
                "sort":[{"column":"1bad","dir":"asc"}]}"#,
        );
        let validated = super::super::schema::SCAN_SCHEMA
            .validate(&value)
            .expect("schema validate ok");
        assert!(matches!(
            build_sort(&validated),
            Err(ScanError::InvalidIdentifier)
        ));
    }

    /// `InvalidSort` の応答文言は固定文言であり、untrusted な `sort` の内容
    /// （列名・`dir` の生値）を一切含まない。
    #[test]
    fn invalid_sort_client_message_is_fixed_and_does_not_leak_input() {
        assert_eq!(
            ScanError::InvalidSort.client_message(),
            "sort must be a non-empty array of {column, dir} with dir \"asc\" or \"desc\""
        );
        assert_eq!(ScanError::InvalidSort.wire_code(), "42601");
    }
}
