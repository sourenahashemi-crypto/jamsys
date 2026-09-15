#!/usr/bin/env python3
"""The socket client, against a stub daemon.

This file exists because client.py had no tests at all, and the worst UI bug found
in review was in it: the 5-second connect timeout was inherited by the file object,
so an idle readline() raised TimeoutError, the reader thread exited, and the client
announced that a perfectly healthy daemon had disconnected. The window hid it by
polling every two seconds; anything using the documented push stream did not.

The stub speaks the real protocol over a real Unix socket, so these drive the same
code paths the daemon does.

    python3 jamsys-ui/tests/client-test.py
"""

import faulthandler
import json
import os
import pathlib
import socket
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import gi  # noqa: E402
gi.require_version("Gtk", "4.0")
from gi.repository import GLib  # noqa: E402

from jamsys_ui import client as client_mod  # noqa: E402
from jamsys_ui.client import Client, DaemonError  # noqa: E402

faulthandler.dump_traceback_later(90, exit=True)

passed = failed = 0


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name}  {detail}")


class StubDaemon:
    """Answers ping/snapshot, and stays silent unless asked to push."""

    def __init__(self, tmpdir):
        self.path = os.path.join(tmpdir, "sock")
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(self.path)
        self.srv.listen(4)
        self.conns = []
        self.requests = []
        self.stop = threading.Event()
        threading.Thread(target=self._accept, daemon=True).start()

    def _accept(self):
        while not self.stop.is_set():
            try:
                c, _ = self.srv.accept()
            except OSError:
                return
            self.conns.append(c)
            threading.Thread(target=self._serve, args=(c,), daemon=True).start()

    def _serve(self, c):
        f = c.makefile("rwb")
        while not self.stop.is_set():
            line = f.readline()
            if not line:
                return
            try:
                req = json.loads(line)
            except ValueError:
                continue
            self.requests.append(req["op"])
            if req["op"] == "ping":
                body = {"version": "stub", "schema": 1, "pid": os.getpid()}
            elif req["op"] == "slow":
                time.sleep(7.0)          # longer than the old 5 s read timeout
                body = {"woke": True}
            else:
                body = {"op": req["op"], "echo": req.get("params", {})}
            try:
                f.write((json.dumps({"id": req.get("id"), "ok": True, "data": body}) + "\n").encode())
                f.flush()
            except OSError:
                return

    def push(self, topic, data):
        for c in list(self.conns):
            try:
                c.sendall((json.dumps({"push": topic, "data": data}) + "\n").encode())
            except OSError:
                pass

    def close(self):
        self.stop.set()
        # shutdown(), not close(). socket.close() with an outstanding makefile()
        # does not release the descriptor, so the peer would never see EOF; and
        # closing the file object from here deadlocks on the buffered-IO lock held
        # by the serving thread sitting in readline(). shutdown() unblocks that
        # thread and gives the peer its EOF.
        for c in self.conns:
            try:
                c.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                c.close()
            except OSError:
                pass
        self.srv.close()


def pump(until, seconds=6.0):
    ctx = GLib.MainContext.default()
    deadline = time.monotonic() + seconds
    while True:
        while ctx.pending():
            ctx.iteration(False)
        if until():
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.02)


tmp = tempfile.mkdtemp(prefix="jamsys-client-test-")
daemon = StubDaemon(tmp)
client_mod.socket_path = lambda: daemon.path

print("an idle connection stays up")
states = []
c = Client(on_state=lambda ok, msg: states.append((ok, msg)))
check("connects", c.connect() and c.connected)
c.subscribe(["snapshot"])
# The bug fired at exactly 5.0 s. Wait comfortably past it, sending nothing.
t0 = time.monotonic()
pump(lambda: not c.connected, seconds=8.0)
check("still connected after 8 idle seconds", c.connected,
      f"dropped after {time.monotonic() - t0:.1f}s")
check("and the reader thread is alive", c._reader.is_alive())
check("no spurious disconnect was announced",
      not any(ok is False for ok, _ in states), str(states))

print("\na request slower than the old timeout still answers")
t0 = time.monotonic()
try:
    r = c.call("slow")
    took = time.monotonic() - t0
    check("a 7-second call returns its result", r.get("woke") is True, str(r))
    check("and the connection survived it", c.connected, f"took {took:.1f}s")
except DaemonError as e:
    check("a 7-second call returns its result", False, str(e))

print("\npushes arrive while the client is otherwise idle")
got = []
c2 = Client(on_push=lambda topic, data: got.append((topic, data)))
c2.connect()
c2.subscribe(["snapshot"])
pump(lambda: False, seconds=0.3)
daemon.push("snapshot", {"health": "healthy"})
check("the push is delivered", pump(lambda: len(got) == 1), str(got))
if got:
    check("with its topic and payload", got[0][0] == "snapshot"
          and got[0][1].get("health") == "healthy", str(got[0]))

print("\nconcurrent calls each get their own answer")
results = {}


def worker(i):
    try:
        results[i] = c.call("echo", n=i)
    except DaemonError as e:
        results[i] = e


threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
for t in threads:
    t.start()
for t in threads:
    t.join(timeout=15)
check("all eight answered", len(results) == 8, str(len(results)))
check("and none got another caller's reply",
      all(not isinstance(v, Exception) and v.get("echo", {}).get("n") == i
          for i, v in results.items()),
      str(results))

print("\na failed reconnect is announced once, not once per attempt")
c3states = []
c3 = Client(on_state=lambda ok, msg: c3states.append(ok))
client_mod.socket_path = lambda: os.path.join(tmp, "nothing-here")
for _ in range(5):
    c3.connect()
pump(lambda: False, seconds=0.3)
check("five failed attempts produce one banner", c3states.count(False) == 1,
      f"got {c3states}")

print("\nthe daemon going away is noticed")
client_mod.socket_path = lambda: daemon.path
c4 = Client()
c4.connect()
check("connected to the stub", c4.connected)
daemon.close()
check("and the drop is seen", pump(lambda: not c4.connected, seconds=6.0))

for cl in (c, c2, c3, c4):
    cl.shutdown()
try:
    os.unlink(daemon.path)
except OSError:
    pass
os.rmdir(tmp)

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
