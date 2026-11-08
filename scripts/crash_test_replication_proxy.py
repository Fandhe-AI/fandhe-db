#!/usr/bin/env python3
"""Issue #2178（RECOVER-15 ポインタ）のレプリケーション障害注入試験用の TCP 中継。

`scripts/crash_test_replication.sh` が、読み取りレプリカ（`--replica-primary`）とプライマリの
レプリケーション用エンドポイントの間に置く。プライマリは再起動のたびにポートが変わるため、中継先を
ファイルから都度読む。両プロセスを生かしたままの接続断（SIGUSR1）と通信の停滞（SIGUSR2 で pause の
切り替え）を注入する。標準ライブラリのみ・ループバック専用で、通過したデータの内容は出力しない。

- `--listen <127.0.0.1:port>`: ループバックだけ受理する（0 で空きポート）
- `--target-file <path>`: 中継先ポート（1〜65535 の数字だけ）。空・不正なら受けた接続を即時に閉じる
- 標準出力: `PROXY_PORT=<n>` / `CUT <n>` / `PAUSE on|off`
"""

import argparse
import ipaddress
import selectors
import signal
import socket
import sys

CHUNK = 64 * 1024
# 対ごとの未送信バッファの上限。超えたら読むのを止めてメモリを有界にする。
BUF_LIMIT = 1024 * 1024


class Pair:
    """中継中の 2 接続（クライアント側・中継先側）と、各ソケットへ送る待ちデータ。"""

    def __init__(self, a, b):
        self.socks = (a, b)
        self.bufs = {a: b"", b: b""}

    def other(self, s):
        return self.socks[1] if s is self.socks[0] else self.socks[0]


def read_target(path):
    """中継先ポートを返す。不正・空なら None。"""
    try:
        with open(path, "r", encoding="ascii") as f:
            raw = f.read(16).strip()
    except (OSError, UnicodeDecodeError):
        return None
    if not raw.isdigit():
        return None
    port = int(raw)
    return port if 1 <= port <= 65535 else None


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", required=True)
    ap.add_argument("--target-file", required=True)
    args = ap.parse_args(argv[1:])
    host, _, port_s = args.listen.rpartition(":")
    try:
        if not ipaddress.ip_address(host).is_loopback:
            raise ValueError("not loopback")
        port = int(port_s)
    except ValueError:
        print("ERROR: --listen must be a loopback address", file=sys.stderr)
        return 2

    # シグナルハンドラはフラグを立てるだけにする。
    state = {"cut": False, "pause": False, "toggled": False, "stop": False}

    def on_usr1(*_):
        state["cut"] = True

    def on_usr2(*_):
        state["pause"] = not state["pause"]
        state["toggled"] = True

    def on_term(*_):
        state["stop"] = True

    signal.signal(signal.SIGUSR1, on_usr1)
    signal.signal(signal.SIGUSR2, on_usr2)
    signal.signal(signal.SIGTERM, on_term)

    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((host, port))
    listener.listen(16)
    listener.setblocking(False)
    sel = selectors.DefaultSelector()
    sel.register(listener, selectors.EVENT_READ, None)
    print("PROXY_PORT=%d" % listener.getsockname()[1], flush=True)

    pairs = set()
    cuts = 0

    def close_pair(p):
        pairs.discard(p)
        for s in p.socks:
            try:
                sel.unregister(s)
            except (KeyError, ValueError):
                pass
            s.close()

    def refresh(p):
        """上限・pause・未送信データの有無から各ソケットの監視イベントを決め直す。"""
        for s in p.socks:
            if s.fileno() < 0:
                return
            ev = 0
            if not state["pause"] and len(p.bufs[p.other(s)]) < BUF_LIMIT:
                ev |= selectors.EVENT_READ
            if p.bufs[s] and not state["pause"]:
                ev |= selectors.EVENT_WRITE
            try:
                sel.unregister(s)
            except (KeyError, ValueError):
                pass
            if ev:
                sel.register(s, ev, p)

    while not state["stop"]:
        if state["toggled"]:
            state["toggled"] = False
            print("PAUSE %s" % ("on" if state["pause"] else "off"), flush=True)
            for p in list(pairs):
                refresh(p)
        if state["cut"]:
            state["cut"] = False
            cuts += 1
            for p in list(pairs):
                close_pair(p)
            print("CUT %d" % cuts, flush=True)
        for key, mask in sel.select(timeout=0.05):
            if key.data is None:
                try:
                    c, _ = listener.accept()
                except OSError:
                    continue
                tport = read_target(args.target_file)
                if tport is None:
                    c.close()
                    continue
                try:
                    u = socket.create_connection(("127.0.0.1", tport), timeout=2)
                except OSError:
                    c.close()
                    continue
                c.setblocking(False)
                u.setblocking(False)
                p = Pair(c, u)
                pairs.add(p)
                refresh(p)
                continue
            p, s = key.data, key.fileobj
            if p not in pairs:
                continue
            try:
                if mask & selectors.EVENT_READ:
                    data = s.recv(CHUNK)
                    if not data:
                        close_pair(p)
                        continue
                    p.bufs[p.other(s)] += data
                if mask & selectors.EVENT_WRITE and p.bufs[s]:
                    n = s.send(p.bufs[s][:CHUNK])
                    p.bufs[s] = p.bufs[s][n:]
            except (BlockingIOError, InterruptedError):
                pass
            except OSError:
                close_pair(p)
                continue
            refresh(p)
    for p in list(pairs):
        close_pair(p)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
