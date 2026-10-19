//! 部分索引の述語の含意判定（INDEX-11、Issue #1655）。
//!
//! 役割と責務境界:
//! - 「クエリの `WHERE`（Q）が TRUE になる行は、必ず索引の述語（P）も TRUE である」ことを保守的に
//!   証明する純関数 [`query_implies_partial_predicate`] を提供する。証明できれば部分索引へ入っている
//!   行だけを見れば Q の結果が欠けない。証明できないまま索引を使うと述語外の行が結果から落ちる
//!   （fail-open）ため、判定は常に「含意しない」側へ倒す（偽陽性なし）。
//! - プランナ（Issue #1656）が部分索引の選択前に呼ぶ想定で、現時点では呼び出し元はテストのみ
//!   （そのため入口に `dead_code` 許可を付けている。#1656 で外す）。行・カタログ・他テナントの
//!   データは読まず、束縛済みの述語だけを入力とする。RLS（`visible()`）は判定に関与しない。
//! - 判定は束縛済み表現（[`MetadataFilter`]・[`BoundExpr`]・[`BoundOrGroup`]）の上で行う。両辺とも
//!   `sql::parser::bind_where_predicates` を通るため、列添字・型付きリテラル・書き換えが一致し、
//!   評価器（`BoundOrGroup::matches`）との意味論のずれが生じない。
//!
//! 判定規則（P の全項が Q から含意されるときだけ `true`）:
//! - P の項が認識済みの原子（等価・範囲・`IN`・`BETWEEN`・`IS [NOT] NULL`・整数列と数値定数の
//!   比較・疑似列 `id` の比較）なら、Q の認識済み原子を列ごとに畳んだ要約に照らして判定する。
//! - Q の未認識の項は無視する（条件を落とすと Q は弱くなるだけで健全）。Q の `OR` 群も無視する。
//! - P の未認識の項（`OR`・`NOT`・`LIKE`・式など）は、Q に構造的に同一の項があるときだけ含意とみなす。
//! - 浮動小数列・`CHAR(n)` 列・TEXT の範囲比較・整数性（`n > 5` ⇒ `n >= 6`）は推論に使わない。
//! - 比較回数に作業予算を設け、超過は `false`（入力は上限付きだが CPU 増幅を防ぐ）。
//!
//! プランナ側の前提（#1656 への申し送り）: 両辺を同じスキーマ世代で束縛すること、プリペアド文は
//! 具体値で束縛した後に毎回判定すること（describe 用ダミー値で判定しない）。

use crate::catalog::{ColumnType, TableSchema};
use crate::declarative_filter::{CompareOp, FilterOp, MetadataFilter, TypedLiteral};
use crate::sql::udf_call::{BinOp, BoundExpr};
use crate::sql::where_tree::{BoundConjunction, BoundOrGroup};
use std::cmp::Ordering;

/// 比較 1 回を 1 単位とする作業予算の既定値。クエリ側の `IN`（256 要素）× フィルタ 256 件規模の
/// 総当たりが収まる大きさで、悪意ある入力による CPU 増幅の上限になる。
const MAX_IMPLICATION_STEPS: usize = 1 << 18;

/// 判定対象（列または疑似列 `id`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subject {
    Column(usize),
    Id,
}

/// 比較に使う値（借用）。順序を持つのは `Typed`・`Num`・`Wide` のみ。
#[derive(Debug, Clone, Copy)]
enum Key<'a> {
    Text(&'a str),
    Bool(bool),
    Typed(&'a TypedLiteral),
    /// 整数列と比較される `f64` 定数（有限値のみ）。
    Num(f64),
    /// 疑似列 `id` の比較値。
    Wide(i128),
}

impl Key<'_> {
    fn ordered(&self) -> bool {
        !matches!(self, Key::Text(_) | Key::Bool(_))
    }
}

/// 区間の端点。
#[derive(Debug, Clone, Copy)]
struct Bound<'a> {
    key: Key<'a>,
    inclusive: bool,
}

