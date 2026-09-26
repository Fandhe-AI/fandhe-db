//! SQL 表層の束縛層（TASK-75、対象ビヘイビア: SQL-1, SQL-2, SQL-3, SQL-4。
//! ポインタ: `docs/spec/05-tasks.md` TASK-75・`docs/spec/04-behavior/sql-surface.md`）。
//!
//! 責務境界: [`allowlist::validate_statement`](crate::sql::allowlist::validate_statement)
//! が返す [`ValidatedStatement`](crate::sql::allowlist::ValidatedStatement)
//! （構造は許可リストを通過済みだが、列名・リテラル値の意味論的妥当性は未検証）を、
//! `catalog.rs` の [`TableSchema`] と照合して意味論的に検証し、[`exec`](crate::sql::exec)
//! が直接実行できる [`BoundStatement`] へ変換する。ここで検出する違反
//! （未知の列名・列型不一致・ベクトルリテラルの不正形式・非有限値・次元不一致・
//! `LIMIT` 範囲外・hybrid の 2 引数形など「受理構文だが値が不正」）は
//! [`SqlSurfaceError::InvalidInput`]（`22000`）または、アロケーション前のサイズ上限
//! 超過は [`SqlSurfaceError::PayloadTooLarge`]（`54000`）で fail-closed に拒否する。
//!
//! `unwrap`/`expect`/添字アクセス `[]` を使わず `get()`・`checked_*` で untrusted な
//! リテラル文字列を解析する（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。

use crate::catalog::{ColumnDef, ColumnDefault, ColumnType, TableSchema};
use crate::declarative_filter::{self, DeclarativeFilter, MetadataFilter};
use crate::sql::allowlist::{
    FunctionArg, InsertLiteral, OnConflictAction, OrderByForm, Projection, UpsertValue,
    ValidatedDelete, ValidatedInsert, ValidatedPredicateDelete, ValidatedStatement, WherePredicate,
};
use crate::sql::plan::EvaluationOrder;
use crate::sql::udf_call::Expr;
use crate::sql::using_operation_id::OperationId;

/// ベクトルリテラルの生バイト長上限（SQL-1）。アロケーション（`Vec<f32>` の確保・
/// カンマ分割）に入る前にこの長さで拒否する。
const MAX_VECTOR_LITERAL_BYTES: usize = 64 * 1024;

/// 配列リテラルの生バイト長上限（TABLE-14・TASK-198、Issue #888・D-A5）。
/// 列 1 個分のペイロード上限（`row_codec::MAX_TEXT_FIELD_LEN` と同値の
/// 実装既定値）と揃え、走査（`Vec<String>` の確保・要素切り出し）に入る前に
/// この長さで拒否する。
const MAX_ARRAY_LITERAL_BYTES: usize = 4 * 1024 * 1024;

/// 投影対象の 1 列。`Id` は疑似列（[`crate::storage::Row::id`] 由来。スキーマの列では
/// ないため `column_index` を持たない）。`Column` の `index` は `TableSchema::columns`
/// の列順インデックス（[`crate::row_codec::decode_scalar_columns`] が返す `Vec` の
/// 添字と一致する。`VECTOR` 列の位置は常に `row_codec::Value::Null` が入るため、
/// `exec.rs` はその位置を投影する際 `storage::Row::embedding` を別途参照する）。
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Computed` variant を
/// 追加した（宣言的 UDF・組み込み関数呼び出しを結果列位置で束縛した式）。
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectedColumn {
    Id,
    Column {
        index: usize,
        name: String,
    },
    /// 式項目（TASK-79・SQL-9）。`name` は `AS <alias>` の指定値、省略時は関数名。
    Computed {
        name: String,
        expr: crate::sql::udf_call::BoundExpr,
    },
}

/// DISTANCE 段のランキング方式。C1/C2/C3（純粋・スカラー条件付き・RLS 適用 Top-k）は
/// `Distance`、C4（ハイブリッド）は `Hybrid` を使う（SQL-1〜4）。
#[derive(Debug, Clone, PartialEq)]
pub enum Ranking {
    Distance {
        query: Vec<f32>,
    },
    Hybrid {
        query: Vec<f32>,
        text_column_index: usize,
        query_text: String,
    },
}

/// 束縛済みの SQL 文（[`exec::execute_statement`](crate::sql::exec::execute_statement)
/// が直接実行する入力形）。
///
/// **TASK-161 で意図的に非公開化した破壊的変更（BREAKING CHANGE）**: 全フィールドを
/// `pub` から `pub(crate)` へ変更し `#[non_exhaustive]` を付与した。クレート外からの
/// 直接のフィールド参照・構造体リテラル構築は今後不可能。構築は [`BoundStatement::new`]
/// ／[`BoundStatement::with_mode`]、読み取りは [`BoundStatement::table`] 等の各アクセサー
/// メソッドを使う（詳細は PR #188 の Breaking Changes 節を参照。TASK-164 拡張点の前方
/// 互換確保とカプセル化のため）。
///
/// `#[non_exhaustive]`: TASK-161（SQL-12）で `mode` フィールドを追加した際、既存の
/// 構造体リテラル構築コードが必須フィールド不足でコンパイル不能になる破壊的変更と
/// なった（AGENTS.md「公開 API・エラー契約の互換性（P1）」）。今後のフィールド追加が
/// 同様の破壊を再発させないよう、外部クレートからの構造体リテラル構築を非対応にする。
/// フィールドはカプセル化のため `pub(crate)` とし（クレート外からの直読み・直書きは
/// 不可。コード内では [`BoundStatement::table`] 等のアクセサーメソッドを経由する）、
/// クレート外からの構築は [`BoundStatement::new`]（既存フィールド相当の引数を取り、
/// `mode` は既定値 [`crate::sql::mode::resolve_mode`]`(None, None)` で構築する）と
/// [`BoundStatement::with_mode`]（TASK-161 で追加した `mode` を設定するビルダー的
/// メソッド）を経由する。本構造体は通常 [`bind_with_session`] の戻り値として取得
/// するが、上記 constructor 経由でも構築できる（PR #188 レビュー指摘対応: 破壊的
/// 変更の移行経路を用意しつつ、直接のフィールド読み書きは許可しない）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct BoundStatement {
    pub(crate) table: String,
    pub(crate) projection: Vec<ProjectedColumn>,
    /// SCALAR 段で適用するメタデータフィルタ（等価・前方一致、TASK-147・EXT-3）。
    /// **TASK-147 で追加した破壊的変更（BREAKING CHANGE）**: 旧 `scalar_filters:
    /// Vec<ScalarEq>`（等価専用）を `declarative_filter::MetadataFilter`
    /// （汎用 API。等価・前方一致の両方を表す）へ置換し、フィールド名も
    /// `metadata_filters` へ改名した。
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    /// `WHERE` 句に `visible()` 呼び出し形が含まれていたか（SQL-3・RLS-7 参照）。
    /// **実行側の RLS 適用はこの値の有無に依存しない**（`exec.rs` は無条件に
    /// `PolicyContext::is_visible` を適用する）。本フィールドは束縛結果の可観測性
    /// （テスト・診断）のためだけに保持する。
    pub(crate) rls_predicate_present: bool,
    /// `WHERE` の式述語（TASK-79・SQL-9）。UDF インライン展開済みで、レジストリを
    /// 参照せず単独で評価できる。既存の `metadata_filters` と同じ SCALAR 段の一部
    /// として扱う（`sql::exec` のモジュールドキュメント参照。既定順では候補構築時の
    /// 行フックで事前適用し、`HINT ORDER` で DISTANCE 先行時は DISTANCE 段の後で
    /// 事後適用する）。
    pub(crate) expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
    /// `expr_filters` をステップ列コンパイルした実行形（Issue #353）。
    /// `expr_filters` と要素数・評価順が 1 対 1 対応する（各インデックス `i`
    /// について `expr_filter_programs[i]` は `expr_filters[i]` のコンパイル
    /// 結果）。`sql::exec` の SCALAR 段行フックはこちらを評価する。
    /// `expr_filters` フィールド自体は EXPLAIN・テスト等の可観測性のため
    /// 残置し、実行経路からは参照しない。
    pub(crate) expr_filter_programs: Vec<crate::sql::expr_program::ExprProgram>,
    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。`AND` で結ぶ
    /// `metadata_filters`／`expr_filters` とは独立に保持し、SCALAR 段は
    /// 「両方が空かどうか」ではなく「3 つとも空かどうか」でゲートする契約に
    /// 変える（`sql::exec` 等のフィルタ空判定を参照）。
    pub(crate) or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
    pub(crate) ranking: Ranking,
    pub(crate) limit: usize,
    /// 取得モードの優先順位解決結果（TASK-161・SQL-12）。クエリ句 `USING MODE`
    /// （[`ValidatedStatement::search_mode`](crate::sql::allowlist::ValidatedStatement)）
    /// とセッション変数（呼び出し元 `core.rs::EngineCore::execute_sql_in_session` が
    /// 渡す [`crate::sql::mode::SessionState`]）から [`crate::sql::mode::resolve_mode`]
    /// が決定する。カーネル選択（`dispatch.rs`）の入力には含めない（`precision` の
    /// 実行契約は TASK-162・SEARCH-9 の管轄。`sql::exec` が本フィールドを見て
    /// 実行可否を判定する）。
    pub(crate) mode: crate::sql::mode::ResolvedMode,
    /// `HINT ORDER(...)` で指定された評価順序（TASK-76・SQL-7）。`allowlist` が
    /// 検証済みの [`EvaluationOrder`] をそのまま素通しする（意味論的な束縛の必要は
    /// ない。実行意味論の解釈は [`crate::sql::plan::ExecutionPlan`] の管轄）。
    pub(crate) evaluation_order: EvaluationOrder,
}

impl BoundStatement {
    /// クレート外から構築するための constructor（TASK-161 で `mode` フィールドを
    /// 追加する以前の既存フィールド相当の引数を取る）。`mode` は
    /// `resolve_mode(None, None)`（クエリ句・セッション変数いずれも未指定時の既定値、
    /// `recall`・[`crate::sql::mode::ModeSource::Default`]）で構築され、必要なら
    /// [`Self::with_mode`] を続けて呼ぶ。フィールドが `pub(crate)` のため、
    /// クレート外から `BoundStatement` を得るにはこの constructor か
    /// [`bind_with_session`] の戻り値を経由するしかない。
    pub fn new(
        table: String,
        projection: Vec<ProjectedColumn>,
        metadata_filters: Vec<MetadataFilter>,
        rls_predicate_present: bool,
        ranking: Ranking,
        limit: usize,
        evaluation_order: EvaluationOrder,
    ) -> Self {
        Self {
            table,
            projection,
            metadata_filters,
            rls_predicate_present,
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            ranking,
            limit,
            mode: crate::sql::mode::resolve_mode(None, None),
            evaluation_order,
        }
    }

    /// `mode`（TASK-161・SQL-12）を設定したコピーを返すビルダー的メソッド。
    /// [`Self::new`] と組み合わせて `mode` を含む値を外部から構築する。
    #[must_use]
    pub fn with_mode(mut self, mode: crate::sql::mode::ResolvedMode) -> Self {
        self.mode = mode;
        self
    }

    /// 束縛対象のテーブル名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 投影対象の列一覧（`Row::id` 疑似列を含みうる）。
    pub fn projection(&self) -> &[ProjectedColumn] {
        &self.projection
    }

    /// SCALAR 段で適用するメタデータフィルタ一覧（等価・前方一致、TASK-147・EXT-3）。
    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        &self.metadata_filters
    }

    /// `WHERE` 句に `visible()` 呼び出し形が含まれていたか（SQL-3・RLS-7 参照）。
    /// **実行側の RLS 適用はこの値の有無に依存しない**（可観測性のためだけの値）。
    pub fn rls_predicate_present(&self) -> bool {
        self.rls_predicate_present
    }

    /// `WHERE` の式述語（TASK-79・SQL-9）。UDF インライン展開済み。
    pub fn expr_filters(&self) -> &[crate::sql::udf_call::BoundExpr] {
        &self.expr_filters
    }

    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。
    pub fn or_filters(&self) -> &[crate::sql::where_tree::BoundOrGroup] {
        &self.or_filters
    }

    /// `metadata_filters`・`expr_filters`・`or_filters` のいずれかが非空か
    /// （TASK-208・Issue #912）。`sql::exec` 等が「WHERE にフィルタ条件が
    /// 1 つも無い」ことを判定する既存の `metadata_filters.is_empty() &&
    /// expr_filters.is_empty()` ゲートは、この判定へ置き換える契約とする
    /// （置き換え漏れは OR 条件が黙って無視される fail-open のバグになる。
    /// security.md「不安全な設計」対応）。
    pub fn has_where_filters(&self) -> bool {
        !self.metadata_filters.is_empty()
            || !self.expr_filters.is_empty()
            || !self.or_filters.is_empty()
    }

    /// DISTANCE 段のランキング方式。
    pub fn ranking(&self) -> &Ranking {
        &self.ranking
    }

    /// `LIMIT` 句の値。
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// 取得モードの優先順位解決結果（TASK-161・SQL-12）。
    pub fn mode(&self) -> crate::sql::mode::ResolvedMode {
        self.mode
    }

    /// `HINT ORDER(...)` で指定された評価順序（TASK-76・SQL-7）。
    pub fn evaluation_order(&self) -> EvaluationOrder {
        self.evaluation_order
    }
}

/// 束縛済みの INSERT 文（SQL-10、TASK-80。
/// [`exec::execute_insert`](crate::sql::exec::execute_insert) が直接実行する入力形）。
/// テナント・可視性はここでは決定しない（`exec::execute_insert` がサーバー側で
/// `PolicyContext` から導出・固定する。`.claude/rules/security.md` P0
/// 「クライアント指定のテナントを信用しない」）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsert {
    pub table: String,
    /// 行キー（疑似列 `id`。列リストへの指定必須）。
    pub id: u64,
    /// `schema.columns` の列順に対応する値列（`catalog::insert_typed_row`/
    /// `tenant::insert_typed_row` の `values` 契約と同一。`id` 疑似列は含まない）。
    pub values: Vec<crate::row_codec::Value>,
    /// TASK-92（RECOVER-1）: `ValidatedInsert.operation_id` をそのまま素通しする。
    /// `LedgerMode::Ledgered`（既定）では `sql::allowlist::validate_insert` が既に
    /// `None` を `23502` で拒否済みのため常に `Some`。`CompareOnlyWithoutLedger`
    /// でのみ `None` になり得る。
    pub operation_id: Option<OperationId>,
}

/// 束縛済みの単一行・`id` 指定形 `DELETE` 文（SQL-18・TASK-191。#867 が
/// `EngineCore::delete_row`〔既存の RLS 可視集合判定・`operation_id` ガード
/// 込みの Rust API〕へ渡す入力形）。`BoundInsert` と異なり `values`／列型情報
/// を持たない（`DELETE` は `id` 以外の列を参照しないため）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundDelete {
    pub table: String,
    pub id: u64,
    /// TASK-92（RECOVER-1）: `ValidatedDelete.operation_id` をそのまま
    /// 素通しする。`LedgerMode::Ledgered`（既定）では
    /// `sql::allowlist::validate_delete` が既に `None` を `23502` で
    /// 拒否済みのため常に `Some`。`CompareOnlyWithoutLedger` でのみ
    /// `None` になり得る。
    pub operation_id: Option<OperationId>,
}

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::{self, SearchMode};

/// `NUMERIC(precision, scale)` 列向けリテラル（[`InsertLiteral::Number`]・
/// [`InsertLiteral::String`] の両方を受理。TABLE-13〔検討中〕・TASK-197、
/// Issue #885・D5）を [`crate::row_codec::Value::Numeric`] へ束縛する。
/// `crate::numeric::parse_for_column` の [`crate::numeric::NumericError`] を
/// `wire_code` へ写像する（`Malformed` → `22000`・`OutOfRange` →
/// `22003`）。`InsertLiteral::Bool` は型不一致として `22000` で拒否する。
/// INSERT（`bind_insert_row`）・UPDATE（`bind_set_assignments`）・UPSERT
/// （`bind_upsert_assignments`）・COPY（`sql::copy::bind_copy_record`。Issue
/// #939 のマージで追加された `Numeric` 列への対応漏れの修正）の各箇所が
/// 共有する（第 2 のパーサーを作らない）。
pub(crate) fn bind_numeric_literal(
    literal: &InsertLiteral,
    name: &str,
    precision: u8,
    scale: u8,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    let text = match literal {
        InsertLiteral::Number(s) | InsertLiteral::String(s) => s.as_str(),
        InsertLiteral::Bool(_) => {
            return Err(SqlSurfaceError::invalid_input(format!(
                "column {name:?} expects a NUMERIC literal, got a boolean literal"
            )))
        }
        // `InsertLiteral::Vector`（Issue #896 レビュー指摘・PR #1038）は
        // `ColumnType::Numeric` 列へは呼び出し元の `match (&column.ty, lit)`
        // 側で構造的に到達しない（`(ColumnType::Numeric{..}, lit)` の catch-all
        // が全 `InsertLiteral` variant を本関数へ委譲するため、ここで拒否する）。
        InsertLiteral::Vector(_) => {
            return Err(SqlSurfaceError::invalid_input(format!(
                "column {name:?} expects a NUMERIC literal, got a vector literal"
            )))
        }
        // 呼び出し元（`bind_insert`／`bind_set_assignments`／
        // `bind_upsert_assignments`）はいずれも `InsertLiteral::Null` を
        // 本関数へ渡すより前に nullable 判定込みで独自に処理するため実際には
        // 到達しないが、`InsertLiteral` は 5 variant の列挙であり本 match の
        // 網羅性のためだけに存在する（Issue #889 レビュー指摘・PR #1014 で
        // `Null` variant が追加された後の到達性を fail-closed に保つ）。
        InsertLiteral::Null => {
            return Err(SqlSurfaceError::invalid_input(format!(
                "column {name:?} does not accept an explicit NULL literal here"
            )))
        }
    };
    match crate::numeric::parse_for_column(text, precision, scale) {
        Ok(d) => Ok(crate::row_codec::Value::Numeric(d)),
        Err(crate::numeric::NumericError::Malformed(detail)) => Err(
            SqlSurfaceError::invalid_input(format!("column {name:?}: {detail}")),
        ),
        Err(crate::numeric::NumericError::OutOfRange) => {
            Err(SqlSurfaceError::numeric_out_of_range(format!(
                "column {name:?} numeric value out of range for NUMERIC({precision},{scale})"
            )))
        }
    }
}

/// `[f1,f2,...]` 形式のベクトルリテラルを解析する（SQL-1）。
///
/// 検証順序: (1) 生バイト長が [`MAX_VECTOR_LITERAL_BYTES`] を超えないこと
/// （超過は [`SqlSurfaceError::PayloadTooLarge`]。カンマ分割・`Vec<f32>` 確保より前に
/// 行う）。(2) `[`〜`]` で囲まれていること。(3) 各要素が `f32` としてパース可能かつ
/// 有限であること。(4) 要素数が `expected_dim` と一致すること。(2)〜(4) の違反は
/// [`SqlSurfaceError::InvalidInput`]。
pub fn parse_vector_literal(literal: &str, expected_dim: u32) -> Result<Vec<f32>, SqlSurfaceError> {
    if literal.len() > MAX_VECTOR_LITERAL_BYTES {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "vector literal length {} exceeds limit {MAX_VECTOR_LITERAL_BYTES}",
            literal.len()
        )));
    }

    let trimmed = literal.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| {
            SqlSurfaceError::invalid_input("vector literal must be of the form [f1,f2,...]")
        })?;

    let mut values: Vec<f32> = Vec::new();
    if !inner.trim().is_empty() {
        for part in inner.split(',') {
            let part = part.trim();
            // `crate::json::parse_f32_text` と同一の実装を経由する（NoSQL 表層
            // `engine::json::JsonNumber::as_f32` が JSON 数値リテラルを `f32` へ
            // 変換する際の丸めと単一実装を共有し、表層横断で同一リテラル・
            // 同一 `operation_id` を再送した際の `content_hash` 一致を保証する。
            // Issue #771 レビュー指摘対応）。
            let v: f32 = crate::json::parse_f32_text(part).map_err(|_| {
                SqlSurfaceError::invalid_input(format!(
                    "vector literal element is not a number: {part:?}"
                ))
            })?;
            if !v.is_finite() {
                return Err(SqlSurfaceError::invalid_input(
                    "vector literal element must be finite (NaN/Inf are not allowed)",
                ));
            }
            values.push(v);
        }
    }

    let dim = u32::try_from(values.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large(format!(
            "vector literal element count {} exceeds representable range",
            values.len()
        ))
    })?;
    if dim != expected_dim {
        return Err(SqlSurfaceError::invalid_input(format!(
            "vector literal dimension mismatch: expected {expected_dim}, got {dim}"
        )));
    }

    Ok(values)
}

/// [`InsertLiteral::Vector`]（NoSQL 表層が JSON 配列から直接構築する、要素
/// ごとに [`engine::json::JsonNumber::as_f32`] で有限値と確認済みの `f32` 列。
/// Issue #896 レビュー指摘・PR #1038）を [`crate::row_codec::Value::Vector`]
/// へ束縛する。[`parse_vector_literal`] のテキスト長上限（64 KiB。SQL
/// リテラルの構文上の制約であり、JSON 配列から届く既に解析済みの数値列には
/// 適用対象が無い）を経由せずに構築するが、次元一致・各要素の有限性は
/// engine 側で改めて検証する（wire-server の検証結果を無条件に信頼せず、
/// engine を唯一の検証点に保つための多層防御。`security.md`「アクセス制御の
/// 不備」観点）。
fn bind_vector_literal_values(
    values: &[f32],
    expected_dim: u32,
    name: &str,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    let dim = u32::try_from(values.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large(format!(
            "column {name:?}: vector element count {} exceeds representable range",
            values.len()
        ))
    })?;
    if dim != expected_dim {
        return Err(SqlSurfaceError::invalid_input(format!(
            "column {name:?}: vector dimension mismatch: expected {expected_dim}, got {dim}"
        )));
    }
    for v in values {
        if !v.is_finite() {
            return Err(SqlSurfaceError::invalid_input(format!(
                "column {name:?}: vector element must be finite (NaN/Inf are not allowed)"
            )));
        }
    }
    Ok(crate::row_codec::Value::Vector(values.to_vec()))
}

/// `INTEGER`／`BIGINT` 列（Issue #881・TABLE-13・TASK-196）向けの数値リテラル
/// 束縛。`literal` は `InsertLiteral::Number`（`allowlist::expect_literal` が
/// 単項マイナスを正規化済み）のみを受理し、`InsertLiteral::String` は
/// `22000`（PG 互換の暗黙変換は行わない設計判断）で拒否する。範囲外
/// （`i32::MIN..=i32::MAX`／`i64::MIN..=i64::MAX`）は `22003`
/// （[`SqlSurfaceError::numeric_out_of_range`]）、小数点・16 進数等の非整数形式は
/// `22000` で拒否する。エラーメッセージには列名のみを含め、リテラル本文は含めない
/// （長大な数字列の反射防止）。
pub(crate) fn bind_integer_literal(
    name: &str,
    ty: ColumnType,
    literal: &InsertLiteral,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    let raw = match literal {
        InsertLiteral::Number(s) => s,
        InsertLiteral::String(_)
        | InsertLiteral::Bool(_)
        | InsertLiteral::Null
        | InsertLiteral::Vector(_) => {
            return Err(SqlSurfaceError::invalid_input(format!(
                "column {name:?} expects an integer literal, got a non-integer literal"
            )))
        }
    };
    match ty {
        ColumnType::Integer => match raw.parse::<i32>() {
            Ok(v) => Ok(crate::row_codec::Value::Integer(v)),
            Err(e) => match e.kind() {
                std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                    Err(SqlSurfaceError::numeric_out_of_range(format!(
                        "value out of range for INTEGER column {name:?}"
                    )))
                }
                _ => Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects an integer literal"
                ))),
            },
        },
        ColumnType::BigInt => match raw.parse::<i64>() {
            Ok(v) => Ok(crate::row_codec::Value::BigInt(v)),
            Err(e) => match e.kind() {
                std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                    Err(SqlSurfaceError::numeric_out_of_range(format!(
                        "value out of range for BIGINT column {name:?}"
                    )))
                }
                _ => Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects an integer literal"
                ))),
            },
        },
        ColumnType::Text
        | ColumnType::Vector(_)
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
        | ColumnType::Uuid => Err(SqlSurfaceError::Internal {
            detail: "bind_integer_literal called for a non-integer column".to_string(),
        }),
    }
}

/// `REAL`／`DOUBLE PRECISION` 列（TABLE-13・TASK-196）のリテラル束縛を 1 箇所へ
/// 集約するヘルパー（F7・Issue #882 計画）。`raw`（`InsertLiteral::Number` の
/// 生テキスト。負号は `expect_literal` が既に前置済み）を
/// [`crate::scalar_float`] の閉じた文法で解析し、`Malformed` は `22000`
/// （既存の型不一致と同じ `InvalidInput`）、`OutOfRange`（非有限化・非ゼロ
/// アンダーフロー）は `22003`（`NumericOutOfRange`）へ写像する。
pub(crate) fn bind_real_literal(raw: &str) -> Result<f32, SqlSurfaceError> {
    crate::scalar_float::parse_real(raw).map_err(|e| match e {
        crate::scalar_float::ParseFloatError::Malformed => {
            SqlSurfaceError::invalid_input(format!("malformed REAL literal: {raw:?}"))
        }
        crate::scalar_float::ParseFloatError::OutOfRange => {
            SqlSurfaceError::numeric_out_of_range(format!("REAL literal out of range: {raw:?}"))
        }
    })
}

/// [`bind_real_literal`] の `DOUBLE PRECISION` 版。
pub(crate) fn bind_double_literal(raw: &str) -> Result<f64, SqlSurfaceError> {
    crate::scalar_float::parse_double(raw).map_err(|e| match e {
        crate::scalar_float::ParseFloatError::Malformed => {
            SqlSurfaceError::invalid_input(format!("malformed DOUBLE PRECISION literal: {raw:?}"))
        }
        crate::scalar_float::ParseFloatError::OutOfRange => SqlSurfaceError::numeric_out_of_range(
            format!("DOUBLE PRECISION literal out of range: {raw:?}"),
        ),
    })
}

/// `DATE`／`TIMESTAMP` 列（TABLE-13・TASK-197、Issue #884）向けの文字列リテラル
/// 束縛。INSERT／UPDATE SET／UPSERT の 3 経路（[`bind_insert_row`]・
/// [`bind_set_assignments`]・[`bind_upsert_assignments`]）が共有する単一情報源。
/// 文法違反（[`crate::datetime::DateTimeLiteralError::Format`]）は既存の
/// `InvalidInput`（`22000`）へ、範囲外・暦上不正
/// （[`crate::datetime::DateTimeLiteralError::Overflow`]）は
/// [`SqlSurfaceError::DatetimeFieldOverflow`]（`22008`）へ写像する（D-1。
/// `docs/design/datetime-column.md` 参照）。
pub(crate) fn bind_datetime_literal(
    column_name: &str,
    ty: ColumnType,
    literal: &str,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    match ty {
        ColumnType::Date => match crate::datetime::parse_date(literal) {
            Ok(days) => Ok(crate::row_codec::Value::Date(days)),
            Err(crate::datetime::DateTimeLiteralError::Format(detail)) => Err(
                SqlSurfaceError::invalid_input(format!("column {column_name:?}: {detail}")),
            ),
            Err(crate::datetime::DateTimeLiteralError::Overflow(detail)) => {
                Err(SqlSurfaceError::datetime_field_overflow(format!(
                    "column {column_name:?}: {detail}"
                )))
            }
        },
        ColumnType::Timestamp => match crate::datetime::parse_timestamp(literal) {
            Ok(micros) => Ok(crate::row_codec::Value::Timestamp(micros)),
            Err(crate::datetime::DateTimeLiteralError::Format(detail)) => Err(
                SqlSurfaceError::invalid_input(format!("column {column_name:?}: {detail}")),
            ),
            Err(crate::datetime::DateTimeLiteralError::Overflow(detail)) => {
                Err(SqlSurfaceError::datetime_field_overflow(format!(
                    "column {column_name:?}: {detail}"
                )))
            }
        },
        ColumnType::Text
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Vector(_)
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Boolean
        | ColumnType::Array(_)
        | ColumnType::Bytea
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Numeric { .. }
        | ColumnType::Enum(_)
        | ColumnType::Uuid => {
            // 呼び出し元（3 経路の `match (column.ty, literal)`）は Date/Timestamp
            // の腕でのみこの関数を呼ぶ契約のため到達しない（fail-closed の保険腕）。
            Err(SqlSurfaceError::invalid_input(format!(
                "column {column_name:?} is not a DATE/TIMESTAMP column"
            )))
        }
    }
}

/// `{v1,v2,...}` 形式の配列リテラルを解析する（TABLE-14・TASK-198、Issue #888・
/// D-A5）。字句解析器（`Token`）は変更せず、`VECTOR` と同じく文字列リテラル
/// `'{...}'` を束縛時に解釈する専用パーサー。
///
/// 検証順序: (1) 生バイト長が [`MAX_ARRAY_LITERAL_BYTES`] を超えないこと（超過は
/// [`SqlSurfaceError::PayloadTooLarge`]。走査より前に行う）。(2) `{`〜`}` で
/// 囲まれていること。(3) 1 パスの状態機械で要素を切り出し、要素数が
/// `array_ty.max_len()` を超えた時点で打ち切る（超過は `PayloadTooLarge`）。
/// (4) 要素型ごとに変換する（`BOOLEAN` は `t|f|true|false` を大小無視で受理）。
/// (2)〜(4) の形式違反（入れ子の `{`、閉じていない引用、末尾カンマ、`BOOLEAN` の
/// 不正語）はすべて [`SqlSurfaceError::InvalidInput`]。NULL 要素（引用なしの
/// `NULL`。大小無視）は D-A6 により本版では受理せず `InvalidInput`。引用つきの
/// `"NULL"` は TEXT 要素の文字列 `NULL` として扱う。
pub fn parse_array_literal(
    literal: &str,
    array_ty: crate::catalog::ArrayType,
) -> Result<crate::row_codec::ArrayValue, SqlSurfaceError> {
    if literal.len() > MAX_ARRAY_LITERAL_BYTES {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "array literal length {} exceeds limit {MAX_ARRAY_LITERAL_BYTES}",
            literal.len()
        )));
    }

    let trimmed = literal.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| {
            SqlSurfaceError::invalid_input("array literal must be of the form {v1,v2,...}")
        })?;

    // `[]` 添字ではなく `Peekable<Chars>` を使う（coding-rust.md「untrusted
    // 入力の扱い」: 受信データ経路では添字アクセスを禁止）。`Vec<char>` への
    // 事前 collect（リテラル長の最大 4 倍のスクラッチ確保）も避け、`inner` を
    // 1 パスで走査する。
    let mut raw_elements: Vec<String> = Vec::new();
    if !inner.trim().is_empty() {
        let mut chars = inner.chars().peekable();
        loop {
            // 要素先頭の空白を読み飛ばす。
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            let Some(&first) = chars.peek() else {
                return Err(SqlSurfaceError::invalid_input(
                    "array literal has a trailing comma or is empty after a comma",
                ));
            };
            let element = if first == '"' {
                // 引用要素: バックスラッシュエスケープ（`\"`・`\\`）を解釈し、
                // 閉じていない引用は拒否する。
                chars.next();
                let mut s = String::new();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    if c == '\\' {
                        let escaped = chars.next().ok_or_else(|| {
                            SqlSurfaceError::invalid_input(
                                "array literal has an unterminated escape sequence",
                            )
                        })?;
                        s.push(escaped);
                    } else if c == '"' {
                        closed = true;
                        break;
                    } else {
                        s.push(c);
                    }
                }
                if !closed {
                    return Err(SqlSurfaceError::invalid_input(
                        "array literal has an unterminated quoted element",
                    ));
                }
                s
            } else {
                // 引用なし要素: 次のカンマ（またはリテラル末尾）までを前後の
                // 空白を除いて要素とする。入れ子の `{`/`}` は非対応。
                let mut raw = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ',' {
                        break;
                    }
                    if c == '{' || c == '}' {
                        return Err(SqlSurfaceError::invalid_input(
                            "array literal must not contain nested braces",
                        ));
                    }
                    raw.push(c);
                    chars.next();
                }
                let trimmed_raw = raw.trim();
                if trimmed_raw.eq_ignore_ascii_case("null") {
                    return Err(SqlSurfaceError::invalid_input(
                        "array literal does not support NULL elements",
                    ));
                }
                if trimmed_raw.is_empty() {
                    return Err(SqlSurfaceError::invalid_input(
                        "array literal has an empty unquoted element (use \"\" for an empty string)",
                    ));
                }
                trimmed_raw.to_string()
            };

            // 要素数上限は確保（`push`）の**前**に検査する（無制限な `Vec`
            // 成長を避ける。D-A5）。
            if raw_elements.len() as u64 >= array_ty.max_len() as u64 {
                return Err(SqlSurfaceError::payload_too_large(format!(
                    "array literal element count exceeds limit {}",
                    array_ty.max_len()
                )));
            }
            raw_elements.push(element);

            // 要素の直後は空白を挟んでカンマか閉じ（走査終端）のいずれか。
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            match chars.peek() {
                None => break,
                Some(',') => {
                    chars.next();
                }
                Some(_) => {
                    return Err(SqlSurfaceError::invalid_input(
                        "array literal element is not properly delimited by a comma",
                    ))
                }
            }
        }
    }

    match array_ty.elem() {
        crate::catalog::ArrayElemType::Text => Ok(crate::row_codec::ArrayValue::Text(raw_elements)),
        crate::catalog::ArrayElemType::Bool => {
            let mut items: Vec<bool> = Vec::new();
            items
                .try_reserve_exact(raw_elements.len())
                .map_err(|_| SqlSurfaceError::Internal {
                    detail: "failed to reserve array literal elements".to_string(),
                })?;
            for raw in &raw_elements {
                let b = if raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("t") {
                    true
                } else if raw.eq_ignore_ascii_case("false") || raw.eq_ignore_ascii_case("f") {
                    false
                } else {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "array literal boolean element is not true/false: {raw:?}"
                    )));
                };
                items.push(b);
            }
            Ok(crate::row_codec::ArrayValue::Bool(items))
        }
    }
}

