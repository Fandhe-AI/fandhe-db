//! トリガ実行の通過証（TABLE-45・Issue #1723）。
//!
//! 発火に対応した書き込み経路だけが `Some(&TriggerRuntime)` を `tenant` の挿入関数へ渡す。
//! `None`（未結線の経路）で INSERT トリガのある表へ書こうとすると `trigger::plan` が `0A000` で
//! 拒否する（黙ってトリガを飛ばさない。実行モデルの D8）。`EngineCore` が 1 つ持つ。

use std::sync::Arc;

use super::call::{TriggerFunctionInvoker, UnwiredInvoker};

/// 呼び出し口を保持する通過証。フィールドは非公開で、構築はこのモジュールだけが行う。
pub(crate) struct TriggerRuntime {
    invoker: Arc<dyn TriggerFunctionInvoker>,
}

impl TriggerRuntime {
    /// 既定（未結線。トリガ関数の実行は `0A000`）。
    pub(crate) fn unwired() -> Self {
        Self {
            invoker: Arc::new(UnwiredInvoker),
        }
    }

    /// 呼び出し口を差し替えた通過証（#2228 が結線する。本 Issue ではテストが使う）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_invoker(invoker: Arc<dyn TriggerFunctionInvoker>) -> Self {
        Self { invoker }
    }

    pub(super) fn invoker(&self) -> &dyn TriggerFunctionInvoker {
        self.invoker.as_ref()
    }
}
