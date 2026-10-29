//! PL/pgSQL の再帰下降パーサー（Issue #2224・SQL-68。設計は `docs/design/plpgsql-subset.md`）。
//!
//! 制御構造（ブロック・`DECLARE`・代入・`IF`／`CASE`・ループ・`EXIT`／`CONTINUE`・`RETURN`・
//! `EXECUTE`・`GET DIAGNOSTICS`）だけを自前で解析する。式と埋め込み SQL は括弧の深さ 0 の終端
//! （`;`・`THEN`・`LOOP` 等）で原文の範囲を切り出し、`sql::lexer::tokenize_with_params` で SQL
//! トークンにして `plpgsql::embedded` へ渡す（PL/pgSQL 独自の式パーサーは持たない）。
//! 変数・ラベルのスコープはここで解決し、範囲外の構文は別の意味へ読み替えず `0A000` で拒否する
//! （fail-closed）。呼び出し元は `plpgsql::parse_body`（#2217／#2225 が結線）。
//! 受信データの経路なので `unwrap`／`expect`／添字アクセスを使わない。再帰は入れ子の深さ上限で有界。

use crate::plpgsql_limits::MAX_PLPGSQL_VARS_PER_STATEMENT;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::{parse_column_type_name, SqlColumnTypeName};
use crate::sql::lexer::{tokenize_with_params, Keyword, Token};

use super::ast::{
    AssignTarget, Block, Body, DeclType, DiagItem, EmbeddedKind, EmbeddedSql, IntoClause,
    ParseOptions, ReturnShape, SqlExpr, Stmt, VarDecl, VarId, VarKind, VarRef,
};
use super::embedded::{
    classify_query, classify_with, has_write_cte, is_ident_position_with, reject_forbidden_clauses,
    split_into, IdentCtx, RawInto,
};
use super::lexer::{scan, TokKind};
use super::{oos_err, syn_err, too_big_err};

/// 変数・ラベル名に使えない PL/pgSQL の予約語（PostgreSQL より厳しい。fail-closed）。
const RESERVED: &[&str] = &[
    "begin", "end", "if", "loop", "elsif", "elseif", "then", "declare", "else", "case", "when",
    "while", "for", "exit", "continue", "return", "execute", "perform", "null", "in",
];

/// SQL 字句解析が `Token::Keyword` 化する語（変数名にできない）。
const SQL_KEYWORD_NAMES: &[&str] = &["select", "from", "where", "and", "order", "by", "limit"];

/// 変数・パラメータ・ループ変数の名前として宣言できない語（引用付き `"..."` でも拒否する）。
///
/// 解析器・`embedded` が識別子の位置判定や文種の判定で「構造語」「リテラル語」として特別扱いする語を
/// この 1 か所に集約する。同名の変数を許すと、`UPDATE t SET x = 1` の `SET` や `IF true` の `true` が
/// 変数参照に読み替わり正しい本体を拒否・誤実行しうる。位置ヒューリスティックで 1 語ずつ塞がず、
/// 曖昧な名前は宣言時に `0A000` で拒否する（fail-closed。設計は `docs/design/plpgsql-subset.md`）。
/// 比較は ASCII の大小を無視する。`declare`・`init_outer_scope` が参照する（ラベルは識別子位置に
/// 現れず `lbl.name` の修飾子としてだけ使うため対象外）。
pub(super) const UNSUPPORTED_NAMES: &[&str] = &[
    // リテラル・値関数の語（`QuotedNull`／`QuotedBoolWord` と未引用の `Ident` の衝突を避ける）。
    "true",
    "false",
    "null",
    "default",
    "array",
    "current_timestamp",
    "current_date",
    "current_time",
    "localtimestamp",
    "localtime",
    // 文頭・文の構造語。
    "insert",
    "update",
    "delete",
    "values",
    "with",
    "get",
    "set",
    "into",
    "using",
    "strict",
    "reverse",
    "exception",
    "operation_id",
    "partitioned",
    "first",
    "next",
    "query",
    "stacked",
    "current",
    "diagnostics",
    "row_count",
    "alias",
    "cursor",
    "scroll",
    "no",
    "constant",
    "collate",
    // 埋め込み SQL の句・演算子の語（`embedded` の識別子位置判定が文脈語として扱う）。
    "group",
    "having",
    "returning",
    "offset",
    "union",
    "intersect",
    "except",
    "join",
    "on",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "outer",
    "lateral",
    "tablesample",
    "overriding",
    "window",
    "fetch",
    "rows",
    "only",
    "asc",
    "desc",
    "distinct",
    "table",
    "constraint",
    "conflict",
    "materialized",
    "as",
    "do",
    "or",
    "not",
];

/// 名前が [`UNSUPPORTED_NAMES`] に含まれるなら `0A000`（エラー文言に名前は含めない）。
fn check_supported_name(name: &str) -> Result<(), SqlSurfaceError> {
    if UNSUPPORTED_NAMES
        .iter()
        .any(|w| w.eq_ignore_ascii_case(name))
    {
        return Err(oos_err(
            "this name cannot be used for a PL/pgSQL variable or parameter",
        ));
    }
    Ok(())
}

/// 範囲外（`0A000`）の文頭の語。
const OUT_OF_SCOPE_STARTS: &[&str] = &[
    "RAISE",
    "ASSERT",
    "OPEN",
    "FETCH",
    "MOVE",
    "CLOSE",
    "FOREACH",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "GRANT",
    "REVOKE",
    "COMMENT",
    "REFRESH",
    "REINDEX",
    "CLUSTER",
    "VACUUM",
    "ANALYZE",
    "LOCK",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
    "START",
    "SET",
    "RESET",
    "CALL",
    "COPY",
    "EXPLAIN",
    "SHOW",
    "PREPARE",
    "DEALLOCATE",
    "DISCARD",
    "LISTEN",
    "NOTIFY",
    "MERGE",
    "IMPORT",
    "ABORT",
];

/// 多相型の型名（`anycompatible*` は前方一致で判定する）。
const POLYMORPHIC: &[&str] = &[
    "anyelement",
    "anyarray",
    "anynonarray",
    "anyenum",
    "anyrange",
    "anymultirange",
];

struct Scope {
    label: Option<String>,
    names: Vec<(String, VarId)>,
}

struct Ctrl {
    label: Option<String>,
    is_loop: bool,
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<super::lexer::Tok>,
    pos: usize,
    opts: &'a ParseOptions,
    vars: Vec<VarDecl>,
    scopes: Vec<Scope>,
    ctrl: Vec<Ctrl>,
    depth: usize,
}

