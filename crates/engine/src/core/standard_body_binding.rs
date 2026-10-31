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
use crate::sql::embedded_unit::{parse_unit_with_lookup, UnitSummary};
use crate::sql::lexer::{Keyword, Token};
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
    core: &'a EngineCore,
    lookup: RecordingLookup<'a>,
    missing: RefCell<Option<String>>,
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
        let result = parse_unit_with_lookup(tokens, depth, is_root, &self.lookup);
        match &result {
            Err(SqlSurfaceError::UndefinedTable { name }) => {
                self.missing
                    .borrow_mut()
                    .get_or_insert_with(|| name.clone());
            }
            // データ変更 CTE を含む最上位の `WITH` は、構造専用の解析では判定できないためカタログ付きの
            // 解析（`EngineCore` の検査器）へ回す（表が無ければそちらが `42P01` を返す）。
            Err(_) if is_root && looks_like_data_modifying_with(tokens) => {
                let fallback = EmbeddedSqlChecker::check_unit(self.core, tokens, depth, is_root);
                if let Err(SqlSurfaceError::UndefinedTable { name }) = &fallback {
                    self.missing
                        .borrow_mut()
                        .get_or_insert_with(|| name.clone());
                }
                return fallback;
            }
            _ => {}
        }
        result
    }
}

/// 文のトークン列に現れる DML の対象表名（`INSERT INTO <t>`・`UPDATE [ONLY] <t>`・
/// `DELETE FROM [ONLY] <t>`）を、データ変更 CTE（`WITH d AS (DELETE FROM t ...)`）の内側も含めて
/// 位置を問わず集める。構造専用の解析は DML の対象表を引かないため、標準形の定義時に明示的に存在を
/// 確認し依存辺へ載せる。`ON CONFLICT DO UPDATE SET`・`FOR UPDATE` の `UPDATE` は対象表を取らないので除く。
fn dml_targets(tokens: &[Token]) -> Vec<&str> {
    let is_with = matches!(tokens.first(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("WITH"));
    let word = |i: Option<usize>, w: &[&str]| matches!(i.and_then(|p| tokens.get(p)), Some(Token::Ident(x)) if w.iter().any(|k| x.eq_ignore_ascii_case(k)));
    (0..tokens.len())
        .filter(|&i| {
            // 文の先頭、または CTE 本体の `AS [[NOT] MATERIALIZED] (` の直後、`WITH` 文の末尾の主文
            // （CTE の閉じ括弧の直後）だけを文頭とみなす。列名・別名の `update` 等を DML と誤認しない。
            let prev = i.checked_sub(1);
            i == 0
                || (matches!(prev.and_then(|p| tokens.get(p)), Some(Token::Punct('(')))
                    && word(i.checked_sub(2), &["AS", "MATERIALIZED"]))
                || (is_with && matches!(prev.and_then(|p| tokens.get(p)), Some(Token::Punct(')'))))
        })
        .filter_map(|i| dml_target_at(tokens, i))
        .collect()
}

/// 位置 `at` から始まる DML の対象表名（[`dml_targets`] の 1 位置分）。
fn dml_target_at(tokens: &[Token], at: usize) -> Option<&str> {
    let tokens_all = tokens;
    let tokens = tokens.get(at..)?;
    let prev_is = |w: &str| {
        at.checked_sub(1)
            .and_then(|p| tokens_all.get(p))
            .is_some_and(|t| matches!(t, Token::Ident(x) if x.eq_ignore_ascii_case(w)))
    };
    let ident_at = |i: usize| match tokens.get(i) {
        Some(Token::Ident(n)) => Some(n.as_str()),
        _ => None,
    };
    let is = |i: usize, w: &str| matches!(tokens.get(i), Some(Token::Ident(x)) if x.eq_ignore_ascii_case(w));
    let skip_only = |i: usize| if is(i, "ONLY") { i + 1 } else { i };
    if is(0, "INSERT") && is(1, "INTO") {
        return ident_at(2);
    }
    if is(0, "UPDATE") {
        if prev_is("DO") || prev_is("FOR") {
            return None;
        }
        return ident_at(skip_only(1)).filter(|n| !n.eq_ignore_ascii_case("SET"));
    }
    if is(0, "DELETE") && matches!(tokens.get(1), Some(Token::Keyword(Keyword::From))) {
        return ident_at(skip_only(2));
    }
    None
}

/// 文のトークン列に現れる `名前(`（関数呼び出しの形）の名前を集める（小文字化。重複は呼び出し側で除く）。
/// `INSERT INTO f(a)` の対象表と列リスト、`AS x(a, b)` の別名と列別名リストは呼び出しではないので除く。
fn call_names(tokens: &[Token]) -> impl Iterator<Item = String> + '_ {
    tokens.iter().enumerate().filter_map(move |(i, t)| {
        let (Token::Ident(n), Some(Token::Punct('('))) = (t, tokens.get(i + 1)) else {
            return None;
        };
        let decl = matches!(
            i.checked_sub(1).and_then(|p| tokens.get(p)),
            Some(Token::Ident(w))
                if ["INTO", "AS", "UPDATE", "ONLY"].iter().any(|k| w.eq_ignore_ascii_case(k))
        );
        (!decl).then(|| n.to_ascii_lowercase())
    })
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
            core: self,
            lookup: RecordingLookup {
                storage: &self.storage,
                seen: RefCell::new(BTreeSet::new()),
            },
            missing: RefCell::new(None),
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
        for stmt in &body.block.stmts {
            let Stmt::Sql(q) = stmt else { continue };
            for target in dml_targets(&q.tokens) {
                if !checker.lookup.table_exists(target)? {
                    return Err(SqlSurfaceError::UndefinedTable {
                        name: target.to_string(),
                    });
                }
                relations.insert(target.to_string());
            }
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