/// 認識済みの原子。
#[derive(Debug)]
enum Atom<'a> {
    IsNull,
    NotNull,
    Eq(Key<'a>),
    In(Vec<Key<'a>>),
    Range {
        lo: Option<Bound<'a>>,
        hi: Option<Bound<'a>>,
    },
}

/// 比較回数の予算。尽きたら `exhausted` を立て、最終結果を `false` にする。
struct Budget {
    left: usize,
    exhausted: bool,
}

impl Budget {
    fn spend(&mut self, n: usize) -> bool {
        match self.left.checked_sub(n) {
            Some(rest) => {
                self.left = rest;
                true
            }
            None => {
                self.left = 0;
                self.exhausted = true;
                false
            }
        }
    }

    /// 値の比較（1 単位消費）。種別が違う・比較不能・予算切れは `None`。
    fn cmp(&mut self, a: &Key<'_>, b: &Key<'_>) -> Option<Ordering> {
        if !self.spend(1) {
            return None;
        }
        match (a, b) {
            (Key::Text(x), Key::Text(y)) => Some(x.cmp(y)),
            (Key::Bool(x), Key::Bool(y)) => Some(x.cmp(y)),
            (Key::Num(x), Key::Num(y)) => x.partial_cmp(y),
            (Key::Wide(x), Key::Wide(y)) => Some(x.cmp(y)),
            (Key::Typed(x), Key::Typed(y)) => cmp_typed(x, y),
            _ => None,
        }
    }

    fn equal(&mut self, a: &Key<'_>, b: &Key<'_>) -> bool {
        self.cmp(a, b) == Some(Ordering::Equal)
    }
}

/// 型付きリテラルの比較。評価器（`FilterOp::eval`・`typed_in_eval`・`typed_between_eval`）と同じ
/// 比較関数を使う（`NUMERIC` はスケール違いを同値にする `cmp_exact`）。variant 違いは `None`。
fn cmp_typed(a: &TypedLiteral, b: &TypedLiteral) -> Option<Ordering> {
    use TypedLiteral as T;
    match (a, b) {
        (T::Date(x), T::Date(y)) => Some(x.cmp(y)),
        (T::Timestamp(x), T::Timestamp(y)) => Some(x.cmp(y)),
        (T::Time(x), T::Time(y)) => Some(x.cmp(y)),
        (T::Interval(x), T::Interval(y)) => Some(x.cmp(y)),
        (T::TimestampTz(x), T::TimestampTz(y)) => Some(x.cmp(y)),
        (T::Numeric(x), T::Numeric(y)) => Some(crate::numeric::cmp_exact(x, y)),
        (T::Uuid(x), T::Uuid(y)) => Some(x.cmp(y)),
        (T::Bytes(x), T::Bytes(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// 比較演算子と値から原子を作る。
fn compare_atom<'a>(op: CompareOp, key: Key<'a>) -> Atom<'a> {
    let b = |inclusive| Some(Bound { key, inclusive });
    match op {
        CompareOp::Eq => Atom::Eq(key),
        CompareOp::Lt => Atom::Range {
            lo: None,
            hi: b(false),
        },
        CompareOp::Le => Atom::Range {
            lo: None,
            hi: b(true),
        },
        CompareOp::Gt => Atom::Range {
            lo: b(false),
            hi: None,
        },
        CompareOp::Ge => Atom::Range {
            lo: b(true),
            hi: None,
        },
    }
}

/// 宣言的フィルタから原子を取り出す。未認識（`CHAR(n)` 列・`LIKE`・`NOT`・配列・JSON 等）は `None`。
fn metadata_atom(f: &MetadataFilter) -> Option<(Subject, Atom<'_>)> {
    if f.is_bpchar() {
        return None;
    }
    let atom = match f.op() {
        FilterOp::Equals(s) => Atom::Eq(Key::Text(s)),
        FilterOp::BoolEquals(b) => Atom::Eq(Key::Bool(*b)),
        FilterOp::TypedCompare { op, value } => compare_atom(*op, Key::Typed(value)),
        FilterOp::InText(vs) => Atom::In(vs.iter().map(|s| Key::Text(s)).collect()),
        FilterOp::InTyped(vs) => Atom::In(vs.iter().map(Key::Typed).collect()),
        FilterOp::Between { low, high } => Atom::Range {
            lo: Some(Bound {
                key: Key::Typed(low),
                inclusive: true,
            }),
            hi: Some(Bound {
                key: Key::Typed(high),
                inclusive: true,
            }),
        },
        FilterOp::IsNull => Atom::IsNull,
        FilterOp::IsNotNull => Atom::NotNull,
        _ => return None,
    };
    Some((Subject::Column(f.column_index()), atom))
}

fn compare_op_of(op: BinOp) -> Option<CompareOp> {
    match op {
        BinOp::Eq => Some(CompareOp::Eq),
        BinOp::Lt => Some(CompareOp::Lt),
        BinOp::Le => Some(CompareOp::Le),
        BinOp::Gt => Some(CompareOp::Gt),
        BinOp::Ge => Some(CompareOp::Ge),
        _ => None,
    }
}

/// 辺を入れ替えたときの演算子（`5 < n` は `n > 5`）。
fn flip(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Eq => CompareOp::Eq,
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::Le => CompareOp::Ge,
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::Ge => CompareOp::Le,
    }
}

/// 式述語から原子を取り出す。対象は「整数列（`SMALLINT`／`INTEGER`／`BIGINT`）と有限の数値定数の
/// 比較」と疑似列 `id` の比較（`id > 3` の `f64` 形と部分問合せ由来の `IdCompare`）だけ。浮動小数列は NaN・符号付きゼロの順序が評価器とずれうるため
/// 対象外（構造的同一だけが含意になる）。
fn expr_atom<'a>(e: &'a BoundExpr, schema: &TableSchema) -> Option<(Subject, Atom<'a>)> {
    match e {
        BoundExpr::IdCompare { op, value } => Some((
            Subject::Id,
            compare_atom(compare_op_of(*op)?, Key::Wide(*value)),
        )),
        BoundExpr::Binary { op, lhs, rhs } => {
            let op = compare_op_of(*op)?;
            let (target, c, op) = match (lhs.as_ref(), rhs.as_ref()) {
                (BoundExpr::ColumnRef { index }, BoundExpr::Number(c)) => {
                    (Subject::Column(*index), *c, op)
                }
                (BoundExpr::Number(c), BoundExpr::ColumnRef { index }) => {
                    (Subject::Column(*index), *c, flip(op))
                }
                (BoundExpr::IdRef, BoundExpr::Number(c)) => (Subject::Id, *c, op),
                (BoundExpr::Number(c), BoundExpr::IdRef) => (Subject::Id, *c, flip(op)),
                _ => return None,
            };
            if !c.is_finite() {
                return None;
            }
            if let Subject::Column(index) = target {
                let col = schema.columns.get(index)?;
                let integer = matches!(
                    col.ty,
                    ColumnType::SmallInt | ColumnType::Integer | ColumnType::BigInt
                );
                if !integer {
                    return None;
                }
            }
            Some((target, compare_atom(op, Key::Num(c))))
        }
        _ => None,
    }
}

/// Q の認識済み原子を 1 つの対象について畳んだ要約。
#[derive(Debug, Default)]
struct Summary<'a> {
    /// `IS NULL` が成り立つ。
    null: bool,
    /// 値が非 NULL であることが成り立つ（等価・範囲・所属・`IS NOT NULL` はいずれも非 NULL を含意）。
    nonnull: bool,
    /// 等価な値（要素 1 個の `IN` を含む）。
    eqs: Vec<Key<'a>>,
    /// 所属集合。
    ins: Vec<Vec<Key<'a>>>,
    /// 順序ありの値の下限・上限（最もきついもの）。
    lo: Option<Bound<'a>>,
    hi: Option<Bound<'a>>,
}

