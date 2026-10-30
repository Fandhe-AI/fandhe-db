//! 埋め込み SQL・式の定義時分類と、`INTO` 句の除去などの純粋な補助処理（Issue #2224・SQL-68）。
//!
//! `plpgsql::parser` が切り出した SQL トークン列を、**SQL 構文解析器の記録**（`sql::colref_sink`。
//! 解析器が列参照と確定した識別子の位置と役割）に通して、変数参照を分類する。PL/pgSQL 側は SQL 文法を
//! 推定しない（位置のマスクもダミー置換も持たない）。分類の流れ:
//!
//! 1. 行数制限句・名前付き `WINDOW` を正規化し（冪等。トークン数が変わるため以降は正規化後の列を保存して
//!    分類する）、本体中の `$n` をスコープ内に見える合成識別子（字句解析器が作らない `$` 付きの名前）へ置く
//! 2. 記録区間（ガード）の下で、変数を置換しないまま構造解析（`sql::embedded_unit`。カタログは引かない）に
//!    通す。サブクエリ・CTE 本体は、解析器が取り込んだ範囲ごとにルート上のスライスで再帰的に解析する
//! 3. 記録された位置のうち、PL/pgSQL のスコープで解決できる名前を [`VarSite`]（位置・役割・変数）として
//!    AST へ保存する。範囲外の列参照・解決できないリテラル専用位置・列しか受理しない位置の変数は `0A000`
//!    （fail-closed。黙って列として読む誤りを作らない）
//!
//! 検証の実体（構造解析）は `EmbeddedSqlChecker` を介して呼ぶ（core が実装。`plpgsql` はストレージにも
//! カタログにも触れない）。呼び出し時の値の置換（役割別の置換・`42702` の判定）は #2226 の範囲で、
//! ここは定義時に記録した位置と役割を後続が使える形で保存するところまでを担う（ADR D3）。

use std::collections::HashSet;
use std::ops::Range;

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::colref_sink::{self, ColrefReport, Role};
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::embedded_unit::{normalize_statement, parse_unit, UnitKind, UnitSummary};
use crate::sql::lexer::{Keyword, Token};

use super::ast::{
    Block, Body, DeclType, EmbeddedKind, EmbeddedSql, ScopeView, SqlExpr, Stmt, VarDecl, VarId,
    VarRef, VarSite,
};
use super::{oos_err, syn_err, too_big_err};

/// `Ident` が（大小無視で）`word` か。
pub(super) fn ident_is(t: &Token, word: &str) -> bool {
    matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(word))
}

/// キーワード判定用の「見え方」を作る。二重引用符付き識別子（`quoted[i]`）の内容の先頭に `"` を付け、
/// [`ident_is`] が決して一致しないようにする。SQL 字句解析器は `"into"` も `into` も `Ident("into")`
/// に潰すため、これが無いと列名・別名の `"into"`／`"partitioned"` を句のキーワードと取り違える。
/// 長さと添字は元の `tokens` と同一（構造の判定専用で、名前の取り出しには元の `tokens` を使う）。
/// `split_into`・`reject_forbidden_clauses`・`IdentCtx::new`・`classify_*` が内部で使う。
pub(super) fn keyword_view(tokens: &[Token], quoted: &[bool]) -> Vec<Token> {
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| match t {
            Token::Ident(s) if quoted.get(i).copied().unwrap_or(false) => {
                Token::Ident(format!("\"{s}"))
            }
            other => other.clone(),
        })
        .collect()
}

/// `INTO` 句の生の対象（修飾子, 名前）。
pub(super) struct RawInto {
    pub(super) strict: bool,
    pub(super) targets: Vec<(Option<String>, String)>,
}

/// [`split_into`] の結果（INTO 区間を除いたトークン列・同じ長さの引用情報・取り出した INTO 句）。
pub(super) type SplitInto = (Vec<Token>, Vec<bool>, Option<RawInto>);

