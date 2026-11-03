"""Elasticsearch（`docker.elastic.co/elasticsearch/elasticsearch:9.1.4`、
127.0.0.1:39200 既定。環境変数 `CROSSDB_ES_PORT` で上書き可）の機能別
ベンチマーク実装。

依存追加なし: Python 標準ライブラリの `http.client`（keep-alive の 1 TCP
接続を使い回す）・`json` のみで REST API を叩く。

自作 DB の `<=>` が内積（`crates/engine/src/kernel.rs` 参照。値が大きいほど
上位）であるため、`dense_vector` フィールドは `similarity: dot_product`
（フィクスチャの embedding は単位ノルムのため要件を満たす）で作る。RLS 相当は、
自作 DB の現行契約（実機確認: どのテナントの wire セッションからも
`visibility = 'public'` の行のみが可視。private 行は所有テナント自身からも
不可視）に合わせ、`visibility` フィールドへの `term` フィルタで模する
（`tenant` では絞り込まない。`common.public_only_where` の SQL 版と同じ方針）。

索引構成は `--config exact|hnsw` で切り替える:
- exact: `embedding` を `index: false` にし、`script_score` クエリ
  （`dotProduct(params.query_vector, 'embedding')`）で全件走査する
  （ES の dense_vector は `index: false` でも script_score からは参照できる。
  公式ドキュメント「Exact kNN using script score」のパターン）。
- hnsw: `index: true, index_options: {type: hnsw, m: 16, ef_construction: 100}`
  にし、`knn` クエリ（`num_candidates = max(64, k)`）で近似探索する。

集計（`agg_count`・`agg_multi`・`group_by_having`）は `_count`／`_search`
の `aggregations` を使う。`hybrid_rrf` は ES 8.16+ の `retriever.rrf`
（テキスト側 `standard` + ベクトル側 `knn`／`script_score`）をネイティブ機能
として試み、クラスタがサポートしない・ライセンス不足等で失敗した場合は
実際のエラー文を添えて `unsupported` にする（クライアント側 RRF 実装は禁止）。
"""

from __future__ import annotations

import http.client
import json
import random
import time
from typing import Any

from common import DIM, build_meta, doc_visibility, env_port, measure, unsupported

HOST = "127.0.0.1"
# 既定値 39200 は containers.sh 側の `${CROSSDB_ES_PORT:-39200}` と一致させること。
PORT = env_port("CROSSDB_ES_PORT", 39200)
INDEX = "docs"
_REQUEST_TIMEOUT_S = 60.0


class EsHttpError(RuntimeError):
    """Elasticsearch が非 2xx を返したときの例外（fail-closed。
    握りつぶして unsupported へ丸めず計測全体を失敗させる。hybrid_rrf の
    ネイティブ機能可否判定のみ、呼び出し元がこの例外を捕まえて実エラー文
    付きの unsupported へ変換する）。"""

    def __init__(self, status: int, body: Any):
        super().__init__(f"HTTP {status}: {json.dumps(body, ensure_ascii=False)[:2000]}")
        self.status = status
        self.body = body


