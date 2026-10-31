//! FROM 句の関数呼び出し `SELECT ... FROM f(<引数>...) [[AS] 別名 [(列名, ...)]] ...` の抽出
//! （Issue #2220・SQL-67 のポインタ。親 #2215。実行は `core::function_scan_exec`）。
//!
//! 役割: セッション最上位の入口（`CteEvaluation::AllowMaterialize`。`sql::allowlist::
//! validate_sql_tokens_impl` が `sql::derived_table::gate` の直後に呼ぶ）で、FROM が「集合返却関数の呼び出し
//! 1 件だけ」の `SELECT` を検出し、関数呼び出し部分を公開名 1 トークンへ置き換えた外側のトークン列と、
//! 関数名・引数のトークン列・別名・列名リストへ分解する。外側（WHERE・射影・ORDER BY・集計・LIMIT）は
//! 評価後射影形ビュー（`Statement::BufferedView`）の外側としてそのまま既存の経路が処理する
//! （第 2 の外側処理を作らない）。ビュー本文・cursor・COPY・CTAS・サブクエリ内側・`EXPLAIN`・CTE 本文は
//! この経路に入らず、従来どおり下流のパーサーが `42601` にする（fail-closed。派生表と同じ方針）。
//!
//! 未対応の形（JOIN／カンマ区切りの複数リレーション・`WITH ORDINALITY`・`ROWS FROM (...)`）は固定文言の
//! `0A000`。関数の解決（レジストリ参照）・引数の型検査・評価は本モジュールでは行わない（呼び出し段が
//! セッションの `UdfRegistry` で行い、他テナントの関数は不在と同じ応答になる）。
//!
//! RLS-10 (b) の不変条件: 本モジュールの型はテナント・`PolicyContext` を持たず、カタログにも触れない。

use std::sync::Arc;

use crate::plpgsql::host::PlScalar;
use crate::sql::allowlist::{
    is_set_operator_ident, parse_derived_alias_head, set_operator_is_followed_by_branch,
    split_parenthesized, SqlSurfaceError, ValidatedFunctionScan,
};
use crate::sql::cast::CastType;
use crate::sql::exec::ColumnMeta;
use crate::sql::lexer::{Keyword, Token};
use crate::sql::routine_fold::{check_arg, eval_arg, FoldMode, Folder};
use crate::sql::udf_call::MAX_UDF_PARAMS;
use crate::sql::udf_call::{RoutineDefinition, RoutineReturn, UdfRegistry};

/// 抽出結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FunctionScanParts {
    /// 呼び出す関数名（綴りのまま）。
    pub(crate) name: String,
    /// 引数ごとのトークン列（書かれた順。個数は `MAX_UDF_PARAMS` 以下）。
    pub(crate) arg_tokens: Vec<Vec<Token>>,
    /// 利用者が書いた別名。
    pub(crate) alias: Option<String>,
    /// 列名リスト（空ならリストなし）。
    pub(crate) column_aliases: Vec<String>,
    /// 関数呼び出し部分を公開名（別名、無ければ関数名）の 1 トークンへ置き換えた外側のトークン列。
    pub(crate) outer_tokens: Vec<Token>,
}

fn unsupported(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

fn is_ident(t: Option<&Token>, word: &str) -> bool {
    matches!(t, Some(Token::Ident(n)) if n.eq_ignore_ascii_case(word))
}

/// 深さ 0 の最初の FROM（`IS [NOT] DISTINCT FROM` を除く）の添字。深さ 0 に集合演算子があれば
/// `Ok(None)`（従来どおり既存パーサーが扱う）。走査は O(トークン数)。
fn find_top_level_from(tokens: &[Token]) -> Option<usize> {
    let mut depth: usize = 0;
    let mut from: Option<usize> = None;
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            // 集合演算の枝のどこかに関数呼び出しがあっても対象外（文全体を従来の経路へ）。
            Token::Ident(n) if depth == 0 && is_set_operator_ident(n) => {
                if set_operator_is_followed_by_branch(tokens, i) {
                    return None;
                }
            }
            Token::Keyword(Keyword::From) if depth == 0 && from.is_none() => {
                let after_distinct = i
                    .checked_sub(1)
                    .and_then(|p| tokens.get(p))
                    .is_some_and(|t| is_ident(Some(t), "DISTINCT"));
                if !after_distinct {
                    from = Some(i);
                }
            }
            _ => {}
        }
    }
    from
}

