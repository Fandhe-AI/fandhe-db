//! BEFORE ROW トリガ付き UPDATE／DELETE の行ごとループの区間を表名で記録する（Issue #3186。実行モデルの D6）。
//!
//! 役割: `tenant/trigger_dml.rs` の行ごとループは、BEFORE 前に読んだ OLD から最終行を作って書く。その最中に
//! トリガ本体（`core::plpgsql_trigger_host`）が同じ表を `UPDATE` すると、外側の書き込みが本体の更新を
//! 黙って上書きする（失われた更新）か、後続の候補行が書き換わる。PostgreSQL はこれを `27000` で文全体の
//! 失敗にするが、`ErrorClass` に該当する variant を持たない（追加は公開 enum の破壊的変更）。そこで
//! 保守的に、ループの区間だけ表名を記録し、本体の同表 `UPDATE` を固定文言の `0A000` で拒否させる
//! （fail-closed。行単位の厳密な検出は後続）。
//!
//! 区間はループだけ（制約検査・AFTER ROW・AFTER STATEMENT の前に drop する）。AFTER の段では外側の書き込みが
//! 済んでおり衝突しないため、AFTER トリガからの同表 `UPDATE` は許す。入れ子（BEFORE UPDATE ON a → INSERT
//! INTO b → b のトリガ → UPDATE a）も、記録がスレッド単位の多重集合なので検出できる。スレッドローカルは
//! 1 文の実行中だけ非空で、テナントをまたぐ状態を持たない（1 スレッドで実行する書き込みは単一テナントの文脈）。

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// 表名（ASCII 小文字化）→ 区間の入れ子の数。
    static ACTIVE: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
}

/// 行ごとループの区間を張る RAII ガード。drop（エラー・早期 return・panic を含む）で必ず戻す。
pub(crate) struct RowLoopScope {
    key: String,
}

impl RowLoopScope {
    /// 表 `table` の行ごとループの開始時に呼ぶ。
    pub(crate) fn enter(table: &str) -> Self {
        let key = table.to_ascii_lowercase();
        ACTIVE.with(|a| {
            let mut m = a.borrow_mut();
            let n = m.entry(key.clone()).or_insert(0);
            *n = n.saturating_add(1);
        });
        Self { key }
    }
}

impl Drop for RowLoopScope {
    fn drop(&mut self) {
        ACTIVE.with(|a| {
            let mut m = a.borrow_mut();
            if let Some(n) = m.get_mut(&self.key) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    m.remove(&self.key);
                }
            }
        });
    }
}

/// 表 `table` が BEFORE ROW の行ごとループの最中か。トリガ本体の `UPDATE` の実行前に呼ぶ。
pub(crate) fn is_row_loop_active(table: &str) -> bool {
    ACTIVE.with(|a| a.borrow().contains_key(&table.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_nests_and_restores_on_drop() {
        assert!(!is_row_loop_active("T"));
        let a = RowLoopScope::enter("t");
        assert!(is_row_loop_active("T"));
        {
            let _b = RowLoopScope::enter("T");
            assert!(is_row_loop_active("t"));
        }
        assert!(is_row_loop_active("t"));
        assert!(!is_row_loop_active("other"));
        drop(a);
        assert!(!is_row_loop_active("t"));
    }
}
