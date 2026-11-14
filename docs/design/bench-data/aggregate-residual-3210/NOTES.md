# #2761 後の agg_multi 残差の再測定（Issue #3210）

設計記録: [aggregate-loop-regression.md](../../aggregate-loop-regression.md)の「#2761 後の agg_multi の残差（Issue #3210）」節。生データ（`self_exact.json`・`stdout.log`）の数値は書き換えない（ローカルの絶対パスのみ `<bench-target-parent>`・`<tmp>` へ置換した）。

## 収録内容

| パス | 内容 |
| ---- | ---- |
| `99fb6d690-run{1..5}/` | #2761 の直前（99fb6d690）の self exact |
| `5a24e8f2c-run{1..5}/` | 計測時点の main（5a24e8f2c）の self exact |
| `summarize-output.md` | 集計（`python3 -I ../aggregate-regression-bisect/summarize.py compare base main base=<dirs> main=<dirs>` の出力） |

## 注意

- ドライバは `../aggregate-regression-bisect/run_bisect_step.sh`。コミット自身のソース・`seed_docs`・ハーネスで計測した。2 コミットを交互（ABAB）に走らせた
- x86_64 の共有機（CPU は i7-13700K、他のエージェントのビルドが並走・loadavg 4〜5 帯）。外れ値が出るため判定は min-of-N
- `stdout.log` は行末の空白を削除して保存した
- 秘密情報は含まない
