//! 相関 `EXISTS` の半結合プローブ（SQL-61 ポインタ、Issue #1842、親 #1840。RLS-10 ポインタ）。
//!
//! 責務境界: `sql::correlated_exists` が内側クエリを 1 回だけ実体化（外側と同じ
//! `PolicyContext`・同じ `read_txn` の走査経路。RLS 暗黙適用）して作る [`CorrelatedProbe`]
//! を保持し、外側の行ごとに「内側に条件を満たす行が `OFFSET` 件を超えて存在するか」を判定する。
//! `WherePredicate::CorrelatedExists`（内部専用の葉）→ `BoundExpr::CorrelatedExists` →
//! `ExprStep::CorrelatedExists` の経路で、外側の走査（`sql::scan`・`sql::aggregate` 等）の
//! 式述語評価から [`CorrelatedProbe::eval`] が呼ばれる。第 2 の述語評価器は作らない。
//!
//! 2 つの評価方式（`sql::correlated_exists` が分解結果から選ぶ）:
//! - **ハッシュ半結合**: 等価の相関述語（`内側列 = 外側列`）だけで書ける形。内側行を結合キーの
//!   正準バイト列ごとの件数へまとめておき、外側の行は自身のキーで 1 回引くだけで判定する
//!   （`IN (SELECT ...)` の集合照合のような値数上限を受け継がない）。キーの型クラスと
//!   正準化は `sql::join::values` を再利用し、結合の 2 系統を作らない。
//! - **行ごとの再評価**: 非等価・外側と内側が混ざった `OR` などの残余述語がある形。等価キーが
//!   あれば該当バケットの内側行だけ、無ければ全内側行に対して残余を既存の述語評価器
//!   （`where_tree`）で評価する。再評価の回数と比較回数は文全体で共有する予算で頭打ちにし、
//!   超過は `54000` で文全体を拒否する（部分結果を返さない。fail-closed）。
//!
//! NULL 規則: 外側キーに NULL があれば偽（`EXISTS` は UNKNOWN にならない）。内側キーに NULL を
//! 含む行はバケットへ入れない（`=` は NULL で真にならない）。
//!
//! 反結合（`NOT EXISTS`、Issue #1843）: 同じプローブの判定を [`CorrelatedProbe::eval`] の出口で
//! 反転するだけで、構築・予算・件数の扱いは `EXISTS` と共通。`EXISTS` は UNKNOWN にならないため
//! 単純な否定になり、外側キーが NULL の行は真（返る）、`OFFSET k` は「一致が `k` 件以下」になる。
//!
//! 相関 `IN`／`NOT IN`（Issue #1844）: 肯定形 `x IN (SELECT y ...)` は、WHERE 文脈で UNKNOWN が偽に
//! 潰れるため `EXISTS (... AND y = x)` と同値で、対象ペア `(y, x)` を等価キーへ足した
//! [`ProbeKind::Exists`] で実行する。`NOT IN` は NULL を考慮した [`ProbeKind::NotIn`] で実行する
//! （`NOT EXISTS (... AND y = x)` の流用は NULL を誤るため不可）。外側 1 行について、S を
//! 「相関条件を満たす内側可視行の投影値 y の多重集合」とすると、値は次のとおり:
//! S が空なら真（x が NULL でも真）／x が NULL なら偽／x が S にあれば偽／S に NULL があれば偽／
//! それ以外は真。
//!
//! 集計形の相関スカラー比較（Issue #1846。親 #1845）: `col <op> (SELECT agg ... WHERE b.k = a.k)` は、
//! `sql::subquery` が内側を相関キーの `GROUP BY` 集計へ書き換えて 1 回だけ実行し、グループ値ごとに
//! 既存の述語評価器で外側の列を比較する述語（[`ScalarTable`]）を束縛して渡す。本モジュールは
//! 外側の行の相関キーでグループの述語を 1 つ引いて評価するだけ（[`ProbeKind::ScalarCompare`]）。
//! 該当する内側行が無い外側の行（キーが NULL の行を含む）は空集合に対する集計値の述語で評価する
//! （`count` は 0、それ以外は NULL＝常に偽）。内側のキーが NULL のグループはどの外側の行にも一致しない。
//!
//! 非集計形の相関スカラー（Issue #1847。親 #1845）: `col <op> (SELECT y ... WHERE b.k = a.k)` と
//! `SELECT ..., (SELECT y ... WHERE b.k = a.k)` は、内側を 1 回だけ実体化し（外側と同じ
//! `PolicyContext`・同じ `read_txn`。RLS 暗黙適用）、外側の行ごとに「一致行」（等価キーのバケット、
//! キーが無ければ全内側行のうち残余が真の行）を数えて値を決める（[`ProbeKind::ScalarRows`]）。
//! 内側 `OFFSET k`／`LIMIT n` は一致行の先頭 k 件を飛ばし最大 n 件を採る意味で保存する。採った件数が
//! 0 件なら NULL（WHERE 位置では偽）、1 件ならその行の値、2 件以上なら `21000`。2 件目の一致が見つかった
//! 時点で走査を打ち切る。`21000` は外側の行を評価したときにだけ発生し（外側が 0 行なら発生しない）、
//! 他テナントの行は内側に現れないため基数に影響しない。残余があるか等価キーが無い形は外側 1 行ごとに
//! 再評価 1 回を計上し、走査した内側行ごとに比較 1 回を計上する（超過は `54000`）。
//!
//! 情報漏えい対策: 内側行はすべて呼び出したセッションの可視行だけ。[`CorrelatedProbeRef`] の
//! `Debug` は行データを一切出さない。エラー文言は静的文字列のみ。バケットの反復順は結果に
//! 影響しない（バケット内は走査順の `Vec`、ハッシュ表は引くだけで走査しない）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::catalog::ColumnType;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::expr_program::StackValue;
use crate::sql::where_tree::BoundOrGroup;

use super::values::{cell_scalar, encode_key_component, key_cell_from_scalar, JoinKeyClass};

/// 相関サブクエリの再評価回数のハード上限（Issue #1842・SQL-61 ポインタ。設計記録
/// `docs/design/sql-subquery.md`）。値は [`crate::correlated_limits`] が単一情報源で、
/// CLI（`--max-correlated-reevaluations`。Issue #1847）はこれを下げる方向にだけ設定できる。
#[cfg(test)]
pub(crate) const MAX_CORRELATED_REEVALUATIONS: usize =
    crate::correlated_limits::MAX_CORRELATED_REEVALUATIONS;

/// 文全体（入れ子の相関 `EXISTS` を含む）で共有する再評価・比較回数の予算。
/// `sql::correlated_exists::CorrelatedCtx` が 1 文の実行ごとに 1 つ作り、各プローブが
/// `Arc` で共有する。カウンタはスレッド間共有を想定しない（1 文は 1 スレッドで評価される）が、
/// `Arc` 越しに `&self` で更新するため原子型を使う。
#[derive(Debug)]
pub(crate) struct CorrelatedBudget {
    reevaluations: AtomicUsize,
    comparisons: AtomicUsize,
    max_reevaluations: usize,
    max_comparisons: usize,
}

impl CorrelatedBudget {
    pub(crate) fn new(max_reevaluations: usize, max_comparisons: usize) -> Self {
        Self {
            reevaluations: AtomicUsize::new(0),
            comparisons: AtomicUsize::new(0),
            max_reevaluations,
            max_comparisons,
        }
    }

    fn charge(
        counter: &AtomicUsize,
        n: usize,
        cap: usize,
        what: &'static str,
    ) -> Result<(), SqlSurfaceError> {
        counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(n).filter(|t| *t <= cap)
            })
            .map(|_| ())
            .map_err(|_| SqlSurfaceError::payload_too_large(what))
    }

    /// 残余述語を評価する外側の行 1 行ぶんを再評価 1 回として計上する。
    fn charge_reevaluation(&self) -> Result<(), SqlSurfaceError> {
        Self::charge(
            &self.reevaluations,
            1,
            self.max_reevaluations,
            "correlated subquery re-evaluation count exceeds the limit",
        )
    }

    /// 外側の行と内側の行の 1 組を比較 1 回として計上する。
    fn charge_comparison(&self) -> Result<(), SqlSurfaceError> {
        Self::charge(
            &self.comparisons,
            1,
            self.max_comparisons,
            "correlated subquery comparison count exceeds the limit",
        )
    }
}

