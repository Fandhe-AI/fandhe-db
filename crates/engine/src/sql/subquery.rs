//! WHERE 句のサブクエリ（`IN (SELECT ...)`・`EXISTS (SELECT ...)`）を実行前に
//! 解決する（Issue #927・SQL-29 (a)・RLS-10 (b)・TASK-213）。
//!
//! 責務境界: `sql::allowlist::Parser` が構文的に受理した
//! [`WherePredicate::InSubquery`]／[`WherePredicate::Exists`]（内側の生
//! トークン列を保持するのみで未評価）を、`core.rs` の `Statement::Scan`／
//! `Statement::Aggregate` 実行アームが束縛（`sql::parser::bind_scan`／
//! `bind_aggregate`）の**前**に本モジュールへ渡す。内側は外側と同じ
//! `PolicyContext`（RLS 暗黙適用）・同じ `read_txn`（同一スナップショット）で
//! `sql::allowlist::validate_sql_tokens_with_subquery_ctx` → `sql::parser::
//! bind_scan` → `sql::scan::execute_scan` という、通常の広域取得 SELECT と
//! 完全に同じ経路で実行し、結果を具体的な `WherePredicate`（`Or`／
//! `Equality`／`BoolEquality`）へ書き換える。これにより `sql::where_tree`・
//! `sql::parser::bind_where_predicates` は一切変更せず、サブクエリを含まない
//! 文と同じ評価器を再利用する（CLAUDE.md「委譲方針」＝第 2 の評価器を作らない）。
//!
//! 対応する内側の形は `SELECT <単一列> FROM <table> [WHERE ...] LIMIT <n>`
//! （広域取得＝[`Statement::Scan`]）のみ。ランキング付き検索 SELECT・集計
//! （`GROUP BY`）を内側に書く形・内側の `LIMIT` 省略・相関参照（外側の列を
//! 内側から参照する形）は本 Issue のスコープ外で `42601`／`22000` にする
//! （`docs/design/sql-subquery.md` 参照）。相関の検出は専用の構造解析を持たず、
//! 内側の束縛（`bind_scan`）が「内側テーブルのスキーマに存在しない列」として
//! 既存の `22000`（unknown column）へ落とすことに委ねる（内側は常に自分の
//! FROM テーブルのスキーマのみで束縛されるため、外側の列を参照しても
//! 構造的に解決できない。fail-closed）。
//!
//! DoS 対策（security.md「不安全な設計｜無制限リソース確保」対応）:
//! - ネスト深さは構文解析段（[`super::allowlist::MAX_SUBQUERY_DEPTH`]・
//!   `Parser::require_subquery_depth`）が担う。
//! - 1 文（トップレベル実行 1 回）あたりの内側クエリ実行回数は本モジュールの
//!   [`MAX_SUBQUERY_EXECUTIONS`] で頭打ちにする（`budget` を呼び出し階層全体で
//!   共有する `&mut usize` として引き回す）。
//! - 内側の可視行数は [`crate::core::MAX_SEARCH_K`] を超えたら `54000`
//!   （内側の `LIMIT` 自体の範囲検証は `bind_scan` の既存契約に委ねた上での
//!   追加の防御的上限）。
//! - `IN (SELECT ...)` は内側の distinct 値（TEXT／ENUM 対象）を
//!   [`crate::declarative_filter::MAX_IN_LIST_ITEMS`] 件以下のチャンクへ分け、
//!   チャンクごとの `WherePredicate::InList`（既存評価器の集合照合＝ソート済み
//!   `Vec` の二分探索）を `Or` に束ねる。取り込める distinct 値の総数は
//!   [`MAX_SUBQUERY_IN_VALUES`]（文全体で共有する `&mut usize` 予算）で頭打ちに
//!   し、distinct 化の後・チャンク（`InList`）構築の前に一括で検査する。収集 Vec
//!   は内側結果行数 ≤ `MAX_SEARCH_K` で有界（Issue #1165。旧方式は
//!   値ごとの `Equality` 葉を 256 個までしか許さず、評価も葉数に比例した。
//!   PR #1103 codex-review P1 指摘対応の趣旨〔サブクエリ経由の展開が通常の
//!   `WHERE` 句より大きな評価コストを発生させない〕は、評価コストを分岐数
//!   ×二分探索へ有界化することで維持する）。BOOLEAN 対象は distinct が高々 2
//!   のため従来どおり `BoolEquality` の `Or` とする。
//! - `EXISTS (SELECT ...)` は可視行が 1 件以上存在するかどうかしか使わない
//!   ため、[`InnerScanIntent::ExistenceOnly`] で内側を実質 `LIMIT 1`・投影
//!   不要へ差し替えて評価する（`WHERE`・RLS の適用は変更しない＝可視性判定
//!   を迂回しない。PR #1103 追加 codex-review P1 指摘対応: 以前はユーザー
//!   指定の投影・`LIMIT` をそのまま使っていたため、可視行があっても
//!   `SELECT *` 等の広い投影×大きい `LIMIT` の組合せで `execute_scan` の
//!   結果バイト上限に達し `EXISTS` 文全体が失敗しえた）。
//! - `IN (SELECT ...)` の投影列数（ちょうど 1 列である契約）も、実行結果
//!   からではなく `validated.projection`／内側スキーマから実行前に静的検証
//!   する（PR #1103 追加 codex-review P1 指摘の自己点検で発見: 上記
//!   `EXISTS` の修正前と同種の「必要以上の投影で走査コストを払ってから
//!   拒否する」問題が `IN` 側にも存在した。`SELECT *` 等の不正な内側クエリ
//!   を、束縛・全件走査より前に `42601` で拒否する）。
//! - 内側にウィンドウ関数（`ValidatedScan::window_items`。SQL-30・TASK-214、
//!   Issue #930）が含まれる場合は `IN`／`EXISTS` いずれも `42601` で一律
//!   拒否する（PR #1103 Cursor Bugbot 指摘対応: `window_items` が非空だと
//!   `sql::scan::execute_scan` は `sql::window::execute_window_scan` へ
//!   分岐し、`LIMIT` による早期終了なしに可視行を全件 materialize する
//!   契約〔`docs/design/window-functions.md` 参照〕。`EXISTS` 側の
//!   [`InnerScanIntent::ExistenceOnly`] は投影・`LIMIT` だけを差し替える
//!   ため、ウィンドウ項目を差し替えずに残すと同じ資源上限問題が
//!   ウィンドウ関数経由で再発する。サブクエリとウィンドウ関数の組合せは
//!   設計上未検証のため、正しく動く保証を作り込むのではなく fail-closed に
//!   倒す。`docs/design/sql-subquery.md` の対象外節に明記する）。

