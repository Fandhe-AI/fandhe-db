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
        self.assertEqual(secs[0].items, ["a", "詳細", "b"])

    def test_fence_ignored(self):
        text = "```\n## 対象外\n- fake\n```\n## 対象外\n- real\n```\n- fenced\n```\n"
        secs = cic.extract_out_of_scope_sections(text)
        self.assertEqual([s.items for s in secs], [["real"]])

    def test_nested_bullets_join(self):
        text = "## 対象外\n- a\n  - sub #5\n  continuation\n- b\n"
        self.assertEqual(
            cic.extract_out_of_scope_sections(text)[0].items, ["a - sub #5 continuation", "b"]
        )

    def test_paragraph_mixed_with_bullets_kept(self):
        text = "## 対象外\n残作業 #600\n\n- 別の作業 #601\n\n後続の段落 #602\n"
        items = cic.extract_out_of_scope_sections(text)[0].items
        self.assertEqual(items, ["残作業 #600", "別の作業 #601", "後続の段落 #602"])

    def test_unindented_continuation_joins_item(self):
        text = "## 対象外\n- 別の作業\n続きの参照 #600\n- 次\n"
        items = cic.extract_out_of_scope_sections(text)[0].items
        self.assertEqual(items, ["別の作業 続きの参照 #600", "次"])

    def test_empty_marker(self):
        for m in ("なし", "特になし", "None", "N/A"):
            self.assertEqual(cic.extract_out_of_scope_sections(f"## 対象外\n- {m}\n")[0].items, [])

    def test_refs(self):
        r = cic.extract_refs
        self.assertEqual(r("x #12 y #12 #7", REPO), [12, 7])
        self.assertEqual(r("other/repo#9 &#123; #<id>.", REPO), [])
        self.assertEqual(r(f"see https://github.com/{REPO}/issues/33", REPO), [33])
        self.assertEqual(r("https://github.com/other/repo/issues/33", REPO), [])
        self.assertEqual(r(f"https://github.com/{REPO.lower()}/issues/600", REPO), [600])
        self.assertEqual(r(f"https://github.com/{REPO.upper()}/pull/601", REPO), [601])
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

    def test_cycle_path_is_real(self):
        self.assertEqual(cic.find_cycles({1: [3], 3: [2], 2: [1]}), [[1, 3, 2]])

    def test_classify_excludes_parent(self):
        infos = {9: cic.Info("issue", "open"), 5: cic.Info("issue", "open")}
        self.assertEqual(cic.classify_item([9], infos, {9}), "closed-only")
        self.assertEqual(cic.classify_item([9, 5], infos, {9}), "ok")

    def test_paragraph_section_is_item(self):
        secs = cic.extract_out_of_scope_sections("## 対象外\n\n残作業あり。\n続き\n\n別段落 #5\n")
        self.assertEqual(secs[0].items, ["残作業あり。 続き", "別段落 #5"])
        secs = cic.extract_out_of_scope_sections("## 対象外\n\nなし\n")
        self.assertEqual(secs[0].items, [])

    def test_fence_kind_and_length(self):
        text = "```\n~~~\n## 対象外\n- fake\n```\n## 対象外\n- real\n"
        secs = cic.extract_out_of_scope_sections(text)
        self.assertEqual([s.items for s in secs], [["real"]])
        text = "````\n```\n## 対象外\n- fake\n````\n## 対象外\n- real\n"
        secs = cic.extract_out_of_scope_sections(text)
        self.assertEqual([s.items for s in secs], [["real"]])

    def test_indented_heading_recognized(self):
        for h in ("  ## 対象外", "   ## 対象外"):
            secs = cic.extract_out_of_scope_sections(f"{h}\n- a #1\n")
            self.assertEqual([s.items for s in secs], [["a #1"]], h)
        # 4 スペース以上はコードブロック扱いで見出しではない
        self.assertEqual(cic.extract_out_of_scope_sections("    ## 対象外\n- a\n"), [])

    def test_qualified_same_repo_ref_normalized(self):
        r = cic.extract_refs
        self.assertEqual(r(f"see {REPO}#600", REPO), [600])
        self.assertEqual(r(f"see {REPO.lower()}#600", REPO), [600])
        self.assertEqual(r(f"{REPO}#5〜#7", REPO), [5, 6, 7])
        self.assertEqual(r(f"other/repo#9 {REPO}x#3 x{REPO}#4", REPO), [])

    def test_deep_heading_with_ref_kept(self):
        text = "## 対象外\n- a #1\n### 残作業 #600\n- b\n### 詳細\n"
        secs = cic.extract_out_of_scope_sections(text)
        self.assertEqual(secs[0].items, ["a #1", "残作業 #600", "b", "詳細"])

    def test_heading_sanitized(self):
        h = cic.sanitize_heading('a\x1b[31m"b' + "x" * 200)
        self.assertNotIn("\x1b", h)
        self.assertLessEqual(len(h), 80)


