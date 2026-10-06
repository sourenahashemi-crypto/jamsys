#!/usr/bin/env python3
"""Real socket regressions for shutdown, failed handshakes and daemon restarts."""
import json
import pathlib
import socket
import sys
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from jamsys_ui.client import Client, DaemonError


class Peer:
    def __init__(self, handler=None):
        self.tmp = tempfile.TemporaryDirectory(prefix="js-client-")
        self.path = self.tmp.name + "/sock"
        self.peers = []
        self.requests = threading.Event()
        self.handler = handler

    def open(self, *_):
        # socketpair exercises the real kernel stream and buffered IO without
        # needing permission to bind a listener in a constrained test runner.
        local, remote = socket.socketpair()
        self.peers.append(remote)
        threading.Thread(target=self.serve, args=(remote,), daemon=True).start()

        class ConnectedSocket:
            def connect(self, _path):
                pass

            def __getattr__(self, name):
                return getattr(local, name)

        return ConnectedSocket()

    def serve(self, sock):
        try:
            with sock.makefile("rb") as stream:
                for line in stream:
                    req = json.loads(line)
                    if self.handler and self.handler(sock, req):
                        continue
                    if req["op"] == "hold":
                        self.requests.set()
                        continue
                    data = {"schema": 1} if req["op"] == "ping" else req["params"]
                    sock.sendall((json.dumps({"id": req["id"], "ok": True,
                                              "data": data}) + "\n").encode())
        except (OSError, ValueError):
            pass

    def close(self):
        for sock in self.peers:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            sock.close()
        self.tmp.cleanup()


class Lifecycle(unittest.TestCase):
    def setUp(self):
        self.peer = Peer()
        self.path = patch("jamsys_ui.client.socket_path", lambda: self.peer.path)
        self.path.start()
        self.transport = patch("jamsys_ui.client.socket", SimpleNamespace(
            socket=lambda *args: self.peer.open(*args), AF_UNIX=socket.AF_UNIX,
            SOCK_STREAM=socket.SOCK_STREAM, SHUT_RDWR=socket.SHUT_RDWR,
            MSG_DONTWAIT=socket.MSG_DONTWAIT))
        self.transport.start()
        self.client = Client()

    def tearDown(self):
        self.peer.close()
        self.client.shutdown()
        self.path.stop()
        self.transport.stop()

    def test_shutdown_unblocks_idle_reader(self):
        self.assertTrue(self.client.connect())
        time.sleep(.03)
        worker = threading.Thread(target=self.client.shutdown, daemon=True)
        worker.start()
        worker.join(.5)
        self.assertFalse(worker.is_alive(), "closing the UI deadlocks on readline()")
        self.assertFalse(self.client.connected)

    def test_disconnect_releases_inflight_request(self):
        self.assertTrue(self.client.connect())
        errors = []

        def call():
            try:
                self.client.call("hold")
            except DaemonError as err:
                errors.append(str(err))

        worker = threading.Thread(target=call, daemon=True)
        worker.start()
        self.assertTrue(self.peer.requests.wait(1))
        self.peer.close()
        worker.join(.5)
        self.assertFalse(worker.is_alive(), "lost requests wait the full timeout")
        self.assertTrue(errors)
        self.assertEqual(self.client._pending, {})

    def test_shutdown_is_terminal(self):
        self.client.shutdown()
        self.assertFalse(self.client.connect(), "a late retry reopens a closed UI")

    def test_broken_handshake_is_a_connection_failure(self):
        def close(sock, req):
            sock.shutdown(socket.SHUT_RDWR)
            return True
        self.peer.handler = close
        self.assertFalse(self.client.connect())
        self.assertFalse(self.client.connected)
        self.assertEqual(self.client._pending, {})

    def test_malformed_frames_do_not_kill_the_reader(self):
        self.assertTrue(self.client.connect())
        self.peer.peers[0].sendall(b'null\n[]\n42\n{"id": []}\nnot-json\n')
        self.assertEqual(self.client.call("echo", n=42), {"n": 42})

    def test_reconnect_uses_a_new_reader(self):
        self.assertTrue(self.client.connect())
        self.peer.close()
        deadline = time.monotonic() + 1
        while self.client.connected and time.monotonic() < deadline:
            time.sleep(.005)
        self.peer = Peer()
        self.assertTrue(self.client.connect())
        self.assertEqual(self.client.call("echo", n=7), {"n": 7})

    def test_concurrent_connects_share_one_handshake(self):
        results = []
        workers = [threading.Thread(target=lambda: results.append(self.client.connect()))
                   for _ in range(8)]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join(2)
        self.assertEqual(results, [True] * 8)
        self.assertEqual(len(self.peer.peers), 1)

    def test_concurrent_calls_keep_their_responses(self):
        self.assertTrue(self.client.connect())
        results = {}

        def call(n):
            results[n] = self.client.call("echo", number=n)

        workers = [threading.Thread(target=call, args=(n,)) for n in range(20)]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join(2)
        self.assertEqual(results, {n: {"number": n} for n in range(20)})

    def test_old_reader_cannot_close_a_replacement(self):
        self.assertTrue(self.client.connect())
        old = self.client._sock
        self.client._close(old)
        self.assertTrue(self.client.connect())
        self.client._close(old)
        self.assertTrue(self.client.connected)
        self.assertEqual(self.client.call("echo", current=True), {"current": True})

    def test_non_finite_requests_are_rejected_without_leaking_waiters(self):
        self.assertTrue(self.client.connect())
        with self.assertRaises(DaemonError):
            self.client.call("set_threshold", value=float("nan"))
        self.assertEqual(self.client._pending, {})
        self.assertTrue(self.client.connected)

    def test_idle_and_slow_replies_do_not_inherit_the_handshake_timeout(self):
        self.assertTrue(self.client.connect())
        time.sleep(5.2)
        self.assertTrue(self.client.connected)

        def slow(sock, req):
            if req["op"] == "slow":
                time.sleep(5.2)
            return False

        self.peer.handler = slow
        self.assertEqual(self.client.call("slow", answer=42), {"answer": 42})
        self.assertTrue(self.client.connected)


if __name__ == "__main__":
    unittest.main()
