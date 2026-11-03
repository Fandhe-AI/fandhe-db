"""MongoDB（`mongodb/mongodb-atlas-local:latest`。mongod + mongot 同梱で
Atlas Vector Search をローカル実行できるイメージ。127.0.0.1:37017 既定。
環境変数 `CROSSDB_MONGODB_PORT` で上書き可）の機能別ベンチマーク実装。

pymongo 4.18.1 で接続する（単一ノードのレプリカセットとして初期化される
イメージのため `directConnection=true` でトポロジ探索〔コンテナ内部の
ホスト名が呼び出し側から解決できない問題〕を回避する）。距離指標は自作 DB
の `<=>`（内積）に合わせ `$vectorSearch` の `similarity: "dotProduct"` を使う。
RLS 相当は、自作 DB の現行契約（実機確認: どのテナントの wire セッションからも
`visibility = 'public'` の行のみが可視。private 行は所有テナント自身からも
不可視）に合わせ、`visibility` フィールドへの `$match`／`$vectorSearch.filter`
で模する（`tenant` では絞り込まない）。

索引構成は `--config exact|hnsw` で切り替える。Atlas Vector Search は索引
なしでは `$vectorSearch` 自体を受け付けないため、exact 構成でも同じ
`vectorSearch` 型索引を作成したうえで `exact: true`（全件探索）を指定する
（README・common.py 同様に「索引ありだが全件探索」であることをここに明記
する）。hnsw 構成は `numCandidates = max(64, k)` の近似探索。索引はどちらの
構成でも作成直後に mongot 側の非同期反映（構築・`queryable` 化）があるため、
`_wait_index_queryable` で `READY`/`queryable` を待ち、さらにプローブ KNN が
安定して k 件返すことを確認してから計測へ進む（fail-closed。タイムアウト時
は計測を拒否する）。

集計（`agg_count`/`agg_multi`/`group_by_having`）は通常の `$match`/`$group`
集計パイプラインで行う（Atlas Search を経由しない、mongod 本体の機能）。
hybrid は MongoDB 8.1+ の `$rankFusion`（本イメージは mongod 8.3.11 で対応
確認済み）を使い、ベクトル側 `$vectorSearch` パイプラインと全文検索側
`$search`（`search` 型索引。text 型フィールド `body`）パイプラインを RRF で
融合する。クライアント側で RRF を計算する経路は使わない（task 指示）。
"""

from __future__ import annotations

import random
import time

from pymongo import MongoClient
from pymongo.operations import SearchIndexModel

from common import doc_visibility, TENANT_VISIBLE, build_meta, env_port, measure, unsupported

HOST = "127.0.0.1"
# 既定値 37017 は containers.sh の `${CROSSDB_MONGODB_PORT:-37017}` と一致させること。
PORT = env_port("CROSSDB_MONGODB_PORT", 37017)
DBNAME = "bench"
COLLECTION = "docs"
VECTOR_INDEX = "vec_idx"
TEXT_INDEX = "text_idx"

# pgvector_db.py の where_compound_count（`id > 100 AND lang = 'ja'`。visibility
# フィルタは共通）と同一条件にする（nosql-spec.md「pgvector_db.py の条件を
# 確認して同一条件にする」指示に基づく。topic は含めない）。
COMPOUND_ID_GT = 100


def _connect() -> MongoClient:
    return MongoClient(f"mongodb://{HOST}:{PORT}/?directConnection=true")


def _setup_collection(client: MongoClient, dim: int):
    db = client[DBNAME]
    db.drop_collection(COLLECTION)
    coll = db.create_collection(COLLECTION)
    coll.create_index("visibility")
    coll.create_index("lang")
    coll.create_index("id")
    return coll


def _create_search_indexes(coll, dim: int) -> None:
    """vectorSearch 索引（embedding + filter 用フィールド）と search 索引
    （全文検索用の body）を作成する。exact 構成でも vectorSearch 索引自体は
    必須（Atlas は索引なしで `$vectorSearch` を受け付けない）。"""
    vs_index = SearchIndexModel(
        definition={
            "fields": [
                {"type": "vector", "path": "embedding", "numDimensions": dim, "similarity": "dotProduct"},
                {"type": "filter", "path": "visibility"},
                {"type": "filter", "path": "lang"},
                {"type": "filter", "path": "topic"},
            ]
        },
        name=VECTOR_INDEX,
        type="vectorSearch",
    )
    coll.create_search_index(model=vs_index)

    ts_index = SearchIndexModel(
        definition={"mappings": {"dynamic": False, "fields": {"body": {"type": "string"}}}},
        name=TEXT_INDEX,
        type="search",
    )
    coll.create_search_index(model=ts_index)