/// 外側の行から値を取り出す元（外側スキーマ上の論理列インデックス、または行 `id`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OuterSource {
    Column(usize),
    Id,
}

/// 結合キー 1 成分の外側側の取り出し元とキーの型クラス。
#[derive(Debug, Clone)]
pub(crate) struct OuterKey {
    pub(crate) source: OuterSource,
    pub(crate) class: JoinKeyClass,
}

/// 非集計形の相関スカラー（Issue #1847）の内側 1 行が出す値。WHERE 位置は外側の列を比較する述語の
/// 添字（[`ProbeRows::ScalarRows`] の `predicates`）、投影位置は投影値のセル。可視行の内容を持つため
/// `Debug` は導出しない。
pub(crate) enum ScalarOut {
    Pred(u32),
    Value(Cell),
}

/// 内側の 1 行（残余述語の評価に必要な列だけを保持する）。内側の可視行の内容を持つため `Debug` は
/// 導出しない（エラー・ログ経由の漏えい防止）。
struct InnerRow {
    id: u64,
    cells: Vec<Cell>,
    /// `NOT IN` の対象値（内側投影列 y）の正準バイト列。`None` は NULL。`NotIn` 以外では未使用。
    value: Option<Vec<u8>>,
    /// 非集計形の相関スカラーの出力値。`ScalarRows` 以外では `None`。
    scalar: Option<ScalarOut>,
}

/// `NOT IN`（残余なし）のバケット。同じ相関キーを持つ内側行（= S）の投影値の集合。
/// 可視行の内容を持つため `Debug` は導出しない。
struct NotInBucket {
    /// S に NULL の投影値を含むか。
    has_null: bool,
    /// S の非 NULL 投影値（正準バイト列）。
    values: HashSet<Vec<u8>>,
}

/// 内側行の保持形。残余が無い（半結合）ときは件数だけ、あるときは行そのものを持つ（`Debug` 非導出）。
enum ProbeRows {
    Counts(HashMap<Vec<u8>, usize>),
    Rows {
        rows: Vec<InnerRow>,
        by_key: HashMap<Vec<u8>, Vec<u32>>,
    },
    /// `NOT IN` で残余が無い形（相関キーごとの [`NotInBucket`]）。
    NotIn(HashMap<Vec<u8>, NotInBucket>),
    /// 集計形の相関スカラー比較（Issue #1846。グループごとに束縛済みの比較述語）。
    Scalar(ScalarTable),
    /// 非集計形の相関スカラー（Issue #1847）。等価キーのバケット（キーが無ければ全行が 1 バケット）。
    /// `predicates` は WHERE 位置のときだけ非空（内側行の値ごとに束縛した外側列の比較述語）。
    ScalarRows {
        rows: Vec<InnerRow>,
        by_key: HashMap<Vec<u8>, Vec<u32>>,
        predicates: Vec<BoundOrGroup>,
    },
}

/// 集計形の相関スカラー比較（Issue #1846）の述語表。`sql::subquery` が内側の `GROUP BY` 集計の
/// 結果（グループ値）から組み立てる。可視行由来の値を束縛済みの述語として保持するため `Debug` は
/// 導出しない（エラー・ログ経由の漏えい防止）。
pub(crate) struct ScalarTable {
    /// 相関キーの正準バイト列 → `predicates` の添字（キーが NULL のグループは入れない）。
    pub(crate) by_key: HashMap<Vec<u8>, u32>,
    /// 外側の列を比較する束縛済みの述語（グループ値の distinct ごと。NULL は常に偽）。
    pub(crate) predicates: Vec<BoundOrGroup>,
    /// 該当する内側のグループが無い外側の行に使う述語（空集合に対する集計値との比較）の添字。
    pub(crate) empty_set: u32,
}

/// プローブの判定方式（Issue #1844）。`eval` は網羅的に `match` し、取りこぼしで述語が
/// 落ちる fail-open を作らない。
#[derive(Debug, Clone)]
pub(crate) enum ProbeKind {
    /// `EXISTS`（`negated` なら `NOT EXISTS`）。肯定形の相関 `IN` もこれ（対象ペアは等価キー）。
    Exists { negated: bool },
    /// `NOT IN`（NULL を考慮した反結合）。`target` は外側の対象値 x の取り出し元と型クラス。
    NotIn { target: OuterKey },
    /// 集計形の相関スカラー比較（Issue #1846）。述語表は [`ProbeRows::Scalar`] が持つ。
    ScalarCompare,
    /// 非集計形の相関スカラー（Issue #1847）。`offset`／`limit` は内側 `OFFSET`／`LIMIT`
    /// （`limit` が `None` は上限なし）で、一致行の先頭 `offset` 件を飛ばし最大 `limit` 件を採る。
    ScalarRows { offset: usize, limit: Option<usize> },
}

/// 残余述語の評価器。内側スキーマに外側参照ごとの列を足した合成スキーマで束縛した結果を
/// 保持し、1 組ごとに合成行（内側スカラー ＋ 外側スカラー）を組んで `where_tree` を呼ぶ。
#[derive(Debug)]
pub(crate) struct Residual {
    pub(crate) group: BoundOrGroup,
    /// `(内側行のセル位置, 合成スキーマ上の列インデックス, 列型)`。
    pub(crate) inner_cols: Vec<(usize, usize, ColumnType)>,
    /// `(合成スキーマ上の列インデックス, 外側の取り出し元, 外側の列型)`。
    /// 疑似列 `id` の列型は `BigInt`（`u64` → `i64` は checked 変換）。
    pub(crate) outer_cols: Vec<(usize, OuterSource, ColumnType)>,
    /// 合成スキーマの列数。
    pub(crate) width: usize,
}

/// 外側の行ごとに判定する相関 `EXISTS` のプローブ。[`CorrelatedProbeRef`] 越しに共有される。
/// 内側の可視行を保持するため `Debug` は導出しない。
pub struct CorrelatedProbe {
    keys: Vec<OuterKey>,
    rows: ProbeRows,
    residual: Option<Residual>,
    /// 一致が必要な最小件数（内側 `OFFSET k` の意味の保存。`k + 1`）。
    min_matches: usize,
    /// 判定方式。`Exists { negated }` は `eval` の出口で判定を反転する（Issue #1843）。
    kind: ProbeKind,
    budget: Arc<CorrelatedBudget>,
    /// 外側スキーマで評価に必要な論理列インデックス（キーと残余が参照する列。重複なし・昇順）。
    outer_columns: Vec<usize>,
}

/// 内側行の入力（`sql::correlated_exists` が走査結果から作る）。
pub(crate) struct InnerInput {
    pub(crate) id: u64,
    /// 結合キー成分のセル（`keys` と同じ順）。
    pub(crate) key_cells: Vec<Cell>,
    /// 残余の評価に必要な内側列のセル（`Residual::inner_cols` と同じ順）。
    pub(crate) cells: Vec<Cell>,
    /// `NOT IN` の対象値（内側投影列 y）のセル。`ProbeKind::NotIn` のときだけ `Some`。
    pub(crate) value_cell: Option<Cell>,
    /// 非集計形の相関スカラーの出力値（[`ProbeKind::ScalarRows`] のときだけ `Some`）。
    pub(crate) scalar: Option<ScalarOut>,
}

/// 非集計形の相関スカラー（Issue #1847）のプローブの構築パラメータ。
pub(crate) struct ScalarRowsSpec {
    pub(crate) keys: Vec<OuterKey>,
    pub(crate) residual: Option<Residual>,
    pub(crate) offset: usize,
    pub(crate) limit: Option<usize>,
    /// WHERE 位置の比較述語表（投影位置は空）。
    pub(crate) predicates: Vec<BoundOrGroup>,
    /// キー・残余以外に外側からデコードが要る列（WHERE 位置の比較対象列）。
    pub(crate) extra_outer_columns: Vec<usize>,
    pub(crate) budget: Arc<CorrelatedBudget>,
    pub(crate) byte_cap: usize,
}