/// 括弧の深さ 0 の文脈キーワード `INTO` と対象列を取り除く。`INSERT INTO` の `INTO` は対象外。
/// 引用付きの `"into"`（列名・別名）は句ではない。`INTO` が 2 個あれば `42601`。
/// 戻り値のトークン列と引用情報は常に同じ長さ（取り除いた区間は両方から落とす）。
pub(super) fn split_into(
    mut tokens: Vec<Token>,
    mut quoted: Vec<bool>,
) -> Result<SplitInto, SqlSurfaceError> {
    let view = keyword_view(&tokens, &quoted);
    let mut depth = 0usize;
    let mut found: Option<usize> = None;
    for (i, t) in view.iter().enumerate() {
        match t {
            Token::Punct('(') | Token::Punct('[') => depth = depth.saturating_add(1),
            Token::Punct(')') | Token::Punct(']') => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && ident_is(t, "INTO") {
            let after_insert = i
                .checked_sub(1)
                .and_then(|p| view.get(p))
                .is_some_and(|p| ident_is(p, "INSERT"));
            if after_insert {
                continue;
            }
            if found.is_some() {
                return Err(syn_err("multiple INTO clauses in a PL/pgSQL statement"));
            }
            found = Some(i);
        }
    }
    let Some(at) = found else {
        return Ok((tokens, quoted, None));
    };
    let mut j = at.saturating_add(1);
    let mut strict = false;
    if view.get(j).is_some_and(|t| ident_is(t, "STRICT")) {
        strict = true;
        j = j.saturating_add(1);
    }
    let mut targets = Vec::new();
    loop {
        match tokens.get(j) {
            Some(Token::Ident(n)) => targets.push((None, n.clone())),
            Some(Token::QualifiedIdent { qualifier, name }) => {
                targets.push((Some(qualifier.clone()), name.clone()))
            }
            _ => return Err(syn_err("invalid INTO target in a PL/pgSQL statement")),
        }
        j = j.saturating_add(1);
        if matches!(tokens.get(j), Some(Token::Punct(','))) {
            j = j.saturating_add(1);
        } else {
            break;
        }
    }
    tokens.drain(at..j.min(tokens.len()));
    quoted.drain(at..j.min(quoted.len()));
    debug_assert_eq!(tokens.len(), quoted.len());
    Ok((tokens, quoted, Some(RawInto { strict, targets })))
}

/// 本体の中で禁止する句（`USING OPERATION_ID`・括弧の深さ 0 の `PARTITIONED`）を `0A000` で拒否する。
/// 引用付きの `"partitioned"`・`"operation_id"`（列名・別名）は句ではないので拒否しない。
pub(super) fn reject_forbidden_clauses(
    tokens: &[Token],
    quoted: &[bool],
) -> Result<(), SqlSurfaceError> {
    let tokens = keyword_view(tokens, quoted);
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') | Token::Punct('[') => depth = depth.saturating_add(1),
            Token::Punct(')') | Token::Punct(']') => depth = depth.saturating_sub(1),
            _ => {}
        }
        if ident_is(t, "USING")
            && tokens
                .get(i.saturating_add(1))
                .is_some_and(|n| ident_is(n, "OPERATION_ID"))
        {
            return Err(oos_err(
                "USING OPERATION_ID is not supported inside a PL/pgSQL body",
            ));
        }
        if depth == 0 && ident_is(t, "PARTITIONED") {
            return Err(oos_err(
                "PARTITIONED is not supported inside a PL/pgSQL body",
            ));
        }
    }
    Ok(())
}

/// 先頭の語から文種を仮置きする（`RETURN QUERY`・問い合わせ `FOR` 用）。`SELECT`／`VALUES`／括弧始まり／
/// `WITH` は `Select`（`WITH` の実際の文種は定義時分類が解析木から確定する）、`INSERT`／`UPDATE`／`DELETE`
/// はその種別。これら以外（引用付き識別子の先頭を含む）は `42601` で拒否する。
pub(super) fn head_kind(
    tokens: &[Token],
    quoted: &[bool],
) -> Result<EmbeddedKind, SqlSurfaceError> {
    let first_quoted = quoted.first().copied().unwrap_or(false);
    match tokens.first() {
        Some(Token::Keyword(Keyword::Select)) | Some(Token::Punct('(')) => Ok(EmbeddedKind::Select),
        Some(Token::Ident(w)) if !first_quoted => match w.to_ascii_uppercase().as_str() {
            "VALUES" | "WITH" => Ok(EmbeddedKind::Select),
            "INSERT" => Ok(EmbeddedKind::Insert),
            "UPDATE" => Ok(EmbeddedKind::Update),
            "DELETE" => Ok(EmbeddedKind::Delete),
            _ => Err(syn_err("unsupported query in a PL/pgSQL statement")),
        },
        _ => Err(syn_err("unsupported query in a PL/pgSQL statement")),
    }
}

