# TEMP TABLE 系列の設計記録

TEMP TABLE（親 Issue #1680・ビヘイビア TABLE-44）の設計記録。仕様本文は private spec
（`docs/spec/04-behavior/data-model.md` の TABLE-44）を参照し、ここには転記しない。

## Issue #1681 セッション単位の一時カタログ

### 置き場所

- 一時カタログ（`sql::temp_catalog::SessionTempCatalog`）は `sql::mode::SessionState` が保持する
  - `SessionState` は wire 接続 1 本につき 1 個の値型。他セッション・他テナントへ漏れる経路が構造上無い
  - 永続化しない
  - NoSQL（HTTP）表層は毎回 `SessionState::default()` を作るため、一時表は見えない
- ROLLBACK／abort の `SessionState` 巻き戻しに乗るため、一時カタログも取り消される
- スキーマは `Arc<TableSchema>` で持ち、BEGIN／メッセージ単位の `clone` を安くする
- 行データは `SessionState` に入れない（#1682 への申し送り）

### 解決規則（修飾なしの名前のみ）

- 一時カタログ → 永続カタログの順に引く。一時表は同名の永続の表・ビューを隠す
- 一時カタログ内の名前重複は `42P07`。1 セッションの数は上限超過で `54000`
- `SessionRelationLookup` が `TableLookup` を包む（trait は変更しない）。一時カタログが空なら素通し
- 解決結果は `ResolvedRelation::{Temp, Persistent}` で型に区別を付ける

### 暫定ゲート（#1681 で導入、#1684 で置換済み）

> #1684 で、対応済みの文種は一時コアへ振り分け、それ以外は `0A000` のままにする規則へ置き換えた
> （下の「Issue #1684」）。以下は #1681 時点の記録。

行ストアと実行器が無い間に一時表を引く文を通すと、同名の永続表へ作用してしまう。
一時カタログが空でないセッションでは、次の文を `0A000` で拒否する。

| 入口 | ゲート |
| ---- | ------ |
| `execute_sql_in_session`・`execute_sql_in_txn` | parse 中の hit 記録 ∪ トークン走査（細かい） |
| `begin_copy`・`begin_copy_in_txn` | トークン走査（細かい） |
| `execute_parsed_in_session`・`execute_parsed_in_txn`・`describe_*` | 一時カタログが空でなければ一律（粗い。トランザクション制御は除く） |

- 明示トランザクション中の拒否は parse 失敗と同じく `Failed` へ遷移させる
- 一時カタログが空のセッションでは挙動は不変
- #1683 で次を例外として通す（実行段が自セッションの一時カタログで名前を解決するため）:
  `CREATE [TEMP] TABLE`・`DROP TABLE` は細かいゲートのトークン走査を飛ばし、粗いゲートも
  `CreateTable`／`CreateTempTable`／`DropTable` を通す。それ以外の文は従来どおり `0A000`

### 後続への申し送り

- #1682: 行ストア・切断時の破棄・総バイト数の上限（完了。下記「Issue #1682 行ストア」）
- #1683: `CREATE [GLOBAL|LOCAL] TEMP TABLE`、`ON COMMIT`、DDL 権限ゲートの対象外とすること、上限の CLI（完了。下記「Issue #1683」）
- #1684: 一時表への DML・索引・制約、暫定ゲートの解除（セッションを受ける拡張クエリ Parse の導入を含む）。永続ビュー本文は永続カタログで解決する
- #1685: 実装済み（下の「Issue #1685」）
- 修飾名（`pg_temp.<名前>`）はスキーマ・`search_path`（TABLE-27）の実装へ申し送る

## Issue #1682 行ストア

> #1684 で、行単位の独自ストアから「永続 DB と同じ物理レイアウトの独立した `Storage` ＋専用の
> `EngineCore`」へ再設計した（下の「Issue #1684」）。以下は #1682 時点の記録。ただし、
> 所有テナントの固定・物理長の backstop・キャッシュのゲート・切断での破棄の方針は引き継いでいる。

