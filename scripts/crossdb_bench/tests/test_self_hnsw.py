"""`self_hnsw.py` の単体テスト（Issue #658。venv 不要・標準ライブラリのみ）。

`python3 -m unittest discover scripts/crossdb_bench/tests` で実行する。
"""

from __future__ import annotations

import http.client
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import self_hnsw  # noqa: E402


class ParseHnswArgsEnvTests(unittest.TestCase):
    def test_none_or_empty_returns_empty_list(self) -> None:
        self.assertEqual(self_hnsw.parse_hnsw_args_env(None), [])
        self.assertEqual(self_hnsw.parse_hnsw_args_env(""), [])
        self.assertEqual(self_hnsw.parse_hnsw_args_env("   "), [])

    def test_single_flag_value_pair(self) -> None:
        self.assertEqual(
            self_hnsw.parse_hnsw_args_env("--hnsw-full-scan-ratio 1/2"),
            ["--hnsw-full-scan-ratio", "1/2"],
        )

    def test_multiple_flag_value_pairs(self) -> None:
        self.assertEqual(
            self_hnsw.parse_hnsw_args_env(
                "--hnsw-full-scan-ratio 1/2 --hnsw-acorn-max-visible-ratio 1/1"
            ),
            [
                "--hnsw-full-scan-ratio",
                "1/2",
                "--hnsw-acorn-max-visible-ratio",
                "1/1",
            ],
        )

    def test_rejects_non_hnsw_flag(self) -> None:
        with self.assertRaises(ValueError):
            self_hnsw.parse_hnsw_args_env("--bind 0.0.0.0:1")

    def test_rejects_non_flag_token(self) -> None:
        with self.assertRaises(ValueError):
            self_hnsw.parse_hnsw_args_env("not-a-flag value")

    def test_rejects_missing_value(self) -> None:
        with self.assertRaises(ValueError):
            self_hnsw.parse_hnsw_args_env("--hnsw-sparse-visited-max")

    def test_rejects_value_that_looks_like_a_flag(self) -> None:
        with self.assertRaises(ValueError):
            self_hnsw.parse_hnsw_args_env(
                "--hnsw-full-scan-ratio --hnsw-acorn-max-visible-ratio 1/1"
            )

    def test_rejects_injection_style_value(self) -> None:
        # 値トークンとしてなら任意の非フラグ文字列を通す設計だが、フラグ名
        # 自体の許可リストは崩れないことを確認する（`--search-engine` 等の
        # 混入を拒否）。
        with self.assertRaises(ValueError):
            self_hnsw.parse_hnsw_args_env("--search-engine hnsw")


class EfEffectiveTests(unittest.TestCase):
    def test_k_below_ef_search_uses_ef_search(self) -> None:
        self.assertEqual(self_hnsw.ef_effective(10), self_hnsw.DEFAULT_EF_SEARCH)

    def test_k_above_ef_search_uses_k(self) -> None:
        self.assertEqual(self_hnsw.ef_effective(1000), 1000)

    def test_k_equal_to_ef_search(self) -> None:
        self.assertEqual(
            self_hnsw.ef_effective(self_hnsw.DEFAULT_EF_SEARCH), self_hnsw.DEFAULT_EF_SEARCH
        )

    def test_custom_ef_search(self) -> None:
        self.assertEqual(self_hnsw.ef_effective(200, ef_search=256), 256)
        self.assertEqual(self_hnsw.ef_effective(10, ef_search=256), 256)


class VerifyExplainRowsTests(unittest.TestCase):
    HNSW_ROWS = [
        "mode_source: default",
        "mode: recall",
        "confidence: n/a",
        "search_terms: probe",
        "engine: hnsw",
        "hnsw_params: m=16,ef_construction=100,ef_search=64,resident=f32",
        "ann_plan: hnsw_full_visible",
        "scalar_plan: plain_scan",
    ]

    DEFAULT_ROWS = [
        "mode_source: default",
        "mode: recall",
        "confidence: n/a",
        "search_terms: probe",
        "engine: parallel_brute_force",
        "ann_plan: plain_scan_engine",
        "scalar_plan: plain_scan",
    ]

    def test_hnsw_rows_pass_when_expected(self) -> None:
        result = self_hnsw.verify_explain_rows(self.HNSW_ROWS, expect_hnsw=True)
        self.assertEqual(result["engine"], "hnsw")
        self.assertEqual(
            result["hnsw_params"], "m=16,ef_construction=100,ef_search=64,resident=f32"
        )
        self.assertEqual(result["ann_plan"], "hnsw_full_visible")

    def test_default_rows_pass_when_non_hnsw_expected(self) -> None:
        result = self_hnsw.verify_explain_rows(self.DEFAULT_ROWS, expect_hnsw=False)
        self.assertEqual(result["engine"], "parallel_brute_force")
        self.assertIsNone(result["hnsw_params"])
        self.assertEqual(result["ann_plan"], "plain_scan_engine")

    def test_hnsw_expected_but_got_default_raises(self) -> None:
        with self.assertRaises(RuntimeError):
            self_hnsw.verify_explain_rows(self.DEFAULT_ROWS, expect_hnsw=True)

    def test_non_hnsw_expected_but_got_hnsw_raises(self) -> None:
        with self.assertRaises(RuntimeError):
            self_hnsw.verify_explain_rows(self.HNSW_ROWS, expect_hnsw=False)

    def test_missing_engine_line_raises(self) -> None:
        with self.assertRaises(RuntimeError):
            self_hnsw.verify_explain_rows(["mode: recall"], expect_hnsw=True)

    def test_missing_ann_plan_line_raises(self) -> None:
        rows = [r for r in self.HNSW_ROWS if not r.startswith("ann_plan:")]
        with self.assertRaises(RuntimeError):
            self_hnsw.verify_explain_rows(rows, expect_hnsw=True)

    def test_unexpected_hnsw_params_when_not_hnsw_raises(self) -> None:
        rows = self.DEFAULT_ROWS + [
            "hnsw_params: m=16,ef_construction=100,ef_search=64,resident=f32"
        ]
        with self.assertRaises(RuntimeError):
            self_hnsw.verify_explain_rows(rows, expect_hnsw=False)


