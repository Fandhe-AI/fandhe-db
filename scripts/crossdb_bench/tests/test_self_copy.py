"""`self_copy.py` の単体テスト（`ingest_bulk` の COPY FROM STDIN 化。venv 不要の純粋部分）。

`python3 -m unittest discover scripts/crossdb_bench/tests` または pytest で実行する。
psycopg を使う `ingest_bulk` 本体は小 fixture での実機確認の担当（ここでは
接続を差し替えたスタブで分割・文・fail-closed のみ確認する）。
"""

from __future__ import annotations

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import self_copy  # noqa: E402


def _doc(i: int, **kw) -> dict:
    d = {"id": i, "lang": "ja", "topic": "t", "body": f"b{i}", "embedding": [0.5, -1.0]}
    d.update(kw)
    return d


class CopyRowsFromEnvTests(unittest.TestCase):
    def test_default_when_unset_or_invalid(self) -> None:
        for raw in (None, "", "abc", "0", "-3", "1000001"):
            self.assertEqual(self_copy.copy_rows_from_env(raw), 64, raw)

    def test_accepts_range(self) -> None:
        self.assertEqual(self_copy.copy_rows_from_env("1"), 1)
        self.assertEqual(self_copy.copy_rows_from_env(" 25000 "), 25000)
        self.assertEqual(self_copy.copy_rows_from_env("1000000"), 1_000_000)


class ChunkedTests(unittest.TestCase):
    def test_splits_with_remainder(self) -> None:
        self.assertEqual([len(c) for c in self_copy.chunked(list(range(150)), 64)], [64, 64, 22])

    def test_exact_multiple_and_rejects_nonpositive(self) -> None:
        self.assertEqual([len(c) for c in self_copy.chunked(list(range(128)), 64)], [64, 64])
        with self.assertRaises(ValueError):
            list(self_copy.chunked([1], 0))


class EncodeCsvTests(unittest.TestCase):
    def test_basic_row(self) -> None:
        out = self_copy.encode_csv([_doc(1)]).decode()
        # 埋め込みリテラルはカンマを含むため引用符付きになる。
        self.assertEqual(out, '1,"[0.50000000,-1.00000000]",ja,t,b1\n')

    def test_special_characters_are_quoted(self) -> None:
        out = self_copy.encode_csv([_doc(2, body='a,"b"\nc\\d')]).decode()
        self.assertTrue(out.endswith('"a,""b""\nc\\d"\n'), out)

    def test_empty_string_is_quoted_not_null(self) -> None:
        out = self_copy.encode_csv([_doc(3, topic="")]).decode()
        self.assertIn(',"",', out)
        self.assertNotIn(",,", out)

    def test_missing_topic_defaults_to_quoted_empty(self) -> None:
        d = _doc(4)
        del d["topic"]
        self.assertIn(',"",', self_copy.encode_csv([d]).decode())


class StatementTests(unittest.TestCase):
    def test_copy_statement_shape(self) -> None:
        sql = self_copy.copy_statement("xdb-bulk-3")
        self.assertEqual(
            sql,
            "COPY docs_bulk (id, embedding, lang, topic, body) FROM STDIN "
            "WITH (FORMAT csv) USING OPERATION_ID 'xdb-bulk-3'",
        )

    def test_copy_statement_rejects_unsafe_op_id(self) -> None:
        for bad in ("", "a'b", "a b", "x;DROP"):
            with self.assertRaises(ValueError):
                self_copy.copy_statement(bad)

    def test_create_table_sql(self) -> None:
        self.assertEqual(
            self_copy.create_table_sql(128),
            "CREATE TABLE docs_bulk (embedding VECTOR(128), lang TEXT, topic TEXT, body TEXT)",
        )
        for bad in (0, -1, "128", True):
            with self.assertRaises(ValueError):
                self_copy.create_table_sql(bad)


class _FakeCopy:
    def __init__(self, cur, stmt):
        self.cur, self.stmt, self.buf = cur, stmt, b""

    def __enter__(self):
        return self

    def write(self, data):
        self.buf += data

    def __exit__(self, *exc):
        self.cur.rowcount = self.buf.count(b"\n") + self.cur.skew
        self.cur.statements.append(self.stmt)


class _FakeCursor:
    def __init__(self, skew=0):
        self.rowcount = -1
        self.skew = skew
        self.statements: list[str] = []

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def execute(self, sql):
        self.statements.append(sql)

    def copy(self, stmt):
        return _FakeCopy(self, stmt)


class _FakeConn:
    def __init__(self, skew=0):
        self.cursors: list[_FakeCursor] = []
        self.skew = skew

    def cursor(self):
        c = _FakeCursor(self.skew)
        self.cursors.append(c)
        return c


class IngestBulkTests(unittest.TestCase):
    def test_splits_into_copies_and_reports(self) -> None:
        conn = _FakeConn()
        docs = [_doc(i) for i in range(1, 151)]
        res = self_copy.ingest_bulk(conn, docs, 2, copy_rows=64)
        self.assertEqual(res["rows"], 150)
        self.assertEqual(res["copies"], 3)
        self.assertEqual(res["rows_per_copy"], 64)
        self.assertEqual(res["last_copy_rows"], 22)
        self.assertEqual(res["commit_granularity"], "per_copy")
        stmts = [s for c in conn.cursors for s in c.statements]
        self.assertTrue(stmts[0].startswith("CREATE TABLE docs_bulk"))
        self.assertTrue(stmts[-1].startswith("DROP TABLE docs_bulk"))
        ops = [s for s in stmts if s.startswith("COPY")]
        self.assertEqual(len(ops), 3)
        self.assertEqual(len(set(ops)), 3)  # operation_id は COPY ごとに別

    def test_row_count_mismatch_fails_closed(self) -> None:
        conn = _FakeConn(skew=-1)
        with self.assertRaises(RuntimeError):
            self_copy.ingest_bulk(conn, [_doc(1), _doc(2)], 2, copy_rows=64)

    def test_empty_docs_rejected(self) -> None:
        with self.assertRaises(ValueError):
            self_copy.ingest_bulk(_FakeConn(), [], 2)


if __name__ == "__main__":
    unittest.main()
