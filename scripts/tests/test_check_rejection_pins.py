"""check_rejection_pins.py / check_rejection_pins.sh の単体テスト（Issue #3164）。

規則は docs/design/rejection-pin-annotation.md（D2〜D9）。fixture はコミットせず、
テスト内の文字列と difflib で生成する（半角スペース 1 文字だけの context 行が
.editorconfig の trim_trailing_whitespace に触れるため）。
実行: python3 -m unittest discover scripts/tests（make scripts-test）。外部通信なし。
"""

from __future__ import annotations

import difflib
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(SCRIPTS))

import check_rejection_pins as crp  # noqa: E402

P = "crates/engine/tests/t.rs"
ALL_DOCS = {"docs/design/x.md", "docs/design/rejection-pin-annotation.md"}


def make_diff(path, old, new, context=0):
    """old/new（None なら新規・削除）から unified diff を生成する。"""
    a = (old or "").split("\n") if old is not None else []
    b = (new or "").split("\n") if new is not None else []
    body = list(
        difflib.unified_diff(a, b, f"a/{path}", f"b/{path}", n=context, lineterm="")
    )
    head = [f"diff --git a/{path} b/{path}"]
    if old is None:
        head.append("new file mode 100644")
        body[0] = "--- /dev/null"
    return "\n".join(head + body) + "\n"


def run_check(path, old, new, docs=ALL_DOCS, context=0):
    diff = make_diff(path, old, new, context)
    files = crp.parse_diff(diff)
    heads = {path: new} if new is not None else {}
    bases = {path: old} if old is not None else {}
    viol, _, _ = crp.check(
        files, heads.get, bases.get, lambda p: p in docs
    )
    return viol


def kinds(viol):
    return sorted(v.kind for v in viol)


REJECT_BODY = """#[test]
fn create_in_tenant_schema_is_unsupported() {
    assert_eq!(err.wire_code(), "0A000", "{sql}");
}
"""


class ParseDiffTest(unittest.TestCase):
    def test_context_and_u0_agree(self):
        old = "a\nb\nc\nd\ne\nf\n"
        new = "a\nb\nc\nX\ne\nf\nY\n"
        d0 = crp.parse_diff(make_diff(P, old, new, 0))[0]
        d3 = crp.parse_diff(make_diff(P, old, new, 3))[0]
        self.assertEqual(d0.added, d3.added)
        self.assertEqual(d0.deleted, d3.deleted)
        self.assertEqual(d0.added, {4, 7})
        self.assertEqual(d0.deleted, {4})

    def test_rename(self):
        text = (
            "diff --git a/crates/a/src/o.rs b/crates/a/src/n.rs\n"
            "similarity index 90%\nrename from crates/a/src/o.rs\nrename to crates/a/src/n.rs\n"
            "--- a/crates/a/src/o.rs\n+++ b/crates/a/src/n.rs\n"
            "@@ -2 +2 @@\n-x\n+y\n"
        )
        fd = crp.parse_diff(text)[0]
        self.assertEqual(fd.old_path, "crates/a/src/o.rs")
        self.assertEqual(fd.new_path, "crates/a/src/n.rs")
        self.assertEqual(fd.added, {2})

    def test_new_and_deleted_file(self):
        fd = crp.parse_diff(make_diff(P, None, "a\nb\n"))[0]
        self.assertIsNone(fd.old_path)
        self.assertEqual(fd.added, {1, 2, 3})
        text = (
            f"diff --git a/{P} b/{P}\ndeleted file mode 100644\n"
            f"--- a/{P}\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n"
        )
        fd = crp.parse_diff(text)[0]
        self.assertIsNone(fd.new_path)
        self.assertEqual(fd.deleted, {1, 2})

    def test_hunk_body_starting_with_plus_markers(self):
        text = f"diff --git a/{P} b/{P}\n--- a/{P}\n+++ b/{P}\n@@ -0,0 +1,2 @@\n++++ x\n+y\n"
        self.assertEqual(crp.parse_diff(text)[0].added, {1, 2})

    def test_quoted_path_is_error(self):
        with self.assertRaises(crp.DiffParseError):
            crp.parse_diff('diff --git "a/x y.rs" "b/x y.rs"\n')

    def test_bad_hunk_header_is_error(self):
        with self.assertRaises(crp.DiffParseError):
            crp.parse_diff(f"diff --git a/{P} b/{P}\n@@ garbage @@\n")


