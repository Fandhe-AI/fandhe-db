# JOIN リレーション数・入出力行数上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1551・SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-join-relations`・`--max-join-input-rows`・`--max-join-output-rows`
- 先行: [group-by-limits-cli.md](group-by-limits-cli.md)（Issue #1550）・[schema-limits-cli.md](schema-limits-cli.md)（Issue #1548）と同型

## 背景

JOIN のリレーション数（`sql::relation::MAX_TABLE_REFS` = 8）・1 辺の入力行数（`MAX_JOIN_INPUT_ROWS` = 100,000）・出力行数（`MAX_JOIN_OUTPUT_ROWS` = 100,000）はコンパイル時定数で、運用者が起動時に変えられなかった。プロセス単位の起動フラグとして設定できるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限は現行定数のまま。設定できるのは**下げる方向だけ**（範囲はリレーション数 `2..=8`・入力行数 `1..=100000`・出力行数 `1..=100000`）。既定値はハード上限と同値で、フラグ未指定の挙動は不変 |
| D2 | リレーション数の下限は 2（JOIN は 2 リレーション以上）。行数の下限は 1 |
| D3 | 値の運搬は `group_by_limits` と同型。`engine::join_limits::JoinSizeLimits`（フィールド非公開・範囲で飽和）を `EngineCore::with_join_limits` で注入する。内部実行用の `sql::join::JoinLimits` とは別型。グローバル状態は使わない。範囲検証は engine の `validate_*` に一本化 |
| D4 | 行数のハード上限定数は公開モジュール `join_limits` に置き、`sql::join` はそれを参照する。リレーション数は `sql::relation::MAX_TABLE_REFS` を参照し複製しない |
| D5 | 判定位置は実行段（`sql::join::execute_with_limits`）だけ。構文段・束縛段の `MAX_TABLE_REFS` 検査はアロケーション上限としてハード上限のまま。そのため拡張プロトコルでは Describe が成功し Execute で `54000` になる |
| D6 | `JoinBudget` のバイト予算（`MAX_ARENA_TOTAL_BYTES`）は変えない。下げる方向だけの設定なので、走査量・ハッシュ表・出力行の実体化量は単調に減り、バイト予算を超える新しい経路は生まれない |
| D7 | フラグ名は `--max-join-relations <2-8>`・`--max-join-input-rows <1-100000>`・`--max-join-output-rows <1-100000>`。2 回目以降の指定は fail-closed（last-wins にしない）。環境変数は設けない |
| D8 | 入力行数は RLS 適用後の可視行（WHERE プッシュダウン後）で数える。他テナントの行は数えない |

## 効く経路

`EngineCore` の `Statement::Join` アーム（`sql::join::execute` の唯一の呼び出し元）を通る全経路（SQL simple／extended・明示トランザクション内・ビュー本文の JOIN・`DECLARE CURSOR`）。

## SQLSTATE（従来と同じ）

リレーション数・入力行数・出力行数の超過とも `54000`。リレーション数の判定は走査より前に行いデータに依存しない（空テーブルでも拒否される）。エラー文には件数と上限値のみを含める。

## 対象外

- ハード上限の引き上げ（`BindingScope`・`relation_snapshot`・`generation_key`・relation マスク・`JoinBudget` の見直しとオーナー判断が必要）
- `MAX_JOIN_CONDITIONS`（ON 条件数）等、他の JOIN 構文上限の設定化
- セッション単位の `SET` による変更
- JOIN 以外の複数リレーション経路（集合演算・サブクエリ・CTE）のリレーション数上限