fn tighten_lo<'a>(cur: &mut Option<Bound<'a>>, cand: Bound<'a>, budget: &mut Budget) {
    match cur {
        None => *cur = Some(cand),
        Some(c) => match budget.cmp(&cand.key, &c.key) {
            Some(Ordering::Greater) => *cur = Some(cand),
            Some(Ordering::Equal) if !cand.inclusive => c.inclusive = false,
            _ => {}
        },
    }
}

fn tighten_hi<'a>(cur: &mut Option<Bound<'a>>, cand: Bound<'a>, budget: &mut Budget) {
    match cur {
        None => *cur = Some(cand),
        Some(c) => match budget.cmp(&cand.key, &c.key) {
            Some(Ordering::Less) => *cur = Some(cand),
            Some(Ordering::Equal) if !cand.inclusive => c.inclusive = false,
            _ => {}
        },
    }
}

impl<'a> Summary<'a> {
    fn absorb(&mut self, atom: Atom<'a>, budget: &mut Budget) {
        match atom {
            Atom::IsNull => self.null = true,
            Atom::NotNull => self.nonnull = true,
            Atom::Eq(k) => {
                self.nonnull = true;
                if k.ordered() {
                    let b = Bound {
                        key: k,
                        inclusive: true,
                    };
                    tighten_lo(&mut self.lo, b, budget);
                    tighten_hi(&mut self.hi, b, budget);
                }
                self.eqs.push(k);
            }
            Atom::In(ks) => {
                self.nonnull = true;
                if let [only] = ks.as_slice() {
                    self.absorb(Atom::Eq(*only), budget);
                    return;
                }
                if let Some((min, max)) = min_max(&ks, budget) {
                    let lo = Bound {
                        key: min,
                        inclusive: true,
                    };
                    let hi = Bound {
                        key: max,
                        inclusive: true,
                    };
                    tighten_lo(&mut self.lo, lo, budget);
                    tighten_hi(&mut self.hi, hi, budget);
                }
                self.ins.push(ks);
            }
            Atom::Range { lo, hi } => {
                self.nonnull = true;
                if let Some(b) = lo {
                    tighten_lo(&mut self.lo, b, budget);
                }
                if let Some(b) = hi {
                    tighten_hi(&mut self.hi, b, budget);
                }
            }
        }
    }
}