use super::allowlist::{Statement, TableLookup, WherePredicate};
use super::exec::{Cell, ColumnMeta};
use super::lexer::Token;
use super::udf_call::UdfRegistry;
use crate::catalog::{ColumnType, TableSchema};
use crate::policy::PolicyContext;

/// 1 文（トップレベル実行 1 回）あたりに実行できる内側サブクエリの総数
/// （実装既定値。Issue #927・TASK-213。ネスト深さ上限
/// [`super::allowlist::MAX_SUBQUERY_DEPTH`] と組み合わせて評価コストを
/// 有界にする）。
pub(crate) const MAX_SUBQUERY_EXECUTIONS: usize = 16;

/// 1 文（トップレベル実行 1 回。ネストしたサブクエリを含む）あたりに
/// `IN (SELECT ...)` の集合照合へ取り込める distinct 値の総数（実装既定値。
/// Issue #1165・SQL-29・TASK-213。PR #1103 codex-review P1 指摘対応で導入した
/// 旧「葉数」予算〔256〕の見直し）。
///
/// 値は [`crate::declarative_filter::MAX_IN_LIST_ITEMS`] 件ずつのチャンクへ
/// 分割し、各チャンクを既存の `InList`（束縛時にソート・重複除去済みの
/// `FilterOp::InText`、評価は二分探索）へ束縛するため、1 行あたりの評価コストは
/// 「分岐数 × O(log 256)」で、旧方式（distinct 値ごとの `Equality` 葉を線形に
/// 走査）の O(値数) より小さい。内側 1 回の可視行数は既に
/// [`crate::core::MAX_SEARCH_K`] で頭打ちのため、単一の `IN` サブクエリは
/// この上限に到達しない（複数の `IN` の distinct 合計のみが上限に効く）。
pub(crate) const MAX_SUBQUERY_IN_VALUES: usize = crate::core::MAX_SEARCH_K;

