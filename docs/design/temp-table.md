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

### 暫定ゲート（#1684 で置換）

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
- #1685: テナント境界・crash 耐性の CI 検査
- 修飾名（`pg_temp.<名前>`）はスキーマ・`search_path`（TABLE-27）の実装へ申し送る

## Issue #1682 行ストア

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

- #1684: 一時表への DML・索引・制約・`TRUNCATE`・`COPY`、暫定ゲートの撤去、temp 書き込みトランザクションを
  `SessionTransaction` の寿命へ統合すること（`COPY` の自動コミット経路への `ON COMMIT` フック統合を含む）
- #1685: テナント境界・crash 耐性の CI 検査
- 未対応: ENUM 列・`CREATE TEMP TABLE ... AS`（SQL-52）・`GLOBAL` の警告・修飾名（TABLE-27）
