//! 集合演算（`UNION`／`UNION ALL`／`INTERSECT`／`INTERSECT ALL`／`EXCEPT`／`EXCEPT ALL`）の束縛・実行本体
//! （SQL-29 (c)・RLS-10 (b)・TASK-213）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens` が構造検証した
//! [`crate::sql::allowlist::ValidatedSetOperation`]（構文木は
//! [`crate::sql::allowlist::SetTree`]。葉は単一テーブルの広域取得
//! [`crate::sql::allowlist::ValidatedScan`]（Issue #1191 で、括弧付き枝の枝内
//! `ORDER BY`／`LIMIT`／`OFFSET` と、集計形の枝
//! [`crate::sql::allowlist::ValidatedAggregate`] を追加。Issue #3270 で式射影の項目と、
//! `FROM` なしの定数枝 [`crate::sql::allowlist::SetConstBranch`]〔`sql::const_select` の束縛・実行を
//! 再利用。表を読まない〕を追加）に限定する——TASK-212（`JOIN`・複数
//! テーブル実行計画基盤）を前提にしない設計判断。TASK-213 は TASK-212 に後続
//! するが、本実装は各枝を独立した単一テーブル走査として扱うことで基盤の完成を
//! 待たずに導入する）を受け取り、`core.rs::EngineCore` の SQL 実行経路
//! （session-less `execute_sql`・セッション経由 `execute_validated_in_session`）
//! から呼ばれる。
//!
//! 各枝は既存の広域取得経路（[`crate::sql::scan::execute_scan_with_budget`]）を
//! そのまま通す。これにより RLS 暗黙適用（RLS-7 相当）・fail-closed のエラー契約を
//! 第 2 の実行器を作らずに継承する（各枝は呼び出しセッション自身の
//! [`crate::policy::PolicyContext`] で独立に評価されるため、他テナント行が
//! 中間結果・重複除去・`INTERSECT`／`EXCEPT` の判定・件数のいずれにも現れない。
//! RLS-10 (b)）。
//!
//! 文全体（全枝の走査・合成・行キー生成）で 1 つの累計バイト予算
//! （[`SetOpBudget`]。単一クエリの結果予算と同じ上限
//! [`crate::arena::MAX_ARENA_TOTAL_BYTES`]）を共有する（PR #1105 レビュー指摘
//! 対応: 各枝が独立に同予算を使い、最大枝数上限（実装既定値 16）分の結果を
//! `eval_tree` が同時に保持し得ると、単一クエリの結果予算をはるかに超える
//! 合計メモリを SQL 入力だけで確保できてしまう〔DoS〕ため）。
//!
//! 括弧なし末尾の `ORDER BY`（集合全体のソートキー。Issue #3266・#3267）は、本モジュールが出力列名・
//! 序数を左端の枝の列へ解決し（[`resolve_order_keys`]）、キー型・照合を検査して比較計画にし
//! （[`plan_set_order_by`]）、合成結果の確定後・`OFFSET`／`LIMIT` の直前に
//! [`crate::sql::cell_sort::sort_rows`]（安定ソート）で並べ替える。比較できない型・非バイト列順の
//! 照合は `0A000` で拒否する（未ソートの行や誤順の行を返さない fail-closed）。実行・Describe・
//! `EXPLAIN`・`DECLARE CURSOR` はいずれも [`prepare`]／[`describe_columns`] を通る。
//!
//! スコープ外（Issue #929 の対象外事項。詳細は PR 本文参照）:
//! - 明示トランザクション内での実行

use std::collections::{HashMap, HashSet};

use crate::catalog::{ColumnType, TableSchema};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{
    SetAggregateBranch, SetConstBranch, SetOperator, SetOrderKey, SetTree, SqlSurfaceError,
    ValidatedScan,
};
use crate::sql::cell_sort::{sort_rows, OrderClass, OrderKeyPlan};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::join::values::{cmp_class, CmpClass, CmpVal};
use crate::sql::order_value::NullPlacement;
use crate::sql::udf_call::UdfRegistry;

/// 重複除去を伴う演算（`UNION`／`INTERSECT`／`EXCEPT`）の行キー集合の基数上限
/// （実装既定値。`core::MAX_SEARCH_K` と同値）。超過は `54000`。合成結果の行数
/// 上限も同じ定数を共有する（重複除去後の行数は必ずこの値以下になる）。
const MAX_SET_OP_ROWS: usize = 10_000;

/// 多重集合演算用の行キー出現数表（Issue #3268）。`eval_tree` の `INTERSECT ALL`
/// と `EXCEPT ALL`（#3269）が使う（`take_one` が `true` の行を、前者は残し後者は相殺する）。
/// 右辺の各行を文全体の [`SetOpBudget`] に計上してから確保する（再出現の行も
/// 常に計上する。過大側に倒れるので fail-closed）。
struct KeyCounts {
    map: HashMap<Vec<u8>, usize>,
}

impl KeyCounts {
    fn from_rows(rows: &[ResultRow], budget: &mut SetOpBudget) -> Result<Self, SqlSurfaceError> {
        let mut map: HashMap<Vec<u8>, usize> = HashMap::new();
        for row in rows {
            crate::interrupt::check()?;
            let len = row_key_len(row)?
                .checked_add(std::mem::size_of::<usize>())
                .ok_or_else(|| SqlSurfaceError::payload_too_large("set operation key too large"))?;
            budget.charge(len)?;
            let key = row_key(row)?;
            let n = map.entry(key).or_insert(0);
            *n = n.saturating_add(1);
        }
        Ok(Self { map })
    }

    /// 残数が 1 以上なら 1 減らして `true`。
    fn take_one(&mut self, key: &[u8]) -> bool {
        match self.map.get_mut(key) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        }
    }
}

/// 集合演算文全体（全枝の走査・合成・行キー生成）で共有する累計バイト予算
/// （PR #1105 レビュー指摘対応）。各枝を独立に
/// [`crate::sql::scan::execute_scan`] の既定予算（[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）
/// で評価すると、最大 16 枝分の結果を `eval_tree` が同時に保持できてしまい、
/// 単一クエリの結果予算をはるかに超える合計メモリを SQL 入力だけで確保できる
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。本予算は
/// 単一クエリの結果予算と同じ上限を文全体で 1 つだけ持ち、枝の評価
/// （[`eval_branch`]）・ノード合成・行キー生成（[`eval_tree`]）のいずれでも
/// 消費し、超過は `54000` で打ち切る。
struct SetOpBudget {
    used: usize,
    cap: usize,
}

impl SetOpBudget {
    fn new(cap: usize) -> Self {
        Self { used: 0, cap }
    }

    /// まだ消費していない残り予算（次に評価する枝の [`crate::sql::scan::
    /// execute_scan_with_budget`] へそのまま渡す上限として使う。枝単体でも
    /// 残り予算を超えられないようにする）。
    fn remaining(&self) -> usize {
        self.cap.saturating_sub(self.used)
    }

    /// `bytes` を消費として計上する。累計が上限を超えたら `54000` で拒否する
    /// （fail-closed。超過分をアロケーションした後の判定ではなく、既に確保済み
    /// の行データのバイト量を根拠にするため、確保自体は上限内に収まった分のみ）。
    fn charge(&mut self, bytes: usize) -> Result<(), SqlSurfaceError> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.cap {
            return Err(SqlSurfaceError::payload_too_large(
                "set operation exceeds the shared result byte budget",
            ));
        }
        Ok(())
    }
}

/// 行集合（1 枝分の実行結果、または合成後の中間結果）の推定バイト量。
/// [`crate::sql::scan::execute_scan_with_budget`] が単一枝内で行う予算計上と
/// 同じ考え方（セル構造体オーバーヘッド＋可変長ペイロード実体）を、複数枝を
/// 横断する累計予算（[`SetOpBudget`]）へ計上するために使う。
fn result_bytes(columns: &[ColumnMeta], rows: &[ResultRow]) -> usize {
    let cell_struct_bytes = columns.len().saturating_mul(std::mem::size_of::<Cell>());
    let result_row_struct_bytes = std::mem::size_of::<ResultRow>();
    let per_row_struct_bytes = cell_struct_bytes.saturating_add(result_row_struct_bytes);
    let mut total = per_row_struct_bytes.saturating_mul(rows.len());
    for row in rows {
        for cell in &row.cells {
            total = total.saturating_add(cell_payload_bytes(cell));
        }
    }
    total
}

/// セル 1 個が持つ可変長ペイロードの推定バイト量（固定長セルは
/// [`result_bytes`] 側の構造体オーバーヘッドで既に計上済みのため `0`）。
fn cell_payload_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Text(s) => s.len(),
        Cell::Bytes(b) => b.len(),
        // wire／HTTP 応答のテキスト展開（ビット数と同じ文字数）後の大きさで課金する。
        Cell::Bit(v) => usize::try_from(v.bit_len()).unwrap_or(usize::MAX),
        // SPARSEVEC はテキスト整形後のおおよその大きさ（非ゼロ 1 要素あたり 32 バイト）で課金する。
        Cell::SparseVec(v) => v.nnz().saturating_mul(32),
        Cell::Json(s) => s.len(),
        // 正確な内部表現サイズではなく十進文字列表現による近似（予算計上の
        // 目的では十分。実際の確保量を下回らない側に倒す）。
        Cell::Numeric(d) => d.to_string().len(),
        Cell::Vector(v) => v.len().saturating_mul(std::mem::size_of::<f32>()),
        Cell::Array(arr) => arr.approx_heap_bytes(),
        // SEARCH 順序列（SQL-35・Issue #2213）。集合演算へは到達しないが、予算計上は過小にしない。
        Cell::Record(_) | Cell::RecordArray(_) => crate::sql::join::cell_payload_bytes(cell),
        Cell::Null
        | Cell::Integer(_)
        | Cell::Float(_)
        | Cell::Bool(_)
        | Cell::SignedInteger(_)
        | Cell::Date(_)
        | Cell::Timestamp(_)
        | Cell::Time(_)
        | Cell::TimestampTz(_)
        | Cell::Interval(_)
        | Cell::Uuid(_) => 0,
    }
}

/// 枝が参照するテーブル名を構文木から集める（重複は 1 回だけ収集する）。
/// `core.rs::EngineCore` が単一の `read_txn` 上で全テーブルのスキーマを解決する
/// ために使う（`read_txn_with_schemas`）。
pub(crate) fn collect_branch_tables(tree: &SetTree, out: &mut Vec<String>) {
    match tree {
        SetTree::Branch(scan) | SetTree::LimitedBranch(scan) => {
            if !out.iter().any(|t| t == &scan.table_name) {
                out.push(scan.table_name.clone());
            }
        }
        SetTree::AggregateBranch(agg) => {
            if !out.iter().any(|t| t == &agg.0.table_name) {
                out.push(agg.0.table_name.clone());
            }
        }
        // Issue #3270: 定数枝は表を読まない。
        SetTree::ConstBranch(_) => {}
        SetTree::Op { left, right, .. } => {
            collect_branch_tables(left, out);
            collect_branch_tables(right, out);
        }
    }
}

/// 集合演算木の左から最初の表の葉の表名（`ExplainTarget::table_name` 用。Issue #2047）。定数枝
/// （Issue #3270）は表を持たないので飛ばし、表の葉が無ければ空文字（構文段が全枝定数の木を
/// 拒否するため実際には到達しない。`ExplainTarget::Join` の先例に倣う）。
pub(crate) fn leftmost_table(tree: &SetTree) -> &str {
    match tree {
        SetTree::Branch(scan) | SetTree::LimitedBranch(scan) => &scan.table_name,
        SetTree::AggregateBranch(agg) => &agg.0.table_name,
        SetTree::ConstBranch(_) => "",
        SetTree::Op { left, right, .. } => {
            let l = leftmost_table(left);
            if l.is_empty() {
                leftmost_table(right)
            } else {
                l
            }
        }
    }
}

/// 1 個の評価結果（枝またはノード合成後）。`has_vector` は結果列に `VECTOR` 列を
/// 含むかどうか（重複除去を伴う演算の可否判定に使う。§2.2 参照）。
struct EvalOutcome {
    columns: Vec<ColumnMeta>,
    rows: Vec<ResultRow>,
    has_vector: bool,
}

