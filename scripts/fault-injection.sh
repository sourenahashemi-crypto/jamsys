#!/usr/bin/env bash
# Controlled fault injection for JamSys.
#
# Two classes of test:
#
#   SAFE       — reversible, no effect on anything you are doing. Run by default.
#   DISRUPTIVE — briefly interrupts a real service, needs physical access, or ends
#                your session. Never run unless you explicitly ask for it.
#
# Usage:
#   ./fault-injection.sh safe            run the safe set
#   ./fault-injection.sh wifi            cycle Wi-Fi (interrupts the network ~10 s)
#   ./fault-injection.sh bluetooth       cycle the Bluetooth adapter
#   ./fault-injection.sh list            show every test and its class
set -uo pipefail

SV="${SV:-$HOME/.local/bin/jamsys}"
q() { "$SV" --json "$@" 2>/dev/null; }
# `date +%s%3N` is not portable — on this system it emits nanoseconds and junk,
# which silently made every "since" filter match nothing.
now_ms() { python3 -c 'import time;print(int(time.time()*1000))'; }

hdr() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
note() { printf '   %s\n' "$1"; }

alerts_since() {
    local since=$1
    q alerts '{"open_only":false,"limit":60}' | python3 -c "
import json,sys
d=json.load(sys.stdin).get('alerts',[])
hits=[a for a in d if a['last_ts'] >= $since]
if not hits: print('   (no alerts recorded in the window)')
for a in hits:
    print('   [%s] %s' % (['INFO','NOTICE','WARNING','CRITICAL'][a['severity']], a['title']))
"
}
events_since() {
    local since=$1
    q events "{\"since_ms\":$since,\"limit\":40}" | python3 -c "
import json,sys
d=json.load(sys.stdin).get('events',[])
if not d: print('   (no events)')
for e in d[:12]:
    print('   %-16s %s' % (e['kind'], e['summary'][:88]))
"
}

metric() {  # subsystem name -> latest value from the snapshot
    q snapshot | python3 -c "
import json,sys
s=json.load(sys.stdin)['snapshot']
try:
    v=s
    for k in '$1'.split('.'): v=v[k]
    print(v)
except Exception: print('n/a')
"
}

# ---------------------------------------------------------------- SAFE

activity() { q stats | python3 -c 'import json,sys;print(json.load(sys.stdin)["activity"])'; }

test_cpu_load() {
    # The cpu.sustained_load rule deliberately requires 300 s of dwell, so a short
    # burst proves nothing. This runs long enough to actually trip it, and samples
    # *during* the load rather than after it — an earlier version measured the
    # recovery and concluded, wrongly, that nothing had been detected.
    hdr "CPU load (SAFE — 340 s at full load, on AC)"
    local t0; t0=$(now_ms)
    note "before: cpu=$(metric cpu.usage_pct)%  activity=$(activity)  temp=$(metric thermal.cpu_package_c)C"
    local n; n=$(nproc)
    note "spinning $n threads for 340 s ..."
    for _ in $(seq "$n"); do (timeout 340 bash -c 'while :; do :; done') & done
    for t in 60 150 240 320; do
        sleep $(( t == 60 ? 60 : 90 ))
        note "  t+${t}s: cpu=$(metric cpu.usage_pct)% activity=$(activity) temp=$(metric thermal.cpu_package_c)C freq=$(metric cpu.freq_mhz)MHz"
    done
    wait 2>/dev/null
    note "alerts raised during the load:"; alerts_since "$t0"
    sleep 20
    note "after recovery: cpu=$(metric cpu.usage_pct)%  activity=$(activity)"
}

