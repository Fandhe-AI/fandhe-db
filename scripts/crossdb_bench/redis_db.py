"""Redis（`redis:8`（Redis 8.10.1・Query Engine=RediSearch 8.1 同梱）、
127.0.0.1:36379 既定。環境変数 `CROSSDB_REDIS_PORT` で上書き可。`containers.sh`
と同じ変数を読む）の機能別ベンチマーク実装。

HASH `doc:<id>` へ列を保持し、RediSearch の `FT.CREATE`（VECTOR フィールド。
exact 構成 = FLAT、hnsw 構成 = HNSW・M=16・EF_CONSTRUCTION=100・検索時
EF_RUNTIME=max(64,k)）で索引を張る。距離指標は自作 DB の `<=>`（内積。値が
大きいほど上位）に合わせ `DISTANCE_METRIC IP` を使う。RediSearch の IP
距離は `1 - 内積`（値が小さいほど上位）としてスコアリングされる
（実機確認: `FT.SEARCH ... SORTBY score` の既定昇順でそのまま上位が先頭に
来る。降順へ反転する必要はない）。

RLS 相当は、自作 DB の現行契約（実機確認: どのテナントの wire セッションから
も `visibility = 'public'` の行のみが可視。private 行は所有テナント自身
からも不可視）に合わせ、TAG フィールド `visibility` への `@visibility:{public}`
フィルタを毎クエリへ前置して模する（`tenant` では絞り込まない）。

集計は `FT.AGGREGATE` の GROUPBY/REDUCE（COUNT/SUM/AVG/MIN/MAX）・FILTER で
実装する。hybrid は RediSearch 8.1 の `FT.HYBRID`（SEARCH 句 = 全文検索、
VSIM 句 = KNN、COMBINE RRF）をネイティブ機能として計測する。
"""

from __future__ import annotations

import os
import struct
import time

import redis
from redis.commands.search.aggregation import AggregateRequest, Desc
from redis.commands.search.field import NumericField, TagField, TextField, VectorField
from redis.commands.search.hybrid_query import (
    CombinationMethods,
    CombineResultsMethod,
    HybridFilter,
    HybridPostProcessingConfig,
    HybridQuery,
    HybridSearchQuery,
    HybridVsimQuery,
    VectorSearchMethods,
)
from redis.commands.search.index_definition import IndexDefinition, IndexType
from redis.commands.search.query import Query
import redis.commands.search.reducers as reducers

from common import build_meta, doc_visibility, env_port, measure, unsupported

HOST = "127.0.0.1"
# 既定値 36379 は containers.sh の `${CROSSDB_REDIS_PORT:-36379}` と一致させること。
PORT = env_port("CROSSDB_REDIS_PORT", 36379)
INDEX = "docs_idx"


def _hybrid_window(default: int) -> int:
    """`hybrid_rrf` フェーズの RRF `WINDOW` を診断目的で上書きする opt-in
    （Issue #848。self の hybrid_rrf は既定でプール深さ 200〔`hybrid.rs::
    RrfConfig::default`〕を使うため、Redis 側の既定 WINDOW=50 との条件差が
    negative の主因かを切り分けるための informational 追加 arm）。

    本番スコアボードの計測条件（WINDOW=50）を変えないため、未設定・空文字
    なら必ず `default` を返す。設定時は 1 以上の 10 進整数のみ受理し、
    不正値は fail-closed に `ValueError` で拒否する（`env_port` と同方針）。
    """
    raw = os.environ.get("CROSSDB_REDIS_HYBRID_WINDOW")
    if raw is None or raw == "":
        return default
    if not (raw.isascii() and raw.isdigit()) or int(raw) < 1:
        raise ValueError(f"CROSSDB_REDIS_HYBRID_WINDOW must be a positive integer (got: {raw!r})")
    return int(raw)


PREFIX = "doc:"


def _connect() -> "redis.Redis":
    return redis.Redis(host=HOST, port=PORT, decode_responses=False)


def _vec_bytes(vec) -> bytes:
    """float32 リトルエンディアンのバイト列へ変換する（RediSearch VECTOR フィールドの格納形式）。"""
    return struct.pack(f"<{len(vec)}f", *vec)


def _setup_index(r: "redis.Redis", dim: int, config: str) -> None:
    try:
        r.ft(INDEX).dropindex(delete_documents=True)
    except redis.exceptions.ResponseError:
        pass
    if config == "hnsw":
        vec_attrs = {
            "TYPE": "FLOAT32",
            "DIM": dim,
            "DISTANCE_METRIC": "IP",
            "M": 16,
            "EF_CONSTRUCTION": 100,
        }
        algo = "HNSW"
    else:
        vec_attrs = {"TYPE": "FLOAT32", "DIM": dim, "DISTANCE_METRIC": "IP"}
        algo = "FLAT"
    schema = (
        NumericField("id"),
        TagField("visibility"),
        TagField("lang"),
        TagField("topic"),
        TextField("body"),
        VectorField("embedding", algo, vec_attrs),
    )
    definition = IndexDefinition(prefix=[PREFIX], index_type=IndexType.HASH)
    r.ft(INDEX).create_index(schema, definition=definition)


