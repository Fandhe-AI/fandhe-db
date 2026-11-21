#!/usr/bin/env bash
# Issue #3226・#3301: mac-mini-ab-3226 の summarize-output.md を summarize.py から再生成する。
# 文書の数値は生成物の値をそのまま写す。再実行して git diff が出なければ数値は再計算できている。
# 使い方: bench-data/ から `bash mac-mini-ab-3226/make_summary.sh`
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
S=aggregate-regression-bisect/summarize.py
M=mac-mini-ab-3226
X=--extra-phases=hybrid_rrf,bulk_hybrid_k200,mode_precision,scan_where_nosort_k500

{
  echo "# summarize.py の出力（Mac mini の同一ホスト A/B。Issue #3226・#3301）"
  echo
  echo "生成: \`bash mac-mini-ab-3226/make_summary.sh\`（手編集しない）。統計量は各 run の p50_us（50 反復の中央値）の 3 回の集計。"
  echo
  echo "## 集計の基準 1e652179 対 是正後 7617fafaa"
  echo
  python3 -I "${S}" "${X}" compare 1e652179 7617fafaa \
    "1e652179=${M}/1e65217-run*" "7617fafaa=${M}/7617faf-run*"
  echo "## hybrid・mode_precision の基準 8429dc11 対 是正後 7617fafaa"
  echo
  python3 -I "${S}" "${X}" compare 8429dc11 7617fafaa \
    "8429dc11=${M}/8429dc1-run*" "7617fafaa=${M}/7617faf-run*"
  echo "## 各 sha のグループ集計（scan_where_nosort_k500 は 3ef68d68・66838ad60・7617fafaa を並べる）"
  echo
  python3 -I "${S}" "${X}" group \
    "1e652179=${M}/1e65217-run*" \
    "8429dc11=${M}/8429dc1-run*" \
    "3ef68d68=${M}/3ef68d6-run*" \
    "66838ad60=${M}/66838ad-run*" \
    "7617fafaa=${M}/7617faf-run*"
} | sed -e 's/[[:space:]]*$//' | cat -s | sed -e '$ { /^$/d; }' >"${M}/summarize-output.md"