/// スキーマの唯一の `VECTOR` 列（インデックス・宣言次元）を返す。`VECTOR` 列を
/// 持たないテーブルは束縛不能（`catalog.rs::validate_schema` が「`VECTOR` 列は
/// 高々 1 つ」を DDL 時点で強制済みのため、複数該当は構造上起こらない）。
/// `sql::using_plan`（TASK-77・SQL-5）が `Embedder` の返すベクトルの次元検証にも
/// 使うため `pub(crate)`。
pub(crate) fn vector_column(schema: &TableSchema) -> Result<(usize, u32), SqlSurfaceError> {
    schema
        .columns
        .iter()
        .enumerate()
        .find_map(|(idx, c)| match &c.ty {
            ColumnType::Vector(dim) => Some((idx, *dim)),
            ColumnType::Text
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
            | ColumnType::Uuid => None,
        })
        .ok_or_else(|| SqlSurfaceError::invalid_input("table has no VECTOR column"))
}

/// NoSQL 表層（Issue #763・TASK-175・NOSQL-2）の `search.plan` 経路向け。`VECTOR`
/// 列の存在のみを束縛時点で検査する公開ラッパー（次元照合は `Embedder` 出力を
/// 要するため #764 の担当）。SQL 表層の `USING PLAN` が
/// `sql::using_plan::pre_check_bindable`／`bind_expansion` で `vector_column
/// (schema)?` を同じ理由で呼び `22000` へ拒否するのと同一の契約を、`VECTOR`
/// 列なしテーブルへの `plan` 受理を束縛時点で塞ぐために wire-server 側へ
/// 公開する（[`bind_body_text_column`] が `text_column_index` を公開する
/// のと同じ判断）。
pub fn require_vector_column(schema: &TableSchema) -> Result<(), SqlSurfaceError> {
    vector_column(schema).map(|_| ())
}

/// `name` に一致する `Text` 列のインデックスを返す（`id` 疑似列は対象外）。
/// `sql::using_plan`（TASK-77・SQL-5）が本文列（規約列 `body`）の解決にも使う
/// ため `pub(crate)`。
pub(crate) fn text_column_index(
    schema: &TableSchema,
    name: &str,
) -> Result<usize, SqlSurfaceError> {
    schema
        .columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))
        .and_then(|idx| {
            let column = schema
                .columns
                .get(idx)
                .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
            match &column.ty {
                ColumnType::Text => Ok(idx),
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
                    "column {name:?} is not a TEXT column"
                ))),
            }
        })
}

/// NoSQL 表層（`wire-server::http::query::search`。Issue #763・TASK-175・
/// NOSQL-2）向けの投影束縛ヘルパー。SQL 表層の `SELECT` リスト構文を経由せず、
/// JSON クエリオブジェクトの `columns`（`Option<&[String]>`）を直接
/// [`bind_projection`] の入力形（[`Projection`]）へ組み立てて委譲する薄い
/// ラッパー。`columns` 省略（`None`）は `SELECT *` と同じ [`Projection::All`]
/// へ、指定時は列名リストをそのまま [`Projection::Columns`] へ写像する。
/// NoSQL 表層に式項目（UDF 呼び出し）は存在しないため空の
/// [`crate::sql::udf_call::UdfRegistry`] で足りる（`bind_projection` が返す
/// [`ProjectedColumn::Computed`] へは到達しない）。実カラム優先・疑似列
/// `id`・未知列 `22000` の判定規則は SQL 表層と完全に共有する
/// （第 2 の実行器を作らない方針）。
pub fn bind_column_projection(
    columns: Option<&[String]>,
    schema: &TableSchema,
) -> Result<Vec<ProjectedColumn>, SqlSurfaceError> {
    let projection = match columns {
        None => Projection::All,
        Some(names) => Projection::Columns(names.to_vec()),
    };
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    bind_projection(
        &projection,
        schema,
        &crate::sql::udf_call::UdfRegistry::default(),
        &mut node_budget,
    )
}

/// `RETURNING` 句（Issue #873・SQL-21）の投影束縛。`INSERT`／`DELETE`（単一行）が
/// 実行結線済みのため、`sql::allowlist::ValidatedInsert::returning`／
/// `sql::allowlist::ValidatedDelete::returning` の `Option<Projection>` を
/// [`bind_column_projection`] と同じ [`bind_projection`] へ委譲する（第 2 の
/// 投影実装を作らない）。UDF レジストリを持たないため `Projection::Items`
/// （関数呼び出し項目）は多層防御として `42601` で拒否する——`sql::allowlist::
/// Parser::parse_returning_clause` が構造検証段で既に同じ判定を行っており、
/// 通常はここへ到達しない契約。
pub fn bind_returning(
    returning: Option<&Projection>,
    schema: &TableSchema,
) -> Result<Option<Vec<ProjectedColumn>>, SqlSurfaceError> {
    let Some(projection) = returning else {
        return Ok(None);
    };
    if matches!(projection, Projection::Items(_)) {
        return Err(SqlSurfaceError::unsupported(
            "RETURNING does not support function-call items",
        ));
    }
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    bind_projection(
        projection,
        schema,
        &crate::sql::udf_call::UdfRegistry::default(),
        &mut node_budget,
    )
    .map(Some)
}

/// NoSQL 表層（Issue #763・TASK-175・NOSQL-2）向けのベクトル値束縛ヘルパー。
/// `search.vector`（JSON 数値配列）の各要素を呼び出し元が
/// `engine::json::JsonNumber::as_f32`（SQL 表層 [`parse_vector_literal`] と
/// 同一の丸め――生リテラル文字列を直接 `f32` へ変換する単一丸め――を経由する
/// 唯一の実装）で `f32` へ変換した後の `values` を受け取り、次元一致のみを
/// 検証して `Vec<f32>` へ束ねる。
///
/// 以前は `&[f64]` を受け取り本関数内で `as f32` キャストしていたが、
/// `JsonNumber::as_f64() -> f64` を経由する呼び出し元が存在すると
/// `str -> f64 -> f32` の 2 回丸めになり、SQL 表層の `str -> f32` 単一丸めと
/// 異なる結果になりうる（`json.rs::JsonNumber::Float` のドキュメンテーション
/// コメント参照）。表層横断で同一リテラルが同一の `f32` になることを
/// 呼び出し元の変換方法に依存させないため、本関数の入力型を `&[f32]` へ
/// 変更した（Issue #771 レビュー指摘対応）。非有限判定は呼び出し元
/// （`JsonNumber::as_f32` が非有限を `None` として弾く）が既に行っている
/// 前提だが、多層防御としてここでも再検査する。
///
/// 検証順序: (1) `values.len()` を [`vector_column`] が返す宣言次元
/// （`u32`）と照合する（次元不一致は [`SqlSurfaceError::invalid_input`]。
/// `values.len()` が `u32::MAX` を超える場合も同じ分岐で拒否する）。
/// (2) 各要素が有限であることを再検査する（[`parse_vector_literal`] の
/// (3)(4) と同じ判定）。
pub fn bind_vector_values(
    values: &[f32],
    schema: &TableSchema,
) -> Result<Vec<f32>, SqlSurfaceError> {
    let (_vec_idx, vec_dim) = vector_column(schema)?;
    let len = u32::try_from(values.len()).map_err(|_| {
        SqlSurfaceError::invalid_input(format!(
            "vector value count {} exceeds representable range",
            values.len()
        ))
    })?;
    if len != vec_dim {
        return Err(SqlSurfaceError::invalid_input(format!(
            "vector dimension mismatch: expected {vec_dim}, got {len}"
        )));
    }
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        if !value.is_finite() {
            return Err(SqlSurfaceError::invalid_input(
                "vector element must be finite (NaN/Inf are not allowed)",
            ));
        }
        out.push(*value);
    }
    Ok(out)
}

/// NoSQL 表層（Issue #763・TASK-175・NOSQL-2）向けの本文列束縛ヘルパー。
/// `search.hybrid.text`（JSON 形状には対象列名が無いため）の疎側入力列を、
/// `USING PLAN`（TASK-77・SQL-5）と同じ規約列 [`crate::sql::using_plan::
/// BODY_COLUMN_NAME`]（`body`）へ固定して解決する。欠落・非 `TEXT` 列は
/// [`text_column_index`] と同じ `22000` で拒否する（spec 側への申し送り:
/// `hybrid` の対象テキスト列を JSON 形状で選択可能にするかは spec 判断）。
pub fn bind_body_text_column(schema: &TableSchema) -> Result<usize, SqlSurfaceError> {
    text_column_index(schema, crate::sql::using_plan::BODY_COLUMN_NAME)
}

/// `ORDER BY` 式（[`OrderByForm`]）を [`Ranking`] へ束縛する。
///
/// `validate_literal` が `false` の場合、ベクトルリテラル文字列の実パース
/// （[`parse_vector_literal`]）を省略し、対象列がテーブルの `VECTOR` 列で
/// あることの構造検証のみ行う（返る `Ranking` の `query` は空——実行には
/// 使わない呼び出し元専用）。Describe（Bind 前の結果列導出。
/// [`bind_projection_for_describe`]）専用の縮退経路であり、通常の実行系
/// （[`bind_in_session`]）は常に `true` を渡す（対象ビヘイビア: Issue #935・
/// WIRE-12・TASK-217。PR #1012 Cursor Bugbot 指摘: `parse_sql_prepared` が
/// `$n` を構造検証専用の固定ダミー値〔`substitute_dummy`〕へ置換するため、
/// `ORDER BY <vec列> <=> $n` を含む文の Describe はダミー値がベクトルとして
/// 不正でも結果列だけは導出できる必要がある——結果列は投影列にのみ依存し
/// ランキングの実値には依存しないため、この省略は安全）。
fn bind_ranking(
    order_by: &OrderByForm,
    schema: &TableSchema,
    validate_literal: bool,
) -> Result<Ranking, SqlSurfaceError> {
    let (vec_idx, vec_dim) = vector_column(schema)?;
    match order_by {
        OrderByForm::Distance { column, literal } => {
            let column_idx = schema
                .columns
                .iter()
                .position(|c| &c.name == column)
                .ok_or_else(|| {
                    SqlSurfaceError::invalid_input(format!("unknown column: {column}"))
                })?;
            if column_idx != vec_idx {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {column:?} is not the table's VECTOR column"
                )));
            }
            let query = if validate_literal {
                parse_vector_literal(literal, vec_dim)?
            } else {
                Vec::new()
            };
            Ok(Ranking::Distance { query })
        }
        OrderByForm::FunctionCall { args, .. } => {
            // 4 引数形（<vec列>, '<vec リテラル>', <text列>, '<query text>'）のみ
            // 実行可能。2 引数形は allowlist（TASK-74）が構造としては受理するが、
            // 密側クエリベクトルをテキストから導出する経路を engine は持たないため
            // 実行不能として拒否する（advisor 方針: 既存 2 引数形の受理自体は
            // 変更しない。実行不能の判定はこの束縛層に閉じる）。
            let (vec_col, vec_literal, text_col, query_text) = match args.as_slice() {
                [FunctionArg::Ident(vec_col), FunctionArg::StringLiteral(vec_literal), FunctionArg::Ident(text_col), FunctionArg::StringLiteral(query_text)] => {
                    (vec_col, vec_literal, text_col, query_text)
                }
                [FunctionArg::Ident(_), FunctionArg::StringLiteral(_)] => {
                    return Err(SqlSurfaceError::invalid_input(
                        "hybrid ORDER BY function requires 4 arguments (vector column, vector literal, text column, query text); the 2-argument form is not executable",
                    ));
                }
                _ => {
                    return Err(SqlSurfaceError::invalid_input(
                        "unsupported hybrid ORDER BY function argument shape",
                    ));
                }
            };
            let vec_col_idx = schema
                .columns
                .iter()
                .position(|c| c.name == *vec_col)
                .ok_or_else(|| {
                    SqlSurfaceError::invalid_input(format!("unknown column: {vec_col}"))
                })?;
            if vec_col_idx != vec_idx {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {vec_col:?} is not the table's VECTOR column"
                )));
            }
            let query = if validate_literal {
                parse_vector_literal(vec_literal, vec_dim)?
            } else {
                Vec::new()
            };
            let text_column_index = text_column_index(schema, text_col)?;
            Ok(Ranking::Hybrid {
                query,
                text_column_index,
                query_text: query_text.clone(),
            })
        }
        // `USING PLAN(...)`（TASK-77・SQL-5）が構文上選ばれた文には `ORDER BY` 節が
        // 存在しない。`core.rs::EngineCore::execute_sql_in_session` は
        // `ValidatedStatement::using_plan` が `Some` の場合に本関数（通常の
        // `bind_in_session` 経路）を呼ばず `sql::using_plan` へ分岐するため、この
        // アームへ到達するのは分岐条件が壊れた場合のみ。黙って既定のランキングへ
        // 縮退させず、内部エラーとして拒否する（fail-closed）。
        OrderByForm::UsingPlan => Err(SqlSurfaceError::Internal {
            detail: "OrderByForm::UsingPlan must not reach bind_ranking (dispatch bug)".to_string(),
        }),
    }
}

/// [`ValidatedStatement`] を `schema` と照合して [`BoundStatement`] へ束縛する
/// （TASK-75 の公開 API）。`schema` は呼び出し元（`core.rs::EngineCore::execute_sql`）が
/// `Storage::get_table_schema` で取得済みのものを渡す。セッション変数を持たない
/// エントリポイント向けの後方互換 API で、[`bind_with_session`]（TASK-161）へ
/// `session_mode: None` で委譲する。
/// `AS <alias>` を省略した SELECT 式項目の既定列名（TASK-79・SQL-9）。頂点の
/// 関数呼び出し名を使う（`Expr::Call` 以外が頂点の式は SELECT リストの構文上
/// 現れない＝`allowlist::Parser::parse_select_item` は必ず `ident '(' ... ')'` から
/// 式項目を作るため `name` は常に取得できる）。
fn default_expr_alias(expr: &Expr) -> String {
    match expr {
        Expr::Call { name, .. } => name.clone(),
        _ => "expr".to_string(),
    }
}

/// SELECT リストの許可形状（[`Projection`]）を束縛する共通ヘルパー（TASK-77・
/// SQL-5 で `bind_in_session` から切り出した。`USING PLAN` 経路（`sql::using_plan`）も
/// 同一の投影列解決規則（実カラム優先・疑似列 `id`・`AS` エイリアス付き式項目）を
/// 必要とするため、この 1 箇所に集約する）。
///
/// TASK-186・NOSQL-3（Issue #766）で `pub(crate)` から `pub` へ昇格した:
/// `wire-server` の NoSQL 表層 `scan` 写像（`http::query::scan`）が SQL テキストを
/// 経由せず `Projection` を直接組み立てて束縛するために、[`bind_scan`] と
/// 同一の投影列解決規則をこの 1 箇所から共有する（第 2 の実装を作らない）。
pub fn bind_projection(
    projection: &Projection,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    node_budget: &mut usize,
) -> Result<Vec<ProjectedColumn>, SqlSurfaceError> {
    match projection {
        Projection::All => {
            let mut cols = Vec::with_capacity(schema.columns.len() + 1);
            cols.push(ProjectedColumn::Id);
            for (index, column) in schema.columns.iter().enumerate() {
                cols.push(ProjectedColumn::Column {
                    index,
                    name: column.name.clone(),
                });
            }
            Ok(cols)
        }
        Projection::Columns(names) => {
            let mut cols = Vec::with_capacity(names.len());
            for name in names {
                // カタログ上の実カラムを疑似列 `id` より優先して照合する（Issue #56
                // レビュー指摘対応: 以前は `name == "id"` を先に判定していたため、
                // スキーマが `id` という実カラムを持っていても常に行キー疑似列へ
                // マップされ、実カラムの値を `SELECT id` で取得する経路がなかった）。
                if let Some(index) = schema.columns.iter().position(|c| &c.name == name) {
                    cols.push(ProjectedColumn::Column {
                        index,
                        name: name.clone(),
                    });
                    continue;
                }
                if name == "id" {
                    cols.push(ProjectedColumn::Id);
                    continue;
                }
                return Err(SqlSurfaceError::invalid_input(format!(
                    "unknown column: {name}"
                )));
            }
            Ok(cols)
        }
        Projection::Items(items) => {
            let mut cols = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    crate::sql::allowlist::SelectItem::Column(name) => {
                        if let Some(index) = schema.columns.iter().position(|c| &c.name == name) {
                            cols.push(ProjectedColumn::Column {
                                index,
                                name: name.clone(),
                            });
                            continue;
                        }
                        if name == "id" {
                            cols.push(ProjectedColumn::Id);
                            continue;
                        }
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "unknown column: {name}"
                        )));
                    }
                    crate::sql::allowlist::SelectItem::Expr { expr, alias } => {
                        let (bound, _ty) =
                            crate::sql::udf_call::bind_expr(expr, schema, udfs, node_budget)?;
                        let name = alias.clone().unwrap_or_else(|| default_expr_alias(expr));
                        cols.push(ProjectedColumn::Computed { name, expr: bound });
                    }
                }
            }
            Ok(cols)
        }
    }
}

/// `WHERE` 句の許可述語列（[`WherePredicate`]）を束縛する共通ヘルパー
/// （TASK-166・SQL-13 で `bind_in_session` から切り出した。検索 SELECT
/// （[`bind_in_session`]）・集計 SELECT（[`bind_aggregate`]）の両方が同一の
/// 意味論（等価・前方一致条件は `declarative_filter::bind_all` に集約、`visible()`
/// はフラグのみ、式述語は `Bool` 型を要求）で WHERE を解釈する必要があるため、
/// 挙動を複製せずこの 1 箇所に集約する。戻り値は
/// `(metadata_filters, expr_filters, rls_predicate_present)` の組。
///
/// `dummy_equality_flags`（PR #1012 Cursor Bugbot 指摘対応。Issue #935・
/// WIRE-12・TASK-217）: `where_predicates` 中に現れる
/// [`WherePredicate::Equality`] を出現順に数えた添字で、その値が
/// `sql::params::substitute_dummy` による `$n` 由来の固定ダミー文字列かどうか
/// を示す。`bind_in_session`（Bind／Execute）は常に `&[]`（すべて実値として
/// 検証）を渡し、`bind_projection_for_describe`（Prepared Describe 専用）のみ
/// `core.rs::PreparedSql` が保持する事前計算済みフラグを渡す（`$n` を含まない
/// 通常の Describe では自然に空になる）。`true` の位置に限り ENUM 列の
/// 語彙照合（`declarative_filter::bind_all_for_describe`）を Describe 時点では
/// 省略する——実際に不正なラベルが束縛された場合は Bind／Execute で
/// `22P02` として検出される（PR #1012 の vector literal 修正と同じ方針。
/// `WherePredicate::Prefix`／`BoolEquality`／`BoolColumn` の右辺値は `$n` に
/// 束縛できない〔`sql::params` モジュールドキュメント〕ため対象外）。
/// `ty` が範囲比較（[`declarative_filter::FilterOp::TypedCompare`]。TABLE-13・
/// TASK-199、Issue #891・レーン B）の対象列型かどうかを判定する。`=` の
/// [`WherePredicate::Equality`] をどちらの経路（TEXT/ENUM 向け `equals`・
/// 非数値型向け `compare`）へ振り分けるかの単一情報源。算術を持つ
/// INTEGER/BIGINT/REAL/DOUBLE（レーン A。式評価系が担当）はここに含めない。
fn is_typed_compare_column_type(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::Numeric { .. }
            | ColumnType::Uuid
            | ColumnType::Bytea
    )
}

/// [`bind_where_predicates`]・[`bind_where_predicates_recursive`] の戻り値
/// （TASK-208・SQL-24、Issue #912）: `(metadata_filters, expr_filters,
/// rls_predicate_present, or_filters)`。clippy `type_complexity` 回避のための
/// 型別名（意味的なラップ型ではなく、そのままタプルとして分配束縛して使う）。
type BoundWherePredicates = (
    Vec<MetadataFilter>,
    Vec<crate::sql::udf_call::BoundExpr>,
    bool,
    Vec<crate::sql::where_tree::BoundOrGroup>,
);

pub(crate) fn bind_where_predicates(
    where_predicates: &[WherePredicate],
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    node_budget: &mut usize,
    dummy_equality_flags: &[bool],
) -> Result<BoundWherePredicates, SqlSurfaceError> {
    // `dummy_equality_flags` は述語ツリー全体（トップレベル・`Or` 分岐の
    // ネストを含む）を通じたソース出現順（深さ優先・左から右）の
    // `Equality` 通し番号で添字付けされる（`sql::params::
    // where_equality_literal_is_param` がトークン順で数える契約と一致させる
    // ため、`equality_ordinal` は再帰全体で 1 つのカウンタを共有する。
    // TASK-208・Issue #912）。
    let mut equality_ordinal: usize = 0;
    bind_where_predicates_recursive(
        where_predicates,
        schema,
        udfs,
        node_budget,
        dummy_equality_flags,
        &mut equality_ordinal,
    )
}

/// [`bind_where_predicates`] の再帰本体。トップレベルの述語列だけでなく、
/// [`WherePredicate::Or`] の各分岐（`AND` 列）を束縛するためにも自分自身を
/// 再帰的に呼ぶ（TASK-208・SQL-24、Issue #912）。
fn bind_where_predicates_recursive(
    where_predicates: &[WherePredicate],
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    node_budget: &mut usize,
    dummy_equality_flags: &[bool],
    equality_ordinal: &mut usize,
) -> Result<BoundWherePredicates, SqlSurfaceError> {
    let mut declarative_filters = Vec::with_capacity(where_predicates.len());
    let mut filter_skip_enum_validation = Vec::with_capacity(where_predicates.len());
    let mut expr_filters = Vec::new();
    let mut rls_predicate_present = false;
    let mut or_filters = Vec::new();
    for predicate in where_predicates {
        match predicate {
            WherePredicate::Equality { column, value } => {
                // `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の `=` は
                // 算術を持たない宣言的経路（レーン B。TABLE-13・TASK-199、
                // Issue #891）へ振り分ける。列が未知の場合（後続の `bind_all`
                // が「unknown column」で拒否する既存契約）はそのまま
                // `DeclarativeFilter::equals` へ流し、挙動を変えない。
                let is_typed_compare_column = schema
                    .columns
                    .iter()
                    .find(|c| &c.name == column)
                    .map(|c| is_typed_compare_column_type(&c.ty))
                    .unwrap_or(false);
                if is_typed_compare_column {
                    declarative_filters.push(DeclarativeFilter::compare(
                        column.clone(),
                        declarative_filter::CompareOp::Eq,
                        value.clone(),
                    ));
                } else {
                    declarative_filters
                        .push(DeclarativeFilter::equals(column.clone(), value.clone()));
                }
                filter_skip_enum_validation.push(
                    dummy_equality_flags
                        .get(*equality_ordinal)
                        .copied()
                        .unwrap_or(false),
                );
                *equality_ordinal += 1;
            }
            WherePredicate::Compare { column, op, value } => {
                // `< > <= >=`（TABLE-13・TASK-199、Issue #891・レーン B）。
                // `$n` はこの述語形の右辺に束縛できない（`sql::params` の
                // パターン 4 は `Ident '=' $n` のみ）ため、常に「実値」として
                // 扱う（Describe 専用のダミー値スキップは対象外）。
                declarative_filters.push(DeclarativeFilter::compare(
                    column.clone(),
                    (*op).into(),
                    value.clone(),
                ));
                filter_skip_enum_validation.push(false);
            }
            WherePredicate::Prefix { column, pattern } => {
                // SQL-24／TASK-208、Issue #914: `WherePredicate::Prefix`
                // （名前は互換性のため据え置き）は LIKE の生パターン全般を
                // 保持する。意味論・振り分け（Equals／StartsWith／Like）は
                // `declarative_filter::DeclarativeFilter::like`（内部で
                // `parse_like_pattern` を呼ぶ）に委ねる。
                declarative_filters.push(DeclarativeFilter::like(column.clone(), pattern.clone()));
                // `LIKE` パターン右辺には `$n` を束縛できない（`sql::params`
                // モジュールドキュメント。パターン 4 は `Ident '=' $n` のみ）ため
                // 常に「実値」として扱う。
                filter_skip_enum_validation.push(false);
            }
            WherePredicate::PredicateCall { .. } => {
                // allowlist が許可する述語呼び出し形は `visible()` のみ
                // （`is_allowed_where_predicate_name`）。名前の再検証はしない
                // （許可リスト層の責務。ここでは可観測性のためのフラグのみ立てる）。
                rls_predicate_present = true;
            }
            WherePredicate::BoolEquality { column, value } => {
                declarative_filters.push(DeclarativeFilter::bool_equals(column.clone(), *value));
                // `$n` は常に `Token::StringLiteral` へ置換されるため
                // `Ident '=' Ident("true"/"false")` の形にはならず、この述語の
                // 右辺も `$n` に由来し得ない。
                filter_skip_enum_validation.push(false);
            }
            WherePredicate::BoolColumn { column } => {
                declarative_filters.push(DeclarativeFilter::bool_equals(column.clone(), true));
                filter_skip_enum_validation.push(false);
            }
            WherePredicate::Expression(expr) => {
                let (bound, ty) = crate::sql::udf_call::bind_expr(expr, schema, udfs, node_budget)?;
                if ty != crate::sql::udf_call::ExprType::Bool {
                    return Err(SqlSurfaceError::invalid_input(
                        "WHERE expression must evaluate to a boolean (use a comparison)",
                    ));
                }
                expr_filters.push(bound);
            }
            WherePredicate::Or(branches) => {
                // TASK-208・SQL-24（Issue #912）: 各分岐を自分自身へ再帰的に
                // 束縛する。`equality_ordinal` は再帰全体で共有するカウンタを
                // そのまま渡し（ソース出現順＝深さ優先・左から右で数える契約）、
                // `node_budget` も共有する（式の総ノード数上限は述語ツリー全体
                // で 1 つ。`udf_call::MAX_EXPR_NODES` の既存契約を変えない）。
                let mut bound_branches = Vec::with_capacity(branches.len());
                for branch in branches {
                    let (branch_metadata, branch_expr, _branch_rls, branch_or) =
                        bind_where_predicates_recursive(
                            branch,
                            schema,
                            udfs,
                            node_budget,
                            dummy_equality_flags,
                            equality_ordinal,
                        )?;
                    bound_branches.push(crate::sql::where_tree::BoundConjunction::new(
                        branch_metadata,
                        branch_expr,
                        branch_or,
                    ));
                }
                or_filters.push(crate::sql::where_tree::BoundOrGroup::new(bound_branches));
            }
        }
    }
    let metadata_filters = declarative_filter::bind_all_for_describe(
        &declarative_filters,
        schema,
        &filter_skip_enum_validation,
    )?;
    Ok((
        metadata_filters,
        expr_filters,
        rls_predicate_present,
        or_filters,
    ))
}

pub fn bind(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
) -> Result<BoundStatement, SqlSurfaceError> {
    bind_with_session(stmt, schema, None)
}

/// 検索 `SELECT`（`ORDER BY`／`USING PLAN` いずれの経路も含む）の `LIMIT`
/// 生値 `raw` を検証し、`1..=`[`crate::core::MAX_SEARCH_K`] の範囲内であることを
/// 確認した `usize` を返す（`bind_in_session`・`crate::sql::using_plan::
/// bind_expansion`・`core.rs::EngineCore::execute_sql_in_session` の `USING PLAN`
/// 分岐が同一の検証ロジック・エラー文言・`wire_code`（`22000`）を共有する
/// 単一実装。`core.rs` 側は `plan_using_plan_expansion`〔辞書スナップショット
/// 構築・LLM クエリ展開・再埋め込み〕という高コスト I/O より前に本関数を呼び、
/// `bind_expansion` 側の呼び出しは多層防御として残す。codex-review P1 指摘
/// 対応、PR #266: 高コスト処理の後段でのみ検証すると、`LIMIT 0`／`LIMIT
/// 4294967295` のような必ず拒否される入力でも untrusted 入力によるリソース
/// 増幅を許してしまう）。
///
/// Issue #763（TASK-175・NOSQL-2）で `pub(crate)` から `pub` へ昇格した。
/// NoSQL 表層（`wire-server::http::query::search`）の `search.limit` 束縛も
/// SQL 表層と同一の範囲検証・`wire_code`（`22000`）を共有するため
/// （第 2 の実行器を作らない方針）。TASK-186・NOSQL-3（Issue #766）の `scan`
/// 写像（`http::query::scan`）も同一ロジックを再利用する（第 2 の実装を
/// 作らない）。
pub fn validate_search_limit(raw: u32) -> Result<usize, SqlSurfaceError> {
    let limit = usize::try_from(raw)
        .map_err(|_| SqlSurfaceError::invalid_input(format!("malformed LIMIT value: {raw}")))?;
    if limit == 0 || limit > crate::core::MAX_SEARCH_K {
        return Err(SqlSurfaceError::invalid_input(format!(
            "LIMIT {limit} out of range (must be 1..={})",
            crate::core::MAX_SEARCH_K
        )));
    }
    Ok(limit)
}

/// 広域取得・`GROUP BY` 集計の `OFFSET` 生値 `raw` を検証し、`0..=`
/// [`crate::core::MAX_SEARCH_K`] の範囲内であることを確認した `usize` を返す
/// （Issue #916・SQL-25 (b)・TASK-209）。`validate_search_limit` と異なり `0`
/// （no-op）を受理する。上限は LIMIT と同じ `MAX_SEARCH_K` を流用する
/// （GROUP BY 側の `MAX_GROUPS` とは独立。現行値はどちらも 10,000 だが、意味論的には
/// 「可視かつ WHERE 一致の行数」に対する上限であり `MAX_GROUPS`〔グループ数上限〕とは
/// 別軸のため）。`pub` にして NoSQL 表層の `offset` 写像（TASK-224・NOSQL-15）からも
/// 再利用できるようにする（第 2 の実装を作らない方針、`validate_search_limit` と
/// 同じ理由）。
pub fn validate_search_offset(raw: u32) -> Result<usize, SqlSurfaceError> {
    let offset = usize::try_from(raw)
        .map_err(|_| SqlSurfaceError::invalid_input(format!("malformed OFFSET value: {raw}")))?;
    if offset > crate::core::MAX_SEARCH_K {
        return Err(SqlSurfaceError::invalid_input(format!(
            "OFFSET {offset} out of range (must be 0..={})",
            crate::core::MAX_SEARCH_K
        )));
    }
    Ok(offset)
}

/// [`ValidatedStatement`] を `schema` と `session_mode`（呼び出し元の
/// [`crate::sql::mode::SessionState::search_mode`]）と照合して [`BoundStatement`] へ
/// 束縛する（TASK-161 の公開 API）。UDF レジストリを持たないエントリポイント向けの
/// 後方互換 API で、[`bind_in_session`]（TASK-79）へ空レジストリで委譲する。
pub fn bind_with_session(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
    session_mode: Option<SearchMode>,
) -> Result<BoundStatement, SqlSurfaceError> {
    bind_in_session(
        stmt,
        schema,
        session_mode,
        &crate::sql::udf_call::UdfRegistry::default(),
    )
}

