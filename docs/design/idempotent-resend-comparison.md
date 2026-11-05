# 冪等再送の他 DB・API 方式との比較

- ステータス: Accepted（実装済みの挙動と、他 API・他 DB の公式ドキュメントの記述を並べた比較記録。本書の追加でコードは変えない）
- 対応 Issue: #2069（親 #2066・祖先 #2061）。根拠テストは #2067・#2068
- ビヘイビア ID ポインタ: RECOVER-1・RECOVER-7・RECOVER-10（`docs/spec/04-behavior/recovery.md`）。
  補助として RECOVER-2・RECOVER-3・RECOVER-11・RECOVER-12・HTTP-14。契約本文は spec を正とし、本書へは転記しない
- 他 API・他 DB の記述の基準: 「参照」節の公式ドキュメントを確認日（2026-11-05）に読んだ内容だけを書く。
  確認できなかった点は「公式ドキュメントで確認できず」と書き、記憶で補わない
- 関連コード: `crates/engine/src/recovery/ledger.rs`・`crates/engine/src/recovery/content_hash.rs`・
  `crates/engine/src/recovery/required_op_id.rs`・`crates/engine/src/sql/using_operation_id.rs`
- 関連 doc: [operation-id-ledger.md](operation-id-ledger.md)・[operation-id-optional.md](operation-id-optional.md)・
  [operation-id-required-guard.md](operation-id-required-guard.md)・[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)・
  [sql-multi-row-insert.md](sql-multi-row-insert.md)・[explicit-transaction.md](explicit-transaction.md)・
  [partitioned-dml.md](partitioned-dml.md)・[alter-table-rename-table.md](alter-table-rename-table.md)・
  `crates/wire-server/docs/nosql-api.md`

## 目的と読み方

fandhe の冪等再送（SQL の `USING OPERATION_ID`、NoSQL 本文の `operation_id`）を、Stripe（API の冪等キー）と
PostgreSQL・MySQL・SQLite（DB コアの代替手段）と同じ軸で並べる。「同等以上」と書くのは、
fandhe 側にテストで固定した根拠がある項目に限る。差異・未提供の点は隠さず書く。

## 前提

- 比較対象の種類が違う。Stripe は HTTP API の要求単位の冪等キー。PostgreSQL・MySQL・SQLite のコアには
  冪等キー専用の機能が無く、一意制約を使う UPSERT／無視の構文か、利用者が作る台帳表で代替する。
  fandhe は DB コアの書き込み文単位の台帳である
- fandhe の既定は任意（`LedgerMode::Optional`）。ID を省略した書き込みは台帳に記録せず、再送も検出しない。
  必須にするには起動フラグ `--require-operation-id` を使う。分割実行 DML は構成によらず必須

## 比較表

| 比較軸 | Stripe | PostgreSQL | MySQL | SQLite | fandhe |
| ------ | ------ | ---------- | ----- | ------ | ------ |
| キーの指定方法 | `Idempotency-Key` ヘッダ（POST） | 専用機能なし。一意列 + `ON CONFLICT` か自前の台帳表 | 専用機能なし。一意索引 + `ON DUPLICATE KEY UPDATE`／`INSERT IGNORE` か自前の台帳表 | 専用機能なし。一意制約 + UPSERT か自前の台帳表 | SQL は `USING OPERATION_ID '<id>'`、NoSQL は本文の `operation_id`。HTTP ヘッダ（HTTP-14）は実装中（#2063）で提供済みではない |
| キー長・文字種 | 255 文字まで | 該当なし | 該当なし | 該当なし | 256 バイトまで（単位はバイト）。制御文字は `22000` で拒否 |
| 名前空間 | 公式ドキュメントで確認できず | 該当なし | 該当なし | 該当なし | `(tenant_id, table_name, operation_id)`。テナント ID はサーバーが導出。テーブル単位でもあり、別テーブルへ同じ ID を使っても再送と見なさない |
| 内容不一致の検出 | 検出する（同じキーで引数が異なるとエラー） | 一意制約の衝突だけを見る。内容の違いは検出しない | 同左（一意索引・主キーの重複だけ） | 同左（一意制約の衝突だけ） | 要求由来の内容ハッシュで照合。一致は `23505`、不一致は `22023` |
| 書き込みとの原子性 | 公式ドキュメントで確認できず | 台帳表を自前で作る場合は同一 Tx にできる（利用者の実装次第） | 同左 | 同左 | 台帳追記を行の書き込みと同じ `redb::WriteTransaction` で行う。keep-first。並行再送は成功 1 件のみ |
| 明示 Tx・失敗時 | 検証失敗や同時実行の衝突は結果を保存せず再試行できる | 該当なし | 該当なし | 該当なし | COMMIT と同時に永続化。ROLLBACK・事前拒否後は記録が残らず ID を再利用できる。同一 Tx 内の再利用は `25000` |
| 対象操作 | POST 要求（GET・DELETE は対象外） | UPSERT は INSERT のみ。UPDATE／DELETE の冪等化は自前の台帳表が要る | 同左（INSERT 系のみ） | 同左（INSERT 系のみ） | INSERT（単一・複数行・ファイル形）・UPSERT・UPDATE／DELETE・TRUNCATE・分割実行 DML。0 行の DELETE も記録 |
| 保持期間（TTL） | v1 は 24 時間以上経過したキーを自動削除しうる。削除後の再利用は新規要求になる | 該当なし | 該当なし | 該当なし | 自動掃除なし。`DROP TABLE` でのみ消え、`RENAME TO` では新しい名前へ移す |
| 結果の再生 | v1 は最初の応答（ステータスとボディ。500 を含む）を保存して返す | 該当なし | 該当なし | 該当なし | 再生しない。再送には `23505`（NoSQL は 409・`DUPLICATE_OPERATION_ID`）を返し、commit 済みの確定根拠とする。例外として分割実行 DML は保存したカーソルから再開する |
| commit 済みかの照会 | 公式ドキュメントで確認できず | 該当なし | 該当なし | 該当なし | engine API の `EngineCore::operation_recorded`・`last_operation_id`（RECOVER-7）のみ。wire・HTTP からの照会は未提供 |
| 必須・任意 | 任意（ヘッダを付けた要求のみ） | 該当なし | 該当なし | 該当なし | 既定は任意。`--require-operation-id` で必須 |

