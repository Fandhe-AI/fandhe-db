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

なお、親 #1686 と本 Issue 起票時の台帳保存の文言は、兄弟 #1692 とその spec ポインタ
（TABLE-24 (3)・RECOVER-10・RECOVER-11）の立場へ統一する（D1）。

## 2. 公開コードの現状（設計の前提）

| 事実 | 所在 |
| ---- | ---- |
| カタログは名前をキーにした平らな redb テーブルで、テナントもスキーマも持たない。表・ビュー・索引は relation の名前空間を共有し、作成時は複数テーブルを同じ write txn で確認する（衝突は `42P07`）。ENUM 型は別の名前空間 | `catalog.rs` |
| 台帳のキーは `(tenant_id, table_name, operation_id)`。照合はハッシュのみで、一致は `23505`、不一致は `22023`（keep-first）。行の書き込みと同じ txn で永続化する（RECOVER-12） | `ledger.rs` |
| 揮発性の式を含む INSERT 系は、構文形をハッシュする別ドメインの `OpTag`（14〜16）を実装済み。省略列へ適用した DEFAULT は内容照合ハッシュの材料から除外され、旧・列の種別バイト `2`（揮発性 DEFAULT の適用）は退役済みで再利用しない | `content_hash.rs`・[now-evaluated-value-ledger.md](now-evaluated-value-ledger.md) |
| 明示トランザクションはセッションごとに共有 `redb::WriteTransaction` を 1 本持ち、ROLLBACK／接続断で drop する。redb は単一ライター | `sql/transaction.rs`・`storage.rs` |
| DDL は `--ddl-allowed-users` で許可したユーザーだけが実行でき、既定では全 DDL が `42501` | `ddl_permission_opt.rs` |
| SCHEMA（#1711）は未着手。`operation_id` の任意化（#1701）は #1704 で engine 既定を任意モード（`LedgerMode::Optional`）へ移し済み（`--require-operation-id` で従来の必須へ戻せる） | Issue・[operation-id-optional.md](operation-id-optional.md) |

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
  新しいカタログタグを足す。ただし省略列へ適用した DEFAULT は既存方針
  （[not-null-default.md](not-null-default.md) の「Issue #1629 追記」）どおり内容照合ハッシュの
  材料から除外し、送られた文に書かれた列だけでハッシュする。退役済みの列の種別バイト `2` は
  再利用しない。DEFAULT 変更後に同じ `operation_id`・同じ INSERT を再送しても、内容不一致にならず
  `23505`（再実行しない）になる。未知のカタログタグは既存どおり fail-closed
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
  - `tenant_id` を先頭に置き、テナント単位の range 走査で完結させる（契約は TABLE-24 (2) のポインタ）
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

- **論点**: 共有 write txn の中で状態を永続化すると ROLLBACK で状態も戻る（契約は TABLE-24 (1) のポインタ）。
  redb は単一ライターで、txn の中から別 txn を開くと自身とデッドロックする
- **採用案: プロセス内アロケータ＋永続化済み高水位**
  - `(tenant_id, sequence_id)` ごとに Mutex で守った単調カウンタを持ち、払い出しは常にメモリから行う
  - 永続化済み高水位を超える払い出しが必要になったら、現在の write txn（autocommit の txn か明示
    トランザクションの共有 txn）の中で、高水位を `CACHE` の幅ぶん先へ進めて書く
  - commit した行に含まれる値は同じ txn で高水位に覆われる（クラッシュ復旧の契約は TABLE-24 (4)・
    RECOVER 系ポインタ）
- **保証範囲**: クラッシュ後の採番値に関する契約は TABLE-24 (4) と RECOVER 系ポインタを参照する
  （本書では再掲しない）。後続実装は spec の契約のみを前提とする
- **補強 txn の要否**: ROLLBACK 後に高水位だけを書く短い write txn を補強として発行するかは、
  #1692 で「採用しない」と確定した（[§11.2](#112-補強-txn-の不採用)。自動運転下の判断で、人間の確認が要る）。
  この部分は spec の契約とは分けて扱う
- **`SELECT nextval(...)`** も write txn を必要とする文として扱い、autocommit で高水位を永続化する
- **`CACHE` の意味**: セッションごとの事前確保（PostgreSQL と同じ。セッション終了で残りは欠番）を推奨する。
  テナント×SEQUENCE 共有アロケータの永続化刻み幅とする案もある。二方式では、複数セッションが交互に
  採番したときにセッションから見える値の列が変わる（例: CACHE 10 で A・B・A の順に呼ぶと、セッション別確保では
  A は 1、2、共有カウンタでは A は 1、3）。採否は、この観測可能な差も含めて #1690 で確定する
