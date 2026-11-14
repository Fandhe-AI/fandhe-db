//! トリガ実行の通過証（TABLE-45・Issue #1723・#1727）。
//!
//! 発火に対応した書き込み経路だけが `Some(&TriggerRuntime)` を `tenant` の挿入関数へ渡す。
//! `None`（未結線の経路）で INSERT トリガのある表へ書こうとすると `trigger::plan` が `0A000` で
//! 拒否する（黙ってトリガを飛ばさない。実行モデルの D8）。`EngineCore` が 1 つ持つ。
//!
//! トリガ関数の呼び出しは [`TriggerRuntime::invoke`] だけを通り、入れ子の深さ上限
//! （`trigger_limits`）を `trigger::depth` で検査する（超過は `54001`）。

use std::sync::Arc;

#[cfg(test)]
use super::call::UnwiredInvoker;
use super::call::{
    PlpgsqlTriggerInvoker, TriggerCall, TriggerFunctionInvoker, TriggerHost, TriggerOutcome,
};
use super::depth;
use crate::sql::allowlist::SqlSurfaceError;
use crate::trigger_limits::TriggerLimits;

/// 呼び出し口と深さ上限を保持する通過証。フィールドは非公開で、構築はこのモジュールだけが行う。
///
/// `'h` は束縛されたホスト（[`TriggerHost`]）の借用の寿命。`EngineCore` が保持する値は未束縛
/// （`'static`・`host` なし）で、書き込み経路へ渡すときだけ [`TriggerRuntime::bind`] で文ごとに
/// ホストを束縛する（Issue #3184。本番の invoker が `&EngineCore` を要するため）。
pub(crate) struct TriggerRuntime<'h> {
    invoker: Arc<dyn TriggerFunctionInvoker>,
    limits: TriggerLimits,
    host: Option<&'h dyn TriggerHost>,
}

impl TriggerRuntime<'static> {
    /// 本番の既定（Issue #3185）。invoker は `PlpgsqlTriggerInvoker`、ホスト未束縛、深さ上限は既定。
    /// `EngineCore::assemble` が全構築子の共通点で 1 回だけ使う。
    pub(crate) fn production() -> Self {
        Self {
            invoker: Arc::new(PlpgsqlTriggerInvoker),
            limits: TriggerLimits::default(),
            host: None,
        }
    }

    /// テスト専用の未結線（トリガ関数の実行は常に `0A000`）。発火側の意味論を invoker から切り離して
    /// 固定するための差し替え口で、本番のビルドからは到達できない。
    #[cfg(test)]
    pub(crate) fn unwired() -> Self {
        Self {
            invoker: Arc::new(UnwiredInvoker),
            limits: TriggerLimits::default(),
            host: None,
        }
    }

    /// 呼び出し口を差し替えた通過証（テスト専用。本番の既定は [`Self::production`]）。
    #[cfg(test)]
    pub(crate) fn with_invoker(invoker: Arc<dyn TriggerFunctionInvoker>) -> Self {
        Self {
            invoker,
            limits: TriggerLimits::default(),
            host: None,
        }
    }

    /// 呼び出し口を保ったまま深さ上限だけ差し替える（起動時の `EngineCore::with_trigger_limits`）。
    pub(crate) fn with_limits(mut self, limits: TriggerLimits) -> Self {
        self.limits = limits;
        self
    }
}

impl<'h> TriggerRuntime<'h> {
    /// 呼び出し口と上限を保ったまま、文の実行の間だけ `host` を束縛した通過証を返す（`Arc` の複製と
    /// 上限の複写だけ）。`EngineCore` の書き込み経路が `Some(&self.trigger_runtime.bind(self))` の形で
    /// 渡す。ホストが無い（未束縛の）通過証で本番の invoker が呼ばれたら `0A000`（fail-closed）。
    pub(crate) fn bind<'b>(&self, host: &'b dyn TriggerHost) -> TriggerRuntime<'b> {
        TriggerRuntime {
            invoker: Arc::clone(&self.invoker),
            limits: self.limits,
            host: Some(host),
        }
    }

    /// 現在の深さ上限。
    pub(crate) fn limits(&self) -> TriggerLimits {
        self.limits
    }

    /// 束縛済みのホスト（未束縛なら `None`）。`trigger::fire` が [`TriggerCall`] へ載せる。
    pub(crate) fn host(&self) -> Option<&'h dyn TriggerHost> {
        self.host
    }

    /// トリガ関数を 1 回呼ぶ唯一の入口。深さが上限に達していれば呼び出し口に触れず `54001`。
    /// 深さは呼び出しの間だけ +1 され、戻り（エラー・panic を含む）で必ず戻る。
    pub(super) fn invoke(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        let _guard = depth::enter(self.limits.max_trigger_depth())?;
        self.invoker.invoke(call)
    }
}