class PlannerStubHttpTests(unittest.TestCase):
    """固定応答スタブの実 HTTP 応答形式を loopback 越しに確認する。"""

    def test_stub_responds_with_fixed_expansion(self) -> None:
        stub = self_hnsw.PlannerStub()
        stub.start()
        try:
            host, port_str = stub.endpoint.split(":")
            conn = http.client.HTTPConnection(host, int(port_str), timeout=5.0)
            try:
                body = b'{"model":"crossdb-stub","prompt":"anything"}'
                conn.request(
                    "POST",
                    "/api/generate",
                    body=body,
                    headers={"Content-Length": str(len(body))},
                )
                resp = conn.getresponse()
                self.assertEqual(resp.status, 200)
                payload = resp.read().decode("utf-8")
            finally:
                conn.close()
        finally:
            stub.stop()

        import json

        parsed = json.loads(payload)
        self.assertIn("response", parsed)
        inner = json.loads(parsed["response"])
        self.assertEqual(inner["search_terms"], ["probe"])
        self.assertIsNone(inner["path_hint"])
        self.assertIsNone(inner["kind_hint"])

    def test_stub_stop_is_idempotent(self) -> None:
        stub = self_hnsw.PlannerStub()
        stub.start()
        stub.stop()
        stub.stop()  # 多重呼び出しでも例外にならない


class ProbeBinaryPathTests(unittest.TestCase):
    def test_nonexistent_override_raises(self) -> None:
        old = os.environ.get("CROSSDB_PLAN_PROBE_BINARY")
        os.environ["CROSSDB_PLAN_PROBE_BINARY"] = "/nonexistent/path/crossdb_plan_probe"
        try:
            with self.assertRaises(FileNotFoundError):
                self_hnsw.probe_binary_path()
        finally:
            if old is None:
                del os.environ["CROSSDB_PLAN_PROBE_BINARY"]
            else:
                os.environ["CROSSDB_PLAN_PROBE_BINARY"] = old

    def test_override_existing_path_is_normalized(self) -> None:
        old = os.environ.get("CROSSDB_PLAN_PROBE_BINARY")
        this_file = os.path.abspath(__file__)
        os.environ["CROSSDB_PLAN_PROBE_BINARY"] = this_file
        try:
            self.assertEqual(self_hnsw.probe_binary_path(), this_file)
        finally:
            if old is None:
                del os.environ["CROSSDB_PLAN_PROBE_BINARY"]
            else:
                os.environ["CROSSDB_PLAN_PROBE_BINARY"] = old


class ParseExplainRowsTests(unittest.TestCase):
    def test_engine_and_ann_plan_present(self) -> None:
        rows = ["engine: brute_force", "ann_plan: plain_scan_opclass_mismatch"]
        self.assertEqual(
            self_hnsw.parse_explain_rows(rows),
            {"engine": "brute_force", "ann_plan": "plain_scan_opclass_mismatch"},
        )

    def test_missing_lines_yield_none(self) -> None:
        self.assertEqual(self_hnsw.parse_explain_rows([]), {"engine": None, "ann_plan": None})
        self.assertEqual(
            self_hnsw.parse_explain_rows(["engine: hnsw"]),
            {"engine": "hnsw", "ann_plan": None},
        )
        self.assertEqual(
            self_hnsw.parse_explain_rows(["ann_plan: x"]),
            {"engine": None, "ann_plan": "x"},
        )


class KnnReachesHnswTests(unittest.TestCase):
    def test_hnsw_plans_reach(self) -> None:
        for plan in ("hnsw_full_visible", "hnsw_subset"):
            self.assertTrue(self_hnsw.knn_reaches_hnsw({"engine": "hnsw", "ann_plan": plan}))

    def test_opclass_mismatch_with_hnsw_engine_does_not_reach(self) -> None:
        # --config hnsw + `<#>`: engine は hnsw のまま plain scan に倒れる（Issue #2952）
        plan = {"engine": "hnsw", "ann_plan": "plain_scan_opclass_mismatch"}
        self.assertFalse(self_hnsw.knn_reaches_hnsw(plan))

    def test_missing_ann_plan_does_not_reach(self) -> None:
        self.assertFalse(self_hnsw.knn_reaches_hnsw({"engine": "hnsw", "ann_plan": None}))
        self.assertFalse(self_hnsw.knn_reaches_hnsw({"engine": None, "ann_plan": "plain_scan_engine"}))


if __name__ == "__main__":
    unittest.main()
