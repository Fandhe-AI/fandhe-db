"""scripts/crash_test_replication_proxy.py（Issue #2178・RECOVER-15 ポインタ）の単体試験。

ループバックの echo サーバーを中継先にして、中継・SIGUSR1 の接続断・SIGUSR2 の停滞・
中継先なしでの即時切断を確かめる。標準ライブラリのみ。
"""

import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

PROXY = Path(__file__).resolve().parents[1] / "crash_test_replication_proxy.py"


def echo_server():
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(8)

    def serve():
        while True:
            try:
                c, _ = srv.accept()
            except OSError:
                return

            def handle(c=c):
                with c:
                    while True:
                        d = c.recv(4096)
                        if not d:
                            return
                        c.sendall(d)

            threading.Thread(target=handle, daemon=True).start()

    threading.Thread(target=serve, daemon=True).start()
    return srv


class ProxyTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.target = os.path.join(self.tmp.name, "target")
        self.echo = echo_server()
        self.proc = subprocess.Popen(
            [sys.executable, "-I", str(PROXY), "--listen", "127.0.0.1:0", "--target-file", self.target],
            stdout=subprocess.PIPE,
            text=True,
        )
        line = self.proc.stdout.readline().strip()
        self.assertTrue(line.startswith("PROXY_PORT="), line)
        self.port = int(line.split("=")[1])

    def tearDown(self):
        self.proc.terminate()
        self.proc.wait(timeout=10)
        self.proc.stdout.close()
        self.echo.close()
        self.tmp.cleanup()

    def set_target(self, text):
        Path(self.target).write_text(text)

    def connect(self):
        c = socket.create_connection(("127.0.0.1", self.port), timeout=3)
        return c

    def test_relays_both_directions(self):
        self.set_target(str(self.echo.getsockname()[1]))
        with self.connect() as c:
            c.sendall(b"hello")
            self.assertEqual(c.recv(16), b"hello")

    def test_closes_immediately_without_target(self):
        for content in (None, "", "abc", "0", "70000"):
            if content is not None:
                self.set_target(content)
            with self.connect() as c:
                self.assertEqual(c.recv(16), b"")

    def test_sigusr1_cuts_connections(self):
        self.set_target(str(self.echo.getsockname()[1]))
        with self.connect() as c:
            c.sendall(b"a")
            self.assertEqual(c.recv(4), b"a")
            self.proc.send_signal(signal.SIGUSR1)
            self.assertEqual(self.proc.stdout.readline().strip(), "CUT 1")
            self.assertEqual(c.recv(4), b"")
        # リスナーは維持され、新しい接続は通る。
        with self.connect() as c:
            c.sendall(b"b")
            self.assertEqual(c.recv(4), b"b")

    def test_sigusr2_pauses_and_resumes(self):
        self.set_target(str(self.echo.getsockname()[1]))
        with self.connect() as c:
            self.proc.send_signal(signal.SIGUSR2)
            self.assertEqual(self.proc.stdout.readline().strip(), "PAUSE on")
            c.sendall(b"z")
            c.settimeout(0.5)
            with self.assertRaises(socket.timeout):
                c.recv(4)
            self.proc.send_signal(signal.SIGUSR2)
            self.assertEqual(self.proc.stdout.readline().strip(), "PAUSE off")
            c.settimeout(3)
            self.assertEqual(c.recv(4), b"z")

    def test_rejects_non_loopback_listen(self):
        r = subprocess.run(
            [sys.executable, "-I", str(PROXY), "--listen", "0.0.0.0:0", "--target-file", self.target],
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(r.returncode, 2)


if __name__ == "__main__":
    unittest.main()
