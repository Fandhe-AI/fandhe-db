# 集計の退行の bisect の生データ（Issue #1491）

設計記録: [aggregate-loop-regression.md](../../aggregate-loop-regression.md)。生データ（`self_exact.json`・`stdout.log`）は書き換えない。

## 収録内容

| パス | 内容 |
| ---- | ---- |
| `step00-good-0ab63b3-run{1,2,3}/` | good 端点（0ab63b3）の self exact |
| `step00-bad-cb73a8e-run{1,2,3}/` | bad 端点（cb73a8e）の self exact |
| `step01-` 〜 `step08-<sha7>-run{1,2,3}/` | bisect の各ステップ（3 回ずつ） |
| `later-{8e68162,dc9261e,ac4bbae}-run{1,2,3}/` | cb73a8e 以降の追加計測（退行の続きの確認。bisect の判定には使わない） |
| `run_bisect_step.sh` | 1 コミット分の計測ドライバ（A/B・ref の計測でも共有） |
| `summarize.py` | 標準ライブラリだけの集計。`python3 -I` で実行する |
| `make_summaries.sh` | 3 つの `summarize-output.md` を再生成する |
| `summarize-output.md` | 集計結果（文書の数値の出典） |
| `env.txt` | 計測環境 |

## 注意

- 環境は x86_64 の共有機（[env.txt](env.txt)）。M4 Max の既存データ（`crossdb-20260918T142251Z-ab`・`crossdb-20261009T140039Z-ab`）とは絶対値を比べない
- ステップごとに、そのコミット自身のソース・`seed_docs`・ハーネスで計測した。redb の形式がコミットで異なるため fixture は共有できない
- `git archive` の mtime が原因で cargo が別コミットの成果物を再利用する問題があり（ed2effa で観測）、最初の試行のデータは破棄して取り直した。
  ドライバは展開直後に mtime を更新する
- 外れ値が出るため、判定は 3 回の min-of-N
- `stdout.log` は行末の空白を削除して保存した（editorconfig-checker のため。内容は同じ）
- `later-*` は計測時の負荷が他と異なる（loadavg は `env.txt` の範囲）。傾向の確認に限る
- 秘密情報は含まない。`meta.version` にローカルのバイナリパスとその sha256 の先頭が入る
