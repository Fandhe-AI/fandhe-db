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
- 一時カタログが空のセッション（本 Issue の時点では全セッション）では挙動は不変

### 後続への申し送り

- #1682: 行ストア・切断時の破棄・総バイト数の上限（完了。下記「Issue #1682 行ストア」）
- #1683: `CREATE [GLOBAL|LOCAL] TEMP TABLE`、`ON COMMIT`（`SessionTransaction::commit` 側）、DDL 権限ゲートの対象外とすること、上限の CLI
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
