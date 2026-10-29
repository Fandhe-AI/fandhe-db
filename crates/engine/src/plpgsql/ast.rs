//! PL/pgSQL 構文解析器の AST 型と解析オプション（Issue #2224・SQL-68）。
//!
//! `plpgsql::parser` が生成し、#2225 の木構造インタプリタが辿る。変数はフラットな表
//! （[`Body::vars`]）に置き、AST からは [`VarId`] で参照する（#2225 の変数環境の添字に
//! そのまま使える）。埋め込み SQL・式は `sql::lexer::Token` 列で保持し、実行時（#2226）に
//! 変数をトークンへ置換して既存の許可リスト解析へ渡す。

use crate::plpgsql_limits::{HARD_MAX_PLPGSQL_BODY_BYTES, HARD_MAX_PLPGSQL_NESTING_DEPTH};
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::lexer::Token;

/// [`Body::vars`] の添字。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct VarId(pub(crate) u32);

/// 関数の戻り値の形（`RETURN` 系の検査に使う。DO ブロックは `Void`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReturnShape {
    Void,
    Scalar,
    SetOf,
    Trigger,
}

/// 関数引数の宣言（名前は任意。`$n` では常に参照できる）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParamDecl {
    pub(crate) name: Option<String>,
    pub(crate) ty: SqlColumnTypeName,
}

/// 解析オプション。上限は既定でハード上限と同値で、[`Self::with_limits`] は下げる方向にだけ効く。
#[derive(Debug, Clone)]
pub(crate) struct ParseOptions {
    pub(crate) params: Vec<ParamDecl>,
    pub(crate) returns: ReturnShape,
    pub(crate) max_nesting_depth: usize,
    pub(crate) max_body_bytes: usize,
}

impl ParseOptions {
    /// ハード上限で解析するオプションを作る。
    pub(crate) fn new(params: Vec<ParamDecl>, returns: ReturnShape) -> Self {
        Self {
            params,
            returns,
            max_nesting_depth: HARD_MAX_PLPGSQL_NESTING_DEPTH,
            max_body_bytes: HARD_MAX_PLPGSQL_BODY_BYTES,
        }
    }

    /// 上限を下げる（ハード上限へ飽和させる。#2225 が `PlpgsqlLimits` の値を渡す入口）。
    pub(crate) fn with_limits(mut self, max_nesting_depth: usize, max_body_bytes: usize) -> Self {
        self.max_nesting_depth = max_nesting_depth.min(HARD_MAX_PLPGSQL_NESTING_DEPTH);
        self.max_body_bytes = max_body_bytes.min(HARD_MAX_PLPGSQL_BODY_BYTES);
        self
    }
}

/// 変数の出自。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VarKind {
    /// 関数引数（1 始まりの `$n`）。
    Param(u16),
    /// `DECLARE` で宣言した変数。
    Local,
    /// 整数 `FOR` のループ変数（暗黙宣言）。
    LoopInt,
    /// 問い合わせ `FOR` が暗黙宣言するレコード変数。
    LoopRecord,
    /// 暗黙宣言の `FOUND`。
    Found,
}

/// 変数の宣言型。`%TYPE`・`%ROWTYPE`・`RECORD` の解決は呼び出し時（#2225）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeclType {
    Named(SqlColumnTypeName),
    /// `x%TYPE`（`path` は `.` 区切りの 1〜3 要素。変数・`表.列`・`スキーマ.表.列`。解決は呼び出し時）。
    PercentType {
        path: Vec<String>,
    },
    /// `表%ROWTYPE`（`path` は 1〜2 要素）。
    PercentRowType {
        path: Vec<String>,
    },
    Record,
}

/// 変数表の 1 要素。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VarDecl {
    /// 小文字へ正規化した名前（引数は無名なら空）。
    pub(crate) name: String,
    pub(crate) kind: VarKind,
    pub(crate) constant: bool,
    pub(crate) not_null: bool,
    pub(crate) ty: DeclType,
    pub(crate) default: Option<SqlExpr>,
    /// 宣言時のスコープの深さ（0 が関数全体の外側スコープ）。
    pub(crate) scope_depth: usize,
}