/// [`ColumnMeta`] 同士の型整合判定（SQL-29 (c) §2.2）。広域取得の枝は `Id`／`Scalar`
/// のほか、式射影の項目・定数枝（Issue #3270）は `Computed` を返す。集計形の枝（Issue #1191）も
/// `Computed` を返し、束縛時に確定した静的型
/// （`Some(ty)`。Issue #1173）を `Scalar` と同じ型として比較する。静的型を持たない
/// `Computed { ty: None }` は不一致として扱う（fail-closed。`42804`）。
fn columns_compatible(a: &ColumnMeta, b: &ColumnMeta) -> bool {
    fn static_type(m: &ColumnMeta) -> Option<&ColumnType> {
        match m {
            ColumnMeta::Scalar { ty, .. } => Some(ty),
            ColumnMeta::Computed { ty: Some(ty), .. } => Some(ty),
            ColumnMeta::Id
            | ColumnMeta::AliasedId { .. }
            | ColumnMeta::Computed { ty: None, .. }
            | ColumnMeta::Record { .. } => None,
        }
    }
    match (a, b) {
        // 疑似列 `id`（`Cell::Integer`）は通常の NUMERIC 列（`Cell::Numeric`）と行キーのタグが
        // 異なり、同値でも重複除去・交差・差が誤るため、`id` 同士以外とは互換にしない
        // （fail-closed。`42804`。`Id` が従来そうである扱いへ `AliasedId` も揃える）。
        // 疑似列 `id` は別名の有無（`Id`／`AliasedId`）にかかわらず同じ値域として互換にする。
        (
            ColumnMeta::Id | ColumnMeta::AliasedId { .. },
            ColumnMeta::Id | ColumnMeta::AliasedId { .. },
        ) => true,
        _ => match (static_type(a), static_type(b)) {
            // 整数型（SMALLINT／INTEGER／BIGINT）は PostgreSQL 同様に相互に互換とし、結果列は
            // 広い方へ拡大する（[`merge_columns`]。Issue #1560）。
            (Some(ta), Some(tb)) => {
                ta.same_value_domain(tb)
                    || (ta.integer_rank().is_some() && tb.integer_rank().is_some())
            }
            _ => false,
        },
    }
}

/// 型整合済みの 2 枝の結果列メタを合成する。名前・由来（`Scalar`／`Computed`）は左に
/// 揃え、整数型どうしは広い方の型へ拡大する（RowDescription の型と実行結果の型を
/// そろえる。セルは `Cell::SignedInteger` で型をまたいで共通。Issue #1560）。
pub(crate) fn merge_columns(left: &[ColumnMeta], right: &[ColumnMeta]) -> Vec<ColumnMeta> {
    fn int_ty(m: &ColumnMeta) -> Option<&ColumnType> {
        match m {
            ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => {
                ty.integer_rank().map(|_| ty)
            }
            _ => None,
        }
    }
    fn static_ty(m: &ColumnMeta) -> Option<&ColumnType> {
        match m {
            ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => Some(ty),
            _ => None,
        }
    }
    // `CHAR(n)` は `is_text_like` に含まれないが、異型合成では長さを外す対象に含める
    // （現状の型整合判定は Bpchar の異型を拒否するため防御的な備え。#1566 の書き込み解禁後も安全側）。
    fn textish(t: &ColumnType) -> bool {
        t.is_text_like() || matches!(t, ColumnType::Bpchar(_))
    }
    left.iter()
        .enumerate()
        .map(|(i, l)| {
            let widened = match (int_ty(l), right.get(i).and_then(int_ty)) {
                (Some(a), Some(b)) => ColumnType::widest_integer(a, b),
                // TEXT 系どうしで型が食い違う合成は、左の型の長さ指定を外す。
                // 右枝が長い値を返しうるため、左の `VARCHAR(n)` のままだと wire 層が
                // 過小な typmod を公告してしまう（Issue #1565・fail-closed で長さなし）。
                _ => match (static_ty(l), right.get(i).and_then(static_ty)) {
                    (Some(a), Some(b)) if textish(a) && textish(b) && a != b => {
                        Some(match a {
                            ColumnType::Varchar(_) => ColumnType::Varchar(None),
                            // 左が `CHAR(n)` の場合も右枝が長い値を返しうるため、長さなしの
                            // TEXT へ落とす（OID 1042 ＋ 過小 typmod の公告を防ぐ）。
                            ColumnType::Bpchar(_) => ColumnType::Text,
                            other => other.clone(),
                        })
                    }
                    _ => None,
                },
            };
            match (l, widened) {
                (ColumnMeta::Scalar { name, .. }, Some(ty)) => ColumnMeta::Scalar {
                    name: name.clone(),
                    ty,
                },
                (ColumnMeta::Computed { name, .. }, Some(ty)) => ColumnMeta::Computed {
                    name: name.clone(),
                    ty: Some(ty),
                },
                _ => l.clone(),
            }
        })
        .collect()
}

/// 2 枝の列数・列型整合を検証する（SQL-29 (c) §2.2）。列数不一致・列型不一致は
/// `DatatypeMismatch`（`42804`）、重複除去を伴う演算での `VECTOR` 列混在は
/// `InvalidInput`（`22000`）で拒否する。[`validate_tree_types`]（実行前の事前
/// 検証）・[`describe_tree`]（Describe 経路）で同じ関数を共有し、判定基準を
/// 単一化する（PR #1105 レビュー指摘対応: 従来はこの判定ロジックを `eval_tree`
/// と `describe_tree` の 2 箇所に重複実装しており、判定基準が乖離しうる構造
/// だった）。
pub(crate) fn check_branch_type_compatibility(
    op: SetOperator,
    l_columns: &[ColumnMeta],
    r_columns: &[ColumnMeta],
    l_has_vector: bool,
    r_has_vector: bool,
) -> Result<(), SqlSurfaceError> {
    // `INTERVAL` 列は正規化比較（#1580）まで集合演算の対象外（`0A000`。fail-closed。
    // Issue #1579）。UNION ALL も含め、重複除去の行キー化に到達させない。
    if columns_have_interval(l_columns) || columns_have_interval(r_columns) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail:
                "INTERVAL, BIT, HALFVEC, INT8VEC and SPARSEVEC columns cannot be used in set operations yet"
                    .to_string(),
        });
    }
    if l_columns.len() != r_columns.len() {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "column count mismatch: {} vs {}",
            l_columns.len(),
            r_columns.len()
        )));
    }
    for (i, (a, b)) in l_columns.iter().zip(r_columns.iter()).enumerate() {
        if !columns_compatible(a, b) {
            return Err(SqlSurfaceError::datatype_mismatch(format!(
                "column {i} type mismatch between set operation branches"
            )));
        }
    }
    let dedup_needed = !matches!(op, SetOperator::UnionAll);
    if dedup_needed && (l_has_vector || r_has_vector) {
        return Err(SqlSurfaceError::invalid_input(
            "VECTOR columns cannot be combined with UNION/INTERSECT/EXCEPT (use UNION ALL)",
        ));
    }
    Ok(())
}

/// 列メタデータに `INTERVAL`／`BIT(n)`／`HALFVEC(N)` 列が含まれるか（集合演算の fail-closed 判定。
/// Issue #1579・#2021・#2014）。
fn columns_have_interval(columns: &[ColumnMeta]) -> bool {
    columns.iter().any(|c| {
        matches!(
            c,
            ColumnMeta::Scalar {
                ty: ColumnType::Interval
                    | ColumnType::Bit(_)
                    | ColumnType::HalfVec(_)
                    | ColumnType::Int8Vec(_)
                    | ColumnType::SparseVec(_),
                ..
            } | ColumnMeta::Computed {
                ty: Some(
                    ColumnType::Interval
                        | ColumnType::Bit(_)
                        | ColumnType::HalfVec(_)
                        | ColumnType::Int8Vec(_)
                        | ColumnType::SparseVec(_),
                ),
                ..
            }
        )
    })
}

pub(crate) fn columns_have_vector(columns: &[ColumnMeta]) -> bool {
    columns.iter().any(|c| {
        matches!(
            c,
            ColumnMeta::Scalar {
                ty: ColumnType::Vector(_),
                ..
            } | ColumnMeta::Computed {
                ty: Some(ColumnType::Vector(_)),
                ..
            }
        )
    })
}

/// `f64` の正準化（`-0.0` を `+0.0` へ、NaN を単一表現へ）。SQL-25 (c) の同値判定
/// 規約と同一（#1098 が `sql::distinct` へ `canon_f64` を追加した場合はそちらへ
/// 統合する。現時点では未マージのためローカル定義とする）。
fn canon_f64_bits(f: f64) -> u64 {
    let normalized = if f == 0.0 { 0.0 } else { f };
    if normalized.is_nan() {
        f64::NAN.to_bits()
    } else {
        normalized.to_bits()
    }
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SqlSurfaceError> {
    let len = u32::try_from(bytes.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("set operation row key exceeds length limit")
    })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// 行キー用の `NUMERIC` 正準テキスト（末尾の小数部ゼロを除く）。整数 `AVG`（Issue #2270）は
/// 値ごとに scale が異なり、`1.5000000000000000` と `1.50` は numeric として等しいため、
/// 重複排除では値の等価で同一視する（PostgreSQL と同じ）。
fn numeric_key_text(d: &crate::numeric::Decimal) -> String {
    let text = d.to_string();
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

/// 行の正準バイト列表現（重複除去・`INTERSECT`／`EXCEPT` の同値判定に使う行
/// キー）。セルごとに「型タグ＋長さ接頭辞（`u32` BE）＋ペイロード」を連結する
/// （SQL-29 (c) §2.3）。`VECTOR` 列は §2.2 の型検証で重複除去を伴う演算からは
/// 排除済みのため、到達した場合は防御的に拒否する（fail-closed）。
pub(crate) fn row_key(row: &ResultRow) -> Result<Vec<u8>, SqlSurfaceError> {
    let mut out = Vec::new();
    for cell in &row.cells {
        match cell {
            Cell::Null => out.push(0),
            // 集計項目（`COUNT` 等。`Cell::Integer`）と `BIGINT` 列（`Cell::SignedInteger`）は
            // 静的型が同じ（`BigInt`）で互換とみなされるため、`i64` に収まる値は同じ
            // 型タグ・同じ表現へ正準化して同値判定する（Issue #1191）。
            Cell::Integer(v) => match i64::try_from(*v) {
                Ok(signed) => {
                    out.push(5);
                    out.extend_from_slice(&signed.to_be_bytes());
                }
                Err(_) => {
                    out.push(1);
                    out.extend_from_slice(&v.to_be_bytes());
                }
            },
            Cell::Text(s) => {
                out.push(2);
                push_len_prefixed(&mut out, s.as_bytes())?;
            }
            Cell::Float(f) => {
                out.push(3);
                out.extend_from_slice(&canon_f64_bits(*f).to_be_bytes());
            }
            Cell::Bool(b) => {
                out.push(4);
                out.push(u8::from(*b));
            }
            Cell::SignedInteger(v) => {
                out.push(5);
                out.extend_from_slice(&v.to_be_bytes());
            }
            // `TIME`（Issue #1570）。0 時起点のマイクロ秒を型タグ 13 で単射に符号化する。
            Cell::Time(t) => {
                out.push(13);
                out.extend_from_slice(&t.to_be_bytes());
            }
            Cell::Date(d) => {
                out.push(6);
                out.extend_from_slice(&d.to_be_bytes());
            }
            Cell::Timestamp(t) => {
                out.push(7);
                out.extend_from_slice(&t.to_be_bytes());
            }
            Cell::Bytes(b) => {
                out.push(8);
                push_len_prefixed(&mut out, b)?;
            }
            Cell::Json(s) => {
                out.push(9);
                push_len_prefixed(&mut out, s.as_bytes())?;
            }
            Cell::Numeric(d) => {
                out.push(10);
                push_len_prefixed(&mut out, numeric_key_text(d).as_bytes())?;
            }
            Cell::Uuid(u) => {
                out.push(11);
                out.extend_from_slice(u.as_bytes());
            }
            Cell::Array(arr) => {
                out.push(12);
                // 行バイトと同じ正準ペイロード（NULL ビットマップを含む単射表現）を
                // 長さ前置で積む。NULL 要素を含む配列は要素型序数へ 0x80 を立てる。
                let (ordinal, flags, payload) =
                    arr.canonical_parts()
                        .map_err(|_| SqlSurfaceError::Internal {
                            detail: "array cell could not be canonicalized for set operation"
                                .to_string(),
                        })?;
                out.push(if flags == 0 { ordinal } else { ordinal | 0x80 });
                let n = u32::try_from(arr.len()).map_err(|_| {
                    SqlSurfaceError::payload_too_large("set operation row key exceeds length limit")
                })?;
                out.extend_from_slice(&n.to_be_bytes());
                push_len_prefixed(&mut out, &payload)?;
            }
            // `TIMESTAMPTZ`（Issue #1576）。UTC マイクロ秒を型タグ 14 で単射に符号化する
            // （時点で同一視するため表示 TZ に依存しない）。
            Cell::TimestampTz(t) => {
                out.push(14);
                out.extend_from_slice(&t.to_be_bytes());
            }
            // `INTERVAL` は `check_branch_type_compatibility` が `0A000` で事前に拒否する
            // ため到達しない。破損状態を防御的に拒否する（fail-closed。Issue #1579）。
            Cell::Interval(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "INTERVAL column reached set operation row key encoding".to_string(),
                });
            }
            // `BIT(n)` は集合演算の型検証で事前に拒否される想定のため到達しない。破損状態を
            // 防御的に拒否する（fail-closed。Issue #2021）。
            Cell::Bit(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "BIT column reached set operation row key encoding".to_string(),
                });
            }
            // `SPARSEVEC(N)` も集合演算の型検証で事前に拒否される想定のため到達しない。破損状態を
            // 防御的に拒否する（fail-closed。Issue #2025）。
            Cell::SparseVec(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "SPARSEVEC column reached set operation row key encoding".to_string(),
                });
            }
            // §2.2 の型検証（重複除去を伴う演算からの `VECTOR` 列排除）で本来
            // 到達しない。破損状態を防御的に拒否する（fail-closed。XX000）。
            Cell::Vector(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "VECTOR column reached set operation row key encoding".to_string(),
                });
            }
            // SEARCH 順序列（SQL-35・Issue #2213）は集合演算へ到達させない（fail-closed）。
            Cell::Record(_) | Cell::RecordArray(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "record column reached set operation row key encoding".to_string(),
                });
            }
        }
    }
    Ok(out)
}