/// 定義時検証に使うダミートークン。整数・浮動小数は `0`、真偽値は `false`、それ以外は文字列 `'0'`。
pub(crate) fn dummy_token(ty: &DeclType) -> Token {
    fn named(t: &SqlColumnTypeName) -> Token {
        match t {
            SqlColumnTypeName::SmallInt
            | SqlColumnTypeName::Integer
            | SqlColumnTypeName::BigInt
            | SqlColumnTypeName::Real
            | SqlColumnTypeName::Double
            | SqlColumnTypeName::Numeric { .. } => Token::Number("0".to_string()),
            SqlColumnTypeName::Boolean => Token::Ident("false".to_string()),
            SqlColumnTypeName::Alias { target, .. } => named(target),
            _ => Token::StringLiteral("0".to_string()),
        }
    }
    match ty {
        DeclType::Named(t) => named(t),
        _ => Token::StringLiteral("0".to_string()),
    }
}

/// 埋め込み SQL の構造解析口。`tokens` は文全体（`depth` 0・`is_root` 真）またはルート上のサブクエリ範囲
/// （スライスをコピーせず渡す。位置の記録がアドレス基準のため）。core が `sql::embedded_unit::parse_unit`
/// へ委譲して実装する（`core::plpgsql_validate`）。エラーは下位の解析器のものをそのまま返し、文言の
/// 丸めは呼び出し側（本モジュール）が行う。
pub(crate) trait EmbeddedSqlChecker {
    fn check_unit(
        &self,
        tokens: &[Token],
        depth: usize,
        is_root: bool,
    ) -> Result<UnitSummary, SqlSurfaceError>;
}

/// カタログを引かない既定の構造解析（`parse_unit` への委譲）。core の実装とテストが共有する。
pub(crate) struct StructuralChecker;

impl EmbeddedSqlChecker for StructuralChecker {
    fn check_unit(
        &self,
        tokens: &[Token],
        depth: usize,
        is_root: bool,
    ) -> Result<UnitSummary, SqlSurfaceError> {
        // 構文エラー以外の畳み込みエラー（0 除算等）は無視して文全体を走査させる。
        let _defer = crate::sql::const_expr::defer_all_non_syntax_errors();
        parse_unit(tokens, depth, is_root)
    }
}

/// 条件式の検証に使う仮の表名。許可リストの `WHERE` は `FROM` の無い `SELECT` では単一の述語しか受理
/// しないため、条件式は `SELECT 1 FROM <仮の表> WHERE <式>` の形で解析する（表は構造解析では常に存在
/// する扱い）。
const DUMMY_RELATION: &str = "plpgsql_validate_dummy_relation";

/// 1 文・式の構造解析で処理するサブクエリ範囲の上限（超過は `54001`。無制限に再帰しない）。
const MAX_UNITS: usize = 256;
/// 列専用位置の判定を位置ごとに行う上限（超過は `0A000`。解析回数が参照数に比例して膨らむのを防ぐ）。
const MAX_PER_SITE_CHECKS: usize = 64;

/// 下位の解析器が「定義時に拒否すべき」とするエラー（それ以外は late binding として先送りする）。
fn is_syntax_class(e: &SqlSurfaceError) -> bool {
    matches!(e.wire_code(), "42601" | "0A000" | "54000" | "54001")
}

fn sanitize(e: &SqlSurfaceError) -> SqlSurfaceError {
    super::sanitize_delegated(e, "an embedded SQL fragment of a PL/pgSQL body")
}

/// 分類の失敗。`Parse` は解析器のエラー（変数の位置に起因する失敗かの説明を試みる）、`Verdict` は
/// 分類側の判定（そのまま返す）。
enum Fail {
    Parse(SqlSurfaceError),
    Verdict(SqlSurfaceError),
}

fn verdict(detail: &'static str) -> Fail {
    Fail::Verdict(oos_err(detail))
}

/// 分類に必要な変数ごとの情報（本体の変数表の写し。分類中に変数表を書き換えるため分けて持つ）。
struct VarInfo {
    record_like: bool,
    dummy: Token,
}

struct Cx<'a> {
    checker: &'a dyn EmbeddedSqlChecker,
    vars: Vec<VarInfo>,
    nparams: usize,
}

