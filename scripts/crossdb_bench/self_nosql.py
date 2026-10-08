"""自作ベクトル DB（wire-server 経由・**NoSQL 表層**）の機能別ベンチマーク実装。

`self_db.py` は PostgreSQL wire（psycopg・簡易クエリプロトコル）経由で SQL 表層を
計測するのに対し、本モジュールは同じ `wire-server` バイナリを `--surface nosql`
opt-in で起動し、HTTP/1.1 最小サブセット（`POST /v1/session`・`/v1/session/close`・
`/v1/query`）経由で計測する。API 契約の単一情報源は `crates/wire-server/docs/
nosql-api.md`（コード・テストのみが根拠。spec 本文は転記しない）。

**keep-alive 非対応**: NoSQL 表層は「応答は常に... `Connection: close` を付け、
1 要求ごとに接続を閉じる」契約（`nosql-api.md`「転送路の共通規則」節。production
実装は `crates/wire-server/src/http/response.rs` が応答へ `Connection: close` を
必ず付与し、`crates/wire-server/src/http/conn.rs` の接続ハンドラも 1 要求を処理
したら接続を閉じる作りになっている）。そのため本モジュールは 1 要求ごとに新規
TCP 接続を張る（`http.client.HTTPConnection` を呼び出しごとに生成・破棄する）。
`meta.connection` にこの事実を明記する。

サーバー起動・停止・作業コピー redb・users.txt 生成のライフサイクルは
`self_db.SelfServer` をそのまま再利用する（`extra_args=["--surface", "nosql"]`
を渡すだけ。`SelfServer.connect()`（psycopg）は本モジュールでは使わない）。

`--config` は `exact` のみを受理する（HNSW opt-in 構成は self_db 側の担当のまま。
本モジュールで `hnsw` を指定すると `ValueError`）。

Cargo 依存・pip 依存は追加しない（Python 標準ライブラリ `http.client`・`json`
のみを使う）。
"""

from __future__ import annotations

import http.client
import json
import os
import random
import shutil
import tempfile
import time

import self_db
from common import DIM, measure, unsupported

BIND_HOST = self_db.BIND_HOST
BIND_PORT = self_db.BIND_PORT
USER_A = self_db.USER_A
USER_B = self_db.USER_B
PASSWORD = self_db.PASSWORD

_REQUEST_TIMEOUT_S = 30.0


class NosqlHttpError(RuntimeError):
    """`POST /v1/query` 等が非 2xx を返したときの例外（fail-closed。
    unsupported へ丸めず計測全体を失敗させるために使う）。"""

    def __init__(self, status: int, wire_code, message, body):
        super().__init__(f"HTTP {status} wire_code={wire_code!r}: {message!r}")
        self.status = status
        self.wire_code = wire_code
        self.message = message
        self.body = body


def _post(port: int, path: str, payload: dict, token: str | None = None) -> dict | None:
    """`path` へ JSON 本文を 1 回 POST する（1 要求 = 1 新規 TCP 接続。
    keep-alive 非対応のため。モジュール docstring 参照）。

    非 2xx は `NosqlHttpError` を送出する（呼び出し元で unsupported へ丸めない
    ことで、接続断・タイムアウト等の実行障害を成功扱いにしない。
    `self_db.py::_is_allowlist_syntax_rejection` と同じ fail-closed 方針）。
    """
    conn = http.client.HTTPConnection(BIND_HOST, port, timeout=_REQUEST_TIMEOUT_S)
    try:
        headers = {"Content-Type": "application/json"}
        if token is not None:
            headers["Authorization"] = f"Bearer {token}"
        body_bytes = json.dumps(payload).encode("utf-8")
        conn.request("POST", path, body=body_bytes, headers=headers)
        resp = conn.getresponse()
        raw = resp.read()
        status = resp.status
    finally:
        conn.close()
    data = json.loads(raw.decode("utf-8")) if raw else None
    if status != 200:
        err = data.get("error", {}) if isinstance(data, dict) else {}
        raise NosqlHttpError(status, err.get("wire_code"), err.get("message"), data)
    return data


def _issue_session(port: int, user: str, password: str) -> str:
    data = _post(port, "/v1/session", {"user": user, "password": password})
    return data["token"]