/// 変数参照（埋め込み SQL のトークン位置に対応する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VarRef {
    Var(VarId),
    /// `rec.field`。
    Field(VarId, String),
}

/// 代入・`INTO` の対象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssignTarget {
    Var(VarId),
    Field(VarId, String),
}

/// 式（`SELECT <expr>` として検証する SQL 式）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SqlExpr {
    pub(crate) tokens: Vec<Token>,
    /// （トークン位置, 解決済みの変数参照）。`Token::Param` もここへ含める。
    pub(crate) var_refs: Vec<(usize, VarRef)>,
    /// 真偽値を取る条件式（`IF`・`WHILE`・検索形 `CASE`・`EXIT/CONTINUE WHEN`）か。許可リスト側は
    /// 比較・論理演算を `WHERE` の中でしか受理しないため、検証では `SELECT 1 WHERE <cond>` の形にする。
    pub(crate) cond: bool,
}

/// 埋め込み SQL 文の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmbeddedKind {
    Select,
    /// `PERFORM`（先頭の `SELECT` へ読み替え済み）。
    Perform,
    Insert,
    Update,
    Delete,
}

/// `INTO [STRICT] targets`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntoClause {
    pub(crate) strict: bool,
    pub(crate) targets: Vec<AssignTarget>,
}

/// 埋め込み SQL 文（`INTO` 句は除去済み）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EmbeddedSql {
    pub(crate) kind: EmbeddedKind,
    pub(crate) tokens: Vec<Token>,
    pub(crate) var_refs: Vec<(usize, VarRef)>,
    pub(crate) into: Option<IntoClause>,
}

/// `GET DIAGNOSTICS` の項目（初版は `ROW_COUNT` のみ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagItem {
    RowCount,
}

/// 文。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stmt {
    Block(Block),
    Assign {
        target: AssignTarget,
        expr: SqlExpr,
    },
    If {
        branches: Vec<(SqlExpr, Vec<Stmt>)>,
        else_: Option<Vec<Stmt>>,
    },
    /// `selector` が `Some` なら単純形（各 `WHEN` は値のリスト）、`None` なら検索形（各 `WHEN` は 1 条件）。
    Case {
        selector: Option<SqlExpr>,
        whens: Vec<(Vec<SqlExpr>, Vec<Stmt>)>,
        else_: Option<Vec<Stmt>>,
    },
    Loop {
        label: Option<String>,
        body: Vec<Stmt>,
    },
    While {
        label: Option<String>,
        cond: SqlExpr,
        body: Vec<Stmt>,
    },
    ForInt {
        label: Option<String>,
        var: VarId,
        reverse: bool,
        lower: SqlExpr,
        upper: SqlExpr,
        step: Option<SqlExpr>,
        body: Vec<Stmt>,
    },
    ForQuery {
        label: Option<String>,
        targets: Vec<AssignTarget>,
        query: EmbeddedSql,
        body: Vec<Stmt>,
    },
    Exit {
        label: Option<String>,
        when: Option<SqlExpr>,
    },
    Continue {
        label: Option<String>,
        when: Option<SqlExpr>,
    },
    Return(Option<SqlExpr>),
    ReturnNext(Option<SqlExpr>),
    ReturnQuery(EmbeddedSql),
    Perform(EmbeddedSql),
    Sql(EmbeddedSql),
    Execute {
        query: SqlExpr,
        into: Option<IntoClause>,
        using: Vec<SqlExpr>,
    },
    GetDiagnostics {
        items: Vec<(AssignTarget, DiagItem)>,
    },
    Null,
}

/// `[<<label>>] [DECLARE ...] BEGIN ... END`。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Block {
    pub(crate) label: Option<String>,
    /// このブロックの `DECLARE` で宣言した変数。
    pub(crate) decls: Vec<VarId>,
    pub(crate) stmts: Vec<Stmt>,
}

/// 解析結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Body {
    pub(crate) block: Block,
    pub(crate) vars: Vec<VarDecl>,
}