/// 深さ 0 の `a, b, c` を分割する（引数リストの内側。空なら 0 個）。個数は確保の前に
/// `MAX_UDF_PARAMS` で検査し、超過は `54000`。
fn split_args(inner: &[Token]) -> Result<Vec<Vec<Token>>, SqlSurfaceError> {
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Vec<Token>> = Vec::new();
    let mut cur: Vec<Token> = Vec::new();
    let mut depth: usize = 0;
    for tok in inner {
        match tok {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Punct(',') if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                if out.len() >= MAX_UDF_PARAMS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many function arguments",
                    ));
                }
                continue;
            }
            _ => {}
        }
        cur.push(tok.clone());
    }
    out.push(cur);
    Ok(out)
}

/// 最上位の `SELECT` の FROM が関数呼び出し 1 件なら分解して返す。該当しない文は `Ok(None)`
/// （呼び出し元は従来の検証へ進む）。形が崩れている・未対応の組み合わせは `42601`／`0A000`。
pub(crate) fn extract(tokens: &[Token]) -> Result<Option<FunctionScanParts>, SqlSurfaceError> {
    if !matches!(tokens.first(), Some(Token::Keyword(Keyword::Select))) {
        return Ok(None);
    }
    let Some(from) = find_top_level_from(tokens) else {
        return Ok(None);
    };
    let name_idx = from.saturating_add(1);
    // 後続のリレーション（`FROM t, f(1)`・`JOIN f(1)`）の関数呼び出しは未対応。FROM 句の範囲（深さ 0）
    // だけを調べる。`ROWS FROM (...)` も未対応。
    if is_ident(tokens.get(name_idx), "ROWS")
        && matches!(
            tokens.get(name_idx.saturating_add(1)),
            Some(Token::Keyword(Keyword::From))
        )
    {
        return Err(unsupported("ROWS FROM is not supported"));
    }
    let candidate = matches!(tokens.get(name_idx), Some(Token::Ident(_)))
        && matches!(
            tokens.get(name_idx.saturating_add(1)),
            Some(Token::Punct('('))
        );
    // 先頭のリレーションが関数呼び出しでない形（`FROM t, f(1)`・`JOIN f(1)` を含む）は従来の経路へ
    // （下流のパーサーが `42601`。既存の契約を変えない）。
    if !candidate {
        return Ok(None);
    }
    let Some(Token::Ident(name)) = tokens.get(name_idx) else {
        return Ok(None);
    };
    let open_idx = name_idx.saturating_add(1);
    let rest = tokens.get(open_idx..).unwrap_or(&[]);
    let (inner, after) = split_parenthesized(rest)?;
    let arg_tokens = split_args(inner)?;
    if is_ident(after.first(), "WITH") {
        return Err(unsupported("WITH ORDINALITY is not supported"));
    }
    let (alias, column_aliases, consumed) = parse_derived_alias_head(after)?;
    let tail = after.get(consumed..).unwrap_or(&[]);
    // 関数呼び出しの直後に別のリレーション（`, g(1)`・`JOIN ...`）が続く形は未対応。
    if matches!(tail.first(), Some(Token::Punct(',')))
        || tail
            .first()
            .is_some_and(|t| matches!(t, Token::Ident(n) if is_join_word(n)))
    {
        return Err(unsupported(
            "a function in FROM cannot be combined with other relations",
        ));
    }
    let public = alias.clone().unwrap_or_else(|| display_function_name(name));
    let mut outer: Vec<Token> = Vec::with_capacity(tokens.len());
    outer.extend(tokens.iter().take(from.saturating_add(1)).cloned());
    outer.push(Token::Ident(public));
    outer.extend(tail.iter().cloned());
    Ok(Some(FunctionScanParts {
        name: name.clone(),
        arg_tokens,
        alias,
        column_aliases,
        outer_tokens: outer,
    }))
}

