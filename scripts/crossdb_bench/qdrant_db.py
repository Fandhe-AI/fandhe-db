"""Qdrant（`qdrant/qdrant:latest`、127.0.0.1:16333/16334 既定。環境変数
`CROSSDB_QDRANT_HTTP_PORT`／`CROSSDB_QDRANT_GRPC_PORT` で上書き可。
`containers.sh` と同じ変数を読む）の機能別ベンチマーク実装。

gRPC 経由（`prefer_grpc=True`）で接続する。距離指標は自作 DB の `<=>`
（内積）に合わせ `Distance.DOT` を使う。RLS 相当は、自作 DB の現行契約
（実機確認: どのテナントの wire セッションからも `visibility = 'public'` の
行のみが可視。private 行は所有テナント自身からも不可視）に合わせ、payload
の `visibility` フィールドへの `Filter` で模する（`tenant` では絞り込まず、
payload index も visibility へ作成する）。

集計は `count` のみをネイティブ機能として計測し、GROUP BY・hybrid は
疎ベクトルを自前生成すると自作 DB の BM25 系 hybrid と非等価になるため
task 指示どおり N/A とする。
"""

from __future__ import annotations

import time

from qdrant_client import QdrantClient, models

from common import doc_visibility, TENANT_VISIBLE, build_meta, env_port, measure, unsupported

HOST = "127.0.0.1"
# 既定値 16333/16334 は containers.sh の `${CROSSDB_QDRANT_HTTP_PORT:-16333}`／
# `${CROSSDB_QDRANT_GRPC_PORT:-16334}` と一致させること。gpu/ 配下の Qdrant GPU
# ベンチも同じ変数名を読むが既定値は 17333/17334（別コンテナ・別ポート）。
HTTP_PORT = env_port("CROSSDB_QDRANT_HTTP_PORT", 16333)
GRPC_PORT = env_port("CROSSDB_QDRANT_GRPC_PORT", 16334)
COLLECTION = "docs"


def _connect() -> QdrantClient:
    return QdrantClient(host=HOST, port=HTTP_PORT, grpc_port=GRPC_PORT, prefer_grpc=True)


def _setup_collection(client: QdrantClient, dim: int, config: str) -> None:
    if client.collection_exists(COLLECTION):
        client.delete_collection(COLLECTION)
    hnsw_config = None
    optimizers_config = None
    if config == "hnsw":
        hnsw_config = models.HnswConfigDiff(m=16, ef_construct=100)
        # 既定の indexing_threshold（10,000 KB）はセグメント単位に適用され、25,000 行・
        # dim 128 では各セグメントが閾値未満のまま HNSW が一度も構築されない
        # （実機確認: status=green のまま indexed_vectors_count=0）。hnsw 構成では閾値を
        # 下げて全セグメントを索引化させ、_wait_indexed で完了を確認してから計測する。
        optimizers_config = models.OptimizersConfigDiff(indexing_threshold=1)
    else:
        # exact 構成は「索引なし全件探索」が比較条件（README）。検索側の `exact=True` は
        # 索引構築そのものを止めないため、既定のままだとセグメントが閾値を超えた時点で
        # HNSW のバックグラウンド構築が走り、その負荷が検索計測へ混入する。公式手順
        # （optimizer ドキュメント）どおり indexing_threshold=0 で自動構築を無効化する
        # （codex-review P2 指摘）。
        optimizers_config = models.OptimizersConfigDiff(indexing_threshold=0)
    client.create_collection(
        collection_name=COLLECTION,
        vectors_config=models.VectorParams(size=dim, distance=models.Distance.DOT),
        hnsw_config=hnsw_config,
        optimizers_config=optimizers_config,
    )
    client.create_payload_index(COLLECTION, "visibility", field_schema=models.PayloadSchemaType.KEYWORD)
    client.create_payload_index(COLLECTION, "lang", field_schema=models.PayloadSchemaType.KEYWORD)


def _wait_indexed(client: QdrantClient, rows: int, timeout_s: float = 300.0) -> dict:
    """HNSW 構成では collection status が green かつ indexed_vectors_count >= rows に
    なるまで待つ。未索引セグメントの全走査や構築中の負荷を hnsw の性能として記録しない
    ため、タイムアウト時は計測を拒否する（fail-closed）。"""
    t0 = time.perf_counter()
    while True:
        info = client.get_collection(COLLECTION)
        indexed = info.indexed_vectors_count or 0
        if str(info.status) == "green" and indexed >= rows:
            return {
                "seconds": time.perf_counter() - t0,
                "final_status": str(info.status),
                "indexed_vectors_count": indexed,
            }
        if time.perf_counter() - t0 > timeout_s:
            raise TimeoutError(
                f"qdrant index build did not complete within {timeout_s:.0f}s "
                f"(status={info.status}, indexed={indexed}/{rows})"
            )
        time.sleep(0.5)