- **却下案**: (b) txn 内で永続化し ROLLBACK で戻す（PostgreSQL との差が出る）・
  (c) 別の redb DB へ即時永続化する（カタログとの整合と孤児回収が複雑になり構成が二重化する）
- **分割実行 DML**（`partitioned_job.rs`）: 部分ごとに commit した行の値は各部分の txn で高水位に覆われる。
  再開時は永続化済み高水位から続け、ジョブ記録の版は変えない

### D5. `operation_id` を省略した書き込み

- 省略時の再送動作の契約は RECOVER-1・TABLE-24 (3) を参照する（本書では再掲しない）。
  engine 既定は #1704 で任意モード（`LedgerWrite::Omitted`）になっており、SEQUENCE の採番もこの前提で扱う。
  `--require-operation-id` を指定した構成では従来どおり省略を拒否する

### D6. セッション状態

- `currval` 用に `(sequence_id → 直近値)` のマップを、`lastval` 用に「最後に `nextval` が成功した
  `sequence_id`」をセッション状態として持つ（`lastval` はこの ID でマップを引く）。未呼び出し時のエラー契約は
  再掲せず、TABLE-24 のポインタに従う
- `lastval` の対象 SEQUENCE が DROP された場合の動作は TABLE-24 のポインタに従う（再掲しない）。
  実装側は古い ID を参照せず、マップの該当エントリも参照時に破棄する（fail-closed）
- 名前から ID を解決して照合し、DROP 後に古い ID を参照しない。他テナントの ID は解決段階で見えない

### D7. 数値契約

- 内部値は `i64`、計算は `checked_*` とする。上限・下限到達時、`setval`・ALTER の範囲外の数値・エラー契約は
  再掲せず、TABLE-24 のポインタと #1689・#1691 の受け入れ条件に従う
- `AS smallint/integer` の範囲は定義時に検証する。`CACHE` の幅には上限を設ける

### D8. 受理範囲と fail-closed

- 最初に受理するのは #1688 の START／INCREMENT／MINVALUE／MAXVALUE／CYCLE
- CACHE・OWNED BY・AS・SERIAL・IDENTITY・
  `DROP SEQUENCE ... CASCADE`（TABLE-37）・`IF [NOT] EXISTS`（TABLE-36）・TEMP SEQUENCE は、
  受理する sub-issue が明記されるまで `0A000`／`42601` で fail-closed とする
- DDL は既存の `--ddl-allowed-users` のゲートに従い、既定拒否を維持する
- `TRUNCATE ... RESTART IDENTITY`（SQL-48 (3)）は Issue #1645 で受理した。所有者を定義 blob（版 3）に格納でき、
  戻す対象は所有 SEQUENCE の自テナントの採番だけ。`OWNED BY`／SERIAL の結線は #1694 で、それまで SQL から
  所有者は作れない。詳細は [truncate-multi-table.md](truncate-multi-table.md) の「Issue #1645 追記」

### D9. 変更ログとレプリカ

- RECOVER-14・RECOVER-15 は未実装。「採番済みの値が記録対象になる」というポインタのみ残し、設計は対象外とする

## 4. 後続 sub-issue の前提

| Issue | 前提とする決定 |
| ----- | -------------- |
| #1688 | D3（定義／状態の 2 テーブル分離・内部 ID・relation 名前空間への参加・`42P01` 一本化）・D8 |
| #1689 | D3（RENAME は定義テーブルのみ書き換え）・D7（`22023`）。RESTART は状態と高水位を書き換えるが台帳は変えない（D1） |
| #1690 | D2（構文形ハッシュと `ColumnDefault` 新 variant）・D4（アロケータと高水位）・D6・D7 |
| #1691 | D4（`setval` は高水位も更新する）・D6（`lastval`）・D7（数値契約） |
| #1692 | D1・D2（golden 不変）・D4（クラッシュ注入での検証。契約は TABLE-24 (4)。既存 crash-test の流儀）・D5 |

外部依存: #1711（D3 のキーの形）。D5 の前提となる #1704（`operation_id` の任意化）は着地済み。

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

## 7. Issue #1688 実装記録（カタログと CREATE／DROP SEQUENCE）

D3・D8 を実装した。コードは `catalog/sequence.rs`（Storage 水準）と `sql::allowlist`・`sql::ddl`（SQL 表層）にある。

### 7.1 格納

| テーブル | キー | 値 |
| -------- | ---- | -- |
| `sequences`（定義） | `(schema, name)`。#1711 より前の schema は `public` 固定。組み立ては `sequence_key` の 1 箇所 | 固定長 blob（版 1・内部 ID・型タグ・increment・min・max・start・cache〔1 固定〕・cycle・owned_by〔なし固定〕） |
| `sequence_state`（採番状態） | `(tenant_id, sequence_id)` | 本 Issue では書き込み経路なし。形式は #1690 が決める |
| `sequence_id_counter` | 固定キー 1 行 | 単調カウンタ。1 始まり・`checked_add`・再利用しない |

