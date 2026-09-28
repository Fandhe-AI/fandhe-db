# stderr 診断ログの書き込み失敗を RECOVER-8 の例外とする判断

## 状況

Rust のランタイムは起動時に `SIGPIPE` を無視する設定にするため、読み手のいない
パイプへ書くと `write` は（プロセスを強制終了させる `SIGPIPE` ではなく）
`EPIPE` エラーとして失敗する。標準ライブラリの `eprintln!` は書き込みエラーが
起きると panic する（`failed printing to stderr`。fd 2 が最初から閉じている
`EBADF` の場合は標準ライブラリが黙って成功扱いにするため、問題になるのは
「読み手が後から居なくなった」場合に限る）。

この panic は [`panic_hook`](../../crates/engine/src/recovery/panic_hook.rs)
（TASK-97・RECOVER-6）・[`fail_fast`](../../crates/engine/src/recovery/fail_fast.rs)
（TASK-99・RECOVER-8）のフックを経て `std::process::abort()` まで進む
（SIGABRT、終了コード 134）。結果として、ログの受け手（パイプ・ログ収集
プロセス）が閉じただけで、データ整合性とは無関係にサーバープロセス全体が
落ちてしまう。

Issue #943（`docs/design/three-client-e2e-harness.md`「子プロセス stderr の
読み続け契約」節）では、テスト側のハーネス（`ServerGuard` の stderr 読み取り
スレッド）が listen 行取得後も読み続けるよう改修することでこの事象を回避した。
production コード側の挙動は当時、別 Issue として扱いを決めることにしていた
（同節末尾の申し送り参照）。

## 決定

オーナー判断（2026-09-27、Issue #1081 コメント。以下は要約で原文の転記では
ない）として、Issue #1081 対応で**診断用 stderr 出力の書き込み失敗はプロセスを
落とさない**（fail-fast の例外とする）。データ整合性に関わる fail-fast
（commit 境界の abort・panic 全般の abort）はこれまでどおり残す。

- 新設ヘルパ `recovery::stderr_log::write_line`・マクロ
  `log_stderr!`（[`crates/engine/src/recovery/stderr_log.rs`](../../crates/engine/src/recovery/stderr_log.rs)）は、
  `io::ErrorKind` を問わず（`EPIPE` に限らず `ENOSPC`・`EIO` 等も含めて）
  書き込みエラーを無視する。
- wire-server の接続ハンドリング・CLI 診断出力（`main.rs`・`server.rs`・
  `protocol_dispatch.rs`・`extended_query.rs`・`auth.rs`・`http/listener.rs`・
  `http/conn.rs`）と engine の実行時ログ（`batch_fallback.rs::
  StderrFallbackObserver`・`recovery::commit_boundary`・`recovery::fail_fast`
  自身のフック文言）を、すべて `eprintln!` からこのヘルパへ置き換えた
  （メッセージ文言は 1 文字も変えていない ―― 既存のログ照合テスト・
  ハーネスとの互換性維持のため）。
- 再混入防止として `crates/engine/src/lib.rs`・`crates/wire-server/src/lib.rs`・
  `crates/wire-server/src/main.rs` の 3 つの crate root に
  `#![cfg_attr(not(test), deny(clippy::print_stderr))]` を追加した
  （`#[cfg(test)]` のテストコード・`test_util` は対象外。`benches/`・
  `examples/` は別の crate root のため影響しない）。

## 例外の範囲

対象は「診断ログとしての stderr 出力の書き込み失敗」のみに限る。

- `catch_unwind` は追加しない。panic は一切捕捉しない
- panic が発生した場合に必ず `std::process::abort()` する契約
  （RECOVER-8・`fail_fast`）は変えない
- commit 成功境界を跨いだ panic の abort 条件（RECOVER-5・
  `commit_boundary`）は変えない
- 緊急応答の送出（RECOVER-6・`panic_hook`）の挙動・優先順位は変えない
- ログに出す項目は増やさない（テナント ID・行データ等を出さない責任は
  引き続き呼び出し側にある）

`hash-password` サブコマンドの `println!`（PHC 文字列を stdout へ出す、
コマンドの成果物そのもの）は対象外とする。stdout が閉じていれば非 0 終了
する現行の挙動を維持する（ログではなく出力そのものであるため、失敗を
無視すると呼び出し元がハッシュを取得できないまま成功扱いになってしまう）。

