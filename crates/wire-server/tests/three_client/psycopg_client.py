#!/usr/bin/env python3
"""psycopg（無改造）で wire-server へ簡易クエリを 1 文送るクライアント。

`crates/wire-server/tests/three_client_e2e.rs`（TASK-73・WIRE-1、`#[ignore]`）から
子プロセスとして起動される。接続情報・SQL・タイムアウトはすべて環境変数で受け取り
（コマンドライン引数・ソース中にダミー資格情報以外の秘密情報を書かない。
security.md P0）、`autocommit=True` で `execute()` する（非 autocommit だと
`BEGIN` が簡易クエリとして先行送信され、許可リスト外構文として engine に
拒否されるため。ハーネス側の設計判断は `docs/design/three-client-e2e-harness.md`
参照）。

`psycopg.ClientCursor` を明示的に使う（既定の `Cursor` は `autocommit=True` でも
サーバーサイドパラメータバインドを伴う拡張クエリプロトコル（Parse/Bind/Execute）
で送信するため。本サーバーは拡張クエリメッセージ未対応のため拒否する。
`ClientCursor` は SQL をクライアント側で文字列合成してから簡易クエリ
プロトコル（'Q' メッセージ）で送るため、WIRE-1 の検証対象と一致する。
codex-review 指摘・PR #210）。

環境変数:
- WIRE_HOST / WIRE_PORT / WIRE_USER / WIRE_PASSWORD: 接続情報。
- WIRE_SQL: 実行する SQL 文。
- WIRE_SQL_PRELUDE（任意・TASK-165）: 同一接続で `WIRE_SQL` より先に順次実行する
  SQL 文の配列を表す JSON 文字列（例 `["SET search_mode = 'precision'"]`）。
  `SET search_mode` を先行実行してから本体の `SELECT` を送る、セッション複数文の
  検証（SQL-12）に使う。配列でない・要素が文字列でない場合は fail-closed で
  エラー終了する（stdin／argv を使わない現行方針を維持。security.md P0）。
- WIRE_SSLMODE（任意・Issue #969・WIRE-9）: 指定時のみ `psycopg.connect(...,
  sslmode=<値>)` を渡す（TLS 越しの接続検証用）。閉じた語彙
  （`disable`／`allow`／`prefer`／`require`／`verify-ca`／`verify-full`。
  psycopg／libpq が受理する値のみ）に限り、語彙外は fail-closed でエラー
  終了する。未指定なら従来どおり kwarg を渡さない（挙動不変）。

- WIRE_PRINT_COMMAND_TAG（任意・Issue #1339・SQL-32）: `"1"` のときのみ、本体 SQL の
  実行後に `cur.statusmessage`（`CommandComplete` のコマンドタグ。例 `UPDATE 2`）を
  stdout へ 1 行出力する（分割実行 DML の件数がドライバ API へ届くことの観測用）。
  語彙は `"1"` のみで、それ以外の値は fail-closed でエラー終了する。未指定なら
  従来どおり何も出さない（挙動不変）。

成功時は結果セットの各行を `|` 区切りで結合した文字列を改行区切りで stdout へ
出力し、終了コード 0（複数列を返す SQL でも列構成・型変換を検証できるよう
全列を出力する。`crates/wire-server/tests/three_client_e2e.rs` の
`run_psql`／`run_pg` と同じ区切り規約）。失敗時はエラーを stderr へ出力し、
終了コード 1（silent skip はしない）。SQLSTATE を伴う失敗（拒否経路の検証。
TASK-165）は `[SQLSTATE=<code>]` を stderr メッセージに含める。commit 成功
境界を跨いだ panic 時の緊急応答（TASK-97・TASK-153・ERR-5・Issue #706）が
持つ `detail` フィールド（`state=may_be_committed`）は psycopg の
`e.diag.message_detail` として読めるため、値がある場合は
`[DETAIL=<detail>]` を続けて stderr メッセージに含める。
"""

import json
import os
import sys


def main() -> int:
    host = os.environ.get("WIRE_HOST")
    port = os.environ.get("WIRE_PORT")
    user = os.environ.get("WIRE_USER")
    password = os.environ.get("WIRE_PASSWORD")
    sql = os.environ.get("WIRE_SQL")
    if not all([host, port, user, password is not None, sql]):
        print("psycopg_client: missing required WIRE_* environment variables", file=sys.stderr)
        return 1

    prelude_raw = os.environ.get("WIRE_SQL_PRELUDE")
    prelude: list[str] = []
    if prelude_raw:
        try:
            parsed = json.loads(prelude_raw)
        except json.JSONDecodeError as e:
            print(f"psycopg_client: WIRE_SQL_PRELUDE is not valid JSON: {e}", file=sys.stderr)
            return 1
        if not isinstance(parsed, list) or not all(isinstance(s, str) for s in parsed):
            print(
                "psycopg_client: WIRE_SQL_PRELUDE must be a JSON array of strings",
                file=sys.stderr,
            )
            return 1
        prelude = parsed

    sslmode = os.environ.get("WIRE_SSLMODE")
    allowed_sslmodes = {"disable", "allow", "prefer", "require", "verify-ca", "verify-full"}
    if sslmode is not None and sslmode not in allowed_sslmodes:
        print(
            f"psycopg_client: WIRE_SSLMODE must be one of {sorted(allowed_sslmodes)}, got {sslmode!r}",
            file=sys.stderr,
        )
        return 1

    print_tag = os.environ.get("WIRE_PRINT_COMMAND_TAG")
    if print_tag is not None and print_tag != "1":
        print(
            f"psycopg_client: WIRE_PRINT_COMMAND_TAG must be \"1\" if set, got {print_tag!r}",
            file=sys.stderr,
        )
        return 1

    try:
        import psycopg
    except ImportError as e:
        print(f"psycopg_client: psycopg is not installed: {e}", file=sys.stderr)
        return 1

    connect_kwargs = {}
    if sslmode is not None:
        connect_kwargs["sslmode"] = sslmode

    try:
        with psycopg.connect(
            host=host,
            port=int(port),
            user=user,
            password=password,
            dbname="irrelevant-db-name",
            autocommit=True,
            connect_timeout=5,
            **connect_kwargs,
        ) as conn:
            with psycopg.ClientCursor(conn) as cur:
                for stmt in prelude:
                    cur.execute(stmt)
                cur.execute(sql)
                # `INSERT`／`CREATE FUNCTION`（TASK-82）等の `CommandComplete` のみ
                # を返す文（結果セットを持たない）は `cur.description` が `None`
                # になり、`fetchall()` を呼ぶと psycopg が
                # `ProgrammingError` を送出する。結果セットが無いことは
                # 失敗ではないため、その場合は行の出力をスキップする（既存の
                # `SELECT` 系呼び出しは常に `description` を持つため挙動は
                # 変わらない）。
                if cur.description is not None:
                    for row in cur.fetchall():
                        print("|".join(str(value) for value in row))
                # コマンドタグ（Issue #1339）。opt-in 時のみ、末尾に 1 行出す。
                if print_tag == "1" and cur.statusmessage:
                    print(cur.statusmessage)
        return 0
    except Exception as e:  # noqa: BLE001 — ハーネスへ理由を伝える最終防波堤
        sqlstate = getattr(e, "sqlstate", None)
        suffix = f" [SQLSTATE={sqlstate}]" if sqlstate else ""
        diag = getattr(e, "diag", None)
        detail = getattr(diag, "message_detail", None) if diag is not None else None
        suffix += f" [DETAIL={detail}]" if detail else ""
        print(f"psycopg_client: query failed{suffix}: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