impl Cx<'_> {
    fn record_like(&self, id: VarId) -> bool {
        self.vars.get(id.0 as usize).is_some_and(|v| v.record_like)
    }

    /// 検証用の置換トークン（呼び出し時に #2226 が置く型付きの値トークンと同じ種類）。
    fn dummy(&self, var: &VarRef) -> Token {
        match var {
            VarRef::Var(id) => self.vars.get(id.0 as usize).map_or_else(
                || Token::StringLiteral("0".to_string()),
                |v| v.dummy.clone(),
            ),
            VarRef::Field(..) => Token::StringLiteral("0".to_string()),
        }
    }
}

/// 分類の前処理済みの 1 本（正規化・`$n` の合成識別子化まで）。
struct Prepared {
    /// 正規化後のトークン列（`Token::Param` を保持。AST へ保存する側）。先頭に `prefix_len` 個の前置き。
    norm: Vec<Token>,
    /// `norm` の `Token::Param` を合成識別子（`$n`）へ置いた解析用の列（位置は `norm` と同じ）。
    analysis: Vec<Token>,
    /// 記録区間へ渡すスコープ内の名前（トークン上の綴りのまま。完全一致で照合される）。
    spelled: HashSet<String>,
    prefix_len: usize,
}

/// 分類結果。
struct Classified {
    tokens: Vec<Token>,
    sites: Vec<VarSite>,
    /// ルートの要約。ルート自体が先送りのエラー（late binding）で終わったときは `None`。
    summary: Option<UnitSummary>,
}

fn prepare(
    view: &ScopeView,
    prefix: &[Token],
    body: &[Token],
) -> Result<Prepared, SqlSurfaceError> {
    let mut all: Vec<Token> = prefix.to_vec();
    all.extend_from_slice(body);
    let known = view.known_names();
    let mut spelled: HashSet<String> = HashSet::new();
    for t in &all {
        match t {
            Token::Ident(n) if known.contains(&n.to_ascii_lowercase()) => {
                spelled.insert(n.clone());
            }
            Token::QualifiedIdent { qualifier, .. }
                if known.contains(&qualifier.to_ascii_lowercase()) =>
            {
                spelled.insert(qualifier.clone());
            }
            // 合成識別子 `$k` は字句解析器が作らない綴り（二重引用符付き識別子も `$` を含められず
            // `unsupported quoted identifier` で字句解析が失敗する）ので、利用者の識別子と衝突しない。
            Token::Param(k) => {
                spelled.insert(format!("${k}"));
            }
            _ => {}
        }
    }
    // `FETCH FIRST <変数> ROWS` の変数名を受理させるため、正規化も記録区間の下で行う（記録は使わない）。
    let guard = colref_sink::enter(&all, spelled.iter().cloned());
    let norm = normalize_statement(all);
    drop(guard);
    let norm = norm?;
    if norm.get(..prefix.len()) != Some(prefix) {
        return Err(oos_err(
            "unsupported statement shape in a PL/pgSQL embedded SQL fragment",
        ));
    }
    let analysis = norm
        .iter()
        .map(|t| match t {
            Token::Param(k) => Token::Ident(format!("${k}")),
            other => other.clone(),
        })
        .collect();
    Ok(Prepared {
        norm,
        analysis,
        spelled,
        prefix_len: prefix.len(),
    })
}

struct Run {
    summary: Option<UnitSummary>,
    report: ColrefReport,
    /// 先送りのエラー（late binding）で解析が途中で終わった単位があった（記録が欠けうる）。
    truncated: bool,
}

/// 記録区間の下で、ルートと、取り込まれたサブクエリ範囲（新しい範囲が出なくなるまで）を解析する。
fn run_units(
    cx: &Cx<'_>,
    root: &[Token],
    spelled: &HashSet<String>,
) -> Result<Run, SqlSurfaceError> {
    let guard = colref_sink::enter(root, spelled.iter().cloned());
    let inner = run_units_inner(cx, root);
    let report = guard.finish();
    let (summary, truncated) = inner?;
    Ok(Run {
        summary,
        report,
        truncated,
    })
}