/// `payload_len` バイトのペイロードに対する [`push_len_prefixed`] の出力長
/// （長さ接頭辞 `u32` BE の 4 byte ＋ペイロード本体）。[`row_key_len`] が
/// `push_len_prefixed` を呼ばずに長さだけを算出するために使う。
fn len_prefixed_total_len(payload_len: usize) -> Result<usize, SqlSurfaceError> {
    u32::try_from(payload_len).map_err(|_| {
        SqlSurfaceError::payload_too_large("set operation row key exceeds length limit")
    })?;
    Ok(4usize.saturating_add(payload_len))
}

/// [`row_key`] が実際に生成するバイト列の長さを、`Vec<u8>` を確保せずに算出
/// する（PR #1105 レビュー指摘対応: 従来は `row_key` が可変長セルを複製した
/// `Vec<u8>` を確保した後で `SetOpBudget::charge` を呼んでいたため、予算上限
/// 付近で同程度の大きさの行キーを、予算検証より前に確保できてしまっていた
/// 〔security.md「長さフィールドは上限検証してからアロケーションに使う」〕。
/// 呼び出し元は本関数の戻り値を `budget.charge` へ渡してから `row_key` を
/// 呼ぶこと。`row_key` と同じ型タグ・長さ接頭辞規約に従う必要があるため、
/// 両者は変更時に必ず同時に保守する（`tests::row_key_len_matches_row_key_
/// output_length` が全 variant を横断して不一致を検出する）。
pub(crate) fn row_key_len(row: &ResultRow) -> Result<usize, SqlSurfaceError> {
    let mut total: usize = 0;
    for cell in &row.cells {
        let cell_len = match cell {
            // 型タグ 1 byte のみ（ペイロードなし）。
            Cell::Null => 1,
            // 型タグ 1 byte ＋ `u64`/`i64`/`f64` の 8 byte。
            Cell::Integer(_) | Cell::Float(_) | Cell::SignedInteger(_) | Cell::Timestamp(_) => {
                1 + 8
            }
            // 型タグ 1 byte ＋ `bool` の 1 byte。
            Cell::Bool(_) => 1 + 1,
            // 型タグ 1 byte ＋ `i32` の 4 byte。
            Cell::Date(_) => 1 + 4,
            // 型タグ 1 byte ＋長さ接頭辞つきペイロード。
            Cell::Text(s) => 1 + len_prefixed_total_len(s.len())?,
            Cell::Bytes(b) => 1 + len_prefixed_total_len(b.len())?,
            Cell::Json(s) => 1 + len_prefixed_total_len(s.len())?,
            Cell::Numeric(d) => 1 + len_prefixed_total_len(numeric_key_text(d).len())?,
            // 型タグ 1 byte ＋ UUID 本体（[`crate::uuid::Uuid::as_bytes`] は
            // 常に 16 byte）。
            Cell::Uuid(_) => 1 + 16,
            // 型タグ 1 byte（12）＋要素種別タグ 1 byte（0／1）＋要素数 `u32`
            // （4 byte）＋各要素のペイロード。
            Cell::Array(arr) => {
                let payload_len =
                    arr.canonical_payload_len()
                        .map_err(|_| SqlSurfaceError::Internal {
                            detail: "array cell could not be sized for set operation".to_string(),
                        })?;
                1usize
                    .saturating_add(1)
                    .saturating_add(4)
                    .saturating_add(len_prefixed_total_len(payload_len)?)
            }
            // 型タグ 1 byte ＋ `i64` の 8 byte（`row_key` の `Cell::Time`。Issue #1570）。
            Cell::Time(_) => 1 + 8,
            // 型タグ 1 byte ＋ `i64` の 8 byte（`row_key` の `Cell::TimestampTz`。Issue #1576）。
            Cell::TimestampTz(_) => 1 + 8,
            // `row_key` が拒否する（到達しない）ため寸法は 0。
            Cell::Interval(_) | Cell::Bit(_) | Cell::SparseVec(_) => 0,
            // §2.2 の型検証で本来到達しない（`row_key` と同じ防御的拒否）。
            Cell::Vector(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "VECTOR column reached set operation row key encoding".to_string(),
                });
            }
            Cell::Record(_) | Cell::RecordArray(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "record column reached set operation row key encoding".to_string(),
                });
            }
        };
        total = total.saturating_add(cell_len);
    }
    Ok(total)
}

/// 単一の枝（`SELECT ... FROM <table> [WHERE ...]`）を束縛・実行する。可視かつ
/// `WHERE` 一致行が `core::MAX_SEARCH_K` を超える場合は `54000`（超過検出のため
/// `MAX_SEARCH_K + 1` を上限として走査する）。文全体で共有する `budget`
/// （[`SetOpBudget`]）の残り予算を枝の実行に渡し（枝単体でも残り予算を超えられ
/// ない）、実行結果のバイト量を消費として計上する。
///
/// `explicit_limit` は括弧付き枝が枝内 `LIMIT`／`OFFSET`／`ORDER BY` を持つ場合
/// （[`SetTree::LimitedBranch`]。Issue #1191）で、枝自身の `limit`・`offset`・
/// `order_by` をそのまま使って切り詰める（`54000` にしない。`bind_scan` が
/// `1..=MAX_SEARCH_K` を検証済みのため結果は上限内）。それ以外（既定の枝）は従来どおり
/// `MAX_SEARCH_K + 1` へ差し替えて超過を検出する。
fn eval_branch(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    validated: &ValidatedScan,
    udfs: &UdfRegistry,
    budget: &mut SetOpBudget,
    explicit_limit: bool,
) -> Result<EvalOutcome, SqlSurfaceError> {
    let mut bound = crate::sql::parser::bind_scan(validated, schema, udfs)?;
    if !explicit_limit {
        // §2.3: 可視行数の上限判定は「可視行数だけに依存」させる（全体 `LIMIT` の
        // 有無で成否が変わらない単純な規則）。`bound.limit`（`pub(crate)`）を
        // `MAX_SEARCH_K + 1` へ差し替えて超過を検出できるようにする。
        bound.limit = MAX_SET_OP_ROWS + 1;
    }
    let result = crate::sql::scan::execute_scan_with_budget(
        read_txn,
        ctx,
        schema,
        &bound,
        budget.remaining(),
    )?;
    if result.rows.len() > MAX_SET_OP_ROWS {
        return Err(SqlSurfaceError::payload_too_large(
            "set operation branch exceeds the visible row limit",
        ));
    }
    budget.charge(result_bytes(&result.columns, &result.rows))?;
    let has_vector = columns_have_vector(&result.columns);
    Ok(EvalOutcome {
        columns: result.columns,
        rows: result.rows,
        has_vector,
    })
}

/// 集計形の枝（[`SetTree::AggregateBranch`]。Issue #1191）を束縛・実行する。通常の
/// 集計 SELECT と同じ `bind_aggregate` → `execute_aggregate_with_cache`（キャッシュ
/// 非経由）で、RLS 暗黙適用・fail-closed を第 2 の実行器なしに継承する。結果バイトは
/// 文全体で共有する `budget` へ計上し、結果行数は `MAX_SET_OP_ROWS` で頭打ちにする。
fn eval_aggregate_branch(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    branch: &SetAggregateBranch,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    budget: &mut SetOpBudget,
) -> Result<EvalOutcome, SqlSurfaceError> {
    let bound = crate::sql::parser::bind_aggregate(&branch.0, schema, udfs)?;
    let result = crate::sql::aggregate::execute_aggregate_with_group_limits(
        read_txn,
        ctx,
        schema,
        &bound,
        budget.remaining(),
        None,
        None,
        None,
        group_limits,
    )?;
    if result.rows.len() > MAX_SET_OP_ROWS {
        return Err(SqlSurfaceError::payload_too_large(
            "set operation branch exceeds the visible row limit",
        ));
    }
    budget.charge(result_bytes(&result.columns, &result.rows))?;
    let has_vector = columns_have_vector(&result.columns);
    Ok(EvalOutcome {
        columns: result.columns,
        rows: result.rows,
        has_vector,
    })
}

/// 定数枝（[`SetTree::ConstBranch`]。Issue #3270）を束縛・実行する。`sql::const_select` の
/// `bind` → `execute` をそのまま使い（第 2 の評価器を作らない）、結果バイトを文全体で共有する
/// `budget` へ計上する。表・ストレージに触れないため RLS の対象行は無く、他テナントの情報にも
/// 依存しない。ルーチン呼び出し・スキーマ関数は `bind` が `0A000` にする（fail-closed）。
fn eval_const_branch(
    branch: &SetConstBranch,
    udfs: &UdfRegistry,
    budget: &mut SetOpBudget,
) -> Result<EvalOutcome, SqlSurfaceError> {
    let bound = crate::sql::const_select::bind(&branch.0, udfs)?;
    let result = crate::sql::const_select::execute(&bound)?;
    budget.charge(result_bytes(&result.columns, &result.rows))?;
    let has_vector = columns_have_vector(&result.columns);
    Ok(EvalOutcome {
        columns: result.columns,
        rows: result.rows,
        has_vector,
    })
}

