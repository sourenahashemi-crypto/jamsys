"""Build one readable report out of everything the daemon knows.

The rest of the interface answers "what is this subsystem doing?". This answers
the question you actually have when something is wrong: *what is wrong, why do
we think so, and what should I do about it* -- in one place, in an order you can
work down, and in a form you can paste into a message to somebody else.

Pure text assembly, deliberately: no GTK here, so it can be unit-tested and also
produced from the command line without a display.
"""

from __future__ import annotations

import datetime
import shutil
from typing import Any


SEVERITY = {0: "info", 1: "notice", 2: "warning", 3: "critical"}


def _ts(ms: Any) -> str:
    try:
        return datetime.datetime.fromtimestamp(int(ms) / 1000).strftime("%Y-%m-%d %H:%M:%S")
    except (TypeError, ValueError, OSError):
        return "unknown"


def _age(ms: Any) -> str:
    """How long ago, in words. 'since 14:02' is useless without today's date."""
    try:
        secs = max(0, int((_now_ms() - int(ms)) / 1000))
    except (TypeError, ValueError):
        return ""
    for limit, div, unit in ((90, 1, "second"), (5400, 60, "minute"),
                             (172800, 3600, "hour")):
        if secs < limit:
            n = max(1, secs // div)
            return f"{n} {unit}{'s' if n != 1 else ''} ago"
    n = secs // 86400
    return f"{n} day{'s' if n != 1 else ''} ago"


def _now_ms() -> int:
    import time
    return int(time.time() * 1000)


def rows(value: Any, *keys: str) -> list:
    """Pull a list out of a daemon reply whatever wrapper it arrived in.

    The IPC replies are not uniformly shaped -- `coverage` returns
    {"collectors": [...]}, `inventory` returns {"items": [...]}, others return
    a bare list. Guessing one shape silently produced empty sections.
    """
    if isinstance(value, list):
        return value
    if isinstance(value, dict):
        for k in keys:
            if isinstance(value.get(k), list):
                return value[k]
        for v in value.values():
            if isinstance(v, list):
                return v
    return []


def summarise(snapshot: dict, alerts: list[dict], coverage: list[dict],
              events: list[dict], stats: dict, inventory: Any = None) -> dict:
    """Reduce the raw data to the few things worth acting on.

    Returns a dict rather than text so the page can render it as widgets and
    `to_markdown` can render the same thing as a paste-able report -- one source
    of judgement, two presentations.
    """
    snap = snapshot.get("snapshot", snapshot) or {}

    open_alerts = [a for a in (alerts or []) if a.get("resolved_ts") is None]
    open_alerts.sort(key=lambda a: (-(a.get("severity") or 0), -(a.get("last_ts") or 0)))

    # Things that are not alerts but will bite: known-bad configuration the
    # collectors noticed in passing.
    risks: list[str] = list((snap.get("bluetooth") or {}).get("risk_notes") or [])

    # Anything the daemon cannot see is part of the picture: an absent sensor
    # explains a missing alert as much as a present one explains a firing alert.
    # Disabled counts as a gap too. It is the user's own choice, but "why am I
    # getting no disk alerts?" is exactly the question this report exists to
    # answer, so it is listed and labelled rather than quietly omitted.
    gaps = [c for c in rows(coverage, "collectors", "coverage")
            if str(c.get("label", "")).lower() != "full"]

    failed = [u.get("name", "?") for u in (snap.get("services") or {}).get("failed", [])]

    verdict = "healthy"
    if any((a.get("severity") or 0) >= 3 for a in open_alerts):
        verdict = "critical"
    elif open_alerts:
        verdict = "attention"

    return {
        "verdict": verdict,
        "alerts": open_alerts,
        "risks": risks,
        "gaps": gaps,
        "failed_units": failed,
        "events": rows(events, "events", "rows")[:15],
        "machine": _machine(snapshot, snap, stats, inventory),
    }


def _machine(envelope: dict, snap: dict, stats: dict, inventory: Any) -> dict:
    # The inventory op returns {"items": [{"key": ..., "value": ...}]}, not a
    # dict keyed by name.
    inv = {i.get("key"): i.get("value")
           for i in rows(inventory, "items") if isinstance(i, dict)}
    power = snap.get("power") or {}
    # collectors_usable/total live on the snapshot envelope, beside "snapshot",
    # not inside it and not in stats.
    usable = envelope.get("collectors_usable", "?")
    total = envelope.get("collectors_total", "?")
    rss = stats.get("rss_bytes")
    return {
        "model": inv.get("product.name") or inv.get("board.name") or "unknown",
        "kernel": inv.get("kernel.release") or "unknown",
        "cpu": inv.get("cpu.model") or "unknown",
        "uptime_s": stats.get("uptime_s") or 0,
        "daemon_rss_mb": round(rss / 1048576, 1) if isinstance(rss, (int, float)) else None,
        "collectors": f"{usable}/{total}",
        "battery_pct": power.get("percent"),
        "charge_limit": power.get("charge_limit_pct"),
        "on_battery": power.get("on_battery"),
    }


def to_markdown(s: dict) -> str:
    """The same summary as something you can paste into a bug report."""
    m = s["machine"]
    out: list[str] = []
    add = out.append

    add("# JamSys report")
    add("")
    add(f"Generated {_ts(_now_ms())}")
    add("")

    headline = {
        "healthy": "Everything is normal.",
        "attention": f"{len(s['alerts'])} thing(s) need attention.",
        "critical": f"{len(s['alerts'])} thing(s) need attention, including a critical one.",
    }[s["verdict"]]
    add(f"**{headline}**")
    add("")

    add("## Machine")
    add("")
    add(f"- Model: {m['model']}")
    add(f"- CPU: {m['cpu']}")
    add(f"- Kernel: {m['kernel']}")
    add(f"- Collectors usable: {m['collectors']}")
    if m["uptime_s"]:
        add(f"- Daemon uptime: {int(m['uptime_s']) // 3600}h"
            f"{(int(m['uptime_s']) % 3600) // 60:02d}m"
            + (f", {m['daemon_rss_mb']} MB resident" if m["daemon_rss_mb"] else ""))
    if m["battery_pct"] is not None:
        limit = m["charge_limit"]
        cap = f", charge limit {limit}%" if limit not in (None, 100) else ""
        state = "on battery" if m["on_battery"] else "on mains"
        add(f"- Battery: {m['battery_pct']:.0f}% ({state}{cap})")
    add("")

    if s["alerts"]:
        add("## Open problems")
        add("")
        for a in s["alerts"]:
            sev = SEVERITY.get(a.get("severity", 0), "?")
            add(f"### {a.get('title', 'Untitled')}  ({sev})")
            add("")
            d = a.get("detail") or {}
            if d.get("what"):
                add(d["what"])
                add("")
            if d.get("expected"):
                add(f"- Expected: {d['expected']}")
            for ev in d.get("evidence") or []:
                add(f"- {ev.get('label', '?')}: {ev.get('value', '')}")
            if d.get("likely_cause"):
                add(f"- Likely cause: {d['likely_cause']}")
            add(f"- First seen: {_ts(a.get('first_ts'))} ({_age(a.get('first_ts'))})")
            if (a.get("count") or 1) > 1:
                add(f"- Occurrences: {a['count']}")
            actions = d.get("actions") or []
            if actions:
                add("")
                add("What to try:")
                for act in actions:
                    add(f"  - {act}")
            add("")
    else:
        add("## Open problems")
        add("")
        add("None.")
        add("")

    if s["risks"]:
        add("## Configuration worth changing")
        add("")
        add("Not faults, but known causes of the kind of problem you would "
            "otherwise chase for a long time.")
        add("")
        for r in s["risks"]:
            add(f"- {r}")
        add("")

    if s["gaps"]:
        add("## What is not being watched")
        add("")
        add("An absent sensor explains a missing alert as much as a present one "
            "explains a firing alert.")
        add("")
        for c in s["gaps"]:
            label = str(c.get("label", "?"))
            reason = c.get("detail") or c.get("reason") or c.get("last_error") or ""
            if label.lower() == "disabled":
                reason = reason or "switched off in settings, not a fault"
            add(f"- **{c.get('name', '?')}** — {label}"
                + (f": {reason}" if reason else ""))
        add("")

    if s["events"]:
        add("## Recent events")
        add("")
        for e in s["events"]:
            add(f"- `{_ts(e.get('ts'))}` {e.get('subsystem', '')}/{e.get('kind', '')}: "
                f"{(e.get('summary') or '').strip()}")
        add("")

    add("---")
    add("")
    add("Collected locally by JamSys. No part of this was sent anywhere; "
        "it contains hostnames, device names and process names, so read it "
        "before sharing.")
    return "\n".join(out)


def to_plain(s: dict) -> str:
    """Markdown minus the markers, for a terminal."""
    text = to_markdown(s)
    width = min(100, max(60, shutil.get_terminal_size((100, 24)).columns))
    lines = []
    for line in text.split("\n"):
        line = line.replace("**", "").replace("`", "")
        if line.startswith("### "):
            line = line[4:]
        elif line.startswith("## "):
            line = line[3:].upper()
        elif line.startswith("# "):
            line = line[2:].upper()
        lines.append(line[:width])
    return "\n".join(lines)
