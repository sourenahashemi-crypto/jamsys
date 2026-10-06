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
import select
import time
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
    """A connection is published only after its handshake and retired by identity.

    One reader owns each socket. A retiring reader must never close the socket
    that replaced it, and closing the window must wake blocked reads and calls.
    """

    def __init__(self, on_push: Optional[Callable[[str, dict], None]] = None,
                 on_state: Optional[Callable[[bool, str], None]] = None):
        self._sock: Optional[socket.socket] = None
        self._file = None
        self._lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._connect_lock = threading.Lock()
        self._next_id = 1
        self._on_push = on_push
        self._on_state = on_state
        self._connected = False
        self._announced: Optional[bool] = None
        self._stop = threading.Event()
        self._reader: Optional[threading.Thread] = None
        self._pending: dict[int, queue.Queue] = {}

    def connect(self) -> bool:
        with self._connect_lock:
            with self._lock:
                if self._stop.is_set():
                    return False
                if self._sock is not None:
                    return True
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            stream = None
            try:
                sock.settimeout(5.0)
                sock.connect(socket_path())
                stream = sock.makefile("rb")
                sock.sendall(b'{"id":0,"op":"ping","params":{}}\n')
                # The deadline applies to the whole handshake, including pushes
                # or malformed frames arriving before the ping response.
                deadline = time.monotonic() + 5.0
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise DaemonError("The monitoring service did not complete its handshake")
                    sock.settimeout(remaining)
                    line = stream.readline()
                    if not line:
                        raise DaemonError("The monitoring service closed the connection")
                    reply = self._decode(line)
                    if reply is not None and reply.get("id") == 0:
                        break
                data = self._result(reply)
                if not isinstance(data, dict) or not isinstance(data.get("schema"), int):
                    raise DaemonError("The monitoring service sent an invalid handshake")
                if data["schema"] != SCHEMA:
                    raise DaemonError(
                        f"The monitoring service speaks protocol {data['schema']} but this "
                        f"interface understands {SCHEMA}. Update JamSys.")
                sock.settimeout(None)
            except (OSError, ValueError, DaemonError) as err:
                sock.close()
                if stream:
                    stream.close()
                with self._lock:
                    if not self._stop.is_set():
                        self._set_state(False, f"Cannot reach the monitoring service: {err}")
                return False

            with self._lock:
                if self._stop.is_set():
                    stream.close()
                    sock.close()
                    return False
                self._sock, self._file = sock, stream
                self._reader = threading.Thread(target=self._read_loop, args=(sock, stream),
                                                daemon=True, name="jamsys-ipc")
                self._set_state(True, f"Connected to jamsysd {data.get('version', '?')}")
                self._reader.start()
            return True

    def _set_state(self, ok: bool, msg: str) -> None:
        # Call while holding _lock so queued state changes follow socket order.
        self._connected = ok
        if ok == self._announced:
            return
        self._announced = ok
        if self._on_state and not self._stop.is_set():
            GLib.idle_add(self._deliver_state, ok, msg)

    def _deliver_state(self, ok, msg):
        if not self._stop.is_set():
            self._on_state(ok, msg)
        return False

    def _close(self, expected=None, message="The monitoring service disconnected. Retrying…"):
        with self._lock:
            if expected is not None and self._sock is not expected:
                return
            sock, stream = self._sock, self._file
            self._sock = self._file = None
            pending, self._pending = self._pending, {}
            self._set_state(False, message)
        for q in pending.values():
            try:
                q.put_nowait(DaemonError(message))
            except queue.Full:
                pass
        # BufferedReader.close() alone waits for readline()'s lock forever.
        # shutdown first wakes that read; close outside _lock lets it retire.
        if sock is not None:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            sock.close()
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass

    @property
    def connected(self) -> bool:
        return self._connected

    @staticmethod
    def _decode(line):
        try:
            value = json.loads(line)
        except (ValueError, UnicodeError):
            return None
        return value if isinstance(value, dict) else None

    @staticmethod
    def _result(reply):
        if reply.get("ok") is not True:
            err = reply.get("error")
            raise DaemonError(err.get("message", "unknown error")
                              if isinstance(err, dict) else "invalid response")
        return reply.get("data") if reply.get("data") is not None else {}

    def call(self, op: str, **params) -> dict:
        """Synchronous request. Must not be called from the GTK main thread."""
        with self._lock:
            sock = self._sock
            if sock is None or self._stop.is_set():
                raise DaemonError("not connected")
            rid = self._next_id
            self._next_id += 1
            q: queue.Queue = queue.Queue(maxsize=1)
            self._pending[rid] = q
        try:
            try:
                payload = (json.dumps({"id": rid, "op": op, "params": params},
                                      allow_nan=False) + "\n").encode()
            except (ValueError, TypeError) as err:
                raise DaemonError(f"invalid request: {err}") from err
            if len(payload) - 1 > 64 * 1024:
                raise DaemonError("request exceeded 64 KiB")
            try:
                # Separate read and write buffers: a blocking readline must not
                # hold the lock needed to send the request it is waiting for.
                with self._write_lock:
                    deadline = time.monotonic() + 10.0
                    view = memoryview(payload)
                    while view:
                        remaining = deadline - time.monotonic()
                        if remaining <= 0 or not select.select([], [sock], [], remaining)[1]:
                            raise OSError("request write timed out")
                        try:
                            sent = sock.send(view, socket.MSG_DONTWAIT)
                        except BlockingIOError:
                            continue
                        if not sent:
                            raise OSError("connection closed during write")
                        view = view[sent:]
            except (OSError, ValueError) as err:
                self._close(sock)
                raise DaemonError(f"write failed: {err}") from err
            try:
                response = q.get(timeout=10)
            except queue.Empty:
                self._close(sock, "The monitoring service did not answer in time. Retrying…")
                raise DaemonError("the monitoring service did not answer in time") from None
            if isinstance(response, DaemonError):
                raise response
            return self._result(response)
        finally:
            with self._lock:
                self._pending.pop(rid, None)

    def subscribe(self, topics: list[str]) -> None:
        self.call("subscribe", topics=topics)

    def _deliver_push(self, sock, topic, data):
        if not self._stop.is_set() and self._sock is sock:
            self._on_push(topic, data)
        return False

    def _read_loop(self, sock, stream) -> None:
        try:
            while not self._stop.is_set():
                line = stream.readline()
                if not line:
                    break
                reply = self._decode(line)
                if reply is None:
                    continue
                if "push" in reply:
                    if self._on_push and isinstance(reply["push"], str):
                        GLib.idle_add(self._deliver_push, sock, reply["push"],
                                      reply.get("data") or {})
                    continue
                rid = reply.get("id")
                if not isinstance(rid, int):
                    continue
                with self._lock:
                    if self._sock is not sock:
                        break
                    q = self._pending.get(rid)
                if q is not None:
                    try:
                        q.put_nowait(reply)
                    except queue.Full:
                        pass
        except (OSError, ValueError):
            pass
        finally:
            self._close(sock)

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
