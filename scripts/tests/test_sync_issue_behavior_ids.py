"""`sync_issue_behavior_ids.py` の純関数（対応表検証・本文 upsert・ページング・
再試行判定）の単体テスト。GitHub API への実通信は対象外。
`python3 -m unittest discover scripts/tests`（`make scripts-test`）で実行する。
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import sync_issue_behavior_ids as sync  # noqa: E402

START = sync.START_MARKER
END = sync.END_MARKER


def _map(entries: dict[str, object], schema: object = 1) -> dict[str, object]:
    return {"schema": schema, "tree_root": 1, "map": entries}


class ValidateMapTests(unittest.TestCase):
    def test_accepts_valid_map(self) -> None:
        got = sync.validate_map(_map({"1516": ["SQL-41", "SQL-46"], "7": ["RLS-1"]}))
        self.assertEqual(got, {1516: ["SQL-41", "SQL-46"], 7: ["RLS-1"]})

    def test_rejects_wrong_schema(self) -> None:
        for schema in (2, 0, "1", True, None):
            with self.subTest(schema=schema):
                with self.assertRaises(sync.MapError):
                    sync.validate_map(_map({"1": ["SQL-1"]}, schema=schema))

    def test_rejects_non_object_or_empty_map(self) -> None:
        for data in ([], "x", {"schema": 1}, {"schema": 1, "map": {}},
                     {"schema": 1, "map": []}):
            with self.subTest(data=data):
                with self.assertRaises(sync.MapError):
                    sync.validate_map(data)

    def test_rejects_invalid_issue_numbers(self) -> None:
        for key in ("0", "-1", "abc", "01", "1.5", "", " 1"):
            with self.subTest(key=key):
                with self.assertRaises(sync.MapError):
                    sync.validate_map(_map({key: ["SQL-1"]}))

    def test_rejects_invalid_ids(self) -> None:
        for ids in ([], "SQL-1", ["sql-1"], ["SQL1"], ["SQL-"], ["SQL-1x"],
                    ["SQL-1\n"], [1], ["S QL-1"], ["SQL-1", "SQL-1"]):
            with self.subTest(ids=ids):
                with self.assertRaises(sync.MapError):
                    sync.validate_map(_map({"1": ids}))

    def test_load_map_rejects_duplicate_keys(self) -> None:
        raw = '{"schema":1,"tree_root":1,"map":{"1":["SQL-1"],"1":["SQL-2"]}}'
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "map.json")
            with open(path, "w", encoding="utf-8") as f:
                f.write(raw)
            with self.assertRaises(sync.MapError):
                sync.load_map(path)

    def test_load_map_rejects_broken_json_and_missing_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "map.json")
            with open(path, "w", encoding="utf-8") as f:
                f.write("{")
            with self.assertRaises(sync.MapError):
                sync.load_map(path)
            with self.assertRaises(sync.MapError):
                sync.load_map(os.path.join(tmp, "missing.json"))

    def test_load_map_accepts_valid_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "map.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(_map({"3": ["CORE-2"]}), f)
            self.assertEqual(sync.load_map(path), {3: ["CORE-2"]})


class MainFailClosedTests(unittest.TestCase):
    def test_invalid_map_exits_nonzero_without_token(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "map.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(_map({"1": ["bad"]}), f)
            self.assertEqual(sync.main(["--map", path, "--repo", "o/r", "--dry-run"]), 2)


class RenderSectionTests(unittest.TestCase):
    def test_contains_only_ids_and_pointers(self) -> None:
        section = sync.render_section(["SQL-41", "SQL-46"])
        self.assertTrue(section.startswith(START))
        self.assertTrue(section.endswith(END))
        self.assertIn("- SQL-41（`docs/spec/04-behavior/`）", section)
        self.assertIn("- SQL-46（`docs/spec/04-behavior/`）", section)
        self.assertNotIn("\r", section)

    def test_crlf_variant(self) -> None:
        section = sync.render_section(["SQL-1"], "\r\n")
        self.assertNotIn("\r\n", section.replace("\r\n", ""))
        self.assertEqual(section.replace("\r\n", "\n"), sync.render_section(["SQL-1"]))


class UpsertSectionTests(unittest.TestCase):
    def test_appends_when_absent_without_touching_body(self) -> None:
        body = "## 概要\n本文\n"
        got = sync.upsert_section(body, ["SQL-1"])
        self.assertTrue(got.startswith(body))
        self.assertEqual(got, body + "\n" + sync.render_section(["SQL-1"]) + "\n")

    def test_appends_with_blank_line_when_no_trailing_newline(self) -> None:
        got = sync.upsert_section("本文", ["SQL-1"])
        self.assertEqual(got, "本文\n\n" + sync.render_section(["SQL-1"]) + "\n")

    def test_handles_none_and_empty_body(self) -> None:
        expected = sync.render_section(["SQL-1"]) + "\n"
        self.assertEqual(sync.upsert_section(None, ["SQL-1"]), expected)
        self.assertEqual(sync.upsert_section("", ["SQL-1"]), expected)

    def test_is_idempotent(self) -> None:
        once = sync.upsert_section("本文\n", ["SQL-1", "RLS-2"])
        self.assertEqual(sync.upsert_section(once, ["SQL-1", "RLS-2"]), once)

    def test_replaces_in_place_and_keeps_outside_text(self) -> None:
        before = "前文\n\n"
        after = "\n\n## 後続節\n末尾"
        body = before + sync.render_section(["SQL-1"]) + after
        got = sync.upsert_section(body, ["SQL-2", "TABLE-3"])
        self.assertEqual(got, before + sync.render_section(["SQL-2", "TABLE-3"]) + after)

    def test_replaces_hand_edited_section(self) -> None:
        body = f"前\n{START}\n手書き\n{END}\n後"
        got = sync.upsert_section(body, ["SQL-9"])
        self.assertEqual(got, "前\n" + sync.render_section(["SQL-9"]) + "\n後")

    def test_crlf_body_is_preserved_and_idempotent(self) -> None:
        body = "## 概要\r\n本文\r\n"
        once = sync.upsert_section(body, ["SQL-1"])
        self.assertTrue(once.startswith(body))
        self.assertNotIn("\n", once.replace("\r\n", ""))
        self.assertEqual(sync.upsert_section(once, ["SQL-1"]), once)

    def test_lf_section_in_crlf_body_is_treated_as_same(self) -> None:
        # Web 編集で節以外が CRLF 化されても、内容が同じなら更新しない
        body = "前\r\n\r\n" + sync.render_section(["SQL-1"]) + "\r\n"
        self.assertEqual(sync.upsert_section(body, ["SQL-1"]), body)

    def test_rejects_malformed_markers(self) -> None:
        for body in (f"{START}\n本文", f"本文\n{END}", f"{END}\n{START}",
                     f"{START}\n{END}\n{START}\n{END}"):
            with self.subTest(body=body):
                with self.assertRaises(sync.MarkerError):
                    sync.upsert_section(body, ["SQL-1"])


class ParseNextLinkTests(unittest.TestCase):
    def test_extracts_next(self) -> None:
        header = ('<https://api.github.com/repositories/1/issues?page=2>; rel="next", '
                  '<https://api.github.com/repositories/1/issues?page=9>; rel="last"')
        self.assertEqual(sync.parse_next_link(header),
                         "https://api.github.com/repositories/1/issues?page=2")

    def test_returns_none_without_next(self) -> None:
        self.assertIsNone(sync.parse_next_link(None))
        self.assertIsNone(sync.parse_next_link(""))
        self.assertIsNone(sync.parse_next_link('<https://x/?page=1>; rel="prev"'))


class RetryTests(unittest.TestCase):
    def test_retryable_statuses(self) -> None:
        self.assertTrue(sync.is_retryable(None, {}, ""))
        self.assertTrue(sync.is_retryable(429, {}, ""))
        self.assertTrue(sync.is_retryable(502, {}, ""))
        self.assertTrue(sync.is_retryable(403, {"retry-after": "60"}, ""))
        self.assertTrue(sync.is_retryable(403, {"x-ratelimit-remaining": "0"}, ""))
        self.assertTrue(sync.is_retryable(403, {}, "You have exceeded a secondary rate limit"))

    def test_non_retryable_statuses(self) -> None:
        self.assertFalse(sync.is_retryable(403, {}, "Resource not accessible"))
        self.assertFalse(sync.is_retryable(404, {}, ""))
        self.assertFalse(sync.is_retryable(422, {}, ""))

    def test_retry_delay_prefers_headers_and_caps(self) -> None:
        self.assertEqual(sync.retry_delay(0, {"retry-after": "30"}, 0.0, 600.0), 30.0)
        self.assertEqual(sync.retry_delay(0, {"retry-after": "9999"}, 0.0, 600.0), 600.0)
        headers = {"x-ratelimit-remaining": "0", "x-ratelimit-reset": "1100"}
        self.assertEqual(sync.retry_delay(0, headers, 1000.0, 600.0), 100.0)
        self.assertEqual(sync.retry_delay(0, {}, 0.0, 600.0), 60.0)
        self.assertEqual(sync.retry_delay(2, {}, 0.0, 600.0), 240.0)
        self.assertEqual(sync.retry_delay(5, {}, 0.0, 600.0), 600.0)


if __name__ == "__main__":
    unittest.main()
