"""自作 DB 側と pgvector 側が同じ距離演算子（`<#>`）を使うことの静的検査。

SQL-33（`<=>` のコサイン距離化）の後も、crossdb ハーネスは両 DB を負の内積・
昇順で揃えて比較する（Issue #1994）。ソースを走査するだけで psycopg は不要。
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PATTERN = re.compile(r"ORDER BY\s+(?:embedding|\{[^}]+\})\s+(<[=#+\-]>)")


def ops(name: str) -> set:
    return set(PATTERN.findall((ROOT / name).read_text(encoding="utf-8")))


class DistanceOperatorParityTest(unittest.TestCase):
    def test_self_side_uses_negative_inner_product_only(self):
        for name in ("self_db.py", "hybrid_after_where.py"):
            found = ops(name)
            self.assertEqual(found, {"<#>"}, f"{name}: unexpected operators {found}")

    def test_pgvector_side_uses_same_operator(self):
        self.assertEqual(ops("pgvector_db.py"), {"<#>"})

    def test_pgvector_hnsw_uses_ip_opclass(self):
        src = (ROOT / "pgvector_db.py").read_text(encoding="utf-8")
        self.assertIn("vector_ip_ops", src)

    def test_no_cosine_operator_in_self_sql(self):
        for name in ("self_db.py", "hybrid_after_where.py"):
            src = (ROOT / name).read_text(encoding="utf-8")
            self.assertNotIn("embedding <=>", src, name)


if __name__ == "__main__":
    unittest.main()
