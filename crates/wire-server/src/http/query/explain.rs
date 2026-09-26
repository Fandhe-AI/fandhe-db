//! `POST /v1/query`（`op: search`・`explain: true`）を検索本体を一切実行せず
//! SQL 表層 `EXPLAIN SELECT ... USING PLAN(...)` と同一内容の `QUERY PLAN`
//! 行へ写像するモジュール（Issue #765・TASK-186・NOSQL-10。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・TASK-186・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-10・
//! `docs/spec/04-behavior/sql-surface.md` SQL-6）。
//!
//! 責務境界: [`super::gate::handle`] が `op: search`・`explain: true` の要求を
//! 通常の `search` 実行（`search::execute`）より前に [`handle`] へ委譲する
//! （`explain: true` は構造的に実行経路へ落ちない。`aggregate.rs` の
//! `explain: true` 拒否と同じ fail-open 防止の思想）。SQL テキストは一切
//! 組み立てず、[`super::search::bind_search`]（Issue #763）が組み立てる
//! [`super::search::PlanSearch`] の投影・フィルタ・`limit`・原質問文字列を
//! [`engine::core::EngineCore::explain_bound_plan_in_session`]（Issue #765・
//! `core.rs::run_explain_plan`）が SQL `Statement::Explain` アームと共有する
//! 私的ヘルパーへそのまま渡す（第 2 の実行器を作らない方針）。
//!
//! `vector` 指定・`plan` 未指定はいずれも SQL-6 の「`USING PLAN` 専用」契約の
//! 写像として [`ExplainError::ExplainRequiresPlan`]（`42601`）で拒否する
//! （`sql::allowlist` が `ORDER BY` 形・集計・広域取得への `EXPLAIN` 前置を
//! 拒否するのと同じ分類）。
//!
//! `table` の識別子形状・`USING PLAN` 質問文字列の長さ検証は
//! [`engine::core::EngineCore::explain_bound_plan_in_session`] を呼ぶより前に
//! ここで一度行う。`mode` の識別子形状・語彙検証はここでは一切行わず、
//! テーブル解決後の binder closure（[`super::search::bind_search`]）へ委ねる
//! （未知テーブル〔`42P01`〕・`plan` 欠落〔`42601`〕が `mode` 値不正
//! 〔`22000`〕より優先される順序を、通常検索（[`super::search::execute`]）
//! と揃えるため。codex-review P1 指摘対応・PR #828。
//! `EngineCore::explain_bound_plan_in_session` のドキュメント参照）。
//!
//! RLS: `PolicyContext` は呼び出し元が渡す
//! [`crate::http::session::middleware::SessionPrincipal::policy_context`]
//! のみから導出する（本モジュールはヘッダ・JSON からテナントを読む経路を
//! シグネチャ上持たない）。

use std::time::SystemTime;

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::policy::PolicyContext;
use engine::sql::allowlist::{validate_using_plan_question, SqlSurfaceError};
use engine::sql::exec::QueryResult;
use engine::sql::explain::ExplainShape;
use engine::sql::mode::SessionState;
use engine::sql::parser::validate_search_limit;

use super::ident::{self, InvalidIdentifier};
use super::response::{self, ResponseEncodeError};
use super::schema::{SchemaError, Validated};
use super::search::{bind_search, BoundSearch, SearchError};
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

/// [`execute`]／[`handle`] の失敗を表す。いずれも [`ClassifiedError`] を実装し
/// `wire_code`／`client_message` を経由して HTTP エラー応答へ射影される。
#[derive(Debug)]
pub enum ExplainError {
    /// [`super::search::bind_search`]（[`Self::Search`] を経由するエラー）と
    /// 同一の分類・文言をそのまま透過する（`table`／`mode`／`columns`／
    /// `filter` の識別子形状・型不整合等）。
    Search(SearchError),
    /// `vector` 指定、または `plan` 未指定（SQL-6 の「`USING PLAN` 専用」
    /// 契約の写像。`42601`）。
    ExplainRequiresPlan,
    /// [`engine::core::EngineCore::explain_bound_plan_in_session`] の実行時
    /// エラー（プランナー未注入・世代競合・辞書必須列不備等）をそのまま透過
    /// する。
    Engine(SqlSurfaceError),
    /// [`super::response::encode_explain`] が `QUERY PLAN` 応答の形状不変
    /// 条件を検出できなかった（到達しないはずの防御的経路）。
    Encode(ResponseEncodeError),
}