## 実装

- `crates/engine/src/recovery/stderr_log.rs`: `write_line`（`io::Result` を
  捨てる 1 行書き込み）・`#[macro_export] log_stderr!`（`eprintln!` と同じ
  書式引数を受け付ける）
- `crates/engine/src/recovery.rs`: `pub mod stderr_log;` の登録
- 置き換え箇所: `crates/engine/src/batch_fallback.rs`・
  `crates/engine/src/recovery/commit_boundary.rs`・
  `crates/engine/src/recovery/fail_fast.rs`・wire-server 側の実行時経路と
  CLI 全般（上記「決定」節参照）
- lint: 3 つの crate root への `deny(clippy::print_stderr)`

## 検証

- 単体（サブプロセス隔離。`crates/engine/src/recovery/stderr_log.rs`
  `#[cfg(test)] mod tests`）:
  - `log_stderr_write_failure_does_not_abort`: 親プロセスが子の stderr 読み口
    を drop した後、子が `log_stderr!` を複数回書いても正常終了することを
    確認する
  - `eprintln_write_failure_still_aborts_as_contrast`: 同条件で `eprintln!`
    を使うと SIGABRT で終了することを対照確認する（「読み手が閉じた」状態を
    正しく作れていることの空振り防止）
- 統合（`crates/wire-server/tests/stderr_closed_no_abort.rs`。`#[ignore]`
  なし、`make ci` で常時実行）: `wire-server` 実バイナリの stderr 読み口を
  drop した状態で、`protocol_dispatch::reject_and_close`（FunctionCall 拒否・
  `0A000`）と `connection error` ログ経路（RST 切断。Issue #943 と同型）の
  両方を複数回通しても、プロセスが生存し新規接続へ `ReadyForQuery` まで
  応答し続けることを確認する。`reject_and_close` は ErrorResponse を書く
  **前**にログを出す契約のため、応答が届くこと自体が「閉じたパイプへの
  書き込みが起き、スレッド・プロセスとも死ななかった」ことの非 vacuous な
  証跡になる
- 空振り確認（コミットに含めない一時的な変更として実施）:
  `protocol_dispatch::reject_and_close` の呼び出しを一時的に `eprintln!` へ
  戻し（`deny` も一時的に `allow` へ）、上記統合テストが失敗することを
  確認した。失敗時は `ErrorResponse` の途中で接続が `UnexpectedEof` になり、
  子プロセスの終了状態を直接ポーリングして確かめたところ SIGABRT（シグナル
  番号 6）で終了していた。確認後は元の実装へ復元した
- 既存回帰: `recovery::fail_fast`・`recovery::commit_boundary`・
  `crates/engine/tests/recover6_panic_hook.rs`・
  `crates/wire-server/tests/wire_emergency_response.rs`・
  `crates/wire-server/tests/three_client_e2e.rs::
  server_guard_keeps_draining_stderr_so_logged_connection_errors_do_not_abort_server`
  がいずれも従来どおり通ることを確認した（panic → abort の契約自体は退行
  していない）
- `scripts/check_core_api.sh`: 差分なし（`stderr_log` は `recovery` モジュール
  配下に置き、`lib.rs` の `pub mod`/`pub use` 一覧は変更していないため）

## 影響

- ログが失われる可能性を許容する（オーナー判断）。読み手が居ない間の
  診断ログは失われるが、プロセスの可用性を優先する
- panic は引き続き捕捉されない。stderr の書き込み失敗だけを独立に扱う設計の
  ため、既存の fail-fast・fail-closed の安全性契約への影響はない
- サービスの生存・接続の健全性そのものは、ログ経路とは別の手段
  （プロセス監視・ヘルスチェック等）で確認する運用を前提とする（ログの
  受け手が止まっても可用性の問題としては顕在化しなくなるため）

## ポインタ

TASK-99・RECOVER-5・RECOVER-6・RECOVER-8（`docs/spec/04-behavior/
recovery.md`）。Issue #1081・Issue #943
（`docs/design/three-client-e2e-harness.md`）。