test_memory_pressure() {
    hdr "Memory pressure (SAFE — allocates a bounded amount and frees it)"
    local t0; t0=$(now_ms)
    local avail_before; avail_before=$(metric memory.available_pct)
    note "before: ${avail_before}% available, PSI some=$(metric memory.psi_some_avg60)"
    python3 - <<'PY' &
import time
# Bounded: 8 GiB against 30 GiB total, touched so it is really resident.
CHUNK = 256 * 1024 * 1024
blocks = []
try:
    for _ in range(32):
        b = bytearray(CHUNK)
        b[::4096] = b"x" * (len(b) // 4096)
        blocks.append(b)
    time.sleep(45)
finally:
    blocks.clear()
PY
    local pid=$!
    sleep 25
    note "during: available=$(metric memory.available_pct)%  PSI some=$(metric memory.psi_some_avg60)"
    sleep 25; kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    sleep 8
    note "after:  $(metric memory.available_pct)% available (recovered)"
    note "alerts raised:"; alerts_since "$t0"
}

test_failed_service() {
    hdr "Failed systemd unit (SAFE — a throwaway user unit)"
    local t0; t0=$(now_ms)
    local unit=jamsys-faulttest.service
    mkdir -p ~/.config/systemd/user
    cat > ~/.config/systemd/user/$unit <<'EOF'
[Unit]
Description=JamSys fault-injection target (safe to remove)
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'exit 7'
EOF
    systemctl --user daemon-reload
    systemctl --user start $unit 2>/dev/null || true
    note "started $unit, which exits 7"
    sleep 70   # the services collector is on the 60 s tier
    note "daemon sees failed units: $(q snapshot | python3 -c "
import json,sys
s=json.load(sys.stdin)['snapshot']['services']
print([u['name'] for u in s['failed']+s['failed_user']])")"
    note "alerts raised:"; alerts_since "$t0"
    note "events:"; events_since "$t0"
    systemctl --user reset-failed $unit 2>/dev/null || true
    rm -f ~/.config/systemd/user/$unit
    systemctl --user daemon-reload
    note "cleaned up"
}

test_nvidia_workload() {
    hdr "NVIDIA workload (SAFE — a short offloaded render)"
    local t0; t0=$(now_ms)
    note "dGPU before: $(metric gpu.nvidia.runtime_status)"
    local a; a=$(cat /sys/class/drm/card2/device/power/runtime_active_time 2>/dev/null || echo 0)
    if command -v glxgears >/dev/null; then
        __NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia \
            timeout 45 glxgears >/dev/null 2>&1 &
        local gp=$!
        sleep 25
        note "during:  status=$(metric gpu.nvidia.runtime_status) util=$(metric gpu.nvidia.util_pct)% power=$(metric gpu.nvidia.power_w)W temp=$(metric gpu.nvidia.temp_c)C"
        wait $gp 2>/dev/null
    else
        note "glxgears not installed; skipping the render"
    fi
    sleep 40
    local b; b=$(cat /sys/class/drm/card2/device/power/runtime_active_time 2>/dev/null || echo 0)
    note "after:   status=$(metric gpu.nvidia.runtime_status)"
    note "dGPU awake during the whole test: $(( b - a )) ms"
    note "events:"; events_since "$t0"
}

test_disk_write() {
    hdr "Heavy disk write (SAFE — 3 GB to a temp file, then removed)"
    local t0; t0=$(now_ms)
    note "before: write=$(metric storage.total_write_bps) B/s"
    local f; f=$(mktemp /var/tmp/jamsys-iotest.XXXXXX)
    dd if=/dev/zero of="$f" bs=1M count=3072 conv=fsync status=none 2>/dev/null
    note "wrote 3 GB"
    sleep 65
    note "after:  write=$(metric storage.total_write_bps) B/s  nvme=$(metric thermal.nvme_c)C"
    rm -f "$f"
    note "alerts raised:"; alerts_since "$t0"
}

# ---------------------------------------------------------- DISRUPTIVE

test_wifi() {
    hdr "Wi-Fi disconnect (DISRUPTIVE — the network drops for about 10 s)"
    command -v nmcli >/dev/null || { note "nmcli not available"; return; }
    local t0; t0=$(now_ms)
    note "before: $(metric network.default_route)"
    nmcli radio wifi off; sleep 10; nmcli radio wifi on
    note "re-enabled; waiting for reconnection ..."
    sleep 45
    note "after:  $(metric network.default_route)"
    note "event timeline:"; events_since "$t0"
    note "alerts raised:"; alerts_since "$t0"
}

test_bluetooth() {
    hdr "Bluetooth adapter cycle (DISRUPTIVE — disconnects paired devices)"
    command -v rfkill >/dev/null || { note "rfkill not available"; return; }
    local t0; t0=$(now_ms)
    rfkill block bluetooth; sleep 12; rfkill unblock bluetooth
    sleep 70
    note "events:"; events_since "$t0"
    note "alerts raised:"; alerts_since "$t0"
}

list_tests() {
    cat <<'EOF'
SAFE (run by `safe`):
  cpu-load          spin every core for 90 s
  memory-pressure   allocate and free 8 GiB
  failed-service    start a throwaway user unit that exits non-zero
  nvidia-workload   offload a short render to the discrete GPU
  disk-write        write and delete a 3 GB file

DISRUPTIVE (run individually, by name):
  wifi              cycle the Wi-Fi radio            ~10 s without network
  bluetooth         cycle the Bluetooth adapter      disconnects paired devices

CANNOT BE AUTOMATED (need you):
  battery-unplug    physically unplug the charger, then compare the Power page
  suspend-resume    close the lid or `systemctl suspend`; ends this session
  usb-camera        physically disconnect a USB device
EOF
}

case "${1:-safe}" in
    safe)
        test_failed_service
        test_cpu_load
        test_memory_pressure
        test_nvidia_workload
        test_disk_write
        ;;
    cpu-load)        test_cpu_load ;;
    memory-pressure) test_memory_pressure ;;
    failed-service)  test_failed_service ;;
    nvidia-workload) test_nvidia_workload ;;
    disk-write)      test_disk_write ;;
    wifi)            test_wifi ;;
    bluetooth)       test_bluetooth ;;
    list)            list_tests ;;
    *) echo "unknown test: $1"; list_tests; exit 2 ;;
esac