class EsClient:
    """1 TCP 接続を使い回す最小限の REST クライアント（依存追加なし）。

    接続が切れた場合（アイドルタイムアウト等）は 1 回だけ再接続して
    リトライする。keep-alive を前提にしているのは Elasticsearch が
    HTTP/1.1 既定の persistent connection をサポートするため（NoSQL 表層の
    `self_nosql.py` とは対照的に、こちらは 1 要求ごとの再接続を避けて
    計測ノイズ〔TCP ハンドシェイク〕を減らす）。
    """

    def __init__(self, host: str, port: int) -> None:
        self._host = host
        self._port = port
        self._conn: http.client.HTTPConnection | None = None

    def _ensure_conn(self) -> http.client.HTTPConnection:
        if self._conn is None:
            self._conn = http.client.HTTPConnection(self._host, self._port, timeout=_REQUEST_TIMEOUT_S)
        return self._conn

    def request(self, method: str, path: str, body: bytes | None = None, headers: dict | None = None) -> tuple[int, bytes]:
        hdrs = dict(headers or {})
        for attempt in range(2):
            conn = self._ensure_conn()
            try:
                conn.request(method, path, body=body, headers=hdrs)
                resp = conn.getresponse()
                raw = resp.read()
                return resp.status, raw
            except (http.client.HTTPException, OSError):
                # keep-alive 接続が相手側でクローズされていた場合に 1 回だけ
                # 再接続する（untrusted なネットワーク状態からの復帰。
                # 2 回目も失敗したら呼び出し元へ伝播させる＝fail-closed）。
                self._conn = None
                if attempt == 1:
                    raise
        raise AssertionError("unreachable")

    def json_request(self, method: str, path: str, payload: dict | None = None) -> dict | None:
        body = json.dumps(payload).encode("utf-8") if payload is not None else None
        headers = {"Content-Type": "application/json"} if body is not None else {}
        status, raw = self.request(method, path, body=body, headers=headers)
        data = json.loads(raw.decode("utf-8")) if raw else None
        if status >= 300:
            raise EsHttpError(status, data)
        return data

    def bulk(self, ndjson_body: bytes) -> dict:
        status, raw = self.request(
            "POST", f"/{INDEX}/_bulk", body=ndjson_body, headers={"Content-Type": "application/x-ndjson"}
        )
        data = json.loads(raw.decode("utf-8"))
        if status >= 300:
            raise EsHttpError(status, data)
        return data

    def close(self) -> None:
        if self._conn is not None:
            self._conn.close()
            self._conn = None


def _wait_ready(client: EsClient, timeout_s: float = 60.0) -> None:
    """`GET /_cluster/health?wait_for_status=yellow&timeout=1s` が 200 を
    返すまで待つ（single-node クラスタなので replica 0 なら green まで
    待てるが、既定 replica=1 のままだと green にならないため yellow で
    十分とする）。"""
    deadline = time.time() + timeout_s
    last_err: Exception | None = None
    while time.time() < deadline:
        try:
            status, _raw = client.request("GET", "/_cluster/health?wait_for_status=yellow&timeout=1s")
            if status == 200:
                return
        except (http.client.HTTPException, OSError) as e:
            last_err = e
        time.sleep(0.5)
    raise TimeoutError(f"elasticsearch did not become ready within {timeout_s:.0f}s (last_err={last_err!r})")


def _setup_index(client: EsClient, dim: int, config: str) -> None:
    # 既存インデックスは冪等に削除してから作り直す（404 は無視）。
    status, _raw = client.request("DELETE", f"/{INDEX}")
    hnsw = config == "hnsw"
    # `similarity`／`index_options` は `index: true` のときしか指定できない
    # （ES 9.x の制約。実機確認: `index: false` へ付けると 400
    # `mapper_parsing_exception`）。exact 構成は index なしの生ベクトル
    # フィールドとし、script_score の `dotProduct` painless 関数で直接
    # 参照する（similarity 設定に依存しない）。
    embedding_field: dict = {"type": "dense_vector", "dims": dim, "index": hnsw}
    if hnsw:
        embedding_field["similarity"] = "dot_product"
        embedding_field["index_options"] = {"type": "hnsw", "m": 16, "ef_construction": 100}
    mapping = {
        "settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {
            "properties": {
                "id": {"type": "integer"},
                "tenant": {"type": "keyword"},
                "visibility": {"type": "keyword"},
                "lang": {"type": "keyword"},
                "topic": {"type": "keyword"},
                "body": {"type": "text", "analyzer": "english"},
                "embedding": embedding_field,
            }
        },
    }
    client.json_request("PUT", f"/{INDEX}", mapping)


