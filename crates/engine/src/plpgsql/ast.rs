//! PL/pgSQL 構文解析器の AST 型と解析オプション（Issue #2224・SQL-68）。
//!
//! `plpgsql::parser` が生成し、#2225 の木構造インタプリタが辿る。変数はフラットな表
//! （[`Body::vars`]）に置き、AST からは [`VarId`] で参照する（#2225 の変数環境の添字に
//! そのまま使える）。埋め込み SQL・式は `sql::lexer::Token` 列で保持し、実行時（#2226）に
//! 変数をトークンへ置換して既存の許可リスト解析へ渡す。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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

/// スコープの登録簿の 1 件（宣言 1 個）。
#[derive(Debug)]
struct ScopeEntry {
    id: VarId,
    /// 宣言したスコープ階層（登録簿内で一意な番号）。
    level: u32,
}

/// 構文解析 1 回分のスコープの登録簿。宣言を追記専用で全件ここへ 1 度だけ持ち、文・式ごとのスコープは
/// [`ScopeView`]（階層の番号列と宣言数の上限）で表す。宣言ごとに名前表全体を複製しないため、保持する
/// 名前の総数は宣言数に比例する（既定値付きの宣言が数千あっても二乗にならない）。
#[derive(Debug, Default)]
pub(crate) struct ScopeRegistry {
    entries: Vec<ScopeEntry>,
    /// 名前から `entries` の添字（昇順）への表。
    by_name: HashMap<String, Vec<u32>>,
    /// 階層ごとのラベル（階層の番号で引く）。
    labels: Vec<Option<String>>,
}

impl ScopeRegistry {
    /// 新しい階層を作って番号を返す。
    pub(super) fn new_level(&mut self, label: Option<String>) -> u32 {
        self.labels.push(label);
        u32::try_from(self.labels.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }

    /// 階層 `level` へ宣言を追記する。
    pub(super) fn add(&mut self, level: u32, name: String, id: VarId) {
        let idx = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
        self.by_name.entry(name.clone()).or_default().push(idx);
        self.entries.push(ScopeEntry { id, level });
    }

    /// 登録済みの宣言の総数。
    #[cfg(test)]
    pub(super) fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// 階層 `level` で、宣言数の上限 `limit` までに見える名前（同名は後勝ち）。
    fn lookup_in_level(&self, level: u32, limit: u32, name: &str) -> Option<VarId> {
        self.by_name
            .get(name)?
            .iter()
            .rev()
            .filter(|i| **i < limit)
            .find_map(|i| {
                let e = self.entries.get(*i as usize)?;
                (e.level == level).then_some(e.id)
            })
    }
}

/// 登録簿の共有ハンドル（構文解析が書き、各文のスコープが読む）。
pub(super) type SharedRegistry = Arc<RwLock<ScopeRegistry>>;

/// 文・式を解析した時点で見えていたスコープ。定義時分類が、解析器の記録を変数へ解決するために使う。
/// `chain` は外側から内側の階層の番号（`chain[0]` は引数と `FOUND` の外側スコープ）、`limit` は
/// その時点の宣言数（それ以降の宣言は見えない）。文ごとの大きさは階層の深さに比例する。
#[derive(Clone)]
pub(crate) struct ScopeView {
    reg: SharedRegistry,
    chain: Vec<u32>,
    limit: u32,
}

impl std::fmt::Debug for ScopeView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeView")
            .field("chain", &self.chain)
            .field("limit", &self.limit)
            .finish()
    }
}

impl PartialEq for ScopeView {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.reg, &other.reg) && self.chain == other.chain && self.limit == other.limit
    }
}

impl ScopeView {
    /// 現在の登録簿と階層の並びからスコープを作る。
    pub(super) fn new(reg: &SharedRegistry, chain: &[u32]) -> Self {
        let limit = reg
            .read()
            .map(|r| u32::try_from(r.entries.len()).unwrap_or(u32::MAX))
            .unwrap_or(0);
        Self {
            reg: Arc::clone(reg),
            chain: chain.to_vec(),
            limit,
        }
    }

    /// 保持している宣言の総数（登録簿は構文解析 1 回で共有。テストが線形性の確認に使う）。
    #[cfg(test)]
    pub(crate) fn registry_entry_count(&self) -> usize {
        self.reg.read().map_or(0, |r| r.entry_count())
    }

