# OPERATION_ID の任意化の方針（ポインタ）

## ステータス・対象

- ステータス: 設計段階（実装は後続 Issue）。詳細な契約は公開境界の確認が済むまで本メモに記載せず、ポインタ表記に留める（[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 親: #1701（operation_id を既定で省略可能にし、従来の必須契約は opt-in の CLI フラグ `--require-operation-id` で選べる方針）
- ビヘイビア ID: RECOVER-1（`docs/spec/04-behavior/recovery.md`）・HTTP-14（NoSQL 表層）。補助として RECOVER-2・RECOVER-7・SQL-10・ERR-2
- 関連: [operation-id-required-guard.md](operation-id-required-guard.md)・[operation-id-ledger.md](operation-id-ledger.md)・[partitioned-dml.md](partitioned-dml.md)・[error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)

## 背景

現状は書き込み文と NoSQL の書き込み op で operation_id が必須であり、省略すると `23502` で拒否される（`crates/engine/src/recovery/required_op_id.rs`）。親 #1701 の方針に沿って任意化を進める。

## 後続 Issue の担当

| Issue | 担当 |
| ----- | ---- |
| #1703 | `--require-operation-id` フラグ（wire-server） |
| #1704 | engine 側の任意モードと SQL 経路 |
| #1705 | 回帰テスト |
| #2203 | NoSQL（HTTP）経路 |

任意モードの既定値・例外・照会契約・設定制約・経路別の扱いなどの詳細は、公開境界の確認後に各 Issue で確定し、必要なら本メモを追記する。

## Issue #1703 追記

- `--require-operation-id` は値を取らない起動時スイッチ。解決は `crates/wire-server/src/require_operation_id_opt.rs` の `resolve` に一本化し、`EngineCore::with_ledger_mode` へ 1 回だけ注入する（セッション・SQL・HTTP からは変更できない）
- 指定時は `LedgerMode::Ledgered`（現行の必須契約）。未指定時は `LedgerMode::default()` を注入するだけで、現時点の挙動は変わらない。任意モードの variant 追加と既定の移行は #1704 の担当
- 未指定を `CompareOnlyWithoutLedger` へ写像しない（台帳書き込みが無効になり再送の冪等性が失われるため）
- 重複指定・`=` 連結形・後続の値は起動エラー（fail-closed）。指定時のみ起動ログに英語 1 行を出す
- 受け入れ条件のうち「未指定時に省略を受理する」実挙動は #1704 マージ後に成立する

## スコープ外

- コード変更（後続 Issue で実施）
- `docs/spec` の変更
