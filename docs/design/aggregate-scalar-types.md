# 新スカラー型の集計と `22003` オーバーフロー契約（Issue #892）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-13
  （スカラー型）・SQL-13（単一行集計）・SQL-14（`GROUP BY`/`HAVING`）・
  TASK-166・TASK-167・TASK-196・TASK-197
- 前提: Issue #880（`ColumnType` 拡張基盤）・Issue #881（`INTEGER`/`BIGINT`）・
  Issue #882（`REAL`/`DOUBLE PRECISION`）・Issue #884（`DATE`/`TIMESTAMP`）・
  Issue #885（`NUMERIC`）・Issue #350（集計経路のデコード tier 分離）

## 背景・目的

`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`（SQL-13・SQL-14）は、疑似列 `id`・`Scalar`
式・`TEXT` 列・その他の新型の `COUNT` までしか受理しておらず、
`INTEGER`/`BIGINT`/`REAL`/`DOUBLE PRECISION` の集計は `COUNT` を含めてすべて
拒否、`NUMERIC`/`DATE`/`TIMESTAMP` は `COUNT` のみ受理していた
（`sql/parser.rs::resolve_aggregate_input` に本 Issue への申し送りコメントが
残っていた）。本 Issue で受理範囲を拡張し、桁あふれの `22003` 契約・`AVG` の
結果型と丸め規則・型ごとの `MIN`/`MAX` 順序規約を確定する。

## 決定

### D1: 受理範囲

| 列型 | `COUNT` | `SUM`/`AVG` | `MIN`/`MAX` |
| ---- | ------- | ----------- | ----------- |
| `INTEGER`/`BIGINT` | 受理 | 受理 | 受理 |
| `REAL`/`DOUBLE PRECISION` | 受理 | 受理 | 受理 |
| `NUMERIC(p,s)` | 受理 | 受理 | 受理 |
| `DATE`/`TIMESTAMP` | 受理 | **拒否**（`22000`） | 受理 |
| `BOOLEAN`/`BYTEA`/`JSON(B)`/`ARRAY`/`UUID`/`ENUM` | 受理 | 拒否（既存契約を維持） | 拒否（既存契約を維持） |

`DATE`/`TIMESTAMP` の `SUM`/`AVG` は合計・平均に意味論が無いため意図的に
拒否のまま維持する。`BOOLEAN`/`UUID` の `MIN`/`MAX`、`ENUM` の `MIN`/`MAX`
（Issue #890 D7 の意図的な判断）は本 Issue の対象外（申し送り）。

### D2: `SUM(INTEGER/BIGINT)` — 確定時判定による `22003`

> **Issue #2271（TABLE-13・TASK-330 ポインタ）による改訂**: `SUM(BIGINT)` の結果は
> `numeric(38,0)`（`Cell::Numeric`、OID 1700）になり、`i64` を超える和もエラーにせず値で返す
> （`Accumulator::BigIntSum`）。`22003` は 38 桁超過のみ。`SUM(INTEGER)` は従来どおり `bigint` で、
> 以下の確定時判定が適用される。単純形 `HAVING`・`ORDER BY` は厳密比較（`i128`）で互換を保つ。
> 式内（投影式・`HAVING` 式・`ORDER BY` 式）の `SUM(BIGINT)` は numeric 算術未対応のため `22000` で
> 拒否し（算術は #1810 の範囲）、バイナリ結果形式は numeric 非対応（WIRE-14）のため `0A000` になる。
> スカラーサブクエリは scale 0 の numeric と整数列の比較を許可する。

`SUM(INTEGER)` の結果は `Cell::SignedInteger(i64)`（`BIGINT` 相当）。累積は `i128` の
`checked_add`（`i128` 自体の桁あふれも `22003`）で行い、`Accumulator::finish`
の確定時に初めて `i64::try_from` を検査する。

部分和が一時的に `i64` を超えても最終値が `i64` に収まれば成功する契約
（例: `[i64::MAX, 1, -1]` の和は `i64::MAX` で成功）。確定時に判定する理由は
次の 2 点。

1. 行の走査順（全行走査か Issue #475 の索引候補走査か）によって部分和の
   通過順序が変わっても、結果が変わらない。
2. `i128` で保持する意味がそこにある（`i64` 単位での逐次検査では走査順依存の
   誤検出・誤許可が起き得る）。

### D3: `AVG(INTEGER/BIGINT)`