class FindTestsTest(unittest.TestCase):
    def test_annotation_above_and_below_attr(self):
        for src in (
            "// tracked: #1\n#[test]\nfn a() {\n    x();\n}\n",
            "#[test]\n// tracked: #1\nfn a() {\n    x();\n}\n",
            "/// tracked: #1\n#[test]\nfn a() {\n    x();\n}\n",
        ):
            (t,) = crp.find_tests(src)
            self.assertEqual(t.name, "a")
            self.assertEqual(t.end, 5)
            self.assertTrue(any("tracked" in l for _, l in t.block))

    def test_blank_line_breaks_block(self):
        (t,) = crp.find_tests("// tracked: #1\n\n#[test]\nfn a() {\n}\n")
        self.assertFalse(any("tracked" in l for _, l in t.block))
        self.assertEqual(t.start, 3)

    def test_multiline_attr(self):
        src = '#[test]\n#[should_panic(\n    expected = "x"\n)]\nfn a() {\n}\n'
        (t,) = crp.find_tests(src)
        self.assertEqual(t.start, 1)
        self.assertEqual(t.fn_line, 5)

    def test_one_line_fn_and_unterminated(self):
        (t,) = crp.find_tests("#[test]\nfn a() {}\n")
        self.assertEqual(t.end, 2)
        (t,) = crp.find_tests("#[test]\nfn a() {\n    x();\n")
        self.assertIsNone(t.end)

    def test_format_braces_do_not_break_range(self):
        src = '#[test]\nfn a() {\n    f("{sql} {");\n}\n#[test]\nfn b() {\n}\n'
        a, b = crp.find_tests(src)
        self.assertEqual((a.end, b.fn_line), (4, 6))

    def test_indented_in_mod(self):
        src = "mod t {\n    #[test]\n    fn a() {\n        x();\n    }\n}\n"
        (t,) = crp.find_tests(src)
        self.assertEqual((t.start, t.end), (2, 5))

    def test_non_test_fn_ignored(self):
        self.assertEqual(crp.find_tests("fn a() {\n}\n"), [])

    def test_bracket_in_attr_string_does_not_swallow_fn(self):
        src = '#[test]\n#[should_panic(expected = "[")]\nfn a() {\n}\n'
        (t,) = crp.find_tests(src)
        self.assertEqual((t.name, t.fn_line, t.end), ("a", 3, 4))
        src = '#[test]\n#[should_panic(expected = r#"]["#)]\nfn a() {\n}\n'
        (t,) = crp.find_tests(src)
        self.assertEqual(t.fn_line, 3)
        src = '#[test]\n#[should_panic(/* [ */ expected = "x")] // [\nfn a() {\n}\n'
        (t,) = crp.find_tests(src)
        self.assertEqual(t.fn_line, 3)

    def test_unterminated_attr_is_error(self):
        with self.assertRaises(crp.DiffParseError):
            crp.find_tests('#[test]\n#[should_panic(expected = "x)]\nfn a() {\n}\n')
        with self.assertRaises(crp.DiffParseError):
            crp.find_tests("#[test]\n#[cfg(\nfn a() {\n}\n")

    def test_bracket_attr_pin_is_detected(self):
        body = '#[test]\n#[should_panic(expected = "[")]\nfn a() {\n    f("0A000");\n}\n'
        v = run_check(P, "fn base() {\n}\n", "fn base() {\n}\n" + body)
        self.assertEqual(kinds(v), ["missing-annotation"])

    def test_test_attr_with_trailing_comment(self):
        (t,) = crp.find_tests("#[test] // regression\nfn a() {\n}\n")
        self.assertEqual(t.name, "a")
        body = '#[test] // regression\nfn a() {\n    f("0A000");\n}\n'
        v = run_check(P, "fn base() {\n}\n", "fn base() {\n}\n" + body)
        self.assertEqual(kinds(v), ["missing-annotation"])


