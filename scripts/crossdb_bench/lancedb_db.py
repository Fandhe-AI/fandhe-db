"""LanceDB 0.38.0（in-process）の機能別ベンチマーク実装。

距離指標は自作 DB の `<=>`（内積）に合わせ `metric="dot"` を使う。
`--config hnsw` は `index_type="IVF_HNSW_FLAT"`（量子化なし・m=16・
ef_construction=100）、`exact` は索引を作らず brute-force。

RLS 相当は、自作 DB の現行契約（実機確認: どのテナントの wire セッションから
も `visibility = 'public'` の行のみが可視。private 行は所有テナント自身から
も不可視）に合わせ、`visibility` 列への `where("visibility = 'public'")` で
模する（`tenant` 列では絞り込まない）。

集計・GROUP BY はテーブルレベルの SQL を持たないため、`.search().where(...)
.to_arrow()` でテナント可視行を取得し Python 側で集約する（task 指示の
「to_pandas/DuckDB 相当があれば試し」に対応。venv に pandas が無いため
pyarrow の `to_arrow()` を使う）。ネイティブ hybrid（FTS + ベクトル、既定
reranker は RRF）を使う。

ストレージは self_db.py・sqlite_vec_db.py と同様に `--workdir` 配下の実行ごと
一意な一時ディレクトリへ隔離し、終了時（正常・異常いずれも）に削除する
（固定パス `<workdir>/lancedb` だと再実行時に既存 `docs` テーブルを無条件削除
してしまうため。codex-review P1）。
"""

from __future__ import annotations

import os
import shutil
import tempfile
import time

import lancedb

from common import doc_visibility, TENANT_VISIBLE, build_meta, measure, sql_escape_literal, unsupported

TABLE = "docs"


def _is_unsupported_feature_error(e: Exception) -> bool:
    """LanceDB（pyo3 経由の lance-core）が機能未対応を示す例外か判定する。
    `NotImplementedError`、またはメッセージに "not supported"／"not
    implemented"／"unsupported" を含む場合のみ未対応とみなす。接続断・
    タイムアウト・ストレージ障害等の実行障害まで unsupported へ丸めて
    成功扱いにしない（codex-review P1）ため、判別できない例外は呼び出し元で
    再送出する。"""
    if isinstance(e, NotImplementedError):
        return True
    msg = str(e).lower()
    return "not supported" in msg or "not implemented" in msg or "unsupported" in msg


def _connect(storage_dir: str) -> lancedb.DBConnection:
    return lancedb.connect(storage_dir)


def _setup_table(db: lancedb.DBConnection, docs: list[dict]):
    if TABLE in db.table_names():
        db.drop_table(TABLE)
    data = [
        {
            "id": d["id"],
            "vector": d["embedding"],
            "tenant": d["tenant"],
            # 旧フィクスチャ（visibility 未導入）との互換のため欠落時は
            # fail-closed で private 扱いにする。
            "visibility": doc_visibility(d),
            "lang": d["lang"],
            "topic": d.get("topic", ""),
            "body": d["body"],
        }
        for d in docs
    ]
    tbl = db.create_table(TABLE, data=data)
    return tbl


def _ingest_bulk(db: lancedb.DBConnection, docs: list[dict]) -> tuple[object, dict]:
    t0 = time.perf_counter()
    tbl = _setup_table(db, docs)
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return tbl, {
        "rows": len(docs),
        "seconds": elapsed,
        "rows_per_sec": len(docs) / elapsed if elapsed > 0 else None,
    }


def _ingest_single_stmt(tbl, dim: int, n_rows: int = 1000) -> dict:
    import random

    t0 = time.perf_counter()
    for n in range(n_rows):
        rid = 10_000_000 + n
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        body = f"crossdb bench ingest row {n}"
        emb = [random.random() * 2 - 1 for _ in range(dim)]
        tbl.add(
            [
                {
                    "id": rid,
                    "vector": emb,
                    "tenant": TENANT_VISIBLE,
                    "visibility": "private",
                    "lang": lang,
                    "topic": topic,
                    "body": body,
                }
            ]
        )
    t1 = time.perf_counter()
    elapsed = t1 - t0
    return {"rows": n_rows, "seconds": elapsed, "rows_per_sec": n_rows / elapsed if elapsed > 0 else None}