/// [`ValidatedStatement`] を `schema`・`session_mode`・UDF レジストリ `udfs`
/// （呼び出し元の [`crate::sql::mode::SessionState::udfs`]）と照合して
/// [`BoundStatement`] へ束縛する（TASK-79・SQL-9 の公開 API。TASK-161 の
/// `bind_with_session` を UDF 呼び出しの束縛（結果列・`WHERE` 式述語）へ拡張した
/// もの）。`stmt.search_mode`（クエリ句 `USING MODE` の生リテラル）を
/// [`SearchMode::parse_literal`] で検証し、`session_mode` とあわせて
/// [`mode::resolve_mode`] で優先順位解決する（クエリ句 > セッション変数 > 既定）。
/// クエリ句のリテラルが `recall`／`precision` 以外の場合は
/// [`SqlSurfaceError::InvalidInput`]（`22000`。構文上受理された値が不正）で
/// fail-closed に拒否し、黙って既定モードへ落とさない。
pub fn bind_in_session(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
    session_mode: Option<SearchMode>,
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundStatement, SqlSurfaceError> {
    let query_mode = match &stmt.search_mode {
        Some(literal) => Some(SearchMode::parse_literal(literal)?),
        None => None,
    };
    let resolved_mode = mode::resolve_mode(query_mode, session_mode);

    // TASK-79・SQL-9: 1 つの `SELECT` 文（結果列＋`WHERE` の全式項目）で共有する
    // インライン展開後ノード数の予算（[`crate::sql::udf_call::MAX_EXPR_NODES`]）。
    // 多段 UDF 呼び出しによる展開後の指数的膨張を、文単位で歯止めする
    // （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;

    let projection = bind_projection(&stmt.projection, schema, udfs, &mut node_budget)?;

    let (metadata_filters, expr_filters, rls_predicate_present, or_filters) =
        bind_where_predicates(&stmt.where_predicates, schema, udfs, &mut node_budget, &[])?;

    let ranking = bind_ranking(&stmt.order_by, schema, true)?;

    let limit = validate_search_limit(stmt.limit)?;

    // Issue #353: `expr_filters` を束縛時に 1 回だけステップ列コンパイルする
    // （行ループでの再帰評価をなくす）。`expr_filters` と要素数・評価順が
    // 1 対 1 対応するよう順序を保って構築する。
    let expr_filter_programs = expr_filters
        .iter()
        .map(crate::sql::expr_program::ExprProgram::compile)
        .collect();

    Ok(BoundStatement {
        table: stmt.table_name.clone(),
        projection,
        metadata_filters,
        rls_predicate_present,
        expr_filters,
        expr_filter_programs,
        or_filters,
        ranking,
        limit,
        mode: resolved_mode,
        evaluation_order: stmt.evaluation_order,
    })
}

/// Describe（拡張クエリプロトコルの 'D' 種別 S。値未確定でも呼べる契約。
/// `core.rs::EngineCore::describe_parsed_in_session`）専用: [`bind_in_session`]
/// と同じ検証（`USING MODE` リテラル・投影列・`WHERE` 式）を行いつつ、結果列
/// （[`ProjectedColumn`]）だけを返す。結果列は投影列にのみ依存しランキング
/// の実値には依存しないため、`ORDER BY` のベクトルリテラルの実パース有無は
/// Describe が返す列を変えない。
///
/// `validate_vector_literal` が `true`（実リテラルを持つ通常の Describe
/// 呼び出し）の場合は [`bind_in_session`] と同じくベクトルリテラル文字列の
/// 実パース（[`parse_vector_literal`] による形式・次元・非有限値・64 KiB
/// 上限検証）を行う。`false`（`EngineCore::describe_prepared_in_session` 専用。
/// 対象ビヘイビア: Issue #935・WIRE-12・TASK-217）の場合に限り、対象列が
/// テーブルの `VECTOR` 列であることの構造検証のみに留めこの実パースを省略
/// する——`EngineCore::parse_sql_prepared` は Parse 時点（値未確定）の構造
/// 検証のため全 `$n` を固定ダミー値（`sql::params::substitute_dummy`）へ
/// 置換しており、`ORDER BY <vec列> <=> $n` を含む文はこのダミー値がベクトル
/// として不正なため実パースを省略しなければ Describe（Bind 前）が常に
/// `22000` で失敗する（PR #1012 レビュー指摘対応: 実リテラルを持つ通常の
/// 呼び出しではこの省略を行わず常に実値を検証し、Execute まで検証が遅延して
/// 既存のエラー契約が壊れるのを防ぐ）。
///
/// `dummy_equality_flags`（PR #1012 Cursor Bugbot 指摘対応）は
/// [`bind_where_predicates`] へそのまま渡す（同関数のドキュメント参照。
/// `validate_vector_literal == true`——実 SQL テキスト経由の通常 Describe
/// ——の呼び出しでは常に空スライスになる）。
pub(crate) fn bind_projection_for_describe(
    stmt: &ValidatedStatement,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    validate_vector_literal: bool,
    dummy_equality_flags: &[bool],
) -> Result<Vec<ProjectedColumn>, SqlSurfaceError> {
    if let Some(literal) = &stmt.search_mode {
        SearchMode::parse_literal(literal)?;
    }

    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let projection = bind_projection(&stmt.projection, schema, udfs, &mut node_budget)?;

    let (_metadata_filters, _expr_filters, _rls_predicate_present, _or_filters) =
        bind_where_predicates(
            &stmt.where_predicates,
            schema,
            udfs,
            &mut node_budget,
            dummy_equality_flags,
        )?;

    let _ranking = bind_ranking(&stmt.order_by, schema, validate_vector_literal)?;
    let _limit = validate_search_limit(stmt.limit)?;

    Ok(projection)
}

/// [`ValidatedDelete`] を意味論的に束縛する（SQL-18・TASK-191 の公開 API）。
/// `id` 疑似列の値を `u64` として解釈する以外に検証すべき列・型情報を
/// 持たないため、`bind_insert` と異なり `TableSchema` を引数に取らない
/// （`DELETE` は `id` 以外のスキーマ実列を一切参照しない）。
///
/// 検出する違反: `id` 値が `u64` として解釈不能（範囲外を含む。`22000`）。
pub fn bind_delete(stmt: &ValidatedDelete) -> Result<BoundDelete, SqlSurfaceError> {
    let id: u64 = stmt.id_literal.parse().map_err(|_| {
        SqlSurfaceError::invalid_input(format!("malformed id value: {}", stmt.id_literal))
    })?;

    Ok(BoundDelete {
        table: stmt.table_name.clone(),
        id,
        operation_id: stmt.operation_id.clone(),
    })
}

/// 述語つき `DELETE`／`UPDATE`（Issue #870・#869）が 1 文で変更してよい行数の
/// 既定上限（本リポの実装既定値であり、spec 由来の数値ではない）。`INSERT` の
/// `MAX_INSERT_ROWS_PER_STATEMENT`（`allowlist.rs`・private・1_000）と同じ
/// 桁に揃える。実際の判定（[`check_affected_row_count`]）は変更開始前・
/// 副作用ゼロの時点で呼ぶ実行結線（#871）の担当（本モジュールは上限値を
/// [`BoundPredicateDelete::max_affected_rows`] として運搬するのみ）。
pub const DEFAULT_MAX_DML_AFFECTED_ROWS: usize = 1_000;

/// 影響行数 `count` が上限 `limit` を超えないことを検査する（Issue #870・#871
/// が結線する実行時判定の共有ヘルパー）。超過は
/// [`SqlSurfaceError::PayloadTooLarge`]（`54000`）。呼び出し元は書き込み開始前・
/// 副作用ゼロの時点で本関数を呼ぶことで、上限超過を「変更を一部だけ適用して
/// から中断」ではなく「一切変更しないまま拒否」にする契約を維持する。
pub fn check_affected_row_count(count: usize, limit: usize) -> Result<(), SqlSurfaceError> {
    if count > limit {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "DML affected row count {count} exceeds limit {limit}"
        )));
    }
    Ok(())
}

/// 束縛済みの述語つき `DELETE ... WHERE` 文（Issue #870・TASK-192・SQL-19）。
/// `BoundScan`（Issue #454）と同じく `WHERE` の意味論
/// （`metadata_filters`／`expr_filters`）を共有し、第 2 の述語評価器を作らない。
/// 実行結線（可視行列挙・1 トランザクション一括適用・影響行数上限の実測判定・
/// 台帳照合）は #871 の担当（本 Issue の成果物は束縛済み実行計画までで、
/// `core.rs`・`sql/exec.rs` の実行経路は変更しない）。
///
/// フィールドは `pub(crate)`（クレート外からの直読み・直書き不可。カプセル化の
/// 方針は `BoundScan` と同じ）。クレート外からはアクセサーメソッド経由で読み取り、
/// [`Self::new`] 経由で構築する（NoSQL 表層 `delete` op〔#875・#876・NOSQL-12〕が
/// SQL テキストを経由せず直接構築する入口。`BoundScan::new` と同じ契約）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct BoundPredicateDelete {
    pub(crate) table: String,
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    pub(crate) expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
    /// `expr_filters` をステップ列コンパイルした実行形（Issue #353 と同じ
    /// 契約。`sql::expr_program` が `pub(crate) mod` のためクレート外に型を
    /// 出せず、アクセサーは設けない。`BoundScan::expr_filter_programs` と
    /// 同じ判断）。
    pub(crate) expr_filter_programs: Vec<crate::sql::expr_program::ExprProgram>,
    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。[`Self::new`]
    /// （NoSQL 表層の直接構築経路）は常に空にする——NoSQL 表層は本 Issue の
    /// スコープ外（計画§「対象外」参照）。
    pub(crate) or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
    pub(crate) operation_id: Option<OperationId>,
    /// 影響行数の上限（[`check_affected_row_count`] へ渡す運搬役。既定値は
    /// [`DEFAULT_MAX_DML_AFFECTED_ROWS`]）。
    pub(crate) max_affected_rows: usize,
}

impl BoundPredicateDelete {
    /// クレート外から `BoundPredicateDelete` を直接構築する constructor
    /// （NoSQL 表層 `delete` op〔#875・#876・NOSQL-12〕の入口。`BoundScan::new`
    /// と同じ契約。`expr_filters` のステップ列コンパイルは内部で行う）。
    /// `or_filters` は常に空（NoSQL 表層は `OR` 未対応。TASK-208・Issue #912）。
    pub fn new(
        table: String,
        metadata_filters: Vec<MetadataFilter>,
        expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
        operation_id: Option<OperationId>,
        max_affected_rows: usize,
    ) -> Self {
        let expr_filter_programs = compile_expr_filter_programs(&expr_filters);
        Self {
            table,
            metadata_filters,
            expr_filters,
            expr_filter_programs,
            or_filters: Vec::new(),
            operation_id,
            max_affected_rows,
        }
    }

    /// 束縛対象のテーブル名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// SCALAR 段で適用するメタデータフィルタ一覧（等価・前方一致、TASK-147・EXT-3）。
    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        &self.metadata_filters
    }

    /// `WHERE` の式述語（TASK-79・SQL-9）。UDF インライン展開済み。
    pub fn expr_filters(&self) -> &[crate::sql::udf_call::BoundExpr] {
        &self.expr_filters
    }

    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。
    pub fn or_filters(&self) -> &[crate::sql::where_tree::BoundOrGroup] {
        &self.or_filters
    }

    /// [`BoundStatement::has_where_filters`] と同じ判定（TASK-208・Issue #912）。
    pub fn has_where_filters(&self) -> bool {
        !self.metadata_filters.is_empty()
            || !self.expr_filters.is_empty()
            || !self.or_filters.is_empty()
    }

    /// 文末専用句で搬送された、検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }

    /// 影響行数の上限（[`check_affected_row_count`] へ渡す値）。
    pub fn max_affected_rows(&self) -> usize {
        self.max_affected_rows
    }
}

/// [`ValidatedPredicateDelete`] を `schema`・UDF レジストリ `udfs` と照合して
/// [`BoundPredicateDelete`] へ束縛する（Issue #870・TASK-192・SQL-19 の公開
/// API）。`WHERE` の意味論は検索 SELECT（[`bind_in_session`]）・集計 SELECT
/// （[`bind_aggregate`]）・広域取得（[`bind_scan`]）と共有する
/// （[`bind_where_predicates`]。第 2 の述語評価器を作らない）。影響行数上限は
/// 既定値（[`DEFAULT_MAX_DML_AFFECTED_ROWS`]）を保持するのみで、実際の判定
/// （[`check_affected_row_count`]）は実行結線（#871）が変更開始前・副作用
/// ゼロの時点で呼ぶ。
pub fn bind_predicate_delete(
    stmt: &ValidatedPredicateDelete,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundPredicateDelete, SqlSurfaceError> {
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;

    let (metadata_filters, expr_filters, _rls_predicate_present, or_filters) =
        bind_where_predicates(stmt.where_predicates(), schema, udfs, &mut node_budget, &[])?;

    let expr_filter_programs = compile_expr_filter_programs(&expr_filters);

    Ok(BoundPredicateDelete {
        table: stmt.table_name().to_string(),
        metadata_filters,
        expr_filters,
        expr_filter_programs,
        or_filters,
        operation_id: stmt.operation_id().cloned(),
        max_affected_rows: DEFAULT_MAX_DML_AFFECTED_ROWS,
    })
}

/// [`ValidatedInsert`] を `schema` と照合して [`BoundInsert`] へ束縛する
/// （SQL-10、TASK-80 の公開 API）。`schema` は呼び出し元
/// （`core.rs::EngineCore::execute_insert_sql`）が `Storage::get_table_schema` で
/// 取得済みのものを渡す。
///
/// **単一行契約**: `stmt.rows.len() == 1` の場合のみ束縛する（`stmt.rows[0]` を
/// 見る）。複数行 `VALUES`（SQL-16、TASK-190。`stmt.rows.len() > 1`）は
/// [`SqlSurfaceError::InvalidInput`]（`22000`）で拒否し、2 行目以降を黙って
/// 無視しない（fail-open な取りこぼしを防ぐ。複数行の束縛は本関数を経由せず
/// [`bind_insert_form`] の `RowBatch` 分岐が担う）。パーサーは常に
/// `rows.len() >= 1` を保証するため、単一行 `INSERT` に対する外部観測可能な
/// 挙動は本関数導入前とビット同一のまま不変。
///
/// 検出する違反はすべて [`SqlSurfaceError::InvalidInput`]（`22000`）:
/// 複数行 `VALUES`・列名重複・列リストに疑似列 `id` を含まない・`id` 値が
/// `u64` として解釈不能（範囲外を含む）・未知の列名・列型とリテラル種別の
/// 不一致（`VECTOR` 列に数値、`TEXT` 列にベクトルリテラルを渡す等）・非
/// nullable 列の欠落。ベクトルリテラル自体の形式・次元・64 KiB 上限は既存の
/// [`parse_vector_literal`] をそのまま再利用する（アロケーション前のサイズ検証を
/// 二重管理しない）。
///
/// テナント・可視性はここで解決しない（`exec::execute_insert` の責務。
/// クライアントが列リストへ `tenant_id`・可視性ラベル相当の名前を指定しても、
/// スキーマ上の実列として照合されるだけで RLS フィールドへは書き込まれない）。
pub fn bind_insert(
    stmt: &ValidatedInsert,
    schema: &TableSchema,
) -> Result<BoundInsert, SqlSurfaceError> {
    // 複数行 `VALUES`（SQL-16、TASK-190）は本関数の単一行契約外であり、2 行目
    // 以降を黙って無視する fail-open を避けるため明示的に拒否する（複数行の
    // 束縛は `bind_insert_form` の `RowBatch` 分岐を経由すること）。
    if stmt.rows.len() != 1 {
        return Err(SqlSurfaceError::invalid_input(
            "bind_insert only accepts a single VALUES row (use bind_insert_form for multi-row VALUES)",
        ));
    }
    // `ON CONFLICT ...`（SQL-20・TASK-193、Issue #872）は本関数の契約外であり、
    // 黙って落として plain INSERT として束縛する fail-open を避けるため明示的に
    // 拒否する（複数行 `VALUES` と同じ理由。UPSERT の束縛は必ず
    // `bind_insert_form` の `Upsert` 分岐を経由すること）。
    if stmt.on_conflict.is_some() {
        return Err(SqlSurfaceError::invalid_input(
            "bind_insert does not accept ON CONFLICT (use bind_insert_form for UPSERT)",
        ));
    }
    let values = stmt
        .rows
        .first()
        .ok_or_else(|| SqlSurfaceError::invalid_input("INSERT statement has no VALUES row"))?;
    bind_insert_row(
        &stmt.table_name,
        &stmt.columns,
        values,
        &stmt.operation_id,
        schema,
    )
}

/// [`bind_insert`] の 1 行分の束縛本体（SQL-16、TASK-190）。複数行 `VALUES` の
/// 各行が行キー `id`・列値の意味論検証（列名重複・`id` 欠落・未知列名・型不一致・
/// 非 nullable 列欠落）を個別に受けられるよう、[`bind_insert`]（単一行形・
/// `stmt.rows[0]` のみを見る）と [`bind_insert_form`] の複数行分岐の双方から
/// 共有する。`table_name`・`operation_id` は行に依存しないため呼び出し元が
/// 1 度だけ渡す。
/// 省略列への `DEFAULT` 補完・非 nullable 検査を一括して行う唯一の適用点
/// （TABLE-16・TASK-204、Issue #904）。`bind_insert_row`（SQL `INSERT`・
/// NoSQL `insert` op が経由する `bind_insert`）・`bind_file_insert`
/// （ファイル形 `INSERT`）・`bind_upsert_form`（`UPSERT` の挿入側提案行）が
/// 共有する。`provided[i]` が立っている列（明示的に値または `NULL` が
/// 与えられた列）はここでは一切触らない——`DEFAULT` は「省略」にのみ適用し、
/// 明示的な `NULL` には適用しない契約（TABLE-16）はこの呼び分けで担保する。
pub(crate) fn fill_omitted_columns(
    columns: &[ColumnDef],
    bound_values: &mut [crate::row_codec::Value],
    provided: &[bool],
) -> Result<(), SqlSurfaceError> {
    for (idx, column) in columns.iter().enumerate() {
        let is_provided = provided.get(idx).copied().unwrap_or(false);
        if is_provided {
            continue;
        }
        match &column.default {
            Some(default) => {
                let value = bind_column_default(column, default)?;
                if let Some(slot) = bound_values.get_mut(idx) {
                    *slot = value;
                }
            }
            None => {
                if !column.nullable {
                    return Err(SqlSurfaceError::not_null_violation(column.name.clone()));
                }
            }
        }
    }
    Ok(())
}

/// [`ColumnDefault`] を実際の列型の [`crate::row_codec::Value`] へ束縛する
/// （TABLE-16・TASK-204、Issue #904）。カタログ層
/// （`catalog::ColumnDefault::compatible_with`。`validate_schema` 経由で
/// 既に検証済み）が型の大分類の整合を保証するが、数値の精度・範囲検証は
/// 既存の `INSERT` リテラル束縛ヘルパーへ委譲し、第 2 の実装を作らない。
fn bind_column_default(
    column: &ColumnDef,
    default: &ColumnDefault,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    let literal = match default {
        ColumnDefault::Text(s) => InsertLiteral::String(s.clone()),
        ColumnDefault::Number(s) => InsertLiteral::Number(s.clone()),
        ColumnDefault::Bool(b) => InsertLiteral::Bool(*b),
    };
    let incompatible = || {
        SqlSurfaceError::invalid_input(format!(
            "column {:?} DEFAULT is not compatible with its type",
            column.name
        ))
    };
    match &column.ty {
        ColumnType::Text => match &literal {
            InsertLiteral::String(s) => Ok(crate::row_codec::Value::Text(s.clone())),
            _ => Err(incompatible()),
        },
        ColumnType::Integer | ColumnType::BigInt => {
            bind_integer_literal(&column.name, column.ty.clone(), &literal)
        }
        ColumnType::Real => match &literal {
            InsertLiteral::Number(n) => Ok(crate::row_codec::Value::Real(bind_real_literal(n)?)),
            _ => Err(incompatible()),
        },
        ColumnType::Double => match &literal {
            InsertLiteral::Number(n) => {
                Ok(crate::row_codec::Value::Double(bind_double_literal(n)?))
            }
            _ => Err(incompatible()),
        },
        ColumnType::Boolean => match &literal {
            InsertLiteral::Bool(b) => Ok(crate::row_codec::Value::Bool(*b)),
            _ => Err(incompatible()),
        },
        ColumnType::Numeric { precision, scale } => {
            bind_numeric_literal(&literal, &column.name, *precision, *scale)
        }
        // `VECTOR` は `DEFAULT` 自体が構文段階（`sql::allowlist::
        // parse_create_table_column`）で拒否されるため到達しない。他の型
        // （配列・日時・ENUM 等）は `catalog::ColumnDefault::compatible_with`
        // が `false` を返しカタログに永続化できないため同様に到達しない。
        // 到達した場合も fail-closed に拒否する。
        _ => Err(incompatible()),
    }
}

fn bind_insert_row(
    table_name: &str,
    columns: &[String],
    values: &[InsertLiteral],
    operation_id: &Option<OperationId>,
    schema: &TableSchema,
) -> Result<BoundInsert, SqlSurfaceError> {
    if columns.len() != values.len() {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT column count does not match value count",
        ));
    }

    let mut seen_columns: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for name in columns {
        if !seen_columns.insert(name.as_str()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate column in INSERT column list: {name}"
            )));
        }
    }

    let id_pos = columns.iter().position(|c| c == "id").ok_or_else(|| {
        SqlSurfaceError::invalid_input("INSERT column list must include the id pseudo-column")
    })?;
    let id_literal = values
        .get(id_pos)
        .ok_or_else(|| SqlSurfaceError::invalid_input("missing value for id pseudo-column"))?;
    let id: u64 = match id_literal {
        InsertLiteral::Number(n) => n
            .parse()
            .map_err(|_| SqlSurfaceError::invalid_input(format!("malformed id value: {n}")))?,
        InsertLiteral::String(_)
        | InsertLiteral::Bool(_)
        | InsertLiteral::Null
        | InsertLiteral::Vector(_) => {
            return Err(SqlSurfaceError::invalid_input(
                "id pseudo-column value must be a number",
            ))
        }
    };

    let mut bound_values: Vec<crate::row_codec::Value> =
        vec![crate::row_codec::Value::Null; schema.columns.len()];
    let mut provided = vec![false; schema.columns.len()];

    for (name, literal) in columns.iter().zip(values.iter()) {
        if name == "id" {
            // 疑似列 `id` は行キーとして上で処理済みであり、スキーマ実列とは
            // 照合しない（既存の SELECT 側 `bind` と同様、実カラム名 `id` を
            // 持つスキーマではその実列を本 INSERT 形から指定する手段がない。
            // 既知の制約としてドキュメント化する）。
            continue;
        }
        let col_idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let column = schema
            .columns
            .get(col_idx)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let value = match (&column.ty, literal) {
            (ColumnType::Vector(dim), InsertLiteral::String(s)) => {
                crate::row_codec::Value::Vector(parse_vector_literal(s, *dim)?)
            }
            (ColumnType::Vector(dim), InsertLiteral::Vector(values)) => {
                bind_vector_literal_values(values, *dim, name)?
            }
            (ColumnType::Vector(_), InsertLiteral::Number(_) | InsertLiteral::Bool(_)) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a vector literal, got a non-vector literal"
                )))
            }
            (ColumnType::Text, InsertLiteral::String(s)) => {
                crate::row_codec::Value::Text(s.clone())
            }
            (
                ColumnType::Text,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a text literal, got a non-text literal"
                )))
            }
            (ColumnType::Boolean, InsertLiteral::Bool(b)) => crate::row_codec::Value::Bool(*b),
            (
                ColumnType::Boolean,
                InsertLiteral::String(_) | InsertLiteral::Number(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a boolean literal (true/false)"
                )))
            }
            (
                ColumnType::Integer | ColumnType::BigInt,
                InsertLiteral::Number(_)
                | InsertLiteral::String(_)
                | InsertLiteral::Bool(_)
                | InsertLiteral::Vector(_),
            ) => bind_integer_literal(name, column.ty.clone(), literal)?,
            // F7（Issue #882 計画）: REAL/DOUBLE は数値リテラルのみ受理する
            // （文字列からの暗黙変換は行わない。#896 へ申し送り）。
            (ColumnType::Real, InsertLiteral::Number(n)) => {
                crate::row_codec::Value::Real(bind_real_literal(n)?)
            }
            (
                ColumnType::Real,
                InsertLiteral::String(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a REAL literal, got a non-numeric literal"
                )))
            }
            (ColumnType::Double, InsertLiteral::Number(n)) => {
                crate::row_codec::Value::Double(bind_double_literal(n)?)
            }
            (
                ColumnType::Double,
                InsertLiteral::String(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a DOUBLE PRECISION literal, got a non-numeric literal"
                )))
            }
            (ColumnType::Date, InsertLiteral::String(s)) => {
                bind_datetime_literal(name, ColumnType::Date, s)?
            }
            (
                ColumnType::Date,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a DATE literal (YYYY-MM-DD)"
                )))
            }
            (ColumnType::Timestamp, InsertLiteral::String(s)) => {
                bind_datetime_literal(name, ColumnType::Timestamp, s)?
            }
            (
                ColumnType::Timestamp,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a TIMESTAMP literal (YYYY-MM-DD HH:MM:SS)"
                )))
            }
            (ColumnType::Array(array_ty), InsertLiteral::String(s)) => {
                crate::row_codec::Value::Array(parse_array_literal(s, *array_ty)?)
            }
            (
                ColumnType::Array(_),
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects an array literal, got a non-array literal"
                )))
            }
            (ColumnType::Bytea, InsertLiteral::String(s)) => bind_bytea_literal(s, name)?,
            (
                ColumnType::Bytea,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a bytea hex literal"
                )))
            }
            (ColumnType::Json | ColumnType::Jsonb, InsertLiteral::String(s)) => {
                bind_json_literal(s, &column.ty, name)?
            }
            (
                ColumnType::Json | ColumnType::Jsonb,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a JSON text literal"
                )))
            }
            (ColumnType::Enum(def), InsertLiteral::String(s)) => bind_enum_literal(def, s, name)?,
            (
                ColumnType::Enum(_),
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a text literal for its enum type"
                )))
            }
            // `InsertLiteral::Null`（Issue #889 レビュー指摘）は SQL テキストの
            // `INSERT ... VALUES` 構文からは構築されない（`sql::allowlist` の
            // VALUES リテラルパーサーは `NULL` トークンを生成しない）が、NoSQL
            // 表層 `insert` op（Issue #904 D5）が JSON `null` を本 variant として
            // 渡すため到達する。TABLE-16 の確定契約: 明示 `NULL` には `DEFAULT`
            // を適用せず、nullable 列は `Value::Null`、非 nullable 列は
            // `NotNullViolation`（`23502`）へ倒す（省略時の `DEFAULT` 補完とは
            // 独立の経路。下の省略列補完ループ参照）。
            (_, InsertLiteral::Null) if column.nullable => crate::row_codec::Value::Null,
            (_, InsertLiteral::Null) => {
                return Err(SqlSurfaceError::not_null_violation(name.clone()))
            }
            (ColumnType::Uuid, InsertLiteral::String(s)) => bind_uuid_literal(s, name)?,
            (
                ColumnType::Uuid,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a UUID text literal"
                )))
            }
            (ColumnType::Numeric { precision, scale }, lit) => {
                bind_numeric_literal(lit, name, *precision, *scale)?
            }
        };
        if let Some(slot) = bound_values.get_mut(col_idx) {
            *slot = value;
        }
        if let Some(flag) = provided.get_mut(col_idx) {
            *flag = true;
        }
    }

    fill_omitted_columns(&schema.columns, &mut bound_values, &provided)?;

    Ok(BoundInsert {
        table: table_name.to_string(),
        id,
        values: bound_values,
        operation_id: operation_id.clone(),
    })
}

/// `BYTEA` 列向けの文字列リテラル（`'\xDEADBEEF'` 形。B4）を
/// [`crate::row_codec::Value::Bytes`] へ束縛する共通ヘルパー。INSERT・UPDATE・
/// UPSERT の 4 束縛箇所が同じ検証・エラー分類を共有する。
///
/// - 形式不正（接頭辞なし・奇数桁・非 16 進）は `22000`（[`SqlSurfaceError::invalid_input`]）。
/// - 長さ超過は `54000`（[`SqlSurfaceError::payload_too_large`]）。
pub(crate) fn bind_bytea_literal(
    s: &str,
    column_name: &str,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    match crate::bytea::parse_hex_text(s) {
        Ok(bytes) => Ok(crate::row_codec::Value::Bytes(bytes)),
        Err(crate::bytea::ByteaTextError::TooLong) => Err(SqlSurfaceError::payload_too_large(
            format!("column {column_name:?} bytea literal exceeds length limit"),
        )),
        Err(_) => Err(SqlSurfaceError::invalid_input(format!(
            "column {column_name:?} expects a valid bytea hex literal (\\x...)"
        ))),
    }
}

/// `JSON`／`JSONB` 列向けの文字列リテラルを [`crate::row_codec::Value::Json`] へ
/// 束縛する共通ヘルパー（Issue #889 D6。`bind_bytea_literal` と同型）。INSERT・
/// UPDATE・UPSERT リテラル・UPSERT `ON CONFLICT` リテラルの 4 束縛箇所が共有する。
/// SQL の文字列リテラルは JSON テキストとして解釈する（トップレベルのスカラー
/// JSON も有効な JSON として受理。`'null'` は JSON の `null` であり SQL `NULL`
/// とは別物）。`JSON` 列は検証のみ（入力テキスト保持）、`JSONB` 列は正規化する。
///
/// - 構文不正・深さ/要素数超過は `42601`（[`SqlSurfaceError::UnsupportedSyntax`]。
///   NOSQL-8 と同一分類）。
/// - 長さ超過は `54000`（[`SqlSurfaceError::payload_too_large`]）。
pub(crate) fn bind_json_literal(
    s: &str,
    column_ty: &ColumnType,
    column_name: &str,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    let text = match column_ty {
        ColumnType::Json => {
            crate::json::validate_json_column_text(s).map_err(|e| json_column_error(e, s))?;
            s.to_string()
        }
        ColumnType::Jsonb => {
            crate::json::canonicalize_jsonb_text(s).map_err(|e| json_column_error(e, s))?
        }
        ColumnType::Text
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Vector(_)
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Boolean
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Array(_)
        | ColumnType::Bytea
        | ColumnType::Enum(_)
        | ColumnType::Numeric { .. }
        | ColumnType::Uuid => {
            return Err(SqlSurfaceError::invalid_input(format!(
                "column {column_name:?} is not a JSON column"
            )));
        }
    };
    Ok(crate::row_codec::Value::Json(text))
}

/// [`crate::json::JsonColumnError`] を SQL 表層の分類（`SqlSurfaceError`）へ写像する
/// （Issue #889 D1）。`s` 自体（内容）はエラーメッセージへ含めない
/// （security.md「テナント境界」: エラー経由の情報漏えい防止）。
fn json_column_error(e: crate::json::JsonColumnError, _s: &str) -> SqlSurfaceError {
    match e {
        crate::json::JsonColumnError::TooLong => {
            SqlSurfaceError::payload_too_large("JSON literal exceeds maximum length")
        }
        crate::json::JsonColumnError::Invalid => {
            SqlSurfaceError::unsupported("invalid JSON literal")
        }
    }
}

/// `ENUM` 列向けの文字列リテラルを [`crate::row_codec::Value::Enum`] へ束縛する
/// 共通ヘルパー（TABLE-14・TASK-198、Issue #890）。INSERT・UPDATE（単一行 SET・
/// 述語形）・UPSERT の各束縛箇所が同じ検証・エラー分類を共有する
/// （[`bind_bytea_literal`] と同じ設計）。語彙外のラベルは書き込みトランザクション
/// 開始前に `22P02`（[`SqlSurfaceError::invalid_text_representation`]）で拒否する。
/// エラーメッセージには語彙の一覧を含めない（型名とクライアント自身の入力値のみ。
/// security.md P0「情報漏えい」対応）。
pub(crate) fn bind_enum_literal(
    def: &crate::catalog::EnumTypeDef,
    s: &str,
    column_name: &str,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    match def.validate_label(s) {
        Ok(()) => Ok(crate::row_codec::Value::Enum(s.to_string())),
        Err(_) => Err(SqlSurfaceError::invalid_text_representation(format!(
            "column {column_name:?} (enum {:?}) does not accept label {s:?}",
            def.name()
        ))),
    }
}

/// `UUID` 列向けの文字列リテラルを [`crate::row_codec::Value::Uuid`] へ束縛する
/// 共通ヘルパー（TABLE-13〔検討中〕・TASK-197、Issue #887）。INSERT・UPDATE
/// （単一行 SET・述語形）・UPSERT の各束縛箇所が同じ検証・エラー分類を共有する
/// （[`bind_enum_literal`] と同じ設計）。厳密文法（[`crate::uuid::parse_uuid_text`]）に
/// 反する入力は書き込みトランザクション開始前に `22P02`
/// （[`SqlSurfaceError::invalid_text_representation`]）で拒否する（U3・U7）。
/// エラーメッセージには列名とクライアント自身の入力値のみを含める
/// （security.md P0「情報漏えい」対応）。COPY（`sql::copy::bind_copy_record`。
/// Issue #939 のマージで追加された `Uuid` 列への対応漏れの修正）も本関数を
/// 共有する。
pub(crate) fn bind_uuid_literal(
    s: &str,
    column_name: &str,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    match crate::uuid::parse_uuid_text(s) {
        Ok(u) => Ok(crate::row_codec::Value::Uuid(u)),
        Err(_) => Err(SqlSurfaceError::invalid_text_representation(format!(
            "column {column_name:?} does not accept {s:?} as a UUID literal"
        ))),
    }
}

/// 束縛済みの UPDATE 文（SQL-17、TASK-191。実行結線は #865 の担当）。
///
/// `assignments` は SET 句が書かれた宣言順を保持する **部分更新** の表現であり、
/// `BoundInsert.values`（`Null` 埋めの全列ベクトル）とは意図的に異なる形にしている。
/// `tenant::update_row_unchecked` は行全体を置換する API であるため、実行結線側
/// （#865）は既存行を読み取り、ここで返す対象列だけを上書きしてから `RowInput` を
/// 構築する（read-merge-write）。宣言順を保持する理由は、`operation_id` の内容照合
/// （RECOVER-10/11 系。#868）が正規化した文字列から計算される想定であり、実装側で
/// 列順を並べ替えるとクライアントの記述順序に同一文の再送判定が依存して崩れうる
/// ため。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundUpdate {
    pub table: String,
    /// `WHERE id = <n>` の行キー（疑似列 `id`）。
    pub id: u64,
    /// SET 句の (`schema.columns` の列インデックス, 束縛済み値) 対応。宣言順を
    /// 保持する（並べ替えない）。
    pub assignments: Vec<(usize, crate::row_codec::Value)>,
    /// TASK-92（RECOVER-1）: [`ValidatedUpdate::operation_id`] をそのまま素通しする。
    /// `LedgerMode::Ledgered`（既定）では `sql::allowlist::validate_update` が既に
    /// `None` を `23502` で拒否済みのため常に `Some`。`CompareOnlyWithoutLedger`
    /// でのみ `None` になり得る。
    pub operation_id: Option<OperationId>,
}