class CheckTest(unittest.TestCase):
    def added(self, new, **kw):
        return run_check(P, "fn base() {\n}\n", "fn base() {\n}\n" + new, **kw)

    def test_missing_annotation(self):
        v = self.added(REJECT_BODY)
        self.assertEqual(kinds(v), ["missing-annotation"])
        self.assertEqual(v[0].test, "create_in_tenant_schema_is_unsupported")
        self.assertEqual(v[0].path, P)
        self.assertIn(P, v[0].render())

    def test_malformed_forms(self):
        for bad in (
            "// Tracked: #3171",
            "// tracked: 3171",
            "// tracked: #3171 (x)",
            "// tracked #3171",
            "// tracked: #3171,#3172",
            "// intentional: docs/spec/anything.md",
            "// intentionally rejected",
        ):
            v = self.added(bad + "\n" + REJECT_BODY)
            self.assertIn("malformed-annotation", kinds(v), bad)

    def test_missing_design_doc(self):
        v = self.added("// intentional: docs/design/nope.md\n" + REJECT_BODY)
        self.assertEqual(kinds(v), ["missing-design-doc"])

    def test_disallowed_design_doc(self):
        v = self.added(
            "// intentional: docs/design/implementation-status.md\n" + REJECT_BODY,
            docs=ALL_DOCS | {"docs/design/implementation-status.md"},
        )
        self.assertEqual(kinds(v), ["disallowed-design-doc"])

    def test_valid_forms(self):
        for ann in (
            "// tracked: #3171",
            "/// tracked: #3183, #3189",
            "// intentional: docs/design/x.md",
            "// intentional: docs/design/x.md, docs/design/rejection-pin-annotation.md",
            "// tracked: #1\n// intentional: docs/design/x.md",
            "// intentional: docs/design/rejection-pin-annotation.md",
        ):
            self.assertEqual(self.added(ann + "\n" + REJECT_BODY), [], ann)

    def test_duplicate_kind(self):
        v = self.added("// tracked: #1\n// tracked: #2\n" + REJECT_BODY)
        self.assertEqual(kinds(v), ["duplicate-annotation"])

    def test_annotation_below_attr_valid(self):
        body = "#[test]\n// tracked: #1\nfn a_unsupported() {\n}\n"
        self.assertEqual(self.added(body), [])

    def test_unrelated_change_ok(self):
        body = "#[test]\nfn plain() {\n    assert_eq!(1, 1);\n}\n"
        self.assertEqual(self.added(body), [])

    def test_vocabulary_forms(self):
        bodies = [
            "#[test]\nfn x_is_unsupported() {\n}\n",
            '#[test]\n#[should_panic(expected = "not supported")]\nfn x() {\n}\n',
            "#[test]\nfn x() {\n    matches!(e, Error::FeatureNotSupported);\n}\n",
            "#[test]\nfn x() {\n    expect_0a000(s);\n}\n",
        ]
        for b in bodies:
            self.assertEqual(kinds(self.added(b)), ["missing-annotation"], b)

    def test_comment_only_vocabulary_not_a_pin(self):
        body = "#[test]\nfn x() {\n    // 0A000 is not supported elsewhere\n    ok();\n}\n"
        self.assertEqual(self.added(body), [])

    def test_removed_annotation_only_is_caught(self):
        old = "// tracked: #1\n" + REJECT_BODY
        new = REJECT_BODY
        for ctx in (0, 3):
            v = run_check(P, old, new, context=ctx)
            self.assertEqual(kinds(v), ["missing-annotation"])

    def test_test_deletion_is_ignored(self):
        old = "// tracked: #1\n" + REJECT_BODY + "fn keep() {\n}\n"
        new = "fn keep() {\n}\n"
        self.assertEqual(run_check(P, old, new), [])

    def test_pure_move_has_no_target(self):
        text = (
            "diff --git a/crates/a/src/o.rs b/crates/a/src/n.rs\n"
            "similarity index 100%\nrename from crates/a/src/o.rs\nrename to crates/a/src/n.rs\n"
        )
        files = crp.parse_diff(text)
        v, n, _ = crp.check(files, {"crates/a/src/n.rs": REJECT_BODY}.get, lambda p: None, lambda p: True)
        self.assertEqual((v, n), ([], 0))

    def test_format_check_without_detection(self):
        body = "// tracked: 12\n#[test]\nfn plain() {\n    assert_eq!(err.wire_code(), \"42601\");\n}\n"
        self.assertEqual(kinds(self.added(body)), ["malformed-annotation"])
        ok = "// tracked: #12\n#[test]\nfn plain() {\n    assert_eq!(err.wire_code(), \"42601\");\n}\n"
        self.assertEqual(self.added(ok), [])

    def test_malformed_inside_body_is_caught(self):
        body = "#[test]\nfn plain() {\n    // tracked: oops\n    ok();\n}\n"
        self.assertEqual(kinds(self.added(body)), ["malformed-annotation"])

    def test_non_target_paths_ignored(self):
        for p in ("crates/engine/benches/x.rs", "scripts/foo.rs", "crates/a/b/src/x.rs"):
            self.assertEqual(run_check(p, "x\n", "x\n" + REJECT_BODY), [], p)

    def test_src_path_is_target(self):
        v = run_check("crates/engine/src/x.rs", "x\n", "x\n" + REJECT_BODY)
        self.assertEqual(kinds(v), ["missing-annotation"])

    def test_unterminated_is_violation(self):
        v = self.added("#[test]\nfn a_unsupported() {\n    x();\n")
        self.assertEqual(kinds(v), ["unterminated-test"])


