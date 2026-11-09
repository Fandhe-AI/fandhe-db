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

Issue #3153（RECOVER-14 ポインタ）の PITR 試験向けに `clock` を追加している:
- `clock`: server の `clock_timestamp()` を 1 本読み、`CLOCK_US=<UTC エポックからの µs>` を出す。
  変更ログの commit 時刻と同じ時計（server の壁時計）で時刻指定の復元点を決めるために使う。

Issue #2178（RECOVER-15 ポインタ）のレプリケーション障害注入試験
（`scripts/crash_test_replication.sh`）向けに、以下を追加している（上の 3 つの挙動は変えない）:
- `rsetup`: 3 表（docs・kv・markers）を作り、種の行を入れる（alice 用。DDL 権限が要る）。
- `rseed <id-base>`: 呼び出したユーザー自身の種の行を入れる（テナントごとに id の範囲を分ける）。
- `churn <id-base>`: 1 文 1 commit の INSERT・UPDATE・DELETE を回し続け、`ACK <seq>` を出す。
- `ddl <step>`: 段の途中の DDL（1: 表の追加と列の追加。2: 同期の段の synclog 表）。
- `marker <id>` / `wait-marker <id> <timeout-secs>`: 追従の目印の書き込みと待機（レプリカ側）。
- `fingerprint <table:pk>...`: 表ごとの列名と全行の sha256（キーセット方式でページング）。
- `sync-append <start-id>`: synclog へ 1 行ずつ INSERT し `ACK <id>`。結果不明は `MAYBE <id>`。
- `ids-present <table> <lo> <hi>`: lo..hi の id がすべて在るか（`PRESENT=<n>`）。
- `churn-verify <id-base> <last-ack>`: `churn` の確認済み操作列から導いた docs・kv の期待状態と照合する。
"""

import calendar
import hashlib
import re
import socket
import struct
import sys
import time

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
    return query_cols(sock, sql)[1]


def query_cols(sock, sql):
    """`query` と同じだが、RowDescription の列名のリストも `(列名, 行)` で返す（#2178 の fingerprint 用）。"""
    data = sql.encode() + b"\0"
    sock.sendall(b"Q" + struct.pack("!I", len(data) + 4) + data)
    rows = []
    cols = []
    err = None
    while True:
        ty, payload = _read_msg(sock)
        if ty == b"T":
            (n,) = struct.unpack("!H", payload[:2])
            off = 2
            cols = []
            for _ in range(n):
                end = payload.index(b"\0", off)
                cols.append(payload[off:end].decode())
                off = end + 1 + 18
        elif ty == b"D":
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
            return cols, rows


_TS_RE = re.compile(
    r"^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,6}))?(?:([+-])(\d{2})(?::?(\d{2}))?)?$"
)


def parse_timestamptz_us(text):
    """`YYYY-MM-DD HH:MM:SS[.f][+HH[:MM]]` を UTC エポックからの µs へ変換する。形式外・範囲外は ValueError。

    この server の timestamp はゾーン表記を付けず UTC の壁時計で返す（実測）ため、ゾーン省略は UTC と読む。

    server が返す時刻の文字列を厳密に解析し、黙って丸めない（fail-closed）。float は使わない。
    """
    m = _TS_RE.match(text)
    if m is None:
        raise ValueError("unexpected timestamptz format: %r" % (text,))
    year, mon, day, hour, minute, sec = (int(m.group(i)) for i in range(1, 7))
    frac = (m.group(7) or "").ljust(6, "0")
    off_h = int(m.group(9) or 0)
    off_m = int(m.group(10) or 0)
    if not (1 <= mon <= 12 and 1 <= day <= calendar.monthrange(year, mon)[1]):
        raise ValueError("date out of range: %r" % (text,))
    if hour > 23 or minute > 59 or sec > 59 or off_h > 23 or off_m > 59:
        raise ValueError("time out of range: %r" % (text,))
    secs = calendar.timegm((year, mon, day, hour, minute, sec, 0, 0, 0))
    offset = (off_h * 3600 + off_m * 60) * (-1 if m.group(8) == "-" else 1)
    return (secs - offset) * 1000000 + int(frac)


def cmd_clock(sock):
    """server の壁時計を読んで `CLOCK_US=<µs>` を出す。結果が 1 行 1 列でなければ終了コード 1。"""
    rows = query(sock, "SELECT clock_timestamp() FROM docs WHERE id >= 0 LIMIT 1")
    if len(rows) != 1 or len(rows[0]) != 1 or rows[0][0] is None:
        print("clock: unexpected result shape: %r" % (rows,), file=sys.stderr)
        return 1
    try:
        us = parse_timestamptz_us(rows[0][0])
    except ValueError as e:
        print("clock: %s" % e, file=sys.stderr)
        return 1
    print("CLOCK_US=%d" % us, flush=True)
    return 0


