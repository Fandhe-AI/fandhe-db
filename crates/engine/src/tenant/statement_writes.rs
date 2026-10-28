//! データ変更 CTE（SQL-63・Issue #1877）の 1 文内で「先の副文が書いた行」を追跡する文内変更集合。
//!
//! 役割: 副文は `WITH` の定義順に同じ write txn へ書き込むため、後の副文の候補列挙は先の副文が
//! 挿入・更新した行を現在の状態として見てしまう。PostgreSQL の意味（全副文が文の開始時点の状態を読み、
//! 同じ行を複数の副文が変更しない）に揃えるため、書き込んだ `(表, id)` を記録し、後の副文の対象選定
//! （述語形の候補列挙・単一行の `id` 指定）で記録済みの行を「無いもの」として扱う。
//!
//! 呼び出し関係: `core/dm_cte_exec.rs` が副文の実行区間だけ [`StatementWritesScope`] を張る。`tenant.rs` の
//! 行書き込み点（挿入・更新・削除）が [`record`] を、対象選定が [`is_prior`] を呼ぶ。スコープが無いとき
//! （通常の DML・他の全経路）は何もせず、挙動はスコープ導入前とビット同一。
//!
//! テナント境界（P0）: 記録は書き込むセッション自身が所有する行だけ（物理キーが `(tenant, id)` の
//! ため他テナントの行へは書き込めない）。集合はスレッドローカルで 1 文の間だけ生き、他テナントの
//! 行の有無で分岐する処理を持たない。

use std::cell::RefCell;
use std::collections::HashSet;

/// 副文の実行区間で保持する書き込み済みの行の集合。
#[derive(Default)]
struct Writes {
    /// 先に完了した副文が書いた行（後の副文はこれを対象にしない）。
    prior: HashSet<(String, u64)>,
    /// 実行中の副文が書いた行。副文の完了時に `prior` へ移す。
    current: HashSet<(String, u64)>,
}

thread_local! {
    static WRITES: RefCell<Option<Writes>> = const { RefCell::new(None) };
}

/// 文内変更集合のスコープを現スレッドへ張る RAII ガード。drop で直前の状態へ戻す
/// （panic・早期 return でも他の文へ漏れない）。
pub(crate) struct StatementWritesScope {
    prev: Option<Writes>,
}

impl StatementWritesScope {
    /// 空の集合でスコープを開始する。
    pub(crate) fn enter() -> Self {
        let prev = WRITES.with(|w| w.replace(Some(Writes::default())));
        StatementWritesScope { prev }
    }

    /// 実行中の副文が完了したことを記録する（その副文が書いた行を後続の副文の除外対象にする）。
    pub(crate) fn finish_substatement(&self) {
        WRITES.with(|w| {
            if let Some(writes) = w.borrow_mut().as_mut() {
                let current = std::mem::take(&mut writes.current);
                writes.prior.extend(current);
            }
        });
    }
}

impl Drop for StatementWritesScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        WRITES.with(|w| {
            *w.borrow_mut() = prev;
        });
    }
}

fn key(table: &str, id: u64) -> (String, u64) {
    (table.to_ascii_lowercase(), id)
}

/// 行の書き込み（挿入・更新・削除）を記録する。スコープが無いときは何もしない。
pub(crate) fn record(table: &str, id: u64) {
    WRITES.with(|w| {
        if let Some(writes) = w.borrow_mut().as_mut() {
            writes.current.insert(key(table, id));
        }
    });
}

/// 先に完了した副文が書いた行か。スコープが無いときは常に偽。後の副文の対象選定が、この行を
/// 「無いもの」として扱うために呼ぶ。
pub(crate) fn is_prior(table: &str, id: u64) -> bool {
    WRITES.with(|w| {
        w.borrow().as_ref().is_some_and(|writes| {
            !writes.prior.is_empty() && writes.prior.contains(&key(table, id))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_become_prior_only_after_the_substatement_finishes() {
        assert!(!is_prior("t", 1));
        record("t", 1);
        let scope = StatementWritesScope::enter();
        record("T", 1);
        assert!(
            !is_prior("t", 1),
            "same substatement must not exclude its own rows"
        );
        scope.finish_substatement();
        assert!(is_prior("t", 1));
        assert!(!is_prior("t", 2));
        assert!(!is_prior("u", 1));
        drop(scope);
        assert!(!is_prior("t", 1), "scope drop must clear the set");
    }
}
