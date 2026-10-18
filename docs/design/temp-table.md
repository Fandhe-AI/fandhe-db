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

- #1682: 行ストア・切断時の破棄・総バイト数の上限
- #1683: `CREATE [GLOBAL|LOCAL] TEMP TABLE`、`ON COMMIT`（`SessionTransaction::commit` 側）、DDL 権限ゲートの対象外とすること、上限の CLI
- #1684: 一時表への DML・索引・制約、暫定ゲートの解除（セッションを受ける拡張クエリ Parse の導入を含む）。永続ビュー本文は永続カタログで解決する
- #1685: テナント境界・crash 耐性の CI 検査
- 修飾名（`pg_temp.<名前>`）はスキーマ・`search_path`（TABLE-27）の実装へ申し送る
