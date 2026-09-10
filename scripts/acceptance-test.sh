#!/usr/bin/env bash
# JamSys acceptance measurements.
#
# Produces the table the specification asks for at completion: what the monitoring
# application itself costs, and whether it disturbs the things it watches.
#
# Everything is measured externally where possible — attaching an IPC client is itself
# one of the conditions that ends the reduced sampling regime, so the daemon cannot
# observe its own idle behaviour without destroying it.
#
# Usage:  ./acceptance-test.sh [seconds]      default 300
set -uo pipefail

WINDOW="${1:-300}"
SV="${SV:-$HOME/.local/bin/jamsys}"
UNIT=jamsysd.service
CARD=/sys/class/drm/card2/device/power

hdr() { printf '\n\033[1m%s\033[0m\n%s\n' "$1" "$(printf '%.0s-' $(seq ${#1}))"; }
row() { printf '  %-38s %s\n' "$1" "$2"; }

daemon_pid() { systemctl --user show -p MainPID --value "$UNIT" 2>/dev/null; }

measure_proc() {  # pid seconds -> "cpu_pct rss_kb ctxsw_per_s"
    python3 - "$1" "$2" <<'PY'
import os, sys, time
pid, dur = int(sys.argv[1]), float(sys.argv[2])
TCK = os.sysconf("SC_CLK_TCK")
def cpu():
    f = open(f"/proc/{pid}/stat").read(); v = f[f.rindex(")")+1:].split()
    return (int(v[11]) + int(v[12])) / TCK
def st(k):
    for l in open(f"/proc/{pid}/status"):
        if l.startswith(k): return int(l.split()[1])
    return 0
c0, v0 = cpu(), st("voluntary_ctxt_switches"); t0 = time.monotonic()
time.sleep(dur)
el = time.monotonic() - t0
print(f"{100*(cpu()-c0)/el:.4f} {st('VmRSS')} {(st('voluntary_ctxt_switches')-v0)/el:.3f}")
PY
}

pss_kb() { awk '/^Pss:/{s+=$2} END{print s+0}' "/proc/$1/smaps_rollup" 2>/dev/null || echo 0; }
bat_w()  { python3 -c "print(f'{$(cat /sys/class/power_supply/BAT0/power_now 2>/dev/null || echo 0)/1e6:.2f}')"; }
on_ac()  { [ "$(cat /sys/class/power_supply/ADP0/online 2>/dev/null || echo 1)" = "1" ]; }
dgpu_active_ms() { cat "$CARD/runtime_active_time" 2>/dev/null || echo 0; }

echo "JamSys acceptance measurements — $(date -Is)"
echo "window: ${WINDOW}s per measurement"

# ---------------------------------------------------------------- daemon
hdr "1. Monitoring daemon"
PID=$(daemon_pid)
if [ -z "$PID" ] || [ "$PID" = "0" ]; then
    row "daemon" "NOT RUNNING — start it with: systemctl --user start jamsysd"
    exit 1
fi
row "pid" "$PID"
read -r CPU RSS CTX <<< "$(measure_proc "$PID" "$WINDOW")"
NCPU=$(nproc)
row "average CPU" "$CPU% of one core  ($(python3 -c "print(f'{$CPU/$NCPU:.4f}')")% of $NCPU threads)"
row "resident memory (RSS)" "$(python3 -c "print(f'{$RSS/1024:.1f} MB')")"
row "proportional memory (PSS)" "$(python3 -c "print(f'{$(pss_kb "$PID")/1024:.1f} MB')")"
row "threads" "$(awk '/Threads/{print $2}' /proc/$PID/status)"
row "voluntary context switches" "$CTX /s  (upper bound on wakeups)"

S=$("$SV" --json stats 2>/dev/null)
py() { python3 -c "import json,sys;d=json.loads(sys.stdin.read());print($1)" <<< "$S"; }
row "epoll wakeups (self-reported)" "$(py "f\"{d['wakeups_per_s']:.3f} /s\"")"
row "scheduler ticks" "$(py "d['ticks']")"
row "reduced-sampling ticks" "$(py "f\"{100*d.get('idle_tick_fraction',0):.0f}% ({d.get('idle_blocked_by') or 'active now'})\"")"
row "events processed / dropped" "$(py "f\"{d['events_processed']} / {d['events_dropped']}\"")"