> **Issue #2270（TABLE-13・TASK-330 ポインタ）による改訂**: `AVG(SMALLINT/INTEGER/BIGINT)` と
> `AVG(id)` の結果は `numeric`（`Cell::Numeric`、OID 1700）になる。累積（`i128`／`u64` の
> 合計と行数）は変えず、`Accumulator::finish` でだけ `numeric::avg_integer` により
> `sum / count` を長除算（half away from zero）して `Decimal` にする。小数部の桁数は
> PostgreSQL 17 の結果 scale の規則（`select_div_scale`。被除数・除数の NBASE=10000 の
> 先頭桁と weight から `16 - 4 * qweight`）を `numeric::pg_div_result_scale` で再現し、
> **値ごとに変わる**（列メタの scale とは一致しない。列メタは名目 scale 16 で公告）。
> 影響する下流経路と扱い:
>
> - `GROUP BY` の `ORDER BY` は `cmp_exact`、単純形 `HAVING` は `Decimal` と `f64` リテラル
>   （最短往復表現の 10 進として読む）の厳密比較にする（scale 不一致で黙って縮退させない）。
> - 式内（投影式・`HAVING` 式・`ORDER BY` 式）の整数 `AVG` は numeric 算術が未対応のため
>   `22000` で拒否する（算術は #1810・#1812 の範囲）。バイナリ結果形式は numeric 非対応
>   （WIRE-14）のため `0A000`（#2441・#2442）。
> - スカラーサブクエリ比較は端数のある numeric を整数列とは floor による厳密な整数比較、
>   浮動小数列とは `f64` 変換で比べる。
> - `UNION` 等の重複排除キーは numeric の末尾の小数部ゼロを除き、scale だけが異なる等しい値を
>   同一視する。
> - 窓関数・JOIN 集計・FROM なし SELECT・NoSQL `aggregate` は同じ `Accumulator`／共有関数で追随する。
>   NoSQL の JSON number は桁を丸めない。

旧仕様（参考）: 結果は `Cell::Float(f64)`（`DOUBLE PRECISION` 相当）で、
`(sum_i128 as f64) / (count as f64)`（IEEE 754 の最近接偶数丸め）。空集合は
`NULL`。

### D4: `SUM/AVG(REAL/DOUBLE PRECISION)`

`f64` で累積する（`REAL` は `f64::from(f32)` で無損失に拡張）。結果はどちらも
`Cell::Float`（`DOUBLE PRECISION`。`SUM(REAL)` を `f64` で返すのは本リポの
実装既定値）。累積値が非有限になったら `22003`
（既存 `Accumulator::FloatSum`/`FloatAvg`（`ScalarExpr` と共有）の
`is_finite()` 検査を再利用）。

### D5: `SUM(NUMERIC(p,s))`

結果は `Cell::Numeric`（scale は列の `s`）。`unscaled` を `i128` の
`checked_add` で累積し、`finish` で `Decimal::fits_precision(MAX_PRECISION=38)`
を検査する。和は列の `p` を超えてよい（上限は `Decimal` の不変条件である
38 桁のみ）。`i128` の中間桁あふれも `22003`。

### D6: `AVG(NUMERIC(p,s))`

結果は `Cell::Numeric`。結果の scale は
`S' = max(s, min(16, 38 - (p - s)))`（整数部 `p - s` 桁と合わせて 38 桁以内に
収まる。列の scale `s` 自体が下限のため `s >= 16` の列では拡張されない）。

除算は `numeric::avg_unscaled`（新設）による 1 桁ずつの長除算（`q = sum / n`、
`r = sum % n` のあと小数桁を `r * 10` で順に求める。各ステップの剰余は常に
`n`（`u64` の非 NULL 行数）未満のため桁あふれしない）で正確に行い、最後の
剰余で half away from zero（`numeric::parse_for_column` のリテラル丸めと
同じ規約）に丸める。丸め後に 38 桁を超えたら `22003`（防御）。

`sum * 10^(to_scale - from_scale)` を素直に計算する方式は、`to_scale` が
大きい場合に中間値が `i128` を超えて桁あふれし得るため採用しなかった。

### D7: `MIN`/`MAX` の順序

| 型 | 比較 | 結果 |
| -- | ---- | ---- |
| `INTEGER`/`BIGINT` | `i64` の全順序 | `Cell::SignedInteger` |
| `REAL`/`DOUBLE PRECISION` | `f64::total_cmp`（格納値は常に有限） | `Cell::Float` |
| `NUMERIC(p,s)` | 同一 scale の unscaled を比較 | `Cell::Numeric` |
| `DATE` | `i32`（1970-01-01 起点の日数） | `Cell::Date` |
| `TIMESTAMP` | `i64`（1970-01-01 起点のマイクロ秒） | `Cell::Timestamp` |

`NUMERIC` の格納値は同一列由来である限り常に同じ scale を持つ契約
（`Accumulator` の各 variant が列の scale をそのまま保持する）。scale が
一致しない値の観測はデコード側の実装バグとして `accumulator_bug`（`XX000`）
で fail-closed に拒否する。`NULL` は無視し、空集合は `NULL`。

### D8: `HAVING`（SQL-14・NOSQL-5 共通）

`Cell::SignedInteger` の結果は、`cmp_integer_to_literal`（`u64` 版）を
符号付きへ拡張した `cmp_signed_to_literal` で `f64` リテラルと厳密に比較する
（`2^63` 境界・非整数リテラルを正しく扱う）。

