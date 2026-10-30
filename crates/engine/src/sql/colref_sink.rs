//! SQL 構文解析器が「列参照として確定した識別子」の位置と役割を記録する受け口（Issue #2224・
//! PL/pgSQL の変数参照解決の基盤）。
//!
//! `sql::allowlist` の `Parser` は列参照を読む確定箇所で [`record`] を呼び、リテラルしか受理しない
//! 位置では [`is_scope_var`] が真のときだけ識別子を受理して [`Role::LiteralSlot`] を記録する。
//! 利用側（`plpgsql` の定義時検証）は [`enter`] が返すガードを生かしたまま既存の検証入口へ
//! 埋め込み SQL のトークン列を通し、[`ColrefGuard::finish`] で記録を取り出す。
//! 呼び出し時は記録された位置を型付きの値トークンへ置き換えて既存の実行経路へ渡す。
//!
//! # 不変条件
//!
//! - **ガードが無いとき既存の挙動を 1 ビットも変えない。** 記録は副作用のみで、構文解析の分岐に
//!   使うのは「ガードがあり、かつ名前がスコープ内の変数」のときだけ（[`is_scope_var`]）。
//! - 保存するのはトークン要素のアドレスから換算した**位置（添字）**だけで、ポインタ・借用は
//!   保持しない（所有データのみ。`const_expr::DEFER_STATE` と同じ構成で非安全コードは不要）。
//! - ガード下で得た AST は分類専用であり実行には使わない（リテラル専用位置では名前の代わりに
//!   プレースホルダ値を AST に入れる）。
//! - ルートのトークン列（および別名登録された 1 対 1 の複製）に解決できない列参照は
//!   [`ColrefReport::out_of_range`] を立てる（記録漏れを黙って許さない fail-closed。
//!   利用側は `0A000` にする）。

use std::cell::RefCell;
use std::collections::HashSet;

use crate::sql::lexer::Token;

/// 記録した列参照の役割。呼び出し時の置換方法（#2226）が役割ごとに異なる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// 式・述語・関数引数など、値として読まれる列参照（修飾付きレコード参照 `rec.field` の
    /// 修飾子側を含む）。
    Value,
    /// WHERE の構造的な葉の先頭（`column: String` を直接持つ形）。
    WhereHead,
    /// 別名なしの投影項目の裸識別子。出力列名が識別子由来のため、置換時は別名付与が必要。
    SelectItemBare,
    /// `ORDER BY` のキー（整数リテラルへの置換は序数参照に化けるため役割を分ける）。
    OrderKey,
    /// `GROUP BY` のキー（`ORDER BY` と同様に序数参照へ化けうる）。
    GroupKey,
    /// リテラルしか受理しない位置（`LIKE` パターン・`IN` リスト・`BETWEEN` 境界・`LIMIT` 等）に
    /// 置かれたスコープ内変数の名前。
    LiteralSlot,
}

/// 1 件の記録（ルートのトークン列上の位置と役割）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Recorded {
    pub(crate) pos: usize,
    pub(crate) role: Role,
}

/// [`ColrefGuard::finish`] が返す収集結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ColrefReport {
    /// 位置昇順・同一位置は最後の記録だけ。
    pub(crate) records: Vec<Recorded>,
    /// 取り込まれたサブクエリ本体のルート上の範囲（`(` と `)` を除く内側）。
    pub(crate) subquery_ranges: Vec<std::ops::Range<usize>>,
    /// ルートへ解決できない列参照・サブクエリ範囲があった（分類失敗。fail-closed）。
    pub(crate) out_of_range: bool,
}

/// トークン列の連続領域（アドレス基準）。
#[derive(Clone, Copy)]
struct Span {
    base: usize,
    len: usize,
}

impl Span {
    fn of(tokens: &[Token]) -> Self {
        Self {
            base: tokens.as_ptr() as usize,
            len: tokens.len(),
        }
    }

    /// `addr` が領域内の要素先頭を指すとき、その添字。
    fn index_of(&self, addr: usize) -> Option<usize> {
        let size = std::mem::size_of::<Token>();
        let off = addr.checked_sub(self.base)?;
        if size == 0 || off % size != 0 {
            return None;
        }
        let idx = off / size;
        (idx < self.len).then_some(idx)
    }
}

