"""Formatting helpers. Kept separate so they can be unit-tested without GTK."""

from __future__ import annotations

import math

#: What every formatter prints when it has no number to show.
UNKNOWN = "—"

SEVERITY = ["INFO", "NOTICE", "WARNING", "CRITICAL"]
SEV_CSS = ["sv-info", "sv-notice", "sv-warning", "sv-critical"]


def human_bytes(b: float) -> str:
    units = ["B", "kB", "MB", "GB", "TB", "PB"]
    n = _num(b)
    if n is None:
        return UNKNOWN
    b, v = n, abs(n)
    i = 0
    while v >= 1024 and i < len(units) - 1:
        v /= 1024.0
        i += 1
    sign = "-" if b < 0 else ""
    if i == 0:
        return _unsigned_zero(f"{sign}{v:.0f} B")
    if v >= 100:
        return _unsigned_zero(f"{sign}{v:.0f} {units[i]}")
    return _unsigned_zero(f"{sign}{v:.1f} {units[i]}")


def human_bps(b: float) -> str:
    v = human_bytes(b)
    # "—/s" reads as a rate. An unknown value has no unit.
    return v if v == UNKNOWN else v + "/s"


def human_duration(secs: float) -> str:
    n = _num(secs)
    if n is None:
        return UNKNOWN
    s = int(max(0, n))
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
    ts, now = _num(ts_ms), _num(now_ms)
    if ts is None or now is None:
        return UNKNOWN
    d = (now - ts) / 1000.0
    if d < 45:
        return "just now"
    return human_duration(d) + " ago"


def clock_hm(ts_ms: float) -> str:
    import time
    n = _num(ts_ms)
    if n is None:
        return UNKNOWN
    try:
        return time.strftime("%H:%M:%S", time.localtime(n / 1000.0))
    except (OSError, OverflowError, ValueError):
        # A timestamp outside the platform's range is not worth a traceback.
        return UNKNOWN


def _num(v) -> "float | None":
    """A finite float, or None for anything that is not one.

    These four are called with whatever the daemon sent. It is typed Rust and
    should only ever send a number or null, but a formatter that raises takes the
    whole page down with it, and "—" is the honest rendering of a value that is
    not a number.
    """
    if v is None or isinstance(v, bool):
        return None
    try:
        f = float(v)
    except (TypeError, ValueError):
        return None
    return f if math.isfinite(f) else None


def _unsigned_zero(text: str) -> str:
    """Drop a minus sign that rounding invented.

    -0.4 % renders as "-0%", which reads as a measurement with a direction rather
    than as nothing much. The sign only survives if some digit actually did.
    """
    head = text.split(" ", 1)[0]
    if text.startswith("-") and not any(d in head for d in "123456789"):
        return text[1:]
    return text


def temp(c) -> str:
    v = _num(c)
    return UNKNOWN if v is None else _unsigned_zero(f"{v:.0f} °C")


def pct(v) -> str:
    n = _num(v)
    return UNKNOWN if n is None else _unsigned_zero(f"{n:.0f}%")


def watts(v) -> str:
    n = _num(v)
    return UNKNOWN if n is None else _unsigned_zero(f"{n:.1f} W")
