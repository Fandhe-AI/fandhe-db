# SQL 表層 複数行 INSERT（SQL-16・TASK-190）結線の判断記録

対象ビヘイビア（ポインタ表記。spec 本文は転記しない）: `docs/spec/04-behavior/
sql-surface.md` SQL-16・`docs/spec/05-tasks.md` TASK-190。関連: SQL-10
（TASK-80・単一行 INSERT）・INDEX-4（TASK-122・一括投入の処理量上限）・
RECOVER-10（TASK-101・台帳エントリの内容照合ハッシュ）・RECOVER-11（内容照合
ハッシュの行順依存性）。

## 1. 背景

複数行 `INSERT INTO <table> (...) VALUES (...), (...), ... USING OPERATION_ID
'<id>'`（行形のみ。ファイル形 `path`/`body` 列との併用は対象外）の構文受理は
Issue #862・PR #978（`5df4fc1`）で実装済み。本 Issue（#863）時点で、実行本体
への結線（`execute_insert_batch` への到達・INDEX-4 上限適用）もその PR が
既に実装していたことが実装着手時の調査で判明したため、本 Issue の実体は
「結線の再実装」ではなく「受入基準 4 項目を層 A で確定させる残差テストの
追加」と「PR #978 が申し送った doc・コメントの陳腐化解消」である。

## 2. 経路図

```text
SQL テキスト
  └─ sql::allowlist::validate_insert_tokens（許可リスト検証。複数行 VALUES を
     複数の値タプルとして受理。列数不一致は 42601）
       └─ sql::parser::bind_insert_form（スキーマに基づく型束縛）
            ├─ 単一行 VALUES (...)        → BoundInsertForm::Row(BoundInsert)
            ├─ 複数行 VALUES (...), (...)  → BoundInsertForm::RowBatch(Vec<BoundInsert>)
            └─ path/body 列（ファイル形）  → BoundInsertForm::File(BoundFileInsert)
                 （RowBatch との併用は 42601。allowlist 段で拒否）
                       │
                       ▼（RowBatch のみ）
       core::EngineCore::execute_insert_form の RowBatch 分岐
         1. ①行数上限: bounds.len() > self.batch_limits.max_files_per_batch → 54000
         2. ②③④: EngineCore::validate_insert_batch_byte_and_chunk_limits
            （batch_limits::validate_batch_shape・validate_chunk_total）→ 54000
         3. sql::exec::execute_insert_batch_with_schema（単一 write トランザク
            ション・単一台帳エントリ・operation_id 必須化ガード込み）
```

NoSQL 表層 `rows[]`（NOSQL-6・TASK-178）は
`core::EngineCore::execute_bound_insert_in_session` から同じ
`validate_insert_batch_byte_and_chunk_limits`・`execute_insert_batch_with_schema`
を呼ぶ。**第 2 の書き込み経路は作らない**設計（`sql::exec::
execute_insert_batch_with_schema` のドキュメンテーションコメント参照）。

## 3. 行数上限の 2 系統

複数行 `VALUES` の行数には独立な 2 つの上限がある。

| 系統 | 定義箇所 | 判定タイミング | 性質 |
| ---- | -------- | --------------- | ---- |
| 構文段上限 | `sql::parser::MAX_INSERT_ROWS_PER_STATEMENT`（1,000） | `bind_insert_form` 内・束縛の一部 | 固定値。1 文の構文解析結果自体を有界にする |
| 運用上限 | `self.batch_limits.max_files_per_batch`（INDEX-4 ①。既定 64・環境変数で上書き可） | `execute_insert_form` の `RowBatch` 分岐・束縛後 | 運用者が調整可能。NoSQL 表層 `rows[]` と共有 |

構文段上限（1,000）は運用上限（既定 64）より大きいため、通常運用では運用上限
が先に効く。ただし運用上限を 1,000 超へ引き上げた場合でも構文段上限が
天井として残るため、束縛後の②③④判定（`validate_insert_batch_byte_and_chunk_
limits`）が想定外に大きい `Vec<BoundInsert>` を受け取ることはない（有界性の
二重防御。coding-rust.md「不安全な設計 / DoS」対応の設計判断として採用）。

