# ADR: テーブル単位 `operation_id` 台帳（RECOVER-2・Issue #81）

- ステータス: Accepted
- 対応: Issue #81（TASK-93）
- 関連: `docs/spec/05-tasks.md`（TASK-92・TASK-93・TASK-94・TASK-98・TASK-101）・
  `docs/spec/04-behavior/recovery.md`（RECOVER-2・関連: RECOVER-1・RECOVER-3・
  RECOVER-7・RECOVER-10）・`docs/spec/04-behavior/error-format.md`（ERR-2）・
  `docs/spec/04-behavior/sql-surface.md`（SQL-10）

## 概要

Issue #81（TASK-93）は、TASK-92（RECOVER-1）が必須化した `operation_id` を、
テナント内・テーブル単位の永続台帳として、当該書き込みと同一 `redb::WriteTransaction`
内で原子的に記録する基盤を追加する対応である。検討内容・判断の根拠は private spec
（`docs/spec/04-behavior/recovery.md` RECOVER-2）側で管理する。本ドキュメントは、
対応する公開コード上の参照先を記録するためのポインタである。本リポジトリで公開して
いる設計方針の範囲は README.md「実装方針（要点）」の通りであり、それを超える内容は
ここに記載しない。

## スコープ

- 台帳テーブル・複合キー設計・行書き込みと同一トランザクションでの原子的追記
- 既存エントリの非置換（keep-first）
- テスト・後続タスク用の最小照会 API（`operation_recorded`）

以下は本タスクのスコープ外（後続タスクの管轄）:

- 同一 `operation_id` 再送の重複拒否（`23505`）・事前チェック（TASK-94）
- 内容正規化ハッシュ・不一致検出（`22023`）（TASK-101）
- 二層台帳照会・拡張レイアウト（TASK-98）
- ファイル形 `INSERT`（置換セマンティクス経路）への台帳結線は、関連 PR の
  マージ状況に応じたフォローアップ（本 PR のスコープ外事項として別途記録）

## 影響を受けるコード

- `crates/engine/src/recovery/ledger.rs`（新規: 台帳テーブル定義・`LedgerWrite`・
  `RecordOutcome`・`LedgerLookup`・`record_in_txn`・`contains_in_read_txn`）
- `crates/engine/src/recovery.rs`（`pub mod ledger;` 追加）
- `crates/engine/src/recovery/required_op_id.rs`（`LedgerMode::resolve` 追加）
- `crates/engine/src/tenant.rs`（`insert_row`/`insert_rows`/`insert_typed_row`/
  `update_row`/`delete_row` の `*_unchecked` 実体への `LedgerWrite` 引数追加・
  台帳追記の結線・`operation_recorded` 追加）
- `crates/engine/src/core.rs`（`EngineCore::{insert_row, update_row, delete_row}` の
  `resolve` 化・`operation_recorded` 追加）
- `crates/engine/src/sql/exec.rs`（`execute_insert` への `LedgerMode` 引数追加・
  台帳追記の結線）
- `crates/engine/tests/recovery_ledger.rs`（結合テスト。原子性・テーブル単位・
  テナント単位・永続性・`CompareOnlyWithoutLedger`・keep-first の検証）

## Issue #2196 追記（行識別の rowid／主キー基準）

親 #1710（TABLE-23 (4)・RECOVER-2・RECOVER-10 のポインタ）。台帳のキー・値（v1／v2／v3）は行識別子を保持せず、フォーマットは変更しない。内容照合ハッシュの行識別材料は**要求由来の `id`（主キー）値**で、行ストアの物理 rowid を含めない（サーバー採番値を混ぜると同一要求の再送が別ハッシュになるため）。出力バイト列は移行前後で不変で、`content_hash` の golden テスト（`id_based_hash_layouts_are_golden_across_rowid_migration` ほか）と、旧形式エントリの再送判定（`23505`／`22023`）を再オープン後に確認する `recovery/table23_ledger_compat_tests.rs` で固定した。

## 参照

- `docs/spec/05-tasks.md`（TASK-92・TASK-93・TASK-94・TASK-98・TASK-101）
- `docs/spec/04-behavior/recovery.md`（RECOVER-2）
- `docs/spec/04-behavior/error-format.md`（ERR-2）
- `docs/spec/04-behavior/sql-surface.md`（SQL-10）