実装は `crates/engine/src/sql/temp_rows.rs`、テストは `temp_rows_tests.rs`（対象ビヘイビア: TABLE-44）。

### 置き場所

- セッションごとのインメモリ `redb::Database`（`InMemoryBackend` を物理上限付きで包む）。
  永続の行テーブルと同じ `(tenant_id, id) → 行バイト列` の形に揃え、#1684 が既存の `ReadSource`
  ベースの読み取り本体と行形式を再利用できるようにする
- `SessionState` が `Option<Arc<TempRowStore>>` で保持する。最初に行領域が要るとき
  （#1683 の `CREATE TEMP TABLE`）に遅延生成する（空でも約 1 MiB を確保するため、接続のたびには作らない）
- 永続 DB の `writer_gate`・台帳・`batch_log`・`table_generation` を通らないため、永続化・変更ログ・
  レプリケーションの対象外であることが構造上保証される

### 破棄と残骸

- ディスクに書かない。切断（エラー終了を含む）で `SessionState` が drop され、最後の `Arc` が落ちると
  メモリごと消える。クラッシュ・再起動後に残る物は無い（カタログ項目・データディレクトリ内の一時領域とも）
- HTTP（NoSQL）表層はリクエストごとに `SessionState::default()` を作るため、ストアは常に `None`

### 総バイト数の上限

- 「tenant_id 長 + 8 + 値長」の論理合計を、temp DB 内のメタテーブルに同一トランザクションで集計し、
  書き込みの前に checked 演算で判定して超過は `54000`（固定文言）。backend の長さは解放ページが戻らず判定に使えない
- 既定は 64 MiB（走査上限と同値）。最悪ケースの論理値は 256 接続 × 64 MiB = 16 GiB。起動時 CLI は #1683
- 多層防御として backend が物理長の上限（論理上限 × 4 + 16 MiB）を超える拡張を拒否し、
  以後そのストアを使用不可にする（fail-closed）
- redb の cache size は小さく（1 MiB）、Durability は `Immediate` のまま（`None` では解放ページが再利用されない）

### キャッシュのゲート・テナント境界

- `TempReadTxn` の `ReadSource::snapshot()` は常に `None`。名前・世代をキーにするキャッシュへ一時表の行が載らない
- ストアは所有テナントを固定し、異なる tenant_id は固定文言の `Internal` で拒否する

### 巻き戻しとの整合

- `SessionState` の clone／restore では `Arc` 共有のストアの中身は戻らない。temp DB の書き込み
  トランザクションの寿命を `SessionTransaction` に合わせる（BEGIN／1 文で開始、COMMIT で commit、
  ROLLBACK・失敗・drop で abort）。結線は #1684
- カタログの巻き戻しで取り残された行領域は `TempWriteTxn::retain_relations` で掃除できる
- `SessionTempCatalog::remove` は削除した `TempRelationId` を返す（DROP が `drop_relation` を呼ぶため）

## Issue #1683 CREATE TEMP TABLE と ON COMMIT

### 構文と表現

- `CREATE [GLOBAL|LOCAL] {TEMP|TEMPORARY} TABLE [IF NOT EXISTS] <名前> (<列定義・制約>)
  [ON COMMIT {PRESERVE ROWS|DELETE ROWS|DROP}]`。`GLOBAL` は `LOCAL` と同義で、PG が出す非推奨の
  警告は出さない（DDL に NoticeResponse の経路が無いため）。`ON COMMIT` 省略時は `PRESERVE ROWS`
- 新しい `ParsedSql::CreateTempTable(ValidatedCreateTempTable { table, on_commit })` を別 variant にした。
  `ValidatedCreateTable` に `temp` フィールドを足す案では、永続表の分岐の確認漏れが「一時表の定義が
  全テナント共有の永続表になる」fail-open になる。別 variant なら網羅 `match` がコンパイル時に漏れを検出する