def _ingest_bulk(client: EsClient, docs: list[dict]) -> dict:
    """`_bulk` で数千行ずつ投入し、末尾に `_refresh` を呼んで検索可視化する
    （ES の既定リフレッシュ間隔 1s を待たず、他 DB の同期的な投入完了と
    公平に比較するため。所要時間は refresh 込みで計測する）。"""
    t0 = time.perf_counter()
    chunk = 5000
    for i in range(0, len(docs), chunk):
        batch = docs[i : i + chunk]
        lines: list[bytes] = []
        for d in batch:
            action = {"index": {"_id": str(d["id"])}}
            source = {
                "id": d["id"],
                "tenant": d["tenant"],
                # 旧フィクスチャ（visibility 未導入）との互換のため
                # 欠落時は fail-closed で private 扱いにする。
                "visibility": doc_visibility(d),
                "lang": d["lang"],
                "topic": d.get("topic", ""),
                "body": d["body"],
                "embedding": d["embedding"],
            }
            lines.append(json.dumps(action).encode("utf-8"))
            lines.append(json.dumps(source).encode("utf-8"))
        ndjson = b"\n".join(lines) + b"\n"
        resp = client.bulk(ndjson)
        if resp.get("errors"):
            first_err = next(
                (item["index"]["error"] for item in resp.get("items", []) if "error" in item.get("index", {})),
                None,
            )
            raise RuntimeError(f"elasticsearch _bulk reported errors: {first_err}")
    client.json_request("POST", f"/{INDEX}/_refresh", None)
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": len(docs),
        "seconds": elapsed,
        "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None,
        "note": "_bulk（5,000 行/chunk）+ 末尾 _refresh を含む所要時間",
    }


def _index_build(client: EsClient) -> dict:
    """hnsw 構成のみ: `_forcemerge?max_num_segments=1` で単一セグメントへ
    統合し、HNSW グラフ構築を確定させる所要時間を計測する（`dense_vector`
    の HNSW グラフは投入時にセグメント単位で逐次構築されるが、DB 間の
    比較条件を揃えるため forcemerge で確定させたタイミングを index_build
    として記録する。`wait_for_completion` は既定 true のため同期的に待つ）。"""
    t0 = time.perf_counter()
    client.json_request("POST", f"/{INDEX}/_forcemerge?max_num_segments=1", None)
    t1 = time.perf_counter()
    return {"seconds": t1 - t0}


def _ingest_single_stmt(client: EsClient, dim: int, n_rows: int = 1000) -> dict:
    """`PUT /docs/_doc/<id>` を 1 行ずつ・refresh なしで実行する（既定の
    translog fsync のみ。ES の単発 index API はデフォルトで `refresh=false`
    ＝durable write ではあるが検索へは即時反映されない。他 DB の
    `ingest_single_stmt` と同じく可視化は要求しない）。"""
    t0 = time.perf_counter()
    for n in range(n_rows):
        rid = 10_000_000 + n
        raw = [random.random() * 2 - 1 for _ in range(dim)]
        # hnsw 構成の `similarity: dot_product` は単位ノルムのベクトルしか
        # 受理しない（実機確認: 400 `document_parsing_exception`）ため、
        # ここで正規化する（フィクスチャの embedding と同じ前提に揃える）。
        norm = sum(x * x for x in raw) ** 0.5
        emb = [x / norm for x in raw] if norm > 0 else raw
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        source = {
            "id": rid,
            "tenant": "tenant-a",
            "visibility": "private",
            "lang": lang,
            "topic": topic,
            "body": f"crossdb bench ingest row {n}",
            "embedding": emb,
        }
        client.json_request("PUT", f"/{INDEX}/_doc/{rid}", source)
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {
        "rows": n_rows,
        "seconds": elapsed,
        "rows_per_sec": n_rows / elapsed if elapsed > 0 else None,
        "note": "PUT /docs/_doc/<id> ×1,000・refresh なし（既定の translog fsync のみ）",
    }


def _public_filter(extra: list[dict] | None = None) -> list[dict]:
    f = [{"term": {"visibility": "public"}}]
    if extra:
        f.extend(extra)
    return f