/// 1 対 1 の複製（`qualified_column` の正規化・`Parser::new` の降格）の別名登録。
struct Alias {
    id: u64,
    copy: Span,
    source_base: usize,
}

struct SinkState {
    root: Span,
    scope: HashSet<String>,
    aliases: Vec<Alias>,
    next_alias_id: u64,
    records: Vec<Recorded>,
    subquery_ranges: Vec<std::ops::Range<usize>>,
    out_of_range: bool,
}

/// 別名の連鎖の最大段数（正規化 → 降格 → …。無限ループ防止）。
const MAX_ALIAS_CHAIN: usize = 8;

impl SinkState {
    /// トークン要素のアドレスをルート上の添字へ換算する（別名を辿る）。
    fn resolve(&self, mut addr: usize) -> Option<usize> {
        for _ in 0..=MAX_ALIAS_CHAIN {
            if let Some(i) = self.root.index_of(addr) {
                return Some(i);
            }
            // 新しい登録ほど優先（解放済み領域の再利用による古い登録との取り違えを避ける）。
            let alias = self
                .aliases
                .iter()
                .rev()
                .find(|a| a.copy.index_of(addr).is_some())?;
            let off = addr.checked_sub(alias.copy.base)?;
            addr = alias.source_base.checked_add(off)?;
        }
        None
    }
}

thread_local! {
    /// 記録中だけ `Some`。復元は [`ColrefGuard`] の Drop が担う（panic 時も元へ戻る）。
    static SINK: RefCell<Option<SinkState>> = const { RefCell::new(None) };
}

/// 記録区間を表す RAII ガード。生きている間だけ記録が有効で、Drop で前の状態へ戻す。
pub(crate) struct ColrefGuard {
    prev: Option<SinkState>,
}

/// 記録区間を開始する。`root` は検証入口へ渡すトークン列（位置の基準）、`scope_vars` は
/// リテラル専用位置で受理してよい変数名（トークン上の綴りと完全一致で照合する）。
pub(crate) fn enter<I, S>(root: &[Token], scope_vars: I) -> ColrefGuard
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let state = SinkState {
        root: Span::of(root),
        scope: scope_vars.into_iter().map(Into::into).collect(),
        aliases: Vec::new(),
        next_alias_id: 0,
        records: Vec::new(),
        subquery_ranges: Vec::new(),
        out_of_range: false,
    };
    let prev = SINK.with(|c| c.replace(Some(state)));
    ColrefGuard { prev }
}

impl ColrefGuard {
    /// 記録を取り出して区間を終える（前の状態へ戻す）。
    pub(crate) fn finish(self) -> ColrefReport {
        let state = SINK.with(|c| c.replace(None));
        let report = state.map(into_report).unwrap_or_default();
        // `Drop` が前の状態を戻す。
        report
    }
}

impl Drop for ColrefGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        // thread-local 破棄後の Drop 等で失敗しても何もしない（`try_with`）。
        let _ = SINK.try_with(|c| {
            if let Ok(mut st) = c.try_borrow_mut() {
                *st = prev;
            }
        });
    }
}

fn into_report(mut s: SinkState) -> ColrefReport {
    s.records.sort_by_key(|r| r.pos);
    s.subquery_ranges.sort_by_key(|r| (r.start, r.end));
    s.subquery_ranges.dedup();
    ColrefReport {
        records: s.records,
        subquery_ranges: s.subquery_ranges,
        out_of_range: s.out_of_range,
    }
}

fn with_state<R>(f: impl FnOnce(&mut SinkState) -> R) -> Option<R> {
    SINK.try_with(|c| {
        let mut b = c.try_borrow_mut().ok()?;
        b.as_mut().map(f)
    })
    .ok()
    .flatten()
}

/// 記録区間が有効か（ガードの復元を確かめるテスト用）。
#[cfg(test)]
pub(crate) fn is_active() -> bool {
    SINK.try_with(|c| c.try_borrow().map(|b| b.is_some()).unwrap_or(false))
        .unwrap_or(false)
}

/// `name` が記録区間のスコープ内変数か。ガードが無いときは必ず `false`
/// （リテラル専用位置の拡張は、この関数が真のときだけ入る）。
pub(crate) fn is_scope_var(name: &str) -> bool {
    with_state(|s| s.scope.contains(name)).unwrap_or(false)
}