/// 集合演算木全体の列数・列型整合を、行走査（[`eval_tree`]・
/// [`crate::sql::scan::execute_scan_with_budget`]）より前に確定させる
/// （PR #1105 レビュー指摘対応: 従来は `eval_tree` が左右の枝を実際に走査して
/// から `check_branch_type_compatibility` を呼んでいたため、型不一致の文でも
/// 全行走査・結果確保が先行し、行数・バイト上限エラー（`54000`）が本来返す
/// べき `DatatypeMismatch`〔`42804`〕より先に成立し得た。`describe_tree` と
/// 同じ判定基準を、行を一切走査しない `bind_scan`〔`eval_branch` が実際の
/// 走査で使うのと同じ束縛〕で走査前に確定させる。戻り値は上位の呼び出し元
/// では使わず、検証のみを目的とする）。
fn validate_tree_types(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    mut leaves: Option<&mut Vec<BoundSetLeaf>>,
) -> Result<(Vec<ColumnMeta>, bool), SqlSurfaceError> {
    match tree {
        SetTree::Branch(validated) | SetTree::LimitedBranch(validated) => {
            let schema =
                schemas
                    .get(&validated.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            let bound = crate::sql::parser::bind_scan(validated, schema, udfs)?;
            let columns = crate::sql::describe::projected_columns(bound.projection(), schema);
            let has_vector = columns_have_vector(&columns);
            if let Some(out) = leaves {
                out.push(BoundSetLeaf {
                    table: validated.table_name.clone(),
                    shape: LeafShape {
                        limited: matches!(tree, SetTree::LimitedBranch(_)),
                        sort_keys: bound.order_by.len(),
                        has_limit: matches!(tree, SetTree::LimitedBranch(_)),
                        has_offset: bound.offset != 0,
                        aggregate: None,
                    },
                    scan: Some(bound),
                });
            }
            Ok((columns, has_vector))
        }
        SetTree::AggregateBranch(branch) => {
            let schema =
                schemas
                    .get(&branch.0.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            let bound = crate::sql::parser::bind_aggregate(&branch.0, schema, udfs)?;
            let columns = crate::sql::describe::aggregate_columns(&bound, schema)?;
            let has_vector = columns_have_vector(&columns);
            if let Some(out) = leaves {
                let grouped = bound.group_by.as_ref();
                out.push(BoundSetLeaf {
                    table: branch.0.table_name.clone(),
                    shape: LeafShape {
                        limited: true,
                        sort_keys: grouped.map_or(0, |g| g.order_by.len()),
                        has_limit: grouped.is_some_and(|g| g.limit.is_some()),
                        has_offset: grouped.is_some_and(|g| g.offset != 0),
                        aggregate: Some(aggregate_shape(&bound)),
                    },
                    scan: None,
                });
            }
            Ok((columns, has_vector))
        }
        SetTree::ConstBranch(branch) => {
            // EXPLAIN（`leaves` を渡す経路）は構文段が定数枝を 0A000 にするため到達しない。
            // 到達したら葉の対応が崩れるので fail-closed にする。
            if leaves.is_some() {
                return Err(SqlSurfaceError::Internal {
                    detail: "FROM-less branch reached the set operation EXPLAIN path".to_string(),
                });
            }
            let bound = crate::sql::const_select::bind(&branch.0, udfs)?;
            let columns = bound.columns();
            let has_vector = columns_have_vector(&columns);
            Ok((columns, has_vector))
        }
        SetTree::Op { op, left, right } => {
            let (l_columns, l_has_vector) =
                validate_tree_types(schemas, left, udfs, leaves.as_deref_mut())?;
            let (r_columns, r_has_vector) = validate_tree_types(schemas, right, udfs, leaves)?;
            check_branch_type_compatibility(
                *op,
                &l_columns,
                &r_columns,
                l_has_vector,
                r_has_vector,
            )?;
            Ok((merge_columns(&l_columns, &r_columns), l_has_vector))
        }
    }
}

/// 解決済みの集合全体 `ORDER BY` キー（Issue #3266）。`index` は合成後の出力列の位置（0 始まり）。
/// `nulls` は未指定を PG 既定（ASC は末尾・DESC は先頭）で確定した値。[`plan_set_order_by`] が
/// 比較クラスを付けて [`OrderKeyPlan`] にし、ソート実行（Issue #3267）が受け取る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedSetOrderKey {
    pub(crate) index: usize,
    pub(crate) descending: bool,
    pub(crate) nulls: NullPlacement,
}

/// 出力列メタデータの列名（疑似列 `id` は `id`）。
fn output_column_name(meta: &ColumnMeta) -> &str {
    match meta {
        ColumnMeta::Id => "id",
        ColumnMeta::AliasedId { name }
        | ColumnMeta::Scalar { name, .. }
        | ColumnMeta::Computed { name, .. }
        | ColumnMeta::Record { name, .. } => name,
    }
}

/// 集合全体 `ORDER BY` のキーを合成後の出力列（左端の枝の列名）へ解決する（Issue #3266）。
/// 序数は `1..=列数`（範囲外は `42P10`）、列名は出力列名と一致するもの（無ければ `42703`、
/// 2 列以上と一致なら `42702`）。枝のテーブルにあっても出力に無い列は `42703`（PG と同じ）。
/// 行を読まないため、可視行の有無によらず分類・文言は同一（テナント境界を緩めない）。
pub(crate) fn resolve_order_keys(
    columns: &[ColumnMeta],
    keys: &[SetOrderKey],
) -> Result<Vec<ResolvedSetOrderKey>, SqlSurfaceError> {
    let mut resolved = Vec::with_capacity(keys.len());
    for key in keys {
        match key {
            SetOrderKey::Ordinal {
                position,
                descending,
                nulls,
            } => {
                let index = usize::try_from(*position)
                    .ok()
                    .and_then(|p| p.checked_sub(1))
                    .filter(|i| *i < columns.len())
                    .ok_or(SqlSurfaceError::OrderByPositionNotInSelectList {
                        position: *position,
                    })?;
                resolved.push(ResolvedSetOrderKey {
                    index,
                    descending: *descending,
                    nulls: nulls.unwrap_or(NullPlacement::pg_default(*descending)),
                });
            }
            SetOrderKey::Column {
                name,
                descending,
                nulls,
            } => {
                let mut matches = columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| output_column_name(c) == name.as_str())
                    .map(|(i, _)| i);
                let index = matches
                    .next()
                    .ok_or_else(|| SqlSurfaceError::undefined_column(name.clone()))?;
                if matches.next().is_some() {
                    return Err(SqlSurfaceError::ambiguous_column(name.clone()));
                }
                resolved.push(ResolvedSetOrderKey {
                    index,
                    descending: *descending,
                    nulls: nulls.unwrap_or(NullPlacement::pg_default(*descending)),
                });
            }
        }
    }
    Ok(resolved)
}

/// 集合全体 `ORDER BY` の実行計画を行を読まずに作る（Issue #3267・SQL-29 (c) ポインタ）。
/// キーを解決（構文・束縛の誤りは先に `42703`／`42702`／`42P10`）したあと、キー列の型から比較
/// クラスを決める。比較できない型（`TIMESTAMPTZ`・`CHAR(n)`・`JSON`・配列・`VECTOR` 等）と、
/// 非バイト列順の照合を持つ列を含む表がある場合の `TEXT` キーは `0A000` で拒否する（照合を
/// 運ばずにバイト列順で黙って比べる fail-open を作らない。`core.rs` の
/// `check_buffered_view_collations` と同じ保守的な表単位の判定）。Execute（[`prepare`]）・
/// Describe・`EXPLAIN`・`DECLARE CURSOR` が同じ関数を通るため受理・拒否と `wire_code` が一致する。
/// 行を読まないので、可視行の有無によらず分類・文言は同一（テナント境界を緩めない）。
/// キーが無ければ空の計画（ソートなし）。
pub(crate) fn plan_set_order_by(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    columns: &[ColumnMeta],
    keys: &[SetOrderKey],
) -> Result<Vec<OrderKeyPlan>, SqlSurfaceError> {
    let resolved = resolve_order_keys(columns, keys)?;
    let mut plans = Vec::with_capacity(resolved.len());
    let mut has_text_key = false;
    for key in &resolved {
        let meta = columns
            .get(key.index)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        let class = match meta {
            ColumnMeta::Id => cmp_class(None),
            ColumnMeta::AliasedId { .. } => cmp_class(Some(&crate::sql::exec::ALIASED_ID_TYPE)),
            ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => {
                cmp_class(Some(ty))
            }
            ColumnMeta::Computed { ty: None, .. } | ColumnMeta::Record { .. } => None,
        }
        .ok_or_else(|| {
            SqlSurfaceError::feature_not_supported(
                "ORDER BY over a set operation result is not supported for this column type",
            )
        })?;
        has_text_key |= matches!(class, CmpClass::Text);
        plans.push(OrderKeyPlan {
            index: key.index,
            class: OrderClass::Scalar(class),
            descending: key.descending,
            nulls: key.nulls,
        });
    }
    if has_text_key {
        let mut names = Vec::new();
        collect_branch_tables(tree, &mut names);
        let collated = names.iter().filter_map(|n| schemas.get(n)).any(|schema| {
            schema
                .columns
                .iter()
                .any(|c| c.collation.is_some_and(|coll| !coll.is_byte_order()))
        });
        if collated {
            return Err(SqlSurfaceError::feature_not_supported(
                "this collation is not supported in this expression",
            ));
        }
    }
    Ok(plans)
}

/// ソート用の比較キー配列・添字列・スロット列を [`SetOpBudget`] へ計上する（確保前に判定する）。
/// 計算は `checked_*` で行い、オーバーフローは予算超過と同じ `54000`。
fn charge_sort_budget(
    budget: &mut SetOpBudget,
    rows: usize,
    keys: usize,
) -> Result<(), SqlSurfaceError> {
    let per_key = std::mem::size_of::<Option<CmpVal<'static>>>();
    let per_row = keys
        .checked_mul(per_key)
        .and_then(|k| k.checked_add(std::mem::size_of::<usize>().checked_mul(2)?))
        .and_then(|k| k.checked_add(std::mem::size_of::<Option<ResultRow>>()));
    match per_row.and_then(|p| p.checked_mul(rows)) {
        Some(total) => budget.charge(total),
        None => Err(SqlSurfaceError::payload_too_large(
            "set operation exceeds the shared result byte budget",
        )),
    }
}

/// 行を読まない準備段（Issue #2047・SQL-75 (2)(6) ポインタ）。全体 `LIMIT`／`OFFSET` の範囲検証
/// → 全枝の束縛と列型整合の検証、の順で行う。実行（[`execute_with_budget_and_result_limits`]）と
/// `EXPLAIN`（`core.rs` の `run_set_operation_explain`）の双方がこの 1 関数を通るため、判定順と
/// `wire_code` が構造的に一致する。`leaves` を渡すと束縛した葉を左から右の順に積む（`EXPLAIN` 用。
/// 実行側は `None`）。戻り値は検証済みの全体 `LIMIT`（`Some` なら範囲内）と `OFFSET`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    limit: Option<u32>,
    offset: u32,
    // Issue #3266: 括弧なし末尾 `ORDER BY` のキー（空は無し）。
    order_by: &[SetOrderKey],
    result_limits: &crate::result_limits::ResultLimits,
    leaves: Option<&mut Vec<BoundSetLeaf>>,
) -> Result<(Option<usize>, usize, Vec<OrderKeyPlan>), SqlSurfaceError> {
    // PR #1105 レビュー指摘対応: 全体 `LIMIT` の範囲検証（`1..=設定値`）を全枝の走査・合成より
    // 前に行う（[`describe_columns`] と同じ検証を共有し、Execute／Describe の受理・拒否を一致させる）。
    let validated_limit = match limit {
        Some(n) => Some(crate::sql::parser::validate_search_limit_with(
            n,
            result_limits,
        )?),
        None => None,
    };
    let validated_offset = crate::sql::parser::validate_search_offset(offset)?;
    // PR #1105 レビュー指摘対応: 列数・列型整合（`DatatypeMismatch`・`42804`）を全枝の走査・合成より
    // 前に確定させる（[`validate_tree_types`] のドキュメンテーションコメント参照）。
    let (columns, _) = validate_tree_types(schemas, tree, udfs, leaves)?;
    // Issue #3266・#3267: キーの解決（`42703`／`42702`／`42P10`）→ キー型・照合（`0A000`）。
    let plans = plan_set_order_by(schemas, tree, &columns, order_by)?;
    Ok((validated_limit, validated_offset, plans))
}