GIT_ENV = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
GIT_FLAGS = [
    "-c", "user.name=test", "-c", "user.email=test@example.invalid",
    "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false",
]


class SameNameModuleTest(unittest.TestCase):
    """同名テストが別モジュールにあっても、削除は同じモジュールのものにだけ作用する（D5）。"""

    SRC = (
        "mod a {\n    #[test]\n    fn reject_unsupported() {\n        let x = 1;\n    }\n}\n"
        "mod b {\n    #[test]\n    fn reject_unsupported() {\n        assert_eq!(err.wire_code(), \"0A000\");\n    }\n}\n"
    )

    def test_module_path_recorded(self):
        ts = crp.find_tests(self.SRC)
        self.assertEqual([(t.module, t.name) for t in ts],
                         [(("a",), "reject_unsupported"), (("b",), "reject_unsupported")])

    def test_deleting_a_does_not_target_b(self):
        new = self.SRC.replace(
            "mod a {\n    #[test]\n    fn reject_unsupported() {\n        let x = 1;\n    }\n}\n", "")
        viol = run_check(P, self.SRC, new)
        self.assertEqual(viol, [])

    def test_parent_mod_rename_with_annotation_removal_is_caught(self):
        old = "mod a {\n    // tracked: #1\n    #[test]\n    fn reject_x() {\n        assert_eq!(err.wire_code(), \"0A000\");\n    }\n}\n"
        new = "mod c {\n    #[test]\n    fn reject_x() {\n        assert_eq!(err.wire_code(), \"0A000\");\n    }\n}\n"
        for ctx in (0, 3):
            self.assertEqual(kinds(run_check(P, old, new, context=ctx)), ["missing-annotation"])

    def test_parent_mod_rename_does_not_target_untouched_sibling(self):
        body = "        assert_eq!(err.wire_code(), \"0A000\");\n"
        def mod(top, ann):
            return (f"mod {top} {{\n"
                    f"    mod p {{\n{ann}        #[test]\n        fn reject_x() {{\n{body}        }}\n    }}\n"
                    f"    mod q {{\n        // tracked: #2\n        #[test]\n        fn reject_x() {{\n{body}        }}\n    }}\n"
                    "}\n")
        old = mod("a", "        // tracked: #1\n")
        new = mod("c", "")
        v = run_check(P, old, new, context=0)
        self.assertEqual(kinds(v), ["missing-annotation"])
        self.assertEqual(len(v), 1)

    def test_deleting_b_targets_nothing_in_a(self):
        # b を削除しても a（未変更・拒否固定でもない）は対象にならない
        new = self.SRC[: self.SRC.index("mod b")]
        self.assertEqual(run_check(P, self.SRC, new), [])


class EndToEndTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name)
        (self.repo / "scripts").mkdir()
        for n in ("check_rejection_pins.sh", "lib.sh", "check_rejection_pins.py"):
            shutil.copy(SCRIPTS / n, self.repo / "scripts" / n)
        (self.repo / "docs/design").mkdir(parents=True)
        (self.repo / "docs/design/x.md").write_text("# x\n")
        (self.repo / "crates/engine/tests").mkdir(parents=True)
        self.test_file = self.repo / "crates/engine/tests/t.rs"
        self.test_file.write_text("fn base() {\n}\n")
        self.git("init", "-q")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("checkout", "-q", "-b", "topic")

    def git(self, *a):
        subprocess.run(["git", *GIT_FLAGS, "-C", str(self.repo), *a], check=True,
                       env=GIT_ENV, capture_output=True)

    def commit(self, text):
        self.test_file.write_text("fn base() {\n}\n" + text)
        self.git("add", ".")
        self.git("commit", "-q", "-m", "change")

    def run_sh(self, *args):
        return subprocess.run(
            ["bash", str(self.repo / "scripts/check_rejection_pins.sh"), *args],
            capture_output=True, text=True, env=GIT_ENV,
        )

    def test_missing_annotation_fails(self):
        self.commit(REJECT_BODY)
        r = self.run_sh("main")
        self.assertEqual(r.returncode, 1, r.stderr)
        self.assertIn("create_in_tenant_schema_is_unsupported", r.stdout)
        self.assertIn("crates/engine/tests/t.rs", r.stdout)

    def test_valid_annotation_passes(self):
        self.commit("// tracked: #3171\n" + REJECT_BODY)
        self.assertEqual(self.run_sh("main").returncode, 0)

    def test_unrelated_change_passes(self):
        self.commit("#[test]\nfn plain() {\n    assert!(true);\n}\n")
        self.assertEqual(self.run_sh("main").returncode, 0)

    def test_missing_design_doc_fails(self):
        self.commit("// intentional: docs/design/nope.md\n" + REJECT_BODY)
        self.assertEqual(self.run_sh("main").returncode, 1)

    def test_existing_design_doc_passes(self):
        self.commit("// intentional: docs/design/x.md\n" + REJECT_BODY)
        self.assertEqual(self.run_sh("main").returncode, 0)

    def test_usage_errors(self):
        r = self.run_sh()
        self.assertEqual(r.returncode, 2)
        self.assertIn("usage", r.stderr)
        self.assertEqual(self.run_sh("a", "b").returncode, 2)
        self.assertEqual(self.run_sh("-x").returncode, 2)
        r = self.run_sh("no-such-ref")
        self.assertEqual(r.returncode, 2)
        self.assertIn("unknown base ref", r.stderr)


class TrailingCommentModTest(unittest.TestCase):
    """末尾コメント付き `mod x {` も識別し、同名テストの取り違えを防ぐ。"""

    def test_mod_with_trailing_comment_recorded(self):
        src = (
            "mod a { // group a\n    #[test]\n    fn t() {\n    }\n}\n"
            "mod b { /* group b */\n    #[test]\n    fn t() {\n    }\n}\n"
        )
        self.assertEqual([t.module for t in crp.find_tests(src)], [("a",), ("b",)])

    def test_deleting_a_does_not_target_b(self):
        body = "        assert_eq!(err.wire_code(), \"0A000\");\n"
        a = "mod a { // group a\n    // tracked: #1\n    #[test]\n    fn reject_x() {\n" + body + "    }\n}\n"
        b = "mod b { // group b\n    #[test]\n    fn reject_x() {\n" + body + "    }\n}\n"
        self.assertEqual(run_check(P, a + b, b), [])


