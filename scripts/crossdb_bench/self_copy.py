"""self（wire-server）の `ingest_bulk` フェーズ: `COPY ... FROM STDIN`（CSV）での一括投入。

`self_db._run_phases` から呼ばれ、fixture の全 docs（`docs25k.jsonl`）を投入専用
テーブル `docs_bulk` へ流し込んで所要時間を返す（pgvector の `_ingest_bulk` と同じ
`{"rows","seconds","rows_per_sec"}` 形）。ポインタ: TASK-220・WIRE-17（COPY プロトコル。
実装記録は `docs/design/wire-copy-protocol.md`）・INDEX-4（一括投入の処理量ガード）。

設計上の要点（結果 JSON の `note` にも同内容を残す）:

- **投入先は計測用 fixture の `docs` ではなく `docs_bulk`**: 作業コピー redb に
  `CREATE TABLE`（要 DDL 権限。呼び出し元が `--ddl-allowed-users` を付けて起動する）で
  作る。`docs` へ入れると fixture の既存 id と衝突し、後続フェーズの可視行数も変わる。
  `CREATE TABLE`／`DROP TABLE` は計測区間の外（pgvector の `_setup_schema` と同じ扱い）。
- **1 COPY あたりの行数は既定 64 行**: COPY の行数は INDEX-4 ①
  （`engine::batch_limits::DEFAULT_MAX_FILES_PER_BATCH`＝64。サーバー側で
  `FANDHE_DB_BATCH_MAX_FILES` により上書き可）に数えられ、超過は `54000` で拒否される。
  そのため fixture 全件を 64 行ずつの複数 COPY に分け、COPY ごとに別の
  `USING OPERATION_ID` を付ける。commit は COPY ごと（pgvector は単一 COPY・単一
  トランザクション）であり、この差は結果 JSON の `commit_granularity` に記録する。
- **tenant／visibility は書き込み接続のテナントに固定**: COPY は `ctx.tenant_id()` と
  `Visibility::Private` で書く（RLS 相当の境界を緩めない）。予約列 `id`／`tenant_id`／
  `visibility` のうち明示指定できるのは `id` のみ。fixture 側の `tenant`／`visibility` は
  投入しない（pgvector は実値を投入するため意味論が異なる）。Private 行は wire 越しに
  読めないため、行数の検証は SELECT ではなく COPY の `CommandComplete`（`COPY n`）の
  合計で行う。
- **計測区間**: 最初の COPY 開始（CSV 化を含む）〜最後の COPY の完了応答まで。pgvector は同区間に
  tsvector 補完と 4 本の索引作成を含むが、self には対応する索引作成処理が無い
  （HNSW 構成の索引は最初のクエリで遅延構築され `hnsw_index_warm` で別計測）。

純粋関数（分割・CSV 化・文の組み立て）は psycopg 非依存で単体テストできる
（`tests/test_self_copy.py`）。
"""

from __future__ import annotations

import os
import time
from typing import Iterable, Iterator

from common import vec_literal

# 投入専用テーブル名（fixture の `docs` を汚さない）。固定の識別子であり外部入力を
# 連結しない。
TABLE = "docs_bulk"

# サーバー側 `engine::batch_limits::DEFAULT_MAX_FILES_PER_BATCH` と同値（INDEX-4 ①）。
DEFAULT_COPY_ROWS = 64
# サーバー側 `MAX_BATCH_MAX_FILES`（`MAX_DML_ROW_LIMIT`）と同値。
MAX_COPY_ROWS = 1_000_000

_ROWS_ENV = "FANDHE_DB_BATCH_MAX_FILES"

# `id` 以外の投入列（fixture の `docs` と同じ並び）。tenant／visibility は予約列で
# 明示指定できない。
COLUMNS = ("id", "embedding", "lang", "topic", "body")


def copy_rows_from_env(raw: str | None) -> int:
    """1 COPY あたりの行数を決める。サーバーの `parse_env_max_files` と同じ規則
    （未設定・非数値・範囲外は既定 64 へ倒す）で、サーバーに渡る環境変数と食い違わない。"""
    if raw is None:
        return DEFAULT_COPY_ROWS
    try:
        value = int(raw.strip())
    except ValueError:
        return DEFAULT_COPY_ROWS
    if 1 <= value <= MAX_COPY_ROWS:
        return value
    return DEFAULT_COPY_ROWS


def create_table_sql(dim: int) -> str:
    """投入先の `CREATE TABLE`。`dim` は int 以外を拒否する（SQL へ数値のみ埋め込む）。"""
    if isinstance(dim, bool) or not isinstance(dim, int) or dim <= 0:
        raise ValueError(f"dim must be a positive int (got {dim!r})")
    return (
        f"CREATE TABLE {TABLE} "
        f"(embedding VECTOR({dim}), lang TEXT, topic TEXT, body TEXT)"
    )


