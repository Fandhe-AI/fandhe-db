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
//! 書き込み時の計算（Issue #3328）: [`GeneratedComputeScope`]（SQL 表層の入口だけが張る「結線済み」の
//! 宣言。許可リスト方式で、張り忘れは `0A000` に倒れる）、[`CompiledGeneratedColumns`]（1 文に 1 回だけ
//! 保存式を再解析・再束縛する単一の計算器。`tenant` の INSERT・UPDATE・UPSERT の計算点が呼ぶ）、
//! 検査点の照合（`constraint::enforce_row_constraints_impl` が [`CompiledGeneratedColumns::verify_row`] を呼ぶ）。
//! 結線していない経路（COPY・NoSQL・ビュー・トリガ・FK の参照アクション等。Issue #3329）は固定文言の
//! `0A000` のまま拒否する。
//!
//! 判定表（SQLSTATE）と PostgreSQL との差は `docs/design/generated-column.md` を参照する。

use crate::catalog::{
    CatalogError, ColumnDef, GeneratedColumnDef, TableSchema, MAX_GENERATED_EXPRESSION_SQL_LEN,
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
        // 他の束縛処理と同じ厳密一致で解決する（字句段は引用符なしの識別子を正規化済み）。
        // 引用符付きで定義された別名の列へ大小無視で寄せると、未定義列が生成列参照として拒否されるため寄せない。
        let index = schema
            .columns
            .iter()
            .position(|c| c.name == name)
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
type WriteError = crate::tenant::TenantWriteError;

thread_local! {
    /// 生成列の計算点が結線された SQL 表層の書き込み区間の対象表（`tenant` に渡る表名と同じ文字列）。
    /// `None` は結線済みの区間の外（既定。生成列を持つ表への書き込みは `0A000`）。
    static WIRED_TABLE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// 生成列の計算点が結線済みであることを宣言する RAII ガード（Issue #3328）。`core` の SQL 表層の入口
/// （INSERT・UPDATE・UPSERT の各形）だけが、対象表名を指定して張る。COPY・NoSQL・PL/pgSQL・トリガの
/// 書き込みは張らないため、共有の書き込み関数へ到達しても [`write_wired`] が偽になり `0A000` に倒れる
/// （`RowPolicyWrite` と違い既定が拒否側。張り忘れは fail-closed）。drop で直前の状態へ戻す。
pub(crate) struct GeneratedComputeScope {
    prev: Option<String>,
}

impl GeneratedComputeScope {
    /// 表 `table` への書き込み区間を結線済みとして張る。
    pub(crate) fn enter(table: &str) -> Self {
        Self {
            prev: WIRED_TABLE.with(|c| c.replace(Some(table.to_string()))),
        }
    }

    /// 入れ子の書き込み（トリガ関数・PL/pgSQL の DML）が結線済みを継承しないよう、一時的に外す。
    pub(crate) fn suspend() -> Self {
        Self {
            prev: WIRED_TABLE.with(|c| c.replace(None)),
        }
    }
}

impl Drop for GeneratedComputeScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        WIRED_TABLE.with(|c| *c.borrow_mut() = prev);
    }
}

/// 表 `table` への書き込みが結線済みの SQL 表層の区間にあるか。結線済みの区間の対象表と一致し、
/// かつビュー経由の書き込み区間の外であることが条件（ビュー・INSTEAD OF は Issue #3329）。
pub(crate) fn write_wired(table: &str) -> bool {
    WIRED_TABLE.with(|c| c.borrow().as_deref() == Some(table))
        && crate::sql::view_write_scope::current_view_name().is_none()
}

/// 書き込み先の表の生成列の保存式を束縛した計算器（1 文に 1 回だけ作る。行ループの外）。
pub(crate) struct CompiledGeneratedColumns {
    cols: Vec<GenCol>,
    /// 生成列を nullable に読み替えた作業用スキーマ（NOT NULL の生成列の仮値 NULL を含む行の符号化・走査に使う）。
    scratch: TableSchema,
}

struct GenCol {
    idx: usize,
    name: String,
    nullable: bool,
    /// `None` は式全体が裸の `NULL`（束縛を省いた定数 NULL）。
    set: Option<crate::sql::set_expr::BoundSetExpr>,
    refs: Vec<usize>,
}

fn corrupt() -> WriteError {
    WriteError::Catalog(CatalogError::CorruptSchema(
        "generated column definition is corrupt".to_string(),
    ))
}

impl CompiledGeneratedColumns {
    /// 生成列が無ければ `None`。結線済みでなければ `0A000`（`GeneratedColumnWriteUnsupported`）。
    /// 保存式の再解析・再束縛・参照列の解決のどれかが失敗した（カタログ漂流）場合は `XX000`
    /// （`CorruptSchema`・固定文言）で拒否する。
    pub(crate) fn for_write(schema: &TableSchema, table: &str) -> Result<Option<Self>, WriteError> {
        if !schema.columns.iter().any(|c| c.generated.is_some()) {
            return Ok(None);
        }
        if !write_wired(table) {
            return Err(WriteError::GeneratedColumnWriteUnsupported);
        }
        Self::compile(schema).map(Some)
    }

    /// 結線の判定なしで束縛する（検査点の照合段が使う）。生成列が無ければ `None`。
    pub(crate) fn compile_any(schema: &TableSchema) -> Result<Option<Self>, WriteError> {
        if !schema.columns.iter().any(|c| c.generated.is_some()) {
            return Ok(None);
        }
        Self::compile(schema).map(Some)
    }

