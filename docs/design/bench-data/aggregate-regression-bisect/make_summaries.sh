#!/usr/bin/env bash
# Issue #1491: 3 つの生データディレクトリの summarize-output.md を summarize.py から再生成する。
# 文書の数値は生成物の値をそのまま写す。再実行して git diff が出なければ数値は再計算できている。
# 使い方: bench-data/ から `bash aggregate-regression-bisect/make_summaries.sh`
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
S=aggregate-regression-bisect/summarize.py

{
  echo "# summarize.py の出力（bisect・背景・以降の追加退行）"
  echo
  echo "生成: \`bash aggregate-regression-bisect/make_summaries.sh\`（手編集しない）。"
  echo
  echo "## 背景: M4 Max の self exact（既存の生データを再集計）"
  echo
  python3 -I "${S}" group \
    'm4-0ab63b3-2026-09-18=crossdb-20260918T142251Z-ab/results/round*' \
    'm4-cb73a8e-2026-10-09=crossdb-20261009T140039Z-ab/results/*round*'
  echo "## bisect（本機 x86_64）"
  echo
  python3 -I "${S}" bisect aggregate-regression-bisect
  echo "## 端点・最終 2 コミット・以降のコミットのグループ集計（本機 x86_64）"
  echo
  python3 -I "${S}" group \
    'good-0ab63b3=aggregate-regression-bisect/step00-good-0ab63b3-run*' \
    'bad-cb73a8e=aggregate-regression-bisect/step00-bad-cb73a8e-run*' \
    'last-good-c94dc5d=aggregate-regression-bisect/step08-c94dc5d-run*' \
    'first-bad-ad07bfd=aggregate-regression-bisect/step07-ad07bfd-run*' \
    'later-8e68162=aggregate-regression-bisect/later-8e68162-run*' \
    'later-dc9261e=aggregate-regression-bisect/later-dc9261e-run*' \
    'later-ac4bbae=aggregate-regression-bisect/later-ac4bbae-run*'
  echo "## 1 セルあたりの増分（本機 x86_64）"
  echo
  python3 -I "${S}" perrow \
    'c94dc5d（last good）=aggregate-regression-bisect/step08-c94dc5d-run*' \
    'ad07bfd（first bad）=aggregate-regression-bisect/step07-ad07bfd-run*' \
    'cb73a8e=aggregate-regression-bisect/step00-bad-cb73a8e-run*'
} > aggregate-regression-bisect/summarize-output.md

{
  echo "# summarize.py の出力（是正前後 f9f3e1dc8 対 2e94de153）"
  echo
  echo "生成: \`bash aggregate-regression-bisect/make_summaries.sh\`（手編集しない）。"
  echo
  python3 -I "${S}" compare before after \
    'before=aggregate-regression-ab/pair*-before' \
    'after=aggregate-regression-ab/pair*-after'
  echo "## 1 セルあたり"
  echo
  python3 -I "${S}" perrow \
    'before=aggregate-regression-ab/pair*-before' \
    'after=aggregate-regression-ab/pair*-after'
  echo "## 退行幅に対する回収"
  echo
  python3 -I "${S}" recovery \
    'firstbad=aggregate-regression-bisect/step07-ad07bfd-run*' \
    'lastgood=aggregate-regression-bisect/step08-c94dc5d-run*' \
    'before=aggregate-regression-ab/pair*-before' \
    'after=aggregate-regression-ab/pair*-after'
  echo "## グループ集計"
  echo
  python3 -I "${S}" group \
    'before=aggregate-regression-ab/pair*-before' \
    'after=aggregate-regression-ab/pair*-after'
} > aggregate-regression-ab/summarize-output.md

{
  echo "# summarize.py の出力（last good c94dc5d 対 2e94de153）"
  echo
  echo "生成: \`bash aggregate-regression-bisect/make_summaries.sh\`（手編集しない）。"
  echo
  python3 -I "${S}" compare ref after \
    'ref=aggregate-regression-ref-ab/pair*-ref' \
    'after=aggregate-regression-ref-ab/pair*-after'
  echo "## 1 セルあたり"
  echo
  python3 -I "${S}" perrow \
    'ref=aggregate-regression-ref-ab/pair*-ref' \
    'after=aggregate-regression-ref-ab/pair*-after'
  echo "## グループ集計"
  echo
  python3 -I "${S}" group \
    'ref=aggregate-regression-ref-ab/pair*-ref' \
    'after=aggregate-regression-ref-ab/pair*-after'
} > aggregate-regression-ref-ab/summarize-output.md

# markdownlint（MD012）のため、連続空行を 1 行にまとめ、末尾の空行を削る。
for d in aggregate-regression-bisect aggregate-regression-ab aggregate-regression-ref-ab; do
  out="${d}/summarize-output.md"
  tmp="$(mktemp)"
  cat -s "${out}" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' > "${tmp}"
  mv "${tmp}" "${out}"
done