def _wait_indexed(r: "redis.Redis", rows: int, timeout_s: float = 300.0) -> dict:
    """`FT.INFO` の `percent_indexed` が 1.0 になるまで待つ（未索引状態の性能を
    hnsw の性能として記録しないための fail-closed 待機。qdrant_db.py と同方針）。"""
    t0 = time.perf_counter()
    while True:
        info = r.ft(INDEX).info()
        pct = float(info.get("percent_indexed", 0))
        num_docs = int(info.get("num_docs", 0))
        if pct >= 1.0 and num_docs >= rows:
            return {
                "seconds": time.perf_counter() - t0,
                "percent_indexed": pct,
                "num_docs": num_docs,
            }
        if time.perf_counter() - t0 > timeout_s:
            raise TimeoutError(
                f"redis(RediSearch) index build did not complete within {timeout_s:.0f}s "
                f"(percent_indexed={pct}, num_docs={num_docs}/{rows})"
            )
        time.sleep(0.5)


def _ingest_bulk(r: "redis.Redis", docs: list[dict]) -> dict:
    t0 = time.perf_counter()
    batch = 500
    for i in range(0, len(docs), batch):
        chunk = docs[i : i + batch]
        pipe = r.pipeline(transaction=False)
        for d in chunk:
            pipe.hset(
                f"{PREFIX}{d['id']}",
                mapping={
                    "id": d["id"],
                    "tenant": d["tenant"],
                    "visibility": doc_visibility(d),
                    "lang": d["lang"],
                    "topic": d.get("topic", ""),
                    "body": d["body"],
                    "embedding": _vec_bytes(d["embedding"]),
                },
            )
        pipe.execute()
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": len(docs),
        "seconds": elapsed,
        "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None,
    }


def _ingest_single_stmt(r: "redis.Redis", dim: int, n_rows: int = 1000) -> dict:
    """1 行ずつ HSET（durable write。Redis 既定の非同期 AOF/RDB のまま——
    fsync なし既定。永続化設定そのものは変更しない）。"""
    import random

    t0 = time.perf_counter()
    for n in range(n_rows):
        rid = 10_000_000 + n
        emb = [random.random() * 2 - 1 for _ in range(dim)]
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        r.hset(
            f"{PREFIX}{rid}",
            mapping={
                "id": rid,
                "tenant": "tenant-a",
                "visibility": "private",
                "lang": lang,
                "topic": topic,
                "body": f"crossdb bench ingest row {n}",
                "embedding": _vec_bytes(emb),
            },
        )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": n_rows,
        "seconds": elapsed,
        "rows_per_sec": n_rows / elapsed if elapsed > 0 else None,
        "extra": "既定の非同期 AOF/RDB のまま（fsync なし既定）。永続化設定は変更していない",
    }


# FT.HYBRID が「存在しない／構文非対応」として拒否されたときの応答文（RediSearch の
# unknown command・Syntax error 系）。これ以外の ResponseError（メモリ上限・
# 実行時エラー）は未対応機能ではないため unsupported へ丸めない。
_HYBRID_UNAVAILABLE_MARKERS = ("unknown command", "unknown subcommand", "syntax error", "not supported", "unsupported")