- decode は未知の版・型タグ・長さ不一致・cycle の不正値・範囲不変条件違反を `CorruptSchema` にする（fail-closed）
- 定義数の上限は `MAX_LIST_TABLES`（超過は `54000`）
- 公開 API（`core_api.snapshot`）を変えないため、エラーは既存の `CatalogError` variant を流用した。`Storage::create_sequence`／`drop_sequence` は `pub(crate)`

### 7.2 relation 名前空間

`index_name_exists_in_txn` を呼ぶ全箇所（表・ビュー・索引・matview の作成、`ALTER TABLE ... RENAME TO`、FK の参照先、`DROP TABLE`／`VIEW`／`INDEX`／`MATERIALIZED VIEW`、`CREATE INDEX` の対象）に SEQUENCE の照合を併記した。衝突は `42P07`、種別違いは `42809`。ENUM 型と一時表は別の名前空間のまま。

### 7.3 構文と意味検証

- 受理: `CREATE SEQUENCE <name>` に `START [WITH]`・`INCREMENT [BY]`・`MINVALUE`／`NO MINVALUE`・`MAXVALUE`／`NO MAXVALUE`・`CYCLE`／`NO CYCLE` を任意順。`DROP SEQUENCE <name> [RESTRICT]`
- `0A000`: `IF [NOT] EXISTS`・`AS`・`CACHE`・`OWNED BY`・`TEMP`／`UNLOGGED`・修飾名・`CASCADE`・複数名。修飾名は表も未対応のため、SEQUENCE だけ先に受理すると不整合になる。#1717 で入れる
- `42601`: 同種オプションの重複（`MINVALUE` と `NO MINVALUE` の組を含む）・小数・指数・未知のトークン。`22003`: i64 の範囲外
- `22023`: `INCREMENT 0`・`MINVALUE >= MAXVALUE`・`START` が範囲外。既定値は昇順が `1`〜`i64::MAX`・`START = MINVALUE`、降順が `i64::MIN`〜`-1`・`START = MAXVALUE`
- 権限ゲートは構文検証の後・カタログ照会の前に置く。権限の無い主体には、対象の有無を問わず `42501`

### 7.4 DROP

同一 write txn で、依存 DEFAULT の検査 → 定義の削除 → 全テナント分の状態行の削除（`retain` の走査削除）を行う。

- 状態テーブルが未作成なら何もせず、テーブルを作らない
- 依存検査は `column_default_references_sequence`（ワイルドカードなしの網羅 match）を呼ぶフックだけで、現時点では常に偽。`ColumnDefault` に SEQUENCE を参照する variant を足す #1690 でコンパイルエラーになり、判定の書き漏れを防ぐ。2BP01 の正例のテストは #1690 で追加する
- 走査コストの実測: 1 万テナント × 2 SEQUENCE（状態 2 万行）で、片方の DROP が約 1.5 ms（最適化ビルドのテスト。`catalog::sequence::tests::drop_scan_cost_measurement`）。DDL の頻度と許可ユーザーの限定を考えると十分小さいため、全走査を維持し、`sequence_id` の逆引き索引は作らない

### 7.5 受け入れ条件「他テナントの SEQUENCE が見えない」の範囲

Issue #1711 より前の定義は全テナント共有の `public` に属するため（D3）、本 Issue が保証するのは次の範囲に限る。

- 状態行のキー先頭が `tenant_id` で、あるテナントのキーから他テナントの状態行を読めない
- DROP は全テナントの状態行を定義の削除と同じ txn で消し、内部 ID を再利用しないため、同名で作り直しても古い状態を拾わない
- 未知の名前の `42P01` はテナントによらず同一の応答で、文言にテナント情報を含めない

**既知の残課題**: 他テナントのスキーマに属する SEQUENCE を名前で解決できないことの完全な実現は、#1711（tenant 所有のスキーマ）と #1717（SEQUENCE の修飾名）に属する。`sequence_key` が差し替え点。

## 8. Issue #1690 実装記録（nextval と currval）

親 #1686。ポインタ: TABLE-24 (1)〜(5)・ERR-6・RECOVER-10・RECOVER-11。

### 8.1 採番は書き込み txn の中で、台帳照合の後に行う

束縛段（`bind_insert_row`・`fill_omitted_columns`）は値を作らず、該当列に仮値 `Null` を置いて印（`VolatileColumn::Sequence`／`DefaultSequence`）だけを運ぶ。`tenant` の INSERT 系書き込みが `ledger::record_in_txn` の後に、書き込み txn 内で名前を解決して払い出し、行のエンコードと制約検査より前に値を差し込む。

