# 数値スカラー関数群（Issue #920・SQL-26・TASK-210）

## ステータス

Accepted（数値関数群のみ）。日時スカラー関数群は本 Issue のスコープ外（下記
「スコープ外・後続課題」参照）。

## 背景

対応ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-26・
`docs/spec/05-tasks.md` TASK-210・SQL-9（宣言的 UDF、同じ関数名前空間）・
`docs/spec/04-behavior/error-format.md` ERR-6。

`sql/udf_call.rs` の組み込み関数（`BuiltinFn`）は `vec_norm`・`vec_sum`・
`vec_div` の 3 本のみで、UDF 本体・`SELECT`/`WHERE` 式から使える数値計算が
不足していた。本 Issue で `ABS`・`ROUND`（1/2 引数）・`FLOOR`・`CEIL`（別名
`CEILING`）・`MOD`・`POWER`・`SQRT` を追加した。

## 設計判断

### 関数解決とオーバーロード

- 単一 arity の関数（`abs`/`floor`/`ceil`/`ceiling`/`mod`/`power`/`sqrt`）は
  既存の `builtin_from_name(name) -> Option<BuiltinFn>` に追記するだけで済む。
- `round` のみ 1 引数形（丸め）と 2 引数形（小数点以下 `n` 桁）を持つため、
  `builtin_from_name` の対象から外し、`bind_call`・`validate_closed_expr` の
  呼び出し側で引数個数を見てから `BuiltinFn::Round1`／`Round2` を選ぶ。
  `is_builtin_function_name`（CHECK 制約・予約名判定が使う）はこの 2 経路を
  `name.eq_ignore_ascii_case("round")` で明示的に吸収する。

### 構文レイヤーは無変更

`sql/allowlist.rs::Parser::parse_call_expr` は既に
`<ident> '(' [expr {',' expr}] ')'` という関数呼び出し文法を任意の識別子
（未知の名前を含む）に対して受理し、名前解決を束縛段（`sql::udf_call::bind_call`）
へ委譲する設計だった。数値関数は新しい構文要素（キーワード・特殊な引数形）を
要しないため、**allowlist.rs は 1 行も変更していない**。同じ理由で
`sql/check_constraint.rs`（`render_expr`・`reject_forbidden_expr` はいずれも
関数名文字列で汎用的に動く）・`recovery/content_hash.rs`（`push_dml_expr` の
`Expr::Call` 直列化は関数名文字列と引数個数のみをハッシュし、`BuiltinFn` の
具体的な variant を見ない）も無変更で新関数を扱える。

### エラー写像

| 状況 | wire_code | 例 |
| ---- | --------- | -- |
| 0 除算・未定義演算（`0` の負数乗・負数の非整数乗・`sqrt` の負数） | `22000` | `sqrt(0 - 1)`・`mod(1, 0)` |
| `round` の第 2 引数が非整数値 | `22000` | `round(1.0, 0.5)` |
| オーバーフロー・アンダーフロー（`i32` 範囲外の丸め桁数を含む） | `22003`（`NumericOutOfRange`。既存の集計オーバーフロー分類を再利用） | `power(10, 400)`・`round(1, 3000000000)` |
| 引数の型不一致・未知関数 | `22000`（既存の `InvalidInput`） | `abs(embedding)` |

新しい `ErrorClass` は追加していない（`docs/spec/04-behavior/error-format.md`
ERR-6 参照。#919/#920/#921 を跨ぐ横断変更は後続課題とする）。

`round(x, n)` の丸め桁数 `n` が極端な値のときの縮退（`10^n` がオーバーフロー
する場合は `x` をそのまま返す、アンダーフローする場合は `0` を返す）は、
黙った精度欠落ではなく「その桁での丸め操作が数学的に意味を持たない」領域への
意図的な縮退である。ただし `10^n` 自体が非零で `scaled` も有限だが、桁を
戻す除算が真にオーバーフローするケースはこの縮退と区別し `22003` へ
写像する（`sql/numeric_fn.rs::round2` のコメント参照）。

### 定数畳み込み

`sql/expr_program.rs::try_fold_scalar` に `BoundExpr::Builtin` の畳み込みを
追加した。`sql::udf_call::is_foldable_builtin`（`_ =>` を使わない網羅
`match`）で対象を数値関数群に限定し、Vector を引数に取る既存組み込み
（`vec_norm`/`vec_sum`/`vec_div`）は畳み込み対象外のまま維持する。畳み込み中に
エラーになる部分式（`sqrt(0 - 1)` 等）は畳み込まず実行時評価へ委ねる
（defer-on-error。可視行 0 件のクエリではエラーが発生しないという既存契約を
保つ）。

### 決定性（非決定的関数の拒否）

`now`・`current_timestamp`・`current_date`・`current_time`・`localtime`・
`localtimestamp`・`clock_timestamp`・`statement_timestamp`・
`transaction_timestamp`・`timeofday`・`random` を UDF の予約名に追加し
（`sql::udf_call::is_non_deterministic_function_name`）、呼び出し自体も
（組み込み・UDF いずれにも解決しないため）既定の「未知の関数」経路で `22000`
に拒否される。SQL-26 が要求する決定性は、これらを一切受理しないことで
「1 文の中で時刻・乱数をどう固定するか」という論点自体が生じない形で満たす。

## スコープ外・後続課題

`EXTRACT(field FROM src)`・`date_part('field', src)`・`date_trunc('unit', src)`・
`DATE`/`TIMESTAMP` リテラルの式内利用・日付算術（`DATE ± Scalar`・
`DATE - DATE`）は本 Issue のスコープ外とした。

理由: 上記は次の変更を要し、数値関数群（構文追加なし・`content_hash`/
`check_constraint` 無変更）と比べてリスク・変更範囲が大きく異なる。

- `ExprType::Date`/`Timestamp`・`ExprValue::Date`/`Timestamp` の新設と、
  `sql/exec.rs`・`sql/scan.rs` 等での `Cell` への変換経路の追加。
- 新規構文（型付きリテラル `DATE '…'`/`TIMESTAMP '…'`・
  `EXTRACT(field FROM expr)` の `FROM` を通常の `SELECT ... FROM` と誤認しない
  よう `allowlist.rs::Parser` の式解析を拡張する必要がある）。
- `recovery/content_hash.rs::push_dml_expr` への新規直列化タグ追加（既存タグは
  不変のまま、新しい `Expr` variant 用のタグ番号を割り当てる必要がある）。

これらは別 Issue として起票が必要だが、本タスクの制約（GitHub Issue の新規
起票は行わない）によりここに記録するに留める。実装時は本ドキュメントと
`docs/spec/04-behavior/sql-surface.md` SQL-26 を出発点にすること。
