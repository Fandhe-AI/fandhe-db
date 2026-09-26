//! `USING PLAN('<query>')` 文末句（`ORDER BY` の代替。TASK-77、対象ビヘイビア:
//! SQL-5。ポインタ: `docs/spec/05-tasks.md` TASK-77・
//! `docs/spec/04-behavior/sql-surface.md` SQL-5）の実行時ディスパッチ。
//!
//! 責務境界: `sql::allowlist` が構造受理した自然言語クエリ
//! （[`ValidatedStatement::using_plan`]）を、既存の LLM クエリ展開ロジック
//! （`core.rs::EngineCore::plan_query`、TASK-110・PLAN-1）が返した
//! [`QueryExpansion`] から、既存 C4 ハイブリッド実行形（`sql::parser::Ranking::
//! Hybrid`）を持つ [`BoundStatement`] へ**一意に**束縛するところまでを担う。
//!
//! `EngineCore` の `embedder`／`query_planner` フィールドは private（クレート内
//! 他モジュールからも不可視）のため、LLM 呼び出し・再埋め込みの実行自体は
//! 呼び出し元（`core.rs::EngineCore::execute_sql_in_session`）が行い、本モジュールへは
//! 展開結果 [`QueryExpansion`] と埋め込み済みクエリベクトルを渡す（本モジュールは
//! 束縛の純粋なロジックのみを持つ）。束縛後の実行（RLS 暗黙適用・`USING MODE`/
//! `SET search_mode` のモード解決）は既存の `sql::exec::execute_statement` を
//! そのまま再利用し、呼び出し元がそちらへ委譲する（本モジュールは複製しない）。
//!
//! fail-closed: 本文列（規約列 [`BODY_COLUMN_NAME`]。増分インデックス
//! （TASK-120）のファイル形 `INSERT` の `path`/`body` 列規約と整合）の欠落・型
//! 不一致は [`SqlSurfaceError::invalid_input`]（`22000`）で拒否する。
//! プランナー未注入・埋め込み未注入・LLM 応答異常は呼び出し元（`core.rs`）が
//! 既存分類（`XX000`・`SqlSurfaceError::Internal`）のみで拒否し、新規分類は
//! 追加しない（ERR-2、TASK-152 の単一真実源を保つ）。エラーへプロンプト本文・
//! LLM 応答本文を含めない（`query_planner.rs` の P0 方針を維持。`PlanError`/
//! `EmbedError` の `Display` はいずれも固定文言のみを持つため、そのまま
//! `SqlSurfaceError::Internal` の detail に使っても入力・応答本文は含まれない）。

use crate::catalog::{ColumnType, TableSchema};
use crate::query_planner::QueryExpansion;
use crate::sql::allowlist::{SqlSurfaceError, ValidatedStatement};
use crate::sql::parser::{self, BoundStatement, Ranking};

/// `USING PLAN` の展開後クエリが疎側（`hybrid_search` の全文検索側入力。
/// [`Ranking::Hybrid::text_column_index`]）の一致判定に使う本文列の規約名。
/// 密側（再埋め込みベクトル）は `VECTOR` 列と比較するためこの列を参照しない
/// （[`expanded_query_text`] のドキュメント参照）。増分インデックス（TASK-120）の
/// ファイル形 `INSERT`（`path`/`body` 列指定）と揃える。
pub(crate) const BODY_COLUMN_NAME: &str = "body";