/// [`ValidatedUpdate`] を `schema` と照合して [`BoundUpdate`] へ束縛する
/// （SQL-17、TASK-191 の公開 API）。`schema` は呼び出し元が
/// `Storage::get_table_schema` で取得済みのものを渡す想定（`bind_insert` と同じ
/// 契約）。
///
/// 検出する違反はすべて [`SqlSurfaceError::InvalidInput`]（`22000`）:
/// SET 対象列に疑似列 `id`・RLS 内部列 `tenant_id`／`visibility` を指定（構造上
/// スキーマに存在しない列だが、`22000`「unknown column」ではなく専用の判定を
/// 先に行い `42601` で拒否する。下記参照）・SET 内の列名重複・未知の列名・
/// 列型とリテラル種別の不一致（`VECTOR` 列に数値、`TEXT` 列にベクトルリテラルを
/// 渡す等）・`id` 値が `u64` として解釈不能（範囲外を含む）。ベクトルリテラル
/// 自体の形式・次元・64 KiB 上限は既存の [`parse_vector_literal`] をそのまま
/// 再利用する。
///
/// `id`／`tenant_id`／`visibility` の SET 対象化は `42601`（許可リスト外の形と同じ
/// 分類）で拒否する: これらはスキーマ実列ではない疑似列・RLS 内部列であり、
/// 未知列として `22000`（型・値の意味論的不正）に丸めるとクライアントが
/// 「別の列名を使えば通る」と誤解しうる。構造的に受理しない形として先に判定する
/// （SQL-17 の「ユーザー列のみを SET 対象にできる」契約。テナント・可視性は
/// `exec::execute_update`〔#865〕がサーバー側で `PolicyContext` から導出・固定し、
/// クライアントが行の所有者・可視性を書き換える経路を作らない）。
///
/// UPDATE は部分更新であるため、`bind_insert` の非 nullable 列欠落チェックは行わない
/// （SET で指定しなかった列は `assignments` に一切現れず、実行結線側の
/// read-merge-write が既存値を保持する）。
///
/// SET 句の束縛本体（[`bind_set_assignments`]）は [`bind_update`]（単一行・id 指定形。
/// SQL-17）と [`bind_update_form`] の述語形腕（SQL-19、TASK-192・Issue #869）が
/// 共有する。
pub fn bind_update(
    stmt: &crate::sql::allowlist::ValidatedUpdate,
    schema: &TableSchema,
) -> Result<BoundUpdate, SqlSurfaceError> {
    let assignments = bind_set_assignments(&stmt.assignments, schema)?;

    let id: u64 = stmt.id_literal.parse().map_err(|_| {
        SqlSurfaceError::invalid_input(format!("malformed id value: {}", stmt.id_literal))
    })?;

    Ok(BoundUpdate {
        table: stmt.table_name.clone(),
        id,
        assignments,
        operation_id: stmt.operation_id.clone(),
    })
}

/// `UPDATE` の SET 句（(列名, リテラル) 対応の宣言順スライス）を `schema` と
/// 照合して束縛する共通ヘルパー（SQL-17・SQL-19、TASK-191・TASK-192・
/// Issue #869 で [`bind_update`] から抽出）。検出する違反・拒否コードは
/// [`bind_update`] のドキュメントに記載のとおり（疑似列・RLS 内部列の SET 対象化は
/// `42601`、それ以外の意味論的不正は `22000`）。
fn bind_set_assignments(
    assignments: &[(String, InsertLiteral)],
    schema: &TableSchema,
) -> Result<Vec<(usize, crate::row_codec::Value)>, SqlSurfaceError> {
    let mut seen_columns: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut bound: Vec<(usize, crate::row_codec::Value)> = Vec::with_capacity(assignments.len());

    for (name, literal) in assignments {
        if name == "id" || name == "tenant_id" || name == "visibility" {
            return Err(SqlSurfaceError::unsupported(format!(
                "column {name:?} cannot be targeted by SET"
            )));
        }
        if !seen_columns.insert(name.as_str()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate column in SET clause: {name}"
            )));
        }
        let col_idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let column = schema
            .columns
            .get(col_idx)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let value = match (&column.ty, literal) {
            (ColumnType::Vector(dim), InsertLiteral::String(s)) => {
                crate::row_codec::Value::Vector(parse_vector_literal(s, *dim)?)
            }
            (ColumnType::Vector(dim), InsertLiteral::Vector(values)) => {
                bind_vector_literal_values(values, *dim, name)?
            }
            (ColumnType::Vector(_), InsertLiteral::Number(_) | InsertLiteral::Bool(_)) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a vector literal, got a non-vector literal"
                )))
            }
            // `VECTOR` 列は `nullable` の値に関わらず常に必須として扱う
            // （`wire-server::http::query::insert::bind_row` の既存契約と同じ
            // 判断。Issue #889 レビュー指摘対応・PR #1014）。
            (ColumnType::Vector(_), InsertLiteral::Null) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} is a VECTOR column and cannot be set to NULL"
                )))
            }
            (ColumnType::Text, InsertLiteral::String(s)) => {
                crate::row_codec::Value::Text(s.clone())
            }
            (
                ColumnType::Text,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a text literal, got a non-text literal"
                )))
            }
            (ColumnType::Boolean, InsertLiteral::Bool(b)) => crate::row_codec::Value::Bool(*b),
            (
                ColumnType::Boolean,
                InsertLiteral::String(_) | InsertLiteral::Number(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a boolean literal (true/false)"
                )))
            }
            (
                ColumnType::Integer | ColumnType::BigInt,
                InsertLiteral::Number(_)
                | InsertLiteral::String(_)
                | InsertLiteral::Bool(_)
                | InsertLiteral::Vector(_),
            ) => bind_integer_literal(name, column.ty.clone(), literal)?,
            (ColumnType::Real, InsertLiteral::Number(n)) => {
                crate::row_codec::Value::Real(bind_real_literal(n)?)
            }
            (
                ColumnType::Real,
                InsertLiteral::String(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a REAL literal, got a non-numeric literal"
                )))
            }
            (ColumnType::Double, InsertLiteral::Number(n)) => {
                crate::row_codec::Value::Double(bind_double_literal(n)?)
            }
            (
                ColumnType::Double,
                InsertLiteral::String(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a DOUBLE PRECISION literal, got a non-numeric literal"
                )))
            }
            (ColumnType::Date, InsertLiteral::String(s)) => {
                bind_datetime_literal(name, ColumnType::Date, s)?
            }
            (
                ColumnType::Date,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a DATE literal (YYYY-MM-DD)"
                )))
            }
            (ColumnType::Timestamp, InsertLiteral::String(s)) => {
                bind_datetime_literal(name, ColumnType::Timestamp, s)?
            }
            (
                ColumnType::Timestamp,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a TIMESTAMP literal (YYYY-MM-DD HH:MM:SS)"
                )))
            }
            (ColumnType::Array(array_ty), InsertLiteral::String(s)) => {
                crate::row_codec::Value::Array(parse_array_literal(s, *array_ty)?)
            }
            (
                ColumnType::Array(_),
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects an array literal, got a non-array literal"
                )))
            }
            (ColumnType::Bytea, InsertLiteral::String(s)) => bind_bytea_literal(s, name)?,
            (
                ColumnType::Bytea,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a bytea hex literal"
                )))
            }
            (ColumnType::Json | ColumnType::Jsonb, InsertLiteral::String(s)) => {
                bind_json_literal(s, &column.ty, name)?
            }
            (
                ColumnType::Json | ColumnType::Jsonb,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a JSON text literal"
                )))
            }
            (ColumnType::Enum(def), InsertLiteral::String(s)) => bind_enum_literal(def, s, name)?,
            (
                ColumnType::Enum(_),
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a text literal for its enum type"
                )))
            }
            // 明示的な SQL `NULL`（`ColumnType::Vector` を除く。Issue #889
            // レビュー指摘・PR #1014）。`column.nullable` を確認したうえで
            // `Value::Null` へ写像し、非 nullable 列は fail-closed に拒否する。
            // NoSQL 表層 `update` op（`wire-server::http::query::update::
            // map_set_assignments`）の JSON 列 `null` 分岐が現状唯一の
            // 構築元だが、SQL 表層 `UPDATE ... SET` 経由（将来 `NULL`
            // リテラルの字句規則が追加された場合）でも同じ扱いを共有する。
            (_, InsertLiteral::Null) if column.nullable => crate::row_codec::Value::Null,
            (_, InsertLiteral::Null) => {
                return Err(SqlSurfaceError::not_null_violation(name.clone()))
            }
            (ColumnType::Uuid, InsertLiteral::String(s)) => bind_uuid_literal(s, name)?,
            (
                ColumnType::Uuid,
                InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
            ) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a UUID text literal"
                )))
            }
            (ColumnType::Numeric { precision, scale }, lit) => {
                bind_numeric_literal(lit, name, *precision, *scale)?
            }
        };
        bound.push((col_idx, value));
    }

    Ok(bound)
}

/// 1 文の `UPDATE`／`DELETE`（述語形。SQL-19・SQL-20 系）が変更してよい行数の
/// 上限（本リポの実装既定値。SQL-16・TASK-190 の `MAX_INSERT_ROWS_PER_STATEMENT`
/// と同じ「1 文あたり」の桁に揃える）。束縛段階では対象行数が確定しないため、
/// 実行結線（Issue #871・#870）が変更を開始する前に [`check_dml_affected_rows`]
/// を呼ぶ契約とする（構造検証・束縛のみを担う本モジュールは値を提供するのみで、
/// 判定自体はここでは行わない）。`count` は対象行集合の全件列挙結果である必要は
/// なく、広い述語（例: 全行に一致する `WHERE`）による無制限列挙を避けるため、
/// 呼び出し元は候補行を `MAX_DML_AFFECTED_ROWS + 1` 件に達した時点で列挙を
/// 打ち切ってその件数を渡してよい（早期終了。security.md「不安全な設計」＝
/// 未検証入力によるリソース増幅の回避）。
pub const MAX_DML_AFFECTED_ROWS: usize = 1_000;

/// `count`（対象行数。[`MAX_DML_AFFECTED_ROWS`] を超えたかどうかの判定にのみ
/// 使うため、呼び出し元は `MAX_DML_AFFECTED_ROWS + 1` 件で打ち切った列挙結果を
/// 渡してよい）が [`MAX_DML_AFFECTED_ROWS`] を超えないか検証する。超過は
/// [`SqlSurfaceError::PayloadTooLarge`]（`54000`）。`detail` には件数と上限のみを
/// 含め、テナント・行内容には触れない（fail-closed。実行前・副作用ゼロの段階で
/// 拒否する契約。呼び出し元は Issue #871（述語つき `UPDATE` 実行結線）・#870
/// （述語つき `DELETE`）が変更開始前に呼ぶ）。
pub fn check_dml_affected_rows(count: usize) -> Result<(), SqlSurfaceError> {
    if count > MAX_DML_AFFECTED_ROWS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "statement would affect {count} rows, exceeding the per-statement limit of {MAX_DML_AFFECTED_ROWS}"
        )));
    }
    Ok(())
}

/// 束縛済みの述語つき `UPDATE` 文（SQL-19、TASK-192・Issue #869。実行結線は
/// Issue #871 の担当）。[`BoundUpdate`]（単一行・id 指定形）とは別型として扱う
/// （[`BoundUpdateForm`] 参照）。
///
/// フィールドは `pub(crate)` のまま公開しない（[`BoundScan`] と同じ作法）。
/// [`Self::new`] も `pub(crate)` の raw constructor であり、クレート外からは
/// 呼べない（詳細は [`Self::new`] のドキュメント参照）。クレート外からは
/// アクセサーメソッド経由で読み取るのみで、構築は [`bind_update_form`] の
/// ような検証済み公開束縛 API を経由してのみ可能（NoSQL 表層の `update` op・
/// Issue #876 が直接束縛の入口を必要とする場合も、同じ検証を実施する別の
/// 公開 API を新設し、本 `new` はその内部実装としてのみ使う）。
///
/// `expr_filter_programs`（ステップ列コンパイル済み実行形。Issue #353 と同型）は
/// 本型では保持しない。実行結線（#871）が候補行確定後に
/// `crate::sql::expr_program::ExprProgram::compile` で都度コンパイルする契約
/// （束縛時点では行ループを持たないため、コンパイル結果を保持しても
/// 使う読み手が本 Issue の範囲には存在しない）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct BoundPredicateUpdate {
    pub(crate) table: String,
    /// SET 句の (`schema.columns` の列インデックス, 束縛済み値) 対応。宣言順を
    /// 保持する（[`BoundUpdate::assignments`] と同じ契約）。
    pub(crate) assignments: Vec<(usize, crate::row_codec::Value)>,
    /// SCALAR 段で適用するメタデータフィルタ一覧（等価・前方一致、TASK-147・EXT-3）。
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    /// `WHERE` の式述語（TASK-79・SQL-9）。UDF インライン展開済み。
    pub(crate) expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。
    pub(crate) or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
    pub(crate) operation_id: Option<OperationId>,
}

impl BoundPredicateUpdate {
    /// クレート内限定の raw constructor（値の意味論検証を強制しない）。
    ///
    /// **`pub` にしない**: [`bind_update_form`] が要求する安全性検証
    /// （`bind_set_assignments` による `id`／`tenant_id`／`visibility` 列への
    /// SET 拒否、`metadata_filters`・`expr_filters` が両方空＝実質無条件更新の
    /// 拒否、`LedgerMode::Ledgered` 下での `operation_id` 必須化は
    /// [`crate::sql::allowlist::validate_update_form`] が `ValidatedUpdateForm` の
    /// 構築時点で強制する）は、これらの検査を経ていない生のフィールドを
    /// そのまま受け取れる公開 constructor を crate 外へ晒した時点で迂回可能に
    /// なる（[`BoundScan::new`] は読み取り専用でありこの意味の安全性検査を
    /// 持たないため同じ設計にはしない）。NoSQL 表層の `update` op
    /// （Issue #876）が SQL テキストを経由しない直接束縛の入口を必要とする
    /// 場合は、[`bind_update_form`] と同じ検証（SET／述語／`operation_id`）を
    /// 必ず実施したうえで [`BoundPredicateUpdate`] を返す**別の**公開 API を
    /// 新設し、本 `new` はその内部実装としてのみ使うこと。
    pub(crate) fn new(
        table: String,
        assignments: Vec<(usize, crate::row_codec::Value)>,
        metadata_filters: Vec<MetadataFilter>,
        expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
        or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
        operation_id: Option<OperationId>,
    ) -> Self {
        Self {
            table,
            assignments,
            metadata_filters,
            expr_filters,
            or_filters,
            operation_id,
        }
    }

    /// 束縛対象のテーブル名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// SET 句の (列インデックス, 束縛済み値) 対応（宣言順）。
    pub fn assignments(&self) -> &[(usize, crate::row_codec::Value)] {
        &self.assignments
    }

    /// SCALAR 段で適用するメタデータフィルタ一覧。
    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        &self.metadata_filters
    }

    /// `WHERE` の式述語（UDF インライン展開済み）。
    pub fn expr_filters(&self) -> &[crate::sql::udf_call::BoundExpr] {
        &self.expr_filters
    }

    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。
    pub fn or_filters(&self) -> &[crate::sql::where_tree::BoundOrGroup] {
        &self.or_filters
    }

    /// 文末専用句で搬送された、検証済みの `operation_id`。
    pub fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }
}

/// [`bind_update_form`] の戻り値。`UPDATE` の `WHERE` 句が単一行・id 指定形
/// （SQL-17）か述語形（SQL-19）かで variant を分ける（[`crate::sql::allowlist::
/// ValidatedUpdateForm`] と対になる束縛結果）。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundUpdateForm {
    Single(BoundUpdate),
    Predicate(BoundPredicateUpdate),
}

/// [`crate::sql::allowlist::ValidatedUpdateForm`] を `schema`・UDF レジストリ
/// `udfs` と照合して [`BoundUpdateForm`] へ束縛する（SQL-19、TASK-192・
/// Issue #869 の公開 API）。`Single` 腕は既存 [`bind_update`] と完全に同一の
/// `BoundUpdate` を返す（SET 束縛を [`bind_set_assignments`] で共有するため）。
///
/// `Predicate` 腕は SET 束縛の後、`WHERE` 述語列を [`bind_where_predicates`]
/// （`SELECT`・集計 `SELECT`・広域取得 `SELECT` と同一の意味論。`VECTOR` 列への
/// 等価／前方一致は `declarative_filter` が `22000`、述語件数は既存の
/// `MAX_METADATA_FILTERS`〔54000〕、式ノードは `MAX_EXPR_NODES` が頭打ちにする）
/// で束縛する。両フィルタが空（`WHERE` 省略に構造上相当する `visible()` 単独の
/// 恒等述語）の場合は実質的な全行更新になるため [`SqlSurfaceError::unsupported`]
/// （`42601`）で拒否する（`WHERE` 自体の省略は許可リスト層〔`sql::allowlist`〕が
/// 構造的に拒否済み。ここでの判定は `visible()` のみという実質的な無条件更新
/// への fail-closed な追加防御）。
pub fn bind_update_form(
    stmt: &crate::sql::allowlist::ValidatedUpdateForm,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundUpdateForm, SqlSurfaceError> {
    use crate::sql::allowlist::ValidatedUpdateForm;

    match stmt {
        ValidatedUpdateForm::Single(single) => {
            Ok(BoundUpdateForm::Single(bind_update(single, schema)?))
        }
        ValidatedUpdateForm::Predicate(predicate) => {
            let assignments = bind_set_assignments(&predicate.assignments, schema)?;

            let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
            let (metadata_filters, expr_filters, _rls_predicate_present, or_filters) =
                bind_where_predicates(
                    &predicate.where_predicates,
                    schema,
                    udfs,
                    &mut node_budget,
                    &[],
                )?;

            // TASK-208・Issue #912: `or_filters` を含めないと `WHERE a OR b`
            // だけの述語（`metadata_filters`／`expr_filters` は両方空）が
            // 「無条件 UPDATE」と誤判定され、正当な OR 述語つき UPDATE が
            // 拒否される（fail-closed の過剰側ではあるが正当な入力を壊す
            // 回帰になるため、判定漏れとして修正する）。
            if metadata_filters.is_empty() && expr_filters.is_empty() && or_filters.is_empty() {
                return Err(SqlSurfaceError::unsupported(
                    "predicate-form UPDATE WHERE clause must contain at least one non-visible() predicate (unconditional UPDATE is not supported; use TRUNCATE for whole-table operations)",
                ));
            }

            Ok(BoundUpdateForm::Predicate(BoundPredicateUpdate::new(
                predicate.table_name.clone(),
                assignments,
                metadata_filters,
                expr_filters,
                or_filters,
                predicate.operation_id.clone(),
            )))
        }
    }
}

/// ファイル形 `INSERT` の束縛結果（TASK-120・対象ビヘイビア: INDEX-1, INDEX-2）。
///
/// `sql::exec::execute_file_insert` → `incremental::index_file` へ渡され、`path`/`body`
/// はそのままチャンク化の入力になる（`incremental.rs` モジュールドキュメント参照）。
/// `template_values` はスキーマ列順で、`path`/`body`/VECTOR 列の位置は必ず
/// `Value::Null`（各チャンク行の構築時に上書きされるプレースホルダ。本文全文を
/// 残さないことでチャンク数分の複製増幅を避ける）、それ以外の Text 列
/// （例 `lang`）は全チャンク行へ複製される値を保持する。
#[derive(Debug, Clone)]
pub struct BoundFileInsert {
    pub table: String,
    pub path: String,
    pub body: String,
    pub path_column_index: usize,
    pub body_column_index: usize,
    pub vector_column_index: usize,
    pub template_values: Vec<crate::row_codec::Value>,
    /// TASK-92（RECOVER-1）: [`BoundInsert::operation_id`] と同じく
    /// `ValidatedInsert.operation_id` をそのまま素通しする（行形・ファイル形で
    /// `sql::allowlist::validate_insert` の必須化ガードを共有するため、
    /// `LedgerMode::Ledgered`（既定）では常に `Some`）。
    pub operation_id: Option<OperationId>,
}

/// [`OnConflictAction::DoUpdate`] の SET 右辺を束縛した形（SQL-20・TASK-193、
/// Issue #872）。`EXCLUDED.<col>` は新規挿入しようとした行（[`BoundUpsert::
/// rows`] の対応する行）の `values` 列インデックス参照へ束縛する（実行時に
/// 都度列名を引き直さない）。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundUpsertValue {
    /// `BoundInsert::values` の列インデックス（同じ行の束縛済み値を指す）。
    Excluded(usize),
    Literal(crate::row_codec::Value),
}

/// 束縛済みの `ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...`（SQL-20・
/// TASK-193、Issue #872）。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundConflictAction {
    DoNothing,
    /// (`schema.columns` の対象列インデックス, 右辺) 対応。宣言順を保持する
    /// （[`BoundUpdate::assignments`] と同じ契約）。
    DoUpdate(Vec<(usize, BoundUpsertValue)>),
}

/// 束縛済みの UPSERT 文（SQL-20・TASK-193、Issue #872。実行結線は
/// `sql::exec::execute_upsert`）。`rows` は [`bind_insert_row`] で個別に束縛
/// 済みの行（行数に関わらず 1 件以上）で、`action` は全行が共有する衝突分岐
/// （`ValidatedInsert` 由来のため構造的に同一テーブル・同一 `operation_id`）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundUpsert {
    pub table: String,
    pub rows: Vec<BoundInsert>,
    pub action: BoundConflictAction,
    pub operation_id: Option<OperationId>,
}

/// [`bind_insert_form`] の束縛結果。行形（既存の 1 行 1 ID `INSERT`）・複数行形
/// （SQL-16、TASK-190。複数行 `VALUES` を持つ行形）・ファイル形（TASK-120。
/// サーバー側チャンク化・ベクトル化を経由する `INSERT`）・UPSERT（SQL-20・
/// TASK-193、Issue #872）を区別する。
#[derive(Debug, Clone)]
pub enum BoundInsertForm {
    Row(BoundInsert),
    /// 複数行 `VALUES (...), (...), ...`（SQL-16、TASK-190）を束縛した行の並び
    /// （投入順を保持）。全行が同一テーブル・同一 `operation_id`
    /// （`ValidatedInsert` 由来のため構造的に保証される）。実行は
    /// `sql::exec::execute_insert_batch_with_schema` が NoSQL 表層の `rows[]`
    /// （NOSQL-6・TASK-178）と共有する（第 2 の書き込み経路を作らない）。
    RowBatch(Vec<BoundInsert>),
    File(BoundFileInsert),
    /// `ON CONFLICT (id) DO NOTHING | DO UPDATE SET ...`（SQL-20・TASK-193、
    /// Issue #872）。単一行・複数行 `VALUES` のいずれも本 variant を経由する
    /// （`Row`／`RowBatch` とは独立。`ON CONFLICT` の有無で分岐する）。
    Upsert(BoundUpsert),
}

/// `ValidatedInsert` の列リストから行形・ファイル形いずれの `INSERT` かを束縛段階で
/// 判別し、対応する束縛結果を返す（TASK-120・対象ビヘイビア: INDEX-1, INDEX-2。
/// 複数行 `VALUES` の受理は SQL-16・TASK-190）。許可リスト（`sql::allowlist`）・
/// 構文（`sql::lexer`）は行形・ファイル形で共通のままであり、本関数だけが形を
/// 分岐させる（`sql/exec.rs`・`core.rs` の呼び出し元モジュールドキュメント参照）。
///
/// 判別規則（すべて満たす場合のみファイル形。行数には依存しない）:
/// - 列リストに疑似列 `id` を含まない
/// - 列リストに、スキーマ上 `VECTOR` 型である列を含まない
/// - 列リストに Text 列 `path` と `body` を両方含む
///
/// いずれか 1 つでも欠ける場合（`id` または VECTOR 列を同時指定した場合を含む）は
/// 行形として扱う。ファイル形と判定されたにもかかわらず複数行 `VALUES` を持つ文は
/// `42601` で拒否する（ファイル複数投入は既存の [`crate::sql::exec::execute_insert_batch`]
/// 系の別経路が担い、複数行 `VALUES` との併用は受理しない設計判断。将来ファイル形の
/// 複数行対応が必要になれば別途構文を起こす）。行形は `stmt.rows.len() == 1` なら
/// 既存の [`bind_insert`] へそのまま委譲し外部観測可能な挙動をビット同一に保ち
/// （黙って別セマンティクスへ丸めない。行形の既存テスト・エラー契約は本関数
/// 導入後も無変更）、`> 1` なら各行を束縛して [`BoundInsertForm::RowBatch`] を返す
/// （束縛失敗はどの行で失敗したかに関わらず全体を `Err` として拒否し、部分成功は
/// しない）。
pub fn bind_insert_form(
    stmt: &ValidatedInsert,
    schema: &TableSchema,
) -> Result<BoundInsertForm, SqlSurfaceError> {
    let has_id = stmt.columns.iter().any(|c| c == "id");
    let has_vector_column = stmt.columns.iter().any(|c| {
        schema
            .columns
            .iter()
            .any(|sc| &sc.name == c && matches!(sc.ty, ColumnType::Vector(_)))
    });
    let has_path = stmt.columns.iter().any(|c| c == "path");
    let has_body = stmt.columns.iter().any(|c| c == "body");
    let is_file_form_shape = !has_id && !has_vector_column && has_path && has_body;

    // `ON CONFLICT ...`（SQL-20・TASK-193、Issue #872）は判別規則より前に分岐
    // する（ファイル形との併用は明示的に `42601` で拒否し、行形との併用は
    // 行数に関わらず必ず `bind_upsert_form` を経由させる）。
    if let Some(action) = &stmt.on_conflict {
        if is_file_form_shape {
            return Err(SqlSurfaceError::unsupported(
                "ON CONFLICT is not supported for file-form INSERT (path/body columns)",
            ));
        }
        return bind_upsert_form(stmt, action, schema);
    }

    if is_file_form_shape {
        if stmt.rows.len() > 1 {
            return Err(SqlSurfaceError::unsupported(
                "multi-row VALUES is not supported for file-form INSERT (path/body columns)",
            ));
        }
        bind_file_insert(stmt, schema).map(BoundInsertForm::File)
    } else if stmt.rows.len() <= 1 {
        bind_insert(stmt, schema).map(BoundInsertForm::Row)
    } else {
        let mut bounds: Vec<BoundInsert> = Vec::new();
        bounds.try_reserve_exact(stmt.rows.len()).map_err(|_| {
            SqlSurfaceError::payload_too_large("failed to reserve INSERT row batch buffer")
        })?;
        for values in &stmt.rows {
            bounds.push(bind_insert_row(
                &stmt.table_name,
                &stmt.columns,
                values,
                &stmt.operation_id,
                schema,
            )?);
        }
        Ok(BoundInsertForm::RowBatch(bounds))
    }
}

/// [`bind_insert_form`] が `stmt.on_conflict` を検出した場合の束縛本体
/// （SQL-20・TASK-193、Issue #872）。行数に関わらず全行を [`bind_insert_row`]
/// で個別に束縛してからバッチ内 `id` 重複を検出する（`tenant::insert_typed_
/// rows_unchecked` の `TenantWriteError::IdConflict`〔`23505`〕は UPSERT の
/// 衝突分岐とは意味が異なり使えないため、束縛時点〔write トランザクション開始
/// 前・決定的〕で `22000` として拒否する。2 行目を「1 行目への更新」と解釈
/// しない）。最後に `DO UPDATE SET` の右辺（[`bind_upsert_assignments`]）を
/// 束縛し、`EXCLUDED.<col>` が参照する列が `Null` かつ対象列が非 nullable の
/// 組み合わせを行ごとに検出する（`docs/design/sql-upsert.md` 参照）。
fn bind_upsert_form(
    stmt: &ValidatedInsert,
    action: &OnConflictAction,
    schema: &TableSchema,
) -> Result<BoundInsertForm, SqlSurfaceError> {
    let mut rows: Vec<BoundInsert> = Vec::new();
    rows.try_reserve_exact(stmt.rows.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("failed to reserve UPSERT row batch buffer")
    })?;
    for values in &stmt.rows {
        rows.push(bind_insert_row(
            &stmt.table_name,
            &stmt.columns,
            values,
            &stmt.operation_id,
            schema,
        )?);
    }

    let mut seen_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    seen_ids
        .try_reserve(rows.len())
        .map_err(|_| SqlSurfaceError::payload_too_large("failed to reserve UPSERT id set"))?;
    for row in &rows {
        if !seen_ids.insert(row.id) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate id {} within the same UPSERT statement",
                row.id
            )));
        }
    }

    let bound_action = match action {
        OnConflictAction::DoNothing => BoundConflictAction::DoNothing,
        OnConflictAction::DoUpdate(assignments) => {
            let bound_assignments = bind_upsert_assignments(assignments, schema)?;
            for row in &rows {
                for (col_idx, value) in &bound_assignments {
                    let BoundUpsertValue::Excluded(src_idx) = value else {
                        continue;
                    };
                    // `EXCLUDED.<col>` が実際に NULL かどうかは `Value::Null`
                    // そのもの（`src_idx` が範囲外の場合も fail-closed に NULL
                    // 扱い）でのみ判定する。以前は `Vector`／`Text` 以外を
                    // すべて NULL とみなしていたため、`Value::Array`（Issue
                    // #888）を持つ NOT NULL な配列列に対する
                    // `ON CONFLICT DO UPDATE SET <array> = EXCLUDED.<array>`
                    // が常に拒否されていた（Cursor Bugbot 指摘・PR #1011）。
                    let is_null = matches!(
                        row.values.get(*src_idx),
                        None | Some(crate::row_codec::Value::Null)
                    );
                    let target_nullable = schema
                        .columns
                        .get(*col_idx)
                        .map(|c| c.nullable)
                        .unwrap_or(false);
                    if is_null && !target_nullable {
                        let target_name = schema
                            .columns
                            .get(*col_idx)
                            .map(|c| c.name.as_str())
                            .unwrap_or("?");
                        let src_name = schema
                            .columns
                            .get(*src_idx)
                            .map(|c| c.name.as_str())
                            .unwrap_or("?");
                        // 文言に行 `id` を含めていた旧実装から、他分類
                        // （`NotNullViolation`・`23502`）との一貫性のため列名の
                        // みを含む固定形式へ変更（TABLE-16・TASK-204、
                        // Issue #904。src_name は EXCLUDED 修飾子の参照先の
                        // ため引き続きログ的に有用だが `client_message` の
                        // 一般文言と型を揃える）。
                        let _ = (src_name, row.id);
                        return Err(SqlSurfaceError::not_null_violation(target_name));
                    }
                }
            }
            BoundConflictAction::DoUpdate(bound_assignments)
        }
    };

    Ok(BoundInsertForm::Upsert(BoundUpsert {
        table: stmt.table_name.clone(),
        rows,
        action: bound_action,
        operation_id: stmt.operation_id.clone(),
    }))
}