/// `EXPLAIN` が葉（枝）ごとに必要とする束縛結果（Issue #2047）。[`prepare`] が左から右の順に積み、
/// `core.rs` が `scan` の走査分類（単一表の広域取得 `EXPLAIN` と共有）を作り、
/// `sql::set_op_explain::explain_tree` が `shape` から段を組む。
#[derive(Debug)]
pub(crate) struct BoundSetLeaf {
    /// 枝の表名（`schemas` の参照キー。出力には載せない）。
    pub(crate) table: String,
    /// 段の木に必要な枝の形（項目数・有無のみ。値は持たない）。
    pub(crate) shape: LeafShape,
    /// スキャン枝の束縛結果（実行と同じ `bind_scan`）。集計枝は `None`。
    pub(crate) scan: Option<crate::sql::parser::BoundScan>,
}

/// 枝の形（SQL-75 (5): 件数・有無のみで LIMIT／OFFSET の値は持たない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeafShape {
    /// 括弧付きで枝内に `ORDER BY`／`LIMIT`／`OFFSET` を持てる枝か（集計枝は常に真）。
    pub(crate) limited: bool,
    /// 枝内の並べ替えキー数。
    pub(crate) sort_keys: usize,
    /// 枝内に `LIMIT` があるか。
    pub(crate) has_limit: bool,
    /// 枝内に `OFFSET`（非 0）があるか。
    pub(crate) has_offset: bool,
    /// 集計枝のときの集計段の情報。
    pub(crate) aggregate: Option<AggregateShape>,
}

/// 集計枝の集計段の情報（`sql::join::explain` の `aggregate` 段と同じ意味）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AggregateShape {
    pub(crate) group_keys: usize,
    pub(crate) aggregates: usize,
    pub(crate) having: bool,
}

/// 束縛済み集計から集計段の情報（キー数・集計数・HAVING の有無）を取る。集合演算の集計枝と
/// CTE・部分クエリの `EXPLAIN`（Issue #2048。`sql::cte_explain`）が共有する。
pub(crate) fn aggregate_shape(bound: &crate::sql::parser::BoundAggregate) -> AggregateShape {
    let grouped = bound.group_by.as_ref();
    AggregateShape {
        group_keys: grouped.map_or(0, |g| g.keys.len()),
        aggregates: bound.items.len(),
        having: bound.ungrouped_having.is_some()
            || grouped.is_some_and(|g| !g.having.is_empty() || !g.having_exprs.is_empty()),
    }
}

/// 集計枝の走査分類の行。[`eval_aggregate_branch`] は可視ビットマップのキャッシュ・arena・
/// スカラー索引のいずれも実行器へ渡さない（常に全走査）ため、`EXPLAIN` も `plain_scan`／`full_scan`
/// に固定する（D5「矛盾出力の防止」）。**`eval_aggregate_branch` がキャッシュや索引を渡すよう
/// 変わるときは、この分類も一緒に変えること。**
pub(crate) fn aggregate_branch_explain_lines() -> Vec<String> {
    crate::sql::explain::relational_explain_lines(
        crate::sql::scalar_plan::ScalarPlan::PlainScan,
        None,
        crate::sql::explain::AccessPath::FullScan,
        &crate::sql::explain::ExplainIndexNames::default(),
        &[],
    )
}

/// 構文木を後行順（postorder）で評価する。結果列名・メタデータは常に左の子
/// （左端の枝）に揃える。`budget` は文全体で共有する累計バイト予算
/// （[`SetOpBudget`]）で、枝の評価・行キー生成のいずれでも消費する。列数・
/// 列型整合の検証は本関数の呼び出し前に [`validate_tree_types`] が完結させて
/// いるため、ここでは繰り返さない（PR #1105 レビュー指摘対応。二重実装を避け、
/// 走査前検証の判定基準を単一化する）。
fn eval_tree(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    group_limits: crate::group_by_limits::GroupByLimits,
    budget: &mut SetOpBudget,
) -> Result<EvalOutcome, SqlSurfaceError> {
    match tree {
        SetTree::Branch(validated) | SetTree::LimitedBranch(validated) => {
            let schema =
                schemas
                    .get(&validated.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            let explicit_limit = matches!(tree, SetTree::LimitedBranch(_));
            eval_branch(
                read_txn,
                ctx,
                schema,
                validated,
                udfs,
                budget,
                explicit_limit,
            )
        }
        SetTree::AggregateBranch(branch) => {
            let schema =
                schemas
                    .get(&branch.0.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            eval_aggregate_branch(read_txn, ctx, schema, branch, udfs, group_limits, budget)
        }
        SetTree::ConstBranch(branch) => eval_const_branch(branch, udfs, budget),
        SetTree::Op { op, left, right } => {
            let l = eval_tree(read_txn, ctx, schemas, left, udfs, group_limits, budget)?;
            let r = eval_tree(read_txn, ctx, schemas, right, udfs, group_limits, budget)?;

            let rows = match op {
                SetOperator::UnionAll => {
                    let mut rows = l.rows;
                    rows.extend(r.rows);
                    if rows.len() > MAX_SET_OP_ROWS {
                        return Err(SqlSurfaceError::payload_too_large(
                            "set operation result exceeds the row limit",
                        ));
                    }
                    rows
                }
                SetOperator::Union => {
                    let mut seen: HashSet<Vec<u8>> = HashSet::new();
                    let mut rows = Vec::new();
                    for row in l.rows.into_iter().chain(r.rows) {
                        // Issue #1952: 協調的中断の確認点（UNION の重複除去の行ごと。commit 前の読み取り経路）。
                        crate::interrupt::check()?;
                        // 行キー生成も文全体で共有する累計バイト予算の対象
                        // （PR #1105 レビュー指摘対応。`seen` への格納で複製される
                        // 分の追加メモリを見逃さない）。長さを `row_key_len` で
                        // 先に算出して予算を確認してから `row_key` で実際に
                        // `Vec<u8>` を確保する（予算超過時に確保前に拒否する。
                        // 逆順だと上限直前で確保してから拒否することになる）。
                        budget.charge(row_key_len(&row)?)?;
                        let key = row_key(&row)?;
                        if seen.contains(&key) {
                            continue;
                        }
                        if rows.len() >= MAX_SET_OP_ROWS {
                            return Err(SqlSurfaceError::payload_too_large(
                                "set operation result exceeds the row limit",
                            ));
                        }
                        seen.insert(key);
                        rows.push(row);
                    }
                    rows
                }
                SetOperator::IntersectAll => {
                    // Issue #3268: 多重集合の交差。右辺の出現数を数え、左辺の各行で
                    // 残数があるときだけ 1 件消費して出力する（min(左, 右) 件）。
                    let mut counts = KeyCounts::from_rows(&r.rows, budget)?;
                    let mut rows = Vec::new();
                    for row in l.rows.into_iter() {
                        crate::interrupt::check()?;
                        budget.charge(row_key_len(&row)?)?;
                        let key = row_key(&row)?;
                        if !counts.take_one(&key) {
                            continue;
                        }
                        if rows.len() >= MAX_SET_OP_ROWS {
                            return Err(SqlSurfaceError::payload_too_large(
                                "set operation result exceeds the row limit",
                            ));
                        }
                        rows.push(row);
                    }
                    rows
                }
                SetOperator::ExceptAll => {
                    // Issue #3269: 多重集合の差。右辺の出現数を数え、左辺の各行で
                    // 残数があれば 1 件相殺して捨て、無ければ出力する（max(左 − 右, 0) 件）。
                    let mut counts = KeyCounts::from_rows(&r.rows, budget)?;
                    let mut rows = Vec::new();
                    for row in l.rows.into_iter() {
                        crate::interrupt::check()?;
                        budget.charge(row_key_len(&row)?)?;
                        let key = row_key(&row)?;
                        if counts.take_one(&key) {
                            continue;
                        }
                        if rows.len() >= MAX_SET_OP_ROWS {
                            return Err(SqlSurfaceError::payload_too_large(
                                "set operation result exceeds the row limit",
                            ));
                        }
                        rows.push(row);
                    }
                    rows
                }
                SetOperator::Intersect | SetOperator::Except => {
                    let mut right_keys: HashSet<Vec<u8>> = HashSet::new();
                    for row in &r.rows {
                        // Issue #1952: 協調的中断の確認点（INTERSECT／EXCEPT の右辺キー構築の行ごと。commit 前の読み取り経路）。
                        crate::interrupt::check()?;
                        // 順序は上と同じ理由（長さ算出 → 予算確認 → 確保）。
                        budget.charge(row_key_len(row)?)?;
                        let key = row_key(row)?;
                        right_keys.insert(key);
                    }
                    let keep_if_present = matches!(op, SetOperator::Intersect);
                    let mut seen: HashSet<Vec<u8>> = HashSet::new();
                    let mut rows = Vec::new();
                    for row in l.rows.into_iter() {
                        // Issue #1952: 協調的中断の確認点（INTERSECT／EXCEPT の左辺突き合わせの行ごと。commit 前の読み取り経路）。
                        crate::interrupt::check()?;
                        // 順序は上と同じ理由（長さ算出 → 予算確認 → 確保）。
                        budget.charge(row_key_len(&row)?)?;
                        let key = row_key(&row)?;
                        let present = right_keys.contains(&key);
                        if present != keep_if_present || seen.contains(&key) {
                            continue;
                        }
                        if rows.len() >= MAX_SET_OP_ROWS {
                            return Err(SqlSurfaceError::payload_too_large(
                                "set operation result exceeds the row limit",
                            ));
                        }
                        seen.insert(key);
                        rows.push(row);
                    }
                    rows
                }
            };

            Ok(EvalOutcome {
                columns: merge_columns(&l.columns, &r.columns),
                rows,
                has_vector: l.has_vector,
            })
        }
    }
}

/// [`crate::sql::allowlist::ValidatedSetOperation`] を実行する（`core.rs::
/// EngineCore::execute_validated_in_session` の `Statement::SetOperation`
/// アーム・session-less `execute_sql` の同アームから呼ばれる）。`schemas` は
/// 呼び出し元が単一の `read_txn`（同一スナップショット）上で解決済みのものを
/// 渡す（`EngineCore::read_txn_with_schemas`）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    limit: Option<u32>,
    // Issue #1796: 全体 `OFFSET`（省略時 0）。評価後の行から先に読み飛ばす。
    offset: u32,
    // Issue #3266: 括弧なし末尾 `ORDER BY` のキー。
    order_by: &[SetOrderKey],
    // Issue #1550: 集計形の枝へ渡す `GROUP BY` の列数・グループ数上限（`EngineCore` 保持）。
    group_limits: crate::group_by_limits::GroupByLimits,
    // Issue #1555: 全体 `LIMIT` の範囲上限（`EngineCore` 保持。枝行数上限
    // `MAX_SET_OP_ROWS` とは連動しない）。
    result_limits: crate::result_limits::ResultLimits,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_with_budget_and_result_limits(
        read_txn,
        ctx,
        schemas,
        tree,
        udfs,
        limit,
        offset,
        order_by,
        crate::arena::MAX_ARENA_TOTAL_BYTES,
        group_limits,
        &result_limits,
    )
}

/// [`execute`] の本体。`budget_cap` は文全体で共有する累計バイト予算
/// （[`SetOpBudget`]）の上限。[`execute`] は既定値
/// （[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）を渡すだけの薄いラッパーで、
/// 小さい上限を注入できる回帰テスト（本モジュール内の `tests`）専用に分離して
/// いる（`sql::scan::execute_scan`／`execute_scan_with_budget` と同じ設計判断）。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_with_budget(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    limit: Option<u32>,
    budget_cap: usize,
    group_limits: crate::group_by_limits::GroupByLimits,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_with_budget_and_result_limits(
        read_txn,
        ctx,
        schemas,
        tree,
        udfs,
        limit,
        0,
        &[],
        budget_cap,
        group_limits,
        &crate::result_limits::ResultLimits::default(),
    )
}

