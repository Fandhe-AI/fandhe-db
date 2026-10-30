//! トリガ呼び出しの入れ子深さの計数（TABLE-45 (5)・Issue #1727）。
//!
//! `trigger::runtime::TriggerRuntime::invoke`（BEFORE/AFTER × ROW/STATEMENT の全発火が通る単一点）
//! から [`enter`] を呼び、現在の深さが上限に達していれば `54001` で拒否する。深さはスレッド
//! ローカルのカウンタで、RAII の [`DepthGuard`] が drop で戻す（早期 return・エラー・panic でも
//! 漏れない）。トリガ本体が `TriggerWriteHandle` 経由で再入しても、将来 PL/pgSQL ホストが
//! `EngineCore` の通常経路で SQL を実行しても、同一スレッドで同期的に実行される限り深さは失われない
//! （`tenant::trigger_writes` の記録区間と同じ前提）。

use std::cell::Cell;

use crate::sql::allowlist::SqlSurfaceError;

thread_local! {
    static CURRENT_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// 深さを 1 つ進めた証。drop で戻す。
pub(crate) struct DepthGuard {
    _private: (),
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        CURRENT_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// 上限 `max` に対して 1 段入る。`current >= max` なら `54001`（固定文言。値・表名を含めない）。
pub(crate) fn enter(max: usize) -> Result<DepthGuard, SqlSurfaceError> {
    CURRENT_DEPTH.with(|d| {
        let current = d.get();
        if current >= max {
            return Err(SqlSurfaceError::StatementTooComplex {
                detail: "trigger nesting depth limit exceeded",
            });
        }
        d.set(current.saturating_add(1));
        Ok(DepthGuard { _private: () })
    })
}

/// テスト用: 現在のスレッドの深さ。
#[cfg(test)]
pub(crate) fn current_depth() -> usize {
    CURRENT_DEPTH.with(|d| d.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nests_and_restores_on_drop() {
        assert_eq!(current_depth(), 0);
        let g1 = enter(2).expect("1");
        let g2 = enter(2).expect("2");
        assert_eq!(current_depth(), 2);
        assert!(enter(2).is_err());
        assert_eq!(current_depth(), 2);
        drop(g2);
        assert_eq!(current_depth(), 1);
        drop(g1);
        assert_eq!(current_depth(), 0);
    }
}