/// `question`（クエリ句のリテラル値）と `expansion`（LLM 展開結果）から、
/// `Ranking::Hybrid::query_text`（`hybrid_search` の疎側〔全文検索側〕入力。
/// `sql::exec` が `sparse::SparseIndex::search`/`search_within` へそのまま渡す）
/// に使う単一のテキストを決定的に構成する。
///
/// 密側（再埋め込みベクトル）には本関数の戻り値を使わない
/// （codex-review P1 指摘対応、PR #266）。密側は
/// [`crate::query_planner::render_reembedding_text`] が課す既存の再埋め込み規則
/// （固定接頭辞・検索語の防御的上限）に従う必要があり、`expanded_query_text` の
/// 単純結合（接頭辞なし・検索語件数の上限なし）とは意図的に異なる文字列になる。
/// 疎側は全文検索の一致判定に使うだけであり、同じ防御的上限を課す必要はないため、
/// 単純結合のままでよい（呼び出し元 `core.rs::EngineCore::
/// plan_using_plan_expansion` は疎側テキストにも `sparse::validate_query_bounds`
/// による長さ上限検証を別途課している）。
///
/// `question` は [`crate::query_planner::sanitize_question`] で `plan_query` が
/// LLM プロンプトへ組み込んだのと**同一の切り詰め結果**へ正規化してから使う
/// （呼び出し元がここで別の切り詰め規則を使うと、LLM が実際に見た質問テキストと
/// 検索に使う質問テキストが食い違い、`54000`〔クエリ句の生バイト長上限〕→
/// `MAX_QUESTION_CHARS`〔意味論側の決定的切り詰め〕→検索語件数上限、という
/// 有界化の連鎖が検索テキスト側で途切れてしまう）。
pub(crate) fn expanded_query_text(question: &str, expansion: &QueryExpansion) -> String {
    let sanitized = crate::query_planner::sanitize_question(question);
    let mut parts: Vec<String> = Vec::with_capacity(1 + expansion.search_terms.len());
    parts.push(sanitized);
    parts.extend(expansion.search_terms.iter().cloned());
    parts.join(" ")
}

/// `schema` 内の規約列 [`BODY_COLUMN_NAME`]（`TEXT`）のインデックスを返す。
/// 欠落・型不一致は [`SqlSurfaceError::invalid_input`]（`22000`）。
///
/// `pub(crate)`（Issue #764・TASK-186・NOSQL-2）: `core.rs::EngineCore::
/// execute_bound_plan_search_in_session` が束縛済み `plan` 検索
/// （NoSQL 表層）の `text_column_index` 解決に本関数をそのまま再利用する
/// （[`bind_expansion`] と同一の本文列規約を共有し、第 2 の実装を作らない）。
pub(crate) fn body_column_index(schema: &TableSchema) -> Result<usize, SqlSurfaceError> {
    let idx = schema
        .columns
        .iter()
        .position(|c| c.name == BODY_COLUMN_NAME)
        .ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!(
                "USING PLAN requires a {BODY_COLUMN_NAME:?} TEXT column"
            ))
        })?;
    let column = schema.columns.get(idx).ok_or_else(|| {
        SqlSurfaceError::invalid_input(format!(
            "USING PLAN requires a {BODY_COLUMN_NAME:?} TEXT column"
        ))
    })?;
    match &column.ty {
        ColumnType::Text => Ok(idx),
        // F10（Issue #882 計画）: `body` 列に REAL/DOUBLE が宣言されるのは
        // 通常あり得ないが、VECTOR 列と同じ「TEXT 列でない」拒否へ合流させる
        // （BOOLEAN も同様。Issue #883）。
        ColumnType::Vector(_)
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Boolean
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Array(_)
        | ColumnType::Bytea
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Enum(_)
        | ColumnType::Numeric { .. }
        | ColumnType::Uuid => Err(SqlSurfaceError::invalid_input(format!(
            "column {BODY_COLUMN_NAME:?} is not a TEXT column"
        ))),
    }
}