- 同じ `operation_id` の再送は台帳照合で `23505` になるため、値を消費しない（TABLE-24 (3)）
- 束縛から書き込みまでの間に `DROP SEQUENCE` が挟まっても、書き込み txn 内の解決で古い ID へ書かない。未知の名前はテナントによらず同一の `42P01`
- 評価順は行は文の順、行の中は列順。同じ行の `VALUES (nextval('s'), currval('s'))` の `currval` は直前の値を返す
- 内容照合ハッシュは構文形（関数名と SEQUENCE 名）だけを使う（種別バイト `1`）。省略列へ適用した `DEFAULT nextval` は材料から除外する（時刻 DEFAULT と同じ）。印を持たない文のハッシュはバイト単位で不変

### 8.2 アロケータと永続化

`catalog::sequence::SequenceAllocator`（`Storage` が 1 つ所有）が `(tenant_id, sequence_id)` ごとの現在値（`last_value`・`is_called`）をプロセス内に持つ。

- 払い出しは初回が START、以降は `checked_add(increment)`。範囲外は CYCLE なら反対側の端へ折り返し、NO CYCLE なら `2200H`（現在値は進めない）
- 払い出した時点で、同じ write txn の `sequence_state` へ現在値を書く。状態値は `[版 1][last_value i64 LE][is_called]` の 10 バイト固定で、未知の版・長さ不一致・`is_called` の不正値は `CorruptSchema`
- メモリに無いときは永続化済みの状態行から種を作り、無ければ START・未呼び出しから始める
- ROLLBACK は永続値だけを戻し、メモリ上の現在値は戻さない。同じプロセスでは同じ値を再び払い出さない（欠番は許容。TABLE-24 (1)）。明示トランザクションは単一ライタを保持するので、commit 済みの行に入った値は必ず同じ txn が永続化した現在値で覆われる（TABLE-24 (4)）
- `DROP SEQUENCE` は commit 成功後に全テナント分のメモリ上の現在値を捨てる
- 保持件数（アロケータ・セッション）は上限つきで、超過は `54000`。ロックの毒化は `XX000`
- ROLLBACK 後にクラッシュした場合の再払い出しを補強 txn で防ぐ設計は #1692 で不採用と確定し（[§11.2](#112-補強-txn-の不採用)）、クラッシュ注入による検証は #1692 で追加した（[§11.4](#114-crash-試験のオラクル)）

### 8.3 CACHE の扱い（D4 が #1690 に割り当てた判断）

CACHE は引き続き `0A000` で、内部の値は 1 固定のまま。永続化の刻み幅は 1（払い出しのたびに書く）なので、共有カウンタとセッション別確保の観測差は生じない。方式の確定は、CACHE を受理する Issue で行う。

### 8.4 セッションの currval と RETURNING

- `currval` 用の状態は `SessionState` が `Arc<Mutex<..>>` で持つ。BEGIN 時のコピーは `Arc` を共有するので、ROLLBACK で `SessionState` を巻き戻しても保持される（PostgreSQL と同じ）。未呼び出しは `55000`
- 書き込み区間の文脈（アロケータとセッション状態）は `sql::sequence_scope::SequenceScope`（スレッドローカルの RAII。`core.rs` の INSERT 入口が autocommit・明示トランザクションの双方で開く）で `WriteTarget::InTxn` まで運ぶ。スコープが無い `Autocommit` は `Storage` のアロケータだけを使う（NoSQL の `DEFAULT nextval`。`currval` は `0A000`）。明示トランザクションでスコープが無ければ `0A000`
- `RETURNING` は書き込み前に列型の代表値でバイト予算を検査し、書き込み後にスコープへ記録された払い出し値で投影し直す。記録が無い列は内部不整合として拒否する（黙って 0 にしない）

### 8.5 DEFAULT

- `ColumnDefault::Sequence(名前)`（カタログタグ `q`）。整数 3 型（SMALLINT・INTEGER・BIGINT）の列だけが受理する。範囲外は採番時に `22003`（値は消費済み）
- CREATE TABLE と `ALTER COLUMN SET DEFAULT` は、永続化と同じ write txn で SEQUENCE の存在を確認する（無ければ `42P01`）。`ADD COLUMN`・FK の `SET DEFAULT`・COPY・ファイル形 INSERT・UPSERT は `0A000`
- 一時表（TABLE-44）はセッション専用の別 DB で永続 SEQUENCE を解決できないため、`CREATE TEMP TABLE ... DEFAULT nextval` と一時表への `INSERT ... nextval` は `0A000` で明示的に拒否する（`42P01` へ化けさせない）。一時コアの実行区間は外側の採番文脈を外す（`SequenceScope::suspend`）
- `DROP SEQUENCE` の依存検査（`column_default_references_sequence`）は `DEFAULT nextval` を参照として数え、`2BP01` を返す（#1688 が先送りした正例）

