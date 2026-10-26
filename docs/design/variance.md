# `variance` / `var_pop` / `var_samp`（Issue #1803）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: SQL-57・SQL-13・SQL-14・RLS-7・RLS-8・WIRE-13・TABLE-13
- 前提: [array-agg.md](array-agg.md)・[string-agg.md](string-agg.md)（SQL-57 の集計関数追加の型。対象外経路の扱いを踏襲する）
- 検証コード: `crates/engine/src/sql/variance.rs`・`numeric.rs`（`U256`・`select_div_scale_wide`・`div_round_wide`）・
  `allowlist.rs`・`parser.rs`・`aggregate.rs`・`const_select.rs`、`crates/engine/tests/sql57_variance.rs`・`sql57_stddev.rs`（#1804）

## 背景・目的

`variance(x)`・`var_pop(x)`・`var_samp(x)` が未実装で拒否されていた。PostgreSQL 17 と同じ意味論・戻り値型で、
GROUP BY の有無を問わず使えるようにする。後続の `stddev` 系（#1804）が分散の確定値に平方根をかぶせるだけで済むよう、
状態の更新（`observe`）と確定（`finish_decimal` / `finish_f64`）を分けて公開する。

## 決定

### D1: 構文

- `AggregateFunc::{Variance, VarPop, VarSamp}` を追加。`variance` は `var_samp` と同義。3 名は集計関数名として予約される
- 既定の結果列名は PostgreSQL と同じく関数名（`variance`／`var_pop`／`var_samp`）
- 列名 `variance` の括弧を伴わない参照は従来どおり列として解釈される（回帰テストあり）

### D2: 引数の型の受理行列

| 入力 | 結果型 | 累積方式 |
| ---- | ------ | -------- |
| smallint・integer・bigint 列、疑似列 `id` | numeric | 正確演算 |
| numeric(p,s) 列 | numeric | 正確演算 |
| real・double precision 列 | double precision | 浮動小数（Youngs-Cramer） |
| 数値スカラー式 | double precision | 浮動小数（既存 `SUM`/`AVG` の式入力と同じ既知差分） |
| text・boolean・date・timestamp・time・uuid・bytea・json・配列・vector 等の列と非数値の式 | `42883` | - |

汎用の列分岐は `SUM`/`AVG` 以外の関数へ非数値列を素通しするため、`resolve_aggregate_input` の先頭で許可型に絞る
（絞らないと束縛が成功し、実行時に内部エラー `XX000` になる）。numeric 式（`CAST` の結果）は他の集計と同じ `0A000`。

### D3: 意味論（PostgreSQL 17 互換）

- 行数 0（全 NULL を含む）は NULL。行数 1 は `var_samp`/`variance` が NULL、`var_pop` が 0。NULL 行は件数にも和にも入らない
- 正確演算: 分子 `N·Σx² − (Σx)²`、分母は標本 `N·(N−1)`・母集団 `N·N`。分子が 0 以下（全値が等しい）なら scale 0 の `0`
- 除算 scale は PostgreSQL の `select_div_scale`（被除数・除数の NBASE=10000 の先頭桁と weight から `16 − 4·qweight`、
  双方の dscale 以上、`0..=1000`）の一般化。分子の dscale は入力 scale の 2 倍。除算は half away from zero で丸める
- 浮動小数: `float8_accum` と同じ Youngs-Cramer 更新。母集団 `Sxx/N`、標本 `Sxx/(N−1)`。数値の期待値は累積順序の
  影響で相対誤差 1e-12 程度の揺れを許容する（テストはこの許容で比較する）

### D4: 正確演算と幅広整数

`bigint` のエポックマイクロ秒（約 1.7e15）程度でも `N·Σx²` は `i128` を超える（PostgreSQL は成功する）。外部クレートを足さず、
`numeric.rs` に最小の 256 bit 符号なし整数 `U256` を自作した（checked 演算のみ・`unsafe` なし）。状態は
`count: u64`・`sum_x: i128`（checked）・`sum_x2: U256`（checked）・`scale: u8`。確定は `U256 ÷ u128` の長除算。

