"""scripts/crash_test_backup_restore_client.py の時刻解析（Issue #3153・RECOVER-14 ポインタ）の単体試験。

crash_test_pitr.sh が server の `clock_timestamp()` の文字列を µs へ変換する純関数
`parse_timestamptz_us` の正常系・境界・拒否を確かめる。標準ライブラリのみ。
"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from crash_test_backup_restore_client import parse_timestamptz_us  # noqa: E402

# 2024-01-02 03:04:05 UTC のエポック秒
BASE = 1704164645


class ParseTimestamptzTest(unittest.TestCase):
    def test_fraction_digits(self):
        self.assertEqual(parse_timestamptz_us("2024-01-02 03:04:05+00"), BASE * 10**6)
        self.assertEqual(parse_timestamptz_us("2024-01-02 03:04:05.5+00"), BASE * 10**6 + 500000)
        self.assertEqual(parse_timestamptz_us("2024-01-02 03:04:05.123456+00"), BASE * 10**6 + 123456)
        self.assertEqual(parse_timestamptz_us("2024-01-02 03:04:05.01+00"), BASE * 10**6 + 10000)

    def test_offsets(self):
        self.assertEqual(parse_timestamptz_us("2024-01-02 12:04:05+09:00"), BASE * 10**6)
        self.assertEqual(parse_timestamptz_us("2024-01-01 22:04:05-05"), BASE * 10**6)
        self.assertEqual(parse_timestamptz_us("2024-01-02 12:04:05+0900"), BASE * 10**6)

    def test_no_zone_is_utc(self):
        self.assertEqual(parse_timestamptz_us("2024-01-02 03:04:05.897727"), BASE * 10**6 + 897727)

    def test_second_rollover(self):
        a = parse_timestamptz_us("2024-12-31 23:59:59.999999+00")
        b = parse_timestamptz_us("2025-01-01 00:00:00+00")
        self.assertEqual(b - a, 1)

    def test_rejects(self):
        for bad in [
            "",
            "2024-01-02T03:04:05+00",
            "2024-01-02 03:04:05.1234567+00",
            "2024-01-02 03:04:05+00 x",
            "2024-13-02 03:04:05+00",
            "2024-02-30 03:04:05+00",
            "2024-01-02 24:04:05+00",
            "2024-01-02 03:60:05+00",
            "2024-01-02 03:04:05+24",
        ]:
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    parse_timestamptz_us(bad)


if __name__ == "__main__":
    unittest.main()