### 8.6 受理する構文と対象外

- 受理: INSERT の `VALUES` 要素と列 DEFAULT の単独形 `nextval('<名前>')`・`currval('<名前>')`（`currval` は VALUES のみ）。名前は文字列リテラル（regclass 相当）で、大文字小文字は保持する
- `0A000`: 修飾名・引用付きの名前（#1717）、SELECT 投影・WHERE・UPDATE SET・ビュー本文などの式位置（書き込み txn を伴う読み取り文の経路が無い。#1691 の `lastval` と合わせて判断する）、UPSERT・COPY・ファイル形 INSERT・分割実行、DEFAULT の `currval`
- 合成式（`nextval('s') + 1` 等）は単独形にならず従来の定数式経路で拒否される。`nextval`／`currval` は UDF 名として予約する
- `ALTER SEQUENCE` の commit 後の `SequenceAllocator::evict`（当該 SEQUENCE の現在値を全テナント分破棄）は `RESTART` を含むときだけ行う。それ以外のオプション変更（INCREMENT・MIN・MAX・CYCLE 等）は現在値を保持する。破棄すると ROLLBACK 済みで消費された値を再払い出ししうるため。破棄は commit 直後に writer permit を保持したまま行う

## 9. Issue #1689 実装記録（ALTER SEQUENCE）

### 9.1 構文と検査順

- 受理: `RESTART [[WITH] n]`・`START [WITH]`・`INCREMENT [BY]`・`MINVALUE`／`NO MINVALUE`・`MAXVALUE`／`NO MAXVALUE`・`CYCLE`／`NO CYCLE` の任意順（1 個以上）と、単独の `RENAME TO <新名>`。タグは `ALTER SEQUENCE`（RENAME も同じ）
- `42601`: オプションなし・同種の重複・`RENAME TO` と他オプションの併用・小数・未知のトークン。`22003`: i64 の範囲外
- `0A000`: `IF EXISTS`・`AS`・`CACHE`・`OWNED BY`・`SET SCHEMA`・`OWNER TO`・`SET LOGGED`／`UNLOGGED`・修飾名。明示トランザクション内の ALTER も `0A000`（既存の末尾の拒否に乗る）
- `22023` の検査順（PostgreSQL 17 の `init_params` と同じ）: `INCREMENT` ≠ 0 → MIN／MAX の決定（`NO MINVALUE`／`NO MAXVALUE` は変更後の INCREMENT の符号から既定値を再導出。未指定は現行値を維持）→ MIN < MAX → START ∈ [MIN, MAX]（START を指定しなくても現行値を常に検査）→ RESTART 値（値なしは START）または現在値 ∈ [MIN, MAX]。文言は値を含まない固定文言
- 判定の順序: 構文 → DDL 権限（`42501`。対象の有無を問わない）→ 名前の解決（`42P01`／`42809`）→ RENAME 先の衝突（`42P07`。同名も）→ 範囲（`22023`）

### 9.2 決定

- **定義 blob v2**: v1 の末尾に `restart_value`（i64）を足す。意味は「状態行を持たないテナントの last_value（`is_called = false`）」。CREATE 時は START、RESTART で更新する。v1 は `restart_value = start` として読み、書き込みは常に v2。`restart_value` が範囲外の blob は `CorruptSchema`
- **RESTART は全テナントへ作用する**: 定義の `restart_value` の更新と、全テナント分の状態行の削除を同一 txn で行う（DROP と同じ `retain` の走査削除）。#1711 より前は定義が全テナント共有で、呼び出し元だけをリセットすると他テナントに新しい範囲の外の現在値が残りうるため。結果が他テナントの状態に依存しない（TABLE-24 (2) のポインタ）。ただし spec が作用範囲を明示していないため自動運転下で安全側に決めた判断で、人間の確認が要る
- **範囲検査の現在値は「呼び出し元テナント＋定義の `restart_value`」に限る**: 他テナントの状態行を検査すると、他テナントの採番の進みが `22023` の有無から観測できる。新しい範囲の外に残った他テナントの現在値は、#1690 の nextval が PG の算術どおりに `2200H`（CYCLE なら折り返し）で処理する
- **RENAME は定義のキーだけを付け替える**: 内部 ID・状態行は不変（D3）。依存 DEFAULT があれば `0A000`（現状は常に偽。#1690 の `ColumnDefault` の variant が名前で SEQUENCE を保持する場合の fail-closed。`sequence_id` で保持すれば外せる）
- `CatalogError` は公開型なので variant を足さず、`catalog::sequence` の `AlterSequenceError`（範囲違反とカタログ系）で区別する

