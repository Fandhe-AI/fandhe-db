# ADR: SEQUENCE の名前空間・採番永続化・台帳保存方式（Issue #1687）

- ステータス: Accepted（設計記録。後続 #1688〜#1692 で実装する）
- 対応 Issue: #1687（親 #1686。後続 #1688・#1689・#1690・#1691・#1692）
- 関連ポインタ: `docs/spec/04-behavior/data-model.md`（TABLE-12・TABLE-23・TABLE-24・
  TABLE-27・TABLE-36・TABLE-37）・`docs/spec/04-behavior/recovery.md`（RECOVER-1・
  RECOVER-10・RECOVER-11・RECOVER-12・RECOVER-14・RECOVER-15）・SQL-21・SQL-48・
  NOSQL-22・TASK-242。spec 本文は転記しない。契約は spec を正とし、本書では再掲しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 関連コード: `crates/engine/src/catalog.rs`（`CATALOG_TABLE`・`VIEWS_TABLE`・
  `INDEX_CATALOG_TABLE`・`ENUM_TYPES_TABLE`・`ColumnDefault`）・`catalog/dependency.rs`・
  `recovery/ledger.rs`・`recovery/content_hash.rs`・`recovery/partitioned_job.rs`・
  `sql/transaction.rs`・`storage.rs`・`sql/udf_call.rs`・`tenant.rs`・
  `crates/wire-server/src/ddl_permission_opt.rs`
- 検証コード: なし（本 Issue は設計記録のみ。コードは変更しない）

## 1. 背景・目的

SEQUENCE と `nextval`／`currval`／`setval`／`lastval`（親 #1686）の実装前に、次の 4 点を
実装設計として固め、後続 sub-issue の判断の揺れをなくす。

1. 名前空間をカタログのキーへどう写すか
2. `nextval` の永続化と、ROLLBACK で戻らないことの両立
3. 台帳への保存形式（`content_hash` の対象と再送時の応答）
4. 採番値のキャッシュとクラッシュ復旧の関係

なお、親 #1686 と本 Issue 起票時の文言には「採番値を台帳に保存し再送で同じ値を返す」と読める
記述がある。一方、兄弟 #1692 とその spec ポインタ（TABLE-24 (3)・RECOVER-10・RECOVER-11）は
「再送は台帳で `23505` になり再実行されない」立場をとる。本書は #1692 側に統一する（D1）。

## 2. 公開コードの現状（設計の前提）

| 事実 | 所在 |
| ---- | ---- |
| カタログは名前をキーにした平らな redb テーブルで、テナントもスキーマも持たない。表・ビュー・索引は relation の名前空間を共有し、作成時は複数テーブルを同じ write txn で確認する（衝突は `42P07`）。ENUM 型は別の名前空間 | `catalog.rs` |
| 台帳のキーは `(tenant_id, table_name, operation_id)`。照合はハッシュのみで、一致は `23505`、不一致は `22023`（keep-first）。行の書き込みと同じ txn で永続化する（RECOVER-12） | `ledger.rs` |
| 揮発性の式・DEFAULT を含む INSERT 系は、構文形をハッシュする別ドメインの `OpTag`（14〜16）と、列の種別バイト `2`（揮発性 DEFAULT の適用）を実装済み | `content_hash.rs`・[now-evaluated-value-ledger.md](now-evaluated-value-ledger.md) |
| 明示トランザクションはセッションごとに共有 `redb::WriteTransaction` を 1 本持ち、ROLLBACK／接続断で drop する。redb は単一ライター | `sql/transaction.rs`・`storage.rs` |
| DDL は `--ddl-allowed-users` で許可したユーザーだけが実行でき、既定では全 DDL が `42501` | `ddl_permission_opt.rs` |
| SCHEMA（#1711）・`operation_id` の任意化（#1701）は未着手 | Issue |

## 3. 決定

### D1. 台帳には採番値を保存しない

- 台帳値のレイアウトは変えない。新しい台帳版も新しい `OpTag` も追加しない
- 再送時の応答と状態遷移の契約は再掲しない。TABLE-24 (3) と `docs/spec/04-behavior/` の該当ビヘイビアに従う（ポインタのみ）
- 経緯: 親・本 Issue の「台帳に保存」という文言は #1692 の立場へ統一した

### D2. `content_hash` の対象は採番前の構文形

- `nextval`・`currval`・`setval`・`lastval` は既存の `Expr::Call`（式タグ 3、名前は小文字化）のままハッシュする
- 時刻関数の先例（[now-evaluated-value-ledger.md](now-evaluated-value-ledger.md)）に倣い、
  `INSERT VALUES` 内の `nextval` は定数へ畳み込まない。`nextval` を含む INSERT・バッチ INSERT・
  UPSERT は既存の `InsertExpr`／`InsertBatchExpr`／`UpsertExpr` に乗せる