- 列定義・制約のパーサー本体は永続表と共有（`parse_create_table_body`）。`SqlOutcome` と CommandComplete
  タグは永続表と同じ（`CREATE TABLE`／`DROP TABLE`）
- 素の `CREATE TABLE ... ON COMMIT ...` は `42P16`

### 作成時に弾く形（fail-closed）

| 形 | 結果 |
| -- | ---- |
| `CREATE TEMP TABLE ... AS ...`（SQL-52 未確定） | `0A000` |
| `VECTOR(N)` 列 | `0A000` |
| ENUM 列・ENUM 配列列（`DROP TYPE`／`ALTER TYPE` の依存追跡が永続表しか追わない） | `0A000` |
| 一時表の `FOREIGN KEY`（参照先が永続表） | `42P16` |
| 一時表の `FOREIGN KEY`（参照先が一時表。実行段は #1684） | `0A000` |
| 永続 `CREATE TABLE` の `FOREIGN KEY` がこのセッションの一時表名を指す | `42P16` |

判定は構造段（VECTOR・ENUM・CTAS）か自セッションの一時カタログだけで行い、永続カタログの存否を
観測するオラクルにならない。

### 実行

- DDL 権限ゲート（SQL-23）の対象外。セッション内で閉じるため
- 順序: `IF NOT EXISTS` の既存一時表 → FK → スキーマ構築（`sql::ddl::build_table_schema` を永続表と
  共有）・検証（`catalog::prepare_temp_table_schema`）→ 重複 `42P07`・件数上限 `54000` → 行ストアの
  遅延生成 → 登録。重い確保は判定の後に行い、拒否された文はセッションを変えない
- 同名の永続表があっても作成できる（一時表が名前解決で優先される）。明示トランザクション内は
  既存の DDL と同じ `0A000`（トランザクションは `Failed`）

### 一時表の DROP TABLE

実行段が `table_names` を自セッションの一時カタログで分類する。すべて一時表なら DDL 権限ゲートの
対象外でカタログから外して行領域を破棄し、どれも一時表でなければ従来の永続の経路（権限ゲート）、
混在は `0A000`（2 つのストアにまたがる原子的な削除ができないため）。
分類は自セッションの一時カタログだけで行うため（永続表の存否を観測しない）、一時表に当たらない名前は
存在の有無によらず「永続側」と見なす。したがって `DROP TABLE IF EXISTS <一時表>, <存在しない名前>` も
混在として `0A000` になる（PG なら成功する文だが、拒否側に倒れるため安全。既知の制約）。

### ON COMMIT

commit 成功後に `ON COMMIT` 適用が失敗した場合（一時表の行ストアは in-memory redb のため発生確率は
極めて低い）、永続側は commit 済みだがクライアントには `XX000` が返る。この食い違いは既知の制約として
許容する（永続側の commit を巻き戻せないため）。

- `TempTableEntry.on_commit` を持ち、`SessionState::apply_temp_on_commit` が commit 成功後に適用する。
  `DROP` はカタログから外して行領域を破棄、`DELETE ROWS` は行を消す。行ストアの更新は 1 本の
  書き込みトランザクションで原子的に行う。対象が無ければ何もしない
- 呼び出し点: 自動コミットの文の成功直後（`execute_parsed_in_session_inner`）と、明示・暗黙
  トランザクションの commit 成功直後（`sql::transaction::commit_active`）。ROLLBACK・失敗した文では呼ばない
- 明示トランザクション内の CREATE は `0A000` のため、`ON COMMIT DROP` の CREATE は常に自動コミットで走り、
  同じ文の終わりで消える（PG の自動コミット時と同じ）。`DELETE ROWS` の行の消え方は DML が #1684 まで
  SQL から書けないため、行ストアへ直接行を置く単体テストで検証した
- フック失敗は固定文言の `XX000`（詳細を渡さない）。孤児の行領域は `retain_relations` で掃除できる