/// `where_predicates`（トップレベルの述語列。`WherePredicate::Or` の分岐も
/// 再帰的に辿る）に含まれる `InSubquery`／`Exists` をすべて解決し、具体的な
/// `WherePredicate` へ書き換えた新しい述語列を返す。`core.rs` の
/// `Statement::Scan`／`Statement::Aggregate` 実行アームが、束縛
/// （`bind_scan`／`bind_aggregate`）の直前に呼ぶ。
///
/// `outer_schema` は `predicates` が参照する列（この呼び出しにとっての
/// 「外側」＝これから束縛される文自身のテーブル）のスキーマ。`IN (SELECT
/// ...)` の対象列の存在・型検証（[`validate_in_target_column`]）に使う
/// （PR #1103 codex-review P1 指摘対応: 内側の結果行数に関わらず必ず検証する。
/// ネストしたサブクエリを解決する再帰呼び出し〔[`execute_inner_scan`]〕では、
/// その内側クエリ自身のスキーマを渡す）。
///
/// `budget` は呼び出し階層全体（ネストしたサブクエリを含む）で共有する残り
/// 実行回数。呼び出し元は [`MAX_SUBQUERY_EXECUTIONS`] で初期化する。
/// `in_value_budget` も同様に呼び出し階層全体で共有する、`IN` 展開で生成
/// できる残り distinct 値数。呼び出し元は [`MAX_SUBQUERY_IN_VALUES`] で初期化する。
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_where_predicates(
    predicates: Vec<WherePredicate>,
    outer_schema: &TableSchema,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<Vec<WherePredicate>, crate::sql::allowlist::SqlSurfaceError> {
    let mut out = Vec::with_capacity(predicates.len());
    for predicate in predicates {
        match predicate {
            WherePredicate::Or(branches) => {
                let mut resolved_branches = Vec::with_capacity(branches.len());
                for branch in branches {
                    resolved_branches.push(resolve_where_predicates(
                        branch,
                        outer_schema,
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
                let resolved = resolve_in_subquery(
                    &column,
                    &inner_tokens,
                    depth,
                    outer_schema,
                    read_txn,
                    ctx,
                    lookup,
                    udfs,
                    budget,
                    in_value_budget,
                )?;
                out.push(resolved);
            }
            WherePredicate::Exists {
                inner_tokens,
                depth,
            } => {
                let exists = resolve_exists_subquery(
                    &inner_tokens,
                    depth,
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
            other => out.push(other),
        }
    }
    Ok(out)
}

/// [`execute_inner_scan`] が内側クエリをどう評価するかを表す（PR #1103 追加
/// codex-review P1 指摘対応: `IN`／`EXISTS` で必要な情報量が異なるため、
/// 同じ実行経路（`bind_scan` → `execute_scan`）を共有しつつ束縛直前の
/// `ValidatedScan` だけを使い分ける）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InnerScanIntent {
    /// `IN (SELECT ...)`: 内側の投影値そのものが必要なため、ユーザー指定の
    /// 投影・`LIMIT` をそのまま使う。
    Values,
    /// `EXISTS (SELECT ...)`: 可視行が 1 件以上存在するかどうかしか使わない。
    /// 投影を空へ、`LIMIT` を実質 1 へ差し替えて評価する（`ValidatedScan::
    /// limit` のドキュメントが説明する早期終了を利用し、先頭の該当行を
    /// 確認した時点で走査を打ち切る）。
    ExistenceOnly,
}

/// 内側トークン列を検証・実行し、`sql::allowlist::Statement::Scan` として
/// 妥当であることを確認した上で、束縛前の残りの解決（自身の WHERE に含まれる
/// さらに深いサブクエリ）を行い、`sql::scan::execute_scan` で実行する。
///
/// 自身の WHERE に含まれるさらに深いサブクエリを解決する際の「外側スキーマ」
/// （[`resolve_where_predicates`] の `outer_schema`）は、この内側クエリ自身の
/// テーブルのスキーマになる（相対的に見て、そのネスト位置での「外側」は
/// このクエリ自身）。そのため `inner_schema` の取得を `bind_scan` 呼び出しより
/// 前へ移し、ネストした `resolve_where_predicates` 呼び出しへ渡す
/// （PR #1103 codex-review P1 指摘対応）。
///
/// `intent` が [`InnerScanIntent::ExistenceOnly`] の場合、`WHERE`・RLS の
/// 適用（`where_predicates`・`ctx`）は通常の内側評価と完全に同一のまま、
/// 投影・`LIMIT` のみを可視性判定に不要な形へ差し替える（PR #1103 追加
/// codex-review P1 指摘対応: 以前は `EXISTS` もユーザー指定の投影・`LIMIT`
/// をそのまま使っていたため、可視行があっても `SELECT *` 等の広い投影×
/// 大きい `LIMIT` の組合せで `execute_scan` の結果バイト上限に達し、
/// `EXISTS` 文全体が失敗しえた）。
#[allow(clippy::too_many_arguments)]
fn execute_inner_scan(
    inner_tokens: &[Token],
    depth: usize,
    intent: InnerScanIntent,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<super::exec::QueryResult, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    *budget = budget.checked_sub(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large(format!(
            "subquery execution count exceeds limit {MAX_SUBQUERY_EXECUTIONS}"
        ))
    })?;

    let stmt =
        super::allowlist::validate_sql_tokens_with_subquery_ctx(inner_tokens, lookup, depth)?;
    let mut validated = match stmt {
        Statement::Scan(validated) => validated,
        _ => {
            return Err(SqlSurfaceError::unsupported(
                "subquery must be a plain SELECT ... FROM ... [WHERE ...] LIMIT n \
                 (no ORDER BY / HYBRID / USING PLAN / GROUP BY)",
            ))
        }
    };

    // Cursor Bugbot 指摘対応: ウィンドウ関数（SQL-30・TASK-214、Issue #930）を
    // 含む内側は `IN`／`EXISTS` いずれも一律拒否する。`window_items` が非空だと
    // `sql::scan::execute_scan` は `sql::window::execute_window_scan` へ分岐し、
    // `LIMIT` による早期終了なしに可視行を全件 materialize する契約
    // （`docs/design/window-functions.md` 参照）。`EXISTS` の
    // `InnerScanIntent::ExistenceOnly` は投影・`LIMIT` だけを可視性判定に
    // 不要な形へ差し替える設計であり、ウィンドウ項目をそのまま残すと同じ
    // 資源上限問題（PR #1103 codex-review P1 指摘対応で一度塞いだもの）が
    // ウィンドウ関数経由で再発する。サブクエリとウィンドウ関数の組合せは
    // 設計上未検証のため、正しく動く経路を作り込まず fail-closed に倒す
    // （`docs/design/sql-subquery.md` 対象外節参照）。
    if !validated.window_items.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "subquery cannot contain window functions",
        ));
    }

    let inner_schema = crate::catalog::get_table_schema_in_txn(read_txn, &validated.table_name)
        .map_err(|e| match e {
            crate::catalog::CatalogError::TableNotFound(name) => {
                SqlSurfaceError::undefined_table(name)
            }
            other => crate::catalog::table_lookup_error(other),
        })?;

    if intent == InnerScanIntent::Values {
        // `IN (SELECT ...)` は投影列がちょうど 1 列であることを要求する
        // 契約（`resolve_in_subquery` の `result.columns.len() != 1` 検査）
        // だが、以前はこれを実行結果からしか検査しておらず、`SELECT *` や
        // 複数列を投影する不正な内側クエリでも、束縛・全件走査（ネストした
        // サブクエリの解決・`bind_scan`・`execute_scan`）を最後まで終えて
        // からようやく拒否していた（PR #1103 追加 codex-review P1 指摘の
        // 自己点検: `EXISTS` と同種の「必要以上の投影で走査コストを払って
        // から拒否する」問題が `IN` 側にも存在した）。`validated.projection`
        // と `inner_schema` から投影列数を実行前に静的に確定できるため、
        // ここで先に検査し、1 列でなければ実行前に `42601` で拒否する
        // （`resolve_in_subquery` 側の実行後チェックは、この静的検査が
        // 想定しない構成を取りこぼさないための多層防御として残す）。
        let projected_len = match &validated.projection {
            super::allowlist::Projection::All => 1 + inner_schema.columns.len(),
            super::allowlist::Projection::Columns(names) => names.len(),
            super::allowlist::Projection::Items(items) => items.len(),
        };
        if projected_len != 1 {
            return Err(SqlSurfaceError::unsupported(
                "subquery used with IN must select exactly one column",
            ));
        }
    }

    // 自身の WHERE に含まれるさらに深いサブクエリを、束縛（`bind_scan`）の前に
    // 解決する（深さ優先。`depth` は構文解析段で `MAX_SUBQUERY_DEPTH` 検査
    // 済みのため、ここでは budget のみ検査すれば足りる）。`outer_schema` には
    // このクエリ自身の `inner_schema` を渡す（このネスト位置での「外側」）。
    validated.where_predicates = resolve_where_predicates(
        validated.where_predicates,
        &inner_schema,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_value_budget,
    )?;

    if intent == InnerScanIntent::ExistenceOnly {
        // ユーザー指定の `LIMIT` 自体の範囲検証（`bind_scan` が本来行う契約）
        // は、これから使う値を 1 へ差し替えても迂回されないよう、差し替え前の
        // 元の値に対して明示的に検証しておく（fail-closed。範囲外の `LIMIT`
        // を指定した `EXISTS` が、値を使わないという理由だけで通ってしまう
        // ことを防ぐ）。`OFFSET` は可視性判定に意味を持つため変更しない
        // （`LIMIT` の範囲が 1 以上である契約と合わせ、`LIMIT` を 1 に
        // 差し替えても「`OFFSET` 分だけ読み飛ばした後に可視行が 1 件以上
        // あるか」という元の意味論と同値になる）。
        super::parser::validate_search_limit(validated.limit)?;
        // ユーザー指定の投影も、実行用に空へ差し替える前に必ず束縛検証する
        // （PR #1103 追加 codex-review P1 指摘対応: 検証前に入力を差し替え
        // ない。差し替えを先に行うと、`EXISTS (SELECT missing_column FROM
        // ... LIMIT 1)` のような存在しない列・非対応の投影を指定した内側
        // クエリが、実際には使わないという理由だけで列検証をすり抜け、
        // 可視行の有無だけで成否が決まってしまう＝列検証・エラー契約を
        // 破ってしまう）。ここでの束縛結果自体は使わない（`bind_projection`
        // が返す `Result` のエラーだけを見る）ため、専用の `node_budget` を
        // 新規に用意する（式項目のノード数上限 `MAX_EXPR_NODES` は呼び出し
        // ごとに独立というのが `bind_scan` 等の既存契約であり、ここでも
        // 同じ契約を保つ）。
        let mut probe_node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
        super::parser::bind_projection(
            &validated.projection,
            &inner_schema,
            udfs,
            &mut probe_node_budget,
        )?;
        validated.projection = super::allowlist::Projection::Columns(Vec::new());
        validated.limit = 1;
    }

    let bound = super::parser::bind_scan(&validated, &inner_schema, udfs)?;
    let result = super::scan::execute_scan(read_txn, ctx, &inner_schema, &bound)?;

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

/// `<column> IN (SELECT ...)` の対象列 `column` を `outer_schema`（この
/// サブクエリを含む文自身のテーブルのスキーマ）に対して検証する。存在しない
/// 列は `22000`（`unknown column`。通常の `WHERE` 等価述語束縛
/// ［`crate::declarative_filter::DeclarativeFilter::bind`］と同じ文言・
/// `wire_code`）、存在しても [`WherePredicate::Equality`]／
/// [`WherePredicate::BoolEquality`] のいずれも束縛できない型（`TEXT`／
/// `ENUM`／`BOOLEAN` 以外）は同じく `22000` で拒否する。
///
/// この検証は内側サブクエリの結果行数（0 行・NULL のみを含む）に一切
/// 依存しない（PR #1103 codex-review P1 指摘対応: 従来は内側の結果行から
/// 変換された葉が実際に束縛される時点でしか列名・型検証が働かず、内側が
/// 0 行／NULL のみの場合は空の `Or`〔常に偽〕へ静かに置き換わり、存在しない
/// 列・非対応型の列を指定しても列名・型検証を回避したまま「空結果で成功」
/// してしまっていた）。
fn validate_in_target_column<'a>(
    column: &str,
    outer_schema: &'a TableSchema,
) -> Result<&'a ColumnType, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    let column_def = outer_schema
        .columns
        .iter()
        .find(|c| c.name == column)
        .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {column}")))?;
    match &column_def.ty {
        ColumnType::Text | ColumnType::Enum(_) | ColumnType::Boolean => Ok(&column_def.ty),
        _ => Err(SqlSurfaceError::invalid_input(format!(
            "column {column:?} is not a TEXT/ENUM/BOOLEAN column (subquery IN target)"
        ))),
    }
}

