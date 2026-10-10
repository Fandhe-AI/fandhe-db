#!/usr/bin/env python3
"""crossdb_bench のエントリポイント。

`--db` で選んだモジュール（self_db.py 等）へフィクスチャを渡して全フェーズを
実行させ、`vector_knn` フェーズの返却 id を ground truth（recall.py）と
突き合わせて `recall_at_10` を計算したうえで `<out-dir>/<db>_<config>.json`
へ書き出す。

対照 DB のコンテナ起動・停止は `containers.sh up|down <db>` に分離してある
（このスクリプトは「既にコンテナが起動している」ことを前提にする。self は
wire-server 子プロセスを db モジュール側で直接起動・停止する）。

使い方:
    python run.py --db self --config exact \\
        --rows-file $S/docs25k.redb --queries-file $S/queries200.jsonl
    python run.py --db pgvector --config hnsw \\
        --rows-file $S/docs25k.jsonl --queries-file $S/queries200.jsonl
"""

from __future__ import annotations

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from common import load_jsonl, write_result  # noqa: E402
from recall import build_ground_truth, recall_at_k, recall_at_k_tie_tolerant  # noqa: E402

DB_MODULES = [
    "self",
    "self_nosql",
    "pgvector",
    "sqlite_vec",
    "qdrant",
    "lancedb",
    "mysql",
    "mongodb",
    "mongodb_plain",
    "redis",
    "elasticsearch",
]

# self（SQL wire）・self_nosql（HTTP NoSQL 表層）はいずれも --rows-file に redb
# ファイルパスを取り、docs jsonl は不要（wire-server が既存 redb をそのまま開く）。
# self 専用の分岐（resolve_docs_file・dispatch）をこの集合で共有する。
_SELF_DB_MODULES = ("self", "self_nosql")


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="crossdb_bench: 機能別ベンチマーク実行")
    p.add_argument("--db", required=True, choices=DB_MODULES)
    p.add_argument("--config", required=True, choices=["exact", "hnsw"])
    p.add_argument(
        "--rows-file",
        required=True,
        help="self の場合は redb ファイルパス。他 DB は docs jsonl（docs25k.jsonl 等）パス",
    )
    p.add_argument("--queries-file", required=True, help="queries200.jsonl のパス")
    p.add_argument(
        "--docs-file",
        default=None,
        help="recall ground truth 計算用の docs jsonl（省略時は self 以外は --rows-file と同じ、"
        "self は --rows-file と同じディレクトリの docs25k.jsonl を既定で探す）",
    )
    p.add_argument("--out-dir", default=None, help="結果 JSON の出力先（既定: <queries-file と同じディレクトリ>/results）")
    p.add_argument("--workdir", default=None, help="作業用一時ディレクトリ（既定: --rows-file と同じディレクトリ）")
    p.add_argument(
        "--expect-dim",
        type=int,
        default=None,
        help="docs／queries の埋め込み長がこの値と一致することを要求する（Issue #466。"
        "dim 別 fixture を取り違えたまま計測を続けさせず、不一致は非 0 終了で拒否する）",
    )
    return p.parse_args()


def resolve_docs_file(args: argparse.Namespace) -> str:
    """recall ground truth 計算用の docs jsonl パスを決める。

    self は `--rows-file`（redb）と対になる docs jsonl を `--docs-file` 省略時に
    自動探索する。dim 別 fixture（`docs25k-d768.redb` 等）でも見つけられるよう、
    固定名 `docs25k.jsonl` ではなく `--rows-file` と同じ basename（拡張子のみ
    `.jsonl` へ）を使う（Issue #466。`docs25k.redb` → `docs25k.jsonl` は従来どおり
    同じ結果になるため後方互換）。
    """
    if args.docs_file:
        return args.docs_file
    if args.db in _SELF_DB_MODULES:
        base = os.path.splitext(os.path.basename(args.rows_file))[0]
        candidate = os.path.join(os.path.dirname(os.path.abspath(args.rows_file)), f"{base}.jsonl")
        return candidate
    return args.rows_file