### 上限の CLI

`--max-temp-tables <1-64>`・`--max-temp-bytes <1-67108864>`（`temp_table_limits`／`temp_table_limits_opt`）。
既定値＝ハード上限で、下げる方向だけ設定できる（SQL-46 と同じ流儀）。上限はセッション単位で、
メモリの最悪値は「最大接続数 × バイト上限」。HTTP（NoSQL）表層は `CreateTempTable` を `0A000` 相当で
拒否する（要求ごとの行ストア確保を避ける多層防御）。

### 後続への申し送り

- #1684: 実装済み（下の「Issue #1684」。`COPY` の自動コミット経路への `ON COMMIT` フック統合を含む）
- #1685: 実装済み（下の「Issue #1685」）
- 未対応: ENUM 列・`CREATE TEMP TABLE ... AS`（SQL-52）・`GLOBAL` の警告・修飾名（TABLE-27）

## Issue #1684 DML・索引・制約の適用

実装は `crates/engine/src/core/temp_route.rs`（振り分けと実行）、`sql/temp_rows.rs`（実体ストア）、
テストは `crates/engine/tests/table44_temp_table_dml.rs`・`crates/wire-server/tests/wire_temp_table.rs`
（対象ビヘイビア: TABLE-44 (4)(5)。関連: SQL-31・RECOVER-1・SQL-23）。

### 設計: 一時コア（独立した `Storage` ＋専用の `EngineCore`）

- セッションごとに、インメモリの独立した `Storage`（永続 DB と同じ物理レイアウト）と、それだけを
  ストレージとする専用の `EngineCore`（以下「一時コア」）を遅延生成する（最初の `CREATE TEMP TABLE`）。
  `SessionState` が `Arc<TempRowStore>` で保持し、切断で破棄される
- 一時表の文は、永続コアと**同じ実行器**（DML の各フォーム関数・読み取り・制約・索引）を、
  一時コアで実行する。`PRIMARY KEY`・`UNIQUE`・`CHECK`・`NOT NULL`・`DEFAULT`・`RETURNING`・
  `ON CONFLICT`・述語形 `UPDATE`／`DELETE`・`TRUNCATE`・集計・JOIN・`EXPLAIN` が永続表と同じ規則で働く
- 採用理由: 既存の書き込み・読み取り本体は `self.storage` を直接引く箇所が多数あり
  （INSERT のスキーマ取得・サブクエリ解決・索引 DDL など）、永続コアで一時表を扱うと同名の永続表へ
  作用する経路を残しうる。一時コアでは `self.storage` が一時 DB なので、その経路が構造上存在しない
- 一時コアは永続コアの各種上限（行数・走査・JOIN・ネスト等）だけを引き継ぎ、ストレージ・キャッシュ・
  embedder・query planner は共有しない。台帳モードは `CompareOnlyWithoutLedger` 固定。検索 provider は
  スレッドを起こさない `CpuScalarProvider`（一時表は `VECTOR` 列を持てずベクトル検索をしない）。
  派生キャッシュ（スカラー索引・可視行など）は一時コア専用で、使われるまで空（各キャッシュ自身の
  容量上限あり）。メモリの最悪値は「最大接続数 × バイト上限」に、これら派生キャッシュが一時表の
  データ量に比例して加わる
- 一時カタログ（`SessionTempCatalog`）は引き続きセッション内の名前解決の正で、一時 DB のカタログに
  同じ定義を持つ。DDL は自動コミットのみで明示トランザクション内は `0A000` のため、両者が食い違うのは
  行（DML）だけで、DML の巻き戻しは書き込みトランザクションの寿命で実現する

### 振り分け（parse 時）

一時カタログが空のセッションは従来と同一。空でない場合、トランザクション制御・`CREATE [TEMP] TABLE`・
`DROP TABLE` を除き、トークン列が一時表の名前を含む文を次のとおり扱う。

