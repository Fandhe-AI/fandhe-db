"""scripts/epoch_time.sh のロケール非依存な時刻解析の試験（Issue #3138）。

crash_test_pitr.sh が呼ぶ parse_epoch_us / read_epoch_us を bash 経由で検証する。
小数点がカンマのロケールでも正しい値になること、形式外は拒否されることを確かめる。
"""

import os
import re
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent.parent
HELPER = SCRIPTS / "epoch_time.sh"
SNIPPET_PARSE = 'source "$0"; parse_epoch_us "$1" && echo "$EPOCH_US"'
SNIPPET_READ = 'source "$0"; read_epoch_us && echo "$EPOCH_US"'


def run_bash(snippet, *args, env=None):
    return subprocess.run(
        ["bash", "-c", snippet, str(HELPER), *args],
        capture_output=True,
        text=True,
        env=env,
    )


def bash_major():
    out = subprocess.run(
        ["bash", "-c", 'echo "${BASH_VERSINFO[0]}"'], capture_output=True, text=True
    )
    return int(out.stdout.strip() or 0)


class ParseEpochUs(unittest.TestCase):
    def test_accepts_dot_and_comma(self):
        cases = {
            "1700000000.123456": "1700000000123456",
            "1700000000,123456": "1700000000123456",
            "1700000000.000001": "1700000000000001",
        }
        for src, want in cases.items():
            with self.subTest(src=src):
                r = run_bash(SNIPPET_PARSE, src)
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertEqual(r.stdout.strip(), want)

    def test_rejects_unexpected_formats(self):
        for src in [
            "",
            "1700000000",
            "1700000000.12345",
            "1700000000.1234567",
            "1700000000;123456",
            "abc",
            " 1700000000.123456",
            "1700000000.123456 ",
        ]:
            with self.subTest(src=src):
                r = run_bash(SNIPPET_PARSE, src)
                self.assertNotEqual(r.returncode, 0)
                self.assertEqual(r.stdout.strip(), "")


class ReadEpochUs(unittest.TestCase):
    def setUp(self):
        if bash_major() < 5:
            self.skipTest("bash 5 or later is required for EPOCHREALTIME")

    def assert_near_now(self, value):
        self.assertGreaterEqual(len(value), 16)
        now_us = time.time_ns() // 1000
        self.assertLess(abs(int(value) - now_us), 5_000_000)

    def test_c_locale(self):
        env = dict(os.environ, LC_ALL="C")
        r = run_bash(SNIPPET_READ, env=env)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assert_near_now(r.stdout.strip())

    def test_comma_locale(self):
        if (
            not shutil.which("localedef")
            or not Path("/usr/share/i18n/locales/de_DE").exists()
        ):
            self.skipTest("localedef or de_DE locale source is not available")
        with tempfile.TemporaryDirectory() as tmp:
            made = subprocess.run(
                ["localedef", "-i", "de_DE", "-f", "UTF-8", f"{tmp}/de_DE.UTF-8"],
                capture_output=True,
                text=True,
            )
            if not Path(tmp, "de_DE.UTF-8").exists():
                self.skipTest(f"localedef failed: {made.stderr.strip()}")
            env = dict(os.environ, LOCPATH=tmp, LC_ALL="de_DE.UTF-8")
            raw = run_bash('echo "$EPOCHREALTIME"', env=env).stdout.strip()
            # 前提: 実際にカンマで出ること（空虚な成功を防ぐ）。
            self.assertIn(",", raw)
            r = run_bash(SNIPPET_READ, env=env)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assert_near_now(r.stdout.strip())


class NoLocaleDependentPattern(unittest.TestCase):
    def test_no_substitution_on_epochrealtime(self):
        pat = re.compile(r"\$\{EPOCHREALTIME/")
        for sh in SCRIPTS.glob("*.sh"):
            with self.subTest(script=sh.name):
                self.assertIsNone(pat.search(sh.read_text()))


if __name__ == "__main__":
    unittest.main()