def _close_session(port: int, token: str | None) -> None:
    """セッション終了（best-effort。計測完了後の後片付けのため、失敗しても
    計測結果そのものには影響させない）。"""
    if token is None:
        return
    try:
        _post(port, "/v1/session/close", {}, token=token)
    except Exception:  # noqa: BLE001 - 後片付けであり計測の成否には影響させない
        pass


def _query(port: int, token: str, payload: dict) -> dict:
    return _post(port, "/v1/query", payload, token=token)


def _search(
    port: int,
    token: str,
    table: str,
    *,
    vector: list[float] | None = None,
    plan: str | None = None,
    limit: int,
    columns: list[str] | None = None,
    filter_: list[dict] | None = None,
    hybrid_: dict | None = None,
    mode: str | None = None,
) -> dict:
    payload: dict = {"op": "search", "table": table, "limit": limit}
    if vector is not None:
        payload["vector"] = vector
    if plan is not None:
        payload["plan"] = plan
    if columns is not None:
        payload["columns"] = columns
    if filter_ is not None:
        payload["filter"] = filter_
    if hybrid_ is not None:
        payload["hybrid"] = hybrid_
    if mode is not None:
        payload["mode"] = mode
    return _query(port, token, payload)


def _scan(
    port: int,
    token: str,
    table: str,
    *,
    limit: int,
    columns: list[str] | None = None,
    filter_: list[dict] | None = None,
) -> dict:
    payload: dict = {"op": "scan", "table": table, "limit": limit}
    if columns is not None:
        payload["columns"] = columns
    if filter_ is not None:
        payload["filter"] = filter_
    return _query(port, token, payload)


def _aggregate(
    port: int,
    token: str,
    table: str,
    *,
    aggregates: list[dict],
    filter_: list[dict] | None = None,
    group_by: list[str] | None = None,
    having: list[dict] | None = None,
) -> dict:
    payload: dict = {"op": "aggregate", "table": table, "aggregates": aggregates}
    if filter_ is not None:
        payload["filter"] = filter_
    if group_by is not None:
        payload["group_by"] = group_by
    if having is not None:
        payload["having"] = having
    return _query(port, token, payload)


def _insert(port: int, token: str, table: str, rows: list[dict], operation_id: str) -> dict:
    payload = {"op": "insert", "table": table, "rows": rows, "operation_id": operation_id}
    return _query(port, token, payload)


def _row_ids(data: dict) -> list:
    """`op: search`／`op: scan` 応答の先頭列（`columns=["id", ...]` 前提）から
    id 列だけを取り出す（`self_db.py::_exec_ids` の NoSQL 版）。"""
    return [row[0] for row in data["rows"]]


# `where_compound_count`（`id > 100 AND lang = 'ja'`）は NoSQL `filter` の語彙
# （`eq`／`prefix` のみ。範囲比較の演算子が存在しない。`nosql-api.md`
# 「`filter` 配列」節）で表現できないため、HTTP 要求を送らずに構造的
# unsupported として記録する。
_WHERE_COMPOUND_REASON = (
    "NoSQL filter[].op は eq/prefix のみで範囲比較演算子が無いため "
    "`id > 100` を表現できない（crates/wire-server/docs/nosql-api.md「filter 配列」節）"
)

# `udf_call`（宣言的 UDF 呼び出し・`CREATE FUNCTION`）は `op` 許可リスト（4 値:
# search/scan/aggregate/insert）に対応形が無い（`nosql-api.md`「SQL ↔ NoSQL
# 対応表」の「対応の無いもの」節）。
_UDF_REASON = (
    "宣言的 UDF 呼び出し（CREATE FUNCTION 相当）は NoSQL op 許可リスト "
    "（search/scan/aggregate/insert）に対応形が無い"
    "（crates/wire-server/docs/nosql-api.md「SQL ↔ NoSQL 対応表」節）"
)

