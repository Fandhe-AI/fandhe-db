# 集計の是正前後の交互 N=5 の生データ（Issue #1491）

設計記録: [aggregate-loop-regression.md](../../aggregate-loop-regression.md)。生データは書き換えない。

## 収録内容

| パス | 内容 |
| ---- | ---- |
| `pair{1..5}-before/` | f9f3e1dc8（#1487 の直前）の self exact |
| `pair{1..5}-after/` | 2e94de153（#1489 の直後）の self exact |
| `loadavg.log` | 各ペア開始時の loadavg |
| `run_pairs.sh` | 交互計測のドライバ（ペア内は before -> after 固定） |
| `summarize-output.md` | 集計結果（文書の数値の出典） |

## 注意

- `scripts/bench_scalar_index_crossdb_ab.sh` は使っていない。付属の summarizer が agg 系を集計せず、fixture を全 arm で共有する方式のため。
  代わりに、各計測でそのコミット自身のソース・`seed_docs`・ハーネスを使うドライバ（`../aggregate-regression-bisect/run_bisect_step.sh`）を使った
- 共有機の参考値（`BENCH_DEDICATED_ENV` 未設定）。計測環境は [env.txt](../aggregate-regression-bisect/env.txt)
- `stdout.log` は行末の空白を削除して保存した