def _wait_index_queryable(coll, name: str, timeout_s: float = 300.0) -> dict:
    """索引 `name` が `status == "READY"` かつ `queryable == True` になるまで
    待つ（mongot の非同期反映）。未反映のまま計測すると空集合・不安定な
    結果を性能として記録してしまうため、タイムアウト時は拒否する
    （fail-closed。qdrant_db.py の `_wait_indexed` と同じ方針）。"""
    t0 = time.perf_counter()
    while True:
        idxs = list(coll.list_search_indexes(name))
        st = idxs[0] if idxs else None
        if st and st.get("status") == "READY" and st.get("queryable"):
            return {"seconds": time.perf_counter() - t0, "final_status": st.get("status")}
        if time.perf_counter() - t0 > timeout_s:
            raise TimeoutError(
                f"mongodb search index {name!r} did not become queryable within "
                f"{timeout_s:.0f}s (last={st})"
            )
        time.sleep(0.5)


def _wait_probe_stable(coll, probe_vec: list[float], k: int, timeout_s: float = 120.0) -> None:
    """`queryable` 化直後は反映件数が揺れることがあるため、同一クエリを 2 回
    連続で実行して `k` 件・同一結果が返るまで待つ（nosql-spec.md「プローブ
    KNN が k 件返して安定するまで待ってから計測」）。安定しないままタイム
    アウトした場合は計測を拒否する（fail-closed）。"""

    def probe():
        res = coll.aggregate(
            [
                {
                    "$vectorSearch": {
                        "index": VECTOR_INDEX,
                        "path": "embedding",
                        "queryVector": probe_vec,
                        "numCandidates": max(64, k),
                        "limit": k,
                        "filter": {"visibility": {"$eq": "public"}},
                    }
                },
                {"$project": {"_id": 0, "id": 1}},
            ]
        )
        return tuple(d["id"] for d in res)

    t0 = time.perf_counter()
    prev = probe()
    while True:
        if time.perf_counter() - t0 > timeout_s:
            raise TimeoutError(
                f"mongodb vector probe did not stabilize within {timeout_s:.0f}s (last={prev})"
            )
        time.sleep(0.5)
        cur = probe()
        if len(cur) == k and cur == prev:
            return
        prev = cur


def _ingest_bulk(coll, docs: list[dict]) -> dict:
    t0 = time.perf_counter()
    batch = 2000
    for i in range(0, len(docs), batch):
        chunk = docs[i : i + batch]
        coll.insert_many(
            [
                {
                    "_id": d["id"],
                    "id": d["id"],
                    "tenant": d["tenant"],
                    # 旧フィクスチャ（visibility 未導入）との互換のため
                    # 欠落時は fail-closed で private 扱いにする。
                    "visibility": doc_visibility(d),
                    "lang": d["lang"],
                    "topic": d.get("topic", ""),
                    "body": d["body"],
                    "embedding": [float(x) for x in d["embedding"]],
                }
                for d in chunk
            ],
            ordered=False,
        )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": len(docs), "seconds": elapsed, "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None}