fn run_units_inner(
    cx: &Cx<'_>,
    root: &[Token],
) -> Result<(Option<UnitSummary>, bool), SqlSurfaceError> {
    let mut truncated = false;
    let summary = match cx.checker.check_unit(root, 0, true) {
        Ok(s) => Some(s),
        Err(e) if is_syntax_class(&e) => return Err(e),
        Err(_) => {
            truncated = true;
            None
        }
    };
    let mut done: Vec<Range<usize>> = Vec::new();
    loop {
        let all = colref_sink::pending_ranges();
        let Some(next) = all.iter().find(|r| !done.contains(r)).cloned() else {
            break;
        };
        if done.len() >= MAX_UNITS {
            return Err(SqlSurfaceError::StatementTooComplex {
                detail: "too many subqueries in a PL/pgSQL embedded SQL fragment",
            });
        }
        let depth = unit_depth(&all, &next);
        let Some(slice) = root.get(next.clone()) else {
            return Err(oos_err(
                "unsupported statement shape in a PL/pgSQL embedded SQL fragment",
            ));
        };
        match cx.checker.check_unit(slice, depth, false) {
            Ok(_) => {}
            Err(e) if is_syntax_class(&e) => return Err(e),
            Err(_) => truncated = true,
        }
        done.push(next);
    }
    Ok((summary, truncated))
}

/// 範囲の入れ子の深さ（自分を囲む範囲の数 + 1）。
fn unit_depth(ranges: &[Range<usize>], r: &Range<usize>) -> usize {
    1 + ranges
        .iter()
        .filter(|o| *o != r && o.start <= r.start && r.end <= o.end)
        .count()
}

/// 解析器が拒否した原因が「リテラルしか受理しない位置の変数」かを確かめる。スコープ内の名前の識別子を
/// すべて数値・文字列のリテラルへ置いて、ルートと取り込み範囲のすべてが受理されるなら、変数の置き場所が
/// 非対応だったということなので `true`（呼び出し側が `0A000` にする）。スコープ内の名前が無い文は常に偽。
fn failure_is_variable_position(cx: &Cx<'_>, prep: &Prepared) -> bool {
    if prep.spelled.is_empty() {
        return false;
    }
    let none = HashSet::new();
    [
        Token::Number("0".to_string()),
        Token::StringLiteral("0".to_string()),
    ]
    .iter()
    .any(|lit| {
        let replaced: Vec<Token> = prep
            .analysis
            .iter()
            .map(|t| match t {
                Token::Ident(n) if prep.spelled.contains(n) => lit.clone(),
                other => other.clone(),
            })
            .collect();
        run_units(cx, &replaced, &none).is_ok()
    })
}

/// 記録のトークンを PL/pgSQL のスコープで変数へ解決する。解決できなければ `None`。
fn resolve_site(cx: &Cx<'_>, view: &ScopeView, tok: &Token) -> Option<VarRef> {
    match tok {
        Token::Ident(n) => match n.strip_prefix('$') {
            Some(k) => {
                let k: usize = k.parse().ok()?;
                (k >= 1 && k <= cx.nparams)
                    .then(|| VarRef::Var(VarId(u32::try_from(k - 1).unwrap_or(u32::MAX))))
            }
            None => view.lookup_var(&n.to_ascii_lowercase()).map(VarRef::Var),
        },
        Token::QualifiedIdent { qualifier, name } => {
            view.lookup_qualified(&|id| cx.record_like(id), qualifier, name)
        }
        _ => None,
    }
}

