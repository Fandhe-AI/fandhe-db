//! 相関 `EXISTS` の述語分解と残余述語の束縛（SQL-61 ポインタ、Issue #1842、親 #1840。
//! RLS-10 ポインタ）。
//!
//! 責務境界: `sql::subquery` が肯定形の `EXISTS (SELECT ...)` の内側で外側の列への参照
//! （[`crate::sql::correlation::Correlation`]）を検出したとき、内側 `WHERE` の連言を本モジュールの
//! [`analyze`] が次の 3 つへ分類する。内側の実体化・走査・プローブ組み立て（実行を伴う部分）は
//! `sql::subquery` と `sql::join::semi` が担い、本モジュールはスキーマだけを見る（行データは読まない。
//! 分類が他テナントの可視行に依存しない）。
//! - 内側だけを参照する連言 → 内側の実体化へ押し下げる（入れ子のサブクエリを含む）。
//! - 等価キー: `内側の裸の列 = 外側参照`（左右どちらでもよい）で、両辺の型クラスが一致するもの。
//!   `sql::join::values::key_class` を再利用し、結合キーの型規則を 2 系統にしない。
//! - 残余: 上記以外で外側を参照するもの（非等価・外側と内側が混ざった `OR`・キーにできない型の等価）。
//!   内側スキーマに外側参照ごとの列を足した合成スキーマで既存の `bind_where_predicates` へ束縛し、
//!   述語評価器を再利用する（第 2 の評価器を作らない）。
//!
//! fail-closed の範囲: 外側参照を含むサブクエリ述語（`IN`／スカラー等の相関は #1844 の範囲）、
//! 直近の外側より外への参照、`WHERE` 以外（投影・`ORDER BY`）の外側参照、述語評価に使えない
//! 列型は、静的文字列のエラー（`42601`／`0A000`）で拒否する。エラー文言に予約接頭辞・行データを
//! 含めない。
//!
//! 上限: [`CorrelatedCtx`] が 1 文ごとに共有する再評価・比較・入力行・バイトの予算を保持する
//! （DoS 対策。超過は `54000` で文全体を拒否し、部分結果を返さない）。

use std::collections::HashSet;
use std::sync::Arc;

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::allowlist::{SqlSurfaceError, WherePredicate};
use crate::sql::correlation::{self, Correlation};
use crate::sql::join::semi::{
    CorrelatedBudget, OuterSource, Residual, MAX_CORRELATED_REEVALUATIONS,
};
use crate::sql::join::values::{key_class, JoinKeyClass};
use crate::sql::udf_call::{BinOp, Expr, UdfRegistry};
use crate::sql::where_tree::{BoundConjunction, BoundOrGroup};

/// 相関 `EXISTS` の実行文脈。`core.rs` の `Statement::Scan`／`Statement::Aggregate` 実行アームが
/// 1 文ごとに [`CorrelatedCtx::enabled`] で作り、`sql::subquery::resolve_where_predicates` へ
/// 引き回す。述語形 `UPDATE`／`DELETE` の `WHERE`・集計の `FILTER (WHERE ...)` は
/// [`CorrelatedCtx::disabled`] を渡し、従来どおり相関を `42601` で拒否する（台帳・content hash への
/// 影響を本 Issue で扱わないため）。
#[derive(Debug, Clone)]
pub(crate) struct CorrelatedCtx {
    enabled: bool,
    budget: Arc<CorrelatedBudget>,
    max_input_rows: usize,
    byte_cap: usize,
}

impl CorrelatedCtx {
    /// 相関を拒否する文脈（従来の挙動）。
    pub(crate) fn disabled() -> Self {
        Self::build(false, crate::join_limits::MAX_JOIN_INPUT_ROWS)
    }

    /// 相関 `EXISTS` を実行する文脈。内側の入力行数の上限は JOIN と同じ起動時設定
    /// （[`crate::join_limits::JoinSizeLimits`]）に従う。
    pub(crate) fn enabled(join_limits: crate::join_limits::JoinSizeLimits) -> Self {
        Self::build(true, join_limits.max_input_rows())
    }

    fn build(enabled: bool, max_input_rows: usize) -> Self {
        Self {
            enabled,
            budget: Arc::new(CorrelatedBudget::new(
                MAX_CORRELATED_REEVALUATIONS,
                crate::join_limits::MAX_JOIN_COMPARISONS,
            )),
            max_input_rows,
            byte_cap: crate::arena::MAX_ARENA_TOTAL_BYTES,
        }
    }

