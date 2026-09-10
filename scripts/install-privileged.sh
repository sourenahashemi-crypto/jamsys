#!/usr/bin/env bash
# Install the two optional privileged helpers. Run once, with sudo.
#
#   sudo ./scripts/install-privileged.sh
#
# Everything else in JamSys works without this. What it adds:
#
#   jamsys-kbd    keyboard backlight and RGB control   (Polkit: allow_active)
#   jamsys-power  battery charge limit                 (Polkit: auth_admin_keep)
#
# Neither is a daemon. Neither listens on anything. Each writes a fixed, tiny set
# of sysfs attributes with range-checked integer arguments, and is invoked through
# pkexec by the JamSys application, which itself never runs as root.
# See docs/privilege-model.md.
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "install-privileged.sh must run as root:  sudo $0" >&2
    exit 1
fi

ROOT="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"

missing=0
for b in jamsys-kbd jamsys-power; do
    if [ ! -x "$TARGET/release/$b" ]; then
        echo "missing $TARGET/release/$b -- build it first:" >&2
        echo "    (cd $ROOT/$b && cargo build --release)" >&2
        missing=1
    fi
done
[ "$missing" -eq 0 ] || exit 2

install -d -m 0755 /usr/libexec
install -o root -g root -m 0755 "$TARGET/release/jamsys-kbd"   /usr/libexec/jamsys-kbd
install -o root -g root -m 0755 "$TARGET/release/jamsys-power" /usr/libexec/jamsys-power
install -o root -g root -m 0644 "$ROOT/packaging/polkit/org.jamsys.keyboard.policy" \
    /usr/share/polkit-1/actions/org.jamsys.keyboard.policy
install -o root -g root -m 0644 "$ROOT/packaging/polkit/org.jamsys.power.policy" \
    /usr/share/polkit-1/actions/org.jamsys.power.policy

echo "installed:"
ls -l /usr/libexec/jamsys-kbd /usr/libexec/jamsys-power | sed 's/^/  /'
echo
echo "Keyboard lighting and the battery charge limit are now available in JamSys."
echo "Restart the daemon so it re-probes:  systemctl --user restart jamsysd"
echo
echo "To remove them again:"
echo "  sudo rm -f /usr/libexec/jamsys-kbd /usr/libexec/jamsys-power \\"
echo "             /usr/share/polkit-1/actions/org.jamsys.{keyboard,power}.policy"
