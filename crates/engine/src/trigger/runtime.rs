//! トリガ実行の通過証（TABLE-45・Issue #1723・#1727）。
//!
//! 発火に対応した書き込み経路だけが `Some(&TriggerRuntime)` を `tenant` の挿入関数へ渡す。
//! `None`（未結線の経路）で INSERT トリガのある表へ書こうとすると `trigger::plan` が `0A000` で
//! 拒否する（黙ってトリガを飛ばさない。実行モデルの D8）。`EngineCore` が 1 つ持つ。
//!
//! トリガ関数の呼び出しは [`TriggerRuntime::invoke`] だけを通り、入れ子の深さ上限
//! （`trigger_limits`）を `trigger::depth` で検査する（超過は `54001`）。

use std::sync::Arc;

use super::call::{TriggerCall, TriggerFunctionInvoker, TriggerOutcome, UnwiredInvoker};
use super::depth;
use crate::sql::allowlist::SqlSurfaceError;
use crate::trigger_limits::TriggerLimits;

/// 呼び出し口と深さ上限を保持する通過証。フィールドは非公開で、構築はこのモジュールだけが行う。
pub(crate) struct TriggerRuntime {
    invoker: Arc<dyn TriggerFunctionInvoker>,
    limits: TriggerLimits,
}

impl TriggerRuntime {
    /// 既定（未結線。トリガ関数の実行は `0A000`。深さ上限は既定）。
    pub(crate) fn unwired() -> Self {
        Self {
            invoker: Arc::new(UnwiredInvoker),
            limits: TriggerLimits::default(),
        }
    }

    /// 呼び出し口を差し替えた通過証（本番の結線は #1720。内部 API は #2228。テストが使う）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_invoker(invoker: Arc<dyn TriggerFunctionInvoker>) -> Self {
        Self {
            invoker,
            limits: TriggerLimits::default(),
        }
    }

    /// 呼び出し口を保ったまま深さ上限だけ差し替える（起動時の `EngineCore::with_trigger_limits`）。
    pub(crate) fn with_limits(mut self, limits: TriggerLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 現在の深さ上限。
    pub(crate) fn limits(&self) -> TriggerLimits {
        self.limits
    }

    /// トリガ関数を 1 回呼ぶ唯一の入口。深さが上限に達していれば呼び出し口に触れず `54001`。
    /// 深さは呼び出しの間だけ +1 され、戻り（エラー・panic を含む）で必ず戻る。
    pub(super) fn invoke(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        let _guard = depth::enter(self.limits.max_trigger_depth())?;
        self.invoker.invoke(call)
    }
}