/// [`execute_with_budget`] の本体（Issue #1555: 全体 `LIMIT` の範囲上限を注入できる版）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_with_budget_and_result_limits(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    limit: Option<u32>,
    offset: u32,
    order_by: &[SetOrderKey],
    budget_cap: usize,
    group_limits: crate::group_by_limits::GroupByLimits,
    result_limits: &crate::result_limits::ResultLimits,
) -> Result<QueryResult, SqlSurfaceError> {
    let (validated_limit, validated_offset, order_plan) = prepare(
        schemas,
        tree,
        udfs,
        limit,
        offset,
        order_by,
        result_limits,
        None,
    )?;
    let mut budget = SetOpBudget::new(budget_cap);
    let outcome = eval_tree(
        read_txn,
        ctx,
        schemas,
        tree,
        udfs,
        group_limits,
        &mut budget,
    )?;
    // Issue #3267: 全体 `ORDER BY` は合成結果が確定した後、`OFFSET`／`LIMIT` の直前に安定ソートする
    // （同値の行は合成結果の順序規約を保つ）。ソート用の領域は確保前に文全体の予算へ計上する。
    let mut outcome = outcome;
    if !order_plan.is_empty() {
        charge_sort_budget(&mut budget, outcome.rows.len(), order_plan.len())?;
        outcome.rows = sort_rows(outcome.rows, &order_plan)?;
    }
    // §2.4: 上限判定（`MAX_SET_OP_ROWS`）は全体 `LIMIT` による切り詰めより前に
    // 完結している（`eval_tree` が既に検証済み）。
    let rows = match validated_limit {
        Some(n) => outcome
            .rows
            .into_iter()
            .skip(validated_offset)
            .take(n)
            .collect(),
        None => {
            // Issue #1796・SQL-34: 全体 `LIMIT` の省略・`LIMIT ALL` は結果行数上限（設定値）を
            // 超えたら `54000`（黙って切り詰めない。`MAX_SET_OP_ROWS` は別軸の固定上限で、
            // 設定値を上げても 10,000 で頭打ちになる）。
            // OFFSET を先に適用し、読み飛ばし後の件数で判定する。
            let remaining = outcome.rows.len().saturating_sub(validated_offset);
            if remaining > result_limits.max_result_rows() {
                return Err(crate::sql::scan::row_limit_exceeded());
            }
            outcome.rows.into_iter().skip(validated_offset).collect()
        }
    };
    Ok(QueryResult {
        columns: outcome.columns,
        rows,
    })
}

/// Describe（拡張クエリプロトコル）向けに、検索本体を実行せず結果列メタデータ
/// だけを導出する。`bind_scan_with_dummy_flags` で枝を束縛し、[`execute`] と
/// 同じ型整合検証（§2.2）を行う（行走査は一切行わない）。`limit` は全体
/// `LIMIT`（任意）で、[`execute`] と同じ範囲検証（`1..=MAX_SEARCH_K`）を行う
/// （PR #1105 レビュー指摘対応: 従来は Describe が全体 `LIMIT` を一切検証せず、
/// Execute では拒否される範囲外の値が Describe だけ受理されてしまっていた）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn describe_columns(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    dummy_equality_flags: &[bool],
    limit: Option<u32>,
    offset: u32,
    order_by: &[SetOrderKey],
    result_limits: &crate::result_limits::ResultLimits,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    if let Some(n) = limit {
        crate::sql::parser::validate_search_limit_with(n, result_limits)?;
    }
    crate::sql::parser::validate_search_offset(offset)?;
    // `sql::params::where_equality_literal_is_param_typed` は集合演算で枝ごとのフラグを最長の
    // 枝へ `false` 埋めして枝順に連結する。枝数で等分できない場合は対応を取れないため、
    // 全枝で検証を省略しない（fail-closed）。
    let leaves = count_leaves(tree);
    let width = if leaves == 0 || !dummy_equality_flags.len().is_multiple_of(leaves) {
        0
    } else {
        dummy_equality_flags.len() / leaves
    };
    let mut next_leaf = 0usize;
    let (columns, _) = describe_tree(
        schemas,
        tree,
        udfs,
        dummy_equality_flags,
        width,
        &mut next_leaf,
    )?;
    // Issue #3266・#3267: Execute（[`prepare`]）と同じ計画関数を通し、Describe と Execute の
    // 受理・拒否を一致させる（計画自体は使わない）。
    plan_set_order_by(schemas, tree, &columns, order_by)?;
    Ok(columns)
}

/// 集合演算木の枝（葉）数。
fn count_leaves(tree: &SetTree) -> usize {
    match tree {
        SetTree::Op { left, right, .. } => count_leaves(left).saturating_add(count_leaves(right)),
        _ => 1,
    }
}

/// 枝順（左から）に `next_leaf` 番目の枝へ割り当てるダミーフラグの区画。
fn branch_flags<'a>(flags: &'a [bool], width: usize, next_leaf: &mut usize) -> &'a [bool] {
    let k = *next_leaf;
    *next_leaf = k.saturating_add(1);
    if width == 0 {
        return &[];
    }
    let start = k.saturating_mul(width);
    flags.get(start..start.saturating_add(width)).unwrap_or(&[])
}

