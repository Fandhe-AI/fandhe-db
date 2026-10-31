//! SQL 標準形の関数本体（`RETURN`・`BEGIN ATOMIC`）の定義時の名前解決と依存の収集
//! （Issue #2219・SQL-67 (2)・SQL-66 (3)。設計記録: `docs/design/plpgsql-subset.md`
//! 「Issue #2219 追記」）。
//!
//! 役割: `core.rs` の `Statement::CreateFunction` 腕が標準形の本体を保存する前に
//! [`EngineCore::validate_sql_standard_function_definition`] を呼ぶ。`LANGUAGE sql` の文字列本体は
//! 構造専用の解析（表は常に存在する扱い＝late binding）だが、標準形は PostgreSQL と同じく定義時に
//! 名前を解決する。実カタログを引く [`RecordingLookup`] を `sql::embedded_unit::parse_unit_with_lookup`
//! へ渡して表・ビューの不在を `42P01` にし、参照した relation と、本体が呼ぶ永続関数を依存辺
//! （`catalog::function_dependency`）として返す。辺は `sql::udf_ddl` が write txn の中で再検証して
//! 記録し、参照先の DROP・改名・列変更が `2BP01` で止める。
//!
//! 列集合の照会は委譲しない（引数名の識別子が列として検証され `42703` に化けるのを避ける。列・引数名の
//! 検証は呼び出し時の完全な検証が担うので fail-open にはならない）。取りこぼした依存（入れ子の DML
//! 文・列挙できない呼び出し等）も、呼び出し時に `42P01`／`42883` になる（fail-closed）。エラー文言に
//! 他テナントの情報は含めない（`UndefinedTable` の名前は呼び出し元が書いた識別子そのもの）。

use std::cell::RefCell;
use std::collections::BTreeSet;

use super::plpgsql_validate::looks_like_data_modifying_with;
use super::EngineCore;
use crate::catalog::function_dependency::{DependencyKind, FunctionDependency};
use crate::plpgsql::ast::{ParseOptions, ReturnShape, Stmt};
use crate::plpgsql::embedded::{validate_embedded, EmbeddedSqlChecker};
use crate::plpgsql::{check_result_shape, parse_sql_function_body};
use crate::sql::allowlist::{SqlSurfaceError, TableLookup};
use crate::sql::cast::CastType;
use crate::sql::embedded_unit::{parse_unit_with_target, UnitSummary};
use crate::sql::lexer::Token;
use crate::sql::statement_splitter::is_non_builtin_call_at;
use crate::storage::Storage;

/// 実カタログへ委譲しつつ、存在を確認できた relation の名前を記録する [`TableLookup`]。
/// 委譲するのは存在の照会だけ（ビューの展開・列集合は既定の `None`）。
struct RecordingLookup<'a> {
    storage: &'a Storage,
    seen: RefCell<BTreeSet<String>>,
}

impl RecordingLookup<'_> {
    fn record(&self, name: &str) {
        self.seen.borrow_mut().insert(name.to_string());
    }
}

impl TableLookup for RecordingLookup<'_> {
    /// 表・ビュー・マテリアライズドビューのいずれかとして存在すれば真（記録する）。ビューは展開せず
    /// 表と同じ「存在する名前」として扱う（`view_definition` は既定の `None`）。展開すると本体のトークン列が
    /// 書き換わり、変数参照の位置の記録（`colref_sink`）が範囲外になって引数を参照できなくなるため。
    /// ビューの中身の検証は呼び出し時の完全な検証が担う。
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        let exists = TableLookup::table_exists(self.storage, name)?
            || TableLookup::view_definition(self.storage, name)?.is_some()
            || TableLookup::materialized_view_columns(self.storage, name)?.is_some();
        if exists {
            self.record(name);
        }
        Ok(exists)
    }
}

