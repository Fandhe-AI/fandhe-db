"""crossdb の hybrid 取得深さ arm（Issue #3225）の純ロジックと静的検査。

`scripts/crossdb_bench/hybrid_depth.py` の定義・指標計算と、redis_db.py／self_db.py の
既存 hybrid フェーズ条件が変わっていないことをソース走査で固定する（numpy・redis 非依存）。
`make scripts-test` で CI の scripts-test ジョブが実行する。
"""

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "crossdb_bench"
sys.path.insert(0, str(ROOT))

import hybrid_depth as hd  # noqa: E402


def read(name: str) -> str:
    return (ROOT / name).read_text(encoding="utf-8")


class DepthDefinitionTest(unittest.TestCase):
    def test_redis_params_k_is_twice_window(self):
        self.assertEqual(hd.redis_params("hybrid_rrf_depth50"), {"K": 100, "WINDOW": 50})
        self.assertEqual(hd.redis_params("hybrid_rrf_depth200"), {"K": 400, "WINDOW": 200})

    def test_self_window_clause(self):
        self.assertEqual(hd.self_window_clause("hybrid_rrf_depth50"), ", window => 50")
        self.assertEqual(hd.self_window_clause("hybrid_rrf_depth200"), ", window => 200")
        with self.assertRaises(KeyError):
            hd.self_window_clause("nope")


class MetricsTest(unittest.TestCase):
    def test_overlap(self):
        self.assertEqual(hd.overlap_at_k([[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], [[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]]), 1.0)
        self.assertEqual(hd.overlap_at_k([[1, 2]], [[3, 4]]), 0.0)
        self.assertAlmostEqual(hd.overlap_at_k([[1, 2, 3, 4, 5]], [[1, 2, 3, 9, 9]], k=5), 0.6)

    def test_overlap_empty_or_mismatch_is_none(self):
        self.assertIsNone(hd.overlap_at_k([], []))
        self.assertIsNone(hd.overlap_at_k([[1]], []))

    def test_topic_hit_rate(self):
        topics = {1: "a", 2: "a", 3: "b"}
        rate, reason = hd.topic_hit_rate([[1, 2, 3]], topics, ["a"], k=4)
        self.assertEqual((rate, reason), (0.5, None))

    def test_topic_missing_is_none_with_reason(self):
        rate, reason = hd.topic_hit_rate([[1]], {}, ["a"])
        self.assertIsNone(rate)
        self.assertTrue(reason)
        rate, reason = hd.topic_hit_rate([[1]], {1: "a"}, [None])
        self.assertIsNone(rate)
        self.assertTrue(reason)

    def test_classify_explain(self):
        self.assertEqual(hd.classify_explain("INTERSECT {\n a\n b\n}"), "INTERSECT")
        self.assertEqual(hd.classify_explain("UNION {\n a\n b\n}"), "UNION")
        self.assertEqual(hd.classify_explain("a"), "unknown")

    def test_summary_line(self):
        line = hd.summary_line("x", {"p50_us": 1.5, "dense_gt_recall_at_10": 0.9})
        self.assertIn("p50_us=1.5", line)
        self.assertIn("dense_gt_recall_at_10=0.9", line)
        self.assertIn("unsupported", hd.summary_line("x", {"unsupported": True, "reason": "r"}))


class StaticConditionTest(unittest.TestCase):
    def test_existing_redis_arms_unchanged(self):
        src = read("redis_db.py")
        self.assertIn("_hybrid_once(i, 50, _hybrid_window(50))", src)
        self.assertIn('vsim_search_method_params={"K": 200}', src)
        self.assertIn("CombinationMethods.RRF, WINDOW=200", src)

    def test_redis_keeps_visibility_filter(self):
        src = read("redis_db.py")
        self.assertGreaterEqual(src.count("@visibility:{public}"), 4)
        self.assertIn('f"(@visibility:{{public}}) @body:({terms})"', src)

    def test_self_existing_hybrid_has_no_window(self):
        src = read("self_db.py")
        self.assertIn("f\"body, '{qt}') LIMIT 10\"", src)

    def test_arm_names_in_both_modules(self):
        # 名前は DEPTH_ARMS から共有する。両モジュールがそれを使うことを固定する。
        self.assertEqual(set(hd.DEPTH_ARMS), {"hybrid_rrf_depth50", "hybrid_rrf_depth200"})
        self.assertIn("DEPTH_ARMS", read("self_db.py"))
        self.assertIn("DEPTH_ARMS", read("redis_db.py"))
        self.assertIn("self_window_clause", read("self_db.py"))
        self.assertIn("redis_params", read("redis_db.py"))


if __name__ == "__main__":
    unittest.main()