| 条件 | 結果 |
| ---- | ---- |
| 一時カタログ優先の lookup で parse して一時表だけに当たる | 一時コアで parse し直し、`ParsedSql::SessionRouted`（一時表の経路）で包む |
| 一時表と永続の関係（表・ビュー）の混在 | `0A000`（固定文言。永続側の存否を含まない） |
| 振り分け対象外の文種（`ALTER`・ビュー・型・カーソル・`PARTITIONED` 等） | `0A000` |
| 一時表の名前が関係として引かれない（列名との衝突・サブクエリ内など） | `0A000`（保守的な拒否） |
| `CREATE INDEX` の対象が一時表 | 一時コアで実行（DDL 権限ゲートの対象外） |
| `DROP INDEX` の名前が一時 DB の索引 | 一時コアで実行（同名の永続の索引より優先。PostgreSQL の `pg_temp` と同じ）。なければ従来の永続の経路 |

- 永続ビューは一時表で隠されない。ビュー本体が同名の永続表を指していても、一時表の名前を含まない
  クエリは従来どおり永続側で実行される。ビューと一時表を同時に参照する文は混在として拒否する
- 一時表を参照するビューの作成は `0A000`
- 一時表の文は `SessionRouted` という別 variant にして、網羅 `match` で永続コアへ落ちる fail-open を
  コンパイル時に防ぐ（`CreateTempTable` を別 variant にした #1683 と同じ理由）

### operation_id（TABLE-44 (5)）

- 一時表への書き込みは `operation_id` 台帳の対象外。`USING OPERATION_ID`（`PARTITIONED` を含む）は黙って
  捨てずに `0A000`
- `--require-operation-id`（`LedgerMode::Ledgered`）の構成でも、一時表への書き込みに id を要求せず、
  台帳表にも触れない（一時コアの台帳モードが `CompareOnlyWithoutLedger` で、別 DB のため）

### トランザクションの寿命

- 自動コミット: 書き込み文は一時の書き込みトランザクションの中で実行し、文の後・commit の前に総バイト数の
  上限を判定する。読み取り文は一時コアの通常の読み取り経路
- 明示・暗黙トランザクション: 最初の一時表の文で一時側の書き込みトランザクションを開き
  （`ActiveTxn` が保持）、読み取りもこのトランザクションを読み取り源にする。ROLLBACK・失敗・期限切れ・
  接続断では `ActiveTxn` の破棄で自動的に abort する
- COMMIT の順序: 永続側 commit → 一時側 commit → `ON COMMIT` の適用。2 つの DB の commit は原子的でなく、
  永続側の commit 後に一時側が失敗すると、クライアントには `XX000` が返るが永続側は commit 済みになる
  （一時 DB はインメモリで発生確率は極めて低い既知の制約）
- 一時 DB の単一ライタは永続 DB のものと別。同じスレッドが既に書き込みトランザクションを保持している
  状態での再取得は、待たずに `55P03` を返す（デッドロックしない）。`ON COMMIT` は一時側を先に
  commit して手放してから適用する
- `COPY ... FROM STDIN` の完了処理は `commit_copy_in_txn_in_session`（セッション対応版）を通る。一時表向けの
  バッチは対象ストアを運び、セッションを持たない永続の入口（`commit_copy_in`・`commit_copy_in_txn`）は
  `0A000` で拒否する。永続表の自動コミットの COPY の後にも `ON COMMIT` を適用する

### 総バイト数の上限（`--max-temp-bytes`）

- 既存の書き込み本体は一時コア専用の課金口を持たないため、文の実行後・commit の前に redb の確保済み
  ページ総量（`allocated_pages * page_size`。行・カタログ・索引・世代表を含む）を判定する。超過は
  abort して `54000`。明示トランザクションでは超過した文で `Failed` になり全体が abort される