# `explain`: `nosql-api.md`「explain」節のとおり、`EXPLAIN` 相当（`explain:
# true`）が受理されるのは `op: search` かつ `plan` 指定（LLM クエリ展開）の
# ときのみ。`vector_knn` 等は `vector` 指定のベクトル検索であり、`vector`
# 指定＋`explain: true` は構造的に `42601` で拒否される契約
# （self_db.py の `explain` フェーズが同じ理由で unsupported とする判断を
# NoSQL 側でも踏襲する）。`plan` 検索の `EXPLAIN` は LLM プランナー未接続
# （`--planner-endpoint` 未指定）のため本ハーネスの対象外のまま。
_EXPLAIN_REASON = (
    "vector 指定の検索への explain:true は 42601 で拒否される契約"
    "（plan 検索の EXPLAIN は LLM プランナー未接続のため対象外。"
    "crates/wire-server/docs/nosql-api.md「explain」節）"
)

# `ingest_bulk`: `op: insert` は `rows` 配列を持つが、1 要求あたりの行数上限は
# 既定 64（INDEX-4 ①。`nosql-api.md`「insert」節）であり、feature_bench.rs の
# `ingest_bulk`（ファイル形・数千チャンク規模の一括投入）に相当する意味論は
# 持たない。
_INGEST_BULK_REASON = (
    "op: insert の rows 配列は 1 要求あたり既定上限 64 行"
    "（INDEX-4 ①。FANDHE_DB_BATCH_MAX_FILES で上書き可能）であり、"
    "feature_bench.rs の ingest_bulk（ファイル形一括投入）に相当する規模の"
    "意味論を持たない（crates/wire-server/docs/nosql-api.md「insert」節）"
)


def run(args, queries: list[dict]) -> dict:
    """self（wire-server `--surface nosql`）の全フェーズを実行する。

    `args.rows_file` は self（`self_db.py` と同様）の場合 redb ファイルパスを
    指す。`args.config` は `exact` のみ受理する（HNSW opt-in 構成は self_db.py
    の担当のまま。本モジュールは対象外）。
    """
    if args.config != "exact":
        raise ValueError(
            f"self_nosql supports --config exact only (got {args.config!r}); "
            "HNSW opt-in (--config hnsw) is self_db.py's responsibility"
        )

    workdir = args.workdir
    os.makedirs(workdir, exist_ok=True)
    # self_db.py::run と同じ理由（redb を直接開かず作業コピーへ実行する。
    # ingest_single_stmt が行と operation_id 台帳を書き加えるため）。
    run_dir = tempfile.mkdtemp(prefix=f"self_nosql_bench_work_{os.getpid()}_", dir=workdir)
    work_db = os.path.join(run_dir, "self_nosql_bench_work.redb")

    rng_dim = len(queries[0]["embedding"]) if queries else DIM

    server: self_db.SelfServer | None = None
    token_a: str | None = None
    token_b: str | None = None
    try:
        shutil.copyfile(args.rows_file, work_db)
        server = self_db.SelfServer(
            db_path=work_db, workdir=workdir, extra_args=["--surface", "nosql"]
        )
        server.start()

        token_a = _issue_session(BIND_PORT, USER_A, PASSWORD)
        token_b = _issue_session(BIND_PORT, USER_B, PASSWORD)

        return _run_phases(args, queries, server, token_a, token_b, rng_dim)
    finally:
        _close_session(BIND_PORT, token_a)
        _close_session(BIND_PORT, token_b)
        if server is not None:
            server.stop()
        shutil.rmtree(run_dir, ignore_errors=True)


