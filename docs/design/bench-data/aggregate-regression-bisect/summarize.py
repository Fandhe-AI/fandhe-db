#!/usr/bin/env python3
"""Issue #1491: self exact 計測 JSON の集計（標準ライブラリのみ。`python3 -I` で実行する）。

文書（docs/design/aggregate-loop-regression.md）に載せる数値はすべて、コミットした
`self_exact.json` をこのスクリプトで集計した出力（summarize-output.md）から写す。

サブコマンド:
  group   <label>=<dir>[,<dir>...] ...   各グループの phase 別 n・min-of-N・median・max・幅
  compare <before_label> <after_label> <label>=<dir>[,<dir>...] ...
                                          2 グループの比（min-of-N 同士・median 同士）と判定クラス
  bisect  <bisect_dir>                    step*/ ディレクトリから bisect の全ステップ表を作る
  recovery firstbad=<dir..> lastgood=<dir..> before=<dir..> after=<dir..>
                                          退行幅に対する是正の回収率
  perrow  <label>=<dir>[,<dir>...] ...    agg 系の 1 セルあたり ns（meta.rows と集計数から導出）

統計量は p50_us（1 計測 = 50 反復の中央値）の run 間集計。`幅` は (max-min)/min。
"""

from __future__ import annotations

import glob
import json
import os
import statistics
import sys

# 本文で扱う phase。前 3 つが集計系（退行の対象）、残りは参照区間（集計を通らない経路）。
AGG_PHASES = ["agg_multi", "agg_count", "rls_isolation"]
REF_PHASES = ["vector_knn", "vector_knn_where", "where_compound_count", "group_by_having"]
PHASES = AGG_PHASES + REF_PHASES

# 判定クラスの固定帯（benchmark-judgement-policy §4 の固定 ±5%）。
FIXED_BAND = 0.05
# bisect の判定規則: 中点の ±15% に入ったら追加計測の対象（本スクリプトは表示のみ）。
NEAR_BAND = 0.15


def load(d: str) -> dict:
    with open(os.path.join(d, "self_exact.json"), encoding="utf-8") as f:
        return json.load(f)


def p50(d: str, phase: str) -> float:
    return float(load(d)["phases"][phase]["p50_us"])


def parse_group(spec: str):
    label, _, dirs = spec.partition("=")
    paths = []
    for part in dirs.split(","):
        paths.extend(sorted(glob.glob(part)))
    if not label or not paths:
        raise SystemExit(f"invalid group spec: {spec}")
    return label, paths


def stats(paths, phase):
    vals = [p50(p, phase) for p in paths]
    mn, mx = min(vals), max(vals)
    return {
        "n": len(vals),
        "min": mn,
        "median": statistics.median(vals),
        "max": mx,
        "spread": (mx - mn) / mn,
    }


def f2(x: float) -> str:
    return f"{x:.2f}"


def pct(x: float) -> str:
    return f"{x * 100:.2f}%"


def cmd_group(specs):
    for spec in specs:
        label, paths = parse_group(spec)
        print(f"### {label}（n={len(paths)}）\n")
        print("| phase | n | min-of-N (us) | median (us) | max (us) | 幅 (max-min)/min |")
        print("| ----- | -- | ------------- | ----------- | -------- | ---------------- |")
        for ph in PHASES:
            s = stats(paths, ph)
            print(f"| {ph} | {s['n']} | {f2(s['min'])} | {f2(s['median'])} | {f2(s['max'])} | {pct(s['spread'])} |")
        print()


def judge(ratio: float, noise_band: float) -> str:
    """固定 ±5% と参照区間の実測幅の両方を超えた差だけを有効とする。"""
    band = max(FIXED_BAND, noise_band)
    if ratio > 1 + band:
        return "悪化"
    if ratio < 1 - band:
        return "改善"
    return "帯内"


def cmd_compare(before_label, after_label, specs):
    groups = dict(parse_group(s) for s in specs)
    b, a = groups[before_label], groups[after_label]
    # 参照区間の実測 run-to-run 幅（両 arm の最大）を、その phase 群共通のノイズ帯とする。
    ref_spread = max(max(stats(b, ph)["spread"], stats(a, ph)["spread"]) for ph in REF_PHASES)
    print(f"比較: {before_label}（n={len(b)}）対 {after_label}（n={len(a)}）\n")
    print(f"参照区間の実測 run-to-run 幅（最大）: {pct(ref_spread)}、固定帯: {pct(FIXED_BAND)}\n")
    print(
        f"| 区間 | {before_label} min | {before_label} median | {after_label} min | "
        f"{after_label} median | ratio (min-of-N) | ratio (median) | 自区間の幅（両 arm の最大） | 判定クラス |"
    )
    print("| ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- | ---- |")
    for ph in PHASES:
        sb, sa = stats(b, ph), stats(a, ph)
        r_min = sa["min"] / sb["min"]
        r_med = sa["median"] / sb["median"]
        print(
            f"| {ph} | {f2(sb['min'])} | {f2(sb['median'])} | {f2(sa['min'])} | {f2(sa['median'])} "
            f"| {r_min:.4f} | {r_med:.4f} | {pct(max(sb['spread'], sa['spread']))} | {judge(r_min, ref_spread)} |"
        )
    print()