- `DEFAULT nextval('s')`（将来の SERIAL・IDENTITY を含む）は、`ColumnDefault` に新 variant と
  新しいカタログタグを足し、ハッシュには列の種別バイト `2` を使う。未知タグは既存どおり fail-closed
- `nextval` を含まない文のハッシュは既存とバイト単位で同一に保つ。golden テストの不変を
  #1690・#1692 の必須条件とする
- 述語形 UPDATE の `SET c = nextval(...)` は既存の構文形ハッシュにそのまま乗る

### D3. 名前空間とカタログのキー設計

- **定義テーブル** `SEQUENCES_TABLE`（redb の別テーブル）
  - キーはスキーマ修飾した名前。#1711 の前は `public` のみで、`public.s` と非修飾名を同じキーへ正規化する
  - 値は版バイト付きの定義 blob。安定した内部 ID（作成時に採番する `u64`）・型・increment・
    min・max・start・cache・cycle・owned_by を持つ。キーの形は #1711 の方式に合わせる
- **relation 名前空間への参加**: SEQUENCE も表・ビュー・索引と同じ名前空間に属する。
  CREATE TABLE／VIEW／INDEX／SEQUENCE はいずれも関連テーブルを同じ write txn で確認し、衝突は `42P07`。
  既存の作成経路へ照合先を足す作業は #1688 の範囲とする
- **状態テーブル** `SEQUENCE_STATE_TABLE`
  - キーは `(tenant_id, sequence_id)`、値は版バイト付きの状態（永続化済み高水位と is_called）
  - `tenant_id` を先頭に置き、テナント単位の range 走査で完結させる。テナント別採番の契約は
    再掲せず、TABLE-24 (2) のポインタに従う
- **名前ではなく内部 ID をキーにする理由**: RENAME（#1689）で状態を書き換えずに済み、
  同名で作り直しても古い状態を拾わない
- **DROP 時の状態削除**: 状態キーは `(tenant_id, sequence_id)` の順なので、`sequence_id` の prefix range では
  全テナント分を消せない。DROP は定義の削除と同じ write txn で状態テーブルを全走査し、キー後半の
  `sequence_id` が一致する行を削除する（採用。DROP は低頻度の DDL であり、走査は DDL 許可ユーザーの
  操作に限られる）。メモリ上のアロケータの該当エントリも commit 後に破棄する。テナント列挙による個別削除は
  テナント一覧の整合に依存するため、逆引き索引は書き込み経路を増やすため、それぞれ採らない。
  走査コストが問題になった場合の逆引き索引への切替は #1688 の実測で判断する
- **`public`**: 共有範囲は TABLE-27 (2) のポインタに従う。他テナントの対象や存在しない名前は
  同じ `42P01` 経路で応答し、存在情報を分岐させない
- 件数の上限は `MAX_LIST_TABLES` を流用し、名前には `validate_identifier` をかける

### D4. 永続化・ROLLBACK・クラッシュ復旧

- **論点**: 共有 write txn の中で状態を永続化すると ROLLBACK で状態も戻り、「`nextval` は戻らない」
  流儀（TABLE-24 (1)）と合わない。redb は単一ライターで、txn の中から別 txn を開くと自身とデッドロックする
- **採用案: プロセス内アロケータ＋永続化済み高水位**
  - `(tenant_id, sequence_id)` ごとに Mutex で守った単調カウンタを持ち、払い出しは常にメモリから行う。
    ROLLBACK しても戻さない（欠番になる）
  - 永続化済み高水位を超える払い出しが必要になったら、現在の write txn（autocommit の txn か明示
    トランザクションの共有 txn）の中で、高水位を `CACHE` の幅ぶん先へ進めて書く
  - commit した行に含まれる値は必ず同じ txn で高水位に覆われるため、クラッシュ後に永続化済み高水位から
    再開しても commit 済みの値と重複しない。失われた払い出し前の範囲は欠番として許す（TABLE-24 (4)）
- **補強**: ROLLBACK や失敗で共有 txn を drop した後、ロック解放後に高水位だけを書く短い write txn を発行し、
  rollback した txn で払い出した値がクラッシュ後に再払い出しされる窓を狭める
- **既知の制約**: ROLLBACK と補強 txn の間でクラッシュした場合など、rollback 済みの値が再払い出しされる
  窓は残りうる。許容範囲は PostgreSQL の公開ドキュメントの挙動と照合して #1692 で確定する（要追確認）