def _vector_search_body(qv: list[float], k: int, filt: list[dict], config: str, num_candidates: int | None = None, source: Any = False) -> dict:
    """config（exact/hnsw）に応じて `script_score`／`knn` のいずれかで
    ベクトル検索本体を組み立てる（`vector_knn` 系フェーズ共通）。"""
    if config == "hnsw":
        body: dict = {
            "knn": {
                "field": "embedding",
                "query_vector": qv,
                "k": k,
                "num_candidates": num_candidates if num_candidates is not None else max(64, k),
                "filter": {"bool": {"filter": filt}},
            },
            "size": k,
            "_source": source,
        }
        return body
    return {
        "size": k,
        "_source": source,
        "query": {
            "script_score": {
                "query": {"bool": {"filter": filt}},
                "script": {
                    "source": "dotProduct(params.query_vector, 'embedding') + 1.0",
                    "params": {"query_vector": qv},
                },
            }
        },
    }


def _search_ids(client: EsClient, body: dict) -> list[int]:
    data = client.json_request("POST", f"/{INDEX}/_search", body)
    return [int(h["_id"]) for h in data["hits"]["hits"]]


def _search_id_body(client: EsClient, body: dict) -> list[tuple[int, str]]:
    data = client.json_request("POST", f"/{INDEX}/_search", body)
    return [(int(h["_id"]), h["_source"].get("body")) for h in data["hits"]["hits"]]


class EsFeatureUnavailable(RuntimeError):
    """ES ネイティブ機能（`retriever.rrf` 等）がライセンス・非対応で使えない場合の
    例外。呼び出し元（`run`）はこの例外**のみ**を unsupported へ変換し、それ以外の
    HTTP エラー（429／500／接続断）は fail-closed で計測全体を失敗させる。"""


# retriever.rrf がライセンス（無償 basic では 403 security_exception／
# license 系）または未対応（400 parsing_exception）で拒否されたときの判定。
_RRF_UNAVAILABLE_STATUS = {400, 403}
_RRF_UNAVAILABLE_TYPES = ("security_exception", "license", "parsing_exception", "unknown_retriever", "x_content_parse_exception")


def _classify_rrf_error(e: EsHttpError) -> EsFeatureUnavailable | None:
    """既知の「機能が使えない」応答だけを `EsFeatureUnavailable` へ写像する。"""
    if e.status not in _RRF_UNAVAILABLE_STATUS:
        return None
    text = str(e.body)
    if any(t in text for t in _RRF_UNAVAILABLE_TYPES):
        return EsFeatureUnavailable(f"retriever.rrf が利用できない（HTTP {e.status}: {text[:200]}）")
    return None


