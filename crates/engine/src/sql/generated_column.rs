//! 生成列 `GENERATED ALWAYS AS (<式>) STORED` の宣言側の検査（TABLE-50・Issue #3327）。
//!
//! 役割と責務境界:
//! - 構文段（カタログ非参照・権限非依存）: [`precheck_tokens`]（部分クエリ・集計・ウィンドウ・SRF の
//!   トークン先読み）と [`check_expression_syntax`]（解析済みの式木の決定性・組み込み以外の関数の拒否）。
//!   `sql::allowlist::Parser` の列制約解析（`CREATE TABLE`）が呼ぶ。式索引の検査
//!   （`sql::index_expr`）を `IndexExprSite::Generated` で共有し、文言と SQLSTATE（`42P17`）だけを
//!   切り替える。構文段はカタログを引かないため存在オラクルにならない。
//! - 束縛段: [`bind_generated_column`]。DDL 権限ゲートの後（`sql::ddl::build_table_schema`）で、
//!   全列がそろったスキーマに対して各生成列の式を束縛・検証し、正規描画と参照列を持つ
//!   `catalog::GeneratedColumnDef` を返す。保存するのは正規描画だけで原文は保存しない。
//!
//! 計算（書き込み時の評価）は本モジュールの責務外（Issue #3328 以降）。本 Issue の間は生成列を持つ表への
//! 書き込みを `constraint` の検査点が固定文言の `0A000` で拒否する。
//!
//! 判定表（SQLSTATE）と PostgreSQL との差は `docs/design/generated-column.md` を参照する。

use crate::catalog::{
    ColumnDef, GeneratedColumnDef, TableSchema, MAX_GENERATED_EXPRESSION_SQL_LEN,
    MAX_GENERATED_REFERENCED_COLUMNS,
};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::index_expr::IndexExprSite;
use crate::sql::lexer::Token;
use crate::sql::udf_call::Expr;

/// 生成列の式への `COLLATE` の拒否文言（式索引の前例に揃える。要確認: Issue #3332）。
const MSG_COLLATE: &str = "COLLATE in a generated column expression is not supported yet";
/// 生成列（自分自身・他の生成列）の参照の拒否文言。
const MSG_REFERENCES_GENERATED: &str =
    "generation expression cannot reference another generated column";
/// ベクトル系の列の参照の拒否文言。
const MSG_REFERENCES_VECTOR: &str = "generation expression cannot reference a vector column";
/// 式で代入できない列型の拒否文言（`UPDATE ... SET` の文言を流用しない。PG との差）。
const MSG_COLUMN_TYPE: &str = "generated columns of this column type are not supported";
/// 正規描画の長さ超過の拒否文言。
const MSG_TOO_LONG: &str = "generation expression is too long";
/// 参照列数の超過の拒否文言。
const MSG_TOO_MANY_REFERENCES: &str = "generation expression references too many columns";
/// 正規描画へ往復できない式の拒否文言（fail-closed。壊れた式を永続化しない）。
const MSG_NOT_STORABLE: &str = "generation expression cannot be stored in normalized form";

/// 生成列の式のトークン区間 `element`（`(` と対応する `)` の内側）を先読み検査する（構文段）。
/// 部分クエリ・集計・ウィンドウ・集合返却関数を `42P17` で拒否する。汎用の式パーサは
/// `count(*)`・`OVER (...)`・`(SELECT ...)` を `42601` で落とすため、解析より前にここで分類する。
pub(crate) fn precheck_tokens(element: &[Token]) -> Result<(), SqlSurfaceError> {
    crate::sql::index_expr::precheck_index_tokens(element, IndexExprSite::Generated)
}

/// 解析済みの式木の構文段の検査（決定性・組み込み関数のみ）。非決定的な式
/// （`now()`・`random()`・`CURRENT_TIMESTAMP` 等）・組み込み以外の関数（IMMUTABLE な UDF を含む）は
/// `42P17`。型に依存する決定性は束縛段（[`bind_generated_column`]）が再判定する。
pub(crate) fn check_expression_syntax(expr: &Expr) -> Result<(), SqlSurfaceError> {
    crate::sql::index_expr::check_generated_expression_determinism(expr)
}