def cmd_perrow(specs):
    print("| グループ | phase | min-of-N (us) | 行数 | セル/行 | 1 セルあたり (ns) |")
    print("| -------- | ----- | ------------- | ---- | ------- | ----------------- |")
    cells = {"agg_multi": 5, "agg_count": 1}
    for spec in specs:
        label, paths = parse_group(spec)
        rows = int(load(paths[0])["meta"]["rows"])
        for ph, per_row in cells.items():
            mn = stats(paths, ph)["min"]
            ns = mn * 1000.0 / (rows * per_row)
            print(f"| {label} | {ph} | {f2(mn)} | {rows} | {per_row} | {ns:.3f} |")
    print()


def cmd_recovery(specs):
    """退行幅（first bad - last good）に対し、是正（before - after）が何割を回収したか。"""
    g = dict(parse_group(s) for s in specs)
    bad, good, before, after = g["firstbad"], g["lastgood"], g["before"], g["after"]
    print("| phase | 退行幅 first bad - last good (us) | 是正の回収量 before - after (us) | 回収率 | after - last good (us) | after / last good |")
    print("| ----- | ------------- | ------------- | ------ | ------------- | ----------------- |")
    for ph in AGG_PHASES:
        reg = stats(bad, ph)["min"] - stats(good, ph)["min"]
        rec = stats(before, ph)["min"] - stats(after, ph)["min"]
        left = stats(after, ph)["min"] - stats(good, ph)["min"]
        ratio = stats(after, ph)["min"] / stats(good, ph)["min"]
        print(f"| {ph} | {f2(reg)} | {f2(rec)} | {pct(rec / reg)} | {f2(left)} | {ratio:.4f} |")
    print()


def cmd_bisect(base):
    def runs(prefix):
        return sorted(glob.glob(os.path.join(base, prefix + "-run*")))

    good = runs("step00-good-*")
    bad = runs("step00-bad-*")
    g = min(p50(p, "agg_multi") for p in good)
    b = min(p50(p, "agg_multi") for p in bad)
    t = (g + b) / 2
    print(f"端点: good min-of-N agg_multi = {f2(g)} us（n={len(good)}）、bad = {f2(b)} us（n={len(bad)}）")
    print(f"判定しきい値 T = (good + bad) / 2 = {f2(t)} us（T 以上なら bad、未満なら good）。")
    print(f"T の ±15% 帯: {f2(t * (1 - NEAR_BAND))} - {f2(t * (1 + NEAR_BAND))} us\n")
    print("| step | commit | agg_multi p50 (us) の各回 | min-of-N | agg_count min-of-N | 判定 |")
    print("| ---- | ------ | ------------------------- | -------- | ------------------ | ---- |")
    steps = {}
    for d in sorted(glob.glob(os.path.join(base, "step*-run*"))):
        name = os.path.basename(d)
        # 端点は step00-(good|bad)-<sha>-runN、通常は stepNN-<sha>-runN。末尾の -runN を除き最後の要素を commit とする
        step, rest = name.split("-", 1)
        sha = rest.rsplit("-run", 1)[0].rsplit("-", 1)[-1]
        steps.setdefault((step, sha), []).append(d)
    for (step, sha), ds in sorted(steps.items()):
        am = [p50(d, "agg_multi") for d in ds]
        ac = [p50(d, "agg_count") for d in ds]
        verdict = "bad" if min(am) >= t else "good"
        if step == "step00":
            verdict = "端点（" + verdict + "）"
        print(
            f"| {step[4:]} | {sha} | {', '.join(f2(x) for x in am)} | {f2(min(am))} "
            f"| {f2(min(ac))} | {verdict} |"
        )
    print()


def main(argv):
    if len(argv) < 2:
        raise SystemExit(__doc__)
    cmd = argv[1]
    if cmd == "group":
        cmd_group(argv[2:])
    elif cmd == "compare":
        cmd_compare(argv[2], argv[3], argv[4:])
    elif cmd == "recovery":
        cmd_recovery(argv[2:])
    elif cmd == "perrow":
        cmd_perrow(argv[2:])
    elif cmd == "bisect":
        cmd_bisect(argv[2])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