def drop_table_sql() -> str:
    return f"DROP TABLE {TABLE}"


def chunked(items: list, size: int) -> Iterator[list]:
    """`items` を `size` 件ずつに分ける（最後は端数）。"""
    if size <= 0:
        raise ValueError(f"size must be positive (got {size})")
    for i in range(0, len(items), size):
        yield items[i : i + size]


def copy_statement(op_id: str) -> str:
    """`COPY ... FROM STDIN` 文。`op_id` は英数字・`-`・`_` のみ許可する
    （`USING OPERATION_ID '<id>'` へ連結するため引用符等を拒否する）。"""
    if not op_id or not all(c.isalnum() or c in "-_" for c in op_id):
        raise ValueError(f"unsafe operation id: {op_id!r}")
    cols = ", ".join(COLUMNS)
    return (
        f"COPY {TABLE} ({cols}) FROM STDIN WITH (FORMAT csv) "
        f"USING OPERATION_ID '{op_id}'"
    )


def encode_csv(docs: Iterable[dict]) -> bytes:
    """docs を CSV（`COLUMNS` の並び・LF 終端）へ符号化する。

    text 形式だと body 中の `\\`・タブ・改行を手でエスケープする必要があるため CSV を
    使う（`,`・`"`・改行を含むフィールドのみ引用符で囲み、`"` は二重化）。CSV では
    引用符なしの空欄が NULL になるため、空文字列は必ず `""` として出す（`topic` 等が
    空でも非 NULL 列に NULL を入れて `22000` にならないようにする）。
    """
    lines = []
    for d in docs:
        fields = [
            str(int(d["id"])),
            vec_literal(d["embedding"]),
            d["lang"],
            d.get("topic", ""),
            d["body"],
        ]
        lines.append(",".join(_csv_field(v) for v in fields))
    return ("\n".join(lines) + "\n").encode("utf-8")


def _csv_field(value: str) -> str:
    """単一フィールドの CSV 表記。空文字列は `""`（NULL と区別する）。"""
    if value == "":
        return '""'
    if any(c in value for c in ',"\n\r'):
        return '"' + value.replace('"', '""') + '"'
    return value


def ingest_bulk(conn, docs: list[dict], dim: int, copy_rows: int | None = None) -> dict:
    """`docs` 全件を `docs_bulk` へ `COPY ... FROM STDIN` で投入して計測する。

    `conn` は `SelfServer.connect` が返す autocommit の psycopg 接続（簡易クエリ
    プロトコル）。DDL 権限のあるユーザーでの接続が前提。`COPY n` の合計が
    `len(docs)` と一致しなければ `RuntimeError` で失敗させる（fail-closed）。
    """
    if not docs:
        raise ValueError("docs is empty")
    size = copy_rows if copy_rows is not None else copy_rows_from_env(os.environ.get(_ROWS_ENV))

    with conn.cursor() as cur:
        cur.execute(create_table_sql(dim))

    copied = 0
    n_copies = 0
    t0 = time.perf_counter()
    with conn.cursor() as cur:
        # CSV 化は計測区間に含める（pgvector も行の符号化を `write_row` 内で行うため）。
        for k, chunk in enumerate(chunked(docs, size)):
            with cur.copy(copy_statement(f"xdb-bulk-{k}")) as copy:
                copy.write(encode_csv(chunk))
            n_copies += 1
            copied += cur.rowcount if cur.rowcount and cur.rowcount > 0 else 0
    t1 = time.perf_counter()

    with conn.cursor() as cur:
        cur.execute(drop_table_sql())

    if copied != len(docs):
        raise RuntimeError(f"COPY reported {copied} rows, expected {len(docs)}")
    elapsed = t1 - t0
    return {
        "rows": copied,
        "seconds": elapsed,
        "rows_per_sec": copied / elapsed if elapsed > 0 else None,
        "copies": n_copies,
        "rows_per_copy": size,
        "last_copy_rows": len(docs) - size * (n_copies - 1),
        "format": "csv",
        "table": TABLE,
        "commit_granularity": "per_copy",
        "note": (
            f"{n_copies} 回の COPY FROM STDIN（CSV・各 {size} 行以下。INDEX-4 ① の既定上限に"
            "合わせて分割）。commit は COPY ごと（pgvector は単一 COPY）。投入先は専用テーブル"
            f" {TABLE}（fixture の docs は不変）で tenant／visibility は接続テナントの"
            " tenant-a／private に固定（fixture 値は投入しない）。計測区間は CSV 化＋COPY のみで、"
            "pgvector が含める tsvector 補完・索引作成に相当する処理は無い"
        ),
    }
