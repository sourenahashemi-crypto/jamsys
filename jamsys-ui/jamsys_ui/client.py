"""IPC client for jamsysd.

A thin, reconnecting client over the documented newline-delimited JSON protocol
(see docs/IPC-PROTOCOL.md). All socket work happens on a background thread; results
are delivered back on the GTK main loop via GLib.idle_add, because touching widgets
from another thread is the classic way to make a GTK app crash at random.
"""

from __future__ import annotations

import json
import os
import socket
import threading
import queue
from typing import Any, Callable, Optional

from gi.repository import GLib

SCHEMA = 1


def socket_path() -> str:
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    return os.path.join(runtime, "jamsys", "sock")


class DaemonError(Exception):
    pass


class Client:
    """Connects lazily and reconnects on failure.

    The UI must survive the daemon being restarted underneath it — that is the whole
    point of separating them — so every call path treats a dropped socket as a normal
    condition rather than an error to show the user.
    """

    def __init__(self, on_push: Optional[Callable[[str, dict], None]] = None,
                 on_state: Optional[Callable[[bool, str], None]] = None):
        self._sock: Optional[socket.socket] = None
        self._file = None
        self._lock = threading.Lock()
        self._next_id = 1
        self._on_push = on_push
        self._on_state = on_state
        self._connected = False
        self._stop = threading.Event()
        self._pushq: "queue.Queue[tuple[str, dict]]" = queue.Queue(maxsize=512)
        self._reader: Optional[threading.Thread] = None
        self._pending: dict[int, queue.Queue] = {}

    # -- connection ------------------------------------------------------

    def connect(self) -> bool:
        with self._lock:
            if self._sock is not None:
                return True
            p = socket_path()
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.settimeout(5.0)
                s.connect(p)
                self._sock = s
                self._file = s.makefile("rwb")
            except OSError as e:
                self._set_state(False, f"Cannot reach the monitoring service: {e.strerror or e}")
                return False
        # Verify the protocol before trusting anything else it says.
        try:
            r = self.call("ping")
        except DaemonError as e:
            self._close()
            self._set_state(False, str(e))
            return False
        if r.get("schema", 0) > SCHEMA:
            self._close()
            self._set_state(False,
                            f"The monitoring service speaks protocol {r['schema']} but this "
                            f"interface understands {SCHEMA}. Update the desktop app.")
            return False
        self._set_state(True, f"Connected to jamsysd {r.get('version','?')}")
        self._start_reader()
        return True

    def _set_state(self, ok: bool, msg: str) -> None:
        if ok == self._connected and ok:
            return
        self._connected = ok
        if self._on_state:
            GLib.idle_add(self._on_state, ok, msg)

    def _close(self) -> None:
        with self._lock:
            try:
                if self._file:
                    self._file.close()
            except OSError:
                pass
            try:
                if self._sock:
                    self._sock.close()
            except OSError:
                pass
            self._sock = None
            self._file = None

    @property
    def connected(self) -> bool:
        return self._connected

    # -- requests --------------------------------------------------------

    def call(self, op: str, **params) -> dict:
        """Synchronous request. Must not be called from the GTK main thread."""
        with self._lock:
            if self._file is None:
                raise DaemonError("not connected")
            rid = self._next_id
            self._next_id += 1
            q: queue.Queue = queue.Queue(maxsize=1)
            self._pending[rid] = q
            try:
                self._file.write((json.dumps({"id": rid, "op": op, "params": params}) + "\n").encode())
                self._file.flush()
            except OSError as e:
                self._pending.pop(rid, None)
                raise DaemonError(f"write failed: {e}") from e
            reader_running = self._reader is not None and self._reader.is_alive()

        if reader_running:
            try:
                resp = q.get(timeout=10)
            except queue.Empty:
                self._pending.pop(rid, None)
                raise DaemonError("the monitoring service did not answer in time")
        else:
            # Before the reader thread starts (the initial ping) read inline.
            resp = self._read_until(rid)
        self._pending.pop(rid, None)
        if not resp.get("ok"):
            err = resp.get("error") or {}
            raise DaemonError(err.get("message", "unknown error"))
        return resp.get("data") or {}

    def _read_until(self, rid: int) -> dict:
        while True:
            line = self._file.readline()
            if not line:
                raise DaemonError("the monitoring service closed the connection")
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if "push" in d:
                continue
            if d.get("id") == rid:
                return d

    def subscribe(self, topics: list[str]) -> None:
        try:
            self.call("subscribe", topics=topics)
        except DaemonError:
            pass

    # -- push stream -----------------------------------------------------

    def _start_reader(self) -> None:
        if self._reader and self._reader.is_alive():
            return
        self._reader = threading.Thread(target=self._read_loop, daemon=True, name="jamsys-ipc")
        self._reader.start()

    def _read_loop(self) -> None:
        f = self._file
        while not self._stop.is_set() and f is not None:
            try:
                line = f.readline()
            except OSError:
                break
            if not line:
                break
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if "push" in d:
                if self._on_push:
                    GLib.idle_add(self._on_push, d["push"], d.get("data") or {})
                continue
            rid = d.get("id")
            q = self._pending.get(rid)
            if q is not None:
                try:
                    q.put_nowait(d)
                except queue.Full:
                    pass
        self._close()
        self._set_state(False, "The monitoring service disconnected. Retrying…")

    def shutdown(self) -> None:
        self._stop.set()
        self._close()


def run_async(fn: Callable[[], Any], done: Callable[[Any, Optional[Exception]], None]) -> None:
    """Run `fn` on a worker thread and deliver the result on the GTK main loop."""
    def worker():
        try:
            r = fn()
            GLib.idle_add(done, r, None)
        except Exception as e:  # noqa: BLE001 - surfaced to the UI, never swallowed
            GLib.idle_add(done, None, e)
    threading.Thread(target=worker, daemon=True).start()