# ---------------------------------------------------------------------------
# Issue #2178 の追加分
# ---------------------------------------------------------------------------

PAGE = 1000
CHURN_PAUSE_SECS = 0.02


def _vec(i):
    return "[%.1f,%.1f,%.1f]" % ((i % 7) / 10.0, (i % 11) / 10.0, (i % 13) / 10.0)


def _body(i):
    # NULL・多バイト文字・引用符を混ぜる（値は id から決まる）。
    if i % 5 == 0:
        return "NULL"
    if i % 5 == 1:
        return "'日本語-%d'" % i
    if i % 5 == 2:
        return "'it''s-%d'" % i
    return "'b%d'" % i


def _seed_rows(sock, base, n):
    for lo in range(0, n, 50):
        hi = min(lo + 50, n)
        vals = ",".join("(%d,'%s',%s)" % (base + i, _vec(i), _body(i)) for i in range(lo, hi))
        query(sock, "INSERT INTO docs (id, embedding, body) VALUES " + vals)
        vals = ",".join("(%d,%s,%d)" % (base + i, _body(i), i * 3) for i in range(lo, hi))
        query(sock, "INSERT INTO kv (k, v, n) VALUES " + vals)


def cmd_rsetup(sock):
    query(sock, "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3), body TEXT)")
    query(sock, "CREATE TABLE kv (k BIGINT PRIMARY KEY, v TEXT, n BIGINT)")
    query(sock, "CREATE TABLE markers (id BIGINT PRIMARY KEY, tag TEXT)")
    print("SETUP", flush=True)


def cmd_rseed(sock, base):
    _seed_rows(sock, base, 300)
    print("SEEDED", 300, flush=True)


def cmd_churn(sock, base, extra):
    """1 文 1 commit で docs・kv を回し続ける。値は seq から決まる。"""
    i = 0
    while True:
        k = base + i
        query(sock, "INSERT INTO docs (id, embedding, body) VALUES (%d,'%s',%s)" % (k, _vec(i), _body(i)))
        if extra:
            query(sock, "INSERT INTO kv (k, v, n, extra) VALUES (%d,%s,%d,'e%d')" % (k, _body(i), i, i))
        else:
            query(sock, "INSERT INTO kv (k, v, n) VALUES (%d,%s,%d)" % (k, _body(i), i))
        if i % 3 == 2:
            query(sock, "UPDATE kv SET v = 'u%d', n = n + 1 WHERE k = %d" % (i, k - 1))
        if i % 5 == 4:
            query(sock, "DELETE FROM kv WHERE k = %d" % (k - 4))
        print("ACK", i, flush=True)
        i += 1
        # レプリカの適用速度を超えて遅れの上限（既定 100000 レコード）へ達しないよう書き込み速度を抑える。
        time.sleep(CHURN_PAUSE_SECS)


def cmd_ddl(sock, step):
    if step == 1:
        query(sock, "CREATE TABLE late (id BIGINT PRIMARY KEY, note TEXT)")
        query(sock, "ALTER TABLE kv ADD COLUMN extra TEXT")
        query(sock, "INSERT INTO late (id, note) VALUES (1,'first')")
    elif step == 2:
        query(sock, "CREATE TABLE synclog (id BIGINT PRIMARY KEY, note TEXT)")
    else:
        raise SystemExit("unknown ddl step")
    print("DDL", step, flush=True)


def cmd_marker(sock, mid):
    query(sock, "INSERT INTO markers (id, tag) VALUES (%d,'m%d')" % (mid, mid))
    print("MARKER", mid, flush=True)