/// プローブの構築パラメータ。
pub(crate) struct ProbeSpec {
    pub(crate) keys: Vec<OuterKey>,
    pub(crate) residual: Option<Residual>,
    pub(crate) min_matches: usize,
    /// 判定方式（`EXISTS`／`NOT EXISTS`／`NOT IN`）。
    pub(crate) kind: ProbeKind,
    pub(crate) budget: Arc<CorrelatedBudget>,
    /// 内側行の保持に使える推定バイト量の上限（確保前に計上して超過なら `54000`）。
    pub(crate) byte_cap: usize,
}

pub(crate) fn key_bytes_overhead(key: &[u8]) -> usize {
    key.len()
        .saturating_add(std::mem::size_of::<Vec<u8>>())
        .saturating_add(std::mem::size_of::<usize>())
}

pub(crate) fn too_large() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large("correlated subquery input exceeds the byte budget")
}

impl CorrelatedProbe {
    /// 内側行（可視行だけ）からプローブを組み立てる。キーに NULL を含む行は入れない。
    /// 確保は予算へ計上してから行い、超過は `54000`（`try_reserve` で失敗しても同じ）。
    pub(crate) fn build(
        spec: ProbeSpec,
        inputs: impl IntoIterator<Item = Result<InnerInput, SqlSurfaceError>>,
    ) -> Result<Self, SqlSurfaceError> {
        let ProbeSpec {
            keys,
            residual,
            min_matches,
            kind,
            budget,
            byte_cap,
        } = spec;
        if matches!(kind, ProbeKind::ScalarRows { .. }) {
            // 非集計形の相関スカラーは [`Self::scalar_rows`] で組み立てる。破れたら fail-closed。
            return Err(SqlSurfaceError::Internal {
                detail: "correlated scalar rows probe was built through the generic path"
                    .to_string(),
            });
        }
        let mut used = 0usize;
        let mut charge = |n: usize| -> Result<(), SqlSurfaceError> {
            used = used.saturating_add(n);
            if used > byte_cap {
                return Err(too_large());
            }
            Ok(())
        };
        let mut counts: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut rows: Vec<InnerRow> = Vec::new();
        let mut by_key: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
        let mut buckets: HashMap<Vec<u8>, NotInBucket> = HashMap::new();
        let not_in_class = match &kind {
            ProbeKind::NotIn { target } => Some(target.class.clone()),
            ProbeKind::Exists { .. } | ProbeKind::ScalarCompare | ProbeKind::ScalarRows { .. } => {
                None
            }
        };
        let keep_rows = residual.is_some();
        'rows: for input in inputs {
            let input = input?;
            let mut key = Vec::new();
            if input.key_cells.len() != keys.len() {
                return Err(SqlSurfaceError::Internal {
                    detail: "correlated key arity mismatch".to_string(),
                });
            }
            for (cell, k) in input.key_cells.iter().zip(&keys) {
                if matches!(cell, Cell::Null) {
                    continue 'rows;
                }
                encode_key_component(cell, &k.class, &mut key)?;
            }
            // `NOT IN` の対象値 y（NULL は `None`）。相関キーが NULL の行は上で除外済み。
            let value: Option<Vec<u8>> = match (&not_in_class, &input.value_cell) {
                (Some(class), Some(cell)) if !matches!(cell, Cell::Null) => {
                    let mut v = Vec::new();
                    encode_key_component(cell, class, &mut v)?;
                    Some(v)
                }
                (Some(_), Some(_)) => None,
                (Some(_), None) => {
                    return Err(SqlSurfaceError::Internal {
                        detail: "correlated NOT IN input is missing the target value".to_string(),
                    })
                }
                (None, _) => None,
            };
            if not_in_class.is_some() && !keep_rows {
                match buckets.get_mut(&key) {
                    Some(b) => Self::add_not_in_value(b, value, &mut charge)?,
                    None => {
                        charge(
                            key_bytes_overhead(&key)
                                .saturating_add(std::mem::size_of::<NotInBucket>()),
                        )?;
                        buckets.try_reserve(1).map_err(|_| too_large())?;
                        let mut b = NotInBucket {
                            has_null: false,
                            values: HashSet::new(),
                        };
                        Self::add_not_in_value(&mut b, value, &mut charge)?;
                        buckets.insert(key, b);
                    }
                }
            } else if keep_rows {
                let idx = u32::try_from(rows.len()).map_err(|_| too_large())?;
                let cell_bytes = input
                    .cells
                    .iter()
                    .map(super::cell_payload_bytes)
                    .fold(0usize, usize::saturating_add);
                charge(
                    std::mem::size_of::<InnerRow>()
                        .saturating_add(
                            input
                                .cells
                                .len()
                                .saturating_mul(std::mem::size_of::<Cell>()),
                        )
                        .saturating_add(cell_bytes)
                        .saturating_add(value.as_ref().map_or(0, |v| key_bytes_overhead(v)))
                        .saturating_add(std::mem::size_of::<u32>()),
                )?;
                rows.try_reserve(1).map_err(|_| too_large())?;
                rows.push(InnerRow {
                    id: input.id,
                    cells: input.cells,
                    value,
                    scalar: None,
                });
                match by_key.get_mut(&key) {
                    Some(v) => {
                        v.try_reserve(1).map_err(|_| too_large())?;
                        v.push(idx);
                    }
                    None => {
                        charge(key_bytes_overhead(&key))?;
                        by_key.try_reserve(1).map_err(|_| too_large())?;
                        by_key.insert(key, vec![idx]);
                    }
                }
            } else {
                match counts.get_mut(&key) {
                    Some(n) => *n = n.saturating_add(1),
                    None => {
                        charge(key_bytes_overhead(&key))?;
                        counts.try_reserve(1).map_err(|_| too_large())?;
                        counts.insert(key, 1);
                    }
                }
            }
        }
        let mut outer_columns: Vec<usize> = keys
            .iter()
            .chain(match &kind {
                ProbeKind::NotIn { target } => Some(target),
                ProbeKind::Exists { .. }
                | ProbeKind::ScalarCompare
                | ProbeKind::ScalarRows { .. } => None,
            })
            .filter_map(|k| match k.source {
                OuterSource::Column(i) => Some(i),
                OuterSource::Id => None,
            })
            .collect();
        if let Some(r) = &residual {
            outer_columns.extend(r.outer_cols.iter().filter_map(|(_, src, _)| match src {
                OuterSource::Column(i) => Some(*i),
                OuterSource::Id => None,
            }));
        }
        outer_columns.sort();
        outer_columns.dedup();
        Ok(Self {
            keys,
            rows: if keep_rows {
                ProbeRows::Rows { rows, by_key }
            } else if not_in_class.is_some() {
                ProbeRows::NotIn(buckets)
            } else {
                ProbeRows::Counts(counts)
            },
            residual,
            min_matches,
            kind,
            budget,
            outer_columns,
        })
    }

    /// 非集計形の相関スカラー（Issue #1847）のプローブを組み立てる。内側行（可視行だけ）は常に保持し、
    /// 等価キーが無ければ全行が 1 バケットになる。キーに NULL を含む行は入れない（`=` は NULL で真に
    /// ならない）。確保は予算へ計上してから行い、超過は `54000`（`try_reserve` で失敗しても同じ）。
    pub(crate) fn scalar_rows(
        spec: ScalarRowsSpec,
        inputs: impl IntoIterator<Item = Result<InnerInput, SqlSurfaceError>>,
    ) -> Result<Self, SqlSurfaceError> {
        let ScalarRowsSpec {
            keys,
            residual,
            offset,
            limit,
            predicates,
            extra_outer_columns,
            budget,
            byte_cap,
        } = spec;
        let mut used = 0usize;
        let mut charge = |n: usize| -> Result<(), SqlSurfaceError> {
            used = used.saturating_add(n);
            if used > byte_cap {
                return Err(too_large());
            }
            Ok(())
        };
        let mut rows: Vec<InnerRow> = Vec::new();
        let mut by_key: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
        'rows: for input in inputs {
            let input = input?;
            if input.key_cells.len() != keys.len() {
                return Err(SqlSurfaceError::Internal {
                    detail: "correlated key arity mismatch".to_string(),
                });
            }
            let mut key = Vec::new();
            for (cell, k) in input.key_cells.iter().zip(&keys) {
                if matches!(cell, Cell::Null) {
                    continue 'rows;
                }
                encode_key_component(cell, &k.class, &mut key)?;
            }
            let scalar = input.scalar.ok_or_else(|| SqlSurfaceError::Internal {
                detail: "correlated scalar input is missing its value".to_string(),
            })?;
            let idx = u32::try_from(rows.len()).map_err(|_| too_large())?;
            let cell_bytes = input
                .cells
                .iter()
                .map(super::cell_payload_bytes)
                .fold(0usize, usize::saturating_add);
            let scalar_bytes = match &scalar {
                ScalarOut::Pred(_) => 0,
                ScalarOut::Value(c) => {
                    super::cell_payload_bytes(c).saturating_add(std::mem::size_of::<Cell>())
                }
            };
            charge(
                std::mem::size_of::<InnerRow>()
                    .saturating_add(
                        input
                            .cells
                            .len()
                            .saturating_mul(std::mem::size_of::<Cell>()),
                    )
                    .saturating_add(cell_bytes)
                    .saturating_add(scalar_bytes)
                    .saturating_add(std::mem::size_of::<u32>()),
            )?;
            rows.try_reserve(1).map_err(|_| too_large())?;
            rows.push(InnerRow {
                id: input.id,
                cells: input.cells,
                value: None,
                scalar: Some(scalar),
            });
            match by_key.get_mut(&key) {
                Some(v) => {
                    v.try_reserve(1).map_err(|_| too_large())?;
                    v.push(idx);
                }
                None => {
                    charge(key_bytes_overhead(&key))?;
                    by_key.try_reserve(1).map_err(|_| too_large())?;
                    by_key.insert(key, vec![idx]);
                }
            }
        }
        let mut outer_columns: Vec<usize> = keys
            .iter()
            .filter_map(|k| match k.source {
                OuterSource::Column(i) => Some(i),
                OuterSource::Id => None,
            })
            .chain(extra_outer_columns)
            .collect();
        if let Some(r) = &residual {
            outer_columns.extend(r.outer_cols.iter().filter_map(|(_, src, _)| match src {
                OuterSource::Column(i) => Some(*i),
                OuterSource::Id => None,
            }));
        }
        outer_columns.sort();
        outer_columns.dedup();
        Ok(Self {
            keys,
            rows: ProbeRows::ScalarRows {
                rows,
                by_key,
                predicates,
            },
            residual,
            min_matches: 1,
            kind: ProbeKind::ScalarRows { offset, limit },
            budget,
            outer_columns,
        })
    }

    /// 集計形の相関スカラー比較（Issue #1846）のプローブを組み立てる。`table` の述語は外側
    /// スキーマへ束縛済みで、`target_column` はその述語が参照する外側の比較対象列（疑似列 `id`
    /// は `None`）。デコード対象へ含める外側列はキーの列と対象列の和集合。
    pub(crate) fn scalar_compare(
        keys: Vec<OuterKey>,
        table: ScalarTable,
        target_column: Option<usize>,
        budget: Arc<CorrelatedBudget>,
    ) -> Self {
        let mut outer_columns: Vec<usize> = keys
            .iter()
            .filter_map(|k| match k.source {
                OuterSource::Column(i) => Some(i),
                OuterSource::Id => None,
            })
            .chain(target_column)
            .collect();
        outer_columns.sort();
        outer_columns.dedup();
        Self {
            keys,
            rows: ProbeRows::Scalar(table),
            residual: None,
            min_matches: 1,
            kind: ProbeKind::ScalarCompare,
            budget,
            outer_columns,
        }
    }

    /// 外側スキーマで評価に必要な論理列インデックス（デコード対象へ含める用）。
    pub(crate) fn outer_columns(&self) -> &[usize] {
        &self.outer_columns
    }

    /// 外側の 1 行に対して判定する。`row_scalars` は外側スキーマの論理列インデックスで引く
    /// 行スカラー値のビュー、`id` は外側の行 `id`。結果は常に真偽（UNKNOWN にならない）。
    /// 反結合（`NOT EXISTS`）は `EXISTS` の判定をこの唯一の出口で反転する（外側キーが NULL の
    /// 早期偽も反転され、その外側行は返る。否定の取りこぼしによる fail-open を作らない）。
    pub(crate) fn eval(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        match &self.kind {
            ProbeKind::Exists { negated } => {
                self.eval_exists(row_scalars, id).map(|e| e != *negated)
            }
            ProbeKind::NotIn { target } => self.eval_not_in(target, row_scalars, id),
            ProbeKind::ScalarCompare => self.eval_scalar_compare(row_scalars, id),
            ProbeKind::ScalarRows { .. } => self.eval_scalar_rows(row_scalars, id),
        }
    }

    /// 非集計形の相関スカラー比較（WHERE 位置）の 1 行判定（Issue #1847）。採った一致行が 0 件なら偽
    /// （NULL との比較は UNKNOWN で WHERE では偽）、1 件ならその行の値で束縛済みの述語を外側の行へ
    /// 適用し、2 件以上は [`Self::pick_scalar`] が `21000` を返す。
    fn eval_scalar_rows(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        let Some(row) = self.pick_scalar(row_scalars, id)? else {
            return Ok(false);
        };
        let ProbeRows::ScalarRows { predicates, .. } = &self.rows else {
            return Err(unexpected_scalar_layout());
        };
        let Some(ScalarOut::Pred(index)) = &row.scalar else {
            return Err(unexpected_scalar_layout());
        };
        let group = usize::try_from(*index)
            .ok()
            .and_then(|i| predicates.get(i))
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "correlated scalar predicate index out of range".to_string(),
            })?;
        let mut scratch: Vec<StackValue> = Vec::new();
        group.matches(row_scalars, id, &[], 0, &mut scratch)
    }

    /// 非集計形の相関スカラー（投影位置）の 1 行の値（Issue #1847）。一致行が 0 件なら `None`
    /// （呼び出し側が NULL にする）、1 件ならその行の投影値、2 件以上は `21000`。
    pub(crate) fn scalar_value(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<Option<&Cell>, SqlSurfaceError> {
        match self.pick_scalar(row_scalars, id)? {
            None => Ok(None),
            Some(row) => match &row.scalar {
                Some(ScalarOut::Value(cell)) => Ok(Some(cell)),
                _ => Err(unexpected_scalar_layout()),
            },
        }
    }

    /// 外側 1 行について、一致行（バケット内で残余が真の行。並びは実体化順）の先頭 `offset` 件を飛ばし
    /// 最大 `limit` 件を採った結果を返す（Issue #1847）。0 件は `None`、1 件はその行、2 件以上は
    /// `21000`。2 件目が見つかった時点で走査を打ち切る。外側キーが NULL の行は一致なし（`=` は偽）。
    /// 残余があるか等価キーが無い形は再評価 1 回を計上する（等価キーのみはハッシュ引きで計上しない）。
    fn pick_scalar(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<Option<&InnerRow>, SqlSurfaceError> {
        let ProbeKind::ScalarRows { offset, limit } = &self.kind else {
            return Err(unexpected_scalar_layout());
        };
        let ProbeRows::ScalarRows { rows, by_key, .. } = &self.rows else {
            return Err(unexpected_scalar_layout());
        };
        // 採る件数は基数判定に要る 2 件までで足りる（`LIMIT 0` は何も採らない）。
        let want = limit.map_or(2, |n| n.min(2));
        if want == 0 {
            return Ok(None);
        }
        let Some(key) = self.outer_key(row_scalars, id)? else {
            return Ok(None);
        };
        let Some(indices) = by_key.get(&key) else {
            return Ok(None);
        };
        let row_at = |i: u32| {
            rows.get(i as usize)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated probe row index out of range".to_string(),
                })
        };
        match &self.residual {
            None => {
                if self.keys.is_empty() {
                    self.budget.charge_reevaluation()?;
                }
                let Some(first) = indices.get(*offset).copied() else {
                    return Ok(None);
                };
                let second = if want >= 2 {
                    offset.checked_add(1).and_then(|j| indices.get(j))
                } else {
                    None
                };
                if second.is_some() {
                    return Err(SqlSurfaceError::cardinality_violation());
                }
                row_at(first).map(Some)
            }
            Some(residual) => {
                self.budget.charge_reevaluation()?;
                let mut seen = 0usize;
                let mut selected = 0usize;
                let mut first: Option<u32> = None;
                self.scan_matches(rows, indices, residual, row_scalars, id, &mut |idx, _| {
                    let pos = seen;
                    seen = seen.saturating_add(1);
                    if pos < *offset {
                        return false;
                    }
                    selected = selected.saturating_add(1);
                    if first.is_none() {
                        first = Some(idx);
                    }
                    selected >= want
                })?;
                if selected >= 2 {
                    return Err(SqlSurfaceError::cardinality_violation());
                }
                match first {
                    Some(i) => row_at(i).map(Some),
                    None => Ok(None),
                }
            }
        }
    }

    /// 集計形の相関スカラー比較の 1 行判定（Issue #1846）。外側の相関キーでグループの述語を引き、
    /// 該当が無い（キーが NULL・内側に一致するグループが無い）行は空集合の述語で評価する。述語は
    /// 外側スキーマに束縛済みのため、外側の行スカラーをそのまま渡して既存の評価器を呼ぶ。
    fn eval_scalar_compare(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        let ProbeRows::Scalar(table) = &self.rows else {
            // `ScalarCompare` の構築は `Scalar` だけ。破れたら fail-closed。
            return Err(SqlSurfaceError::Internal {
                detail: "correlated scalar probe has an unexpected row layout".to_string(),
            });
        };
        let index = match self.outer_key(row_scalars, id)? {
            Some(key) => table.by_key.get(&key).copied().unwrap_or(table.empty_set),
            None => table.empty_set,
        };
        let group = usize::try_from(index)
            .ok()
            .and_then(|i| table.predicates.get(i))
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "correlated scalar predicate index out of range".to_string(),
            })?;
        let mut scratch: Vec<StackValue> = Vec::new();
        group.matches(row_scalars, id, &[], 0, &mut scratch)
    }

    /// `NOT IN` の 1 行判定（真理値表は本モジュール冒頭）。外側の相関キーが NULL の行は S が空
    /// （どの内側行にも一致しない）ため、対象値 x が NULL でも真になる。
    fn eval_not_in(
        &self,
        target: &OuterKey,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        // x は先に取り出す（範囲外の外側列は NULL と取り違えず fail-closed にする）。
        let x: Option<Vec<u8>> = match target.source {
            OuterSource::Id => {
                let mut v = Vec::new();
                encode_key_component(&Cell::Integer(id), &target.class, &mut v)?;
                Some(v)
            }
            OuterSource::Column(i) => match outer_scalar(row_scalars, i)? {
                None => None,
                Some(scalar) => {
                    let cell = key_cell_from_scalar(&scalar, &target.class)?;
                    let mut v = Vec::new();
                    encode_key_component(&cell, &target.class, &mut v)?;
                    Some(v)
                }
            },
        };
        let Some(key) = self.outer_key(row_scalars, id)? else {
            // 外側の相関キーが NULL: 内側のどの行にも一致せず S は空 → NOT IN は真。
            return Ok(true);
        };
        match &self.rows {
            ProbeRows::NotIn(map) => {
                let Some(bucket) = map.get(&key) else {
                    return Ok(true);
                };
                // バケットが在る = S は非空。
                let Some(x) = x else { return Ok(false) };
                Ok(!bucket.has_null && !bucket.values.contains(&x))
            }
            ProbeRows::Rows { rows, by_key } => {
                let Some(indices) = by_key.get(&key) else {
                    return Ok(true);
                };
                let residual = self.residual_required()?;
                self.budget.charge_reevaluation()?;
                // S の要素（残余が真の行）が 1 つでも x と一致・NULL、または x が NULL なら偽。
                let decided_false = self.scan_matches(
                    rows,
                    indices,
                    residual,
                    row_scalars,
                    id,
                    &mut |_, row| match (&x, &row.value) {
                        (Some(x), Some(y)) => x == y,
                        _ => true,
                    },
                )?;
                Ok(!decided_false)
            }
            // `NotIn` の構築は `NotIn`／`Rows` のどちらかだけ。破れたら fail-closed。
            ProbeRows::Counts(_) | ProbeRows::Scalar(_) | ProbeRows::ScalarRows { .. } => {
                Err(SqlSurfaceError::Internal {
                    detail: "correlated NOT IN probe has an unexpected row layout".to_string(),
                })
            }
        }
    }

    /// 外側の相関キーの正準バイト列。いずれかの成分が NULL なら `None`。
    fn outer_key(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<Option<Vec<u8>>, SqlSurfaceError> {
        let mut key = Vec::new();
        for k in &self.keys {
            let scalar = match k.source {
                OuterSource::Id => {
                    encode_key_component(&Cell::Integer(id), &k.class, &mut key)?;
                    continue;
                }
                OuterSource::Column(i) => outer_scalar(row_scalars, i)?,
            };
            let Some(scalar) = scalar else {
                return Ok(None);
            };
            let cell = key_cell_from_scalar(&scalar, &k.class)?;
            encode_key_component(&cell, &k.class, &mut key)?;
        }
        Ok(Some(key))
    }

    /// 残余を持たない `Rows` は構築されない不変条件。破れたら fail-closed。
    fn residual_required(&self) -> Result<&Residual, SqlSurfaceError> {
        self.residual
            .as_ref()
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "correlated probe has rows without a residual".to_string(),
            })
    }

    /// 否定を適用する前の `EXISTS` 判定（[`Self::eval`] 専用）。
    fn eval_exists(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        // 外側キーが NULL: `=` は真にならないため偽（EXISTS は UNKNOWN にならない）。
        let Some(key) = self.outer_key(row_scalars, id)? else {
            return Ok(false);
        };
        match &self.rows {
            ProbeRows::Counts(map) => Ok(map.get(&key).is_some_and(|n| *n >= self.min_matches)),
            ProbeRows::Rows { rows, by_key } => {
                let Some(indices) = by_key.get(&key) else {
                    return Ok(false);
                };
                let residual = self.residual_required()?;
                self.budget.charge_reevaluation()?;
                self.count_matches(rows, indices, residual, row_scalars, id)
            }
            // `Exists` 種別では構築されない形。破れたら fail-closed。
            ProbeRows::NotIn(_) | ProbeRows::Scalar(_) | ProbeRows::ScalarRows { .. } => {
                Err(SqlSurfaceError::Internal {
                    detail: "correlated EXISTS probe has an unexpected row layout".to_string(),
                })
            }
        }
    }

    fn count_matches(
        &self,
        rows: &[InnerRow],
        indices: &[u32],
        residual: &Residual,
        row_scalars: &[Option<ScalarRef<'_>>],
        outer_id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        let mut matched = 0usize;
        let min = self.min_matches;
        self.scan_matches(
            rows,
            indices,
            residual,
            row_scalars,
            outer_id,
            &mut |_, _| {
                matched = matched.saturating_add(1);
                matched >= min
            },
        )
    }

    /// バケット内の各内側行について残余を評価し、真の行ごとに `on_match` を呼ぶ。`on_match` が
    /// `true` を返した時点で打ち切り `true` を返す（最後まで打ち切られなければ `false`）。
    /// 行ごとに比較予算を計上し協調的中断を確認する（キーなし相関は内側全行に及ぶため）。
    fn scan_matches(
        &self,
        rows: &[InnerRow],
        indices: &[u32],
        residual: &Residual,
        row_scalars: &[Option<ScalarRef<'_>>],
        outer_id: u64,
        on_match: &mut dyn FnMut(u32, &InnerRow) -> bool,
    ) -> Result<bool, SqlSurfaceError> {
        let mut scanned: Vec<Option<ScalarRef<'_>>> = vec![None; residual.width];
        for (slot, source, _ty) in &residual.outer_cols {
            let value = match source {
                OuterSource::Column(i) => outer_scalar(row_scalars, *i)?,
                OuterSource::Id => Some(ScalarRef::BigInt(i64::try_from(outer_id).map_err(
                    |_| SqlSurfaceError::numeric_out_of_range("outer row id is out of range"),
                )?)),
            };
            if let Some(dst) = scanned.get_mut(*slot) {
                *dst = value;
            } else {
                return Err(SqlSurfaceError::Internal {
                    detail: "correlated residual slot out of range".to_string(),
                });
            }
        }
        let mut scratch: Vec<StackValue> = Vec::new();
        for &idx in indices {
            self.budget.charge_comparison()?;
            crate::interrupt::check()?;
            let row = rows
                .get(idx as usize)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "correlated probe row index out of range".to_string(),
                })?;
            for (cell_pos, slot, ty) in &residual.inner_cols {
                let cell = row
                    .cells
                    .get(*cell_pos)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "correlated probe cell position out of range".to_string(),
                    })?;
                let value = cell_scalar(cell, ty)?;
                match scanned.get_mut(*slot) {
                    Some(dst) => *dst = value,
                    None => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "correlated residual slot out of range".to_string(),
                        })
                    }
                }
            }
            if residual
                .group
                .matches(&scanned, row.id, &[], 0, &mut scratch)?
                && on_match(idx, row)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `NotInBucket` へ投影値 1 つを加える。新しい値だけを予算へ計上してから確保する。
    fn add_not_in_value(
        bucket: &mut NotInBucket,
        value: Option<Vec<u8>>,
        charge: &mut impl FnMut(usize) -> Result<(), SqlSurfaceError>,
    ) -> Result<(), SqlSurfaceError> {
        match value {
            None => bucket.has_null = true,
            Some(v) => {
                if !bucket.values.contains(&v) {
                    charge(key_bytes_overhead(&v))?;
                    bucket.values.try_reserve(1).map_err(|_| too_large())?;
                    bucket.values.insert(v);
                }
            }
        }
        Ok(())
    }
}

