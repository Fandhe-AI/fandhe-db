# TRUNCATE の TABLE 省略・複数テーブル・ONLY（Issue #1643）

- 対象ビヘイビア: SQL-48 (1)(4)（ポインタ: `docs/spec/04-behavior/sql-surface.md`、TASK-286）
- 関連ビヘイビア: SQL-22（TASK-193）・TABLE-17・RECOVER-1/2/10/11・SQL-31
- 関連コード: `sql::allowlist`（`parse_truncate`・`ValidatedTruncate`）・`tenant::truncate_tables_unchecked`・
  `recovery::content_hash::for_truncate_tables`
- 範囲外: `RESTART IDENTITY`（#1645。従来どおり `42601`）。`CASCADE`／`RESTRICT` は Issue #1644 で追加（末尾の追記節）

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

## Issue #1644 追記: CASCADE／RESTRICT

- 対象ビヘイビア: SQL-48 (2)（ポインタ: `docs/spec/04-behavior/sql-surface.md`）。関連: TABLE-17・RECOVER-10・SQL-31
- 関連コード: `catalog::dependency::truncate_closure_in_txn`（FK 参照元の閉包）・
  `tenant::truncate_tables_unchecked`・`recovery::content_hash::for_truncate_tables_cascade`・
  `sql::exec::TruncateOutcome::cascaded`

### 構文

`TRUNCATE [TABLE] [ONLY] <table> [*] [, ...] [CASCADE | RESTRICT] USING OPERATION_ID '<id>' [;]`

- `CASCADE`／`RESTRICT` は表リストの直後に 1 つだけ書ける。省略時は `RESTRICT`
  （`ValidatedTruncate::behavior`。DROP と同じ `DropBehavior`）。表名は先に読み切るため、`cascade`
  という名前の表は従来どおり表名として扱われる。`RESTART IDENTITY`（#1645）は表リストと behavior の間に
  差し込めばよい
- 2 つ並べる・表リストの途中に置く・`USING OPERATION_ID` の後ろに置く形は `42601`

### 判定はパス 0・カタログ宣言だけ（破壊的変更）

行を消す前に、同じ write txn で対象集合を確定する（`truncate_closure_in_txn`）。

- RESTRICT: 指定した表を、**指定していない表**の FK が参照していれば、参照元の行の有無・他テナントの行に
  関係なく `0A000` で拒否する。従来の「行を消した後に FK を検査し、自テナントの子の行が残れば `23503`」
  を置き換える。判定が自テナントの行に依存しなくなるため、他テナントの行に応答が左右されない。
  拒否は行を消す前なので、台帳も残らない。文言はカタログ上の表名だけを含む
- 自己参照 FK は依存に数えない。同じ文で指定した表どうしの参照は RESTRICT でも通る
- 遅延 FK（`DEFERRABLE`）でも判定は文の実行時点で、COMMIT まで遅らせない（旧 R1 の「TRUNCATE だけで終えた
  場合に COMMIT が違反になる」経路は、RESTRICT の拒否に置き換わった）
- CASCADE: 参照元の表を推移的に対象へ加え、同じ txn で自テナントの行を削除する。参照アクション
  （`ON DELETE CASCADE`／`SET NULL` 等）は発火しない。走査は再帰せずキューで反復し、対象数は
  `MAX_LIST_TABLES` で頭打ちにする（超過は `Err` で全体 abort）。順序は決定的
  （明示した表の文中順 → 連鎖した表の発見順・同じ親の子は名前順）
- FK の列挙は `referencing_foreign_keys_in_txn`（FK を持ちうる全版の接頭辞を見る既存関数）を再利用する
- 従来のパス 2 の事後 FK 検査は多層防御として残す（新しい判定の後では常に通る想定）

### 台帳・結果・トランザクション

- 内容ハッシュ: RESTRICT（省略・明示とも）は従来のハッシュ（1 表は `for_truncate()`、2 表以上は
  `for_truncate_tables`。バイト列は変えない）。CASCADE は `OpTag::TruncateCascade = 18` で**利用者が書いた表**の
  正規化集合をハッシュする。連鎖した表はカタログの状態で変わるため含めない
- 台帳キーは最終集合（明示した表 + 連鎖した表）の全表に同じ `operation_id` と同じハッシュで記録する
- `TruncateOutcome::cascaded()` が連鎖した表名を返す（RESTRICT では空）。NOTICE の送出は wire 側の後続作業。
  `Copy` は外した
- 明示トランザクションでは `core.rs` が明示した表と連鎖した表の両方に `mark_written` を呼ぶ
- 権限検査（#1968）を結線する際は、連鎖後の最終集合に対して行う必要がある
