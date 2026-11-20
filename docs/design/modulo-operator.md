# 剰余演算子 `%` の拒否の根拠（Issue #3528）

- ステータス: **Accepted**（設計記録。production コード・スクリプト・CI は無変更）
- 対応: Issue #3528（拒否固定テストの棚卸し）。注記書式は [rejection-pin-annotation.md](rejection-pin-annotation.md)
- 本書は private spec の本文を転記しない。PostgreSQL 17 の挙動は公開の挙動だけを書く

## 1. 背景

`%` は `* /` と同じ優先度の二項演算子として実装されている（実装記録は `implementation-status.md` の
SQL-42 の項）。整数どうしの剰余だけを評価し、それ以外の型の組み合わせは束縛時に拒否する。
本書は、拒否のうち PostgreSQL 17 と同じ結果になるものの根拠を残す。

## 2. 意図的な拒否（PostgreSQL 17 と同じ）

| 拒否する形 | 本実装の SQLSTATE | PostgreSQL 17 の実測 | 維持する理由 |
| ---------- | ----------------- | -------------------- | ------------ |
| `real % integer`・`double precision % integer`・`integer % double precision` | `42883` | `42883`（`operator does not exist: double precision % integer` など） | PostgreSQL に浮動小数点の `%` 演算子は無い。f64 の `fmod` へ流すと PostgreSQL に無い結果を返すため、同じく拒否する |
| `text % integer` | `42883` | `42883`（`operator does not exist: text % integer`） | 型の組み合わせに演算子が無い。暗黙の型変換で受理しない |
| `COALESCE`・`NULLIF`・`CASE` で包んだ `double precision` の `%` | `42883` | `42883`（結果型が `double precision` のまま演算子の解決に失敗する） | 包んだ式でも直接の `double precision` と同じ結果型の規則で拒否する。ラッパーで型を隠して通す経路を作らない |

- 上の表は `docs/design` に残す根拠であり、テストは `sql42_remainder_operator.rs` の
  `unsupported_operand_types_are_42883` が固定する
- PostgreSQL 17 の実測は、公式の `postgres:17` コンテナで同じ形の文を実行して確かめた（使い捨て・資格情報なし）。上の表は比較できた型（real・double precision・integer・text）に限る

## 3. 本書の対象外

次は PostgreSQL 17 では受理される形であり、本書では意図的な拒否として扱わない。

- 小数リテラルどうしの剰余（`0.3 % 0.1`。PostgreSQL では numeric の剰余）は `0A000`
- 精度を保てない整数式のラッパー（`22003`）

上の 2 件はいずれも注記の無いまま残しており、受け皿の Issue が無い（Issue #3528 の対象外節に記録した）。

ベクトル型の列に対する `%`（`42883`）は PostgreSQL 17 との比較ができないため、上の表に含めず設計上の対象外として記録する。標準の PostgreSQL 17 に当該型が無く、拡張型も未実測である。検索用の型に算術演算子を持ち込まない方針で拒否を維持する。