class DetachedAnnotationTest(unittest.TestCase):
    """注記とテストの間への空行挿入・削除をテストの変更として扱う（D5）。"""

    ANN = "// tracked: #1\n"
    FN = "#[test]\nfn reject_x() {\n    assert_eq!(err.wire_code(), \"0A000\");\n}\n"

    def test_blank_line_insertion_detected(self):
        old = self.ANN + self.FN
        new = self.ANN + "\n" + self.FN
        for ctx in (0, 3):
            self.assertEqual(kinds(run_check(P, old, new, context=ctx)), ["missing-annotation"])

    def test_blank_line_removal_not_flagged_when_reattached(self):
        old = self.ANN + "\n" + self.FN
        new = self.ANN + self.FN
        self.assertEqual(run_check(P, old, new), [])

    def test_unrelated_header_comment_edit_not_flagged(self):
        # 空行で隔てられたファイル先頭コメントだけの変更は未変更テストを対象にしない
        old = "// module overview\n\n" + self.FN
        new = "// module overview (edited)\n\n" + self.FN
        for ctx in (0, 3):
            self.assertEqual(run_check(P, old, new, context=ctx), [])

    def test_detaching_existing_annotation_still_detected_with_header(self):
        old = "// module overview\n\n" + self.ANN + self.FN
        new = "// module overview\n\n" + self.ANN + "\n" + self.FN
        self.assertEqual(kinds(run_check(P, old, new)), ["missing-annotation"])


class BlankBetweenAttrAndFnTest(unittest.TestCase):
    """`#[test]` と fn の間の空行・ブロックコメントでテスト検出が外れない（D3 と D4 の分離）。"""

    BODY = "fn reject_x() {\n    assert_eq!(err.wire_code(), \"0A000\");\n}\n"

    def test_blank_line_between_attr_and_fn_still_detected(self):
        src = "#[test]\n\n" + self.BODY
        self.assertEqual([t.name for t in crp.find_tests(src)], ["reject_x"])
        self.assertEqual(kinds(run_check(P, None, src)), ["missing-annotation"])

    def test_block_comment_between_attr_and_fn_still_detected(self):
        src = "#[test]\n/* note\n   more */\n" + self.BODY
        self.assertEqual(kinds(run_check(P, None, src)), ["missing-annotation"])

    def test_annotation_before_blank_is_not_contiguous(self):
        src = "// tracked: #1\n#[test]\n\n" + self.BODY
        self.assertEqual(kinds(run_check(P, None, src)), ["missing-annotation"])

    def test_attr_then_non_fn_code_resets(self):
        src = "#[test]\nstruct S;\n" + self.BODY
        self.assertEqual(crp.find_tests(src), [])


class AmbiguousRenameTest(unittest.TestCase):
    """同名テストを持つ複数モジュールのリネームで対応を確定できなければ検査不能で失敗（D5）。"""

    BODY = "        assert_eq!(err.wire_code(), \"0A000\");\n"

    def mod(self, name, ann):
        return (f"mod {name} {{\n{ann}    #[test]\n    fn reject_x() {{\n{self.BODY}    }}\n}}\n")

    def test_tie_fails_closed(self):
        old = self.mod("a", "    // tracked: #1\n") + self.mod("b", "    // tracked: #2\n")
        new = self.mod("c", "") + self.mod("d", "    // tracked: #2\n")
        for ctx in (0, 3):
            with self.assertRaises(crp.DiffParseError):
                run_check(P, old, new, context=ctx)

    def test_unique_body_disambiguates(self):
        other = "        let y = 2;\n" + self.BODY
        def m(name, ann, body):
            return f"mod {name} {{\n{ann}    #[test]\n    fn reject_x() {{\n{body}    }}\n}}\n"
        old = m("a", "    // tracked: #1\n", self.BODY) + m("b", "    // tracked: #2\n", other)
        new = m("c", "", self.BODY) + m("d", "    // tracked: #2\n", other)
        self.assertEqual(kinds(run_check(P, old, new)), ["missing-annotation"])


if __name__ == "__main__":
    unittest.main()
