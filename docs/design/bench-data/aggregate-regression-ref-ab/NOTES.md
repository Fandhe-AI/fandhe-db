# last good（c94dc5d）との交互 N=5 の生データ（Issue #1491）

設計記録: [aggregate-loop-regression.md](../../aggregate-loop-regression.md)。生データは書き換えない。

## 収録内容

| パス | 内容 |
| ---- | ---- |
| `pair{1..5}-ref/` | c94dc5d（bisect の last good）の self exact |
| `pair{1..5}-after/` | 2e94de153 の self exact |
| `loadavg.log` | 各ペア開始時の loadavg |
| `summarize-output.md` | 集計結果（文書の数値の出典） |

## 注意

- 計測ドライバは `../aggregate-regression-ab/run_pairs.sh`（`ref` を A、`after` を B として呼ぶ。ペア内は ref -> after 固定）
- arm ごとに、そのコミット自身の `seed_docs` で fixture を作り直した（redb の形式が非互換のため共有できない）
- 2e94de153 は c94dc5d より DISTINCT 系を含む多くの機能を持つため、同じ計測でも経路は同一ではない
- 共有機の参考値（`BENCH_DEDICATED_ENV` 未設定）
