# `array_agg`（集計内 `ORDER BY`・`DISTINCT` 対応）（Issue #1801）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: SQL-57・SQL-13・SQL-14・SQL-25 (c)・
  SQL-56・TABLE-35・WIRE-13・RLS-7・RLS-8
- 前提: [string-agg.md](string-agg.md)（集計内 `ORDER BY`・`DISTINCT`・状態モジュールの仕組み。本 Issue はこれを再利用する）・
  [error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)（公開 enum の方針）
- 検証コード: `crates/engine/src/sql/array_agg.rs`・`allowlist.rs`・`parser.rs`・`array_expr.rs`・`aggregate.rs`・
  `group_by.rs`、`crates/engine/tests/sql57_array_agg.rs`

## 背景・目的

`array_agg(x)` が無く `42883` になっていた。`string_agg`（#1800）で入った集計内 `ORDER BY`
（`AggregateItem.order_by`）・`DISTINCT` の `42P10`・データを溜めて安定ソートする状態モジュールを再利用し、
入力の型の 1 次元配列を返す `array_agg([DISTINCT] <式> [ORDER BY ...])` を追加する。

## 決定

### D1: 構文

- `AggregateFunc::ArrayAgg` を追加（`array_agg` は集計関数名として予約され、同名の UDF は定義できない）
- 引数は 1 つ。`array_agg(a, b)` は `42883`、`array_agg()`・`array_agg(*)` は `42601`
- `DISTINCT` は `COUNT`・`string_agg`・`array_agg` が受け付ける。`array_agg` では `DISTINCT` の直後の
  数値リテラル・文字列リテラル・`(` も修飾子と読む。列名 `distinct` の形は壊さない
- 内側 `ORDER BY` は `string_agg` と `array_agg`。キー数の上限（8）の超過は確保前に `54000`

### D2: 束縛と要素型

- 引数が**裸の列**: 列の値を行の借用値からネイティブに複製する（`bigint` を f64 へ落とさず、2^53 を超える値も正確）

  | 列型 | 要素型 |
  | ---- | ------ |
  | text・varchar | text |
  | boolean・integer・bigint・real・double | 同名 |
  | date・timestamp・uuid | 同名 |
  | numeric(p,s) | numeric(p,s)（列の位取りのまま） |
  | bytea・json・jsonb | 同名 |
  | smallint・char(n)・time・timestamptz・interval・vector・配列・ENUM・疑似列 `id` | `0A000` |

  `char(n)` は PostgreSQL では bpchar 配列（空白込み）になるため text 配列へ黙って変換しない。ENUM は語彙の持ち回しを対象外にした。
- 引数が**式**: `ARRAY[<式>]` の 1 要素と同じ型規則（`array_expr::bind_single_array_element`。コンストラクタと
  共有の `unify_and_bind_items`）。数値・smallint・`character(n)`・入れ子の配列は `0A000`/`42804`
- 型を決められない引数（裸の `NULL`・型なし文字列リテラル）は `42804`（PostgreSQL の多相型の解決失敗と同じ分類）
- 内側 `ORDER BY` のキーは `string_agg` と共有の自由キー束縛（`bind_aggregate_free_order_key`）で、裸の列は広域取得と
  同じ比較規約・照合、疑似列 `id`、それ以外は式。引数と構文上等しいキーも同じ経路で再評価する
- `DISTINCT` は全キーが引数と構文上等しくなければ `42P10`。`ORDER BY` に引数が無ければ引数の昇順キーを合成する
- 比較できない型: `json`（原文保存）の `DISTINCT`／引数を指す `ORDER BY` は PostgreSQL と同じ `42883`、`bytea` は
  比較キーを持たないため `0A000`。通常の集約（並べ替えなし）は受理する

### D3: 意味論

- NULL の入力は NULL 要素として配列に含める。入力行が無ければ結果は NULL（空配列ではない）。全行 NULL は `{NULL,...}`
- 内側 `ORDER BY` が無ければ入力順（行の処理順）。同値キーは安定ソートで入力順を保つ。NULL の位置は SQL-56 に従う
- `DISTINCT` は要素値の同値で重複を除き、NULL は 1 個だけ残す。重複判定は要素の正準バイト列
  （先頭 1 バイトで NULL/非 NULL、浮動小数は `-0.0`・NaN を正規化）。既出の要素は状態・件数の予算を消費する前に捨てる
