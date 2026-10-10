# crossdb-20261009T140039Z-ab の注記

- 計測: main cb73a8e・交互 N=5（round1〜5）・共有機参考値。計測環境は `env.txt`、5 ラウンド集計は `summarize-output.md`（`summarize` の出力そのまま）
- 収録: 差分量の都合でラウンド単位の PR に分けて記録した（round1 #2248・round2 #2249・round3 #2250・round4 #2251・round5 と集計 #2252）。内容は計測時の生データから変更していない
- **self（`self/exact`・`self/hnsw`）の `explain` は判定に使わない**: 計測スクリプト `scripts/crossdb_bench/self_db.py` が、素の `EXPLAIN` が成功した場合も `unsupported`（「到達しないはずの分岐（EXPLAIN が成功した）」）を記録していたため、結果 JSON の `unsupported: true` と集計の「非観測」は実際の非対応を意味しない。生成元は #2253 で修正した（成功時は計測し出力を記録）。本ディレクトリの記録は計測時点の値として書き換えない
