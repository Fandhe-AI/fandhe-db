//! WHERE 句のサブクエリ（`IN (SELECT ...)`・`NOT IN`・`EXISTS`・`NOT EXISTS`・
//! 値位置のスカラーサブクエリ `<col> <op> (SELECT ...)`）を実行前に解決する
//! （Issue #927・#1191・SQL-29 (a)・RLS-10 (b)・TASK-213）。
//!
//! 責務境界: `sql::allowlist::Parser` が構文的に受理した
//! [`WherePredicate::InSubquery`]／[`WherePredicate::Exists`]／
//! [`WherePredicate::ScalarSubqueryCompare`]（およびそれらを包む
//! [`WherePredicate::Not`]。内側の生トークン列を保持するのみで未評価）を、
//! `core.rs` の `Statement::Scan`／`Statement::Aggregate` 実行アームが束縛
//! （`sql::parser::bind_scan`／`bind_aggregate`）の**前**に本モジュールへ渡す。
//! 内側は外側と同じ `PolicyContext`（RLS 暗黙適用）・同じ `read_txn`（同一
//! スナップショット）で `sql::allowlist::validate_sql_tokens_with_subquery_ctx` →
//! `sql::parser::bind_scan`（集計内側は `bind_aggregate`）→ 既存の実行器という、
//! 通常の SELECT と完全に同じ経路で実行し、結果を具体的な `WherePredicate`
//! （`Or`／`Equality`／`InList`／`Compare`／`Expression` 等）へ書き換える。これにより
//! `sql::where_tree`・`sql::parser::bind_where_predicates` は一切変更せず、
//! サブクエリを含まない文と同じ評価器を再利用する（CLAUDE.md「委譲方針」＝第 2 の
//! 評価器を作らない）。スカラーサブクエリの結果は「同じ列型で `col <op> <リテラル>`
//! と書いたときにパーサーが生成する AST」と同一形へ書き換える。
//!
//! 対応する内側の形:
//! - `IN`／`NOT IN`／`EXISTS`／`NOT EXISTS`: `SELECT <単一列> FROM <table> [WHERE ...]
//!   LIMIT <n>`（広域取得＝[`Statement::Scan`]）のみ。
//! - スカラー: 上記に加え、単一集計項目の集計形（[`Statement::Aggregate`]。
//!   `GROUP BY` の有無を問わず `LIMIT` 不要）。
//!
//! ランキング付き検索 SELECT・集合演算・JOIN・内側の `LIMIT` 省略（Scan 形）・
//! 内側自身の投影位置スカラーサブクエリは `42601`（`docs/design/sql-subquery.md` 参照）。
//!
//! 投影位置のスカラーサブクエリ（Issue #1352。外側は広域取得 SELECT のみ）は
//! [`resolve_scalar_projection_items`] が内側を実行して列メタデータと値（0 行は NULL）へ
//! 解決し、[`merge_scalar_projection_items`] が外側の走査結果へ SELECT リスト上の位置で
//! 合流する。内側が 2 行以上なら外側の結果が 1 行以上のときだけ `21000`。
//! 内側 WHERE に入れ子のスカラーサブクエリの `21000` も同様に外側の行数判明まで遅延する（Issue #1432）。
//!
//! 相関サブクエリ（内側が外側の列を非修飾名で参照する形）は束縛前の静的走査で
//! `42601` にする（PostgreSQL の名前解決順と同じく、内側スキーマに無く外側の
//! いずれかのスコープに有る列名を相関参照とみなす。どこにも無い名前は従来どおり
//! 内側の束縛が `22000`〔unknown column〕で拒否する）。外側スコープは
//! `outer_scopes`（末尾が直近の外側）としてネストの深さ分を引き回す。
//!
//! NULL 規則（三値論理。RLS 境界内＝可視行のみから決まる）:
//! - `NOT IN`: 内側 0 行なら常に真（外側値が NULL でも真）。内側の結果に NULL を
//!   1 つでも含めば真にならない。それ以外は distinct 値ごとの否定を連言で並べ、
//!   外側値が NULL の行は UNKNOWN として除外される。
//! - `NOT EXISTS`: 可視行が 1 件でもあれば常に偽、無ければ常に真。
//! - スカラー: 0 行または NULL は UNKNOWN（否定は構文段で演算子反転済みのため
//!   常に偽）。2 行以上は「先頭行を採用」せずエラー（`21000`。Issue #1404）。
//!
//! DoS 対策（security.md「不安全な設計｜無制限リソース確保」対応）:
//! - ネスト深さは構文解析段（[`super::allowlist::MAX_SUBQUERY_DEPTH`]・
//!   `Parser::require_subquery_depth`）が担う。
//! - 1 文（トップレベル実行 1 回）あたりの内側クエリ実行回数は本モジュールの
//!   [`MAX_SUBQUERY_EXECUTIONS`] で頭打ちにする（`budget` を呼び出し階層全体で
//!   共有する `&mut usize` として引き回す）。
//! - 内側の可視行数は [`crate::core::MAX_SEARCH_K`] を超えたら `54000`。
//! - `IN`／`NOT IN` は内側の distinct 値を [`crate::declarative_filter::MAX_IN_LIST_ITEMS`]
//!   件以下のチャンクへ分け、チャンクごとの `WherePredicate::InList`（既存評価器の
//!   集合照合）を `Or`（`NOT IN` は `Not(InList)` の連言）に束ねる。取り込める
//!   distinct 値の総数は [`MAX_SUBQUERY_IN_VALUES`]（文全体で共有する予算）で頭打ち。
//!   整数列は既存の数値リテラル `IN` と同じ式脱糖形になるため、1 サイトの distinct
//!   値数を式ノード予算に収まる上限（`IN` は 256、`NOT IN` は 128）で `54000` にする。
//! - `EXISTS (SELECT ...)` は可視行が 1 件以上存在するかどうかしか使わないため、
//!   [`InnerScanIntent::ExistenceOnly`] で内側を実質 `LIMIT 1`・投影不要へ差し替える
//!   （`WHERE`・RLS の適用は変更しない＝可視性判定を迂回しない）。
//! - 投影列数（ちょうど 1 列である契約）・相関参照・値族の組合せは、実行結果ではなく
//!   `validated.projection`／内側スキーマ／結果列メタデータから、内側の行数・値に
//!   依存せず静的に検証する（`IN` は走査前に列数を拒否する）。
//! - 内側にウィンドウ関数（SQL-30・TASK-214、Issue #930）が含まれる場合は一律
//!   `42601` で拒否する（`execute_window_scan` が可視行を全件 materialize するため。
//!   `docs/design/sql-subquery.md` の対象外節参照）。

use std::collections::HashSet;

use super::allowlist::{
    AggregateArg, AggregateSelectItem, CompareOp, Projection, ScalarSubqueryItem, ScalarSubqueryOp,
    SelectItem, SqlSurfaceError, Statement, TableLookup, ValidatedAggregate, ValidatedScan,
    WherePredicate,
};
use super::exec::{Cell, ColumnMeta};
use super::lexer::Token;
use super::udf_call::{BinOp, Expr, UdfRegistry};
use crate::catalog::{ColumnType, TableSchema};
use crate::policy::PolicyContext;

/// 1 文（トップレベル実行 1 回）あたりに実行できる内側サブクエリの総数
/// （実装既定値。Issue #927・TASK-213。ネスト深さ上限
/// [`super::allowlist::MAX_SUBQUERY_DEPTH`] と組み合わせて評価コストを
/// 有界にする）。
pub(crate) const MAX_SUBQUERY_EXECUTIONS: usize = 16;

/// 1 文（トップレベル実行 1 回。ネストしたサブクエリを含む）あたりに
/// `IN`／`NOT IN (SELECT ...)` の集合照合へ取り込める distinct 値の総数（実装
/// 既定値。Issue #1165・SQL-29・TASK-213）。
///
/// 値は [`crate::declarative_filter::MAX_IN_LIST_ITEMS`] 件ずつのチャンクへ
/// 分割し、各チャンクを既存の `InList`（束縛時にソート・重複除去済みの
/// `FilterOp::InText`、評価は二分探索）へ束縛するため、1 行あたりの評価コストは
/// 「分岐数 × O(log 256)」で有界。内側 1 回の可視行数は既に
/// [`crate::core::MAX_SEARCH_K`] で頭打ちのため、単一の `IN` サブクエリは
/// この上限に到達しない（複数の `IN` の distinct 合計のみが上限に効く）。
pub(crate) const MAX_SUBQUERY_IN_VALUES: usize = crate::core::MAX_SEARCH_K;

/// 整数列に対する `NOT IN` の 1 サイトあたり distinct 値数の上限。値ごとに
/// `col < v OR col > v`（式ノード 6 個）を連言で並べるため、束縛の式ノード予算
/// （`udf_call::MAX_EXPR_NODES`＝1024）に収まる値数へ抑える（超過は `54000`）。
const MAX_INT_NOT_IN_VALUES: usize = crate::declarative_filter::MAX_IN_LIST_ITEMS / 2;