    /// 同じ登録簿を共有しているか（テスト用）。
    #[cfg(test)]
    pub(crate) fn shares_registry_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.reg, &other.reg)
    }

    /// このスコープの階層の深さ（テスト用）。
    #[cfg(test)]
    pub(crate) fn chain_len(&self) -> usize {
        self.chain.len()
    }

    pub(crate) fn lookup_var(&self, name: &str) -> Option<VarId> {
        let reg = self.reg.read().ok()?;
        self.chain
            .iter()
            .rev()
            .find_map(|l| reg.lookup_in_level(*l, self.limit, name))
    }

    /// `qualifier.name` の解決。ラベル修飾（`lbl.var`）を先に、次にレコードのフィールド。
    pub(crate) fn lookup_qualified(
        &self,
        record_like: &dyn Fn(VarId) -> bool,
        qualifier: &str,
        name: &str,
    ) -> Option<VarRef> {
        let q = qualifier.to_ascii_lowercase();
        let n = name.to_ascii_lowercase();
        {
            let reg = self.reg.read().ok()?;
            for (idx, level) in self.chain.iter().enumerate().rev() {
                let label = reg.labels.get(*level as usize).and_then(|l| l.as_deref());
                if label == Some(q.as_str()) {
                    if let Some(id) = reg.lookup_in_level(*level, self.limit, &n) {
                        return Some(VarRef::Var(id));
                    }
                    // 最外ブロック（`chain[1]`。`chain[0]` は引数と `FOUND` の外側スコープ）のラベルでも、
                    // PostgreSQL と同様に引数と `FOUND` を修飾参照できる。
                    if idx == 1 {
                        if let Some(id) = self
                            .chain
                            .first()
                            .and_then(|o| reg.lookup_in_level(*o, self.limit, &n))
                        {
                            return Some(VarRef::Var(id));
                        }
                    }
                }
            }
        }
        let id = self.lookup_var(&q)?;
        record_like(id).then_some(VarRef::Field(id, n))
    }

    /// 変数名またはラベル名として見える名前か（`name` は小文字）。SQL 構文解析器へ渡すスコープ内の名前の
    /// 判定に使う。
    pub(crate) fn is_known(&self, name: &str) -> bool {
        if self.lookup_var(name).is_some() {
            return true;
        }
        self.reg.read().is_ok_and(|reg| {
            self.chain
                .iter()
                .any(|l| reg.labels.get(*l as usize).and_then(|x| x.as_deref()) == Some(name))
        })
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

impl Body {
    /// 本体がデータを書き得るか（Issue #2226・ADR D5）。書き込みを含む埋め込み SQL
    /// （`EmbeddedSql::writes`）または `EXECUTE`（動的 SQL は静的に判定できないため保守的に書き得る
    /// とみなす）を含めば真。入れ子のブロック・分岐・ループを辿る。呼び出し元（#2217 が揮発性属性と
    /// 組み合わせて `PlCallOptions::may_write` を決める）が事前の経路決定に使う。
    pub(crate) fn may_write(&self) -> bool {
        fn any_write(stmts: &[Stmt]) -> bool {
            stmts.iter().any(|s| match s {
                Stmt::Block(b) => any_write(&b.stmts),
                Stmt::If { branches, else_ } => {
                    branches.iter().any(|(_, st)| any_write(st))
                        || else_.as_deref().is_some_and(any_write)
                }
                Stmt::Case { whens, else_, .. } => {
                    whens.iter().any(|(_, st)| any_write(st))
                        || else_.as_deref().is_some_and(any_write)
                }
                Stmt::Loop { body, .. } | Stmt::While { body, .. } | Stmt::ForInt { body, .. } => {
                    any_write(body)
                }
                Stmt::ForQuery { query, body, .. } => query.writes || any_write(body),
                Stmt::ReturnQuery(q) | Stmt::Perform(q) | Stmt::Sql(q) => q.writes,
                Stmt::Execute { .. } => true,
                Stmt::Assign { .. }
                | Stmt::Exit { .. }
                | Stmt::Continue { .. }
                | Stmt::Return(_)
                | Stmt::ReturnNext(_)
                | Stmt::GetDiagnostics { .. }
                | Stmt::Null => false,
            })
        }
        any_write(&self.block.stmts)
    }
}