/// スキーマ修飾の呼び出しが解決器で内部キー（`#<id>.<name>`）へ書き換わっていても、クライアントへ出す
/// 既定の列名・relation 名には関数名だけを使う（schema id を RowDescription へ漏らさない）。
fn display_function_name(name: &str) -> String {
    match name.strip_prefix('#').and_then(|r| r.split_once('.')) {
        Some((_, bare)) => bare.to_string(),
        None => name.to_string(),
    }
}

fn is_join_word(n: &str) -> bool {
    ["JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL"]
        .iter()
        .any(|w| n.eq_ignore_ascii_case(w))
}

/// 解決済みの関数呼び出し（Describe と Execute が同じ [`plan`] を通り、解決エラーがそろう）。
#[derive(Debug, Clone)]
pub(crate) struct ScanPlan {
    pub(crate) def: Arc<RoutineDefinition>,
    /// 出力列（公開名・宣言型）。名前は `SETOF` なら別名（無ければ関数名）、`TABLE` なら宣言名。
    pub(crate) columns: Vec<(String, CastType)>,
}

/// 関数呼び出し `scan` をレジストリで解決し、引数の個数・型を検査して出力列を決める（本体は実行しない）。
/// 関数が無い（他テナントの関数・不在・個数不一致）は区別せず `42883`。集合返却でない関数
/// （宣言的 UDF・WASM・スカラー・トリガ）と引数中のルーチン呼び出しは固定の `0A000`。
pub(crate) fn plan(
    registry: &UdfRegistry,
    scan: &ValidatedFunctionScan,
) -> Result<ScanPlan, SqlSurfaceError> {
    let missing = || {
        SqlSurfaceError::undefined_function(
            "function does not exist or its argument count does not match",
        )
    };
    let Some(def) = registry.get_routine(&scan.name).cloned() else {
        // 同名の宣言的 UDF・WASM UDF は FROM の関数としては使えない（集合を返さない）。
        if registry.is_name_taken(&scan.name.to_ascii_lowercase())
            || crate::sql::udf_call::is_reserved_function_name(&scan.name)
        {
            return Err(unsupported(
                "only set-returning LANGUAGE sql functions are supported in FROM",
            ));
        }
        return Err(missing());
    };
    let RoutineReturn::Set(cols) = &def.ret else {
        return Err(unsupported(
            "only set-returning LANGUAGE sql functions are supported in FROM",
        ));
    };
    if scan.args.len() != def.param_types.len() {
        return Err(missing());
    }
    for arg in &scan.args {
        let mut detect = Folder::new(registry, FoldMode::Detect);
        detect.fold(arg, false)?;
        if detect.found() {
            return Err(unsupported(
                "a function call as an argument of a function in FROM is not supported",
            ));
        }
    }
    for (arg, declared) in scan.args.iter().zip(def.param_types.iter()) {
        check_arg(registry, arg, declared)?;
    }
    let default_name = scan
        .alias
        .clone()
        .unwrap_or_else(|| display_function_name(&scan.name).to_ascii_lowercase());
    let columns = cols
        .iter()
        .map(|(n, t)| (n.clone().unwrap_or_else(|| default_name.clone()), t.clone()))
        .collect();
    Ok(ScanPlan { def, columns })
}

/// 出力列の結果列メタ（`ColumnMeta::Scalar`）。型が結果列型に写せなければ `0A000`。
pub(crate) fn column_metas(plan: &ScanPlan) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    plan.columns
        .iter()
        .map(|(name, ty)| {
            let ty = ty.result_column_type().ok_or_else(|| {
                unsupported("unsupported result column type of a function in FROM")
            })?;
            Ok(ColumnMeta::Scalar {
                name: name.clone(),
                ty,
            })
        })
        .collect()
}