fn classify_prepared(cx: &Cx<'_>, view: &ScopeView, prep: &Prepared) -> Result<Classified, Fail> {
    let run = run_units(cx, &prep.analysis, &prep.spelled).map_err(Fail::Parse)?;
    if run.report.out_of_range {
        return Err(verdict(
            "unsupported statement shape for variable resolution in a PL/pgSQL body",
        ));
    }
    let mut sites: Vec<VarSite> = Vec::new();
    for rec in &run.report.records {
        let Some(tok) = prep.analysis.get(rec.pos) else {
            return Err(verdict("unsupported statement shape in a PL/pgSQL body"));
        };
        match resolve_site(cx, view, tok) {
            Some(var) => {
                if rec.pos < prep.prefix_len {
                    return Err(verdict("unsupported statement shape in a PL/pgSQL body"));
                }
                sites.push(VarSite {
                    pos: rec.pos,
                    role: rec.role,
                    var,
                });
            }
            // リテラル専用位置は、スコープ内の名前だけが記録される。変数に解決できない名前（ラベル名等）は
            // 受理しない。
            None if rec.role == Role::LiteralSlot => {
                return Err(verdict(
                    "a name cannot be used at this position in a PL/pgSQL embedded SQL fragment",
                ));
            }
            None => {}
        }
    }
    // `$n` は必ず記録されていなければならない（未記録は列参照でない位置にあり、置換できない）。
    for (i, t) in prep.norm.iter().enumerate() {
        if matches!(t, Token::Param(_)) && !sites.iter().any(|s| s.pos == i) {
            return Err(verdict(
                "a positional parameter cannot be used at this position in a PL/pgSQL body",
            ));
        }
    }
    // 解析が途中で終わった場合、未記録の変数名の綴りは列参照だったかを判定できない（fail-closed）。
    if run.truncated {
        let recorded: HashSet<usize> = run.report.records.iter().map(|r| r.pos).collect();
        for (i, t) in prep.analysis.iter().enumerate().skip(prep.prefix_len) {
            let named = match t {
                Token::Ident(n) => prep.spelled.contains(n),
                Token::QualifiedIdent { qualifier, .. } => prep.spelled.contains(qualifier),
                _ => false,
            };
            if named && !recorded.contains(&i) {
                return Err(verdict(
                    "a name cannot be classified in this PL/pgSQL embedded SQL fragment",
                ));
            }
        }
    }
    // 参照する変数の種類数の上限（超えた時点で打ち切る。無制限に集合を育てない）。
    let mut distinct: HashSet<VarId> = HashSet::new();
    for s in &sites {
        let (VarRef::Var(id) | VarRef::Field(id, _)) = &s.var;
        distinct.insert(*id);
        if distinct.len() > crate::plpgsql_limits::MAX_PLPGSQL_VARS_PER_STATEMENT {
            return Err(Fail::Verdict(too_big_err(
                "too many PL/pgSQL variables referenced in one statement",
            )));
        }
    }
    verify_positions(cx, prep, &run.report, &sites)?;
    let tokens = prep.norm.get(prep.prefix_len..).unwrap_or(&[]).to_vec();
    let sites = sites
        .into_iter()
        .map(|s| VarSite {
            pos: s.pos - prep.prefix_len,
            ..s
        })
        .collect();
    Ok(Classified {
        tokens,
        sites,
        summary: run.summary,
    })
}

/// 変数の位置が、値（型付きリテラル）を置いても構文として成立するかを確かめる。成立しない位置（列名が
/// そのまま構造になる窓の `PARTITION BY`／`ORDER BY`・`DISTINCT`・`RETURNING`・`IN (SELECT)` の左辺等）に
/// 変数があれば `0A000`。呼び出し時（#2226）に値を置いても成立しない形を、定義時に拒否する。
fn verify_positions(
    cx: &Cx<'_>,
    prep: &Prepared,
    report: &ColrefReport,
    sites: &[VarSite],
) -> Result<(), Fail> {
    if sites.is_empty() {
        return Ok(());
    }
    let n = prep.analysis.len();
    let mut replaced = prep.analysis.clone();
    for s in sites {
        if let Some(slot) = replaced.get_mut(s.pos) {
            *slot = cx.dummy(&s.var);
        }
    }
    // 速い経路: 全変数を型付きリテラルへ置いたまま、ルートと各範囲が成立するか。
    let mut ok = true;
    let units: Vec<(Range<usize>, usize, bool)> = std::iter::once((0..n, 0, true))
        .chain(
            report
                .subquery_ranges
                .iter()
                .map(|r| (r.clone(), unit_depth(&report.subquery_ranges, r), false)),
        )
        .collect();
    for (r, depth, is_root) in &units {
        let Some(slice) = replaced.get(r.clone()) else {
            return Err(verdict("unsupported statement shape in a PL/pgSQL body"));
        };
        if let Err(e) = cx.checker.check_unit(slice, *depth, *is_root) {
            if is_syntax_class(&e) {
                ok = false;
                break;
            }
        }
    }
    if ok {
        return Ok(());
    }
    // 遅い経路: 位置ごとに、数値・文字列のどちらかのリテラルで成立するか（他の変数は記録区間の下で
    // そのまま識別子として受理させる）。
    if sites.len() > MAX_PER_SITE_CHECKS {
        return Err(verdict(
            "too many variable references to validate in a PL/pgSQL embedded SQL fragment",
        ));
    }
    for s in sites {
        let (range, depth, is_root) = units
            .iter()
            .filter(|(r, _, _)| r.contains(&s.pos))
            .min_by_key(|(r, _, _)| r.end - r.start)
            .cloned()
            .unwrap_or((0..n, 0, true));
        let accepted = [
            Token::Number("0".to_string()),
            Token::StringLiteral("0".to_string()),
        ]
        .into_iter()
        .any(|lit| {
            let mut v = prep.analysis.clone();
            if let Some(slot) = v.get_mut(s.pos) {
                *slot = lit;
            }
            let guard = colref_sink::enter(&v, prep.spelled.iter().cloned());
            let r = match v.get(range.clone()) {
                Some(slice) => cx.checker.check_unit(slice, depth, is_root),
                None => Ok(UnitSummary {
                    kind: UnitKind::Select,
                    has_write_cte: false,
                }),
            };
            drop(guard);
            match r {
                Ok(_) => true,
                Err(e) => !is_syntax_class(&e),
            }
        });
        if !accepted {
            return Err(verdict(
                "a variable cannot be used at this position of a PL/pgSQL embedded SQL fragment",
            ));
        }
    }
    Ok(())
}

