"""`sync_issue_behavior_ids.py` の純関数（対応表検証・本文 upsert・ページング・
再試行判定）と書き込み手順（待機は GET より前・PATCH 再試行は最新本文の再取得から）
の単体テスト。GitHub API への実通信は対象外（偽クライアント・urlopen 差し替え）。
`python3 -m unittest discover scripts/tests`（`make scripts-test`）で実行する。
"""

from __future__ import annotations

import argparse
import io
import json
import os
import sys
import tempfile
import unittest
import urllib.error
from contextlib import redirect_stdout
from unittest import mock

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


class FakeClient:
    """issue 本文・状態を持つ偽 GitHub クライアント。呼び出し順を events に記録する。

    patch_failures で PATCH を指定回数だけ失敗させ、on_patch_failure で失敗直後に
    「待機中の手動編集・close」を模擬する。
    """

    def __init__(self, issues: dict[int, dict[str, object]], events: list[str]) -> None:
        self.issues = issues
        self.events = events
        self.patch_failures: list[sync.ApiError] = []
        self.on_patch_failure = None
        self.patched: list[tuple[int, str]] = []

    @staticmethod
    def _number(url: str) -> int:
        return int(url.rsplit("/", 1)[1])

    def request(self, method, url, payload=None, retry=True):
        if "?state=open" in url:
            self.events.append("LIST")
            items = [{"number": n, "body": i["body"]} for n, i in self.issues.items()
                     if i["state"] == "open"]
            return items, {}
        number = self._number(url)
        if method == "GET":
            self.events.append(f"GET#{number}")
            issue = self.issues[number]
            return {"state": issue["state"], "body": issue["body"]}, {}
        assert method == "PATCH"
        # PATCH は呼び出し側で再試行するため、クライアント内再試行を無効化していること
        assert retry is False
        self.events.append(f"PATCH#{number}")
        if self.patch_failures:
            exc = self.patch_failures.pop(0)
            if self.on_patch_failure:
                self.on_patch_failure(number)
            raise exc
        self.issues[number]["body"] = payload["body"]
        self.patched.append((number, payload["body"]))
        return {}, {}


class SyncIssueTests(unittest.TestCase):
    URL = "https://api.github.com/repos/o/r/issues/5"

    def setUp(self) -> None:
        self.events: list[str] = []
        self.client = FakeClient({5: {"state": "open", "body": "v1"}}, self.events)

    def _sleep(self, seconds: float) -> None:
        self.events.append(f"SLEEP{seconds:g}")

    def test_retry_refetches_and_rebuilds_payload_from_latest_body(self) -> None:
        self.client.patch_failures = [sync.ApiError(429, "PATCH failed", True)]

        def edit_during_wait(number: int) -> None:
            self.client.issues[number]["body"] = "v2 手動編集"

        self.client.on_patch_failure = edit_during_wait
        with redirect_stdout(io.StringIO()):
            got = sync.sync_issue(self.client, self.URL, ["SQL-1"], self._sleep)
        self.assertEqual(got, "updated")
        self.assertEqual(self.events, ["GET#5", "PATCH#5", "SLEEP60", "GET#5", "PATCH#5"])
        # 再送 payload は待機中の手動編集を含む最新本文から作られる（古い v1 ではない）
        self.assertEqual(self.client.patched,
                         [(5, sync.upsert_section("v2 手動編集", ["SQL-1"]))])

    def test_retry_stops_when_issue_closed_during_wait(self) -> None:
        self.client.patch_failures = [sync.ApiError(502, "PATCH failed", True)]

        def close_during_wait(number: int) -> None:
            self.client.issues[number]["state"] = "closed"

        self.client.on_patch_failure = close_during_wait
        with redirect_stdout(io.StringIO()):
            got = sync.sync_issue(self.client, self.URL, ["SQL-1"], self._sleep)
        self.assertEqual(got, "not-open")
        self.assertEqual(self.client.patched, [])
        self.assertEqual(self.events, ["GET#5", "PATCH#5", "SLEEP60", "GET#5"])

    def test_retry_skips_when_body_already_synced(self) -> None:
        # PATCH が実は反映済み（応答だけ失われた）でも再取得で冪等に終わる
        self.client.patch_failures = [sync.ApiError(None, "PATCH failed", True)]

        def applied_anyway(number: int) -> None:
            self.client.issues[number]["body"] = sync.upsert_section("v1", ["SQL-1"])

        self.client.on_patch_failure = applied_anyway
        with redirect_stdout(io.StringIO()):
            got = sync.sync_issue(self.client, self.URL, ["SQL-1"], self._sleep)
        self.assertEqual(got, "unchanged")

    def test_non_retryable_patch_failure_is_raised_without_retry(self) -> None:
        self.client.patch_failures = [sync.ApiError(422, "PATCH failed", False)]
        with self.assertRaises(sync.ApiError):
            sync.sync_issue(self.client, self.URL, ["SQL-1"], self._sleep)
        self.assertEqual(self.events, ["GET#5", "PATCH#5"])

    def test_gives_up_after_max_attempts(self) -> None:
        self.client.patch_failures = [sync.ApiError(429, "PATCH failed", True)] * 3
        with redirect_stdout(io.StringIO()), self.assertRaises(sync.ApiError):
            sync.sync_issue(self.client, self.URL, ["SQL-1"], self._sleep, max_attempts=3)
        self.assertEqual(self.events.count("PATCH#5"), 3)
        self.assertEqual(self.events.count("GET#5"), 3)

    def test_patch_backoff_grows_without_rate_limit_headers(self) -> None:
        # Retry-After・x-ratelimit-reset の無い連続失敗では sync_issue の試行回数で
        # 指数バックオフ（60/120/240/480）する
        self.client.patch_failures = [sync.ApiError(503, "PATCH failed", True)] * 4
        with redirect_stdout(io.StringIO()):
            got = sync.sync_issue(self.client, self.URL, ["SQL-1"], self._sleep)
        self.assertEqual(got, "updated")
        self.assertEqual([e for e in self.events if e.startswith("SLEEP")],
                         ["SLEEP60", "SLEEP120", "SLEEP240", "SLEEP480"])

    def test_patch_backoff_is_capped(self) -> None:
        self.client.patch_failures = [sync.ApiError(429, "PATCH failed", True)] * 5
        with redirect_stdout(io.StringIO()):
            got = sync.sync_issue(self.client, self.URL, ["SQL-1"], self._sleep,
                                  max_attempts=6, backoff_cap=300.0)
        self.assertEqual(got, "updated")
        self.assertEqual([e for e in self.events if e.startswith("SLEEP")],
                         ["SLEEP60", "SLEEP120", "SLEEP240", "SLEEP300", "SLEEP300"])

    def test_patch_backoff_honors_retry_after(self) -> None:
        self.client.patch_failures = [
            sync.ApiError(403, "PATCH failed", True, {"retry-after": "30"})]
        with redirect_stdout(io.StringIO()):
            sync.sync_issue(self.client, self.URL, ["SQL-1"], self._sleep)
        self.assertEqual([e for e in self.events if e.startswith("SLEEP")], ["SLEEP30"])


