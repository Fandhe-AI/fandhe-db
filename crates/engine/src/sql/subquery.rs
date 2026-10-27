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
//!   [LIMIT <n>]`（広域取得＝[`Statement::Scan`]）のみ。`LIMIT` の省略・`LIMIT ALL` も
//!   受理する（Issue #1853・SQL-29・SQL-34。上限の扱いは下記）。
//! - スカラー: 上記に加え、単一集計項目の集計形（[`Statement::Aggregate`]。
//!   `GROUP BY` の有無を問わず `LIMIT` 不要）。
//! - `<col> <op> ANY|SOME|ALL (SELECT ...)`（Issue #1855・SQL-61 (3)）: 内側の形は `IN` と
//!   同じ。`= ANY`／`<> ALL` は `IN`／`NOT IN` へ委譲し、それ以外は内側の値集合の境界値
//!   （最小・最大）との比較へ書き換える（空集合・NULL を含む集合・外側値 NULL の三値論理は
//!   [`resolve_quantified_compare`] 参照）。
//!
//! ランキング付き検索 SELECT・集合演算・JOIN・
//! 内側自身の投影位置スカラーサブクエリは `42601`（`docs/design/sql-subquery.md` 参照）。
//!
//! 投影位置のスカラーサブクエリ（Issue #1352。外側は広域取得 SELECT のみ）は
//! [`resolve_scalar_projection_items`] が内側を実行して列メタデータと値（0 行は NULL）へ
//! 解決し、[`merge_scalar_projection_items`] が外側の走査結果へ SELECT リスト上の位置で
//! 合流する。内側が 2 行以上なら外側の結果が 1 行以上のときだけ `21000`。
//! 内側 WHERE に入れ子のスカラーサブクエリの `21000` も同様に外側の行数判明まで遅延する（Issue #1432）。
//!
//! 相関サブクエリ（内側が外側の列を参照する形）は束縛前の静的走査で外側スコープへ束縛する
//! （PostgreSQL の名前解決順と同じく、内側スキーマに無く外側のいずれかのスコープに有る列名を
//! 相関参照とみなす。どこにも無い名前は従来どおり内側の束縛が `42703`〔未定義列。Issue #2332〕で
//! 拒否する）。外側スコープは `outer_scopes`（末尾が直近の外側）としてネストの深さ分を引き回す。
//! 相関 `EXISTS`／`NOT EXISTS`（Issue #1842・#1843・SQL-61）は、内側 `WHERE` の連言を等価キー・残余・内側だけの
//! 条件へ分解して（`sql::correlated_exists`）内側を 1 回だけ実体化し、外側の行ごとに判定する内部専用の葉
//! （[`WherePredicate::CorrelatedExists`]。ハッシュ半結合、残余があれば行ごとの再評価。
//! `sql::join::semi`）へ書き換える。`NOT EXISTS` は同じプローブの判定を反転した反結合として扱う。
//! 相関 `IN`／`NOT IN`（Issue #1844）も同じ分解・実体化を使い、肯定形は対象ペア `(y, x)` を等価キーへ
//! 足した半結合、`NOT IN` は NULL を考慮した反結合（`ProbeKind::NotIn`）へ書き換える
//! （[`resolve_in_predicate`]）。集計形の相関スカラー（Issue #1846。単一集計項目で外側参照が内側 `WHERE` の
//! 等価の相関条件だけの形）は、内側を相関キーの `GROUP BY` 集計へ書き換えて 1 回だけ実行し、外側の行ごとに
//! キーで引く（左外部結合＋集計と同値。WHERE 位置・投影位置の両方。[`resolve_correlated_scalar_aggregate`]）。
//! 非集計形のスカラー・`ANY`／`ALL`・述語形 DML・集計の `FILTER` の相関は従来どおり `42601` で拒否する
//! （fail-closed）。
//!
//! NULL 規則（三値論理。RLS 境界内＝可視行のみから決まる）:
//! - `NOT IN`: 内側 0 行なら常に真（外側値が NULL でも真）。内側の結果に NULL を
//!   1 つでも含めば真にならない。それ以外は distinct 値ごとの否定を連言で並べ、
//!   外側値が NULL の行は UNKNOWN として除外される。
//! - 相関 `NOT IN`: 外側の行ごとに、相関条件を満たす内側可視行の投影値の集合 S について、S が空なら
//!   真（外側値が NULL でも真）、外側値が NULL・S に外側値がある・S に NULL がある場合は偽、それ以外は真。
//!   相関 `IN` は WHERE 文脈で UNKNOWN が偽に潰れるため `EXISTS (... AND y = x)` と同値。
//! - `NOT EXISTS`: 可視行が 1 件でもあれば常に偽、無ければ常に真。相関 `NOT EXISTS` は外側の行ごとに
//!   `EXISTS` の単純な否定（外側キーが NULL の行は真）で、内側 `OFFSET k` は「一致が `k` 件以下」。
//! - スカラー: 0 行または NULL は UNKNOWN（否定は構文段で演算子反転済みのため
//!   常に偽）。2 行以上は「先頭行を採用」せずエラー（`21000`。Issue #1404）。
//!
//! 式の内側の相関スカラー（Issue #1848）: 算術・比較・`COALESCE`・`CASE` の内側に埋め込まれた相関
//! スカラーサブクエリは [`resolve_embedded_scalar_subqueries`] が WHERE 側の解決の後・外側の束縛の前に
//! 内側を 1 回だけ実行し、値プローブを式木の葉（`BoundExpr::CorrelatedScalar`）へ解決する。
//! 相関の無い内側は `42601`。詳細は `docs/design/sql-subquery.md` の「Issue #1848 追記」節。
//!
//! DoS 対策（security.md「不安全な設計｜無制限リソース確保」対応）:
//! - ネスト深さは構文解析段（[`super::allowlist::MAX_SUBQUERY_DEPTH`]・
//!   `Parser::require_subquery_depth`）が担う。
//! - 1 文（トップレベル実行 1 回）あたりの内側クエリ実行回数は本モジュールの
//!   [`MAX_SUBQUERY_EXECUTIONS`] で頭打ちにする（`budget` を呼び出し階層全体で
//!   共有する `&mut usize` として引き回す）。入れ子 WHERE が遅延対象エラーで打ち切られた
//!   ときの静的検証（[`static_validate_subquery_predicates`]）は内側を実行せず、スキーマ取得と
//!   束縛のみ（文の長さとネスト深さで有界）のため、この予算を消費しない（Issue #1432）。
//! - 内側の可視行数は結果行数上限（`--max-result-rows`。`ResultLimits::max_result_rows()`）を
//!   超えたら `54000`。`LIMIT` 省略形の `IN`／`NOT IN` は上限 + 1 件で取得を止めて拒否し、
//!   `EXISTS` は 1 件、スカラー用途は 2 件（`21000` 判定用）へ差し替えて有界にする。
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
    CompareOp, Projection, ScalarSubqueryItem, ScalarSubqueryOp, SelectItem, SqlSurfaceError,
    Statement, TableLookup, ValidatedAggregate, ValidatedScan, WherePredicate,
};
use super::correlated_exists::{self, CorrelatedCtx};
use super::correlation;
use super::exec::{Cell, ColumnMeta};
use super::lexer::Token;
use super::udf_call::{BinOp, Expr, UdfRegistry};
use crate::catalog::{ColumnType, TableSchema};
use crate::policy::PolicyContext;

/// 1 文（トップレベル実行 1 回）あたりに実行できる内側サブクエリの総数
/// （実装既定値。Issue #927・TASK-213。ネスト深さ上限
/// [`super::allowlist::MAX_SUBQUERY_DEPTH`] と組み合わせて評価コストを
/// 有界にする）。Issue #1552: 本定数は**ハード上限（＝既定値）**で、運用者は起動時に
/// [`crate::nesting_limits::NestingLimits`] で下げられる（引き上げは不可）。
pub(crate) const MAX_SUBQUERY_EXECUTIONS: usize = 16;

/// 1 文（トップレベル実行 1 回。ネストしたサブクエリを含む）あたりに
/// `IN`／`NOT IN (SELECT ...)` の集合照合へ取り込める distinct 値の総数（実装
/// 既定値。Issue #1165・SQL-29・TASK-213）。
///
/// 値は [`crate::declarative_filter::MAX_IN_LIST_ITEMS`] 件ずつのチャンクへ
/// 分割し、各チャンクを既存の `InList`（束縛時にソート・重複除去済みの
/// `FilterOp::InText`、評価は二分探索）へ束縛するため、1 行あたりの評価コストは
/// 「分岐数 × O(log 256)」で有界。内側 1 回の可視行数は既に
/// 結果行数上限（既定 10,000）で頭打ちのため、既定構成では単一の `IN` サブクエリは
/// この上限に到達しない（複数の `IN` の distinct 合計のみが上限に効く）。
/// `--max-result-rows` を引き上げた構成では、単一の `IN` でも distinct 値数がこの上限を
/// 超えれば `54000`（評価コスト予算。Issue #1853）。
///
/// 現行の既定値では結果行数上限の既定と同値（10,000）だが、参照ではなく独立した
/// リテラル定数とする。検索 k 上限を設定可能にしても本予算が連動しないようにする
/// ため（Issue #1517・SQL-34・SQL-46）。
pub(crate) const MAX_SUBQUERY_IN_VALUES: usize = 10_000;

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
/// （[`bind_and_reject_correlated`]）に使う。ネストしたサブクエリを解決する再帰呼び出し
/// （[`execute_inner_query`]）は、内側クエリ自身のスキーマを末尾へ積んで渡す。
///
/// `budget` は呼び出し階層全体（ネストしたサブクエリを含む）で共有する残り
/// 実行回数。呼び出し元は起動時設定値 `NestingLimits::max_subquery_executions`
/// （ハード上限 [`MAX_SUBQUERY_EXECUTIONS`] 以下）で初期化する。
/// `nesting_limits` は内側クエリの再検証（サブクエリ深さ・CTE 上限）に使う（Issue #1552）。
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
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
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
                        group_limits,
                        nesting_limits,
                        result_limits,
                        budget,
                        in_value_budget,
                        corr,
                    )?);
                }
                out.push(WherePredicate::Or(resolved_branches));
            }
            WherePredicate::InSubquery {
                column,
                inner_tokens,
                depth,
            } => {
                out.extend(resolve_in_predicate(
                    &column,
                    &inner_tokens,
                    depth,
                    false,
                    outer_scopes,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    group_limits,
                    nesting_limits,
                    result_limits,
                    budget,
                    in_value_budget,
                    corr,
                )?);
            }
            WherePredicate::Exists {
                inner_tokens,
                depth,
            } => {
                match resolve_exists_subquery(
                    &inner_tokens,
                    depth,
                    true,
                    outer_scopes,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    group_limits,
                    nesting_limits,
                    result_limits,
                    budget,
                    in_value_budget,
                    corr,
                )? {
                    ExistsOutcome::Constant(false) => {
                        // 常に偽: 分岐 0 個の `Or` は `where_tree::BoundOrGroup::matches`
                        // が必ず `false` を返す（`sql::where_tree` モジュール
                        // ドキュメント参照）。`EXISTS` が偽の行を除外しつつ、
                        // `has_where_filters()` は真のままに保つ（フィルタなし専用
                        // キャッシュへ誤って乗せない）。
                        out.push(WherePredicate::Or(Vec::new()));
                    }
                    // 真の場合は述語自体を追加しない（制約を課さない＝常に真）。
                    ExistsOutcome::Constant(true) => {}
                    // 相関 `EXISTS`（Issue #1842）: 外側の行ごとに判定する内部専用の葉。
                    ExistsOutcome::Probe(probe) => {
                        out.push(WherePredicate::CorrelatedExists(probe))
                    }
                }
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
                    group_limits,
                    nesting_limits,
                    result_limits,
                    budget,
                    in_value_budget,
                    corr,
                )?);
            }
            // `<col> <op> ANY|SOME|ALL (SELECT ...)`（Issue #1855・SQL-61 (3)）。否定は構文段が
            // 演算子と量化子の反転として葉へ押し下げ済みのため、`Not` に包まれては届かない。
            WherePredicate::QuantifiedSubqueryCompare {
                column,
                op,
                quantifier,
                inner_tokens,
                depth,
            } => {
                out.extend(resolve_quantified_compare(
                    &column,
                    op,
                    quantifier,
                    &inner_tokens,
                    depth,
                    outer_scopes,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    group_limits,
                    nesting_limits,
                    result_limits,
                    budget,
                    in_value_budget,
                    corr,
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
                    out.extend(resolve_in_predicate(
                        &column,
                        &inner_tokens,
                        depth,
                        true,
                        outer_scopes,
                        read_txn,
                        ctx,
                        lookup,
                        udfs,
                        group_limits,
                        nesting_limits,
                        result_limits,
                        budget,
                        in_value_budget,
                        corr,
                    )?);
                }
                // `NOT EXISTS (SELECT ...)`（Issue #1191）: 可視行があれば常に偽、
                // 無ければ述語を追加しない（常に真）。
                WherePredicate::Exists {
                    inner_tokens,
                    depth,
                } => {
                    // 相関の `NOT EXISTS`（Issue #1843）は反結合プローブ（negated）として返る。
                    // 3 値すべてを明示的に扱う（`Probe` を取りこぼして述語を落とすと全行通過の
                    // fail-open になるため、`matches!` 等の網羅しない判定にしない）。
                    match resolve_exists_subquery(
                        &inner_tokens,
                        depth,
                        false,
                        outer_scopes,
                        read_txn,
                        ctx,
                        lookup,
                        udfs,
                        group_limits,
                        nesting_limits,
                        result_limits,
                        budget,
                        in_value_budget,
                        corr,
                    )? {
                        ExistsOutcome::Constant(true) => out.push(WherePredicate::Or(Vec::new())),
                        ExistsOutcome::Constant(false) => {}
                        ExistsOutcome::Probe(probe) => {
                            out.push(WherePredicate::CorrelatedExists(probe))
                        }
                    }
                }
                // 構文段の不変条件（量化比較の否定は反転済み）が破れた場合の fail-closed
                // （述語を黙って通さない）。
                WherePredicate::QuantifiedSubqueryCompare { .. } => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "negated quantified subquery comparison reached resolution"
                            .to_string(),
                    });
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
    // `ORDER BY` の素の名前が投影の出力別名と一致する場合は、PostgreSQL と同じく出力列を
    // 優先するため、外側の同名列への相関候補にしない（Issue #1841）。
    let output_aliases: HashSet<&str> = match &validated.projection {
        Projection::Items(items) => items
            .iter()
            .filter_map(|item| match item {
                SelectItem::Expr {
                    alias: Some(alias), ..
                } => Some(alias.as_str()),
                _ => None,
            })
            .collect(),
        _ => HashSet::new(),
    };
    cols.extend(
        validated
            .order_by
            .iter()
            .filter(|k| !output_aliases.contains(k.column.as_str()))
            .map(|k| k.column.clone()),
    );
    // `NULLS FIRST|LAST` を明示した列キーは完全形（`order_keys`）に載る（SQL-56・Issue #1791）。
    // 振り分けで相関参照の検出が変わらないよう、列キーの名前もここで集める。
    for key in &validated.order_keys {
        if let super::allowlist::ScanOrderKey::Column { key, .. } = key {
            if output_aliases.contains(key.column.as_str()) {
                continue;
            }
            cols.insert(key.column.clone());
        }
        // 式キーの中の修飾付き外側参照（プレースホルダ）も相関の検出対象に含める
        // （漏れると相関が内側表上の `42703` として誤報される。Cursor Bugbot 指摘）。
        if let super::allowlist::ScanOrderKey::Expr { expr, .. } = key {
            collect_outer_ref_placeholders(expr, &mut cols);
        }
    }
    cols
}

/// `expr` 中の外側参照プレースホルダだけを `out` へ集める。出力別名と見分けのつかない
/// 素の識別子は外側列の候補にしない（`ORDER BY`／`HAVING` の別名参照を相関と誤認しない）。
fn collect_outer_ref_placeholders(expr: &Expr, out: &mut HashSet<String>) {
    let mut idents = HashSet::new();
    super::parser::collect_expr_idents(expr, &mut idents);
    out.extend(
        idents
            .into_iter()
            .filter(|i| correlation::parse_outer_ref_placeholder(i).is_some()),
    );
}

/// [`scan_referenced_columns`] の集計版（集計引数・グループキー・`GROUP BY` 列・WHERE）。
/// `HAVING`・集計 `ORDER BY` の対象は内側の出力名（別名）であり表の列ではないため
/// 対象外（未知名は内側の束縛が拒否する）。
fn aggregate_referenced_columns(validated: &ValidatedAggregate) -> HashSet<String> {
    let mut cols = HashSet::new();
    for item in &validated.items {
        item.collect_referenced_idents(&mut cols);
    }
    if let Some(group_by) = &validated.group_by {
        // Issue #1817・SQL-58: 式キーの予約内部名は列名ではないため除き、式木の識別子を集める
        // （サブクエリ内の式キーは構文段が拒否済みのため通常は到達しない多層防御）。
        cols.extend(
            group_by
                .columns
                .iter()
                .filter(|c| crate::sql::allowlist::group_key_expr_placeholder_index(c).is_none())
                .cloned(),
        );
        for e in &group_by.key_exprs {
            super::parser::collect_expr_idents(e, &mut cols);
        }
        group_by.collect_having_aggregate_idents(&mut cols);
        // `HAVING`・集計 `ORDER BY` は出力名（別名）を指すため素の識別子は集めないが、
        // 修飾付き外側参照のプレースホルダは相関の検出対象に含める。
        for h in &group_by.having {
            if correlation::parse_outer_ref_placeholder(&h.item_name).is_some() {
                cols.insert(h.item_name.clone());
            }
        }
        for h in &group_by.having_exprs {
            collect_outer_ref_placeholders(&h.lhs, &mut cols);
            collect_outer_ref_placeholders(&h.rhs, &mut cols);
        }
        for o in &group_by.order_by {
            if correlation::parse_outer_ref_placeholder(&o.target).is_some() {
                cols.insert(o.target.clone());
            }
            if let Some(e) = &o.expr {
                collect_outer_ref_placeholders(e, &mut cols);
            }
        }
    }
    super::parser::collect_where_predicate_idents(&validated.where_predicates, &mut cols);
    cols
}

/// 内側クエリの事前検査結果。`correlation` は外側スコープへの束縛結果（相関の IR。
/// SQL-61・Issue #1841）。この経路では相関があれば事前検査が
/// [`correlation::reject_if_correlated`] で拒否するため、返る時点では相関なしである
/// （相関 `EXISTS`／`NOT EXISTS` は [`try_resolve_correlated_exists`] が先に処理する。Issue #1842・#1843）。
struct InnerPrecheck {
    schema: TableSchema,
    #[allow(dead_code)] // スカラー等の相関への書き換えが読む。
    correlation: correlation::Correlation,
}

