"""Recall@10 の正解集合（ground truth）を作る配線検算ユーティリティ。

`docs25k.jsonl` から RLS 相当で可視な行の embedding を numpy に乗せ、内積
（自作 DB の `<=>` と同じ指標。`crates/engine/src/kernel.rs` の「スコアは
内積」に合わせる）で 200 クエリそれぞれの正解を計算する。

可視性モデル（実機確認済みの現行契約）: 自作 DB はどのテナントの wire
セッションからも `visibility = 'public'` の行のみが可視であり、private 行は
所有テナント自身のセッションからも不可視（`docs25k.jsonl`: tenant-a 行 =
public 23,000・tenant-b 行 = private 2,000。tenant-a・tenant-b いずれの
セッションで `COUNT(*)` しても 23,000 になることを実機で確認済み）。正解集合は
tenant を問わず `visibility == "public"` の行のみから作る。`visibility`
フィールドを持たない旧フィクスチャとの互換のため、ファイル全体に一つも
`visibility` キーが無い場合のみ従来どおり tenant-a 一致で判定する。

同点許容の Recall@10: 実測データには同一スコアの文書が多数存在し（合成
embedding の構造上、内積が完全一致するグループができやすい）、「厳密な
上位 10 件」の境界がどの 10 件を含むかは同点内の順序規約に依存してしまう
（exact 構成でも DB 間で一致しない）。そのため正解集合を、境界より厳密に
上位の集合 `strict_above`（スコア > 境界スコア + ε）と境界での同点集合
`tie_boundary`（|スコア − 境界スコア| ≤ ε）に分けて持つ
（`recall_at_k_tie_tolerant`）。recall の分子は
`|hits ∩ strict_above| + min(|hits ∩ tie_boundary|, k − |strict_above|)`
（`tie_boundary` からの充当は「境界より厳密上位の欠落を埋め合わせない」よう
残り枠 `k − |strict_above|` で頭打ちにする）、分母は `min(k, 可視件数)` と
する。こうすることで、`strict_above` の文書を返さず `tie_boundary` の文書
だけで枠を埋めても満点にはならない（例: strict_above 1 件・tie_boundary
20 件のとき、strict_above を落として tie_boundary から 10 件返すと
9/10。同点内の入れ替えのみなら 1.0 のまま）。従来の厳密一致
（同点は決定的だが恣意的な順序で 10 件に切る）は `recall_at_k`／
`recall_at_10_strict` として引き続き提供する。

同点判定には小さな許容誤差（`TIE_EPSILON`）を設ける。数学的には同一の
内積値でも、DB ごとに float32 の総和順序が異なると計算結果が最終桁で
ずれる（実機確認: pgvector と numpy で同一の意味的な同点グループのうち
1 件だけ `0.4850712716579437` と `0.48507124185562134` のように 1e-7 台で
食い違い、許容誤差なしの厳密な `>=` 比較では同点集合から漏れる）。
`TIE_EPSILON = 1e-4` は float32 の総和誤差（次元 128 程度で概ね 1e-6〜1e-7）
より十分大きく、かつこのフィクスチャで観測された「真に異なるスコア間の
最小差」（1e-2 オーダー）より十分小さい値として選んだ。
"""

from __future__ import annotations

from typing import NamedTuple

import numpy as np

from common import doc_visibility, load_jsonl

# float32 の総和順序差に起因する同点境界の見落としを防ぐための許容誤差。
# モジュール docstring の「同点判定には小さな許容誤差」を参照。
TIE_EPSILON = 1e-4


class TieBoundary(NamedTuple):
    """1 クエリ分の同点許容正解集合。

    - strict_above: 境界スコアより厳密に上位（score > kth_score + ε）の
      文書 id 集合。同点の有無によらず必ず返却されているべき文書。
    - tie_boundary: 境界での同点集合（|score - kth_score| <= ε）。返却側の
      同点内順序規約の違いを許容するための集合。
    - visible_count: このクエリの正解計算対象になった可視文書の総数
      （strict_above・tie_boundary に含まれない、境界より下位の文書も含む）。
      recall の分母 `min(k, visible_count)` の算出に使う。
    """

    strict_above: set[int]
    tie_boundary: set[int]
    visible_count: int