## 4. 表層間の判定順序の違い（許容した設計判断）

- NoSQL 表層（`execute_bound_insert_in_session`）: ①行数上限をスキーマ取得
  **前**に判定する（束縛済み `bounds` を要求しない軽量ガードのため）。
- SQL 表層（`execute_insert_form`）: ①行数上限を `bind_insert_form` による
  束縛（スキーマ取得後）の**後**に判定する（SQL テキストの構文検証・型束縛が
  スキーマに依存するため、束縛前に行数だけを取り出せない）。

この順序差は構文段上限（§3）により束縛自体が有界なため、DoS 耐性上の実害は
ない（束縛処理量は最大でも 1,000 行分）と判断し、判定順序を表層間で無理に
揃える変更は行わなかった。

## 5. 原子性・台帳キー空間・RLS の維持契約

- **原子性**: `execute_insert_batch_with_schema` は単一の write トランザクション
  内で「台帳エントリの記録 → 全行の書き込み」を行う。バッチ内 `id` 重複・
  既存テナント行との `id` 衝突のいずれでも、衝突検出時点で write トランザクション
  がコミットされずに終了するため、部分書き込みは残らない
  （`crates/engine/tests/insert_multi_row.rs::
  multi_row_insert_id_collision_with_existing_row_rejects_whole_statement`
  で固定）。
- **台帳キー空間**: 複数行 `VALUES` の `operation_id` は単一行 INSERT・NoSQL
  `rows[]` と同一の `(tenant, table, operation_id)` 台帳キー空間を共有する
  （`bounds.len() == 1` は `execute_insert_with_schema` へ委譲し単一行と
  同一ハッシュ空間になる設計。表層を跨いだ再送も同じキーで判定される）。
- **RLS**: 可視性は常に `Visibility::Private` に固定（単一行 INSERT・NoSQL
  `insert` と同じ）。他テナント名義での書き込みは `PolicyContext::is_owner`
  経由で遮断される。

## 6. 層 A テストの対応表

| 受入基準 | 検証ファイル |
| -------- | ------------ |
| ①原子性（部分成功なし） | `crates/engine/tests/insert_multi_row.rs`（`multi_row_insert_id_collision_with_existing_row_rejects_whole_statement`・`multi_row_insert_rejects_duplicate_id_within_batch`） |
| ②INDEX-4 4 上限（54000・副作用ゼロ） | `insert_multi_row.rs`（①行数・②行バイト量・③バッチ合計バイト量・④チャンク総量。いずれも読み戻し 0 件・台帳未記録を確認） |
| ③バッチ内 `id` 重複の fail-closed 拒否 | `insert_multi_row.rs::multi_row_insert_rejects_duplicate_id_within_batch` |
| ④台帳キー空間の共有 | `insert_multi_row.rs`（単一行→複数行・複数行→単一行の双方向 `23505`／`22023`）・`crates/wire-server/tests/nosql6_insert.rs`（SQL 複数行 ⇄ HTTP `rows[]` の双方向） |
| wire フレーミング越しの確認（層 A） | `crates/wire-server/tests/wire_insert_operation_id.rs`（`INSERT 0 N` タグ・`54000`・バッチ内重複 `23505`・接続維持・RLS-11 読み戻し） |

## 7. 申し送り（本 Issue の対象外）

- SQL-16 の wire 経由 3 クライアント層 B 検証（`extended_syntax_e2e.rs`）は
  PR #978 と同じく対象外のまま。
- ファイル形（`path`/`body`）INSERT への複数行 `VALUES` 併用は `42601` の
  まま（別構文の検討は別 Issue）。
- 上記はユーザー承認なしに Issue を起票せず、PR 本文の「対象外」節へ記載する。

## 移行済み表での `id` 省略（Issue #1700）

移行済み表で列リストから `id` を省略した複数行 `VALUES` は、1 文 = 1 台帳エントリのまま行ごとに
採番する。内容照合ハッシュは `id` を材料にしない別タグ（`InsertIdentityBatch`・
`InsertIdentityBatchExpr`）で、全行が明示 `id` のバッチは従来のタグ・バイト列のまま。
詳細は [migrated-id-identity.md](migrated-id-identity.md) の D8。