# ---------------------------------------------------------------- store
hdr "2. History store"
row "database on disk" "$(py "f\"{d['db_bytes']/1e6:.2f} MB\"")"
row "  ten-second samples" "$(py "f\"{d['sample_rows']:,}\"")"
row "  rollup rows" "$(py "f\"{d['rollup_rows']:,}\"")"
row "  events" "$(py "f\"{d['event_rows']:,}\"")"
row "full-resolution buffer (memory)" "$(py "f\"{d['live_bytes']/1024:.1f} kB, {d['live_points']:,} points\"")"
UP=$(py "d['uptime_s']")
row "growth rate" "$(py "f\"{d['db_bytes']/1e6/max(d['uptime_s'],1)*86400:.1f} MB/day at this rate\"")"
BEFORE_DB=$(py "d['db_bytes']")
sleep 60
S=$("$SV" --json stats 2>/dev/null)
row "write frequency" "one transaction per 15 s (measured delta over 60 s: $(py "f\"{(d['db_bytes']-$BEFORE_DB)/1024:.0f} kB\""))"

# ---------------------------------------------------------------- UI
hdr "3. Desktop interface"
UIPID=$(pgrep -f 'python3 .*bin/jamsys$' | head -1)
if [ -n "$UIPID" ]; then
    row "window RSS" "$(python3 -c "print(f'{$(awk '/VmRSS/{print $2}' /proc/$UIPID/status)/1024:.1f} MB')")"
    row "window PSS" "$(python3 -c "print(f'{$(pss_kb "$UIPID")/1024:.1f} MB')")"
else
    row "window" "not running (it is transient by design)"
fi
if gnome-extensions info jamsys@jamsys.org >/dev/null 2>&1; then
    ENABLED=$(gnome-extensions info jamsys@jamsys.org | awk -F': ' '/State/{print $2}')
    row "shell extension" "installed, state: $ENABLED"
    SHPID=$(pgrep -x gnome-shell | head -1)
    [ -n "$SHPID" ] && row "gnome-shell RSS (with readout)" \
        "$(python3 -c "print(f'{$(awk '/VmRSS/{print $2}' /proc/$SHPID/status)/1024:.1f} MB')")"
else
    row "shell extension" "installed on disk but not loaded — GNOME Shell needs a"
    row "" "session restart on Wayland before a NEW extension appears"
fi

# ---------------------------------------------------------------- NVIDIA
hdr "4. Discrete GPU: does monitoring keep it awake?"
row "current runtime state" "$(cat "$CARD/runtime_status" 2>/dev/null || echo n/a)"
echo "  measuring with the daemon RUNNING ..."
A=$(dgpu_active_ms); sleep "$WINDOW"; B=$(dgpu_active_ms)
ON_MS=$(( B - A ))
row "dGPU awake, monitor ON" "${ON_MS} ms of $((WINDOW*1000)) ms"
echo "  stopping the daemon and repeating ..."
systemctl --user stop "$UNIT"; sleep 20
A=$(dgpu_active_ms); sleep "$WINDOW"; B=$(dgpu_active_ms)
OFF_MS=$(( B - A ))
row "dGPU awake, monitor OFF" "${OFF_MS} ms of $((WINDOW*1000)) ms"
row "attributable to JamSys" "$(( ON_MS - OFF_MS )) ms"

# ---------------------------------------------------------------- battery
hdr "5. Battery draw: monitor ON versus OFF"
if on_ac; then
    row "SKIPPED" "the machine is on AC; power_now measures charging, not draw"
    row "to run this" "unplug the charger, wait a minute, then re-run"
    systemctl --user start "$UNIT"
else
    echo "  daemon is stopped; sampling idle draw ..."
    OFFW=$(python3 -c "
import time
s=[]
for _ in range(20):
    s.append(int(open('/sys/class/power_supply/BAT0/power_now').read())/1e6); time.sleep(3)
s.sort(); print(f'{s[len(s)//2]:.2f}')")
    row "median draw, monitor OFF" "${OFFW} W"
    systemctl --user start "$UNIT"; sleep 30
    ONW=$(python3 -c "
import time
s=[]
for _ in range(20):
    s.append(int(open('/sys/class/power_supply/BAT0/power_now').read())/1e6); time.sleep(3)
s.sort(); print(f'{s[len(s)//2]:.2f}')")
    row "median draw, monitor ON" "${ONW} W"
    row "difference" "$(python3 -c "print(f'{$ONW-$OFFW:+.2f} W')")  (sensor quantum is ~0.01 W)"
fi

systemctl --user start "$UNIT" 2>/dev/null
hdr "Done"
echo "  The daemon has been restarted if it was stopped."