/// 内側クエリの参照列を内側スキーマ・外側スコープ連鎖に束縛し（`sql::correlation`。
/// 内側優先で近い外側へ。曖昧は `42702`、外側表で修飾した存在しない列は `42703`）、
/// 相関があれば `42601` で拒否する（Issue #1191・#1841。相関サブクエリの実行は対象外）。
/// エラー文言は静的文字列と利用者自身が書いた列名のみ（他テナントの情報を含めない）。
fn bind_and_reject_correlated(
    referenced: &HashSet<String>,
    inner_schema: &TableSchema,
    outer_scopes: &[&TableSchema],
) -> Result<correlation::Correlation, SqlSurfaceError> {
    let bound = correlation::bind_correlation(referenced, inner_schema, outer_scopes)?;
    correlation::reject_if_correlated(&bound)?;
    Ok(bound)
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
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
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
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
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
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
    meta_sink: &mut Option<Vec<ColumnMeta>>,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    *budget = budget.checked_sub(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large(format!(
            "subquery execution count exceeds limit {}",
            nesting_limits.max_subquery_executions()
        ))
    })?;

    let stmt = super::allowlist::validate_sql_tokens_with_subquery_ctx_limits(
        inner_tokens,
        lookup,
        depth,
        nesting_limits,
    )?;
    let result = match (stmt, intent) {
        (Statement::Scan(validated), _) => execute_inner_scan_statement(
            validated,
            intent,
            outer_scopes,
            read_txn,
            ctx,
            lookup,
            udfs,
            group_limits,
            nesting_limits,
            result_limits,
            budget,
            in_value_budget,
            corr,
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
            group_limits,
            nesting_limits,
            result_limits,
            budget,
            in_value_budget,
            corr,
            meta_sink,
        )?,
        _ => return Err(unsupported_inner_statement()),
    };

    // 内側の可視結果行数を結果行数上限（`--max-result-rows`）で頭打ちにする多層防御
    // （security.md「不安全な設計」対応）。`LIMIT` 省略形は scan 側の
    // `row_limit_exceeded()` が先に `54000` を返すため、本ガードは明示 `LIMIT` と
    // 集計形のための保険として残す。
    // スカラー用途は基数判定用に最大 2 件取得するため、cap が 1 でも 2 件目までは
    // 許容する（2 件以上は呼び出し側が `21000` を返す）。
    let max_rows = if matches!(
        intent,
        InnerScanIntent::Scalar | InnerScanIntent::ScalarValue
    ) {
        result_limits.max_result_rows().max(2)
    } else {
        result_limits.max_result_rows()
    };
    if result.rows.len() > max_rows {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "subquery result row count exceeds limit {max_rows}"
        )));
    }

    Ok(result)
}

/// 内側が広域取得・スカラー用途の集計形のいずれでもない場合の `42601`。
/// [`execute_inner_query_with_meta`] と [`static_validate_inner_query`] で共有する。
fn unsupported_inner_statement() -> SqlSurfaceError {
    SqlSurfaceError::unsupported(
        "subquery must be a plain SELECT ... FROM ... [WHERE ...] [LIMIT n] \
         (no HYBRID / USING PLAN / set operation / JOIN; GROUP BY only as a scalar \
         aggregate)",
    )
}

/// 内側 Scan の、WHERE 解決・束縛より前に行う静的検査（ウィンドウ関数・未知テーブル・
/// 相関参照・内側の投影位置サブクエリ・投影列数）を行い、内側スキーマを返す。
/// 実行経路（[`execute_inner_scan_statement`]）と失敗時の静的検証
/// （[`static_validate_inner_query`]）が同じ検査を共有する（2 系統の検査を分岐させない）。
fn inner_scan_prechecks(
    validated: &ValidatedScan,
    intent: InnerScanIntent,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
) -> Result<InnerPrecheck, SqlSurfaceError> {
    // Issue #1853・SQL-29・SQL-34: 内側の `LIMIT` 省略・`LIMIT ALL` は受理する。
    // 取得上限は用途ごとに呼び出し側（実行経路・静的検証経路）が束縛前に確定する
    // （`Values` は結果行数上限 + 1、`ExistenceOnly` は 1、スカラー用途は 2）。
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
    let correlation = bind_and_reject_correlated(
        &scan_referenced_columns(validated),
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
            Projection::All => {
                usize::from(inner_schema.has_pseudo_id()) + inner_schema.columns.len()
            }
            Projection::Columns(names) => names.len(),
            Projection::Items(items) => items.len(),
        };
        if projected_len != 1 {
            return Err(SqlSurfaceError::unsupported(
                "subquery used as a value must select exactly one column",
            ));
        }
    }
    Ok(InnerPrecheck {
        schema: inner_schema,
        correlation,
    })
}