### 9.3 #1690 への申し送り

- 状態行の値の形式は #1690 が決める。呼び出し元テナントの状態行を読むフック `caller_current_value_in_txn` は、行が存在する間は形式が未確定のため `CorruptSchema` で拒否する。#1690 は状態の decode へ置き換える。置き換えを忘れても ALTER が拒否側に倒れるだけで、黙って検査が飛ぶことはない
- ALTER の commit 後の結線点 `on_sequence_altered` は空。#1690 のプロセス内アロケータ（D4）は、ここで該当 `sequence_id` のエントリを全テナント分破棄する。破棄の対象は RESTART のみで、INCREMENT・MIN・MAX・CYCLE の変更では現在値を保持する（実装は §8.6 を参照）
- 受け入れ条件「変更後の nextval が PG 17 と同じ値」は nextval の着地後に SQL 水準で検証する。期待値の表は `tests/table24_alter_sequence.rs` の冒頭にある。本 Issue では Storage 水準で格納値（`restart_value`・params・状態行の削除）を検証した
- #1689 と #1690 のどちらかが先にマージされた場合、後の側が rebase で上記を統合する

### 9.4 既知の差

- 同種オプションの重複は構文段で検出するため、未知の名前と重複が同時にあると、PG（リレーション解決が先で `42P01`）と違い `42601` が先に返る
- `ALTER TABLE <sequence> RENAME TO ...`（PG は許す）は現状の挙動のまま

## 10. Issue #1691 実装記録（setval と lastval）

### 10.1 構文と置き場

- 受理: `SELECT setval('<名>', <符号付き整数>[, TRUE|FALSE]) [AS <別名>] [;]` と `SELECT lastval() [AS <別名>] [;]` の単独形だけ。結果は 1 行 1 列の `bigint`（列名は関数名か別名）
- 専用の `ParsedSql::SequenceFunction` として受ける。`ConstSelect`（#1531）はストレージに触れない契約で、書き込み txn を伴う関数を式として評価できない。wire の `is_write_statement` は `ParsedSql::Statement` を常に「commit なし」と扱うため、`ConstSelect` に乗せると autocommit の `setval` が `committed = false` と誤分類され、RECOVER-5 の `state=may_be_committed` 契約に反する
- 検出は「先頭が `SELECT` + `setval`／`lastval` + `(`」に限り、検出後は専用の検証器だけが結論を出す（`ConstSelect` へ流さない）
- `22003`: 値が i64 の範囲外。`42601`: 小数・文字列・引数の過不足・識別子規則違反の名前。`0A000`: `NULL`・`$n`・式・キャスト・修飾名・引用付きの名前・`is_called` が `TRUE`／`FALSE` 以外・単独形以外（`FROM`・追加項目・演算子・`WHERE`・`LIMIT`・複数文）
- 式位置（`SELECT 1, setval(...)`・`WHERE`・`UPDATE SET`・`INSERT VALUES`・ビュー本文など）は許可リスト外の関数呼び出しとして `42601` で閉じ、実行されない（FROM なしの `SELECT 1, setval(...)` だけは式束縛の拒否で `0A000`。`nextval`／`currval` の式位置と同じ振り分けで、SQLSTATE も揃っている）。`CREATE FUNCTION setval` は予約名として `42723`。式束縛にも `0A000` の拒否を置いた（多層防御）
- `$n` は拡張プロトコルの Parse で `0A000`（ダミー置換より前に判定する）

### 10.2 決定

