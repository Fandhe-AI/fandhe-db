# 明示 LIMIT 上限（結果行数上限）の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1555・SQL-34・SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-result-rows`
- 先行: [join-limits-cli.md](join-limits-cli.md)（Issue #1551）・[group-by-limits-cli.md](group-by-limits-cli.md)（Issue #1550）と同型

## 背景

明示 `LIMIT n` の範囲上限はコンパイル時定数 `core::MAX_SEARCH_K`（10,000）で固定されていた。これを起動時の CLI フラグで上げ下げ両方向に設定できるようにする。同じフラグは、`LIMIT` 省略時の結果行数上限（`54000`）を導入する後続作業も共有する想定（`EngineCore::result_limits()` と `result_limits_opt::MAX_RESULT_ROWS_FLAG` を再利用する）。

## 決定

| # | 内容 |
| - | ---- |
| D1 | 範囲は `1..=100000`、既定は 10,000（従来と同値でフラグ未指定の挙動は不変）。兄弟フラグと違い**上げる方向も**設定できる。ハード上限 100,000 は暫定値（オーナー判断で変更可） |
| D2 | ハード上限の根拠: hybrid の `MAX_POOL_DEPTH` を同値まで引き上げても `MAX_FETCH_K`（4 倍）で有界、走査結果のメモリは `MAX_SCAN_RESULT_BYTES`（バイト上限）で別途有界、kNN の `TopKSelector` は事前確保せず k 件まで成長、JOIN の行数ハード上限と桁を揃える。いずれも参照ではなくリテラル定数で持ち、他定数と連動させない（SQL-46 (6)） |
| D3 | 値の運搬は `join_limits` と同型。`engine::result_limits::ResultLimits`（フィールド非公開・範囲で飽和）を `EngineCore::with_result_limits` で起動時に 1 回注入する。グローバル状態は使わない（並列テスト干渉の回避）。範囲検証は `validate_max_result_rows` に一本化し、wire-server は厳密パース（ASCII 数字のみ）して渡すだけ |
| D4 | 既存 `pub` API のシグネチャは変えない。`parser::validate_search_limit`・`bind_in_session`・`bind_scan` は既定値（10,000）で委譲する shim として残し、`validate_search_limit_with`・crate 内の `*_with_limits` 変種を追加した |
| D5 | 多層防御: NoSQL など既定値で束縛した計画が `EngineCore` の束縛済み計画入口（`execute_bound_search_in_session`・`execute_bound_scan_in_session`・`explain_bound_*`）に渡された場合も、入口で `bound.limit <= 設定値` を再検査し超過は `22000`（設定を下げた構成での fail-open を防ぐ）。評価後射影形ビューの外側 `LIMIT` も構文段が既定値でしか検査できないため同様に再検査する |
| D6 | 上げる方向の下流整合: `hybrid::MAX_POOL_DEPTH` を 100,000（リテラル）へ引き上げ、`sql::exec` のコンパイル時 assert を `MAX_RESULT_ROWS_HARD_CAP <= MAX_POOL_DEPTH` に更新した（`LIMIT 10001` 以上の hybrid が `XX000` にならない）。`MAX_BOOST_IDS` は 10,000 のリテラルで据え置き。HNSW は `k > MAX_EF`（10,000）のとき既存の `ef_cap_fallbacks` 経路（Issue #410）で総当たりへ縮退するため、`MAX_EF` は変えない |
| D7 | `MAX_SEARCH_K`（10,000）自体は内部の固定防御上限として残す（`OFFSET`・集合演算の枝行数 `MAX_SET_OP_ROWS`〔リテラル 10,000 へ置換〕・サブクエリ内側行数・precision の取得上限）。これらは設定値と連動しない |
| D8 | フラグ名定数 `MAX_RESULT_ROWS_FLAG` は `wire_server::result_limits_opt` を唯一の定義とし、2 回目以降の指定は fail-closed（last-wins にしない）。環境変数は設けない |

## 効く経路

| 経路 | 設定値 |
| ---- | ------ |
| 走査・kNN・hybrid・precision の `LIMIT`（simple／extended・Describe） | 効く |
| `DECLARE CURSOR` の内側 `LIMIT`・`FETCH n` | 効く |
| 集合演算全体の `LIMIT`・JOIN 文の `LIMIT`・評価後射影形ビューの外側 `LIMIT` | 効く |
| `USING PLAN` の前段検証（LLM 呼び出しより前に拒否） | 効く |
| NoSQL（HTTP）`search`／`scan`／`explain` の `limit` | 効く |
| `OFFSET` の範囲（`0..=10000`） | 連動しない |
| 集合演算の枝 `LIMIT`・サブクエリ／CTE／ビュー本文内側の `LIMIT` | 連動しない（既定の 10,000 のまま。各々の行数上限が別軸で有界） |
| 集計（`GROUP BY`）の `LIMIT` 範囲 | 連動しない |

## SQLSTATE（従来と同じ）

範囲外の `LIMIT` は `22000`（メッセージの範囲表示だけが設定値に追随する）。`LIMIT 0` の扱いは従来どおり拒否（下限は 1）。

## 対象外

- `LIMIT 0`・`LIMIT ALL`・`LIMIT` 省略の受理、結果行数上限（`54000`）の適用（別 Issue。本フラグ・アクセサを再利用する）
- `OFFSET` 範囲の設定化、集計の `LIMIT` 範囲の設定値連動
- セッション変数・テナント単位・環境変数での設定
- 直接 Rust API（`EngineCore::search`・`PrefilterIndex::search`）の `k` 上限（`MAX_SEARCH_K` のまま）