/// 内側の集計形（スカラー用途）の、WHERE 解決・束縛より前に行う静的検査（集計項目数・
/// 未知テーブル・相関参照）を行い、内側スキーマを返す。[`inner_scan_prechecks`] の集計版。
fn inner_aggregate_prechecks(
    validated: &ValidatedAggregate,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
) -> Result<InnerPrecheck, SqlSurfaceError> {
    if validated.items.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used as a value must select exactly one column",
        ));
    }
    let inner_schema = inner_table_schema(read_txn, &validated.table_name)?;
    let correlation = bind_and_reject_correlated(
        &aggregate_referenced_columns(validated),
        &inner_schema,
        outer_scopes,
    )?;
    Ok(InnerPrecheck {
        schema: inner_schema,
        correlation,
    })
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
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
    meta_sink: &mut Option<Vec<ColumnMeta>>,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    let inner_schema = inner_scan_prechecks(&validated, intent, outer_scopes, read_txn)?.schema;

    // 自身の WHERE に含まれるさらに深いサブクエリを、束縛（`bind_scan`）の前に
    // 解決する（深さ優先。`depth` は構文解析段で `MAX_SUBQUERY_DEPTH` 検査
    // 済みのため、ここでは budget のみ検査すれば足りる）。
    // 実行時エラー時に、解決前の述語で静的検証を完了させるための控え。
    let original_preds: Vec<WherePredicate> = validated.where_predicates.clone();
    let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
    chain.push(&inner_schema);
    validated.where_predicates = match resolve_where_predicates(
        validated.where_predicates,
        &chain,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    ) {
        Ok(p) => p,
        Err(e) => {
            // 入れ子 WHERE サブクエリの実行時データ例外は、投影位置の外側が行数判明まで
            // 遅延できるよう、投影メタデータ（WHERE に依存しない）を先に確定する。
            // 内側 WHERE の静的な誤り（未知列等）はデータ依存の実行時エラーより優先して
            // 返す（可視データで契約が変わらないよう、サブクエリを含まない述語は
            // 束縛検証を完了させる）。
            if intent != InnerScanIntent::ExistenceOnly {
                if is_deferrable_runtime_error(&e) {
                    // 実行せず予算も消費しない静的検証（fail-closed。Issue #1432）。
                    static_validate_subquery_predicates(
                        &original_preds,
                        &chain,
                        read_txn,
                        lookup,
                        udfs,
                        nesting_limits,
                        result_limits,
                    )?;
                }
                validated.where_predicates = static_validation_predicates(&original_preds);
                let bound = super::parser::bind_scan_with_dummy_flags(
                    &validated,
                    &inner_schema,
                    udfs,
                    &[],
                    &result_limits,
                )?;
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
        // `LIMIT` 省略形は検証対象の明示値が無く、下の差し替えで有界値（1）になる。
        if !validated.limit_implicit {
            super::parser::validate_search_limit_with(validated.limit, &result_limits)?;
        }
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
        // 有界値（1）への差し替えと同時に暗黙上限扱いを解く（fail-open 防止）。
        validated.limit = 1;
        validated.limit_implicit = false;
    }

    let bind_limits = apply_scalar_inner_limit(&mut validated, intent, &result_limits)?;

    let bound = super::parser::bind_scan_with_dummy_flags(
        &validated,
        &inner_schema,
        udfs,
        &[],
        &bind_limits,
    )?;
    *meta_sink = super::describe::scan_columns(&bound, &inner_schema).ok();
    super::scan::execute_scan(read_txn, ctx, &inner_schema, &bound)
}

/// スカラー用途（`Scalar`／`ScalarValue`）の内側 Scan の取得上限を確定する（Issue #1853・
/// SQL-29・SQL-34）。`LIMIT` 省略・`LIMIT ALL` は 2 件（基数違反 `21000` の判定に足りる
/// 最小）へ差し替え、暗黙上限扱いを同時に解く。明示 `LIMIT` は結果行数上限で範囲検証し、
/// `ScalarValue` のみ 2 へ頭打ちにする（`Scalar` は従来どおり値を保つ）。
/// 検証は差し替えより先に行い、差し替えで迂回されないようにする（fail-closed）。
/// 実行経路と静的検証経路の両方から呼び、2 系統の挙動を分岐させない。
/// 戻り値は bind に渡す上限。省略形の 2 件は基数判定用の内部プローブであり利用者の
/// 明示 `LIMIT` ではないため、cap が 1 でも bind の範囲検証で `22000` にならないよう
/// `max(cap, 2)` の上限を返す（`Scalar`／`ScalarValue` 以外と明示 `LIMIT` は cap のまま）。
fn apply_scalar_inner_limit(
    validated: &mut ValidatedScan,
    intent: InnerScanIntent,
    result_limits: &crate::result_limits::ResultLimits,
) -> Result<crate::result_limits::ResultLimits, SqlSurfaceError> {
    if !matches!(
        intent,
        InnerScanIntent::Scalar | InnerScanIntent::ScalarValue
    ) {
        return Ok(*result_limits);
    }
    if validated.limit_implicit {
        validated.limit = 2;
        validated.limit_implicit = false;
        return Ok(crate::result_limits::ResultLimits::new(
            result_limits.max_result_rows().max(2),
        ));
    }
    super::parser::validate_search_limit_with(validated.limit, result_limits)?;
    if intent == InnerScanIntent::ScalarValue {
        validated.limit = validated.limit.min(2);
    }
    Ok(*result_limits)
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
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
    meta_sink: &mut Option<Vec<ColumnMeta>>,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    let inner_schema = inner_aggregate_prechecks(&validated, outer_scopes, read_txn)?.schema;
    // 実行時エラー時に、解決前の述語で静的検証を完了させるための控え。
    let original_preds: Vec<WherePredicate> = validated.where_predicates.clone();
    let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
    chain.push(&inner_schema);
    validated.where_predicates = match resolve_where_predicates(
        validated.where_predicates,
        &chain,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    ) {
        Ok(p) => p,
        Err(e) => {
            // スキャン側と同じ理由で、入れ子 WHERE の失敗時も投影メタデータを確定する。
            if is_deferrable_runtime_error(&e) {
                static_validate_subquery_predicates(
                    &original_preds,
                    &chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?;
            }
            validated.where_predicates = static_validation_predicates(&original_preds);
            let bound = super::parser::bind_aggregate(&validated, &inner_schema, udfs)?;
            *meta_sink = Some(super::describe::aggregate_columns(&bound, &inner_schema)?);
            return Err(e);
        }
    };
    let bound = super::parser::bind_aggregate(&validated, &inner_schema, udfs)?;
    *meta_sink = Some(super::describe::aggregate_columns(&bound, &inner_schema)?);
    super::aggregate::execute_aggregate_with_group_limits(
        read_txn,
        ctx,
        &inner_schema,
        &bound,
        super::aggregate::MAX_AGGREGATE_RESULT_BYTES,
        None,
        None,
        None,
        group_limits,
    )
}

/// 外側の行数が判明するまで遅延してよい実行時エラーか（入れ子スカラーサブクエリの
/// `21000`、または静的な `22000` 以外の式評価データ例外 `22xxx`）。
/// [`resolve_scalar_projection_items`] と内側クエリの失敗時静的検証から呼ばれる。
fn is_deferrable_runtime_error(e: &SqlSurfaceError) -> bool {
    matches!(e, SqlSurfaceError::CardinalityViolation)
        || (e.wire_code().starts_with("22") && e.wire_code() != "22000")
}

/// 先行サブクエリが遅延対象エラー（入れ子の `21000` 等）で解決を打ち切った後に、
/// サブクエリを含む述語（`OR` の分岐・`NOT` の内側を含む）を**実行せずに**静的検証し、
/// 静的・構造的なエラー（未知テーブル・未知列・投影列数・相関参照・値族の不一致等）を
/// 最初の 1 件返す（Issue #1432。`execute_inner_scan_statement`／
/// `execute_inner_aggregate_statement` の失敗時経路から呼ばれる）。
///
/// 内側クエリを実行しないため、実行回数予算（[`MAX_SUBQUERY_EXECUTIONS`]）・`IN` 値予算は
/// 消費せず、予算の残量によって検査が打ち切られることもない（予算枯渇を「静的エラー
/// なし」とみなして後続の静的エラーを見落とす経路を作らない＝fail-closed）。
/// コストはカタログからのスキーマ取得と束縛のみで、文の長さと
/// [`super::allowlist::MAX_SUBQUERY_DEPTH`] で有界（可視行の読み取りは行わない）。
/// 行数・値に依存する検査（`21000`・結果行数上限超過・`IN` 値予算）は対象外。
fn static_validate_subquery_predicates(
    preds: &[WherePredicate],
    chain: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
) -> Result<(), SqlSurfaceError> {
    for pred in preds {
        match pred {
            WherePredicate::Or(branches) => {
                for branch in branches {
                    static_validate_subquery_predicates(
                        branch,
                        chain,
                        read_txn,
                        lookup,
                        udfs,
                        nesting_limits,
                        result_limits,
                    )?;
                }
            }
            // 相関の `NOT EXISTS` は反結合の範囲（#1843）で従来どおり拒否するため、否定形の `EXISTS` は
            // 相関を実行する肯定形と区別して検証する（Issue #1842）。
            WherePredicate::Not(inner) => match inner.as_ref() {
                WherePredicate::Exists {
                    inner_tokens,
                    depth,
                } => static_validate_exists(
                    inner_tokens,
                    *depth,
                    chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?,
                other => static_validate_subquery_predicates(
                    std::slice::from_ref(other),
                    chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?,
            },
            WherePredicate::InSubquery {
                column,
                inner_tokens,
                depth,
            } => {
                // 相関 `IN`（Issue #1844）は実行経路と同じ準備（静的検査・分解・残余の束縛）と
                // 内側だけの連言の静的検証・束縛まで行う（実行はしない）。
                if static_validate_correlated_in(
                    column,
                    inner_tokens,
                    *depth,
                    chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )? {
                    continue;
                }
                // [`resolve_in_subquery`] と同じ順序（内側 → 投影列数 → 対象列 → 値族）で検証する。
                let meta = static_validate_inner_query(
                    inner_tokens,
                    *depth,
                    InnerScanIntent::Values,
                    chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?;
                let [inner_meta] = meta.as_slice() else {
                    return Err(SqlSurfaceError::unsupported(
                        "subquery used with IN must select exactly one column",
                    ));
                };
                let (_, family) = validate_in_target_column(column, owner_schema(chain)?)?;
                if !in_family_compatible(inner_meta, family) {
                    return Err(in_family_mismatch(column));
                }
            }
            WherePredicate::Exists {
                inner_tokens,
                depth,
            } => {
                static_validate_exists(
                    inner_tokens,
                    *depth,
                    chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?;
            }
            WherePredicate::ScalarSubqueryCompare {
                column,
                inner_tokens,
                depth,
                ..
            } => {
                // 集計形の相関スカラー（Issue #1846）は実行経路と同じ準備・静的検証・対象列と値族の
                // 検査まで行う（実行はしない）。
                if static_validate_correlated_scalar(
                    column,
                    inner_tokens,
                    *depth,
                    chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )? {
                    continue;
                }
                // [`resolve_scalar_compare`] と同じ順序（内側 → 投影列数 → 対象列 → 値族）。
                let meta = static_validate_inner_query(
                    inner_tokens,
                    *depth,
                    InnerScanIntent::Scalar,
                    chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?;
                let [inner_meta] = meta.as_slice() else {
                    return Err(SqlSurfaceError::unsupported(
                        "subquery used as a value must select exactly one column",
                    ));
                };
                let (_, target_family) = scalar_target_column(column, chain)?;
                check_scalar_family_compatible(column, inner_meta, target_family)?;
            }
            WherePredicate::QuantifiedSubqueryCompare {
                column,
                op,
                quantifier,
                inner_tokens,
                depth,
            } => {
                // [`resolve_quantified_compare`] と同じ順序（内側 → 投影列数 → 対象列 → 値族）。
                let meta = static_validate_inner_query(
                    inner_tokens,
                    *depth,
                    InnerScanIntent::Values,
                    chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?;
                let [inner_meta] = meta.as_slice() else {
                    return Err(SqlSurfaceError::unsupported(
                        "subquery used with ANY/ALL must select exactly one column",
                    ));
                };
                if quantified_delegates_to_in(*op, *quantifier).is_some() {
                    let (_, family) = validate_in_target_column(column, owner_schema(chain)?)?;
                    if !in_family_compatible(inner_meta, family) {
                        return Err(in_family_mismatch(column));
                    }
                } else {
                    let (_, target_family) = quantified_target_column(column, *op, chain)?;
                    check_scalar_family_compatible(column, inner_meta, target_family)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// [`static_validate_subquery_predicates`] 用に、内側トークン列を実行せずに
/// [`execute_inner_query_with_meta`] と同じ静的検証（構文・文の形・相関参照・投影列数・
/// 入れ子 WHERE の再帰的な静的検証・束縛）にかけ、投影列メタデータを返す。
/// 実行回数予算は消費しない（呼び出し元の契約参照）。
#[allow(clippy::too_many_arguments)]
fn static_validate_inner_query(
    inner_tokens: &[Token],
    depth: usize,
    intent: InnerScanIntent,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    let stmt = super::allowlist::validate_sql_tokens_with_subquery_ctx_limits(
        inner_tokens,
        lookup,
        depth,
        nesting_limits,
    )?;
    match (stmt, intent) {
        (Statement::Scan(mut validated), _) => {
            let inner_schema =
                inner_scan_prechecks(&validated, intent, outer_scopes, read_txn)?.schema;
            let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
            chain.push(&inner_schema);
            static_validate_subquery_predicates(
                &validated.where_predicates,
                &chain,
                read_txn,
                lookup,
                udfs,
                nesting_limits,
                result_limits,
            )?;
            validated.where_predicates = static_validation_predicates(&validated.where_predicates);
            let mut scalar_bind_limits = result_limits;
            match intent {
                InnerScanIntent::ExistenceOnly => {
                    // 実行経路（`execute_inner_scan_statement`）と同じく、差し替え前の
                    // `LIMIT`・投影を検証してから可視性判定用の形へ差し替える。
                    if !validated.limit_implicit {
                        super::parser::validate_search_limit_with(validated.limit, &result_limits)?;
                    }
                    let mut probe_node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
                    super::parser::bind_projection(
                        &validated.projection,
                        &inner_schema,
                        udfs,
                        &mut probe_node_budget,
                    )?;
                    validated.projection = Projection::Columns(Vec::new());
                    validated.limit = 1;
                    validated.limit_implicit = false;
                }
                InnerScanIntent::ScalarValue | InnerScanIntent::Scalar => {
                    scalar_bind_limits =
                        apply_scalar_inner_limit(&mut validated, intent, &result_limits)?;
                }
                InnerScanIntent::Values => {}
            }
            let bound = super::parser::bind_scan_with_dummy_flags(
                &validated,
                &inner_schema,
                udfs,
                &[],
                &scalar_bind_limits,
            )?;
            super::describe::scan_columns(&bound, &inner_schema)
        }
        (
            Statement::Aggregate(mut validated),
            InnerScanIntent::Scalar | InnerScanIntent::ScalarValue,
        ) => {
            let inner_schema =
                inner_aggregate_prechecks(&validated, outer_scopes, read_txn)?.schema;
            let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
            chain.push(&inner_schema);
            static_validate_subquery_predicates(
                &validated.where_predicates,
                &chain,
                read_txn,
                lookup,
                udfs,
                nesting_limits,
                result_limits,
            )?;
            validated.where_predicates = static_validation_predicates(&validated.where_predicates);
            let bound = super::parser::bind_aggregate(&validated, &inner_schema, udfs)?;
            super::describe::aggregate_columns(&bound, &inner_schema)
        }
        _ => Err(unsupported_inner_statement()),
    }
}

/// 内側クエリ失敗時の静的束縛検証に使う述語を作る。サブクエリを含まない述語に加え、
/// サブクエリを含む `OR` の分岐のうちサブクエリを含まない葉も個別に取り出す
/// （`OR` 全体を落とすと、その葉の未知列等の束縛時エラーが遅延対象エラーに
/// 隠れるため）。束縛検証専用で実行はしないため、AND 連結の意味差は影響しない。
fn static_validation_predicates(preds: &[WherePredicate]) -> Vec<WherePredicate> {
    fn collect_leaves(pred: &WherePredicate, out: &mut Vec<WherePredicate>) {
        if !predicate_contains_subquery(pred) {
            out.push(pred.clone());
        } else if let WherePredicate::Or(branches) = pred {
            for p in branches.iter().flatten() {
                collect_leaves(p, out);
            }
        }
    }
    let mut out = Vec::new();
    for p in preds {
        collect_leaves(p, &mut out);
    }
    out
}

/// 集計項目（`HAVING` 内の集計呼び出しを含む）の `FILTER (WHERE ...)` 中の部分クエリを解決する（SQL-57・Issue #1806）。
/// `core.rs` の `Statement::Aggregate` 実行アームが、文の `WHERE` の解決（
/// [`resolve_where_predicates`]）の直後・束縛の前に呼ぶ。内側は外側と同じ `PolicyContext`・
/// 同じ読み取りスナップショット・同じ予算で実行されるため、テナント境界は文の `WHERE` の
/// 部分クエリと同一（他テナント行は内側にも見えない）。部分クエリを含む述語を持つ集計だけを
/// 書き換え、それ以外は触らない。本関数を経ない経路（カーソル・ビュー・EXPLAIN 等）の
/// 未解決の部分クエリは束縛段が `42601` で拒否する（fail-closed）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_aggregate_item_filters(
    items: &mut [crate::sql::allowlist::AggregateSelectItem],
    having_aggregates: &mut [crate::sql::allowlist::AggregateItem],
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<(), SqlSurfaceError> {
    use crate::sql::allowlist::AggregateSelectItem;
    // 集計の `FILTER (WHERE ...)` では相関 `EXISTS` を実行しない（従来どおり `42601`。Issue #1842）。
    let corr = CorrelatedCtx::disabled();
    let mut resolve =
        |agg: &mut crate::sql::allowlist::AggregateItem| -> Result<(), SqlSurfaceError> {
            if !agg.filter.iter().any(predicate_contains_subquery) {
                return Ok(());
            }
            agg.filter = resolve_where_predicates(
                std::mem::take(&mut agg.filter),
                outer_scopes,
                read_txn,
                ctx,
                lookup,
                udfs,
                group_limits,
                nesting_limits,
                result_limits,
                budget,
                in_value_budget,
                &corr,
            )?;
            Ok(())
        };
    for item in items {
        match item {
            AggregateSelectItem::GroupKey { .. } => {}
            AggregateSelectItem::Aggregate(agg) => resolve(agg)?,
            AggregateSelectItem::Expr { aggregates, .. } => {
                for agg in aggregates {
                    resolve(agg)?;
                }
            }
        }
    }
    // `HAVING` 内の集計呼び出しの `FILTER` も SELECT 項目と同じ文脈・予算で解決する。
    for agg in having_aggregates {
        resolve(agg)?;
    }
    Ok(())
}

/// 述語がサブクエリ（`IN`／`EXISTS`／スカラー比較）を含むかを再帰的に判定する。
/// 入れ子 WHERE の実行時エラー時に、サブクエリを含まない述語だけで静的検証を
/// 完了させるために使う（`execute_inner_query` 系から呼ばれる）。
pub(crate) fn predicate_contains_subquery(pred: &WherePredicate) -> bool {
    match pred {
        WherePredicate::InSubquery { .. }
        | WherePredicate::Exists { .. }
        | WherePredicate::ScalarSubqueryCompare { .. }
        | WherePredicate::QuantifiedSubqueryCompare { .. } => true,
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
    /// `TIME`（Issue #1570）。
    Time,
    /// `TIMESTAMPTZ`（Issue #1576）。外側へ渡す値は UTC の明示 offset 付きテキスト。
    TimestampTz,
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
        ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Enum(_) => {
            Some(SubqueryValueFamily::Text)
        }
        ColumnType::Boolean => Some(SubqueryValueFamily::Boolean),
        ColumnType::Date => Some(SubqueryValueFamily::Date),
        ColumnType::Timestamp => Some(SubqueryValueFamily::Timestamp),
        ColumnType::Numeric { .. } => Some(SubqueryValueFamily::Numeric),
        ColumnType::Uuid => Some(SubqueryValueFamily::Uuid),
        ColumnType::Bytea => Some(SubqueryValueFamily::Bytea),
        ColumnType::SmallInt | ColumnType::Integer | ColumnType::BigInt => {
            Some(SubqueryValueFamily::Integer)
        }
        ColumnType::Real | ColumnType::Double => Some(SubqueryValueFamily::Float),
        ColumnType::Time => Some(SubqueryValueFamily::Time),
        ColumnType::TimestampTz => Some(SubqueryValueFamily::TimestampTz),
        ColumnType::Vector(_)
        | ColumnType::Array(_)
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Bpchar(_)
        // `INTERVAL` の値族は未対応（fail-closed。正規化等価の代表値の規則が未確定で担当 Issue 未定。
        // Issue #1579・#1580）。
        | ColumnType::Interval => None,
    }
}

/// 内側の単一投影列の静的メタデータ（[`ColumnMeta`]）から値族を判定する。
/// `None` は「対応外（値族が不明）」を表し、呼び出し元は `22000` で拒否する。
/// 疑似列 `id`（[`ColumnMeta::Id`]）は整数族、式・集計列（[`ColumnMeta::Computed`]）は
/// 束縛時に確定した静的型（Issue #1173）があればそれ、無ければ `None`。
fn inner_value_family(meta: &ColumnMeta) -> Option<SubqueryValueFamily> {
    match meta {
        ColumnMeta::Id | ColumnMeta::AliasedId { .. } => Some(SubqueryValueFamily::Integer),
        ColumnMeta::Scalar { ty, .. } => family_of_type(ty),
        ColumnMeta::Computed { ty: Some(ty), .. } => family_of_type(ty),
        ColumnMeta::Computed { ty: None, .. } => None,
    }
}

/// `<column> [NOT] IN (SELECT ...)` の対象列 `column` を `outer_schema`（この
/// サブクエリを含む文自身のテーブルのスキーマ）に対して検証し、列型（疑似列 `id` は
/// 型なし＝`None`）と値族を返す。存在しない列は `42703`（未定義列。通常の
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
        if column == "id" && outer_schema.has_pseudo_id() {
            return Ok((None, SubqueryValueFamily::Integer));
        }
        return Err(SqlSurfaceError::undefined_column(column));
    };
    match family_of_type(&column_def.ty) {
        Some(family) => Ok((Some(&column_def.ty), family)),
        None => Err(SqlSurfaceError::invalid_input(format!(
            "column {column:?} type is not supported as a subquery IN target"
        ))),
    }
}

/// `IN`／`NOT IN` の内側投影列が対象列の値族と比較可能か。値族の完全一致に加え、
/// 任意 scale の `NUMERIC`（`SUM(BIGINT)`〔Issue #2271〕・整数 `AVG`〔Issue #2270〕等）は
/// 整数・浮動小数列の対象と比較できる（PostgreSQL と同じ。従来受理していた比較を `SUM` の戻り値型変更で
/// 拒否しない）。実行時のセル取り出しは [`resolve_in_subquery`] が担う。
fn in_family_compatible(inner_meta: &ColumnMeta, family: SubqueryValueFamily) -> bool {
    inner_value_family(inner_meta) == Some(family)
        || (matches!(
            family,
            SubqueryValueFamily::Integer | SubqueryValueFamily::Float
        ) && is_numeric_meta(inner_meta))
}

/// `IN` の内側投影列と対象列の値族不一致の静的エラー（`resolve_in_subquery` 用）。
fn in_family_mismatch(column: &str) -> SqlSurfaceError {
    SqlSurfaceError::invalid_input(format!(
        "subquery projection type is not compatible with IN target column {column:?} \
         (the subquery projection must have the same value family as the target column)"
    ))
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
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    let mut meta_sink: Option<Vec<ColumnMeta>> = None;
    let result = match execute_inner_query_with_meta(
        inner_tokens,
        depth,
        InnerScanIntent::Values,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
        &mut meta_sink,
    ) {
        Ok(r) => r,
        Err(e) => {
            // 内側の入れ子 `21000`／実行時データ例外は外側の行数判明まで遅延される契約の
            // ため、同じ述語の対象列・値族エラーを隠さないよう、遅延対象エラーのときだけ
            // 静的検証を先に行う（Issue #1432。fail-closed）。
            if is_deferrable_runtime_error(&e) {
                let (_, family) = validate_in_target_column(column, owner_schema(outer_scopes)?)?;
                if let Some([inner_meta]) = meta_sink.as_deref() {
                    if !in_family_compatible(inner_meta, family) {
                        return Err(in_family_mismatch(column));
                    }
                }
            }
            return Err(e);
        }
    };
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
    if !in_family_compatible(inner_meta, family) {
        return Err(in_family_mismatch(column));
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
            | SubqueryValueFamily::Time
            | SubqueryValueFamily::TimestampTz
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
                // scale 0 の numeric（`SUM(BIGINT)`。Issue #2271）は `i128` の厳密値として
                // 保持し、`i64` 超でも誤った一致・不一致を出さない。
                Cell::Numeric(d) if d.scale() == 0 => ints.push(d.unscaled()),
                // 小数部を持つ numeric（整数 `AVG`。Issue #2270）: 小数部が 0 なら整数として
                // 厳密に照合し、非 0 なら整数列のどの値とも等しくないため集合へ入れない。
                Cell::Numeric(d) => {
                    let div = 10i128.checked_pow(u32::from(d.scale()));
                    if let Some(div) = div {
                        if d.unscaled() % div == 0 {
                            ints.push(d.unscaled() / div);
                        }
                    }
                }
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
                // scale 0 の numeric（Issue #2271）は float8 へ写して比較する
                // （PostgreSQL の float8 × numeric 比較と同じく numeric 側を float8 へ変換）。
                // 任意 scale（整数 `AVG`。Issue #2270）は 10 進文字列の最近接 f64 へ写す。
                Cell::Numeric(d) => {
                    let f: f64 = d.to_string().parse().map_err(|_| unexpected_cell_type())?;
                    floats.push(if f == 0.0 { 0.0 } else { f });
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
    /// 集計形の相関スカラー（Issue #1846）の値表。`Some` のとき `cell` は使わず、外側の行ごとに
    /// 隠しキー列から相関キーを作って引く（[`merge_scalar_projection_items`]）。
    correlated: Option<CorrelatedScalarValues>,
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
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<Vec<ResolvedScalarItem>, SqlSurfaceError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        // 集計形の相関スカラー（Issue #1846）。内側を 1 回だけ `GROUP BY` 集計で実行し、外側の行ごとの
        // 引き当ては [`merge_scalar_projection_items`] が隠しキー列で行う。`corr` が無効な文脈は
        // 従来どおり内側の事前検査が `42601` で拒否する。
        if corr.is_enabled() {
            if let Some((validated, inner_schema, correlation)) = classify_correlated_aggregate(
                &item.inner_tokens,
                item.depth,
                outer_scopes,
                read_txn,
                lookup,
                nesting_limits,
            )? {
                let mut agg_meta: Option<ColumnMeta> = None;
                match resolve_correlated_scalar_aggregate(
                    validated,
                    inner_schema,
                    correlation,
                    outer_scopes,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    group_limits,
                    nesting_limits,
                    result_limits,
                    budget,
                    in_value_budget,
                    corr,
                    &mut agg_meta,
                ) {
                    Ok(table) => out.push(ResolvedScalarItem {
                        position: item.position,
                        meta: alias_scalar_meta(&table.meta, item.alias.as_ref()),
                        cell: Cell::Null,
                        multi_row: false,
                        deferred_error: None,
                        correlated: Some(CorrelatedScalarValues::Agg {
                            values: std::sync::Arc::new(table.values),
                            empty: table.empty,
                            keys: table.keys,
                            key_names: table.key_names,
                        }),
                    }),
                    // 束縛後の実行時データ例外は外側の行数判明まで遅延する（非相関と同じ契約）。
                    Err(e) if is_deferrable_runtime_error(&e) => {
                        let Some(inner_meta) = agg_meta else {
                            return Err(e);
                        };
                        out.push(ResolvedScalarItem {
                            position: item.position,
                            meta: alias_scalar_meta(&inner_meta, item.alias.as_ref()),
                            cell: Cell::Null,
                            multi_row: false,
                            deferred_error: Some(e),
                            correlated: None,
                        });
                    }
                    Err(e) => return Err(e),
                }
                continue;
            }
        }
        // 非集計形の相関スカラー（Issue #1847）。内側を 1 回だけ実体化し、外側の行ごとの引き当て・
        // 基数判定（`21000`）は [`merge_scalar_projection_items`] が隠し列で行う。
        if corr.is_enabled() {
            if let Some((validated, inner_schema, correlation)) = classify_correlated_scan(
                &item.inner_tokens,
                item.depth,
                outer_scopes,
                read_txn,
                lookup,
                nesting_limits,
            )? {
                let mut rows_meta: Option<ColumnMeta> = None;
                match resolve_correlated_scalar_rows(
                    validated,
                    inner_schema,
                    correlation,
                    &ScalarPosition::Projection,
                    outer_scopes,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    group_limits,
                    nesting_limits,
                    result_limits,
                    budget,
                    in_value_budget,
                    corr,
                    &mut rows_meta,
                ) {
                    Ok(resolved) => out.push(ResolvedScalarItem {
                        position: item.position,
                        meta: alias_scalar_meta(&resolved.meta, item.alias.as_ref()),
                        cell: Cell::Null,
                        multi_row: false,
                        deferred_error: None,
                        correlated: Some(CorrelatedScalarValues::Rows {
                            probe: resolved.probe_ref(),
                            outer: std::sync::Arc::new(resolved.outer),
                            width: resolved.width,
                        }),
                    }),
                    // 束縛後の実行時データ例外は外側の行数判明まで遅延する（非相関と同じ契約）。
                    Err(e) if is_deferrable_runtime_error(&e) => {
                        let Some(inner_meta) = rows_meta else {
                            return Err(e);
                        };
                        out.push(ResolvedScalarItem {
                            position: item.position,
                            meta: alias_scalar_meta(&inner_meta, item.alias.as_ref()),
                            cell: Cell::Null,
                            multi_row: false,
                            deferred_error: Some(e),
                            correlated: None,
                        });
                    }
                    Err(e) => return Err(e),
                }
                continue;
            }
        }
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
            group_limits,
            nesting_limits,
            result_limits,
            budget,
            in_value_budget,
            corr,
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
                    correlated: None,
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
            correlated: None,
        });
    }
    Ok(out)
}

/// 式の内側に埋め込まれた相関スカラーサブクエリ（Issue #1848・SQL-61 ポインタ。RLS-10 ポインタ）を
/// すべて実行し、式木の葉（[`crate::sql::correlated_scalar::CorrelatedScalarRef`]）へ解決する。
/// `core.rs` の `Statement::Scan` アームが WHERE 側の解決（[`resolve_where_predicates`]）の後・外側の
/// 束縛の前に、同じ `budget`／`in_value_budget`・`read_txn`・`ctx`・相関予算で呼ぶ。
///
/// - 集計形（Issue #1846 と同じ受理形・拒否文言）: 内側を `GROUP BY` 集計で 1 回だけ実行し、相関キー →
///   集計値の表を値プローブへ詰める（空集合の値は `count` が 0、それ以外は NULL）。
/// - 非集計形（Issue #1847 と同じ受理形・拒否文言）: 内側を 1 回だけ実体化し、外側の行ごとに 0 件は
///   NULL・2 件以上は `21000` を葉の評価時点で返す。
/// - 相関の無い内側は `42601`（SQL-29 の契約。式の内側への埋め込みは相関だけを対象とする）。
/// - 束縛後の実行時データ例外（`22xxx`・入れ子の `21000`）は外側の行を評価するまで遅延する。
///
/// 内側は WHERE 側と同じ経路（RLS 暗黙適用・同一スナップショット）だけを通り、新しい可視性判定や
/// 評価器は作らない。静的エラー（未知列・投影列数・未対応の型・受理形外）は外側の行数に依存せず返る。
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_embedded_scalar_subqueries(
    items: &[super::allowlist::EmbeddedScalarSubquery],
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<Vec<super::correlated_scalar::CorrelatedScalarRef>, SqlSurfaceError> {
    use super::correlated_scalar::{column_type_of_meta, expr_type_for, CorrelatedScalarRef};
    use super::join::semi::{CorrelatedProbe, CorrelatedProbeRef};
    // 値として運べない型は内側の実行前ではなく結果列メタデータの確定後に静的に拒否する。
    let checked_type = |meta: &ColumnMeta| -> Result<ColumnType, SqlSurfaceError> {
        let ty = column_type_of_meta(meta)?;
        expr_type_for(&ty)?;
        Ok(ty)
    };
    if !corr.is_enabled() {
        return Err(SqlSurfaceError::unsupported(
            "scalar subquery inside an expression is not allowed in this context",
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Some((validated, inner_schema, correlation)) = classify_correlated_aggregate(
            &item.inner_tokens,
            item.depth,
            outer_scopes,
            read_txn,
            lookup,
            nesting_limits,
        )? {
            let mut agg_meta: Option<ColumnMeta> = None;
            match resolve_correlated_scalar_aggregate(
                validated,
                inner_schema,
                correlation,
                outer_scopes,
                read_txn,
                ctx,
                lookup,
                udfs,
                group_limits,
                nesting_limits,
                result_limits,
                budget,
                in_value_budget,
                corr,
                &mut agg_meta,
            ) {
                Ok(table) => {
                    let ty = checked_type(&table.meta)?;
                    let probe = CorrelatedProbe::scalar_agg_value(
                        table.keys,
                        table.values,
                        table.empty,
                        corr.budget(),
                    );
                    out.push(CorrelatedScalarRef::from_probe(
                        CorrelatedProbeRef::new(probe),
                        ty,
                    ));
                }
                // 束縛後の実行時データ例外は外側の行を評価するまで遅延する（投影位置と同じ契約）。
                Err(e) if is_deferrable_runtime_error(&e) => {
                    let Some(meta) = agg_meta else {
                        return Err(e);
                    };
                    out.push(CorrelatedScalarRef::deferred(e, checked_type(&meta)?));
                }
                Err(e) => return Err(e),
            }
            continue;
        }
        if let Some((validated, inner_schema, correlation)) = classify_correlated_scan(
            &item.inner_tokens,
            item.depth,
            outer_scopes,
            read_txn,
            lookup,
            nesting_limits,
        )? {
            let mut rows_meta: Option<ColumnMeta> = None;
            match resolve_correlated_scalar_rows(
                validated,
                inner_schema,
                correlation,
                &ScalarPosition::Projection,
                outer_scopes,
                read_txn,
                ctx,
                lookup,
                udfs,
                group_limits,
                nesting_limits,
                result_limits,
                budget,
                in_value_budget,
                corr,
                &mut rows_meta,
            ) {
                Ok(resolved) => {
                    let ty = checked_type(&resolved.meta)?;
                    out.push(CorrelatedScalarRef::from_probe(resolved.probe, ty));
                }
                Err(e) if is_deferrable_runtime_error(&e) => {
                    let Some(meta) = rows_meta else {
                        return Err(e);
                    };
                    out.push(CorrelatedScalarRef::deferred(e, checked_type(&meta)?));
                }
                Err(e) => return Err(e),
            }
            continue;
        }
        // 相関の無い内側（非相関のスカラーサブクエリ）は式の内側へ埋め込めない（SQL-29 の契約）。
        return Err(SqlSurfaceError::unsupported(
            "scalar subqueries inside expressions are only supported when correlated",
        ));
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
        (ColumnMeta::Id | ColumnMeta::AliasedId { .. }, Some(alias)) => {
            crate::sql::exec::aliased_id_column_meta(alias)
        }
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
/// - `hidden` は外側の結果の末尾に並ぶ隠しキー列の名前（[`hidden_key_columns`]。無ければ空）。
///   集計形の相関スカラー（Issue #1846）が行ごとに値を引き当てるために使い、合流前に取り除く。
pub(crate) fn merge_scalar_projection_items(
    mut result: super::exec::QueryResult,
    items: Vec<ResolvedScalarItem>,
    hidden: &[String],
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
    // 隠しキー列（相関項目が引き当てに使う。末尾に並ぶ）の開始位置。
    let hidden_base = result
        .columns
        .len()
        .checked_sub(hidden.len())
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "scalar subquery hidden key columns are missing".to_string(),
        })?;
    // 外側結果の使用量を引き継ぎ、追加分との合計を確保前に検査する。
    let mut total_bytes: usize = super::cursor::estimate_result_bytes(&result);
    // 項目ごとの行別セル（相関項目のみ。行順）。隠し列を取り除く前に引き当てる。
    let mut per_row: Vec<Option<Vec<Cell>>> = Vec::with_capacity(items.len());
    for item in &items {
        let Some(c) = &item.correlated else {
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
            per_row.push(None);
            continue;
        };
        let mut cells: Vec<Cell> = Vec::new();
        cells
            .try_reserve(result.rows.len())
            .map_err(|_| merge_too_large())?;
        for row in &result.rows {
            let value = c.value_for(row.id, &row.cells, hidden_base, hidden)?;
            let per_cell = std::mem::size_of::<Cell>()
                .checked_add(super::cursor::estimate_cell_bytes(&value))
                .ok_or_else(merge_too_large)?;
            total_bytes = total_bytes
                .checked_add(per_cell)
                .ok_or_else(merge_too_large)?;
            if total_bytes > crate::arena::MAX_ARENA_TOTAL_BYTES {
                return Err(merge_too_large());
            }
            cells.push(value);
        }
        per_row.push(Some(cells));
    }
    // 隠しキー列を取り除いてから、元の位置どおりに合流する。
    if !hidden.is_empty() {
        result.columns.truncate(hidden_base);
        for row in &mut result.rows {
            row.cells.truncate(hidden_base);
        }
    }
    for (item, cells) in items.into_iter().zip(per_row) {
        if item.position > result.columns.len() {
            return Err(SqlSurfaceError::Internal {
                detail: "scalar subquery position out of range".to_string(),
            });
        }
        result.columns.insert(item.position, item.meta);
        let mut per_row_cells = cells.map(Vec::into_iter);
        for row in &mut result.rows {
            if item.position > row.cells.len() {
                return Err(SqlSurfaceError::Internal {
                    detail: "scalar subquery position out of range".to_string(),
                });
            }
            let cell = match per_row_cells.as_mut() {
                Some(it) => it.next().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated scalar subquery row value is missing".to_string(),
                })?,
                None => item.cell.clone(),
            };
            row.cells.insert(item.position, cell);
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
        (SubqueryValueFamily::Time, Cell::Time(t)) => Ok(crate::datetime::format_time(*t)),
        // UTC の明示 offset で渡し、外側の比較がセッション TZ に影響されないようにする。
        (SubqueryValueFamily::TimestampTz, Cell::TimestampTz(t)) => Ok(
            crate::datetime::format_timestamptz(*t, &crate::datetime::SessionTimeZone::utc()),
        ),
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
        // scale 0 の numeric（`SUM(BIGINT)`。Issue #2271）を浮動小数列との比較用に写す。
        // 2^53 以上は正確表現ガードに拒否されるため小数点付きで表す。
        Cell::Numeric(d) if d.scale() == 0 => {
            let n = d.unscaled();
            if n.unsigned_abs() >= 9_007_199_254_740_992 {
                Ok(format!("{n}.0"))
            } else {
                Ok(n.to_string())
            }
        }
        // Issue #2270: 端数のある numeric（整数 `AVG`）を浮動小数列との比較用に `f64` へ写す
        // （PostgreSQL の `float8` と `numeric` の比較と同じ向き）。
        Cell::Numeric(d) => {
            let f: f64 = d.to_string().parse().map_err(|_| unexpected_cell_type())?;
            number_cell_text(&Cell::Float(f))
        }
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

/// 内側投影列が任意 scale の `NUMERIC` か（整数 `AVG`〔Issue #2270〕は値ごとに scale が
/// 変わる numeric。スカラー比較では実行時に厳密比較へ写す）。
fn is_numeric_meta(meta: &ColumnMeta) -> bool {
    match meta {
        ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => {
            matches!(ty, ColumnType::Numeric { .. })
        }
        _ => false,
    }
}

/// スカラーサブクエリ比較の内側投影列の値族が対象列の値族と比較可能かを検証する
/// （数値族どうしは可。内側の結果行数・値に依存しない静的検証）。
fn check_scalar_family_compatible(
    column: &str,
    inner_meta: &ColumnMeta,
    target_family: SubqueryValueFamily,
) -> Result<(), SqlSurfaceError> {
    let compatible = match inner_value_family(inner_meta) {
        Some(inner) => {
            inner == target_family
                || (inner.is_number() && target_family.is_number())
                // Issue #2271（TABLE-13）: `SUM(BIGINT)` は scale 0 の numeric。整数列との
                // 比較は PostgreSQL と同じく許可し、実行時は `i128` の厳密比較で行う。
                || (matches!(
                    target_family,
                    SubqueryValueFamily::Integer | SubqueryValueFamily::Float
                ) && is_numeric_meta(inner_meta))
        }
        None => false,
    };
    if !compatible {
        return Err(SqlSurfaceError::invalid_input(format!(
            "subquery result type is not compatible with column {column:?} \
             (the subquery must have the same value family as the column)"
        )));
    }
    Ok(())
}

/// スカラーサブクエリ比較の対象列（`outer_scopes` 末尾のテーブル）の型と値の族を検証して返す。
/// 疑似列 `id` は整数族。未知列・型非対応は `invalid_input`（静的エラー）。
/// [`resolve_scalar_compare`] から、内側の実行結果に依存せず呼ばれる。
fn scalar_target_column<'a>(
    column: &str,
    outer_scopes: &[&'a TableSchema],
) -> Result<(Option<&'a ColumnType>, SubqueryValueFamily), SqlSurfaceError> {
    let owner = owner_schema(outer_scopes)?;
    match owner.columns.iter().find(|c| c.name == column) {
        Some(def) => Ok((
            Some(&def.ty),
            family_of_type(&def.ty).ok_or_else(|| {
                SqlSurfaceError::invalid_input(format!(
                    "column {column:?} type is not supported for a scalar subquery comparison"
                ))
            })?,
        )),
        None if column == "id" && owner.has_pseudo_id() => Ok((None, SubqueryValueFamily::Integer)),
        None => Err(SqlSurfaceError::undefined_column(column)),
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
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<WherePredicate, SqlSurfaceError> {
    // 集計形の相関スカラー（Issue #1846）は内側を `GROUP BY` 集計で 1 回だけ実行する相関プローブへ
    // 解決する。`corr` が無効な文脈（述語形 DML・集計の `FILTER`）・非集計形の相関は従来どおり
    // 内側の事前検査が `42601` で拒否する。
    if corr.is_enabled() {
        if let Some(pred) = try_resolve_correlated_scalar_compare(
            column,
            op,
            inner_tokens,
            depth,
            outer_scopes,
            read_txn,
            ctx,
            lookup,
            udfs,
            group_limits,
            nesting_limits,
            result_limits,
            budget,
            in_value_budget,
            corr,
        )? {
            return Ok(pred);
        }
    }
    let mut meta_sink: Option<Vec<ColumnMeta>> = None;
    let result = match execute_inner_query_with_meta(
        inner_tokens,
        depth,
        InnerScanIntent::Scalar,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
        &mut meta_sink,
    ) {
        Ok(r) => r,
        Err(e) => {
            // 内側の入れ子 `21000`／実行時データ例外は外側の行数判明まで遅延される契約の
            // ため、同じ述語の静的エラー（対象列の未知・型非対応）を隠さないよう、
            // 遅延対象エラーのときだけ対象列検証を先に行って静的エラーを優先する
            // （Issue #1432。fail-closed）。それ以外の内側エラーは従来どおり即返す。
            if is_deferrable_runtime_error(&e) {
                let (_, target_family) = scalar_target_column(column, outer_scopes)?;
                if let Some([inner_meta]) = meta_sink.as_deref() {
                    check_scalar_family_compatible(column, inner_meta, target_family)?;
                }
            }
            return Err(e);
        }
    };
    if result.columns.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used as a value must select exactly one column",
        ));
    }
    // 対象列（疑似列 `id` は整数族）。行数に依存せず必ず検証する。
    let (target_ty, target_family) = scalar_target_column(column, outer_scopes)?;
    let inner_meta = result
        .columns
        .first()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery result missing projected column metadata".to_string(),
        })?;
    check_scalar_family_compatible(column, inner_meta, target_family)?;

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

/// `= ANY` は `IN`、`<> ALL` は `NOT IN` と同じ意味のため、`resolve_in_subquery` の
/// `negated` 引数へ写す（Issue #1855・SQL-61 (3)）。委譲しない組合せは `None`。
fn quantified_delegates_to_in(
    op: ScalarSubqueryOp,
    quantifier: crate::sql::udf_call::Quantifier,
) -> Option<bool> {
    use crate::sql::udf_call::Quantifier;
    match (op, quantifier) {
        (ScalarSubqueryOp::Eq, Quantifier::Any) => Some(false),
        (ScalarSubqueryOp::Ne, Quantifier::All) => Some(true),
        _ => None,
    }
}

/// 量化比較の対象列を [`scalar_target_column`] で検証し、範囲比較（`< <= > >=`）の対象が
/// BOOLEAN／ENUM なら `22000`、TEXT 系で列の照合がバイト列順でなければ `0A000`（比較の葉が
/// 照合を運ばないため境界値の選び方と評価順序がずれる。fail-closed）で拒否する。内側の
/// 行数・値に依存せず必ず実行する（Issue #1855・SQL-61 (3)）。
fn quantified_target_column<'a>(
    column: &str,
    op: ScalarSubqueryOp,
    outer_scopes: &[&'a TableSchema],
) -> Result<(Option<&'a ColumnType>, SubqueryValueFamily), SqlSurfaceError> {
    let (ty, family) = scalar_target_column(column, outer_scopes)?;
    let is_range = matches!(
        op,
        ScalarSubqueryOp::Lt | ScalarSubqueryOp::Le | ScalarSubqueryOp::Gt | ScalarSubqueryOp::Ge
    );
    if is_range {
        match (family, ty) {
            (SubqueryValueFamily::Boolean, _) => {
                return Err(SqlSurfaceError::invalid_input(
                    "range comparison is not supported for BOOLEAN columns",
                ));
            }
            (SubqueryValueFamily::Text, Some(ColumnType::Enum(_))) => {
                return Err(SqlSurfaceError::invalid_input(
                    "range comparison is not supported for ENUM columns",
                ));
            }
            (SubqueryValueFamily::Text, _) => {
                let collation = owner_schema(outer_scopes)?
                    .columns
                    .iter()
                    .find(|c| c.name == column)
                    .and_then(|c| c.collation);
                crate::sql::udf_call::require_wired_collation(collation)?;
            }
            _ => {}
        }
    }
    Ok((ty, family))
}

/// 数値セルか（整数・浮動小数・numeric）。
fn is_numeric_cell(cell: &Cell) -> bool {
    matches!(
        cell,
        Cell::Integer(_) | Cell::SignedInteger(_) | Cell::Float(_) | Cell::Numeric(_)
    )
}

/// 数値セルを `f64` へ写す（`-0.0` は `+0.0` へ正規化。非有限は `22000`）。
fn numeric_cell_f64(cell: &Cell) -> Result<f64, SqlSurfaceError> {
    let f = match cell {
        Cell::Integer(n) => *n as f64,
        Cell::SignedInteger(n) => *n as f64,
        Cell::Float(f) => *f,
        Cell::Numeric(d) => d.to_string().parse().map_err(|_| unexpected_cell_type())?,
        _ => return Err(unexpected_cell_type()),
    };
    if !f.is_finite() {
        return Err(SqlSurfaceError::invalid_input(
            "subquery returned a non-finite number",
        ));
    }
    Ok(if f == 0.0 { 0.0 } else { f })
}

/// 数値セルを厳密な 10 進数へ写す（浮動小数を含まない組合せ専用）。
fn numeric_cell_decimal(cell: &Cell) -> Result<crate::numeric::Decimal, SqlSurfaceError> {
    match cell {
        Cell::Integer(n) => crate::numeric::Decimal::from_parts(i128::from(*n), 0),
        Cell::SignedInteger(n) => crate::numeric::Decimal::from_parts(i128::from(*n), 0),
        Cell::Numeric(d) => return Ok(*d),
        _ => return Err(unexpected_cell_type()),
    }
    .map_err(|_| unexpected_cell_type())
}

/// 量化比較の境界値選びに使う、同じ値族の 2 セルの全順序。`group_by::cmp_cell_values` は
/// 型が合わないときに `Equal` へ落ちるため再利用せず、想定外の組合せは `Err`（fail-closed）に
/// する。TEXT はバイト列順（照合がバイト列順でない列の範囲比較は
/// [`quantified_target_column`] が先に拒否する。等価判定は deterministic な照合でもバイト列の
/// 等値と一致する）。浮動小数を含む数値は `f64`、それ以外の数値は厳密な 10 進数で比べる。
fn cmp_quantified_cells(a: &Cell, b: &Cell) -> Result<std::cmp::Ordering, SqlSurfaceError> {
    match (a, b) {
        (Cell::Text(x), Cell::Text(y)) => Ok(x.as_bytes().cmp(y.as_bytes())),
        (Cell::Bool(x), Cell::Bool(y)) => Ok(x.cmp(y)),
        (Cell::Date(x), Cell::Date(y)) => Ok(x.cmp(y)),
        (Cell::Timestamp(x), Cell::Timestamp(y))
        | (Cell::Time(x), Cell::Time(y))
        | (Cell::TimestampTz(x), Cell::TimestampTz(y)) => Ok(x.cmp(y)),
        (Cell::Uuid(x), Cell::Uuid(y)) => Ok(x.cmp(y)),
        (Cell::Bytes(x), Cell::Bytes(y)) => Ok(x.cmp(y)),
        _ if is_numeric_cell(a) && is_numeric_cell(b) => {
            if matches!(a, Cell::Float(_)) || matches!(b, Cell::Float(_)) {
                Ok(numeric_cell_f64(a)?.total_cmp(&numeric_cell_f64(b)?))
            } else {
                Ok(crate::numeric::cmp_exact(
                    &numeric_cell_decimal(a)?,
                    &numeric_cell_decimal(b)?,
                ))
            }
        }
        _ => Err(unexpected_cell_type()),
    }
}

/// `<column> <op> ANY|SOME|ALL (SELECT ...)`（Issue #1855・SQL-61 (3)・RLS-10）を解決し、
/// 連言の述語列を返す（空は常に真）。`= ANY`／`<> ALL` は [`resolve_in_subquery`] へ委譲する
/// （予算・NULL 規則・値族検証・ENUM の語彙外ラベル処理は `IN` と同一）。それ以外は内側の
/// 非 NULL 値の最小 `min`・最大 `max` だけから、既存の [`scalar_literal_predicate`] で
/// `col <op> <リテラル>` を作る（第 2 の評価器を作らない。生成する述語は高々 2 つ）。
///
/// 三値論理（`S` は内側の全行。否定は構文段で押し下げ済みのため「真か、真以外か」だけを
/// 区別する）:
/// - `ANY`: 非 NULL 値が無ければ常に偽（空集合・NULL のみ）。NULL は無視する。
///   `> >=` は `min`、`< <=` は `max`、`<>` は `min == max` なら `col <> min`、異なれば
///   `col <> min OR col <> max`。
/// - `ALL`: 内側が 0 行なら常に真（外側値が NULL でも真）。NULL を含めば真にならない
///   （偽か UNKNOWN）ため常に偽。`> >=` は `max`、`< <=` は `min`、`=` は `min == max` なら
///   `col = min`、異なれば `col = min AND col = max`（型変換で両方と等しくなりうるため）。
///
/// 外側値が NULL の行は生成した比較が UNKNOWN になり除外される。判定材料は
/// 可視行（外側と同じ `PolicyContext`・同じスナップショット）だけで、他テナントの行は影響しない。
#[allow(clippy::too_many_arguments)]
fn resolve_quantified_compare(
    column: &str,
    op: ScalarSubqueryOp,
    quantifier: crate::sql::udf_call::Quantifier,
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    use crate::sql::udf_call::Quantifier;
    use std::cmp::Ordering;

    if let Some(negated) = quantified_delegates_to_in(op, quantifier) {
        return resolve_in_subquery(
            column,
            inner_tokens,
            depth,
            negated,
            outer_scopes,
            read_txn,
            ctx,
            lookup,
            udfs,
            group_limits,
            nesting_limits,
            result_limits,
            budget,
            in_value_budget,
            corr,
        );
    }
    let mut meta_sink: Option<Vec<ColumnMeta>> = None;
    let result = match execute_inner_query_with_meta(
        inner_tokens,
        depth,
        InnerScanIntent::Values,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
        &mut meta_sink,
    ) {
        Ok(r) => r,
        Err(e) => {
            // 遅延対象エラーのときだけ、同じ述語の静的エラー（対象列・値族）を先に返す
            // （Issue #1432。[`resolve_scalar_compare`] と同じ fail-closed の規則）。
            if is_deferrable_runtime_error(&e) {
                let (_, target_family) = quantified_target_column(column, op, outer_scopes)?;
                if let Some([inner_meta]) = meta_sink.as_deref() {
                    check_scalar_family_compatible(column, inner_meta, target_family)?;
                }
            }
            return Err(e);
        }
    };
    if result.columns.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used with ANY/ALL must select exactly one column",
        ));
    }
    // 内側の行数に依存せず、対象列・値族を必ず検証する。
    let (target_ty, target_family) = quantified_target_column(column, op, outer_scopes)?;
    let inner_meta = result
        .columns
        .first()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery result missing projected column metadata".to_string(),
        })?;
    check_scalar_family_compatible(column, inner_meta, target_family)?;

    let inner_empty = result.rows.is_empty();
    let mut has_null = false;
    let mut extremes: Option<(&Cell, &Cell)> = None;
    for row in &result.rows {
        let cell = row.cells.first().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery row missing projected cell".to_string(),
        })?;
        if matches!(cell, Cell::Null) {
            has_null = true;
            continue;
        }
        // 非有限の浮動小数は比較が定義できないため `IN` と同じく `22000`。
        if let Cell::Float(f) = cell {
            if !f.is_finite() {
                return Err(SqlSurfaceError::invalid_input(
                    "subquery returned a non-finite number",
                ));
            }
        }
        extremes = Some(match extremes {
            None => (cell, cell),
            Some((min, max)) => (
                if cmp_quantified_cells(cell, min)? == Ordering::Less {
                    cell
                } else {
                    min
                },
                if cmp_quantified_cells(cell, max)? == Ordering::Greater {
                    cell
                } else {
                    max
                },
            ),
        });
    }
    let falsum = || vec![WherePredicate::Or(Vec::new())];
    let literal = |op: ScalarSubqueryOp, cell: &Cell| {
        scalar_literal_predicate(column, op, cell, target_ty, target_family)
    };
    match quantifier {
        Quantifier::Any => {
            let Some((min, max)) = extremes else {
                return Ok(falsum());
            };
            match op {
                ScalarSubqueryOp::Gt | ScalarSubqueryOp::Ge => Ok(vec![literal(op, min)?]),
                ScalarSubqueryOp::Lt | ScalarSubqueryOp::Le => Ok(vec![literal(op, max)?]),
                ScalarSubqueryOp::Ne => {
                    if cmp_quantified_cells(min, max)? == Ordering::Equal {
                        Ok(vec![literal(ScalarSubqueryOp::Ne, min)?])
                    } else {
                        Ok(vec![WherePredicate::Or(vec![
                            vec![literal(ScalarSubqueryOp::Ne, min)?],
                            vec![literal(ScalarSubqueryOp::Ne, max)?],
                        ])])
                    }
                }
                // `= ANY` は冒頭で `IN` へ委譲済み。
                ScalarSubqueryOp::Eq => Err(SqlSurfaceError::Internal {
                    detail: "= ANY must be delegated to IN".to_string(),
                }),
            }
        }
        Quantifier::All => {
            // 内側 0 行は常に真（外側値が NULL でも真）。
            if inner_empty {
                return Ok(Vec::new());
            }
            // NULL を含む、または非 NULL 値が無ければ真にならない。
            let Some((min, max)) = extremes.filter(|_| !has_null) else {
                return Ok(falsum());
            };
            match op {
                ScalarSubqueryOp::Gt | ScalarSubqueryOp::Ge => Ok(vec![literal(op, max)?]),
                ScalarSubqueryOp::Lt | ScalarSubqueryOp::Le => Ok(vec![literal(op, min)?]),
                ScalarSubqueryOp::Eq => {
                    if cmp_quantified_cells(min, max)? == Ordering::Equal {
                        Ok(vec![literal(ScalarSubqueryOp::Eq, min)?])
                    } else {
                        // 異なる境界値でも、外側列の比較型（整数列×浮動小数、DOUBLE 列×BIGINT の
                        // f64 変換等）では両方と等しくなりうる。個別比較の連言と一致させるため、
                        // 恒偽にせず `col = min AND col = max` を返す（変換は単調なので中間値も同値）。
                        Ok(vec![
                            literal(ScalarSubqueryOp::Eq, min)?,
                            literal(ScalarSubqueryOp::Eq, max)?,
                        ])
                    }
                }
                // `<> ALL` は冒頭で `NOT IN` へ委譲済み。
                ScalarSubqueryOp::Ne => Err(SqlSurfaceError::Internal {
                    detail: "<> ALL must be delegated to NOT IN".to_string(),
                }),
            }
        }
    }
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
            if matches!(cell, Cell::Integer(_) | Cell::SignedInteger(_))
                || matches!(cell, Cell::Numeric(_)) =>
        {
            // 整数列 × 整数セルは `2^53` 超でも比較できるよう `int_compare` へ写像する。
            // scale 0 の numeric（`SUM(BIGINT)`。Issue #2271）も同じ経路で厳密に比べる。
            let n: i128 = match cell {
                Cell::Integer(u) => i128::from(*u),
                Cell::SignedInteger(i) => i128::from(*i),
                Cell::Numeric(d) if d.scale() == 0 => d.unscaled(),
                // Issue #2270: 端数のある numeric（整数 `AVG`）は floor で厳密な整数比較へ
                // 書き換える。整数列は端数を持てないため `=` は常に偽、`>`/`>=` は
                // `> floor`、`<`/`<=` は `<= floor`、`<>` は非 NULL の全行。
                Cell::Numeric(d) => {
                    let divisor = 10i128
                        .checked_pow(u32::from(d.scale()))
                        .ok_or_else(unexpected_cell_type)?;
                    let q = d.unscaled() / divisor;
                    let r = d.unscaled() % divisor;
                    if r == 0 {
                        q
                    } else {
                        let floor = if d.unscaled() < 0 { q - 1 } else { q };
                        let le = target_int_compare(column, target_ty, BinOp::Le, floor);
                        let gt = target_int_compare(column, target_ty, BinOp::Gt, floor);
                        return Ok(match op {
                            ScalarSubqueryOp::Eq => WherePredicate::Or(Vec::new()),
                            ScalarSubqueryOp::Gt | ScalarSubqueryOp::Ge => gt,
                            ScalarSubqueryOp::Lt | ScalarSubqueryOp::Le => le,
                            ScalarSubqueryOp::Ne => WherePredicate::Or(vec![vec![le], vec![gt]]),
                        });
                    }
                }
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

/// [`resolve_exists_subquery`] の解決結果。
#[derive(Debug)]
enum ExistsOutcome {
    /// 外側の行に依存しない結果（可視行の有無）。
    Constant(bool),
    /// 相関 `EXISTS`（Issue #1842）: 外側の行ごとに判定するプローブ。
    Probe(super::join::semi::CorrelatedProbeRef),
}

/// `EXISTS (SELECT ...)` を解決する。非相関なら内側は [`InnerScanIntent::ExistenceOnly`] で評価し
/// （投影不要・実質 `LIMIT` 1。PR #1103 追加 codex-review P1 指摘対応の詳細は
/// [`execute_inner_scan_statement`] 参照）、可視行の有無を [`ExistsOutcome::Constant`] で返す。
/// 非相関の `NOT EXISTS` は呼び出し側が結果を反転する（相関は反結合プローブが反転を持つ）。
///
/// 相関（Issue #1842・#1843・SQL-61）: `corr` が有効なとき、内側が外側の列を参照していれば
/// [`resolve_correlated_exists`] で内側を 1 回だけ実体化したプローブを返す。`positive` が偽
/// （`NOT EXISTS`）ならプローブは反結合（判定を反転）として組み立てる。相関スカラー・`corr` 無効の
/// 文脈（述語形 DML・集計の `FILTER`）は従来どおり内側の事前検査が `42601` で拒否する。
#[allow(clippy::too_many_arguments)]
fn resolve_exists_subquery(
    inner_tokens: &[Token],
    depth: usize,
    positive: bool,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<ExistsOutcome, SqlSurfaceError> {
    if corr.is_enabled() {
        if let Some(probe) = try_resolve_correlated_exists(
            inner_tokens,
            depth,
            !positive,
            outer_scopes,
            read_txn,
            ctx,
            lookup,
            udfs,
            group_limits,
            nesting_limits,
            result_limits,
            budget,
            in_value_budget,
            corr,
        )? {
            return Ok(ExistsOutcome::Probe(probe));
        }
    }
    let result = execute_inner_query(
        inner_tokens,
        depth,
        InnerScanIntent::ExistenceOnly,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    )?;
    Ok(ExistsOutcome::Constant(!result.rows.is_empty()))
}

/// 内側が広域取得で外側の列を参照する形か判定し、そうなら検証済みの内側・内側スキーマ・相関 IR を返す。
/// 相関が無い・広域取得でない・ウィンドウ関数を含む内側は `None`（呼び出し側の従来経路が同じ検査・
/// 拒否を行う）。実行経路（[`try_resolve_correlated_exists`]）と失敗時の静的検証
/// （[`static_validate_exists`]）が共有する（2 系統の検査を分岐させない）。
fn classify_correlated_scan(
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    lookup: &impl TableLookup,
    nesting_limits: crate::nesting_limits::NestingLimits,
) -> Result<Option<(ValidatedScan, TableSchema, correlation::Correlation)>, SqlSurfaceError> {
    let Statement::Scan(validated) =
        super::allowlist::validate_sql_tokens_with_subquery_ctx_limits(
            inner_tokens,
            lookup,
            depth,
            nesting_limits,
        )?
    else {
        return Ok(None);
    };
    if !validated.window_items.is_empty() {
        return Ok(None);
    }
    let inner_schema = inner_table_schema(read_txn, &validated.table_name)?;
    let referenced = scan_referenced_columns(&validated);
    let correlation = correlation::bind_correlation(&referenced, &inner_schema, outer_scopes)?;
    if !correlation.is_correlated() {
        return Ok(None);
    }
    if !validated.scalar_subquery_items.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "a subquery cannot contain a scalar subquery in its SELECT list",
        ));
    }
    Ok(Some((validated, inner_schema, correlation)))
}

/// 内側が外側の列を参照するときだけ、相関 `EXISTS` のプローブを作って返す（それ以外は `None`）。
#[allow(clippy::too_many_arguments)]
fn try_resolve_correlated_exists(
    inner_tokens: &[Token],
    depth: usize,
    negated: bool,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<Option<super::join::semi::CorrelatedProbeRef>, SqlSurfaceError> {
    let Some((validated, inner_schema, correlation)) = classify_correlated_scan(
        inner_tokens,
        depth,
        outer_scopes,
        read_txn,
        lookup,
        nesting_limits,
    )?
    else {
        return Ok(None);
    };
    resolve_correlated_exists(
        validated,
        inner_schema,
        correlation,
        &CorrelatedShape::Exists { negated },
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    )
    .map(Some)
}

/// 相関サブクエリ述語の形（[`prepare_correlated_exists`]・[`resolve_correlated_exists`] の分岐）。
enum CorrelatedShape<'a> {
    /// `EXISTS`／`NOT EXISTS`（`negated` なら反結合。Issue #1842・#1843）。
    Exists { negated: bool },
    /// `target_column [NOT] IN (SELECT y ...)`（Issue #1844）。
    In {
        target_column: &'a str,
        negated: bool,
    },
    /// 非集計形の相関スカラー（Issue #1847。WHERE 位置の比較・投影位置の両方）。
    Scalar,
}

/// 相関 `EXISTS` の、実行を伴わない準備の結果（[`prepare_correlated_exists`]）。
struct PreparedCorrelated {
    /// `WHERE` を取り除いた内側（実体化・静的検証用の土台）。
    validated: ValidatedScan,
    analysis: correlated_exists::Analysis,
    /// 実体化する内側列（等価キー → 残余の順。重複なし。疑似列 `id` は含めない）。
    projected: Vec<String>,
    residual: Option<super::join::semi::Residual>,
    /// 内側 `OFFSET`（検証済み）。
    offset: usize,
    /// 相関 `NOT IN` の対象ペア `(y, x)`（等価キーには入れない。`ProbeKind::NotIn` が持つ）。
    not_in_target: Option<correlated_exists::KeyPair>,
    /// 非集計形の相関スカラー（Issue #1847）の投影列 y（疑似列 `id` を含む）。それ以外は `None`。
    scalar_column: Option<String>,
    /// y が疑似列 `id` か（セルは行 `id` から作る）。
    scalar_pseudo_id: bool,
    /// y の列メタデータ（外側の行数・値に依存しない。非集計形の相関スカラーだけ `Some`）。
    scalar_meta: Option<ColumnMeta>,
    /// 内側の明示 `LIMIT n`（省略・`LIMIT ALL` は `None`）。
    scalar_limit: Option<usize>,
}

/// 相関 `EXISTS` の準備（Issue #1842）。実行も走査もしない静的な処理だけを行い、実行経路
/// （[`resolve_correlated_exists`]）と失敗時の静的検証（[`static_validate_correlated_exists`]）が共有する。
///
/// 手順: (1) 外側参照が `WHERE` の中だけであることを確認、(2) `LIMIT`／`OFFSET`／投影／`ORDER BY` を
/// 従来の `EXISTS` と同じく差し替え前に検証、(3) `WHERE` の連言を分解
/// （[`correlated_exists::analyze`]）、(4) 残余を合成スキーマへ束縛する。
fn prepare_correlated_exists(
    validated: ValidatedScan,
    inner_schema: &TableSchema,
    correlation: &correlation::Correlation,
    shape: &CorrelatedShape<'_>,
    outer_scopes: &[&TableSchema],
    udfs: &UdfRegistry,
    result_limits: crate::result_limits::ResultLimits,
) -> Result<PreparedCorrelated, SqlSurfaceError> {
    let outer_schema = owner_schema(outer_scopes)?;
    // (1) 外側参照は `WHERE` の中だけを対象にする。投影・`ORDER BY` の外側参照は真偽に影響しないが、
    // 束縛できる形に直す対象外とし、従来どおり `42601` で拒否する（`WHERE` を除いた参照名が
    // 外側へ束縛されるかで判定する。`WHERE` と同じ列名を投影が参照する場合も拒否できる）。
    let mut non_where = validated.clone();
    non_where.where_predicates = Vec::new();
    let non_where_correlation = correlation::bind_correlation(
        &scan_referenced_columns(&non_where),
        inner_schema,
        outer_scopes,
    )?;
    if non_where_correlation.is_correlated() {
        return Err(SqlSurfaceError::unsupported(
            "correlated subqueries are not supported",
        ));
    }

    // (2) 差し替え前の検証（従来の `EXISTS` と同じ契約。差し替えで迂回しない）。
    if !validated.limit_implicit {
        super::parser::validate_search_limit_with(validated.limit, &result_limits)?;
    }
    let offset = super::parser::validate_search_offset(validated.offset)?;
    let mut probe_node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let projected_cols = super::parser::bind_projection(
        &validated.projection,
        inner_schema,
        udfs,
        &mut probe_node_budget,
    )?;
    // 投影は元のまま残す（`ORDER BY` の出力別名・序数の解決が投影を参照するため）。
    let mut shape_probe = validated.clone();
    shape_probe.where_predicates = Vec::new();
    shape_probe.limit = 1;
    shape_probe.limit_implicit = false;
    let shape_bound = super::parser::bind_scan_with_dummy_flags(
        &shape_probe,
        inner_schema,
        udfs,
        &[],
        &result_limits,
    )?;
    // 非集計形の相関スカラー（Issue #1847）の値の列メタデータ（外側の行数・値に依存しない）。
    let shape_meta = match super::describe::scan_columns(&shape_bound, inner_schema) {
        Ok(metas) => match metas.as_slice() {
            [m] => Some(m.clone()),
            _ => None,
        },
        Err(_) => None,
    };

    // (2b) 相関 `IN`／`NOT IN` の静的検査（非相関 `IN` と同じ順序・同じコード・文言。内側の行数・値に
    // 依存しない）。対象ペア `(y, x)` を結合キーの形へ写せない組は `0A000`。
    let in_pair = match shape {
        CorrelatedShape::Exists { .. } => None,
        CorrelatedShape::Scalar => {
            if projected_cols.len() != 1 {
                return Err(SqlSurfaceError::unsupported(
                    "subquery used as a value must select exactly one column",
                ));
            }
            // 外側の行ごとの先頭 n 件（`ORDER BY` と `LIMIT`／`OFFSET` の併用）は実現できない。
            // `ORDER BY` だけなら一致行の個数に影響しないため受理する（fail-closed で拒否する範囲を狭めない）。
            if (!validated.order_by.is_empty() || !validated.order_keys.is_empty())
                && (!validated.limit_implicit || offset > 0)
            {
                return Err(SqlSurfaceError::unsupported(
                    "correlated scalar subqueries with ORDER BY and LIMIT or OFFSET are not supported",
                ));
            }
            None
        }
        CorrelatedShape::In { target_column, .. } => {
            if projected_cols.len() != 1 {
                return Err(SqlSurfaceError::unsupported(
                    "subquery used with IN must select exactly one column",
                ));
            }
            let (_, family) = validate_in_target_column(target_column, outer_schema)?;
            let y = single_projected_column(&validated.projection)?;
            let inner_ty = correlated_exists::inner_column_type(inner_schema, &y)?;
            let compatible = match &inner_ty {
                None => family == SubqueryValueFamily::Integer,
                Some(ty) => {
                    family_of_type(ty) == Some(family)
                        || (matches!(
                            family,
                            SubqueryValueFamily::Integer | SubqueryValueFamily::Float
                        ) && matches!(ty, ColumnType::Numeric { .. }))
                }
            };
            if !compatible {
                return Err(in_family_mismatch(target_column));
            }
            // 集合の帰属を変える `LIMIT n`／`OFFSET k` は外側の行ごとの先頭 n 件になり実現できない
            // （`EXISTS` のように最小一致件数へ変換できない）。fail-closed で拒否する。
            if !validated.limit_implicit || offset > 0 {
                return Err(SqlSurfaceError::unsupported(
                    "correlated IN subqueries with LIMIT or OFFSET are not supported",
                ));
            }
            Some(correlated_exists::in_target_key(
                inner_schema,
                outer_schema,
                &y,
                target_column,
            )?)
        }
    };

    // (3) 述語の分解。
    let mut validated = validated;
    let mut analysis = correlated_exists::analyze(
        std::mem::take(&mut validated.where_predicates),
        inner_schema,
        outer_schema,
        correlation,
    )?;
    // 肯定形の相関 `IN` は `EXISTS (... AND y = x)` と同値（UNKNOWN は WHERE で偽）。対象ペアを等価キーへ
    // 足す。`NOT IN` は NULL を考慮するため等価キーに入れず、専用のプローブ種別が持つ。
    let mut not_in_target = None;
    if let (Some(pair), CorrelatedShape::In { negated, .. }) = (in_pair, shape) {
        if *negated {
            not_in_target = Some(pair);
        } else {
            analysis.keys.push(pair);
        }
    }

    // 非集計形の相関スカラー（Issue #1847）の投影列 y。単一の裸の列だけを受理する。
    let (scalar_column, scalar_pseudo_id) = match shape {
        CorrelatedShape::Scalar => {
            let y = single_projected_column(&validated.projection).map_err(|_| {
                SqlSurfaceError::unsupported(
                    "a correlated scalar subquery must select a plain column \
                     (expressions are not supported)",
                )
            })?;
            let pseudo = correlated_exists::inner_column_type(inner_schema, &y)?.is_none();
            (Some(y), pseudo)
        }
        CorrelatedShape::Exists { .. } | CorrelatedShape::In { .. } => (None, false),
    };
    let scalar_limit = if validated.limit_implicit {
        None
    } else {
        Some(
            usize::try_from(validated.limit)
                .map_err(|_| SqlSurfaceError::invalid_input("LIMIT value is out of range"))?,
        )
    };

    // 実体化する内側列。疑似列 `id` は行 `id` で賄う。
    let mut projected: Vec<String> = Vec::new();
    for key in &analysis.keys {
        if !key.inner_is_pseudo_id && !projected.contains(&key.inner_column) {
            projected.push(key.inner_column.clone());
        }
    }
    if let Some(y) = &scalar_column {
        if !scalar_pseudo_id && !projected.contains(y) {
            projected.push(y.clone());
        }
    }
    if let Some(pair) = &not_in_target {
        if !pair.inner_is_pseudo_id && !projected.contains(&pair.inner_column) {
            projected.push(pair.inner_column.clone());
        }
    }
    let mut residual_idents = HashSet::new();
    super::parser::collect_where_predicate_idents(&analysis.residual, &mut residual_idents);
    let mut residual_idents: Vec<String> = residual_idents.into_iter().collect();
    residual_idents.sort();
    for ident in residual_idents {
        if inner_schema.columns.iter().any(|c| c.name == ident) && !projected.contains(&ident) {
            projected.push(ident);
        }
    }

    // (4) 残余の束縛（静的に確定する誤りは実体化より前に返す）。
    let residual = if analysis.residual.is_empty() {
        None
    } else {
        Some(correlated_exists::bind_residual(
            &analysis.residual,
            inner_schema,
            outer_schema,
            correlation,
            udfs,
            &|name: &str| {
                projected
                    .iter()
                    .position(|p| p == name)
                    .unwrap_or(usize::MAX)
            },
        )?)
    };
    Ok(PreparedCorrelated {
        validated,
        analysis,
        projected,
        residual,
        offset,
        not_in_target,
        scalar_column,
        scalar_pseudo_id,
        scalar_meta: if matches!(shape, CorrelatedShape::Scalar) {
            shape_meta
        } else {
            None
        },
        scalar_limit,
    })
}

/// 相関 `IN` の内側投影が「単一の裸の内側列（実列または疑似列 `id`）」であることを確認して列名を返す。
/// 別名付きの列参照（Issue #1540）も受理する。式投影は実体化できず `42601`（静的文字列のみ）。
fn single_projected_column(projection: &Projection) -> Result<String, SqlSurfaceError> {
    let name = match projection {
        Projection::Columns(cols) => match cols.as_slice() {
            [c] => Some(c.clone()),
            _ => None,
        },
        Projection::Items(items) => match items.as_slice() {
            [SelectItem::Column(c)] => Some(c.clone()),
            [SelectItem::Expr {
                expr: Expr::Ident(c),
                alias: Some(_),
            }] => Some(c.clone()),
            _ => None,
        },
        Projection::All => None,
    };
    name.ok_or_else(|| {
        SqlSurfaceError::unsupported(
            "a correlated IN subquery must select a plain column (expressions are not supported)",
        )
    })
}

/// 内側を実体化用の形（等価キー・残余が使う列だけを投影し、真偽に影響しない `ORDER BY`・`OFFSET`・
/// 上限を外した形）へ整える。実体化と静的検証が同じ形で束縛する。
fn shape_for_materialization(validated: &mut ValidatedScan, projected: &[String]) {
    validated.projection = Projection::Columns(projected.to_vec());
    validated.order_by.clear();
    validated.order_keys.clear();
    validated.physical_order_keys.clear();
    validated.window_items.clear();
    validated.scalar_subquery_items.clear();
    validated.offset = 0;
    validated.with_ties = false;
    validated.limit = 1;
    validated.limit_implicit = false;
}

/// 準備済みの相関サブクエリの内側を 1 回だけ実体化する（Issue #1842。非集計形の相関スカラー
///〔Issue #1847〕と共有）。内側だけの連言（入れ子のサブクエリを含む）を解決し、外側と同じ `ctx`・
/// `read_txn`（RLS 暗黙適用・同一スナップショット）で走査する。内側の入力行数・バイト量・実行回数予算の
/// 超過は `54000`。`prepared.analysis.inner_only` は解決のため取り出す（呼び出し側が失敗時の静的検証に
/// 要るなら事前に複製する）。
#[allow(clippy::too_many_arguments)]
fn materialize_correlated(
    prepared: &mut PreparedCorrelated,
    inner_schema: &TableSchema,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<super::exec::QueryResult, SqlSurfaceError> {
    // 内側だけの連言を解決し、1 回だけ実体化する。
    let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
    chain.push(inner_schema);
    prepared.validated.where_predicates = resolve_where_predicates(
        std::mem::take(&mut prepared.analysis.inner_only),
        &chain,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    )?;
    *budget = budget.checked_sub(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large(format!(
            "subquery execution count exceeds limit {}",
            nesting_limits.max_subquery_executions()
        ))
    })?;
    shape_for_materialization(&mut prepared.validated, &prepared.projected);
    let mut bound = super::parser::bind_scan_with_dummy_flags(
        &prepared.validated,
        inner_schema,
        udfs,
        &[],
        &result_limits,
    )?;
    // 取得上限は内側の入力行数の上限 + 1 件（超過の検出用）。文の `LIMIT` ではなく JOIN の入力と同じ
    // 設定値で頭打ちにし、超過は黙って切り詰めず `54000` にする。
    bound.limit = corr.max_input_rows().saturating_add(1);
    let result = super::scan::execute_scan_with_budget(
        read_txn,
        ctx,
        inner_schema,
        &bound,
        corr.byte_cap(),
    )?;
    if result.rows.len() > corr.max_input_rows() {
        return Err(SqlSurfaceError::payload_too_large(
            "correlated subquery input exceeds the visible row limit",
        ));
    }
    Ok(result)
}

/// 相関 `EXISTS` を実行用のプローブへ解決する（Issue #1842・SQL-61 ポインタ・RLS-10 ポインタ）。
///
/// [`prepare_correlated_exists`] の後、内側だけの連言（入れ子のサブクエリを含む）を解決して
/// 外側と同じ `ctx`・`read_txn`（RLS 暗黙適用・同一スナップショット）で 1 回だけ実体化し、
/// 等価キーのハッシュ表と残余を持つプローブを組み立てる。`OFFSET k` は「一致が `k + 1` 件以上」で
/// 保存する。内側の入力行数・バイト量・実行回数予算の超過は `54000`。
#[allow(clippy::too_many_arguments)]
fn resolve_correlated_exists(
    validated: ValidatedScan,
    inner_schema: TableSchema,
    correlation: correlation::Correlation,
    shape: &CorrelatedShape<'_>,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<super::join::semi::CorrelatedProbeRef, SqlSurfaceError> {
    use super::join::semi::{
        CorrelatedProbe, CorrelatedProbeRef, InnerInput, OuterKey, ProbeKind, ProbeSpec,
    };

    let mut prepared = prepare_correlated_exists(
        validated,
        &inner_schema,
        &correlation,
        shape,
        outer_scopes,
        udfs,
        result_limits,
    )?;
    let result = materialize_correlated(
        &mut prepared,
        &inner_schema,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    )?;
    let PreparedCorrelated {
        analysis,
        projected,
        residual,
        offset,
        not_in_target,
        ..
    } = prepared;

    // プローブの組み立て。
    let keys: Vec<OuterKey> = analysis
        .keys
        .iter()
        .map(|k| OuterKey {
            source: k.outer.source,
            class: k.class.clone(),
        })
        .collect();
    let key_positions: Vec<Option<usize>> = analysis
        .keys
        .iter()
        .map(|k| {
            if k.inner_is_pseudo_id {
                None
            } else {
                projected.iter().position(|p| p == &k.inner_column)
            }
        })
        .collect();
    // 判定方式。全形を明示的に扱う（取りこぼしで述語が落ちる fail-open を作らない）。
    let kind = match (shape, &not_in_target) {
        (CorrelatedShape::Exists { negated }, _) => ProbeKind::Exists { negated: *negated },
        (CorrelatedShape::In { negated: false, .. }, _) => ProbeKind::Exists { negated: false },
        (CorrelatedShape::In { negated: true, .. }, Some(pair)) => ProbeKind::NotIn {
            target: OuterKey {
                source: pair.outer.source,
                class: pair.class.clone(),
            },
        },
        (CorrelatedShape::In { negated: true, .. }, None) => {
            return Err(SqlSurfaceError::Internal {
                detail: "correlated NOT IN is missing its target pair".to_string(),
            })
        }
        // 非集計形の相関スカラーは `resolve_correlated_scalar_rows` が組み立てる。破れたら fail-closed。
        (CorrelatedShape::Scalar, _) => {
            return Err(SqlSurfaceError::Internal {
                detail: "correlated scalar was routed to the EXISTS builder".to_string(),
            })
        }
    };
    // `NOT IN` の対象値 y の取り出し位置（疑似列 `id` は行 `id` から作る）。
    let value_pos: Option<Option<usize>> = not_in_target.as_ref().map(|pair| {
        if pair.inner_is_pseudo_id {
            None
        } else {
            projected.iter().position(|p| p == &pair.inner_column)
        }
    });
    let value_is_id = not_in_target
        .as_ref()
        .is_some_and(|pair| pair.inner_is_pseudo_id);
    let keep_cells = residual.is_some();
    let min_matches = offset.saturating_add(1);
    let inputs =
        result.rows.into_iter().map(|row| {
            let mut key_cells = Vec::with_capacity(key_positions.len());
            for (pos, key) in key_positions.iter().zip(&analysis.keys) {
                let cell = match pos {
                    Some(i) => {
                        row.cells
                            .get(*i)
                            .cloned()
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "correlated subquery key column is missing from the row"
                                    .to_string(),
                            })?
                    }
                    None if key.inner_is_pseudo_id => Cell::Integer(row.id),
                    None => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "correlated subquery key column was not projected".to_string(),
                        })
                    }
                };
                key_cells.push(cell);
            }
            let value_cell =
                match value_pos {
                    None => None,
                    Some(Some(i)) => Some(row.cells.get(i).cloned().ok_or_else(|| {
                        SqlSurfaceError::Internal {
                            detail: "correlated subquery value column is missing from the row"
                                .to_string(),
                        }
                    })?),
                    Some(None) if value_is_id => Some(Cell::Integer(row.id)),
                    Some(None) => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "correlated subquery value column was not projected"
                                .to_string(),
                        })
                    }
                };
            let cells: Vec<Cell> = if keep_cells { row.cells } else { Vec::new() };
            Ok(InnerInput {
                id: row.id,
                key_cells,
                cells,
                value_cell,
                scalar: None,
            })
        });
    let probe = CorrelatedProbe::build(
        ProbeSpec {
            keys,
            residual,
            min_matches,
            kind,
            budget: corr.budget(),
            byte_cap: corr.byte_cap(),
        },
        inputs,
    )?;
    Ok(CorrelatedProbeRef::new(probe))
}