/// 順序ありの集合の最小・最大。空・順序なし・比較不能は `None`。
fn min_max<'a>(ks: &[Key<'a>], budget: &mut Budget) -> Option<(Key<'a>, Key<'a>)> {
    let first = *ks.first()?;
    if !first.ordered() {
        return None;
    }
    let (mut min, mut max) = (first, first);
    for k in ks.iter().skip(1) {
        if budget.cmp(k, &min)? == Ordering::Less {
            min = *k;
        }
        if budget.cmp(k, &max)? == Ordering::Greater {
            max = *k;
        }
    }
    Some((min, max))
}

/// P の下限 `p` を Q の下限 `q` が満たす（Q の区間が P の区間の下端以上から始まる）か。
fn lo_ok(q: &Option<Bound<'_>>, p: &Bound<'_>, budget: &mut Budget) -> bool {
    let Some(q) = q else { return false };
    match budget.cmp(&q.key, &p.key) {
        Some(Ordering::Greater) => true,
        Some(Ordering::Equal) => p.inclusive || !q.inclusive,
        _ => false,
    }
}

fn hi_ok(q: &Option<Bound<'_>>, p: &Bound<'_>, budget: &mut Budget) -> bool {
    let Some(q) = q else { return false };
    match budget.cmp(&q.key, &p.key) {
        Some(Ordering::Less) => true,
        Some(Ordering::Equal) => p.inclusive || !q.inclusive,
        _ => false,
    }
}

