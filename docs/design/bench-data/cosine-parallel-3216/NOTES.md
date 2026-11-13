# Issue #3216 コサイン経路の並列化 A/B の生データ

`ParallelSearchProvider` に `search_cosine`／`search_cosine_subset` を実装した変更の前後を、
`docs/design/bench-data/aggregate-regression-bisect/run_bisect_step.sh` で計測した生データ。
各 sha はそのコミット自身のハーネス・seed（25,000 行・128 次元）で動く。要約は
[hybrid-rrf-latency-breakdown.md](../../hybrid-rrf-latency-breakdown.md) 末尾の「Issue #3216」節。

| ディレクトリ | sha | 内容 |
| ------------ | --- | ---- |
| `base1`〜`base3` | `3c8056a9`（変更前の main） | `self_exact.json`・`stdout.log` |
| `head1`・`head2` | `96c174e2`（本変更のコード部分。計測後に docs を同一コミットへ統合したため最終 sha は異なる） | 同上 |

- `env.txt`: 計測機（Apple M4 Max・16 論理コア）・計測時の loadavg（約 20〜23。他プロセス常駐の共有機）。
  **参考値**であり、判定は min-of-N（外乱は加算的なため）で行う。専有環境での確定値ではない。
- `meta.version` などにローカルの一時パスが含まれる場合がある（計測時の作業領域。機密ではない）。
- x86_64 で取った `aggregate-regression-bisect` の絶対値とは比較しない（機種が異なる）。
- 指標の対応づけ（本 Issue の解釈）: k10 は `mode_precision`（`<=>` LIMIT 10 USING MODE 'precision'）、
  k400 は `hybrid_rrf` と `bulk_hybrid_k200`（密側 `search_cosine` が fetch_k = 400 で呼ばれる）。