/// `target_column [NOT] IN (SELECT ...)` を解決する。内側が外側の列を参照する広域取得で `corr` が
/// 有効なら、内側を 1 回だけ実体化した相関プローブ（肯定形は半結合、`NOT IN` は NULL を考慮した
/// 反結合）へ書き換える（Issue #1844・SQL-61 ポインタ・RLS-10 ポインタ）。それ以外（非相関・`corr`
/// 無効の文脈）は従来の [`resolve_in_subquery`] に委ねる（非相関の挙動は不変。相関は事前検査が
/// `42601` で拒否する）。
#[allow(clippy::too_many_arguments)]
fn resolve_in_predicate(
    column: &str,
    inner_tokens: &[Token],
    depth: usize,
    negated: bool,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    if corr.is_enabled() {
        if let Some((validated, inner_schema, correlation)) = classify_correlated_scan(
            inner_tokens,
            depth,
            outer_scopes,
            read_txn,
            lookup,
            nesting_limits,
        )? {
            let probe = resolve_correlated_exists(
                validated,
                inner_schema,
                correlation,
                &CorrelatedShape::In {
                    target_column: column,
                    negated,
                },
                outer_scopes,
                read_txn,
                ctx,
                lookup,
                udfs,
                group_limits,
                nesting_limits,
                result_limits,
                budget,
                in_value_budget,
                corr,
            )?;
            // 内部専用の葉。`NOT` の下には置かない（`NOT IN` は専用プローブが NULL を含めて判定する）。
            return Ok(vec![WherePredicate::CorrelatedExists(probe)]);
        }
    }
    resolve_in_subquery(
        column,
        inner_tokens,
        depth,
        negated,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    )
}