def _is_hybrid_unavailable(e: redis.exceptions.ResponseError) -> bool:
    text = str(e).lower()
    return any(m in text for m in _HYBRID_UNAVAILABLE_MARKERS)


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    r = _connect()
    dim = len(docs[0]["embedding"]) if docs else 128
    _setup_index(r, dim, args.config)
    phases: dict = {}
    phases["ingest_bulk"] = _ingest_bulk(r, docs)
    if args.config == "hnsw":
        phases["index_build"] = _wait_indexed(r, len(docs))

    query_vecs = [q["embedding"] for q in queries]
    query_texts = [q["text"] for q in queries]
    ef_runtime = None
    if args.config == "hnsw":
        ef_runtime = 64  # max(64, k) with k=10

    def _knn_query(k: int, ef: int | None, extra_filter: str = "") -> str:
        base = f"(@visibility:{{public}}{extra_filter})"
        clause = f"[KNN {k} @embedding $vec"
        if ef is not None:
            clause += f" EF_RUNTIME {ef}"
        clause += " AS score]"
        return f"{base}=>{clause}"

    def knn(qv):
        q = (
            Query(_knn_query(10, ef_runtime))
            .sort_by("score")
            .return_fields("id")
            .dialect(2)
            .paging(0, 10)
        )
        res = r.ft(INDEX).search(q, query_params={"vec": _vec_bytes(qv)})
        return [int(doc.id.split(":", 1)[1]) for doc in res.docs]

    stats, _ = measure(knn, query_vecs)
    knn_ids_all = [knn(qv) for qv in query_vecs]
    phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

    def knn_where(qv):
        q = (
            Query(_knn_query(10, ef_runtime, r" @lang:{ja}"))
            .sort_by("score")
            .return_fields("id")
            .dialect(2)
            .paging(0, 10)
        )
        res = r.ft(INDEX).search(q, query_params={"vec": _vec_bytes(qv)})
        return [int(doc.id.split(":", 1)[1]) for doc in res.docs]

    stats, _ = measure(knn_where, query_vecs)
    phases["vector_knn_where"] = stats
    phases["point_where"] = {"note": "vector_knn_where と同一クエリ形のため統合", **stats}

    def compound_count(_):
        req = AggregateRequest(
            "(@visibility:{public} @lang:{ja} @id:[(100 +inf])"
        ).group_by([], reducers.count().alias("n"))
        res = r.ft(INDEX).aggregate(req)
        if not res.rows:
            return 0
        row = res.rows[0]
        return int(row[row.index(b"n") + 1]) if b"n" in row else 0

    stats, last = measure(compound_count, [None])
    phases["where_compound_count"] = {**stats, "result": last}

    def agg_count(_):
        req = AggregateRequest("(@visibility:{public})").group_by(
            [], reducers.count().alias("n")
        )
        res = r.ft(INDEX).aggregate(req)
        if not res.rows:
            return 0
        row = res.rows[0]
        return int(row[row.index(b"n") + 1])

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    def agg_multi(_):
        req = AggregateRequest("(@visibility:{public})").group_by(
            [],
            reducers.count().alias("cnt"),
            reducers.sum("@id").alias("sum_id"),
            reducers.avg("@id").alias("avg_id"),
            reducers.min("@id").alias("min_id"),
            reducers.max("@id").alias("max_id"),
        )
        res = r.ft(INDEX).aggregate(req)
        if not res.rows:
            return None
        row = res.rows[0]
        d = dict(zip(row[0::2], row[1::2]))
        return {k.decode(): float(v) for k, v in d.items()}

    stats, last = measure(agg_multi, [None])
    phases["agg_multi"] = {**stats, "result": last}

    def group_by_having(_):
        req = (
            AggregateRequest("(@visibility:{public})")
            .group_by(["@lang"], reducers.count().alias("n"))
            .filter("@n > 1")
            .sort_by(Desc("@n"), max=5)
        )
        res = r.ft(INDEX).aggregate(req)
        out = []
        for row in res.rows:
            d = dict(zip(row[0::2], row[1::2]))
            out.append({k.decode(): (v.decode() if k == b"lang" else int(v)) for k, v in d.items()})
        return out

    stats, last = measure(group_by_having, [None])
    phases["group_by_having"] = {**stats, "result": last}

    # --- hybrid_rrf: FT.HYBRID（SEARCH 句=全文・VSIM 句=KNN・COMBINE RRF） ---
    def hybrid(i):
        qv, qt = query_vecs[i], query_texts[i]
        terms = " ".join(t for t in qt.split() if t.isalnum())[:200] or "the"
        sq = HybridSearchQuery(f"(@visibility:{{public}}) @body:({terms})")
        vq = HybridVsimQuery(
            "@embedding",
            "$vec",
            vsim_search_method=VectorSearchMethods.KNN,
            vsim_search_method_params={"K": 50},
            filter=HybridFilter("@visibility:{public}"),
        )
        hq = HybridQuery(sq, vq)
        combine = CombineResultsMethod(CombinationMethods.RRF, WINDOW=_hybrid_window(50))
        pp = HybridPostProcessingConfig().load("@id").limit(0, 10)
        res = r.ft(INDEX).hybrid_search(
            hq, combine_method=combine, post_processing=pp, params_substitution={"vec": _vec_bytes(qv)}
        )
        return [int(row["id"]) for row in res.results]

    idxs = list(range(len(queries)))
    try:
        stats, _ = measure(hybrid, idxs)
        phases["hybrid_rrf"] = stats
    except redis.exceptions.ResponseError as e:
        # 既知の「機能が無い／構文非対応」応答のみ unsupported へ。接続断・タイムアウト・
        # 結果処理の例外は fail-closed（そのまま伝播して計測全体を失敗させる）
        if not _is_hybrid_unavailable(e):
            raise
        phases["hybrid_rrf"] = unsupported(f"FT.HYBRID が利用できない: {e}")

    phases["mode_recall"] = unsupported("RediSearch にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("RediSearch にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported("自作 DB の宣言的 UDF 呼び出し相当の機能が無い")

    # --- 広域取得（bulk fetch）: id と body を Top-N でまとめて返す ---
    def bulk_ef(k: int) -> int | None:
        return max(64, k) if args.config == "hnsw" else None

    def bulk_knn(k: int, extra_filter: str = ""):
        ef = bulk_ef(k)

        def _run(qv):
            q = (
                Query(_knn_query(k, ef, extra_filter))
                .sort_by("score")
                .return_fields("id", "body")
                .dialect(2)
                .paging(0, k)
            )
            res = r.ft(INDEX).search(q, query_params={"vec": _vec_bytes(qv)})
            return [(int(doc.id.split(":", 1)[1]), doc.body) for doc in res.docs]

        return _run

    for k in (200, 1000):
        stats, last = measure(bulk_knn(k), query_vecs)
        phases[f"bulk_knn_k{k}"] = {**stats, "k": k, "rows_returned": len(last), "ef_runtime": bulk_ef(k)}

    stats, last = measure(bulk_knn(200, r" @lang:{ja}"), query_vecs)
    phases["bulk_knn_where_k200"] = {**stats, "k": 200, "rows_returned": len(last), "ef_runtime": bulk_ef(200)}

    def bulk_hybrid(i):
        qv, qt = query_vecs[i], query_texts[i]
        terms = " ".join(t for t in qt.split() if t.isalnum())[:200] or "the"
        sq = HybridSearchQuery(f"(@visibility:{{public}}) @body:({terms})")
        vq = HybridVsimQuery(
            "@embedding",
            "$vec",
            vsim_search_method=VectorSearchMethods.KNN,
            vsim_search_method_params={"K": 200},
            filter=HybridFilter("@visibility:{public}"),
        )
        hq = HybridQuery(sq, vq)
        combine = CombineResultsMethod(CombinationMethods.RRF, WINDOW=200)
        pp = HybridPostProcessingConfig().load("@id", "@body").limit(0, 200)
        res = r.ft(INDEX).hybrid_search(
            hq, combine_method=combine, post_processing=pp, params_substitution={"vec": _vec_bytes(qv)}
        )
        return [(int(row["id"]), row.get("body")) for row in res.results]

    try:
        stats, last = measure(bulk_hybrid, idxs)
        phases["bulk_hybrid_k200"] = {**stats, "k": 200, "rows_returned": len(last)}
    except redis.exceptions.ResponseError as e:
        if not _is_hybrid_unavailable(e):
            raise
        phases["bulk_hybrid_k200"] = unsupported(f"FT.HYBRID が利用できない: {e}")

    def scan_nosort(_):
        q = (
            Query("(@visibility:{public} @lang:{ja})")
            .return_fields("id", "body")
            .paging(0, 500)
            .dialect(2)
        )
        res = r.ft(INDEX).search(q)
        return [(int(doc.id.split(":", 1)[1]), doc.body) for doc in res.docs]

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

    # Redis にはテナント別セッションの概念が無いため、agg_count と同じ
    # public-only フィルタを再実行して値の一致確認とする（qdrant_db.py と同方針）。
    def rls_count(_):
        req = AggregateRequest("(@visibility:{public})").group_by(
            [], reducers.count().alias("n")
        )
        res = r.ft(INDEX).aggregate(req)
        if not res.rows:
            return 0
        row = res.rows[0]
        return int(row[row.index(b"n") + 1])

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {
        **stats,
        "tenant_b_count": last,
        "note": "Redis にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    def explain(_):
        return r.execute_command("FT.EXPLAIN", INDEX, "(@visibility:{public} @lang:{ja})", "DIALECT", "2")

    stats, last = measure(explain, [None])
    phases["explain"] = {**stats, "result": last.decode(errors="replace") if isinstance(last, bytes) else last}

    info = r.ft(INDEX).info()
    phases["ingest_single_stmt"] = _ingest_single_stmt(r, dim)

    server_info = r.info()
    modules = r.execute_command("MODULE", "LIST")
    search_ver = None
    for m in modules:
        if m.get(b"name") == b"search":
            search_ver = m.get(b"ver")
            break

    meta = build_meta(
        db="redis",
        version=(
            f"redis {server_info.get('redis_version')} "
            f"(RediSearch ver={search_ver}, image redis:8)"
        ),
        connection="TCP (redis-py 8.1.0, RESP2)",
        config=args.config,
        rows=len(docs),
        dim=dim,
        extra={
            "distance": "IP",
            "hnsw_config": {"m": 16, "ef_construct": 100} if args.config == "hnsw" else None,
            "search_params": "ef_runtime=64" if args.config == "hnsw" else "FLAT (exact)",
            "num_docs": info.get("num_docs"),
        },
    )
    return {"meta": meta, "phases": phases}
