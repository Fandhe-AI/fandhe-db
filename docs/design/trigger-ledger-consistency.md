# トリガ副作用と operation_id 台帳の整合（Issue #1728）

ポインタ: TABLE-45・TABLE-24・RECOVER-11・RLS-10（spec 本文は転記しない）。実行モデルの D10
（[trigger-execution-model.md](trigger-execution-model.md)）の実装記録。

## 範囲

本 Issue は本番コードを変更せず、D10 が定める性質をテストで固定する。D10 の順序は、事前照合（BEFORE STATEMENT より前）→
BEFORE STATEMENT → 書き込み → AFTER ROW・AFTER STATEMENT（すべてのトリガの副作用）→ 台帳の
記録 → commit で、すべて同じ write txn の単一 commit である。

実装（INSERT 経路の `tenant.rs`）は、この 2 段を分けていない。`ledger::record_in_txn` が照合と
追記を 1 回で行い、BEFORE STATEMENT の発火より前に台帳へ追記する。D10 の順序（全トリガの副作用の
後に記録）とは異なる。ただし追記は同じ write txn 内で、トリガの失敗・abort では txn ごと破棄される。
再送は追記時点で `23505`・`22023` となりトリガを呼ばない。このため、テストが固定する外から見える
性質（再送でトリガ 0 回・副作用なし、失敗時に台帳が残らない）は D10 と一致する。順序そのものを
D10 に揃える変更（事前照合と事後記録への分離）は本 Issue の範囲外で、追跡候補とする。
Issue #1723〜#1727 の実装はこの順序のまま完了している。

## テストと D10 の対応

テストはすべて `crates/engine/src/core/trigger_ledger_tests.rs`。

| 性質 | テスト |
| ---- | ------ |
| 再送は `23505`、トリガ 0 回、副作用・採番不変（INSERT 各形・UPDATE・DELETE・TRUNCATE） | `replay_is_rejected_without_firing_triggers_for_every_statement_form` |
| 内容違いの再送は `22023`。BEFORE の書き換え前の値で照合する | 同上 |
| トリガ内の書き込みは台帳に記録しない | `trigger_side_writes_leave_no_ledger_entry` |
| 入れ子の内側のトリガも再送で呼ばれない | `nested_trigger_effects_are_not_repeated_on_replay` |
| 必須モードの省略は、トリガを呼ぶ前に `23502` | `required_ledger_mode_rejects_before_any_trigger` |
| 明示 Tx の再送・ROLLBACK・COMMIT | `explicit_transaction_replay_and_rollback` |
| 再オープン後の再送 | `replay_after_reopen_is_rejected` |
| 3 テナントの境界・エラー文言が他テナントの有無で変わらない | `tenant_boundary_for_trigger_side_effects_and_ledger` |
| トリガ内の書き込み失敗でも漏えいせず、副作用が残らない | `trigger_write_failures_do_not_leak_or_leave_side_effects` |
| 決定的な中断（abort）で行・監査行・台帳がすべて残らない | `crash_during_trigger_leaves_no_partial_effects` |
| SIGKILL でも整合（オラクル）が保たれる | `sigkill_during_trigger_writes_keeps_ledger_rows_and_side_effects_consistent` |
| オラクルの空振り防止（負の対照） | `oracle_detects_partial_states` |

## crash 注入の方式

本ファイルのテストは `cfg(test)` の `with_trigger_invoker_for_test` で注入した invoker を使う（既定は #3185 で本番の
invoker になり、公開 API の結合テストでも SQL だけで発火できる。注入 invoker は発火側の意味論の固定用に残し、本物の invoker での再検証は下の「本番 invoker での再検証（Issue #3188）」節）。公開 API や feature ゲートを
増やさないため、`recovery::commit_boundary` と同じ自己再帰（`current_exe()` を `--exact` と環境
変数付きで子として再実行）をテストに置いた。新しい example・script・CI ジョブは作らない。

- abort: 副作用を書いた直後に `abort()`（AFTER ROW・BEFORE STATEMENT・TRUNCATE の AFTER
  STATEMENT の 3 か所）。親は SIGABRT を確認し、再オープン後に行・監査行が無いこと、同じ SQL が
  成功してトリガがちょうど 1 回分発火することを確かめる
