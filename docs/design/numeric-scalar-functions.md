# 数値スカラー関数群（Issue #920・SQL-26・TASK-210）

## ステータス

Accepted（数値関数群のみ）。日時スカラー関数群は別 PR で実装済み
（`docs/design/datetime-scalar-functions.md` 参照。下記「スコープ外・
後続課題」は当時の記録として残す）。

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
| 0 除算（`mod(x, 0)`。Issue #1163 で分離） | `22012` | `mod(1, 0)` |
| 未定義演算（`0` の負数乗・負数の非整数乗・`sqrt` の負数） | `22000` | `sqrt(0 - 1)` |
| `round` の第 2 引数が非整数値 | `22000` | `round(1.0, 0.5)` |
| オーバーフロー・アンダーフロー（`i32` 範囲外の丸め桁数を含む） | `22003`（`NumericOutOfRange`。既存の集計オーバーフロー分類を再利用） | `power(10, 400)`・`round(1, 3000000000)` |
| 引数の型不一致・未知関数 | `22000`（既存の `InvalidInput`） | `abs(embedding)` |

当初（#920）は新しい `ErrorClass` を追加せず、0 除算も `22000` に含めていた。
その後 Issue #1163 で 0 除算専用の `ErrorClass::DivisionByZero`（`22012`・
`DIVISION_BY_ZERO`・HTTP 400）を追加し、`mod(x, 0)` をスカラー `/`・
ベクトル÷スカラー・`vec_div` と同じ `22012` へ分離した（ERR-6・SQL-26
ポインタ）。オーバーフロー・アンダーフローは従来どおり既存の `NumericOutOfRange`
（`22003`）を再利用しており、この節で残る後続課題はない。

`round(x, n)` は `x * 10^n` を `f64` の乗除算で計算せず、`x` の最短往復表現
（Rust の `{}` フォーマット。ある `f64` 値へ一意に戻る最短の 10 進数字列）を
10 進数字列として直接切り捨て・繰り上げする方式で実装する（PR #1107
codex-review P1 是正）。`x * 10^n` を `f64` で計算すると、10 進小数として
ちょうど中間値（例: `1.005`）が二進化の丸め誤差でわずかにずれ、`round(1.005, 2)`
が期待される `1.01` ではなく `1.00` になる不具合があったため、浮動小数点の
乗除算を経由しない方式へ変更した。丸め桁数 `n` が `x` の実際の桁数
（10 進展開した整数部・小数部の桁数）を超える場合は丸める余地がないため `x`
をそのまま返し、丸め単位（`10^-n`）が `x` の絶対値の桁数を超える場合は
最上位桁でのみ丸め上げの要否を判定し、丸め上げが起きなければ `0` を返す
（黙った精度欠落ではなく、その桁での丸め操作が数学的に意味を持たない領域への
意図的な縮退）。丸め後の 10 進数字列を `f64` へ変換した結果が `Infinity` に
なる場合（真のオーバーフロー）のみ `22003` へ写像する（`sql/numeric_fn.rs::round2`
のコメント参照）。

### 定数畳み込み

`sql/expr_program.rs::try_fold_scalar` に `BoundExpr::Builtin` の畳み込みを
追加した。`sql::udf_call::is_foldable_builtin`（`_ =>` を使わない網羅
`match`）で対象を数値関数群に限定し、Vector を引数に取る既存組み込み
（`vec_norm`/`vec_sum`/`vec_div`）は畳み込み対象外のまま維持する。畳み込み中に
エラーになる部分式（`sqrt(0 - 1)` 等）は畳み込まず実行時評価へ委ねる
（defer-on-error。可視行 0 件のクエリではエラーが発生しないという既存契約を
保つ）。

### 決定性（本 PR の対象範囲）

本 PR が追加する数値関数（`abs`/`round`/`floor`/`ceil`/`ceiling`/`mod`/`power`/
`sqrt`）はいずれも純粋関数（同じ引数には常に同じ結果）であり、決定性は
実装上自明に満たされる。`now`・`random` 等の非決定的関数は本 PR では実装せず、
呼び出し自体は（組み込み・UDF いずれにも解決しないため）既定の「未知の関数」
経路で `22000` に拒否されるが、これらの名前を UDF の予約名に含めることは
**行わない**（PR #1107 codex-review P1 是正。従来登録・呼び出しできていた
`now`／`random` 等の名前の宣言的 UDF・WASM UDF を登録時に拒否する破壊的変更に
なっており、本 PR（数値関数のみ）にはその変更を正当化する非決定的関数の実装が
含まれないため）。非決定的関数名の UDF 予約化と決定性要件（SQL-26 が要求する
「1 文の中で時刻・乱数をどう固定するか」という論点を生じさせない設計）は、
日時スカラー関数群を実装する後続作業（下記「スコープ外・後続課題」参照）で
spec と対にして判断する。

## スコープ外・後続課題（当時の記録。実装済みの部分は `docs/design/datetime-scalar-functions.md` を参照）

`date_part('field', src)`・`date_trunc('unit', src)`・`DATE`/`TIMESTAMP`
リテラルの式内利用・日付算術（`DATE ± Scalar`・`DATE - DATE`）・
`DATE`/`TIMESTAMP` 列の式内参照は別 PR（`docs/design/
datetime-scalar-functions.md`）で実装済み。`EXTRACT(field FROM src)` 構文の
みは同 ADR のスコープ外として残っている（`date_part` で代替可能）。

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

上記に加え、`now`・`current_timestamp`・`current_date`・`current_time`・
`localtime`・`localtimestamp`・`clock_timestamp`・`statement_timestamp`・
`transaction_timestamp`・`timeofday`・`random` を UDF の予約名とするかどうかの
判断（PR #1107 codex-review P1 是正で本 PR からは除外。上記「決定性（本 PR の
対象範囲）」参照）も、日時スカラー関数群の実装時に spec の決定性要件と対にして
判断すること。
