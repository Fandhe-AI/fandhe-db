//! インタプリタと外界（式の評価・SQL 実行・カタログ型解決）の境界（Issue #2225・SQL-68。
//! 設計は `docs/design/plpgsql-subset.md` D1・D3・D4）。
//!
//! `plpgsql::interp` は制御フローと変数環境だけを持ち、SQL も式も自分では評価しない
//! （第 2 の SQL 実行器を作らない）。式の評価・埋め込み SQL の実行・`%TYPE` 等の型解決は
//! [`PlHost`] へ委ね、core 側の実装（変数値のトークン置換・同じ Tx／RLS での実行）は #2226 が
//! 入れる。`EmbeddedSqlChecker`（定義時検証）と同じ型の分業。#2225 の時点ではテスト専用ホスト
//! （`interp_tests`）だけが実装する。メソッドの形は #2226 の要件で調整されうる。
//!
//! ホストは入れ子の関数呼び出し（式の中の PL/pgSQL 関数）で `run_function` を再帰的に呼べるよう、
//! 実行予算 [`super::interp::ExecBudget`] を引数で受け取る（文数・反復・呼び出し深さの合算）。

use crate::catalog::ColumnType;
use crate::plpgsql::ast::{DeclType, EmbeddedSql, SqlExpr, VarRef};
use crate::plpgsql::interp::ExecBudget;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;

/// 型付きのスカラー値（NULL も型を持つ）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlScalar {
    pub(crate) cell: Cell,
    pub(crate) ty: ColumnType,
}

impl PlScalar {
    pub(crate) fn new(cell: Cell, ty: ColumnType) -> Self {
        Self { cell, ty }
    }

    pub(crate) fn null(ty: ColumnType) -> Self {
        Self {
            cell: Cell::Null,
            ty,
        }
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(self.cell, Cell::Null)
    }
}

/// レコード変数の値。`fields` が `None` なら未代入（フィールド参照は `55000`）。
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct PlRecord {
    pub(crate) fields: Option<Vec<(String, PlScalar)>>,
}

/// `%TYPE`・`%ROWTYPE`・`RECORD` を呼び出し時に解決した型。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ResolvedType {
    Scalar(ColumnType),
    /// `fields` が `Some` なら行型（`%ROWTYPE`）、`None` なら `RECORD`（代入時に形が決まる）。
    Record(Option<Vec<(String, ColumnType)>>),
}

/// 埋め込み SQL に求める結果の行数（実体化の上限を呼び出し側が指定する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowDemand {
    /// 行を使わない（`PERFORM`・DML・`INTO` なしの `EXECUTE`）。
    None,
    /// 最大 2 行（`INTO` の `STRICT` 判定。2 行目の有無だけを知る）。
    AtMostTwo,
    /// 全行（`FOR`・`RETURN QUERY`。実体化は `--max-result-rows` 以下）。
    All,
}

/// 埋め込み SQL の実行結果。
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct SqlOutcome {
    /// 列の名前と型（行が 0 でも与える）。
    pub(crate) columns: Vec<(String, ColumnType)>,
    pub(crate) rows: Vec<Vec<Cell>>,
    /// 影響行数（DML）または返した行数。
    pub(crate) row_count: u64,
    /// データを書き換える文か（`RETURNING INTO` が 2 行以上で `P0003` になる判定に使う）。
    pub(crate) dml: bool,
}

/// 変数の値を読むだけの view（ホストが式・SQL の変数参照を値へ引くのに使う）。
pub(crate) trait VarLookup {
    /// `Var`（スカラー変数）または `Field`（レコードのフィールド）の値。未代入のレコードの
    /// フィールドは `55000`、存在しないフィールドは `42703`。
    fn scalar(&self, r: &VarRef) -> Result<&PlScalar, SqlSurfaceError>;
}

/// インタプリタが外界へ要求する操作。エラーは固定文言の `SqlSurfaceError` で返す。
pub(crate) trait PlHost {
    /// 値の式を評価する。
    fn eval_value(
        &mut self,
        expr: &SqlExpr,
        env: &dyn VarLookup,
        budget: &mut ExecBudget,
    ) -> Result<PlScalar, SqlSurfaceError>;

    /// 条件式を評価する（NULL は `None`）。
    fn eval_cond(
        &mut self,
        expr: &SqlExpr,
        env: &dyn VarLookup,
        budget: &mut ExecBudget,
    ) -> Result<Option<bool>, SqlSurfaceError>;

    /// 単純形 `CASE` の比較（`a = b`。どちらかが NULL なら `None`）。インタプリタが `=` の SQL を
    /// 組み立てないために置く。
    fn values_equal(&mut self, a: &PlScalar, b: &PlScalar)
        -> Result<Option<bool>, SqlSurfaceError>;

    /// `SELECT`／`PERFORM`／DML／`FOR` の問い合わせ／`RETURN QUERY` を実行する（`INTO` 除去済み）。
    fn exec_sql(
        &mut self,
        sql: &EmbeddedSql,
        env: &dyn VarLookup,
        demand: RowDemand,
        budget: &mut ExecBudget,
    ) -> Result<SqlOutcome, SqlSurfaceError>;

    /// `EXECUTE` の動的 SQL を実行する（許可リスト検証はホストの責務）。
    fn exec_dynamic(
        &mut self,
        query: &str,
        using: &[PlScalar],
        demand: RowDemand,
        budget: &mut ExecBudget,
    ) -> Result<SqlOutcome, SqlSurfaceError>;

    /// `%TYPE`・`%ROWTYPE`・`RECORD` の呼び出し時の解決（D4）。`Named` はインタプリタ自身が解く。
    fn resolve_type(&mut self, ty: &DeclType) -> Result<ResolvedType, SqlSurfaceError>;
}
