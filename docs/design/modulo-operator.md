# 剰余演算子 `%` の拒否の根拠（Issue #3528）

- ステータス: **Accepted**（設計記録。production コード・スクリプト・CI は無変更）
- 対応: Issue #3528（拒否固定テストの棚卸し）。注記書式は [rejection-pin-annotation.md](rejection-pin-annotation.md)
- 本書は private spec の本文を転記しない。PostgreSQL 17 の挙動は公開の挙動だけを書く

## 1. 背景

`%` は `* /` と同じ優先度の二項演算子として実装されている（実装記録は `implementation-status.md` の
SQL-42 の項）。通常の投影では整数どうしの剰余だけを評価する。集計式（`sum(n) % 4`・`avg(n) % 1` など）では `DecimalOp::Rem` により numeric の剰余も評価する（`aggregate_projection_uses_numeric_remainder` が固定する）。上記以外のうち未対応の被演算子型・式の形は束縛時に拒否する（型検査）。整数式の値に依存する範囲検査（例: §4 の `(b + 1) % 2` の `22003`）は束縛を通り、評価時に行う。剰余の拒否の分岐は `bind_expr_any` にあり、両辺が `Scalar` のとき、両被演算子が整数と判定できない組を拒否する。確認済みの形は次のとおり。

- 剰余の直接の被演算子が整数と判定できない小数リテラル（`Expr::Number`。例: `0.3 % 0.1`・`7.5 % 2`・`d % 0.5`）は `0A000`
- 小数リテラルを直接の被演算子としない、整数と判定できない `Scalar` の組は `42883`（`r % 2`・`1.5::float8 % 2`・`COALESCE`／`NULLIF`／`CASE` で包んだ `float8` など。小数リテラルが式の内部に含まれるだけでは `0A000` にならない）
- `Scalar` 以外の組（`text`・ベクトル型など）は束縛時に `42883` などで拒否する

上記以外の形（`NUMERIC` 列・`::numeric` など）の SQLSTATE は本書では断定しない。
本書は、拒否のうち PostgreSQL 17 と同じ結果になるもの（§2）と、設計上の意図的な拒否（§3）の根拠を残す。

## 2. 意図的な拒否（PostgreSQL 17 と同じ）

| 拒否する形 | 本実装の SQLSTATE | PostgreSQL 17 の実測 | 維持する理由 |
| ---------- | ----------------- | -------------------- | ------------ |
| `real % integer`・`double precision % integer`・`integer % double precision` | `42883` | `42883`（`operator does not exist: double precision % integer` など） | PostgreSQL に浮動小数点の `%` 演算子は無い。f64 の `fmod` へ流すと PostgreSQL に無い結果を返すため、同じく拒否する |
| `text % integer` | `42883` | `42883`（`operator does not exist: text % integer`） | 型の組み合わせに演算子が無い。暗黙の型変換で受理しない |
| `COALESCE`・`NULLIF`・`CASE` で包んだ `double precision` の `%` | `42883` | `42883`（結果型が `double precision` のまま演算子の解決に失敗する） | 包んだ式でも直接の `double precision` と同じ結果型の規則で拒否する。ラッパーで型を隠して通す経路を作らない |

- 上の表は PostgreSQL 17 との一致を根拠にできる形の一覧であり、本実装で `42883` になる型の全集合ではない（ベクトル型は §3）。表は `docs/design` に残す根拠であり、テストは `sql42_remainder_operator.rs` の
  `unsupported_operand_types_are_42883` が固定する
- PostgreSQL 17 の実測は、公式の `postgres:17` コンテナで同じ形の文を実行して確かめた（使い捨て・資格情報なし）。上の表は比較できた型（real・double precision・integer・text）に限る

## 3. 設計上の意図的な拒否（PostgreSQL 17 との比較なし）

- ベクトル型の列に対する `%`（`embedding % 2`）は `42883`。標準の PostgreSQL 17 に当該型が無く拡張型も未実測のため、§2 の「PostgreSQL 17 と同じ」を根拠にしない。ベクトルとスカラーの `*`・`/` は実装済み（`apply_vector_scalar_op`）だが、`%` はベクトルに対する定義が無く、fail-closed のまま拒否を維持する。`unsupported_operand_types_are_42883` の `embedding % 2` の根拠はこの項である

## 4. 本書の対象外

次は PostgreSQL 17 が受理する形、または PostgreSQL 17 でも別の理由で拒否される形であり、本書では意図的な拒否として扱わない。

- PostgreSQL 17 が受理するが本実装が拒否する形
  - 小数リテラルどうしの剰余（`0.3 % 0.1`。PostgreSQL では numeric の剰余）は `0A000`
  - 値が 2^53 を超える BIGINT 被演算子（`where_remainder_large_bigint_operand_is_22003` の `(b + 1) % 2`）。PostgreSQL は整数のまま剰余を評価するが、本実装は f64 の精度を保てないため 22003 で拒否する
- PostgreSQL 17 でも乗算のオーバーフローで 22003 になる形: `where_remainder_hardens_wrapper_interiors` の SELECT 側のケース（`COALESCE(CAST(b*b AS bigint), 0) % 2` など）。fixture の `b = 4_000_000_000` では bigint の乗算が上限を超え、剰余の評価前に PostgreSQL 17 でも 22003 になる。本実装の 22003 は PostgreSQL と一致する挙動で、SQLSTATE の一致を固定しているのはこの SELECT 側に限る（同テストの WHERE 句のケースは本実装で 22003 と 0A000 のどちらも許容しており、SQLSTATE の一致までは固定しない）

上の形はいずれも注記の無いまま残しており、受け皿の Issue が無い（Issue #3528 の対象外節に記録した）。
