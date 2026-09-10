"""Formatting helpers. Kept separate so they can be unit-tested without GTK."""

from __future__ import annotations

SEVERITY = ["INFO", "NOTICE", "WARNING", "CRITICAL"]
SEV_CSS = ["sv-info", "sv-notice", "sv-warning", "sv-critical"]


def human_bytes(b: float) -> str:
    units = ["B", "kB", "MB", "GB", "TB", "PB"]
    v = abs(float(b))
    i = 0
    while v >= 1024 and i < len(units) - 1:
        v /= 1024.0
        i += 1
    sign = "-" if b < 0 else ""
    if i == 0:
        return f"{sign}{v:.0f} B"
    if v >= 100:
        return f"{sign}{v:.0f} {units[i]}"
    return f"{sign}{v:.1f} {units[i]}"


def human_bps(b: float) -> str:
    return human_bytes(b) + "/s"


def human_duration(secs: float) -> str:
    s = int(max(0, secs))
    if s < 60:
        return f"{s}s"
    if s < 3600:
        m, r = divmod(s, 60)
        return f"{m}m" if r == 0 else f"{m}m {r}s"
    if s < 86400:
        h, r = divmod(s, 3600)
        m = r // 60
        return f"{h}h" if m == 0 else f"{h}h {m}m"
    d, r = divmod(s, 86400)
    h = r // 3600
    return f"{d}d" if h == 0 else f"{d}d {h}h"


def human_ago(ts_ms: float, now_ms: float) -> str:
    d = (now_ms - ts_ms) / 1000.0
    if d < 45:
        return "just now"
    return human_duration(d) + " ago"


def clock_hm(ts_ms: float) -> str:
    import time
    return time.strftime("%H:%M:%S", time.localtime(ts_ms / 1000.0))


def temp(c) -> str:
    return "—" if c is None else f"{c:.0f} °C"


def pct(v) -> str:
    return "—" if v is None else f"{v:.0f}%"


def watts(v) -> str:
    return "—" if v is None else f"{v:.1f} W"