def _visible_mask(docs: list[dict]) -> list[bool]:
    # 投入側（各 `*_db.py`）と同じ `common.doc_visibility` で判定し、旧フィクスチャ
    # （visibility 未導入）の欠落値の扱いを両側で統一する。
    return [doc_visibility(d) == "public" for d in docs]


def build_ground_truth(
    docs_path: str, queries: list[dict], k: int = 10
) -> tuple[list[list[int]], list[TieBoundary]]:
    """`visibility == "public"` な行に対する内積正解を各クエリについて返す。

    戻り値は (strict_top_k, tie_boundaries) のタプル。
    - strict_top_k: 各クエリの厳密な上位 k 件 id リスト（同点は決定的な順序
      ——`np.argsort` の安定ソートにより可視行の元の並び順、`docs25k.jsonl`
      内の出現順——で切る。従来の `recall_at_10_strict` 用）。
    - tie_boundaries: 各クエリの `TieBoundary`（`recall_at_k_tie_tolerant` 用。
      詳細はモジュール docstring の「同点許容の Recall@10」節）。
    """
    docs = load_jsonl(docs_path)
    mask = _visible_mask(docs)
    visible = [d for d, m in zip(docs, mask) if m]
    strict_top_k: list[list[int]] = []
    tie_boundaries: list[TieBoundary] = []
    if not visible:
        # 可視行が無い場合は行列積へ進まない（`np.array([])` は形状 (0,) になり
        # `mat @ qv` が次元不一致で失敗する）。各クエリの正解集合は空。
        empty_boundary = TieBoundary(strict_above=set(), tie_boundary=set(), visible_count=0)
        return [[] for _ in queries], [empty_boundary for _ in queries]
    ids = np.array([d["id"] for d in visible], dtype=np.int64)
    mat = np.array([d["embedding"] for d in visible], dtype=np.float32)  # (N, dim)
    visible_count = len(visible)

    for q in queries:
        qv = np.array(q["embedding"], dtype=np.float32)
        scores = mat @ qv  # 内積
        # 安定ソート（同点は元の並び順を保つ）で降順（大きいほど上位）にする。
        order = np.argsort(-scores, kind="stable")
        top_idx = order[: min(k, len(order))]
        strict_top_k.append([int(ids[i]) for i in top_idx])

        if len(scores) == 0:
            tie_boundaries.append(TieBoundary(strict_above=set(), tie_boundary=set(), visible_count=0))
            continue
        kth_pos = min(k, len(scores)) - 1
        kth_score = scores[order[kth_pos]]
        strict_above_mask = scores > (kth_score + TIE_EPSILON)
        tie_boundary_mask = np.abs(scores - kth_score) <= TIE_EPSILON
        tie_boundaries.append(
            TieBoundary(
                strict_above={int(x) for x in ids[strict_above_mask]},
                tie_boundary={int(x) for x in ids[tie_boundary_mask]},
                visible_count=visible_count,
            )
        )
    return strict_top_k, tie_boundaries


def recall_at_k(returned_ids_per_query: list[list[int]], ground_truth: list[list[int]]) -> float:
    """厳密一致版 Recall@k（従来どおり）。returned と ground_truth（固定長リスト）
    を突き合わせ平均 Recall@k を返す。"""
    if not ground_truth:
        return float("nan")
    scores = []
    for returned, truth in zip(returned_ids_per_query, ground_truth):
        if not truth:
            continue
        hit = len(set(returned) & set(truth))
        scores.append(hit / len(truth))
    if not scores:
        return float("nan")
    return sum(scores) / len(scores)


