# GROUP BY の ROLLUP・CUBE・GROUPING SETS の構文解析と展開（Issue #1821・SQL-59・親 #1820）

## ステータス

Accepted・構文段と展開（Issue #1821）に加え、実行段（セットごとの集約と結果の合成。Issue #1822）と `GROUPING()` 関数（Issue #1823）、他機能との併用・上限境界の回帰テスト（Issue #1824）も実装済み。

## ポインタ

- spec: `docs/spec/04-behavior/sql-surface.md` SQL-59（期待値は PostgreSQL 17 と同じ）。
  エラー分類は `docs/spec/04-behavior/error-format.md` の `54001`・`54011` の行
- 前提: [multi-column-group-by.md](multi-column-group-by.md)・[order-by-having-expressions.md](order-by-having-expressions.md)（式キー・構造一致。Issue #1817・#1818）
- 後続: セットごとの集約の実行と結果の合成（Issue #1822）、`GROUPING()` 関数（Issue #1823）、
  他機能との併用の検証（Issue #1824。上記「Issue #1824 追記」）。`54023` の行は #1823

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

### 実行前の関門（Issue #1821 の暫定。Issue #1822 で撤去）

Issue #1821 の時点では、実行段が無いまま通常の `GROUP BY` として実行すると誤った行を黙って返すため、
束縛段の入口で `0A000` により拒否していた。実行段の実装（Issue #1822）で、この関門は撤去した。
判定順は構文エラー（`42601`）→ `54011` → `54001` → `54000`（実行時のグループ数上限）の順。

### 実行方式と結果の合成（Issue #1822 追記）

- **1 パス・セットごとの集計表**: 可視性判定・本体デコード・`WHERE`・可視性の再検査を通った行 1 件ごとに、
  和集合キー（式キーを含む）を 1 回だけ評価し、セットごとに「セットに含まれないキー成分を NULL にした
  キー」で専用の集計表（`BTreeMap`）へ累積する。複数パス方式はセット数（最大 4,096）に比例して全表走査を
  繰り返すため採らない。末端粒度で集約してから畳み込む方式も、分解できない集計（`COUNT(DISTINCT)`・
  `array_agg`・`string_agg` 等）に使えないため採らない
- **NULL の出力**: セットに含まれないキー列は NULL として評価・出力する（HAVING・ORDER BY・投影式の
  行ビューも同じ）。データ上の NULL と小計・総計の NULL は別の行として出る
- **重複セット**: 同内容のセットも重複排除せず、PostgreSQL と同じく行が重複して出る
- **空のセット（総計）**: 走査前に事前作成する。入力 0 行でも、空のセット 1 個につき 1 行
  （`count(*)` は 0・他の集計は NULL）を返す。可視行に依存しない行で、他テナントの情報は含まない。
  `GROUP BY ()` は「キー 0 個のグループ化あり」として同じ経路で実行する
- **上限**: グループ数上限（既定 10,000）・結果行数上限・グループキー累計・TEXT 集計状態・
  `COUNT(DISTINCT)` の予算は、すべて全セット合計で検査し、超過は `54000`（fail-closed）
- **並び順**: `ORDER BY` なしはセット宣言順 → キー昇順。`ORDER BY` ありは従来の比較の後にセット番号で
  タイブレークする。結果行の順序は契約にしない（決定的だが保証はしない）。セット番号は終端処理まで
  保持し、`GROUPING()`（Issue #1823）が参照できるようにしている
- **索引経路を使わない**: 単一 `TEXT` キーの `ROLLUP(lang)` でも、索引の列挙形・候補走査形へは流さず
  全走査する（`single_text_key_column` が `None` を返す。EXPLAIN の分類も共有）
- **fail-closed の維持**: ビュー本体・ビュー外側集計・`GroupedRowAccumulator`（評価後射影形ビュー）は
  `0A000`（または構文段の `42601`）のまま。通常の `GROUP BY` として誤った行を返す経路は作らない

## Issue #1823 追記: `GROUPING()` 関数

エラー分類は `docs/spec/04-behavior/error-format.md` の `54023` の行（SQL-59 (1)）。

- **方式（擬似集計）**: `GROUPING` を集計関数名として扱い（`AggregateFunc::Grouping`）、既存の集計呼び出しの
  仕組み（SELECT・`HAVING`・`ORDER BY`・式項目での内部名への脱糖と重複排除、参照列の公開範囲検査）に乗せる。
  累積器（`Accumulator::Grouping`）は行を観測せず状態を持たない。値は `finish_groups` が `finish` の直後
  （従来形の `HAVING`・式の `HAVING`／`ORDER BY`・投影より前）に、セット番号とセットごとのキー所属マスクから
  計算して上書きする。値を NULL かどうかから推定しない（データ上の NULL と小計の NULL を区別するため）
