"""scripts/check_issue_closeout.py の単体試験（Issue #3168）。

本文解析・参照抽出・分類・循環検出・取得層の fail-closed・`--check-closed` を、
偽の gh runner（ネットワーク不使用）と合成した PR 本文で検証する。fixture は
テスト内の文字列だけで、実在 PR 本文のコピーは使わない。
"""

import io
import os
import re
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import check_issue_closeout as cic  # noqa: E402

SENTINEL = "SENTINEL-DO-NOT-ECHO-3168"
REPO = "Fandhe-AI/fandhe-db"


def body(*items, heading="## 対象外（out-of-scope）"):
    return "## Summary\n\n- x\n\n" + heading + "\n\n" + "\n".join(f"- {i}" for i in items) + "\n"


class ParseTests(unittest.TestCase):
    def test_heading_variants(self):
        for h in ("## 対象外（out-of-scope）", "## 対象外", "### Out of scope"):
            secs = cic.extract_out_of_scope_sections(body("a #1", heading=h))
            self.assertEqual(len(secs), 1, h)
            self.assertEqual(secs[0].items, ["a #1"])

    def test_nested_heading_and_same_level_end(self):
        text = "## 対象外\n- a\n### 詳細\n- b\n## 次\n- c\n"
        secs = cic.extract_out_of_scope_sections(text)
        self.assertEqual(secs[0].items, ["a", "b"])

    def test_fence_ignored(self):
        text = "```\n## 対象外\n- fake\n```\n## 対象外\n- real\n```\n- fenced\n```\n"
        secs = cic.extract_out_of_scope_sections(text)
        self.assertEqual([s.items for s in secs], [["real"]])

    def test_nested_bullets_join(self):
        text = "## 対象外\n- a\n  - sub #5\n  continuation\n- b\n"
        self.assertEqual(
            cic.extract_out_of_scope_sections(text)[0].items, ["a - sub #5 continuation", "b"]
        )

    def test_empty_marker(self):
        for m in ("なし", "特になし", "None", "N/A"):
            self.assertEqual(cic.extract_out_of_scope_sections(f"## 対象外\n- {m}\n")[0].items, [])

    def test_refs(self):
        r = cic.extract_refs
        self.assertEqual(r("x #12 y #12 #7", REPO), [12, 7])
        self.assertEqual(r("other/repo#9 &#123; #<id>.", REPO), [])
        self.assertEqual(r(f"see https://github.com/{REPO}/issues/33", REPO), [33])
        self.assertEqual(r("https://github.com/other/repo/issues/33", REPO), [])
        self.assertEqual(r("#1698〜#1700", REPO), [1698, 1699, 1700])
        self.assertEqual(r("#1〜#100", REPO), [1, 100])
        self.assertEqual(r("#5 #6", REPO, {5}), [6])
        self.assertEqual(r("#1757に委ねる", REPO), [1757])

    def test_classify(self):
        infos = {
            1: cic.Info("issue", "closed"),
            2: cic.Info("issue", "open"),
            3: cic.Info("pr", "closed"),
            4: cic.Info("missing"),
        }
        self.assertEqual(cic.classify_item([], infos), "untracked")
        self.assertEqual(cic.classify_item([1], infos), "closed-only")
        self.assertEqual(cic.classify_item([3], infos), "closed-only")
        self.assertEqual(cic.classify_item([4], infos), "closed-only")
        self.assertEqual(cic.classify_item([1, 2], infos), "ok")

    def test_cycles(self):
        self.assertEqual(cic.find_cycles({1: [2], 2: [1]}), [[1, 2]])
        self.assertEqual(cic.find_cycles({1: [2], 2: [3], 3: [1]}), [[1, 2, 3]])
        self.assertEqual(cic.find_cycles({1: [2], 2: [3]}), [])
        self.assertEqual(cic.find_cycles({1: [1]}), [])

    def test_heading_sanitized(self):
        h = cic.sanitize_heading('a\x1b[31m"b' + "x" * 200)
        self.assertNotIn("\x1b", h)
        self.assertLessEqual(len(h), 80)


class FakeGh:
    """parent=100 の子 101..104。101 untracked、102 closed のみ、103/104 が循環、それぞれ PR を持つ。"""

    def __init__(self, bodies=None, states=None, tree=None):
        self.tree = tree or {100: [101, 102, 103, 104], 101: [], 102: [], 103: [], 104: []}
        self.bodies = bodies or {
            101: (901, body(f"nothing here {SENTINEL}")),
            102: (902, body(f"see #500 {SENTINEL}")),
            103: (903, body("hand over to #104")),
            104: (904, body("hand over to #103 and #600")),
        }
        self.states = states or {500: "CLOSED", 600: "OPEN", 103: "CLOSED", 104: "CLOSED"}
        self.calls = 0

    def __call__(self, query, variables):
        self.calls += 1
        if "number:$number" in query:
            n = variables["number"]
            pr = self.bodies.get(n)
            return {"data": {"repository": {"issue": {
                "number": n, "state": "CLOSED" if n != 100 else "OPEN",
                "subIssues": {"pageInfo": {"hasNextPage": False}, "nodes": [
                    {"number": c, "state": "CLOSED"} for c in self.tree.get(n, [])]},
                "closedByPullRequestsReferences": {"pageInfo": {"hasNextPage": False},
                    "nodes": [{"number": pr[0], "merged": True, "body": pr[1]}] if pr else []},
            }}}}
        nums = [int(x) for x in re.findall(r"n(\d+): issueOrPullRequest", query)]
        data, errors = {}, []
        for n in nums:
            st = self.states.get(n)
            if st is None:
                data[f"n{n}"] = None
                errors.append({"type": "NOT_FOUND", "path": [f"n{n}"]})
            else:
                data[f"n{n}"] = {"__typename": "Issue", "state": st}
        resp = {"data": {"repository": data}}
        if errors:
            resp["errors"] = errors
        return resp


