# Mac mini の同一ホスト A/B の生データ（Issue #3226・#3301）

設計記録: [aggregate-loop-regression.md](../../aggregate-loop-regression.md)・[hybrid-rrf-latency-breakdown.md](../../hybrid-rrf-latency-breakdown.md)。生データ（`self_exact.json`・`stdout.log`）は書き換えない（下記のパス置換と行末空白の削除を除く）。

## 収録内容

| パス | 内容 |
| ---- | ---- |
| `1e65217-run{1,2,3}/` | 1e652179（集計の基準。0dfa1a9a の直前）の self exact |
| `8429dc1-run{1,2,3}/` | 8429dc11（hybrid・mode_precision の基準。74eeed13 の直前） |
| `3ef68d6-run{1,2,3}/` | 3ef68d68（#3301 用。Phase 2 の前） |
| `66838ad-run{1,2,3}/` | 66838ad60（#3301 用。#3223 の後） |
| `7617faf-run{1,2,3}/` | 7617fafaa（是正後の main） |
| `loadavg.log` | 各回の前後の `uptime`（UTC 時刻つき） |
| `run_all.sh` | 5 sha × 3 巡を交互に回したループ（`aggregate-regression-bisect/run_bisect_step.sh` を呼ぶ） |
| `make_summary.sh` | `summarize-output.md` を再生成する |
| `summarize-output.md` | 集計結果（文書の数値の出典） |

## 環境

- 機種: Mac mini（Apple M4・10 コア〔4 P + 6 E〕・16 GB）
- OS: macOS 27.0.1（arm64）
- rustc 1.100.0（0ef1bc386 2026-11-09）・cargo 1.100.0
- Python 3.14.8・psycopg 3.3.5（ハーネスは各コミット自身の `scripts/crossdb_bench/run.py`）
- fixture: 25,000 行（可視 23,000 行）・128 次元・queries 200。コミットごとに `seed_docs` で作り直す
- 共有 `CARGO_TARGET_DIR` を全 sha で使い回す（ドライバが展開直後に mtime を更新して再ビルドさせる）
- 計測日: 2026-11-21（10:17〜10:31 UTC）。ログイン 1 人、GUI の作業なし。Docker Desktop は常駐していたがコンテナ処理なし（CPU 1% 未満）

## 手順

- 順番: `1e652179 → 8429dc11 → 3ef68d68 → 66838ad60 → 7617fafaa` を 1 巡として 3 巡（同じ sha が続かないよう交互）
- 1 回: `BENCH_TARGET=<共有 CARGO_TARGET_DIR> run_bisect_step.sh <sha> <out_dir> 15491`
- 集計: `bash mac-mini-ab-3226/make_summary.sh`（`summarize.py` の `--extra-phases=` で hybrid 系と scan を表に足す）
- 判定: 同一ホストの min-of-N（N=3）の p50 同士で「以下」か。Issue 本文の絶対値（別ホストで測った値）とは比べない

## loadavg の範囲

- 開始前（計測ループを起動する約 1 分前）: 1 分平均 1.22・ログイン 1 人。CPU 上位は syspolicyd の一時的な負荷だけで、他は 1% 未満
- 各回の前後（`loadavg.log`）の 1 分平均: 2.98〜10.87。計測開始時（`meta.loadavg`）: 5.8〜10.9
- 計測中の値が高いのは、各回の直前に走るドライバ自身の release ビルド（10 コアを並列で使う）の残りと見られる。計測中のプロセスの内訳は採っていない
- 1 分平均は 2 回目以降 6 前後で横ばいで、sha ごとの偏りは見られない

## ハーネスの差

- 各 sha は自身の `scripts/crossdb_bench/run.py`・`self_db.py` で計測した。判定対象の phase と `scan_where_nosort_k500` の SQL は 5 sha で同じ。`agg_count`・`agg_multi`・`rls_isolation` の結果（`result`・`tenant_b_count`）、`bulk_hybrid_k200`・`scan_where_nosort_k500` の `rows_returned` も一致する
- 3ef68d68・66838ad60 は kNN の phase（`vector_knn` 等）の演算子が `<#>`（#2952〜#3224 の間）。他の 3 sha は `<=>`
- 7617fafaa は `hybrid_rrf_depth{50,200}`（#3225）の phase が増え、`mode_*`・`bulk_*`・`scan_*` の前に流れる

## 注意

- 巡の中の順番は固定のため、7617fafaa は毎巡の最後に計測した。順番による偏りは分離していない。判定対象の phase の run 間の幅は 7.4% 以下（`summarize-output.md` の「幅」列）
- 1e652179 の `scan_where_nosort_k500` の run1 は 434.9 us で、run2・run3（474.2・484.0 us）より低い（幅 11.28%）。min-of-N の比較ではこの値が基準になる
- `stdout.log` は行末の空白を削除して保存した（editorconfig-checker のため）
- ローカルの絶対パスはプレースホルダに置き換えた: 作業ディレクトリ（`meta.version` のバイナリパスを含む）は `<BENCH_ROOT>`、ドライバの一時領域は `<TMPDIR>`。`meta.version` の sha256 の先頭はそのまま残す
- 秘密情報は含まない
