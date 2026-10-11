# 列数上限定数の統合と行コーデック列数幅の調査

## ステータス・対象

- ステータス: 採用（Issue #1518・SQL-46 のポインタのみ。spec 本文は転記しない）

## 背景

テーブル列数の上限 256 が `catalog::MAX_COLUMN_COUNT` のほか、`sql::allowlist` の 3 定数・`catalog` の別名定数・EXPLAIN のビットセット長に重複していた。片方だけ変えると構文段とカタログ段の上限がずれるため、1 つに集約した。

## 決定

| # | 内容 |
| - | ---- |
| D1 | `allowlist.rs` の `MAX_INSERT_COLUMNS`・`MAX_INDEX_DDL_COLUMNS`・`MAX_CREATE_TABLE_COLUMNS` を削除し、`MAX_COLUMN_COUNT` を直接参照する（比較演算子・判定位置は不変） |
| D2 | `catalog.rs` の別名 `MAX_INDEX_DEF_COLUMNS` も削除する |
| D3 | `sql/explain.rs` の `FILTER_COLS_WORDS` を `MAX_COLUMN_COUNT.div_ceil(64)` から導出し、`FilterColumnSet` の配列長・`CAPACITY` も追従させる。`insert` の範囲外判定は `CAPACITY` 基準に揃え fail-closed を保つ |
| D4 | カタログ値長の静的アサーションは既に `MAX_COLUMN_COUNT` から導出された式のためコード変更なし |
| D5 | 列数ではない 256（`MAX_UPDATE_SET_ASSIGNMENTS`・`MAX_METADATA_FILTERS`・`MAX_IN_LIST_ITEMS`・`MAX_ENUM_LABELS` など）は意味が異なるため変更しない |

## 調査結果: 行コーデックの列数エンコード幅

- `row_codec` のフル行形式（v1）とスカラーペイロードは列数フィールドを持たず、列の並びは `TableSchema` から決まる（スロットごとに presence 1 バイト＋値。削除済み列は NULL）。`storage` の行形式 v2 も行内に列数を持たない
- カタログ側の列数はテキスト形式の `cols:<10進数>` で、デコード時に `MAX_COLUMN_COUNT` 超過を確保前に拒否する。固定幅整数ではない
- `catalog` の `to_le_bytes`／`from_le_bytes` は ENUM ラベル数（u16）で列とは無関係
- 結論: 行形式に 255/256 を上限とする幅の制約はなく、上限を 256 より上げても行形式の版上げは不要

## 上限を上げる場合の確認事項

- カタログ静的アサーション（`MAX_COLUMN_DEFAULT_LEN * 2 * MAX_COLUMN_COUNT + 500_000 <= MAX_CATALOG_VALUE_LEN`）の余裕は、`(1_048_576 - 500_000) / 2048` ≒ 267 列まで。これを超えるなら `MAX_CATALOG_VALUE_LEN` か余裕分の見直しが必要
- `MAX_SCALAR_PAYLOAD_LEN`（4 MiB）の累計上限は値が変わらず、1 列あたりの余裕が減る
- EXPLAIN のビットセットは導出式により自動追従する
- pg wire の RowDescription／DataRow のフィールド数は i16（最大 32767）で制約にならない

## 対象外

- 上限 256 自体の引き上げ
- `crates/engine/tests/` の結合テスト中のリテラル（`pub(crate)` を参照できないため）