/// `where_predicates`（トップレベルの述語列。`WherePredicate::Or` の分岐も
/// 再帰的に辿る）に含まれるサブクエリ述語（`InSubquery`／`Exists`／
/// `ScalarSubqueryCompare` とそれらを包む `Not`）をすべて解決し、具体的な
/// `WherePredicate` へ書き換えた新しい述語列（連言）を返す。`core.rs` の
/// `Statement::Scan`／`Statement::Aggregate` 実行アームが、束縛
/// （`bind_scan`／`bind_aggregate`）の直前に呼ぶ。
///
/// `outer_scopes` は `predicates` を含む文から見た外側スコープ連鎖のスキーマ列
/// （末尾が `predicates` を含む文自身のテーブル）。末尾は対象列の存在・型検証
/// （[`validate_in_target_column`] 等。PR #1103 codex-review P1 指摘対応: 内側の
/// 結果行数に関わらず必ず検証する）に使い、全体は内側クエリの相関参照検出
/// （[`reject_correlated`]）に使う。ネストしたサブクエリを解決する再帰呼び出し
/// （[`execute_inner_query`]）は、内側クエリ自身のスキーマを末尾へ積んで渡す。
///
/// `budget` は呼び出し階層全体（ネストしたサブクエリを含む）で共有する残り
/// 実行回数。呼び出し元は [`MAX_SUBQUERY_EXECUTIONS`] で初期化する。
/// `in_value_budget` も同様に共有する、`IN` 展開で生成できる残り distinct 値数。
/// 呼び出し元は [`MAX_SUBQUERY_IN_VALUES`] で初期化する。
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_where_predicates(
    predicates: Vec<WherePredicate>,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    let mut out = Vec::with_capacity(predicates.len());
    for predicate in predicates {
        match predicate {
            WherePredicate::Or(branches) => {
                let mut resolved_branches = Vec::with_capacity(branches.len());
                for branch in branches {
                    resolved_branches.push(resolve_where_predicates(
                        branch,
                        outer_scopes,
                        read_txn,
                        ctx,
                        lookup,
                        udfs,
                        budget,
                        in_value_budget,
                    )?);
                }
                out.push(WherePredicate::Or(resolved_branches));
            }
            WherePredicate::InSubquery {
                column,
                inner_tokens,
                depth,
            } => {
                out.extend(resolve_in_subquery(
                    &column,
                    &inner_tokens,
                    depth,
                    false,
                    outer_scopes,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    budget,
                    in_value_budget,
                )?);
            }
            WherePredicate::Exists {
                inner_tokens,
                depth,
            } => {
                let exists = resolve_exists_subquery(
                    &inner_tokens,
                    depth,
                    outer_scopes,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    budget,
                    in_value_budget,
                )?;
                if !exists {
                    // 常に偽: 分岐 0 個の `Or` は `where_tree::BoundOrGroup::matches`
                    // が必ず `false` を返す（`sql::where_tree` モジュール
                    // ドキュメント参照）。`EXISTS` が偽の行を除外しつつ、
                    // `has_where_filters()` は真のままに保つ（フィルタなし専用
                    // キャッシュへ誤って乗せない）。
                    out.push(WherePredicate::Or(Vec::new()));
                }
                // 真の場合は述語自体を追加しない（制約を課さない＝常に真）。
            }
            WherePredicate::ScalarSubqueryCompare {
                column,
                op,
                inner_tokens,
                depth,
            } => {
                out.push(resolve_scalar_compare(
                    &column,
                    op,
                    &inner_tokens,
                    depth,
                    outer_scopes,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    budget,
                    in_value_budget,
                )?);
            }
            WherePredicate::Not(inner) => match *inner {
                // `NOT IN (SELECT ...)`（Issue #1191）。NULL 規則は
                // [`resolve_in_subquery`] が担う。
                WherePredicate::InSubquery {
                    column,
                    inner_tokens,
                    depth,
                } => {
                    out.extend(resolve_in_subquery(
                        &column,
                        &inner_tokens,
                        depth,
                        true,
                        outer_scopes,
                        read_txn,
                        ctx,
                        lookup,
                        udfs,
                        budget,
                        in_value_budget,
                    )?);
                }
                // `NOT EXISTS (SELECT ...)`（Issue #1191）: 可視行があれば常に偽、
                // 無ければ述語を追加しない（常に真）。
                WherePredicate::Exists {
                    inner_tokens,
                    depth,
                } => {
                    let exists = resolve_exists_subquery(
                        &inner_tokens,
                        depth,
                        outer_scopes,
                        read_txn,
                        ctx,
                        lookup,
                        udfs,
                        budget,
                        in_value_budget,
                    )?;
                    if exists {
                        out.push(WherePredicate::Or(Vec::new()));
                    }
                }
                other => out.push(WherePredicate::Not(Box::new(other))),
            },
            other => out.push(other),
        }
    }
    Ok(out)
}

/// [`execute_inner_query`] が内側クエリをどう評価するかを表す（PR #1103 追加
/// codex-review P1 指摘対応: 用途ごとに必要な情報量が異なるため、同じ実行経路
/// （`bind_scan` → `execute_scan`）を共有しつつ束縛直前の `ValidatedScan` だけを
/// 使い分ける）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InnerScanIntent {
    /// `IN`／`NOT IN (SELECT ...)`: 内側の投影値そのものが必要なため、ユーザー
    /// 指定の投影・`LIMIT` をそのまま使う。
    Values,
    /// `EXISTS`／`NOT EXISTS (SELECT ...)`: 可視行が 1 件以上存在するかどうか
    /// しか使わない。投影を空へ、`LIMIT` を実質 1 へ差し替えて評価する
    /// （`ValidatedScan::limit` のドキュメントが説明する早期終了を利用する）。
    ExistenceOnly,
    /// スカラーサブクエリ（Issue #1191）: 単一列の Scan、または単一集計項目の
    /// 集計形を、ユーザー指定のまま実行して行数・値を確認する。
    Scalar,
    /// 投影位置のスカラーサブクエリ（Issue #1352）: [`Self::Scalar`] と同じく単一列の
    /// Scan または単一集計項目の集計形を実行するが、値は 0 行（NULL）または 1 行しか
    /// 使わない。そのため Scan 形は元の `LIMIT` を検証した**後に**実質 `LIMIT 2`
    /// （「2 行目が存在するか」の判定に足りる最小値）へ差し替える。`WHERE`・RLS の
    /// 適用は変更しない。
    ScalarValue,
}

/// 内側 Scan が参照する非修飾の列名を集める（相関検出専用。疑似列 `id` を含みうる）。
/// ウィンドウ項目は呼び出し前に拒否済み。
fn scan_referenced_columns(validated: &ValidatedScan) -> HashSet<String> {
    let mut cols = HashSet::new();
    match &validated.projection {
        Projection::All => {}
        Projection::Columns(names) => cols.extend(names.iter().cloned()),
        Projection::Items(items) => {
            for item in items {
                match item {
                    SelectItem::Column(name) => {
                        cols.insert(name.clone());
                    }
                    SelectItem::Expr { expr, .. } => {
                        super::parser::collect_expr_idents(expr, &mut cols)
                    }
                }
            }
        }
    }
    super::parser::collect_where_predicate_idents(&validated.where_predicates, &mut cols);
    cols.extend(validated.order_by.iter().map(|k| k.column.clone()));
    cols
}

/// [`scan_referenced_columns`] の集計版（集計引数・グループキー・`GROUP BY` 列・WHERE）。
/// `HAVING`・集計 `ORDER BY` の対象は内側の出力名（別名）であり表の列ではないため
/// 対象外（未知名は内側の束縛が拒否する）。
fn aggregate_referenced_columns(validated: &ValidatedAggregate) -> HashSet<String> {
    let mut cols = HashSet::new();
    for item in &validated.items {
        match item {
            AggregateSelectItem::Aggregate(agg) => {
                if let AggregateArg::Expr(expr) = &agg.arg {
                    super::parser::collect_expr_idents(expr, &mut cols);
                }
            }
            AggregateSelectItem::GroupKey { column, .. } => {
                cols.insert(column.clone());
            }
        }
    }
    if let Some(group_by) = &validated.group_by {
        cols.extend(group_by.columns.iter().cloned());
    }
    super::parser::collect_where_predicate_idents(&validated.where_predicates, &mut cols);
    cols
}

/// 内側クエリの参照列に相関参照（内側スキーマに無く、外側スコープのいずれかに
/// 有る名前）が含まれれば `42601` で拒否する（Issue #1191・SQL-29。相関
/// サブクエリは対象外）。PostgreSQL の名前解決順（内側優先）に合わせ、内側に
/// 同名列が有れば内側の列として扱う。疑似列 `id` は全テーブルにあるため対象外。
/// エラー文言は静的文字列のみ（他テナントの情報・列名を含めない）。
fn reject_correlated(
    referenced: &HashSet<String>,
    inner_schema: &TableSchema,
    outer_scopes: &[&TableSchema],
) -> Result<(), SqlSurfaceError> {
    for name in referenced {
        if name == "id" || inner_schema.columns.iter().any(|c| &c.name == name) {
            continue;
        }
        if outer_scopes
            .iter()
            .any(|scope| scope.columns.iter().any(|c| &c.name == name))
        {
            return Err(SqlSurfaceError::unsupported(
                "correlated subqueries are not supported",
            ));
        }
    }
    Ok(())
}

/// 内側スキーマを取得する（存在しないテーブルは `42P01`）。
fn inner_table_schema(
    read_txn: &impl crate::storage::read_source::ReadSource,
    table_name: &str,
) -> Result<TableSchema, SqlSurfaceError> {
    crate::catalog::get_table_schema_in_txn(read_txn, table_name).map_err(|e| match e {
        crate::catalog::CatalogError::TableNotFound(name) => SqlSurfaceError::undefined_table(name),
        other => crate::catalog::table_lookup_error(other),
    })
}

/// 内側トークン列を検証・実行し、`sql::allowlist::Statement::Scan`（スカラー
/// 用途では単一集計項目の `Statement::Aggregate` も可）として妥当であることを
/// 確認した上で、相関参照の検出・束縛前の残りの解決（自身の WHERE に含まれる
/// さらに深いサブクエリ）を行い、既存の実行器で実行する。
///
/// 自身の WHERE に含まれるさらに深いサブクエリを解決する際の外側スコープ連鎖
/// （[`resolve_where_predicates`] の `outer_scopes`）は、`outer_scopes` の末尾に
/// この内側クエリ自身のスキーマを積んだものになる。`intent` が
/// [`InnerScanIntent::ExistenceOnly`] の場合、`WHERE`・RLS の適用は通常の内側評価と
/// 完全に同一のまま、投影・`LIMIT` のみを可視性判定に不要な形へ差し替える
/// （PR #1103 追加 codex-review P1 指摘対応）。
#[allow(clippy::too_many_arguments)]
fn execute_inner_query(
    inner_tokens: &[Token],
    depth: usize,
    intent: InnerScanIntent,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    let mut meta_sink = None;
    execute_inner_query_with_meta(
        inner_tokens,
        depth,
        intent,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_value_budget,
        &mut meta_sink,
    )
}

