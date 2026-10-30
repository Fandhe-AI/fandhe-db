# TRIGGER カタログと CREATE／DROP TRIGGER（Issue #1722）

親 Issue はトリガ機構の実装系列。ビヘイビア ID は TABLE-45 のポインタ表記のみで参照し、spec 本文は
転記しない。実行モデルの決定（D 番号）は [trigger-execution-model.md](trigger-execution-model.md)。
本 Issue の担当は D8（定義対象の制限）・D12（カタログ・DDL の契約）・D2（型検査）・D7（`WHEN` の定義時検査）
のうち、下記「範囲」の部分。発火（#1723 以降）は含まない。

## 範囲

- 実装した: TRIGGER カタログ（`crates/engine/src/catalog/trigger.rs`）、`CREATE [OR REPLACE] TRIGGER`・
  `DROP TRIGGER [IF EXISTS]` の構文・実行（`sql/allowlist/trigger.rs`・`sql/trigger_ddl.rs`）、
  表・関数・SCHEMA のライフサイクルとの連動（下記）、wire のコマンドタグ。
- 暫定ガードで止めた（黙って受理しない）: `WHEN`（`0A000`）、`UPDATE OF` が参照する列の
  `RENAME COLUMN`・`ALTER COLUMN TYPE`（`0A000`）・`DROP COLUMN`（`2BP01`。CASCADE の連鎖削除は未対応で
  RESTRICT・CASCADE とも `2BP01`）。
- 範囲外（後続）: 発火、`RETURNS trigger` 関数の保存形式と成功経路の E2E（#2228・#1723）、
  `DROP FUNCTION ... CASCADE` によるトリガの連鎖削除（#1917）、件数上限の CLI 設定、一時表のトリガ。

## 関数側が未整備であることの帰結

`RETURNS trigger` 関数（#2228）はまだ無く、UDF カタログの `FunctionDef` はトリガ関数を表現できない。
`catalog::trigger::returns_trigger` は常に `false` を返す単一の判定関数で、#2228 が表現を足したらここだけを
結線する。現状の SQL 経路は、関数が無ければ `42883`、あれば `42P17` で、成功経路には到達しない
（fail-closed）。受入基準の「再起動後も定義が残る」「他テナントのトリガが見えない」は、判定関数を注入できる
`Storage::create_trigger_inner` を使った Storage 水準のテストで固定している。

## テーブル

| 表 | キー | 値 |
| -- | ---- | -- |
| `triggers` | `(table_key, scope, trigger_name)` | 定義 blob |
| `trigger_id_counter` | 固定キー `next_id` | `u64`（1 始まり・再利用しない） |

どちらも最初の書き込みまで作らない（遅延作成。読み取りと連動操作は不在を 0 件と扱い、表を作らない）。
既存 DB のファイル構成は変わらない。

## キー設計とテナント境界（P0）

- `scope` は検証済みの `tenant_id`。全 API が `scope` を必須引数に取り、範囲走査でも必ず突き合わせるため、
  他テナントの定義は引けない。他テナントにしか無いトリガは不在と同じ（`DROP` は `42704`）で、同じ共有
  `public` 表に別テナントが同名のトリガを作っても `42710` で存在を漏らさない。
- 表キー（`table_key`）を先頭に置くのは、表の DROP・RENAME が全テナント分の定義を前方一致の範囲走査
  だけで処理するため（共有 `public` 表には複数テナントのトリガがありうる）。
- **トリガ関数は同じ scope の関数に限る**。UDF カタログがテナント単位で、他テナントの関数を引く経路が
  無いため。D9（`trigger-execution-model.md`）の「共有 `public` 表のトリガが全テナントの書き込みで発火
  する」契約は本記録では変更しない。現状のカタログ（`scope` = 作成テナント）はこの契約をまだ表現できない
  ため、**D9 の全テナント発火は未実装の残課題**として #1723（発火処理）で扱う。それまで本カタログは
  暫定的に作成テナント単位の定義だけを持ち、発火経路が未接続のため発火漏れは起きない。
  全テナント共有には予約 scope の追加等が必要で、blob の形式は変わらない。契約の確定（D9 を維持するか
  狭めるか）はオーナー・spec 側の判断事項であり、spec リポ側の課題として別途報告する。