def _ingest_bulk(client: QdrantClient, docs: list[dict]) -> dict:
    t0 = time.perf_counter()
    batch = 500
    for i in range(0, len(docs), batch):
        chunk = docs[i : i + batch]
        client.upsert(
            collection_name=COLLECTION,
            points=models.Batch(
                ids=[d["id"] for d in chunk],
                vectors=[d["embedding"] for d in chunk],
                payloads=[
                    {
                        # where_compound_count の `id > 100` 範囲条件は payload の
                        # `id` を参照する（Qdrant の point id には Range 条件を
                        # 適用できないため）。欠落すると条件が 1 件も一致せず
                        # COUNT が 0 になり他 DB と比較不能になる。
                        "id": d["id"],
                        "tenant": d["tenant"],
                        # 旧フィクスチャ（visibility 未導入）との互換のため
                        # 欠落時は fail-closed で private 扱いにする。
                        "visibility": doc_visibility(d),
                        "lang": d["lang"],
                        "topic": d.get("topic", ""),
                        # 広域取得（bulk_* フェーズ）が id と本文を返すために保持する。
                        # 他 DB は当初から body 列を投入しており、投入量の公平性は
                        # むしろ揃う方向（結果 JSON の note にも記録する）。
                        "body": d["body"],
                    }
                    for d in chunk
                ],
            ),
        )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": len(docs),
        "seconds": elapsed,
        "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None,
        "note": "payload に body を追加（bulk_* フェーズの本文返却用。他 DB の投入内容と同等化）",
    }