- **範囲外は `22003`**（`min_value <= n <= max_value`）。ALTER の `22023` は流用しない。文言は値・名前を含まない固定文（ERR-6 のポインタ）
- **状態は `SeqCursor { last_value: n, is_called }`**。既存の純関数 `advance` が `is_called = true` なら次は `n + increment`（範囲外は CYCLE で折り返し、NO CYCLE で `2200H`）、`false` なら次は `n` そのものを満たす。戻り値は `is_called` によらず `n`
- **作用は呼び出し元テナントの状態行 `(tenant_id, sequence_id)` だけ**（TABLE-24 (2)）。operation_id 台帳は開かない（D1。冪等で、台帳には採番値を保存しない）
- **DDL 権限ゲートはかけない**。PostgreSQL でも UPDATE 権限相当で、作用範囲が自テナントに閉じるため（全テナントへ作用する RESTART とは異なる）。自動運転下で安全側に倒した判断のため、人間の確認が要る
- **メモリ上の現在値（D4）**: アロケータはメモリを優先して読むため、`setval` が後退させうる。トランザクション内は共有 write txn へ状態行を書いた直後にメモリも更新する（ROLLBACK しても戻らず、PostgreSQL の `setval` の非トランザクション性と一致。戻るのは永続値だけ）。autocommit は `commit_and_finish` の commit 後クロージャでメモリを更新する（writer permit を保持したまま）。commit 前に更新すると、commit 失敗でメモリが永続値より後ろに下がり、払い出し済みの値を再び払い出しうる
- **`has_writes`**: `commit_active` は `has_writes == false` で write txn を破棄する。`setval` は表を書かず `mark_written` の相手がないため、`SessionTransaction::mark_sequence_written` が `has_writes` だけを立てる（`written_tables` に偽の名前を入れない）。これが無いと `BEGIN; setval; COMMIT` の永続化が落ちる
- **セッション状態（D6）**: `setval(..., true)` は `currval` を更新するが、`lastval` の対象（`last`）は変えない（PostgreSQL 17 の `do_setval` は `last_used_seq` に触れない）。`is_called = false` はどちらも更新しない。件数上限（`54000`）は commit の前に検査する
- **`lastval`**: `last` が無い、または別テナントのものは `55000`。対象の SEQUENCE の存在は**内部 ID**で確認する（RENAME は ID を保つため生き、DROP 後は同名で作り直しても別 ID なので `55000`）。消えていたらセッションの該当エントリ（`currval` と `last`）を捨てる。走査は `MAX_SEQUENCES` で有界、復号失敗は `CorruptSchema`
- 未知の名前は `42P01`、他種別は `42809`（DROP・ALTER と同じ写像。テナント非依存の同一応答）

### 10.3 対象外・申し送り

- SELECT 位置の `nextval`／`currval`（#1690 からの申し送り）: 本 Issue では拡張していない。`SequenceFnKind` に `Nextval`／`Currval` を足せば小差分で対応できる。追記先（既存 Issue か新規起票か）はユーザー確認が要る
- ROLLBACK の後にクラッシュした場合の `setval` の消失（PostgreSQL は即時に永続化する）。補強 txn と同じ系統として扱い、#1692 で不採用と確定した（[§11.2](#112-補強-txn-の不採用)）
- `INSERT VALUES (lastval())`・`setval` の `$n`・`NULL` 引数（PostgreSQL は strict で `NULL` を返す）・キャスト付き引数は `0A000`／`42601`
- 修飾名（#1717）、CACHE（後続）
- テスト: `tests/table24_sequence_setval.rs`（冒頭に PG 17 の期待値表）・`catalog::sequence::setval_tests`・`sql::allowlist::sequence_function_tests`・`sql::sequence_scope::tests`・`wire_sequence_setval.rs`

## 11. Issue #1692 実装記録（採番値の operation_id 台帳）

対応ビヘイビア（ポインタのみ）: TABLE-24 (3)(4)・RECOVER-1・RECOVER-10・RECOVER-11・RECOVER-12。

### 11.1 着地済み範囲の確認

台帳との統合の中核は #1690・#1691 で満たしており、本 Issue はコードのロジックを変えず、回帰テストと crash 試験で固定した。

- 内容照合ハッシュは採番前の構文形（`HashColumnInput::ValuesSequence`、種別 1）で計算する。省略列へ適用した `DEFAULT nextval` は材料から除外する（D2）。台帳の値のレイアウトは不変（D1。新しい OpTag も版もない）
- 採番状態は commit と同じ write txn で永続化する（`SequenceAllocator::next_value_in_txn`）。commit 済みの行に入った値は、必ず同じ txn の状態行で覆われる
- 同じ op の再送は、台帳が `23505`（`DuplicateOperationId`）で拒否し、再実行も追加の採番もしない。再送は同じ値を再現しない（Issue タイトルの「同じ値を返す」は、受け入れ条件と TABLE-24 の改訂注記に従い「採番は 1 回だけ」で満たす）

### 11.2 補強 txn の不採用

ROLLBACK・文の失敗・接続断の後に、払い出し済みの採番状態だけを書く短い write txn（補強 txn）は**採用しない**。自動運転下の判断で、人間の確認が要る。

- 根拠 1: TABLE-24 (4) が求める「commit と同じ txn での永続化」は既に満たしている。補強 txn は D4 が「要追確認」とした任意の強化で、本 Issue の受け入れ条件に含まれない
- 根拠 2: txn が終わる経路は多い（`rollback`・`fail`・`release_if_expired`・暗黙 abort・`Drop`〔接続断〕・COMMIT 失敗・autocommit の文失敗）。ここへ書き込みを足すと、`Drop` 内の I/O や writer gate の再取得待ちが生じ、SEQUENCE を使わないセッションにも影響する（スコープ外の挙動変更）
- 残る事象は 1 つだけ: rolled back した txn・失敗した文で払い出した値（`RETURNING`・`currval` で観測しうる）は、直後にクラッシュすると再起動後に再払い出しされうる。commit 済みの行どうしの二重採番は起きない。§10.3 の `setval`（ROLLBACK 後のクラッシュで消失）も同じ系統