/// [`execute_inner_query`] の本体。束縛（`bind_scan`／`bind_aggregate`）に成功した時点で、
/// 実行とは独立に確定する投影列メタデータを `meta_sink` へ書き出す（Issue #1352。
/// 実行時エラーを遅延する投影位置のスカラーサブクエリが、外側の行数に関わらず同じ列型を
/// 公告するため）。束縛前に失敗した場合（静的エラー）は `meta_sink` が `None` のまま残る。
#[allow(clippy::too_many_arguments)]
fn execute_inner_query_with_meta(
    inner_tokens: &[Token],
    depth: usize,
    intent: InnerScanIntent,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
    meta_sink: &mut Option<Vec<ColumnMeta>>,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    *budget = budget.checked_sub(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large(format!(
            "subquery execution count exceeds limit {MAX_SUBQUERY_EXECUTIONS}"
        ))
    })?;

    let stmt =
        super::allowlist::validate_sql_tokens_with_subquery_ctx(inner_tokens, lookup, depth)?;
    let result = match (stmt, intent) {
        (Statement::Scan(validated), _) => execute_inner_scan_statement(
            validated,
            intent,
            outer_scopes,
            read_txn,
            ctx,
            lookup,
            udfs,
            budget,
            in_value_budget,
            meta_sink,
        )?,
        (
            Statement::Aggregate(validated),
            InnerScanIntent::Scalar | InnerScanIntent::ScalarValue,
        ) => execute_inner_aggregate_statement(
            validated,
            outer_scopes,
            read_txn,
            ctx,
            lookup,
            udfs,
            budget,
            in_value_budget,
            meta_sink,
        )?,
        _ => {
            return Err(SqlSurfaceError::unsupported(
                "subquery must be a plain SELECT ... FROM ... [WHERE ...] LIMIT n \
                 (no HYBRID / USING PLAN / set operation / JOIN; GROUP BY only as a scalar \
                 aggregate)",
            ))
        }
    };

    // `bind_scan` の LIMIT 範囲検証とは独立に、内側の可視結果行数を
    // `MAX_SEARCH_K` で頭打ちにする防御的な上限（security.md「不安全な設計」
    // 対応。内側 LIMIT の値自体は利用者が指定できるため、既存の範囲検証の
    // 上限がこの値より緩い場合の保険）。
    if result.rows.len() > crate::core::MAX_SEARCH_K {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "subquery result row count exceeds limit {}",
            crate::core::MAX_SEARCH_K
        )));
    }

    Ok(result)
}

/// [`execute_inner_query`] の広域取得（`Statement::Scan`）側の本体。
#[allow(clippy::too_many_arguments)]
fn execute_inner_scan_statement(
    mut validated: ValidatedScan,
    intent: InnerScanIntent,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
    meta_sink: &mut Option<Vec<ColumnMeta>>,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    // Cursor Bugbot 指摘対応: ウィンドウ関数（SQL-30・TASK-214、Issue #930）を
    // 含む内側は一律拒否する。`window_items` が非空だと `sql::scan::execute_scan` は
    // `sql::window::execute_window_scan` へ分岐し、`LIMIT` による早期終了なしに
    // 可視行を全件 materialize する契約（`docs/design/window-functions.md` 参照）。
    // サブクエリとウィンドウ関数の組合せは設計上未検証のため fail-closed に倒す。
    if !validated.window_items.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "subquery cannot contain window functions",
        ));
    }

    let inner_schema = inner_table_schema(read_txn, &validated.table_name)?;

    // 相関サブクエリは束縛・走査より前に静的に拒否する（Issue #1191）。
    reject_correlated(
        &scan_referenced_columns(&validated),
        &inner_schema,
        outer_scopes,
    )?;

    // Issue #1352: 内側自身の投影位置スカラーサブクエリは非対応（WHERE 経由の入れ子は
    // 深さ上限内で従来どおり許可）。投影を黙って落とさないよう intent に関わらず拒否する。
    if !validated.scalar_subquery_items.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "a subquery cannot contain a scalar subquery in its SELECT list",
        ));
    }

    if matches!(
        intent,
        InnerScanIntent::Values | InnerScanIntent::Scalar | InnerScanIntent::ScalarValue
    ) {
        // 投影列数（ちょうど 1 列）を実行前に静的に確定して拒否する（PR #1103
        // 追加 codex-review P1 指摘の自己点検: 必要以上の投影で走査コストを払って
        // から拒否する問題を避ける。`resolve_in_subquery` 等の実行後チェックは
        // 想定外の構成を取りこぼさないための多層防御として残す）。
        let projected_len = match &validated.projection {
            Projection::All => 1 + inner_schema.columns.len(),
            Projection::Columns(names) => names.len(),
            Projection::Items(items) => items.len(),
        };
        if projected_len != 1 {
            return Err(SqlSurfaceError::unsupported(
                "subquery used as a value must select exactly one column",
            ));
        }
    }

    // 自身の WHERE に含まれるさらに深いサブクエリを、束縛（`bind_scan`）の前に
    // 解決する（深さ優先。`depth` は構文解析段で `MAX_SUBQUERY_DEPTH` 検査
    // 済みのため、ここでは budget のみ検査すれば足りる）。
    // 実行時エラー時に静的検証を完了させるための、サブクエリを含まない述語の控え。
    let static_preds: Vec<WherePredicate> = validated
        .where_predicates
        .iter()
        .filter(|p| !predicate_contains_subquery(p))
        .cloned()
        .collect();
    // 先行サブクエリの遅延対象エラーで解決が打ち切られても、後続サブクエリの静的エラーを
    // 検出できるよう、サブクエリを含む述語の控えも持つ。
    let subquery_preds: Vec<WherePredicate> = validated
        .where_predicates
        .iter()
        .filter(|p| predicate_contains_subquery(p))
        .cloned()
        .collect();
    let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
    chain.push(&inner_schema);
    validated.where_predicates = match resolve_where_predicates(
        validated.where_predicates,
        &chain,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_value_budget,
    ) {
        Ok(p) => p,
        Err(e) => {
            // 入れ子 WHERE サブクエリの実行時データ例外は、投影位置の外側が行数判明まで
            // 遅延できるよう、投影メタデータ（WHERE に依存しない）を先に確定する。
            // 内側 WHERE の静的な誤り（未知列等）はデータ依存の実行時エラーより優先して
            // 返す（可視データで契約が変わらないよう、サブクエリを含まない述語は
            // 束縛検証を完了させる）。
            if intent == InnerScanIntent::ScalarValue {
                if is_deferrable_runtime_error(&e) {
                    if let Some(static_err) = first_static_error_in_subquery_predicates(
                        &subquery_preds,
                        &chain,
                        read_txn,
                        ctx,
                        lookup,
                        udfs,
                    ) {
                        return Err(static_err);
                    }
                }
                validated.where_predicates = static_preds;
                let bound = super::parser::bind_scan(&validated, &inner_schema, udfs)?;
                *meta_sink = super::describe::scan_columns(&bound, &inner_schema).ok();
            }
            return Err(e);
        }
    };

    if intent == InnerScanIntent::ExistenceOnly {
        // ユーザー指定の `LIMIT` 自体の範囲検証（`bind_scan` が本来行う契約）
        // は、これから使う値を 1 へ差し替えても迂回されないよう、差し替え前の
        // 元の値に対して明示的に検証しておく（fail-closed）。`OFFSET` は可視性
        // 判定に意味を持つため変更しない。
        super::parser::validate_search_limit(validated.limit)?;
        // ユーザー指定の投影も、実行用に空へ差し替える前に必ず束縛検証する
        // （検証前に入力を差し替えると、存在しない列を指定した内側クエリが列検証を
        // すり抜け、可視行の有無だけで成否が決まってしまう）。束縛結果自体は使わない。
        let mut probe_node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        super::parser::bind_projection(
            &validated.projection,
            &inner_schema,
            udfs,
            &mut probe_node_budget,
        )?;
        validated.projection = Projection::Columns(Vec::new());
        validated.limit = 1;
    }

    if intent == InnerScanIntent::ScalarValue {
        // 元の `LIMIT` 自体の範囲検証は差し替えで迂回されないよう先に行う（fail-closed）。
        super::parser::validate_search_limit(validated.limit)?;
        validated.limit = validated.limit.min(2);
    }

    let bound = super::parser::bind_scan(&validated, &inner_schema, udfs)?;
    *meta_sink = super::describe::scan_columns(&bound, &inner_schema).ok();
    super::scan::execute_scan(read_txn, ctx, &inner_schema, &bound)
}

/// [`execute_inner_query`] の集計（`Statement::Aggregate`。スカラー用途のみ）側の
/// 本体。通常の集計 SELECT と同じ `bind_aggregate` → `execute_aggregate_with_cache`
/// （キャッシュ非経由）で実行し、第 2 の集計評価器を作らない。
#[allow(clippy::too_many_arguments)]
fn execute_inner_aggregate_statement(
    mut validated: ValidatedAggregate,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
    meta_sink: &mut Option<Vec<ColumnMeta>>,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    if validated.items.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used as a value must select exactly one column",
        ));
    }
    let inner_schema = inner_table_schema(read_txn, &validated.table_name)?;
    reject_correlated(
        &aggregate_referenced_columns(&validated),
        &inner_schema,
        outer_scopes,
    )?;
    // 実行時エラー時に静的検証を完了させるための、サブクエリを含まない述語の控え。
    let static_preds: Vec<WherePredicate> = validated
        .where_predicates
        .iter()
        .filter(|p| !predicate_contains_subquery(p))
        .cloned()
        .collect();
    // 先行サブクエリの遅延対象エラーで解決が打ち切られても、後続サブクエリの静的エラーを
    // 検出できるよう、サブクエリを含む述語の控えも持つ。
    let subquery_preds: Vec<WherePredicate> = validated
        .where_predicates
        .iter()
        .filter(|p| predicate_contains_subquery(p))
        .cloned()
        .collect();
    let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
    chain.push(&inner_schema);
    validated.where_predicates = match resolve_where_predicates(
        validated.where_predicates,
        &chain,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_value_budget,
    ) {
        Ok(p) => p,
        Err(e) => {
            // スキャン側と同じ理由で、入れ子 WHERE の失敗時も投影メタデータを確定する。
            if is_deferrable_runtime_error(&e) {
                if let Some(static_err) = first_static_error_in_subquery_predicates(
                    &subquery_preds,
                    &chain,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                ) {
                    return Err(static_err);
                }
            }
            validated.where_predicates = static_preds;
            let bound = super::parser::bind_aggregate(&validated, &inner_schema, udfs)?;
            *meta_sink = Some(super::describe::aggregate_columns(&bound, &inner_schema)?);
            return Err(e);
        }
    };
    let bound = super::parser::bind_aggregate(&validated, &inner_schema, udfs)?;
    *meta_sink = Some(super::describe::aggregate_columns(&bound, &inner_schema)?);
    super::aggregate::execute_aggregate_with_cache(
        read_txn,
        ctx,
        &inner_schema,
        &bound,
        super::aggregate::MAX_AGGREGATE_RESULT_BYTES,
        None,
        None,
        None,
    )
}

/// 外側の行数が判明するまで遅延してよい実行時エラーか（入れ子スカラーサブクエリの
/// `21000`、または静的な `22000` 以外の式評価データ例外 `22xxx`）。
/// [`resolve_scalar_projection_items`] と内側クエリの失敗時静的検証から呼ばれる。
fn is_deferrable_runtime_error(e: &SqlSurfaceError) -> bool {
    matches!(e, SqlSurfaceError::CardinalityViolation)
        || (e.wire_code().starts_with("22") && e.wire_code() != "22000")
}

