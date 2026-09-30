# 日時スカラー関数群（Issue #920・SQL-26・TASK-210）

## ステータス

Accepted。数値スカラー関数群は先行 PR（#1107）で実装済み（
`docs/design/numeric-scalar-functions.md` 参照）。本 ADR は Issue #920 の
残りの部分（日時関数群・型付きリテラル・`DATE`／`TIMESTAMP` 列の式内参照・
`DATE` 算術）を扱う。

対応ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-26・
`docs/spec/05-tasks.md` TASK-210・TASK-79（SQL-9、宣言的 UDF の名前空間を
共有）・`docs/spec/04-behavior/error-format.md` ERR-6・TABLE-13・TASK-197
（`DATE`／`TIMESTAMP` 列型、Issue #884）。

## 背景

`DATE`／`TIMESTAMP` 列（Issue #884）はカタログ・行コーデック・投影テキスト
整形には対応済みだったが、式（`SELECT`／`WHERE`）の中から参照する経路が
存在しなかった（`udf_call::bind_expr_in` が `22000` で拒否）。日時スカラー
関数（`date_part`／`date_trunc`）・型付きリテラル（`DATE '…'`／
`TIMESTAMP '…'`）・`DATE` 算術も未実装だった。

## 対象機能

| 区分 | 対象 |
| ---- | ---- |
| 関数 | `date_part(field, src)`・`date_trunc(unit, src)` |
| リテラル | `DATE 'YYYY-MM-DD'`・`TIMESTAMP 'YYYY-MM-DD HH:MM:SS[.f]'`（`crate::datetime::parse_date`／`parse_timestamp` と同一の閉じた文法） |
| 列参照 | `DATE`／`TIMESTAMP` 列を式中で参照可能にする |
| 算術 | `DATE + n`・`n + DATE`・`DATE - n`・`DATE - DATE` |
| 比較 | `DATE`⋈`DATE`・`TIMESTAMP`⋈`TIMESTAMP`・`DATE`⋈`TIMESTAMP`（`DATE` 側を深夜 0 時の `TIMESTAMP` へ暗黙昇格） |
| CASE/COALESCE/NULLIF | 分岐・引数が同じ日時型どうしなら受理 |

`EXTRACT(field FROM src)` 構文は Issue #1188 で追加した（構文段で
`date_part('field', src)` へ脱糖するため、値・NULL 伝播・エラー分類は
`date_part` と同一。詳細は `docs/design/order-by-having-expressions.md`）。

## 型規約とエラー写像

| 式 | 戻り型 | エラー |
| -- | ------ | ------ |
| `date_part(f, DATE\|TIMESTAMP)` | Scalar（f64） | 未知の field: `22000`（束縛時） |
| `date_trunc(u, DATE\|TIMESTAMP)` | TIMESTAMP | 未知の unit: `22000`（束縛時）。結果が範囲外（1〜9 年の `decade` 切り捨て等）: `22008` |
| `DATE ± n` / `n + DATE` | DATE | `n` が非整数: `22000`。`n` が `i32` 範囲外: `22003`（`NumericOutOfRange`）。結果が `DATE` 受理範囲外: `22008` |
| `DATE - DATE` | Scalar（日数） | なし（範囲内の差は必ず表現できる） |
| `TIMESTAMP ± n`・`TIMESTAMP - TIMESTAMP`・`DATE * / n` | — | `42804`（`INTERVAL` 型が無いため対象外。`bind_binary` の型不一致経路） |
| 型付きリテラルの書式違反 / 範囲外 | — | `22007` / `22008`（Issue #1187。`datetime-column.md` と同じ写像） |
| 引数の型不一致 | — | `42804`（Issue #1186） |
| arity 違反 | — | `22000` |
| NULL 入力 | NULL | すべて strict（いずれかの引数が NULL なら NULL） |

## 意味論（PostgreSQL 互換）

`sql::datetime_fn` が独立オラクル（PostgreSQL の公開済み挙動）で固定した
値レベル計算（`crates/engine/src/sql/datetime_fn.rs` の単体テスト参照）:

- `date_part` の field は大小無視・単数形/複数形の双方を受理する:
  `microseconds`／`milliseconds`／`second`／`minute`／`hour`／`day`／
  `week`／`month`／`quarter`／`year`／`decade`／`century`／`millennium`／
  `dow`／`isodow`／`doy`／`isoyear`／`epoch`。`timezone*`・`julian`・
  略記形は `22000`。
- `week`／`isoyear` は ISO 8601 基準（年境界をまたぐ週は所属する ISO 暦年が
  西暦年と食い違いうる）。`dow` は日曜 = 0、`isodow` は月曜 = 1〜日曜 = 7。
- `century` = `(年+99)/100`、`millennium` = `(年+999)/1000`、`decade` =
  `年/10`（いずれも整数除算）。
- `epoch` は 1970-01-01 からの秒数（小数秒を含む）。
- `DATE` を入力に取る場合は深夜 0 時の `TIMESTAMP` へ暗黙昇格する
  （`BuiltinFn::DateToTimestamp`。`bind_call`／`bind_binary` が挿入する）。

### 既知の PostgreSQL との差分

- (a) PG14+ の `EXTRACT(hour FROM date)` はエラーになるが、本実装は
  `date_part` と同じく `DATE` を昇格して 0 を返す。
- (b) PG は `DATE` を入力にした `date_trunc` で `timestamptz` を返すが、
  本実装は naive `TIMESTAMP` を返す（`TIMESTAMPTZ` は未対応）。