/// `ON CONFLICT ... DO UPDATE SET` の右辺を束縛する（SQL-20・TASK-193、
/// Issue #872）。対象列の検証（禁止列・重複・未知列）は
/// [`bind_set_assignments`] と同じ規約だが、右辺が `EXCLUDED.<col>`
/// （[`UpsertValue::Excluded`]）を取り得る点が異なるため独立した実装とする
/// （`UpdateWhereForm`／`OnConflictAction` は互いに独立した文法であり、右辺の
/// 型が異なる〔`InsertLiteral` 対 `UpsertValue`〕ため共通化すると分岐が
/// かえって読みにくくなる）。
///
/// 検出する違反（拒否コードは [`bind_set_assignments`] と同じ分類）:
/// - 対象列に疑似列 `id`・RLS 内部列 `tenant_id`／`visibility` を指定 → `42601`
/// - `EXCLUDED.<src>` の `src` に同じ禁止列を指定 → `42601`（サーバー側が
///   導出・固定する `id`／`tenant_id`／`visibility` を、新規行側の値を経由して
///   書き換える迂回路を作らないため）
/// - SET 内の対象列名重複 → `22000`
/// - 対象列・`src` 列のいずれかが未知の列名 → `22000`
/// - `EXCLUDED.<src>` の型が対象列の型と不一致（`ColumnType` は `VECTOR(N)`
///   の次元も含めて `PartialEq` で完全一致比較する）→ `22000`
/// - リテラル右辺の型不一致（列型とリテラル種別）→ `22000`
fn bind_upsert_assignments(
    assignments: &[(String, UpsertValue)],
    schema: &TableSchema,
) -> Result<Vec<(usize, BoundUpsertValue)>, SqlSurfaceError> {
    let mut seen_columns: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut bound: Vec<(usize, BoundUpsertValue)> = Vec::with_capacity(assignments.len());

    for (name, value) in assignments {
        if name == "id" || name == "tenant_id" || name == "visibility" {
            return Err(SqlSurfaceError::unsupported(format!(
                "column {name:?} cannot be targeted by SET"
            )));
        }
        if !seen_columns.insert(name.as_str()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate column in SET clause: {name}"
            )));
        }
        let col_idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let column = schema
            .columns
            .get(col_idx)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;

        let bound_value = match value {
            UpsertValue::Excluded(src) => {
                if src == "id" || src == "tenant_id" || src == "visibility" {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "column {src:?} cannot be referenced by EXCLUDED"
                    )));
                }
                let src_idx = schema
                    .columns
                    .iter()
                    .position(|c| &c.name == src)
                    .ok_or_else(|| {
                        SqlSurfaceError::invalid_input(format!("unknown column: {src}"))
                    })?;
                let src_column = schema.columns.get(src_idx).ok_or_else(|| {
                    SqlSurfaceError::invalid_input(format!("unknown column: {src}"))
                })?;
                if src_column.ty != column.ty {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} and EXCLUDED.{src} have mismatched types"
                    )));
                }
                BoundUpsertValue::Excluded(src_idx)
            }
            UpsertValue::Literal(literal) => {
                let v = match (&column.ty, literal) {
                    (ColumnType::Vector(dim), InsertLiteral::String(s)) => {
                        crate::row_codec::Value::Vector(parse_vector_literal(s, *dim)?)
                    }
                    (ColumnType::Vector(dim), InsertLiteral::Vector(values)) => {
                        bind_vector_literal_values(values, *dim, name)?
                    }
                    (ColumnType::Vector(_), InsertLiteral::Number(_) | InsertLiteral::Bool(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a vector literal, got a non-vector literal"
                        )))
                    }
                    (ColumnType::Text, InsertLiteral::String(s)) => {
                        crate::row_codec::Value::Text(s.clone())
                    }
                    (ColumnType::Text, InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a text literal, got a non-text literal"
                        )))
                    }
                    (ColumnType::Boolean, InsertLiteral::Bool(b)) => {
                        crate::row_codec::Value::Bool(*b)
                    }
                    (ColumnType::Boolean, InsertLiteral::String(_) | InsertLiteral::Number(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a boolean literal (true/false)"
                        )))
                    }
                    (ColumnType::Integer | ColumnType::BigInt, InsertLiteral::Number(_) | InsertLiteral::String(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        bind_integer_literal(name, column.ty.clone(), literal)?
                    }
                    (ColumnType::Real, InsertLiteral::Number(n)) => {
                        crate::row_codec::Value::Real(bind_real_literal(n)?)
                    }
                    (ColumnType::Real, InsertLiteral::String(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a REAL literal, got a non-numeric literal"
                        )))
                    }
                    (ColumnType::Double, InsertLiteral::Number(n)) => {
                        crate::row_codec::Value::Double(bind_double_literal(n)?)
                    }
                    (ColumnType::Double, InsertLiteral::String(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a DOUBLE PRECISION literal, got a non-numeric literal"
                        )))
                    }
                    (ColumnType::Date, InsertLiteral::String(s)) => {
                        bind_datetime_literal(name, ColumnType::Date, s)?
                    }
                    (ColumnType::Date, InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a DATE literal (YYYY-MM-DD)"
                        )))
                    }
                    (ColumnType::Timestamp, InsertLiteral::String(s)) => {
                        bind_datetime_literal(name, ColumnType::Timestamp, s)?
                    }
                    (ColumnType::Timestamp, InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a TIMESTAMP literal (YYYY-MM-DD HH:MM:SS)"
                        )))
                    }
                    (ColumnType::Array(array_ty), InsertLiteral::String(s)) => {
                        crate::row_codec::Value::Array(parse_array_literal(s, *array_ty)?)
                    }
                    (ColumnType::Array(_), InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects an array literal, got a non-array literal"
                        )))
                    }
                    (ColumnType::Bytea, InsertLiteral::String(s)) => bind_bytea_literal(s, name)?,
                    (ColumnType::Bytea, InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a bytea hex literal"
                        )))
                    }
                    (ColumnType::Json | ColumnType::Jsonb, InsertLiteral::String(s)) => {
                        bind_json_literal(s, &column.ty, name)?
                    }
                    (
                        ColumnType::Json | ColumnType::Jsonb,
                        InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_),
                    ) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a JSON text literal"
                        )))
                    }
                    (ColumnType::Enum(def), InsertLiteral::String(s)) => {
                        bind_enum_literal(def, s, name)?
                    }
                    (ColumnType::Enum(_), InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a text literal for its enum type"
                        )))
                    }
                    // `InsertLiteral::Null`（Issue #889 レビュー指摘）は
                    // `ON CONFLICT ... DO UPDATE SET` の SQL 構文からは構築
                    // されない到達不能パス（match の網羅性のためだけの分岐）。
                    // 到達した場合も明示 NULL の確定契約（TABLE-16）へ倒す。
                    (_, InsertLiteral::Null) if column.nullable => crate::row_codec::Value::Null,
                    (_, InsertLiteral::Null) => {
                        return Err(SqlSurfaceError::not_null_violation(name.clone()))
                    }
                    (ColumnType::Uuid, InsertLiteral::String(s)) => bind_uuid_literal(s, name)?,
                    (ColumnType::Uuid, InsertLiteral::Number(_) | InsertLiteral::Bool(_) | InsertLiteral::Vector(_)) => {
                        return Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} expects a UUID text literal"
                        )))
                    }
                    (ColumnType::Numeric { precision, scale }, lit) => {
                        bind_numeric_literal(lit, name, *precision, *scale)?
                    }
                };
                BoundUpsertValue::Literal(v)
            }
        };
        bound.push((col_idx, bound_value));
    }

    Ok(bound)
}

/// [`bind_insert_form`] がファイル形と判定した場合の束縛本体。呼び出し元
/// （[`bind_insert_form`]）が `stmt.rows.len() > 1` を先に `42601` で拒否するため、
/// ここでは常に `stmt.rows.first()` の単一行を見る（SQL-16、TASK-190）。
fn bind_file_insert(
    stmt: &ValidatedInsert,
    schema: &TableSchema,
) -> Result<BoundFileInsert, SqlSurfaceError> {
    let values = stmt
        .rows
        .first()
        .ok_or_else(|| SqlSurfaceError::invalid_input("INSERT statement has no VALUES row"))?;
    if stmt.columns.len() != values.len() {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT column count does not match value count",
        ));
    }

    let mut seen_columns: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for name in &stmt.columns {
        if !seen_columns.insert(name.as_str()) {
            return Err(SqlSurfaceError::invalid_input(format!(
                "duplicate column in INSERT column list: {name}"
            )));
        }
    }

    let vector_column_index = schema
        .columns
        .iter()
        .position(|c| matches!(c.ty, ColumnType::Vector(_)))
        .ok_or_else(|| SqlSurfaceError::invalid_input("table has no VECTOR column"))?;
    let path_column_index = schema
        .columns
        .iter()
        .position(|c| c.name == "path")
        .ok_or_else(|| SqlSurfaceError::invalid_input("table has no path column"))?;
    let body_column_index = schema
        .columns
        .iter()
        .position(|c| c.name == "body")
        .ok_or_else(|| SqlSurfaceError::invalid_input("table has no body column"))?;
    match schema.columns.get(path_column_index) {
        Some(c) if matches!(c.ty, ColumnType::Text) => {}
        _ => return Err(SqlSurfaceError::invalid_input("path column must be Text")),
    }
    match schema.columns.get(body_column_index) {
        Some(c) if matches!(c.ty, ColumnType::Text) => {}
        _ => return Err(SqlSurfaceError::invalid_input("body column must be Text")),
    }

    let mut template_values: Vec<crate::row_codec::Value> =
        vec![crate::row_codec::Value::Null; schema.columns.len()];
    let mut provided = vec![false; schema.columns.len()];
    let mut path_value: Option<String> = None;
    let mut body_value: Option<String> = None;

    for (name, literal) in stmt.columns.iter().zip(values.iter()) {
        let col_idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let column = schema
            .columns
            .get(col_idx)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let value = match (&column.ty, literal) {
            (ColumnType::Text, InsertLiteral::String(s)) => {
                crate::row_codec::Value::Text(s.clone())
            }
            (ColumnType::Text, InsertLiteral::Number(_) | InsertLiteral::Bool(_)) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a text literal, got a non-text literal"
                )))
            }
            // `InsertLiteral::Vector`（Issue #896 レビュー指摘・PR #1038）は
            // NoSQL 表層専用であり、ファイル形 `INSERT` の VALUES 構文からは
            // 構築されない到達不能パス（match の網羅性のためだけの分岐）。
            (ColumnType::Text, InsertLiteral::Vector(_)) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} expects a text literal, got a vector literal"
                )))
            }
            // `InsertLiteral::Null`（Issue #889 レビュー指摘）はファイル形
            // `INSERT` の VALUES 構文からは構築されない到達不能パス（match の
            // 網羅性のためだけの分岐）。到達した場合も明示 NULL の確定契約
            // （TABLE-16。nullable なら NULL・非 nullable なら `23502`）へ倒す。
            (ColumnType::Text, InsertLiteral::Null) if column.nullable => {
                crate::row_codec::Value::Null
            }
            (ColumnType::Text, InsertLiteral::Null) => {
                return Err(SqlSurfaceError::not_null_violation(name.clone()))
            }
            // `bind_insert_form` の判別規則により VECTOR 列名は列リストに含まれない
            // 前提だが、防御的に拒否する（各チャンクのベクトルはサーバー側が
            // `incremental.rs` で埋め込み結果から設定し、クライアント入力を使わない）。
            (ColumnType::Vector(_), _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: VECTOR column must not be provided for file-form INSERT"
                )))
            }
            // INTEGER／BIGINT 列も他のスカラー型（REAL／DOUBLE PRECISION 等）と同じ
            // 理由でファイル形 INSERT の対象外とする（Issue #881 レビュー指摘。
            // typed INSERT/UPDATE/UPSERT 向けの `bind_integer_literal` をファイル形へ
            // 露出させない。当初この分岐だけ他の非 TEXT 型より緩く受理していたのを
            // codex/review・Cursor 指摘で是正）。
            (ColumnType::Integer | ColumnType::BigInt, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: INTEGER/BIGINT column is not supported for file-form INSERT"
                )))
            }
            // REAL／DOUBLE PRECISION 列も他のスカラー型（BOOLEAN 等）と同じ理由で
            // ファイル形 INSERT の対象外とする（Issue #882 レビュー指摘。typed
            // INSERT/UPDATE 向けの束縛処理をファイル形へ露出させない）。
            (ColumnType::Real | ColumnType::Double, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: REAL/DOUBLE PRECISION column is not supported for file-form INSERT"
                )))
            }
            // ファイル形 INSERT は `path`/`body` の TEXT 列規約専用（本モジュール
            // ドキュメント参照）。BOOLEAN 列は対象外として拒否する（Issue #883）。
            (ColumnType::Boolean, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: BOOLEAN column is not supported for file-form INSERT"
                )))
            }
            // DATE／TIMESTAMP 列も同じ理由で対象外（TABLE-13・TASK-197、
            // Issue #884）。
            (ColumnType::Date | ColumnType::Timestamp, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: DATE/TIMESTAMP column is not supported for file-form INSERT"
                )))
            }
            // 配列列（TABLE-14・Issue #888）も BOOLEAN と同じく対象外として拒否する。
            (ColumnType::Array(_), _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: ARRAY column is not supported for file-form INSERT"
                )))
            }
            // BYTEA 列も同じ理由で対象外とする（Issue #886）。
            (ColumnType::Bytea, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: BYTEA column is not supported for file-form INSERT"
                )))
            }
            // JSON／JSONB 列も同じ理由で対象外とする（Issue #889 D6）。
            (ColumnType::Json | ColumnType::Jsonb, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: JSON column is not supported for file-form INSERT"
                )))
            }
            // ENUM 列も同じ理由で対象外とする（Issue #890）。
            (ColumnType::Enum(_), _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: ENUM column is not supported for file-form INSERT"
                )))
            }
            // NUMERIC 列も同じ理由で対象外（TABLE-13〔検討中〕・TASK-197、
            // Issue #885）。
            (ColumnType::Numeric { .. }, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: NUMERIC column is not supported for file-form INSERT"
                )))
            }
            // UUID 列も同じ理由で対象外（TABLE-13〔検討中〕・TASK-197、
            // Issue #887・U9）。
            (ColumnType::Uuid, _) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?}: UUID column is not supported for file-form INSERT"
                )))
            }
        };
        if col_idx == path_column_index {
            if let crate::row_codec::Value::Text(ref s) = value {
                path_value = Some(s.clone());
            }
        }
        if col_idx == body_column_index {
            if let crate::row_codec::Value::Text(ref s) = value {
                body_value = Some(s.clone());
            }
        }
        if let Some(slot) = template_values.get_mut(col_idx) {
            *slot = value;
        }
        if let Some(flag) = provided.get_mut(col_idx) {
            *flag = true;
        }
    }

    // 省略列への `DEFAULT` 補完・非 nullable 検査（TABLE-16・TASK-204、
    // Issue #904）。VECTOR 列はクライアントが指定しない（埋め込み結果で後から
    // 埋める）ため、この判定自体の対象外として読み飛ばす
    // （`fill_omitted_columns` を素朴に適用すると VECTOR 列が
    // 「省略・DEFAULT なし・非 nullable」として誤って拒否されるため専用ループを保つ）。
    for (idx, column) in schema.columns.iter().enumerate() {
        if matches!(column.ty, ColumnType::Vector(_)) {
            continue;
        }
        let is_provided = provided.get(idx).copied().unwrap_or(false);
        if is_provided {
            continue;
        }
        match &column.default {
            Some(default) => {
                let value = bind_column_default(column, default)?;
                if let Some(slot) = template_values.get_mut(idx) {
                    *slot = value;
                }
            }
            None => {
                if !column.nullable {
                    return Err(SqlSurfaceError::not_null_violation(column.name.clone()));
                }
            }
        }
    }

    let path = path_value
        .ok_or_else(|| SqlSurfaceError::invalid_input("missing value for path column"))?;
    let body = body_value
        .ok_or_else(|| SqlSurfaceError::invalid_input("missing value for body column"))?;

    // `path`/`body`/VECTOR 列の位置はチャンク行ごとに必ず上書きされるため、テンプレート
    // 側では `Value::Null` に戻して保持する。ここに本文全文を残すと
    // `incremental::index_file` のチャンクループが行ごとに本文全体を複製 → 直後に破棄
    // することになり、単一の untrusted 入力で「本文サイズ × チャンク数」の確保・コピーを
    // 誘発できる（codex-review P1 指摘・PR #221。security.md「不安全な設計 / DoS」）。
    for idx in [path_column_index, body_column_index, vector_column_index] {
        if let Some(slot) = template_values.get_mut(idx) {
            *slot = crate::row_codec::Value::Null;
        }
    }

    Ok(BoundFileInsert {
        table: stmt.table_name.clone(),
        path,
        body,
        path_column_index,
        body_column_index,
        vector_column_index,
        template_values,
        operation_id: stmt.operation_id.clone(),
    })
}

/// 集計項目 1 つの引数を意味論的に解決した結果（TASK-166・SQL-13）。
/// `sql::aggregate::execute_aggregate` はこの enum だけを見て走査中の 1 行から
/// 集計対象値を取り出す（`schema`・`udfs` を再度参照しない自己完結な形）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AggregateInput {
    /// `COUNT(*)`・`COUNT(id)`・`COUNT(<Scalar 型の式>)` のいずれか。可視行はすべて
    /// 対象（NULL・非存在の概念がない）。`COUNT` 以外の関数からこの variant を得る
    /// ことはない（[`resolve_aggregate_input`] 参照）。`VECTOR` 列の裸の列参照は
    /// nullable 属性を持つため対象外（[`AggregateInput::VectorColumnPresence`]）。
    AllVisible,
    /// 疑似列 `id`（`SUM`/`AVG`/`MIN`/`MAX`）。`f64` へ変換せず `u64` の
    /// `checked_add` で正確に演算する（`docs/spec/04-behavior/error-format.md`
    /// ERR-2 が新設する `22003` で桁あふれを fail-closed に拒否するため）。
    IdU64,
    /// `TEXT` 列の裸の列参照（`schema.columns` の添字）。`COUNT`（非 NULL 行数）・
    /// `MIN`/`MAX`（バイト順・NULL 無視）でのみ使う（`SUM`/`AVG` は
    /// [`resolve_aggregate_input`] が型不整合として拒否済み）。
    TextColumn(usize),
    /// `INTEGER` 列の裸の列参照（`schema.columns` の添字）。`COUNT`/`SUM`/
    /// `AVG`/`MIN`/`MAX` のすべてで使う（TABLE-13・TASK-196、Issue #881・
    /// #892）。`SUM` の結果は `i128` 累積・確定時に `i64` へ収まるか検査する
    /// （D2。`Accumulator::IntSum`）。
    IntegerColumn(usize),
    /// `BIGINT` 列の裸の列参照。`IntegerColumn` と同じ受理範囲・累積方式を
    /// 共有する（`ScalarRef::BigInt` からそのまま `i64` を取り出す点のみが
    /// 異なる）。
    BigIntColumn(usize),
    /// `REAL` 列の裸の列参照。`COUNT`/`SUM`/`AVG`/`MIN`/`MAX` のすべてで使う
    /// （Issue #892）。`f32` は `f64::from` で無損失に拡張し、既存の
    /// `Accumulator::FloatSum`/`FloatAvg`/`FloatMin`/`FloatMax`（`ScalarExpr`
    /// と共有）へ観測する。結果はいずれも `Cell::Float`（DOUBLE PRECISION
    /// 相当。本リポの実装既定値）。
    RealColumn(usize),
    /// `DOUBLE PRECISION` 列の裸の列参照。`RealColumn` と同じ受理範囲・
    /// 累積方式を共有する（`f64` をそのまま使う点のみが異なる）。
    DoubleColumn(usize),
    /// `BOOLEAN` 列の裸の列参照（`schema.columns` の添字）。`COUNT`（非 NULL
    /// 行数）でのみ使う（TABLE-13・TASK-196、Issue #883）。`SUM`/`AVG`/`MIN`/
    /// `MAX` は `TextColumn` と同じパターンで [`resolve_aggregate_input`] が
    /// 型不整合として拒否する。
    BooleanColumn(usize),
    /// `DATE` 列の裸の列参照（`schema.columns` の添字）。`COUNT`（非 NULL
    /// 行数）・`MIN`/`MAX`（1970-01-01 起点の日数の全順序比較・NULL 無視）
    /// で使う（TABLE-13・TASK-197、Issue #884・#892）。`SUM`/`AVG` は
    /// [`resolve_aggregate_input`] が型不整合として拒否する（暦日の合計・平均に
    /// 意味論がないため）。
    DateColumn(usize),
    /// `TIMESTAMP` 列の裸の列参照（`schema.columns` の添字）。`DateColumn` と
    /// 同じ受理範囲（`COUNT`・`MIN`/`MAX`）を持つ（TABLE-13・TASK-197、
    /// Issue #884・#892）。
    TimestampColumn(usize),
    /// `ARRAY` 列の裸の列参照（`schema.columns` の添字）。`COUNT`（非 NULL 行数）
    /// でのみ使う（TABLE-14・TASK-198、Issue #888・D-A8）。`SUM`/`AVG`/`MIN`/
    /// `MAX` は `TextColumn`/`BooleanColumn` と同じパターンで
    /// [`resolve_aggregate_input`] が型不整合として拒否する。
    ArrayColumn(usize),
    /// `BYTEA` 列の裸の列参照（`schema.columns` の添字）。`COUNT`（非 NULL
    /// 行数）でのみ使う（TABLE-13・TASK-197、Issue #886）。`SUM`/`AVG`/`MIN`/
    /// `MAX` は `BooleanColumn` と同じパターンで [`resolve_aggregate_input`] が
    /// 型不整合として拒否する。
    ByteaColumn(usize),
    /// `JSON`／`JSONB` 列の裸の列参照（`schema.columns` の添字）。`COUNT`（非 NULL
    /// 行数）でのみ使う（TABLE-14・TASK-198、Issue #889）。`SUM`/`AVG`/`MIN`/`MAX`
    /// は `ByteaColumn` と同じパターンで [`resolve_aggregate_input`] が型不整合
    /// として拒否する。
    JsonColumn(usize),
    /// `ENUM` 列の裸の列参照（`COUNT` 限定。TABLE-14・TASK-198、Issue #890）。
    /// `SUM`/`AVG`/`MIN`/`MAX` は `BooleanColumn`／`ByteaColumn` と同じパターンで
    /// [`resolve_aggregate_input`] が型不整合として拒否する（PostgreSQL の enum は
    /// 宣言順で `MIN`/`MAX` 比較できるが、辞書順で代用すると意味論が食い違うため
    /// 意図的に受理しない。Issue #890 D7）。
    EnumColumn(usize),
    /// `NUMERIC(p, s)` 列の裸の列参照（TABLE-13〔検討中〕・TASK-197、
    /// Issue #885・#892）。`COUNT`（非 NULL 行数）・`SUM`/`AVG`/`MIN`/`MAX`
    /// （unscaled i128 累積・列の `precision`/`scale` を保持）のすべてで使う。
    /// `precision`/`scale` は `Accumulator::new`（`SUM`/`AVG` の桁あふれ判定・
    /// `AVG` の結果 scale 決定）が必要とするため、列参照の時点で複製して
    /// 保持する（行走査のたびにスキーマを引き直さない設計）。
    NumericColumn {
        index: usize,
        precision: u8,
        scale: u8,
    },
    /// `UUID` 列の裸の列参照（`schema.columns` の添字）。`COUNT`（非 NULL
    /// 行数）でのみ使う（TABLE-13〔検討中〕・TASK-197、Issue #887）。`SUM`/
    /// `AVG`/`MIN`/`MAX` は `TextColumn`/`BooleanColumn` と同じパターンで
    /// [`resolve_aggregate_input`] が型不整合として拒否する。
    UuidColumn(usize),
    /// 上記以外の `Scalar` 型に束縛された式（列参照 `id` 単体を除く。`vec_norm(...)`
    /// 等の組み込み関数・宣言的 UDF 呼び出し・四則演算）。`program`（束縛時に
    /// ステップ列コンパイル済み、Issue #353）を行ループで評価する。`source` は
    /// 元の `BoundExpr`（EXPLAIN・テストの可観測性のため残置。実行経路は
    /// `program` のみを見る）。
    ScalarExpr {
        source: crate::sql::udf_call::BoundExpr,
        program: crate::sql::expr_program::ExprProgram,
    },
    /// `VECTOR` 列の裸の列参照（`COUNT` 限定。[`resolve_aggregate_input`] 参照）。
    /// 列は `ALTER TABLE ADD COLUMN`（TABLE-5）で追加された nullable な `VECTOR`
    /// 列の可能性があり、値が未設定の可視行は NULL として `COUNT` から除外する
    /// （`row.embedding` が空 = 未設定という [`crate::storage::Row`] の既存契約に
    /// 従う。PR #229 codex-review 指摘対応）。
    VectorColumnPresence,
}

/// 集計項目 1 つの引数を SQL テキスト非経由で表す形（TASK-186・NOSQL-4）。
/// [`crate::sql::allowlist::AggregateArg`] の 2 variant（`Star`／`Expr`）を、
/// クレート外から構築できる最小の閉じた形へ単純化したもの。`Column` は
/// 単純な識別子（`id`・実カラム名）のみを表し、複合式（`vec_norm(...)` 等）は
/// 対象外（[`crate::sql::udf_call::BoundExpr`] を公開せずに式を組み立てる
/// 手段が無いため。式が必要な集計は引き続き SQL テキスト経由
/// （[`bind_aggregate`]）でのみ構築できる）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateTarget {
    /// `COUNT(*)` 相当。[`BoundAggregateItem::bind`] は
    /// `func != AggregateFunc::Count` の場合これを構文層の拒否と同じ
    /// `42601`（[`SqlSurfaceError::unsupported`]）で拒否する（SQL 表層の
    /// `Parser::parse_aggregate_item` が `Star` を `COUNT` 専用に絞り込む
    /// 構造的制約を、直接構築経路でも同じ分類で再現する）。
    Star,
    /// 識別子（疑似列 `id`・実カラム名）。
    Column(String),
}

/// 束縛済みの集計項目 1 つ（TASK-166・SQL-13）。
///
/// フィールドは `pub(crate)` のまま公開しない（`BoundScan`・`BoundStatement` と
/// 同じ作法）。クレート外からは [`Self::func`]／[`Self::name`] アクセサー経由で
/// 読み取る。`input`（[`AggregateInput`]）は `ScalarExpr` variant が
/// `pub(crate) mod` の [`crate::sql::expr_program::ExprProgram`] を保持するため
/// 公開せず、アクセサーも設けない（TASK-186・NOSQL-4・NOSQL-5）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct BoundAggregateItem {
    pub(crate) func: crate::sql::allowlist::AggregateFunc,
    pub(crate) input: AggregateInput,
    /// `AS <alias>` の指定値、省略時は関数名小文字
    /// （[`crate::sql::allowlist::AggregateFunc::default_alias`]）。
    pub(crate) name: String,
    /// `COUNT(DISTINCT <expr>)` の修飾子（SQL-25 (c)・TASK-209）。`func !=
    /// Count` では常に `false`（[`resolve_aggregate_input`] が `COUNT` 以外での
    /// `DISTINCT` を構造的に拒否済み）。[`Self::bind`]（クレート外からの直接
    /// 構築経路）は常に `false` 固定（NoSQL 表層での DISTINCT はスコープ外）。
    pub(crate) distinct: bool,
}

impl BoundAggregateItem {
    /// クレート外から集計項目 1 つを直接構築する（TASK-186・NOSQL-4。SQL
    /// テキストの構文解析を経由しない入口）。列名解決は [`resolve_aggregate_input`]
    /// （[`bind_aggregate`] と共有する単一実装）へそのまま委譲するため、
    /// 型不整合（`VECTOR`／`TEXT` 列と関数の組み合わせ・未知列）の判定は SQL
    /// 表層と完全に同一。
    ///
    /// `target == AggregateTarget::Star` かつ `func != AggregateFunc::Count`
    /// は、SQL 表層では構文層（`Parser::parse_aggregate_item`）が構造的に
    /// 拒否する組み合わせであり、意味論層（`resolve_aggregate_input`）には
    /// 到達しない。直接構築経路にはその構文層が存在しないため、ここで
    /// 同じ分類（`42601`）を明示的に再現する（さもなければ
    /// `resolve_aggregate_input` の `AggregateArg::Star` 分岐が無条件で
    /// `AggregateInput::AllVisible` を返し、`SUM(*)` 相当が誤って受理
    /// されてしまう）。
    ///
    /// `name`（出力列名）は常に [`crate::sql::allowlist::AggregateFunc::
    /// default_alias`]（`AS <alias>` 相当の指定は本入口では対象外）。
    pub fn bind(
        func: crate::sql::allowlist::AggregateFunc,
        target: AggregateTarget,
        schema: &TableSchema,
    ) -> Result<Self, SqlSurfaceError> {
        use crate::sql::allowlist::{AggregateArg, AggregateFunc};

        if matches!(target, AggregateTarget::Star) && func != AggregateFunc::Count {
            return Err(SqlSurfaceError::unsupported("* is only allowed with COUNT"));
        }

        let arg = match target {
            AggregateTarget::Star => AggregateArg::Star,
            AggregateTarget::Column(name) => AggregateArg::Expr(Expr::Ident(name)),
        };

        let udfs = crate::sql::udf_call::UdfRegistry::default();
        let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        let input = resolve_aggregate_input(func, &arg, schema, &udfs, &mut node_budget)?;
        let name = func.default_alias().to_string();

        Ok(BoundAggregateItem {
            func,
            input,
            name,
            distinct: false,
        })
    }

    /// 集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`）。
    pub fn func(&self) -> crate::sql::allowlist::AggregateFunc {
        self.func
    }

    /// 出力列名（`AS <alias>` の指定値、省略時は関数名小文字）。
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// SELECT リストの出力列 1 つ（TASK-167・SQL-14。SQL-25 (d) で複数列 `GROUP BY`
/// へ拡張）。`GROUP BY` なしの単一行集計（TASK-166・SQL-13）では `bind_aggregate`
/// が `items` の宣言順で自動生成し、既存挙動を変えない。`GROUP BY` ありの場合は
/// `AggregateSelectItem::GroupKey`／`Aggregate` の並び順をそのまま反映する。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProjectionColumn {
    /// `GROUP BY` 列の値（`sql::group_by::GroupKey` の `key_index` 番目の成分から
    /// 復元。`key_index` は [`BoundGroupBy::column_indices`] の添字）。
    GroupKey { key_index: usize, name: String },
    /// `items[item_index]` の集計結果。
    Aggregate { item_index: usize, name: String },
}

/// HAVING 述語 1 つを束縛した形（TASK-167・SQL-14）。`item_index` は
/// [`BoundAggregate::items`] の添字（HAVING は SELECT リストの集計項目のみを
/// 参照できるため、常に既存の `items` を指す。新規アキュムレータを追加しない）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BoundHaving {
    pub(crate) item_index: usize,
    pub(crate) op: crate::sql::udf_call::BinOp,
    pub(crate) literal: f64,
}

/// `ORDER BY` 対象を束縛した形（TASK-167・SQL-14。SQL-25 (d) で `GroupKey` に
/// キー番号〔[`BoundGroupBy::column_indices`] の添字〕を持たせた）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum OrderTarget {
    GroupKey(usize),
    Aggregate(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BoundOrderBy {
    pub(crate) target: OrderTarget,
    pub(crate) descending: bool,
}

/// 束縛済みの `GROUP BY` 句（TASK-167・SQL-14。SQL-25 (d) で複数列へ拡張）。
/// `column_indices` は宣言順を保持した `schema.columns` の添字列（束縛段で全て
/// `TEXT` 列であることを確認済み）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundGroupBy {
    pub(crate) column_indices: Vec<usize>,
    pub(crate) having: Vec<BoundHaving>,
    pub(crate) order_by: Option<BoundOrderBy>,
    pub(crate) limit: Option<usize>,
    /// `OFFSET` の検証済み値（`0..=core::MAX_SEARCH_K`。Issue #916・SQL-25 (b)・
    /// TASK-209）。ソート済みグループ列に対し `truncate(limit)` の前に適用する
    /// （`sql::group_by`）。`limit` が `None`（`LIMIT` 句なし）のときは構文段
    /// （`allowlist::parse_aggregate_shape`）が `OFFSET` 単独を `42601` へ落とすため
    /// 常に `0`。[`BoundAggregate::new_grouped`]（TASK-186・NOSQL-5）は本 Issue の
    /// 対象外のため `0` 固定（NoSQL 表層の offset 写像は #947・NOSQL-15 の管轄）。
    pub(crate) offset: usize,
}

/// 束縛済みの集計 SELECT 文（TASK-166・SQL-13。TASK-167・SQL-14 で `group_by`・
/// `projection` を追加）。[`crate::sql::aggregate::execute_aggregate`] が直接実行する
/// 入力形。`BoundStatement` と異なり検索固有のフィールド（`ranking`・`limit`・
/// `mode`・`evaluation_order`）を持たない（集計結果の順位付け・取得モードは
/// `sql::group_by`（`GROUP BY` ありの場合のみ）が別途扱うため。
/// [`crate::sql::allowlist::ValidatedAggregate`] のドキュメント参照）。
///
/// フィールドは `pub(crate)` のまま公開しない（`BoundScan`・`BoundStatement` と
/// 同じ作法）。クレート外からはアクセサーメソッド経由で読み取る。`projection`
/// （[`ProjectionColumn`]）・`group_by`（[`BoundGroupBy`]）は非公開のまま維持し
/// （`GroupKey`/`Aggregate` の内部添字・`BoundHaving`/`BoundOrderBy` を経由しない
/// 独立したアクセサーが必要になるため）、代わりに [`Self::has_group_by`] のみを
/// 公開する。SQL テキストを経由しない直接構築は [`Self::new`]（`BoundScan::new`
/// と同じ作法。`GROUP BY` を持たない単一行集計〔TASK-166・SQL-13〕限定。
/// TASK-186・NOSQL-4）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct BoundAggregate {
    pub(crate) table: String,
    /// 集計項目（アキュムレータを持つ項目のみ。`GroupKey` 項目は含まない）。
    pub(crate) items: Vec<BoundAggregateItem>,
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    pub(crate) expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
    /// `expr_filters` をステップ列コンパイルした実行形（Issue #353。
    /// `BoundStatement::expr_filter_programs` と同じ 1 対 1 対応の契約）。
    pub(crate) expr_filter_programs: Vec<crate::sql::expr_program::ExprProgram>,
    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。[`Self::new`]・
    /// [`Self::new_grouped`]（NoSQL 表層の直接構築経路）は常に空にする。
    pub(crate) or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
    pub(crate) rls_predicate_present: bool,
    /// 出力列順（`items` とは独立。`GROUP BY` の有無によらず常に構築する）。
    pub(crate) projection: Vec<ProjectionColumn>,
    /// `GROUP BY` 句（TASK-167・SQL-14）。`None` なら TASK-166・SQL-13 の単一行集計。
    pub(crate) group_by: Option<BoundGroupBy>,
}