- 値・`ORDER BY` の式は値が NULL の行でも評価し、評価エラーは文全体のエラーとして伝播する

### D4: 実行と資源上限

- 状態は `Accumulator::ArrayAgg`（`sql/array_agg.rs`）。要素は `Option<Elem>`（`ArrayValue` の要素型 variant と 1 対 1）
- 上限（いずれも確保の前に `checked_*`・`try_reserve` で検査し、超過は `54000`。部分的な結果は返さず、他テナントの行の有無に依存しない。
  エラー文言は固定の英語で値・列名を含めない）:
  - 要素数 `MAX_ARRAY_ELEMENTS`（1,024）。`DISTINCT` では重複を除いた後の件数で判定する。起動時設定
    `--max-array-elements` を式の結果へ通すのは #1606 で対象外のため定数を使う
  - 可変長要素の本文の累計 4 MiB（配列コンストラクタ・配列 codec と同じ）
  - 溜めた状態 16 MiB（1 件あたりの固定見積りを含み、NULL 要素の大量蓄積も有界）
  - 状態の大きさは `Accumulator::text_len` で報告し、`GROUP BY` の全体予算（16 MiB）に計上される。単一行集計では
    `array_agg` を含む文の状態カウンタを 16 MiB に頭打ちにする。`DISTINCT` の重複判定キーは `distinct_footprint` 経由で文全体の予算に計上される
- `ReferencedColumns::derive`: 列経路は `BoundExpr` を通らないため列の mask を明示的に立てる（漏れると列のデコードが
  省かれ要素が黙って NULL になる。`aggregate-decode-skip.md` の契約）
- 索引経路: `ORDER BY` なしの `array_agg` は処理順が要素順になるため、`GROUP BY` のキー順の列挙形を使わない
  （`has_text_min_max_aggregate` を `string_agg` と共有）

### D5: 結果型

- 結果列型は `Array(elem)`（`aggregate_result_type`）で、WIRE-13 の配列 OID（`int4[]` など）で公告される。出力は既存の配列テキスト形式
- 式中の集計列型は式で使えない扱い（`array_agg(x) || ...` などの式項目は拒否）

### D6: 文レベルの比較の穴を塞ぐ

`group_by::cmp_cell_values` は未定義の組を等値として扱うため、`ORDER BY <array_agg の別名>` を通すと並びが黙って崩れる。
`resolve_group_reference` で配列結果の集計項目を指す参照（`ORDER BY`・`HAVING`）を束縛時に `0A000` で拒否する。

### D7: 対象外の経路は fail-closed

| 経路 | 結果 |
| ---- | ---- |
| FROM なし・JOIN | `0A000` |
| ウィンドウ `OVER` | `42601` |
| NoSQL（`BoundAggregateItem::bind`） | `42601` |
| ビュー本体・評価後射影形ビューの外側 | 拒否 |

ビュー越しでは値・内側 `ORDER BY` の式が公開列に限定され、別名は物理列へ写る（`string_agg` と同じ）。

## PostgreSQL との差分

- 型を決められない引数・未対応の要素型は text 配列へ倒さず拒否する（`42804`/`0A000`）
- 配列結果を式の部品・`ORDER BY`・`HAVING` に使う形は未対応
- 集計の入れ子の `42803`・引数中のウィンドウ関数の `42P20` は現状 `42601`

## 契約変更（BREAKING）

- `AggregateFunc` に `ArrayAgg` variant を追加（`non_exhaustive` ではない公開 enum）
- `array_agg` が予約語になり、同名の `CREATE FUNCTION` は拒否される

## 対象外（追跡）

- FROM のない `array_agg`、JOIN、ウィンドウ（SQL-64）、NoSQL 表層、`FILTER (WHERE ...)`
- ビュー本体・評価後射影形ビューの外側での `array_agg`
- 配列を入力にした多次元の結果、ENUM・smallint・char(n)・time・timestamptz・interval の要素
- `array_agg(...)[i]` などの式・HAVING・文レベル ORDER BY で配列結果を使う形
- 要素数の上限の起動時設定（SQL-46 の範囲。#1606 の申し送りと同じ）