/// `k` が `set` のいずれかと等しいか。
fn contains(set: &[Key<'_>], k: &Key<'_>, budget: &mut Budget) -> bool {
    for s in set {
        if budget.equal(s, k) {
            return true;
        }
        if budget.exhausted {
            return false;
        }
    }
    false
}

/// Q の要約 `s` が原子 `atom`（P の項）を含意するか。
fn holds(s: &Summary<'_>, atom: &Atom<'_>, budget: &mut Budget) -> bool {
    match atom {
        Atom::NotNull => s.nonnull,
        Atom::IsNull => s.null,
        Atom::Eq(k) => {
            if contains(&s.eqs, k, budget) {
                return true;
            }
            // 閉区間 [k, k] に潰れている場合。
            match (&s.lo, &s.hi) {
                (Some(lo), Some(hi)) if lo.inclusive && hi.inclusive && k.ordered() => {
                    budget.equal(&lo.key, k) && budget.equal(&hi.key, k)
                }
                _ => false,
            }
        }
        Atom::In(ks) => {
            if s.eqs.iter().any(|q| contains(ks, q, budget)) {
                return true;
            }
            for t in &s.ins {
                if t.iter().all(|q| contains(ks, q, budget)) && !budget.exhausted {
                    return true;
                }
            }
            match (&s.lo, &s.hi) {
                (Some(lo), Some(hi)) if lo.inclusive && hi.inclusive && lo.key.ordered() => {
                    budget.equal(&lo.key, &hi.key) && contains(ks, &lo.key, budget)
                }
                _ => false,
            }
        }
        Atom::Range { lo, hi } => {
            if lo.is_none() && hi.is_none() {
                return false;
            }
            lo.iter().all(|p| lo_ok(&s.lo, p, budget)) && hi.iter().all(|p| hi_ok(&s.hi, p, budget))
        }
    }
}

/// `query`（Q）の項に `needle` と構造的に同一のものがあるか（1 項 1 単位消費）。
fn structurally_present<T: PartialEq>(query: &[T], needle: &T, budget: &mut Budget) -> bool {
    for q in query {
        if !budget.spend(1) {
            return false;
        }
        if q == needle {
            return true;
        }
    }
    false
}

/// 予算を指定した本体。テストが小さい予算で打ち切りを検証できるよう分離している。
fn implies_with_budget(
    query_metadata: &[MetadataFilter],
    query_exprs: &[BoundExpr],
    query_or_groups: &[BoundOrGroup],
    index: &BoundConjunction,
    schema: &TableSchema,
    steps: usize,
) -> bool {
    if index.metadata_filters.is_empty()
        && index.expr_filters.is_empty()
        && index.or_groups.is_empty()
    {
        return false;
    }
    let mut budget = Budget {
        left: steps,
        exhausted: false,
    };

    // Q の認識済み原子を対象ごとに畳む。
    let mut summaries: Vec<(Subject, Summary<'_>)> = Vec::new();
    let atoms = query_metadata
        .iter()
        .filter_map(metadata_atom)
        .chain(query_exprs.iter().filter_map(|e| expr_atom(e, schema)));
    for (subject, atom) in atoms {
        let pos = match summaries.iter().position(|(s, _)| *s == subject) {
            Some(p) => p,
            None => {
                summaries.push((subject, Summary::default()));
                summaries.len() - 1
            }
        };
        if let Some((_, summary)) = summaries.get_mut(pos) {
            summary.absorb(atom, &mut budget);
        }
    }
    let empty = Summary::default();
    let summary_of = |subject: Subject| {
        summaries
            .iter()
            .find(|(s, _)| *s == subject)
            .map_or(&empty, |(_, summary)| summary)
    };

    let mut ok = true;
    for pf in &index.metadata_filters {
        let implied = structurally_present(query_metadata, pf, &mut budget)
            || metadata_atom(pf)
                .is_some_and(|(subject, atom)| holds(summary_of(subject), &atom, &mut budget));
        if !implied {
            ok = false;
            break;
        }
    }
    if ok {
        for pe in &index.expr_filters {
            let implied = structurally_present(query_exprs, pe, &mut budget)
                || expr_atom(pe, schema)
                    .is_some_and(|(subject, atom)| holds(summary_of(subject), &atom, &mut budget));
            if !implied {
                ok = false;
                break;
            }
        }
    }
    if ok {
        // P の OR 群は構造的同一だけを含意とみなす。
        ok = index
            .or_groups
            .iter()
            .all(|pg| structurally_present(query_or_groups, pg, &mut budget));
    }
    ok && !budget.exhausted
}

/// クエリの `WHERE`（`query_*`。AND 連言）が TRUE になる行が必ず索引述語 `index` も TRUE にする
/// ことを保守的に判定する。`true` のときだけ部分索引を使える。判定できない形・予算超過・未認識の
/// P の項はすべて `false`。両辺を同じスキーマ世代で束縛し、プリペアド文は具体値で束縛し直してから
/// 呼ぶこと（Issue #1656 のプランナが呼ぶ契約）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn query_implies_partial_predicate(
    query_metadata: &[MetadataFilter],
    query_exprs: &[BoundExpr],
    query_or_groups: &[BoundOrGroup],
    index: &BoundConjunction,
    schema: &TableSchema,
) -> bool {
    implies_with_budget(
        query_metadata,
        query_exprs,
        query_or_groups,
        index,
        schema,
        MAX_IMPLICATION_STEPS,
    )
}

#[cfg(test)]
#[path = "index_implication_tests.rs"]
mod tests;