- **`SELECT nextval(...)`** も write txn を必要とする文として扱い、autocommit で高水位を永続化する
- **`CACHE` の意味**: セッションごとの事前確保（PostgreSQL と同じ。セッション終了で残りは欠番）を推奨する。
  テナント×SEQUENCE 共有アロケータの永続化刻み幅とする案もある。二方式では、複数セッションが交互に
  採番したときにセッションから見える値の列が変わる（例: CACHE 10 で A・B・A の順に呼ぶと、セッション別確保では
  A は 1、2、共有カウンタでは A は 1、3）。採否は、この観測可能な差も含めて #1690 で確定する
- **却下案**: (b) txn 内で永続化し ROLLBACK で戻す（PostgreSQL との差が出る）・
  (c) 別の redb DB へ即時永続化する（カタログとの整合と孤児回収が複雑になり構成が二重化する）
- **分割実行 DML**（`partitioned_job.rs`）: 部分ごとに commit した行の値は各部分の txn で高水位に覆われる。
  再開時は永続化済み高水位から続け、ジョブ記録の版は変えない

### D5. `operation_id` を省略した書き込み（#1701 着地後）

- 台帳エントリを作らないため、採番値は台帳に残らない。再送すると新しい値を採番する
  （RECOVER-1・TABLE-24 (3)）。#1701 着地までは `operation_id` 必須のままなので、着地時に適用する

### D6. セッション状態

- `currval`／`lastval` はセッション状態へ `(sequence_id → 直近値)` として持つ。未呼び出し時のエラー契約は
  再掲せず、TABLE-24 のポインタに従う
- 名前から ID を解決して照合し、DROP 後に古い ID を参照しない。他テナントの ID は解決段階で見えない

### D7. 数値契約

- 内部値は `i64`、計算は `checked_*` とする。上限・下限到達時、`setval`・ALTER の範囲外の数値・エラー契約は
  再掲せず、TABLE-24 のポインタと #1689・#1691 の受け入れ条件に従う
- `AS smallint/integer` の範囲は定義時に検証する。`CACHE` の幅には上限を設ける

### D8. 受理範囲と fail-closed

- 最初に受理するのは #1688 の START／INCREMENT／MINVALUE／MAXVALUE／CYCLE
- CACHE・OWNED BY・AS・SERIAL・IDENTITY・`TRUNCATE ... RESTART IDENTITY`（SQL-48）・
  `DROP SEQUENCE ... CASCADE`（TABLE-37）・`IF [NOT] EXISTS`（TABLE-36）・TEMP SEQUENCE は、
  受理する sub-issue が明記されるまで `0A000`／`42601` で fail-closed とする
- DDL は既存の `--ddl-allowed-users` のゲートに従い、既定拒否を維持する

### D9. 変更ログとレプリカ

- RECOVER-14・RECOVER-15 は未実装。「採番済みの値が記録対象になる」というポインタのみ残し、設計は対象外とする

## 4. 後続 sub-issue の前提

| Issue | 前提とする決定 |
| ----- | -------------- |
| #1688 | D3（定義／状態の 2 テーブル分離・内部 ID・relation 名前空間への参加・`42P01` 一本化）・D8 |
| #1689 | D3（RENAME は定義テーブルのみ書き換え）・D7（`22023`）。RESTART は状態と高水位を書き換えるが台帳は変えない（D1） |
| #1690 | D2（構文形ハッシュと `ColumnDefault` 新 variant）・D4（アロケータと高水位）・D6・D7 |
| #1691 | D4（`setval` は高水位も更新する）・D6（`lastval`）・D7（数値契約） |
| #1692 | D1・D2（golden 不変）・D4（クラッシュ注入での二重採番ゼロ。既存 crash-test の流儀）・D5 |

外部依存: #1711（D3 のキーの形）・#1701（D5 の適用時期）。

## 5. 対象外

- SCHEMA の実装（#1711）
- 一時表と TEMP SEQUENCE
- SERIAL／IDENTITY（TABLE-23 (8)・TABLE-24 の後続）
- 変更ログとレプリカ（RECOVER-14／15）
- NoSQL の returning（NOSQL-22）

## 6. セキュリティ上の不変条件

- 状態キー先頭の `tenant_id` と `42P01` 一本化により、他テナントの存在・採番量を漏らさない
- 未知の格納版・カタログタグは拒否する（fail-closed）。依存は追加しない
- SEQUENCE 名は `validate_identifier` で検証した識別子のみを使い、SQL／プラン文字列へ連結しない