/// `IN` の対象列（外側）・内側の単一投影列が、同じ「値族」（`TEXT`／`ENUM`
/// はいずれも文字列ラベルとして比較するため同族、`BOOLEAN` は別族）に
/// 属するかどうかを表す（PR #1103 追加 codex-review P1 指摘対応）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubqueryValueFamily {
    /// `TEXT`／`ENUM` 列（ラベルを文字列として比較する）。
    Text,
    /// `BOOLEAN` 列。
    Boolean,
}

/// `outer_ty`（[`validate_in_target_column`] が返す対象列の型。常に
/// `TEXT`／`ENUM`／`BOOLEAN` のいずれか）の値族を返す。
fn outer_value_family(outer_ty: &ColumnType) -> SubqueryValueFamily {
    match outer_ty {
        ColumnType::Text | ColumnType::Enum(_) => SubqueryValueFamily::Text,
        ColumnType::Boolean => SubqueryValueFamily::Boolean,
        // `validate_in_target_column` が事前にこの 3 種類だけを許可するため、
        // ここへは到達しない（防御的に fail-closed 側＝`None` 扱いへ倒す）。
        _ => SubqueryValueFamily::Text,
    }
}

/// 内側の単一投影列の静的メタデータ（[`ColumnMeta`]）から値族を判定する。
/// `None` は「対応外（値族が不明、または `TEXT`／`ENUM`／`BOOLEAN` のいずれ
/// でもない）」を表し、呼び出し元は `22000` で拒否する。
///
/// 疑似列 `id`（[`ColumnMeta::Id`]。実行結果は常に `Cell::Integer`）は
/// `None` を返す（PR #1103 追加 codex-review P1 指摘対応の回帰対象:
/// 以前は `Cell::Integer` を素通しで文字列化して `WherePredicate::Equality`
/// へ変換していたため、`<TEXT 列> IN (SELECT id FROM ...)` で `id` の文字列
/// 表現と偶然一致する `TEXT` 値が誤って一致してしまっていた——型の異なる
/// 値の暗黙同一視。内側の結果行数に関わらず、この静的メタデータの時点で
/// 判定を確定させる）。式項目（[`ColumnMeta::Computed`]）も静的な型情報を
/// 持たないため同様に `None`（実装既定値のスコープ外。`docs/design/
/// sql-subquery.md` 参照）。
fn inner_value_family(meta: &ColumnMeta) -> Option<SubqueryValueFamily> {
    match meta {
        ColumnMeta::Scalar {
            ty: ColumnType::Text | ColumnType::Enum(_),
            ..
        } => Some(SubqueryValueFamily::Text),
        ColumnMeta::Scalar {
            ty: ColumnType::Boolean,
            ..
        } => Some(SubqueryValueFamily::Boolean),
        ColumnMeta::Id | ColumnMeta::Scalar { .. } | ColumnMeta::Computed { .. } => None,
    }
}

