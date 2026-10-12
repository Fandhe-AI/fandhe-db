# スキーマ定義系 3 上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1548・SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-table-columns`・`--max-array-elements`・`--max-enum-labels`
- 前提: 列数上限の定数は Issue #1518 で `catalog::MAX_COLUMN_COUNT` に一本化済み（[column-count-limit-consolidation.md](column-count-limit-consolidation.md)）

## 背景

1 テーブルの列数（256）・配列列 1 個の要素数（1,024）・ENUM 型 1 個のラベル数（256）はコンパイル時定数で、運用者が起動時に変えられなかった。`--max-dml-affected-rows` 等（`dml_limits_opt`）と同じ方式で、プロセス単位の起動フラグとして設定できるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限は現行定数のまま。設定できるのは**下げる方向だけ**（範囲は列数 `1..=256`・配列要素 `1..=1024`・ラベル `1..=256`）。既定値はハード上限と同値で、フラグ未指定の挙動は不変 |
| D2 | 設定値は「新しく定義が増える操作」の実行段（`sql::ddl`）にだけ効かせる。カタログ decode・`validate_schema`・ENUM decode・`ArrayType::new` 経由の復元・`row_codec` の要素数検査には入れない |
| D3 | 判定位置は構文段ではなく実行段。構文段の上限（INSERT・COPY・CREATE INDEX・CREATE TABLE の列リスト、`CREATE TYPE` のラベル）はアロケーション上限のためハード上限のまま。SQL と NoSQL は同じ実行器を通るので 1 か所で両方を網羅する |
| D4 | ADD COLUMN は write txn 内（`Storage::alter_table_add_column_with_limit`）で物理スロット数（生存列＋墓標）と比較する。事前チェックでは並行 ADD COLUMN が上限を 1 超える余地が残るため |
| D5 | `<型>[]`（要素数省略）は設定値を要素数上限にする。構文段が省略情報を失わないよう `ValidatedCreateTable::array_len_defaulted` で実行段へ運ぶ。ADD COLUMN・ALTER COLUMN TYPE は `to_static_column_type_with_array_default` に設定値を渡す |
| D6 | 値の運搬は `DmlLimits` と同型。`engine::schema_limits::SchemaLimits`（フィールド非公開・ハード上限で飽和）を `EngineCore::with_schema_limits` で注入する。範囲検証は engine の `validate_*` に一本化し、wire-server は CLI 文字列を厳密パースして渡すだけ |

## SQLSTATE（従来と同じ）

| 操作 | 超過時 |
| ---- | ------ |
| CREATE TABLE の列数 | `54000` |
| ADD COLUMN（物理スロット数が上限以上） | `54000` |
| CREATE TYPE AS ENUM のラベル数 | `54000`（storage 呼び出しより前に判定。後だと `42601` に写像されるため） |
| 配列 `[N]`（CREATE TABLE・ADD COLUMN・ALTER COLUMN TYPE） | `42601` |

## 既存データの保護

上限を下げて再起動しても、すでに設定値を超える列数・要素数・ラベル数を持つテーブル・型は読み出し・INSERT・DROP COLUMN 等がこれまでどおり動く。設定値を decode 経路に入れると `CorruptSchema` で読めなくなる（可用性の喪失）ため、設定値は定義が増える操作にだけ効かせる。

## 上限を引き上げる場合の障害（対象外）

- カタログ値長の静的アサーション（`MAX_COLUMN_DEFAULT_LEN * 2 * MAX_COLUMN_COUNT + 500_000 <= MAX_CATALOG_VALUE_LEN`）の余裕は約 267 列まで
- EXPLAIN のビットセット（`FILTER_COLS_WORDS`・`FilterColumnSet::CAPACITY`）は `MAX_COLUMN_COUNT` から導出したコンパイル時の値
- `MAX_ARRAY_ELEMENTS`／`MAX_ENUM_LABELS` は decode 時のアロケーション上限を兼ね、ENUM 型の blob 上限（64 KiB）にも制約される

## 対象外

- ハード上限の引き上げ（資源上限の変更はオーナー判断が必要）
- `Storage::alter_enum_type_add_value`（Rust API 専用。SQL・NoSQL に公開されていない）への設定値の適用
- ラベル長（63。PG と同じ値）の設定化
