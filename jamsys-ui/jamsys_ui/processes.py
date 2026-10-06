"""Selection of the daemon's top process samples; never a second collector."""


def select_processes(sample: dict, query: str = "", sort: str = "cpu") -> list[dict]:
    # A process commonly appears in both lists. PID is the identity within this
    # snapshot; never retain a previous process after that PID has disappeared.
    by_pid = {}
    for key in ("top_cpu", "top_mem"):
        for proc in sample.get(key) or []:
            by_pid.setdefault(proc["pid"], proc)
    query = query.strip().casefold()
    matches = [proc for proc in by_pid.values()
               if not query or query in str(proc.get("name", "")).casefold()
               or query in str(proc["pid"])]
    if sort == "name":
        return sorted(matches, key=lambda p: (str(p.get("name", "")).casefold(), p["pid"]))
    key = "rss_bytes" if sort == "memory" else "cpu_pct"
    return sorted(matches, key=lambda p: (-(p.get(key) or 0), p["pid"]))