/// 列参照として確定したトークン `tok` の位置と役割を記録する。ガードが無ければ何もしない。
/// ルートへ解決できなければ範囲外フラグを立てる。同一位置は最後の記録が残る。
pub(crate) fn record(tok: &Token, role: Role) {
    let addr = tok as *const Token as usize;
    with_state(|s| match s.resolve(addr) {
        Some(pos) => {
            if let Some(r) = s.records.iter_mut().find(|r| r.pos == pos) {
                r.role = role;
            } else {
                s.records.push(Recorded { pos, role });
            }
        }
        None => s.out_of_range = true,
    });
}

/// 直近に [`Role::Value`] で記録した位置を `role` へ付け替える（位置が呼び出し側から
/// 分からない括弧付き裸識別子用。`record` 呼び出しの直後にだけ使う）。
pub(crate) fn retag_last_value(role: Role) {
    with_state(|s| {
        if let Some(r) = s.last_value_mut() {
            r.role = role;
        }
    });
}

impl SinkState {
    fn last_value_mut(&mut self) -> Option<&mut Recorded> {
        self.records
            .iter_mut()
            .rev()
            .find(|r| r.role == Role::Value)
    }
}

/// 取り込まれたサブクエリ本体（`inner`。ルート上の連続範囲）を登録する。解決できなければ
/// 範囲外フラグを立てる。空の範囲は無視する。
pub(crate) fn register_subquery(inner: &[Token]) {
    let Some(first) = inner.first() else {
        return;
    };
    let addr = first as *const Token as usize;
    let len = inner.len();
    with_state(|s| match s.resolve(addr) {
        Some(start) => match start.checked_add(len) {
            Some(end) => s.subquery_ranges.push(start..end),
            None => s.out_of_range = true,
        },
        None => s.out_of_range = true,
    });
}

/// これまでに取り込まれたサブクエリ本体の範囲（重複・順不同）。利用側が記録区間を保ったまま、
/// 範囲ごとの再帰解析を進める（新しい範囲が出なくなるまで繰り返す）ために使う。
pub(crate) fn pending_ranges() -> Vec<std::ops::Range<usize>> {
    with_state(|s| s.subquery_ranges.clone()).unwrap_or_default()
}

/// 別名登録の解除用ガード（Drop で登録を外す）。ガードが無いとき・登録不要のときは何もしない。
pub(crate) struct AliasGuard {
    id: Option<u64>,
}

impl AliasGuard {
    /// 登録なしのガード（`Parser` の既定値）。
    pub(crate) const fn none() -> Self {
        Self { id: None }
    }
}

impl Drop for AliasGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            with_state(|s| s.aliases.retain(|a| a.id != id));
        }
    }
}

/// `copy` が `source` の 1 対 1 の複製（同じ長さ・同じ並び）であることを登録する。以後 `copy`
/// の要素アドレスは `source` 上の位置へ換算される。同一領域・長さ不一致・ガード無しでは登録しない。
pub(crate) fn register_alias(copy: &[Token], source: &[Token]) -> AliasGuard {
    if copy.len() != source.len() || copy.is_empty() || std::ptr::eq(copy.as_ptr(), source.as_ptr())
    {
        return AliasGuard { id: None };
    }
    let id = with_state(|s| {
        let id = s.next_alias_id;
        s.next_alias_id = s.next_alias_id.wrapping_add(1);
        s.aliases.push(Alias {
            id,
            copy: Span::of(copy),
            source_base: source.as_ptr() as usize,
        });
        id
    });
    AliasGuard { id }
}

/// サブクエリ・CTE 本体の取り込み（`Vec<Token>` への複製）と同時に、元の範囲を
/// [`register_subquery`] へ登録する拡張メソッド。複製はルートの外に出るため、元の範囲を
/// 登録しておき、利用側が範囲ごとに再帰して分類する。
pub(crate) trait RegisterInner {
    /// 登録してから複製する。ガードが無ければ単なる複製。
    fn to_vec_registered(&self) -> Vec<Token>;
}

impl RegisterInner for [Token] {
    fn to_vec_registered(&self) -> Vec<Token> {
        register_subquery(self);
        self.to_vec()
    }
}