def run(args, docs: list[dict], queries: list[dict]) -> dict:
    # 他 DB（self・sqlite_vec）と同じく、`--workdir` 配下の実行ごとに一意な
    # 作業ディレクトリへ隔離する（codex-review P1）。固定パス `<workdir>/lancedb`
    # だと再実行時に既存の `docs` テーブルを無条件削除してしまうため。
    # 終了時（正常・異常いずれも）に自分が作成した領域だけを削除する。
    workdir = getattr(args, "workdir", None) or tempfile.gettempdir()
    os.makedirs(workdir, exist_ok=True)
    storage_dir = tempfile.mkdtemp(prefix=f"lancedb_bench_{os.getpid()}_", dir=workdir)
    try:
        return _run_with_storage(storage_dir, args, docs, queries)
    finally:
        shutil.rmtree(storage_dir, ignore_errors=True)


def _run_with_storage(storage_dir: str, args, docs: list[dict], queries: list[dict]) -> dict:
    db = _connect(storage_dir)
    dim = len(docs[0]["embedding"]) if docs else 128
    phases: dict = {}
    tbl, ingest_stat = _ingest_bulk(db, docs)
    phases["ingest_bulk"] = ingest_stat

    if args.config == "hnsw":
        tbl.create_index(
            metric="dot",
            vector_column_name="vector",
            index_type="IVF_HNSW_FLAT",
            m=16,
            ef_construction=100,
            replace=True,
        )
    fts_error: str | None = None
    try:
        # `create_fts_index` は 0.25 以降非推奨（`create_index(config=FTS())` が推奨）だが
        # 0.38.0 でも動作する（実機確認）。hybrid フェーズが使う FTS 索引をここで作る。
        # 失敗は握りつぶさず hybrid フェーズを理由付き unsupported にする。
        tbl.create_fts_index("body", replace=True)
    except Exception as e:  # noqa: BLE001 - 機能未対応のみ捕捉し、それ以外は再送出する
        if not _is_unsupported_feature_error(e):
            raise
        fts_error = repr(e)

    public_filter = "visibility = 'public'"
    public_lang_filter = "visibility = 'public' AND lang = 'ja'"

    def knn(qv):
        q = tbl.search(qv, vector_column_name="vector").metric("dot").where(public_filter).limit(10)
        if args.config == "hnsw":
            q = q.ef(64)
        return [r["id"] for r in q.select(["id"]).to_list()]

    query_vecs = [q["embedding"] for q in queries]
    stats, _ = measure(knn, query_vecs)
    knn_ids_all = [knn(qv) for qv in query_vecs]
    phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

    def knn_where(qv):
        q = tbl.search(qv, vector_column_name="vector").metric("dot").where(public_lang_filter).limit(10)
        if args.config == "hnsw":
            q = q.ef(64)
        return [r["id"] for r in q.select(["id"]).to_list()]

    stats, _ = measure(knn_where, query_vecs)
    phases["vector_knn_where"] = stats
    phases["point_where"] = {"note": "vector_knn_where と同一クエリ形のため統合", **stats}

    def compound_count(_):
        f = "visibility = 'public' AND id > 100 AND lang = 'ja'"
        return tbl.count_rows(f)

    stats, last = measure(compound_count, [None])
    phases["where_compound_count"] = {**stats, "result": last}

    def agg_count(_):
        return tbl.count_rows(public_filter)

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    # 集計系の走査は全件を対象にする。`limit(1_000_000)` のような固定上限は入力が
    # それを超えると黙って部分集計になる（codex-review P2）ため、`limit(None)`
    # （上限なし）を使う。venv の lancedb 0.38.0 で `QueryBuilder.limit(None)` が
    # 受理され、既定上限（10 件）を超える全行が返ることを実機確認済み。
    # よって「agg_count の実測件数が上限超なら ValueError」の代替案は不要。
    def scan_ids(filter_str: str) -> list[int]:
        rows = tbl.search().where(filter_str).select(["id"]).limit(None).to_arrow().to_pylist()
        return [r["id"] for r in rows]

    def agg_multi(_):
        ids = scan_ids(public_filter)
        if not ids:
            # 可視集合が空のときの SUM/AVG/MIN/MAX は PostgreSQL/SQLite/MySQL の
            # `SUM(id)` 等と同じく NULL（COUNT のみ 0）に揃える（codex-review P2）。
            return (0, None, None, None, None)
        return (len(ids), sum(ids), sum(ids) / len(ids), min(ids), max(ids))

    stats, last = measure(agg_multi, [None])
    phases["agg_multi"] = {**stats, "result": list(last) if last else None}

    def group_by(_):
        rows = (
            tbl.search()
            .where(public_filter)
            .select(["lang"])
            # 全件走査（上記 scan_ids と同じ理由で上限なし）。
            .limit(None)
            .to_arrow()
            .to_pylist()
        )
        counts: dict[str, int] = {}
        for r in rows:
            counts[r["lang"]] = counts.get(r["lang"], 0) + 1
        filtered = [(lang, n) for lang, n in counts.items() if n > 1]
        filtered.sort(key=lambda kv: -kv[1])
        return filtered[:5]

    stats, last = measure(group_by, [None])
    phases["group_by_having"] = {**stats, "result": last}

    def hybrid(i):
        qv = query_vecs[i]
        qt = sql_escape_literal(queries[i].get("text", ""))
        # 例外は伝播させ、呼び出し側でフェーズ全体を unsupported として記録する
        # （空配列に変換すると失敗が正常なレイテンシとして残るため）。
        q = (
            tbl.search(query_type="hybrid")
            .vector(qv)
            .text(qt)
            # 通常 KNN と同じく内積に統一する（未指定だと exact 構成では既定の L2 になる）。
            .metric("dot")
            .where(public_filter)
        )
        # 通常 KNN（`knn`／`knn_where`）と同じ探索幅 ef=64 に揃える（meta.index の記録値と一致させる）。
        if args.config == "hnsw":
            q = q.ef(64)
        rows = q.limit(10).select(["id"]).to_list()
        return [r["id"] for r in rows]

    idxs = list(range(len(query_vecs)))
    if fts_error is not None:
        phases["hybrid_rrf"] = unsupported(f"FTS index creation failed: {fts_error}")
    else:
        try:
            stats, last = measure(hybrid, idxs)
            phases["hybrid_rrf"] = stats
        except Exception as e:  # noqa: BLE001 - 機能未対応のみ捕捉し、それ以外は再送出する
            if not _is_unsupported_feature_error(e):
                raise
            phases["hybrid_rrf"] = unsupported(f"hybrid search failed: {e!r}")

    # --- 広域取得（bulk fetch）: id と body を Top-N でまとめて返す ---
    # LLM のコンテキストへ丸ごと渡す想定のため本文（body）の送出コストを含める。
    # hnsw 構成では ef（64）が limit を下回ると返却が目減りし得るため bulk フェーズ
    # では ef を max(64, k) へ引き上げ、結果に記録する。
    def bulk_ef(k: int):
        return max(64, k) if args.config == "hnsw" else None

    def bulk_knn(k: int, filter_str: str):
        def _run(qv):
            q = tbl.search(qv, vector_column_name="vector").metric("dot").where(filter_str).limit(k)
            if args.config == "hnsw":
                q = q.ef(bulk_ef(k))
            return [(r["id"], r["body"]) for r in q.select(["id", "body"]).to_list()]

        return _run

    for k in (200, 1000):
        stats, last = measure(bulk_knn(k, public_filter), query_vecs)
        phases[f"bulk_knn_k{k}"] = {**stats, "k": k, "rows_returned": len(last), "ef": bulk_ef(k)}

    stats, last = measure(bulk_knn(200, public_lang_filter), query_vecs)
    phases["bulk_knn_where_k200"] = {**stats, "k": 200, "rows_returned": len(last), "ef": bulk_ef(200)}

    def bulk_hybrid(i):
        qv = query_vecs[i]
        qt = sql_escape_literal(queries[i].get("text", ""))
        q = (
            tbl.search(query_type="hybrid")
            .vector(qv)
            .text(qt)
            .metric("dot")
            .where(public_filter)
            .limit(200)
        )
        if args.config == "hnsw":
            # bulk_knn と同じく密側の候補幅を k 以上へ引き上げる
            q = q.ef(bulk_ef(200))
        rows = q.select(["id", "body"]).to_list()
        return [(r["id"], r["body"]) for r in rows]

    if fts_error is not None:
        phases["bulk_hybrid_k200"] = unsupported(f"FTS index creation failed: {fts_error}")
    else:
        try:
            stats, last = measure(bulk_hybrid, idxs)
            phases["bulk_hybrid_k200"] = {**stats, "k": 200, "rows_returned": len(last), "ef": bulk_ef(200)}
        except Exception as e:  # noqa: BLE001 - 機能未対応のみ捕捉し、それ以外は再送出する
            if not _is_unsupported_feature_error(e):
                raise
            phases["bulk_hybrid_k200"] = unsupported(f"hybrid search failed: {e!r}")

    # ベクトルなしの where スキャン（ORDER BY なし）。
    def scan_nosort(_):
        rows = tbl.search().where(public_lang_filter).limit(500).select(["id", "body"]).to_list()
        return [(r["id"], r["body"]) for r in rows]

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last)}

    phases["mode_recall"] = unsupported("LanceDB にモード切替（recall/precision）の概念が無い")
    phases["mode_precision"] = unsupported("LanceDB にモード切替（recall/precision）の概念が無い")
    phases["udf_call"] = unsupported("自作 DB の宣言的 UDF 呼び出し相当の機能が無い")

    # LanceDB には自作 DB の wire セッションに相当するテナント別接続の概念が
    # 無いため、tenant-b「セッション」を模す接続は作らず、同じ public-only
    # フィルタを再実行して agg_count と同値になることの一致確認とする
    # （自作 DB は tenant-a・tenant-b いずれのセッションでも `visibility =
    # 'public'` の行のみ可視という現行契約——実機確認済み）。
    def rls_count(_):
        return tbl.count_rows(public_filter)

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {
        **stats,
        "tenant_b_count": last,
        "note": "LanceDB にテナント別セッションの概念が無いため agg_count と同一フィルタで再実行（値の一致確認用）",
    }

    def explain(qv):
        q = tbl.search(qv, vector_column_name="vector").metric("dot").where(public_filter).limit(10)
        return q.explain_plan(True)

    try:
        stats, last = measure(explain, query_vecs)
        phases["explain"] = {**stats, "sample_output": last}
    except Exception as e:  # noqa: BLE001 - 機能未対応のみ捕捉し、それ以外は再送出する
        if not _is_unsupported_feature_error(e):
            raise
        phases["explain"] = unsupported(f"explain_plan failed: {e!r}")

    # ingest_single_stmt はテーブルへ合成行（乱数ベクトル）を追加するため、
    # 他フェーズ（特に vector_knn の recall 検算）を汚染しないよう最後に実行する
    # （self_db.py と同じ順序方針）。
    phases["ingest_single_stmt"] = _ingest_single_stmt(tbl, dim)

    meta = build_meta(
        db="lancedb",
        version="lancedb 0.38.0",
        connection="in-process",
        config=args.config,
        rows=len(docs),
        dim=dim,
        extra={
            "index": "IVF_HNSW_FLAT(m=16,ef_construction=100,ef=64)"
            if args.config == "hnsw"
            else "exact (no vector index)",
            "storage": "per-run temp dir under --workdir (removed on exit)",
        },
    )
    return {"meta": meta, "phases": phases}