    fn compile(schema: &TableSchema) -> Result<Self, WriteError> {
        let mut cols = Vec::new();
        let mut scratch = schema.clone();
        for (idx, column) in schema.columns.iter().enumerate() {
            let Some(generated) = &column.generated else {
                continue;
            };
            let expr =
                crate::sql::allowlist::parse_generated_expression_text(&generated.expression)
                    .map_err(|_| corrupt())?;
            let set = if matches!(expr, Expr::Null) {
                None
            } else {
                Some(
                    crate::sql::set_expr::bind_set_expr(&column.name, &expr, column, schema)
                        .map_err(|_| corrupt())?,
                )
            };
            let mut refs = Vec::new();
            for name in &generated.referenced_columns {
                let i = schema
                    .columns
                    .iter()
                    .position(|c| &c.name == name)
                    .ok_or_else(corrupt)?;
                refs.push(i);
            }
            if let Some(c) = scratch.columns.get_mut(idx) {
                c.nullable = true;
            }
            cols.push(GenCol {
                idx,
                name: column.name.clone(),
                nullable: column.nullable,
                set,
                refs,
            });
        }
        Ok(Self { cols, scratch })
    }

    /// 変更された列 `changed` に依存して再計算が要る生成列の位置（生成列自身が `changed` に含まれる場合も含む）。
    pub(crate) fn affected(&self, changed: &[usize]) -> Vec<usize> {
        self.cols
            .iter()
            .filter(|c| changed.contains(&c.idx) || c.refs.iter().any(|r| changed.contains(r)))
            .map(|c| c.idx)
            .collect()
    }

    /// 行の通常列（`scalars`）から、`only`（`None` は全生成列）の生成列を評価して `(列位置, 値)` を返す。
    fn eval_into(
        &self,
        id: u64,
        embedding: &[f32],
        scalars: &[Option<crate::row_codec::ScalarRef<'_>>],
        only: Option<&[usize]>,
    ) -> Result<Vec<(usize, crate::row_codec::Value)>, WriteError> {
        let mut out = Vec::with_capacity(self.cols.len());
        for col in &self.cols {
            if let Some(only) = only {
                if !only.contains(&col.idx) {
                    continue;
                }
            }
            let value = match &col.set {
                None => {
                    if !col.nullable {
                        return Err(WriteError::SetExpressionEvaluationFailed(
                            SqlSurfaceError::not_null_violation(col.name.clone()),
                        ));
                    }
                    crate::row_codec::Value::Null
                }
                Some(set) => {
                    crate::sql::set_expr::eval_set_expr_to_value(set, id, embedding, scalars)
                        .map_err(WriteError::SetExpressionEvaluationFailed)?
                }
            };
            out.push((col.idx, value));
        }
        Ok(out)
    }

    /// 符号化済みの行 `metadata` から生成列を評価する（UPDATE が使う）。
    pub(crate) fn compute_from_metadata(
        &self,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
        only: Option<&[usize]>,
    ) -> Result<Vec<(usize, crate::row_codec::Value)>, WriteError> {
        let scalars = crate::row_codec::scan_scalar_columns(&self.scratch, metadata)
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        self.eval_into(id, embedding, &scalars, only)
    }

    /// 値の列 `values`（スキーマ幅）の生成列の位置を、通常列から計算した値で上書きする（INSERT・UPSERT が使う）。
    /// `only` が `Some` のときはその位置だけ再計算する。
    pub(crate) fn compute_values(
        &self,
        id: u64,
        embedding: &[f32],
        values: &mut [crate::row_codec::Value],
        only: Option<&[usize]>,
    ) -> Result<(), WriteError> {
        let buf = crate::row_codec::encode_scalar_columns(&self.scratch, values)
            .map_err(|e| CatalogError::Invalid(e.to_string()))?;
        let computed = self.compute_from_metadata(id, embedding, &buf, only)?;
        for (idx, v) in computed {
            let slot = values.get_mut(idx).ok_or_else(corrupt)?;
            *slot = v;
        }
        Ok(())
    }

    /// 書き込んだ行の生成列が式の再計算結果と一致することを確かめる（検査点の多層防御）。
    /// 比較は `Value` の等値ではなく符号化バイトで行う（NaN・-0.0 を誤検出しない）。不一致は `XX000`。
    pub(crate) fn verify_row(
        &self,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
    ) -> Result<(), WriteError> {
        let scalars = crate::row_codec::scan_scalar_columns(&self.scratch, metadata)
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        let computed = self.eval_into(id, embedding, &scalars, None)?;
        let overrides: Vec<(usize, &crate::row_codec::Value)> =
            computed.iter().map(|(i, v)| (*i, v)).collect();
        let expected =
            crate::row_codec::merge_encode_scalar_columns(&self.scratch, &scalars, &overrides)
                .map_err(|e| CatalogError::Invalid(e.to_string()))?;
        let stored = crate::row_codec::merge_encode_scalar_columns(&self.scratch, &scalars, &[])
            .map_err(|e| CatalogError::Invalid(e.to_string()))?;
        if expected != stored {
            return Err(WriteError::Catalog(CatalogError::CorruptSchema(
                "generated column value does not match its expression".to_string(),
            )));
        }
        Ok(())
    }
}