### D5: 結果型の公告

`aggregate_result_type` が整数系・`id` は `AVG` と同じ名目 scale 16 の numeric、numeric 列は `max(2s, 16)`（上限 38）の numeric、
浮動小数・式は double を返す。セルの scale は値ごとに変わる（`AVG(integer)` と同じ扱い）。式中の集計列型は、numeric 結果は
式で使えない扱い（`22000`）、double 結果は double。`HAVING`・`ORDER BY` は別名経由で使える。

### D6: 範囲超過は fail-closed

累積のあふれ、numeric 結果が 38 桁を超える場合、結果 scale が 38 を超える場合（`numeric(p,s)` で `s > 19` 等）、
浮動小数が非有限になる場合は `22003`。PostgreSQL の任意精度との差分（PG は成功しうる）。内部不変条件の違反は `XX000`。
エラー文言は固定の英語で、他テナントの値・存在情報を含めない。

### D7: 対象外の経路は fail-closed

| 経路 | 結果 |
| ---- | ---- |
| FROM なし | `0A000` |
| JOIN | `0A000` |
| ウィンドウ `OVER` | 既存の拒否（`42601`/`0A000`） |
| `DISTINCT`・内側 `ORDER BY`（`FILTER` は Issue #1806 で対応済み） | 既存の構文層の拒否 |
| NoSQL（`BoundAggregateItem::bind`） | `42601` |
| ビュー本体・評価後射影形ビューの外側 | 拒否 |

通常のビュー越しの分散は元表と同じ結果になる。

## stddev 系（Issue #1804）

- `stddev`（`stddev_samp` と同義）・`stddev_pop`・`stddev_samp` は分散と同じ累積状態を共有し、確定時だけ平方根をかぶせる
  （`VarianceOutput::Stddev`。構文・型の受理行列・結果型・対象外経路は `is_variance_family` で分散と共通）
- numeric は PostgreSQL 17 の手順どおり、丸め済みの分散を同じ scale で平方根にして half-up 丸めする
  （`numeric::sqrt_round_decimal`。`U256::isqrt` は二分探索）。`N·Σx²/den` の平方根を直接取る改良は PG とずれるため行わない。
  全値が等しい場合は scale 0 の `0`
- double は分散の `f64::sqrt`。非有限は防御として `22003`
- 範囲超過は分散段階の判定（38 桁・結果 scale 38 超）をそのまま継承する。stddev 固有の `22003` 経路はない
- 既定の結果列名は関数名。HAVING 内の直接呼び出し（#2687）も通る
- 期待値は PG 手順を Python の整数演算で独立に再現して導出した固定値（`sql57_stddev.rs`）

## 資源・安全性

- 状態は O(1)（行数に比例して増えない）。`Accumulator` の大きさは増やさない（正確演算は `Box`。単体テストで上限を固定）
- 集計対象は既存走査（RLS を暗黙適用した可視行）のみ。新しい走査経路・索引経路は作らない。他テナントの Private 行が
  結果・`22003` の有無に影響しないことを結合テストで固定

## PostgreSQL との差分

- 数値スカラー式の入力は double（PG は式の型に従い numeric になりうる）
- numeric の 38 桁・結果 scale 38 の上限による `22003`（PG は任意精度）
- 浮動小数の累積は走査順で丸めがわずかに揺れうる

## 契約変更（BREAKING）

- `AggregateFunc` に `Variance`・`VarPop`・`VarSamp`（#1804 で `Stddev`・`StddevPop`・`StddevSamp`）を追加（`non_exhaustive` ではない公開 enum）
- 6 名が予約語になり、同名の `CREATE FUNCTION` は拒否される

## 対象外（追跡）

- `FILTER`（#1806）
- `DISTINCT` 付き分散、FROM なし・JOIN・ウィンドウ（SQL-64）・NoSQL 表層・ビュー本体の分散
- 式入力の numeric 化、任意精度の numeric
