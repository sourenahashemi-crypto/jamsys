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

# Find the built binaries. sudo scrubs CARGO_TARGET_DIR from the environment, so
# relying on it here silently sends the search to a directory that does not exist --
# and on a machine using the staged toolchain, the artefacts are under the *invoking
# user's* cache, not root's. Look everywhere they legitimately land.
# sudo exports SUDO_USER; pkexec exports PKEXEC_UID instead. Handle both, so the
# script works however it was elevated.
invoker_home=""
if [ -n "${SUDO_USER:-}" ]; then
    invoker_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
elif [ -n "${PKEXEC_UID:-}" ]; then
    invoker_home="$(getent passwd "$PKEXEC_UID" | cut -d: -f6)"
fi

find_binary() {
    local name="$1" c
    for c in \
        ${CARGO_TARGET_DIR:+"$CARGO_TARGET_DIR/release/$name"} \
        "$ROOT/target/release/$name" \
        ${invoker_home:+"$invoker_home/.cache/jamsys-target/release/$name"} \
        "$HOME/.cache/jamsys-target/release/$name"
    do
        [ -x "$c" ] && { printf '%s' "$c"; return 0; }
    done
    return 1
}

KBD="$(find_binary jamsys-kbd)" || {
    echo "cannot find a built jamsys-kbd. Build both helpers first, as your normal user:" >&2
    echo "    cd $ROOT && source scripts/devenv.sh" >&2
    echo "    (cd jamsys-kbd && cargo build --release)" >&2
    echo "    (cd jamsys-power && cargo build --release)" >&2
    exit 2
}
PWR="$(find_binary jamsys-power)" || {
    echo "cannot find a built jamsys-power. Build it first, as your normal user:" >&2
    echo "    cd $ROOT && source scripts/devenv.sh && (cd jamsys-power && cargo build --release)" >&2
    exit 2
}
echo "using:"
echo "  $KBD"
echo "  $PWR"

install -d -m 0755 /usr/libexec
install -o root -g root -m 0755 "$KBD" /usr/libexec/jamsys-kbd
install -o root -g root -m 0755 "$PWR" /usr/libexec/jamsys-power
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
