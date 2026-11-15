"""crossdb ハーネスの距離演算子の静的検査。

自作 DB 側は全 kNN 形（`USING MODE 'precision'` を含む）で `<=>`（コサイン距離）を使う
（SQL-33。Issue #3224）。Issue #2952 で一時 `<#>` へ切り替えたが、`<#>`（Ip）は HNSW の
対応 metric 外で `--config hnsw` が厳密全走査になるため戻した。他 DB・recall 正解は内積
（pgvector は `<#>` + `vector_ip_ops`）のままで、フィクスチャが L2 正規化済みのため順位は
一致する（`recall.fixture_norm_stats` が `meta.fixture_norm` に記録）。
ソースを走査するだけで psycopg は不要。`make scripts-test` で CI（scripts-test ジョブ）が実行する。
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "crossdb_bench"
PATTERN = re.compile(r"ORDER BY\s+(?:embedding|\{[^}]+\})\s+(<[=#+\-]>)")
PRECISION = re.compile(
    r"ORDER BY\s+embedding\s+(<[=#+\-]>)\s+'[^']*'\s+LIMIT\s+\d+\s+USING MODE 'precision'"
)
# 「自作 DB の `<=>` は内積」系の古い説明。
STALE_DOC = re.compile(r"`<=>`[^\n]{0,6}内積|内積[^\n]{0,6}`<=>`")
# 「自作 DB の `<#>`」系の古い説明（pgvector の `<#>` の説明には一致しない）。
STALE_SELF_IP = re.compile(r"自作 DB(?:側も|は|の)\s*`<#>`")


def read(name: str) -> str:
    return (ROOT / name).read_text(encoding="utf-8")


def ops(name: str) -> set:
    return set(PATTERN.findall(read(name)))


class DistanceOperatorParityTest(unittest.TestCase):
    def test_self_side_uses_cosine(self):
        for name in ("self_db.py", "hybrid_after_where.py"):
            found = ops(name)
            self.assertEqual(found, {"<=>"}, f"{name}: unexpected operators {found}")

    def test_precision_uses_cosine_like_the_rest(self):
        self.assertEqual(PRECISION.findall(read("self_db.py")), ["<=>"])

    def test_precision_pattern_does_not_match_variable_mode(self):
        mutated = "ORDER BY embedding <=> '{qv}' LIMIT 10 USING MODE '{mode}'"
        self.assertEqual(PRECISION.findall(mutated), [])

    def test_pgvector_side_uses_recall_ground_truth_inner_product(self):
        self.assertEqual(ops("pgvector_db.py"), {"<#>"})

    def test_pgvector_hnsw_uses_ip_opclass(self):
        self.assertIn("vector_ip_ops", read("pgvector_db.py"))

    def _doc_names(self):
        return [p.name for p in ROOT.glob("*.py")] + [
            "README.md",
            "gpu/faiss_batch_bench.py",
        ] + [f"tests/{p.name}" for p in (ROOT / "tests").glob("*.py")]

    def test_no_stale_cosine_is_inner_product_description(self):
        for name in self._doc_names():
            self.assertIsNone(
                STALE_DOC.search(read(name)), f"{name}: stale '<=> is inner product' text"
            )

    def test_no_stale_self_is_negative_inner_product_description(self):
        for name in self._doc_names():
            self.assertIsNone(
                STALE_SELF_IP.search(read(name)), f"{name}: stale 'self uses <#>' text"
            )


if __name__ == "__main__":
    unittest.main()
