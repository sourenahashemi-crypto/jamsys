#!/usr/bin/env python3
"""Tests for the report summariser.

The daemon's IPC replies are not uniformly shaped: `coverage` returns
{"collectors": [...]}, `inventory` returns {"items": [...]}, `alerts` returns
{"alerts": [...]}, and some return a bare list. Assuming one shape produced a
report that looked complete and silently omitted whole sections -- which is the
worst failure mode for something whose entire job is to tell you what is wrong.

    python3 jamsys-ui/tests/report-test.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from jamsys_ui import report  # noqa: E402

passed = failed = 0


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name}  {detail}")


print("reply shapes")
check("a bare list passes through", report.rows([1, 2]) == [1, 2])
check("coverage's {'collectors': [...]} is found",
      report.rows({"collectors": [1]}, "collectors") == [1])
check("inventory's {'items': [...]} is found",
      report.rows({"items": [1, 2]}, "items") == [1, 2])
check("an unnamed list inside a dict is still found",
      report.rows({"whatever": [7]}) == [7])
check("None yields an empty list", report.rows(None) == [])
check("a scalar yields an empty list", report.rows(42) == [])

print("\nverdict")
base = {"snapshot": {}}
s = report.summarise(base, [], {}, [], {})
check("no alerts reads as healthy", s["verdict"] == "healthy")
s = report.summarise(base, [{"severity": 2, "title": "x", "resolved_ts": None}], {}, [], {})
check("an open warning needs attention", s["verdict"] == "attention")
s = report.summarise(base, [{"severity": 3, "title": "x", "resolved_ts": None}], {}, [], {})
check("an open critical is critical", s["verdict"] == "critical")
s = report.summarise(base, [{"severity": 3, "title": "x", "resolved_ts": 123}], {}, [], {})
check("a resolved alert is not a problem", s["verdict"] == "healthy")

print("\nordering and selection")
alerts = [
    {"severity": 1, "title": "low", "resolved_ts": None, "last_ts": 900},
    {"severity": 3, "title": "worst", "resolved_ts": None, "last_ts": 100},
    {"severity": 2, "title": "mid", "resolved_ts": None, "last_ts": 800},
]
s = report.summarise(base, alerts, {}, [], {})
check("most severe first, not most recent",
      [a["title"] for a in s["alerts"]] == ["worst", "mid", "low"],
      str([a["title"] for a in s["alerts"]]))

print("\ncoverage gaps")
cov = {"collectors": [
    {"name": "cpu", "label": "Full"},
    {"name": "storage", "label": "Disabled"},
    {"name": "gpu", "label": "Unavailable", "last_error": "no driver"},
]}
s = report.summarise(base, [], cov, [], {})
names = [c["name"] for c in s["gaps"]]
check("a working collector is not a gap", "cpu" not in names)
check("an unavailable collector is a gap", "gpu" in names)
check("a disabled collector is listed too", "storage" in names,
      "the report exists to answer 'why am I getting no disk alerts?'")

print("\nmachine identity comes from the right places")
env = {"collectors_usable": 12, "collectors_total": 13,
       "snapshot": {"power": {"percent": 93.0, "charge_limit_pct": 80,
                              "on_battery": True}}}
inv = {"items": [{"key": "product.name", "value": "ASUS TUF"},
                 {"key": "kernel.release", "value": "7.0.0-31-generic"},
                 {"key": "cpu.model", "value": "i7-14650HX"}]}
s = report.summarise(env, [], {}, [], {"rss_bytes": 27262976, "uptime_s": 3700}, inv)
m = s["machine"]
check("model comes from inventory items", m["model"] == "ASUS TUF", m["model"])
check("kernel comes from inventory items", m["kernel"].startswith("7.0.0"), m["kernel"])
check("collector counts come from the snapshot envelope, not stats",
      m["collectors"] == "12/13", m["collectors"])
check("rss is converted from bytes", m["daemon_rss_mb"] == 26.0, str(m["daemon_rss_mb"]))
check("the charge limit is carried through", m["charge_limit"] == 80)

print("\nmarkdown")
md = report.to_markdown(s)
check("has a title", md.startswith("# JamSys report"))
check("names the machine", "ASUS TUF" in md)
check("states the battery and its limit", "93%" in md and "charge limit 80%" in md, )
check("warns before sharing", "read it before sharing" in md)
check("says nothing was sent anywhere", "sent anywhere" in md)

alerted = report.summarise(
    base,
    [{"severity": 3, "title": "CPU is critically hot", "resolved_ts": None,
      "first_ts": report._now_ms() - 60_000, "count": 3,
      "detail": {"what": "CPU package is 97 C.", "expected": "below 95 C",
                 "evidence": [{"label": "Fans", "value": "5200 rpm"}],
                 "likely_cause": "a runaway process",
                 "actions": ["Check the Processes page"]}}],
    {}, [], {})
md = report.to_markdown(alerted)
for want in ("CPU is critically hot", "critical", "97 C", "below 95 C",
             "5200 rpm", "runaway process", "Check the Processes page",
             "Occurrences: 3"):
    check(f"the report carries {want!r}", want in md)

print("\nplain text")
plain = report.to_plain(alerted)
check("markdown markers are stripped", "**" not in plain and "# " not in plain)
check("but the content survives", "CPU is critically hot" in plain)

print("\nmissing data never raises")
for bad in ({}, {"snapshot": None}, {"snapshot": {"power": None}}):
    try:
        report.to_markdown(report.summarise(bad, None, None, None, {}, None))
        check(f"survives {bad}", True)
    except Exception as e:  # noqa: BLE001
        check(f"survives {bad}", False, repr(e))

print(f"\n{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
