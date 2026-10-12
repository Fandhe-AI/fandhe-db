# サブクエリ深さ・実行回数・CTE 上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1552・SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-subquery-depth`・`--max-subquery-executions`・`--max-cte-definitions`・`--max-cte-nesting-depth`
- 先行: [schema-limits-cli.md](schema-limits-cli.md)（Issue #1548）・[group-by-limits-cli.md](group-by-limits-cli.md)（Issue #1550）と同型

## 背景

次の 4 上限はコンパイル時定数で、運用者が起動時に変えられなかった。

| 対象 | 定数 | 現行値 |
| ---- | ---- | ------ |
| サブクエリの入れ子深さ | `sql::allowlist::MAX_SUBQUERY_DEPTH` | 4 |
| 1 文あたりの内側サブクエリ実行回数 | `sql::subquery::MAX_SUBQUERY_EXECUTIONS` | 16 |
| CTE 定義数 | `sql::cte::MAX_CTE_DEFINITIONS` | 16 |
| CTE→CTE 連鎖の深さ | `sql::cte::MAX_CTE_NESTING_DEPTH` | 4 |

深さは再帰とスタックに直結し、実行回数は「内側を束縛前に実行する」方式（[sql-subquery.md](sql-subquery.md)）のコストに直結する。`--max-dml-affected-rows` 等と同じ方式で、プロセス単位の起動フラグとして**下げる方向だけ**設定できるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限は現行定数のまま（既定値と同値）。設定できるのは下げる方向だけで、範囲は深さ `1..=4`・実行回数 `1..=16`・CTE 定義数 `1..=16`・CTE 連鎖深さ `1..=4`。フラグ未指定の挙動は不変。引き上げは対象外 |
| D2 | 値の運搬は `group_by_limits` と同型。`engine::nesting_limits::NestingLimits`（`Copy`・フィールド非公開・ハード上限で飽和）を `EngineCore::with_nesting_limits` で起動時に 1 回注入する。グローバル状態は使わない（並列テストの干渉を避ける）。範囲検証は engine の `validate_*` に一本化し、wire-server は CLI 文字列を厳密パースして渡すだけ |
| D3 | `GROUP BY` 上限（実行段だけで判定）とは異なり、**構文解析段でも設定値で判定する**。CTE はパース時に `ValidatedScan` へ展開され、サブクエリ深さも `Parser::require_subquery_depth` が判定するため、実行段に判定できる場所がない。そのため拡張プロトコルでも Parse の段で `54000` になる |
| D4 | ハード定数のまま残す構造ガード: スカラーサブクエリ項目数の構文段上限、`qualified_column` のスコープ深さガード、`MAX_CTE_REFERENCES`（`MAX_CTE_NESTING_DEPTH + 1` から導いた値）。設定値による判定はこれらとは別に追加し、設定値は常にハード上限以下 |
| D5 | 設定値が効く経路: セッション SQL（simple／extended の Parse・`execute_sql`・`DECLARE CURSOR ... FOR <query>` の内側）のパース、およびサブクエリ解決（実行回数予算・内側の再検証）。評価後射影形ビューの本文実行を通る 2 段目以降のサブクエリにも効く |
| D6 | **格納済みビュー本文の再パースはハード上限のまま**（`CREATE VIEW` の構造検証・参照時の再検証）。上限を下げて再起動しても既存ビューの定義検証は壊れない（schema-limits の「カタログ decode には適用しない」と同じ考え方）。したがってビュー本文の中の CTE 定義数・CTE 連鎖深さには設定値が効かない。ハード上限は効くので資源上限は維持される |
| D7 | フラグ名は `--max-subquery-depth <1-4>`・`--max-subquery-executions <1-16>`・`--max-cte-definitions <1-16>`・`--max-cte-nesting-depth <1-4>`。2 回目以降の指定は fail-closed（last-wins にしない） |
| D8 | エラーメッセージには設定値を出す（例: `subquery nesting exceeds limit 2`）。SQLSTATE は従来どおり `54000` |

## 効く経路

| 上限 | 判定位置 |
| ---- | -------- |
| サブクエリ深さ | `Parser::require_subquery_depth`（構文解析段）。内側の再検証でも `validate_sql_tokens_with_subquery_ctx_limits` が同じ設定値で判定 |
| 実行回数 | `EngineCore` が予算を設定値で初期化し、`sql::subquery` の `execute_inner_query_with_meta` が消費（WHERE の `IN`／`EXISTS`／スカラー比較と投影位置のスカラーサブクエリ） |
| CTE 定義数 | `Parser::parse_with_clause` → `cte::check_definition_count` |
| CTE 連鎖深さ | `cte::resolve_relation`（`WITH` 分岐が設定値を渡す） |

## SQLSTATE（従来と同じ）

いずれの超過も `54000`。判定は構文・文の形のみで決まり、データや他テナントの存在情報には依存しない。

## 対象外

- ハード上限（深さ 4・実行回数 16・CTE 定義数 16・CTE 連鎖深さ 4）の引き上げ。深さは再帰とスタック、`MAX_CTE_REFERENCES` は `MAX_CTE_NESTING_DEPTH + 1` から導いた値で、`MAX_CTE_NESTING_DEPTH` は `catalog::MAX_VIEW_NESTING_DEPTH` と同値に揃えてあり、オーナー判断が必要
- 格納済みビュー本文の中の CTE 上限に設定値を効かせること（D6）
- `MAX_SUBQUERY_IN_VALUES`・`MAX_CTE_REFERENCES`・VIEW のネスト上限の設定化
- セッション単位の `SET` による変更
