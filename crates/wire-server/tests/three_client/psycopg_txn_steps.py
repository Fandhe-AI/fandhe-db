#!/usr/bin/env python3
"""psycopg（無改造）で明示トランザクション内の述語形書き込みと 40001 の再試行
（Issue #2412・SQL-31・SQL-19・ERR-7）を観測するクライアント。

`crates/wire-server/tests/three_client_txn_retry_e2e.rs`（`#[ignore]`）から子プロセスとして
起動される層 B のドライバ。1 プロセス内で「主セッション」と「他セッション群」の接続を
開き、与えられた文の列を**逐次**に実行する（スレッド・sleep は使わない）。主接続は
シナリオ全体を通して開いたままで、他セッションの commit をまたいで 40001 を観測できる。

`autocommit=True` で接続し `BEGIN`/`COMMIT`/`ROLLBACK` を文として送る（暗黙の BEGIN を
挿入させず、3 クライアントの送信列を揃えるため。`psycopg_client.py` と同じ理由）。
簡易モードは `ClientCursor`（パラメータ無し＝簡易クエリ）、拡張モードは `RawCursor`
（`$n` のまま Parse/Bind。パラメータ無しは `prepare=True` で拡張を強制）で送る。

環境変数（すべて必須。語彙外は終了コード 1）:
- WIRE_HOST / WIRE_PORT: 接続先。
- WIRE_CREDENTIALS: ユーザー名 -> ダミーパスワードの JSON オブジェクト。
- WIRE_TXN_MODE: `simple` または `extended` のみ。
- WIRE_TXN_STEPS: `[{"s": <セッション鍵>, "u": <ユーザー>, "sql": <文>, "p": [<文字列>...],
  "close": <bool>}]` の JSON 配列（`close` は実行後にそのセッションを閉じる）。

出力（stdout）: 文ごとに、SELECT は結果行（`|` 区切り）、それ以外はコマンドタグを出し、
続けて `@@<idx> <SQLSTATE>`（成功は `00000`）を出す。失敗時はさらに
`@@msg <idx> <主メッセージ>` を出して**続行する**（40001 の後の 25P02・ROLLBACK・再試行を
観測するため）。SQL 本文・資格情報は出力しない（security.md P0）。
"""

import json
import os
import sys


def fail(msg: str) -> int:
    print(f"psycopg_txn_steps: {msg}", file=sys.stderr)
    return 1


def main() -> int:
    host = os.environ.get("WIRE_HOST")
    port = os.environ.get("WIRE_PORT")
    mode = os.environ.get("WIRE_TXN_MODE")
    creds_raw = os.environ.get("WIRE_CREDENTIALS")
    steps_raw = os.environ.get("WIRE_TXN_STEPS")
    if not all([host, port, creds_raw, steps_raw]) or mode not in ("simple", "extended"):
        return fail("missing or invalid WIRE_* environment variables")
    try:
        creds = json.loads(creds_raw)
        steps = json.loads(steps_raw)
        port_num = int(port)
    except (ValueError, json.JSONDecodeError) as e:
        return fail(f"invalid JSON or port: {e}")
    if not isinstance(creds, dict) or not isinstance(steps, list):
        return fail("WIRE_CREDENTIALS must be an object and WIRE_TXN_STEPS an array")

    try:
        import psycopg
    except ImportError as e:
        return fail(f"psycopg is not installed: {e}")

    cursor_cls = psycopg.ClientCursor if mode == "simple" else psycopg.RawCursor
    conns = {}
    try:
        for idx, step in enumerate(steps):
            if not isinstance(step, dict) or set(step) != {"s", "u", "sql", "p", "close"}:
                return fail("steps must have exactly s, u, sql, p, close")
            key, user, sql, params = step["s"], step["u"], step["sql"], step["p"]
            if user not in creds or not isinstance(params, list):
                return fail("unknown user or malformed params")
            if key not in conns:
                conns[key] = psycopg.connect(
                    host=host,
                    port=port_num,
                    user=user,
                    password=creds[user],
                    dbname="irrelevant-db-name",
                    connect_timeout=5,
                    autocommit=True,
                    # 自動 prepare を無効化する。長寿命接続で同一文が繰り返されると
                    # prepare_threshold（既定 5）超過後に自動 prepare され、ROLLBACK 後に
                    # `DEALLOCATE ALL`（本サーバーは 42601 で拒否）が送られるため。
                    prepare_threshold=None,
                )
            conn = conns[key]
            try:
                with cursor_cls(conn) as cur:
                    if mode == "simple":
                        cur.execute(sql)
                    elif params:
                        cur.execute(sql, params)
                    else:
                        # パラメータ無しの文を拡張プロトコルで送るため pipeline を使う
                        # （pipeline の文は常に Parse/Bind/Execute/Sync）。`prepare=True`
                        # は ROLLBACK 後に psycopg が `DEALLOCATE ALL` を送るため使えない。
                        with conn.pipeline() as pipe:
                            cur.execute(sql)
                            pipe.sync()
                    if cur.description is not None:
                        for row in cur.fetchall():
                            print("|".join(str(c) for c in row))
                    else:
                        print(cur.statusmessage)
                print(f"@@{idx} 00000")
            except psycopg.Error as e:
                msg = (e.diag.message_primary or "").replace("\n", " ")
                print(f"@@{idx} {e.sqlstate or 'UNKNOWN'}")
                print(f"@@msg {idx} {msg}")
            sys.stdout.flush()
            if step["close"]:
                conns.pop(key).close()
        return 0
    except Exception as e:  # noqa: BLE001 — ハーネスへ理由を伝える最終防波堤（接続失敗など）
        return fail(f"failed: {type(e).__name__}")
    finally:
        for c in conns.values():
            try:
                c.close()
            except Exception:  # noqa: BLE001
                pass


if __name__ == "__main__":
    sys.exit(main())