def _try_hybrid_rrf(
    client: EsClient,
    qv: list[float],
    qt: str,
    config: str,
    k: int,
    rank_window: int,
    *,
    with_body: bool = False,
) -> list[int] | list[tuple[int, str]]:
    """ES ネイティブの `retriever.rrf`（standard: match body ＋ knn／
    script_score のベクトル側）で RRF 融合する。`with_body=True` は広域取得契約
    （id＋body の Top-N）どおり `_source: ["body"]` で本文も取得して
    `(id, body)` を返す。機能が使えない既知の応答は `EsFeatureUnavailable` を送出し、
    それ以外の `EsHttpError` はそのまま伝播する（fail-closed）。"""
    vector_retriever: dict
    if config == "hnsw":
        vector_retriever = {
            "knn": {
                "field": "embedding",
                "query_vector": qv,
                "k": rank_window,
                "num_candidates": max(64, rank_window),
                "filter": {"bool": {"filter": _public_filter()}},
            }
        }
    else:
        vector_retriever = {
            "standard": {
                "query": {
                    "script_score": {
                        "query": {"bool": {"filter": _public_filter()}},
                        "script": {
                            "source": "dotProduct(params.query_vector, 'embedding') + 1.0",
                            "params": {"query_vector": qv},
                        },
                    }
                }
            }
        }
    text_retriever = {
        "standard": {
            "query": {
                "bool": {
                    "must": [{"match": {"body": qt}}],
                    "filter": _public_filter(),
                }
            }
        }
    }
    body = {
        "retriever": {
            "rrf": {
                "retrievers": [text_retriever, vector_retriever],
                "rank_window_size": rank_window,
                "rank_constant": 60,
            }
        },
        "size": k,
        "_source": ["body"] if with_body else False,
    }
    try:
        data = client.json_request("POST", f"/{INDEX}/_search", body)
    except EsHttpError as e:
        unavailable = _classify_rrf_error(e)
        if unavailable is not None:
            raise unavailable from e
        raise
    hits = data["hits"]["hits"]
    if with_body:
        return [(int(h["_id"]), h["_source"].get("body")) for h in hits]
    return [int(h["_id"]) for h in hits]


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    client = EsClient(HOST, PORT)
    _wait_ready(client)
    dim = len(docs[0]["embedding"]) if docs else DIM
    _setup_index(client, dim, args.config)
    phases: dict = {}
    phases["ingest_bulk"] = _ingest_bulk(client, docs)
    if args.config == "hnsw":
        phases["index_build"] = _index_build(client)

    query_vecs = [q["embedding"] for q in queries]
    query_texts = [q.get("text", "") for q in queries]

    def knn(qv):
        return _search_ids(client, _vector_search_body(qv, 10, _public_filter(), args.config))

    stats, _ = measure(knn, query_vecs)
    knn_ids_all = [knn(qv) for qv in query_vecs]
    phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

    def knn_where(qv):
        return _search_ids(client, _vector_search_body(qv, 10, _public_filter([{"term": {"lang": "ja"}}]), args.config))

    stats, _ = measure(knn_where, query_vecs)
    phases["vector_knn_where"] = stats
    phases["point_where"] = {"note": "vector_knn_where と同一クエリ形のため統合", **stats}

    def compound_count(_):
        body = {
            "query": {
                "bool": {
                    "filter": _public_filter([{"term": {"lang": "ja"}}, {"range": {"id": {"gt": 100}}}])
                }
            }
        }
        data = client.json_request("POST", f"/{INDEX}/_count", body)
        return data["count"]

    stats, last = measure(compound_count, [None])
    phases["where_compound_count"] = {**stats, "result": last}

    def agg_count(_):
        body = {"query": {"bool": {"filter": _public_filter()}}}
        data = client.json_request("POST", f"/{INDEX}/_count", body)
        return data["count"]

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    def agg_multi(_):
        body = {
            "size": 0,
            "query": {"bool": {"filter": _public_filter()}},
            "aggs": {
                "cnt": {"value_count": {"field": "id"}},
                "sum_id": {"sum": {"field": "id"}},
                "avg_id": {"avg": {"field": "id"}},
                "min_id": {"min": {"field": "id"}},
                "max_id": {"max": {"field": "id"}},
            },
        }
        data = client.json_request("POST", f"/{INDEX}/_search", body)
        a = data["aggregations"]
        return [a["cnt"]["value"], a["sum_id"]["value"], a["avg_id"]["value"], a["min_id"]["value"], a["max_id"]["value"]]

    stats, last = measure(agg_multi, [None])
    phases["agg_multi"] = {**stats, "result": last}

    def group_by(_):
        body = {
            "size": 0,
            "query": {"bool": {"filter": _public_filter()}},
            "aggs": {
                "by_lang": {
                    "terms": {"field": "lang", "min_doc_count": 2, "size": 5, "order": {"_count": "desc"}}
                }
            },
        }
        data = client.json_request("POST", f"/{INDEX}/_search", body)
        buckets = data["aggregations"]["by_lang"]["buckets"]
        return [[b["key"], b["doc_count"]] for b in buckets]

    stats, last = measure(group_by, [None])
    phases["group_by_having"] = {**stats, "result": last}

    idxs = list(range(len(query_vecs)))

    def hybrid(i):
        return _try_hybrid_rrf(client, query_vecs[i], query_texts[i], args.config, 10, 50)

    try:
        stats, _ = measure(hybrid, idxs)
        phases["hybrid_rrf"] = stats
    except EsFeatureUnavailable as e:
        # 既知の「機能が使えない」応答のみ unsupported へ。他の HTTP エラー・接続断は
        # fail-closed（そのまま伝播して計測全体を失敗させる）
        phases["hybrid_rrf"] = unsupported(str(e))

    phases["mode_recall"] = unsupported("Elasticsearch にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("Elasticsearch にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported("自作 DB の宣言的 UDF 呼び出し相当の機能が無い")

    # --- 広域取得（bulk fetch）: id と body を Top-N でまとめて返す ---
    def bulk_knn(k: int, filt: list[dict]):
        def _run(qv):
            return _search_id_body(
                client,
                _vector_search_body(qv, k, filt, args.config, num_candidates=max(64, k), source=["body"]),
            )

        return _run

    for k in (200, 1000):
        stats, last = measure(bulk_knn(k, _public_filter()), query_vecs)
        phases[f"bulk_knn_k{k}"] = {**stats, "k": k, "rows_returned": len(last)}

    stats, last = measure(bulk_knn(200, _public_filter([{"term": {"lang": "ja"}}])), query_vecs)
    phases["bulk_knn_where_k200"] = {**stats, "k": 200, "rows_returned": len(last)}

    def bulk_hybrid(i):
        # 広域取得契約（id＋body の Top-200）どおり本文も取得する
        return _try_hybrid_rrf(client, query_vecs[i], query_texts[i], args.config, 200, 200, with_body=True)

    try:
        stats, last = measure(bulk_hybrid, idxs)
        phases["bulk_hybrid_k200"] = {**stats, "k": 200, "rows_returned": len(last)}
    except EsFeatureUnavailable as e:
        phases["bulk_hybrid_k200"] = unsupported(str(e))

    def scan_nosort(_):
        body = {
            "size": 500,
            "_source": ["body"],
            "query": {"bool": {"filter": _public_filter([{"term": {"lang": "ja"}}])}},
        }
        return _search_id_body(client, body)

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

    # Elasticsearch にはテナント別セッションの概念が無いため、tenant-b
    # 「セッション」を模す接続は作らず、agg_count と同じ public-only フィルタを
    # 再実行して値の一致確認とする（他 DB モジュールと同じ方針）。
    def rls_count(_):
        body = {"query": {"bool": {"filter": _public_filter()}}}
        data = client.json_request("POST", f"/{INDEX}/_count", body)
        return data["count"]

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {
        **stats,
        "tenant_b_count": last,
        "note": "Elasticsearch にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    # explain: script_score クエリ（index 構成に依らず一貫した形にするため
    # exact 側と同じ script_score を使う）へ `explain: true` を付けて計測する。
    def explain(qv):
        body = _vector_search_body(qv, 10, _public_filter(), "exact", source=False)
        body["explain"] = True
        data = client.json_request("POST", f"/{INDEX}/_search", body)
        hits = data["hits"]["hits"]
        return [h.get("_explanation", {}).get("description") for h in hits[:3]]

    stats, last = measure(explain, query_vecs)
    phases["explain"] = {**stats, "sample_output": last}

    version_info = client.json_request("GET", "/")
    es_version = version_info.get("version", {}).get("number", "unknown")

    # ingest_single_stmt はインデックスへ合成行（乱数ベクトル）を追加するため、
    # 他フェーズ（特に vector_knn の recall 検算）を汚染しないよう最後に実行する
    # （他モジュールと同じ順序方針）。
    phases["ingest_single_stmt"] = _ingest_single_stmt(client, dim)

    count_data = client.json_request("POST", f"/{INDEX}/_count", {"query": {"match_all": {}}})
    meta = build_meta(
        db="elasticsearch",
        version=f"elasticsearch {es_version} (image docker.elastic.co/elasticsearch/elasticsearch:9.1.4)",
        connection="loopback HTTP (http.client, keep-alive)",
        config=args.config,
        rows=len(docs),
        dim=dim,
        extra={
            "similarity": "dot_product",
            "index_options": {"type": "hnsw", "m": 16, "ef_construction": 100} if args.config == "hnsw" else None,
            "doc_count": count_data.get("count"),
        },
    )
    client.close()
    return {"meta": meta, "phases": phases}