def cmd_wait_marker(sock, mid, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        rows = query(sock, "SELECT count(*) FROM markers WHERE id = %d" % mid)
        if rows and rows[0][0] == "1":
            print("SEEN", mid, flush=True)
            return 0
        time.sleep(0.1)
    print("TIMEOUT", mid, flush=True)
    return 1


def cmd_fingerprint(sock, specs):
    for spec in specs:
        name, _, pk = spec.partition(":")
        if not name.isidentifier() or not pk.isidentifier():
            raise SystemExit("bad table spec")
        h = hashlib.sha256()
        total = 0
        last = -1
        cols = None
        try:
            while True:
                cols, rows = query_cols(
                    sock,
                    "SELECT * FROM %s WHERE %s > %d ORDER BY %s LIMIT %d" % (name, pk, last, pk, PAGE),
                )
                if total == 0 and cols:
                    h.update(("|".join(cols) + "\n").encode())
                for r in rows:
                    for v in r:
                        h.update(b"\x00N" if v is None else b"\x01" + v.encode() + b"\x02")
                    h.update(b"\n")
                total += len(rows)
                if len(rows) < PAGE:
                    break
                last = int(rows[-1][cols.index(pk)])
        except RuntimeError:
            print("TABLE %s missing" % name, flush=True)
            continue
        print("TABLE %s rows=%d sha256=%s" % (name, total, h.hexdigest()), flush=True)


def cmd_sync_append(sock, start):
    i = start
    while True:
        try:
            query(sock, "INSERT INTO synclog (id, note) VALUES (%d,'s%d')" % (i, i))
        except (RuntimeError, OSError, ConnectionError, ValueError):
            print("MAYBE", i, flush=True)
            return 3
        print("ACK", i, flush=True)
        i += 1


def cmd_ids_present(sock, table, lo, hi):
    if not table.isidentifier():
        raise SystemExit("bad table")
    rows = query(sock, "SELECT count(*) FROM %s WHERE id >= %d AND id <= %d" % (table, lo, hi))
    n = int(rows[0][0])
    print("PRESENT=%d" % n, flush=True)
    return 0 if n == hi - lo + 1 else 1


def _body_value(i):
    """`_body` が作る SQL リテラルに対応する、読み戻し時の値（NULL は None）。"""
    lit = _body(i)
    return None if lit == "NULL" else lit[1:-1].replace("''", "'")


def cmd_churn_verify(sock, base, last):
    """確認済み（ACK 済み）の最後の連番 `last` までの `churn` の結果が残っているかを検査する。

    docs の最終行と kv の最終行の存在、および kv の UPDATE・DELETE 後の状態を照合する。
    kill 時点で実行中だった次の操作（連番 last+1）が触れうる行（last と last-3）は
    検査から外し、それ以外を厳密に確認する。
    """
    if last < 0:
        return 0
    rows = query(sock, "SELECT count(*) FROM docs WHERE id = %d" % (base + last))
    if int(rows[0][0]) != 1:
        print("MISSING docs %d" % (base + last), flush=True)
        return 1
    rows = query(sock, "SELECT count(*) FROM kv WHERE k = %d" % (base + last))
    if int(rows[0][0]) != 1:
        print("MISSING kv %d" % (base + last), flush=True)
        return 1
    lo = max(0, last - 12)
    hi = last - 4
    if hi < lo:
        return 0
    got = {}
    for r in query(sock, "SELECT k, v, n FROM kv WHERE k >= %d AND k <= %d" % (base + lo, base + hi)):
        got[int(r[0])] = (r[1], int(r[2]))
    for j in range(lo, hi + 1):
        k = base + j
        if j % 5 == 0:
            if k in got:
                print("NOT-DELETED kv %d" % k, flush=True)
                return 1
            continue
        if (j + 1) % 3 == 2:
            want = ("u%d" % (j + 1), j + 1)
        else:
            want = (_body_value(j), j)
        if got.get(k) != want:
            print("MISMATCH kv %d got=%r want=%r" % (k, got.get(k), want), flush=True)
            return 1
    print("VERIFIED", last, flush=True)
    return 0


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
    elif cmd == "clock":
        return cmd_clock(sock)
    elif cmd == "rsetup":
        cmd_rsetup(sock)
    elif cmd == "rseed":
        cmd_rseed(sock, int(argv[5]))
    elif cmd == "churn":
        cmd_churn(sock, int(argv[5]), len(argv) > 6 and argv[6] == "extra")
    elif cmd == "ddl":
        cmd_ddl(sock, int(argv[5]))
    elif cmd == "marker":
        cmd_marker(sock, int(argv[5]))
    elif cmd == "wait-marker":
        return cmd_wait_marker(sock, int(argv[5]), float(argv[6]))
    elif cmd == "fingerprint":
        cmd_fingerprint(sock, argv[5:])
    elif cmd == "sync-append":
        return cmd_sync_append(sock, int(argv[5]))
    elif cmd == "ids-present":
        return cmd_ids_present(sock, argv[5], int(argv[6]), int(argv[7]))
    elif cmd == "churn-verify":
        return cmd_churn_verify(sock, int(argv[5]), int(argv[6]))
    else:
        print("unknown subcommand", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