## 定義 blob v1

`u8 version(=1) / u64 id / u8 owner(予約=0) / u8 timing / u8 events / u8 level / update_of 列名リスト /
function_key / TG_ARGV リスト / WHEN の有無 + 正規化テキスト`。文字列は `u32` 長 + UTF-8、整数はリトル
エンディアン。

- 未知の版・タグ・ビット、切断、末尾の余り、上限超過、形状不正は固定文言の `CorruptSchema`
  （blob の断片・tenant_id・本文を含めない）。件数・長さは確保の前に検査する。
- 上限（実装既定値。spec 由来ではない）: 1 表・1 テナントあたり 64 件、`TG_ARGV` 100 個・各 1 KiB、
  `UPDATE OF` は列数上限以内、`WHEN` はビュー本体と同じ上限。

## 書き込み経路と判定順

単一 write txn・1 回の `commit_boundary::commit`（クラッシュしても全か無か）。成功時に対象表の世代を進める。

1. 構文（`0A000`: `INSTEAD OF`・`REFERENCING`・`CONSTRAINT`・`DEFERRABLE`・`FROM`・`WHEN`・
   `TRUNCATE` の行トリガ。`42601`: 構文違反・トリガ名の SCHEMA 修飾・イベント重複）
2. DDL 権限（`42501`。カタログ照会より前。対象の有無を問わない）
3. 明示トランザクション内は `0A000`（既存 DDL と同じ）
4. 以降は txn 内: SCHEMA の現存 → 対象の種別（ビュー・索引・SEQUENCE・マテリアライズドビューは `42809`）→
   表の存在（`42P01`）→ `UPDATE OF` の列（`42703`）→ 関数（不在 `42883`・トリガ関数でない `42P17`）→
   同名（`42710`。`OR REPLACE` は既存の内部 ID を引き継いで置換）→ 件数上限（`54000`）

`DROP TRIGGER`: 表が無ければ `42P01`、トリガが無ければ `42704`。`IF EXISTS` は表の不在も含めて成功の
no-op。`CASCADE`／`RESTRICT` は受理する（トリガに依存するオブジェクトは無い）。

関係名の解決は `sql/relation_name.rs` が担う。`ON <表>` の直後だけを関係位置にし、`EXECUTE FUNCTION s.f(`
は関数名として解決する。`UPDATE OF`・`TRUNCATE ON` の語を関係位置と誤検出しない。

## ライフサイクルとの連動（同一 write txn）

| 操作 | 動作 |
| ---- | ---- |
| `DROP TABLE`（単独・複数・CASCADE・`DROP SCHEMA ... CASCADE` の表削除） | 全テナント分のトリガを削除（同名で再作成しても復活しない） |
| `ALTER TABLE ... RENAME TO` | 全テナント分を新しい表キーへ移す |
| `DROP FUNCTION` | その関数を実行するトリガがあれば `2BP01`（固定文言。トリガ名・表を含めない） |
| `DROP SCHEMA ... CASCADE` | 配下の関数を実行するトリガ（`public` 表のものを含む）を、関数を消す前に削除 |
| `UPDATE OF` が参照する列の `DROP COLUMN` | `2BP01` |
| 同列の `RENAME COLUMN`・`ALTER COLUMN TYPE` | `0A000`（追随が入るまでの暫定ガード） |

## 既知の制限

- DDL 権限は全テナント共有のカタログを扱う別軸の権限のため、`DROP COLUMN` の `2BP01` が
  別テナントのトリガの存在を 1 ビットだけ示しうる（固定文言でトリガ名・テナントは出さない）。
- `ALTER TABLE ... SET SCHEMA` との連動は、現状その実移動経路が無いため対象外。
