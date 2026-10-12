# GROUP BY 列数・グループ数上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1550・SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-group-by-columns`・`--max-groups`
- 先行: [schema-limits-cli.md](schema-limits-cli.md)（Issue #1548）と同型

## 背景

`GROUP BY` のキー列数（`sql::allowlist::MAX_GROUP_BY_COLUMNS` = 8）とグループ数（`sql::group_by::MAX_GROUPS` = 10,000）はコンパイル時定数で、運用者が起動時に変えられなかった。`--max-dml-affected-rows` 等と同じ方式で、プロセス単位の起動フラグとして設定できるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限は現行定数のまま。設定できるのは**下げる方向だけ**（範囲は列数 `1..=8`・グループ数 `1..=10000`）。既定値はハード上限と同値で、フラグ未指定の挙動は不変。列数は実行段の固定長スタック配列の長さ、グループ数はウィンドウ・集計 `LIMIT` 範囲・結果バイト見積りの前提でもあるため、引き上げは対象外 |
| D2 | `MAX_GROUP_KEY_TOTAL_BYTES` などのバイト予算は変えない。保持メモリ（グループキー累計・TEXT 集計状態・`ResultBudget` のグループ数比例分）はグループ数に対して単調増加なので、下げる方向の設定でバイト予算を超えることはない |
| D3 | 値の運搬は `schema_limits` と同型。`engine::group_by_limits::GroupByLimits`（フィールド非公開・ハード上限で飽和）を `EngineCore::with_group_by_limits` で注入する。グローバル状態は使わない（並列テストの干渉を避ける）。範囲検証は engine の `validate_*` に一本化し、wire-server は CLI 文字列を厳密パースして渡すだけ |
| D4 | 判定位置は実行段だけ。構文段の `check_group_by_column_count` と束縛段はアロケーション上限としてハード上限のまま。そのため設定値による拒否は束縛エラーより後に起きる（拡張プロトコルでは Describe が成功し Execute で `54000`） |
| D5 | `SELECT DISTINCT` は脱糖後に同じ集計実行器を通るため、列数・グループ数とも設定値が効く |
| D6 | ハード上限のまま変えないもの: 集計の `LIMIT` 範囲、ウィンドウ関数のパーティション上限（`MAX_WINDOW_PARTITIONS`）、NoSQL HTTP の `group_by` 配列長の構文検査 |
| D7 | フラグ名は `--max-group-by-columns <1-8>`・`--max-groups <1-10000>`。2 回目以降の指定は fail-closed（last-wins にしない） |

## 効く経路

| 経路 | 注入点 |
| ---- | ------ |
| 単一表の集計 SELECT（SQL simple／extended・NoSQL HTTP の集計・`DECLARE CURSOR` の内側） | `EngineCore::run_aggregate_plan` → `execute_aggregate_with_group_limits` → `execute_grouped_aggregate` |
| 集合演算の集計枝 | `set_op::execute` → `eval_aggregate_branch` |
| サブクエリ内側の集計 | `subquery::execute_inner_aggregate_statement`（配線済み。現行の構文ではスカラー形の内側に `GROUP BY` を置けないため、到達する文は現時点で無い） |
| ビュー本文に対する外側の集計（バッファ形） | `view_buffered::apply_outer` → `GroupedRowAccumulator::new` |
| JOIN 結果の集計 | `join::execute` の `JoinLimits { max_groups, max_group_by_columns }` |

## SQLSTATE（従来と同じ）

列数超過・グループ数超過とも `54000`。列数の判定は走査より前に行い、データに依存しない（空テーブルでも拒否される）。グループ数は RLS 適用後の可視行だけで数える。

## 対象外

- ハード上限（8 列・10,000 グループ）の引き上げ。スタック配列・バイト予算（`MAX_GROUP_KEY_TOTAL_BYTES`・`MAX_TEXT_ACCUMULATOR_TOTAL_BYTES`）・`MAX_WINDOW_PARTITIONS`・集計 `LIMIT` 範囲の見直しとオーナー判断が必要
- ウィンドウ関数のパーティション上限の設定化
- 集計 `LIMIT` 範囲を設定値に連動させること
- セッション単位の `SET` による変更