impl BoundAggregate {
    /// クレート外から `BoundAggregate` を直接構築する constructor（TASK-186・
    /// NOSQL-4。SQL テキストの構文解析・[`crate::sql::allowlist::validate_sql`]
    /// を経由せずに束縛済み実行計画を組み立てる入口。[`BoundScan::new`] と同じ
    /// 作法）。`GROUP BY` を持たない単一行集計（TASK-166・SQL-13）限定
    /// （`group_by: None` 固定）。`GROUP BY`／`HAVING` を伴う計画
    /// （TASK-167・SQL-14）を直接構築する場合は [`Self::new_grouped`]
    /// （TASK-186・NOSQL-5）を使う。
    ///
    /// `items` が空なら [`SqlSurfaceError::unsupported`]（`42601`。SQL 側で
    /// 集計項目 0 個の SELECT リストは構文エラーになるのと同じ分類）、
    /// [`crate::sql::allowlist::MAX_AGGREGATE_ITEMS`] 超過なら
    /// [`SqlSurfaceError::payload_too_large`]（`54000`）で拒否する
    /// （[`crate::sql::allowlist::check_aggregate_item_count`] と同じ判定を
    /// `Vec` 確保より前に行う）。`rls_predicate_present` は常に `false`
    /// 固定とする（クレート外の呼び出し元は `WHERE` 句の構文を持たないため、
    /// RLS 相当の述語をクライアントが明示的に指定する経路が存在しない。
    /// `EngineCore::execute_bound_aggregate_in_session`（Issue #728）が
    /// `ctx`〔`PolicyContext`〕から RLS を暗黙適用する既存契約はこのフィールド
    /// に依存しないため、`false` 固定でも RLS 境界は保たれる）。
    pub fn new(
        table: String,
        items: Vec<BoundAggregateItem>,
        metadata_filters: Vec<MetadataFilter>,
        expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
    ) -> Result<Self, SqlSurfaceError> {
        if items.is_empty() {
            return Err(SqlSurfaceError::unsupported(
                "aggregate SELECT list must have at least one item",
            ));
        }
        crate::sql::allowlist::check_aggregate_item_count(items.len())?;

        let projection = items
            .iter()
            .enumerate()
            .map(|(item_index, item)| ProjectionColumn::Aggregate {
                item_index,
                name: item.name.clone(),
            })
            .collect();
        let expr_filter_programs = compile_expr_filter_programs(&expr_filters);

        Ok(BoundAggregate {
            table,
            items,
            metadata_filters,
            expr_filters,
            expr_filter_programs,
            or_filters: Vec::new(),
            rls_predicate_present: false,
            projection,
            group_by: None,
        })
    }

    /// クレート外から単一列 `GROUP BY`／`HAVING` 付き実行計画を直接構築する
    /// constructor（TASK-186・NOSQL-5。[`Self::new`] の `GROUP BY` あり版）。
    /// SQL-25 (d) で複数列へ拡張した [`Self::new_grouped_by_columns`] へ
    /// `&[group_by_column]` を渡すだけの委譲になり、挙動・エラー分類は変わらない
    /// （既存呼び出し元の互換性を維持する。破壊的変更にしない）。詳細な検査内容は
    /// [`Self::new_grouped_by_columns`] のドキュメント参照。
    /// [`Self::new`] と同じ理由で常に `false` 固定。
    pub fn new_grouped(
        table: String,
        items: Vec<BoundAggregateItem>,
        metadata_filters: Vec<MetadataFilter>,
        expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
        group_by_column: &str,
        having: Vec<HavingSpec>,
        schema: &TableSchema,
    ) -> Result<Self, SqlSurfaceError> {
        Self::new_grouped_by_columns(
            table,
            items,
            metadata_filters,
            expr_filters,
            &[group_by_column],
            having,
            schema,
        )
    }

    /// クレート外から複数列 `GROUP BY`／`HAVING` 付き実行計画を直接構築する
    /// constructor（SQL-25 (d)。[`Self::new_grouped`] の複数キー版で、単一列
    /// 経路は本関数へ `&[group_by_column]` を渡すだけの委譲になった）。
    /// SQL テキストを一切組み立てず、列名解決（[`resolve_group_by_column`]）・
    /// HAVING 対象の型検査（[`check_having_target_is_numeric`]）を SQL テキスト
    /// 経由の [`bind_group_by_clause`] と共有する。
    ///
    /// `items`（空・[`crate::sql::allowlist::MAX_AGGREGATE_ITEMS`] 超過）・
    /// `having`（件数・非有限リテラル・範囲外 `item_index`・非数値対象）の検査は
    /// [`Self::new_grouped`] と同じ。`group_by_columns` は空スライスなら `42601`
    /// （SQL テキスト側で `GROUP BY` に列 0 個は構文的に書けないのと同じ分類）、
    /// [`crate::sql::allowlist::MAX_GROUP_BY_COLUMNS`] 超過は `54000`
    /// （[`crate::sql::allowlist::check_group_by_column_count`] と同じ判定を
    /// `Vec` 確保より前に行う）、重複する列名は `42601`（SQL テキスト経由の
    /// `Parser::parse_group_by_clause` と同じ分類）、各列は `schema` 上の既存
    /// `TEXT` 列名限定（未知列・`VECTOR` 列・疑似列 `id` はいずれも `22000`）。
    /// `ORDER BY`／`LIMIT` 相当は本入口の対象外（`order_by: None`・`limit: None`
    /// 固定。NoSQL 表層のスキーマにこれらに相当するキーが存在しないため）。
    /// `projection` は `[GroupKey{0..k}] ++ items`（宣言順）の規範形に固定する
    /// （SQL の規範形 `SELECT <col...>, <aggs...> FROM t GROUP BY <col...>` と
    /// 同一の列順・既定エイリアス名）。`rls_predicate_present` は [`Self::new`]
    /// と同じ理由で常に `false` 固定。
    pub fn new_grouped_by_columns(
        table: String,
        items: Vec<BoundAggregateItem>,
        metadata_filters: Vec<MetadataFilter>,
        expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
        group_by_columns: &[&str],
        having: Vec<HavingSpec>,
        schema: &TableSchema,
    ) -> Result<Self, SqlSurfaceError> {
        if items.is_empty() {
            return Err(SqlSurfaceError::unsupported(
                "aggregate SELECT list must have at least one item",
            ));
        }
        crate::sql::allowlist::check_aggregate_item_count(items.len())?;
        crate::sql::allowlist::check_having_predicate_count(having.len())?;

        if group_by_columns.is_empty() {
            return Err(SqlSurfaceError::unsupported(
                "GROUP BY must reference at least one column",
            ));
        }
        crate::sql::allowlist::check_group_by_column_count(group_by_columns.len())?;
        for (i, a) in group_by_columns.iter().enumerate() {
            if group_by_columns[..i].contains(a) {
                return Err(SqlSurfaceError::unsupported(format!(
                    "duplicate GROUP BY column {a:?}"
                )));
            }
        }

        let mut column_indices = Vec::with_capacity(group_by_columns.len());
        for column in group_by_columns {
            column_indices.push(resolve_group_by_column(schema, column)?);
        }

        let mut bound_having = Vec::with_capacity(having.len());
        for spec in having {
            if !spec.literal.is_finite() {
                return Err(SqlSurfaceError::unsupported(
                    "HAVING literal must be finite",
                ));
            }
            let item = items.get(spec.item_index).ok_or_else(|| {
                SqlSurfaceError::invalid_input(format!(
                    "HAVING item_index {} is out of range",
                    spec.item_index
                ))
            })?;
            check_having_target_is_numeric(item, &item.name)?;
            bound_having.push(BoundHaving {
                item_index: spec.item_index,
                op: spec.op.to_bin_op(),
                literal: spec.literal,
            });
        }

        let mut projection = Vec::with_capacity(items.len() + group_by_columns.len());
        for (key_index, column) in group_by_columns.iter().enumerate() {
            projection.push(ProjectionColumn::GroupKey {
                key_index,
                name: column.to_string(),
            });
        }
        for (item_index, item) in items.iter().enumerate() {
            projection.push(ProjectionColumn::Aggregate {
                item_index,
                name: item.name.clone(),
            });
        }
        let expr_filter_programs = compile_expr_filter_programs(&expr_filters);

        Ok(BoundAggregate {
            table,
            items,
            metadata_filters,
            expr_filters,
            expr_filter_programs,
            or_filters: Vec::new(),
            rls_predicate_present: false,
            projection,
            group_by: Some(BoundGroupBy {
                column_indices,
                having: bound_having,
                order_by: None,
                limit: None,
                offset: 0,
            }),
        })
    }

    /// 束縛対象のテーブル名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 集計項目一覧（アキュムレータを持つ項目のみ。`GROUP BY` 列は含まない）。
    pub fn items(&self) -> &[BoundAggregateItem] {
        &self.items
    }

    /// SCALAR 段で適用するメタデータフィルタ一覧（等価・前方一致、TASK-147・EXT-3）。
    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        &self.metadata_filters
    }

    /// `WHERE` の式述語（TASK-79・SQL-9）。UDF インライン展開済み。
    pub fn expr_filters(&self) -> &[crate::sql::udf_call::BoundExpr] {
        &self.expr_filters
    }

    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。
    pub fn or_filters(&self) -> &[crate::sql::where_tree::BoundOrGroup] {
        &self.or_filters
    }

    /// [`BoundStatement::has_where_filters`] と同じ判定（TASK-208・Issue #912）。
    pub fn has_where_filters(&self) -> bool {
        !self.metadata_filters.is_empty()
            || !self.expr_filters.is_empty()
            || !self.or_filters.is_empty()
    }

    /// `WHERE` 句に RLS 相当の述語（テナント境界を表す条件）が含まれるか。
    pub fn rls_predicate_present(&self) -> bool {
        self.rls_predicate_present
    }

    /// `GROUP BY` 句を持つか（`true` なら
    /// [`crate::sql::group_by::execute_grouped_aggregate`]（TASK-167・SQL-14）へ、
    /// `false` なら単一行集計（TASK-166・SQL-13）へ振り分けられる）。
    pub fn has_group_by(&self) -> bool {
        self.group_by.is_some()
    }
}

/// 集計項目 1 つの引数（[`crate::sql::allowlist::AggregateArg`]）を `schema` と
/// 照合し、[`AggregateInput`] へ解決する（TASK-166・SQL-13）。列名解決の優先順位
/// （実カラム＞疑似列 `id`）は [`bind_in_session`] の投影束縛・
/// `sql::udf_call::bind_expr_in` と揃える（Issue #56 レビュー指摘で確立した既存
/// 規約）。
///
/// 型ごとの受理・拒否は以下（対象ビヘイビア: SQL-13。Issue #892 で
/// `INTEGER`/`BIGINT`/`REAL`/`DOUBLE PRECISION`/`NUMERIC` の `SUM`/`AVG`/
/// `MIN`/`MAX`・`DATE`/`TIMESTAMP` の `MIN`/`MAX` を追加受理した）:
/// - `*`（`COUNT` 限定。構文層が既に強制済み）→ [`AggregateInput::AllVisible`]
/// - `id` → `COUNT` は [`AggregateInput::AllVisible`]、それ以外は
///   [`AggregateInput::IdU64`]
/// - `TEXT` 列 → `SUM`/`AVG` は型不整合（`22000`）、それ以外は
///   [`AggregateInput::TextColumn`]
/// - `INTEGER`/`BIGINT`/`REAL`/`DOUBLE PRECISION`/`NUMERIC` 列 → すべての
///   集計関数を受理（[`AggregateInput::IntegerColumn`] 等）
/// - `DATE`/`TIMESTAMP` 列 → `COUNT`・`MIN`/`MAX` を受理、`SUM`/`AVG` は
///   型不整合（`22000`）
/// - `VECTOR` 列（裸の列参照）→ `COUNT` は [`AggregateInput::VectorColumnPresence`]
///   （非 NULL 行のみ数える）、それ以外は型不整合（`22000`）
/// - 上記以外の識別子 → 未知の列（`22000`）
/// - 複合式（`Expr::Call`・`Expr::Binary`・`Expr::Number`）→
///   `sql::udf_call::bind_expr` に委譲し、`Scalar` 型のみ
///   [`AggregateInput::ScalarExpr`] として受理、`Vector`/`Bool` 型は型不整合
///   （`22000`）
fn resolve_aggregate_input(
    func: crate::sql::allowlist::AggregateFunc,
    arg: &crate::sql::allowlist::AggregateArg,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    node_budget: &mut usize,
) -> Result<AggregateInput, SqlSurfaceError> {
    use crate::sql::allowlist::{AggregateArg, AggregateFunc};
    use crate::sql::udf_call::ExprType;

    match arg {
        AggregateArg::Star => Ok(AggregateInput::AllVisible),
        AggregateArg::Expr(Expr::Ident(name)) => {
            if let Some((index, column)) = schema
                .columns
                .iter()
                .enumerate()
                .find(|(_, c)| &c.name == name)
            {
                return match (&column.ty, func) {
                    (ColumnType::Text, AggregateFunc::Sum | AggregateFunc::Avg) => {
                        Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} is TEXT and cannot be used with SUM/AVG"
                        )))
                    }
                    (ColumnType::Text, _) => Ok(AggregateInput::TextColumn(index)),
                    (ColumnType::Vector(_), AggregateFunc::Count) => {
                        Ok(AggregateInput::VectorColumnPresence)
                    }
                    (ColumnType::Vector(_), _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is VECTOR and cannot be used with SUM/AVG/MIN/MAX"
                    ))),
                    // `INTEGER`／`BIGINT` 列は `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`
                    // のすべてで受理する（TABLE-13・TASK-196、Issue #881・
                    // #892）。
                    (ColumnType::Integer, _) => Ok(AggregateInput::IntegerColumn(index)),
                    (ColumnType::BigInt, _) => Ok(AggregateInput::BigIntColumn(index)),
                    // `REAL`／`DOUBLE PRECISION` 列も同様にすべての集計関数を
                    // 受理する（Issue #892）。
                    (ColumnType::Real, _) => Ok(AggregateInput::RealColumn(index)),
                    (ColumnType::Double, _) => Ok(AggregateInput::DoubleColumn(index)),
                    (ColumnType::Boolean, AggregateFunc::Count) => {
                        Ok(AggregateInput::BooleanColumn(index))
                    }
                    (ColumnType::Boolean, _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is BOOLEAN and cannot be used with SUM/AVG/MIN/MAX"
                    ))),
                    // `DATE`／`TIMESTAMP` 列は `COUNT`・`MIN`/`MAX` を受理する
                    // （暦日・時刻の全順序比較。Issue #892）。`SUM`/`AVG` は
                    // 合計・平均に意味論がないため引き続き拒否する。
                    (
                        ColumnType::Date,
                        AggregateFunc::Count | AggregateFunc::Min | AggregateFunc::Max,
                    ) => Ok(AggregateInput::DateColumn(index)),
                    (ColumnType::Date, _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is DATE and cannot be used with SUM/AVG"
                    ))),
                    (
                        ColumnType::Timestamp,
                        AggregateFunc::Count | AggregateFunc::Min | AggregateFunc::Max,
                    ) => Ok(AggregateInput::TimestampColumn(index)),
                    (ColumnType::Timestamp, _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is TIMESTAMP and cannot be used with SUM/AVG"
                    ))),
                    (ColumnType::Array(_), AggregateFunc::Count) => {
                        Ok(AggregateInput::ArrayColumn(index))
                    }
                    (ColumnType::Array(_), _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is ARRAY and cannot be used with SUM/AVG/MIN/MAX"
                    ))),
                    (ColumnType::Bytea, AggregateFunc::Count) => {
                        Ok(AggregateInput::ByteaColumn(index))
                    }
                    (ColumnType::Bytea, _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is BYTEA and cannot be used with SUM/AVG/MIN/MAX"
                    ))),
                    (ColumnType::Json | ColumnType::Jsonb, AggregateFunc::Count) => {
                        Ok(AggregateInput::JsonColumn(index))
                    }
                    (ColumnType::Json | ColumnType::Jsonb, _) => {
                        Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} is JSON and cannot be used with SUM/AVG/MIN/MAX"
                        )))
                    }
                    (ColumnType::Enum(_), AggregateFunc::Count) => {
                        Ok(AggregateInput::EnumColumn(index))
                    }
                    (ColumnType::Enum(_), _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is ENUM and cannot be used with SUM/AVG/MIN/MAX"
                    ))),
                    // `NUMERIC(p, s)` 列は `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` の
                    // すべてで受理する（Issue #892）。列の `precision`/`scale`
                    // をここで複製し保持する（`Accumulator::new` が桁あふれ
                    // 判定・`AVG` の結果 scale 決定に使う）。
                    (ColumnType::Numeric { precision, scale }, _) => {
                        Ok(AggregateInput::NumericColumn {
                            index,
                            precision: *precision,
                            scale: *scale,
                        })
                    }
                    (ColumnType::Uuid, AggregateFunc::Count) => {
                        Ok(AggregateInput::UuidColumn(index))
                    }
                    (ColumnType::Uuid, _) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is UUID and cannot be used with SUM/AVG/MIN/MAX"
                    ))),
                };
            }
            if name == "id" {
                return match func {
                    AggregateFunc::Count => Ok(AggregateInput::AllVisible),
                    _ => Ok(AggregateInput::IdU64),
                };
            }
            Err(SqlSurfaceError::invalid_input(format!(
                "unknown column: {name}"
            )))
        }
        AggregateArg::Expr(expr) => {
            let (bound, ty) = crate::sql::udf_call::bind_expr(expr, schema, udfs, node_budget)?;
            match ty {
                ExprType::Scalar => {
                    let program = crate::sql::expr_program::ExprProgram::compile(&bound);
                    Ok(AggregateInput::ScalarExpr {
                        source: bound,
                        program,
                    })
                }
                ExprType::Vector | ExprType::Bool => Err(SqlSurfaceError::invalid_input(
                    "aggregate argument must evaluate to a scalar",
                )),
            }
        }
    }
}

/// `COUNT(DISTINCT <expr>)`（SQL-25 (c)・TASK-209）の入力解決。関数は常に
/// `COUNT` のため `resolve_aggregate_input` と異なり `func` を引数に取らない。
/// `resolve_aggregate_input` との差分:
///
/// - `id` は（`COUNT(id)` が `AllVisible` へ縮退するのと異なり）実際の値が
///   異なり数の判定に必要なため常に [`AggregateInput::IdU64`] に束縛する。
/// - `VECTOR` 列は正準等価の定義を持たないため `22000`
///   （[`SqlSurfaceError::invalid_input`]）で拒否する（非 DISTINCT の
///   `COUNT(<VECTOR 列>)` は `VectorColumnPresence` へ縮退するが、DISTINCT は
///   実際の embedding 値同士の比較を要求するため対象外）。
/// - `ARRAY`／`JSON`／`JSONB` 列も同じ理由（正準等価未定義）で `22000`。
/// - それ以外の列型（`TEXT`／`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION`／
///   `BOOLEAN`／`DATE`／`TIMESTAMP`／`NUMERIC`／`BYTEA`／`UUID`／`ENUM`）は
///   `resolve_aggregate_input` と同じ `AggregateInput::*Column` へ束縛する
///   （`sql::aggregate::Accumulator::observe_distinct` が `scanned` から実値を
///   直接読み、`Accumulator::observe` の「存在のみを数える」経路とは独立に
///   異なり値を判定する）。
/// - `<Scalar 式>`（組み込み関数・UDF・四則演算）は受理する（結果の `f64` を
///   [`crate::sql::distinct::canon_f64`] で正準化してキーにする）。
fn resolve_count_distinct_input(
    arg: &crate::sql::allowlist::AggregateArg,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    node_budget: &mut usize,
) -> Result<AggregateInput, SqlSurfaceError> {
    use crate::sql::allowlist::AggregateArg;
    use crate::sql::udf_call::ExprType;

    match arg {
        // 構文層（`Parser::parse_aggregate_item`）が `COUNT(DISTINCT *)` を
        // 既に `42601` で拒否しているため、ここへは到達しない想定だが、直接
        // 構築経路が生まれた場合に備えて fail-closed に扱う。
        AggregateArg::Star => Err(SqlSurfaceError::unsupported(
            "COUNT(DISTINCT *) is not supported",
        )),
        AggregateArg::Expr(Expr::Ident(name)) => {
            if let Some((index, column)) = schema
                .columns
                .iter()
                .enumerate()
                .find(|(_, c)| &c.name == name)
            {
                return match &column.ty {
                    ColumnType::Text => Ok(AggregateInput::TextColumn(index)),
                    ColumnType::Vector(_) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is VECTOR and cannot be used with COUNT(DISTINCT ...)"
                    ))),
                    ColumnType::Integer => Ok(AggregateInput::IntegerColumn(index)),
                    ColumnType::BigInt => Ok(AggregateInput::BigIntColumn(index)),
                    ColumnType::Real => Ok(AggregateInput::RealColumn(index)),
                    ColumnType::Double => Ok(AggregateInput::DoubleColumn(index)),
                    ColumnType::Boolean => Ok(AggregateInput::BooleanColumn(index)),
                    ColumnType::Date => Ok(AggregateInput::DateColumn(index)),
                    ColumnType::Timestamp => Ok(AggregateInput::TimestampColumn(index)),
                    ColumnType::Array(_) => Err(SqlSurfaceError::invalid_input(format!(
                        "column {name:?} is ARRAY and cannot be used with COUNT(DISTINCT ...)"
                    ))),
                    ColumnType::Bytea => Ok(AggregateInput::ByteaColumn(index)),
                    ColumnType::Json | ColumnType::Jsonb => {
                        Err(SqlSurfaceError::invalid_input(format!(
                            "column {name:?} is JSON and cannot be used with COUNT(DISTINCT ...)"
                        )))
                    }
                    ColumnType::Enum(_) => Ok(AggregateInput::EnumColumn(index)),
                    ColumnType::Numeric { precision, scale } => Ok(AggregateInput::NumericColumn {
                        index,
                        precision: *precision,
                        scale: *scale,
                    }),
                    ColumnType::Uuid => Ok(AggregateInput::UuidColumn(index)),
                };
            }
            if name == "id" {
                return Ok(AggregateInput::IdU64);
            }
            Err(SqlSurfaceError::invalid_input(format!(
                "unknown column: {name}"
            )))
        }
        AggregateArg::Expr(expr) => {
            let (bound, ty) = crate::sql::udf_call::bind_expr(expr, schema, udfs, node_budget)?;
            match ty {
                ExprType::Scalar => {
                    let program = crate::sql::expr_program::ExprProgram::compile(&bound);
                    Ok(AggregateInput::ScalarExpr {
                        source: bound,
                        program,
                    })
                }
                ExprType::Vector | ExprType::Bool => Err(SqlSurfaceError::invalid_input(
                    "aggregate argument must evaluate to a scalar",
                )),
            }
        }
    }
}

/// [`crate::sql::allowlist::ValidatedAggregate`] を `schema`・UDF レジストリ `udfs`
/// と照合して [`BoundAggregate`] へ束縛する（TASK-166・SQL-13 の公開 API。
/// TASK-167・SQL-14 で `GROUP BY`/`HAVING`/`ORDER BY`/`LIMIT` の束縛を追加。
/// TASK-186・NOSQL-4・NOSQL-5 で engine クレート外へ公開 API として昇格）。
/// `WHERE` 句の意味論は [`bind_where_predicates`] を検索 SELECT
/// （[`bind_in_session`]）と共有する。式ノード予算（[`crate::sql::udf_call::MAX_EXPR_NODES`]）
/// は集計項目＋`WHERE` の全式項目で 1 文につき共有する（`bind_in_session` と同じ
/// 歯止め）。`stmt`（[`crate::sql::allowlist::ValidatedAggregate`]）に `pub`
/// constructor が無いため、クレート外からの到達は現状
/// [`crate::sql::allowlist::validate_sql`]（SQL テキスト経由）のみ。
///
/// 公開 API は常に全値検証を行う（ENUM ラベルの語彙照合を含む。PR #1012
/// codex-review P1 指摘対応: 検証省略フラグは公開シグネチャへ露出しない）。
/// Prepared Describe 専用の縮退経路は crate 内限定の
/// [`bind_aggregate_with_dummy_flags`] が担う。
pub fn bind_aggregate(
    stmt: &crate::sql::allowlist::ValidatedAggregate,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundAggregate, SqlSurfaceError> {
    bind_aggregate_with_dummy_flags(stmt, schema, udfs, &[])
}

/// [`bind_aggregate`] の本体（crate 内限定。Issue #935・WIRE-12・TASK-217）。
/// `dummy_equality_flags` は [`bind_where_predicates`] へそのまま渡す（同関数の
/// ドキュメント参照）。空スライスは全値検証で [`bind_aggregate`] と同一。
/// 非空のフラグを渡すのは `core.rs::EngineCore::describe_prepared_in_session`
/// 経由の Prepared Describe（Bind 前・ダミー値束縛済み）だけであり、フラグは
/// `core.rs::PreparedSql` が Parse 時点の元トークン列から計算した値に限る
/// （クレート外から任意のフラグを渡して ENUM ラベル検証を省略させる経路を
/// 作らないため `pub(crate)` に留める。PR #1012 codex-review P1 指摘対応）。
pub(crate) fn bind_aggregate_with_dummy_flags(
    stmt: &crate::sql::allowlist::ValidatedAggregate,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    dummy_equality_flags: &[bool],
) -> Result<BoundAggregate, SqlSurfaceError> {
    use crate::sql::allowlist::AggregateSelectItem;

    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;

    // GROUP BY 列名一覧（`SELECT` リストの `GroupKey` 項目の照合・`ORDER BY`/
    // `LIMIT` 束縛より前に確定させる。`GROUP BY` なしなら空スライス）。
    let group_by_columns: &[String] = stmt.group_by().map(|g| g.columns.as_slice()).unwrap_or(&[]);

    let mut items = Vec::new();
    let mut projection = Vec::with_capacity(stmt.items().len());
    // `GROUP BY` 列に SELECT リストで `AS` エイリアスが付いた場合の実効名一覧
    // （`ORDER BY`/`HAVING` がこれらのエイリアスを参照できるよう
    // `bind_group_by_clause` へ渡す。PR #230 Bugbot 指摘対応: `resolve_target` が
    // 生の列名にしか一致しないと、SELECT リストの実効名であるはずのエイリアスが
    // `ORDER BY` から unknown 扱いされる。PR #230 codex-review P1 指摘対応:
    // `SELECT lang AS a, lang AS b, ...` のように同一 `GROUP BY` 列を複数回
    // 別名で射影できるため、単一 `Option<String>` では後勝ちで先のエイリアスが
    // 失われる。全エイリアスを保持する。SQL-25 (d) で複数列化: どのキー番号
    // （`group_by_columns` の添字）に付けられたエイリアスかを保持するため
    // `Vec<(usize, String)>` にする）。
    let mut group_key_aliases: Vec<(usize, String)> = Vec::new();
    for item in stmt.items() {
        match item {
            AggregateSelectItem::Aggregate(item) => {
                // SQL-25 (c)・TASK-209: `DISTINCT` 修飾の有無で入力解決を分ける
                // （`resolve_count_distinct_input` は `id`/`VECTOR`/`ARRAY`/
                // `JSON` の扱いが非 DISTINCT の `resolve_aggregate_input` と
                // 異なる。`item.distinct` は構文層で `COUNT` 限定に絞り込み済み）。
                let input = if item.distinct {
                    resolve_count_distinct_input(&item.arg, schema, udfs, &mut node_budget)?
                } else {
                    resolve_aggregate_input(item.func, &item.arg, schema, udfs, &mut node_budget)?
                };
                let name = item
                    .alias
                    .clone()
                    .unwrap_or_else(|| item.func.default_alias().to_string());
                let item_index = items.len();
                items.push(BoundAggregateItem {
                    func: item.func,
                    input,
                    name: name.clone(),
                    distinct: item.distinct,
                });
                projection.push(ProjectionColumn::Aggregate { item_index, name });
            }
            // `allowlist::parse_aggregate_shape` が `GROUP BY` 句自体の有無・
            // 列名一致（いずれかの `GROUP BY` 列と同名）を構造検証済みのため、
            // ここへ到達する `GroupKey` 項目は必ず `group_by_columns` のいずれか
            // 1 つと同名（構造上の前提）。`key_index` はその位置。
            AggregateSelectItem::GroupKey { column, alias } => {
                let key_index = match group_by_columns.iter().position(|c| c == column) {
                    Some(index) => index,
                    None => {
                        // 構文層が既に列名一致を検証済み（上記コメント参照）。
                        // 到達しないはずの分岐だが、行経路の `unwrap`/`expect`
                        // 相当を避けるため internal エラーへ落とし panic も
                        // fail-open な既定値継続もさせず、`Err` を返す
                        // （`.claude/rules/coding-rust.md`・`security.md`
                        // 「fail-open にする変更は P0」）。
                        return Err(crate::sql::aggregate::accumulator_bug(
                            "GroupKey column must match a GROUP BY column at this point",
                        ));
                    }
                };
                let name = alias.clone().unwrap_or_else(|| column.clone());
                if let Some(alias) = alias.clone() {
                    group_key_aliases.push((key_index, alias));
                }
                projection.push(ProjectionColumn::GroupKey { key_index, name });
            }
        }
    }

    let (metadata_filters, expr_filters, rls_predicate_present, or_filters) =
        bind_where_predicates(
            stmt.where_predicates(),
            schema,
            udfs,
            &mut node_budget,
            dummy_equality_flags,
        )?;

    let group_by = match stmt.group_by() {
        None => None,
        Some(clause) => Some(bind_group_by_clause(
            clause,
            schema,
            &items,
            &group_key_aliases,
        )?),
    };

    // Issue #353: `BoundStatement` と同じく `expr_filters` を束縛時に 1 回だけ
    // ステップ列コンパイルする。
    let expr_filter_programs = expr_filters
        .iter()
        .map(crate::sql::expr_program::ExprProgram::compile)
        .collect();

    Ok(BoundAggregate {
        table: stmt.table_name().to_string(),
        items,
        metadata_filters,
        expr_filters,
        expr_filter_programs,
        or_filters,
        rls_predicate_present,
        projection,
        group_by,
    })
}

/// 束縛済みの広域取得（ソートなしのフィルタ取得）`SELECT` 文（Issue #454）。
/// [`crate::sql::scan::execute_scan`] が直接実行する入力形。`BoundStatement` と
/// 異なりランキング段固有のフィールド（`ranking`・`mode`・`evaluation_order`）を
/// 持たない（[`crate::sql::allowlist::ValidatedScan`] のドキュメント参照）。
///
/// フィールドは `pub(crate)` のまま公開しない（`BoundStatement` と同じ作法。
/// PR #188 レビュー指摘対応の方針を踏襲）。クレート外からはアクセサーメソッド
/// 経由で読み取り、[`Self::new`] 経由で構築する（TASK-186・NOSQL-3。SQL テキストを
/// 経由しない直接束縛の入口）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct BoundScan {
    pub(crate) table: String,
    pub(crate) projection: Vec<ProjectedColumn>,
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    pub(crate) expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
    /// `expr_filters` をステップ列コンパイルした実行形（Issue #353。
    /// `BoundStatement::expr_filter_programs` と同じ 1 対 1 対応の契約）。
    /// `sql::expr_program` が `pub(crate) mod` のためクレート外に型を出せず、
    /// アクセサーは設けない（`BoundStatement::expr_filter_programs` と同じ判断）。
    pub(crate) expr_filter_programs: Vec<crate::sql::expr_program::ExprProgram>,
    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。[`Self::new`]
    /// （NoSQL 表層の直接構築経路）は常に空にする。
    pub(crate) or_filters: Vec<crate::sql::where_tree::BoundOrGroup>,
    /// `LIMIT` の検証済み値（`1..=core::MAX_SEARCH_K`。[`validate_search_limit`]）。
    pub(crate) limit: usize,
    /// `OFFSET` の検証済み値（`0..=core::MAX_SEARCH_K`。[`validate_search_offset`]。
    /// Issue #916・SQL-25 (b)・TASK-209）。既定は 0（no-op）で、[`Self::new`] 経由の
    /// 直接構築（TASK-186・NOSQL-3）や既存呼び出し元との後方互換を保つ。
    pub(crate) offset: usize,
}

impl BoundScan {
    /// クレート外から `BoundScan` を直接構築する constructor（TASK-186・NOSQL-3。
    /// SQL テキストの構文解析・[`crate::sql::allowlist::validate_sql`] を経由せずに
    /// 束縛済み実行計画を組み立てる入口）。`expr_filters` のステップ列コンパイル
    /// （[`compile_expr_filter_programs`]）は内部で行う。
    ///
    /// **`limit` はここでは検証しない**（[`validate_search_limit`] は
    /// `pub`（TASK-186・NOSQL-3〔Issue #766〕で昇格）だが、呼び出しは呼び出し元の
    /// 任意判断に委ねる。`new` 自身が検証を強制しない契約は不変。SQL テキスト
    /// 経由の [`bind_scan`] は引き続き必ず検証する。`BoundStatement::new` と
    /// 同じ設計判断）。ただし [`crate::sql::scan::execute_scan`]
    /// は `bound.limit` の値によらず結果セットの累計バイト予算
    /// （`MAX_SCAN_RESULT_BYTES`）で走査を打ち切るため、未検証の巨大な `limit` を
    /// 渡しても無制限なメモリ確保には至らない（fail-closed。OWASP「不安全な設計」
    /// 観点）。
    pub fn new(
        table: String,
        projection: Vec<ProjectedColumn>,
        metadata_filters: Vec<MetadataFilter>,
        expr_filters: Vec<crate::sql::udf_call::BoundExpr>,
        limit: usize,
    ) -> Self {
        let expr_filter_programs = compile_expr_filter_programs(&expr_filters);
        Self {
            table,
            projection,
            metadata_filters,
            expr_filters,
            expr_filter_programs,
            or_filters: Vec::new(),
            limit,
            offset: 0,
        }
    }