fn describe_tree(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    dummy_equality_flags: &[bool],
    width: usize,
    next_leaf: &mut usize,
) -> Result<(Vec<ColumnMeta>, bool), SqlSurfaceError> {
    match tree {
        SetTree::Branch(validated) | SetTree::LimitedBranch(validated) => {
            let branch_dummy = branch_flags(dummy_equality_flags, width, next_leaf);
            let schema =
                schemas
                    .get(&validated.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                validated,
                schema,
                udfs,
                branch_dummy,
                // 枝の `LIMIT` は枝行数上限（`MAX_SET_OP_ROWS`）側で扱う独立上限のため
                // 既定値のまま（設定値とは連動させない。SQL-46 (6)）。
                &crate::result_limits::ResultLimits::default(),
            )?;
            let columns = crate::sql::describe::projected_columns(bound.projection(), schema);
            let has_vector = columns_have_vector(&columns);
            Ok((columns, has_vector))
        }
        SetTree::AggregateBranch(branch) => {
            let branch_dummy = branch_flags(dummy_equality_flags, width, next_leaf);
            let schema =
                schemas
                    .get(&branch.0.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            let bound = crate::sql::parser::bind_aggregate_with_dummy_flags(
                &branch.0,
                schema,
                udfs,
                branch_dummy,
            )?;
            let columns = crate::sql::describe::aggregate_columns(&bound, schema)?;
            let has_vector = columns_have_vector(&columns);
            Ok((columns, has_vector))
        }
        SetTree::ConstBranch(branch) => {
            // 定数枝もダミーフラグの区画を 1 枝分進める（`count_leaves` が葉として数えるため。
            // 進めないと後続の枝のフラグの対応がずれる）。
            let _ = branch_flags(dummy_equality_flags, width, next_leaf);
            // Execute（`eval_const_branch`／`validate_tree_types`）と同じ通常の `bind` を使い、
            // ルーチン呼び出しの 0A000 拒否を Describe と Execute で一致させる。
            let bound = crate::sql::const_select::bind(&branch.0, udfs)?;
            let columns = bound.columns();
            let has_vector = columns_have_vector(&columns);
            Ok((columns, has_vector))
        }
        SetTree::Op { op, left, right } => {
            let (l_columns, l_has_vector) =
                describe_tree(schemas, left, udfs, dummy_equality_flags, width, next_leaf)?;
            let (r_columns, r_has_vector) =
                describe_tree(schemas, right, udfs, dummy_equality_flags, width, next_leaf)?;
            check_branch_type_compatibility(
                *op,
                &l_columns,
                &r_columns,
                l_has_vector,
                r_has_vector,
            )?;
            Ok((merge_columns(&l_columns, &r_columns), l_has_vector))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::recovery::required_op_id::OperationId;
    use crate::row_codec::Value;
    use crate::sql::allowlist::{Projection, SetOperator, ValidatedScan};
    use crate::sql::udf_call::UdfRegistry;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    /// Issue #3270 レビュー対応: 別名付き `id`（`Cell::Integer`）と NUMERIC 列（`Cell::Numeric`）は
    /// 行キーのタグが異なるため互換にしない。`id` 同士は別名の有無にかかわらず互換。
    #[test]
    fn aliased_id_is_incompatible_with_numeric_but_compatible_with_id() {
        let aliased = ColumnMeta::AliasedId { name: "x".into() };
        let numeric = ColumnMeta::Scalar {
            name: "amount".into(),
            ty: ColumnType::Numeric {
                precision: 20,
                scale: 0,
            },
        };
        assert!(!columns_compatible(&aliased, &numeric));
        assert!(!columns_compatible(&numeric, &aliased));
        assert!(columns_compatible(&aliased, &ColumnMeta::Id));
        assert!(columns_compatible(&aliased, &aliased));
    }

    fn text_schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    fn insert_text_row(storage: &Storage, table: &str, tenant_ctx: &PolicyContext, body: &str) {
        let op_id = OperationId::parse(&format!("seed-{table}")).expect("valid operation_id");
        crate::tenant::insert_typed_row(
            storage,
            table,
            tenant_ctx,
            1,
            Visibility::Public,
            &[Value::Vector(vec![0.0, 0.0]), Value::Text(body.to_string())],
            &op_id,
        )
        .expect("insert row");
    }

    fn text_branch(table: &str) -> SetTree {
        SetTree::Branch(Box::new(ValidatedScan {
            table_name: table.to_string(),
            projection: Projection::Columns(vec!["body".to_string()]),
            where_predicates: Vec::new(),
            limit: crate::core::MAX_SEARCH_K as u32,
            limit_implicit: false,
            order_by: Vec::new(),
            offset: 0,
            window_items: Vec::new(),
            order_keys: Vec::new(),
            scalar_subquery_items: Vec::new(),
            physical_order_keys: Vec::new(),
            embedded_scalar_subqueries: Vec::new(),
            with_ties: false,
            distinct_on: None,
        }))
    }

    /// PR #1105 レビュー指摘の回帰: 各枝が独立に既定予算（1 GiB）を使うのではなく、
    /// 文全体で 1 つの累計バイト予算を共有すること。1 枝だけなら収まるが、2 枝を
    /// 合成すると超える上限を注入し、`UNION ALL`（重複除去なし。行キー生成の影響を
    /// 排除して枝単体の計上だけを確認する）が `54000` になることを確認する。
    #[test]
    fn shared_byte_budget_rejects_when_combined_branches_exceed_the_injected_cap() {
        let path = unique_db_path("set-op-shared-budget-reject");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        storage.create_table(&text_schema("b")).expect("create b");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let body = "x".repeat(4096);
        insert_text_row(&storage, "a", &tenant_ctx, &body);
        insert_text_row(&storage, "b", &tenant_ctx, &body);

        let tree = SetTree::Op {
            op: SetOperator::UnionAll,
            left: Box::new(text_branch("a")),
            right: Box::new(text_branch("b")),
        };
        let mut table_names = Vec::new();
        collect_branch_tables(&tree, &mut table_names);

        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), text_schema("b"));
        let udfs = UdfRegistry::default();

        // 1 枝分（約 4096 バイト＋構造体オーバーヘッド）は収まるが、2 枝合計は
        // 超える上限を注入する。
        let small_cap = 4096 + 512;
        let err = execute_with_budget(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            None,
            small_cap,
            crate::group_by_limits::GroupByLimits::default(),
        )
        .expect_err("combined branch bytes must exceed the injected shared budget");
        assert_eq!(err.wire_code(), "54000");
    }

    /// 上のちょうど対照: 2 枝合計が収まる上限を注入すれば成功することを確認する
    /// （回帰テストが常に失敗するだけの壊れた検証になっていないことの確認）。
    #[test]
    fn shared_byte_budget_succeeds_when_combined_branches_fit_the_injected_cap() {
        let path = unique_db_path("set-op-shared-budget-accept");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        storage.create_table(&text_schema("b")).expect("create b");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let body = "x".repeat(4096);
        insert_text_row(&storage, "a", &tenant_ctx, &body);
        insert_text_row(&storage, "b", &tenant_ctx, &body);

        let tree = SetTree::Op {
            op: SetOperator::UnionAll,
            left: Box::new(text_branch("a")),
            right: Box::new(text_branch("b")),
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), text_schema("b"));
        let udfs = UdfRegistry::default();

        let generous_cap = 1024 * 1024;
        let result = execute_with_budget(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            None,
            generous_cap,
            crate::group_by_limits::GroupByLimits::default(),
        )
        .expect("combined branch bytes must fit the generous injected budget");
        assert_eq!(result.rows.len(), 2);
    }

    /// PR #1105 レビュー指摘の回帰: 行キー生成（`UNION` の重複除去）も共有予算の
    /// 対象であること。2 枝分の行データ自体は収まるが、行キーの複製分を追加すると
    /// 超える上限では `UNION`（重複除去あり）が `54000` になることを確認する
    /// （`UNION ALL` は行キーを生成しないため同じ上限でも成功するはずだが、本
    /// テストは行キー計上の有無に絞って確認するため `UNION` 側のみを検証する）。
    #[test]
    fn shared_byte_budget_also_charges_row_key_generation() {
        let path = unique_db_path("set-op-shared-budget-row-key");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        storage.create_table(&text_schema("b")).expect("create b");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // UNION の重複除去では左右が別内容でないと両方が結果に残らないが、行キー
        // 生成自体は一致・不一致に関わらず両方の行に対して行うため、内容は異なる
        // ものにしておく。
        insert_text_row(&storage, "a", &tenant_ctx, &"x".repeat(4096));
        insert_text_row(&storage, "b", &tenant_ctx, &"y".repeat(4096));

        let tree = SetTree::Op {
            op: SetOperator::Union,
            left: Box::new(text_branch("a")),
            right: Box::new(text_branch("b")),
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), text_schema("b"));
        let udfs = UdfRegistry::default();

        // 2 枝分の行データ（約 8192 バイト＋構造体オーバーヘッド）はちょうど収まるが、
        // 行キー（各行の複製で約 8192 バイト追加）までは収まらない上限。
        let cap_without_row_key_room = 4096 * 2 + 1024;
        let err = execute_with_budget(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            None,
            cap_without_row_key_room,
            crate::group_by_limits::GroupByLimits::default(),
        )
        .expect_err("row key generation must be charged against the shared budget");
        assert_eq!(err.wire_code(), "54000");
    }

    /// Issue #3268: `INTERSECT ALL` の右辺カウント表構築も共有予算の対象で、
    /// 超過は `54000`、十分な上限なら多重度 min(左, 右) の件数で成功する。
    #[test]
    fn intersect_all_charges_budget_and_keeps_min_multiplicity() {
        let path = unique_db_path("set-op-intersect-all-budget");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        storage.create_table(&text_schema("b")).expect("create b");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let body = "x".repeat(4096);
        insert_text_row(&storage, "a", &tenant_ctx, &body);
        // 2 行目は id・operation_id を変えて同内容の重複行にする。
        crate::tenant::insert_typed_row(
            &storage,
            "a",
            &tenant_ctx,
            2,
            Visibility::Public,
            &[Value::Vector(vec![0.0, 0.0]), Value::Text(body.clone())],
            &OperationId::parse("seed-a-2").expect("valid operation_id"),
        )
        .expect("insert second row");
        insert_text_row(&storage, "b", &tenant_ctx, &body);

        let tree = SetTree::Op {
            op: SetOperator::IntersectAll,
            left: Box::new(text_branch("a")),
            right: Box::new(text_branch("b")),
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), text_schema("b"));
        let udfs = UdfRegistry::default();
        let run = |cap: usize| {
            execute_with_budget(
                &read_txn,
                &tenant_ctx,
                &schemas,
                &tree,
                &udfs,
                None,
                cap,
                crate::group_by_limits::GroupByLimits::default(),
            )
        };
        let err = run(4096 * 3 + 1024).expect_err("row keys must be charged");
        assert_eq!(err.wire_code(), "54000");
        let ok = run(1024 * 1024).expect("generous cap succeeds");
        assert_eq!(ok.rows.len(), 1);
    }

    /// Issue #3269: `EXCEPT ALL` も右辺の行キーを予算へ計上し、左 2 件・右 1 件で 1 件残す。
    #[test]
    fn except_all_charges_budget_and_keeps_difference_multiplicity() {
        let path = unique_db_path("set-op-except-all-budget");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        storage.create_table(&text_schema("b")).expect("create b");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let body = "x".repeat(4096);
        insert_text_row(&storage, "a", &tenant_ctx, &body);
        // 2 行目は id・operation_id を変えて同内容の重複行にする。
        crate::tenant::insert_typed_row(
            &storage,
            "a",
            &tenant_ctx,
            2,
            Visibility::Public,
            &[Value::Vector(vec![0.0, 0.0]), Value::Text(body.clone())],
            &OperationId::parse("seed-a-2").expect("valid operation_id"),
        )
        .expect("insert second row");
        insert_text_row(&storage, "b", &tenant_ctx, &body);

        let tree = SetTree::Op {
            op: SetOperator::ExceptAll,
            left: Box::new(text_branch("a")),
            right: Box::new(text_branch("b")),
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), text_schema("b"));
        let udfs = UdfRegistry::default();
        let run = |cap: usize| {
            execute_with_budget(
                &read_txn,
                &tenant_ctx,
                &schemas,
                &tree,
                &udfs,
                None,
                cap,
                crate::group_by_limits::GroupByLimits::default(),
            )
        };
        let err = run(4096 * 3 + 1024).expect_err("row keys must be charged");
        assert_eq!(err.wire_code(), "54000");
        let ok = run(1024 * 1024).expect("generous cap succeeds");
        assert_eq!(ok.rows.len(), 1);
    }

    /// PR #1105 レビュー指摘の回帰: 全体 `LIMIT` の範囲外検証（`22000`）は全枝の
    /// 走査より前に行う。存在しないテーブルを枝に含めても（走査すれば `XX000`
    /// 相当の内部エラーになるはずの構成）、範囲外 `LIMIT` が先に `22000` で拒否
    /// することを確認する。
    #[test]
    fn out_of_range_limit_is_rejected_before_branches_are_scanned() {
        let path = unique_db_path("set-op-limit-before-scan");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let tree = text_branch("missing_table");
        let read_txn = storage.db().begin_read().expect("begin_read");
        // 意図的にスキーマを解決しない（走査されれば `schemas.get` が `None` を
        // 返し `Internal` エラーになる構成）。
        let schemas: HashMap<String, TableSchema> = HashMap::new();
        let udfs = UdfRegistry::default();

        let err = execute(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            Some(0),
            0,
            &[],
            crate::group_by_limits::GroupByLimits::default(),
            crate::result_limits::ResultLimits::default(),
        )
        .expect_err("LIMIT 0 must be rejected before any branch is scanned");
        assert_eq!(err.wire_code(), "22000");

        let err = execute(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            Some(crate::core::MAX_SEARCH_K as u32 + 1),
            0,
            &[],
            crate::group_by_limits::GroupByLimits::default(),
            crate::result_limits::ResultLimits::default(),
        )
        .expect_err("LIMIT over MAX_SEARCH_K must be rejected before any branch is scanned");
        assert_eq!(err.wire_code(), "22000");
    }

    /// PR #1105 レビュー指摘の回帰: Describe 経路（`describe_columns`）も Execute
    /// と同じ全体 `LIMIT` 範囲検証を行う（従来は検証していなかった）。
    #[test]
    fn describe_columns_rejects_out_of_range_limit_like_execute() {
        let tree = text_branch("a");
        let schemas: HashMap<String, TableSchema> = HashMap::new();
        let udfs = UdfRegistry::default();

        let err = describe_columns(
            &schemas,
            &tree,
            &udfs,
            &[],
            Some(0),
            0,
            &[],
            &Default::default(),
        )
        .expect_err("LIMIT 0 must be rejected by describe_columns");
        assert_eq!(err.wire_code(), "22000");

        let err = describe_columns(
            &schemas,
            &tree,
            &udfs,
            &[],
            Some(crate::core::MAX_SEARCH_K as u32 + 1),
            0,
            &[],
            &Default::default(),
        )
        .expect_err("LIMIT over MAX_SEARCH_K must be rejected by describe_columns");
        assert_eq!(err.wire_code(), "22000");
    }

    fn int_schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("score", ColumnType::Integer, false),
            ],
        )
    }

    fn int_branch(table: &str) -> SetTree {
        SetTree::Branch(Box::new(ValidatedScan {
            table_name: table.to_string(),
            projection: Projection::Columns(vec!["score".to_string()]),
            where_predicates: Vec::new(),
            limit: crate::core::MAX_SEARCH_K as u32,
            limit_implicit: false,
            order_by: Vec::new(),
            offset: 0,
            window_items: Vec::new(),
            order_keys: Vec::new(),
            scalar_subquery_items: Vec::new(),
            physical_order_keys: Vec::new(),
            embedded_scalar_subqueries: Vec::new(),
            with_ties: false,
            distinct_on: None,
        }))
    }

    /// PR #1105 レビュー指摘の回帰（set_op.rs:353）: 列型不一致
    /// （`DatatypeMismatch`・`42804`）の判定は、`eval_tree` が左右の枝を実際に
    /// 走査するより前に確定していること。枝 `a`（TEXT 列）には実データを
    /// 投入し、枝 `b`（INTEGER 列）とは列型が一致しない構成にした上で、
    /// 予算上限を `0` にする（`a` を少しでも走査すれば必ず `54000` になる
    /// 構成）。従来（列型検証が `eval_tree` の走査後）は `54000` が先に成立して
    /// いたが、修正後は走査前の [`validate_tree_types`] が `42804` を確定させる
    /// ため、`54000` に先行されない。
    #[test]
    fn type_mismatch_is_rejected_before_any_branch_is_scanned_even_with_a_zero_budget() {
        let path = unique_db_path("set-op-type-mismatch-before-scan");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_text_row(&storage, "a", &tenant_ctx, "some text");

        let tree = SetTree::Op {
            op: SetOperator::UnionAll,
            left: Box::new(text_branch("a")),
            right: Box::new(int_branch("b")),
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), int_schema("b"));
        let udfs = UdfRegistry::default();

        let err = execute_with_budget(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            None,
            0,
            crate::group_by_limits::GroupByLimits::default(),
        )
        .expect_err("type mismatch must be rejected even with a zero-byte budget");
        assert_eq!(err.wire_code(), "42804");
    }

    /// Issue #2270: 値が等しく scale だけ異なる numeric（整数 `AVG` の値ごとの scale）は
    /// 重複排除キーで同一視され、値が異なるものは区別される。
    #[test]
    fn row_key_treats_numerics_with_different_scale_as_equal() {
        let key = |text: &str| {
            let d = crate::numeric::parse_literal_exact(text).expect("valid decimal");
            let row = ResultRow {
                id: 0,
                score: 0.0,
                cells: vec![Cell::Numeric(d)],
            };
            (row_key(&row).expect("key"), row_key_len(&row).expect("len"))
        };
        let (a, a_len) = key("1.5000000000000000");
        let (b, b_len) = key("1.50000000000000000000");
        assert_eq!(a, b);
        assert_eq!(a.len(), a_len);
        assert_eq!(b.len(), b_len);
        assert_ne!(key("1.5").0, key("1.51").0);
        assert_eq!(key("10").0, key("10.0000").0);
        assert_ne!(key("10").0, key("1").0);
    }

    /// PR #1105 レビュー指摘の回帰（set_op.rs:393・415・423）: [`row_key_len`]
    /// が算出する長さは、[`row_key`] が実際に確保する `Vec<u8>` の長さと
    /// 全 variant で一致すること（両者は変更時に必ず同時に保守する必要が
    /// あるため、この一致検証が乖離を検出する）。
    #[test]
    fn row_key_len_matches_row_key_output_length() {
        let uuid = crate::uuid::parse_uuid_text("550e8400-e29b-41d4-a716-446655440000")
            .expect("valid uuid literal");
        let decimal = crate::numeric::parse_literal_exact("123.45").expect("valid decimal literal");
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![
                Cell::Null,
                Cell::Integer(42),
                Cell::Text("hello world".to_string()),
                Cell::Float(3.5),
                Cell::Bool(true),
                Cell::SignedInteger(-7),
                Cell::Date(19_000),
                Cell::Timestamp(1_700_000_000_000_000),
                Cell::Bytes(vec![1, 2, 3, 4, 5]),
                Cell::Json("{\"a\":1}".to_string()),
                Cell::Numeric(decimal),
                Cell::Uuid(uuid),
                Cell::Array(crate::row_codec::ArrayValue::Text(vec![
                    Some("x".to_string()),
                    Some("yz".to_string()),
                ])),
                Cell::Array(crate::row_codec::ArrayValue::Bool(vec![
                    Some(true),
                    Some(false),
                    Some(true),
                ])),
            ],
        };

        let computed_len = row_key_len(&row).expect("row_key_len should succeed");
        let actual_len = row_key(&row).expect("row_key should succeed").len();
        assert_eq!(
            computed_len, actual_len,
            "row_key_len must predict the exact byte length row_key allocates"
        );
    }

    fn scalar(ty: ColumnType) -> ColumnMeta {
        ColumnMeta::Scalar {
            name: "c".to_string(),
            ty,
        }
    }

    fn merged_ty(l: ColumnType, r: ColumnType) -> ColumnType {
        match merge_columns(&[scalar(l)], &[scalar(r)]).remove(0) {
            ColumnMeta::Scalar { ty, .. } => ty,
            other => panic!("unexpected meta: {other:?}"),
        }
    }

    /// TEXT 系の異型合成は長さ指定を外し、同型は保持する（Issue #1565）。
    #[test]
    fn merge_columns_drops_varchar_length_on_mismatch() {
        use ColumnType::{Bpchar, Text, Varchar};
        assert_eq!(
            merged_ty(Varchar(Some(10)), Varchar(Some(20))),
            Varchar(None)
        );
        assert_eq!(merged_ty(Varchar(Some(10)), Text), Varchar(None));
        assert_eq!(merged_ty(Text, Varchar(Some(5))), Text);
        assert_eq!(
            merged_ty(Varchar(Some(10)), Varchar(Some(10))),
            Varchar(Some(10))
        );
        assert_eq!(merged_ty(Bpchar(5), Bpchar(5)), Bpchar(5));
        assert_eq!(merged_ty(Bpchar(5), Bpchar(10)), Text);
        assert_eq!(merged_ty(Bpchar(5), Text), Text);
        assert_eq!(merged_ty(Bpchar(5), Varchar(None)), Text);
    }

    fn tstz_schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("at", ColumnType::TimestampTz, true),
            ],
        )
    }

    fn tstz_branch(table: &str) -> SetTree {
        SetTree::Branch(Box::new(ValidatedScan {
            table_name: table.to_string(),
            projection: Projection::Columns(vec!["at".to_string()]),
            where_predicates: Vec::new(),
            limit: crate::core::MAX_SEARCH_K as u32,
            limit_implicit: false,
            order_by: Vec::new(),
            offset: 0,
            window_items: Vec::new(),
            order_keys: Vec::new(),
            scalar_subquery_items: Vec::new(),
            physical_order_keys: Vec::new(),
            embedded_scalar_subqueries: Vec::new(),
            with_ties: false,
            distinct_on: None,
        }))
    }

    /// Issue #1576: TIMESTAMPTZ 列の集合演算は演算 4 種・データ 3 状況（空表・NULL のみ・非 NULL
    /// 値あり）・実行／Describe の両経路で成功し、重複除去は UTC の時点（値）で行う。
    #[test]
    fn timestamptz_set_operations_dedup_by_instant_for_all_ops_and_data_states() {
        let ops = [
            SetOperator::Union,
            SetOperator::UnionAll,
            SetOperator::Intersect,
            SetOperator::IntersectAll,
            SetOperator::Except,
            SetOperator::ExceptAll,
        ];
        for state in ["empty", "null-only", "non-null"] {
            let path = unique_db_path(&format!("set-op-tstz-{state}"));
            let storage = Storage::open(&path).expect("open storage");
            let _guard = CleanupGuard(path);
            storage.create_table(&tstz_schema("a")).expect("create a");
            storage.create_table(&tstz_schema("b")).expect("create b");
            let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
            let value = match state {
                "null-only" => Some(Value::Null),
                "non-null" => Some(Value::TimestampTz(1_000_000)),
                _ => None,
            };
            if let Some(v) = value {
                for t in ["a", "b"] {
                    let op_id = OperationId::parse(&format!("seed-{t}")).expect("valid op id");
                    crate::tenant::insert_typed_row(
                        &storage,
                        t,
                        &tenant_ctx,
                        1,
                        Visibility::Public,
                        &[Value::Vector(vec![0.0, 0.0]), v.clone()],
                        &op_id,
                    )
                    .expect("insert row");
                }
            }
            let mut schemas = HashMap::new();
            schemas.insert("a".to_string(), tstz_schema("a"));
            schemas.insert("b".to_string(), tstz_schema("b"));
            let udfs = UdfRegistry::default();
            for op in ops {
                let tree = SetTree::Op {
                    op,
                    left: Box::new(tstz_branch("a")),
                    right: Box::new(tstz_branch("b")),
                };
                let read_txn = storage.db().begin_read().expect("begin_read");
                let result = execute_with_budget(
                    &read_txn,
                    &tenant_ctx,
                    &schemas,
                    &tree,
                    &udfs,
                    None,
                    usize::MAX,
                    crate::group_by_limits::GroupByLimits::default(),
                )
                .unwrap_or_else(|e| panic!("execute {op:?} {state}: {e:?}"));
                // 両枝は同一の 1 行（空表なら 0 行）。
                let present = usize::from(state != "empty");
                let expected = match op {
                    SetOperator::UnionAll => 2 * present,
                    SetOperator::Union | SetOperator::Intersect | SetOperator::IntersectAll => {
                        present
                    }
                    SetOperator::Except | SetOperator::ExceptAll => 0,
                };
                assert_eq!(result.rows.len(), expected, "{op:?} {state}");
                describe_columns(
                    &schemas,
                    &tree,
                    &udfs,
                    &[],
                    None,
                    0,
                    &[],
                    &Default::default(),
                )
                .unwrap_or_else(|e| panic!("describe {op:?} {state}: {e:?}"));
            }
        }
    }

    fn named(name: &str) -> ColumnMeta {
        ColumnMeta::Scalar {
            name: name.to_string(),
            ty: ColumnType::Text,
        }
    }

    fn ord_key(position: i64, descending: bool) -> SetOrderKey {
        SetOrderKey::Ordinal {
            position,
            descending,
            nulls: None,
        }
    }

    /// Issue #3266: 序数は `1..=列数` だけを受理し、境界の外は `42P10`。
    #[test]
    fn resolve_order_keys_checks_ordinal_bounds() {
        let cols = vec![named("a"), named("b")];
        for bad in [0, 3, -1] {
            let err = resolve_order_keys(&cols, &[ord_key(bad, false)]).expect_err("out of range");
            assert_eq!(err.wire_code(), "42P10", "{bad}");
        }
        let ok = resolve_order_keys(&cols, &[ord_key(1, false), ord_key(2, true)]).unwrap();
        assert_eq!((ok[0].index, ok[1].index), (0, 1));
    }

    /// Issue #3266: 列名の一致・不一致（`42703`）・重複（`42702`）と `NULLS` 既定値の確定。
    #[test]
    fn resolve_order_keys_resolves_names_and_defaults_nulls() {
        let cols = vec![named("a"), named("b"), named("a")];
        let col = |n: &str, d: bool| SetOrderKey::Column {
            name: n.to_string(),
            descending: d,
            nulls: None,
        };
        assert_eq!(
            resolve_order_keys(&cols, &[col("zz", false)])
                .expect_err("unknown")
                .wire_code(),
            "42703"
        );
        assert_eq!(
            resolve_order_keys(&cols, &[col("a", false)])
                .expect_err("duplicate")
                .wire_code(),
            "42702"
        );
        let ok = resolve_order_keys(&cols, &[col("b", false), col("b", true)]).unwrap();
        assert_eq!(ok[0].nulls, NullPlacement::Last);
        assert_eq!(ok[1].nulls, NullPlacement::First);
        let explicit = SetOrderKey::Column {
            name: "b".to_string(),
            descending: false,
            nulls: Some(NullPlacement::First),
        };
        assert_eq!(
            resolve_order_keys(&cols, &[explicit]).unwrap()[0].nulls,
            NullPlacement::First
        );
    }

    /// Issue #3267: 解決済みキーは比較計画（位置・降順・NULL 位置）になり、キー無しは空の計画。
    /// 解決の誤りは型検査より先に `42P10`。
    #[test]
    fn plan_set_order_by_returns_resolved_key_plans() {
        let schemas = HashMap::new();
        let tree = text_branch("a");
        let cols = vec![named("a"), named("b")];
        assert!(plan_set_order_by(&schemas, &tree, &cols, &[])
            .expect("no keys")
            .is_empty());
        let plans = plan_set_order_by(
            &schemas,
            &tree,
            &cols,
            &[ord_key(2, true), ord_key(1, false)],
        )
        .expect("plans");
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].index, 1);
        assert!(plans[0].descending);
        assert_eq!(plans[0].nulls, NullPlacement::First);
        assert_eq!(plans[1].index, 0);
        assert_eq!(plans[1].nulls, NullPlacement::Last);
        let err = plan_set_order_by(&schemas, &tree, &cols, &[ord_key(3, false)])
            .expect_err("resolve first");
        assert_eq!(err.wire_code(), "42P10");
    }

    /// Issue #3267: 比較できない型（型を決められない式列・`VECTOR`）のキーは `0A000`。
    #[test]
    fn plan_set_order_by_rejects_unsortable_column_types() {
        let schemas = HashMap::new();
        let tree = text_branch("a");
        for meta in [
            ColumnMeta::Scalar {
                name: "v".to_string(),
                ty: ColumnType::Vector(2),
            },
            ColumnMeta::Computed {
                name: "v".to_string(),
                ty: None,
            },
        ] {
            let err = plan_set_order_by(&schemas, &tree, &[meta], &[ord_key(1, false)])
                .expect_err("unsortable");
            assert_eq!(err.wire_code(), "0A000");
        }
    }

    /// Issue #3267: 非バイト列順の照合列を持つ表がある文の `TEXT` キーは `0A000`
    /// （照合を運ばずにバイト列順で比べない）。キーが `TEXT` でなければ影響しない。
    #[test]
    fn plan_set_order_by_rejects_text_key_with_non_byte_order_collation() {
        let mut schemas = HashMap::new();
        schemas.insert(
            "a".to_string(),
            TableSchema::new(
                "a",
                vec![ColumnDef::new("body", ColumnType::Text, false)
                    .with_collation(crate::collation::Collation::UndIcu)],
            ),
        );
        let tree = text_branch("a");
        let err = plan_set_order_by(&schemas, &tree, &[named("body")], &[ord_key(1, false)])
            .expect_err("collation");
        assert_eq!(err.wire_code(), "0A000");
        let int_col = ColumnMeta::Scalar {
            name: "n".to_string(),
            ty: ColumnType::Integer,
        };
        assert!(plan_set_order_by(&schemas, &tree, &[int_col], &[ord_key(1, false)]).is_ok());
    }

    /// Issue #3267: ソート領域の計上は文全体の予算を超えたら `54000`（確保前に判定）。
    #[test]
    fn sort_budget_is_charged_and_overflow_is_rejected() {
        let mut budget = SetOpBudget::new(1 << 20);
        charge_sort_budget(&mut budget, 10, 2).expect("small sort fits");
        assert!(budget.used > 0);
        let mut small = SetOpBudget::new(64);
        let err = charge_sort_budget(&mut small, 1000, 1).expect_err("over budget");
        assert_eq!(err.wire_code(), "54000");
        let mut huge = SetOpBudget::new(usize::MAX);
        let err = charge_sort_budget(&mut huge, usize::MAX, usize::MAX).expect_err("overflow");
        assert_eq!(err.wire_code(), "54000");
    }
}