FOOTER = "\U0001F916 Generated with [Claude Code](https://claude.com/claude-code)"
CURSOR_BLOCK = (
    "<!-- CURSOR_SUMMARY -->\n---\n\n> **Note**\n> synthetic text\n"
    "> <sup>Reviewed by [Cursor Bugbot](https://cursor.com/bugbot) for commit abcdef1. Configure [here](https://www.cursor.com/dashboard?tab=bugbot).</sup>\n"
    "<!-- /CURSOR_SUMMARY -->"
)
BASE = "## 対象外\n\n- a #1\n- b #2\n"
REAL_SIGNATURE = (
    "> <sup>Reviewed by [Cursor Bugbot](https://cursor.com/bugbot) for commit "
    "0123456789abcdef0123456789abcdef01234567. Bugbot is set up for automated code reviews on this repo. "
    "Configure [here](https://www.cursor.com/dashboard/bugbot).</sup>"
)


def items_of(text):
    secs = cic.extract_out_of_scope_sections(text)
    return secs[0].items if secs else []


class AutoSummaryTests(unittest.TestCase):
    def test_auto_summaries_excluded_items_unchanged(self):
        secs = cic.extract_out_of_scope_sections(
            BASE + "\n" + FOOTER + "\n\n" + CURSOR_BLOCK + "\n")
        self.assertEqual(secs[0].items, ["a #1", "b #2"])
        self.assertEqual(secs[0].excluded, {"cursor-summary": 1, "claude-code-footer": 1})

    def test_cursor_missing_end_marker_kept(self):
        text = BASE + CURSOR_BLOCK.replace("<!-- /CURSOR_SUMMARY -->", "") + "\n"
        self.assertGreater(len(items_of(text)), 2)

    def test_cursor_malformed_kept(self):
        variants = {
            "list": CURSOR_BLOCK.replace("> synthetic text", "- remaining work"),
            "heading": CURSOR_BLOCK.replace("> synthetic text", "### remaining"),
            "fence": CURSOR_BLOCK.replace("> synthetic text", "```"),
            "nested": CURSOR_BLOCK.replace("> synthetic text", "<!-- CURSOR_SUMMARY -->"),
            "no-hr": CURSOR_BLOCK.replace("---", "***"),
            "no-reviewed": CURSOR_BLOCK.replace("Reviewed by", "Seen by"),
            "indent": " " + CURSOR_BLOCK,
            "truncated-signature": CURSOR_BLOCK.replace(
                "> synthetic text", "> remaining work"
            ).replace(" for commit abcdef1. Configure [here](https://www.cursor.com/dashboard?tab=bugbot).</sup>", ""),
            "free-text-after-link": CURSOR_BLOCK.replace(
                " for commit abcdef1.", " remaining work #9 for commit abcdef1."),
            "trailing-space": CURSOR_BLOCK.replace("<!-- CURSOR_SUMMARY -->", "<!-- CURSOR_SUMMARY --> "),
        }
        for name, blk in variants.items():
            secs = cic.extract_out_of_scope_sections(BASE + "\n" + blk + "\n")
            self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 0, name)
            self.assertGreater(len(secs[0].items), 2, name)

    def test_cursor_malformed_signature_mixed_with_valid_kept(self):
        # 引用記号直後の空白が二つの崩れた署名が、正常な署名と同一区間に混在しても区間を除外しない
        good = CURSOR_BLOCK.splitlines()[4]
        bad = ">  <sup>Reviewed by [Cursor Bugbot](https://cursor.com/bugbot) remaining work #9"
        lines = CURSOR_BLOCK.splitlines()
        lines.insert(4, bad)
        self.assertIn(good, lines)
        blk = "\n".join(lines)
        secs = cic.extract_out_of_scope_sections(BASE + "\n" + blk + "\n")
        self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 0)
        self.assertIn("#9", " ".join(secs[0].items))

    def test_cursor_signature_missing_bracket_mixed_with_valid_kept(self):
        # 閉じ角括弧が欠けた署名風の行は、正常な署名と同一区間でも区間を除外しない（fail-closed）
        lines = CURSOR_BLOCK.splitlines()
        lines.insert(4, "> <sup>Reviewed by [Cursor Bugbot(https://cursor.com/bugbot) remaining work #9")
        blk = "\n".join(lines)
        secs = cic.extract_out_of_scope_sections(BASE + "\n" + blk + "\n")
        self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 0)
        self.assertIn("#9", " ".join(secs[0].items))

    def test_verbose_reports_both_same_heading_sections_outside_tree(self):
        # ツリー外 PR に同名見出しの対象外節が 2 つあり両方の自動要約を除外したとき、2 つとも EXCLUDED に出る
        sec = "## 対象外\n\n- see #600\n\n" + FOOTER + "\n\n"
        g = FakeGh(bodies={101: (901, body("#700")), 700: (950, sec + sec)},
                   states={700: "CLOSED"}, tree={100: [101], 101: []})
        code, out, _ = run_cli(g, ["100", "--verbose"])
        self.assertEqual(len([l for l in out.splitlines() if l.startswith("EXCLUDED") and "pr #950" in l]), 2, out)

    def test_cursor_quote_mentioning_reviewed_by_with_valid_signature_excluded(self):
        # 正式な署名が別にあるとき、本文の引用行に Reviewed by / <sup> が含まれるだけでは区間を検査対象に戻さない
        lines = CURSOR_BLOCK.splitlines()
        lines.insert(4, "> note: Reviewed by the team, see <sup>1</sup>")
        blk = "\n".join(lines)
        secs = cic.extract_out_of_scope_sections(BASE + "\n" + blk + "\n")
        self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 1)
        self.assertEqual(secs[0].items, ["a #1", "b #2"])

    def test_cursor_real_signature_format_excluded(self):
        # 実データの署名（40 桁の commit・"Bugbot is set up ..." の一文・/dashboard/bugbot）は既定の形として除外する
        lines = CURSOR_BLOCK.splitlines()
        lines[5] = REAL_SIGNATURE
        secs = cic.extract_out_of_scope_sections(BASE + "\n" + "\n".join(lines) + "\n")
        self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 1)
        self.assertEqual(secs[0].items, ["a #1", "b #2"])

    def test_cursor_real_signature_sentence_altered_kept(self):
        # 署名の定型文を改変した行は既定の形と認めず、区間を検査対象に残す（fail-closed）
        lines = CURSOR_BLOCK.splitlines()
        lines[5] = REAL_SIGNATURE.replace("on this repo.", "on this repo, remaining work #9.")
        secs = cic.extract_out_of_scope_sections(BASE + "\n" + "\n".join(lines) + "\n")
        self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 0)
        self.assertIn("#9", " ".join(secs[0].items))

    def test_cursor_unrelated_sup_signature_quote_with_valid_signature_excluded(self):
        # Bugbot の手掛かりを含まない別の署名の引用は、正常な署名と同じ区間にあっても区間を検査対象に戻さない
        for sig in (CURSOR_BLOCK.splitlines()[5], REAL_SIGNATURE):
            for quote in ("> <sup>Reviewed by the documentation team</sup>",
                          ">  <sup> reviewed by QA</sup>"):
                lines = CURSOR_BLOCK.splitlines()
                lines[5] = sig
                lines.insert(4, quote)
                secs = cic.extract_out_of_scope_sections(BASE + "\n" + "\n".join(lines) + "\n")
                self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 1, quote)
                self.assertEqual(secs[0].items, ["a #1", "b #2"], quote)

    def test_cursor_bugbot_hint_mixed_with_valid_kept(self):
        # 表示名・署名用 URL の手掛かりを含むのに既定書式と一致しない引用行は、正常な署名があっても除外しない
        for bad in ("> Reviewed by [Cursor Bugbot](https://cursor.com/bugbot) remaining work #9",
                    "> <sup>see https://cursor.com/bugbot remaining work #9</sup>",
                    "> <sup>Reviewed by Cursor remaining work #9</sup>",
                    "> <sup>Reviewed by [Cursor  Bugbot](https://cursor.com/bugbot) for commit abcdef1. #9</sup>"):
            for sig in (CURSOR_BLOCK.splitlines()[5], REAL_SIGNATURE):
                lines = CURSOR_BLOCK.splitlines()
                lines[5] = sig
                lines.insert(4, bad)
                secs = cic.extract_out_of_scope_sections(BASE + "\n" + "\n".join(lines) + "\n")
                self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 0, bad)
                self.assertIn("#9", " ".join(secs[0].items), bad)

    def test_cursor_body_mentioning_bugbot_with_valid_signature_excluded(self):
        # 要約本文が Bugbot の表示名・URL に触れるだけの引用行は署名候補にせず、正常な署名があれば除外する
        for mention in ("> This PR excludes **Cursor Bugbot** summaries (https://cursor.com/bugbot).",
                        "> no longer treats well-formed Cursor Bugbot blocks as items"):
            for sig in (CURSOR_BLOCK.splitlines()[5], REAL_SIGNATURE):
                lines = CURSOR_BLOCK.splitlines()
                lines[5] = sig
                lines.insert(4, mention)
                secs = cic.extract_out_of_scope_sections(BASE + "\n" + "\n".join(lines) + "\n")
                self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 1, mention)
                self.assertEqual(secs[0].items, ["a #1", "b #2"], mention)

    def test_cursor_nested_inner_start_not_recognized(self):
        # 外側が不成立のとき内側の開始行を新候補にせず、外側の引用行・参照を検査に残す
        inner = CURSOR_BLOCK
        text = (BASE + "\n<!-- CURSOR_SUMMARY -->\n---\n> outer work #77\n" + inner + "\n")
        secs = cic.extract_out_of_scope_sections(text)
        self.assertEqual(secs[0].excluded.get("cursor-summary", 0), 0)
        self.assertIn("#77", " ".join(secs[0].items))

    def test_footer_between_item_and_continuation_keeps_item(self):
        text = "## 対象外\n\n- a\n" + FOOTER + "\ncontinued #5\n"
        self.assertEqual(items_of(text), ["a continued #5"])

    def test_cursor_block_does_not_merge_neighbors(self):
        # 区間の前後に空行が無くても、終了の印直後の記号なし段落は単独項目として残る
        text = BASE.rstrip("\n") + "\n" + CURSOR_BLOCK + "\nremaining work\n"
        self.assertEqual(items_of(text), ["a #1", "b #2", "remaining work"])

    def test_items_after_cursor_block_kept(self):
        text = BASE + "\n" + CURSOR_BLOCK + "\n\n- later #3\n"
        self.assertEqual(items_of(text), ["a #1", "b #2", "later #3"])

    def test_indented_items_after_cursor_block_stay_independent(self):
        # 列頭の項目の後、要約区間を挟んで 2 スペース字下げの受け皿なし項目が続いても結合しない
        text = "## 対象外\n\n- a #1\n" + CURSOR_BLOCK + "\n  - later work\n  - more work\n"
        self.assertEqual(items_of(text), ["a #1", "later work", "more work"])

    def test_fence_closing_inside_cursor_block_not_fail_open(self):
        blk = CURSOR_BLOCK.replace("> synthetic text", "```")
        text = "## 対象外\n\n```\n" + blk + "\n\n- later #3\n"
        # 区間より前で開いた fence は区間内の ``` で閉じる。後続の項目は落とさない
        self.assertIn("later #3", items_of(text))

    def test_footer_lazy_continuation(self):
        self.assertEqual(items_of(BASE + FOOTER + "\n"), ["a #1", "b #2"])

    def test_footer_variants_kept(self):
        for v in (FOOTER.replace("Generated", "Made"), "  " + FOOTER, FOOTER + "\ufe0f"):
            self.assertIn(v.strip(), " ".join(items_of(BASE + "\n" + v + "\n")), v)

    def test_auto_summary_outside_section_noop(self):
        text = BASE + "\n## 次\n\n" + FOOTER + "\n" + CURSOR_BLOCK + "\n"
        secs = cic.extract_out_of_scope_sections(text)
        self.assertEqual(secs[0].items, ["a #1", "b #2"])
        self.assertEqual(secs[0].excluded, {})


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

    def test_verbose_reports_excluded_without_echo(self):
        tail = (FOOTER + "\n" + CURSOR_BLOCK.replace("synthetic text", SENTINEL) + "\n")
        plain = FakeGh(bodies={101: (901, "## 対象外\n\n- see #600\n")}, tree={100: [101], 101: []})
        g = FakeGh(bodies={101: (901, "## 対象外\n\n- see #600\n\n" + tail)}, tree={100: [101], 101: []})
        code, out, err = run_cli(g, ["100", "--verbose"])
        pcode, pout, _ = run_cli(plain, ["100", "--verbose"])
        self.assertIn("EXCLUDED    issue #101 pr #901", out)
        self.assertIn("claude-code-footer=1", out)
        self.assertIn("cursor-summary=1", out)
        self.assertNotIn(SENTINEL, out + err)
        self.assertEqual(code, pcode)
        self.assertEqual([l for l in out.splitlines() if l.startswith("summary:")],
                         [l for l in pout.splitlines() if l.startswith("summary:")])
        _, quiet, _ = run_cli(g, ["100"])
        self.assertNotIn("EXCLUDED", quiet)

    def test_verbose_excluded_dedup_across_outside_nodes(self):
        # ツリー外の 2 Issue が同じ PR に close されても、その PR の除外は EXCLUDED に 1 回だけ出る
        sec = "## 対象外\n\n- see #600\n\n" + FOOTER + "\n"
        g = FakeGh(bodies={101: (901, body("#700 #701")), 700: (950, sec), 701: (950, sec)},
                   states={700: "CLOSED", 701: "CLOSED", 600: "OPEN"}, tree={100: [101], 101: []})
        _, out, _ = run_cli(g, ["100", "--verbose"])
        self.assertEqual(len([l for l in out.splitlines() if l.startswith("EXCLUDED") and "pr #950" in l]), 1, out)

    def test_excluded_log_collected_only_when_verbose(self):
        # 除外ログは --verbose のときだけ集める（通常の実行では items_from_prs に収集先を渡さない）
        sec = "## 対象外\n\n- see #600\n\n" + FOOTER + "\n"
        orig = cic.items_from_prs
        for argv, collected in ((["100"], False), (["100", "--verbose"], True)):
            seen = []

            def spy(*args, **kwargs):
                ex = args[4] if len(args) > 4 else kwargs.get("excluded_out")
                seen.append(ex is not None)
                return orig(*args, **kwargs)

            g = FakeGh(bodies={101: (901, sec + "\n- #700\n"), 700: (950, sec)},
                       states={700: "CLOSED", 600: "OPEN"}, tree={100: [101], 101: []})
            cic.items_from_prs = spy
            try:
                run_cli(g, argv)
            finally:
                cic.items_from_prs = orig
            self.assertTrue(seen, argv)
            if collected:
                self.assertIn(True, seen, argv)
            else:
                self.assertNotIn(True, seen, argv)

    def test_auto_summary_no_false_untracked(self):
        tail = "\n" + FOOTER + "\n\n" + CURSOR_BLOCK + "\n"
        g = FakeGh(bodies={101: (901, "## 対象外\n\n- see #600\n" + tail)}, tree={100: [101], 101: []})
        code, out, _ = run_cli(g, ["100"])
        self.assertEqual(code, 0, out)

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

    def test_shared_closing_pr_keeps_edges(self):
        # PR 901 が #101 と #102 を close し #103 と open の #600 を参照。#103 の PR は #102 を参照
        class Shared(FakeGh):
            def __call__(self, q, v):
                r = super().__call__(q, v)
                if "number:$number" in q and v["number"] == 102:
                    nodes = r["data"]["repository"]["issue"]["closedByPullRequestsReferences"]["nodes"]
                    nodes.append({"number": 901, "merged": True, "body": body("#103 and #600")})
                return r
        g = Shared(
            bodies={101: (901, body("#103 and #600")), 103: (903, body("#102"))},
            states={103: "CLOSED", 600: "OPEN"},
            tree={100: [101, 102, 103], 101: [], 102: [], 103: []},
        )
        code, out, _ = run_cli(g, ["100"])
        self.assertEqual(code, 1, out)
        self.assertIn("CYCLE       #102 -> #103 -> #102", out)

    def test_deep_heading_ref_closed_is_detected(self):
        g = FakeGh(bodies={101: (901, "## 対象外\n### 残作業 #600\n")},
                   states={600: "CLOSED"}, tree={100: [101], 101: []})
        code, out, _ = run_cli(g, ["100"])
        self.assertEqual(code, 1, out)
        self.assertIn("CLOSED-ONLY issue #101", out)

    def test_deep_heading_without_ref_is_untracked(self):
        g = FakeGh(bodies={101: (901, "## 対象外\n### リトライ処理の実装\n")},
                   states={}, tree={100: [101], 101: []})
        code, out, _ = run_cli(g, ["100"])
        self.assertEqual(code, 1, out)
        self.assertIn("UNTRACKED", out)

    def test_qualified_ref_to_open_issue_is_ok(self):
        g = FakeGh(bodies={101: (901, body(f"see {REPO}#600"))},
                   states={600: "OPEN"}, tree={100: [101], 101: []})
        code, out, _ = run_cli(g, ["100"])
        self.assertEqual(code, 0, out)

    def test_parent_only_reference_is_closed_only(self):
        g = FakeGh(bodies={101: (901, body("see #100"))}, states={100: "OPEN"},
                   tree={100: [101], 101: []})
        code, out, _ = run_cli(g, ["100"])
        self.assertEqual(code, 1, out)
        self.assertIn("CLOSED-ONLY issue #101", out)
        self.assertIn("#100(parent,excluded)", out)

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


class ScanLimitTests(unittest.TestCase):
    def test_oversize_file_fails_closed(self):
        with tempfile.TemporaryDirectory() as d:
            rel = "crates/engine/tests/a/main.rs"
            os.makedirs(os.path.join(d, os.path.dirname(rel)))
            with open(os.path.join(d, rel), "w") as f:
                f.write("// x\n" * (cic.MAX_SCAN_FILE_BYTES // 5 + 10) + "// tracked: #500\n")
            args = cic.build_parser().parse_args(["--check-closed", "--root", d])
            out, err = io.StringIO(), io.StringIO()
            code = cic.run(args, FakeGh(), lambda r: [rel], out, err)
        self.assertEqual(code, 3)
        self.assertIn("scan size limit", err.getvalue())


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