/// 失敗時の静的検証（Issue #1432 の経路）での相関 `IN`／`NOT IN`。内側が外側を参照するときは、
/// 実行せずに [`prepare_correlated_exists`] と内側だけの連言の静的検証・束縛までを行い `true` を返す
/// （否定形は準備の結果が同じなので肯定形として検証する）。相関でなければ `false`（従来の静的検証へ）。
#[allow(clippy::too_many_arguments)]
fn static_validate_correlated_in(
    column: &str,
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
) -> Result<bool, SqlSurfaceError> {
    let Some((validated, inner_schema, correlation)) = classify_correlated_scan(
        inner_tokens,
        depth,
        outer_scopes,
        read_txn,
        lookup,
        nesting_limits,
    )?
    else {
        return Ok(false);
    };
    let mut prepared = prepare_correlated_exists(
        validated,
        &inner_schema,
        &correlation,
        &CorrelatedShape::In {
            target_column: column,
            negated: false,
        },
        outer_scopes,
        udfs,
        result_limits,
    )?;
    let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
    chain.push(&inner_schema);
    static_validate_subquery_predicates(
        &prepared.analysis.inner_only,
        &chain,
        read_txn,
        lookup,
        udfs,
        nesting_limits,
        result_limits,
    )?;
    prepared.validated.where_predicates =
        static_validation_predicates(&prepared.analysis.inner_only);
    shape_for_materialization(&mut prepared.validated, &prepared.projected);
    super::parser::bind_scan_with_dummy_flags(
        &prepared.validated,
        &inner_schema,
        udfs,
        &[],
        &result_limits,
    )?;
    Ok(true)
}