    /// テスト専用: 上限を差し替えた文脈（`sql::join::mod` の `JoinLimits` と同じ流儀）。
    #[cfg(test)]
    pub(crate) fn with_limits(
        max_reevaluations: usize,
        max_comparisons: usize,
        max_input_rows: usize,
        byte_cap: usize,
    ) -> Self {
        Self {
            enabled: true,
            budget: Arc::new(CorrelatedBudget::new(max_reevaluations, max_comparisons)),
            max_input_rows,
            byte_cap,
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn budget(&self) -> Arc<CorrelatedBudget> {
        Arc::clone(&self.budget)
    }

    pub(crate) fn max_input_rows(&self) -> usize {
        self.max_input_rows
    }

    pub(crate) fn byte_cap(&self) -> usize {
        self.byte_cap
    }
}

/// 外側スコープの列 1 つ（相関参照の解決結果）。
#[derive(Debug, Clone)]
pub(crate) struct OuterRef {
    /// 内側 `WHERE` の AST 上の識別子（予約内部名、または非修飾名）。合成スキーマの列名にもなる。
    pub(crate) ident: String,
    pub(crate) source: OuterSource,
    /// 外側の列型。疑似列 `id` は `BigInt`。
    pub(crate) ty: ColumnType,
    /// キーの型クラス判定に使う列型（疑似列 `id` は `None`）。
    key_ty: Option<ColumnType>,
}

/// 等価キー 1 組。
#[derive(Debug, Clone)]
pub(crate) struct KeyPair {
    pub(crate) inner_column: String,
    /// 内側が疑似列 `id` のとき真（セルは行 `id` から作る）。
    pub(crate) inner_is_pseudo_id: bool,
    pub(crate) outer: OuterRef,
    pub(crate) class: JoinKeyClass,
}

/// 内側 `WHERE` の連言の分類結果。
#[derive(Debug)]
pub(crate) struct Analysis {
    pub(crate) inner_only: Vec<WherePredicate>,
    pub(crate) keys: Vec<KeyPair>,
    pub(crate) residual: Vec<WherePredicate>,
}

fn not_supported() -> SqlSurfaceError {
    SqlSurfaceError::unsupported("correlated subqueries are not supported")
}

fn beyond_enclosing_query() -> SqlSurfaceError {
    SqlSurfaceError::unsupported(
        "correlated subqueries are not supported beyond the immediately enclosing query",
    )
}

fn inner_has(inner: &TableSchema, name: &str) -> bool {
    inner.columns.iter().any(|c| c.name == name) || (name == "id" && inner.has_pseudo_id())
}

/// 内側の列型（実在の列）。疑似列 `id` は `None`、存在しない名前は `Err`。
fn inner_column_type(
    inner: &TableSchema,
    name: &str,
) -> Result<Option<ColumnType>, SqlSurfaceError> {
    match inner.columns.iter().find(|c| c.name == name) {
        Some(c) => Ok(Some(c.ty.clone())),
        None if name == "id" && inner.has_pseudo_id() => Ok(None),
        None => Err(SqlSurfaceError::undefined_column(name)),
    }
}

enum Scope {
    Inner,
    Outer(OuterRef),
    /// どのスコープにも無い名前（束縛が `42703` にする）。
    Unknown,
}

/// 識別子を内側／直近の外側／不明に分ける。直近より外への参照は `42601`。
fn scope_of(
    ident: &str,
    inner: &TableSchema,
    outer: &TableSchema,
    corr: &Correlation,
) -> Result<Scope, SqlSurfaceError> {
    let name = if let Some((levels_up, name)) = correlation::parse_outer_ref_placeholder(ident) {
        if levels_up != 1 {
            return Err(beyond_enclosing_query());
        }
        name
    } else if inner_has(inner, ident) {
        return Ok(Scope::Inner);
    } else {
        match corr.outer_refs().iter().find(|r| r.column == ident) {
            Some(r) if r.levels_up == 1 => ident,
            Some(_) => return Err(beyond_enclosing_query()),
            None => return Ok(Scope::Unknown),
        }
    };
    let (source, ty, key_ty) = match outer.columns.iter().position(|c| c.name == name) {
        Some(idx) => {
            let col = outer.columns.get(idx).ok_or_else(not_supported)?;
            (
                OuterSource::Column(idx),
                col.ty.clone(),
                Some(col.ty.clone()),
            )
        }
        None if name == "id" && outer.has_pseudo_id() => {
            (OuterSource::Id, ColumnType::BigInt, None)
        }
        None => return Err(SqlSurfaceError::undefined_column(name)),
    };
    Ok(Scope::Outer(OuterRef {
        ident: ident.to_string(),
        source,
        ty,
        key_ty,
    }))
}

/// 述語が `内側の裸の列 = 外側参照`（左右どちらでも）なら等価キーとして返す。型クラスが
/// 一致しない・キーにできない型の組は `None`（残余へ回す。PostgreSQL は受理するため `42601` にしない）。
fn as_equality_key(
    pred: &WherePredicate,
    inner: &TableSchema,
    outer: &TableSchema,
    corr: &Correlation,
) -> Result<Option<KeyPair>, SqlSurfaceError> {
    let WherePredicate::Expression(Expr::Binary {
        op: BinOp::Eq,
        lhs,
        rhs,
    }) = pred
    else {
        return Ok(None);
    };
    let (Expr::Ident(a), Expr::Ident(b)) = (lhs.as_ref(), rhs.as_ref()) else {
        return Ok(None);
    };
    let (inner_ident, outer_ref) = match (
        scope_of(a, inner, outer, corr)?,
        scope_of(b, inner, outer, corr)?,
    ) {
        (Scope::Inner, Scope::Outer(o)) => (a, o),
        (Scope::Outer(o), Scope::Inner) => (b, o),
        _ => return Ok(None),
    };
    let inner_ty = inner_column_type(inner, inner_ident)?;
    let (Ok(inner_class), Ok(outer_class)) = (
        key_class(inner_ty.as_ref()),
        key_class(outer_ref.key_ty.as_ref()),
    ) else {
        return Ok(None);
    };
    if inner_class != outer_class {
        return Ok(None);
    }
    Ok(Some(KeyPair {
        inner_column: inner_ident.clone(),
        inner_is_pseudo_id: inner_ty.is_none(),
        outer: outer_ref,
        class: inner_class,
    }))
}

/// 内側 `WHERE` の連言を分類する。`corr` は内側全体の相関束縛結果（直近の外側への参照の確認に使う）。
pub(crate) fn analyze(
    preds: Vec<WherePredicate>,
    inner: &TableSchema,
    outer: &TableSchema,
    corr: &Correlation,
) -> Result<Analysis, SqlSurfaceError> {
    let mut analysis = Analysis {
        inner_only: Vec::new(),
        keys: Vec::new(),
        residual: Vec::new(),
    };
    for pred in preds {
        let mut idents: HashSet<String> = HashSet::new();
        crate::sql::parser::collect_where_predicate_idents(
            std::slice::from_ref(&pred),
            &mut idents,
        );
        let mut sorted: Vec<&String> = idents.iter().collect();
        sorted.sort();
        let mut touches_outer = false;
        for ident in sorted {
            if matches!(scope_of(ident, inner, outer, corr)?, Scope::Outer(_)) {
                touches_outer = true;
            }
        }
        if !touches_outer {
            analysis.inner_only.push(pred);
            continue;
        }
        // 外側参照を持つサブクエリ述語（相関 `IN`／スカラー等）は本 Issue の範囲外（#1844）。
        if crate::sql::subquery::predicate_contains_subquery(&pred) {
            return Err(not_supported());
        }
        match as_equality_key(&pred, inner, outer, corr)? {
            Some(key) => analysis.keys.push(key),
            None => analysis.residual.push(pred),
        }
    }
    Ok(analysis)
}

/// 残余述語の評価に使える内側列の型か（`join::values::cell_scalar` が写像できる型）。
fn inner_cell_supported(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Text
            | ColumnType::Varchar(_)
            | ColumnType::Enum(_)
            | ColumnType::SmallInt
            | ColumnType::Integer
            | ColumnType::BigInt
            | ColumnType::Real
            | ColumnType::Double
            | ColumnType::Boolean
            | ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::Numeric { .. }
            | ColumnType::Uuid
            | ColumnType::Bytea
            | ColumnType::Json
    )
}

fn unsupported_column_type() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "this column type is not supported in a correlated subquery condition",
    )
}

