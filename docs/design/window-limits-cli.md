# 窓キー数上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1900・SQL-46・SQL-64 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-window-keys`
- 先行: [nesting-limits-cli.md](nesting-limits-cli.md)（Issue #1552）・[group-by-limits-cli.md](group-by-limits-cli.md)（Issue #1550）と同型

## 背景

ウィンドウ項目の `PARTITION BY` と `ORDER BY` のキー数上限はコンパイル時定数
`sql::allowlist::MAX_WINDOW_KEYS`（8）で、運用者が起動時に変えられなかった。
[window-functions.md](window-functions.md) の上限一覧にある値を、プロセス単位の起動フラグとして
**下げる方向だけ**設定できるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限は現行定数のまま（既定値と同値）。設定できるのは下げる方向だけで、範囲は `1..=8`。フラグ未指定の挙動は不変。引き上げは対象外 |
| D2 | フラグは `--max-window-keys <1-8>` の 1 つ。`PARTITION BY` と `ORDER BY` は 1 つの定数を共有していたため、1 つの値で両方を判定する |
| D3 | 判定は構文解析段（`Parser::parse_window_spec`）。名前付きウィンドウもパース前にインライン `OVER (...)` へ展開されるため、実行段に判定場所がない。拡張プロトコルでは Parse の段で `54000` になる |
| D4 | 値の運搬は `nesting_limits` と同型。`engine::window_limits::WindowLimits`（`Copy`・フィールド非公開・ハード上限で飽和）を `EngineCore::with_window_limits` で起動時に 1 回注入する。入れ子パーサー（サブクエリ・派生表・CTE 本文）へは、既存の `NestingLimits` に crate 内専用の運搬フィールドを足して届ける（約 50 の関数シグネチャを変えないため）。`with_nesting_limits` と `with_window_limits` はどちらの順で呼んでも同じ結果になるよう再合成する。範囲検証は engine の `validate_max_window_keys` に一本化 |
| D5 | 設定値が効く経路: セッション SQL（simple／extended の Parse・`execute_sql`・`DECLARE CURSOR` の内側）のパース、およびサブクエリ・派生表・CTE 本文の入れ子パーサー |
| D6 | ハード上限のまま残すもの: 参照されない `WINDOW w AS (...)` 定義の検証（`Parser::new` の既定）、CHECK・索引式など既定パーサーの入口。参照された定義は展開後に本パーサーが設定値で判定する。ビュー本文は窓を受理しない。いずれもハード上限は効くため資源上限は維持される |
| D7 | SQLSTATE は従来どおり `54000`。エラーメッセージの文言も変えない（`too many PARTITION BY columns`／`too many ORDER BY keys`）。データ・テナント情報は含めない |
| D8 | 2 回目以降の指定は fail-closed（last-wins にしない）。値は ASCII 10 進のみ厳密にパースし、先頭 `+`・空白・全角数字・空文字・範囲外・桁あふれは起動エラー |

## SQLSTATE（従来と同じ）

超過は `54000`。判定は文の形（キー数）のみで決まり、データや他テナントの存在情報には依存しない。

## 対象外

- `MAX_WINDOW_PARTITIONS`・`MAX_WINDOW_ROWS`・`MAX_WINDOW_FRAME_ROWS`・`MAX_WINDOW_STATE_BYTES` の設定化
- ハード上限の引き上げ
- セッション単位の `SET`