/// 引数を宣言型へ評価する（型検査済みの `plan` の後に呼ぶ）。`STRICT` で NULL 引数があれば `None`
/// （本体を実行せず空集合にする）。
pub(crate) fn eval_args(
    registry: &UdfRegistry,
    scan: &ValidatedFunctionScan,
    plan: &ScanPlan,
) -> Result<Option<Vec<PlScalar>>, SqlSurfaceError> {
    let mut values: Vec<PlScalar> = Vec::with_capacity(scan.args.len());
    for (arg, declared) in scan.args.iter().zip(plan.def.param_types.iter()) {
        values.push(eval_arg(registry, arg, declared)?);
    }
    if plan.def.attrs.strict && values.iter().any(PlScalar::is_null) {
        return Ok(None);
    }
    Ok(Some(values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn parts(sql: &str) -> Option<FunctionScanParts> {
        extract(&tokenize(sql).expect("tokenize")).expect("extract")
    }

    fn code(sql: &str) -> &'static str {
        extract(&tokenize(sql).expect("tokenize"))
            .expect_err(sql)
            .wire_code()
    }

    /// SQL-67: 関数呼び出し 1 件の FROM を分解し、公開名（別名または関数名）へ置き換える。
    #[test]
    fn extracts_single_function_relation() {
        let p = parts("SELECT * FROM f(1, 'a') WHERE x > 1").expect("found");
        assert_eq!(p.name, "f");
        assert_eq!(p.arg_tokens.len(), 2);
        assert_eq!(p.alias, None);
        assert!(p
            .outer_tokens
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n == "f")));
        assert!(!p
            .outer_tokens
            .iter()
            .any(|t| matches!(t, Token::Punct('('))));
        let p = parts("SELECT t.a FROM f() AS t(a, b)").expect("found");
        assert_eq!(p.arg_tokens.len(), 0);
        assert_eq!(p.alias.as_deref(), Some("t"));
        assert_eq!(p.column_aliases, vec!["a".to_string(), "b".to_string()]);
    }

    /// 関数呼び出しでない FROM・他の位置の括弧・集合演算は対象外（従来の経路へ）。
    #[test]
    fn ignores_other_shapes() {
        for sql in [
            "SELECT * FROM t",
            "SELECT * FROM t WHERE a IN (SELECT a FROM t)",
            "SELECT extract(year FROM ts) FROM t",
            "SELECT a IS DISTINCT FROM lower(b) FROM t",
            "SELECT 1",
            "SELECT * FROM t UNION SELECT * FROM u",
            "SELECT count(*) FROM t UNION ALL SELECT upper(a), count(*) FROM t GROUP BY upper(a)",
            // 先頭のリレーションが関数でない形は従来の経路（下流が `42601`）へ。
            "SELECT * FROM t, f(1)",
            "SELECT * FROM t JOIN f(1) ON true",
        ] {
            assert!(parts(sql).is_none(), "{sql}");
        }
    }

    /// 未対応の組み合わせは固定の `0A000`。
    #[test]
    fn rejects_unsupported_combinations() {
        for sql in [
            "SELECT * FROM f(1), t",
            "SELECT * FROM f(1) JOIN t ON true",
            "SELECT * FROM f(1) WITH ORDINALITY",
            "SELECT * FROM ROWS FROM (f(1))",
        ] {
            assert_eq!(code(sql), "0A000", "{sql}");
        }
    }

    /// 引数の個数は確保の前に上限で検査する（`54000`）。
    #[test]
    fn argument_count_is_bounded() {
        let args = vec!["1"; MAX_UDF_PARAMS + 1].join(", ");
        assert_eq!(code(&format!("SELECT * FROM f({args})")), "54000");
    }
}
