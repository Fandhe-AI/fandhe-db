# トリガの再帰（入れ子）深さ上限と CLI（Issue #1727）

ポインタ: TABLE-45 (5)・SQL-46（spec 本文は転記しない）。実行モデルの D11
（[trigger-execution-model.md](trigger-execution-model.md)）の実装記録。

## 目的

トリガ本体の書き込みが別のトリガを発火させる入れ子（相互再帰・自己再帰を含む）を、
これまで一律 `0A000` で拒否していた（[trigger-after-insert.md](trigger-after-insert.md)）。
本 Issue で入れ子の発火を解禁し、同時に深さの上限を設けて、超過は `54001` で文全体を
fail-closed に拒否する。上限値は起動時 CLI `--max-trigger-depth` でのみ（下げる方向だけ）変えられる。

## 決定

| 項目 | 内容 |
| ---- | ---- |
| 深さの定義 | 外側の文が直接発火したトリガ呼び出しを深さ 1、そのトリガ内の書き込みが発火させたものを深さ 2 …とする。兄弟トリガ・複数行は累積しない |
| 範囲・既定 | `1..=16`、既定＝ハード上限＝16（**実装既定値であり spec 由来ではない**）。最小値 1 のため設定でトリガを黙って無効化できない |
| 検査点 | `trigger::runtime::TriggerRuntime::invoke` の 1 か所。BEFORE/AFTER × ROW/STATEMENT × INSERT/UPDATE/DELETE/TRUNCATE の全発火が通る。呼び出し口（`invoker`）は外から直接呼べない |
| 計数 | `trigger::depth` のスレッドローカルカウンタ＋RAII ガード。drop で戻すため、エラー・早期 return・panic でも漏れない。同一スレッドで同期的に実行される前提（`tenant::trigger_writes` の記録区間と同じ） |
| エラー | `54001`（`SqlSurfaceError::StatementTooComplex`）。固定文言 `trigger nesting depth limit exceeded`（値・表名・テナントを含めない） |
| 入れ子の解禁 | `TriggerWriteHandle` が外側の文の通過証を引き継ぎ、`insert_row` が同じ通過証で発火させる。通過証の無い（未結線の）経路は従来どおり `0A000` |
| 設定経路 | `EngineCore::with_trigger_limits`（所有権消費ビルダー・起動時 1 回）と `wire-server --max-trigger-depth` のみ。クエリ・セッション変数・HTTP からは変えられない |
| CLI の検証 | ASCII 数字のみ。範囲外・非数値・値欠落・重複指定は起動エラー（既定へ黙って読み替えない）。範囲検証は `engine::trigger_limits` が単一情報源 |

## PostgreSQL との差

PostgreSQL は専用の深さ上限を持たず、スタック深さ超過（`54001`）で止まる。本実装は
既定 16 の専用上限を持ち、スタックを使い切る前に決定的に止まる。深さ 16 の循環が
2 MiB スタックのスレッドに収まることをテストで固定している。

## OWASP 観点

- 不安全な設計（DoS）: 循環トリガの無限再帰とスタック枯渇を `54001` で遮断。行ごと・呼び出しごとの
  割り込み（`57014`）は従来どおり
- fail-open の回避: 超過は文全体を拒否し副作用を残さない。未結線経路は `0A000` のまま
- テナント境界: 入れ子の書き込みも呼び出しテナントの `PolicyContext` を借用し、差し替え不可
- 情報漏えい: エラーは固定文言のみ

## 対象外

- トリガ呼び出しを関数呼び出し深さ（`--max-function-call-depth`）へ 1 として数える件（#2228）
- トリガ内からの UPDATE／DELETE の窓口（#2226／#2228。深さ検査は `invoke` 単一点のため自動で効く）
- 自己変更の衝突、台帳整合と crash-test（#1728）
