#!/usr/bin/env python3
"""Exercise real window scheduling methods without requiring a display server.

GTK imports are real. The host stands in only for an unconstructed window;
socket work and GLib completion delivery still run on their actual threads.
"""
import pathlib
import sys
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from gi.repository import GLib
from jamsys_ui.app import MainWindow, ReportPage


def pump(predicate, timeout=2):
    deadline = time.monotonic() + timeout
    context = GLib.MainContext.default()
    while time.monotonic() < deadline:
        for _ in range(100):
            if not context.pending():
                break
            context.iteration(False)
        if predicate():
            return True
        time.sleep(.005)
    return False


class WindowLifecycle(unittest.TestCase):
    def test_connect_and_subscribe_are_off_the_main_thread_and_single_flight(self):
        main_thread = threading.get_ident()
        release = threading.Event()
        calls, refreshes = [], []

        def connect():
            calls.append(("connect", threading.get_ident()))
            release.wait(1)
            return True

        def subscribe(topics):
            calls.append(("subscribe", threading.get_ident()))
            self.assertEqual(topics, ["alert", "event"])

        host = SimpleNamespace(_closed=False, _connecting=False,
                               client=SimpleNamespace(connect=connect, subscribe=subscribe),
                               refresh=lambda: refreshes.append(threading.get_ident()))
        MainWindow._connect_async(host)
        MainWindow._connect_async(host)
        release.set()
        self.assertTrue(pump(lambda: not host._connecting))
        self.assertEqual([op for op, _ in calls], ["connect", "subscribe"])
        self.assertTrue(all(tid != main_thread for _, tid in calls))
        self.assertEqual(refreshes, [main_thread])

    def test_late_connect_completion_cannot_refresh_a_closed_window(self):
        work, done = [], []
        host = SimpleNamespace(_closed=False, _connecting=False,
                               refresh=lambda: self.fail("refreshed a destroyed window"))
        with patch("jamsys_ui.app.run_async", lambda fn, cb: (work.append(fn), done.append(cb))):
            MainWindow._connect_async(host)
            host._closed = True
            done[0](True, None)
            MainWindow._connect_async(host)
        self.assertEqual(len(work), 1)

    def test_close_removes_the_retry_timer(self):
        removed, stopped = [], []
        host = SimpleNamespace(_closed=False, _tick_source=123,
                               client=SimpleNamespace(shutdown=lambda: stopped.append(True)))
        with patch("jamsys_ui.app.GLib.source_remove", removed.append):
            self.assertFalse(MainWindow._on_close(host))
            self.assertFalse(MainWindow._tick(host))
        self.assertEqual(removed, [123])
        self.assertEqual(stopped, [True])
        self.assertTrue(host._closed)

    def test_report_completion_is_throttled_and_uses_interaction_safe_update(self):
        callbacks, updates = [], []
        win = SimpleNamespace(_closed=False, client=SimpleNamespace(connected=True),
                              last_snapshot={})
        host = SimpleNamespace(_pending=False, _summary=None, _fetched_at=0,
                               win=win, update=lambda snap: updates.append(snap))
        win.current_page = lambda: host
        with patch("jamsys_ui.app.run_async", lambda fn, cb: callbacks.append(cb)):
            ReportPage._fetch(host)
            ReportPage._fetch(host)
            self.assertEqual(len(callbacks), 1)
            callbacks[0]({"verdict": "unknown"}, None)
            ReportPage._fetch(host)
        self.assertEqual(len(callbacks), 1)
        self.assertEqual(updates, [{}])
        self.assertLess(time.monotonic() - host._fetched_at, .5)


if __name__ == "__main__":
    unittest.main()