/// 先行サブクエリが遅延対象エラーで解決を打ち切った後に、サブクエリを含む各述語を
/// 個別に解決し直して、遅延対象でない（静的・構造的な）エラーを最初の 1 件返す。
/// 遅延対象エラーと成功は無視する。過去の予算消費に影響されないよう、プローブ用に
/// 予算を初期値で独立に確保する（エラー経路専用。通常経路では呼ばれない）。
fn first_static_error_in_subquery_predicates(
    subquery_preds: &[WherePredicate],
    chain: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
) -> Option<SqlSurfaceError> {
    let mut budget = MAX_SUBQUERY_EXECUTIONS;
    let mut in_value_budget = MAX_SUBQUERY_IN_VALUES;
    for pred in subquery_preds {
        if let Err(e) = resolve_where_predicates(
            vec![pred.clone()],
            chain,
            read_txn,
            ctx,
            lookup,
            udfs,
            &mut budget,
            &mut in_value_budget,
        ) {
            if !is_deferrable_runtime_error(&e) {
                return Some(e);
            }
        }
    }
    None
}

/// 述語がサブクエリ（`IN`／`EXISTS`／スカラー比較）を含むかを再帰的に判定する。
/// 入れ子 WHERE の実行時エラー時に、サブクエリを含まない述語だけで静的検証を
/// 完了させるために使う（`execute_inner_query` 系から呼ばれる）。
fn predicate_contains_subquery(pred: &WherePredicate) -> bool {
    match pred {
        WherePredicate::InSubquery { .. }
        | WherePredicate::Exists { .. }
        | WherePredicate::ScalarSubqueryCompare { .. } => true,
        WherePredicate::Not(inner) => predicate_contains_subquery(inner),
        WherePredicate::Or(branches) => branches.iter().flatten().any(predicate_contains_subquery),
        _ => false,
    }
}

/// `outer_scopes` の末尾（＝サブクエリを含む文自身のテーブル）を返す。
fn owner_schema<'a>(outer_scopes: &[&'a TableSchema]) -> Result<&'a TableSchema, SqlSurfaceError> {
    outer_scopes
        .last()
        .copied()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery resolution without an owner schema".to_string(),
        })
}

/// サブクエリの値比較で使う「値族」（型の暗黙同一視を避けるため、対象列と内側の
/// 投影列は同じ族でなければ `22000` で拒否する。PR #1103 追加 codex-review P1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubqueryValueFamily {
    /// `TEXT`／`ENUM`（ラベルを文字列として比較する）。
    Text,
    Boolean,
    Date,
    Timestamp,
    Numeric,
    Uuid,
    Bytea,
    /// `INTEGER`／`BIGINT`／疑似列 `id`。
    Integer,
    /// `REAL`／`DOUBLE PRECISION`（スカラー比較のみ。`IN` の対象外）。
    Float,
}

impl SubqueryValueFamily {
    fn is_number(self) -> bool {
        matches!(self, Self::Integer | Self::Float)
    }
}

/// 列型から値族を判定する。対応外（`VECTOR`・配列・JSON 等）は `None`。
fn family_of_type(ty: &ColumnType) -> Option<SubqueryValueFamily> {
    match ty {
        ColumnType::Text | ColumnType::Enum(_) => Some(SubqueryValueFamily::Text),
        ColumnType::Boolean => Some(SubqueryValueFamily::Boolean),
        ColumnType::Date => Some(SubqueryValueFamily::Date),
        ColumnType::Timestamp => Some(SubqueryValueFamily::Timestamp),
        ColumnType::Numeric { .. } => Some(SubqueryValueFamily::Numeric),
        ColumnType::Uuid => Some(SubqueryValueFamily::Uuid),
        ColumnType::Bytea => Some(SubqueryValueFamily::Bytea),
        ColumnType::Integer | ColumnType::BigInt => Some(SubqueryValueFamily::Integer),
        ColumnType::Real | ColumnType::Double => Some(SubqueryValueFamily::Float),
        ColumnType::Vector(_) | ColumnType::Array(_) | ColumnType::Json | ColumnType::Jsonb => None,
    }
}

/// 内側の単一投影列の静的メタデータ（[`ColumnMeta`]）から値族を判定する。
/// `None` は「対応外（値族が不明）」を表し、呼び出し元は `22000` で拒否する。
/// 疑似列 `id`（[`ColumnMeta::Id`]）は整数族、式・集計列（[`ColumnMeta::Computed`]）は
/// 束縛時に確定した静的型（Issue #1173）があればそれ、無ければ `None`。
fn inner_value_family(meta: &ColumnMeta) -> Option<SubqueryValueFamily> {
    match meta {
        ColumnMeta::Id => Some(SubqueryValueFamily::Integer),
        ColumnMeta::Scalar { ty, .. } => family_of_type(ty),
        ColumnMeta::Computed { ty: Some(ty), .. } => family_of_type(ty),
        ColumnMeta::Computed { ty: None, .. } => None,
    }
}

/// `<column> [NOT] IN (SELECT ...)` の対象列 `column` を `outer_schema`（この
/// サブクエリを含む文自身のテーブルのスキーマ）に対して検証し、列型（疑似列 `id` は
/// 型なし＝`None`）と値族を返す。存在しない列は `22000`（`unknown column`。通常の
/// `WHERE` 等価述語束縛と同じ文言・`wire_code`）、`IN` で扱えない型（`VECTOR`・配列・
/// JSON）は同じく `22000` で拒否する。
///
/// 疑似列 `id` はスキーマに同名の実カラムが無い場合に限り整数族として受理する
/// （実カラム優先。`udf_call` の識別子束縛・スカラー比較〔[`resolve_scalar_compare`]〕
/// と同じ規則。Issue #1352）。`REAL`／`DOUBLE PRECISION` は浮動小数族として受理する。
///
/// この検証は内側サブクエリの結果行数（0 行・NULL のみを含む）に一切依存しない
/// （PR #1103 codex-review P1 指摘対応: 内側が 0 行／NULL のみでも列名・型検証を
/// 回避したまま「空結果で成功」してしまうことを防ぐ）。
fn validate_in_target_column<'a>(
    column: &str,
    outer_schema: &'a TableSchema,
) -> Result<(Option<&'a ColumnType>, SubqueryValueFamily), SqlSurfaceError> {
    let Some(column_def) = outer_schema.columns.iter().find(|c| c.name == column) else {
        if column == "id" {
            return Ok((None, SubqueryValueFamily::Integer));
        }
        return Err(SqlSurfaceError::invalid_input(format!(
            "unknown column: {column}"
        )));
    };
    match family_of_type(&column_def.ty) {
        Some(family) => Ok((Some(&column_def.ty), family)),
        None => Err(SqlSurfaceError::invalid_input(format!(
            "column {column:?} type is not supported as a subquery IN target"
        ))),
    }
}