`NUMERIC` を返す集計（`SUM`/`AVG`/`MIN`/`MAX`）と `DATE`/`TIMESTAMP` の
`MIN`/`MAX` を `HAVING` の対象にした場合は、束縛段
（`sql::parser::check_having_target_is_numeric`）で **`22000`** として拒否
する。`f64` リテラルとの厳密な数値比較に意味論が無いため、黙って `false`
（常に不一致）へ縮退させない（`TEXT` 型集計の既存契約と同じ扱い）。

### D9: `ORDER BY`（`GROUP BY` 経路）

`cmp_cell_values` に `SignedInteger`・`Numeric`（同一 scale 比較。scale が
一致しない組み合わせは到達しない想定の防御的フォールバックとして `Equal`）・
`Date`・`Timestamp` の腕を追加した。これをしないと既存の `_ => Equal` へ
落ち、`ORDER BY` が黙って効かなくなる（PR #230 系のレビューで確立した検出
パターンと同じ懸念）。

## デコード tier の維持（受入条件 4・Issue #350）

新しく追加した列入力 variant（`IntegerColumn`/`BigIntColumn`/`RealColumn`/
`DoubleColumn`/`NumericColumn`/`DateColumn`/`TimestampColumn`）はすべて
`ReferencedColumns::derive` で `has_scalar_reference = true`・
`scalar_mask[index] = true` を立て、`needs_embedding` は立てない。これにより
`DecodeTier::DimAndScalar` に着地し（`Embedding` へは昇格せず、
`Fast`〔= `VisibleBitmapCache` 経路〕にも誤って入らない）、RLS の可視判定・
TABLE-12 のキー/ヘッダ tenant 整合検査・`decode_row_dim_and_metadata_borrowed`
による構造検証は全 tier で従来どおり行われる。`sql::group_by` は元々
`DecodeTier::Fast` を選択しない設計のため、`GROUP BY` 側のデコード tier 判定
は無変更。

## 実装

- `crates/engine/src/numeric.rs`: `avg_unscaled`（長除算）を追加。
- `crates/engine/src/sql/parser.rs`: `AggregateInput` へ
  `IntegerColumn`/`BigIntColumn`/`RealColumn`/`DoubleColumn`/`DateColumn`/
  `TimestampColumn` を追加し、`NumericColumn` を
  `{ index, precision, scale }` へ拡張。`resolve_aggregate_input`・
  `check_having_target_is_numeric` を D1・D8 に合わせて更新。
- `crates/engine/src/sql/aggregate.rs`: `Accumulator` へ
  `IntSum`/`IntAvg`/`IntMin`/`IntMax`/`NumericSum`/`NumericAvg`/
  `NumericMin`/`NumericMax`/`DateMin`/`DateMax`/`TimestampMin`/
  `TimestampMax` を追加。`Accumulator::finish` を `Result<Cell,
  SqlSurfaceError>` へ変更（D2・D5・D6 の確定時検査のため）。
  `finish_aggregate_result`（3 箇所の呼び出し元）・
  `sql::group_by::execute_grouped_aggregate` の `finish` 呼び出しを
  `Result` 伝播へ追従。
- `crates/engine/src/sql/group_by.rs`: `having_matches` へ
  `Cell::SignedInteger` の腕（`cmp_signed_to_literal`）を追加。
  `cmp_cell_values` へ D9 の腕を追加。

## 対象外・申し送り

- `WHERE` 述語・式（`vec_norm(n)` 等）・`GROUP BY` キー列としての新型直接参照
  は Issue #891 の担当のまま（本 Issue は集計関数の引数としての参照のみ）。
- `BOOLEAN`/`UUID` の `MIN`/`MAX`・`ENUM` の `MIN`/`MAX`（Issue #890 D7 の
  意図的判断）は対象外。
- NoSQL 表層（`op: aggregate`）は `engine::sql::parser::BoundAggregate`/
  `BoundAggregateItem` を共有するため本 Issue の変更がそのまま適用される
  （wire-server 側の production コード変更は不要。`cargo check` で確認済み）。
  新型集計の SQL⇄NoSQL パリティを検証する専用の wire 層 A テストは、既存の
  `tests/nosql4_5_aggregate_wire_parity.rs` が TEXT/`id` 中心のため追加して
  いない（後続の担当として申し送り）。
- 索引経路（Issue #475・#474。`sql::scalar_index`）の候補削減・列挙形
  （`sql::group_by::observe_group_enumeration`）は新しい `Accumulator`
  variant がいずれも O(1) の固定長状態であるため既存の TEXT `MIN`/`MAX`
  専用ゲート（`has_text_min_max_aggregate`）の対象外のまま動作する
  （`ScalarExpr` の `FloatMin`/`FloatMax` と同じ扱い）。
- `sql::aggregate`/`sql::group_by` の tier 判定ロジックを共有関数
  （`select_decode_tier`）へ抽出するリファクタは、既存構造のまま
  `ReferencedColumns::derive` の拡張のみで受入条件 4 を満たせたため見送った
  （挙動は不変。将来の重複解消候補として記録）。