/// 非集計形の相関スカラーのプローブの配線不備（fail-closed。静的文字列のみ）。
fn unexpected_scalar_layout() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "correlated scalar rows probe has an unexpected layout".to_string(),
    }
}

/// 外側の行スカラーから `index` 列を取り出す。範囲外は配線不備として fail-closed
/// （実 NULL と取り違えない）。
fn outer_scalar<'a>(
    row_scalars: &[Option<ScalarRef<'a>>],
    index: usize,
) -> Result<Option<ScalarRef<'a>>, SqlSurfaceError> {
    row_scalars
        .get(index)
        .copied()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "correlated outer column is outside the decoded row scalar view".to_string(),
        })
}

/// [`CorrelatedProbe`] の共有ハンドル。`WherePredicate`／`BoundExpr` の内部専用の葉が保持する。
///
/// `PartialEq`／`Eq` は同一プローブか（`Arc::ptr_eq`）で判定し、`Debug` は行データを出さない
/// （エラー・ログ経由で内側の可視行の内容が漏れないようにする。RLS-10 ポインタ）。
#[derive(Clone)]
pub struct CorrelatedProbeRef(Arc<CorrelatedProbe>);

impl CorrelatedProbeRef {
    pub(crate) fn new(probe: CorrelatedProbe) -> Self {
        Self(Arc::new(probe))
    }