/// `IN`／`NOT IN (SELECT ...)` を解決し、連言の述語列を返す。内側は投影列が
/// ちょうど 1 列であることを要求し（`42601`）、[`validate_in_target_column`] で
/// 対象列を検証した上で、内側の投影列の値族が対象列の値族と一致することを検証する
/// （内側の結果行数・値に依存しない静的検証）。
///
/// 非否定は distinct 値を集合照合の `Or` へ（0 行・NULL のみなら空の `Or`＝常に偽）。
/// 否定（`NOT IN`）は NULL 規則に従う: 内側 0 行なら述語なし（常に真）、内側に NULL を
/// 含めば空の `Or`（常に偽）、それ以外は distinct 値ごとの否定の連言（外側値が NULL の
/// 行は UNKNOWN として除外される）。NULL の検出は語彙外 ENUM ラベルの除外より前に行う。
///
/// `in_value_budget` は文全体で共有する残り distinct 値予算（[`MAX_SUBQUERY_IN_VALUES`]
/// 参照）で、枯渇したら `54000` で拒否する（Issue #1165・SQL-29・TASK-213）。
#[allow(clippy::too_many_arguments)]
fn resolve_in_subquery(
    column: &str,
    inner_tokens: &[Token],
    depth: usize,
    negated: bool,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    let result = execute_inner_query(
        inner_tokens,
        depth,
        InnerScanIntent::Values,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_value_budget,
    )?;
    if result.columns.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used with IN must select exactly one column",
        ));
    }
    // 内側の結果行数（0 行を含む）に関わらず、対象列の存在・型を必ず検証する。
    let (target_ty, family) = validate_in_target_column(column, owner_schema(outer_scopes)?)?;

    // 内側の結果行数・値に一切依存しない静的な組合せ検証（PR #1103 追加
    // codex-review P1 指摘対応）。`result.columns` は上で長さ 1 を確認済みだが、
    // untrusted 入力経路のため `[0]` ではなく `first()` で明示的に扱う。
    let inner_meta = result
        .columns
        .first()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery result missing projected column metadata".to_string(),
        })?;
    if inner_value_family(inner_meta) != Some(family) {
        return Err(SqlSurfaceError::invalid_input(format!(
            "subquery projection type is not compatible with IN target column {column:?} \
             (the subquery projection must have the same value family as the target column)"
        )));
    }

    let inner_empty = result.rows.is_empty();
    let mut has_null = false;
    let mut texts: Vec<String> = Vec::new();
    let mut bools: Vec<bool> = Vec::new();
    let mut ints: Vec<i128> = Vec::new();
    let mut floats: Vec<f64> = Vec::new();
    for row in &result.rows {
        let cell = row.cells.first().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery row missing projected cell".to_string(),
        })?;
        if matches!(cell, Cell::Null) {
            // NULL は照合集合へ入れず、`NOT IN` 判定用に検出だけ行う。
            has_null = true;
            continue;
        }
        match family {
            SubqueryValueFamily::Text => {
                let value = typed_cell_text(cell, family)?;
                // 対象列が ENUM の場合、語彙外ラベルは「その値には一致しない」
                // として除外する（除外しないと後段の束縛が 22P02 で文全体を
                // 落とす。PR #1103 追加 codex-review P1 指摘対応）。
                if let Some(ColumnType::Enum(def)) = target_ty {
                    if !def.contains(&value) {
                        continue;
                    }
                }
                texts.push(value);
            }
            SubqueryValueFamily::Date
            | SubqueryValueFamily::Timestamp
            | SubqueryValueFamily::Numeric
            | SubqueryValueFamily::Uuid
            | SubqueryValueFamily::Bytea => texts.push(typed_cell_text(cell, family)?),
            SubqueryValueFamily::Boolean => match cell {
                Cell::Bool(b) => bools.push(*b),
                _ => return Err(unexpected_cell_type()),
            },
            SubqueryValueFamily::Integer => match cell {
                Cell::SignedInteger(n) => ints.push(i128::from(*n)),
                // 疑似列 `id` は `u64` 全域を取り得るため、`i128` で保持して落とさない
                // （`i64` 超の値も等価比較の対象とする。範囲外の写像は `int_compare` が担う）。
                Cell::Integer(n) => ints.push(i128::from(*n)),
                _ => return Err(unexpected_cell_type()),
            },
            SubqueryValueFamily::Float => match cell {
                // 非有限値（NaN・無限大）は比較が定義できないため、スカラー比較
                // （[`number_cell_text`]）と同じく `22000` で fail-closed にする。
                Cell::Float(f) if f.is_finite() => {
                    // `-0.0` と `+0.0` は等しいため `+0.0` へ正規化して重複除去へ載せる。
                    floats.push(if *f == 0.0 { 0.0 } else { *f });
                }
                Cell::Float(_) => {
                    return Err(SqlSurfaceError::invalid_input(
                        "subquery returned a non-finite number",
                    ))
                }
                _ => return Err(unexpected_cell_type()),
            },
        }
    }

    // 重複除去（IN は集合所属であり重複は結果に影響しない。Cursor Bugbot 指摘対応）。
    // 予算は distinct 値単位で消費する。
    texts.sort_unstable();
    texts.dedup();
    bools.sort_unstable();
    bools.dedup();
    ints.sort_unstable();
    ints.dedup();
    floats.sort_by(f64::total_cmp);
    floats.dedup();
    let distinct = texts.len() + bools.len() + ints.len() + floats.len();
    // 整数・浮動小数列は値ごとの式述語（式ノード予算あり）へ展開するため、1 サイトの
    // distinct 値数を式ノード予算に収まる上限へ抑える。
    if matches!(
        family,
        SubqueryValueFamily::Integer | SubqueryValueFamily::Float
    ) {
        let cap = if negated {
            MAX_INT_NOT_IN_VALUES
        } else {
            crate::declarative_filter::MAX_IN_LIST_ITEMS
        };
        if ints.len().max(floats.len()) > cap {
            return Err(SqlSurfaceError::payload_too_large(format!(
                "subquery IN distinct value count exceeds limit {cap} for numeric columns"
            )));
        }
    }
    *in_value_budget = in_value_budget.checked_sub(distinct).ok_or_else(|| {
        SqlSurfaceError::payload_too_large(format!(
            "subquery IN distinct value count exceeds limit {MAX_SUBQUERY_IN_VALUES}"
        ))
    })?;

    if !negated {
        let predicate = match family {
            SubqueryValueFamily::Boolean => WherePredicate::Or(
                bools
                    .into_iter()
                    .map(|value| {
                        vec![WherePredicate::BoolEquality {
                            column: column.to_string(),
                            value,
                        }]
                    })
                    .collect(),
            ),
            SubqueryValueFamily::Integer => WherePredicate::Or(
                ints.into_iter()
                    .map(|n| vec![target_int_compare(column, target_ty, BinOp::Eq, n)])
                    .collect(),
            ),
            SubqueryValueFamily::Float => WherePredicate::Or(
                floats
                    .into_iter()
                    .map(|f| float_compare(column, BinOp::Eq, f).map(|p| vec![p]))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            _ => build_in_set_predicate(column, texts),
        };
        return Ok(vec![predicate]);
    }

    // `NOT IN`: 内側 0 行なら常に真（外側値が NULL でも真）。
    if inner_empty {
        return Ok(Vec::new());
    }
    // 内側に NULL を含めば `x NOT IN (..., NULL)` は真にならない（偽か UNKNOWN）。
    if has_null {
        return Ok(vec![WherePredicate::Or(Vec::new())]);
    }
    // 非 NULL の照合集合が空（語彙外 ENUM ラベルのみ等）でも、内側は非空のため
    // 外側値が非 NULL の行だけが真になる（NULL 行は UNKNOWN で除外）。
    let not_null = || WherePredicate::IsNull {
        column: column.to_string(),
        negated: true,
    };
    Ok(match family {
        SubqueryValueFamily::Boolean => match bools.as_slice() {
            [] => vec![not_null()],
            [only] => vec![WherePredicate::BoolEquality {
                column: column.to_string(),
                value: !*only,
            }],
            _ => vec![WherePredicate::Or(Vec::new())],
        },
        SubqueryValueFamily::Integer => {
            if ints.is_empty() {
                vec![not_null()]
            } else {
                ints.into_iter()
                    .map(|n| {
                        WherePredicate::Or(vec![
                            vec![target_int_compare(column, target_ty, BinOp::Lt, n)],
                            vec![target_int_compare(column, target_ty, BinOp::Gt, n)],
                        ])
                    })
                    .collect()
            }
        }
        SubqueryValueFamily::Float => {
            if floats.is_empty() {
                vec![not_null()]
            } else {
                floats
                    .into_iter()
                    .map(|f| {
                        Ok(WherePredicate::Or(vec![
                            vec![float_compare(column, BinOp::Lt, f)?],
                            vec![float_compare(column, BinOp::Gt, f)?],
                        ]))
                    })
                    .collect::<Result<Vec<_>, SqlSurfaceError>>()?
            }
        }
        _ => {
            if texts.is_empty() {
                vec![not_null()]
            } else {
                // 各チャンクを `Not(InList)` として連言で並べる。評価は宣言的
                // フィルタの三値評価（外側値が NULL の行は UNKNOWN で除外）。
                let WherePredicate::Or(branches) = build_in_set_predicate(column, texts) else {
                    return Err(SqlSurfaceError::Internal {
                        detail: "build_in_set_predicate returned a non-Or predicate".to_string(),
                    });
                };
                branches
                    .into_iter()
                    .flatten()
                    .map(|leaf| WherePredicate::Not(Box::new(leaf)))
                    .collect()
            }
        }
    })
}

/// 整数値 `n` との比較述語を対象列に応じて組み立てる。疑似列 `id`（スキーマに同名の
/// 実カラムが無く `target_ty == None`）は `u64` 全域を取り得るため、式評価器の `f64`
/// 写像（`2^53` 超で `22003`）を避けて厳密整数比較の [`WherePredicate::IdCompare`] を
/// 返す。実カラム（`BIGINT` 等）は従来どおり [`int_compare`] の式述語を返す。
/// `op` は `Eq`／`Lt`／`Le`／`Gt`／`Ge` のみを想定する（Issue #1352）。
fn target_int_compare(
    column: &str,
    target_ty: Option<&ColumnType>,
    op: BinOp,
    n: i128,
) -> WherePredicate {
    if target_ty.is_none() && column == "id" {
        WherePredicate::IdCompare { op, value: n }
    } else {
        int_compare(column, op, n)
    }
}

/// 式評価器（`f64`）が正確に表現できる整数の絶対値上限（`2^53`）。列値側の検査
/// （`numeric_scalar_from_ref`・`id_as_finite_scalar`）と同じ境界。
const MAX_EXACT_INT_ABS: i128 = 1 << 53;

/// `<column> <op> <整数>` の式述語（数値リテラルの比較と同じ AST 形）。
///
/// 内側の値 `n` が `2^53` を超える場合、数値リテラルへ直接変換すると式束縛の
/// 正確表現ガード（`parse_number_literal`）に拒否され、正当な比較まで `22003`
/// になる（PR #1235 codex-review P1）。範囲内の列値は `|v| <= 2^53` なので、
/// 範囲外の `n` との大小関係は符号だけで決まる。そこで範囲外の `n` は境界値
/// （`±2^53`）との等価な比較へ写像する。列参照は式に残すため、範囲外の列値は
/// 従来どおり評価時に `22003`（fail-closed）で拒否され、黙って丸められない。
/// `op` は `Eq`／`Lt`／`Le`／`Gt`／`Ge` のみを想定する。
fn int_compare(column: &str, op: BinOp, n: i128) -> WherePredicate {
    let (op, n) = if n > MAX_EXACT_INT_ABS {
        let op = match op {
            BinOp::Lt | BinOp::Le => BinOp::Le,
            _ => BinOp::Gt,
        };
        (op, MAX_EXACT_INT_ABS)
    } else if n < -MAX_EXACT_INT_ABS {
        let op = match op {
            BinOp::Gt | BinOp::Ge => BinOp::Ge,
            _ => BinOp::Lt,
        };
        (op, -MAX_EXACT_INT_ABS)
    } else {
        (op, n)
    };
    WherePredicate::Expression(Expr::Binary {
        op,
        lhs: Box::new(Expr::Ident(column.to_string())),
        rhs: Box::new(Expr::Number(n.to_string())),
    })
}

/// `<column> <op> <浮動小数>` の式述語（浮動小数リテラルの比較と同じ AST 形）。
/// `f` は有限値のみ（呼び出し側で検査済み）。`REAL` 列の値は `f64` へ無損失拡大して
/// 評価されるため、内側の `REAL` セル（同じ拡大値）とは厳密に一致し、`DOUBLE` 値とは
/// PostgreSQL と同じく拡大後の値どうしで比較される（Issue #1352）。
fn float_compare(column: &str, op: BinOp, f: f64) -> Result<WherePredicate, SqlSurfaceError> {
    Ok(WherePredicate::Expression(Expr::Binary {
        op,
        lhs: Box::new(Expr::Ident(column.to_string())),
        rhs: Box::new(Expr::Number(number_cell_text(&Cell::Float(f))?)),
    }))
}

/// 解決済みの投影位置スカラーサブクエリ 1 項目（Issue #1352）。
#[derive(Debug, Clone)]
pub(crate) struct ResolvedScalarItem {
    position: usize,
    meta: ColumnMeta,
    cell: Cell,
    /// 内側が 2 行以上を返したか。外側の結果が 1 行以上のときだけエラーにする
    /// （PostgreSQL の遅延評価と同じ。判定は自テナント可視行のみで決まる）。
    multi_row: bool,
    /// 内側の実行時エラー（式評価のデータ例外 `22xxx`。0 除算 `22012`・数値あふれ `22003` 等。静的な `22000` は含めない）。外側が 1 行以上の
    /// ときだけ返す（PostgreSQL の遅延評価と同じ。外側 0 行では内側は評価されない）。
    /// 静的エラー（`42xxx`・`22000`・`54000` 等）はここへ入れず解決時点で即返す。
    deferred_error: Option<SqlSurfaceError>,
}