/// `<column> IN (SELECT ...)` を解決する。内側は投影列がちょうど 1 列である
/// ことを要求し（`22000`）、[`validate_in_target_column`] で対象列
/// `column`（外側スキーマ）を検証した上で、内側の投影列の値族
/// （[`inner_value_family`]）が対象列の値族（[`outer_value_family`]）と
/// 一致することを検証する（内側の結果行数・値に一切依存しない静的な検証。
/// PR #1103 追加 codex-review P1 指摘対応）。適合を確認した上で、
/// TEXT／ENUM 対象は distinct 値を集めて [`build_in_set_predicate`] の
/// チャンク化 `InList` の `Or` へ、BOOLEAN 対象は `BoolEquality` の `Or` へ
/// 書き換える（0 行・NULL のみなら空の `Or` ＝常に偽）。
///
/// NULL・語彙外 ENUM ラベルの除外は重複除去より前に行い、いずれも予算を
/// 消費しない。`in_value_budget` は文全体で共有する残り distinct 値予算
/// （[`MAX_SUBQUERY_IN_VALUES`] 参照）で、枯渇したら `54000` で拒否する
/// （Issue #1165・SQL-29・TASK-213）。
#[allow(clippy::too_many_arguments)]
fn resolve_in_subquery(
    column: &str,
    inner_tokens: &[Token],
    depth: usize,
    outer_schema: &TableSchema,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<WherePredicate, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    let result = execute_inner_scan(
        inner_tokens,
        depth,
        InnerScanIntent::Values,
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
    let target_ty = validate_in_target_column(column, outer_schema)?;

    // 内側の結果行数・値に一切依存しない静的な組合せ検証（PR #1103 追加
    // codex-review P1 指摘対応）。`result.columns` は上で長さ 1 を確認済みの
    // ため `first()` は必ず `Some`（untrusted 入力経路のため `[0]` ではなく
    // `ok_or_else` で明示的に扱う）。
    let inner_meta = result
        .columns
        .first()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "subquery result missing projected column metadata".to_string(),
        })?;
    let outer_family = outer_value_family(target_ty);
    match inner_value_family(inner_meta) {
        Some(inner_family) if inner_family == outer_family => {}
        _ => {
            return Err(SqlSurfaceError::invalid_input(format!(
                "subquery projection type is not compatible with IN target column {column:?} \
                 (supported combinations: TEXT/ENUM subquery projection with TEXT/ENUM target, \
                 BOOLEAN subquery projection with BOOLEAN target)"
            )));
        }
    }

    match outer_family {
        SubqueryValueFamily::Text => {
            // NULL・語彙外 ENUM ラベルは照合から除き、予算も消費しない。
            let mut values: Vec<String> = Vec::new();
            for row in &result.rows {
                let cell = row.cells.first().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "subquery row missing projected cell".to_string(),
                })?;
                let value = match cell_to_equality_predicate(column, cell)? {
                    // NULL セルは照合から除く（`WherePredicate::InSubquery`
                    // ドキュメント・NULL の意味論参照）。
                    None => continue,
                    Some(WherePredicate::Equality { value, .. }) => value,
                    Some(_) => {
                        return Err(SqlSurfaceError::Internal {
                            detail:
                                "cell_to_equality_predicate returned an unexpected predicate variant"
                                    .to_string(),
                        });
                    }
                };
                // 対象列が ENUM の場合、語彙外ラベルは「その値には一致しない」
                // として除外する（除外しないと後段の束縛が 22P02 で文全体を
                // 落とす。PR #1103 追加 codex-review P1 指摘対応）。
                if let ColumnType::Enum(def) = target_ty {
                    if !def.contains(&value) {
                        continue;
                    }
                }
                values.push(value);
            }
            // 重複除去（IN は集合所属であり重複は結果に影響しない。Cursor
            // Bugbot 指摘対応）。予算は distinct 値単位で消費する。
            values.sort_unstable();
            values.dedup();
            *in_value_budget = in_value_budget.checked_sub(values.len()).ok_or_else(|| {
                SqlSurfaceError::payload_too_large(format!(
                    "subquery IN distinct value count exceeds limit {MAX_SUBQUERY_IN_VALUES}"
                ))
            })?;
            Ok(build_in_set_predicate(column, values))
        }
        SubqueryValueFamily::Boolean => {
            // distinct は高々 2（true／false）。
            let mut seen: std::collections::HashSet<bool> = std::collections::HashSet::new();
            let mut branches = Vec::with_capacity(2);
            for row in &result.rows {
                let cell = row.cells.first().ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "subquery row missing projected cell".to_string(),
                })?;
                match cell_to_equality_predicate(column, cell)? {
                    None => continue,
                    Some(leaf @ WherePredicate::BoolEquality { value, .. }) => {
                        if !seen.insert(value) {
                            continue;
                        }
                        *in_value_budget = in_value_budget.checked_sub(1).ok_or_else(|| {
                            SqlSurfaceError::payload_too_large(format!(
                                "subquery IN distinct value count exceeds limit \
                                 {MAX_SUBQUERY_IN_VALUES}"
                            ))
                        })?;
                        branches.push(vec![leaf]);
                    }
                    Some(_) => {
                        return Err(SqlSurfaceError::Internal {
                            detail:
                                "cell_to_equality_predicate returned an unexpected predicate variant"
                                    .to_string(),
                        });
                    }
                }
            }
            Ok(WherePredicate::Or(branches))
        }
    }
}