impl From<SearchError> for ExplainError {
    fn from(err: SearchError) -> Self {
        ExplainError::Search(err)
    }
}

impl From<SchemaError> for ExplainError {
    fn from(err: SchemaError) -> Self {
        ExplainError::Search(SearchError::from(err))
    }
}

impl From<InvalidIdentifier> for ExplainError {
    fn from(err: InvalidIdentifier) -> Self {
        ExplainError::Search(SearchError::from(err))
    }
}

impl From<SqlSurfaceError> for ExplainError {
    fn from(err: SqlSurfaceError) -> Self {
        ExplainError::Engine(err)
    }
}

impl From<ResponseEncodeError> for ExplainError {
    fn from(err: ResponseEncodeError) -> Self {
        ExplainError::Encode(err)
    }
}

impl ClassifiedError for ExplainError {
    fn error_class(&self) -> ErrorClass {
        match self {
            ExplainError::Search(err) => err.error_class(),
            ExplainError::ExplainRequiresPlan => ErrorClass::UnsupportedSqlSyntax,
            ExplainError::Engine(err) => err.error_class(),
            ExplainError::Encode(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            ExplainError::Search(err) => err.client_message(),
            ExplainError::ExplainRequiresPlan => {
                "explain is only supported for search requests with \"plan\"".to_string()
            }
            ExplainError::Engine(err) => err.client_message(),
            ExplainError::Encode(err) => err.client_message(),
        }
    }
}

/// `validated`（[`super::schema::SEARCH_SCHEMA`] 検証済みの `search` 要求。
/// `explain: true` 判定は呼び出し元 [`super::gate::handle`] が行う）から
/// `EXPLAIN` 応答を組み立てる。
///
/// 処理順序（fail-closed。順序自体が契約の一部。[`super::search::execute`]・
/// SQL `EXPLAIN` 経路〔`core.rs::run_explain_plan`〕と同一の優先順位に
/// 揃える。codex-review P1 指摘対応・PR #828）:
/// 1. `table`（識別子形状検査）を読み取る。
/// 2. `vector` 指定を拒否する（`42601`。`plan` の有無によらずテーブル解決を
///    要さない構造的な契約違反のため最優先。`vector`／`plan` 同時指定も
///    この分岐で拒否される）。
/// 3. `plan` が指定されている場合のみ、[`validate_using_plan_question`] で
///    長さ上限を検証する（`54000`。[`super::search::execute`] の `plan`
///    分岐と同じくテーブル解決より前に行う）。`plan` 未指定（`None`）は
///    ここでは拒否せず手順 6 へ委ねる。
/// 4. `limit` の型・範囲を [`validate_search_limit`] で検証する（`22000`。
///    SQL `EXPLAIN` 経路が `run_explain_plan` 呼び出し前に同じ検証を行うのと
///    同一の優先順位でテーブル解決より前に行う）。
/// 5. `mode` は識別子形状検査を含め一切ここでは検証しない（生リテラルの
///    まま手順 6 へ渡す）。
/// 6. [`engine::core::EngineCore::explain_bound_plan_in_session`] を呼ぶ
///    （`plan` 未指定の場合もプレースホルダの空文字列を渡して呼び出し自体は
///    行う。テーブル解決が先に走り、テーブルが存在すれば binder closure が
///    テーブル解決後に初めて `plan` 欠落を判定するため、未知テーブルの
///    `42P01` が `plan` 欠落の `42601` より優先される。binder closure は
///    [`bind_search`] の完全な束縛結果から [`BoundSearch::Plan`] のフィルタ
///    のみを取り出す——`bind_search` 自身がテーブル解決後に初めて `mode`
///    の識別子形状検査・語彙解析（`SearchMode::parse_literal`）の双方を
///    行うため、未知テーブル（`42P01`）・`plan` 欠落（`42601`）のいずれも
///    `mode` 値不正（`22000`）より優先される（`BoundSearch::Vector` への
///    到達は手順 2 により構造上ないが、多層防御として [`ExplainError::
///    ExplainRequiresPlan`] へ拒否する）。`run_explain_plan` はこの
///    binder closure（`bind`）が返す `explain_shape` を得たあとに初めて
///    `mode_literal` を独立に再解析して `SearchMode` へ変換するが、その
///    時点では `bind_search` の検証を既に通過しているため、この再解析が
///    未検証の生リテラルを直接処理することはない）。
pub fn execute(
    core: &EngineCore,
    ctx: &PolicyContext,
    validated: &Validated<'_>,
) -> Result<QueryResult, ExplainError> {
    let table = validated.required_str("table")?;
    ident::check_identifier(table)?;

    if validated.optional_array("vector")?.is_some() {
        return Err(ExplainError::ExplainRequiresPlan);
    }

    // `plan` が指定されている場合のみ長さ上限を検証する。未指定はここでは
    // 拒否しない（手順 6 のコメント参照。未知テーブルの `42P01` を `plan`
    // 欠落の `42601` より優先させるため）。
    let question = validated.optional_str("plan")?;
    if let Some(q) = question {
        validate_using_plan_question(q)?;
    }

    let limit_raw = validated.required_u32("limit")?;
    validate_search_limit(limit_raw)?;

    // `mode` は識別子形状検査を含め一切ここでは検証しない（codex-review
    // P1 指摘対応・PR #828）。ここで形状検査だけでも先に行うと、テーブル
    // 未存在＋ `mode` 形状不正の要求で `42P01`（テーブル未存在）より先に
    // `42601`（識別子形状違反）が確定してしまい、通常検索（`search::
    // bind_search` は識別子形状検査もテーブル解決後の binder closure 内で
    // 行う）と優先順位が食い違う。生リテラルのまま
    // [`engine::core::EngineCore::explain_bound_plan_in_session`]
    // （`core.rs::run_explain_plan`）へ渡し、テーブル解決・`bind` 呼び出し
    // （`plan` 欠落判定を含む）を終えた後で初めて識別子形状検査・語彙解析
    // （`SearchMode::parse_literal`）の双方を行わせる。
    let mode_literal = validated.optional_str("mode")?;

    let session = SessionState::default();
    let result = core.explain_bound_plan_in_session(
        ctx,
        &session,
        table,
        question.unwrap_or(""),
        mode_literal,
        |schema, _udfs| -> Result<ExplainShape, ExplainError> {
            // テーブル解決後に初めて `plan` 欠落を判定する（codex-review P1
            // 指摘対応・PR #828。`super::search::execute` が `vector`／`plan`
            // 両方欠落の判定を `bind_search` 自身のテーブル解決後へ委ねるのと
            // 同じ設計）。
            question.ok_or(ExplainError::ExplainRequiresPlan)?;
            match bind_search(validated, schema)? {
                BoundSearch::Plan(plan) => {
                    // NoSQL 表層の `plan` 検索経路は `OR` 未対応（TASK-208・
                    // Issue #912。計画§「対象外」参照）。
                    Ok(ExplainShape::from_filters(
                        plan.metadata_filters(),
                        &[],
                        &[],
                    ))
                }
                BoundSearch::Vector(_) => Err(ExplainError::ExplainRequiresPlan),
            }
        },
    )?;
    Ok(result)
}

/// `POST /v1/query`（`op: search`・`explain: true`）を処理し応答バイト列を
/// 返す（[`super::gate`] から呼ばれる。認証・スキーマ検証済みの要求のみ）。
pub fn handle(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: SystemTime,
) -> Vec<u8> {
    match execute(core, principal.policy_context(), validated) {
        Ok(result) => match response::encode_explain(&result) {
            Ok(body) => http_response::encode_ok(&body, now_wall),
            Err(err) => {
                http_response::encode_error(err.error_class(), &err.client_message(), now_wall)
            }
        },
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}
