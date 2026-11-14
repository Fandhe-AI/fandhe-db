# summarize.py の出力（Issue #3210）

生成: 下記コマンドの出力（手編集しない）。

```text
python3 -I aggregate-regression-bisect/summarize.py compare base main base=<99fb6d690-run1..5> main=<5a24e8f2c-run1..5>
```

比較: base（n=5）対 main（n=5）

参照区間の実測 run-to-run 幅（最大）: 159.00%、固定帯: 5.00%

| 区間 | base min | base median | main min | main median | ratio (min-of-N) | ratio (median) | 自区間の幅（両 arm の最大） | 判定クラス |
| ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- |
| agg_multi | 876.69 | 884.64 | 792.46 | 802.14 | 0.9039 | 0.9067 | 22.02% | 帯内 |
| agg_count | 195.78 | 199.23 | 195.14 | 199.72 | 0.9968 | 1.0025 | 23.56% | 帯内 |
| rls_isolation | 209.74 | 215.22 | 206.61 | 208.09 | 0.9851 | 0.9669 | 8.53% | 帯内 |
| vector_knn | 632.90 | 675.66 | 400.16 | 442.41 | 0.6323 | 0.6548 | 159.00% | 帯内 |
| vector_knn_where | 260.62 | 284.35 | 277.06 | 293.48 | 1.0631 | 1.0321 | 58.80% | 帯内 |
| where_compound_count | 117.88 | 120.36 | 113.11 | 117.40 | 0.9596 | 0.9754 | 37.13% | 帯内 |
| group_by_having | 47.74 | 48.63 | 53.57 | 55.71 | 1.1220 | 1.1456 | 70.02% | 帯内 |
