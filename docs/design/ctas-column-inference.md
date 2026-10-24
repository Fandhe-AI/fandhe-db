# CREATE TABLE AS（SQL-52）の列型推論・構文・実行（Issue #1677・#1678）

`CREATE TABLE ... AS SELECT` の表定義を、SELECT の出力列から導く部品の設計記録。
ビヘイビアはポインタ表記のみ: SQL-52 (1)・(5)（`docs/spec/04-behavior/sql-surface.md`）。関連 TABLE-1・TABLE-23。

## 範囲

- 本 Issue: 純粋関数（`sql::ctas`）と Describe を使うヘルパ（`core::ctas_exec`）、単体・結合テスト。
- 対象外（#1678・#1679）: 構文受理・DDL 権限ゲート後の実行本体・作成と行投入の原子化・LIMIT 省略・
  TEMP・`WITH [NO] DATA`・台帳と上限。ヘルパは #1678 の実行腕が DDL 権限ゲートの後に呼ぶ前提。

## 判断

| 論点 | 判断 |
| ---- | ---- |
| `id` 列 | `ColumnMeta::Id`／`AliasedId` は通常の列（型は numeric(20,0)。wire の Describe の公告と一致）。表は採番方式で PK なし |
| 名前のない式 | 列名リストで名付けない限り `42601`（固定文言。識別子規則は緩めない）。SQL-52 (1) との差分 |
| 静的な型なし | `0A000`（CAST を促す） |
| 型の修飾 | 精度・長さ・次元・配列・ENUM をそのまま保つ。ENUM は `build_table_schema` が名前で再解決する |
| VECTOR | 非 nullable（エンジンの不変条件。PG との差分）。2 本以上は `42601` |
| 照合順序 | `ColumnMeta` が持たないため `None`（既知の制限） |
| 文字列リテラルの投影 | 現行の SELECT 構文が受理しないため、本 Issue では推論対象にならない |

判定順序（決定的）: 出力 0 本 `0A000` → 列数のハード上限 `54000` → 列名リスト超過 `42601` →
列ごと（不正名 `42601`・重複 `42701`・型なし `0A000`）→ VECTOR 本数 `42601` → `build_table_schema`
（`--max-table-columns` の `54000`・ENUM 解決 `42704` など）。エラー文言は英語の固定文言で、
含めるのはクライアント自身が書いた重複列名だけ。

## セキュリティ

Describe は行を評価せず RLS の経路に触れない。見えない元表は通常の SELECT と同じエラーになり、新しい存在オラクルを作らない。
列数はアロケーション前に上限で拒否する。

## 第 II 部 構文と原子的な作成＋投入（Issue #1678）

`CREATE TABLE [IF NOT EXISTS] <名前> [(<列名>, ...)] AS <問い合わせ> [WITH [NO] DATA] [USING OPERATION_ID '<id>']`
を受理し、表の作成と行の投入を 1 つの write txn で行う。ビヘイビアはポインタ表記のみ: SQL-52 (1)〜(5)
（関連: TABLE-23・TABLE-36・TABLE-44・SQL-23・SQL-31・SQL-51・RECOVER-1）。

### 実行の骨格

`matview_exec` の `CREATE MATERIALIZED VIEW` と同じ骨格。書き込みゲートを持つ write txn を読み取り源にも書き込み先にも使い、
作成・評価・投入を行って commit は 1 回だけ。途中で失敗したら commit 前に txn を破棄するので表は残らない（クラッシュ時は
redb の単一 commit が保証する）。CTAS が作る表は FK・DEFAULT・SEQUENCE を持たないため、`WriteTarget::InTxn` で DEFERRED の
検査が COMMIT へ先送りされる懸念は当たらない。

判定順（決定的）: 構文 `42601`（カタログ非参照）→ DDL 権限 `42501` → 作成先判定 → `USING OPERATION_ID` `0A000`
→ 同名の関係（`IF NOT EXISTS` は no-op、無指定は `42P07`）→ 内側の問い合わせの実カタログ検証（`42P01` など）
→ 台帳必須モードの `WITH DATA` `23502` → 書き込みゲート → 列定義の導出と検査 → 表の作成 → 評価・上限・投入 → 世代 bump → commit。

### 判断

