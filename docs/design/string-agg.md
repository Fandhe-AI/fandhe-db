# `string_agg`（集計内 `ORDER BY`・`DISTINCT` 対応）（Issue #1800）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: SQL-57 (1)(2)(4)(5)(6)・
  SQL-13・SQL-14・SQL-25 (c)・SQL-56・RLS-7・RLS-8
- 前提: [aggregate-order-by-and-scalar-group-keys.md](aggregate-order-by-and-scalar-group-keys.md)
  （文レベルの集計 `ORDER BY`・NULL 位置）・
  [error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)（公開 enum の方針）
- 検証コード: `crates/engine/src/sql/string_agg.rs`・`allowlist.rs`・`parser.rs`・`aggregate.rs`・
  `group_by.rs`、`crates/engine/tests/sql57_string_agg.rs`

## 背景・目的

集計関数は `COUNT`／`SUM`／`AVG`／`MIN`／`MAX` だけで、引数は 1 つ、`DISTINCT` は `COUNT` だけだった。
`string_agg(<値>, <区切り> [ORDER BY ...])` を追加し、PostgreSQL 17 と同じ意味論で連結する。
Issue 本文は「集計内 `ORDER BY` の既存経路へ接続する」としていたが、既存の `ORDER BY` はすべて
文レベル（結果行の並べ替え）で、集計の内側の並べ替えは存在しなかった。このため構文・束縛・比較器の
部品（`parse_order_nulls`・`bind_order_column_at`・`bind_order_expr`・`order_value`）を再利用して新設した。
後続の `array_agg`（#1801。[array-agg.md](array-agg.md)）が同じ仕組みを使えるよう、`AggregateItem` には汎用の拡張フィールド
（`extra_args`・`order_by`）を置いた。

## 決定

### D1: 構文

- `AggregateFunc::StringAgg` を追加（`string_agg` は集計関数名として予約され、同名の UDF は定義できない）
- 引数は 2 つ。区切りは**行ごとに評価する式**でもよい。引数の個数違いは `42883`、`string_agg(*)` は `42601`
- `DISTINCT` は `COUNT` と `string_agg` だけが受け付ける。内側 `ORDER BY` は `string_agg` だけ
  （`<式> [ASC|DESC] [NULLS FIRST|LAST]` の複数キー。キー数の上限は 8 で、超過は確保前に `54000`）。
  他の集計に広げない（`42601` のまま）
- 集計の内側の整数リテラルは定数式で、出力列の位置（序数）とは解釈しない
- ウィンドウ（`OVER`）・JOIN・FROM なし・NoSQL（`BoundAggregateItem::bind`）は対象外として拒否する
  （`OVER` は `42601`、JOIN・FROM なしは `0A000`、NoSQL の直接構築は `42601`）

### D2: 束縛と意味論

- 引数は text だけ（`VARCHAR`・`CHAR(n)` を含む。`CHAR(n)` は末尾の半角空白を除く）。bytea は対象外で `0A000`、
  それ以外は `42883`。裸の text 列は式束縛を経由せず列参照へ束縛するため、非バイト列順の照合の列でも値として使える
- 値が NULL の行は連結から除く（その行の区切りも使わない）。2 つめ以降の値の前にその行の区切りを置き、
  区切りが NULL の行は何も置かない。入力なし・全行 NULL は NULL。結果型は text
- 値・区切り・`ORDER BY` の式は値が NULL の行でも評価し、評価エラーは文全体のエラーとして伝播する
- 内側 `ORDER BY` が無ければ入力順（行の処理順）。同値のキーは安定ソートで入力順を保つ。NULL の位置は
  SQL-56 に従う（既定は ASC で末尾・DESC で先頭）。キーは値・区切りと構文上等しければその引数を再利用し、
  裸の列・疑似列 `id` は広域取得と同じ列比較（型ごとの比較規約・照合）、それ以外は式として束縛する
- `DISTINCT` の重複判定は引数の組（値, 区切り）全体（SQL-25 (c) と同じ同値）。`ORDER BY` が無い `DISTINCT` は
  PostgreSQL の並べ替えによる重複排除に合わせて（値, 区切り）の昇順（NULL は末尾）で出力する。
  `ORDER BY` の式が引数に現れない場合は `42P10`（`SqlSurfaceError::AggregateOrderByNotInArguments`）。
  `ORDER BY` のキーの後ろに未指定の引数を昇順キーとして足す

### D3: 実行と資源上限

- 状態は `Accumulator::StringAgg`（`sql/string_agg.rs`）。`ORDER BY`・`DISTINCT` が無ければ逐次連結、
  あれば行を溜めて `finish` で安定ソートする
- 上限（いずれも確保の前に検査し、超過は `54000`。部分的な結果は返さず、他テナントの行の有無に依存しない）:
  - 結果の長さ 4 MiB（`TEXT` 1 値の上限と同じ）。溜める形では値の長さの合計（結果長の下限）を観測の途中で、
    `finish` の連結は `checked_add`・`try_reserve` で検査する
  - 溜めた状態は 1 アキュムレータあたり 16 MiB（1 件あたり 64 バイトの固定見積りを含み、空文字列の大量蓄積も有界）
  - 状態の大きさは `Accumulator::text_len` で報告し、`GROUP BY` の全体予算（16 MiB）と結果予算に計上される。
    単一行集計では `string_agg` を観測する際、共有の状態カウンタ（`MIN`/`MAX(TEXT)` の保持分を含む）を 16 MiB に頭打ちにする（既定の 1 GiB は項目 32 個分の上限を許すため。専用カウンタは設けず、TEXT の `MIN`/`MAX` と併用した文では後者の実効予算も 16 MiB になる。fail-closed）
  - `DISTINCT` の重複判定キーは `distinct_footprint` 経由で文全体の予算（100,000 件・16 MiB。SQL-25）に計上される
- `ReferencedColumns::derive` は値・区切り・`ORDER BY` キーの参照列と embedding を反映する
- 索引経路: `ORDER BY` なしの `string_agg` は行の処理順が連結順になるため、`GROUP BY` のキー順の列挙形を使わない
  （`has_text_min_max_aggregate` を実行時と EXPLAIN が共有する判定として拡張）。候補走査形は物理行順を保つ

## 契約変更（BREAKING）

- `AggregateFunc` に `StringAgg` variant を追加（`non_exhaustive` ではない公開 enum）
- `SqlSurfaceError` に `AggregateOrderByNotInArguments` を追加
- `string_agg` が予約語になり、同名の `CREATE FUNCTION` は拒否される

## 対象外（追跡）

- FROM のない `string_agg`、JOIN の集計、ウィンドウの `string_agg`（SQL-64）、`string_agg(bytea, bytea)`、
  `FILTER (WHERE ...)`
- 集計の入れ子の `42803`・引数中のウィンドウ関数の `42P20` への是正（現状は `42601`）
- 上限の起動時設定（SQL-46 の範囲）
