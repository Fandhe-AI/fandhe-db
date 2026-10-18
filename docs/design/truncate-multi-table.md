# TRUNCATE の TABLE 省略・複数テーブル・ONLY（Issue #1643）

- 対象ビヘイビア: SQL-48 (1)(4)（ポインタ: `docs/spec/04-behavior/sql-surface.md`、TASK-286）
- 関連ビヘイビア: SQL-22（TASK-193）・TABLE-17・RECOVER-1/2/10/11・SQL-31
- 関連コード: `sql::allowlist`（`parse_truncate`・`ValidatedTruncate`）・`tenant::truncate_tables_unchecked`・
  `recovery::content_hash::for_truncate_tables`
- 範囲外: `CASCADE`／`RESTRICT`（#1644）・`RESTART IDENTITY`（#1645）。いずれも従来どおり `42601`

## 受理する形

`TRUNCATE [TABLE] [ONLY] <table> [*] [, [ONLY] <table> [*] ...] USING OPERATION_ID '<id>' [;]`

- 継承が無いため `ONLY`／`*` は意味を変えない（no-op）
- 重複した表名は初出順を保って 1 表に畳む（`TRUNCATE a, a` は成功）
- `TABLE`／`ONLY` は予約語ではない。直後が `,`・`*`・`USING OPERATION_ID` の導入部なら同名の表名と
  みなし、従来の解釈を変えない。文末などではキーワードと取り、後続の識別子検査が `42601` にする
- 存在確認は文中の順。最初に見つかった不在の表を `42P01` で返す（ビューは既存の再分類で `42809`）

## 実行: 単一 write トランザクション・2 パス

表ごとに commit するループは Autocommit で N 回 commit され原子性を失うため採らない。
1 つの `with_txn` の中で次の順に処理し、途中のエラーは drop（abort）で全表の副作用を捨てる。

1. パス 0: 全表のスキーマを確定する
2. パス 1（表ごと）: 台帳記録 → 自テナント行の削除 → 一意索引・キー索引のテナント範囲クリア
3. パス 2（表ごと）: 参照先側の FK 検査（`Truncated`）→ 世代 bump

行とキー索引を全表で先に消してから FK を検査するため、親子を同じ文で指定すると子に行があっても
`23503` にならない。親だけを指定して子に自テナントの行が残る場合は従来どおり `23503`。

明示トランザクションでは `core.rs` が文の対象の全表に `mark_written` を呼ぶ
（漏れると遅延 FK の COMMIT 時検査が取りこぼす）。

## 台帳（RECOVER-10）

- 台帳キーは従来どおり表ごとの `(tenant, table, operation_id)`。全表に同じ `operation_id` を記録する
- 内容ハッシュ: 重複除去後に 1 表なら従来の `for_truncate()`（バイト列は変えない。固定値テストで保護。
  変わると既存エントリの再送が `22023` になる）。2 表以上なら `OpTag::TruncateMulti = 17` で正規化した
  表集合（整列・長さプレフィクス付き）をハッシュする
- 結果: 同じ表集合（順序違いを含む）の再送は `23505`、集合が違えば `22023`。いずれも全体が abort される