/// 投影位置のスカラーサブクエリ（Issue #1352・SQL-29 (a)・RLS-10 (b)・TASK-213）を
/// すべて実行し、列メタデータと値（0 行は NULL）へ解決する。`core.rs` の
/// `Statement::Scan` アームが WHERE 側の解決（[`resolve_where_predicates`]）と同じ
/// `budget`／`in_value_budget`・`read_txn`・`ctx` で呼ぶ。内側は WHERE 側と同じ
/// 経路（相関拒否・RLS 暗黙適用）を通る。静的エラー（未知列・投影列数・相関）は
/// 外側の行数に関わらず返る。
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_scalar_projection_items(
    items: &[ScalarSubqueryItem],
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<Vec<ResolvedScalarItem>, SqlSurfaceError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let mut meta_sink: Option<Vec<ColumnMeta>> = None;
        let result = match execute_inner_query_with_meta(
            &item.inner_tokens,
            item.depth,
            InnerScanIntent::ScalarValue,
            outer_scopes,
            read_txn,
            ctx,
            lookup,
            udfs,
            budget,
            in_value_budget,
            &mut meta_sink,
        ) {
            Ok(r) => r,
            // 束縛に成功した後の実行時データ例外（`22xxx`。0 除算・数値あふれ等）だけを、
            // 外側の行数が判明するまで遅延する。束縛前に失敗した静的エラー（`meta_sink` が
            // 未確定。`22P02` 等の bind 時エラーを含む）と `22000` は即返す。列メタデータは
            // 実行とは独立に束縛結果から確定済みのため、外側の行数で列型は変わらない。
            // 内側 WHERE に入れ子になったスカラーサブクエリの行数違反（`21000`。投影位置
            // サブクエリ自身の 2 行以上は `multi_row` で扱うため、ここへ届く `21000` は
            // 入れ子由来のみ）も、`22xxx` と同じく外側の行数が判明するまで遅延する
            // （Issue #1432。SQL-29・ERR-6）。`meta_sink` 未確定なら従来どおり即返す。
            Err(e) if is_deferrable_runtime_error(&e) => {
                let inner_meta = match meta_sink.as_deref() {
                    Some([m]) => m.clone(),
                    _ => return Err(e),
                };
                out.push(ResolvedScalarItem {
                    position: item.position,
                    meta: alias_scalar_meta(&inner_meta, item.alias.as_ref()),
                    cell: Cell::Null,
                    multi_row: false,
                    deferred_error: Some(e),
                });
                continue;
            }
            Err(e) => return Err(e),
        };
        if result.columns.len() != 1 {
            return Err(SqlSurfaceError::unsupported(
                "subquery used as a value must select exactly one column",
            ));
        }
        let inner_meta = result
            .columns
            .first()
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "subquery result missing projected column metadata".to_string(),
            })?;
        let meta = alias_scalar_meta(inner_meta, item.alias.as_ref());
        let multi_row = result.rows.len() > 1;
        let cell = match result.rows.first() {
            None => Cell::Null,
            Some(_) if multi_row => Cell::Null,
            Some(row) => row
                .cells
                .first()
                .cloned()
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "subquery row missing projected cell".to_string(),
                })?,
        };
        out.push(ResolvedScalarItem {
            position: item.position,
            meta,
            cell,
            multi_row,
            deferred_error: None,
        });
    }
    Ok(out)
}

/// 内側の投影列メタデータへ別名を適用する。別名があれば列名だけ差し替え、型 OID の
/// 根拠となる型は保持する。疑似列 `id`（名前を持てない）は JOIN の別名処理と同じく
/// numeric 静的型の `Computed` へ載せ替える（wire 上の型は `Id` と同じ numeric）。
fn alias_scalar_meta(inner_meta: &ColumnMeta, alias: Option<&String>) -> ColumnMeta {
    match (inner_meta, alias) {
        (meta, None) => meta.clone(),
        (ColumnMeta::Scalar { ty, .. }, Some(alias)) => ColumnMeta::Scalar {
            name: alias.clone(),
            ty: ty.clone(),
        },
        (ColumnMeta::Computed { ty, .. }, Some(alias)) => ColumnMeta::Computed {
            name: alias.clone(),
            ty: ty.clone(),
        },
        (ColumnMeta::Id, Some(alias)) => ColumnMeta::Computed {
            name: alias.clone(),
            ty: Some(crate::catalog::ColumnType::Numeric {
                precision: 20,
                scale: 0,
            }),
        },
    }
}

/// 解決済みの投影位置スカラーサブクエリを、外側の結果（投影位置の項目を含まない列）へ
/// SELECT リスト上の位置どおりに合流する（Issue #1352）。
///
/// - 内側が 2 行以上で外側の結果が 1 行以上なら `21000`（Issue #1404）。外側が 0 行ならエラーにしない。
/// - 外側結果の推定バイトに追加セルの推定バイト（全行ぶん）を加えた合計を確保前に `checked_*` で検査し、結果バイト上限
///   （[`crate::arena::MAX_ARENA_TOTAL_BYTES`]。`sql::scan` の結果バイト上限と同値）を
///   超えれば `54000`。
/// - 位置が現在の列数を超える場合は `Internal`（fail-closed。添字アクセスは使わない）。
pub(crate) fn merge_scalar_projection_items(
    mut result: super::exec::QueryResult,
    items: Vec<ResolvedScalarItem>,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    // 投影セルが無ければ合流しない。外側結果は `sql::scan` が同じ上限で検査済みのため再検査しない。
    if items.is_empty() {
        return Ok(result);
    }
    if !result.rows.is_empty() {
        if let Some(e) = items.iter().find_map(|i| i.deferred_error.clone()) {
            return Err(e);
        }
    }
    if !result.rows.is_empty() && items.iter().any(|i| i.multi_row) {
        return Err(SqlSurfaceError::cardinality_violation());
    }
    // 外側結果の使用量を引き継ぎ、追加分との合計を確保前に検査する。
    let mut total_bytes: usize = super::cursor::estimate_result_bytes(&result);
    for item in &items {
        let per_cell = std::mem::size_of::<Cell>()
            .checked_add(super::cursor::estimate_cell_bytes(&item.cell))
            .ok_or_else(merge_too_large)?;
        let added = per_cell
            .checked_mul(result.rows.len())
            .ok_or_else(merge_too_large)?;
        total_bytes = total_bytes.checked_add(added).ok_or_else(merge_too_large)?;
        if total_bytes > crate::arena::MAX_ARENA_TOTAL_BYTES {
            return Err(merge_too_large());
        }
    }
    for item in items {
        if item.position > result.columns.len() {
            return Err(SqlSurfaceError::Internal {
                detail: "scalar subquery position out of range".to_string(),
            });
        }
        result.columns.insert(item.position, item.meta);
        for row in &mut result.rows {
            if item.position > row.cells.len() {
                return Err(SqlSurfaceError::Internal {
                    detail: "scalar subquery position out of range".to_string(),
                });
            }
            row.cells.insert(item.position, item.cell.clone());
        }
    }
    Ok(result)
}

fn merge_too_large() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large("scalar subquery projection result exceeds size limit")
}

/// 期待外のセル型（静的な値族検証を通ったのに実行時セルが食い違う場合）。fail-closed。
fn unexpected_cell_type() -> SqlSurfaceError {
    SqlSurfaceError::invalid_input("subquery value type is not compatible with the target column")
}

/// 非数値の値族のセルを、既存の型付き束縛が厳密に往復できる正準テキストへ
/// 変換する（wire のテキスト出力と同じ既存フォーマッタを使う）。
fn typed_cell_text(cell: &Cell, family: SubqueryValueFamily) -> Result<String, SqlSurfaceError> {
    match (family, cell) {
        (SubqueryValueFamily::Text, Cell::Text(s)) => Ok(s.clone()),
        (SubqueryValueFamily::Date, Cell::Date(d)) => Ok(crate::datetime::format_date(*d)),
        (SubqueryValueFamily::Timestamp, Cell::Timestamp(t)) => {
            Ok(crate::datetime::format_timestamp(*t))
        }
        (SubqueryValueFamily::Numeric, Cell::Numeric(d)) => Ok(d.to_string()),
        (SubqueryValueFamily::Uuid, Cell::Uuid(u)) => Ok(u.to_string()),
        (SubqueryValueFamily::Bytea, Cell::Bytes(b)) => Ok(crate::bytea::format_hex_text(b)),
        _ => Err(unexpected_cell_type()),
    }
}

/// 数値族（整数・浮動小数）のセルを数値リテラルのテキストへ変換する。
/// 非有限の浮動小数は `22000` で拒否する（比較が定義できないため）。
fn number_cell_text(cell: &Cell) -> Result<String, SqlSurfaceError> {
    match cell {
        Cell::Integer(n) => Ok(n.to_string()),
        Cell::SignedInteger(n) => Ok(n.to_string()),
        // 2^53 以上の整数値は全桁の整数リテラルになり、式束縛の正確表現ガード
        // （`parse_number_literal`）に拒否されるため、小数点付きで表す。
        Cell::Float(f)
            if f.is_finite() && f.fract() == 0.0 && f.abs() >= 9_007_199_254_740_992.0 =>
        {
            Ok(format!("{f:.1}"))
        }
        Cell::Float(f) if f.is_finite() => Ok(f.to_string()),
        Cell::Float(_) => Err(SqlSurfaceError::invalid_input(
            "subquery returned a non-finite number",
        )),
        _ => Err(unexpected_cell_type()),
    }
}

/// `<column> <op> (SELECT ...)`（スカラーサブクエリ。Issue #1191）を解決し、
/// `<column> <op> <リテラル>` と同じ AST の述語 1 つを返す。
///
/// 内側は単一列の Scan または単一集計項目の集計形。静的検証（投影列数・対象列の
/// 存在・値族の一致）は内側の行数・値に依存せず先に行う。結果は 0 行または NULL なら
/// UNKNOWN（空の `Or`＝常に偽。否定は構文段で演算子反転済み）、1 行ならその値、
/// 2 行以上なら先頭行を採用せず `21000` でエラーにする（判定は内側の可視行のみに
/// 依存するため、他テナント行では発火しない）。
#[allow(clippy::too_many_arguments)]
fn resolve_scalar_compare(
    column: &str,
    op: ScalarSubqueryOp,
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<WherePredicate, SqlSurfaceError> {
    let result = execute_inner_query(
        inner_tokens,
        depth,
        InnerScanIntent::Scalar,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_value_budget,
    )?;
    if result.columns.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used as a value must select exactly one column",
        ));
    }
    // 対象列（疑似列 `id` は整数族）。行数に依存せず必ず検証する。
    let owner = owner_schema(outer_scopes)?;
    let (target_ty, target_family) = match owner.columns.iter().find(|c| c.name == column) {
        Some(def) => (
            Some(&def.ty),
            family_of_type(&def.ty).ok_or_else(|| {
                SqlSurfaceError::invalid_input(format!(
                    "column {column:?} type is not supported for a scalar subquery comparison"
                ))
            })?,
        ),
        None if column == "id" => (None, SubqueryValueFamily::Integer),
        None => {
            return Err(SqlSurfaceError::invalid_input(format!(
                "unknown column: {column}"
            )))
        }
    };
    let inner_meta = result
        .columns
        .first()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery result missing projected column metadata".to_string(),
        })?;
    let compatible = match inner_value_family(inner_meta) {
        Some(inner) => inner == target_family || (inner.is_number() && target_family.is_number()),
        None => false,
    };
    if !compatible {
        return Err(SqlSurfaceError::invalid_input(format!(
            "subquery result type is not compatible with column {column:?} \
             (the subquery must have the same value family as the column)"
        )));
    }

    let mut rows = result.rows.iter();
    let Some(first) = rows.next() else {
        return Ok(WherePredicate::Or(Vec::new()));
    };
    if rows.next().is_some() {
        return Err(SqlSurfaceError::cardinality_violation());
    }
    let cell = first
        .cells
        .first()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery row missing projected cell".to_string(),
        })?;
    if matches!(cell, Cell::Null) {
        return Ok(WherePredicate::Or(Vec::new()));
    }
    scalar_literal_predicate(column, op, cell, target_ty, target_family)
}

