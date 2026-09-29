#!/usr/bin/env python3
"""psycopg（無改造）で wire-server へ拡張クエリプロトコルの 1 文を送る型付きクライアント。

`crates/wire-server/tests/three_client_extended_e2e.rs`（Issue #1176・WIRE-11／12／13／14、
`#[ignore]`）から子プロセスとして起動される層 B ハーネス。簡易クエリ経路を固定する
`psycopg_client.py` とは責務を分け、こちらは Parse／Describe／Bind／Execute を駆動する。

`psycopg.RawCursor` を使う理由: `$n` プレースホルダをそのまま SQL に書け、psql `\\bind`・
node pg と同一の SQL 定数を 3 クライアントで共有できる（既定 `Cursor` の `%s` も送信時に
`$n` へ変換されるため Parse のバイト列は同一）。

環境変数（argv／stdin は使わない。語彙外は fail-closed で終了コード 1）:
- WIRE_HOST / WIRE_PORT / WIRE_USER / WIRE_PASSWORD / WIRE_SQL: 必須。
- WIRE_PARAMS（任意）: 文字列の JSON 配列（テキストパラメータとして束縛）。
- WIRE_PARAMS_INT（任意）: 整数の JSON 配列。Python `int` はバイナリパラメータとして
  送られるため、非 text スロットのバイナリ束縛が `0A000` で拒否される負のケースに使う。
- WIRE_BINARY（任意）: `1` のみ受理。結果をバイナリ形式で要求する。
- WIRE_EXTENDED_NOPARAM（任意）: `1` のみ受理。パラメータ無しでも `prepare=True` で
  名前付き文（拡張プロトコル）として送る。

出力: 各行のセルを `<型名>:<正準表現>` にして `|` で連結し改行区切りで stdout へ。
bytes は小文字 hex、float は repr、それ以外は str()。失敗時は stderr に
`[SQLSTATE=<code>]` を含めて終了コード 1。
"""

import json
import os
import sys


def _flag(name: str) -> bool:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return False
    if raw != "1":
        print(f"psycopg_extended: {name} must be '1' if set, got {raw!r}", file=sys.stderr)
        sys.exit(1)
    return True


def _json_list(name: str, elem_type: type):
    raw = os.environ.get(name)
    if not raw:
        return None
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as e:
        print(f"psycopg_extended: {name} is not valid JSON: {e}", file=sys.stderr)
        sys.exit(1)
    if not isinstance(parsed, list) or not all(type(v) is elem_type for v in parsed):
        print(
            f"psycopg_extended: {name} must be a JSON array of {elem_type.__name__}",
            file=sys.stderr,
        )
        sys.exit(1)
    return parsed


def _cell(v) -> str:
    if isinstance(v, (bytes, bytearray, memoryview)):
        canonical = bytes(v).hex()
    elif isinstance(v, float):
        canonical = repr(v)
    else:
        canonical = str(v)
    return f"{type(v).__name__}:{canonical}"


def main() -> int:
    host = os.environ.get("WIRE_HOST")
    port = os.environ.get("WIRE_PORT")
    user = os.environ.get("WIRE_USER")
    password = os.environ.get("WIRE_PASSWORD")
    sql = os.environ.get("WIRE_SQL")
    if not all([host, port, user, password is not None, sql]):
        print("psycopg_extended: missing required WIRE_* environment variables", file=sys.stderr)
        return 1

    str_params = _json_list("WIRE_PARAMS", str)
    int_params = _json_list("WIRE_PARAMS_INT", int)
    if str_params is not None and int_params is not None:
        print("psycopg_extended: WIRE_PARAMS and WIRE_PARAMS_INT are exclusive", file=sys.stderr)
        return 1
    params = str_params if str_params is not None else int_params
    binary = _flag("WIRE_BINARY")
    noparam_extended = _flag("WIRE_EXTENDED_NOPARAM")

    try:
        import psycopg
    except ImportError as e:
        print(f"psycopg_extended: psycopg is not installed: {e}", file=sys.stderr)
        return 1

    try:
        with psycopg.connect(
            host=host,
            port=int(port),
            user=user,
            password=password,
            dbname="irrelevant-db-name",
            autocommit=True,
            connect_timeout=5,
        ) as conn:
            with psycopg.RawCursor(conn) as cur:
                cur.execute(
                    sql,
                    params,
                    binary=binary,
                    prepare=True if noparam_extended else None,
                )
                if cur.description is not None:
                    for row in cur.fetchall():
                        print("|".join(_cell(v) for v in row))
        return 0
    except Exception as e:  # noqa: BLE001 — ハーネスへ理由を伝える最終防波堤
        sqlstate = getattr(e, "sqlstate", None)
        suffix = f" [SQLSTATE={sqlstate}]" if sqlstate else ""
        print(f"psycopg_extended: query failed{suffix}: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