/// `plan_using_plan_expansion`（辞書スナップショット構築・LLM クエリ展開・
/// 再埋め込みという高コスト I/O）より**前**に、`stmt` がスキーマ `schema` 上で
/// 構造的に束縛可能であることを検証する（codex-review P1 指摘対応、PR #266）。
///
/// 従来 `USING MODE` リテラルの解析・`VECTOR` 列の存在・投影列（`SELECT`
/// リスト）／`WHERE` 述語の束縛は、いずれも I/O 完了後の [`bind_expansion`]
/// 内でのみ行われていた。`USING MODE 'invalid'` や未知列参照のように構文上は
/// 受理されるが必ず拒否される入力でも、拒否より先に LLM 呼び出し・再埋め込みが
/// 実行されてしまい、untrusted クエリによるリソース増幅になっていた
/// （`LIMIT` 範囲検証・辞書用 `path`/`body` 列検証を I/O 前へ前倒しした既存対応
/// と同種の指摘）。
///
/// 束縛結果自体は破棄する（呼び出し元 `core.rs::EngineCore::
/// execute_sql_in_session` は I/O 完了後に最新スナップショットを取得し直し、
/// [`bind_expansion`] で改めて束縛する。I/O 中の DDL によるスキーマ食い違いを
/// 避ける既存方針を維持するため、ここでの結果を実行に使い回さない）。
///
/// fail-closed: 各検証はいずれも既存の関数（[`crate::sql::mode::SearchMode::
/// parse_literal`]・[`parser::vector_column`]・[`parser::bind_projection`]・
/// [`parser::bind_where_predicates`]）をそのまま呼ぶだけで、エラーの
/// `wire_code`・メッセージは変えない（多層防御: I/O 後の [`bind_expansion`] も
/// 同一スキーマ上で再度同じ検証を行う）。
pub(crate) fn pre_check_bindable(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<crate::sql::explain::ExplainShape, SqlSurfaceError> {
    if let Some(literal) = stmt.search_mode() {
        crate::sql::mode::SearchMode::parse_literal(literal)?;
    }
    parser::vector_column(schema)?;

    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    parser::bind_projection(stmt.projection(), schema, udfs, &mut node_budget)?;
    let (metadata_filters, expr_filters, _rls_predicate_present, or_filters) =
        parser::bind_where_predicates(
            stmt.where_predicates(),
            schema,
            udfs,
            &mut node_budget,
            &[],
        )?;
    // Issue #474・#765: `EXPLAIN` の `scalar_plan:` 行（`sql::explain`）が
    // 要求する静的判定は [`crate::sql::explain::ExplainShape::from_filters`]
    // （`scalar_prefilter: true` 固定。`USING PLAN` は `HINT ORDER` を受理
    // しないため SCALAR 段は常に DISTANCE 段より先に評価される）へ委譲する。
    Ok(crate::sql::explain::ExplainShape::from_filters(
        &metadata_filters,
        &expr_filters,
        &or_filters,
    ))
}

/// `stmt`（`using_plan()` が `Some` である前提）・展開結果 `expansion`・埋め込み
/// 済みの再埋め込みクエリベクトル `query_vector` を `schema` へ束縛し、既存 C4
/// ハイブリッド実行形（[`Ranking::Hybrid`]）を持つ [`BoundStatement`] を構成する
/// （TASK-77・SQL-5 の一意ディスパッチ先）。
///
/// `SELECT` リスト・`WHERE` 述語の束縛規則は既存の検索 `SELECT` 経路
/// （`sql::parser::bind_in_session`）と完全に同一の共通ヘルパー
/// （[`parser::bind_projection`]/[`parser::bind_where_predicates`]）を再利用する
/// （挙動を複製しない）。`resolved_mode`（`USING MODE`／`SET search_mode` の優先順位
/// 解決結果）は呼び出し元がそのまま渡す。
///
/// `expansion.path_hint`／`expansion.kind_hint`（TASK-110・PLAN-1）は本メソッドでは
/// 意図的に読まない。ソフトブースト（`hybrid::apply_soft_boost`、TASK-111）を
/// `sql::exec` の融合段へ接続する結線は本タスク（TASK-77）のスコープ外（後続タスク
/// の管轄）。
pub(crate) fn bind_expansion(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
    question: &str,
    expansion: &QueryExpansion,
    query_vector: Vec<f32>,
    udfs: &crate::sql::udf_call::UdfRegistry,
    resolved_mode: crate::sql::mode::ResolvedMode,
) -> Result<BoundStatement, SqlSurfaceError> {
    let text_column_index = body_column_index(schema)?;

    // `Embedder::dim` はテーブルの `VECTOR(N)` と突き合わせて検証する契約
    // （`embedding.rs` モジュールドキュメント「呼び出し元は対象テーブルの
    // `VECTOR(N)` と突き合わせて次元不一致を検出する」）。既存の `ORDER BY`
    // 経路（`sql::parser::parse_vector_literal`）が全てのベクトルリテラルへ
    // 課している検証と同じ不変条件を、埋め込み由来のベクトルにも課す
    // （fail-closed。次元不一致のベクトルを検索カーネルへ黙って渡さない）。
    let (_, vec_dim) = parser::vector_column(schema)?;
    let got_dim = u32::try_from(query_vector.len()).map_err(|_| {
        SqlSurfaceError::invalid_input(format!(
            "USING PLAN re-embedded vector length {} exceeds representable range",
            query_vector.len()
        ))
    })?;
    if got_dim != vec_dim {
        return Err(SqlSurfaceError::invalid_input(format!(
            "USING PLAN re-embedded vector dimension mismatch: expected {vec_dim}, got {got_dim}"
        )));
    }

    // 次元検証と同じ位置で非有限（NaN・Inf）値も拒否する（codex-review P1 指摘
    // 対応。次元しか検証しないと、外部埋め込み実装が異常値を返した場合に
    // `Ranking::Hybrid` へそのまま渡り、既存のベクトルリテラル経路
    // （`parser::parse_vector_literal`）や `EngineCore::search` が課している
    // 非有限値拒否と非対称になり、リソース増幅を許してしまう）。
    if query_vector.iter().any(|v| !v.is_finite()) {
        return Err(SqlSurfaceError::invalid_input(
            "USING PLAN re-embedded vector must not contain NaN/Inf".to_string(),
        ));
    }

    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let projection = parser::bind_projection(stmt.projection(), schema, udfs, &mut node_budget)?;
    let (metadata_filters, expr_filters, rls_predicate_present, or_filters) =
        parser::bind_where_predicates(
            stmt.where_predicates(),
            schema,
            udfs,
            &mut node_budget,
            &[],
        )?;

    // `core.rs::EngineCore::execute_sql_in_session` の `USING PLAN` 分岐が
    // `plan_using_plan_expansion`（高コスト I/O）より前に同じ検証
    // （[`parser::validate_search_limit`]）を既に一度通しているが、本関数は
    // `pub(crate)` として他呼び出し元（単体テスト等）からも呼ばれうるため、
    // ここでも検証する（多層防御。codex-review P1 指摘対応、PR #266）。
    let limit = parser::validate_search_limit(stmt.limit())?;

    let query_text = expanded_query_text(question, expansion);

    // `BoundStatement` のフィールドは `pub(crate)`（クレート外からの構造体リテラル
    // 構築は不可・本モジュールはクレート内のため可）。既存 `sql::parser::
    // bind_in_session` と同じ束縛結果の形へ直接組み立てる（`expr_filters` を
    // 引き継ぐため、`BoundStatement::new`（`expr_filters: Vec::new()` 固定の
    // 外部向け constructor）は使わない）。
    // Issue #353: `bind_in_session`（`sql::parser`）と同じく `expr_filters` を
    // 束縛時に 1 回だけステップ列コンパイルする。
    let expr_filter_programs = expr_filters
        .iter()
        .map(crate::sql::expr_program::ExprProgram::compile)
        .collect();

    Ok(BoundStatement {
        table: stmt.table_name().to_string(),
        projection,
        metadata_filters,
        rls_predicate_present,
        expr_filters,
        expr_filter_programs,
        or_filters,
        ranking: Ranking::Hybrid {
            query: query_vector,
            text_column_index,
            query_text,
        },
        limit,
        mode: resolved_mode,
        evaluation_order: stmt.evaluation_order(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};

    fn schema_with_body() -> TableSchema {
        TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding".to_string(), ColumnType::Vector(2), false),
                ColumnDef::new("body".to_string(), ColumnType::Text, false),
            ],
        )
    }

    fn sample_expansion() -> QueryExpansion {
        QueryExpansion {
            search_terms: vec!["alpha".to_string(), "beta".to_string()],
            path_hint: None,
            kind_hint: None,
            mode_hint: None,
        }
    }

    #[test]
    fn expanded_query_text_places_question_first_and_keeps_term_order() {
        let text = expanded_query_text("find auth", &sample_expansion());
        assert_eq!(text, "find auth alpha beta");
    }

    #[test]
    fn expanded_query_text_is_deterministic() {
        let expansion = sample_expansion();
        let a = expanded_query_text("q", &expansion);
        let b = expanded_query_text("q", &expansion);
        assert_eq!(a, b);
    }

    #[test]
    fn bind_expansion_produces_hybrid_ranking_with_expanded_text() {
        let schema = schema_with_body();
        let stmt = ValidatedStatement::new(
            "documents".to_string(),
            crate::sql::allowlist::Projection::All,
            crate::sql::allowlist::OrderByForm::UsingPlan,
            Vec::new(),
            5,
            crate::sql::plan::EvaluationOrder::DEFAULT,
        )
        .with_using_plan(Some("find auth".to_string()));
        let expansion = sample_expansion();
        let bound = bind_expansion(
            &stmt,
            &schema,
            "find auth",
            &expansion,
            vec![0.1, 0.2],
            &crate::sql::udf_call::UdfRegistry::default(),
            crate::sql::mode::resolve_mode(None, None),
        )
        .expect("bind_expansion should succeed with a body column present");

        match bound.ranking() {
            Ranking::Hybrid {
                query,
                text_column_index,
                query_text,
            } => {
                assert_eq!(query, &vec![0.1, 0.2]);
                assert_eq!(*text_column_index, 1);
                assert_eq!(query_text, "find auth alpha beta");
            }
            other => panic!("expected Ranking::Hybrid, got {other:?}"),
        }
        assert_eq!(bound.limit(), 5);
    }

    #[test]
    fn bind_expansion_rejects_missing_body_column() {
        let schema = TableSchema::new(
            "documents",
            vec![ColumnDef::new(
                "embedding".to_string(),
                ColumnType::Vector(2),
                false,
            )],
        );
        let stmt = ValidatedStatement::new(
            "documents".to_string(),
            crate::sql::allowlist::Projection::All,
            crate::sql::allowlist::OrderByForm::UsingPlan,
            Vec::new(),
            5,
            crate::sql::plan::EvaluationOrder::DEFAULT,
        )
        .with_using_plan(Some("find auth".to_string()));
        let expansion = sample_expansion();
        let err = bind_expansion(
            &stmt,
            &schema,
            "find auth",
            &expansion,
            vec![0.1, 0.2],
            &crate::sql::udf_call::UdfRegistry::default(),
            crate::sql::mode::resolve_mode(None, None),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_expansion_rejects_embedder_dim_mismatch_against_table_vector_column() {
        // `embedding.rs` の契約: 呼び出し元が `Embedder::dim` を対象テーブルの
        // `VECTOR(N)` と突き合わせて検証する。`schema_with_body` は
        // `VECTOR(2)` だが、ここでは次元 3 のベクトルを渡し、既存の
        // `parse_vector_literal` と同じ不変条件が埋め込み由来のベクトルにも
        // 課されることを固定する。
        let schema = schema_with_body();
        let stmt = ValidatedStatement::new(
            "documents".to_string(),
            crate::sql::allowlist::Projection::All,
            crate::sql::allowlist::OrderByForm::UsingPlan,
            Vec::new(),
            5,
            crate::sql::plan::EvaluationOrder::DEFAULT,
        )
        .with_using_plan(Some("find auth".to_string()));
        let expansion = sample_expansion();
        let err = bind_expansion(
            &stmt,
            &schema,
            "find auth",
            &expansion,
            vec![0.1, 0.2, 0.3],
            &crate::sql::udf_call::UdfRegistry::default(),
            crate::sql::mode::resolve_mode(None, None),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }
}
