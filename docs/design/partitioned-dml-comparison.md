# 分割実行 DML と他 DB の大量行 DML 手法の比較

- ステータス: Accepted（実装済みの挙動と、他 DB・ツールの公式ドキュメントの記述を並べた比較記録。本書の追加でコードは変えない）
- 対応 Issue: #2074（親 #2070）。前提の実装は #2071（チャンク間隔）・#2072（1 回の実行の時間上限）
- ビヘイビア ID ポインタ: SQL-32・SQL-77（`docs/spec/04-behavior/`）。
  補助として SQL-19・RECOVER-11・RECOVER-12・RLS-9・RLS-10・NOSQL-12。契約本文は spec を正とし、本書へは転記しない
- 他 DB・ツールの記述の基準: 「参照」節の公式ドキュメントを確認日（2026-11-05）に読んだ内容だけを書く。
  確認できなかった点は「公式ドキュメントで確認できず」と書き、記憶で補わない
- 関連コード: `crates/engine/src/tenant/partitioned_dml.rs`・`crates/engine/src/recovery/partitioned_job.rs`・
  `crates/engine/src/sql/partitioned.rs`・`crates/engine/src/sql/parser.rs`（`PartitionedDmlLimits`）
- 関連 doc: [partitioned-dml.md](partitioned-dml.md)・[idempotent-resend-comparison.md](idempotent-resend-comparison.md)・
  [statement-interrupt.md](statement-interrupt.md)・[multi-row-dml-operation-id.md](multi-row-dml-operation-id.md)・
  `crates/wire-server/docs/nosql-api.md`

## 目的と読み方

fandhe の分割実行 DML（`UPDATE`／`DELETE ... USING OPERATION_ID '<id>' PARTITIONED [CHUNK n]`、NoSQL の `"mode": "partitioned"`）を、
他 DB・ツールの大量行 DML 手法と同じ軸で並べる。「同等以上」と書くのは、fandhe 側にテストで固定した根拠がある行に限る。
差異・未提供の点は隠さず書く。

## 前提

- 比較対象の種類が違う。PostgreSQL の `ctid` を使うバッチ処理と MySQL の `DELETE ... LIMIT` の繰り返しは、
  クライアント側のループという慣用句で、サーバー側にジョブの状態を持たない。このため多くのセルは
  「該当なし（クライアント側ループで利用者が実装）」になる
- pt-archiver（Percona Toolkit）は DB の外にあるツールで、DB コアの機能ではない
- Cloud Spanner の Partitioned DML はサーバー側の機能だが、キー空間の分割を Spanner が決め、並列に実行する。
  fandhe はキー順にチャンクを逐次実行する同期実行の文修飾で、SQL・NoSQL の両表層が同じ実行器を使う
- TiDB・CockroachDB は本表の対象外とする（比較の経緯は [partitioned-dml.md](partitioned-dml.md) §1）

## 比較表