def _ingest_single_stmt(coll, dim: int, n_rows: int = 1000) -> dict:
    """1 行ずつ `insert_one`（既定の write concern `w:1`。durable write）で
    1,000 件投入する。合成行は他フェーズ（recall 検算含む）を汚染しないよう
    最後に実行する（qdrant_db.py／pgvector_db.py と同じ順序方針）。"""
    t0 = time.perf_counter()
    for n in range(n_rows):
        rid = 10_000_000 + n
        emb = [random.random() * 2 - 1 for _ in range(dim)]
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        coll.insert_one(
            {
                "_id": rid,
                "id": rid,
                "tenant": TENANT_VISIBLE,
                "visibility": "private",
                "lang": lang,
                "topic": topic,
                "body": f"crossdb bench ingest row {n}",
                "embedding": emb,
            }
        )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": n_rows, "seconds": elapsed, "rows_per_sec": n_rows / elapsed if elapsed > 0 else None}


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    client = _connect()
    dim = len(docs[0]["embedding"]) if docs else 128
    coll = _setup_collection(client, dim)

    phases: dict = {}
    phases["ingest_bulk"] = _ingest_bulk(coll, docs)

    _create_search_indexes(coll, dim)
    vec_wait = _wait_index_queryable(coll, VECTOR_INDEX)
    txt_wait = _wait_index_queryable(coll, TEXT_INDEX)
    if args.config == "hnsw":
        # exact 構成では「索引なし全件探索」相当（exact:true）のため index_build
        # フェーズとしては計上しない（nosql-spec.md「hnsw のみ」）。索引自体は
        # exact 構成でも必須のため _wait_index_queryable はどちらの構成でも呼ぶ。
        phases["index_build"] = {**vec_wait, "text_index_seconds": txt_wait["seconds"]}

    query_vecs = [[float(x) for x in q["embedding"]] for q in queries]
    query_texts = [q.get("text", "") for q in queries]

    # プローブの期待件数は可視（public）行数に合わせる（可視行が k 未満の fixture でも
    # 索引準備完了を判定できるように）。可視行 0 件は計測対象として成立しないため拒否する
    n_public = sum(1 for d in docs if doc_visibility(d) == "public")
    if n_public == 0:
        raise RuntimeError("fixture has no public rows; nothing to probe or measure")
    _wait_probe_stable(coll, query_vecs[0], min(10, n_public))

    public_only_filter = {"visibility": {"$eq": "public"}}
    public_only_lang_ja_filter = {"$and": [{"visibility": {"$eq": "public"}}, {"lang": {"$eq": "ja"}}]}

    def vector_search_stage(qv, k, flt, num_candidates=None):
        stage = {
            "index": VECTOR_INDEX,
            "path": "embedding",
            "queryVector": qv,
            "limit": k,
            "filter": flt,
        }
        if args.config == "hnsw":
            stage["numCandidates"] = num_candidates if num_candidates is not None else max(64, k)
        else:
            stage["exact"] = True
        return {"$vectorSearch": stage}

    def knn(qv):
        res = coll.aggregate(
            [
                vector_search_stage(qv, 10, public_only_filter),
                {"$project": {"_id": 0, "id": 1}},
            ]
        )
        return [d["id"] for d in res]

    stats, _ = measure(knn, query_vecs)
    knn_ids_all = [knn(qv) for qv in query_vecs]
    phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

    def knn_where(qv):
        res = coll.aggregate(
            [
                vector_search_stage(qv, 10, public_only_lang_ja_filter),
                {"$project": {"_id": 0, "id": 1}},
            ]
        )
        return [d["id"] for d in res]

    stats, _ = measure(knn_where, query_vecs)
    phases["vector_knn_where"] = stats
    phases["point_where"] = {"note": "vector_knn_where と同一クエリ形のため統合", **stats}

    def compound_count(_):
        return coll.count_documents(
            {"visibility": "public", "lang": "ja", "id": {"$gt": COMPOUND_ID_GT}}
        )

    stats, last = measure(compound_count, [None])
    phases["where_compound_count"] = {**stats, "result": last}

    def agg_count(_):
        return coll.count_documents({"visibility": "public"})

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    def agg_multi(_):
        res = list(
            coll.aggregate(
                [
                    {"$match": {"visibility": "public"}},
                    {
                        "$group": {
                            "_id": None,
                            "count": {"$sum": 1},
                            "sum_id": {"$sum": "$id"},
                            "avg_id": {"$avg": "$id"},
                            "min_id": {"$min": "$id"},
                            "max_id": {"$max": "$id"},
                        }
                    },
                ]
            )
        )
        return res[0] if res else None

    stats, last = measure(agg_multi, [None])
    phases["agg_multi"] = {
        **stats,
        "result": [last["count"], last["sum_id"], last["avg_id"], last["min_id"], last["max_id"]]
        if last
        else None,
    }

    def group_by(_):
        return list(
            coll.aggregate(
                [
                    {"$match": {"visibility": "public"}},
                    {"$group": {"_id": "$lang", "n": {"$sum": 1}}},
                    {"$match": {"n": {"$gt": 1}}},
                    {"$sort": {"n": -1}},
                    {"$limit": 5},
                ]
            )
        )

    stats, last = measure(group_by, [None])
    phases["group_by_having"] = {**stats, "result": [(d["_id"], d["n"]) for d in last]}

    # --- hybrid_rrf: $rankFusion によるネイティブ RRF 融合（k=10。vec/txt 各 top50） ---
    def hybrid(i):
        qv, qt = query_vecs[i], query_texts[i]
        pipeline = [
            {
                "$rankFusion": {
                    "input": {
                        "pipelines": {
                            "vec": [vector_search_stage(qv, 50, public_only_filter)],
                            "txt": [
                                {"$search": {"index": TEXT_INDEX, "text": {"query": qt, "path": "body"}}},
                                {"$match": {"visibility": "public"}},
                                {"$limit": 50},
                            ],
                        }
                    },
                    "combination": {"weights": {"vec": 1, "txt": 1}},
                }
            },
            {"$limit": 10},
            {"$project": {"_id": 0, "id": 1}},
        ]
        return [d["id"] for d in coll.aggregate(pipeline)]

    idxs = list(range(len(query_vecs)))
    stats, _ = measure(hybrid, idxs)
    phases["hybrid_rrf"] = stats

    phases["mode_recall"] = unsupported("MongoDB にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("MongoDB にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported("自作 DB の宣言的 UDF 呼び出し相当の機能が無い")

    def explain_query(qv):
        cmd = {
            "aggregate": COLLECTION,
            "pipeline": [
                vector_search_stage(qv, 10, public_only_filter),
                {"$project": {"_id": 0, "id": 1}},
            ],
            "cursor": {},
        }
        res = client[DBNAME].command("explain", cmd, verbosity="queryPlanner")
        return res.get("queryPlanner") or res.get("stages")

    stats, last = measure(explain_query, query_vecs)
    phases["explain"] = {**stats, "sample_output": str(last)[:2000]}

    # --- 広域取得（bulk fetch）: id と body を Top-N でまとめて返す ---
    def bulk_knn(k: int, flt):
        def _run(qv):
            res = coll.aggregate(
                [
                    vector_search_stage(qv, k, flt),
                    {"$project": {"_id": 0, "id": 1, "body": 1}},
                ]
            )
            return [(d["id"], d["body"]) for d in res]

        return _run

    for k in (200, 1000):
        stats, last = measure(bulk_knn(k, public_only_filter), query_vecs)
        phases[f"bulk_knn_k{k}"] = {
            **stats,
            "k": k,
            "rows_returned": len(last),
            "num_candidates": max(64, k) if args.config == "hnsw" else None,
        }

    stats, last = measure(bulk_knn(200, public_only_lang_ja_filter), query_vecs)
    phases["bulk_knn_where_k200"] = {
        **stats,
        "k": 200,
        "rows_returned": len(last),
        "num_candidates": max(64, 200) if args.config == "hnsw" else None,
    }

    def bulk_hybrid(i):
        qv, qt = query_vecs[i], query_texts[i]
        pipeline = [
            {
                "$rankFusion": {
                    "input": {
                        "pipelines": {
                            "vec": [vector_search_stage(qv, 200, public_only_filter, num_candidates=max(64, 200))],
                            "txt": [
                                {"$search": {"index": TEXT_INDEX, "text": {"query": qt, "path": "body"}}},
                                {"$match": {"visibility": "public"}},
                                {"$limit": 200},
                            ],
                        }
                    },
                    "combination": {"weights": {"vec": 1, "txt": 1}},
                }
            },
            {"$limit": 200},
            {"$project": {"_id": 0, "id": 1, "body": 1}},
        ]
        return [(d["id"], d["body"]) for d in coll.aggregate(pipeline)]

    stats, last = measure(bulk_hybrid, idxs)
    phases["bulk_hybrid_k200"] = {**stats, "k": 200, "rows_returned": len(last), "candidate_pool": 200}

    def scan_nosort(_):
        res = coll.find(
            {"visibility": "public", "lang": "ja"}, {"_id": 0, "id": 1, "body": 1}
        ).limit(500)
        return [(d["id"], d["body"]) for d in res]

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

    # MongoDB にはテナント別セッション（自作 DB の wire セッションに相当する
    # もの）の概念が無いため、tenant-b「セッション」を模す接続は作らず、
    # 同じ public-only フィルタを再実行して agg_count と同値になることの
    # 一致確認とする（自作 DB は tenant-a・tenant-b いずれのセッションでも
    # `visibility = 'public'` の行のみ可視という現行契約——実機確認済み）。
    def rls_count(_):
        return coll.count_documents({"visibility": "public"})

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {
        **stats,
        "tenant_b_count": last,
        "note": "MongoDB にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    # ingest_single_stmt はテーブルへ合成行（乱数ベクトル）を追加するため、
    # 他フェーズ（特に vector_knn の recall 検算）を汚染しないよう最後に実行する。
    phases["ingest_single_stmt"] = _ingest_single_stmt(coll, dim)

    build_info = client.admin.command("buildInfo")
    meta = build_meta(
        db="mongodb",
        version=f"mongod {build_info.get('version')} (image mongodb/mongodb-atlas-local:latest)",
        connection="TCP (directConnection)",
        config=args.config,
        rows=len(docs),
        dim=dim,
        extra={
            "similarity": "dotProduct",
            "vector_index": "vectorSearch (exact=True)" if args.config == "exact" else "vectorSearch (numCandidates=max(64,k))",
            "text_index": "search (dynamic=False, body:string)",
        },
    )
    return {"meta": meta, "phases": phases}