    pub(crate) fn probe(&self) -> &CorrelatedProbe {
        &self.0
    }
}

impl PartialEq for CorrelatedProbeRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CorrelatedProbeRef {}

impl std::fmt::Debug for CorrelatedProbeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CorrelatedProbeRef(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Arc<CorrelatedBudget> {
        Arc::new(CorrelatedBudget::new(
            MAX_CORRELATED_REEVALUATIONS,
            super::super::MAX_JOIN_COMPARISONS,
        ))
    }

    fn int_key() -> Vec<OuterKey> {
        vec![OuterKey {
            source: OuterSource::Column(0),
            class: JoinKeyClass::Integer,
        }]
    }

    fn input(key: Cell) -> InnerInput {
        InnerInput {
            id: 0,
            key_cells: vec![key],
            cells: Vec::new(),
            value_cell: None,
            scalar: None,
        }
    }

    fn probe(min_matches: usize, keys: Vec<Cell>) -> CorrelatedProbe {
        probe_with(min_matches, false, keys)
    }

    fn probe_with(min_matches: usize, negated: bool, keys: Vec<Cell>) -> CorrelatedProbe {
        CorrelatedProbe::build(
            ProbeSpec {
                keys: int_key(),
                residual: None,
                min_matches,
                kind: ProbeKind::Exists { negated },
                budget: budget(),
                byte_cap: usize::MAX,
            },
            keys.into_iter().map(|c| Ok(input(c))),
        )
        .expect("build")
    }

    #[test]
    fn semi_join_matches_across_integer_classes_and_skips_null_keys() {
        let p = probe(
            1,
            vec![Cell::SignedInteger(7), Cell::Integer(9), Cell::Null],
        );
        assert!(p.eval(&[Some(ScalarRef::Integer(7))], 1).unwrap());
        assert!(p.eval(&[Some(ScalarRef::BigInt(9))], 1).unwrap());
        assert!(!p.eval(&[Some(ScalarRef::BigInt(8))], 1).unwrap());
        // 外側キーが NULL の行は偽（UNKNOWN にならない）。
        assert!(!p.eval(&[None], 1).unwrap());
    }

    #[test]
    fn anti_join_inverts_every_exit_including_null_outer_key() {
        let p = probe_with(
            1,
            true,
            vec![Cell::SignedInteger(7), Cell::Integer(9), Cell::Null],
        );
        // 一致あり → 偽、一致なし → 真。内側の NULL キー行は不一致。
        assert!(!p.eval(&[Some(ScalarRef::Integer(7))], 1).unwrap());
        assert!(p.eval(&[Some(ScalarRef::BigInt(8))], 1).unwrap());
        // 外側キーが NULL の行は EXISTS が偽のため NOT EXISTS は真。
        assert!(p.eval(&[None], 1).unwrap());
        // 範囲外の外側列は否定しても fail-closed のまま。
        assert_eq!(p.eval(&[], 1).unwrap_err().wire_code(), "XX000");
    }

    #[test]
    fn anti_join_inverts_offset_threshold() {
        let p = probe_with(
            2,
            true,
            vec![
                Cell::SignedInteger(1),
                Cell::SignedInteger(1),
                Cell::SignedInteger(2),
            ],
        );
        assert!(!p.eval(&[Some(ScalarRef::BigInt(1))], 1).unwrap());
        assert!(p.eval(&[Some(ScalarRef::BigInt(2))], 1).unwrap());
    }

    fn not_in_probe(
        rows: Vec<(Cell, Cell)>,
        byte_cap: usize,
    ) -> Result<CorrelatedProbe, SqlSurfaceError> {
        // 相関キー（外側列 0）と対象値（外側列 1）はともに整数クラス。
        CorrelatedProbe::build(
            ProbeSpec {
                keys: int_key(),
                residual: None,
                min_matches: 1,
                kind: ProbeKind::NotIn {
                    target: OuterKey {
                        source: OuterSource::Column(1),
                        class: JoinKeyClass::Integer,
                    },
                },
                budget: budget(),
                byte_cap,
            },
            rows.into_iter().map(|(k, v)| {
                Ok(InnerInput {
                    id: 0,
                    key_cells: vec![k],
                    cells: Vec::new(),
                    value_cell: Some(v),
                    scalar: None,
                })
            }),
        )
    }

    fn outer(key: Option<i64>, x: Option<i64>) -> Vec<Option<ScalarRef<'static>>> {
        vec![key.map(ScalarRef::BigInt), x.map(ScalarRef::BigInt)]
    }

    #[test]
    fn not_in_follows_the_three_valued_truth_table() {
        use Cell::{Null, SignedInteger as I};
        // 相関キー 1 の S = {1, 2}、キー 2 の S = {NULL, 5}、キー 3 の S = {7}。キー NULL の行は誰の S にも属さない。
        let p = not_in_probe(
            vec![
                (I(1), I(1)),
                (I(1), I(2)),
                (I(2), Null),
                (I(2), I(5)),
                (I(3), I(7)),
                (Null, I(9)),
            ],
            usize::MAX,
        )
        .expect("build");
        // 1. S が空（バケット無し・外側キー NULL）は x が NULL でも真。
        assert!(p.eval(&outer(Some(4), None), 1).unwrap());
        assert!(p.eval(&outer(Some(4), Some(1)), 1).unwrap());
        assert!(p.eval(&outer(None, None), 1).unwrap());
        assert!(p.eval(&outer(None, Some(9)), 1).unwrap());
        // 2. x が NULL で S 非空は偽（UNKNOWN）。
        assert!(!p.eval(&outer(Some(3), None), 1).unwrap());
        // 3. x ∈ S は偽。
        assert!(!p.eval(&outer(Some(1), Some(2)), 1).unwrap());
        assert!(!p.eval(&outer(Some(2), Some(5)), 1).unwrap());
        // 4. x ∉ S かつ S に NULL は偽（UNKNOWN）。
        assert!(!p.eval(&outer(Some(2), Some(6)), 1).unwrap());
        // 5. x ∉ S かつ NULL なしで S 非空は真。
        assert!(p.eval(&outer(Some(1), Some(3)), 1).unwrap());
        assert!(p.eval(&outer(Some(3), Some(8)), 1).unwrap());
    }

    #[test]
    fn not_in_fails_closed_on_missing_wiring_and_charges_the_byte_budget() {
        use Cell::SignedInteger as I;
        let p = not_in_probe(vec![(I(1), I(1))], usize::MAX).expect("build");
        // 対象列（外側列 1）が行ビューに無い場合は NULL と取り違えず XX000。
        let e = p.eval(&[Some(ScalarRef::BigInt(1))], 1).unwrap_err();
        assert_eq!(e.wire_code(), "XX000");
        // 対象列がデコード対象に含まれる。
        assert_eq!(p.outer_columns(), &[0, 1]);
        let e = not_in_probe((0..10).map(|i| (I(i), I(i))).collect(), 8)
            .err()
            .expect("over budget");
        assert_eq!(e.wire_code(), "54000");
    }

    #[test]
    fn not_in_requires_the_target_value_cell() {
        let e = CorrelatedProbe::build(
            ProbeSpec {
                keys: int_key(),
                residual: None,
                min_matches: 1,
                kind: ProbeKind::NotIn {
                    target: OuterKey {
                        source: OuterSource::Column(1),
                        class: JoinKeyClass::Integer,
                    },
                },
                budget: budget(),
                byte_cap: usize::MAX,
            },
            std::iter::once(Ok(input(Cell::SignedInteger(1)))),
        )
        .err()
        .expect("missing value");
        assert_eq!(e.wire_code(), "XX000");
    }

    #[test]
    fn semi_join_has_no_distinct_value_cap() {
        let keys: Vec<Cell> = (0..2_000_i64).map(Cell::SignedInteger).collect();
        let p = probe(1, keys);
        assert!(p.eval(&[Some(ScalarRef::BigInt(1_999))], 1).unwrap());
        assert!(!p.eval(&[Some(ScalarRef::BigInt(2_000))], 1).unwrap());
    }

    #[test]
    fn min_matches_models_inner_offset() {
        let p = probe(
            2,
            vec![
                Cell::SignedInteger(1),
                Cell::SignedInteger(1),
                Cell::SignedInteger(2),
            ],
        );
        assert!(p.eval(&[Some(ScalarRef::BigInt(1))], 1).unwrap());
        assert!(!p.eval(&[Some(ScalarRef::BigInt(2))], 1).unwrap());
    }

    #[test]
    fn out_of_range_outer_column_fails_closed() {
        let p = probe(1, vec![Cell::SignedInteger(1)]);
        let e = p.eval(&[], 1).expect_err("range");
        assert_eq!(e.wire_code(), "XX000");
    }

    #[test]
    fn byte_budget_is_charged_before_allocation() {
        let e = CorrelatedProbe::build(
            ProbeSpec {
                keys: int_key(),
                residual: None,
                min_matches: 1,
                kind: ProbeKind::Exists { negated: false },
                budget: budget(),
                byte_cap: 8,
            },
            (0..10_i64).map(|i| Ok(input(Cell::SignedInteger(i)))),
        )
        .err()
        .expect("over budget");
        assert_eq!(e.wire_code(), "54000");
    }

    #[test]
    fn budget_counters_reject_past_the_cap() {
        let b = CorrelatedBudget::new(2, 3);
        assert!(b.charge_reevaluation().is_ok());
        assert!(b.charge_reevaluation().is_ok());
        assert_eq!(b.charge_reevaluation().unwrap_err().wire_code(), "54000");
        for _ in 0..3 {
            assert!(b.charge_comparison().is_ok());
        }
        assert_eq!(b.charge_comparison().unwrap_err().wire_code(), "54000");
    }

    #[test]
    fn probe_ref_debug_hides_data_and_eq_is_identity() {
        let a = CorrelatedProbeRef::new(probe(1, vec![Cell::SignedInteger(42)]));
        let b = a.clone();
        let c = CorrelatedProbeRef::new(probe(1, vec![Cell::SignedInteger(42)]));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(!format!("{a:?}").contains("42"));
    }

    #[test]
    fn expression_leaf_agrees_between_recursive_and_compiled_evaluators() {
        use crate::sql::expr_program::ExprProgram;
        use crate::sql::udf_call::{
            eval_with_scalars, mark_referenced_scalar_columns, visit_referenced_scalar_columns,
            BoundExpr, ExprValue,
        };
        let probe = CorrelatedProbeRef::new(probe(1, vec![Cell::SignedInteger(7)]));
        let expr = BoundExpr::CorrelatedExists(probe);
        let program = ExprProgram::compile(&expr);
        let mut scratch = Vec::new();
        let rows: Vec<Vec<Option<ScalarRef<'_>>>> = vec![
            vec![Some(ScalarRef::BigInt(7))],
            vec![Some(ScalarRef::BigInt(8))],
            vec![None],
        ];
        for row in &rows {
            let recursive = eval_with_scalars(&expr, 1, &[], row).expect("recursive");
            let compiled = program.eval(1, &[], row, &mut scratch).expect("compiled");
            assert_eq!(recursive, compiled);
            assert!(matches!(recursive, ExprValue::Bool(_)));
        }
        // 外側キー列がデコード対象に含まれる（欠けるとマスク外参照になる）。
        let mut mask = [false; 3];
        assert!(mark_referenced_scalar_columns(&expr, &mut mask));
        assert_eq!(mask, [true, false, false]);
        let mut visited = Vec::new();
        visit_referenced_scalar_columns(&expr, &mut |i| visited.push(i));
        assert_eq!(visited, vec![0]);
    }
    /// 常に真の述語（空の連言）と常に偽の述語（分岐 0 個）。
    fn always_true() -> BoundOrGroup {
        BoundOrGroup::new(vec![crate::sql::where_tree::BoundConjunction::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )])
    }

    fn always_false() -> BoundOrGroup {
        BoundOrGroup::new(Vec::new())
    }

    /// キー 7 → 真、キー 8 → 偽、それ以外・キー NULL → 空集合の述語（`empty_true` で切り替え）。
    fn scalar_probe(empty_true: bool) -> CorrelatedProbe {
        let mut by_key = HashMap::new();
        for (k, idx) in [(7_i128, 0_u32), (8_i128, 1_u32)] {
            by_key.insert(k.to_be_bytes().to_vec(), idx);
        }
        let table = ScalarTable {
            by_key,
            predicates: vec![
                always_true(),
                always_false(),
                if empty_true {
                    always_true()
                } else {
                    always_false()
                },
            ],
            empty_set: 2,
        };
        CorrelatedProbe::scalar_compare(int_key(), table, Some(1), budget())
    }

    #[test]
    fn scalar_compare_uses_group_predicate_then_empty_set_predicate() {
        let p = scalar_probe(true);
        assert!(p.eval(&[Some(ScalarRef::BigInt(7)), None], 1).unwrap());
        assert!(!p.eval(&[Some(ScalarRef::BigInt(8)), None], 1).unwrap());
        // 内側に一致するグループが無い外側の行は空集合の述語（ここでは真）。
        assert!(p.eval(&[Some(ScalarRef::BigInt(99)), None], 1).unwrap());
        // 外側キーが NULL の行も空集合の述語（`count` の 0 との比較は真になりうる）。
        assert!(p.eval(&[None, None], 1).unwrap());
        let p = scalar_probe(false);
        assert!(!p.eval(&[None, None], 1).unwrap());
        assert!(!p.eval(&[Some(ScalarRef::BigInt(99)), None], 1).unwrap());
    }

    #[test]
    fn scalar_compare_fails_closed_on_missing_outer_column_and_bad_index() {
        let p = scalar_probe(true);
        assert_eq!(p.eval(&[], 1).unwrap_err().wire_code(), "XX000");
        let table = ScalarTable {
            by_key: HashMap::new(),
            predicates: Vec::new(),
            empty_set: 0,
        };
        let broken = CorrelatedProbe::scalar_compare(int_key(), table, None, budget());
        assert_eq!(
            broken
                .eval(&[Some(ScalarRef::BigInt(1))], 1)
                .unwrap_err()
                .wire_code(),
            "XX000"
        );
    }

    #[test]
    fn scalar_compare_outer_columns_include_key_and_target_and_debug_is_opaque() {
        let p = scalar_probe(true);
        assert_eq!(p.outer_columns(), &[0, 1]);
        let r = CorrelatedProbeRef::new(p);
        assert_eq!(format!("{r:?}"), "CorrelatedProbeRef(..)");
    }

    /// 非集計形の相関スカラー（Issue #1847）の値プローブ。キー 1 に値 10・20・30、キー 2 に値 7、
    /// キー NULL の行（入れない）。
    fn rows_probe(
        offset: usize,
        limit: Option<usize>,
        byte_cap: usize,
    ) -> Result<CorrelatedProbe, SqlSurfaceError> {
        let data = [
            (Cell::SignedInteger(1), 10),
            (Cell::SignedInteger(1), 20),
            (Cell::SignedInteger(1), 30),
            (Cell::SignedInteger(2), 7),
            (Cell::Null, 99),
        ];
        CorrelatedProbe::scalar_rows(
            ScalarRowsSpec {
                keys: int_key(),
                residual: None,
                offset,
                limit,
                predicates: Vec::new(),
                extra_outer_columns: Vec::new(),
                budget: budget(),
                byte_cap,
            },
            data.into_iter().map(|(k, v)| {
                Ok(InnerInput {
                    id: 0,
                    key_cells: vec![k],
                    cells: Vec::new(),
                    value_cell: None,
                    scalar: Some(ScalarOut::Value(Cell::SignedInteger(v))),
                })
            }),
        )
    }

    fn picked(p: &CorrelatedProbe, key: Option<i64>) -> Result<Option<i64>, String> {
        let outer = [key.map(ScalarRef::BigInt)];
        match p.scalar_value(&outer, 1) {
            Ok(None) => Ok(None),
            Ok(Some(Cell::SignedInteger(v))) => Ok(Some(*v)),
            Ok(Some(other)) => panic!("unexpected cell {other:?}"),
            Err(e) => Err(e.wire_code().to_string()),
        }
    }

    #[test]
    fn scalar_rows_cardinality_follows_zero_one_many() {
        let p = rows_probe(0, None, usize::MAX).expect("build");
        // 0 件（バケット無し・外側キー NULL）は None、1 件はその値、2 件以上は 21000。
        assert_eq!(picked(&p, Some(9)), Ok(None));
        assert_eq!(picked(&p, None), Ok(None));
        assert_eq!(picked(&p, Some(2)), Ok(Some(7)));
        assert_eq!(picked(&p, Some(1)), Err("21000".to_string()));
    }

    #[test]
    fn scalar_rows_offset_and_limit_select_a_window_of_matches() {
        // LIMIT 1: 先頭 1 件だけを採るので 21000 にならない。
        let p = rows_probe(0, Some(1), usize::MAX).expect("build");
        assert_eq!(picked(&p, Some(1)), Ok(Some(10)));
        // OFFSET 1 LIMIT 1: 2 件目。OFFSET 2 LIMIT 1: 3 件目。OFFSET 3: 残り無し。
        let p = rows_probe(1, Some(1), usize::MAX).expect("build");
        assert_eq!(picked(&p, Some(1)), Ok(Some(20)));
        assert_eq!(picked(&p, Some(2)), Ok(None));
        let p = rows_probe(2, Some(1), usize::MAX).expect("build");
        assert_eq!(picked(&p, Some(1)), Ok(Some(30)));
        let p = rows_probe(3, None, usize::MAX).expect("build");
        assert_eq!(picked(&p, Some(1)), Ok(None));
        // OFFSET 2（LIMIT なし）は残り 1 件。OFFSET 1（LIMIT なし）は残り 2 件で 21000。
        let p = rows_probe(2, None, usize::MAX).expect("build");
        assert_eq!(picked(&p, Some(1)), Ok(Some(30)));
        let p = rows_probe(1, None, usize::MAX).expect("build");
        assert_eq!(picked(&p, Some(1)), Err("21000".to_string()));
        // LIMIT 2 は 2 件を採るので 21000。LIMIT 0 は何も採らない。
        let p = rows_probe(0, Some(2), usize::MAX).expect("build");
        assert_eq!(picked(&p, Some(1)), Err("21000".to_string()));
        let p = rows_probe(0, Some(0), usize::MAX).expect("build");
        assert_eq!(picked(&p, Some(1)), Ok(None));
    }

    #[test]
    fn scalar_rows_fail_closed_budget_wiring_and_opaque_debug() {
        let e = rows_probe(0, None, 8).err().expect("over budget");
        assert_eq!(e.wire_code(), "54000");
        let p = rows_probe(0, None, usize::MAX).expect("build");
        // 範囲外の外側列は NULL と取り違えず XX000。
        assert_eq!(p.scalar_value(&[], 1).unwrap_err().wire_code(), "XX000");
        // 他の判定方式で組み立てたプローブは値の引き当てに使えない。
        let exists = probe(1, vec![Cell::SignedInteger(1)]);
        assert_eq!(
            exists
                .scalar_value(&[Some(ScalarRef::BigInt(1))], 1)
                .unwrap_err()
                .wire_code(),
            "XX000"
        );
        // 汎用の構築経路は `ScalarRows` を拒否する。
        let e = CorrelatedProbe::build(
            ProbeSpec {
                keys: int_key(),
                residual: None,
                min_matches: 1,
                kind: ProbeKind::ScalarRows {
                    offset: 0,
                    limit: None,
                },
                budget: budget(),
                byte_cap: usize::MAX,
            },
            std::iter::empty(),
        )
        .err()
        .expect("generic build is rejected");
        assert_eq!(e.wire_code(), "XX000");
        assert_eq!(p.outer_columns(), &[0]);
        let r = CorrelatedProbeRef::new(p);
        assert_eq!(format!("{r:?}"), "CorrelatedProbeRef(..)");
    }
}
