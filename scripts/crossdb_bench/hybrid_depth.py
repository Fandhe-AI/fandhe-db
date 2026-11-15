"""hybrid の取得深さを self と Redis で揃えた arm の定義と、品質指標の純ロジック（Issue #3225）。

役割:
    self_db.py・redis_db.py が同名のフェーズ（`hybrid_rrf_depth50`・`hybrid_rrf_depth200`）を
    同じ深さ対応で計測するための定数・整形関数と、run.py が結果 JSON へ書き込む
    品質指標（recall 相当・重なり・topic 一致率）の計算を置く。標準ライブラリだけで動かし、
    `make scripts-test`（numpy・redis 非依存）から単体テストできるようにしている。

深さの対応:
    Redis の WINDOW = self の pool_depth（`window` 引数。SEARCH-13 のポインタ）、
    Redis の K = self の初回 fetch_k（2 x pool_depth）。既存の `hybrid_rrf`
    （Redis K=50・WINDOW=50）・`bulk_hybrid_k200` の条件は変えない。

品質指標（いずれも「BM25 の関連性 recall」ではない。DB 間で共通の hybrid 正解は作れないため）:
    - dense_gt_recall_at_10: hybrid 上位 10 件を内積総当たり正解と照合した割合（run.py が recall.py で計算）
    - overlap_at_10_vs_depth200: 同じ DB の depth200 arm 上位 10 件との重なり（depth50 のみ）
    - topic_hit_rate_at_10: クエリの topic と一致する返却文書の割合（ラベル基準の代理指標）
"""

from __future__ import annotations

# フェーズ名 -> self の pool_depth（= Redis の WINDOW）。
DEPTH_ARMS: dict[str, int] = {
    "hybrid_rrf_depth50": 50,
    "hybrid_rrf_depth200": 200,
}

# 結果の top-k。
TOP_K = 10


def self_window_clause(arm: str) -> str:
    """self の SQL へ足す `, window => N` 句を返す（N は DEPTH_ARMS の整数定数のみ。未知 arm は KeyError）。"""
    return f", window => {int(DEPTH_ARMS[arm])}"


def redis_params(arm: str) -> dict[str, int]:
    """Redis FT.HYBRID の `{"K": 2 x pool, "WINDOW": pool}` を返す（self の初回 fetch_k と pool_depth に対応）。"""
    pool = int(DEPTH_ARMS[arm])
    return {"K": 2 * pool, "WINDOW": pool}


def overlap_at_k(a_per_q: list[list[int]], b_per_q: list[list[int]], k: int = TOP_K) -> float | None:
    """クエリ平均の |top-k(A) ∩ top-k(B)| / k。クエリ数が食い違う・空なら None（0 とは書かない）。"""
    if not a_per_q or len(a_per_q) != len(b_per_q) or k <= 0:
        return None
    total = 0.0
    for a, b in zip(a_per_q, b_per_q):
        total += len(set(a[:k]) & set(b[:k])) / k
    return total / len(a_per_q)


def topic_hit_rate(
    ids_per_q: list[list[int]],
    doc_topic: dict[int, str],
    query_topics: list[str | None],
    k: int = TOP_K,
) -> tuple[float | None, str | None]:
    """クエリ topic と一致する返却文書の割合（クエリ平均）と、算出不能時の理由を返す。

    返却が k 件未満でも分母は k のまま（取りこぼしを不利に数える）。topic が docs・queries の
    どちらかに無ければ `(None, 理由)`（fail-closed）。
    """
    if not ids_per_q or len(ids_per_q) != len(query_topics):
        return None, "queries と返却 id の件数が一致しない、または空"
    if not doc_topic or any(t is None for t in query_topics):
        return None, "docs または queries に topic が無い"
    total = 0.0
    for ids, qt in zip(ids_per_q, query_topics):
        total += sum(1 for i in ids[:k] if doc_topic.get(i) == qt) / k
    return total / len(ids_per_q), None


def classify_explain(text: str) -> str:
    """FT.EXPLAIN 出力から語の結合を判定する。INTERSECT のみ→AND、UNION のみ→OR、それ以外 unknown。"""
    upper = text.upper()
    has_i, has_u = "INTERSECT" in upper, "UNION" in upper
    if has_i and not has_u:
        return "INTERSECT"
    if has_u and not has_i:
        return "UNION"
    return "unknown"


def summary_line(phase: str, data: dict) -> str:
    """arm 1 つ分の p50 と品質指標を stdout 1 行へ整形する。unsupported はその旨を出す。"""
    if data.get("unsupported"):
        return f"{phase}: unsupported ({data.get('reason', '')})"
    parts = [f"p50_us={data.get('p50_us')}"]
    for key in (
        "dense_gt_recall_at_10",
        "dense_gt_recall_at_10_strict",
        "overlap_at_10_vs_depth200",
        "topic_hit_rate_at_10",
    ):
        if key in data:
            parts.append(f"{key}={data[key]}")
    if data.get("quality_unsupported"):
        parts.append(f"quality_unsupported={data['quality_unsupported']}")
    return f"{phase}: " + " ".join(parts)