/// 失敗時の静的検証（Issue #1432 の経路）での `EXISTS`／`NOT EXISTS`。内側が外側を参照するときは、
/// 実行せずに [`prepare_correlated_exists`]（相関の分解・残余の束縛）と内側だけの連言の静的検証・
/// 束縛までを行う（実行経路と同じ検査を共有する）。それ以外は従来の静的検証（相関は `42601`）。
#[allow(clippy::too_many_arguments)]
fn static_validate_exists(
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
) -> Result<(), SqlSurfaceError> {
    if let Some((validated, inner_schema, correlation)) = classify_correlated_scan(
        inner_tokens,
        depth,
        outer_scopes,
        read_txn,
        lookup,
        nesting_limits,
    )? {
        let mut prepared = prepare_correlated_exists(
            validated,
            &inner_schema,
            &correlation,
            &CorrelatedShape::Exists { negated: false },
            outer_scopes,
            udfs,
            result_limits,
        )?;
        let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
        chain.push(&inner_schema);
        static_validate_subquery_predicates(
            &prepared.analysis.inner_only,
            &chain,
            read_txn,
            lookup,
            udfs,
            nesting_limits,
            result_limits,
        )?;
        prepared.validated.where_predicates =
            static_validation_predicates(&prepared.analysis.inner_only);
        shape_for_materialization(&mut prepared.validated, &prepared.projected);
        super::parser::bind_scan_with_dummy_flags(
            &prepared.validated,
            &inner_schema,
            udfs,
            &[],
            &result_limits,
        )?;
        return Ok(());
    }
    static_validate_inner_query(
        inner_tokens,
        depth,
        InnerScanIntent::ExistenceOnly,
        outer_scopes,
        read_txn,
        lookup,
        udfs,
        nesting_limits,
        result_limits,
    )
    .map(|_| ())
}
// ===== 集計形の相関スカラーサブクエリ（Issue #1846・SQL-61・親 #1845。RLS-10 ポインタ） =====
//
// `col <op> (SELECT agg(...) FROM b WHERE b.k = a.k [AND <内側だけの条件>])`（WHERE 位置）と
// `SELECT ..., (SELECT agg(...) FROM b WHERE b.k = a.k)`（投影位置）を、内側を相関キーの
// `GROUP BY` 集計へ書き換えて 1 回だけ既存の集計器で実行し、外側の行ごとにキーで引く
// （左外部結合＋集計と同値）。該当する内側行が無い外側の行は空集合に対する集計値
// （`count` は 0、それ以外は NULL）になる。受理形は「GROUP BY の無い単一集計項目で、外側参照が
// 内側 `WHERE` の等価の相関条件だけ」で、キーごとに必ず 1 行になるため `21000` は発生しない。
// それ以外の相関（非等価・OR 混在・内側の GROUP BY／HAVING／LIMIT・集計引数内の外側参照・
// 非集計形）は従来どおり静的文字列の `42601`／`0A000` で拒否する（fail-closed）。
//
// 上限（DoS）: 内側の可視行は集計器が走査するだけで保持せず、保持量はグループ数
// （`GroupByLimits::max_groups`。ハード上限 `MAX_GROUPS` = 10,000）と結果バイト上限で有界。
// このため内側の結果行数上限（`--max-result-rows`）の検査は適用しない（グループ数超過は集計器が
// `54000`）。実行回数は入れ子を含む文全体の予算を 1 消費する。

/// 受理形の外の相関スカラー集計の拒否（静的文字列のみ。他テナントの情報・予約内部名を含めない）。
fn correlated_scalar_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::unsupported(
        "this form of correlated scalar subquery is not supported (a single plain column, or a \
         single aggregate whose correlation is equality conditions in WHERE)",
    )
}

/// 内側が単一集計の集計形で外側の列を参照する形か判定し、そうなら検証済みの内側・内側スキーマ・
/// 相関 IR を返す。集計形でない・相関が無い内側は `None`（呼び出し側の従来経路が同じ検査・拒否を行う。
/// 非集計形の相関スカラーは従来どおり `42601`）。実行経路・静的検証経路が共有する。
fn classify_correlated_aggregate(
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    lookup: &impl TableLookup,
    nesting_limits: crate::nesting_limits::NestingLimits,
) -> Result<Option<(ValidatedAggregate, TableSchema, correlation::Correlation)>, SqlSurfaceError> {
    let Statement::Aggregate(validated) =
        super::allowlist::validate_sql_tokens_with_subquery_ctx_limits(
            inner_tokens,
            lookup,
            depth,
            nesting_limits,
        )?
    else {
        return Ok(None);
    };
    let inner_schema = inner_table_schema(read_txn, &validated.table_name)?;
    let referenced = aggregate_referenced_columns(&validated);
    let correlation = correlation::bind_correlation(&referenced, &inner_schema, outer_scopes)?;
    if !correlation.is_correlated() {
        return Ok(None);
    }
    Ok(Some((validated, inner_schema, correlation)))
}

/// 集計形の相関スカラーの、実行を伴わない準備の結果（[`prepare_correlated_scalar_aggregate`]）。
struct PreparedScalarAggregate {
    /// 元の集計（`WHERE` は内側だけの連言。サブクエリは未解決）。空集合の値・列メタデータの束縛用。
    plain: ValidatedAggregate,
    /// 相関キーの `GROUP BY` 集計へ書き換えた形（`WHERE` は `plain` と同じ。先頭がキー列、末尾が元の集計）。
    grouped: ValidatedAggregate,
    /// 等価の相関条件（`GROUP BY` のキーと同じ順）。
    keys: Vec<correlated_exists::KeyPair>,
}

