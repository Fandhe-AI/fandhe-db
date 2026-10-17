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
- WIRE_PARAMS_DECIMAL（任意）: 文字列の JSON 配列。各要素を `decimal.Decimal` にして
  束縛する。psycopg の AUTO 形式は `Decimal` を text（`DecimalDumper`）で送るため、
  公開 API `conn.adapters.register_dumper` で `DecimalBinaryDumper` を登録し直し、
  numeric（OID 1700）のバイナリパラメータ（WIRE-26。Issue #2445）として送らせる
  （ドライバ無改造）。`WIRE_PARAMS`／`WIRE_PARAMS_INT` とは排他。
- WIRE_PARAMS_TYPED（任意）: `[{"type": <語彙>, "value": <値|null>}, ...]` の JSON 配列
  （Issue #2440。WIRE-24・SQL-45 のポインタ）。位置ごとに Python の型を変え、Parse で宣言される
  型 OID を駆動する。語彙は閉じた集合で、語彙外・型の不一致・余計なキーは終了コード 1:
  `unknown`(str・OID 0)／`text`(str を OID 25 で宣言)／`oid705`(str を OID 705 で宣言)／
  `int`(Python int)／`int2`／`int4`／`int8`／`float4`／`float8`(psycopg のラッパー)／
  `float`／`bool`／`decimal`(str から Decimal)／`date`(ISO 文字列から date)／
  `int_list`(int の配列。int2[] として送られる)／`null`(値は null。OID 0 の NULL)。
  任意 OID の宣言は公開 API `conn.adapters.register_dumper` と `StrDumper` 派生の組で行い、
  ドライバは改造しない。`WIRE_PARAMS`／`WIRE_PARAMS_INT`／`WIRE_PARAMS_DECIMAL` とは排他。
- WIRE_SQL_AFTER（任意）: 1 つ目の文が失敗しても同じ接続で実行する 2 つ目の文（パラメータ
  無し）。1 つ目の失敗は stderr に `[SQLSTATE=...]` 付きで出し、2 つ目の結果行は stdout へ
  `after:` を前置して出す。終了コードは 2 つ目が成功したときだけ 0（接続が失敗後も
  生きていることの確認用）。
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


_TYPED_KEYS = {"type", "value"}
_STR_TYPES = {"unknown", "text", "oid705", "decimal", "date"}
_INT_TYPES = {"int", "int2", "int4", "int8"}


def _typed_fail(msg: str):
    print(f"psycopg_extended: WIRE_PARAMS_TYPED {msg}", file=sys.stderr)
    sys.exit(1)


