#!/usr/bin/env bash
# Run every test suite in the project.
#
# This exists because there was no single entry point: suites were listed in
# three documents, in three different combinations, and two of them were listed
# nowhere that anyone actually ran. A suite nobody runs is not a suite.
#
#   ./scripts/run-tests.sh            # everything that needs no display
#   ./scripts/run-tests.sh --all      # also the GTK and live-daemon suites
#
# Exits non-zero if anything fails, and prints one line per suite either way.
set -uo pipefail

ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
cd "$ROOT"
# shellcheck source=/dev/null
[ -f scripts/devenv.sh ] && source scripts/devenv.sh

ALL=0
[ "${1:-}" = "--all" ] && ALL=1

pass=0; fail=0; skip=0
run() {
    local label="$1"; shift
    local out rc
    out="$("$@" 2>&1)"; rc=$?
    if [ "$rc" -eq 0 ]; then
        pass=$((pass + 1))
        # Sum every "N passed" in the output: cargo prints one line per test
        # binary (lib, integration, doc-tests), so reporting only the last gives
        # a misleading zero for the crate that has the most tests.
        printf '  \033[32mok\033[0m    %-34s %s\n' "$label" \
            "$(printf '%s' "$out" | grep -Eo '[0-9]+ (passed|checks)' \
               | awk '{n += $1} END {if (n) printf "%d checks", n}')"
    else
        fail=$((fail + 1))
        printf '  \033[31mFAIL\033[0m  %-34s\n' "$label"
        printf '%s\n' "$out" | tail -15 | sed 's/^/        /'
    fi
}

skip_note() {
    skip=$((skip + 1))
    printf '  \033[33mskip\033[0m  %-34s %s\n' "$1" "$2"
}

echo "Rust"
for c in jamsys-daemon jamsys-helper jamsys-kbd jamsys-power; do
    run "$c" bash -c "cd '$ROOT/$c' && cargo test --release"
done

echo "Python"
run "xabove"        python3 jamsys-ui/tests/xabove-test.py
run "charge-limit"  python3 jamsys-ui/tests/charge-limit-test.py
run "report"        python3 jamsys-ui/tests/report-test.py
if [ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]; then
    run "page-refresh (GTK)" python3 jamsys-ui/tests/page-refresh-test.py
else
    skip_note "page-refresh (GTK)" "no display"
fi

echo "JavaScript"
for t in format-test gauges-test shell-api-test offline-render-test \
         standalone-lifecycle-test; do
    run "$t" gjs -m "gnome-extension/tests/$t.js"
done

echo "Against the running system"
if [ "$ALL" -eq 1 ]; then
    if gdbus introspect --session -d org.jamsys.Daemon -o /org/jamsys/Daemon >/dev/null 2>&1; then
        run "live-dbus" gjs -m gnome-extension/tests/live-dbus-test.js
    else
        skip_note "live-dbus" "jamsysd is not on the session bus"
    fi
else
    skip_note "live-dbus" "needs --all and a running daemon"
fi

echo
printf '%d passed, %d failed, %d skipped\n' "$pass" "$fail" "$skip"
[ "$fail" -eq 0 ]