/// 集計形の相関スカラーの準備（実行も走査もしない静的な処理）。受理形の検査・`WHERE` の分解・
/// キー型の検査・`GROUP BY` 形への書き換えを行い、実行経路（[`resolve_correlated_scalar_aggregate`]）と
/// 失敗時の静的検証（[`static_validate_correlated_scalar`]）が共有する。
fn prepare_correlated_scalar_aggregate(
    validated: ValidatedAggregate,
    inner_schema: &TableSchema,
    correlation: &correlation::Correlation,
    outer_scopes: &[&TableSchema],
) -> Result<PreparedScalarAggregate, SqlSurfaceError> {
    use super::allowlist::{AggregateSelectItem, GroupByClause, MAX_GROUP_BY_COLUMNS};
    let outer_schema = owner_schema(outer_scopes)?;
    if validated.items.len() != 1 {
        return Err(SqlSurfaceError::unsupported(
            "subquery used as a value must select exactly one column",
        ));
    }
    // 受理形: GROUP BY・HAVING・（集計 ORDER BY・LIMIT・OFFSET を含む）句なしの単一の集計項目。
    // 式項目（`count(*)+1`）・`SELECT DISTINCT` の脱糖形（`GroupKey`）は空グループの値の意味論が
    // 変わるため拒否する。
    let item = match (validated.items.as_slice(), &validated.group_by) {
        ([AggregateSelectItem::Aggregate(item)], None) => item.clone(),
        _ => return Err(correlated_scalar_unsupported()),
    };
    // 外側参照は内側 `WHERE` の中だけ（集計引数・FILTER・extra_args・集計内 ORDER BY の外側参照は拒否）。
    let mut non_where = validated.clone();
    non_where.where_predicates = Vec::new();
    let non_where_correlation = correlation::bind_correlation(
        &aggregate_referenced_columns(&non_where),
        inner_schema,
        outer_scopes,
    )?;
    if non_where_correlation.is_correlated() {
        return Err(correlated_scalar_unsupported());
    }

    let analysis = correlated_exists::analyze(
        validated.where_predicates.clone(),
        inner_schema,
        outer_schema,
        correlation,
    )?;
    // 等価の相関条件だけで書ける形（キー 1 本以上・残余なし）に限る。残余あり・キーなしは行ごとの
    // 再評価が要るため範囲外（`42601`）。
    if analysis.keys.is_empty() || !analysis.residual.is_empty() {
        return Err(correlated_scalar_unsupported());
    }
    if analysis.keys.len() > MAX_GROUP_BY_COLUMNS {
        return Err(SqlSurfaceError::payload_too_large(
            "correlated scalar subquery has too many correlation keys",
        ));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for key in &analysis.keys {
        // `GROUP BY` で束ねられない（並べ替え不能な）キー型は `0A000`。
        if key.class == super::join::values::JoinKeyClass::Bytea {
            return Err(SqlSurfaceError::feature_not_supported(
                "this column type is not supported in a correlated scalar subquery condition",
            ));
        }
        // 同じ内側列への等価の重複は `GROUP BY` のキー重複になるため拒否する（fail-closed）。
        if !seen.insert(key.inner_column.as_str()) {
            return Err(correlated_scalar_unsupported());
        }
    }

    let mut plain = validated;
    plain.where_predicates = analysis.inner_only;
    let mut items: Vec<AggregateSelectItem> = analysis
        .keys
        .iter()
        .enumerate()
        .map(|(i, k)| AggregateSelectItem::GroupKey {
            column: k.inner_column.clone(),
            // 利用者の別名・列名と衝突しない予約名（制御文字接頭辞）。
            alias: Some(format!("\u{3}corrkey{i}")),
        })
        .collect();
    items.push(AggregateSelectItem::Aggregate(item));
    let grouped = ValidatedAggregate {
        table_name: plain.table_name.clone(),
        items,
        where_predicates: plain.where_predicates.clone(),
        group_by: Some(GroupByClause {
            columns: analysis
                .keys
                .iter()
                .map(|k| k.inner_column.clone())
                .collect(),
            key_exprs: Vec::new(),
            grouping_sets: None,
            having: Vec::new(),
            having_exprs: Vec::new(),
            having_aggregates: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            with_ties: false,
            offset: 0,
        }),
    };
    Ok(PreparedScalarAggregate {
        plain,
        grouped,
        keys: analysis.keys,
    })
}

/// 束縛済みの集計形の相関スカラー（[`bind_scalar_aggregate`]）。
struct BoundScalarAggregate {
    grouped: super::parser::BoundAggregate,
    /// 集計項目 1 列の列メタデータ（外側の行数・値に依存しない）。
    meta: ColumnMeta,
    /// 空集合に対する集計値（`count` は 0、それ以外は NULL。既存の集計評価器の空入力時の値）。
    empty: Cell,
}

/// `where_preds`（解決済み、または静的検証用に整えた述語）で元の集計とキー付き集計を束縛する。
/// 空集合の値は第 2 の評価器を作らず、集計アキュムレータの空入力時の `finish` をそのまま使う。
fn bind_scalar_aggregate(
    prepared: &PreparedScalarAggregate,
    where_preds: Vec<WherePredicate>,
    inner_schema: &TableSchema,
    udfs: &UdfRegistry,
) -> Result<BoundScalarAggregate, SqlSurfaceError> {
    let mut plain = prepared.plain.clone();
    plain.where_predicates = where_preds.clone();
    let bound_plain = super::parser::bind_aggregate(&plain, inner_schema, udfs)?;
    let metas = super::describe::aggregate_columns(&bound_plain, inner_schema)?;
    let [meta] = metas.as_slice() else {
        return Err(SqlSurfaceError::Internal {
            detail: "correlated scalar aggregate has an unexpected column count".to_string(),
        });
    };
    let item = bound_plain
        .items
        .first()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "correlated scalar aggregate is missing its aggregate item".to_string(),
        })?;
    let empty = super::aggregate::Accumulator::for_item(item)?.finish()?;
    let mut grouped = prepared.grouped.clone();
    grouped.where_predicates = where_preds;
    let grouped = super::parser::bind_aggregate(&grouped, inner_schema, udfs)?;
    Ok(BoundScalarAggregate {
        grouped,
        meta: meta.clone(),
        empty,
    })
}

/// 内側の `GROUP BY` 集計の結果（相関キー → 集計値）。外側の行ごとにキーで引く。
/// 可視行由来の値を持つため `Debug` は導出しない。
struct ScalarAggTable {
    /// 相関キーの正準バイト列 → 集計値（キーが NULL のグループは含めない）。
    values: std::collections::HashMap<Vec<u8>, Cell>,
    /// 空集合に対する集計値。
    empty: Cell,
    meta: ColumnMeta,
    /// 外側の取り出し元と型クラス（`values` のキーと同じ順）。
    keys: Vec<super::join::semi::OuterKey>,
    /// 外側の列名（疑似列 `id` は `None`。`keys` と同じ順）。
    key_names: Vec<Option<String>>,
}

/// 集計形の相関スカラーを内側 1 回の `GROUP BY` 集計で実行して [`ScalarAggTable`] にする。
/// 内側だけの連言（入れ子のサブクエリを含む）は先に解決し、外側と同じ `ctx`・`read_txn`
/// （RLS 暗黙適用・同一スナップショット）の既存集計器で実行する。実行回数予算を 1 消費する。
/// 束縛に成功した時点で列メタデータを `meta_sink` へ書き出す（投影位置の実行時データ例外を
/// 外側の行数判明まで遅延するため。[`resolve_scalar_projection_items`] と同じ契約）。
#[allow(clippy::too_many_arguments)]
fn resolve_correlated_scalar_aggregate(
    validated: ValidatedAggregate,
    inner_schema: TableSchema,
    correlation: correlation::Correlation,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
    meta_sink: &mut Option<ColumnMeta>,
) -> Result<ScalarAggTable, SqlSurfaceError> {
    use super::join::semi::{key_bytes_overhead, too_large, OuterKey, OuterSource};
    let outer_schema = owner_schema(outer_scopes)?;
    let prepared =
        prepare_correlated_scalar_aggregate(validated, &inner_schema, &correlation, outer_scopes)?;
    let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
    chain.push(&inner_schema);
    let original_preds = prepared.plain.where_predicates.clone();
    let resolved = match resolve_where_predicates(
        original_preds.clone(),
        &chain,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    ) {
        Ok(p) => p,
        Err(e) => {
            // 入れ子 WHERE の実行時データ例外は、投影位置の外側が行数判明まで遅延できるよう、
            // 実行せずに静的検証・束縛してメタデータを確定する（Issue #1432 と同じ契約）。
            if is_deferrable_runtime_error(&e) {
                static_validate_subquery_predicates(
                    &original_preds,
                    &chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?;
            }
            let bound = bind_scalar_aggregate(
                &prepared,
                static_validation_predicates(&original_preds),
                &inner_schema,
                udfs,
            )?;
            *meta_sink = Some(bound.meta);
            return Err(e);
        }
    };
    *budget = budget.checked_sub(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large(format!(
            "subquery execution count exceeds limit {}",
            nesting_limits.max_subquery_executions()
        ))
    })?;
    let bound = bind_scalar_aggregate(&prepared, resolved, &inner_schema, udfs)?;
    *meta_sink = Some(bound.meta.clone());
    let result = super::aggregate::execute_aggregate_with_group_limits(
        read_txn,
        ctx,
        &inner_schema,
        &bound.grouped,
        super::aggregate::MAX_AGGREGATE_RESULT_BYTES,
        None,
        None,
        None,
        group_limits,
    )?;

    let keys: Vec<OuterKey> = prepared
        .keys
        .iter()
        .map(|k| OuterKey {
            source: k.outer.source,
            class: k.class.clone(),
        })
        .collect();
    let mut key_names: Vec<Option<String>> = Vec::with_capacity(keys.len());
    for k in &keys {
        key_names.push(match k.source {
            OuterSource::Column(i) => Some(
                outer_schema
                    .columns
                    .get(i)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "correlated scalar key column is outside the outer schema"
                            .to_string(),
                    })?
                    .name
                    .clone(),
            ),
            OuterSource::Id => None,
        });
    }

    // 結果行: 先頭がキー列（`keys` と同じ順）、末尾が集計値。確保前に予算へ計上する。
    let mut values: std::collections::HashMap<Vec<u8>, Cell> = std::collections::HashMap::new();
    let mut used = 0usize;
    'rows: for row in result.rows {
        if row.cells.len() != keys.len().saturating_add(1) {
            return Err(SqlSurfaceError::Internal {
                detail: "correlated scalar aggregate row has an unexpected width".to_string(),
            });
        }
        let mut key = Vec::new();
        for (cell, k) in row.cells.iter().zip(&keys) {
            if matches!(cell, Cell::Null) {
                // キーが NULL のグループは `=` で真にならず、どの外側の行にも一致しない。
                continue 'rows;
            }
            super::join::values::encode_key_component(cell, &k.class, &mut key)?;
        }
        let value =
            row.cells
                .into_iter()
                .nth(keys.len())
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated scalar aggregate row is missing its value".to_string(),
                })?;
        used = used
            .saturating_add(key_bytes_overhead(&key))
            .saturating_add(std::mem::size_of::<Cell>())
            .saturating_add(super::cursor::estimate_cell_bytes(&value));
        if used > corr.byte_cap() {
            return Err(too_large());
        }
        values.try_reserve(1).map_err(|_| too_large())?;
        if values.insert(key, value).is_some() {
            // `GROUP BY` はキーごとに 1 行。破れたら黙って上書きせず fail-closed。
            return Err(SqlSurfaceError::Internal {
                detail: "correlated scalar aggregate returned a duplicate group".to_string(),
            });
        }
    }
    Ok(ScalarAggTable {
        values,
        empty: bound.empty,
        meta: bound.meta,
        keys,
        key_names,
    })
}

/// 比較述語の表を組み立てる（同じ値のグループは 1 つの述語を共有する）。
struct ScalarPredicateBuilder<'a> {
    op: ScalarSubqueryOp,
    column: &'a str,
    target_ty: Option<&'a ColumnType>,
    family: SubqueryValueFamily,
    outer_schema: &'a TableSchema,
    udfs: &'a UdfRegistry,
    byte_cap: usize,
    used: usize,
    predicates: Vec<super::where_tree::BoundOrGroup>,
    index_by_value: std::collections::HashMap<String, u32>,
}

impl ScalarPredicateBuilder<'_> {
    /// `cell` に対する比較述語の添字を返す（無ければ束縛して追加する）。NULL は常に偽
    /// （UNKNOWN は WHERE で偽に潰れる。否定は構文段で演算子反転済み）。非 NULL は
    /// 非相関のスカラー比較と同じ [`scalar_literal_predicate`] を外側スキーマへ束縛する。
    fn index_for(&mut self, cell: &Cell) -> Result<u32, SqlSurfaceError> {
        use super::join::semi::too_large;
        let dedup = format!("{cell:?}");
        if let Some(i) = self.index_by_value.get(&dedup) {
            return Ok(*i);
        }
        self.used = self
            .used
            .saturating_add(std::mem::size_of::<super::where_tree::BoundOrGroup>())
            .saturating_add(512)
            .saturating_add(super::cursor::estimate_cell_bytes(cell).saturating_mul(2))
            .saturating_add(dedup.len());
        if self.used > self.byte_cap {
            return Err(too_large());
        }
        let group = if matches!(cell, Cell::Null) {
            super::where_tree::BoundOrGroup::new(Vec::new())
        } else {
            let pred =
                scalar_literal_predicate(self.column, self.op, cell, self.target_ty, self.family)?;
            let mut node_budget = super::udf_call::MAX_EXPR_NODES;
            let (metadata, expr, _rls, or_filters) = super::parser::bind_where_predicates(
                std::slice::from_ref(&pred),
                self.outer_schema,
                self.udfs,
                &mut node_budget,
                &[],
            )?;
            super::where_tree::BoundOrGroup::new(vec![super::where_tree::BoundConjunction::new(
                metadata, expr, or_filters,
            )])
        };
        let index = u32::try_from(self.predicates.len()).map_err(|_| too_large())?;
        self.predicates.try_reserve(1).map_err(|_| too_large())?;
        self.predicates.push(group);
        self.index_by_value.insert(dedup, index);
        Ok(index)
    }
}

// ===== 非集計形の相関スカラーサブクエリ（Issue #1847・SQL-61・親 #1845。RLS-10 ポインタ） =====
//
// `col <op> (SELECT y FROM b WHERE <相関条件>)`（WHERE 位置）と `SELECT ..., (SELECT y FROM b WHERE
// <相関条件>)`（投影位置）を、外側の行ごとの再評価で実行する。内側を外側の行ごとに実行し直す方式
// （外側の値を内側 AST へ埋めて再実行する案）は #1842 の設計記録で却下済みのため、`EXISTS` と同じ
// 方式を採る: 内側は外側と同じ `PolicyContext`・同じ `read_txn`（RLS 暗黙適用）で 1 回だけ実体化し、
// 外側の各行では等価キーのバケット（キーが無ければ全内側行）を走査して残余を既存の述語評価器で
// 評価する（`sql::join::semi` の `ProbeKind::ScalarRows`。第 2 の述語評価器は作らない）。
// 一致行 0 件は NULL（WHERE 位置では偽）、1 件はその行の値、2 件以上は `21000`。判定は可視行だけで
// 決まり、外側の行を評価したときにだけ発生する（外側が 0 行ならエラーにならない）。
//
// 受理形: 内側は `Statement::Scan` で投影が単一の裸の列、外側参照は内側 `WHERE` の中だけ。内側の
// `LIMIT`／`OFFSET` は一致行の先頭 k 件を飛ばし最大 n 件を採る意味で保存する（`ORDER BY` と併用する
// 外側の行ごとの top-n は実現できず `42601`）。それ以外は静的文字列の `42601`／`0A000` で拒否する。

/// 非集計形の相関スカラーの位置（WHERE 位置の比較か、投影位置か）。
enum ScalarPosition<'a> {
    Where {
        column: &'a str,
        op: ScalarSubqueryOp,
    },
    Projection,
}

/// 非集計形の相関スカラーの解決結果。
struct ScalarRowsResolved {
    probe: super::join::semi::CorrelatedProbeRef,
    /// 値（投影列 y）の列メタデータ。
    meta: ColumnMeta,
    /// 投影位置で隠し列として外側の走査へ足す外側列（`(論理列インデックス, 列名, 列型)`）。
    /// WHERE 位置では空。
    outer: Vec<(usize, String, ColumnType)>,
    /// 外側スキーマの列数。
    width: usize,
}

impl ScalarRowsResolved {
    fn probe_ref(&self) -> super::join::semi::CorrelatedProbeRef {
        self.probe.clone()
    }
}

/// `cell_scalar`（外側セル → 行スカラー）が扱える列型か。投影位置で外側のセルを行スカラーへ写す
/// ため、扱えない型（TIME／TIMESTAMPTZ／INTERVAL／JSONB／ARRAY／VECTOR）は静的に `0A000` にする。
fn outer_cell_scalar_supported(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Text
            | ColumnType::Varchar(_)
            | ColumnType::Bpchar(_)
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

/// 内側の 1 行から結合キー成分のセルを作る（疑似列 `id` は行 `id`）。
fn inner_key_cells(
    row: &super::exec::ResultRow,
    key_positions: &[Option<usize>],
    keys: &[correlated_exists::KeyPair],
) -> Result<Vec<Cell>, SqlSurfaceError> {
    let mut key_cells = Vec::with_capacity(key_positions.len());
    for (pos, key) in key_positions.iter().zip(keys) {
        let cell = match pos {
            Some(i) => row
                .cells
                .get(*i)
                .cloned()
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated subquery key column is missing from the row".to_string(),
                })?,
            None if key.inner_is_pseudo_id => Cell::Integer(row.id),
            None => {
                return Err(SqlSurfaceError::Internal {
                    detail: "correlated subquery key column was not projected".to_string(),
                })
            }
        };
        key_cells.push(cell);
    }
    Ok(key_cells)
}