/// スカラーサブクエリの 1 セルを、同じ列型で `col <op> <リテラル>` と書いたときに
/// パーサーが生成する AST と同一の述語へ変換する（第 2 の評価器を作らない）。
/// パーサーでは生じない分岐（`<>`・ENUM の語彙外値）は明示的に扱う。
///
/// - `TEXT`／`ENUM`: `=` は `Equality`、`<>` は `Not(Equality)`、範囲は `Compare`
///   （`TEXT` のみ。`ENUM` の範囲比較は既存契約どおり `22000`）。
/// - `BOOLEAN`: `=`／`<>` のみ（`BoolEquality`）。範囲は `22000`。
/// - `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA`: `=` は `Equality`、`<>` は
///   `Not(Equality)`、範囲は `Compare`（値は正準テキスト）。
/// - 整数・浮動小数（疑似列 `id` を含む）: 式述語 `Expression`（`<>` は `<` と `>` の `Or`）。
fn scalar_literal_predicate(
    column: &str,
    op: ScalarSubqueryOp,
    cell: &Cell,
    target_ty: Option<&ColumnType>,
    family: SubqueryValueFamily,
) -> Result<WherePredicate, SqlSurfaceError> {
    let col = column.to_string();
    let compare_op = |op: ScalarSubqueryOp| match op {
        ScalarSubqueryOp::Lt => Some(CompareOp::Lt),
        ScalarSubqueryOp::Le => Some(CompareOp::Le),
        ScalarSubqueryOp::Gt => Some(CompareOp::Gt),
        ScalarSubqueryOp::Ge => Some(CompareOp::Ge),
        ScalarSubqueryOp::Eq | ScalarSubqueryOp::Ne => None,
    };
    match family {
        SubqueryValueFamily::Boolean => {
            let Cell::Bool(b) = cell else {
                return Err(unexpected_cell_type());
            };
            match op {
                ScalarSubqueryOp::Eq => Ok(WherePredicate::BoolEquality {
                    column: col,
                    value: *b,
                }),
                // 非 NULL 行では `<> b` は `= !b` と同値（NULL 行は共に UNKNOWN）。
                ScalarSubqueryOp::Ne => Ok(WherePredicate::BoolEquality {
                    column: col,
                    value: !*b,
                }),
                _ => Err(SqlSurfaceError::invalid_input(
                    "range comparison is not supported for BOOLEAN columns",
                )),
            }
        }
        SubqueryValueFamily::Integer
            if matches!(cell, Cell::Integer(_) | Cell::SignedInteger(_)) =>
        {
            // 整数列 × 整数セルは `2^53` 超でも比較できるよう `int_compare` へ写像する。
            let n: i128 = match cell {
                Cell::Integer(u) => i128::from(*u),
                Cell::SignedInteger(i) => i128::from(*i),
                _ => return Err(unexpected_cell_type()),
            };
            Ok(match op {
                ScalarSubqueryOp::Eq => target_int_compare(column, target_ty, BinOp::Eq, n),
                ScalarSubqueryOp::Lt => target_int_compare(column, target_ty, BinOp::Lt, n),
                ScalarSubqueryOp::Le => target_int_compare(column, target_ty, BinOp::Le, n),
                ScalarSubqueryOp::Gt => target_int_compare(column, target_ty, BinOp::Gt, n),
                ScalarSubqueryOp::Ge => target_int_compare(column, target_ty, BinOp::Ge, n),
                ScalarSubqueryOp::Ne => WherePredicate::Or(vec![
                    vec![target_int_compare(column, target_ty, BinOp::Lt, n)],
                    vec![target_int_compare(column, target_ty, BinOp::Gt, n)],
                ]),
            })
        }
        SubqueryValueFamily::Integer | SubqueryValueFamily::Float => {
            let number = Expr::Number(number_cell_text(cell)?);
            let cmp = |op: BinOp| {
                WherePredicate::Expression(Expr::Binary {
                    op,
                    lhs: Box::new(Expr::Ident(column.to_string())),
                    rhs: Box::new(number.clone()),
                })
            };
            Ok(match op {
                ScalarSubqueryOp::Eq => cmp(BinOp::Eq),
                ScalarSubqueryOp::Lt => cmp(BinOp::Lt),
                ScalarSubqueryOp::Le => cmp(BinOp::Le),
                ScalarSubqueryOp::Gt => cmp(BinOp::Gt),
                ScalarSubqueryOp::Ge => cmp(BinOp::Ge),
                // `BinOp` に `<>` が無いため `Or([<], [>])`（NULL 行は共に UNKNOWN）。
                ScalarSubqueryOp::Ne => {
                    WherePredicate::Or(vec![vec![cmp(BinOp::Lt)], vec![cmp(BinOp::Gt)]])
                }
            })
        }
        _ => {
            let value = typed_cell_text(cell, family)?;
            if let (SubqueryValueFamily::Text, Some(ColumnType::Enum(def))) = (family, target_ty) {
                if compare_op(op).is_some() {
                    return Err(SqlSurfaceError::invalid_input(
                        "range comparison is not supported for ENUM columns",
                    ));
                }
                if !def.contains(&value) {
                    // 語彙外の値は、どの行とも等しくない。`=` は常に偽、`<>` は
                    // 非 NULL の全行で真。
                    return Ok(match op {
                        ScalarSubqueryOp::Eq => WherePredicate::Or(Vec::new()),
                        _ => WherePredicate::IsNull {
                            column: col,
                            negated: true,
                        },
                    });
                }
            }
            Ok(match op {
                ScalarSubqueryOp::Eq => WherePredicate::Equality { column: col, value },
                ScalarSubqueryOp::Ne => {
                    WherePredicate::Not(Box::new(WherePredicate::Equality { column: col, value }))
                }
                other => WherePredicate::Compare {
                    column: col,
                    op: compare_op(other).ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "unexpected scalar subquery operator".to_string(),
                    })?,
                    value,
                },
            })
        }
    }
}

/// ソート・重複除去済みの distinct 値 `values`（TEXT／ENUM／型付き対象列 `column`）を、
/// [`crate::declarative_filter::MAX_IN_LIST_ITEMS`] 件以下のチャンクごとの
/// `WherePredicate::InList` を分岐とする `Or` へ変換する（Issue #1165）。
///
/// 各チャンクは既存経路（`sql::parser::declarative_leaf_to_filter` →
/// `DeclarativeFilter::in_list` → `FilterOp::InText`／`InTyped`）で束縛されるため、
/// 第 2 の評価器は作らない。`bind` 側の `MAX_IN_LIST_ITEMS` 検査（NoSQL 表層と共有する
/// 事前防御点）はチャンク幅で満たすので緩めない。常に `Or` で包む（1 チャンクでも）
/// ことで、計画形状（`scalar_plan` は `Or` を含む文を `PlainScan` に固定する）を
/// 内側データ量へ依存させない。0 件は分岐 0 個の `Or`（常に偽）。
fn build_in_set_predicate(column: &str, values: Vec<String>) -> WherePredicate {
    let chunk_size = crate::declarative_filter::MAX_IN_LIST_ITEMS;
    let mut branches = Vec::with_capacity(values.len().div_ceil(chunk_size));
    let mut iter = values.into_iter();
    loop {
        let chunk: Vec<String> = iter.by_ref().take(chunk_size).collect();
        if chunk.is_empty() {
            break;
        }
        branches.push(vec![WherePredicate::InList {
            column: column.to_string(),
            values: chunk,
        }]);
    }
    WherePredicate::Or(branches)
}

