#!/usr/bin/env python3
"""Process filtering uses only this snapshot and preserves honest top-N scope."""
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from jamsys_ui.processes import select_processes


class ProcessSelection(unittest.TestCase):
    def setUp(self):
        self.a = {"pid": 12, "name": "Browser & Helper", "cpu_pct": 120, "rss_bytes": 100}
        self.b = {"pid": 7, "name": "Editor", "cpu_pct": 2, "rss_bytes": 900}
        self.sample = {"top_cpu": [self.a, self.b], "top_mem": [self.b, self.a]}

    def test_deduplicates_without_mutating_the_sample(self):
        self.assertEqual(select_processes(self.sample), [self.a, self.b])
        self.assertEqual(self.sample["top_mem"], [self.b, self.a])

    def test_memory_sort(self):
        self.assertEqual(select_processes(self.sample, sort="memory"), [self.b, self.a])

    def test_name_sort(self):
        self.assertEqual(select_processes(self.sample, sort="name"), [self.a, self.b])

    def test_search_name_or_pid(self):
        self.assertEqual(select_processes(self.sample, " BROWSER & "), [self.a])
        self.assertEqual(select_processes(self.sample, "7"), [self.b])
        self.assertEqual(select_processes(self.sample, "missing"), [])

    def test_missing_or_removed_samples(self):
        self.assertEqual(select_processes({}), [])
        self.assertEqual(select_processes({"top_cpu": None, "top_mem": [self.b]}), [self.b])


if __name__ == "__main__":
    unittest.main()
