"""`recall.fixture_norm_stats` と、単位ノルムでのコサイン／内積の順位一致の単体テスト（Issue #3224）。"""

import json
import sys
import tempfile
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import recall  # noqa: E402


def _write(docs: list[dict]) -> str:
    f = tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False, encoding="utf-8")
    with f:
        for d in docs:
            f.write(json.dumps(d) + "\n")
    return f.name


def _doc(i: int, emb: list[float]) -> dict:
    return {"id": i, "tenant": "tenant-a", "visibility": "public", "embedding": emb}


class FixtureNormStatsTest(unittest.TestCase):
    def _stats(self, docs, queries):
        path = _write(docs)
        try:
            return recall.fixture_norm_stats(path, [{"embedding": q} for q in queries])
        finally:
            Path(path).unlink()

    def test_unit_norm(self):
        s = self._stats([_doc(1, [1.0, 0.0]), _doc(2, [0.6, 0.8])], [[0.0, 1.0]])
        self.assertLess(s["max_abs_norm_deviation"], 1e-6)
        self.assertEqual((s["zero_norm_docs"], s["zero_norm_queries"]), (0, 0))

    def test_non_unit_norm(self):
        s = self._stats([_doc(1, [2.0, 0.0])], [[1.0, 0.0]])
        self.assertAlmostEqual(s["max_abs_norm_deviation"], 1.0, places=6)

    def test_zero_norm(self):
        s = self._stats([_doc(1, [0.0, 0.0]), _doc(2, [1.0, 0.0])], [[0.0, 0.0]])
        self.assertEqual((s["zero_norm_docs"], s["zero_norm_queries"]), (1, 1))

    def test_cosine_and_inner_product_rank_agree_for_unit_vectors(self):
        rng = np.random.default_rng(0)
        m = rng.normal(size=(200, 16))
        m /= np.linalg.norm(m, axis=1, keepdims=True)
        q = m[0]
        ip_order = np.argsort(-(m @ q), kind="stable")
        cos_dist = 1.0 - (m @ q) / (np.linalg.norm(m, axis=1) * np.linalg.norm(q))
        self.assertEqual(list(ip_order[:10]), list(np.argsort(cos_dist, kind="stable")[:10]))


if __name__ == "__main__":
    unittest.main()