def _run_phases(
    args,
    queries: list[dict],
    server: self_db.SelfServer,
    token_a: str,
    token_b: str,
    rng_dim: int,
) -> dict:
    phases: dict = {}
    port = BIND_PORT
    table = "docs"

    query_vecs = [q["embedding"] for q in queries]
    query_texts = [q.get("text", "") for q in queries]

    # ANN opt-in を持たないため（`--config exact` 固定）、bulk 系フェーズの
    # ef フィールドは self_db.py の exact 構成と同じく常に None（self_db.py
    # 出力とキー構成を揃え、結果 JSON の横並び比較を容易にするためのみの目的。
    # 意味は「索引を持たないため候補幅の概念が無い」）。
    _ef_none = {"ef_search": None, "ef_effective": None}

    # --- vector_knn ---
    def knn(qv):
        data = _search(port, token_a, table, vector=qv, limit=10, columns=["id"])
        return _row_ids(data)

    stats, _ = measure(knn, query_vecs)
    knn_ids_all = [knn(qv) for qv in query_vecs]
    phases["vector_knn"] = {**stats, "ids_per_query": knn_ids_all}

    # --- vector_knn_where（point_where と同形のため統合。self_db.py 準拠） ---
    lang_ja_filter = [{"column": "lang", "op": "eq", "value": "ja"}]

    def knn_where(qv):
        data = _search(
            port, token_a, table, vector=qv, limit=10, columns=["id"], filter_=lang_ja_filter
        )
        return _row_ids(data)

    stats, _ = measure(knn_where, query_vecs)
    phases["vector_knn_where"] = stats
    phases["point_where"] = {
        "note": "vector_knn_where と同一クエリ形のため統合（task 指示準拠）",
        **stats,
    }

    # --- where_compound_count（filter 語彙に範囲比較が無く表現不能） ---
    phases["where_compound_count"] = unsupported(_WHERE_COMPOUND_REASON)

    # --- agg_count ---
    def agg_count(_):
        data = _aggregate(port, token_a, table, aggregates=[{"fn": "count", "column": "*"}])
        return data["rows"][0][0]

    stats, last = measure(agg_count, [None])
    phases["agg_count"] = {**stats, "result": last}

    # --- agg_multi ---
    def agg_multi(_):
        data = _aggregate(
            port,
            token_a,
            table,
            aggregates=[
                {"fn": "count", "column": "*"},
                {"fn": "sum", "column": "id"},
                {"fn": "avg", "column": "id"},
                {"fn": "min", "column": "id"},
                {"fn": "max", "column": "id"},
            ],
        )
        return data["rows"][0]

    stats, last = measure(agg_multi, [None])
    phases["agg_multi"] = {**stats, "result": list(last) if last else None}

    # --- group_by_having ---
    # NoSQL の GROUP BY には ORDER BY／LIMIT に相当するフィールドが無い
    # （`nosql-api.md`「SQL ↔ NoSQL 対応表」の「対応の無いもの」節）ため、
    # self_db.py の `... ORDER BY n DESC LIMIT 5` とは全件取得の点で異なる
    # （集計自体の意味論・HAVING 条件は同一）。
    def group_by(_):
        data = _aggregate(
            port,
            token_a,
            table,
            aggregates=[{"fn": "count", "column": "*"}],
            group_by=["lang"],
            having=[{"fn": "count", "column": "*", "op": ">", "value": 1}],
        )
        return data["rows"]

    stats, last = measure(group_by, [None])
    phases["group_by_having"] = {
        "note": (
            "NoSQL の GROUP BY には ORDER BY/LIMIT が無いため全件取得"
            "（self_db.py は ORDER BY n DESC LIMIT 5）。集計・HAVING 条件は同一"
        ),
        **stats,
        "result": last,
    }

    # --- hybrid_rrf ---
    def hybrid(i):
        qv, qt = query_vecs[i], query_texts[i]
        data = _search(
            port, token_a, table, vector=qv, limit=10, columns=["id"], hybrid_={"text": qt}
        )
        return _row_ids(data)

    idxs = list(range(len(query_vecs)))
    stats, _ = measure(hybrid, idxs)
    phases["hybrid_rrf"] = stats

    # --- mode_recall / mode_precision ---
    def mode_query(mode):
        def _run(qv):
            data = _search(port, token_a, table, vector=qv, limit=10, columns=["id"], mode=mode)
            return _row_ids(data)

        return _run

    stats, _ = measure(mode_query("recall"), query_vecs)
    phases["mode_recall"] = stats
    stats, _ = measure(mode_query("precision"), query_vecs)
    phases["mode_precision"] = stats

    # --- 広域取得（bulk fetch）: id と body を Top-N でまとめて返す ---
    def bulk_knn(k: int):
        def _run(qv):
            return _search(port, token_a, table, vector=qv, limit=k, columns=["id", "body"])

        return _run

    for k in (200, 1000):
        stats, last = measure(bulk_knn(k), query_vecs)
        phases[f"bulk_knn_k{k}"] = {
            **stats,
            "k": k,
            "rows_returned": len(last["rows"]),
            **_ef_none,
        }

    def bulk_knn_where(qv):
        return _search(
            port,
            token_a,
            table,
            vector=qv,
            limit=200,
            columns=["id", "body"],
            filter_=lang_ja_filter,
        )

    stats, last = measure(bulk_knn_where, query_vecs)
    phases["bulk_knn_where_k200"] = {
        **stats,
        "k": 200,
        "rows_returned": len(last["rows"]),
        **_ef_none,
    }

    def bulk_hybrid(i):
        qv, qt = query_vecs[i], query_texts[i]
        return _search(
            port,
            token_a,
            table,
            vector=qv,
            limit=200,
            columns=["id", "body"],
            hybrid_={"text": qt},
        )

    stats, last = measure(bulk_hybrid, idxs)
    phases["bulk_hybrid_k200"] = {
        **stats,
        "k": 200,
        "rows_returned": len(last["rows"]),
        "ef_search": None,
        "ef_effective": None,
        "dense_fetch_k_initial": None,
        "dense_fetch_k_may_expand": None,
    }

    # --- scan_where_nosort_k500（SQL-15 の bare 形。NoSQL の op: scan に対応） ---
    def scan_nosort(_):
        return _scan(port, token_a, table, limit=500, columns=["id", "body"], filter_=lang_ja_filter)

    stats, last = measure(scan_nosort, [None])
    phases["scan_where_nosort_k500"] = {**stats, "k": 500, "rows_returned": len(last["rows"])}

    # --- rls_isolation（tenant-b セッションで COUNT） ---
    def rls_count(_):
        data = _aggregate(port, token_b, table, aggregates=[{"fn": "count", "column": "*"}])
        return data["rows"][0][0]

    stats, last = measure(rls_count, [None])
    phases["rls_isolation"] = {**stats, "tenant_b_count": last}

    # --- udf_call（op 語彙に対応形が無い） ---
    phases["udf_call"] = unsupported(_UDF_REASON)

    # --- explain（vector 指定検索への explain:true は構造的に拒否される） ---
    phases["explain"] = unsupported(_EXPLAIN_REASON)

    # --- ingest_bulk（insert の rows 上限が feature_bench 規模のバルクに満たない） ---
    phases["ingest_bulk"] = unsupported(_INGEST_BULK_REASON)

    # --- ingest_single_stmt: 行形 insert を 1 要求 1 行で 1,000 回送る ---
    def make_row(n: int) -> dict:
        rid = 10_000_000 + n
        emb = [random.random() * 2 - 1 for _ in range(rng_dim)]
        lang = "ja" if n % 2 == 0 else "en"
        topic = f"topic-{n % 20:02d}"
        body = f"crossdb bench ingest row {n}"
        return {"id": rid, "embedding": emb, "lang": lang, "topic": topic, "body": body}

    n_ingest = 1000
    t0 = time.perf_counter()
    rows_ok = 0
    for n in range(n_ingest):
        _insert(port, token_a, table, [make_row(n)], operation_id=f"xdb-nosql-{n}")
        rows_ok += 1
    t1 = time.perf_counter()
    elapsed = t1 - t0
    phases["ingest_single_stmt"] = {
        "rows": rows_ok,
        "seconds": elapsed,
        "rows_per_sec": rows_ok / elapsed if elapsed > 0 else None,
        "commit_granularity": "per_request",
    }

    rows_visible_raw = phases.get("agg_count", {}).get("result")
    try:
        rows_visible = int(rows_visible_raw)
    except (TypeError, ValueError):
        rows_visible = 0

    from common import build_meta

    meta = build_meta(
        db="self_nosql",
        version=self_db._binary_version_string(server.binary),
        connection=(
            "loopback TCP (HTTP/1.1 POST /v1/query; 1 request per TCP connection; "
            "Connection: close — NoSQL 表層は keep-alive 非対応。"
            "crates/wire-server/docs/nosql-api.md「転送路の共通規則」節)"
        ),
        config=args.config,
        rows=rows_visible,
        dim=rng_dim,
        extra={
            "surface": "nosql",
            "index": "exact (no index)",
            "search_engine": "default",
            "keepalive": False,
        },
    )
    return {"meta": meta, "phases": phases}