- 判定の高速経路: 確保済みページ総量は物理長以下なので、物理長が上限以内なら上限以内と判定する
  （`stats()` は全表の木を辿るため、毎文呼ぶと一時表が大きいほど 1 文あたりの費用が線形に増える。
  20,000 行の連続 INSERT で永続表と同等の所要時間に収まることを確認した）。物理長が上限を超えている間だけ
  正確な確保済み総量を測り直す
- 判定の時点（書き込み前の論理集計 → 文の後・commit の前の確保済み総量）と単位（ページ単位）が #1682 から
  変わった。`--max-temp-bytes` の意味（既定値・下げる方向だけ設定可能）は変えていない
- 物理長の backstop（上限 × 4 + 16 MiB。超過で以後そのストアを使用不可にして `54000`）は多層防御として残す。
  redb は I/O エラー後にそのデータベースを使えなくするため、通常の上限判定の代わりにはできない

### 準備済み文（拡張クエリ）

- `parse_sql_prepared_with_declared_in_session`・`bind_prepared_in_session` を追加した。一時表の名前を含む文は
  Parse で一時コアにより構造検証し、Bind も同じ一時コアで行って `SessionRouted` を返す
- 一時カタログに世代（`SessionTempCatalog::epoch`。プロセス全体で単調な採番で、ROLLBACK の巻き戻しでは値も
  戻る）を持たせ、`SessionRouted` は振り分け時の世代と参照する一時表を覚える。一時表の作成・削除・索引 DDL の後に
  実行すると `0A000`（PostgreSQL なら再計画する場面。本実装は再計画せず fail-closed。再 Parse すれば通る）
- 一時カタログが空でない間に Bind した永続側の文は、一時表の名前を含まないことを確かめてから世代つきで
  `SessionRouted`（永続側）に包む（後から作られた同名の一時表に、準備済みの永続側の文が黙って
  別の意味で解決されるのを防ぐ）。含む場合は `0A000`
- wire-server は `$n` なしの文も元の `PreparedSql` を保持し、一時表を持つセッションでは Bind のたびに
  セッション対応の経路で検証し直す（一時表を持たないセッションは従来どおり Parse 時の構文形を使う）

### 既知の差分・対象外

- 一時表と永続の関係を 1 文で参照する文（JOIN・`INSERT ... SELECT`・サブクエリ）は `0A000`。2 つの DB を
  1 つの読み取り元で扱う合成が要るため後続（PostgreSQL では可能）
- 一時表と同名の識別子を列名などに使う永続表の文（例: 一時表 `name` があるセッションでの
  `SELECT name FROM perm`）も、サブクエリ内の関係参照を parse 時に見分けられないため `0A000` にする
  （同名の永続表を黙って読む fail-open を避ける保守的な拒否。#1681 のゲートと同じ範囲で、一時表を
  作らないセッションには影響しない）
- サブクエリ内で永続表を参照する一時表の文は、一時コアが永続表を知らないため `42P01`（読めない）になる
  （データは返さない。`0A000` の混在とは別のエラーコードになる）
- 一時表の `ALTER TABLE`・一時表どうしの FK・`VECTOR`／HNSW／疎索引・ENUM 列・CTAS（SQL-52）・
  修飾名 `pg_temp.`（TABLE-27）・カーソル（`DECLARE ... FOR <一時表>`）・一時表を参照するビューは `0A000`
- 陳腐化した準備済み文は再計画せず `0A000`
- 2 つの DB の commit が非原子（上記）
- `ON COMMIT DELETE ROWS` は commit のたびに一時 DB の該当表を `TRUNCATE` する（対象表の行が空でも
  書き込みトランザクションを 1 回張る）
- テナント境界・crash 耐性の CI 検査: 実装済み（下の「Issue #1685」）

## Issue #1685 テナント境界・crash 耐性の検査

TABLE-44 (1)(2)(6) のポインタ。一時表は接続ごとのインメモリ領域に閉じる設計のため、他セッション・
他テナントからの不可視と、永続側に残骸が無いことは構造上成り立つ。本 Issue は実装を変えず、その性質を
回帰テストと CI で固定する（将来の spill 化や、文を永続コアへ誤って振り分ける変更を CI で検出する）。

