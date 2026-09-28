//! TASK-92（対象ビヘイビア: RECOVER-1）の入口モジュール。障害回復系のガード・台帳の
//! 実装をここへ集約する（ポインタ: docs/spec/05-tasks.md TASK-92〜94・TASK-96・
//! TASK-98・TASK-101・docs/spec/04-behavior/recovery.md RECOVER-1〜3・RECOVER-5・
//! RECOVER-7・RECOVER-10）。
//!
//! [`required_op_id`] が `operation_id` 必須化ガード（RECOVER-1）を提供する
//! （詳細は `required_op_id` モジュールドキュメント参照）。[`ledger`] がテーブル単位
//! `operation_id` 台帳（TASK-93、対象ビヘイビア: RECOVER-2）を提供する（詳細は
//! `ledger` モジュールドキュメント参照）。[`content_hash`] が台帳エントリの内容照合
//! ハッシュ（TASK-101、対象ビヘイビア: RECOVER-10）を提供する（詳細は
//! `content_hash` モジュールドキュメント参照）。重複拒否（`23505`、TASK-94・
//! RECOVER-3）は独立モジュールを持たず、[`ledger::record_in_txn`] が内容ハッシュ照合
//! の一部として（一致なら `23505`・不一致なら `22023`）直接判定する。
//! [`commit_boundary`] が commit 成功境界と応答一意性の保証（TASK-96、対象
//! ビヘイビア: RECOVER-5）を提供する（詳細は `commit_boundary` モジュール
//! ドキュメント参照）。
//!
//! [`ledger`] は TASK-98（対象ビヘイビア: RECOVER-7）の対応も含む（詳細は
//! `ledger` モジュールドキュメント参照）。
//!
//! [`panic_hook`] が commit 成功境界の観測可能性側（TASK-97、対象ビヘイビア:
//! RECOVER-6・ERR-1）を提供する（詳細は `panic_hook` モジュールドキュメント
//! 参照）。[`commit_boundary`] の abort（安全性側）を無効化せず、その前段で
//! 緊急応答の送出を試みる。
//!
//! [`fail_fast`] が内部エラーの 2 系統統一（TASK-99、対象ビヘイビア:
//! RECOVER-8）を提供する（詳細は `fail_fast` モジュールドキュメント参照）。
//! `Result::Err` は ERR-1 応答で処理継続、panic は経路・スレッドを問わず
//! プロセスを終了させる ―― `commit_boundary`・`panic_hook` の狭い
//! commit-pending 限定の abort とは異なり、プロセス全体への最終防衛線となる。
//!
//! [`stderr_log`] は `fail_fast`（RECOVER-8）の例外として、診断用 stderr
//! 出力の書き込み失敗（読み手が閉じた後の `EPIPE` 等）を無視する
//! （Issue #1081、判断の記録: `docs/design/stderr-log-write-failure.md`）。
//! `fail_fast` 本体の abort 契約は変えない ―― 例外の範囲は診断ログの
//! 書き込みそのものに限る。

pub mod commit_boundary;
pub(crate) mod content_hash;
pub mod fail_fast;
pub mod ledger;
pub mod panic_hook;
pub mod required_op_id;
pub mod stderr_log;