| 比較軸 | PostgreSQL（`ctid` バッチ） | MySQL（`DELETE ... LIMIT`） | pt-archiver | Cloud Spanner Partitioned DML | fandhe |
| ------ | ------ | ------ | ------ | ------ | ------ |
| サーバー側の進捗照会 | 該当なし（クライアント側ループで利用者が実装） | 該当なし（同左。繰り返しの終了判定は影響行数で行う） | `--progress` で X 行ごとに進捗を出力、`--statistics` で統計を出力（ツール側の出力） | Active partitioned DMLs の統計テーブルで監視できる | `SHOW PARTITIONED DML '<id>' ON <table>`／NoSQL `show_partitioned_dml`。writer を使わず状態と処理済み件数を返す。根拠: `partitioned_dml_sql.rs` の `cancel_interrupted_job_then_resend_is_vd002_and_show_reports_cancelled` |
| 取り消し | 該当なし（ループを止める） | 該当なし（同左） | `--stop`／sentinel ファイルで実行を止める。位置は保存しない | 取り消すと実行中のパーティションが止まり、未開始は飛ばす。完了済みは戻らない | `CANCEL PARTITIONED DML`／`cancel_partitioned_dml`。チャンク境界とチャンク間の待機中に止まり `VD002`。commit 済みチャンクは残る。根拠: `partitioned_dml/tests.rs` の `cancel_at_chunk_boundary_writes_cancelled_record_and_resend_is_rejected`・`cancel_during_interval_stops_without_waiting_for_the_rest`・`cancel_during_scan_of_only_chunk_discards_chunk_and_leaves_no_record` |
| 再開位置の永続化 | 該当なし（利用者が覚える） | 該当なし（同左） | 公式ドキュメントに再開機能の記述なし。前進位置は実行中のメモリ上だけ | 公式ドキュメントで確認できず（失敗時は全体が止まり、一部だけ実行済みになりうる） | カーソルをチャンクの行変更と同じ write トランザクションで保存。同じ文の再送で保存位置から再開。根拠: `interrupted_job_resumes_from_cursor_and_skips_rows_before_it`・`recovery/partitioned_job.rs` の `persisted_job_record_bytes_are_golden` |
| 再開時の WHERE 照合 | 該当なし | 該当なし | 該当なし（再開しない） | 公式ドキュメントで確認できず | 内容照合ハッシュ（分割実行専用の操作種別タグ。チャンク幅は含めない）。不一致は `22023`。根拠: `resend_after_completion_is_duplicate_or_content_mismatch`・`ledger_entry_between_chunks_stops_with_22023_when_no_job_record_exists`・wire 側 `nosql12_partitioned_dml.rs` の `atomic_and_partitioned_nosql_do_not_share_a_hash_in_either_direction`。範囲は完了後の再送と、原子実行との混用の拒否 |
| 決定的な前進順序 | `ctid` の物理位置。公式ドキュメントは行識別子に使うべきでないと明記（更新や `VACUUM FULL` で変わる） | `ORDER BY` と `LIMIT` を併用すれば順序を指定できる（単一表のみ） | 走査する索引（既定は主キー）で昇順に前進。直前の行の値から次の `WHERE` を作る | 分割は Spanner が決める。利用者は順序を制御できない | 物理キー `(tenant, rowid)` 昇順の単調カーソル。下端排他。カーソル値は応答・SHOW・EXPLAIN に出さない。根拠: `interrupted_job_resumes_from_cursor_and_skips_rows_before_it` |
| チャンク間隔・時間上限 | 該当なし（利用者が実装） | 該当なし（同左） | `--sleep`（取得の間隔）・`--run-time`（実行時間の上限）・`--limit`・`--txn-size` | 公式ドキュメントのこのページで確認できず | engine の上限 `PartitionedDmlLimits` の `chunk_interval`（既定 0）・`max_run_time`（既定 0 は無効。1 回の実行単位で、再送で 0 から数え直す）。待機中は writer を保持しない。超過は `VD001`。運用者が設定する起動フラグは #2073 で実装中・未提供。根拠: `chunk_interval_sleeps_between_chunks_in_slices`・`writer_gate_is_free_during_interval`・`run_time_limit_stops_after_a_changed_chunk_and_resend_counts_from_zero`・`partitioned_dml_sql.rs` の `run_time_limit_stops_every_run_with_vd001_and_resends_finish_each_row_once` |
| RLS（テナント境界）の継続 | 該当なし（ループ内の各文に通常の RLS が掛かる） | 該当なし（RLS 機能なし） | 該当なし（DB の外のツール） | 公式ドキュメントのこのページで確認できず | 各チャンクがテナント所有かつ可視の範囲だけを走査。再開時は再開したセッションの認証主体から改めて導出し、RLS 文脈は永続化しない。SHOW・CANCEL の「存在しない」と「他テナント」は同一応答。SQL 表層は各チャンクで権限を再検査する。NoSQL 表層は権限の再検査を渡さない（差異）。根拠: `not_found_cases_are_indistinguishable_for_show_and_cancel`・`table_rls/partitioned_dml_bound_session.rs`・`partitioned_dml_concurrency.rs` |
| 冪等キーとの結合 | 該当なし | 該当なし | 該当なし | 文が冪等である必要がある（ネットワーク再試行で同じ行へ複数回実行されうる）。冪等キーは公式ドキュメントで確認できず | `operation_id` 必須（構成によらず）。完了時の台帳エントリを最後のチャンクと同じ txn で記録。完了後の再送は `23505`、内容不一致は `22023`、部分完了は `VD001`、取り消し済みへの再送は `VD002`。根拠: `partial_completion_reports_vd001_and_resend_resumes_to_cumulative_total`・`completed_job_resend_from_the_other_surface_is_23505_or_22023` |