fn to_embedded_kind(k: UnitKind) -> EmbeddedKind {
    match k {
        UnitKind::Select => EmbeddedKind::Select,
        UnitKind::Insert => EmbeddedKind::Insert,
        UnitKind::Update => EmbeddedKind::Update,
        UnitKind::Delete => EmbeddedKind::Delete,
    }
}

/// 埋め込み SQL 文 1 個を分類する。トークン列を正規化後のものへ置き換え、変数参照・文種・書き込みの
/// 有無を確定する。
fn classify_sql(cx: &Cx<'_>, s: &mut EmbeddedSql) -> Result<(), SqlSurfaceError> {
    let view = s.scope.clone();
    let prep = prepare(&view, &[], &s.tokens)?;
    let c = match classify_prepared(cx, &view, &prep) {
        Ok(c) => c,
        Err(Fail::Verdict(e)) => return Err(e),
        Err(Fail::Parse(e)) => {
            return Err(if failure_is_variable_position(cx, &prep) {
                oos_err("a variable cannot be used at this position of a PL/pgSQL embedded SQL fragment")
            } else {
                sanitize(&e)
            })
        }
    };
    let starts_with_with = matches!(
        c.tokens.first(),
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("WITH")
    );
    let (kind, write_cte) = match c.summary {
        Some(sum) => (
            if s.kind == EmbeddedKind::Perform {
                EmbeddedKind::Perform
            } else {
                to_embedded_kind(sum.kind)
            },
            sum.has_write_cte,
        ),
        // ルートが late binding で終わった: `WITH` 始まりはデータ変更 CTE の有無を判定できない。
        None if starts_with_with => {
            return Err(oos_err(
                "a WITH statement could not be classified in a PL/pgSQL body",
            ))
        }
        None => (s.kind, false),
    };
    s.writes = matches!(
        kind,
        EmbeddedKind::Insert | EmbeddedKind::Update | EmbeddedKind::Delete
    ) || write_cte;
    s.kind = kind;
    s.tokens = c.tokens;
    s.var_refs = c.sites;
    Ok(())
}

/// 式 1 個を分類する。値の式は `SELECT <式>`、許可リストが `WHERE` の中でしか受理しない比較・論理式は
/// `SELECT 1 FROM <仮の表> WHERE <式>` の形で解析し、最初に受理された形の記録を採る。すべて拒否なら
/// 本来の構文誤りとして最初のエラー（`42601`）を返す。
fn classify_expr(cx: &Cx<'_>, e: &mut SqlExpr) -> Result<(), SqlSurfaceError> {
    let select_prefix = vec![Token::Keyword(Keyword::Select)];
    let cond_prefix = vec![
        Token::Keyword(Keyword::Select),
        Token::Number("1".to_string()),
        Token::Keyword(Keyword::From),
        Token::Ident(DUMMY_RELATION.to_string()),
        Token::Keyword(Keyword::Where),
    ];
    let prefixes: Vec<&Vec<Token>> = if e.cond {
        vec![&cond_prefix]
    } else {
        vec![&select_prefix, &cond_prefix]
    };
    let view = e.scope.clone();
    let mut first: Option<SqlSurfaceError> = None;
    let mut explained = false;
    for prefix in prefixes {
        let prep = prepare(&view, prefix, &e.tokens)?;
        match classify_prepared(cx, &view, &prep) {
            Ok(c) => {
                e.tokens = c.tokens;
                e.var_refs = c.sites;
                return Ok(());
            }
            Err(Fail::Verdict(err)) => return Err(err),
            Err(Fail::Parse(err)) => {
                explained = explained || failure_is_variable_position(cx, &prep);
                first.get_or_insert(err);
            }
        }
    }
    if explained {
        return Err(oos_err(
            "a variable cannot be used at this position of a PL/pgSQL embedded SQL fragment",
        ));
    }
    Err(first.map_or_else(
        || syn_err("invalid expression in a PL/pgSQL body"),
        |err| sanitize(&err),
    ))
}

