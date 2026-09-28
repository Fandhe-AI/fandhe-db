# `VECTOR` 列を持たないテーブルへの INSERT 系書き込みを受理する（Issue #995）

- ステータス: Implemented
- 対応: Issue #995（親 #898・PR #993 レビューで見つかったスコープ外事項）
- 関連ポインタ: TABLE-1・TABLE-12・SQL-10・SQL-16・SQL-20・#899（`CREATE TABLE`
  で `VECTOR` 列なしテーブルを作れるようになる Phase 3）。spec 本文は転記しない
  （[spec-confidentiality](../../.claude/rules/spec-confidentiality.md)）
- 検証コード: `crates/engine/src/catalog.rs`（`TableSchema::validate_row_embedding_dim`
  単体テスト）・`crates/engine/tests/insert_without_vector_column.rs`（結合テスト）・
  `crates/engine/tests/sql_aggregate.rs`（非空コーパスでの集計）・
  `crates/engine/tests/scalar_index_aggregate.rs`（非空コーパスでも plain scan の
  まま不変であることの確認）

## 背景

`TableSchema::validate_embedding_dim`（TABLE-1）は `VECTOR` 列を持たないスキーマで
常に `Err` を返す契約を持つ。INSERT 系の書き込み入口（行形・バッチ形・型付き行・
UPSERT）はいずれもこれを無条件に呼んでいたため、`VECTOR` 列を持たないテーブルへは
（読み取り経路・集計経路が既に対応しているにもかかわらず）行を 1 件も書き込めな
かった。単一行 UPDATE（PR #989）・述語つき UPDATE（PR #993）は「`VECTOR` 列への
SET があった場合のみ次元検証する」形にすでに揃えていたが、INSERT 系は未対応の
ままだった。

## 契約

**`VECTOR` 列を持たないテーブルでは、embedding は空（`dim == 0`）のときのみ受理し、
非空 embedding は引き続き fail-closed に拒否する。** 検証を省略するのではなく、
既存の読み取り経路（`sql/scan.rs`・`sql/aggregate.rs` の `expected_dim: Option<u32>`・
`storage::decode_row*`）がすでに採用している「dim 0 の行として扱う」モデルへ
INSERT 系を揃える設計とした。

`VECTOR` 列を持つスキーマでの次元検証・エラー分類（`wire_code`）は一切変えない
（`catalog.rs::TableSchema::validate_row_embedding_dim` が `VECTOR` 列ありの場合は
既存の `validate_embedding_dim` へそのまま委譲する）。

## 対象入口

| 層 | 関数 | 変更内容 |
| -- | ---- | -------- |
| `catalog.rs`（テスト専用の生書き込み経路） | `Storage::insert_row_into_table`／`insert_rows_into_table`／`insert_typed_row` | `validate_embedding_dim` → `validate_row_embedding_dim`。`insert_typed_row` は `VECTOR` 列位置を `Option<usize>` 化し `None` なら embedding を空にする |
| `tenant.rs`（行形） | `insert_row_unchecked`／`insert_rows_unchecked` | 同上の置き換えのみ |
| `tenant.rs`（型付き行） | `insert_typed_row_unchecked`／`insert_typed_rows_unchecked` | `vector_idx: Option<usize>` 化・`None` なら embedding 空・`named_columns` フィルタの比較を `Some(*idx) != vector_idx` に変更 |
| `tenant.rs`（UPSERT） | `upsert_typed_rows_unchecked` | 同上（`vector_idx` の `Option` 化・新規挿入分岐と `DO UPDATE` の列照合 `Some(*col_idx) == vector_idx` の両方） |

## 対象外（明示的にスコープ外）

- **ファイル形 `INSERT`**（`path`/`body` 列指定。`tenant::replace_typed_rows_by_text_key`）:
  束縛層 `sql::parser::bind_file_insert` が `VECTOR` 列を必須として束縛時点で
  `22000` 拒否するため、実行本体（`replace_typed_rows_by_text_key`）へは
  `VECTOR` 列なしテーブルの要求が到達しない。サーバー側 `Embedder` によるベクトル
  生成を前提とする設計であるため対象外とした。
- **`tenant::update_row_unchecked`**（`RowInput` による行全体置換 UPDATE）: 本 Issue
  の時点では `schema.validate_embedding_dim` を無条件に呼ぶ同型の制約を持ち
  スコープ外としていたが、Issue #1079 で `validate_row_embedding_dim` への
  置き換えにより解消済み。
- **NoSQL 表層 `insert` op**（`wire-server::http::query::insert`）: engine 側の
  `tenant::insert_typed_row(s)_unchecked` をそのまま呼ぶため production コードの
  変更は不要。結合テストは `crates/wire-server/tests/nosql6_insert.rs` へ追加可能
  だが、production 経路は本 Issue の対象変更を経由して既に動作する。
