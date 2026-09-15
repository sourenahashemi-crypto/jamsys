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
expect_uid=""
if [ -n "${SUDO_USER:-}" ]; then
    invoker_home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
    expect_uid="$(id -u "$SUDO_USER")"
elif [ -n "${PKEXEC_UID:-}" ]; then
    invoker_home="$(getent passwd "$PKEXEC_UID" | cut -d: -f6)"
    expect_uid="$PKEXEC_UID"
fi

# These binaries become pkexec targets running as root, so where they come from
# matters. The build directories are owned by the invoking user -- that is the
# documented workflow on a machine with a staged toolchain -- so user ownership is
# expected and fine. What is not fine is anyone *else* being able to swap the file
# before root runs it. So: never world-writable, and group-writable only when the
# group is private to the owner (the usual "user private group" layout, which is
# what cargo's 0775 under a 002 umask produces).
writable_by_others() {
    local path="$1" perms gid members
    perms="$(stat -c %a "$path")"
    gid="$(stat -c %g "$path")"
    if [ $((0$perms & 0002)) -ne 0 ]; then
        echo "world-writable ($perms)"
        return 0
    fi
    if [ $((0$perms & 0020)) -ne 0 ]; then
        members="$(getent group "$gid" | cut -d: -f4)"
        if [ -n "$members" ] && [ "$members" != "$(id -un "$(stat -c %u "$path")" 2>/dev/null)" ]; then
            echo "group-writable ($perms) and group $gid has other members: $members"
            return 0
        fi
    fi
    return 1
}

safe_to_install() {
    local f="$1" d why
    d="$(dirname "$f")"
    local owner downer
    owner="$(stat -c %u "$f")"; downer="$(stat -c %u "$d")"
    if [ "$owner" != "0" ] && [ "$owner" != "${expect_uid:-$owner}" ]; then
        echo "refusing $f: owned by uid $owner, which is neither root nor the invoking user" >&2
        return 1
    fi
    if [ "$downer" != "0" ] && [ "$downer" != "${expect_uid:-$downer}" ]; then
        echo "refusing $f: its directory $d is owned by uid $downer" >&2
        return 1
    fi
    if why="$(writable_by_others "$f")"; then
        echo "refusing $f: $why -- anyone in that set could swap the binary root is about to run" >&2
        return 1
    fi
    if why="$(writable_by_others "$d")"; then
        echo "refusing $f: its directory $d is $why" >&2
        return 1
    fi
    return 0
}

find_binary() {
    local name="$1" c
    for c in \
        ${CARGO_TARGET_DIR:+"$CARGO_TARGET_DIR/release/$name"} \
        "$ROOT/target/release/$name" \
        ${invoker_home:+"$invoker_home/.cache/jamsys-target/release/$name"} \
        "$HOME/.cache/jamsys-target/release/$name"
    do
        [ -x "$c" ] || continue
        safe_to_install "$c" || continue
        printf '%s' "$c"
        return 0
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
