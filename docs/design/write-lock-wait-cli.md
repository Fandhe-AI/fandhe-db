# 書き込みロック待機上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1554・SQL-46 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--write-lock-wait-ms`
- 先行: [join-limits-cli.md](join-limits-cli.md)（Issue #1551）・[explicit-transaction.md](explicit-transaction.md)（待機上限の既定値）と同型

## 背景

書き込みロック（単一ライタの `writer_gate`）の待機上限は `engine::storage::DEFAULT_WRITE_LOCK_WAIT`（30 秒）に固定されており、運用者が起動時に変えられなかった。超過は `StorageError::WriteLockTimeout`（SQLSTATE `55P03`）になる。プロセス単位の起動フラグとして設定できるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | フラグ名は `--write-lock-wait-ms <100-60000>`（ミリ秒）。兄弟フラグと同じく単位を接尾辞にする |
| D2 | 範囲は `MIN_WRITE_LOCK_WAIT_MS` = 100 から `MAX_WRITE_LOCK_WAIT_MS` = 60,000（ハード上限。独立したリテラル定数）。既定 30 秒は範囲内で、上下どちらへも設定できる。下限・上限は暫定値で**オーナー確認待ち** |
| D3 | 0 は受け付けない（「待たない」「無制限」のどちらにも読めるため） |
| D4 | 範囲の強制は CLI 経路（`validate_write_lock_wait_ms`）だけ。既存の `Storage::with_write_lock_wait` には範囲検査を入れない（既存テストが範囲外の値を直接渡すため） |
| D5 | 注入は `EngineCore::with_write_lock_wait`（`Storage::set_write_lock_wait` に委譲）。`EngineCore` から `Storage` を取り出す経路は設けず、読み取りは `write_lock_wait()` のみ |
| D6 | `main.rs` は未指定でも常に注入する（未指定は `DEFAULT_WRITE_LOCK_WAIT` で、未注入とビット同一） |
| D7 | 重複指定・値欠落・範囲外・非数値は fail-closed で起動エラー（bind より前に解決）。環境変数・セッション単位 `SET`・テナント単位の設定は設けない |
| D8 | `Instant::now() + timeout` は上限 60 秒ならオーバーフローしない |

## 効く経路

autocommit の書き込み、明示トランザクションの `BEGIN`、分割実行 DML のチャンク（いずれも同じ `write_lock_wait`）。SQL 表層と NoSQL（HTTP）表層の両方（同じ `EngineCore`）。

## SQLSTATE（従来と同じ）

待機超過は `55P03`。エラー文は英語でフラグ名・値・範囲だけを含む。

## 運用上の注意

- 待機中は接続ハンドラのスレッドと同時接続枠を占有する。待機上限を上げると枠が長く塞がるため、ハード上限 60 秒が DoS の境界になる。
- [partitioned-dml.md](partitioned-dml.md) は「保持時間上限 × 2 ≤ 30 秒」で見積もっている。待機上限をこれより下げると、通常の書き込みがチャンク保持中に `55P03` を受け得る。

## 対象外

- ハード上限・下限の確定（オーナー判断）
- 待機上限が分割実行 DML の保持時間上限 × 2 を下回る組み合わせへの起動時警告
- 明示トランザクション持続時間上限との整合警告（トランザクション上限の CLI 化が入った時点で、比較対象を設定値へ差し替える）
- 環境変数・セッション単位の `SET lock_timeout` 相当・テナント単位の設定