/// 非集計形の相関スカラーを実行用のプローブへ解決する（Issue #1847）。
///
/// [`prepare_correlated_exists`]（静的検査・分解・残余の束縛）の後、WHERE 位置は対象列と値族を内側の
/// 実行より前に検査し、投影位置は外側の列型を検査する（静的エラーは外側の行数・内側の値に依存せず
/// 返る）。内側は [`materialize_correlated`] で 1 回だけ実体化する。入れ子 WHERE の実行時データ例外は
/// 外側の行数判明まで遅延できるよう、実行せずに静的検証してから `Err` を返す（`meta_sink` へ値の
/// 列メタデータを書き出す。[`resolve_scalar_projection_items`] と同じ契約）。
#[allow(clippy::too_many_arguments)]
fn resolve_correlated_scalar_rows(
    validated: ValidatedScan,
    inner_schema: TableSchema,
    correlation: correlation::Correlation,
    position: &ScalarPosition<'_>,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
    meta_sink: &mut Option<ColumnMeta>,
) -> Result<ScalarRowsResolved, SqlSurfaceError> {
    use super::join::semi::{
        CorrelatedProbe, CorrelatedProbeRef, InnerInput, OuterKey, OuterSource, ScalarOut,
        ScalarRowsSpec,
    };
    let outer_schema = owner_schema(outer_scopes)?;
    let mut prepared = prepare_correlated_exists(
        validated,
        &inner_schema,
        &correlation,
        &CorrelatedShape::Scalar,
        outer_scopes,
        udfs,
        result_limits,
    )?;
    let meta = prepared
        .scalar_meta
        .clone()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "correlated scalar subquery is missing its column metadata".to_string(),
        })?;
    *meta_sink = Some(meta.clone());

    // 静的検査（内側の実行より前）。
    let mut builder: Option<ScalarPredicateBuilder<'_>> = None;
    let mut outer_cols: Vec<usize> = Vec::new();
    let mut extra_outer_columns: Vec<usize> = Vec::new();
    match position {
        ScalarPosition::Where { column, op } => {
            let (target_ty, family) = scalar_target_column(column, outer_scopes)?;
            check_scalar_family_compatible(column, &meta, family)?;
            if let Some(i) = outer_schema.columns.iter().position(|c| c.name == *column) {
                extra_outer_columns.push(i);
            }
            builder = Some(ScalarPredicateBuilder {
                op: *op,
                column,
                target_ty,
                family,
                outer_schema,
                udfs,
                byte_cap: corr.byte_cap(),
                used: 0,
                predicates: Vec::new(),
                index_by_value: std::collections::HashMap::new(),
            });
        }
        ScalarPosition::Projection => {
            // 外側のセルを行スカラーへ写す列（キー・残余の外側参照）の型を静的に検査する。
            let sources = prepared.analysis.keys.iter().map(|k| k.outer.source).chain(
                prepared
                    .residual
                    .iter()
                    .flat_map(|r| r.outer_cols.iter().map(|(_, src, _)| *src)),
            );
            for source in sources {
                if let OuterSource::Column(i) = source {
                    if !outer_cols.contains(&i) {
                        outer_cols.push(i);
                    }
                }
            }
            for i in &outer_cols {
                let def =
                    outer_schema
                        .columns
                        .get(*i)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "correlated scalar outer column is outside the outer schema"
                                .to_string(),
                        })?;
                if !outer_cell_scalar_supported(&def.ty) {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "this column type is not supported in a correlated scalar subquery \
                         condition",
                    ));
                }
            }
        }
    }

    let original_inner_only = prepared.analysis.inner_only.clone();
    let result = match materialize_correlated(
        &mut prepared,
        &inner_schema,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
    ) {
        Ok(r) => r,
        Err(e) => {
            // 入れ子 WHERE の実行時データ例外は、外側の行数判明まで遅延できるよう、実行せずに
            // 静的検証して静的エラー（未知列・値族の不一致等）を優先する（Issue #1432 と同じ契約）。
            if is_deferrable_runtime_error(&e) {
                let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
                chain.push(&inner_schema);
                static_validate_subquery_predicates(
                    &original_inner_only,
                    &chain,
                    read_txn,
                    lookup,
                    udfs,
                    nesting_limits,
                    result_limits,
                )?;
            }
            return Err(e);
        }
    };

    let PreparedCorrelated {
        analysis,
        projected,
        residual,
        offset,
        scalar_pseudo_id,
        scalar_column,
        scalar_limit,
        ..
    } = prepared;
    let keys: Vec<OuterKey> = analysis
        .keys
        .iter()
        .map(|k| OuterKey {
            source: k.outer.source,
            class: k.class.clone(),
        })
        .collect();
    let key_positions: Vec<Option<usize>> = analysis
        .keys
        .iter()
        .map(|k| {
            if k.inner_is_pseudo_id {
                None
            } else {
                projected.iter().position(|p| p == &k.inner_column)
            }
        })
        .collect();
    let scalar_pos: Option<usize> = match (&scalar_column, scalar_pseudo_id) {
        (Some(y), false) => projected.iter().position(|p| p == y),
        _ => None,
    };
    let keep_cells = residual.is_some();
    let mut inputs: Vec<InnerInput> = Vec::new();
    inputs
        .try_reserve(result.rows.len())
        .map_err(|_| super::join::semi::too_large())?;
    for row in result.rows {
        let key_cells = inner_key_cells(&row, &key_positions, &analysis.keys)?;
        let id = row.id;
        let y = match scalar_pos {
            Some(i) => row
                .cells
                .get(i)
                .cloned()
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated scalar value column is missing from the row".to_string(),
                })?,
            None if scalar_pseudo_id => Cell::Integer(id),
            None => {
                return Err(SqlSurfaceError::Internal {
                    detail: "correlated scalar value column was not projected".to_string(),
                })
            }
        };
        let scalar = match builder.as_mut() {
            Some(b) => ScalarOut::Pred(b.index_for(&y)?),
            None => ScalarOut::Value(y),
        };
        let cells: Vec<Cell> = if keep_cells { row.cells } else { Vec::new() };
        inputs.push(InnerInput {
            id,
            key_cells,
            cells,
            value_cell: None,
            scalar: Some(scalar),
        });
    }
    let predicates = builder.map(|b| b.predicates).unwrap_or_default();
    let probe = CorrelatedProbe::scalar_rows(
        ScalarRowsSpec {
            keys,
            residual,
            offset,
            limit: scalar_limit,
            predicates,
            extra_outer_columns,
            budget: corr.budget(),
            byte_cap: corr.byte_cap(),
        },
        inputs.into_iter().map(Ok),
    )?;
    // 投影位置: プローブがデコードを要する外側列を隠し列として返す（疑似列 `id` は行 `id` で賄う）。
    let mut outer: Vec<(usize, String, ColumnType)> = Vec::new();
    if matches!(position, ScalarPosition::Projection) {
        for i in probe.outer_columns() {
            let def = outer_schema
                .columns
                .get(*i)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated scalar outer column is outside the outer schema"
                        .to_string(),
                })?;
            outer.push((*i, def.name.clone(), def.ty.clone()));
        }
    }
    Ok(ScalarRowsResolved {
        probe: CorrelatedProbeRef::new(probe),
        meta,
        outer,
        width: outer_schema.columns.len(),
    })
}

/// WHERE 位置 `col <op> (SELECT agg ...)` の内側が集計形の相関なら、内側を 1 回だけ実行した
/// 相関プローブ（[`WherePredicate::CorrelatedExists`] の葉。判定方式は
/// `ProbeKind::ScalarCompare`）へ解決する。相関でなければ `None`（従来の
/// [`resolve_scalar_compare`] の経路）。
#[allow(clippy::too_many_arguments)]
fn try_resolve_correlated_scalar_compare(
    column: &str,
    op: ScalarSubqueryOp,
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
    budget: &mut usize,
    in_value_budget: &mut usize,
    corr: &CorrelatedCtx,
) -> Result<Option<WherePredicate>, SqlSurfaceError> {
    use super::join::semi::{CorrelatedProbe, CorrelatedProbeRef, ScalarTable};
    let Some((validated, inner_schema, correlation)) = classify_correlated_aggregate(
        inner_tokens,
        depth,
        outer_scopes,
        read_txn,
        lookup,
        nesting_limits,
    )?
    else {
        // 非集計形（Issue #1847）。相関が無ければ `None`（従来の非相関経路）。
        let Some((validated, inner_schema, correlation)) = classify_correlated_scan(
            inner_tokens,
            depth,
            outer_scopes,
            read_txn,
            lookup,
            nesting_limits,
        )?
        else {
            return Ok(None);
        };
        let mut meta_sink: Option<ColumnMeta> = None;
        let resolved = resolve_correlated_scalar_rows(
            validated,
            inner_schema,
            correlation,
            &ScalarPosition::Where { column, op },
            outer_scopes,
            read_txn,
            ctx,
            lookup,
            udfs,
            group_limits,
            nesting_limits,
            result_limits,
            budget,
            in_value_budget,
            corr,
            &mut meta_sink,
        )?;
        return Ok(Some(WherePredicate::CorrelatedExists(resolved.probe)));
    };
    let mut meta_sink: Option<ColumnMeta> = None;
    let table = match resolve_correlated_scalar_aggregate(
        validated,
        inner_schema,
        correlation,
        outer_scopes,
        read_txn,
        ctx,
        lookup,
        udfs,
        group_limits,
        nesting_limits,
        result_limits,
        budget,
        in_value_budget,
        corr,
        &mut meta_sink,
    ) {
        Ok(t) => t,
        Err(e) => {
            // 遅延対象エラーのときだけ、同じ述語の静的エラー（対象列・値族）を優先する
            // （[`resolve_scalar_compare`] と同じ契約。Issue #1432）。
            if is_deferrable_runtime_error(&e) {
                let (_, target_family) = scalar_target_column(column, outer_scopes)?;
                if let Some(meta) = &meta_sink {
                    check_scalar_family_compatible(column, meta, target_family)?;
                }
            }
            return Err(e);
        }
    };
    let (target_ty, family) = scalar_target_column(column, outer_scopes)?;
    check_scalar_family_compatible(column, &table.meta, family)?;

    let outer_schema = owner_schema(outer_scopes)?;
    let ScalarAggTable {
        values,
        empty,
        keys,
        ..
    } = table;
    let mut builder = ScalarPredicateBuilder {
        op,
        column,
        target_ty,
        family,
        outer_schema,
        udfs,
        byte_cap: corr.byte_cap(),
        used: 0,
        predicates: Vec::new(),
        index_by_value: std::collections::HashMap::new(),
    };
    let empty_set = builder.index_for(&empty)?;
    let mut by_key: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
    by_key
        .try_reserve(values.len())
        .map_err(|_| super::join::semi::too_large())?;
    for (key, cell) in values {
        let index = builder.index_for(&cell)?;
        by_key.insert(key, index);
    }
    let target_column = outer_schema.columns.iter().position(|c| c.name == column);
    let probe = CorrelatedProbe::scalar_compare(
        keys,
        ScalarTable {
            by_key,
            predicates: builder.predicates,
            empty_set,
        },
        target_column,
        corr.budget(),
    );
    Ok(Some(WherePredicate::CorrelatedExists(
        CorrelatedProbeRef::new(probe),
    )))
}

/// 失敗時の静的検証（Issue #1432 の経路）での WHERE 位置の集計形の相関スカラー。内側が集計形で
/// 外側を参照するときは、実行せずに [`prepare_correlated_scalar_aggregate`] と内側だけの連言の
/// 静的検証・束縛、対象列・値族の検査までを行い `true` を返す（実行回数予算は消費しない）。
/// 相関でなければ `false`（従来の静的検証へ）。
#[allow(clippy::too_many_arguments)]
fn static_validate_correlated_scalar(
    column: &str,
    inner_tokens: &[Token],
    depth: usize,
    outer_scopes: &[&TableSchema],
    read_txn: &impl crate::storage::read_source::ReadSource,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    nesting_limits: crate::nesting_limits::NestingLimits,
    result_limits: crate::result_limits::ResultLimits,
) -> Result<bool, SqlSurfaceError> {
    let Some((validated, inner_schema, correlation)) = classify_correlated_aggregate(
        inner_tokens,
        depth,
        outer_scopes,
        read_txn,
        lookup,
        nesting_limits,
    )?
    else {
        // 非集計形（Issue #1847）: 実行経路と同じ準備と、内側だけの連言の静的検証・束縛、
        // 対象列と値族の検査までを行う（実行はしない）。相関でなければ `false`。
        let Some((validated, inner_schema, correlation)) = classify_correlated_scan(
            inner_tokens,
            depth,
            outer_scopes,
            read_txn,
            lookup,
            nesting_limits,
        )?
        else {
            return Ok(false);
        };
        let mut prepared = prepare_correlated_exists(
            validated,
            &inner_schema,
            &correlation,
            &CorrelatedShape::Scalar,
            outer_scopes,
            udfs,
            result_limits,
        )?;
        let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
        chain.push(&inner_schema);
        static_validate_subquery_predicates(
            &prepared.analysis.inner_only,
            &chain,
            read_txn,
            lookup,
            udfs,
            nesting_limits,
            result_limits,
        )?;
        prepared.validated.where_predicates =
            static_validation_predicates(&prepared.analysis.inner_only);
        shape_for_materialization(&mut prepared.validated, &prepared.projected);
        super::parser::bind_scan_with_dummy_flags(
            &prepared.validated,
            &inner_schema,
            udfs,
            &[],
            &result_limits,
        )?;
        let meta = prepared
            .scalar_meta
            .as_ref()
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "correlated scalar subquery is missing its column metadata".to_string(),
            })?;
        let (_, target_family) = scalar_target_column(column, outer_scopes)?;
        check_scalar_family_compatible(column, meta, target_family)?;
        return Ok(true);
    };
    let prepared =
        prepare_correlated_scalar_aggregate(validated, &inner_schema, &correlation, outer_scopes)?;
    let mut chain: Vec<&TableSchema> = outer_scopes.to_vec();
    chain.push(&inner_schema);
    static_validate_subquery_predicates(
        &prepared.plain.where_predicates,
        &chain,
        read_txn,
        lookup,
        udfs,
        nesting_limits,
        result_limits,
    )?;
    let bound = bind_scalar_aggregate(
        &prepared,
        static_validation_predicates(&prepared.plain.where_predicates),
        &inner_schema,
        udfs,
    )?;
    let (_, target_family) = scalar_target_column(column, outer_scopes)?;
    check_scalar_family_compatible(column, &bound.meta, target_family)?;
    Ok(true)
}

/// 投影位置の相関スカラー 1 項目の値表。外側の行ごとに、隠し列（疑似列 `id` は行の `id`）から
/// 値を引き当てる。可視行由来の値を持つため `Debug` は行データを出さない。
#[derive(Clone)]
pub(crate) enum CorrelatedScalarValues {
    /// 集計形（Issue #1846）。隠しキー列から相関キーを作ってグループの値を引く。
    Agg {
        values: std::sync::Arc<std::collections::HashMap<Vec<u8>, Cell>>,
        empty: Cell,
        keys: Vec<super::join::semi::OuterKey>,
        /// キーごとの外側の列名（疑似列 `id` は `None`）。隠しキー列の位置引きに使う。
        key_names: Vec<Option<String>>,
    },
    /// 非集計形（Issue #1847）。隠し列の外側セルを行スカラーへ写し、プローブの基数判定
    /// （0 件は NULL・1 件はその値・2 件以上は `21000`）を呼ぶ。
    Rows {
        probe: super::join::semi::CorrelatedProbeRef,
        /// `(外側スキーマ上の論理列インデックス, 列名, 列型)`。プローブがデコードを要する外側列。
        outer: std::sync::Arc<Vec<(usize, String, ColumnType)>>,
        /// 外側スキーマの列数（行スカラービューの長さ）。
        width: usize,
    },
}

impl std::fmt::Debug for CorrelatedScalarValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CorrelatedScalarValues(..)")
    }
}

/// 隠しキー列の出力名（利用者の識別子・出力別名と衝突しない制御文字接頭辞。利用者へは返さない）。
fn hidden_key_alias(column: &str) -> String {
    format!("\u{3}corrkey.{column}")
}

/// 解決済みの相関項目が要求する外側列の名前（重複なし・初出順。疑似列 `id` は含めない）。
pub(crate) fn hidden_key_columns(items: &[ResolvedScalarItem]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let names: Vec<&String> = match &item.correlated {
            Some(CorrelatedScalarValues::Agg { key_names, .. }) => {
                key_names.iter().flatten().collect()
            }
            Some(CorrelatedScalarValues::Rows { outer, .. }) => {
                outer.iter().map(|(_, name, _)| name).collect()
            }
            None => Vec::new(),
        };
        for name in names {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

/// 外側の走査へ、相関項目が要求する隠し列を投影の末尾へ足す（元の投影位置・`ORDER BY` の
/// 序数は変わらない）。`*` との併用は疑似列 `id` 以外の列を持てないため `42601`（fail-closed）。
/// 隠し列は [`merge_scalar_projection_items`] が値の引き当てに使った後に取り除く。
pub(crate) fn append_hidden_key_columns(
    validated: &mut ValidatedScan,
    hidden: &[String],
) -> Result<(), SqlSurfaceError> {
    if hidden.is_empty() {
        return Ok(());
    }
    let mut items: Vec<SelectItem> =
        match std::mem::replace(&mut validated.projection, Projection::Columns(Vec::new())) {
            Projection::All => return Err(correlated_scalar_unsupported()),
            Projection::Columns(names) => names.into_iter().map(SelectItem::Column).collect(),
            Projection::Items(items) => items,
        };
    for name in hidden {
        items.push(SelectItem::Expr {
            expr: Expr::Ident(name.clone()),
            alias: Some(hidden_key_alias(name)),
        });
    }
    validated.projection = Projection::Items(items);
    Ok(())
}

impl CorrelatedScalarValues {
    /// 外側の 1 行の値（該当が無い・キーが NULL なら空集合の値＝集計形、NULL＝非集計形）。`cells` は
    /// 隠し列を含む外側の行セル、`hidden_base` は隠し列の開始位置、`hidden` は
    /// [`hidden_key_columns`] の結果。非集計形で一致行が 2 件以上なら `21000`。
    fn value_for(
        &self,
        row_id: u64,
        cells: &[Cell],
        hidden_base: usize,
        hidden: &[String],
    ) -> Result<Cell, SqlSurfaceError> {
        let hidden_cell = |name: &str| -> Result<&Cell, SqlSurfaceError> {
            let pos = hidden
                .iter()
                .position(|h| h == name)
                .and_then(|p| hidden_base.checked_add(p))
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated scalar hidden key column is missing".to_string(),
                })?;
            cells.get(pos).ok_or_else(|| SqlSurfaceError::Internal {
                detail: "correlated scalar hidden key cell is missing".to_string(),
            })
        };
        match self {
            Self::Agg {
                values,
                empty,
                keys,
                key_names,
            } => {
                let mut key = Vec::new();
                for (k, name) in keys.iter().zip(key_names) {
                    let cell = match name {
                        None => Cell::Integer(row_id),
                        Some(name) => hidden_cell(name)?.clone(),
                    };
                    if matches!(cell, Cell::Null) {
                        return Ok(empty.clone());
                    }
                    super::join::values::encode_key_component(&cell, &k.class, &mut key)?;
                }
                Ok(values.get(&key).unwrap_or(empty).clone())
            }
            Self::Rows {
                probe,
                outer,
                width,
            } => {
                let mut scalars: Vec<Option<crate::row_codec::ScalarRef<'_>>> = Vec::new();
                scalars.try_reserve(*width).map_err(|_| merge_too_large())?;
                scalars.resize(*width, None);
                for (index, name, ty) in outer.iter() {
                    let cell = hidden_cell(name)?;
                    let slot =
                        scalars
                            .get_mut(*index)
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "correlated scalar outer column is outside the row view"
                                    .to_string(),
                            })?;
                    *slot = super::join::values::cell_scalar(cell, ty)?;
                }
                Ok(probe
                    .probe()
                    .scalar_value(&scalars, row_id)?
                    .cloned()
                    .unwrap_or(Cell::Null))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #1517: 他定数を参照しない独立定数としての現行値を固定する
    /// （SQL-34・SQL-46）。値を変える場合は設定化 Issue（#1549／#1555 等）で
    /// 意図的に変更すること。
    #[test]
    fn max_subquery_in_values_is_fixed_at_current_default() {
        assert_eq!(MAX_SUBQUERY_IN_VALUES, 10_000);
    }
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
            crate::group_by_limits::GroupByLimits::default(),
            crate::nesting_limits::NestingLimits::default(),
            crate::result_limits::ResultLimits::default(),
            &mut budget,
            &mut in_value_budget,
            &CorrelatedCtx::disabled(),
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
            crate::group_by_limits::GroupByLimits::default(),
            crate::nesting_limits::NestingLimits::default(),
            crate::result_limits::ResultLimits::default(),
            &mut budget,
            &mut in_value_budget,
            &CorrelatedCtx::disabled(),
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
    /// Issue #1855・SQL-61 (3): 量化比較の境界値選びに使う比較器は、同じ値族の順序を正しく
    /// 与え、型の食い違うセルは `Equal` に落とさず `Err` にする（fail-closed）。
    #[test]
    fn cmp_quantified_cells_orders_and_rejects_mismatches() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let dec =
            |u: i128, sc: u8| Cell::Numeric(crate::numeric::Decimal::from_parts(u, sc).unwrap());
        let t = |s: &str| Cell::Text(s.to_string());
        assert_eq!(cmp_quantified_cells(&t("a"), &t("b")).unwrap(), Less);
        assert_eq!(
            cmp_quantified_cells(&Cell::Bool(true), &Cell::Bool(false)).unwrap(),
            Greater
        );
        // scale 違いの numeric は値で比べる（`1.5` と `1.50` は等価）。
        assert_eq!(
            cmp_quantified_cells(&dec(15, 1), &dec(150, 2)).unwrap(),
            Equal
        );
        // 整数と numeric は厳密に比べる。
        assert_eq!(
            cmp_quantified_cells(&Cell::SignedInteger(2), &dec(25, 1)).unwrap(),
            Less
        );
        assert_eq!(
            cmp_quantified_cells(&Cell::Integer(u64::MAX), &Cell::SignedInteger(-1)).unwrap(),
            Greater
        );
        // `-0.0` と `+0.0` は等価。
        assert_eq!(
            cmp_quantified_cells(&Cell::Float(-0.0), &Cell::Float(0.0)).unwrap(),
            Equal
        );
        // 型の食い違い・非有限は `Err`。
        assert!(cmp_quantified_cells(&t("a"), &Cell::SignedInteger(1)).is_err());
        assert!(cmp_quantified_cells(&Cell::Float(f64::NAN), &Cell::Float(1.0)).is_err());
    }
}
