# FROM なし SELECT（定数式 SELECT）

- 対象: SQL-43（ポインタ表記のみ）・Issue #1531（親 #1530。wire 層の確認は #1532）
- 状態: engine 実装済み・wire（simple／extended）確認済み（Issue #1532）。列参照は Issue #2328 で `42703` 化

## 背景

ドライバ・ORM の接続確認クエリ（`SELECT 1` 等）は `FROM` を持たないため、従来は許可リストの
`FROM` 必須により `42601` で拒否されていた。

## 決定

| 項目 | 内容 |
| ---- | ---- |
| 分岐 | `validate_select_statement` の冒頭で、括弧の深さ 0 に `FROM` が無い `SELECT` を `Statement::ConstSelect` へ振り分ける。`FROM` ありの既存形は判定が偽で挙動不変 |
| 構文 | `SELECT <式> [AS <別名>] [, ...] [;]`。`*`・`WHERE`・`LIMIT` 等の余剰トークン・空リスト・末尾カンマは `42601` |
| 禁止 | 列参照は `42703`（Issue #2328。`FROM` なしでは全識別子が一律に未定義のため存在情報を示さない）。集計関数（`COUNT`／`SUM`／`AVG`／`MIN`／`MAX`。入れ子含む）は `42601`。メッセージはいずれも固定の英語で識別子名を含めない |
| 型 | 整数リテラルは `integer`→`bigint`→`numeric`、小数・指数は厳密 `numeric`、文字列は `text`。その他の式は既存の式束縛（`udf_call`）に委譲し結果型で `ColumnMeta::Computed` を決める。式（`1+1`）は `double precision` |
| 列名 | `AS` 別名、なければ関数名等の既定名、リテラルは `?column?` |
| 上限 | 項目数 1024（超過は `54000`）。式ノード数・深さ・文字列長は既存の上限を共有 |
| 実行 | ストレージ・スナップショット・RLS 述語に触れず 1 行を返す。Describe は評価せず束縛結果の列のみ返す。トランザクション内は dirty テーブルの有無に依らず直接実行 |
| 非対応 | `EXPLAIN`・`DECLARE CURSOR`・`COPY (...) TO`・`CREATE VIEW` の内側、集合演算は従来どおり拒否 |

## テナント境界

読むデータが無いため他テナントの情報に依存する経路が無い。実行は認証済み `PolicyContext` を要する
既存入口からのみ到達する。

## 対象外（後続）

`FROM` なしの `WHERE`／`LIMIT`／`HAVING`、`$n` パラメータ、裸の `NULL`／`TRUE`／`FALSE`、
`AS` なし別名、整数同士の算術を整数型で返すこと。

## wire 層の確認（Issue #1532）

wire-server は汎用経路のまま ConstSelect を通し、本番コードの変更は不要だった。
`crates/wire-server/tests/wire_sql43_from_less_select.rs`（層 A）で固定している。

| 項目 | 確認内容 |
| ---- | -------- |
| simple | `SELECT 1` は `?column?`・OID 23・typlen 4・text `1`・タグ `SELECT 1`。複数文・明示トランザクション（ReadyForQuery `T`）も通る |
| extended | Parse → Describe(S) → Sync と Bind（全列 binary）→ Describe(P) → Execute → Sync を確認。binary は int4 の 4 バイト。無名 statement の text 結果も通る |
| Describe 整合 | statement／portal／simple の列名・OID・typlen が一致（portal は format 欄のみ 1） |
| 拒否 | `SELECT *`・`SELECT $1` は `42601`、列参照は `42703`。extended は Sync で回復する |

`SELECT $1` を `42P18` で返すかは、ERR-6 への行追加が前提のためオーナー判断待ち（現状は既存の `42601` を維持）。

対象外: `FROM` ありの未定義列（`22000`）は変更しない。裸の `TRUE`／`FALSE`／`NULL` は値位置で識別子として扱われるため、本変更で `SELECT true`・`SELECT NULL` は `42601` から `42703` に変わる（受理は後続）。