/// 入口。本体長はスキャンより前に検査する（無制限確保の禁止）。
pub(super) fn parse(src: &str, opts: &ParseOptions) -> Result<Body, SqlSurfaceError> {
    if src.len() > opts.max_body_bytes {
        return Err(too_big_err("PL/pgSQL body exceeds the size limit"));
    }
    let toks = scan(src)?;
    let mut p = Parser {
        src,
        toks,
        pos: 0,
        opts,
        vars: Vec::new(),
        scopes: Vec::new(),
        ctrl: Vec::new(),
        depth: 0,
    };
    p.init_outer_scope()?;
    let block = p.parse_top()?;
    Ok(Body {
        block,
        vars: p.vars,
    })
}

/// 全体を囲む外側の括弧を剥がす（`(SELECT ...)` → `SELECT ...`。定義時検証は SELECT 始まりを要求する）。
fn strip_outer_parens(mut t: Vec<Token>) -> Vec<Token> {
    loop {
        if !matches!(t.first(), Some(Token::Punct('(')))
            || !matches!(t.last(), Some(Token::Punct(')')))
        {
            return t;
        }
        let mut d = 0usize;
        for (i, x) in t.iter().enumerate() {
            match x {
                Token::Punct('(') => d = d.saturating_add(1),
                Token::Punct(')') => {
                    d = d.saturating_sub(1);
                    if d == 0 && i.saturating_add(1) != t.len() {
                        return t;
                    }
                }
                _ => {}
            }
        }
        t.remove(0);
        t.pop();
    }
}

fn sql_tokens(text: &str) -> Result<Vec<Token>, SqlSurfaceError> {
    let t = tokenize_with_params(text)
        .map_err(|_| syn_err("invalid token in an embedded SQL fragment of the PL/pgSQL body"))?;
    if t.is_empty() {
        return Err(syn_err("empty SQL fragment in PL/pgSQL body"));
    }
    Ok(t)
}

impl<'a> Parser<'a> {
    // ---- 基本操作 ----

    fn kind(&self, off: usize) -> Option<&TokKind> {
        self.toks.get(self.pos.saturating_add(off)).map(|t| &t.kind)
    }