採用する場合の設計案（別 Issue で実装する。起票はユーザー承認後）:

- フック点: `core.rs` の `TxnControl::Rollback` 腕と、autocommit INSERT が採番後に失敗した経路
- 対象集合: セッションの `currval` マップ（`SessionState::sequences_handle()`。テナント込みのキー）。DROP・RESTART 済みの ID は `evict` でメモリから消えているため書かれない。書く前に `sequence_id_exists_in` でも確認する
- 処理: `Storage::set_sequence_value` と同じ `begin_write_txn` → `SequenceAllocator::persist_current_in_txn` → `commit_boundary::commit_and_finish`。値を巻き戻さない冪等な書き込み
- 残課題: 接続断（`Drop` 内で I/O しない）、writer gate の待機、sessionless 経路（NoSQL の `DEFAULT nextval`）で対象集合をどう取るか

### 11.3 テスト行列

`crates/engine/tests/table24_sequence_ledger.rs`（採番と台帳の相互作用に限る）:

| 観点 | 内容 |
| ---- | ---- |
| 再送の拒否 | VALUES・`DEFAULT`・複数行・`RETURNING` の各形で `DuplicateOperationId`（行制約由来の `IdConflict` と variant で区別）。行数・次の値は不変で、`RETURNING` の再送は値を返さない |
| 再起動 | 書き込み → 再オープン → 再送が拒否され、系列が続きの値で進む |
| 明示 txn | autocommit 済みの op を `BEGIN` 内で再送すると `23505` で txn は `Failed`。`COMMIT` 済みの op を autocommit で再送しても `23505`。同じ txn 内の同一 op は `InvalidTransactionState`（`23505` ではない） |
| 内容不一致 | SEQUENCE 名・`nextval` → `currval`・払い出し値のリテラル化・省略列の明示化は `22023`。値を消費しない |
| DEFAULT 変更後 | 省略列の DEFAULT は材料外なので、変更後の再送は不一致にならず `23505`（D2） |
| 省略・必須モード | 既定では台帳に記録せず、id まで同一の再送は `IdConflict` で欠番になる。必須モードは `23502` で値を消費しない |
| NoSQL 経路 | `bind_insert` ＋ `execute_bound_insert_in_session` で、`DEFAULT nextval` の再送は `23505`、省略は新しい値 |
| テナント境界 | 同じ op 文字列でも他テナントと衝突せず、再送が他テナントの系列を動かさない |

### 11.4 crash 試験のオラクル

`crates/engine/examples/crash_tool_sequence.rs` と `scripts/crash_test_sequence.sh`（`make crash-test-sequence`、CI の独立ジョブ `crash-test-sequence`）。`write` を SIGKILL し、再オープン後の `verify` で次を検査する。

- 行 id が 1 起点で連続し、全行の採番値が NULL でなく 1 以上
- **採番値が行 id の順に狭義単調増加**する（二重採番と、再起動後に commit 済みの値以下を払い出すことの両方を検出する）
- kill 前に確認済みの行数・採番値を下回らない
- セットの最後に `finish`: 最後の commit の再送が `DuplicateOperationId`、SEQUENCE 名を変えた同 op の文が `OperationIdContentMismatch`、続く新しい 1 行の値がちょうど `max + 1`（拒否された文が値を消費していない）
- 負の対照 `self-check`: 重複・逆順・欠落・NULL の列と、DB へ書いた重複値をオラクルが拒否する

ROLLBACK した値の再出現はオラクルにしない。`write` は数行ごとに `BEGIN` → `INSERT … nextval` → `ROLLBACK` を挟んでメモリ上の値を永続値より先行させるが、rolled back の払い出しは永続化の約束外（§11.2）で、再起動後の再払い出しは仕様どおりの事象であり失敗にしない。

### 11.5 golden の追加

`recovery::content_hash::tests::typed_insert_layouts_with_sequence_calls_are_golden` が、`nextval`／`currval` の構文形を含む単一行・バッチ・UPSERT のハッシュを hex で固定する。永続化済みの台帳エントリとの再送互換（D2）のため、定数は採取時の値のまま更新しない。既存の `typed_insert_and_upsert_layouts_without_volatile_defaults_are_golden` は不変。

### 11.6 対象外・申し送り

- 接続断（`Drop`）後のクラッシュでの再払い出し、§10.3 の `setval` の消失（§11.2）
- NoSQL 表層の `returning`（NOSQL-22）、CACHE の受理、変更ログ・レプリカ（RECOVER-14・RECOVER-15）
- 新しいジョブ `crash-test-sequence` の必須チェック登録はオーナー作業（実装側は ruleset を変更しない）