class RunWriteOrderTests(unittest.TestCase):
    def test_write_interval_sleep_precedes_get(self) -> None:
        events: list[str] = []
        client = FakeClient({1: {"state": "open", "body": "a"},
                             2: {"state": "open", "body": "b"},
                             3: {"state": "open", "body": "c"}}, events)
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "map.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(_map({"1": ["SQL-1"], "2": ["SQL-2"], "3": ["SQL-3"]}), f)
            args = argparse.Namespace(map=path, repo="o/r", dry_run=False,
                                      max_writes=10, write_interval=2.5)
            with redirect_stdout(io.StringIO()):
                code = sync.run(args, client, lambda s: events.append(f"SLEEP{s:g}"))
        self.assertEqual(code, 0)
        self.assertEqual(events, ["LIST",
                                  "GET#1", "PATCH#1",
                                  "SLEEP2.5", "GET#2", "PATCH#2",
                                  "SLEEP2.5", "GET#3", "PATCH#3"])
        # GET と PATCH の間に待機が挟まらない
        for i, event in enumerate(events):
            if event.startswith("GET#"):
                self.assertTrue(events[i + 1].startswith("PATCH#"))


class ClientRetryModeTests(unittest.TestCase):
    def _http_error(self) -> urllib.error.HTTPError:
        return urllib.error.HTTPError("https://api.github.com/x", 429, "Too Many Requests",
                                      {"Retry-After": "7"}, io.BytesIO(b""))

    def test_patch_mode_sends_once_and_reports_retry_info(self) -> None:
        sleeps: list[float] = []
        client = sync.GitHubClient("t", sleep=sleeps.append)
        with mock.patch.object(sync.urllib.request, "urlopen",
                               side_effect=self._http_error()) as urlopen:
            with self.assertRaises(sync.ApiError) as ctx:
                client.request("PATCH", "https://api.github.com/x", {"body": "b"},
                               retry=False)
        self.assertEqual(urlopen.call_count, 1)
        self.assertEqual(sleeps, [])
        self.assertTrue(ctx.exception.retryable)
        self.assertEqual(ctx.exception.retry_headers, {"retry-after": "7"})

    def test_get_mode_retries_inside_client(self) -> None:
        sleeps: list[float] = []
        client = sync.GitHubClient("t", max_attempts=3, sleep=sleeps.append)
        with mock.patch.object(sync.urllib.request, "urlopen",
                               side_effect=[self._http_error() for _ in range(3)]) as urlopen:
            with redirect_stdout(io.StringIO()), self.assertRaises(sync.ApiError):
                client.request("GET", "https://api.github.com/x")
        self.assertEqual(urlopen.call_count, 3)
        self.assertEqual(sleeps, [7.0, 7.0])


if __name__ == "__main__":
    unittest.main()
