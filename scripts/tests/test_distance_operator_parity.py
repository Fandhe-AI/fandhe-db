"""crossdb ハーネスの距離演算子の静的検査。

現行の自作 DB は `<=>`（内積順）のみ実行でき、pgvector 側は `<#>`（負の内積・昇順）で
揃えて比較する。`<#>` の実行経路（#1977・#1991）が main に入った時点で自作 DB 側を
`<#>` へ切り替え、本検査の期待値も同時に更新する（Issue #1994）。ソースを走査するだけで psycopg は不要。`make scripts-test` で CI（scripts-test ジョブ）が実行する。
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "crossdb_bench"
PATTERN = re.compile(r"ORDER BY\s+(?:embedding|\{[^}]+\})\s+(<[=#+\-]>)")


def ops(name: str) -> set:
    return set(PATTERN.findall((ROOT / name).read_text(encoding="utf-8")))


class DistanceOperatorParityTest(unittest.TestCase):
    def test_self_side_uses_single_executable_operator(self):
        for name in ("self_db.py", "hybrid_after_where.py"):
            found = ops(name)
            self.assertEqual(found, {"<=>"}, f"{name}: unexpected operators {found}")

    def test_pgvector_side_uses_same_operator(self):
        self.assertEqual(ops("pgvector_db.py"), {"<#>"})

    def test_pgvector_hnsw_uses_ip_opclass(self):
        src = (ROOT / "pgvector_db.py").read_text(encoding="utf-8")
        self.assertIn("vector_ip_ops", src)


if __name__ == "__main__":
    unittest.main()