| 論点 | 判断 |
| ---- | ---- |
| 存在オラクル | 構文段は内側の問い合わせを構造検証だけ行いトークン列を保持する。実カタログでの再検証は DDL 権限ゲートの後。権限の無い主体は常に `42501` |
| 内側の文種 | `INSERT ... SELECT` と共有する許可リスト（`require_row_query_statement`）。`EXPLAIN`・`SET`・`SHOW` は `42601`、`USING PLAN` は `0A000` |
| `WITH NO DATA` | 表だけ作り、内側の問い合わせは評価しない（列の導出には元表が要る） |
| 上限 | 評価は `cap+1` で超過を検出して `54000`。書き込み前に `--max-insert-rows` と INDEX-4 のバッチ上限を必ず通す。判定点の集約と境界テストは #1679 |
| `USING OPERATION_ID` | 構文は受理し、実行は `0A000`。台帳必須モードの `WITH DATA` は `23502`（台帳は #1679） |
| 配列列 | `WITH DATA` では評価の前に `0A000`（代入変換が未対応。0 行のときだけ成功する結果のぶれを避ける） |
| 照合順序 | 作る表は照合を引き継がないため、非バイト順の照合を持つ表を読む問い合わせは `0A000` |
| 一時表・明示トランザクション・`$n` | いずれも `0A000`（`CREATE TEMP TABLE ... AS` を含む） |
| 応答タグ | 投入した場合は `SELECT <n>`（自セッションの文脈で挿入した件数）。`WITH NO DATA`・no-op は `CREATE TABLE AS` |
| 破壊的変更 | `ParsedSql`・`SqlOutcome` に variant を追加 |

### 対象外

- `operation_id` の台帳・冪等な再送・入力側上限の判定点の集約（#1679。第 III 部で対応済み）
- 内側の SELECT で LIMIT を省略した場合の受理（現行の SELECT 検証器の範囲のまま）
- `CREATE TEMP TABLE ... AS`・照合順序の引き継ぎ・配列列の投入・名前のない式の既定列名

## 第 III 部 台帳・冪等な再送・上限（Issue #1679）

ポインタ: SQL-52 (4)・(2)、RECOVER-1・RECOVER-10・RECOVER-11、TABLE-36、SQL-34、INDEX-4。

### 判定順（`core/ctas_exec.rs`）

1. `operation_id` 指定時は、表の重複判定（`42P07`）より前に台帳を照合する。同じ文の再送は `23505`、別の文は `22023`
2. 同名の関係があれば `42P07`。`IF NOT EXISTS` かつ `operation_id` 省略なら no-op。`operation_id` 指定ありなら no-op でも台帳へ記録する（後の再送が別内容で作り直す事故を防ぐ）
3. 内側の問い合わせの実カタログ検証 → 台帳必須モードの `WITH DATA` で `operation_id` 省略は `23502`
4. 書き込みゲート取得後に台帳を再照合する（ゲート待ちの間に同じ文が commit した場合も `42P07` でなく `23505`）
5. 表の作成 → （`WITH DATA`）評価 → 入力側上限 → 書き込み側上限（`--max-insert-rows`・INDEX-4）→ 投入 → 世代 bump → commit は 1 回だけ

### 台帳の記録

- 文ハッシュは `recovery::content_hash::for_create_table_as`（`OpTag::CreateTableAs = 26`）。材料は `USING OPERATION_ID` 句と末尾 `;` を除いた文のトークン列だけで、内側の結果行の値は含めない。文字の違いはすべて別の文として扱う（`IF NOT EXISTS`・明示の `WITH DATA`・列名リスト・識別子の大小）
- 記録は経路ごとにちょうど 1 回。1 行以上は `InsertSelectLedgerScope` で書き込み経路の記録ハッシュを文ハッシュに差し替える。0 行・`WITH NO DATA` は台帳だけを記録する。二重に記録すると自分自身に `23505` を返すため、入口を 1 つに絞る
- 台帳エントリは表の作成と同じ commit に入る。失敗すれば表も台帳も残らず、同じ `operation_id` で再試行できる
- `DROP TABLE` は表単位の台帳を消すため、DROP の後に同じ文を再送すると再作成される（既存の台帳の意味のとおり）
- `CompareOnlyWithoutLedger` は台帳に触れず、再送は `42P07`。台帳のキーはテナントを含むため、他テナントの台帳の有無は観測できない

### 上限

- 入力側の判定は `ctas_input_row_limit`／`check_ctas_input_rows` の 1 か所に集約した（算出は `INSERT ... SELECT`・matview と共有の `write_input_cap`〔`core/write_input_cap.rs`〕を呼ぶ。#2212）。値は `--max-result-rows` 由来
- 内側の `LIMIT` は `--max-result-rows` 以下に制限されるため、実体化後の入力側 `54000` は多重防御として残る。書き込み側は `check_insert_row_count_limit` と INDEX-4 のバッチ上限で、どちらも書き込みの前に判定する
