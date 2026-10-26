# GROUP BY の ROLLUP・CUBE・GROUPING SETS の構文解析と展開（Issue #1821・SQL-59・親 #1820）

## ステータス

Accepted・構文段と展開は実装済み。実行段は未実装（Issue #1822）で、それまでは `0A000` で拒否する。

## ポインタ

- spec: `docs/spec/04-behavior/sql-surface.md` SQL-59（期待値は PostgreSQL 17 と同じ）。
  エラー分類は `docs/spec/04-behavior/error-format.md` の `54001`・`54011` の行
- 前提: [multi-column-group-by.md](multi-column-group-by.md)・[order-by-having-expressions.md](order-by-having-expressions.md)（式キー・構造一致。Issue #1817・#1818）
- 後続: セットごとの集約の実行と結果の合成（Issue #1822）、`GROUPING()` 関数（Issue #1823）、
  他機能との併用の検証（Issue #1824）

## 目的

`GROUP BY` の `ROLLUP(...)`・`CUBE(...)`・`GROUPING SETS (...)` と、それらの複合指定
（`a, ROLLUP(b, c)` 等）を構文解析し、内部表現「グルーピングセットの列」へ正規化する。組合せの爆発は
展開（確保）より前に上限で拒否する（fail-closed）。

## 設計

### 受理する文法

トップレベルの集計 `SELECT` に限って受理する。`ROLLUP`・`CUBE`・`GROUPING`・`SETS` は予約語化せず、
次の並びのときだけ構文として扱う（PR #189 の方針）: `ROLLUP (`・`CUBE (`・`GROUPING SETS (`・`( )`。
`GROUP BY rollup` は従来どおり列キー。

- 要素のキーは既存の経路（`group_key_range_end`・`precheck_group_key_tokens`・式パーサ）で読む。
  集計は `42803`、ウィンドウは `42P20`、`$n`・部分クエリ・位置指定は `42601`。
- `ROLLUP`・`CUBE` の要素は単一キーまたは複合要素 `(a, b)`。内側に別のグルーピング構文を置くと `42601`。
- `GROUPING SETS` の要素は単一キー・複合要素・`()`・入れ子のグルーピング構文（展開時に平坦化）。
- `42601` で拒否する形: 空の `ROLLUP()`・`CUBE()`・`GROUPING SETS ()`、トップレベルの複合
  `GROUP BY (a, b)`（従来どおり）、`GROUP BY DISTINCT`、トップレベル以外の文脈
  （CTE 主クエリ・集合演算の枝・サブクエリ・ビュー本体）での構文。

### 内部表現

`sql::grouping_sets`（`pub(crate)`）が純粋な展開ロジックを持つ。構文段はキーの出現を順にプールへ積み、
項目の木（`GroupingElement`）にはプール添字だけを持たせる。`GroupByClause` に
`grouping_sets: Option<Vec<Vec<usize>>>` を追加した。

- `columns`・`key_exprs` は全セットに現れるキーの**和集合**（初出順・一意）にする。これにより
  構造一致・HAVING・ORDER BY の名前解決・`MAX_GROUP_BY_COLUMNS`・ビューの公開列検査が変更なしで
  和集合に効く。
- `grouping_sets` の各セットは `columns` の添字の昇順・重複なし。空のセットは総計。`None` は
  通常の単一セット `GROUP BY`（構文を含まない従来形の挙動は変えない）。

### 展開の規則

- トップレベル: 各項目のセット列の直積（左の項目が外側のループ）。各組の和集合が 1 セット。
- `ROLLUP(e1..en)`: 長い接頭辞から `(e1..en), ..., (e1), ()` の n+1 セット。
- `CUBE(e1..en)`: 冪集合で 2^n セット。第 1 要素を最上位ビットとするマスクを全 1 から 0 へ降順にたどる
  （`CUBE(a,b,c)` は `(a,b,c),(a,b),(a,c),(a),(b,c),(b),(c),()`）。
- `GROUPING SETS`: 各要素の展開を出現順に連結する。
- 重複: グルーピング構文を含む句ではキーの重複参照を受理し（PostgreSQL と同じ）一意化する。展開後に
  同じセットが複数できた場合は保持する（行の重複の合成は Issue #1822）。構文を含まない従来の
  `GROUP BY a, a` は `42601` のまま。
- 出力順は上記の宣言順で決定的。結果行の順序は契約にしない。

### 上限と判定順

確保の前に次の順で判定する。いずれも文の形だけで決まり、行・テナントに依存しない。

| 順 | 判定 | タイミング | コード |
| -- | ---- | ---------- | ------ |
| 1 | `CUBE` の要素数が 12 を超える | 構文解析中。13 個目を読んだ時点で中断 | `54011` |
| 2 | `GROUPING SETS` の入れ子が 8 段を超える | 構文解析中。再帰に入る前 | `54001` |
| 3 | 展開後のセット数が 4,096 を超える | 木の形から飽和演算で数える。具体化より前 | `54001` |
| 4 | 一意キー数が 8 を超える | キーの一意化時（`check_group_by_column_count`） | `54000` |
| 5 | 具体化 | 検証済みの総数（4,096 以下）で確保 | — |

`54001` は `ErrorClass::StatementTooComplex`、`54011` は `ErrorClass::TooManyColumns` として追加した
（`CatalogError::TooManyColumns` の `54000` とは別）。HTTP 射影は 400。

### 実行前の関門（Issue #1822 までの暫定）

実行段が無い間に通常の `GROUP BY` として実行すると誤った行を黙って返すため、
`sql::parser::bind_aggregate_with_dummy_flags` の入口で `grouping_sets` が `Some` なら
`0A000`（`GROUPING SETS, ROLLUP and CUBE are not supported yet`）で拒否する。キー 0 個の
`GROUP BY ()` を「`GROUP BY` なしの `HAVING`」と取り違えないよう、その判定より前に置く。
ビュー本体の検査（`check_body_aggregate`・評価後射影形ビューの外側集計）にも `grouping_sets` の
判定を足した（多層防御）。判定順は構文エラー（`42601`）→ `54011` → `54001` → `54000` → `0A000`。

## 対象外

- セットごとの集約の実行・結果の合成・小計行の NULL・展開後のグループ合計の上限（Issue #1822）
- `GROUPING()` 関数と `54023`（Issue #1823）
- FILTER・DISTINCT・HAVING・ORDER BY・ウィンドウとの併用の検証（Issue #1824）
- `GROUP BY DISTINCT`・`GROUP BY ALL`、トップレベルの複合 `GROUP BY (a, b)` の平坦化（`42601` のまま）
- 上限（4,096・12・8 段）の設定化
- NoSQL の `aggregate` は対象外（`42601` のまま）