## TTL を設けない理由と台帳の増え方

- 理由: エントリの存在を commit 済みの確定根拠にし、keep-first を恒久の契約にしている。期限で消すと、
  遅れて届いた再送が新しい書き込みとして二重に適用されうる。そのため ID を再利用できない状態を保つ
- 増え方: 書き込み 1 文につき 1 エントリ。複数行 INSERT も 1 文 = 1 エントリ
  （[sql-multi-row-insert.md](sql-multi-row-insert.md)・[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)）。
  0 行の DELETE と TRUNCATE も 1 エントリ。ID を省略した書き込みは記録しない
- 大きさ: 値は v2 で 33 バイト、v3 で 41 バイト。キーはテナント ID・表名・ID（最大 256 バイト）。
  全体の大きさは記録した書き込み文の数に比例する
- 代償: 掃除・圧縮の仕組みは無く、ディスク使用量は単調に増える。運用上の差として明記する。
  掃除を足すかは本書の範囲外で、利用者の判断事項とする

## 書き分け

- 同等以上（根拠テストあり）:
  - 内容不一致の検出（`recovery_content_hash.rs` の `insert_resend_same_content_is_23505_and_different_content_is_22023`・`update_resend_with_different_content_is_22023`）。DB コアの代替手段は一意キーの衝突しか見ない
  - 書き込みとの原子性と keep-first（`recovery_ledger.rs` の `t2_ledger_write_is_atomic_with_row_write_failure`・`t8_keep_first_entry_remains_recorded_after_second_use`）
  - 並行再送で成功 1 件のみ（`table_rls/recovery_content_hash.rs`、`concurrent_same_operation_id_same_content_inserts_yield_exactly_one_success`）
  - テナントの分離（`t4_ledger_scope_is_per_tenant`。ID はサーバー導出）
  - UPDATE／DELETE／TRUNCATE まで対象にしていること（DB コアの UPSERT 系は INSERT のみ）
  - 失敗した Tx が記録を残さないこと（`core_misc/recover12_txn_failure_no_ledger.rs`・`recover12_explicit_txn_resend.rs`）
- 同等: キー長の上限の水準（255 文字と 256 バイト。単位が違う）
- 差異・未提供:
  - 結果を再生せずエラーを返す（Stripe v1 は保存した応答を返す）
  - テーブル単位の名前空間（`t3_ledger_scope_is_per_table`）
  - TTL と掃除が無い
  - wire・HTTP からの照会 API が無い
  - HTTP ヘッダでの指定は実装中（#2063）
  - 既定は任意

## 参照

確認日は 2026-11-05。

- Stripe API Reference「Idempotent requests」（<https://docs.stripe.com/api/idempotent_requests>）
- PostgreSQL 17「INSERT」（<https://www.postgresql.org/docs/17/sql-insert.html>）
- MySQL 8.4「INSERT ... ON DUPLICATE KEY UPDATE」（<https://dev.mysql.com/doc/refman/8.4/en/insert-on-duplicate.html>）
- SQLite「UPSERT」（<https://www.sqlite.org/lang_upsert.html>）
- fandhe 側は上記のファイル名・関数名・テスト名で参照する（行番号は書かない）