    fn is_kw(&self, off: usize, kw: &str) -> bool {
        matches!(self.kind(off), Some(TokKind::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    fn is_punct(&self, off: usize, c: char) -> bool {
        matches!(self.kind(off), Some(TokKind::Punct(p)) if *p == c)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(0, kw) {
            self.pos = self.pos.saturating_add(1);
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), SqlSurfaceError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(syn_err(format!("expected {kw} in PL/pgSQL body")))
        }
    }

    fn expect_punct(&mut self, c: char) -> Result<(), SqlSurfaceError> {
        if self.is_punct(0, c) {
            self.pos = self.pos.saturating_add(1);
            Ok(())
        } else {
            Err(syn_err(format!("expected '{c}' in PL/pgSQL body")))
        }
    }

    fn eat_assign_op(&mut self) -> bool {
        if matches!(self.kind(0), Some(TokKind::Assign)) || self.is_punct(0, '=') {
            self.pos = self.pos.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// 入れ子を 1 段深くする。上限超過は `54001`（再帰スタックの保護も兼ねる）。
    fn enter(&mut self) -> Result<(), SqlSurfaceError> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > self.opts.max_nesting_depth {
            return Err(SqlSurfaceError::StatementTooComplex {
                detail: "PL/pgSQL nesting depth exceeds the limit",
            });
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// 名前（変数・ラベル）を 1 個読む。小文字へ正規化する。未引用の予約語は拒否する。
    fn take_name(&mut self) -> Result<String, SqlSurfaceError> {
        let name = match self.kind(0) {
            Some(TokKind::Word(w)) => {
                let l = w.to_ascii_lowercase();
                // SQL 側で `Token::Keyword` 化される語（limit/order/by 等）と同名の変数は式中で参照を
                // 解決できないため、宣言時に拒否する（fail-closed）。
                if RESERVED.contains(&l.as_str()) || SQL_KEYWORD_NAMES.contains(&l.as_str()) {
                    return Err(syn_err("reserved word used as a PL/pgSQL name"));
                }
                l
            }
            Some(TokKind::Quoted(q)) => q.to_ascii_lowercase(),
            _ => return Err(syn_err("expected a name in PL/pgSQL body")),
        };
        self.pos = self.pos.saturating_add(1);
        Ok(name)
    }

    /// `name [. name]` を読む。
    fn take_dotted(&mut self) -> Result<(Option<String>, String), SqlSurfaceError> {
        let first = self.take_name()?;
        if self.is_punct(0, '.') {
            self.pos = self.pos.saturating_add(1);
            let second = self.take_name()?;
            Ok((Some(first), second))
        } else {
            Ok((None, first))
        }
    }

    fn span_text(&self, a: usize, b: usize) -> Result<&'a str, SqlSurfaceError> {
        let first = self.toks.get(a);
        let last = b.checked_sub(1).and_then(|l| self.toks.get(l));
        match (first, last) {
            (Some(f), Some(l)) if a < b => self
                .src
                .get(f.start..l.end)
                .ok_or_else(|| syn_err("invalid span in PL/pgSQL body")),
            _ => Err(syn_err("empty expression in PL/pgSQL body")),
        }
    }

    /// `from` 以降で、括弧と SQL の `CASE ... END` の外にある最初の終端を探す。`;` は常に終端。
    /// 終端が無ければ `42601`。
    fn find_end(
        &self,
        from: usize,
        stop: &dyn Fn(&TokKind) -> bool,
    ) -> Result<usize, SqlSurfaceError> {
        let mut paren = 0usize;
        let mut case = 0usize;
        let mut i = from;
        while let Some(t) = self.toks.get(i) {
            match &t.kind {
                TokKind::Punct('(') | TokKind::Punct('[') => paren = paren.saturating_add(1),
                TokKind::Punct(')') | TokKind::Punct(']') => {
                    if paren == 0 {
                        return Err(syn_err("unbalanced parenthesis in PL/pgSQL body"));
                    }
                    paren -= 1;
                }
                TokKind::Word(w) if w.eq_ignore_ascii_case("CASE") => case = case.saturating_add(1),
                TokKind::Word(w) if case > 0 && w.eq_ignore_ascii_case("END") => case -= 1,
                _ => {}
            }
            if paren == 0 && ((case == 0 && stop(&t.kind)) || matches!(t.kind, TokKind::Punct(';')))
            {
                return Ok(i);
            }
            i = i.saturating_add(1);
        }
        Err(syn_err("unexpected end of PL/pgSQL body"))
    }

    /// `[a, e)` を括弧の深さ 0 のカンマで分割する。
    fn split_commas(&self, a: usize, e: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut paren = 0usize;
        let mut case = 0usize;
        let mut start = a;
        let mut i = a;
        while i < e {
            match self.toks.get(i).map(|t| &t.kind) {
                Some(TokKind::Punct('(')) | Some(TokKind::Punct('[')) => {
                    paren = paren.saturating_add(1)
                }
                Some(TokKind::Punct(')')) | Some(TokKind::Punct(']')) => {
                    paren = paren.saturating_sub(1)
                }
                Some(TokKind::Word(w)) if w.eq_ignore_ascii_case("CASE") => {
                    case = case.saturating_add(1)
                }
                Some(TokKind::Word(w)) if case > 0 && w.eq_ignore_ascii_case("END") => case -= 1,
                Some(TokKind::Punct(',')) if paren == 0 && case == 0 => {
                    out.push((start, i));
                    start = i.saturating_add(1);
                }
                _ => {}
            }
            i = i.saturating_add(1);
        }
        out.push((start, e));
        out
    }

    // ---- 変数・スコープ ----

    fn init_outer_scope(&mut self) -> Result<(), SqlSurfaceError> {
        let mut scope = Scope {
            label: None,
            names: Vec::new(),
        };
        for (i, p) in self.opts.params.iter().enumerate() {
            let n = u16::try_from(i.saturating_add(1))
                .map_err(|_| too_big_err("too many parameters for a PL/pgSQL function"))?;
            let id = VarId(u32::from(n) - 1);
            let name = p.name.clone().unwrap_or_default().to_ascii_lowercase();
            self.vars.push(VarDecl {
                name: name.clone(),
                kind: VarKind::Param(n),
                constant: false,
                not_null: false,
                ty: DeclType::Named(p.ty.clone()),
                default: None,
                scope_depth: 0,
            });
            if !name.is_empty() {
                check_supported_name(&name)?;
                scope.names.push((name, id));
            }
        }
        let found = VarId(u32::try_from(self.vars.len()).unwrap_or(u32::MAX));
        self.vars.push(VarDecl {
            name: "found".to_string(),
            kind: VarKind::Found,
            constant: true,
            not_null: false,
            ty: DeclType::Named(SqlColumnTypeName::Boolean),
            default: None,
            scope_depth: 0,
        });
        scope.names.push(("found".to_string(), found));
        self.scopes.push(scope);
        Ok(())
    }

    fn declare(
        &mut self,
        name: String,
        kind: VarKind,
        ty: DeclType,
        constant: bool,
        not_null: bool,
        default: Option<SqlExpr>,
    ) -> Result<VarId, SqlSurfaceError> {
        check_supported_name(&name)?;
        let id = VarId(
            u32::try_from(self.vars.len())
                .map_err(|_| too_big_err("too many PL/pgSQL variables"))?,
        );
        let scope_depth = self.scopes.len().saturating_sub(1);
        self.vars.push(VarDecl {
            name: name.clone(),
            kind,
            constant,
            not_null,
            ty,
            default,
            scope_depth,
        });
        if let Some(s) = self.scopes.last_mut() {
            s.names.push((name, id));
        }
        Ok(id)
    }

    fn declared_in_current_scope(&self, name: &str) -> bool {
        self.scopes
            .last()
            .is_some_and(|s| s.names.iter().any(|(n, _)| n == name))
    }

    fn lookup_var(&self, name: &str) -> Option<VarId> {
        self.scopes.iter().rev().find_map(|s| {
            s.names
                .iter()
                .rev()
                .find(|(n, _)| n == name)
                .map(|(_, id)| *id)
        })
    }

    fn var(&self, id: VarId) -> Option<&VarDecl> {
        self.vars.get(id.0 as usize)
    }

    fn is_record_like(&self, id: VarId) -> bool {
        self.var(id)
            .is_some_and(|v| matches!(v.ty, DeclType::Record | DeclType::PercentRowType { .. }))
    }

    /// `qualifier.name` の解決。ラベル修飾（`lbl.var`）を先に、次にレコードのフィールド。
    fn lookup_qualified(&self, qualifier: &str, name: &str) -> Option<VarRef> {
        let q = qualifier.to_ascii_lowercase();
        let n = name.to_ascii_lowercase();
        for (idx, s) in self.scopes.iter().enumerate().rev() {
            if s.label.as_deref() == Some(q.as_str()) {
                if let Some((_, id)) = s.names.iter().rev().find(|(x, _)| *x == n) {
                    return Some(VarRef::Var(*id));
                }
                // 最外ブロック（`scopes[1]`。`scopes[0]` はパラメータと `FOUND` の外側スコープ）の
                // ラベルでも、PostgreSQL と同様にパラメータと `FOUND` を修飾参照できる。
                if idx == 1 {
                    if let Some((_, id)) = self
                        .scopes
                        .first()
                        .and_then(|o| o.names.iter().rev().find(|(x, _)| *x == n))
                    {
                        return Some(VarRef::Var(*id));
                    }
                }
            }
        }
        let id = self.lookup_var(&q)?;
        if self.is_record_like(id) {
            Some(VarRef::Field(id, n))
        } else {
            None
        }
    }

    /// 代入・`INTO` の対象を解決する。`CONSTANT`・`FOUND` への代入は拒否する。
    fn resolve_target(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<AssignTarget, SqlSurfaceError> {
        let target = match qualifier {
            None => self
                .lookup_var(name)
                .map(AssignTarget::Var)
                .ok_or_else(|| syn_err("assignment target is not a declared variable"))?,
            Some(q) => match self.lookup_qualified(q, name) {
                Some(VarRef::Var(id)) => AssignTarget::Var(id),
                Some(VarRef::Field(id, f)) => AssignTarget::Field(id, f),
                None => return Err(syn_err("assignment target is not a declared variable")),
            },
        };
        let (AssignTarget::Var(id) | AssignTarget::Field(id, _)) = &target;
        if self.var(*id).is_some_and(|v| v.constant) {
            return Err(syn_err("cannot assign to a constant PL/pgSQL variable"));
        }
        Ok(target)
    }

    fn resolve_refs(&self, tokens: &[Token]) -> Result<Vec<(usize, VarRef)>, SqlSurfaceError> {
        let mut refs: Vec<(usize, VarRef)> = Vec::new();
        // 関係リスト位置は文ごとに 1 回だけ求め、識別子ごとの再走査（二乗コスト）を避ける。
        let slots = IdentCtx::new(tokens);
        for (i, t) in tokens.iter().enumerate() {
            match t {
                Token::Param(n) => {
                    let idx = usize::from(*n);
                    if idx == 0 || idx > self.opts.params.len() {
                        return Err(SqlSurfaceError::UndefinedParameter { param: *n });
                    }
                    refs.push((i, VarRef::Var(VarId(u32::from(*n) - 1))));
                }
                Token::Ident(name) if !is_ident_position_with(tokens, i, &slots) => {
                    if let Some(id) = self.lookup_var(&name.to_ascii_lowercase()) {
                        refs.push((i, VarRef::Var(id)));
                    }
                }
                Token::QualifiedIdent { qualifier, name }
                    if !is_ident_position_with(tokens, i, &slots) =>
                {
                    if let Some(r) = self.lookup_qualified(qualifier, name) {
                        refs.push((i, r));
                    }
                }
                _ => {}
            }
        }
        let mut distinct: Vec<&VarId> = Vec::new();
        for (_, r) in &refs {
            let (VarRef::Var(id) | VarRef::Field(id, _)) = r;
            if !distinct.contains(&id) {
                distinct.push(id);
            }
        }
        if distinct.len() > MAX_PLPGSQL_VARS_PER_STATEMENT {
            return Err(too_big_err(
                "too many PL/pgSQL variables referenced in one statement",
            ));
        }
        Ok(refs)
    }

    // ---- 式・SQL の構築 ----

    fn build_expr(&self, a: usize, b: usize, cond: bool) -> Result<SqlExpr, SqlSurfaceError> {
        let tokens = sql_tokens(self.span_text(a, b)?)?;
        let var_refs = self.resolve_refs(&tokens)?;
        Ok(SqlExpr {
            tokens,
            var_refs,
            cond,
        })
    }

    /// 終端が `words` のいずれかになる式を読む（終端は消費しない）。
    fn expr_until(&mut self, words: &[&str], cond: bool) -> Result<SqlExpr, SqlSurfaceError> {
        let a = self.pos;
        let e = self.find_end(
            a,
            &|k| matches!(k, TokKind::Word(w) if words.iter().any(|x| w.eq_ignore_ascii_case(x))),
        )?;
        match self.toks.get(e).map(|t| &t.kind) {
            Some(TokKind::Word(_)) => {}
            _ => {
                return Err(syn_err(
                    "missing keyword after an expression in PL/pgSQL body",
                ))
            }
        }
        let x = self.build_expr(a, e, cond)?;
        self.pos = e;
        Ok(x)
    }

    /// `;` までの式を読む（`;` は消費しない）。
    fn expr_to_semicolon(&mut self, cond: bool) -> Result<SqlExpr, SqlSurfaceError> {
        let a = self.pos;
        let e = self.find_end(a, &|_| false)?;
        let x = self.build_expr(a, e, cond)?;
        self.pos = e;
        Ok(x)
    }

    fn build_sql(
        &self,
        text_tokens: Vec<Token>,
        kind: EmbeddedKind,
        allow_into: bool,
    ) -> Result<EmbeddedSql, SqlSurfaceError> {
        reject_forbidden_clauses(&text_tokens)?;
        let (tokens, raw) = split_into(text_tokens)?;
        if raw.is_some() && !allow_into {
            return Err(syn_err("INTO is not allowed in this PL/pgSQL statement"));
        }
        let into = match raw {
            Some(r) => Some(self.resolve_into(r)?),
            None => None,
        };
        let var_refs = self.resolve_refs(&tokens)?;
        let writes = matches!(
            kind,
            EmbeddedKind::Insert | EmbeddedKind::Update | EmbeddedKind::Delete
        ) || has_write_cte(&tokens);
        Ok(EmbeddedSql {
            kind,
            tokens,
            var_refs,
            into,
            writes,
        })
    }

    fn resolve_into(&self, raw: RawInto) -> Result<IntoClause, SqlSurfaceError> {
        let mut targets = Vec::new();
        for (q, n) in raw.targets {
            targets.push(self.resolve_target(q.as_deref(), &n.to_ascii_lowercase())?);
        }
        Ok(IntoClause {
            strict: raw.strict,
            targets,
        })
    }

    // ---- ブロック ----

    fn parse_top(&mut self) -> Result<Block, SqlSurfaceError> {
        if matches!(self.kind(0), Some(TokKind::Punct('#'))) {
            return Err(oos_err("PL/pgSQL compiler directives are not supported"));
        }
        let label = self.parse_label_opt()?;
        if !(self.is_kw(0, "DECLARE") || self.is_kw(0, "BEGIN")) {
            return Err(syn_err("PL/pgSQL body must be a block"));
        }
        self.parse_block(label, true)
    }

    fn parse_label_opt(&mut self) -> Result<Option<String>, SqlSurfaceError> {
        if matches!(self.kind(0), Some(TokKind::LabelOpen)) {
            self.pos = self.pos.saturating_add(1);
            let name = self.take_name()?;
            if !matches!(self.kind(0), Some(TokKind::LabelClose)) {
                return Err(syn_err("expected '>>' after a label in PL/pgSQL body"));
            }
            self.pos = self.pos.saturating_add(1);
            Ok(Some(name))
        } else {
            Ok(None)
        }
    }

    fn end_label(&mut self, label: &Option<String>) -> Result<(), SqlSurfaceError> {
        let name = match self.kind(0) {
            Some(TokKind::Word(w)) => w.to_ascii_lowercase(),
            Some(TokKind::Quoted(q)) => q.to_ascii_lowercase(),
            _ => return Ok(()),
        };
        if label.as_deref() != Some(name.as_str()) {
            return Err(syn_err("END label does not match the block or loop label"));
        }
        self.pos = self.pos.saturating_add(1);
        Ok(())
    }

    fn parse_block(&mut self, label: Option<String>, top: bool) -> Result<Block, SqlSurfaceError> {
        self.enter()?;
        self.scopes.push(Scope {
            label: label.clone(),
            names: Vec::new(),
        });
        let mut decls = Vec::new();
        if self.eat_kw("DECLARE") {
            while !self.is_kw(0, "BEGIN") {
                decls.push(self.parse_decl()?);
            }
        }
        self.expect_kw("BEGIN")?;
        self.ctrl.push(Ctrl {
            label: label.clone(),
            is_loop: false,
        });
        let stmts = self.parse_stmts()?;
        self.ctrl.pop();
        if self.is_kw(0, "EXCEPTION") {
            return Err(oos_err("EXCEPTION blocks are not supported in PL/pgSQL"));
        }
        self.expect_kw("END")?;
        self.end_label(&label)?;
        self.scopes.pop();
        self.leave();
        if top {
            if self.is_punct(0, ';') {
                self.pos = self.pos.saturating_add(1);
            }
            if self.pos < self.toks.len() {
                return Err(syn_err("unexpected tokens after the PL/pgSQL body"));
            }
        } else {
            self.expect_punct(';')?;
        }
        Ok(Block {
            label,
            decls,
            stmts,
        })
    }

    fn parse_decl(&mut self) -> Result<VarId, SqlSurfaceError> {
        let name = self.take_name()?;
        if self.is_kw(0, "ALIAS") {
            return Err(oos_err("ALIAS declarations are not supported in PL/pgSQL"));
        }
        if self.is_kw(0, "CURSOR") || self.is_kw(0, "SCROLL") || self.is_kw(0, "NO") {
            return Err(oos_err("cursor variables are not supported in PL/pgSQL"));
        }
        let constant = self.eat_kw("CONSTANT");
        let ts = self.pos;
        let te = self.find_end(ts, &|k| match k {
            TokKind::Word(w) => ["COLLATE", "NOT", "DEFAULT"]
                .iter()
                .any(|x| w.eq_ignore_ascii_case(x)),
            TokKind::Assign => true,
            TokKind::Punct('=') => true,
            _ => false,
        })?;
        let ty = self.parse_decl_type(ts, te)?;
        self.pos = te;
        if self.is_kw(0, "COLLATE") {
            // 照合順序は AST で保持しないため、黙って捨てず範囲外として拒否する。
            return Err(oos_err("COLLATE in PL/pgSQL declarations is not supported"));
        }
        let mut not_null = false;
        if self.is_kw(0, "NOT") && self.is_kw(1, "NULL") {
            not_null = true;
            self.pos = self.pos.saturating_add(2);
        }
        let default = if self.eat_kw("DEFAULT") || self.eat_assign_op() {
            Some(self.expr_to_semicolon(false)?)
        } else {
            None
        };
        self.expect_punct(';')?;
        if not_null && default.is_none() {
            return Err(syn_err("NOT NULL variable requires a default value"));
        }
        if self.declared_in_current_scope(&name) {
            return Err(syn_err("duplicate PL/pgSQL variable declaration"));
        }
        self.declare(name, VarKind::Local, ty, constant, not_null, default)
    }

    fn parse_decl_type(&self, ts: usize, te: usize) -> Result<DeclType, SqlSurfaceError> {
        let n = te.saturating_sub(ts);
        if n == 0 {
            return Err(syn_err("missing type in PL/pgSQL declaration"));
        }
        let toks = self.toks.get(ts..te).unwrap_or(&[]);
        let tail_word = |off: usize| match toks.get(n.saturating_sub(off)).map(|t| &t.kind) {
            Some(TokKind::Word(w)) => Some(w.to_ascii_lowercase()),
            _ => None,
        };
        if n >= 3
            && matches!(toks.get(n - 2).map(|t| &t.kind), Some(TokKind::Punct('%')))
            && matches!(tail_word(1).as_deref(), Some("type") | Some("rowtype"))
        {
            let rowtype = tail_word(1).as_deref() == Some("rowtype");
            let mut path = Vec::new();
            let mut expect_name = true;
            for t in toks.iter().take(n - 2) {
                match (&t.kind, expect_name) {
                    (TokKind::Word(w), true) => path.push(w.to_ascii_lowercase()),
                    (TokKind::Quoted(q), true) => path.push(q.to_ascii_lowercase()),
                    (TokKind::Punct('.'), false) => {}
                    _ => return Err(syn_err("invalid %TYPE/%ROWTYPE reference")),
                }
                expect_name = !expect_name;
            }
            if expect_name || path.is_empty() || path.len() > 3 || (rowtype && path.len() > 2) {
                return Err(syn_err("invalid %TYPE/%ROWTYPE reference"));
            }
            return Ok(if rowtype {
                DeclType::PercentRowType { path }
            } else {
                DeclType::PercentType { path }
            });
        }
        if n == 1 {
            if let Some(TokKind::Word(w)) = toks.first().map(|t| &t.kind) {
                let l = w.to_ascii_lowercase();
                if l == "record" {
                    return Ok(DeclType::Record);
                }
                if l == "refcursor" {
                    return Err(oos_err("refcursor variables are not supported in PL/pgSQL"));
                }
                if POLYMORPHIC.contains(&l.as_str()) || l.starts_with("anycompatible") {
                    return Err(oos_err("polymorphic types are not supported in PL/pgSQL"));
                }
            }
        }
        let tokens = sql_tokens(self.span_text(ts, te)?)?;
        let mut p = 0usize;
        let ty = parse_column_type_name(&tokens, &mut p)
            .map_err(|e| super::sanitize_delegated(&e, "a PL/pgSQL declaration type"))?;
        if p != tokens.len() {
            return Err(syn_err("invalid type in PL/pgSQL declaration"));
        }
        Ok(DeclType::Named(ty))
    }

    // ---- 文 ----

    fn parse_stmts(&mut self) -> Result<Vec<Stmt>, SqlSurfaceError> {
        let mut out = Vec::new();
        loop {
            match self.kind(0) {
                None => return Err(syn_err("unexpected end of PL/pgSQL body")),
                Some(TokKind::Word(w))
                    if ["END", "ELSE", "ELSIF", "ELSEIF", "WHEN", "EXCEPTION"]
                        .iter()
                        .any(|x| w.eq_ignore_ascii_case(x)) =>
                {
                    return Ok(out)
                }
                _ => {}
            }
            out.push(self.parse_stmt()?);
        }
    }

    fn assignment_ahead(&self) -> bool {
        let name_at = |off: usize| {
            matches!(
                self.kind(off),
                Some(TokKind::Word(_)) | Some(TokKind::Quoted(_))
            )
        };
        if !name_at(0) {
            return false;
        }
        let mut off = 1usize;
        if self.is_punct(off, '.') && name_at(off.saturating_add(1)) {
            off = off.saturating_add(2);
        }
        matches!(self.kind(off), Some(TokKind::Assign)) || self.is_punct(off, '=')
    }

    fn parse_stmt(&mut self) -> Result<Stmt, SqlSurfaceError> {
        let label = self.parse_label_opt()?;
        let word = match self.kind(0) {
            Some(TokKind::Word(w)) => Some(w.to_ascii_uppercase()),
            Some(_) => None,
            None => return Err(syn_err("unexpected end of PL/pgSQL body")),
        };
        if label.is_some()
            && !matches!(
                word.as_deref(),
                Some("DECLARE") | Some("BEGIN") | Some("LOOP") | Some("WHILE") | Some("FOR")
            )
        {
            return Err(syn_err("a label can only precede a block or a loop"));
        }
        let Some(w) = word else {
            return self.parse_assign();
        };
        match w.as_str() {
            "DECLARE" | "BEGIN" => Ok(Stmt::Block(self.parse_block(label, false)?)),
            "IF" => self.parse_if(),
            "CASE" => self.parse_case(),
            "LOOP" | "WHILE" | "FOR" => self.parse_loop(label, &w),
            "EXIT" | "CONTINUE" => self.parse_exit(w == "EXIT"),
            "RETURN" => self.parse_return(),
            "PERFORM" => self.parse_perform(),
            "EXECUTE" => self.parse_execute(),
            "GET" => self.parse_get(),
            "NULL" => {
                self.pos = self.pos.saturating_add(1);
                self.expect_punct(';')?;
                Ok(Stmt::Null)
            }
            "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "VALUES" => self.parse_sql(&w),
            _ => {
                if self.assignment_ahead() {
                    self.parse_assign()
                } else if OUT_OF_SCOPE_STARTS.contains(&w.as_str()) {
                    Err(oos_err("this statement is not supported in PL/pgSQL"))
                } else {
                    Err(syn_err("unrecognized PL/pgSQL statement"))
                }
            }
        }
    }

    fn parse_assign(&mut self) -> Result<Stmt, SqlSurfaceError> {
        let (q, n) = self.take_dotted()?;
        let target = self.resolve_target(q.as_deref(), &n)?;
        if !self.eat_assign_op() {
            return Err(syn_err("expected := or = in a PL/pgSQL assignment"));
        }
        let expr = self.expr_to_semicolon(false)?;
        self.expect_punct(';')?;
        Ok(Stmt::Assign { target, expr })
    }

    fn parse_if(&mut self) -> Result<Stmt, SqlSurfaceError> {
        self.enter()?;
        self.pos = self.pos.saturating_add(1);
        let mut branches = Vec::new();
        let cond = self.expr_until(&["THEN"], true)?;
        self.expect_kw("THEN")?;
        let st = self.parse_stmts()?;
        branches.push((cond, st));
        let mut else_ = None;
        loop {
            if self.is_kw(0, "ELSIF") || self.is_kw(0, "ELSEIF") {
                self.pos = self.pos.saturating_add(1);
                let cond = self.expr_until(&["THEN"], true)?;
                self.expect_kw("THEN")?;
                let st = self.parse_stmts()?;
                branches.push((cond, st));
            } else if self.eat_kw("ELSE") {
                else_ = Some(self.parse_stmts()?);
                break;
            } else {
                break;
            }
        }
        self.expect_kw("END")?;
        self.expect_kw("IF")?;
        self.expect_punct(';')?;
        self.leave();
        Ok(Stmt::If { branches, else_ })
    }

    fn parse_case(&mut self) -> Result<Stmt, SqlSurfaceError> {
        self.enter()?;
        self.pos = self.pos.saturating_add(1);
        let selector = if self.is_kw(0, "WHEN") {
            None
        } else {
            Some(self.expr_until(&["WHEN"], false)?)
        };
        let mut whens = Vec::new();
        while self.eat_kw("WHEN") {
            let a = self.pos;
            let e = self.find_end(
                a,
                &|k| matches!(k, TokKind::Word(w) if w.eq_ignore_ascii_case("THEN")),
            )?;
            if !matches!(self.toks.get(e).map(|t| &t.kind), Some(TokKind::Word(_))) {
                return Err(syn_err("missing THEN in a PL/pgSQL CASE statement"));
            }
            let ranges = self.split_commas(a, e);
            if selector.is_none() && ranges.len() != 1 {
                return Err(syn_err("invalid searched CASE condition"));
            }
            let mut exprs = Vec::new();
            for (x, y) in ranges {
                exprs.push(self.build_expr(x, y, selector.is_none())?);
            }
            self.pos = e;
            self.expect_kw("THEN")?;
            let st = self.parse_stmts()?;
            whens.push((exprs, st));
        }
        if whens.is_empty() {
            return Err(syn_err("CASE statement requires at least one WHEN"));
        }
        let else_ = if self.eat_kw("ELSE") {
            Some(self.parse_stmts()?)
        } else {
            None
        };
        self.expect_kw("END")?;
        self.expect_kw("CASE")?;
        self.expect_punct(';')?;
        self.leave();
        Ok(Stmt::Case {
            selector,
            whens,
            else_,
        })
    }

    fn loop_body(&mut self, label: &Option<String>) -> Result<Vec<Stmt>, SqlSurfaceError> {
        self.ctrl.push(Ctrl {
            label: label.clone(),
            is_loop: true,
        });
        let body = self.parse_stmts()?;
        self.ctrl.pop();
        self.expect_kw("END")?;
        self.expect_kw("LOOP")?;
        self.end_label(label)?;
        self.expect_punct(';')?;
        Ok(body)
    }

    fn parse_loop(&mut self, label: Option<String>, kw: &str) -> Result<Stmt, SqlSurfaceError> {
        self.enter()?;
        self.pos = self.pos.saturating_add(1);
        let stmt = match kw {
            "LOOP" => {
                let body = self.loop_body(&label)?;
                Stmt::Loop { label, body }
            }
            "WHILE" => {
                let cond = self.expr_until(&["LOOP"], true)?;
                self.expect_kw("LOOP")?;
                let body = self.loop_body(&label)?;
                Stmt::While { label, cond, body }
            }
            _ => self.parse_for(label)?,
        };
        self.leave();
        Ok(stmt)
    }

    fn parse_for(&mut self, label: Option<String>) -> Result<Stmt, SqlSurfaceError> {
        let mut raw_targets: Vec<(Option<String>, String)> = Vec::new();
        loop {
            raw_targets.push(self.take_dotted()?);
            if self.is_punct(0, ',') {
                self.pos = self.pos.saturating_add(1);
            } else {
                break;
            }
        }
        self.expect_kw("IN")?;
        if self.is_kw(0, "EXECUTE") {
            return Err(oos_err("FOR ... IN EXECUTE is not supported in PL/pgSQL"));
        }
        let reverse = self.eat_kw("REVERSE");
        let a = self.pos;
        let e = self.find_end(a, &|k| {
            matches!(k, TokKind::DotDot)
                || matches!(k, TokKind::Word(w) if w.eq_ignore_ascii_case("LOOP"))
        })?;
        match self.toks.get(e).map(|t| &t.kind) {
            Some(TokKind::DotDot) => self.parse_for_int(label, raw_targets, reverse, a, e),
            Some(TokKind::Word(_)) => self.parse_for_query(label, raw_targets, reverse, a, e),
            _ => Err(syn_err("malformed PL/pgSQL FOR loop")),
        }
    }

    fn parse_for_int(
        &mut self,
        label: Option<String>,
        raw_targets: Vec<(Option<String>, String)>,
        reverse: bool,
        a: usize,
        e: usize,
    ) -> Result<Stmt, SqlSurfaceError> {
        let name = match raw_targets.as_slice() {
            [(None, n)] => n.clone(),
            _ => return Err(syn_err("integer FOR loop requires a single loop variable")),
        };
        let lower = self.build_expr(a, e, false)?;
        self.pos = e.saturating_add(1);
        let upper = self.expr_until(&["BY", "LOOP"], false)?;
        let step = if self.eat_kw("BY") {
            Some(self.expr_until(&["LOOP"], false)?)
        } else {
            None
        };
        self.expect_kw("LOOP")?;
        self.scopes.push(Scope {
            label: label.clone(),
            names: Vec::new(),
        });
        let var = self.declare(
            name,
            VarKind::LoopInt,
            DeclType::Named(SqlColumnTypeName::Integer),
            false,
            false,
            None,
        )?;
        let body = self.loop_body(&label)?;
        self.scopes.pop();
        Ok(Stmt::ForInt {
            label,
            var,
            reverse,
            lower,
            upper,
            step,
            body,
        })
    }

    fn parse_for_query(
        &mut self,
        label: Option<String>,
        raw_targets: Vec<(Option<String>, String)>,
        reverse: bool,
        a: usize,
        e: usize,
    ) -> Result<Stmt, SqlSurfaceError> {
        // 括弧で始まる問い合わせ（`FOR r IN (SELECT ...) LOOP`）も受理する。
        let first_is_query = match self.kind(0) {
            Some(TokKind::Word(w)) => ["SELECT", "WITH", "VALUES", "INSERT", "UPDATE", "DELETE"]
                .iter()
                .any(|x| w.eq_ignore_ascii_case(x)),
            Some(TokKind::Punct('(')) => true,
            _ => false,
        };
        if !first_is_query || reverse {
            // `FOR r IN cursor_name LOOP` はカーソル FOR（範囲外）。それ以外は構文誤り。
            if !reverse && e == a.saturating_add(1) {
                return Err(oos_err("cursor FOR loops are not supported in PL/pgSQL"));
            }
            return Err(syn_err("malformed PL/pgSQL FOR loop"));
        }
        let tokens = strip_outer_parens(sql_tokens(self.span_text(a, e)?)?);
        let kind = classify_query(&tokens)?;
        let query = self.build_sql(tokens, kind, false)?;
        self.pos = e;
        self.expect_kw("LOOP")?;
        // 対象の解決は問い合わせ（外側のスコープ）の後。未宣言の単独対象は暗黙のレコード変数にする。
        self.scopes.push(Scope {
            label: label.clone(),
            names: Vec::new(),
        });
        let mut targets = Vec::new();
        let single = raw_targets.len() == 1;
        for (q, n) in &raw_targets {
            let resolved = match (q, self.lookup_var(n)) {
                (None, None) if single => AssignTarget::Var(self.declare(
                    n.clone(),
                    VarKind::LoopRecord,
                    DeclType::Record,
                    false,
                    false,
                    None,
                )?),
                _ => self.resolve_target(q.as_deref(), n)?,
            };
            targets.push(resolved);
        }
        let body = self.loop_body(&label)?;
        self.scopes.pop();
        Ok(Stmt::ForQuery {
            label,
            targets,
            query,
            body,
        })
    }

    fn parse_exit(&mut self, is_exit: bool) -> Result<Stmt, SqlSurfaceError> {
        self.pos = self.pos.saturating_add(1);
        let label = if matches!(
            self.kind(0),
            Some(TokKind::Word(_)) | Some(TokKind::Quoted(_))
        ) && !self.is_kw(0, "WHEN")
        {
            Some(self.take_name()?)
        } else {
            None
        };
        let when = if self.eat_kw("WHEN") {
            Some(self.expr_to_semicolon(true)?)
        } else {
            None
        };
        self.expect_punct(';')?;
        match &label {
            None => {
                if !self.ctrl.iter().any(|c| c.is_loop) {
                    return Err(syn_err("EXIT/CONTINUE cannot be used outside a loop"));
                }
            }
            Some(l) => match self
                .ctrl
                .iter()
                .rev()
                .find(|c| c.label.as_deref() == Some(l))
            {
                None => return Err(syn_err("EXIT/CONTINUE refers to an unknown label")),
                Some(c) if !is_exit && !c.is_loop => {
                    return Err(syn_err("CONTINUE cannot refer to a block label"))
                }
                Some(_) => {}
            },
        }
        Ok(if is_exit {
            Stmt::Exit { label, when }
        } else {
            Stmt::Continue { label, when }
        })
    }

    fn parse_return(&mut self) -> Result<Stmt, SqlSurfaceError> {
        self.pos = self.pos.saturating_add(1);
        let shape = self.opts.returns;
        if self.is_kw(0, "NEXT") {
            if shape != ReturnShape::SetOf {
                return Err(syn_err(
                    "RETURN NEXT is only allowed in a set-returning function",
                ));
            }
            self.pos = self.pos.saturating_add(1);
            // OUT/INOUT 引数は未サポートのため、値なしの `RETURN NEXT;` は不正構文。
            if self.is_punct(0, ';') {
                return Err(syn_err("RETURN NEXT must have a parameter"));
            }
            let e = self.expr_to_semicolon(false)?;
            self.expect_punct(';')?;
            return Ok(Stmt::ReturnNext(Some(e)));
        }
        if self.is_kw(0, "QUERY") {
            if shape != ReturnShape::SetOf {
                return Err(syn_err(
                    "RETURN QUERY is only allowed in a set-returning function",
                ));
            }
            self.pos = self.pos.saturating_add(1);
            if self.is_kw(0, "EXECUTE") {
                return Err(oos_err("RETURN QUERY EXECUTE is not supported in PL/pgSQL"));
            }
            let a = self.pos;
            let e = self.find_end(a, &|_| false)?;
            // `FOR ... IN` と同じく外側の括弧を剥がす（`RETURN QUERY (SELECT ...)`）。
            let tokens = strip_outer_parens(sql_tokens(self.span_text(a, e)?)?);
            let kind = classify_query(&tokens)?;
            let q = self.build_sql(tokens, kind, false)?;
            self.pos = e;
            self.expect_punct(';')?;
            return Ok(Stmt::ReturnQuery(q));
        }
        if self.is_punct(0, ';') {
            // 値を返す形（スカラ・トリガ）では値なしの `RETURN;` は不正（OUT 引数は未サポート）。
            if matches!(shape, ReturnShape::Scalar | ReturnShape::Trigger) {
                return Err(syn_err(
                    "RETURN must have a parameter in this PL/pgSQL function",
                ));
            }
            self.pos = self.pos.saturating_add(1);
            return Ok(Stmt::Return(None));
        }
        if matches!(shape, ReturnShape::Void | ReturnShape::SetOf) {
            return Err(SqlSurfaceError::datatype_mismatch(
                "RETURN cannot have a parameter in this PL/pgSQL function",
            ));
        }
        let e = self.expr_to_semicolon(false)?;
        self.expect_punct(';')?;
        Ok(Stmt::Return(Some(e)))
    }

    fn parse_perform(&mut self) -> Result<Stmt, SqlSurfaceError> {
        self.pos = self.pos.saturating_add(1);
        let a = self.pos;
        let e = self.find_end(a, &|_| false)?;
        let mut tokens = vec![Token::Keyword(Keyword::Select)];
        tokens.extend(sql_tokens(self.span_text(a, e)?)?);
        let q = self.build_sql(tokens, EmbeddedKind::Perform, false)?;
        self.pos = e;
        self.expect_punct(';')?;
        Ok(Stmt::Perform(q))
    }

    fn parse_sql(&mut self, word: &str) -> Result<Stmt, SqlSurfaceError> {
        let a = self.pos;
        let e = self.find_end(a, &|_| false)?;
        let tokens = sql_tokens(self.span_text(a, e)?)?;
        let kind = match word {
            "INSERT" => EmbeddedKind::Insert,
            "UPDATE" => EmbeddedKind::Update,
            "DELETE" => EmbeddedKind::Delete,
            "WITH" => classify_with(&tokens)?.0,
            _ => EmbeddedKind::Select,
        };
        let q = self.build_sql(tokens, kind, true)?;
        self.pos = e;
        self.expect_punct(';')?;
        Ok(Stmt::Sql(q))
    }

    fn parse_into_targets(&mut self) -> Result<IntoClause, SqlSurfaceError> {
        let strict = self.eat_kw("STRICT");
        let mut targets = Vec::new();
        loop {
            let (q, n) = self.take_dotted()?;
            targets.push(self.resolve_target(q.as_deref(), &n)?);
            if self.is_punct(0, ',') {
                self.pos = self.pos.saturating_add(1);
            } else {
                break;
            }
        }
        Ok(IntoClause { strict, targets })
    }

    fn parse_execute(&mut self) -> Result<Stmt, SqlSurfaceError> {
        self.pos = self.pos.saturating_add(1);
        let a = self.pos;
        let e = self.find_end(a, &|k| {
            matches!(k, TokKind::Word(w) if w.eq_ignore_ascii_case("INTO") || w.eq_ignore_ascii_case("USING"))
        })?;
        let query = self.build_expr(a, e, false)?;
        self.pos = e;
        let mut into = None;
        let mut using = Vec::new();
        let mut seen_using = false;
        loop {
            if self.is_kw(0, "INTO") {
                if into.is_some() {
                    return Err(syn_err("multiple INTO clauses in EXECUTE"));
                }
                self.pos = self.pos.saturating_add(1);
                into = Some(self.parse_into_targets()?);
            } else if self.is_kw(0, "USING") {
                if seen_using {
                    return Err(syn_err("multiple USING clauses in EXECUTE"));
                }
                seen_using = true;
                self.pos = self.pos.saturating_add(1);
                if self.is_kw(0, "OPERATION_ID") {
                    return Err(oos_err(
                        "USING OPERATION_ID is not supported inside a PL/pgSQL body",
                    ));
                }
                let a = self.pos;
                let e = self.find_end(a, &|k| {
                    matches!(k, TokKind::Word(w)
                            if w.eq_ignore_ascii_case("INTO") || w.eq_ignore_ascii_case("USING"))
                })?;
                for (x, y) in self.split_commas(a, e) {
                    using.push(self.build_expr(x, y, false)?);
                }
                self.pos = e;
            } else {
                break;
            }
        }
        self.expect_punct(';')?;
        Ok(Stmt::Execute { query, into, using })
    }

    fn parse_get(&mut self) -> Result<Stmt, SqlSurfaceError> {
        self.pos = self.pos.saturating_add(1);
        if self.is_kw(0, "STACKED") {
            return Err(oos_err(
                "GET STACKED DIAGNOSTICS is not supported in PL/pgSQL",
            ));
        }
        self.eat_kw("CURRENT");
        self.expect_kw("DIAGNOSTICS")?;
        let mut items = Vec::new();
        loop {
            let (q, n) = self.take_dotted()?;
            let target = self.resolve_target(q.as_deref(), &n)?;
            if !self.eat_assign_op() {
                return Err(syn_err("expected = or := in GET DIAGNOSTICS"));
            }
            if !self.is_kw(0, "ROW_COUNT") {
                return Err(oos_err("only ROW_COUNT is supported in GET DIAGNOSTICS"));
            }
            self.pos = self.pos.saturating_add(1);
            items.push((target, DiagItem::RowCount));
            if self.is_punct(0, ',') {
                self.pos = self.pos.saturating_add(1);
            } else {
                break;
            }
        }
        self.expect_punct(';')?;
        Ok(Stmt::GetDiagnostics { items })
    }
}