/// 定義時に名前を解決する [`EmbeddedSqlChecker`]。`EngineCore` の構造専用の検査器と同じ流れ（構文
/// 級以外のエラーは `validate_embedded` が late binding として先送りする）だが、表の照会に
/// [`RecordingLookup`] を使う。解決できない表（`42P01`）は先送りされて失われないよう `missing` に残す。
struct BindingChecker<'a> {
    lookup: RecordingLookup<'a>,
    missing: RefCell<Option<String>>,
    /// 解析器が確定した DML 文の対象表名（`INSERT INTO`・`UPDATE`・`DELETE FROM` の対象）。
    /// 綴りの走査ではなく解析木から取るので、`UPDATE only SET ...`（表 `only`）や引用識別子の
    /// `"SET"` も取りこぼさない。定義時の存在確認と依存辺の記録に使う。
    targets: RefCell<BTreeSet<String>>,
}

impl EmbeddedSqlChecker for BindingChecker<'_> {
    fn check_unit(
        &self,
        tokens: &[Token],
        depth: usize,
        is_root: bool,
    ) -> Result<UnitSummary, SqlSurfaceError> {
        // 構文エラー以外の畳み込みエラー（0 除算等）は無視して文全体を走査させる（構造専用の検査器と同じ）。
        let _defer = crate::sql::const_expr::defer_all_non_syntax_errors();
        // データ変更 CTE は入れ子の DML の対象表を依存辺へ記録しきれないため、標準形の定義時には
        // 対象表の存否によらず拒否する（fail-closed。保存して依存を取りこぼす経路を作らない）。
        if looks_like_data_modifying_with(tokens) {
            return Err(SqlSurfaceError::feature_not_supported(
                "data-modifying WITH in a standard function body is not supported",
            ));
        }
        let result = parse_unit_with_target(tokens, depth, is_root, &self.lookup);
        match result {
            Ok((summary, target)) => {
                // データ変更 CTE は上の目印を外れても解析木で判定できる。保存して依存を取りこぼさない
                // よう同じく拒否する（fail-closed）。
                if summary.has_write_cte {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "data-modifying WITH in a standard function body is not supported",
                    ));
                }
                if let Some(t) = target {
                    self.targets.borrow_mut().insert(t);
                }
                Ok(summary)
            }
            Err(e) => {
                if let SqlSurfaceError::UndefinedTable { name } = &e {
                    self.missing
                        .borrow_mut()
                        .get_or_insert_with(|| name.clone());
                }
                Err(e)
            }
        }
    }
}

/// 文のトークン列に現れる関数呼び出し（`名前(`）の名前を集める（小文字化。重複は呼び出し側で除く）。
/// 構文語（`x IN (`・`WHERE EXISTS (SELECT`・`INSERT ... VALUES (` 等）は、`statement_splitter` の
/// 判定（[`is_non_builtin_call_at`]。構文位置と確認できる語だけ除外し、曖昧なものは呼び出し扱い）を
/// 共有して除くので、同名の永続関数への依存にならない。引用付きの名前（`"f"(`）は構文語になりえない
/// ので常に呼び出し。`INSERT INTO f(a)` の対象表と列リスト、`AS x(a, b)` の別名と列別名リストは
/// 呼び出しではないので除く。
fn call_names(tokens: &[Token]) -> impl Iterator<Item = String> + '_ {
    tokens.iter().enumerate().filter_map(move |(i, t)| {
        if !matches!(tokens.get(i + 1), Some(Token::Punct('('))) {
            return None;
        }
        let name = match t {
            Token::QuotedNull(n) => n,
            Token::Ident(n)
                if is_non_builtin_call_at(tokens, i) && !is_values_clause(tokens, i) =>
            {
                n
            }
            _ => return None,
        };
        if is_cte_declaration(tokens, i) {
            return None;
        }
        let decl = matches!(
            i.checked_sub(1).and_then(|p| tokens.get(p)),
            Some(Token::Ident(w))
                if ["INTO", "AS", "UPDATE", "ONLY"].iter().any(|k| w.eq_ignore_ascii_case(k))
        );
        (!decl).then(|| name.to_ascii_lowercase())
    })
}