fn walk_stmts(
    stmts: &mut [Stmt],
    vars: &mut [VarDecl],
    cx: &Cx<'_>,
) -> Result<(), SqlSurfaceError> {
    for s in stmts {
        match s {
            Stmt::Block(b) => walk_block(b, vars, cx)?,
            Stmt::Assign { expr, .. } => classify_expr(cx, expr)?,
            Stmt::If { branches, else_ } => {
                for (cond, st) in branches {
                    classify_expr(cx, cond)?;
                    walk_stmts(st, vars, cx)?;
                }
                if let Some(st) = else_ {
                    walk_stmts(st, vars, cx)?;
                }
            }
            Stmt::Case {
                selector,
                whens,
                else_,
            } => {
                if let Some(sel) = selector {
                    classify_expr(cx, sel)?;
                }
                for (exprs, st) in whens {
                    for e in exprs {
                        classify_expr(cx, e)?;
                    }
                    walk_stmts(st, vars, cx)?;
                }
                if let Some(st) = else_ {
                    walk_stmts(st, vars, cx)?;
                }
            }
            Stmt::Loop { body: st, .. } => walk_stmts(st, vars, cx)?,
            Stmt::While { cond, body: st, .. } => {
                classify_expr(cx, cond)?;
                walk_stmts(st, vars, cx)?;
            }
            Stmt::ForInt {
                lower,
                upper,
                step,
                body: st,
                ..
            } => {
                classify_expr(cx, lower)?;
                classify_expr(cx, upper)?;
                if let Some(e) = step {
                    classify_expr(cx, e)?;
                }
                walk_stmts(st, vars, cx)?;
            }
            Stmt::ForQuery {
                query, body: st, ..
            } => {
                classify_sql(cx, query)?;
                walk_stmts(st, vars, cx)?;
            }
            Stmt::Exit { when, .. } | Stmt::Continue { when, .. } => {
                if let Some(e) = when {
                    classify_expr(cx, e)?;
                }
            }
            Stmt::Return(e) | Stmt::ReturnNext(e) => {
                if let Some(e) = e {
                    classify_expr(cx, e)?;
                }
            }
            Stmt::ReturnQuery(q) | Stmt::Perform(q) | Stmt::Sql(q) => classify_sql(cx, q)?,
            Stmt::Execute { query, using, .. } => {
                classify_expr(cx, query)?;
                for e in using {
                    classify_expr(cx, e)?;
                }
            }
            Stmt::GetDiagnostics { .. } | Stmt::Null => {}
        }
    }
    Ok(())
}

fn walk_block(b: &mut Block, vars: &mut [VarDecl], cx: &Cx<'_>) -> Result<(), SqlSurfaceError> {
    for id in &b.decls {
        if let Some(d) = vars.get_mut(id.0 as usize).and_then(|v| v.default.as_mut()) {
            classify_expr(cx, d)?;
        }
    }
    walk_stmts(&mut b.stmts, vars, cx)
}

/// 本体の全ての埋め込み SQL・式を SQL 構文解析器の記録で分類し、変数参照（位置・役割・変数）・正規化後の
/// トークン列・文種・書き込みの有無を AST へ書き込む（ADR D3）。構文誤り・範囲外は定義時に拒否する。
/// 解析木の深さは `parse_body` が上限で抑えているため、再帰で辿ってよい。
pub(crate) fn validate_embedded(
    body: &mut Body,
    checker: &dyn EmbeddedSqlChecker,
) -> Result<(), SqlSurfaceError> {
    let nparams = body
        .vars
        .iter()
        .filter(|v| matches!(v.kind, super::ast::VarKind::Param(_)))
        .count();
    let cx = Cx {
        checker,
        vars: body
            .vars
            .iter()
            .map(|v| VarInfo {
                record_like: matches!(v.ty, DeclType::Record | DeclType::PercentRowType { .. }),
                dummy: dummy_token(&v.ty),
            })
            .collect(),
        nparams,
    };
    let Body { block, vars } = body;
    walk_block(block, vars, &cx)
}
