# 総走査行数上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1549・SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-scanned-rows <1-1000000>`
- 前提: `--max-dml-affected-rows` 等（`dml_limits_opt`）・スキーマ系 3 上限（[schema-limits-cli.md](schema-limits-cli.md)）と同じ「プロセス起動時に 1 回だけ注入する」方式

## 背景

1 文あたりの総走査行数上限 `tenant::MAX_SCANNED_ROWS`（1,000,000）はコンパイル時定数で、運用者が起動時に変えられなかった。大量の不可視行・不一致行に対する計算量 DoS を防ぐ資源上限であり、環境によってはより小さい値で運用したい。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限は現行の 1,000,000 のまま。設定できるのは**下げる方向だけ**（範囲 `1..=1,000,000`）。既定値はハード上限と同値で、フラグ未指定の挙動はビット同一 |
| D2 | フラグ名は `--max-scanned-rows`（`--max-dml-affected-rows`・`--max-insert-rows` と命名をそろえる） |
| D3 | 値の運搬は `schema_limits` と同型。`engine::scan_limits::ScanLimits`（フィールド非公開・ハード上限で飽和）を `EngineCore::with_scan_limits` で注入する。グローバル（`static`／`AtomicUsize`）は使わない。範囲検証は engine の `validate_max_scanned_rows` に一本化し、wire-server は CLI 文字列を厳密パースして渡すだけ |
| D4 | `--partitioned-dml-scan-budget` とは連動させず、起動時の組合せ検査も入れない（下記） |
| D5 | `TenantWriteError::TooManyRowsScanned` を `TooManyRowsScanned { max: usize }` へ変更（`TenantError` 側とそろえる）。`Display` が実効上限を表示するため。公開 enum の variant 形状変更（**破壊的変更**）。クライアントへ返す文言 `too many rows scanned`（`54000`）は不変 |
| D6 | `tenant::visible_rows` のシグネチャは変えない。上限を指定する `visible_rows_with_scan_limit` を追加し、core はこちらを呼ぶ |

## 効く経路

| 経路 | 内容 |
| ---- | ---- |
| 述語形 `UPDATE`／`DELETE`（RETURNING の有無を問わない） | `tenant::enumerate_dml_candidates` が対象テナント所有行だけを数える |
| 辞書スナップショット構築 | `core.rs` の可視行走査と write txn 内の走査 |
| ファイル形 `INSERT` の置換前走査 | `tenant::replace_typed_rows_by_text_key` |

NoSQL（HTTP）表層は同じ `EngineCore` を通るため、追加の配線なしで同じ値が効く。

## SQLSTATE（従来と同じ）

超過時は部分結果を返さず、write txn を commit せず破棄する（行・台帳とも痕跡ゼロ）。述語形 DML は `54000`。

## `--partitioned-dml-scan-budget` との関係

分割実行 DML のチャンクごとの走査予算（`scan_budget_rows`）は、原子経路の総走査上限の代わりにチャンク単位で走査を区切るための別機構（[partitioned-dml.md](partitioned-dml.md)）。本フラグは原子経路（1 文 1 txn）の総走査上限だけを決め、分割実行のチャンク予算は従来どおり `scan_budget_rows` が決める。予算の範囲上限 `MAX_PARTITIONED_SCAN_BUDGET_ROWS` もハード上限の定数を参照したままにする。

起動時の組合せ検査は入れない。入れると、`--max-scanned-rows 50000` だけを指定した場合に未指定の既定予算 100,000 と衝突して起動に失敗し、「未指定なら挙動不変」の原則と矛盾するため。

## 対象外

- ハード上限（1,000,000）の引き上げと全行走査（上限なし）の許可。DoS 面を広げる変更（1 txn で更新できる行数・候補 id の確保量が比例して増える）であり、オーナー確認が要る
- セッション単位の `SET` による変更
- SELECT・JOIN の走査上限（`storage::MAX_SCAN_TOTAL_ROWS` 等）の設定化
- TRUNCATE（設計上、総走査上限の対象外）
- `constraint.rs`（意図的に総走査上限を継承しない）