/// 束縛エラーの文言に外側参照の予約内部名（制御文字の接頭辞）が含まれる場合は、固定文言の
/// `42601` へ置き換える（合成スキーマの列名が型エラー等の文言へ引用されても、予約名をクライアントへ
/// 出さない。fail-closed）。含まれない場合はそのまま返す。
fn scrub_reserved_names(e: SqlSurfaceError) -> SqlSurfaceError {
    if e.to_string().contains(correlation::OUTER_REF_PREFIX) {
        not_supported()
    } else {
        e
    }
}

/// 残余述語を合成スキーマ（内側スキーマ ＋ 外側参照ごとの列）へ束縛する。`inner_cell_offset` は
/// 実体化の投影で残余用の内側列が始まるセル位置（先頭にキー列が並ぶため）。
pub(crate) fn bind_residual(
    residual_preds: &[WherePredicate],
    inner: &TableSchema,
    outer: &TableSchema,
    corr: &Correlation,
    udfs: &UdfRegistry,
    inner_cell_offset: &dyn Fn(&str) -> usize,
) -> Result<Residual, SqlSurfaceError> {
    let mut idents: HashSet<String> = HashSet::new();
    crate::sql::parser::collect_where_predicate_idents(residual_preds, &mut idents);
    let mut sorted: Vec<&String> = idents.iter().collect();
    sorted.sort();

    let mut composite = inner.clone();
    let mut outer_cols: Vec<(usize, OuterSource, ColumnType)> = Vec::new();
    let mut inner_cols: Vec<(usize, usize, ColumnType)> = Vec::new();
    for ident in sorted {
        match scope_of(ident, inner, outer, corr)? {
            Scope::Outer(o) => {
                if matches!(o.ty, ColumnType::Vector(_)) {
                    return Err(unsupported_column_type());
                }
                let slot = composite.columns.len();
                // 外側の列定義をそのまま写す（照合など型以外の属性を落とさない）。名前だけを AST 上の
                // 識別子へ差し替え、NULL を許す（外側の値は NULL になりうる）。
                let def = match o.source {
                    OuterSource::Column(idx) => {
                        let mut def = outer.columns.get(idx).ok_or_else(not_supported)?.clone();
                        def.name = o.ident.clone();
                        def.nullable = true;
                        def
                    }
                    OuterSource::Id => ColumnDef::new(o.ident.clone(), ColumnType::BigInt, true),
                };
                composite.columns.push(def);
                outer_cols.push((slot, o.source, o.ty));
            }
            Scope::Inner => {
                if let Some(idx) = inner.columns.iter().position(|c| &c.name == ident) {
                    let col = inner.columns.get(idx).ok_or_else(not_supported)?;
                    if !inner_cell_supported(&col.ty) {
                        return Err(unsupported_column_type());
                    }
                    inner_cols.push((inner_cell_offset(ident), idx, col.ty.clone()));
                }
                // 疑似列 `id` は行 `id` で評価するため列の実体化は不要。
            }
            Scope::Unknown => {}
        }
    }

    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (metadata, expr, _rls, or_filters) = crate::sql::parser::bind_where_predicates(
        residual_preds,
        &composite,
        udfs,
        &mut node_budget,
        &[],
    )
    .map_err(scrub_reserved_names)?;
    let group = BoundOrGroup::new(vec![BoundConjunction::new(metadata, expr, or_filters)]);
    // 内側の VECTOR 列は残余では評価できない（実体化しない）。参照していれば拒否する。
    if group.references_embedding() {
        return Err(unsupported_column_type());
    }
    Ok(Residual {
        group,
        inner_cols,
        outer_cols,
        width: composite.columns.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;

    fn schema(name: &str, cols: &[(&str, ColumnType)]) -> TableSchema {
        TableSchema::new(
            name,
            cols.iter()
                .map(|(n, t)| ColumnDef::new(*n, t.clone(), true))
                .collect(),
        )
    }

    fn ph(name: &str) -> String {
        correlation::outer_ref_placeholder(1, name)
    }

    fn eq(a: &str, b: &str) -> WherePredicate {
        WherePredicate::Expression(Expr::Binary {
            op: BinOp::Eq,
            lhs: Box::new(Expr::Ident(a.to_string())),
            rhs: Box::new(Expr::Ident(b.to_string())),
        })
    }

    fn corr_for(inner: &TableSchema, outer: &TableSchema, idents: &[String]) -> Correlation {
        let set: HashSet<String> = idents.iter().cloned().collect();
        correlation::bind_correlation(&set, inner, &[outer]).expect("bind")
    }

    #[test]
    fn classifies_inner_only_key_and_residual() {
        let inner = schema(
            "b",
            &[
                ("aid", ColumnType::BigInt),
                ("v", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
            ],
        );
        let outer = schema(
            "a",
            &[("x", ColumnType::Integer), ("w", ColumnType::Integer)],
        );
        let preds = vec![
            WherePredicate::BoolColumn {
                column: "flag".to_string(),
            },
            eq("aid", &ph("x")),
            eq(&ph("x"), "aid"),
            WherePredicate::Expression(Expr::Binary {
                op: BinOp::Gt,
                lhs: Box::new(Expr::Ident("v".to_string())),
                rhs: Box::new(Expr::Ident(ph("w"))),
            }),
        ];
        let idents = vec!["flag".into(), "aid".into(), ph("x"), "v".into(), ph("w")];
        let corr = corr_for(&inner, &outer, &idents);
        let a = analyze(preds, &inner, &outer, &corr).expect("analyze");
        assert_eq!(a.inner_only.len(), 1);
        assert_eq!(a.keys.len(), 2);
        assert_eq!(a.residual.len(), 1);
        assert_eq!(a.keys[0].inner_column, "aid");
    }

    #[test]
    fn unqualified_outer_name_is_resolved_through_the_correlation_ir() {
        let inner = schema("b", &[("aid", ColumnType::BigInt)]);
        let outer = schema("a", &[("only_outer", ColumnType::BigInt)]);
        let idents = vec!["aid".to_string(), "only_outer".to_string()];
        let corr = corr_for(&inner, &outer, &idents);
        let a = analyze(vec![eq("aid", "only_outer")], &inner, &outer, &corr).expect("analyze");
        assert_eq!(a.keys.len(), 1);
        assert!(a.residual.is_empty());
    }

    #[test]
    fn mismatched_or_unkeyable_equalities_fall_back_to_residual() {
        let inner = schema("b", &[("t", ColumnType::Text), ("d", ColumnType::Double)]);
        let outer = schema(
            "a",
            &[("n", ColumnType::Integer), ("e", ColumnType::Double)],
        );
        let idents = vec!["t".into(), ph("n"), "d".into(), ph("e")];
        let corr = corr_for(&inner, &outer, &idents);
        let a = analyze(
            vec![eq("t", &ph("n")), eq("d", &ph("e"))],
            &inner,
            &outer,
            &corr,
        )
        .expect("analyze");
        assert!(a.keys.is_empty());
        assert_eq!(a.residual.len(), 2);
    }

    #[test]
    fn levels_beyond_the_enclosing_query_are_rejected() {
        let inner = schema("b", &[("aid", ColumnType::BigInt)]);
        let outer = schema("a", &[("x", ColumnType::BigInt)]);
        let corr = Correlation::default();
        let e = analyze(
            vec![eq("aid", &correlation::outer_ref_placeholder(2, "x"))],
            &inner,
            &outer,
            &corr,
        )
        .expect_err("levels");
        assert_eq!(e.wire_code(), "42601");
        assert!(!format!("{e}").contains('\u{2}'));
    }

    #[test]
    fn outer_reference_inside_a_subquery_predicate_is_rejected() {
        let inner = schema("b", &[("aid", ColumnType::BigInt)]);
        let outer = schema("a", &[("x", ColumnType::BigInt)]);
        let idents = vec![ph("x")];
        let corr = corr_for(&inner, &outer, &idents);
        let pred = WherePredicate::InSubquery {
            column: ph("x"),
            inner_tokens: Vec::new(),
            depth: 1,
        };
        let e = analyze(vec![pred], &inner, &outer, &corr).expect_err("subquery");
        assert_eq!(e.wire_code(), "42601");
    }

    #[test]
    fn scrub_reserved_names_hides_the_reserved_prefix_but_keeps_other_errors() {
        let leaky = SqlSurfaceError::invalid_input(format!(
            "column \"{}\" cannot be used in an expression",
            ph("x")
        ));
        let scrubbed = scrub_reserved_names(leaky);
        assert_eq!(scrubbed.wire_code(), "42601");
        assert!(!format!("{scrubbed}").contains('\u{2}'));
        let plain = SqlSurfaceError::undefined_column("zz");
        assert_eq!(scrub_reserved_names(plain).wire_code(), "42703");
    }

    // --- 上限（`CorrelatedCtx::with_limits` で差し替え）の単体テスト ---------------------------
    // 再評価回数・比較回数・内側入力行数・バイト量の超過が `54000` で文全体を拒否すること、
    // 半結合は再評価を数えないこと、他テナントの行が判定・上限に影響しないことを固定する。

    use crate::kernel::CpuScalarProvider;
    use crate::policy::PolicyContext;
    use crate::sql::mode::SessionState;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn limits_core() -> (crate::core::EngineCore, CleanupGuard) {
        let path = unique_db_path("correlated-exists-limits");
        let storage = Storage::open(&path).expect("open storage");
        for (name, key) in [("a", "k"), ("b", "aid")] {
            storage
                .create_table(&TableSchema::new(
                    name,
                    vec![
                        ColumnDef::new("embedding", ColumnType::Vector(2), false),
                        ColumnDef::new(key, ColumnType::BigInt, true),
                        ColumnDef::new("v", ColumnType::Integer, true),
                    ],
                ))
                .expect("create table");
        }
        let core = crate::core::EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        (core, CleanupGuard(path))
    }

    fn tenant(name: &str) -> PolicyContext {
        PolicyContext::with_visibilities(name, [Visibility::Public, Visibility::Private])
            .expect("tenant")
    }

    fn seed(
        core: &crate::core::EngineCore,
        ctx: &PolicyContext,
        table: &str,
        key: &str,
        rows: &[(u64, i64, i64)],
    ) {
        for (id, k, v) in rows {
            core.execute_sql_in_session(
                ctx,
                &mut SessionState::default(),
                &format!(
                    "INSERT INTO {table} (id, embedding, {key}, v) VALUES ({id}, '[0.1,0.2]', {k}, {v}) \
                     USING OPERATION_ID '{table}-{id}'"
                ),
            )
            .expect("seed");
        }
    }

    /// 外側 `a` に対する WHERE を `sql::subquery` の解決段 → 束縛 → 走査で評価し、一致した `id` を返す。
    fn eval_where(
        core: &crate::core::EngineCore,
        ctx: &PolicyContext,
        where_clause: &str,
        corr: &CorrelatedCtx,
    ) -> Result<Vec<u64>, SqlSurfaceError> {
        let storage = core.storage_for_test();
        let sql = format!("SELECT id FROM a WHERE {where_clause} LIMIT 100");
        let tokens = crate::sql::lexer::tokenize(&sql).expect("tokenize");
        let crate::sql::allowlist::Statement::Scan(mut validated) =
            crate::sql::allowlist::validate_sql_tokens_with_subquery_ctx_limits(
                &tokens,
                storage,
                0,
                crate::nesting_limits::NestingLimits::default(),
            )?
        else {
            panic!("scan expected");
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let schema = crate::catalog::get_table_schema_in_txn(&read_txn, "a").expect("schema");
        let udfs = UdfRegistry::default();
        let mut budget = 16usize;
        let mut in_budget = crate::sql::subquery::MAX_SUBQUERY_IN_VALUES;
        validated.where_predicates = crate::sql::subquery::resolve_where_predicates(
            validated.where_predicates,
            &[&schema],
            &read_txn,
            ctx,
            storage,
            &udfs,
            crate::group_by_limits::GroupByLimits::default(),
            crate::nesting_limits::NestingLimits::default(),
            crate::result_limits::ResultLimits::default(),
            &mut budget,
            &mut in_budget,
            corr,
        )?;
        let bound = crate::sql::parser::bind_scan_with_dummy_flags(
            &validated,
            &schema,
            &udfs,
            &[],
            &crate::result_limits::ResultLimits::default(),
        )?;
        let result = crate::sql::scan::execute_scan(&read_txn, ctx, &schema, &bound)?;
        let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        Ok(ids)
    }

    const BIG: usize = 1_000_000;

    fn fixture() -> (crate::core::EngineCore, CleanupGuard) {
        let (core, guard) = limits_core();
        let t1 = tenant("t1");
        seed(&core, &t1, "a", "k", &[(1, 1, 10), (2, 2, 20), (3, 3, 30)]);
        seed(&core, &t1, "b", "aid", &[(1, 1, 5), (2, 2, 6), (3, 3, 7)]);
        // 他テナントの行（どの上限にも数えられない）。
        let t2 = tenant("t2");
        seed(
            &core,
            &t2,
            "b",
            "aid",
            &[(11, 1, 5), (12, 1, 5), (13, 2, 5), (14, 3, 5), (15, 9, 5)],
        );
        (core, guard)
    }

    const KEYED_RESIDUAL: &str = "EXISTS (SELECT 1 FROM b WHERE b.aid = a.k AND b.v < a.v)";
    const KEYLESS_NO_MATCH: &str = "EXISTS (SELECT 1 FROM b WHERE b.v > a.v + 1000)";
    const SEMI: &str = "EXISTS (SELECT 1 FROM b WHERE b.aid = a.k)";

    #[test]
    fn reevaluation_count_over_the_cap_is_rejected_without_partial_results() {
        let (core, _g) = fixture();
        let t1 = tenant("t1");
        // キー付き残余: 外側 3 行がいずれもバケットに当たるため再評価 3 回。
        let ok = CorrelatedCtx::with_limits(3, BIG, BIG, usize::MAX);
        assert_eq!(
            eval_where(&core, &t1, KEYED_RESIDUAL, &ok).unwrap(),
            vec![1, 2, 3]
        );
        let over = CorrelatedCtx::with_limits(2, BIG, BIG, usize::MAX);
        let e = eval_where(&core, &t1, KEYED_RESIDUAL, &over).expect_err("over cap");
        assert_eq!(e.wire_code(), "54000");
        assert!(!format!("{e}").contains('\u{2}'));
    }

    #[test]
    fn hash_semi_join_does_not_consume_the_reevaluation_budget() {
        let (core, _g) = fixture();
        let t1 = tenant("t1");
        let zero = CorrelatedCtx::with_limits(0, 0, BIG, usize::MAX);
        assert_eq!(eval_where(&core, &t1, SEMI, &zero).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn comparison_count_over_the_cap_is_rejected() {
        let (core, _g) = fixture();
        let t1 = tenant("t1");
        // キー無しの残余で一致なし: 外側 3 行 × 内側 3 行 = 9 回の比較。
        let ok = CorrelatedCtx::with_limits(BIG, 9, BIG, usize::MAX);
        assert_eq!(
            eval_where(&core, &t1, KEYLESS_NO_MATCH, &ok).unwrap(),
            Vec::<u64>::new()
        );
        let over = CorrelatedCtx::with_limits(BIG, 8, BIG, usize::MAX);
        let e = eval_where(&core, &t1, KEYLESS_NO_MATCH, &over).expect_err("over cap");
        assert_eq!(e.wire_code(), "54000");
    }

    #[test]
    fn inner_input_rows_over_the_cap_are_rejected_using_visible_rows_only() {
        let (core, _g) = fixture();
        let t1 = tenant("t1");
        let t2 = tenant("t2");
        // t1 の可視 b は 3 行、t2 は 5 行（表全体は 8 行）。
        let cap3 = CorrelatedCtx::with_limits(BIG, BIG, 3, usize::MAX);
        assert_eq!(eval_where(&core, &t1, SEMI, &cap3).unwrap(), vec![1, 2, 3]);
        let cap2 = CorrelatedCtx::with_limits(BIG, BIG, 2, usize::MAX);
        let e = eval_where(&core, &t1, SEMI, &cap2).expect_err("over cap");
        assert_eq!(e.wire_code(), "54000");
        // t2 は外側 a の行を持たないので結果は空だが、入力行数 5 は上限 4 を超えて 54000。
        // 他テナントの行数ではなく自テナントの可視行数だけで判定される。
        let cap4 = CorrelatedCtx::with_limits(BIG, BIG, 4, usize::MAX);
        assert_eq!(
            eval_where(&core, &t2, SEMI, &cap4)
                .expect_err("t2 over cap")
                .wire_code(),
            "54000"
        );
        assert_eq!(eval_where(&core, &t1, SEMI, &cap4).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn byte_budget_over_the_cap_is_rejected() {
        let (core, _g) = fixture();
        let t1 = tenant("t1");
        let tiny = CorrelatedCtx::with_limits(BIG, BIG, BIG, 1);
        let e = eval_where(&core, &t1, SEMI, &tiny).expect_err("byte budget");
        assert_eq!(e.wire_code(), "54000");
    }

    #[test]
    fn disabled_context_rejects_positive_correlated_exists() {
        let (core, _g) = fixture();
        let t1 = tenant("t1");
        let e = eval_where(&core, &t1, SEMI, &CorrelatedCtx::disabled()).expect_err("disabled");
        assert_eq!(e.wire_code(), "42601");
    }
}
