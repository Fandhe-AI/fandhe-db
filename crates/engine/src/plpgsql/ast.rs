//! PL/pgSQL 構文解析器の AST 型と解析オプション（Issue #2224・SQL-68）。
//!
//! `plpgsql::parser` が生成し、#2225 の木構造インタプリタが辿る。変数はフラットな表
//! （[`Body::vars`]）に置き、AST からは [`VarId`] で参照する（#2225 の変数環境の添字に
//! そのまま使える）。埋め込み SQL・式は `sql::lexer::Token` 列で保持し、実行時（#2226）に
//! 変数をトークンへ置換して既存の許可リスト解析へ渡す。

use std::collections::HashMap;
use std::sync::Arc;

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

/// SQL 構文解析器が記録した列参照の役割（`sql::colref_sink::Role`）。#2226 の呼び出し時の置換が
/// 役割ごとに方法を変える（`OrderKey`／`GroupKey` は序数参照への化け、`SelectItemBare` は出力列名の維持）。
pub(crate) use crate::sql::colref_sink::Role as VarRole;

/// 定義時分類の結果 1 件。`pos` は保存した `tokens` 上の位置、`role` は解析器が確定した役割、
/// `var` は PL/pgSQL のスコープで解決できた変数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VarSite {
    pub(crate) pos: usize,
    pub(crate) role: VarRole,
    pub(crate) var: VarRef,
}

/// スコープの 1 階層（ブロック・ループ）。同名の再登録は後勝ち。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScopeLevel {
    pub(crate) label: Option<String>,
    pub(crate) names: HashMap<String, VarId>,
}

/// 文・式を解析した時点で見えていたスコープ（外側から内側の順）。定義時分類が、解析器の記録を
/// 変数へ解決するために使う。`levels[0]` は引数と `FOUND` の外側スコープ。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScopeView {
    pub(crate) levels: Vec<ScopeLevel>,
}

/// 構文解析中の生きたスコープ列と、保存済みの [`ScopeView`] が共有する名前解決。
/// `record_like` は変数がレコード型（`RECORD`・`%ROWTYPE`）かの判定。
pub(super) fn lookup_var_in(levels: &[ScopeLevel], name: &str) -> Option<VarId> {
    levels.iter().rev().find_map(|s| s.names.get(name).copied())
}

/// `qualifier.name` の解決。ラベル修飾（`lbl.var`）を先に、次にレコードのフィールド。
pub(super) fn lookup_qualified_in(
    levels: &[ScopeLevel],
    record_like: &dyn Fn(VarId) -> bool,
    qualifier: &str,
    name: &str,
) -> Option<VarRef> {
    let q = qualifier.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    for (idx, s) in levels.iter().enumerate().rev() {
        if s.label.as_deref() == Some(q.as_str()) {
            if let Some(id) = s.names.get(&n) {
                return Some(VarRef::Var(*id));
            }
            // 最外ブロック（`levels[1]`。`levels[0]` は引数と `FOUND` の外側スコープ）のラベルでも、
            // PostgreSQL と同様に引数と `FOUND` を修飾参照できる。
            if idx == 1 {
                if let Some(id) = levels.first().and_then(|o| o.names.get(&n)) {
                    return Some(VarRef::Var(*id));
                }
            }
        }
    }
    let id = lookup_var_in(levels, &q)?;
    if record_like(id) {
        Some(VarRef::Field(id, n))
    } else {
        None
    }
}

impl ScopeView {
    pub(crate) fn lookup_var(&self, name: &str) -> Option<VarId> {
        lookup_var_in(&self.levels, name)
    }

    pub(crate) fn lookup_qualified(
        &self,
        record_like: &dyn Fn(VarId) -> bool,
        qualifier: &str,
        name: &str,
    ) -> Option<VarRef> {
        lookup_qualified_in(&self.levels, record_like, qualifier, name)
    }

    /// SQL 構文解析器へ渡す、スコープ内の名前の集合（小文字。変数名とブロック・ループのラベル）。
    pub(crate) fn known_names(&self) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for l in &self.levels {
            if let Some(label) = &l.label {
                out.insert(label.clone());
            }
            out.extend(l.names.keys().cloned());
        }
        out
    }
}

/// 保存するスコープの共有ハンドル（同じスコープの文どうしで共有する）。
pub(crate) type ScopeRef = Arc<ScopeView>;

/// 代入・`INTO` の対象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssignTarget {
    Var(VarId),
    Field(VarId, String),
}

/// 式（`SELECT <expr>` として検証する SQL 式）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SqlExpr {
    /// 正規化後のトークン列（定義時分類の後。それまでは切り出した原文のまま）。
    pub(crate) tokens: Vec<Token>,
    /// 定義時分類（`embedded::validate_embedded`）が埋める変数参照。`Token::Param`（`$n`）も含める。
    pub(crate) var_refs: Vec<VarSite>,
    /// 解析時点のスコープ（定義時分類が解析器の記録を変数へ解決するために使う）。
    pub(crate) scope: ScopeRef,
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
    /// 文種。先頭の語から仮置きし、`WITH` 始まりは定義時分類が解析木から確定する。
    pub(crate) kind: EmbeddedKind,
    /// 正規化後（行数制限句・名前付き `WINDOW`。`INTO` 除去後）のトークン列。定義時分類の前は原文のまま。
    pub(crate) tokens: Vec<Token>,
    /// 定義時分類が埋める変数参照（位置・役割・変数）。
    pub(crate) var_refs: Vec<VarSite>,
    pub(crate) into: Option<IntoClause>,
    /// 書き込みを含むか（`kind` が DML、またはデータ変更 CTE を含む。ADR D5）。定義時分類が確定する。
    pub(crate) writes: bool,
    /// 解析時点のスコープ。
    pub(crate) scope: ScopeRef,
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