### 追加したテスト

| 観点 | 場所 |
| ---- | ---- |
| 全文種（SELECT/INSERT/UPDATE/DELETE/TRUNCATE/CREATE INDEX/DROP INDEX/DROP TABLE/COPY）で、他セッション・他テナントの応答が「一度も存在しなかった DB」の応答（`wire_code` と `Display`）と一致 | `crates/engine/tests/table44_temp_table_isolation.rs` |
| 同名の一時表が 3 者（A・同一テナント別セッション・別テナント）で独立 | 同上 |
| 他テナントの行が同名の永続表に大量にあっても結果が不変（Public のみ・Public+Private の 2 モード） | 同上 |
| 他セッションが同名の一時表を持つか否かでエラー応答が変わらない（行データの非混入） | 同上 |
| 一時表・一時索引・トランザクション中の書き込みが永続カタログ・ファイルに到達しない（コア破棄・再オープン後も） | 同上 |
| `SessionState` を受け取らない入口（NoSQL 表層と同形）が一時表名を解決しない | 同上 |
| wire 越しの別テナント接続の不可視（応答差分とカナリア文字列）、トランザクション途中の異常切断後の破棄と永続側の書き込みロック解放 | `crates/wire-server/tests/wire_temp_table.rs` |
| pg wire で作った一時表が HTTP（NoSQL）表層からは存在しない表と同じ応答 | `crates/wire-server/tests/nosql13_ddl.rs` |

### crash 耐性の検査

- `crates/engine/examples/crash_tool_temp_table.rs` の `write` が、複数の一時セッション（永続表と同名の一時表・
  一時索引・`ON COMMIT DELETE ROWS`・別テナントの同名一時表）を生かしたまま、明示トランザクション内で一時表と
  永続表へ同時に書き込む。`scripts/crash_test_temp_table.sh` が SIGKILL し、`verify` が次を検査する。
  永続カタログと redb の生テーブルに一時表・一時索引由来の名前が無いこと、永続表の行が `perm-` 始まりで
  0 起点連続かつバッチ単位であること、データディレクトリに DB ファイル以外が無いこと、新しいセッションから
  一時表が見えないこと、同名で作り直すと空であること。
- 負の対照 `self-check`: 健全な DB を受理し、永続表 `scratch_tmp` のある DB・永続表に `tmp-` 行がある DB・
  DB の隣に余分なファイルがある DB を拒否できることを、反復の前に 1 回確かめる（「何も無いから通る」を排除）。
- 専用の空 `TMPDIR` を writer と verify に与え、全反復の後に空であることを確かめる（spill の検出）。
- CI ジョブ `crash-test-temp-table`（独立ジョブ）と `make crash-test-temp-table`。`make ci` の依存にも含む。
- `scripts/check_tmp_leak.sh` の対象に一時表のテスト（engine の 3 本と `wire_temp_table`）を追加した。

### 対象外・現状維持

- 同一 `SessionState` を別テナントの ctx で使った場合の固定文言 `XX000` は、wire からは到達できない
  防御経路のため変更しない（データは返さない）。
- 修飾名 `pg_temp.` は TABLE-27、2 つの DB の commit の非原子性は #1684 の既知の制約のまま。
- 新ジョブ `crash-test-temp-table` を必須チェックへ登録するには ruleset の更新が必要で、運用者が行う。

## Issue #2193 追記（rowid 採番状態と一時表）

一時表は独立した in-memory `Storage` を持つため、rowid 採番状態（`rowid_state`）もその `Storage` 内に独立して置かれ、
永続 DB へは到達しない。採番状態の表は初回払い出し時に遅延生成し、従来方式の一時表（現状すべて）では一度も作られない
ため、確保ゼロ・ページ予算（ポインタ: TABLE-44 (6)）は変わらない。