- (c) （解消）`EXTRACT` 構文は Issue #1188 で実装済み。既定の結果列名は PG 互換の `extract`（明示の `date_part(...)` は `date_part` のまま）。
- (d) `EXTRACT`/`date_part` の戻り値は PG では `numeric` だが、本実装は
  `Scalar`（`f64`）。

## 設計判断

### field / unit を束縛時に解決する

`BuiltinFn::DatePart(DatePartField)` と `BuiltinFn::DateTrunc(DateTruncUnit)`
のペイロードに field / unit を持たせ、束縛後の第 1 引数が
`BoundExpr::Text` リテラルでなければエラーにする（`udf_call::
bind_date_part_or_trunc`）。第 1 引数がテキスト型でない場合は `42804`、
テキスト型だがリテラルでない（列参照・式）場合は `22000` とする。
実行時の引数は `src` 1 個だけになり、行ごとに
文字列を解析しない。`date_part`／`date_trunc` は名前だけでは variant が
決まらないため `builtin_from_name` には入れず、`is_variadic_or_overloaded_
builtin_name` に追加する（`round` と同じ流儀）。

未知の field/unit は可視行が 0 件でも束縛時エラーになる（**束縛時の検証**。
`try_fold_scalar` の defer-on-error 契約とは別物）。定数算術の失敗（例:
`DATE '9999-12-31' + 1`）は畳み込まずに実行時へ遅延させる。

### 定数畳み込みの対象外

`sql::expr_program::FoldedConst` は `Scalar`／`Bool` のみを表現できる型
（文字列組み込みと同じ制約。`docs/design/numeric-scalar-functions.md`
参照）。`DATE`／`TIMESTAMP` リテラル・列参照・`DatePart`／`DateTrunc`／
`DateToTimestamp` はいずれも `is_foldable_builtin` の対象外とし、常に
`ConstDate`／`ConstTimestamp`／`PushDateColumn`／`PushTimestampColumn`
ステップへ平坦化するだけに留める。正しさには影響しない（defer-on-error は
`Case`/`Coalesce` のジャンプ命令コンパイルで別途成立する）。

### NOW() と決定性

現在時刻系（`now`・`current_timestamp`・`current_date`・`localtimestamp`
等）は実装しない。呼び出しは未知の関数として `42883` になる（Issue #1186）。これらの
名前は UDF 予約名にも**しない**（PR #1107 の codex 是正を維持。組み込み
関数はすべて純粋関数で時計を読まないため、`now` という名前の UDF があっても
決定性は崩れない）。

### UDF 予約名

`date_part`・`date_trunc` は `is_builtin_function_name` 経由で予約名になる。
`date`／`timestamp`／`extract` は予約しない（`EXTRACT` 構文は `extract` の直後の
括弧内が「識別子または文字列リテラル `FROM`」の並びのときだけ専用形として解析する先読み
方式のため、それ以外の `extract(...)` は通常の関数呼び出し・UDF 名として使える）。

### 行スカラービューの一般化

`sql::udf_call::eval_with_scalars`・`sql::expr_program::ExprProgram::eval`
の引数を `text_columns: &[Option<&str>]` から
`row_scalars: &[Option<crate::row_codec::ScalarRef>]` へ一般化した
（`ScalarRef` は既に `Date`／`Timestamp` variant を持つ既存型。Issue #884）。
`TextColumnRef` の解決は `ScalarRef::as_text()` 経由に変わり、新しい
`DateColumnRef`／`TimestampColumnRef` は同じ規則（`Some(Some(不一致型))` は
`Internal` で fail-closed）で解決する。呼び出し元（`sql::scan`／
`sql::aggregate`／`sql::group_by`／`sql::window`／`sql::exec`／
`sql::where_tree`／`sql::check_constraint`）は、既に `ScalarRef` を持って
いた箇所はそのまま渡すだけになり、`.as_text()` へ写す中間 `Vec` を削除
できた。`mark_referenced_scalar_columns`／`visit_referenced_scalar_columns`
は新しい列参照 variant も反映する（マスク外参照＝実 NULL の誤判定を防ぐ。
`sql26_datetime_functions.rs` の `coalesce_branch_only_datetime_reference_
is_not_masked_out` で分岐内参照のみの回帰を固定する）。

### content_hash タグ 10

`recovery::content_hash::push_dml_expr` にタグ 10（型バイト 0=DATE/
1=TIMESTAMP、続けて LE の内部表現バイト）を追加した。既存タグ 1〜9 は
不変。日付だけが異なる DML・同じ暦日を表す `DATE` と `TIMESTAMP` は
いずれも異なるハッシュになる。

## スコープ外・後続課題

- （解消: Issue #1188）`EXTRACT(field FROM src)` 構文。
- `INTERVAL`・`TIMESTAMPTZ`・`AT TIME ZONE`・`make_date` 系関数。
- `GROUP BY` キーの式化・`SELECT` 頂点の非関数式
  （`SELECT d + 1` 等。`sql::allowlist::Parser::parse_select_item` は
  `CASE`／関数呼び出し以外の識別子始まりの式を投影項目として受理しない
  既存の構造的制約。数値スカラー関数群 ADR と同じ既知の制約）。
  `ORDER BY`／`HAVING` の式位置は Issue #1188 で対応済み
  （`docs/design/order-by-having-expressions.md`）。
- `42804`/`42883` への移行（ERR-6・TASK-227 の横断課題）。
- NoSQL（HTTP）表層での日時関数対応。
- `DATE`／`TIMESTAMP` 列型の `CREATE TABLE` SQL DDL 経由の宣言（現状は
  Rust API 経由のみ。既存の Issue #884 のスコープ外事項がそのまま残る）。