- **ビット規則**: 引数 i は、そのキーが現在のセットに含まれないとき 1。最初の引数が最上位ビット
  （`n` 個なら i 番目は `1 << (n - 1 - i)`）。結果型は `integer`（int4）で、既定の列名は `grouping`。
  通常の `GROUP BY`（グルーピングセットなし）は常に 0。同じキーの重複引数は引数ごとにビットを持つ
- **引数**: `GROUP BY` のキー（列名、または構造一致するキー式。全セットの和集合）に限る。キーでない既存の列・
  キーと一致しない式・`GROUP BY` なし・`GROUP BY ()` は `42803`、未知の列は `42703`
- **上限と判定順**: 引数は 31 個まで。32 個目へ進む時点で確保・束縛の前に `54023`（`ErrorClass::TooManyArguments`。
  HTTP 400）。構文エラー（`42601`）→ `54023` → 既存の `54011`・`54001` → 束縛段の `42703`・`42803` の順
- **構文**: 引数 0 個・`*`・`DISTINCT`・`FILTER`・内側 `ORDER BY` は `42601`。入れ子の集計・`GROUPING` も
  既存規約どおり `42601`。`WHERE` など集計を置けない位置は既存の拒否のまま
- **fail-closed**: FROM なしは `42803`、JOIN 集計は `0A000`、ビュー（本体・外側集計）は `0A000`、
  NoSQL の直接構築入口（`BoundAggregateItem::bind`）は `42601`。CTE の主クエリ・集合演算の枝・
  サブクエリは従来どおり集計式項目を許さず `42601`
- **索引経路**: `GROUPING()` は行を観測しないため、単一 `TEXT` キーの索引の列挙形・候補走査形でも
  `COUNT(*)` のみの近道を妨げない（値は常に 0）
- **互換性**: `grouping` は集計関数名になり、`CREATE FUNCTION grouping(...)` は予約名として拒否される
  （PostgreSQL でも `GROUPING` は関数名に使えない）。`AggregateFunc`・`ErrorClass` の variant 追加は
  公開 enum の破壊的変更
- **検証**: PostgreSQL 17 の公開仕様から導出した固定値で検証した（実機との照合は未実施）。
  `crates/engine/tests/sql59_grouping_function.rs`

## Issue #1824 追記: 他機能との併用と上限境界の固定

挙動変更なし（テストのみ）。`crates/engine/tests/sql59_grouping_sets_combos.rs`（期待値は PostgreSQL 17 の
公開仕様から導出した固定値。実機との照合は未実施）。

- **`FILTER`**: 述語はセット射影後の NULL ではなく入力行で評価する（総計行でも元の行を数える）。
  `GROUPING()` との併用（`GROUPING()` は `HAVING`、`FILTER` は選択リスト）も固定
- **集計引数の `DISTINCT`・非分解集計**: `count(DISTINCT ...)`・`array_agg(DISTINCT ... ORDER BY ...)`・
  `array_agg(... ORDER BY ...)` はセットごとに累積され、総計は小計の和ではない。`COUNT(DISTINCT)` の
  中間状態予算（異なり値 10 万件）は全セット合計で計上する（4,000 セット × 25 値で成功、26 値で `54000`）
- **`HAVING`・`ORDER BY ... NULLS`・`LIMIT`・`OFFSET`・`FETCH FIRST ... WITH TIES`**: 全セットの合成後に適用する。
  データ上の NULL と小計の NULL は `lang` では同値のため、区別は `GROUPING()` で行う（`HAVING` は `IS NULL` 非対応）
- **上限境界**: 結果行数上限は全セット合計（`HAVING`・`OFFSET` 適用後、`WITH TIES` の同値追加後）で判定し、
  超過は `54000` で `Err` のみ（部分結果を返さない）。グループ数上限も `FILTER`・`DISTINCT` 併用時に全セット合計
- **fail-closed のまま**: 窓関数との併用・`SELECT DISTINCT` と `GROUP BY` の併用・JOIN 集計・集合演算の枝は
  いずれも `42601`（誤った行を返さない）。サポート追加は本記録の対象外
- **RLS**: 併用時も他テナントの Private 行の値はどのセットにも現れない

## 対象外

- 窓関数・`SELECT DISTINCT`・JOIN 集計との併用のサポート（現状 `42601` で fail-closed）
- `GROUP BY DISTINCT`・`GROUP BY ALL`、トップレベルの複合 `GROUP BY (a, b)` の平坦化（`42601` のまま）
- 上限（4,096・12・8 段）の設定化
- NoSQL の `aggregate` は対象外（`42601` のまま）