    /// `offset` を設定した [`Self`] を返す（Issue #916・SQL-25 (b)・TASK-209。
    /// TASK-186・NOSQL-3 の直接構築経路〔`Self::new`〕から `OFFSET` 付き広域取得を
    /// 組み立てるための builder）。**ここでは検証しない**契約は [`Self::new`] の
    /// `limit` と同じ（[`validate_search_offset`] の呼び出しは呼び出し元の任意
    /// 判断に委ねる）。ただし [`crate::sql::scan::execute_scan`] はスキップ済み行を
    /// 投影・確保しないため（結果セットの累計バイト予算・早期終了で有界）、未検証の
    /// 巨大な `offset` を渡しても無制限なメモリ確保には至らない。
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// `OFFSET` 句の値（既定 0）。
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 束縛対象のテーブル名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 投影対象の列一覧（`Row::id` 疑似列を含みうる）。
    pub fn projection(&self) -> &[ProjectedColumn] {
        &self.projection
    }

    /// SCALAR 段で適用するメタデータフィルタ一覧（等価・前方一致、TASK-147・EXT-3）。
    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        &self.metadata_filters
    }

    /// `WHERE` の式述語（TASK-79・SQL-9）。UDF インライン展開済み。
    pub fn expr_filters(&self) -> &[crate::sql::udf_call::BoundExpr] {
        &self.expr_filters
    }

    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。
    pub fn or_filters(&self) -> &[crate::sql::where_tree::BoundOrGroup] {
        &self.or_filters
    }

    /// [`BoundStatement::has_where_filters`] と同じ判定（TASK-208・Issue #912）。
    pub fn has_where_filters(&self) -> bool {
        !self.metadata_filters.is_empty()
            || !self.expr_filters.is_empty()
            || !self.or_filters.is_empty()
    }

    /// `LIMIT` 句の値。
    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// `expr_filters` を束縛時に 1 回だけステップ列コンパイルする（Issue #353）。
/// [`bind_scan`]・[`BoundScan::new`] の双方が共有する（行ループでの再帰評価を
/// なくす契約は SQL テキスト経由・直接構築経由のいずれでも同一）。
fn compile_expr_filter_programs(
    expr_filters: &[crate::sql::udf_call::BoundExpr],
) -> Vec<crate::sql::expr_program::ExprProgram> {
    expr_filters
        .iter()
        .map(crate::sql::expr_program::ExprProgram::compile)
        .collect()
}

/// [`crate::sql::allowlist::ValidatedScan`] を `schema`・UDF レジストリ `udfs` と
/// 照合して [`BoundScan`] へ束縛する（Issue #454・TASK-186・NOSQL-3 の公開 API）。
/// 投影・`WHERE` の意味論は検索 SELECT（[`bind_in_session`]）・集計 SELECT
/// （[`bind_aggregate`]）と共有する（[`bind_projection`]・[`bind_where_predicates`]）。
/// ランキング段（`ORDER BY`・`USING PLAN`）・取得モード（`USING MODE`）は関与しない
/// （[`crate::sql::allowlist::ValidatedScan`] が構造上持たないため）。
///
/// 公開 API は常に全値検証を行う（[`bind_aggregate`] と同じ方針。Prepared
/// Describe 専用の縮退経路は crate 内限定の [`bind_scan_with_dummy_flags`]）。
pub fn bind_scan(
    stmt: &crate::sql::allowlist::ValidatedScan,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
) -> Result<BoundScan, SqlSurfaceError> {
    bind_scan_with_dummy_flags(stmt, schema, udfs, &[])
}

/// [`bind_scan`] の本体（crate 内限定。Issue #935・WIRE-12・TASK-217）。
/// `dummy_equality_flags` の契約・可視性の理由は
/// [`bind_aggregate_with_dummy_flags`] と同じ。
pub(crate) fn bind_scan_with_dummy_flags(
    stmt: &crate::sql::allowlist::ValidatedScan,
    schema: &TableSchema,
    udfs: &crate::sql::udf_call::UdfRegistry,
    dummy_equality_flags: &[bool],
) -> Result<BoundScan, SqlSurfaceError> {
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;

    let projection = bind_projection(stmt.projection(), schema, udfs, &mut node_budget)?;

    let (metadata_filters, expr_filters, _rls_predicate_present, or_filters) =
        bind_where_predicates(
            stmt.where_predicates(),
            schema,
            udfs,
            &mut node_budget,
            dummy_equality_flags,
        )?;

    let limit = validate_search_limit(stmt.limit())?;
    // Issue #916・SQL-25 (b)・TASK-209: `OFFSET` は `LIMIT` と同じ束縛段で検証する
    // （構文段の許可リストは値の上限を持たない生値のまま通すため）。
    let offset = validate_search_offset(stmt.offset())?;

    // Issue #353 と同じく、`expr_filters` を束縛時に 1 回だけステップ列コンパイル
    // する（行ループでの再帰評価をなくす）。
    let expr_filter_programs = compile_expr_filter_programs(&expr_filters);

    Ok(BoundScan {
        table: stmt.table_name().to_string(),
        projection,
        metadata_filters,
        expr_filters,
        expr_filter_programs,
        or_filters,
        limit,
        offset,
    })
}

/// `GROUP BY` 列名を `schema` と照合し `TEXT` 列の添字へ解決する
/// （TASK-167・SQL-14。`VECTOR`・疑似列 `id`・未知列はいずれも型不整合
/// `22000`）。SQL テキスト経由の [`bind_group_by_clause`] と直接構築経由の
/// [`BoundAggregate::new_grouped`]（TASK-186・NOSQL-5）が共有する単一実装
/// （`id` によるグルーピングは対象外＝将来拡張候補）。
fn resolve_group_by_column(schema: &TableSchema, column: &str) -> Result<usize, SqlSurfaceError> {
    schema
        .columns
        .iter()
        .position(|c| c.name == column)
        .filter(|&idx| {
            matches!(
                schema.columns.get(idx).map(|c| c.ty.clone()),
                Some(ColumnType::Text)
            )
        })
        .ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!(
                "GROUP BY column {column:?} must reference an existing TEXT column"
            ))
        })
}

/// `HAVING` が数値比較できる集計結果を指しているかを検証する（TASK-167・
/// SQL-14）。`COUNT(<TEXT 列>)` は結果が常に整数のため許可し、`MIN`/
/// `MAX(<TEXT 列>)`（結果が `Cell::Text`）のみ型不整合 `22000` として拒否する
/// （`AggregateFunc::Count` を除く `AggregateInput::TextColumn` 入力）。SQL
/// テキスト経由の [`bind_group_by_clause`] と直接構築経由の
/// [`BoundAggregate::new_grouped`]（TASK-186・NOSQL-5）が共有する単一実装。
/// `target_name` はエラー文言用（SQL 経由は `HAVING` 述語の識別子、直接構築
/// 経由は集計項目の実効名）。
///
/// Issue #892（D8）: `HAVING` は `f64` リテラルとの厳密な数値比較
/// （[`crate::sql::group_by::having_matches`]）しか行わないため、`NUMERIC`
/// 型の集計結果（`SUM`/`AVG`/`MIN`/`MAX(<NUMERIC 列>)`。`Cell::Numeric`）と
/// `DATE`/`TIMESTAMP` の `MIN`/`MAX`（`Cell::Date`/`Cell::Timestamp`）は
/// 黙って `false` へ縮退させず、`TEXT` と同じく型不整合 `22000` で拒否する
/// （`COUNT` はいずれの列型でも結果が `Cell::Integer` になるため対象外）。
fn check_having_target_is_numeric(
    item: &BoundAggregateItem,
    target_name: &str,
) -> Result<(), SqlSurfaceError> {
    use crate::sql::allowlist::AggregateFunc;

    let is_non_count = !matches!(item.func, AggregateFunc::Count);
    let is_unsupported = is_non_count
        && matches!(
            item.input,
            AggregateInput::TextColumn(_)
                | AggregateInput::NumericColumn { .. }
                | AggregateInput::DateColumn(_)
                | AggregateInput::TimestampColumn(_)
        );
    if is_unsupported {
        return Err(SqlSurfaceError::invalid_input(format!(
            "HAVING target {target_name:?} is not a numerically comparable aggregate result"
        )));
    }
    Ok(())
}

/// [`crate::sql::allowlist::GroupByClause`] を `schema`・束縛済み `items`（アキュムレータ
/// 一覧）と照合して [`BoundGroupBy`] へ束縛する（TASK-167・SQL-14。SQL-25 (d) で
/// 複数キーへ拡張）。`HAVING`/`ORDER BY` の対象名は SELECT リストの集計項目の
/// 実効名（`item.name`）、いずれかの `GROUP BY` 列名そのもの、または SELECT
/// リストでそのキーに付けた `group_key_aliases`（キー番号ごとの全エイリアス）
/// のいずれかに解決する（これらのエイリアスは SELECT リストの実効名であり
/// `ORDER BY` から参照できて然るべきため。PR #230 Bugbot 指摘対応。同一
/// `GROUP BY` 列を複数回別名で射影できるため複数保持する。PR #230
/// codex-review P1 指摘対応）。複数キーに一致する識別子（例: 2 つのキーへ同じ
/// 別名を付けた場合）・キーと集計項目の双方に一致する識別子はいずれも曖昧
/// として `22000` で拒否する（§計画 3.2）。
fn bind_group_by_clause(
    clause: &crate::sql::allowlist::GroupByClause,
    schema: &TableSchema,
    items: &[BoundAggregateItem],
    group_key_aliases: &[(usize, String)],
) -> Result<BoundGroupBy, SqlSurfaceError> {
    // GROUP BY 列は TEXT 列のみ許可する（VECTOR・疑似列 `id`・未知列はいずれも
    // 型不整合として拒否。§計画 3.2。`id` によるグルーピングは本タスクの対象外
    // ＝将来拡張候補）。SQL テキスト経由・直接構築経由（[`BoundAggregate::
    // new_grouped_by_columns`]・TASK-186・NOSQL-5・SQL-25 (d)）が
    // [`resolve_group_by_column`] を共有する。
    let mut column_indices = Vec::with_capacity(clause.columns.len());
    for column in &clause.columns {
        column_indices.push(resolve_group_by_column(schema, column)?);
    }

    // HAVING/ORDER BY の対象名解決: いずれかの `GROUP BY` 列名そのもの、その
    // キーの SELECT リストでの実効名（`group_key_aliases` のいずれか）、または
    // `items` のいずれか 1 つの実効名に一意に一致する識別子のみを受理する
    // （曖昧・非存在は `22000`）。
    let resolve_target = |name: &str| -> Result<OrderTarget, SqlSurfaceError> {
        let mut key_matches: Vec<usize> = clause
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.as_str() == name)
            .map(|(idx, _)| idx)
            .collect();
        for (idx, alias) in group_key_aliases {
            if alias == name && !key_matches.contains(idx) {
                key_matches.push(*idx);
            }
        }
        let item_matches: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.name == name)
            .map(|(idx, _)| idx)
            .collect();
        match (key_matches.as_slice(), item_matches.as_slice()) {
            ([key_idx], []) => Ok(OrderTarget::GroupKey(*key_idx)),
            ([], [idx]) => Ok(OrderTarget::Aggregate(*idx)),
            ([], []) => Err(SqlSurfaceError::invalid_input(format!(
                "unknown GROUP BY reference: {name}"
            ))),
            _ => Err(SqlSurfaceError::invalid_input(format!(
                "ambiguous GROUP BY reference: {name}"
            ))),
        }
    };

    let mut having = Vec::with_capacity(clause.having.len());
    for pred in &clause.having {
        let target = resolve_target(&pred.item_name)?;
        let item_index = match target {
            OrderTarget::Aggregate(idx) => idx,
            OrderTarget::GroupKey(_) => {
                // GROUP BY 列（TEXT）は数値比較の対象にならない（HAVING 右辺は
                // 常に数値リテラル）。列名一致でも `GroupKey` を指した場合は
                // 型不整合として拒否する。
                return Err(SqlSurfaceError::invalid_input(format!(
                    "HAVING cannot compare the GROUP BY key column {:?} to a numeric literal",
                    pred.item_name
                )));
            }
        };
        // HAVING が数値比較できる集計結果のみを許可する（[`resolve_group_by_column`]
        // と同じく直接構築経由と共有する [`check_having_target_is_numeric`]）。
        let bound_item = items
            .get(item_index)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "HAVING item_index resolved out of bounds".to_string(),
            })?;
        check_having_target_is_numeric(bound_item, &pred.item_name)?;
        having.push(BoundHaving {
            item_index,
            op: pred.op,
            literal: pred.literal,
        });
    }

    let order_by = match &clause.order_by {
        None => None,
        Some(ob) => Some(BoundOrderBy {
            target: resolve_target(&ob.target)?,
            descending: ob.descending,
        }),
    };

    let limit = match clause.limit {
        None => None,
        Some(raw) => {
            let limit = usize::try_from(raw).map_err(|_| {
                SqlSurfaceError::invalid_input(format!("malformed LIMIT value: {raw}"))
            })?;
            if limit == 0 || limit > crate::sql::group_by::MAX_GROUPS {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "LIMIT {limit} out of range (must be 1..={})",
                    crate::sql::group_by::MAX_GROUPS
                )));
            }
            Some(limit)
        }
    };

    // Issue #916・SQL-25 (b)・TASK-209: `OFFSET` は `LIMIT` を伴う場合のみ構文段が
    // 受理する（`allowlist::parse_aggregate_shape`）ため、`clause.offset` は
    // `limit.is_none()` のとき常に `0`。`MAX_SEARCH_K` を上限に用いる理由は
    // `validate_search_offset` のドキュメント参照（`MAX_GROUPS`〔グループ数上限〕
    // とは別軸の「可視かつ WHERE 一致の行数」に対する上限）。
    let offset = validate_search_offset(clause.offset)?;

    Ok(BoundGroupBy {
        column_indices,
        having,
        order_by,
        limit,
        offset,
    })
}

/// `HAVING` 述語の比較演算子（TASK-186・NOSQL-5）。SQL テキスト経由の
/// [`crate::sql::udf_call::BinOp`] は算術 variant（`Add`/`Sub`/`Mul`/`Div`）も
/// 含む式全体の演算子語彙であり、`HAVING` の右辺が常に数値リテラルである
/// 直接構築経路にはクレート外から安全に構築できる比較専用の閉じた語彙を
/// 別途用意する（[`Self::to_bin_op`] で `BoundHaving::op` の型へ変換する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HavingOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl HavingOp {
    fn to_bin_op(self) -> crate::sql::udf_call::BinOp {
        use crate::sql::udf_call::BinOp;
        match self {
            HavingOp::Eq => BinOp::Eq,
            HavingOp::Lt => BinOp::Lt,
            HavingOp::Le => BinOp::Le,
            HavingOp::Gt => BinOp::Gt,
            HavingOp::Ge => BinOp::Ge,
        }
    }
}