/// `EXISTS (SELECT ...)` を解決し、可視行が 1 件以上存在するかどうかを返す。
/// 内側は [`InnerScanIntent::ExistenceOnly`] で評価する（投影不要・実質 `LIMIT` 1。
/// PR #1103 追加 codex-review P1 指摘対応の詳細は [`execute_inner_scan_statement`]
/// 参照）。`NOT EXISTS` は呼び出し側が結果を反転する。
#[allow(clippy::too_many_arguments)]
fn resolve_exists_subquery(
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<bool, SqlSurfaceError> {
    let result = execute_inner_query(
        inner_tokens,
        depth,
        InnerScanIntent::ExistenceOnly,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_value_budget,
    )?;
    Ok(!result.rows.is_empty())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::storage::{encode_row, RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    /// PR #1235 codex-review P1 の回帰テスト: `2^53` を超える整数との比較が
    /// 数値リテラルの正確表現ガードに拒否されず、境界値との等価な比較へ写像される。
    #[test]
    fn int_compare_maps_out_of_range_values_to_boundary() {
        let m = MAX_EXACT_INT_ABS;
        let parts = |p: WherePredicate| match p {
            WherePredicate::Expression(Expr::Binary { op, rhs, .. }) => match *rhs {
                Expr::Number(t) => (op, t),
                _ => panic!("rhs is not a number"),
            },
            _ => panic!("not an expression predicate"),
        };
        let big = 9_007_199_254_740_993_i128;
        assert_eq!(
            parts(int_compare("c", BinOp::Eq, big)),
            (BinOp::Gt, m.to_string())
        );
        assert_eq!(
            parts(int_compare("c", BinOp::Lt, big)),
            (BinOp::Le, m.to_string())
        );
        assert_eq!(
            parts(int_compare("c", BinOp::Gt, big)),
            (BinOp::Gt, m.to_string())
        );
        assert_eq!(
            parts(int_compare("c", BinOp::Eq, -big)),
            (BinOp::Lt, (-m).to_string())
        );
        assert_eq!(
            parts(int_compare("c", BinOp::Gt, -big)),
            (BinOp::Ge, (-m).to_string())
        );
        assert_eq!(
            parts(int_compare("c", BinOp::Lt, -big)),
            (BinOp::Lt, (-m).to_string())
        );
        // 境界ちょうどは写像せずそのまま。
        assert_eq!(
            parts(int_compare("c", BinOp::Eq, m)),
            (BinOp::Eq, m.to_string())
        );
    }

    fn docs_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), true)],
        )
    }

    /// 検証専用: `encode_row`（低レベル API）で直接行を書き込む
    /// （`sql::scan` モジュール内テストの `write_row_direct` と同型）。
    fn write_row_direct(storage: &Storage, tenant_id: &str, id: u64, embedding: &[f32]) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            let buf = encode_row(&RowInput {
                tenant_id,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            })
            .expect("encode row");
            table
                .insert((tenant_id, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// PR #1103 追加 codex-review P1 指摘の回帰テスト:
    /// [`InnerScanIntent::ExistenceOnly`] で `execute_inner_scan` を呼ぶと、
    /// 内側が `SELECT *`（全列投影）・大きい `LIMIT`（500）を指定していても、
    /// 実際に返る `QueryResult` は投影列ゼロ（`columns.is_empty()`）・行数
    /// 高々 1 件（`rows.len() <= 1`）に抑えられることを固定する（複数件の
    /// 可視行が存在する場合でも同じ）。この O(1) 化が、`EXISTS` が
    /// `execute_scan` の結果バイト上限（`sql::scan::MAX_SCAN_RESULT_BYTES`）
    /// に達しなくなる根拠そのもの——以前は投影・`LIMIT` をユーザー指定の
    /// ままユーザー指定した内側 `SELECT` を丸ごと実行していたため、幅広い
    /// 投影×大きい `LIMIT` の組合せでは可視行があっても `EXISTS` 全体が
    /// 資源上限で失敗しえた（列幅が大きいほど悪化するが、行数自体を 1 件に
    /// 抑える本修正は列幅に関わらず効く）。
    #[test]
    fn execute_inner_scan_existence_only_caps_projection_and_row_count() {
        let path = unique_db_path("subquery-exists-existence-only");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = docs_schema();
        storage.create_table(&schema).expect("create table");
        // 複数件の可視行を用意する（`LIMIT 500` を素通しした場合は全件が
        // 返りうる状態）。
        for id in 1..=5u64 {
            write_row_direct(&storage, "tenant-a", id, &[1.0, 2.0, 3.0]);
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let udfs = UdfRegistry::default();
        let mut budget = MAX_SUBQUERY_EXECUTIONS;
        let mut in_value_budget = MAX_SUBQUERY_IN_VALUES;
        let inner_tokens =
            super::super::lexer::tokenize("SELECT * FROM docs LIMIT 500").expect("tokenize");

        let result = execute_inner_query(
            &inner_tokens,
            0,
            InnerScanIntent::ExistenceOnly,
            &[],
            &read_txn,
            &ctx,
            &storage,
            &udfs,
            &mut budget,
            &mut in_value_budget,
        )
        .expect("existence-only scan should succeed");

        assert!(
            result.columns.is_empty(),
            "ExistenceOnly must project zero columns regardless of SELECT *, got {:?}",
            result.columns
        );
        assert!(
            result.rows.len() <= 1,
            "ExistenceOnly must cap the row count at 1 regardless of LIMIT/visible row count, \
             got {} rows",
            result.rows.len()
        );
        assert_eq!(
            result.rows.len(),
            1,
            "5 visible rows exist, so exactly 1 must be returned"
        );
    }

    /// 対照実験: [`InnerScanIntent::Values`]（`IN` が使う経路）はユーザー
    /// 指定の投影・`LIMIT` をそのまま使う（挙動不変であることの固定）。
    #[test]
    fn execute_inner_scan_values_keeps_user_projection_and_limit() {
        let path = unique_db_path("subquery-in-values-unchanged");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = docs_schema();
        storage.create_table(&schema).expect("create table");
        for id in 1..=5u64 {
            write_row_direct(&storage, "tenant-a", id, &[1.0, 2.0, 3.0]);
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let udfs = UdfRegistry::default();
        let mut budget = MAX_SUBQUERY_EXECUTIONS;
        let mut in_value_budget = MAX_SUBQUERY_IN_VALUES;
        let inner_tokens = super::super::lexer::tokenize("SELECT embedding FROM docs LIMIT 500")
            .expect("tokenize");

        let result = execute_inner_query(
            &inner_tokens,
            0,
            InnerScanIntent::Values,
            &[],
            &read_txn,
            &ctx,
            &storage,
            &udfs,
            &mut budget,
            &mut in_value_budget,
        )
        .expect("values scan should succeed");

        assert_eq!(
            result.columns.len(),
            1,
            "user projection (embedding) must be preserved"
        );
        assert_eq!(
            result.rows.len(),
            5,
            "all 5 visible rows must be returned (LIMIT 500 > 5)"
        );
    }

    /// Issue #1165: [`build_in_set_predicate`] のチャンク化（0 件は空の `Or`、
    /// 256 件以下は 1 分岐、超過分は次の分岐へ。全要素が欠落・重複なく保たれる。
    /// distinct 値予算〔`MAX_SUBQUERY_IN_VALUES`〕の消費・超過拒否は
    /// `tests/sql29_subquery.rs`・`tests/rls10_relational_paths.rs` が固定する）。
    #[test]
    fn build_in_set_predicate_chunks_by_max_in_list_items() {
        let max = crate::declarative_filter::MAX_IN_LIST_ITEMS;
        for (n, expected_branches) in [
            (0usize, 0usize),
            (1, 1),
            (max, 1),
            (max + 1, 2),
            (2 * max, 2),
            (2 * max + 1, 3),
        ] {
            let values: Vec<String> = (0..n).map(|i| format!("v{i:05}")).collect();
            let WherePredicate::Or(branches) = build_in_set_predicate("lang", values.clone())
            else {
                panic!("must be Or");
            };
            assert_eq!(branches.len(), expected_branches, "n={n}");
            let mut merged = Vec::new();
            for branch in branches {
                assert_eq!(branch.len(), 1);
                let WherePredicate::InList { column, values } = &branch[0] else {
                    panic!("branch must be InList");
                };
                assert_eq!(column, "lang");
                assert!(values.len() <= max && !values.is_empty());
                merged.extend(values.iter().cloned());
            }
            assert_eq!(merged, values, "n={n}");
        }
    }

    /// Issue #1191 の設計契約: スカラーサブクエリの解決結果（[`scalar_literal_predicate`]）は、
    /// 同じ列型で `col <op> <リテラル>` と書いたときにパーサーが生成する AST と同一形
    /// （第 2 の評価器を作らない）。値族ごとに独立オラクル（実パーサーの出力）と照合する。
    /// `<>` はパーサーが受理しない形のためここでは対象外（`scalar_literal_predicate` 側の
    /// 明示分岐は結合テスト `tests/sql29_subquery_scalar.rs` が固定する）。
    #[test]
    fn scalar_literal_predicate_matches_parser_output_per_family() {
        use crate::catalog::ColumnDef;
        use crate::sql::allowlist::{ScalarSubqueryOp, Statement};

        let path = unique_db_path("subquery-scalar-oracle");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let numeric_ty = ColumnType::Numeric {
            precision: 10,
            scale: 2,
        };
        storage
            .create_table(&TableSchema::new(
                "t",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("name", ColumnType::Text, true),
                    ColumnDef::new("day", ColumnType::Date, true),
                    ColumnDef::new("at", ColumnType::Timestamp, true),
                    ColumnDef::new("price", numeric_ty.clone(), true),
                    ColumnDef::new("ext", ColumnType::Uuid, true),
                    ColumnDef::new("blob", ColumnType::Bytea, true),
                    ColumnDef::new("qty", ColumnType::BigInt, true),
                    ColumnDef::new("ratio", ColumnType::Double, true),
                ],
            ))
            .expect("create table");

        let uuid_text = "00000000-0000-0000-0000-00000000002a";
        let cases: Vec<(&str, ColumnType, Cell, String)> = vec![
            (
                "name",
                ColumnType::Text,
                Cell::Text("abc".to_string()),
                "'abc'".to_string(),
            ),
            (
                "day",
                ColumnType::Date,
                Cell::Date(19_000),
                format!("'{}'", crate::datetime::format_date(19_000)),
            ),
            (
                "at",
                ColumnType::Timestamp,
                Cell::Timestamp(1_600_000_000_000_000),
                format!(
                    "'{}'",
                    crate::datetime::format_timestamp(1_600_000_000_000_000)
                ),
            ),
            (
                "price",
                numeric_ty,
                Cell::Numeric(crate::numeric::parse_literal_exact("12.50").expect("decimal")),
                "'12.50'".to_string(),
            ),
            (
                "ext",
                ColumnType::Uuid,
                Cell::Uuid(crate::uuid::parse_uuid_text(uuid_text).expect("uuid")),
                format!("'{uuid_text}'"),
            ),
            (
                "blob",
                ColumnType::Bytea,
                Cell::Bytes(vec![0x01, 0xab]),
                "'\\x01ab'".to_string(),
            ),
            (
                "qty",
                ColumnType::BigInt,
                Cell::SignedInteger(42),
                "42".to_string(),
            ),
            (
                "ratio",
                ColumnType::Double,
                Cell::Float(1.5),
                "1.5".to_string(),
            ),
        ];
        let ops = [
            (ScalarSubqueryOp::Eq, "="),
            (ScalarSubqueryOp::Lt, "<"),
            (ScalarSubqueryOp::Le, "<="),
            (ScalarSubqueryOp::Gt, ">"),
            (ScalarSubqueryOp::Ge, ">="),
        ];
        for (col, ty, cell, literal) in &cases {
            let family = family_of_type(ty).expect("family");
            for (op, text) in ops {
                let sql = format!("SELECT id FROM t WHERE {col} {text} {literal} LIMIT 1");
                let Statement::Scan(parsed) =
                    crate::sql::allowlist::validate_sql(&sql, &storage).expect("parse")
                else {
                    panic!("expected scan for {sql}");
                };
                let resolved = scalar_literal_predicate(col, op, cell, Some(ty), family)
                    .unwrap_or_else(|e| panic!("resolve failed for {sql}: {e:?}"));
                assert_eq!(vec![resolved], parsed.where_predicates.clone(), "sql={sql}");
            }
        }
    }
}