/// 位置 `at` の `VALUES (` が行値の句と、構文位置で厳密に確認できるか。確認できる形は次の 3 つだけで、
/// それ以外は呼び出し（依存として記録＝fail-closed）として扱う。`statement_splitter` の共有判定は
/// `VALUES` を式の先頭の呼び出しと区別できないため常に呼び出し扱いにするので、ここで追加で除く。
///   - `INSERT INTO <表名> VALUES (`・`INSERT INTO <表名> (<列リスト>) VALUES (`（トークン位置で照合）
///   - `FROM (VALUES (`・`JOIN (VALUES (`（開き括弧の直後）
///   - `UNION|INTERSECT|EXCEPT [ALL] VALUES (`
fn is_values_clause(tokens: &[Token], at: usize) -> bool {
    let word = |j: usize, ws: &[&str]| matches!(tokens.get(j), Some(Token::Ident(x)) if ws.iter().any(|w| x.eq_ignore_ascii_case(w)));
    if !word(at, &["VALUES"]) {
        return false;
    }
    let Some(prev) = at.checked_sub(1) else {
        return false;
    };
    // `INSERT INTO <表名>` の直後、または列リストの閉じ括弧の直後。
    if word(0, &["INSERT"])
        && word(1, &["INTO"])
        && matches!(tokens.get(2), Some(Token::Ident(_) | Token::QuotedNull(_)))
    {
        if at == 3 {
            return true;
        }
        if matches!(tokens.get(3), Some(Token::Punct('('))) {
            let mut depth = 0usize;
            for (j, t) in tokens.iter().enumerate().skip(3) {
                match t {
                    Token::Punct('(') => depth = depth.saturating_add(1),
                    Token::Punct(')') => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            if j == prev {
                                return true;
                            }
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    match tokens.get(prev) {
        Some(Token::Punct('(')) => {
            let before = prev.checked_sub(1);
            matches!(
                before.and_then(|p| tokens.get(p)),
                Some(Token::Keyword(crate::sql::lexer::Keyword::From))
            ) || before.is_some_and(|p| word(p, &["JOIN"]))
        }
        Some(Token::Ident(x)) if x.eq_ignore_ascii_case("ALL") => prev
            .checked_sub(1)
            .is_some_and(|p| word(p, &["UNION", "INTERSECT", "EXCEPT"])),
        Some(Token::Ident(x)) => ["UNION", "INTERSECT", "EXCEPT"]
            .iter()
            .any(|w| x.eq_ignore_ascii_case(w)),
        _ => false,
    }
}

/// 位置 `at` の `名前(` が CTE の列名リスト付き宣言（`WITH f(a, b) AS [[NOT] MATERIALIZED] (`）か。
/// 対応する `)` の直後が `AS` で、その次が `(`・`NOT`・`MATERIALIZED` のとき宣言とみなす
/// （関数呼び出しの直後にこの形は現れない）。
fn is_cte_declaration(tokens: &[Token], at: usize) -> bool {
    let mut depth = 0usize;
    let mut close = None;
    for (j, t) in tokens.iter().enumerate().skip(at + 1) {
        match t {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            _ => {}
        }
    }
    let word = |j: usize, w: &str| matches!(tokens.get(j), Some(Token::Ident(x)) if x.eq_ignore_ascii_case(w));
    let Some(c) = close else { return false };
    word(c + 1, "AS")
        && (matches!(tokens.get(c + 2), Some(Token::Punct('(')))
            || word(c + 2, "NOT")
            || word(c + 2, "MATERIALIZED"))
}

impl EngineCore {
    /// SQL 標準形の関数本体を定義時に検証し、依存辺を返す（保存はしない。Issue #2219）。
    ///
    /// 本体を `LANGUAGE sql` の本体と同じ経路（`parse_sql_function_body`・`validate_embedded`・
    /// `check_result_shape`）で検証しつつ、実カタログで名前を解決する。不在の表・ビューは `42P01`。
    /// 戻り値は参照した表・ビュー・マテリアライズドビューと、本体が呼ぶ `tenant_id` の永続関数。
    /// 自己参照・循環・参照先の現存の再検証は、カタログへ書く write txn の中で行う
    /// （`Storage::create_function_with_deps`・`replace_function_with_deps`）。
    /// 呼び出し元は `core.rs` の `Statement::CreateFunction` 腕（`require_ddl_permission` の後・
    /// カタログ書き込みの前）。
    pub(crate) fn validate_sql_standard_function_definition(
        &self,
        tenant_id: &str,
        own_name: &str,
        params: &[String],
        param_types: &[Option<CastType>],
        body_source: &str,
    ) -> Result<Vec<FunctionDependency>, SqlSurfaceError> {
        let decls = super::function_define::param_decls(params, param_types)?;
        let limits = &self.plpgsql_limits;
        let opts = ParseOptions::new(decls, ReturnShape::Scalar)
            .with_limits(limits.max_nesting_depth(), limits.max_body_bytes());
        let mut body = parse_sql_function_body(body_source, &opts)?;
        let checker = BindingChecker {
            lookup: RecordingLookup {
                storage: &self.storage,
                seen: RefCell::new(BTreeSet::new()),
            },
            missing: RefCell::new(None),
            targets: RefCell::new(BTreeSet::new()),
        };
        let validated = validate_embedded(&mut body, &checker);
        // 解決できなかった表は、他の失敗（先送りされた解析の打ち切りに由来する拒否）より優先して返す。
        if let Some(name) = checker.missing.borrow_mut().take() {
            return Err(SqlSurfaceError::UndefinedTable { name });
        }
        validated?;
        check_result_shape(&body)?;

        let mut relations = checker.lookup.seen.borrow().clone();
        let mut functions: BTreeSet<String> = BTreeSet::new();
        // DML の対象表は構造専用の解析では引かれないので、解析器が確定した名前で存在を確認する
        // （`exists` の副作用で `relations` と同じ記録に載る）。
        for target in checker.targets.borrow().iter() {
            if !checker.lookup.table_exists(target)? {
                return Err(SqlSurfaceError::UndefinedTable {
                    name: target.clone(),
                });
            }
            relations.insert(target.clone());
        }
        for stmt in &body.block.stmts {
            let Stmt::Sql(q) = stmt else { continue };
            for name in call_names(&q.tokens) {
                // カタログの照会失敗は依存を取りこぼさないよう拒否する（fail-closed）。
                let overloads = self.storage.list_overloads(tenant_id, &name).map_err(|_| {
                    SqlSurfaceError::Internal {
                        detail: "function catalog operation failed".to_string(),
                    }
                })?;
                // 新規作成では定義対象がまだカタログに無く `overloads` が空になるため、自己参照は
                // 存在判定と独立に拾う（辺に載せ、カタログ側の write txn 内の検査が `0A000` で拒否する）。
                if !overloads.is_empty() || name.eq_ignore_ascii_case(own_name) {
                    functions.insert(name);
                }
            }
        }
        let mut deps: Vec<FunctionDependency> = relations
            .into_iter()
            .map(|key| FunctionDependency {
                kind: DependencyKind::Relation,
                key,
            })
            .collect();
        deps.extend(functions.into_iter().map(|key| FunctionDependency {
            kind: DependencyKind::Function,
            key,
        }));
        Ok(deps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn values_calls(sql: &str) -> bool {
        let tokens = tokenize(sql).expect("tokenize");
        let found = call_names(&tokens).any(|n| n == "values");
        found
    }

    /// `VALUES (` を句と確定できる構文位置（INSERT の表名・列リストの直後、`FROM (`・`JOIN (` の直後、
    /// 集合演算の直後）だけが依存から除かれ、INSERT ... SELECT 内の呼び出しは記録される。
    #[test]
    fn values_clause_requires_a_confirmed_syntax_position() {
        for clause in [
            "INSERT INTO t VALUES (1)",
            "INSERT INTO t (a, b) VALUES (1, 2)",
            "SELECT a FROM (VALUES (1)) v",
            "SELECT 1 UNION ALL VALUES (1)",
            "SELECT 1 EXCEPT VALUES (1)",
            "SELECT t.a FROM t JOIN (VALUES (1)) v ON t.a = v.column1",
        ] {
            assert!(!values_calls(clause), "{clause}");
        }
        for call in [
            "INSERT INTO t SELECT a FROM t JOIN u ON values(1) = u.a",
            "INSERT INTO t SELECT a FROM t GROUP BY a HAVING values(1) > 0",
            "INSERT INTO t SELECT ALL values(1)",
            "INSERT INTO t (a) SELECT values(1)",
            "SELECT ALL values(1)",
            "SELECT 1 WHERE x = values(1)",
        ] {
            assert!(values_calls(call), "{call}");
        }
    }
}
