#!/usr/bin/env python3
"""WHERE 実行後の hybrid_rrf レイテンシ・RSS を計測する専用ハーネス（Issue #633）。

Issue #632（`docs/design/scalar-index-generation-cache.md`「追記（Issue #632）」節）
の切り分けで使った検証用スクリプト（scratch 上の `hybrid_only.py`。未追跡）の
tracked 版。`scripts/crossdb_bench/run.py` の全フェーズ実行とは別に、
「WHERE クエリを何本か実行してテナントのスカラー索引を暖機した後の
hybrid_rrf レイテンシ」だけを単独で交互計測するための最小ハーネス。

呼び出し元は `scripts/bench_scalar_index_crossdb_ab.sh`（本 Issue のドライバ）。
呼び出し先は `self_db.py`（wire-server 子プロセスのライフサイクル）・
`common.py`（jsonl 読み込み・SQL リテラル整形・レイテンシ統計）。

3 モード:
  - hybrid: ウォームアップなしで hybrid_rrf のみを計測する（ScalarIndex
    構築の gate が発火しない対照）。
  - warm_where_then_hybrid: `WHERE lang = 'ja' ORDER BY <=>` を
    `--warm-where` 本（既定 50）実行して ScalarIndex を構築させてから
    hybrid_rrf を計測する（Issue #632 が実測した退行の再現条件）。
  - body_predicate: 索引対象から除外された列（`body`）への前方一致述語
    `WHERE body LIKE '<prefix>%' ORDER BY <=> LIMIT 10` を計測する
    （除外列がスカラー事前フィルタ経路で plain scan へ縮退する影響の実測）。

出力は 1 本の JSON（`--out` 必須。命名規約はドライバ側が決める）で、
`meta`（起動バイナリ識別・mode・iters・warm 本数・loadavg・timestamp）・
`rss`（start／after_warm／end の 3 時点の VmRSS・VmHWM）・
`latencies_us`（生レイテンシ配列。hybrid_rrf 呼び出しのみ）・
`stats`（`common.latency_stats` の p50/p95 等）を持つ。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from common import (  # noqa: E402
    load_jsonl,
    latency_stats,
    read_loadavg,
    sql_escape_literal,
    vec_literal,
)
import self_db  # noqa: E402

MODES = ("hybrid", "warm_where_then_hybrid", "body_predicate")

# LIKE 前置一致の prefix に使ってよい文字集合（許可リストの `StartsWith` 形と
# 整合させ、`%`／`_`／`'` を含む値は曖昧・エスケープ漏れの温床になるため
# 候補から除外する。coding-rust.md「SQL 文字列組み立てへの未検証入力連結
# 禁止」を Python 側でも踏襲し、`common.sql_escape_literal` を必ず通す）。
_UNSAFE_PREFIX_CHARS = ("%", "_", "'")


def _rss_snapshot(pid: int) -> dict:
    """`/proc/<pid>/status` から VmRSS・VmHWM を KiB 単位で読む
    （プロセスが既に終了している場合は空 dict。fail-closed に例外を伏せる
    のではなく「観測できなかった」ことを出力へ残す）。"""
    out: dict = {}
    try:
        with open(f"/proc/{pid}/status", "r", encoding="utf-8") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    out["vm_rss_kib"] = int(line.split()[1])
                elif line.startswith("VmHWM:"):
                    out["vm_hwm_kib"] = int(line.split()[1])
    except (OSError, ValueError, IndexError):
        pass
    return out


def _pick_body_prefix(docs: list[dict], prefix_len: int = 16) -> str:
    """`body_predicate` モード向けに、docs fixture から決定的に 1 件選び、
    その本文先頭 `prefix_len` 文字を LIKE 前置一致の prefix にする。

    `visibility == "public"`（自テナントから可視）の行に限定し、`%`／`_`／`'`
    を含む候補は曖昧・エスケープ漏れの温床になるため飛ばす。見つからなければ
    固定リテラルへ fail-closed に倒す（計測全体を止めない）。
    """
    for doc in docs:
        if doc.get("visibility") != "public":
            continue
        body = doc.get("body")
        if not isinstance(body, str) or len(body) < prefix_len:
            continue
        candidate = body[:prefix_len]
        if any(ch in candidate for ch in _UNSAFE_PREFIX_CHARS):
            continue
        return candidate
    return "crossdb bench"


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="WHERE 実行後の hybrid_rrf レイテンシ・RSS 計測（Issue #633）"
    )
    p.add_argument("--rows-file", required=True, help="計測対象の redb ファイルパス（作業コピーへ複製して使う）")
    p.add_argument("--queries-file", required=True, help="queries jsonl のパス")
    p.add_argument(
        "--docs-file",
        default=None,
        help="body_predicate モードの prefix 選定に使う docs jsonl（未指定時は body_predicate を拒否）",
    )
    p.add_argument("--mode", required=True, choices=MODES)
    p.add_argument("--iters", type=int, default=200, help="hybrid_rrf 計測の反復回数（既定 200・上限 5000）")
    p.add_argument("--warm-where", type=int, default=50, help="warm_where_then_hybrid の WHERE 本数（既定 50・上限 5000）")
    p.add_argument("--out", required=True, help="結果 JSON の出力先パス（既存ファイルは上書き拒否）")
    p.add_argument("--workdir", default=None, help="作業用一時ディレクトリ（既定: --rows-file と同じディレクトリ）")
    return p.parse_args()


def main() -> int:
    args = parse_args()

    if not (1 <= args.iters <= 5000):
        print(f"ERROR: --iters must be within 1..5000, got {args.iters}", file=sys.stderr)
        return 2
    if not (0 <= args.warm_where <= 5000):
        print(f"ERROR: --warm-where must be within 0..5000, got {args.warm_where}", file=sys.stderr)
        return 2
    if args.mode == "body_predicate" and not args.docs_file:
        print("ERROR: --mode body_predicate requires --docs-file", file=sys.stderr)
        return 2
    if os.path.exists(args.out):
        print(f"ERROR: refusing to overwrite existing output: {args.out}", file=sys.stderr)
        return 2

    workdir = args.workdir or os.path.dirname(os.path.abspath(args.rows_file))
    os.makedirs(workdir, exist_ok=True)

    queries = load_jsonl(args.queries_file)
    if not queries:
        print("ERROR: queries-file is empty", file=sys.stderr)
        return 2
    query_vecs = [vec_literal(q["embedding"]) for q in queries]
    query_texts = [sql_escape_literal(q.get("text", "")) for q in queries]

    body_prefix = None
    if args.mode == "body_predicate":
        docs = load_jsonl(args.docs_file)
        body_prefix = sql_escape_literal(_pick_body_prefix(docs))

    # 計測は redb を書き換えないが（hybrid_rrf・SELECT はいずれも読み取り専用）、
    # run.py::run と同じく複数 arm・複数 run を同一 fixture へ向けて交互起動
    # する運用のため、作業コピーへ複製してから開く（他 run と同時実行しても
    # 元 fixture を破壊しない・`self_bench_work.redb` の固定名衝突を避ける）。
    run_dir = tempfile.mkdtemp(prefix=f"hybrid_after_where_{os.getpid()}_", dir=workdir)
    work_db = os.path.join(run_dir, "hybrid_after_where.redb")
    server: self_db.SelfServer | None = None
    try:
        shutil.copyfile(args.rows_file, work_db)
        server = self_db.SelfServer(db_path=work_db, workdir=workdir)
        server.start()
        conn = server.connect(self_db.USER_A)

        # 暖機・計測ループの開始前に取得する（起動直後の loadavg）。
        # ループ終了後に取得すると計測自体の負荷が混入し、`loadavg_start`
        # という変数名と観測時点が一致しなくなる（codex-review P2 指摘）。
        loadavg_start = read_loadavg()

        rss: dict = {}
        rss["start"] = _rss_snapshot(server.proc.pid)

        if args.mode == "warm_where_then_hybrid":
            for i in range(args.warm_where):
                qv = query_vecs[i % len(query_vecs)]
                self_db._exec_ids(
                    conn,
                    f"SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{qv}' LIMIT 10",
                )
        elif args.mode == "body_predicate":
            for i in range(args.warm_where):
                self_db._exec_ids(
                    conn,
                    f"SELECT id FROM docs WHERE body LIKE '{body_prefix}%' "
                    f"ORDER BY embedding <=> '{query_vecs[i % len(query_vecs)]}' LIMIT 10",
                )
        rss["after_warm"] = _rss_snapshot(server.proc.pid)

        latencies_us: list[float] = []
        if args.mode == "body_predicate":
            for i in range(args.iters):
                qv = query_vecs[i % len(query_vecs)]
                sql = (
                    f"SELECT id FROM docs WHERE body LIKE '{body_prefix}%' "
                    f"ORDER BY embedding <=> '{qv}' LIMIT 10"
                )
                t0 = time.perf_counter()
                self_db._exec_ids(conn, sql)
                latencies_us.append((time.perf_counter() - t0) * 1_000_000.0)
        else:
            for i in range(args.iters):
                qv = query_vecs[i % len(query_vecs)]
                qt = query_texts[i % len(query_texts)]
                sql = (
                    f"SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '{qv}', "
                    f"body, '{qt}') LIMIT 10"
                )
                t0 = time.perf_counter()
                self_db._exec_ids(conn, sql)
                latencies_us.append((time.perf_counter() - t0) * 1_000_000.0)

        rss["end"] = _rss_snapshot(server.proc.pid)

        meta = {
            "mode": args.mode,
            "iters": args.iters,
            "warm_where": args.warm_where if args.mode != "hybrid" else 0,
            "body_prefix": body_prefix,
            "binary": self_db._binary_version_string(server.binary),
            "loadavg_start": loadavg_start,
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        }
        payload = {
            "meta": meta,
            "rss": rss,
            "latencies_us": latencies_us,
            "stats": latency_stats(latencies_us),
        }
        with open(args.out, "w", encoding="utf-8") as f:
            json.dump(payload, f, ensure_ascii=False, indent=2)
            f.write("\n")
        return 0
    finally:
        if server is not None:
            server.stop()
        shutil.rmtree(run_dir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
