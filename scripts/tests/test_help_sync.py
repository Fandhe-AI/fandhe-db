"""Makefile の `## ` ターゲット一覧と scripts/help.sh の一覧が一致することを検査する。

ターゲットを追加・改名・削除したのに scripts/help.sh を更新し忘れる事故
（Make 非依存の一覧の陳腐化）を `make scripts-test`（CI の scripts-test ジョブ）で検知する。
標準ライブラリのみ・cargo 不要。
"""

import re
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def makefile_targets():
    pattern = re.compile(r"^([a-zA-Z0-9_-]+):.*## ", re.MULTILINE)
    text = (ROOT / "Makefile").read_text(encoding="utf-8")
    return [t for t in pattern.findall(text) if t != "help"]


def help_sh_targets():
    out = subprocess.run(
        [str(ROOT / "scripts" / "help.sh")],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return re.findall(r"^  ([a-zA-Z0-9_-]+)$", out, re.MULTILINE)


class HelpSyncTest(unittest.TestCase):
    def test_help_sh_lists_every_makefile_target(self):
        self.assertEqual(sorted(makefile_targets()), sorted(help_sh_targets()))

    def test_help_sh_has_no_duplicate_entries(self):
        targets = help_sh_targets()
        self.assertEqual(len(targets), len(set(targets)))


if __name__ == "__main__":
    unittest.main()
