# TRUNCATE の TABLE 省略・複数テーブル・ONLY（Issue #1643）

- 対象ビヘイビア: SQL-48 (1)(4)（ポインタ: `docs/spec/04-behavior/sql-surface.md`、TASK-286）
- 関連ビヘイビア: SQL-22（TASK-193）・TABLE-17・RECOVER-1/2/10/11・SQL-31
- 関連コード: `sql::allowlist`（`parse_truncate`・`ValidatedTruncate`）・`tenant::truncate_tables_unchecked`・
  `recovery::content_hash::for_truncate_tables`
- 範囲外: `RESTART IDENTITY`（Issue #1645 で追加。末尾の追記節）。`CASCADE`／`RESTRICT` は Issue #1644 で追加（末尾の追記節）

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

## Issue #1645 追記: RESTART IDENTITY／CONTINUE IDENTITY

- 対象ビヘイビア: SQL-48 (3)・TABLE-24（ポインタ: `docs/spec/04-behavior/sql-surface.md`・`data-model.md`）。
  関連: RECOVER-10・RECOVER-11・SQL-31

### 構文

`TRUNCATE [TABLE] [ONLY] <表> [, ...] [RESTART IDENTITY | CONTINUE IDENTITY] [CASCADE | RESTRICT] USING OPERATION_ID '<id>'`

- 識別句は表リストと `CASCADE`／`RESTRICT` の間に置く。`RESTART`／`CONTINUE` の直後が `IDENTITY` のときだけ
  句として読む 2 トークン先読みで、`restart` という名前の表は従来どおり表名として扱われる。
- 省略と明示の `CONTINUE IDENTITY` は同じ意味（採番を戻さない）で、内容ハッシュも同一。
- 重複・順序違い・`IDENTITY` 欠落・`USING OPERATION_ID` の後ろは `42601`。
- 公開 API は `ValidatedTruncate::restart_identity` の追加（破壊的変更）。

### 「所有」の定義

- 戻す対象は、対象表（CASCADE では連鎖後の最終集合）が**所有する** SEQUENCE の自テナントの採番だけ。
  `DEFAULT nextval('s')` から参照されるだけの SEQUENCE は戻さない（戻すと同じ SEQUENCE を使う別の表で
  値が重複する。PostgreSQL と同じ）。
- 所有関係は SEQUENCE の定義 blob に格納する。所有者ありだけ版 3（版 2 の固定部 + 表名・列名を u16 LE 長さ付きで
  連結）で書き、所有者なしは従来の版 2 のバイト列のまま。旧バイナリは版 3 を `CorruptSchema` で拒否する。
  `ALTER SEQUENCE` は所有者を引き継ぐ。
- 所有者を設定する入口は `set_sequence_owner_in_txn`（`pub(crate)`）。SERIAL の結線と DROP TABLE／DROP COLUMN／
  RENAME での追従は Issue #1694 で着地した（[sequence-namespace-and-ledger.md](sequence-namespace-and-ledger.md) §12）。
  `OWNED BY` 構文・IDENTITY の結線は未着地のため、SERIAL 以外の表では `RESTART IDENTITY` は受理されるが採番には
  作用しない（所有 SEQUENCE が無い表では PostgreSQL も no-op）。

### 実行

- パス 0（行を消す前）で所有 SEQUENCE を列挙する。カタログ宣言だけで決まり、テナントの採番状態には依存しない。
- 戻す値は `params.start`・`is_called = false`。状態行を削除せず明示的に書く（削除すると次の払い出しが
  `ALTER SEQUENCE ... RESTART WITH` でずれうる `restart_value` にフォールバックするため。PostgreSQL の
  `ResetSequence` も START に戻す）。書き込みは FK 検査・世代 bump・台帳を通過した後で、`(自テナント, seq_id)` の
  行だけを対象にする（他テナントの行には触れない）。
- Autocommit は commit 成功後に writer permit を保持したまま `(tenant, seq_id)` のメモリ上の現在値だけを捨てる
  （`WriteTarget::with_txn_post_commit`・`SequenceAllocator::evict_tenant`）。commit 前に巻き戻すと commit 失敗で
  メモリが永続値より後ろへ下がり、permit 解放後に巻き戻すと他のライタと競合する。いずれも重複採番になる。
- 一時表の DB には SEQUENCE の定義表が無く、列挙結果が空になるため何もしない。

### 明示・暗黙トランザクション内（fail-closed）

- 対象の閉包に所有 SEQUENCE が 1 つでもあれば、行を消す前に `0A000` で拒否する（`ALTER SEQUENCE` と同じ方針）。
  所有 SEQUENCE が無ければ従来どおり成功する。判定は全テナント共有のカタログだけで行い、存在オラクルにならない。
- 理由: アロケータのメモリ上の現在値は ROLLBACK で戻らない設計のため、トランザクション内で正しく戻すには
  `ActiveTxn` への保留リセットの保持、同じ txn 内の `nextval` が状態行から種を作る経路、保留キーでの
  `persist_current_in_txn` の抑止、`commit_active` の `commit_and_finish` 化が要る。所有者を SQL から作れるのが
  #1694 以降のため、完全対応はフォローアップとする。

### 台帳

- `RESTART IDENTITY` は `OpTag::TruncateRestartIdentity = 20`（behavior バイト + 利用者が書いた表の正規化集合）。
  CONTINUE（省略・明示）は既存の 3 領域のままでバイト列を変えない。取り違えた再送は `22023`、同じ文の再送は
  `23505` で、`23505` のときは採番を戻さない。