/// `HAVING` 述語 1 つをクレート外から直接構築する入力形（TASK-186・NOSQL-5。
/// [`AggregateTarget`] と同じ作法）。`item_index` は
/// [`BoundAggregate::new_grouped`] に渡す `items`（宣言順）の添字であり、
/// SELECT リストの集計項目のみを参照できる契約は SQL テキスト経由
/// （[`bind_group_by_clause`]）と同一（範囲外は `22000`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HavingSpec {
    pub item_index: usize,
    pub op: HavingOp,
    pub literal: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::sql::allowlist::validate_statement;
    use std::collections::HashSet;

    struct FakeCatalog {
        tables: HashSet<&'static str>,
    }
    impl crate::sql::allowlist::TableLookup for FakeCatalog {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(self.tables.contains(name))
        }
    }

    fn docs_schema() -> TableSchema {
        TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        )
    }

    fn bind_sql(sql: &str) -> Result<BoundStatement, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = validate_statement(sql, &lookup).expect("must pass allowlist");
        bind(&stmt, &docs_schema())
    }

    // --- parse_vector_literal --------------------------------------------------

    #[test]
    fn parse_vector_literal_accepts_matching_dim() {
        let v = parse_vector_literal("[1.0,2.0,3.0]", 3).expect("valid literal");
        assert_eq!(v, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn parse_vector_literal_rejects_dim_mismatch() {
        let err = parse_vector_literal("[1.0,2.0]", 3).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_vector_literal_rejects_non_finite() {
        let err = parse_vector_literal("[1.0,nan,3.0]", 3).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
        let err = parse_vector_literal("[1.0,inf,3.0]", 3).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_vector_literal_rejects_malformed_brackets() {
        assert_eq!(
            parse_vector_literal("1.0,2.0,3.0", 3)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
        assert_eq!(
            parse_vector_literal("[1.0,2.0,3.0", 3)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_vector_literal_rejects_oversized_payload() {
        let huge = format!("[{}]", "1.0,".repeat(20_000));
        let err = parse_vector_literal(&huge, 20_000).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn parse_vector_literal_accepts_boundary_length() {
        // ちょうど MAX_VECTOR_LITERAL_BYTES に収まる場合は PayloadTooLarge にならない
        // （不正形式であっても 22000 になる＝バイト長検証自体は通過する）。
        let padding = "0".repeat(MAX_VECTOR_LITERAL_BYTES - 2);
        let literal = format!("[{padding}"); // 閉じ括弧なしで意図的に不正形状にする
        assert_eq!(literal.len(), MAX_VECTOR_LITERAL_BYTES - 1);
        let err = parse_vector_literal(&literal, 1).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- parse_array_literal（TABLE-14・TASK-198、Issue #888） ------------------

    fn text_array_ty(max_len: u32) -> crate::catalog::ArrayType {
        crate::catalog::ArrayType::new(crate::catalog::ArrayElemType::Text, max_len)
            .expect("array ty")
    }

    fn bool_array_ty(max_len: u32) -> crate::catalog::ArrayType {
        crate::catalog::ArrayType::new(crate::catalog::ArrayElemType::Bool, max_len)
            .expect("array ty")
    }

    #[test]
    fn parse_array_literal_accepts_empty_array() {
        let v = parse_array_literal("{}", text_array_ty(4)).expect("valid literal");
        assert_eq!(v, crate::row_codec::ArrayValue::Text(vec![]));
    }

    #[test]
    fn parse_array_literal_accepts_unquoted_text_elements() {
        let v = parse_array_literal("{a,b,c}", text_array_ty(4)).expect("valid literal");
        assert_eq!(
            v,
            crate::row_codec::ArrayValue::Text(vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string()
            ])
        );
    }

    #[test]
    fn parse_array_literal_accepts_quoted_elements_with_escapes() {
        let v = parse_array_literal(r#"{"a,b","c\"d","",  spaced }"#, text_array_ty(4))
            .expect("valid literal");
        assert_eq!(
            v,
            crate::row_codec::ArrayValue::Text(vec![
                "a,b".to_string(),
                "c\"d".to_string(),
                "".to_string(),
                "spaced".to_string(),
            ])
        );
    }

    #[test]
    fn parse_array_literal_quoted_null_is_literal_string() {
        let v = parse_array_literal(r#"{"NULL"}"#, text_array_ty(4)).expect("valid literal");
        assert_eq!(
            v,
            crate::row_codec::ArrayValue::Text(vec!["NULL".to_string()])
        );
    }

    #[test]
    fn parse_array_literal_rejects_unquoted_null_element() {
        let err = parse_array_literal("{a,null,b}", text_array_ty(4)).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
        let err = parse_array_literal("{NULL}", text_array_ty(4)).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_array_literal_rejects_malformed_brackets() {
        assert_eq!(
            parse_array_literal("a,b,c", text_array_ty(4))
                .unwrap_err()
                .wire_code(),
            "22000"
        );
        assert_eq!(
            parse_array_literal("{a,b,c", text_array_ty(4))
                .unwrap_err()
                .wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_array_literal_rejects_unterminated_quote() {
        let err = parse_array_literal(r#"{"a}"#, text_array_ty(4)).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_array_literal_rejects_trailing_comma() {
        let err = parse_array_literal("{a,b,}", text_array_ty(4)).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_array_literal_rejects_nested_braces() {
        let err = parse_array_literal("{a,{b,c}}", text_array_ty(4)).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn parse_array_literal_rejects_element_count_exceeding_max_len() {
        let err = parse_array_literal("{a,b,c}", text_array_ty(2)).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn parse_array_literal_rejects_oversized_payload() {
        let huge = format!("{{{}}}", "a,".repeat(2_000_000));
        let err = parse_array_literal(&huge, text_array_ty(crate::catalog::MAX_ARRAY_ELEMENTS))
            .unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn parse_array_literal_accepts_bool_elements_case_insensitive() {
        let v = parse_array_literal("{t,F,true,FALSE}", bool_array_ty(8)).expect("valid literal");
        assert_eq!(
            v,
            crate::row_codec::ArrayValue::Bool(vec![true, false, true, false])
        );
    }

    #[test]
    fn parse_array_literal_rejects_invalid_bool_word() {
        let err = parse_array_literal("{t,maybe}", bool_array_ty(8)).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind: C1（純粋 Top-k） --------------------------------------------------

    #[test]
    fn binds_distance_form_to_ranking_distance() {
        let bound =
            bind_sql("SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
                .expect("bind should succeed");
        assert_eq!(bound.table, "documents");
        assert_eq!(bound.limit, 5);
        assert!(matches!(bound.ranking, Ranking::Distance { .. }));
        assert!(bound.metadata_filters.is_empty());
        assert!(!bound.rls_predicate_present);
    }

    #[test]
    fn binds_projection_all_to_id_plus_schema_columns() {
        let bound =
            bind_sql("SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
                .expect("bind should succeed");
        assert_eq!(
            bound.projection,
            vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 0,
                    name: "embedding".to_string()
                },
                ProjectedColumn::Column {
                    index: 1,
                    name: "body".to_string()
                },
                ProjectedColumn::Column {
                    index: 2,
                    name: "lang".to_string()
                },
            ]
        );
    }

    #[test]
    fn binds_explicit_projection_including_id_pseudo_column() {
        let bound = bind_sql(
            "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("bind should succeed");
        assert_eq!(
            bound.projection,
            vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 1,
                    name: "body".to_string()
                },
            ]
        );
    }

    #[test]
    fn binds_real_id_column_over_pseudo_column_when_schema_declares_it() {
        // Issue #56 レビュー指摘対応（P1/Medium: User id column is shadowed）:
        // カタログ上に実カラム `id`（`ColumnType::Text`）が存在する場合、
        // `SELECT id` は行キー疑似列ではなくその実カラムへ束縛されなければならない。
        let schema = TableSchema::new(
            "labeled_docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("id", ColumnType::Text, false),
            ],
        );
        let lookup = FakeCatalog {
            tables: ["labeled_docs"].into_iter().collect(),
        };
        let stmt = validate_statement(
            "SELECT id FROM labeled_docs ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
            &lookup,
        )
        .expect("must pass allowlist");
        let bound = bind(&stmt, &schema).expect("bind should succeed");
        assert_eq!(
            bound.projection,
            vec![ProjectedColumn::Column {
                index: 1,
                name: "id".to_string()
            }]
        );
    }

    #[test]
    fn rejects_unknown_projected_column() {
        let err =
            bind_sql("SELECT nope FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
                .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_distance_order_by_column_not_the_vector_column() {
        let err =
            bind_sql("SELECT * FROM documents ORDER BY body <=> '[0.1]' LIMIT 5").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_limit_zero_and_over_max() {
        assert!(
            bind_sql("SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 1")
                .is_ok()
        );
        let err = bind_sql(&format!(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT {}",
            crate::core::MAX_SEARCH_K + 1
        ))
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn validate_search_offset_accepts_zero_and_max() {
        // Issue #916・SQL-25 (b)・TASK-209: `validate_search_limit` と異なり `0`
        // （no-op）を受理する。
        assert_eq!(validate_search_offset(0).unwrap(), 0);
        assert_eq!(
            validate_search_offset(crate::core::MAX_SEARCH_K as u32).unwrap(),
            crate::core::MAX_SEARCH_K
        );
    }

    #[test]
    fn validate_search_offset_rejects_over_max() {
        let err = validate_search_offset(crate::core::MAX_SEARCH_K as u32 + 1).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind: C2（スカラー条件付き） ----------------------------------------------

    #[test]
    fn binds_scalar_equality_filter() {
        let bound = bind_sql(
            "SELECT * FROM documents WHERE lang = 'ja' ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("bind should succeed");
        assert_eq!(bound.metadata_filters.len(), 1);
        let filter = &bound.metadata_filters[0];
        assert_eq!(filter.column_index(), 2);
        assert_eq!(
            filter.op(),
            &crate::declarative_filter::FilterOp::Equals("ja".to_string())
        );
    }

    #[test]
    fn rejects_scalar_equality_on_vector_column() {
        let err = bind_sql(
            "SELECT * FROM documents WHERE embedding = 'x' ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_scalar_equality_on_unknown_column() {
        let err = bind_sql(
            "SELECT * FROM documents WHERE nope = 'x' ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind: C3（RLS。`visible()` の有無だけを観測する） --------------------------

    #[test]
    fn binds_visible_predicate_call_sets_flag_only() {
        let bound = bind_sql(
            "SELECT * FROM documents WHERE visible() ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("bind should succeed");
        assert!(bound.rls_predicate_present);
        assert!(bound.metadata_filters.is_empty());
    }

    // --- bind: C4（ハイブリッド） --------------------------------------------------

    #[test]
    fn binds_hybrid_four_arg_form_to_ranking_hybrid() {
        let bound = bind_sql(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, '[0.1,0.2,0.3]', body, 'query text') LIMIT 5",
        )
        .expect("bind should succeed");
        match bound.ranking {
            Ranking::Hybrid {
                query,
                text_column_index,
                query_text,
            } => {
                assert_eq!(query, vec![0.1, 0.2, 0.3]);
                assert_eq!(text_column_index, 1);
                assert_eq!(query_text, "query text");
            }
            other => panic!("expected Ranking::Hybrid, got {other:?}"),
        }
    }

    #[test]
    fn binds_hybrid_alternate_name_four_arg_form() {
        let bound = bind_sql(
            "SELECT * FROM documents ORDER BY HYBRID(embedding, '[0.1,0.2,0.3]', body, 'query text') LIMIT 5",
        )
        .expect("bind should succeed");
        assert!(matches!(bound.ranking, Ranking::Hybrid { .. }));
    }

    #[test]
    fn rejects_hybrid_two_arg_form_as_not_executable() {
        let err = bind_sql(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, 'query text') LIMIT 5",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind: evaluation_order 素通し（TASK-76・SQL-7） -------------------------

    #[test]
    fn binds_default_evaluation_order_when_hint_order_absent() {
        let bound =
            bind_sql("SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5")
                .expect("bind should succeed");
        assert_eq!(bound.evaluation_order, EvaluationOrder::DEFAULT);
    }

    #[test]
    fn binds_explicit_evaluation_order_from_hint_order() {
        let bound = bind_sql(
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 HINT ORDER(DISTANCE, SCALAR, RLS)",
        )
        .expect("bind should succeed");
        assert_eq!(
            bound.evaluation_order.stages(),
            [
                crate::sql::plan::Stage::Distance,
                crate::sql::plan::Stage::Scalar,
                crate::sql::plan::Stage::Rls,
            ]
        );
    }

    #[test]
    fn rejects_hybrid_four_arg_form_with_non_text_second_column() {
        let err = bind_sql(
            "SELECT * FROM documents ORDER BY hybrid_rrf(embedding, '[0.1,0.2,0.3]', embedding, 'q') LIMIT 5",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind_insert（SQL-10、TASK-80） ----------------------------------------

    fn bind_insert_sql(sql: &str) -> Result<BoundInsert, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_insert(
            sql,
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        bind_insert(&stmt, &docs_schema())
    }

    #[test]
    fn binds_insert_with_all_columns() {
        let bound = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body, lang) VALUES (1, '[0.1,0.2,0.3]', 'hello', 'ja') USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_insert should succeed");
        assert_eq!(bound.table, "documents");
        assert_eq!(bound.id, 1);
        assert_eq!(
            bound.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
        assert_eq!(
            bound.values,
            vec![
                crate::row_codec::Value::Vector(vec![0.1, 0.2, 0.3]),
                crate::row_codec::Value::Text("hello".to_string()),
                crate::row_codec::Value::Text("ja".to_string()),
            ]
        );
    }

    /// `bind_json_literal`（`sql::parser::bind_insert_row` から呼ばれる束縛層の
    /// 単一チョークポイント）は `MAX_JSON_FIELD_LEN` を 1 バイトでも超える入力を
    /// `54000`（`PayloadTooLarge`）で拒否する（PR #1014 レビュー指摘対応）。
    #[test]
    fn bind_json_literal_rejects_body_one_byte_over_max_json_field_len() {
        let oversized = "1".repeat(crate::json::MAX_JSON_FIELD_LEN + 1);
        let err = bind_json_literal(&oversized, &ColumnType::Json, "doc").unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    /// `MAX_JSON_FIELD_LEN` ちょうどの有効な JSON は束縛層を通過し、かつその
    /// 束縛結果（`Value::Json`）が実際の `row_codec::encode_scalar_columns`
    /// （presence(1) + 長さ(4) バイトのフレーミングを付与する行コーデック）でも
    /// 単独で `MAX_SCALAR_PAYLOAD_LEN` へ収まることを固定する。旧
    /// `MAX_JSON_FIELD_LEN` 定義（`MAX_TEXT_FIELD_LEN` と同値）では、この境界値が
    /// 束縛層を通過した**後**にフレーミング分だけ `encode_scalar_columns` 側で
    /// 拒否され得た（codex-review P1 指摘・PR #1014）。
    #[test]
    fn bind_json_literal_at_exact_max_json_field_len_fits_scalar_payload_after_encode() {
        // 単一の文字列リテラルは `MAX_JSON_STRING_CHARS`（1 MiB）に抵触するため、
        // `json.rs` の境界値テストと同じ方式（複数要素の配列）で目標バイト数を
        // ちょうど組み立てる: `[` + 5 要素（`"`×2 + 本体） + 4 個の `,` + `]`。
        let target = crate::json::MAX_JSON_FIELD_LEN;
        let overhead = 1 + 1 + 4 + 5 * 2;
        let content_total = target - overhead;
        let base = content_total / 5;
        let remainder = content_total % 5;
        let lens = [base, base, base, base, base + remainder];
        let mut doc = String::new();
        doc.push('[');
        for (i, len) in lens.iter().enumerate() {
            if i > 0 {
                doc.push(',');
            }
            doc.push('"');
            doc.push_str(&"a".repeat(*len));
            doc.push('"');
        }
        doc.push(']');
        assert_eq!(doc.len(), target);

        let value =
            bind_json_literal(&doc, &ColumnType::Json, "doc").expect("must bind at exact limit");

        let schema = TableSchema::new(
            "docs",
            vec![
                crate::catalog::ColumnDef::new("embedding", ColumnType::Vector(2), false),
                crate::catalog::ColumnDef::new("doc", ColumnType::Json, true),
            ],
        );
        let values = vec![crate::row_codec::Value::Vector(vec![0.1, 0.2]), value];
        crate::row_codec::encode_scalar_columns(&schema, &values)
            .expect("bind_json_literal output must always fit the scalar payload budget");
    }

    #[test]
    fn binds_insert_leaving_nullable_column_null_when_omitted() {
        let bound = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_insert should succeed");
        assert_eq!(
            bound.values,
            vec![
                crate::row_codec::Value::Vector(vec![0.1, 0.2, 0.3]),
                crate::row_codec::Value::Text("hello".to_string()),
                crate::row_codec::Value::Null,
            ]
        );
    }

    #[test]
    fn rejects_insert_missing_id_pseudo_column() {
        let err = bind_insert_sql(
            "INSERT INTO documents (embedding, body) VALUES ('[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_id_value_out_of_u64_range() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body) VALUES (18446744073709551616, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_unknown_column() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding, nope) VALUES (1, '[0.1,0.2,0.3]', 'x') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_type_mismatch_number_for_text_column() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 42) USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_type_mismatch_string_for_vector_column_wrong_dim() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_insert_missing_non_nullable_column() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn rejects_insert_duplicate_column_in_list() {
        let err = bind_insert_sql(
            "INSERT INTO documents (id, id) VALUES (1, 2) USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind_delete（SQL-18、TASK-191） ----------------------------------------

    fn bind_delete_sql(sql: &str) -> Result<BoundDelete, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_delete(
            sql,
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        bind_delete(&stmt)
    }

    #[test]
    fn bind_delete_accepts_valid_id() {
        let bound =
            bind_delete_sql("DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'op-0001'")
                .expect("bind_delete should succeed");
        assert_eq!(bound.table, "documents");
        assert_eq!(bound.id, 1);
        assert_eq!(
            bound.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
    }

    #[test]
    fn bind_delete_rejects_id_overflowing_u64() {
        // 構文解析（`expect_number`）は桁数を制限しないため、`u64` の範囲外は
        // bind 段で明示的に検出する必要がある（許可リストを直接構築して固定）。
        let stmt = ValidatedDelete {
            table_name: "documents".to_string(),
            id_literal: "18446744073709551616".to_string(),
            operation_id: Some(OperationId::parse("op-0001").expect("valid operation_id")),
            returning: None,
        };
        let err = bind_delete(&stmt).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_delete_rejects_decimal_id() {
        // 構文解析は `WHERE id = 1.5` の小数形を `Token::Number("1.5")` として
        // 通過させうる（字句解析は小数を数値として許容するため）。`u64` への
        // 解釈不能は bind 段で明示的に拒否する。
        let stmt = ValidatedDelete {
            table_name: "documents".to_string(),
            id_literal: "1.5".to_string(),
            operation_id: Some(OperationId::parse("op-0001").expect("valid operation_id")),
            returning: None,
        };
        let err = bind_delete(&stmt).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind_predicate_delete（述語つき DELETE、Issue #870・TASK-192・SQL-19） ---

    fn bind_predicate_delete_sql(sql: &str) -> Result<BoundPredicateDelete, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_delete_statement(
            sql,
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        let pd = match stmt {
            crate::sql::allowlist::DeleteStatement::Predicate(pd) => pd,
            crate::sql::allowlist::DeleteStatement::SingleRow(_) => {
                panic!("must classify as predicate form")
            }
        };
        bind_predicate_delete(
            &pd,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
    }

    #[test]
    fn bind_predicate_delete_maps_equality_and_prefix_predicates_to_metadata_filters() {
        let bound = bind_predicate_delete_sql(
            "DELETE FROM documents WHERE lang = 'ja' AND body LIKE 'src/%' USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_predicate_delete should succeed");
        assert_eq!(bound.table, "documents");
        assert_eq!(bound.metadata_filters.len(), 2);
        assert!(bound.expr_filters.is_empty());
        assert_eq!(
            bound.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
        assert_eq!(bound.max_affected_rows, DEFAULT_MAX_DML_AFFECTED_ROWS);
    }

    #[test]
    fn bind_predicate_delete_maps_comparison_predicate_to_expr_filters() {
        let bound = bind_predicate_delete_sql(
            "DELETE FROM documents WHERE id > 5 USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_predicate_delete should succeed");
        assert!(bound.metadata_filters.is_empty());
        assert_eq!(bound.expr_filters.len(), 1);
    }

    #[test]
    fn bind_predicate_delete_accepts_visible_predicate_without_producing_a_filter() {
        let bound = bind_predicate_delete_sql(
            "DELETE FROM documents WHERE visible() USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_predicate_delete should succeed");
        assert!(bound.metadata_filters.is_empty());
        assert!(bound.expr_filters.is_empty());
    }

    #[test]
    fn rejects_predicate_delete_unknown_column() {
        let err = bind_predicate_delete_sql(
            "DELETE FROM documents WHERE ghost = 'x' USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_predicate_delete_vector_column_predicate() {
        let err = bind_predicate_delete_sql(
            "DELETE FROM documents WHERE embedding = 'x' USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_predicate_delete_rejects_metadata_filter_count_over_limit() {
        // 字句解析のトークン上限に先に当たらないよう、許可リストを直接構築する
        // （`declarative_filter::check_filter_count_accepts_at_limit_and_rejects_over_limit`
        // と同じ手法。`MAX_METADATA_FILTERS` = 256）。
        let where_predicates: Vec<WherePredicate> = (0
            ..=crate::declarative_filter::MAX_METADATA_FILTERS)
            .map(|i| WherePredicate::Equality {
                column: "lang".to_string(),
                value: format!("v{i}"),
            })
            .collect();
        let stmt = ValidatedPredicateDelete {
            table_name: "documents".to_string(),
            where_predicates,
            operation_id: Some(OperationId::parse("op-0001").expect("valid operation_id")),
        };
        let err = bind_predicate_delete(
            &stmt,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    /// R1: 同一述語・同一スキーマで `bind_predicate_delete` の
    /// `metadata_filters`／`expr_filters` が `bind_scan`（広域取得、Issue #454）
    /// のものと構造的に一致することを機械検証する（第 2 の述語評価器を作らない
    /// 契約の固定）。
    #[test]
    fn bind_predicate_delete_matches_bind_scan_for_same_predicate_text() {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let predicate_text = "lang = 'ja' AND id > 5";

        let delete_bound = bind_predicate_delete_sql(&format!(
            "DELETE FROM documents WHERE {predicate_text} USING OPERATION_ID 'op-0001'"
        ))
        .expect("bind_predicate_delete should succeed");

        let scan_stmt = match crate::sql::allowlist::validate_sql(
            &format!("SELECT id FROM documents WHERE {predicate_text} LIMIT 1"),
            &lookup,
        )
        .expect("SELECT ... LIMIT should be accepted as a scan statement")
        {
            crate::sql::allowlist::Statement::Scan(scan) => scan,
            other => panic!("must classify as Statement::Scan, got {other:?}"),
        };
        let scan_bound = bind_scan(
            &scan_stmt,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
        .expect("bind_scan should succeed");

        assert_eq!(delete_bound.metadata_filters, scan_bound.metadata_filters);
        assert_eq!(delete_bound.expr_filters, scan_bound.expr_filters);
    }

    // --- check_affected_row_count（Issue #870・#871 が結線する実行時判定の
    // 共有ヘルパー。本 Issue の時点では呼び出し元が存在しないため、境界値
    // （`count == limit` と `count == limit + 1`）を直接固定する） -------------

    #[test]
    fn check_affected_row_count_accepts_count_at_limit() {
        assert!(check_affected_row_count(
            DEFAULT_MAX_DML_AFFECTED_ROWS,
            DEFAULT_MAX_DML_AFFECTED_ROWS
        )
        .is_ok());
    }

    #[test]
    fn check_affected_row_count_rejects_count_over_limit_by_one() {
        let err = check_affected_row_count(
            DEFAULT_MAX_DML_AFFECTED_ROWS + 1,
            DEFAULT_MAX_DML_AFFECTED_ROWS,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn check_affected_row_count_accepts_zero_count_against_zero_limit() {
        // `limit == 0` は「一切変更を許さない」極端値。0 行の変更は許容される
        // ことを固定する（`count > limit` の厳密な比較が境界で崩れていないか
        // の確認）。
        assert!(check_affected_row_count(0, 0).is_ok());
    }

    #[test]
    fn check_affected_row_count_rejects_any_count_against_zero_limit() {
        let err = check_affected_row_count(1, 0).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    // --- bind_update（SQL-17、TASK-191） ----------------------------------------

    fn bind_update_sql(sql: &str) -> Result<BoundUpdate, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_update(
            sql,
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        bind_update(&stmt, &docs_schema())
    }

    #[test]
    fn binds_update_of_text_column() {
        let bound = bind_update_sql(
            "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_update should succeed");
        assert_eq!(bound.table, "documents");
        assert_eq!(bound.id, 1);
        assert_eq!(
            bound.operation_id.as_ref().map(OperationId::as_str),
            Some("op-0001")
        );
        // `lang` は `docs_schema()` の列順で index 2（embedding=0, body=1, lang=2）。
        assert_eq!(
            bound.assignments,
            vec![(2, crate::row_codec::Value::Text("en".to_string()))]
        );
    }

    #[test]
    fn binds_update_of_vector_column_with_matching_dim() {
        let bound = bind_update_sql(
            "UPDATE documents SET embedding = '[0.1,0.2,0.3]' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_update should succeed");
        assert_eq!(
            bound.assignments,
            vec![(0, crate::row_codec::Value::Vector(vec![0.1, 0.2, 0.3]))]
        );
    }

    #[test]
    fn binds_update_of_multiple_columns_preserving_declared_order() {
        let bound = bind_update_sql(
            "UPDATE documents SET lang = 'ja', body = 'x' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_update should succeed");
        // 宣言順（lang → body）を保持し、スキーマ列順（body=1 → lang=2）へは
        // 並べ替えない（部分更新の正規化基準。BoundUpdate ドキュメンテーション
        // コメント参照）。
        assert_eq!(
            bound.assignments,
            vec![
                (2, crate::row_codec::Value::Text("ja".to_string())),
                (1, crate::row_codec::Value::Text("x".to_string())),
            ]
        );
    }

    #[test]
    fn bind_update_is_partial_and_omits_unset_columns() {
        let bound = bind_update_sql(
            "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .expect("bind_update should succeed");
        // SET で指定していない列（embedding・body）は assignments に一切現れない
        // （部分更新。INSERT の Null 埋め全列ベクトルとは異なる契約）。
        assert_eq!(bound.assignments.len(), 1);
        assert!(bound
            .assignments
            .iter()
            .all(|(idx, _)| *idx != 0 && *idx != 1));
    }

    #[test]
    fn rejects_update_vector_column_with_dim_mismatch() {
        let err = bind_update_sql(
            "UPDATE documents SET embedding = '[0.1,0.2]' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_update_vector_column_with_number_literal() {
        let err = bind_update_sql(
            "UPDATE documents SET embedding = 5 WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_update_text_column_with_number_literal() {
        let err = bind_update_sql(
            "UPDATE documents SET lang = 5 WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_update_unknown_column() {
        let err = bind_update_sql(
            "UPDATE documents SET ghost = 'x' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_update_duplicate_column_in_set_clause() {
        let err = bind_update_sql(
            "UPDATE documents SET lang = 'en', lang = 'ja' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_update_set_id_pseudo_column() {
        let err = bind_update_sql(
            "UPDATE documents SET id = 5 WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_set_tenant_id_column() {
        let err = bind_update_sql(
            "UPDATE documents SET tenant_id = 'evil' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_set_visibility_column() {
        let err = bind_update_sql(
            "UPDATE documents SET visibility = 'public' WHERE id = 1 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_update_id_out_of_u64_range() {
        let err = bind_update_sql(
            "UPDATE documents SET lang = 'en' WHERE id = 99999999999999999999 USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind_update_form（SQL-19、TASK-192・Issue #869） -----------------------

    fn bind_update_form_sql(sql: &str) -> Result<BoundUpdateForm, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_update_form(
            sql,
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        bind_update_form(
            &stmt,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
    }

    #[test]
    fn bind_update_form_single_arm_matches_bind_update() {
        let sql = "UPDATE documents SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-0001'";
        let via_bind_update = bind_update_sql(sql).expect("bind_update must succeed");
        let via_form = bind_update_form_sql(sql).expect("bind_update_form must succeed");
        match via_form {
            BoundUpdateForm::Single(bound) => assert_eq!(bound, via_bind_update),
            BoundUpdateForm::Predicate(_) => panic!("expected Single variant"),
        }
    }

    #[test]
    fn bind_update_form_predicate_arm_binds_equality_metadata_filter() {
        let bound = bind_update_form_sql(
            "UPDATE documents SET body = 'x' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
        )
        .expect("predicate-form UPDATE must bind");
        match bound {
            BoundUpdateForm::Predicate(p) => {
                assert_eq!(p.metadata_filters().len(), 1);
                assert!(p.expr_filters().is_empty());
                assert_eq!(
                    p.assignments(),
                    &[(1, crate::row_codec::Value::Text("x".to_string()))]
                );
            }
            BoundUpdateForm::Single(_) => panic!("expected Predicate variant"),
        }
    }

    #[test]
    fn bind_update_form_predicate_arm_binds_expression_filter_for_id_comparison() {
        let bound = bind_update_form_sql(
            "UPDATE documents SET body = 'x' WHERE id > 10 USING OPERATION_ID 'op-0001'",
        )
        .expect("id > 10 must bind as an expression filter");
        match bound {
            BoundUpdateForm::Predicate(p) => {
                assert!(p.metadata_filters().is_empty());
                assert_eq!(p.expr_filters().len(), 1);
            }
            BoundUpdateForm::Single(_) => panic!("expected Predicate variant"),
        }
    }

    #[test]
    fn bind_update_form_predicate_arm_rejects_set_of_id_column() {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_update_form(
            "UPDATE documents SET id = '5' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        let err = bind_update_form(
            &stmt,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_update_form_predicate_arm_rejects_vector_column_predicate() {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_update_form(
            "UPDATE documents SET body = 'x' WHERE embedding = 'x' USING OPERATION_ID 'op-0001'",
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        let err = bind_update_form(
            &stmt,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_update_form_predicate_arm_rejects_visible_only_where() {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_update_form(
            "UPDATE documents SET body = 'x' WHERE visible() USING OPERATION_ID 'op-0001'",
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        let err = bind_update_form(
            &stmt,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn bind_update_form_predicate_arm_rejects_id_equality_with_non_numeric_literal() {
        // `WHERE id = 'x'` は id 単純形の 3 トークン一致（`Number` 期待）に外れて
        // 述語形へ流れ、`declarative_filter` が `id` を未知列として `22000` で
        // 拒否する（`validate_update` 経由の `42601` とは異なるエントリポイント
        // ごとの契約差。計画 §2.2 参照）。
        let err = bind_update_form_sql(
            "UPDATE documents SET body = 'x' WHERE id = 'x' USING OPERATION_ID 'op-0001'",
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bound_predicate_update_new_round_trips_through_accessors() {
        // `BoundPredicateUpdate::new` は `pub(crate)` の raw constructor
        // （codex-review 指摘・PR #985 是正）であり、意味論検証（SET 対象列・
        // 無条件更新拒否・`operation_id` 必須化）は行わない契約のまま。
        // 本テストはアクセサーの往復のみを検証し、「空フィルタが安全に構築できる
        // 公開 API がある」ことは意味しない（crate 外からの直接構築は不可能）。
        let bound = BoundPredicateUpdate::new(
            "documents".to_string(),
            vec![(1, crate::row_codec::Value::Text("x".to_string()))],
            vec![],
            vec![],
            vec![],
            Some(OperationId::parse("op-0001").expect("valid operation_id")),
        );
        assert_eq!(bound.table(), "documents");
        assert_eq!(
            bound.assignments(),
            &[(1, crate::row_codec::Value::Text("x".to_string()))]
        );
        assert!(bound.metadata_filters().is_empty());
        assert!(bound.expr_filters().is_empty());
        assert_eq!(
            bound.operation_id().map(OperationId::as_str),
            Some("op-0001")
        );
    }

    #[test]
    fn check_dml_affected_rows_accepts_up_to_limit_and_rejects_over_limit() {
        assert!(check_dml_affected_rows(MAX_DML_AFFECTED_ROWS).is_ok());
        let err = check_dml_affected_rows(MAX_DML_AFFECTED_ROWS + 1).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    // --- bind_insert_form: 形判別（TASK-120・INDEX-1, INDEX-2） -----------------

    fn file_docs_schema() -> TableSchema {
        TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        )
    }

    fn bind_insert_form_sql_with_schema(
        sql: &str,
        schema: &TableSchema,
    ) -> Result<BoundInsertForm, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_insert(
            sql,
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");
        bind_insert_form(&stmt, schema)
    }

    #[test]
    fn bind_insert_form_detects_file_form_without_id_or_vector_column() {
        let bound = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path, body) VALUES ('a.txt', 'hello world') USING OPERATION_ID 'op-file-1'",
            &file_docs_schema(),
        )
        .expect("bind_insert_form should succeed");
        match bound {
            BoundInsertForm::File(f) => {
                assert_eq!(f.table, "documents");
                assert_eq!(f.path, "a.txt");
                assert_eq!(f.body, "hello world");
                assert_eq!(f.vector_column_index, 0);
                assert_eq!(f.path_column_index, 1);
                assert_eq!(f.body_column_index, 2);
                // `path`/`body`/VECTOR 列の位置は本文全文を保持しない
                // （チャンク数分の複製増幅の防止。codex-review P1 指摘・PR #221）。
                assert_eq!(
                    f.template_values.get(f.vector_column_index),
                    Some(&crate::row_codec::Value::Null)
                );
                assert_eq!(
                    f.template_values.get(f.path_column_index),
                    Some(&crate::row_codec::Value::Null)
                );
                assert_eq!(
                    f.template_values.get(f.body_column_index),
                    Some(&crate::row_codec::Value::Null)
                );
            }
            other => panic!("expected file form, got {other:?}"),
        }
    }

    #[test]
    fn bind_insert_form_detects_row_form_with_id_and_vector_column() {
        let bound = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (id, embedding, path, body) VALUES (1, '[0.1,0.2,0.3]', 'a.txt', 'hello') USING OPERATION_ID 'op-row-1'",
            &file_docs_schema(),
        )
        .expect("bind_insert_form should succeed");
        match bound {
            BoundInsertForm::Row(r) => {
                assert_eq!(r.id, 1);
            }
            other => panic!("expected row form, got {other:?}"),
        }
    }

    #[test]
    fn bind_insert_form_treats_id_plus_path_body_as_row_form_and_fails_without_vector() {
        // path/body を指定していても id を同時指定した場合は行形として扱われ、
        // 行形の既存検証（embedding 未提供で 22000）にそのまま倒れる。
        let err = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (id, path, body) VALUES (1, 'a.txt', 'hello') USING OPERATION_ID 'op-row-2'",
            &file_docs_schema(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn bind_insert_form_treats_vector_plus_path_body_as_row_form_and_fails_without_id() {
        // VECTOR 列を同時指定した場合も行形として扱われ、id 未提供で 22000 になる。
        let err = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (embedding, path, body) VALUES ('[0.1,0.2,0.3]', 'a.txt', 'hello') USING OPERATION_ID 'op-row-3'",
            &file_docs_schema(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_insert_form_file_form_missing_body_falls_back_to_row_form_and_fails() {
        // path のみ・body 欠落は行形へフォールバックし、id 未提供で 22000 になる
        // （黙って file 形へ丸めない）。
        let err = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path) VALUES ('a.txt') USING OPERATION_ID 'op-row-4'",
            &file_docs_schema(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_insert_form_file_form_copies_other_text_column_into_template_values() {
        let bound = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path, body, lang) VALUES ('a.txt', 'hello', 'ja') USING OPERATION_ID 'op-file-2'",
            &file_docs_schema(),
        )
        .expect("bind_insert_form should succeed");
        match bound {
            BoundInsertForm::File(f) => {
                assert_eq!(
                    f.template_values.get(3),
                    Some(&crate::row_codec::Value::Text("ja".to_string()))
                );
            }
            other => panic!("expected file form, got {other:?}"),
        }
    }

    #[test]
    // codex-review P1 指摘（PR #1007・Issue #882・`crates/engine/src/sql/
    // parser.rs:2572` 指摘）: `bind_file_insert` は `path`／`body` の TEXT 列
    // 専用のチャンク化・埋め込み経路であり、他の追加スカラー型
    // （BOOLEAN・DATE・ARRAY・BYTEA・JSON・ENUM・NUMERIC）は一律 `not supported
    // for file-form INSERT` として拒否している。REAL／DOUBLE PRECISION も同じ
    // 理由で対象外であることを固定する（typed INSERT/UPDATE 向けの数値束縛を
    // ファイル形 INSERT へ誤って露出させない）。
    fn bind_insert_form_file_form_rejects_real_and_double_columns() {
        let mut schema = file_docs_schema();
        schema
            .columns
            .push(ColumnDef::new("score", ColumnType::Real, true));
        schema
            .columns
            .push(ColumnDef::new("weight", ColumnType::Double, true));

        let err_real = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path, body, score) VALUES ('a.txt', 'hello', 1.5) USING OPERATION_ID 'op-file-real'",
            &schema,
        )
        .expect_err("REAL column must be rejected for file-form INSERT");
        assert_eq!(err_real.wire_code(), "22000");

        let err_double = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path, body, weight) VALUES ('a.txt', 'hello', 1.5) USING OPERATION_ID 'op-file-double'",
            &schema,
        )
        .expect_err("DOUBLE PRECISION column must be rejected for file-form INSERT");
        assert_eq!(err_double.wire_code(), "22000");
    }

    #[test]
    // codex/review P1・Cursor Medium 指摘（PR #1008・Issue #881）: `bind_file_insert`
    // が INTEGER／BIGINT だけを typed INSERT 向け `bind_integer_literal` で受理し、
    // REAL・DOUBLE・BOOLEAN 等の他の非 TEXT スカラー型と異なる緩い扱いになっていた。
    // 他の非 TEXT 型と同じ `not supported for file-form INSERT`（`22000`）へ是正した
    // ことを固定する。
    fn bind_insert_form_file_form_rejects_integer_and_bigint_columns() {
        let mut schema = file_docs_schema();
        schema
            .columns
            .push(ColumnDef::new("count", ColumnType::Integer, true));
        schema
            .columns
            .push(ColumnDef::new("big_count", ColumnType::BigInt, true));

        let err_integer = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path, body, count) VALUES ('a.txt', 'hello', 1) USING OPERATION_ID 'op-file-int'",
            &schema,
        )
        .expect_err("INTEGER column must be rejected for file-form INSERT");
        assert_eq!(err_integer.wire_code(), "22000");

        let err_bigint = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path, body, big_count) VALUES ('a.txt', 'hello', 1) USING OPERATION_ID 'op-file-bigint'",
            &schema,
        )
        .expect_err("BIGINT column must be rejected for file-form INSERT");
        assert_eq!(err_bigint.wire_code(), "22000");
    }

    #[test]
    fn bind_insert_form_file_form_rejects_missing_non_nullable_text_column() {
        let mut schema = file_docs_schema();
        // `lang` を非 nullable 化して未指定時に 22000 になることを確認する。
        if let Some(c) = schema.columns.get_mut(3) {
            c.nullable = false;
        }
        let err = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path, body) VALUES ('a.txt', 'hello') USING OPERATION_ID 'op-file-3'",
            &schema,
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn bind_insert_form_rejects_table_without_path_or_body_column() {
        let err = bind_insert_form_sql_with_schema(
            "INSERT INTO documents (path, body) VALUES ('a.txt', 'hello') USING OPERATION_ID 'op-file-4'",
            &docs_schema(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind_with_session: 取得モードの優先順位解決（TASK-161・SQL-12） -------------

    fn bind_sql_with_session(
        sql: &str,
        session_mode: Option<SearchMode>,
    ) -> Result<BoundStatement, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = validate_statement(sql, &lookup).expect("must pass allowlist");
        bind_with_session(&stmt, &docs_schema(), session_mode)
    }

    const SELECT_NO_MODE: &str =
        "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";

    #[test]
    fn bind_defaults_to_recall_when_no_clause_and_no_session() {
        let bound = bind_sql_with_session(SELECT_NO_MODE, None).expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Recall);
        assert_eq!(bound.mode.source, mode::ModeSource::Default);
    }

    #[test]
    fn bind_uses_session_variable_when_no_query_clause() {
        let bound = bind_sql_with_session(SELECT_NO_MODE, Some(SearchMode::Precision))
            .expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Precision);
        assert_eq!(bound.mode.source, mode::ModeSource::SessionVariable);
    }

    #[test]
    fn bind_query_clause_wins_over_session_variable() {
        let sql =
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 USING MODE 'recall'";
        let bound =
            bind_sql_with_session(sql, Some(SearchMode::Precision)).expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Recall);
        assert_eq!(bound.mode.source, mode::ModeSource::QueryClause);
    }

    #[test]
    fn bind_query_clause_alone_resolves_without_session() {
        let sql =
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 USING MODE 'precision'";
        let bound = bind_sql_with_session(sql, None).expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Precision);
        assert_eq!(bound.mode.source, mode::ModeSource::QueryClause);
    }

    #[test]
    fn bind_rejects_unknown_query_clause_mode_value() {
        let sql =
            "SELECT * FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5 USING MODE 'fuzzy'";
        let err = bind_sql_with_session(sql, None).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn plain_bind_resolves_default_mode_without_session_argument() {
        // 既存 `bind`（後方互換 API）は `bind_with_session(.., None)` へ委譲するため、
        // 句・セッションいずれも無指定なら既定 `recall` を解決する。
        let bound = bind_sql(SELECT_NO_MODE).expect("bind should succeed");
        assert_eq!(bound.mode.mode, SearchMode::Recall);
        assert_eq!(bound.mode.source, mode::ModeSource::Default);
    }

    // --- bind_aggregate（TASK-166・SQL-13） -------------------------------------

    fn bind_aggregate_sql(sql: &str) -> Result<BoundAggregate, SqlSurfaceError> {
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_sql(sql, &lookup).expect("must pass allowlist");
        let agg = match stmt {
            crate::sql::allowlist::Statement::Aggregate(agg) => agg,
            other => panic!("expected Statement::Aggregate, got {other:?}"),
        };
        bind_aggregate(
            &agg,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
    }

    #[test]
    fn binds_sum_avg_min_max_on_vector_column_as_type_mismatch() {
        for func in ["SUM", "AVG", "MIN", "MAX"] {
            let sql = format!("SELECT {func}(embedding) FROM documents");
            let err = bind_aggregate_sql(&sql).unwrap_err();
            assert_eq!(
                err.wire_code(),
                "22000",
                "{func}(embedding) should be 22000"
            );
        }
    }

    #[test]
    fn binds_sum_avg_on_text_column_as_type_mismatch() {
        for func in ["SUM", "AVG"] {
            let sql = format!("SELECT {func}(lang) FROM documents");
            let err = bind_aggregate_sql(&sql).unwrap_err();
            assert_eq!(err.wire_code(), "22000", "{func}(lang) should be 22000");
        }
    }

    #[test]
    fn binds_unknown_column_as_invalid_input() {
        let err = bind_aggregate_sql("SELECT SUM(ghost) FROM documents").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn binds_sum_on_bool_typed_expression_as_type_mismatch() {
        // 実際の SQL 文法では比較演算子（`>` 等）は集計引数の式文法
        // （`parse_value_expr`）に現れないため、この組み合わせは構文上到達しない。
        // `bind_aggregate` 自体の型検査（`ExprType::Bool` を拒否する分岐）が
        // 独立して機能することを確認するため、AST を直接組み立てて渡す
        // （防御的実装の単体検証。§計画 6-B）。
        use crate::sql::allowlist::{
            AggregateArg, AggregateFunc, AggregateItem, AggregateSelectItem, ValidatedAggregate,
        };
        use crate::sql::udf_call::BinOp;

        // `ValidatedAggregate`/`AggregateItem` のフィールドは `pub(crate)` のため、
        // 同一クレート内であるこのテストからは構造体リテラルで直接組み立てられる
        // （`allowlist.rs` のカプセル化はクレート外からの構築のみを禁じる）。
        let agg = ValidatedAggregate {
            table_name: "documents".to_string(),
            items: vec![AggregateSelectItem::Aggregate(AggregateItem {
                func: AggregateFunc::Sum,
                arg: AggregateArg::Expr(Expr::Binary {
                    op: BinOp::Gt,
                    lhs: Box::new(Expr::Ident("id".to_string())),
                    rhs: Box::new(Expr::Number("1".to_string())),
                }),
                alias: None,
                distinct: false,
            })],
            where_predicates: Vec::new(),
            group_by: None,
        };
        let err = bind_aggregate(
            &agg,
            &docs_schema(),
            &crate::sql::udf_call::UdfRegistry::default(),
        )
        .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn binds_sum_on_vector_typed_expression_as_type_mismatch() {
        let err =
            bind_aggregate_sql("SELECT SUM(vec_div(embedding, 2)) FROM documents").unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn binds_count_sum_avg_min_max_on_id_and_udf_call() {
        let bound = bind_aggregate_sql(
            "SELECT COUNT(embedding), COUNT(lang), SUM(id), AVG(vec_norm(embedding)), MIN(lang) FROM documents",
        )
        .expect("bind should succeed");
        assert_eq!(bound.items.len(), 5);
        assert_eq!(bound.items[0].input, AggregateInput::VectorColumnPresence);
        assert!(matches!(
            bound.items[1].input,
            AggregateInput::TextColumn(_)
        ));
        assert_eq!(bound.items[2].input, AggregateInput::IdU64);
        assert!(matches!(
            bound.items[3].input,
            AggregateInput::ScalarExpr { .. }
        ));
        assert!(matches!(
            bound.items[4].input,
            AggregateInput::TextColumn(_)
        ));
    }

    #[test]
    fn binds_default_alias_to_lowercase_function_name() {
        let bound =
            bind_aggregate_sql("SELECT COUNT(*) FROM documents").expect("bind should succeed");
        assert_eq!(bound.items[0].name, "count");
    }

    #[test]
    fn binds_explicit_alias_over_default() {
        let bound = bind_aggregate_sql("SELECT COUNT(*) AS total FROM documents")
            .expect("bind should succeed");
        assert_eq!(bound.items[0].name, "total");
    }

    #[test]
    fn aggregate_binds_real_id_column_over_pseudo_column_when_schema_declares_it() {
        let schema = TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("id", ColumnType::Text, false),
            ],
        );
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_sql("SELECT MIN(id) FROM documents", &lookup)
            .expect("must pass allowlist");
        let agg = match stmt {
            crate::sql::allowlist::Statement::Aggregate(agg) => agg,
            other => panic!("expected Statement::Aggregate, got {other:?}"),
        };
        let bound = bind_aggregate(&agg, &schema, &crate::sql::udf_call::UdfRegistry::default())
            .expect("bind should succeed");
        // スキーマが実カラム `id`（TEXT）を宣言しているため、疑似列ではなく実カラムへ
        // 束縛される（`resolve_aggregate_input` の優先順位。Issue #56 と同じ規約）。
        assert!(matches!(
            bound.items[0].input,
            AggregateInput::TextColumn(_)
        ));
    }

    // --- bind_column_projection（Issue #763・NOSQL-2） -------------------------

    #[test]
    fn bind_column_projection_none_is_select_all() {
        let cols = bind_column_projection(None, &docs_schema()).expect("bind ok");
        // 疑似列 `id` を先頭に、以降はスキーマの列順（`Projection::All` と同じ）。
        assert_eq!(cols.len(), docs_schema().columns.len() + 1);
        assert_eq!(cols[0], ProjectedColumn::Id);
    }

    #[test]
    fn bind_column_projection_real_column_takes_priority_over_id_pseudo_column() {
        let schema = TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("id", ColumnType::Text, false),
            ],
        );
        let names = vec!["id".to_string()];
        let cols = bind_column_projection(Some(&names), &schema).expect("bind ok");
        assert_eq!(cols.len(), 1);
        assert!(matches!(&cols[0], ProjectedColumn::Column { name, .. } if name == "id"));
    }

    #[test]
    fn bind_column_projection_maps_id_pseudo_column_when_no_real_column() {
        let names = vec!["id".to_string()];
        let cols = bind_column_projection(Some(&names), &docs_schema()).expect("bind ok");
        assert_eq!(cols, vec![ProjectedColumn::Id]);
    }

    #[test]
    fn bind_column_projection_rejects_unknown_column() {
        let names = vec!["nope".to_string()];
        let err = bind_column_projection(Some(&names), &docs_schema()).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // --- bind_vector_values（Issue #763・NOSQL-2） ------------------------------

    #[test]
    fn bind_vector_values_accepts_matching_dim() {
        let values = vec![1.0_f32, 2.0, 3.0];
        let bound = bind_vector_values(&values, &docs_schema()).expect("bind ok");
        assert_eq!(bound, vec![1.0_f32, 2.0, 3.0]);
    }

    #[test]
    fn bind_vector_values_rejects_dim_mismatch() {
        let values = vec![1.0_f32, 2.0];
        let err = bind_vector_values(&values, &docs_schema()).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_vector_values_rejects_empty_when_dim_nonzero() {
        let values: Vec<f32> = vec![];
        let err = bind_vector_values(&values, &docs_schema()).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_vector_values_rejects_non_finite_defense_in_depth() {
        // 呼び出し元（`JsonNumber::as_f32`）が非有限を既に `None` として弾く
        // 契約だが、本関数自身も多層防御として非有限を拒否することを固定する。
        let values = vec![f32::NAN, 2.0, 3.0];
        assert_eq!(
            bind_vector_values(&values, &docs_schema())
                .unwrap_err()
                .wire_code(),
            "22000"
        );
        let values = vec![f32::INFINITY, 2.0, 3.0];
        assert_eq!(
            bind_vector_values(&values, &docs_schema())
                .unwrap_err()
                .wire_code(),
            "22000"
        );
    }

    // --- bind_body_text_column（Issue #763・NOSQL-2） ---------------------------

    #[test]
    fn bind_body_text_column_resolves_body_column() {
        let idx = bind_body_text_column(&docs_schema()).expect("bind ok");
        let schema = docs_schema();
        assert_eq!(schema.columns[idx].name, "body");
    }

    #[test]
    fn bind_body_text_column_rejects_missing_body_column() {
        let schema = TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        );
        let err = bind_body_text_column(&schema).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_body_text_column_rejects_non_text_body_column() {
        let schema = TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Vector(3), false),
            ],
        );
        let err = bind_body_text_column(&schema).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // 対象指摘: Cursor Bugbot Medium（PR #1011）。`bind_upsert_form` の NULL 判定が
    // `Value::Vector`／`Value::Text` 以外をすべて NULL 扱いしていたため、
    // `Value::Array`（Issue #888）を持つ NOT NULL 配列列に対する
    // `ON CONFLICT (id) DO UPDATE SET tags = EXCLUDED.tags` が、実際には
    // 配列値が存在するにもかかわらず「NULL を NOT NULL 列へ代入しようとした」
    // として常に拒否されていた。NOT NULL な配列列を対象にした UPSERT が
    // 受理されることを固定する。
    #[test]
    fn bind_upsert_form_accepts_excluded_array_value_for_not_null_array_column() {
        let schema = TableSchema::new(
            "documents",
            vec![ColumnDef::new(
                "tags",
                ColumnType::Array(text_array_ty(8)),
                false,
            )],
        );
        let lookup = FakeCatalog {
            tables: ["documents"].into_iter().collect(),
        };
        let stmt = crate::sql::allowlist::validate_insert(
            "INSERT INTO documents (id, tags) VALUES (1, '{a,b}') \
             ON CONFLICT (id) DO UPDATE SET tags = EXCLUDED.tags \
             USING OPERATION_ID 'op-upsert-array'",
            &lookup,
            crate::recovery::required_op_id::LedgerMode::Ledgered,
        )
        .expect("must pass allowlist");

        let bound =
            bind_insert_form(&stmt, &schema).expect("array EXCLUDED value must not be NULL");
        match bound {
            BoundInsertForm::Upsert(upsert) => {
                assert_eq!(
                    upsert.action,
                    BoundConflictAction::DoUpdate(vec![(0, BoundUpsertValue::Excluded(0))])
                );
            }
            other => panic!("expected BoundInsertForm::Upsert, got {other:?}"),
        }
    }
}
