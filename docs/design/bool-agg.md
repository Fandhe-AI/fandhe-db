# `bool_and` / `bool_or` / `every`（Issue #1805）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: SQL-57・SQL-13・SQL-14・RLS-7・RLS-8・WIRE-13
- 前提: [array-agg.md](array-agg.md)・[string-agg.md](string-agg.md)（拒否経路の分類を踏襲する）・
  [error-enum-non-exhaustive-policy.md](error-enum-non-exhaustive-policy.md)（公開 enum の方針）
- 検証コード: `crates/engine/src/sql/allowlist.rs`・`parser.rs`・`aggregate.rs`・`group_by.rs`・`const_select.rs`、
  `crates/engine/tests/sql57_bool_agg.rs`

## 背景・目的

真偽値集計が無く、関数名の段階で `42601` になっていた。比較式を引数に書く形（`bool_and(n > 1)`）も、
集計引数の構文が比較演算子を読まないため失敗していた。PostgreSQL 17 と同じ意味論で 3 関数を
単一表集計（GROUP BY の有無を問わない）へ追加する。

## 決定

### D1: 意味論

- `bool_and`・`every`: 全入力が真なら真（`every` は SQL 標準名で同一意味論。既定列名だけが `every`）
- `bool_or`: どれか 1 行が真なら真
- NULL 入力は無視。全行 NULL・入力 0 行は NULL。結果型は boolean（wire OID 16）
- 状態は `Option<bool>` の固定長。行数に比例する確保はしない

### D2: 構文と束縛

- `AggregateFunc::{BoolAnd, BoolOr, Every}` を追加（3 名は集計関数名として予約され、同名の UDF は定義できない）
- 引数は 1 つ。2 引数以上は `42883`、`*`・空引数・`DISTINCT`・集計内 `ORDER BY`・`FILTER`・`OVER` は `42601`
- 受理する引数は boolean 列の裸参照（既存の `AggregateInput::BooleanColumn` を再利用）と、比較式
  `<値式> <比較演算子> <値式>`（`AggregateInput::BoolExpr`。`bind_expr` が boolean 型に束縛した式）
- 比較式の受理は bool 系の関数に限った局所拡張（`parse_value_expr` の直後が比較演算子のときだけ右辺を読む）。
  他の集計関数の引数文法は変えず、集計引数の式の一般化（#1807 系）とは独立している
- 非 boolean の列・式・疑似列 `id` は `42883`、未知の列は `42703`（汎用の束縛へ落とすと、束縛に成功した後に
  実行時の内部エラーになるため、専用の解決関数で受け口を絞る）
- 値式中の `true` は識別子として読まれるため `bool_and(flag = true)` は `42703`。慣用形の `bool_and(flag)` を正とする

### D3: 実行

- `Accumulator::BoolAnd(Option<bool>)`・`BoolOr(Option<bool>)`。`every` は `BoolAnd` を共有する
- `COUNT(<boolean 列>)` は従来どおり非 NULL 件数（`observe_bool` の `Count` 腕）
- 比較式の参照列は `ScalarExpr` と同じく `ReferencedColumns` へ反映する（漏れると縮小デコードした行で
  列が NULL と取り違えられ結果が黙って誤る）
- GROUP BY の結果の並べ替えは `Cell::Bool` の比較（false < true）を追加した

### D4: 未対応形の拒否（fail-closed）

| 形 | 扱い |
| -- | ---- |
| FROM なし・JOIN | `0A000` |
| ビュー本体・評価後射影形ビューの外側 | 拒否（`42601` 系） |
| ウィンドウ（`OVER`） | `42601` |
| NoSQL の直接構築 | `42601` |
| HAVING の従来形（数値比較）で bool 系の結果を指す | `22000` |

他テナントの行の有無は結果にもエラーの種類にも影響しない（可視行に対するアキュムレータのみを追加した）。

## 対象外（追跡）

- `FILTER (WHERE ...)`: #1806
- 論理演算・述語形の引数（`AND`/`OR`/`NOT`/`IS NULL`/`IN`/`BETWEEN`/`LIKE`、boolean リテラルとの比較）: #1807 系
- `DISTINCT`・集計内 `ORDER BY`（結果は不変だが受理しない）
- HAVING で bool の結果を直接述語にする形（`HAVING bool_and(x)`）
- FROM なし・JOIN・ウィンドウ・ビュー本体・NoSQL HTTP 表層の bool 集計

## 契約変更（BREAKING）

`AggregateFunc` に 3 variant を追加し（`#[non_exhaustive]` ではない公開 enum）、`bool_and`・`bool_or`・`every` を
予約名にした。
