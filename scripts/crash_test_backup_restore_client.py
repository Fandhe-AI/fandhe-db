#!/usr/bin/env python3
"""Issue #2157（RECOVER-13 ポインタ）のバックアップ復元 crash 回帰テスト用の最小 pg wire クライアント。

`scripts/crash_test_backup_restore.sh` が起動した wire-server（cleartext 認証・Simple Query のみ）へ
接続して、シード投入・追記・件数取得を行う。標準ライブラリのみを使い（外部クライアント・依存を
増やさない）、`psql` の有無に左右されない。bash だけでは pg wire のバイナリ入出力を扱えないため
補助として置く。

サブコマンド（いずれも `<port> <user> <password>` が先頭）:
- `seed <rows>`: テーブルを作り、`rows` 行を 1 バッチ 50 行（バッチ件数の既定上限内）で投入する。
- `append <start-id>`: 1 行ずつ INSERT し、コミット応答を受けるたびに `ACK <id>` を標準出力へ出す
  （呼び出し側がサーバーを SIGKILL する直前までの確認済み最大 id を知るための同期点）。
- `count`: `COUNT=<n> MAXID=<m>` を 1 行で出す（行が飛んでいないかの検査用）。
"""

import socket
import struct
import sys

PAYLOAD = "x" * 1000


def _recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("connection closed")
        buf += chunk
    return buf


def _read_msg(sock):
    head = _recv_exact(sock, 5)
    ty = head[:1]
    (length,) = struct.unpack("!I", head[1:])
    if length < 4 or length > 64 * 1024 * 1024:
        raise ValueError("bad message length")
    return ty, _recv_exact(sock, length - 4)


def _error_message(payload):
    """ErrorResponse のフィールド列から `M`（メッセージ）を取り出す。"""
    for field in payload.split(b"\0"):
        if field[:1] == b"M":
            return field[1:].decode(errors="replace")
    return "unknown"


def connect(port, user, password):
    sock = socket.create_connection(("127.0.0.1", port), timeout=30)
    params = b"user\0" + user.encode() + b"\0database\0db\0\0"
    body = struct.pack("!I", 196608) + params
    sock.sendall(struct.pack("!I", len(body) + 4) + body)
    while True:
        ty, payload = _read_msg(sock)
        if ty == b"R":
            (code,) = struct.unpack("!I", payload[:4])
            if code == 3:
                pw = password.encode() + b"\0"
                sock.sendall(b"p" + struct.pack("!I", len(pw) + 4) + pw)
            elif code != 0:
                raise RuntimeError("unsupported auth code %d" % code)
        elif ty == b"E":
            raise RuntimeError("auth error")
        elif ty == b"Z":
            return sock


def query(sock, sql):
    """Simple Query を 1 本実行し、DataRow を文字列タプルのリストで返す。エラー応答は例外。"""
    data = sql.encode() + b"\0"
    sock.sendall(b"Q" + struct.pack("!I", len(data) + 4) + data)
    rows = []
    err = None
    while True:
        ty, payload = _read_msg(sock)
        if ty == b"D":
            (n,) = struct.unpack("!H", payload[:2])
            off = 2
            row = []
            for _ in range(n):
                (ln,) = struct.unpack("!i", payload[off : off + 4])
                off += 4
                if ln < 0:
                    row.append(None)
                else:
                    row.append(payload[off : off + ln].decode())
                    off += ln
            rows.append(tuple(row))
        elif ty == b"E":
            err = payload
        elif ty == b"Z":
            if err is not None:
                raise RuntimeError("server error: " + _error_message(err))
            return rows


def main(argv):
    if len(argv) < 5:
        print("usage: client.py <seed|append|count> <port> <user> <password> [arg]", file=sys.stderr)
        return 2
    cmd, port, user, password = argv[1], int(argv[2]), argv[3], argv[4]
    sock = connect(port, user, password)
    if cmd == "seed":
        total = int(argv[5])
        query(sock, "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)")
        batch = 50
        for lo in range(0, total, batch):
            hi = min(lo + batch, total)
            values = ",".join("(%d,'%s')" % (i, PAYLOAD) for i in range(lo, hi))
            query(sock, "INSERT INTO docs (id, body) VALUES " + values)
        print("SEEDED", total, flush=True)
    elif cmd == "append":
        i = int(argv[5])
        while True:
            query(sock, "INSERT INTO docs (id, body) VALUES (%d,'%s')" % (i, PAYLOAD))
            print("ACK", i, flush=True)
            i += 1
    elif cmd == "count":
        rows = query(sock, "SELECT count(*), max(id) FROM docs")
        print("COUNT=%s MAXID=%s" % rows[0], flush=True)
    else:
        print("unknown subcommand", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
