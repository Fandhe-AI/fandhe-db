# 剰余演算子 `%` の拒否の根拠（Issue #3528）

- ステータス: **Accepted**（設計記録。production コード・スクリプト・CI は無変更）
- 対応: Issue #3528（拒否固定テストの棚卸し）。注記書式は [rejection-pin-annotation.md](rejection-pin-annotation.md)
- 本書は private spec の本文を転記しない。PostgreSQL 17 の挙動は公開の挙動だけを書く

## 1. 背景

`%` は `* /` と同じ優先度の二項演算子として実装されている（実装記録は `implementation-status.md` の
SQL-42 の項）。本書は、`sql42_remainder_operator.rs` のテストが固定する拒否のうち、注記（`intentional:`）の根拠になるものだけを残す。

## 2. 意図的な拒否

| 拒否する形 | SQLSTATE | 固定するテスト | 維持する理由 |
| ---------- | -------- | -------------- | ------------ |
| `real`・`double precision` を被演算子に含む `%`（`r % 2`・`d % 2`・`2 % d`・`1.5::float8 % 2`・`n + r % 2`）。`COALESCE`・`NULLIF`・`CASE` で包んだ `float8` も同じ | `42883` | `unsupported_operand_types_are_42883` | PostgreSQL に浮動小数点の `%` 演算子は無い（PostgreSQL 17 で `operator does not exist` を確認）。f64 の `fmod` へ流すと PostgreSQL に無い結果を返すため拒否する。包んだ式でも同じ |
| `text % integer`（`s % 2`） | `42883` | 同上 | PostgreSQL 17 でも `operator does not exist`。暗黙の型変換で受理しない |
| ベクトル型の列に対する `%`（`embedding % 2`） | `42883` | 同上 | `%` はベクトルに対する定義が無く、fail-closed のまま拒否する。PostgreSQL 17 との比較はしていない（根拠にしない） |

- PostgreSQL 17 の確認は、公式の `postgres:17` コンテナで同じ形の文を実行した（使い捨て・資格情報なし）。確認したのは real・double precision・integer・text に限る
- 上の表以外の形（numeric・BIGINT の範囲検査など）の SQLSTATE は本書では断定しない。小数リテラルの剰余はこの例外で、期待値は `0A000` に固定される（`decimal_literal_remainder_is_rejected_not_approximated`）。これは未対応分で、受け皿 Issue #3534 を `tracked:` 注記で指しており、意図的な拒否ではないため根拠はここに書かない
