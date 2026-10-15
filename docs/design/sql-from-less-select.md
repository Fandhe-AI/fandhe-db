# FROM なし SELECT（定数式 SELECT）

- 対象: SQL-43（ポインタ表記のみ）・Issue #1531（親 #1530。wire 層の確認は #1532）・Issue #2329（`WHERE`／`LIMIT`／`OFFSET`）
- 状態: engine 実装済み・wire（simple／extended）確認済み（Issue #1532）。列参照は Issue #2328 で `42703` 化

## 背景

ドライバ・ORM の接続確認クエリ（`SELECT 1` 等）は `FROM` を持たないため、従来は許可リストの
`FROM` 必須により `42601` で拒否されていた。

## 決定

| 項目 | 内容 |
| ---- | ---- |
| 分岐 | `validate_select_statement` の冒頭で、括弧の深さ 0 に `FROM` が無い `SELECT` を `Statement::ConstSelect` へ振り分ける。`FROM` ありの既存形は判定が偽で挙動不変 |
| 構文 | `SELECT <式> [AS <別名>] [, ...] [WHERE <条件>] [HAVING <条件>] [LIMIT <n>] [OFFSET <m>] [;]`（句の順序は固定）。`*`・上記以外の余剰トークン・空リスト・末尾カンマは `42601` |
| 禁止 | 列参照は `42703`（Issue #2328。`FROM` なしでは全識別子が一律に未定義のため存在情報を示さない）。集計を含む式・入れ子の集計関数は `42601`（トップレベルの集計項目は Issue #2330 の行を参照）。メッセージはいずれも固定の英語で識別子名を含めない |
| 型 | 整数リテラルは `integer`→`bigint`→`numeric`、小数・指数は厳密 `numeric`、文字列は `text`。その他の式は既存の式束縛（`udf_call`）に委譲し結果型で `ColumnMeta::Computed` を決める。整数同士の算術は `integer`／`bigint`（Issue #1539）、小数が混ざると `double precision` |
| 列名 | `AS` 別名、なければ関数名等の既定名、リテラルは `?column?` |
| 上限 | 項目数 1024（超過は `54000`）。式ノード数・深さ・文字列長は既存の上限を共有 |
| 実行 | ストレージ・スナップショット・RLS 述語に触れず 1 行を返す。Describe は評価せず束縛結果の列のみ返す。トランザクション内は dirty テーブルの有無に依らず直接実行 |
| WHERE（#2329） | ダミー入力 1 行への絞り込み。`TRUE`／`FALSE`／`NULL` は構文段でリテラルに畳み（直後が文末・`;`・`HAVING`・`LIMIT`・`OFFSET` のときのみ）、他は値式＋比較演算子 1 つを既存の定数式束縛・評価器で評価する。偽・`NULL` は 0 行、真は 1 行。真偽型でなければ `42804`。列参照は `42703`・`WHERE` 内の集計は `42601`（従来どおり） |
| LIMIT／OFFSET（#2329） | `['-'] <整数リテラル>` のみ（小数・`ALL`・`NULL`・式・`$n`・`i64` 超は `42601`）。結果は最大 1 行なので `OFFSET > 0` か `LIMIT = 0` で 0 行。負値は実行時に `22000`（`LIMIT must not be negative`／`OFFSET must not be negative` の固定英語文言。Describe は成功し Execute で失敗）。`-0` は 0。`--max-result-rows` 上限は適用しない（結果が最大 1 行のため） |
| 評価順（#2329） | 束縛（禁止検査・型検査）→ 負値検査 → WHERE 評価 → 投影を全項目評価 → 行選択。`SELECT 1/0 WHERE false` は `22012`（絞り込み結果にエラー有無が左右されない） |
| 集計（#2330） | トップレベルの `count(*)`／`count([DISTINCT] 式)`／`sum`／`avg`／`min`／`max`（`GROUP BY` なし）を受理。入力はダミー 1 行を `WHERE` で絞った 0 か 1 行で、集計を含む文は常に 1 行を返し、`LIMIT`／`OFFSET` を出力行に適用する。`FROM` あり側の集計実行（`sql::aggregate`）には依存せず直接評価する。`count` は入力行数（式は非 NULL の行数）、他は入力 0 行・引数 NULL で `NULL`。引数は `WHERE` の結果に関わらず評価する。結果型は PG 準拠（`count`→`bigint`、`sum(int4)`→`bigint`、`sum(int8)`→`numeric`、`sum(numeric)`→`numeric`、`sum(real)`→`real`、`sum(double)`→`double`、`avg(整数)`／`avg(numeric)`→`numeric`、`avg(実数)`→`double`、`min`／`max`→引数型）。非対応の引数型は暗黙変換せず `42883`。列名は別名、なければ関数名。対象外は集計を含む式（`count(*) + 1`）・`WHERE` 内の集計・入れ子集計・集合演算の枝・スカラーサブクエリ内（いずれも `42601`）。既知の差分: `FROM` あり側は `avg(int)`→`double`・`sum(int8)`→`bigint` のままで（TASK-330 のポインタ）型が一時的に食い違う。`avg(整数)` の numeric は小数 16 桁（PG は 20 桁） |
| HAVING（#2331） | `GROUP BY` なしの 1 グループへの条件。`WHERE` の後・`LIMIT` の前に置き、条件の形は `WHERE` と同じ（`TRUE`／`FALSE`／`NULL` または値式＋比較演算子 1 つ）。条件には定数式と集計を含む式（`count(*) = 0`・`coalesce(max(1), 0) > 0`）を書け、集計は構文段で内部名の識別子へ置換して収集する（件数は既存の上限で有界、超過は `54000`）。束縛は内部名だけを許す禁止検査（他の識別子は固定文言の `42703`）→ 集計の束縛 → 内部名を列名・集計結果型を列型とする合成スキーマでの型付け（真偽型でなければ `42804`）。`HAVING` があれば集計項目が無くても集計問い合わせとして 1 グループを扱い、`SELECT 1 WHERE false HAVING true` は 1 行。偽・`NULL` は 0 行。評価順は WHERE → 投影 → HAVING の集計引数 → HAVING 条件 → 行選択で、`SELECT 1/0 HAVING false` は `22012`。結果型が `numeric` の集計（`sum(bigint)`・`sum(numeric)`・`avg`・`numeric` 引数の `min`／`max`）は HAVING では `42601` |
| 非対応 | `EXPLAIN`・`DECLARE CURSOR`・`COPY (...) TO`・`CREATE VIEW` の内側、集合演算は従来どおり拒否 |

## テナント境界

読むデータが無いため他テナントの情報に依存する経路が無い。実行は認証済み `PolicyContext` を要する
既存入口からのみ到達する。

## 対象外（後続）

`FROM` なしの `GROUP BY`／`ORDER BY`、`WHERE`／`HAVING` 内の `AND`／`OR`／`NOT`／`IS`、`numeric` 結果の集計の `HAVING` での使用、
`OFFSET` を `LIMIT` より前に置く形、`LIMIT ALL`／`NULL`／式／`$n`、`WHERE 'true'` の暗黙の
真偽変換、`$n` パラメータ、`AS` なし別名。いずれも `42601`／`42804` のまま。

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