def recall_at_k_tie_tolerant(
    returned_ids_per_query: list[list[int]], tie_boundaries: list[TieBoundary], k: int = 10
) -> float:
    """同点許容版 Recall@k。

    分子は `|hits ∩ strict_above| + min(|hits ∩ tie_boundary|, k − |strict_above|)`
    （`tie_boundary` からの充当を残り枠で頭打ちにし、`strict_above` の欠落を
    `tie_boundary` の過剰一致で埋め合わせられないようにする）、分母は
    `min(k, visible_count)` とする（strict 版 `recall_at_k` と同じ分母定義）。
    詳細・具体例はモジュール docstring の「同点許容の Recall@10」節を参照。
    """
    if not tie_boundaries:
        return float("nan")
    scores = []
    for returned, boundary in zip(returned_ids_per_query, tie_boundaries):
        denom = min(k, boundary.visible_count)
        if denom == 0:
            continue
        returned_set = set(returned)
        strict_hits = len(returned_set & boundary.strict_above)
        tie_hits = len(returned_set & boundary.tie_boundary)
        tie_room = max(0, k - len(boundary.strict_above))
        numerator = strict_hits + min(tie_hits, tie_room)
        scores.append(min(numerator, denom) / denom)
    if not scores:
        return float("nan")
    return sum(scores) / len(scores)


if __name__ == "__main__":
    # 自己テスト 1: 可視 9 行・k=10 のとき strict/tie-tolerant 両版の分母が
    # min(k, 可視件数) = 9 に揃い、可視 9 行を全件返せば両版とも 1.0 になる
    # ことを確認する（codex-review P2 指摘の分母不整合の再発防止）。
    visible_ids = list(range(9))
    ground_truth = [visible_ids]
    all_tie_boundary = TieBoundary(strict_above=set(), tie_boundary=set(visible_ids), visible_count=9)
    returned = [visible_ids]

    strict = recall_at_k(returned, ground_truth)
    tie_tolerant = recall_at_k_tie_tolerant(returned, [all_tie_boundary], k=10)
    assert strict == 1.0, f"strict recall expected 1.0, got {strict}"
    assert tie_tolerant == 1.0, f"tie-tolerant recall expected 1.0, got {tie_tolerant}"
    print("recall.py self-test 1 OK: visible=9, k=10 -> strict=1.0, tie_tolerant=1.0")

    # 自己テスト 2: strict_above 1 件・tie_boundary 20 件（例: スコア 1.0 が
    # 1 件・0.5 が 20 件、k=10）で strict_above の 1 件を落として tie_boundary
    # から 10 件だけ返すと 9/10 になる（境界より厳密上位の欠落を tie_boundary
    # の過剰一致で埋め合わせない。codex-review P2 指摘の再発防止）。
    strict_above_id = 1000
    tie_boundary_ids = list(range(20))
    boundary = TieBoundary(
        strict_above={strict_above_id}, tie_boundary=set(tie_boundary_ids), visible_count=21
    )
    returned_missing_top = [tie_boundary_ids[:10]]
    recall_missing_top = recall_at_k_tie_tolerant(returned_missing_top, [boundary], k=10)
    assert recall_missing_top == 0.9, f"expected 0.9, got {recall_missing_top}"
    print("recall.py self-test 2 OK: strict_above dropped -> tie_tolerant=0.9")

    # 自己テスト 3: 同点内の入れ替えのみ（strict_above は含み、tie_boundary は
    # 別の 9 件を返す）なら 1.0 のままであることを確認する。
    returned_tie_swapped = [[strict_above_id] + tie_boundary_ids[10:19]]
    recall_tie_swapped = recall_at_k_tie_tolerant(returned_tie_swapped, [boundary], k=10)
    assert recall_tie_swapped == 1.0, f"expected 1.0, got {recall_tie_swapped}"
    print("recall.py self-test 3 OK: tie-boundary swap only -> tie_tolerant=1.0")