def _typed_params(raw: str):
    """WIRE_PARAMS_TYPED を検証し、(束縛値, 必要な Dumper 登録 [(クラス, OID)]) を返す。"""
    from psycopg.types.numeric import Float4, Float8, Int2, Int4, Int8

    try:
        spec = json.loads(raw)
    except json.JSONDecodeError as e:
        _typed_fail(f"is not valid JSON: {e}")
    if not isinstance(spec, list):
        _typed_fail("must be a JSON array")
    oid_classes = {"text": 25, "oid705": 705}
    made = {}
    values = []
    registrations = []
    for item in spec:
        if not isinstance(item, dict) or set(item.keys()) != _TYPED_KEYS:
            _typed_fail("items must be objects with exactly type and value")
        kind, v = item["type"], item["value"]
        if kind in _STR_TYPES:
            if type(v) is not str:
                _typed_fail(f"{kind} needs a string value")
            if kind == "unknown":
                values.append(v)
            elif kind in oid_classes:
                if kind not in made:
                    made[kind] = type(f"Declared{kind}", (str,), {})
                    registrations.append((made[kind], oid_classes[kind]))
                values.append(made[kind](v))
            elif kind == "decimal":
                import decimal

                try:
                    values.append(decimal.Decimal(v))
                except decimal.InvalidOperation:
                    _typed_fail("decimal value is invalid")
            else:  # date
                import datetime

                try:
                    values.append(datetime.date.fromisoformat(v))
                except ValueError:
                    _typed_fail("date value is invalid")
        elif kind in _INT_TYPES:
            if type(v) is not int:
                _typed_fail(f"{kind} needs an integer value")
            wrap = {"int": int, "int2": Int2, "int4": Int4, "int8": Int8}[kind]
            values.append(wrap(v))
        elif kind in ("float", "float4", "float8"):
            if type(v) not in (int, float):
                _typed_fail(f"{kind} needs a number value")
            wrap = {"float": float, "float4": Float4, "float8": Float8}[kind]
            values.append(wrap(v))
        elif kind == "bool":
            if type(v) is not bool:
                _typed_fail("bool needs a boolean value")
            values.append(v)
        elif kind == "int_list":
            if not isinstance(v, list) or not all(type(x) is int for x in v):
                _typed_fail("int_list needs an array of integers")
            values.append(v)
        elif kind == "null":
            if v is not None:
                _typed_fail("null needs a null value")
            values.append(None)
        else:
            _typed_fail(f"has an unknown type {kind!r}")
    return values, registrations


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
    dec_raw = _json_list("WIRE_PARAMS_DECIMAL", str)
    if dec_raw is not None and (str_params is not None or int_params is not None):
        print(
            "psycopg_extended: WIRE_PARAMS_DECIMAL is exclusive with WIRE_PARAMS/WIRE_PARAMS_INT",
            file=sys.stderr,
        )
        return 1
    decimal_params = None
    if dec_raw is not None:
        import decimal

        try:
            decimal_params = [decimal.Decimal(v) for v in dec_raw]
        except decimal.InvalidOperation as e:
            print(f"psycopg_extended: WIRE_PARAMS_DECIMAL has an invalid decimal: {e}", file=sys.stderr)
            return 1
    typed_raw = os.environ.get("WIRE_PARAMS_TYPED")
    typed_params = None
    typed_registrations = []
    if typed_raw:
        if str_params is not None or int_params is not None or dec_raw is not None:
            print("psycopg_extended: WIRE_PARAMS_TYPED is exclusive with other params", file=sys.stderr)
            return 1
        typed_params, typed_registrations = _typed_params(typed_raw)
    sql_after = os.environ.get("WIRE_SQL_AFTER") or None
    params = str_params if str_params is not None else int_params
    if typed_params is not None:
        params = typed_params
    if decimal_params is not None:
        params = decimal_params
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
            if decimal_params is not None:
                from decimal import Decimal

                from psycopg.types.numeric import DecimalBinaryDumper

                conn.adapters.register_dumper(Decimal, DecimalBinaryDumper)
            for cls, oid in typed_registrations:
                from psycopg.types.string import StrDumper

                dumper = type(f"Oid{oid}Dumper", (StrDumper,), {"oid": oid})
                conn.adapters.register_dumper(cls, dumper)
            with psycopg.RawCursor(conn) as cur:
                try:
                    cur.execute(
                        sql,
                        params,
                        binary=binary,
                        prepare=True if noparam_extended else None,
                    )
                    if cur.description is not None:
                        for row in cur.fetchall():
                            print("|".join(_cell(v) for v in row))
                except psycopg.Error as e:
                    if sql_after is None:
                        raise
                    sqlstate = getattr(e, "sqlstate", None)
                    suffix = f" [SQLSTATE={sqlstate}]" if sqlstate else ""
                    print(f"psycopg_extended: query failed{suffix}: {e}", file=sys.stderr)
                if sql_after is not None:
                    cur.execute(sql_after)
                    if cur.description is not None:
                        for row in cur.fetchall():
                            print("after:" + "|".join(_cell(v) for v in row))
        return 0
    except Exception as e:  # noqa: BLE001 — ハーネスへ理由を伝える最終防波堤
        sqlstate = getattr(e, "sqlstate", None)
        suffix = f" [SQLSTATE={sqlstate}]" if sqlstate else ""
        print(f"psycopg_extended: query failed{suffix}: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