def _ingest_single_stmt(client: QdrantClient, dim: int, n_rows: int = 1000) -> dict:
    import random

    t0 = time.perf_counter()
    for n in range(n_rows):
        rid = 10_000_000 + n
        emb = [random.random() * 2 - 1 for _ in range(dim)]
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        client.upsert(
            collection_name=COLLECTION,
            points=[
                models.PointStruct(
                    id=rid,
                    vector=emb,
                    payload={
                        "tenant": TENANT_VISIBLE,
                        "visibility": "private",
                        "lang": lang,
                        "topic": topic,
                        "body": f"crossdb bench ingest row {n}",
                    },
                )
            ],
        )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": n_rows,
        "seconds": elapsed,
        "rows_per_sec": n_rows / elapsed if elapsed > 0 else None,
        "note": "payload に body を追加（bulk_* フェーズの本文返却用。他 DB の投入内容と同等化）",
    }


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    client = _connect()
    dim = len(docs[0]["embedding"]) if docs else 128
    _setup_collection(client, dim, args.config)
    phases: dict = {}
    phases["ingest_bulk"] = _ingest_bulk(client, docs)
    if args.config == "hnsw":
        # 投入直後は HNSW 構築が非同期に進行中のため、完了を確認してから計測へ進む。
        phases["index_build"] = _wait_indexed(client, len(docs))

    public_only_filter = models.Filter(
        must=[models.FieldCondition(key="visibility", match=models.MatchValue(value="public"))]
    )
    public_only_lang_ja_filter = models.Filter(
        must=[
            models.FieldCondition(key="visibility", match=models.MatchValue(value="public")),
            models.FieldCondition(key="lang", match=models.MatchValue(value="ja")),
        ]
    )
    search_params = None
    if args.config == "hnsw":
        search_params = models.SearchParams(hnsw_ef=64, exact=False)
    else:
        search_params = models.SearchParams(exact=True)

    def knn(qv):
        res = client.query_points(
            collection_name=COLLECTION,
            query=qv,
            query_filter=public_only_filter,
            search_params=search_params,
            limit=10,
            with_payload=False,
        )
        return [p.id for p in res.points]

    query_vecs = [q["embedding"] for q in queries]
    stats, _ = measure(knn, query_vecs)
    knn_ids_all = [knn(qv) for qv in query_vecs]
    phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

    def knn_where(qv):
        res = client.query_points(
            collection_name=COLLECTION,
            query=qv,
            query_filter=public_only_lang_ja_filter,
            search_params=search_params,
            limit=10,
            with_payload=False,
        )
        return [p.id for p in res.points]

    stats, _ = measure(knn_where, query_vecs)
    phases["vector_knn_where"] = stats
    phases["point_where"] = {"note": "vector_knn_where と同一クエリ形のため統合", **stats}

    def compound_count(_):
        f = models.Filter(
            must=[
                models.FieldCondition(key="visibility", match=models.MatchValue(value="public")),
                models.FieldCondition(key="lang", match=models.MatchValue(value="ja")),
                models.FieldCondition(key="id", range=models.Range(gt=100)),
            ]
        )
        return client.count(COLLECTION, count_filter=f, exact=True).count

    stats, last = measure(compound_count, [None])
    phases["where_compound_count"] = {**stats, "result": last}

    def agg_count(_):
        return client.count(COLLECTION, count_filter=public_only_filter, exact=True).count

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    phases["agg_multi"] = unsupported("Qdrant に SUM/AVG/MIN/MAX 相当の集計 API が無い（count のみ）")
    phases["group_by_having"] = unsupported("Qdrant に GROUP BY/HAVING 相当の API が無い")
    phases["hybrid_rrf"] = unsupported(
        "疎ベクトルを自前生成すると自作 DB の BM25 系 hybrid と非等価なため対象外（task 指示）"
    )
    phases["mode_recall"] = unsupported("Qdrant にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("Qdrant にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported("自作 DB の宣言的 UDF 呼び出し相当の機能が無い")

    # --- 広域取得（bulk fetch）: id と body を Top-N でまとめて返す ---
    # LLM のコンテキストへ丸ごと渡す想定のため本文（payload の body）の送出コストを
    # 含める。hnsw 構成では hnsw_ef（64）が limit を下回ると返却が目減りし得るため
    # bulk フェーズでは ef を max(64, k) へ引き上げ、結果に記録する。
    def bulk_search_params(k: int):
        if args.config == "hnsw":
            return models.SearchParams(hnsw_ef=max(64, k), exact=False), max(64, k)
        return models.SearchParams(exact=True), None

    def bulk_knn(k: int, flt):
        params, _ = bulk_search_params(k)

        def _run(qv):
            res = client.query_points(
                collection_name=COLLECTION,
                query=qv,
                query_filter=flt,
                search_params=params,
                limit=k,
                with_payload=["id", "body"],
            )
            return [(p.id, p.payload.get("body")) for p in res.points]

        return _run

    for k in (200, 1000):
        stats, last = measure(bulk_knn(k, public_only_filter), query_vecs)
        phases[f"bulk_knn_k{k}"] = {
            **stats,
            "k": k,
            "rows_returned": len(last),
            "hnsw_ef": bulk_search_params(k)[1],
        }

    stats, last = measure(bulk_knn(200, public_only_lang_ja_filter), query_vecs)
    phases["bulk_knn_where_k200"] = {
        **stats,
        "k": 200,
        "rows_returned": len(last),
        "hnsw_ef": bulk_search_params(200)[1],
    }

    phases["bulk_hybrid_k200"] = unsupported(
        "疎ベクトルを自前生成すると自作 DB の BM25 系 hybrid と非等価なため対象外（task 指示）"
    )

    # ORDER BY なしの WHERE スキャン相当は scroll API（ベクトル非返却・payload のみ）。
    def scan_nosort(_):
        points, _next = client.scroll(
            collection_name=COLLECTION,
            scroll_filter=public_only_lang_ja_filter,
            limit=500,
            with_payload=["id", "body"],
            with_vectors=False,
        )
        return [(p.id, p.payload.get("body")) for p in points]

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

    # Qdrant には自作 DB の wire セッションに相当するテナント別接続の概念が
    # 無いため、tenant-b「セッション」を模す接続は作らず、同じ public-only
    # フィルタを再実行して agg_count と同値になることの一致確認とする
    # （自作 DB は tenant-a・tenant-b いずれのセッションでも `visibility =
    # 'public'` の行のみ可視という現行契約——実機確認済み）。
    def rls_count(_):
        return client.count(COLLECTION, count_filter=public_only_filter, exact=True).count

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {
        **stats,
        "tenant_b_count": last,
        "note": "Qdrant にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    phases["explain"] = unsupported("Qdrant に EXPLAIN 相当の API が無い")

    info = client.get_collection(COLLECTION)
    # ingest_single_stmt はテーブルへ合成行（乱数ベクトル）を追加するため、
    # 他フェーズ（特に vector_knn の recall 検算）を汚染しないよう最後に実行する
    # （self_db.py と同じ順序方針）。
    phases["ingest_single_stmt"] = _ingest_single_stmt(client, dim)

    # 実行した Qdrant の実バージョンをサーバー自身に問い合わせて記録する
    # （`client.info()` → `VersionInfo.version`。イメージタグ `latest` は時点で
    # 中身が変わるため固定文言では再現性が担保できない）。取得失敗は握りつぶさず
    # 伝播させ、バージョン不明の結果を書き出さない（fail-closed）。
    server_version = client.info().version
    meta = build_meta(
        db="qdrant",
        version=f"qdrant {server_version} (image qdrant/qdrant:latest)",
        connection="gRPC",
        config=args.config,
        rows=len(docs),
        dim=dim,
        extra={
            "distance": "DOT",
            "hnsw_config": {"m": 16, "ef_construct": 100} if args.config == "hnsw" else None,
            "search_params": "hnsw_ef=64" if args.config == "hnsw" else "exact=True",
            "points_count": info.points_count,
        },
    )
    return {"meta": meta, "phases": phases}