- SIGKILL: 子が commit のたびに印を出し、親は最初の印を開始同期点として固定待機の後に kill する。
  オラクルは「`trg` の id が連続」「監査行がちょうど 2 倍」「監査 id に欠番なし」「確認済みの commit
  が残る」。再送は `23505`・トリガ 0 回で、次の operation_id は成功する

## 既知の制限・追跡候補

- 台帳の記録を全トリガの副作用の後へ移す分離（D10 の順序との差の解消）は未実施
- script／CI 形の crash 試験は Issue #3188 で `crash_test_sequence.sh` の `trigger` モードとして実現済み（専用の `crash-test-trigger` は作らない。下の節）
- PL/pgSQL 本体の `nextval()` を含む再送・越境の検証は #2228 以降
- wire 経由の INSERT 発火の E2E は #3185（`wire_trigger_fire.rs`）。網羅・crash は #3188 で実施済み
- 共有 `public` 表のトリガの全テナント発火は未決定（[trigger-after-insert.md](trigger-after-insert.md)）
- トリガ定義を置換・削除した後の再送（D10 の既知の性質）は、定義の置換経路の整備後に固定する

## 本番 invoker での再検証（Issue #3188）

既定の invoker が本番の `PlpgsqlTriggerInvoker` になった（#3185〜#3187）ため、注入 invoker で固定してきた性質を、
SQL だけで定義した本体で再検証した。注入 invoker のテストは発火側の意味論の固定用に残す。

| 性質 | 本番 invoker での検証 |
| ---- | ---------------------- |
| 本体の書き込みが他テナントへ届かない・存在情報を漏らさない | `table45_trigger_prod_invoker.rs`（`body_writes_never_reach_other_tenants`・`body_error_does_not_reveal_other_tenants`） |
| 再送は `23505` でトリガ 0 回、内容違いは `22023` | 同（`replay_does_not_refire_any_trigger_level`）・`wire_trigger_fire.rs`（末尾 `;` 付き） |
| `Ledgered` は外側だけが operation_id を要求。台帳を持たない構成でも本体は台帳に触れない | 同（`ledgered_mode_requires_outer_op_id_only`・`compare_only_without_ledger_fires_without_touching_ledger`・`body_target_tables_are_not_recorded`） |
| 本体の失敗は全部取り消され、同じ operation_id で再試行できる | 同（`failed_body_leaves_nothing_and_retry_fires_once`） |
| 明示 Tx の COMMIT・ROLLBACK・再送、再オープン後の再送 | 同（`explicit_transaction_commit_rollback_and_resend`・`replay_after_reopen_does_not_refire`） |
| 未対応の本体の形は `0A000` で残渣なし | 同（`unsupported_body_forms_are_0a000_without_residue`） |
| SIGKILL でも文は全か無か（本表・監査・カウンタ）・接頭辞性・第 2 テナント不変 | `crash_tool_sequence` の `*-trigger`（`scripts/crash_test_sequence.sh` の `trigger` モード） |

- crash 試験の置き場は `crash_test_sequence.sh` の 1 モードで、`DO` ブロックの段（#2229）と同じ方式にした。
  専用の `crash-test-trigger` ターゲットや CI ジョブは作らない（必須チェック名の増減を避け、`make crash-test-sequence` が
  自動的に本番 invoker の crash を検証する）
- 台帳は、対象表がテナント SCHEMA 配下で内部キーになるため `operation_recorded` で直接照会できない。crash の
  `finish-trigger` が、適用済みの全 `k` の再送が `23505` になること、未適用の `k` が `23505` にならず 1 回分だけ発火すること
  （台帳に未適用の `k` が載っていないこと）で検証する
- 深さ上限（`54001`）は、本体の書き込み先が public 表の裸の名前に限られ、トリガ表が SCHEMA 配下に限られるため本番
  invoker では再帰を構成できず、注入 invoker の `core/trigger_depth_tests.rs` が担う