/// 生成列 `column` の式 `expr` を `schema`（全列がそろった定義）に対して束縛・検証し、永続化する
/// 正規描画と参照列を返す（束縛段。DDL 権限ゲートの後）。`generated_names` は同じ表で生成列として
/// 宣言された全列名（宣言順。前方参照の判定に使う。式は生成列を参照できない）。
///
/// 判定順（固定。SQLSTATE は固定文言で列名・値を載せない。応答に載せるのは利用者が書いた列名だけ）:
/// 1. 列型が式で代入できない型なら `0A000`
/// 2. 式木内の `COLLATE` は `0A000`
/// 3. ゾーン無し `TIMESTAMPTZ` リテラルを定義時のセッション TZ で確定値へ畳む
/// 4. 参照する識別子の解決: 未知の列は `42703`、生成列は `42P17`、ベクトル系の列は `0A000`
///    （修飾名・`tenant_id`・`visibility` は通常の列として解決できず `42703` で拒否する）
/// 5. 代入の束縛（`set_expr::bind_set_expr`。型の不一致は `42804`）
/// 6. 型に依存する決定性の再判定（`42P17`）
/// 7. 正規描画（長さ超過は `54000`）と、再解析との一致確認（不一致は `0A000`）
/// 8. 参照列の抽出（宣言順・重複なし。上限超過は `54000`）
pub(crate) fn bind_generated_column(
    expr: &Expr,
    column: &ColumnDef,
    schema: &TableSchema,
    generated_names: &[String],
) -> Result<GeneratedColumnDef, SqlSurfaceError> {
    if !crate::sql::set_expr::column_type_accepts_expression(&column.ty) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: MSG_COLUMN_TYPE.to_string(),
        });
    }
    if crate::sql::index_expr::expr_contains_collate(expr) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: MSG_COLLATE.to_string(),
        });
    }
    let folded = crate::sql::index_expr::fold_session_local_literals(expr)?;

    // 参照する識別子を集める（1 回の走査。識別子の写像を訪問器として使う）。
    let seen: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
    let _ = crate::sql::check_constraint::map_expr_idents(&folded, &|name| {
        seen.borrow_mut().push(name.to_string());
        name.to_string()
    });
    let mut referenced_indexes: Vec<usize> = Vec::new();
    for name in seen.into_inner() {
        // 完全一致を優先し、無ければ大小無視で解決する（字句段は引用符なしの識別子を正規化済み）。
        let index = schema
            .columns
            .iter()
            .position(|c| c.name == name)
            .or_else(|| {
                schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&name))
            })
            .ok_or_else(|| SqlSurfaceError::undefined_column(name.clone()))?;
        let target = schema
            .columns
            .get(index)
            .ok_or_else(|| SqlSurfaceError::undefined_column(name.clone()))?;
        if generated_names.iter().any(|g| g == &target.name) {
            return Err(SqlSurfaceError::InvalidObjectDefinition {
                detail: MSG_REFERENCES_GENERATED,
            });
        }
        if target.ty.is_vector_family() {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: MSG_REFERENCES_VECTOR.to_string(),
            });
        }
        if !referenced_indexes.contains(&index) {
            referenced_indexes.push(index);
        }
    }
    if referenced_indexes.len() > MAX_GENERATED_REFERENCED_COLUMNS {
        return Err(SqlSurfaceError::payload_too_large(MSG_TOO_MANY_REFERENCES));
    }

    // 代入の束縛（型の不一致は `42804`）。結果の束縛式は使わず、検証だけを行う。
    // 式全体が裸の `NULL` の場合は束縛器が「NULL は CASE／COALESCE／NULLIF の被演算子のみ」として拒否するが、
    // `NULL` はどの列型にも代入できるため束縛検証を省く（定数 NULL の生成式を宣言可能にする）。
    if !matches!(folded, Expr::Null) {
        let _ = crate::sql::set_expr::bind_set_expr(&column.name, &folded, column, schema)?;
    }
    // 型に依存する決定性（`TIMESTAMPTZ` の CAST 等）の再判定。式索引の文言を生成列の文言へ差し替える。
    if let Err(e) =
        crate::sql::index_expr::check_bound_index_expression_determinism(&folded, schema)
    {
        return Err(match e {
            SqlSurfaceError::InvalidObjectDefinition { .. } => {
                crate::sql::index_expr::not_immutable_at(IndexExprSite::Generated)
            }
            other => other,
        });
    }

    // 代入キャストがセッション TZ 依存の変換（`TIMESTAMP`／`DATE` ⇔ `TIMESTAMPTZ`）を挟む宣言は、
    // 構文木に現れない暗黙変換のため上の再判定を素通りする。式を不変に限る（G2）ため拒否する。
    if crate::sql::index_expr::assignment_cast_is_session_dependent(&folded, &column.ty, schema) {
        return Err(crate::sql::index_expr::not_immutable_at(
            IndexExprSite::Generated,
        ));
    }

    // 正規描画と往復一致（保存するのは描画結果だけ。原文は連結しない）。
    let rendered = crate::sql::check_constraint::render_expr(&folded);
    if rendered.len() > MAX_GENERATED_EXPRESSION_SQL_LEN {
        return Err(SqlSurfaceError::payload_too_large(MSG_TOO_LONG));
    }
    let not_storable = || SqlSurfaceError::FeatureNotSupported {
        detail: MSG_NOT_STORABLE.to_string(),
    };
    let reparsed = crate::sql::allowlist::parse_generated_expression_text(&rendered)
        .map_err(|_| not_storable())?;
    if reparsed != folded {
        return Err(not_storable());
    }

    // 参照列は宣言順に並べる。
    referenced_indexes.sort_unstable();
    let referenced_columns: Vec<String> = referenced_indexes
        .iter()
        .filter_map(|i| schema.columns.get(*i).map(|c| c.name.clone()))
        .collect();
    Ok(GeneratedColumnDef {
        expression: rendered,
        referenced_columns,
    })
}