/// ソート・重複除去済みの distinct 値 `values`（TEXT／ENUM 対象列 `column`）を、
/// [`crate::declarative_filter::MAX_IN_LIST_ITEMS`] 件以下のチャンクごとの
/// `WherePredicate::InList` を分岐とする `Or` へ変換する（Issue #1165）。
///
/// 各チャンクは既存経路（`sql::parser::declarative_leaf_to_filter` →
/// `DeclarativeFilter::in_list` → `FilterOp::InText`）でソート済み集合の二分
/// 探索へ束縛されるため、第 2 の評価器は作らない。`bind` 側の
/// `MAX_IN_LIST_ITEMS` 検査（NoSQL 表層と共有する事前防御点）はチャンク幅で
/// 満たすので緩めない。常に `Or` で包む（1 チャンクでも）ことで、計画形状
/// （`scalar_plan` は `Or` を含む文を `PlainScan` に固定する）を内側データ量へ
/// 依存させない。0 件は分岐 0 個の `Or`（常に偽・`has_where_filters()` は真）。
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
/// 内側は [`InnerScanIntent::ExistenceOnly`] で評価する（投影不要・実質
/// `LIMIT` 1。PR #1103 追加 codex-review P1 指摘対応の詳細は
/// [`execute_inner_scan`] のドキュメント参照）。
#[allow(clippy::too_many_arguments)]
fn resolve_exists_subquery(
    inner_tokens: &[Token],
    depth: usize,
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    lookup: &impl TableLookup,
    udfs: &UdfRegistry,
    budget: &mut usize,
    in_value_budget: &mut usize,
) -> Result<bool, crate::sql::allowlist::SqlSurfaceError> {
    let result = execute_inner_scan(
        inner_tokens,
        depth,
        InnerScanIntent::ExistenceOnly,
        read_txn,
        ctx,
        lookup,
        udfs,
        budget,
        in_value_budget,
    )?;
    Ok(!result.rows.is_empty())
}