def _peek_embedding_dim(jsonl_path: str) -> int | None:
    """`jsonl_path`（docs25k.jsonl 等）の先頭 1 行だけを読み `embedding` の長さを
    返す（Issue #466。dim 一致検査のためだけに数万行のフィクスチャ全体を
    `load_jsonl` で読み込むのは dim=768／1536 で数百 MB になり無駄なため、
    行単位で最初の非空行のみをパースする）。"""
    import json as _json

    with open(jsonl_path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            doc = _json.loads(line)
            embedding = doc.get("embedding")
            return len(embedding) if embedding is not None else None
    return None


def _scan_docs_dim_streaming(jsonl_path: str) -> tuple[int | None, str | None]:
    """`jsonl_path`（docs jsonl）を行単位ストリーミングで全件走査し、`embedding`
    次元が全行で一致するかを検証する（codex-review P2 指摘・PR #557。従来の
    `_peek_embedding_dim` は先頭の非空行だけを見ており、後続レコードだけ次元が
    異なる fixture（dim 混在）を `--expect-dim` 検査が見逃していた。数万行でも
    メモリに全件展開しない行単位読み込みを維持する）。

    `embedding` が欠落・null のレコードは黙って読み飛ばさず、行番号付きエラー
    として拒否する（codex-review P2 指摘・PR #557。従来は `continue` で除外して
    おり、queries を参照しない mysql アダプタ等では欠落を検知できないまま
    adapter 呼び出し・結果 JSON の書き出しまで進んでしまっていた。README の
    「docs／queries の埋め込み長を fail-closed に検証する」契約に合わせる）。

    戻り値は `(確認できた次元, 不一致時のエラー理由)`。次元を 1 件も確認できな
    かった場合は `(None, None)` を返す（呼び出し側で「取得失敗」として扱う）。
    """
    import json as _json

    dim: int | None = None
    with open(jsonl_path, "r", encoding="utf-8") as f:
        for lineno, line in enumerate(f, start=1):
            line = line.strip()
            if not line:
                continue
            doc = _json.loads(line)
            embedding = doc.get("embedding")
            if embedding is None:
                return dim, f"docs line {lineno} is missing embedding (null or absent)"
            current = len(embedding)
            if dim is None:
                dim = current
            elif current != dim:
                return dim, (
                    f"docs line {lineno} has embedding dim {current}, "
                    f"expected {dim} (from an earlier line)"
                )
    return dim, None


def _scan_queries_dim(queries: list[dict]) -> tuple[int | None, str | None]:
    """読み込み済み `queries`（`load_jsonl` 済みのリスト）全件の `embedding`
    次元が一致するかを検証する（codex-review P2 指摘・PR #557。従来は
    `queries[0]` のみを見ており、後続クエリだけ次元が異なる fixture の
    取り違えを見逃していた）。

    `embedding` が欠落・null のクエリは黙って読み飛ばさず、インデックス付き
    エラーとして拒否する（codex-review P2 指摘・PR #557。`_scan_docs_dim_streaming`
    と同型の理由。README の「docs／queries の埋め込み長を fail-closed に検証
    する」契約に合わせる）。

    戻り値は `(確認できた次元, 不一致時のエラー理由)`。
    """
    dim: int | None = None
    for i, q in enumerate(queries):
        embedding = q.get("embedding")
        if embedding is None:
            return dim, f"queries[{i}] is missing embedding (null or absent)"
        current = len(embedding)
        if dim is None:
            dim = current
        elif current != dim:
            return dim, (
                f"queries[{i}] has embedding dim {current}, "
                f"expected {dim} (from an earlier query)"
            )
    return dim, None


def main() -> int:
    args = parse_args()
    if args.out_dir is None:
        args.out_dir = os.path.join(os.path.dirname(os.path.abspath(args.queries_file)), "results")
    if args.workdir is None:
        args.workdir = os.path.dirname(os.path.abspath(args.rows_file))

    queries = load_jsonl(args.queries_file)

    # docs／queries の埋め込み長が食い違ったまま計測を続けると、dim 別 fixture
    # の取り違え（例: dim=128 の docs に dim=768 の queries）を検出できないまま
    # 不正な結果 JSON を書き出してしまう。self は queries と `--docs-file`（省略時
    # は `resolve_docs_file`）の docs を、他 DB は `--rows-file`（docs 本体）を
    # 突き合わせる（Issue #466。unsupported へ丸めず非 0 終了で拒否する）。
    dim_source_docs = resolve_docs_file(args) if args.db in _SELF_DB_MODULES else args.rows_file
    query_dim = len(queries[0]["embedding"]) if queries and queries[0].get("embedding") is not None else None
    docs_dim = None
    if os.path.exists(dim_source_docs):
        docs_dim = _peek_embedding_dim(dim_source_docs)
    if query_dim is not None and docs_dim is not None and query_dim != docs_dim:
        print(
            f"error: embedding dim mismatch between docs ({docs_dim}) and queries ({query_dim})",
            file=sys.stderr,
        )
        return 1
    if args.expect_dim is not None:
        # --expect-dim 指定時は docs／queries 双方の全レコードを次元検証する
        # （codex-review P2 指摘・PR #557。当初は queries[0] と docs 先頭の
        # 非空行だけを見ており、後続レコードだけ次元が異なる fixture（dim 混在。
        # 例: queries[0]=768 次元・2 件目以降=128 次元）を見逃していた。特に
        # mysql アダプタのように queries を検索に使わない経路では、不一致の
        # まま結果 JSON が書き出されてしまう。読み込み済み queries は全件、
        # docs は行単位ストリーミングで全件検証することで検証漏れを防ぐ）。
        docs_dim_full, docs_dim_err = _scan_docs_dim_streaming(dim_source_docs) if os.path.exists(
            dim_source_docs
        ) else (None, None)
        # docs_dim_err を docs_dim_full is None より先に判定する（Cursor Bugbot 指摘・
        # PR #557 スレッド PRRT_kwDOUAKASM6fqypA）。先頭レコードの embedding が
        # 欠落している場合、`_scan_docs_dim_streaming` は dim を 1 件も確定できない
        # まま行番号付きの理由を docs_dim_err で返す（dim=None・err=行番号付き理由）。
        # 逆順で判定すると「dim を確認できなかった」汎用メッセージに理由が
        # 上書きされ、fixture 生成時のどの行が壊れているか運用者に伝わらなかった。
        if docs_dim_err is not None:
            print(f"error: --expect-dim {args.expect_dim} rejected ({docs_dim_err})", file=sys.stderr)
            return 1
        if docs_dim_full is None:
            reason = (
                f"docs file not found: {dim_source_docs}"
                if not os.path.exists(dim_source_docs)
                else f"failed to determine embedding dim from docs file: {dim_source_docs}"
            )
            print(
                f"error: --expect-dim {args.expect_dim} requires docs dim to be verified ({reason})",
                file=sys.stderr,
            )
            return 1
        # queries 側も同様に次元検証を必須にする（codex-review P2 指摘・PR #557
        # r3943524978）。queries JSONL が空だと query_dim が None のまま docs_dim
        # だけで --expect-dim 判定を素通りしてしまい、mysql アダプタ等 queries を
        # 参照しない db モジュールでは不正な queries フィクスチャ（空・取り違え）を
        # 検出できずに adapter 呼び出しへ進んでしまう。README の「docs／queries の
        # 埋め込み長を fail-closed に検証する」契約に合わせ、queries 側の次元が
        # 取得できない場合も adapter 呼び出し前に非 0 終了で拒否する。
        query_dim_full, query_dim_err = _scan_queries_dim(queries)
        # docs 側と同型の理由で query_dim_err を先に判定する（Cursor Bugbot 指摘・
        # PR #557 スレッド PRRT_kwDOUAKASM6fqypA）。
        if query_dim_err is not None:
            print(f"error: --expect-dim {args.expect_dim} rejected ({query_dim_err})", file=sys.stderr)
            return 1
        if query_dim_full is None:
            print(
                f"error: --expect-dim {args.expect_dim} requires queries dim to be verified "
                f"(queries file is empty or has no embedding: {args.queries_file})",
                file=sys.stderr,
            )
            return 1
        if query_dim_full != docs_dim_full:
            print(
                f"error: embedding dim mismatch between docs ({docs_dim_full}) and queries ({query_dim_full})",
                file=sys.stderr,
            )
            return 1
        actual_dim = docs_dim_full
        if actual_dim != args.expect_dim:
            print(
                f"error: --expect-dim {args.expect_dim} does not match actual dim {actual_dim}",
                file=sys.stderr,
            )
            return 1

    if args.db == "self":
        import self_db

        result = self_db.run(args, queries, docs_file=resolve_docs_file(args))
    elif args.db == "self_nosql":
        import self_nosql

        result = self_nosql.run(args, queries)
    else:
        docs = load_jsonl(args.rows_file)
        module = {
            "pgvector": "pgvector_db",
            "sqlite_vec": "sqlite_vec_db",
            "qdrant": "qdrant_db",
            "lancedb": "lancedb_db",
            "mysql": "mysql_db",
            # NoSQL 対照（2026-09-18 追加）。mongodb は Atlas local（mongot 同梱・
            # ベクトル検索あり）、mongodb_plain は Community（ベクトル検索なし）。
            "mongodb": "mongodb_db",
            "mongodb_plain": "mongodb_plain_db",
            "redis": "redis_db",
            "elasticsearch": "elasticsearch_db",
        }[args.db]
        db_mod = __import__(module)
        result = db_mod.run(args, docs, queries)

    meta = result["meta"]
    phases = result["phases"]

    # --- recall_at_10: vector_knn フェーズが ids_per_query を返していれば ground truth と照合 ---
    docs_file = resolve_docs_file(args)
    knn_phase = phases.get("vector_knn", {})
    ids_per_query = knn_phase.get("ids_per_query") if isinstance(knn_phase, dict) else None
    if ids_per_query and os.path.exists(docs_file):
        strict_top_k, tie_boundaries = build_ground_truth(docs_file, queries, k=10)
        recall_strict = recall_at_k(ids_per_query, strict_top_k)
        recall_tie = recall_at_k_tie_tolerant(ids_per_query, tie_boundaries, k=10)
        # recall_at_10 は同点許容版を主指標とする（同点境界の順序差で exact
        # 構成でも 1.0 にならない問題を解消するため）。従来の厳密一致値は
        # recall_at_10_strict として併記する。
        phases["recall_at_10"] = {
            "recall_at_10": recall_tie,
            "recall_at_10_strict": recall_strict,
            "queries": len(tie_boundaries),
        }
    else:
        phases["recall_at_10"] = {
            "unsupported": True,
            "reason": "vector_knn が unsupported、または docs ファイルが見つからない",
        }

    # ids_per_query は正解照合専用の中間データであり、結果 JSON を肥大化させるため保存しない。
    if isinstance(knn_phase, dict) and "ids_per_query" in knn_phase:
        del knn_phase["ids_per_query"]

    out_path = write_result(args.out_dir, args.db, args.config, meta, phases)
    print(f"wrote: {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