補足（比較軸以外の事実）:

- Spanner の返す行数は変更行数の下限で、文は部分的にだけ実行されうる。fandhe は処理済み件数を SHOW で返し、部分完了を `VD001` で明示する
- MySQL の公式ドキュメントは、大量削除を「影響行数が `LIMIT` 未満になるまで繰り返す」と説明している。
  この繰り返しの管理（再開・取り消し・冪等）は利用者の責務になる
- PostgreSQL の `ctid` は更新で変わるため、ループの途中の再開位置には使いにくい。fandhe は行 ID 由来の単調カーソルを使う

## 書き分け

- 同等以上（根拠テストあり）:
  - サーバー側の進捗照会と取り消し（クライアント側ループの手法には無く、Spanner と同等の機能を SQL 文で提供）
  - 再開位置のチャンクと同一 txn での原子的な永続化と、再送による再開
  - 再開・再送時の内容照合（`22023`）
  - テナント境界の継続（各チャンクの可視範囲の制限・存在情報を漏らさない応答）
  - 冪等キー（`operation_id`）との結合
- 差異・未提供（隠さず書く）:
  - 間隔・時間上限は engine では実装済みだが、運用者が設定する起動フラグは #2073 で実装中で、提供済みではない
  - 時間上限はジョブ全体ではなく 1 回の実行単位（再送で数え直す）
  - pg wire の CancelRequest では止められない。専用文で取り消す
  - 起動時の自動再開（C2）は持たない
  - NoSQL 表層は権限の再検査を行わない
  - `EXPLAIN ... PARTITIONED` に実効の待機は出ない（[partitioned-dml.md](partitioned-dml.md) の #2071 追記節の申し送り）
  - 並列実行は無い。Spanner はキー空間を並列に処理する

## 意図的に持たないもの

理由の詳細は [partitioned-dml.md](partitioned-dml.md) を正とし、ここでは要点だけを記す。

- `RETURNING` との併用（`42601`）: 返却行がチャンクを跨ぐと、原子性のない結果セットになるため（§3.1・§3.2）
- トランザクション内での実行（`25001`）: チャンクごとの commit が、明示・暗黙トランザクションの 1 回の commit と両立しない。
  黙って原子実行へ縮退すると fail-open になるため拒否する（§3.3）
- 非同期ジョブとしての起動（S3 案として見送り）: バックグラウンド実行器と、セッションを持たない実行主体による
  RLS 文脈の永続化が要り、テナント境界の検査面が増えるため（§3.2 と「スコープ外・申し送り」節）

## 参照

確認日は 2026-11-05。

- PostgreSQL 17「System Columns」（<https://www.postgresql.org/docs/17/ddl-system-columns.html>）
- MySQL 8.4「DELETE Statement」（<https://dev.mysql.com/doc/refman/8.4/en/delete.html>）
- Percona Toolkit「pt-archiver」（<https://docs.percona.com/percona-toolkit/pt-archiver.html>）
- Google Cloud Spanner「Partitioned DML」（<https://docs.cloud.google.com/spanner/docs/dml-partitioned>）
- fandhe 側は上記のファイル名・関数名・テスト名で参照する（行番号は書かない）
