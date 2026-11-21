#!/usr/bin/env bash
# Issue #3226・#3301: 5 sha × 3 巡を交互に計測する（Mac mini で実行）
set -euo pipefail
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
W="$HOME/ab3226"; R="$W/repo"; OUT="$W/out"
export BENCH_TARGET="$W/target" CROSSDB_PYTHON="$HOME/crossdb-venv/bin/python3"
mkdir -p "$OUT"
cd "$R"
DRV="$R/docs/design/bench-data/aggregate-regression-bisect/run_bisect_step.sh"
SHAS="1e652179 8429dc11 3ef68d68 66838ad60 7617fafaa"
for n in 1 2 3; do
  for s in $SHAS; do
    s7="${s:0:7}"; d="$OUT/${s7}-run${n}"
    echo "$(date -u +%FT%TZ) before ${s7}-run${n}: $(uptime)" >> "$OUT/loadavg.log"
    bash "$DRV" "$s" "$d" 15491
    echo "$(date -u +%FT%TZ) after  ${s7}-run${n}: $(uptime)" >> "$OUT/loadavg.log"
  done
done
echo ALL_DONE