def run_cli(runner, argv, git_files=()):
    args = cic.build_parser().parse_args(argv)
    out, err = io.StringIO(), io.StringIO()
    code = cic.run(args, runner, lambda root: list(git_files), out, err)
    return code, out.getvalue(), err.getvalue()


class IntegrationTests(unittest.TestCase):
    def test_classification_and_no_echo(self):
        code, out, err = run_cli(FakeGh(), ["100"])
        self.assertEqual(code, 1)
        self.assertIn("UNTRACKED   issue #101 pr #901", out)
        self.assertIn("CLOSED-ONLY issue #102 pr #902", out)
        self.assertIn("#500(issue,closed)", out)
        self.assertIn("CYCLE       #103 -> #104 -> #103", out)
        self.assertIn("untracked=1 closed-only=2 cycles=1 ok=1", out)
        self.assertNotIn(SENTINEL, out + err)

    def test_clean_tree(self):
        g = FakeGh(bodies={101: (901, body("see #600"))}, tree={100: [101], 101: []})
        code, out, _ = run_cli(g, ["100"])
        self.assertEqual(code, 0, out)

    def test_external_hop_cycle(self):
        # 子 101 -> 外部 closed issue 700 -> 101（700 の PR 本文）
        g = FakeGh(bodies={101: (901, body("#700")), 700: (950, body("#101"))},
                   states={700: "CLOSED"}, tree={100: [101], 101: []})
        code, out, _ = run_cli(g, ["100"])
        self.assertIn("CYCLE       #101 -> #700 -> #101", out)
        code, out, _ = run_cli(g, ["100", "--max-hops", "0"])
        self.assertNotIn("CYCLE", out)

    def test_fetch_failures_fail_closed(self):
        def boom(q, v):
            raise cic.FetchError("gh api failed")
        code, _, err = run_cli(boom, ["100"])
        self.assertEqual(code, 3)
        self.assertIn("error:", err)

        class Truncated(FakeGh):
            def __call__(self, q, v):
                r = super().__call__(q, v)
                if "number:$number" in q:
                    r["data"]["repository"]["issue"]["closedByPullRequestsReferences"][
                        "pageInfo"]["hasNextPage"] = True
                return r
        self.assertEqual(run_cli(Truncated(), ["100"])[0], 3)
        self.assertEqual(run_cli(FakeGh(), ["100", "--max-nodes", "2"])[0], 3)

        class OtherError(FakeGh):
            def __call__(self, q, v):
                r = super().__call__(q, v)
                if "issueOrPullRequest" in q:
                    r["errors"] = [{"type": "FORBIDDEN", "path": ["n500"]}]
                return r
        self.assertEqual(run_cli(OtherError(), ["100"])[0], 3)

    def test_check_closed(self):
        with tempfile.TemporaryDirectory() as d:
            rel = "crates/engine/tests/a/main.rs"
            os.makedirs(os.path.join(d, os.path.dirname(rel)))
            with open(os.path.join(d, rel), "w") as f:
                f.write("// tracked: #500\n/// tracked: #600, #500\n// tracked: 500\n"
                        "// tracked: #500 (x)\n// tracked: #999\n")
            g = FakeGh(states={500: "CLOSED", 600: "OPEN"})
            args = cic.build_parser().parse_args(["--check-closed", "--root", d])
            out, err = io.StringIO(), io.StringIO()
            code = cic.run(args, g, lambda r: [rel, "README.md"], out, err)
        o = out.getvalue()
        self.assertEqual(code, 1)
        self.assertIn(f"TRACKED-CLOSED {rel}:1 #500", o)
        self.assertIn(f"TRACKED-CLOSED {rel}:2 #500", o)
        self.assertIn(f"TRACKED-NOT-ISSUE {rel}:5 #999 (missing)", o)
        self.assertNotIn("#600", o)
        self.assertIn("annotations=4", o)


class UsageTests(unittest.TestCase):
    def usage_code(self, argv):
        err = io.StringIO()
        old = sys.stderr
        sys.stderr = err
        try:
            cic.main(argv)
        except SystemExit as exc:
            return exc.code
        finally:
            sys.stderr = old
        return None

    def test_bad_usage(self):
        for argv in ([], ["0"], ["abc"], ["1", "--repo", "bad repo"], ["1", "--max-hops", "9"]):
            self.assertEqual(self.usage_code(argv), 2, argv)

    def test_check_closed_alone_parses(self):
        args = cic.build_parser().parse_args(["--check-closed"])
        self.assertIsNone(args.parent)


if __name__ == "__main__":
    unittest.main()
