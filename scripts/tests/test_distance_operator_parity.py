"""crossdb ハーネスの距離演算子の静的検査。

自作 DB 側は `<#>`（負の内積の昇順＝内積の降順）で、pgvector の `<#>`・recall 正解
（numpy 内積）・Qdrant `DOT` 等と同じ順位で比較する（SQL-33。Issue #2952）。`<=>` は
コサイン距離なので比較には使わない。例外は `USING MODE 'precision'` の 1 文だけで、
`<#>` との併用を engine が `0A000` で拒否するため `<=>` のまま残す（self 専用フェーズで
DB 間比較・recall 評価の対象外）。ソースを走査するだけで psycopg は不要。
`make scripts-test` で CI（scripts-test ジョブ）が実行する。
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "crossdb_bench"
PATTERN = re.compile(r"ORDER BY\s+(?:embedding|\{[^}]+\})\s+(<[=#+\-]>)")
# precision の 1 文（1 行の f-string）だけを例外として特定する。`'{mode}'` のような
# 変数展開にはマッチしない（recall 側へ `<=>` が残ると検出されるようにする）。
PRECISION = re.compile(
    r"ORDER BY\s+embedding\s+(<[=#+\-]>)\s+'[^']*'\s+LIMIT\s+\d+\s+USING MODE 'precision'"
)
# 「自作 DB の `<=>` は内積」系の古い説明（受入基準 3 の退行防止）。
STALE_DOC = re.compile(r"`<=>`[^\n]{0,6}内積|内積[^\n]{0,6}`<=>`")


def read(name: str) -> str:
    return (ROOT / name).read_text(encoding="utf-8")


def ops(name: str) -> set:
    return set(PATTERN.findall(read(name)))


class DistanceOperatorParityTest(unittest.TestCase):
    def test_self_side_uses_negative_inner_product(self):
        for name in ("self_db.py", "hybrid_after_where.py"):
            rest = PRECISION.sub("", read(name))
            found = set(PATTERN.findall(rest))
            self.assertEqual(found, {"<#>"}, f"{name}: unexpected operators {found}")

    def test_precision_is_the_only_cosine_exception(self):
        found = PRECISION.findall(read("self_db.py"))
        self.assertEqual(found, ["<=>"])
        self.assertEqual(PRECISION.findall(read("hybrid_after_where.py")), [])

    def test_precision_exception_does_not_match_variable_mode(self):
        mutated = "ORDER BY embedding <=> '{qv}' LIMIT 10 USING MODE '{mode}'"
        self.assertEqual(PRECISION.findall(mutated), [])
        self.assertEqual(set(PATTERN.findall(PRECISION.sub("", mutated))), {"<=>"})

    def test_pgvector_side_uses_same_operator(self):
        self.assertEqual(ops("pgvector_db.py"), {"<#>"})

    def test_pgvector_hnsw_uses_ip_opclass(self):
        self.assertIn("vector_ip_ops", read("pgvector_db.py"))

    def test_no_stale_cosine_is_inner_product_description(self):
        names = [p.name for p in ROOT.glob("*.py")] + ["README.md", "gpu/faiss_batch_bench.py"]
        for name in names:
            self.assertIsNone(
                STALE_DOC.search(read(name)), f"{name}: stale '<=> is inner product' text"
            )


if __name__ == "__main__":
    unittest.main()