/// 内側の投影セル 1 件を `<column> = <値>` 相当の `WherePredicate` 葉へ変換する。
/// `NULL` は `Ok(None)`（呼び出し元が集合から除外する）。対応するのは `TEXT`
/// （`Cell::Text`）・`BOOLEAN`（`Cell::Bool`）のみ。
///
/// `INTEGER`/`BIGINT` 列（`Cell::SignedInteger`）は対象外とする（レビュー
/// 指摘対応。Issue #927 push 前 Review）。`WherePredicate::Equality` は
/// `TEXT`／`ENUM` 列専用で `INTEGER`/`BIGINT` 列を「TEXT 列でない」として
/// 拒否する契約であり（`sql::parser::bind_where_predicates_recursive`）、
/// `INTEGER`/`BIGINT` 列の等価比較自体がこのリポでは未実装（`レーン A`。
/// `sql::udf_call::bind_expr_in` が `INTEGER`/`BIGINT` 列参照を式評価から
/// 一律拒否する契約。`sql::check_constraint` モジュールコメント参照）。
/// 通常の `<col> = <整数リテラル>` も同じ理由で現状は受理されないため、
/// 本関数だけが先取りして対応する処置は取らず、既存の実装既定値の範囲外
/// （`22000`）として明示的に拒否する（`docs/design/sql-subquery.md` 参照）。
///
/// `Cell::Integer`（疑似列 `id`／`COUNT` 相当）も同じ理由で本関数の到達範囲
/// としては残すが対象外（このリポの既存 `WherePredicate::Equality` 束縛
/// 自体が疑似列 `id` を対象にしていないため。`tests/sql29_subquery.rs`
/// 参照）。それ以外（`VECTOR`・`DATE`・`TIMESTAMP`・`NUMERIC`・`UUID`・
/// `BYTEA`・配列・JSON・式評価の `Float`）も同様に `22000`（実装既定値の
/// スコープ外。`docs/design/sql-subquery.md` 参照）。
fn cell_to_equality_predicate(
    column: &str,
    cell: &Cell,
) -> Result<Option<WherePredicate>, crate::sql::allowlist::SqlSurfaceError> {
    use crate::sql::allowlist::SqlSurfaceError;

    match cell {
        Cell::Null => Ok(None),
        Cell::Text(s) => Ok(Some(WherePredicate::Equality {
            column: column.to_string(),
            value: s.clone(),
        })),
        Cell::Integer(n) => Ok(Some(WherePredicate::Equality {
            column: column.to_string(),
            value: n.to_string(),
        })),
        Cell::Bool(b) => Ok(Some(WherePredicate::BoolEquality {
            column: column.to_string(),
            value: *b,
        })),
        Cell::SignedInteger(_)
        | Cell::Vector(_)
        | Cell::Float(_)
        | Cell::Date(_)
        | Cell::Timestamp(_)
        | Cell::Array(_)
        | Cell::Bytes(_)
        | Cell::Json(_)
        | Cell::Numeric(_)
        | Cell::Uuid(_) => Err(SqlSurfaceError::invalid_input(
            "unsupported column type for subquery IN target (implementation scope: \
             TEXT / BOOLEAN / id only; INTEGER/BIGINT equality is not yet implemented)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::storage::{encode_row, RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

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

        let result = execute_inner_scan(
            &inner_tokens,
            0,
            InnerScanIntent::ExistenceOnly,
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

        let result = execute_inner_scan(
            &inner_tokens,
            0,
            InnerScanIntent::Values,
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
}
