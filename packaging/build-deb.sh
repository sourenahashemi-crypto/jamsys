#!/usr/bin/env bash
# Build a .deb for JamSys.
#
# Requires: cargo, a C toolchain, dpkg-deb. No network access at package time if the
# cargo registry is already populated.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$HERE")"
VERSION="$(grep -m1 '^version' "$ROOT/jamsys-daemon/Cargo.toml" | cut -d'"' -f2)"
ARCH="$(dpkg --print-architecture)"
STAGE="${BUILD_DIR:-$ROOT/build}/jamsys_${VERSION}_${ARCH}"

echo "==> building jamsys ${VERSION} for ${ARCH}"
rm -rf "$STAGE"
mkdir -p "$STAGE"/{DEBIAN,usr/bin,usr/libexec,usr/lib/systemd/user,usr/lib/systemd/system,usr/lib/tmpfiles.d,usr/share/applications,usr/share/doc/jamsys,usr/share/doc/jamsys/examples,usr/lib/python3/dist-packages/jamsys_ui,usr/share/polkit-1/actions,usr/share/gnome-shell/extensions/jamsys@jamsys.org/schemas,usr/share/glib-2.0/schemas}

echo "==> cargo build --release (daemon)"
( cd "$ROOT/jamsys-daemon" && cargo build --release )
echo "==> cargo build --release (metric helper)"
( cd "$ROOT/jamsys-helper" && cargo build --release )
echo "==> cargo build --release (keyboard helper)"
( cd "$ROOT/jamsys-kbd" && cargo build --release )

TARGET="${CARGO_TARGET_DIR:-$ROOT/jamsys-daemon/target}"
install -m 0755 "$TARGET/release/jamsysd"        "$STAGE/usr/bin/jamsysd"
install -m 0755 "$TARGET/release/jamsys-helper"  "$STAGE/usr/libexec/jamsys-helper"
install -m 0755 "$TARGET/release/jamsys-kbd"     "$STAGE/usr/libexec/jamsys-kbd"

# UI
install -m 0644 "$ROOT"/jamsys-ui/jamsys_ui/*.py "$STAGE/usr/lib/python3/dist-packages/jamsys_ui/"
install -m 0755 "$ROOT/jamsys-ui/bin/jamsys"     "$STAGE/usr/bin/jamsys"
install -m 0755 "$ROOT/jamsys-ui/bin/jamsys-cluster" "$STAGE/usr/bin/jamsys-cluster"
# The launcher's dev-tree sys.path insert is harmless but pointless once installed.
sed -i 's|^sys.path.insert.*$|# installed under dist-packages; no path juggling needed|' \
    "$STAGE/usr/bin/jamsys"

install -m 0644 "$ROOT/packaging/systemd/jamsysd.service"          "$STAGE/usr/lib/systemd/user/"
install -m 0644 "$ROOT/packaging/systemd/jamsys-helper.service"    "$STAGE/usr/lib/systemd/system/"
install -m 0644 "$ROOT/packaging/systemd/jamsys-helper.timer"      "$STAGE/usr/lib/systemd/system/"
install -m 0644 "$ROOT/packaging/systemd/tmpfiles-jamsys.conf"     "$STAGE/usr/lib/tmpfiles.d/jamsys.conf"
install -m 0644 "$ROOT/packaging/desktop/org.jamsys.Monitor.desktop" "$STAGE/usr/share/applications/"
install -m 0644 "$ROOT/packaging/desktop/org.jamsys.Cluster.desktop" "$STAGE/usr/share/applications/"
install -m 0644 "$ROOT/packaging/polkit/org.jamsys.keyboard.policy" "$STAGE/usr/share/polkit-1/actions/"
# The udev rule is shipped as an example rather than installed: it is the alternative
# to the helper, and installing both would be contradictory.
install -m 0644 "$ROOT/packaging/udev/99-jamsys-keyboard.rules" "$STAGE/usr/share/doc/jamsys/examples/"

# GNOME Shell extension
EXTDIR="$STAGE/usr/share/gnome-shell/extensions/jamsys@jamsys.org"
# extension.js, prefs.js, format.js, gauges.js
install -m 0644 "$ROOT"/gnome-extension/jamsys@jamsys.org/*.js       "$EXTDIR/"
install -m 0644 "$ROOT/gnome-extension/jamsys@jamsys.org/metadata.json"   "$EXTDIR/"
install -m 0644 "$ROOT/gnome-extension/jamsys@jamsys.org/stylesheet.css"  "$EXTDIR/"
install -m 0644 "$ROOT"/gnome-extension/jamsys@jamsys.org/schemas/*.gschema.xml "$EXTDIR/schemas/"
# The extension's settings schema must also be visible to prefs.js.
install -m 0644 "$ROOT"/gnome-extension/jamsys@jamsys.org/schemas/*.gschema.xml \
                "$STAGE/usr/share/glib-2.0/schemas/"
install -m 0644 "$ROOT/README.md" "$ROOT"/docs/*.md                  "$STAGE/usr/share/doc/jamsys/"

SIZE="$(du -sk "$STAGE" | cut -f1)"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: jamsys
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Maintainer: JamSys <jamsys@localhost>
Installed-Size: ${SIZE}
Depends: libc6, python3 (>= 3.10), python3-gi, gir1.2-gtk-4.0, gir1.2-adw-1, systemd, policykit-1 | polkitd
Recommends: libnotify-bin, gnome-shell (>= 48), gjs
Suggests: nvidia-utils-535 | libnvidia-ml1
Description: Lightweight local system-health monitor
 JamSys answers one question: is this machine behaving normally right now, and
 has anything unusual happened?
 .
 It watches CPU, memory, GPUs, battery and power draw, thermals and fans, storage
 and NVMe health, network and Wi-Fi, Bluetooth, audio, USB devices, systemd units,
 the kernel journal and suspend/resume — using /proc, /sys, netlink, D-Bus and NVML
 directly rather than by shelling out on a timer.
 .
 Anomalies are found with deterministic safety thresholds plus learned per-context
 baselines using robust statistics. Every alert states the measurement, the expected
 range, how long it has been true and what to check. Nothing ever says only
 "anomaly detected".
 .
 Everything is local: no cloud, no telemetry, no account, no packet capture. The
 daemon and the interface both run as your own user; an optional root helper adds
 CPU package power and NVMe SMART and can be left uninstalled.
EOF

cat > "$STAGE/DEBIAN/conffiles" <<'EOF'
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    systemd-tmpfiles --create /usr/lib/tmpfiles.d/jamsys.conf 2>/dev/null || true
    systemctl daemon-reload 2>/dev/null || true
    glib-compile-schemas /usr/share/glib-2.0/schemas 2>/dev/null || true
    glib-compile-schemas /usr/share/gnome-shell/extensions/jamsys@jamsys.org/schemas 2>/dev/null || true

    # The privileged helper is opt-in: installing the package must not silently start
    # something as root. Enable it deliberately with the command printed below.
    echo ""
    echo "JamSys installed."
    echo ""
    echo "  Start the monitor for your user:"
    echo "      systemctl --user daemon-reload"
    echo "      systemctl --user enable --now jamsysd"
    echo ""
    echo "  Then open 'JamSys' from your applications, or run: jamsys"
    echo ""
    echo "  See the instrument cluster right now, in its own window:"
    echo "      jamsys-cluster"
    echo ""
    echo "  Or put it in the GNOME Shell itself. A newly installed extension needs"
    echo "  a session restart on Wayland - log out and back in, then:"
    echo "      gnome-extensions enable jamsys@jamsys.org"
    echo ""
    echo "  Optional, adds CPU package power and NVMe SMART (runs briefly as root"
    echo "  once a minute; see /usr/share/doc/jamsys/privilege-model.md):"
    echo "      sudo systemctl enable --now jamsys-helper.timer"
    echo ""
    echo "  Keyboard RGB control is ready to use: /usr/libexec/jamsys-kbd is"
    echo "  installed with a Polkit action. No extra step needed."
    echo ""
fi
exit 0
EOF

cat > "$STAGE/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
    systemctl disable --now jamsys-helper.timer 2>/dev/null || true
    systemctl stop jamsys-helper.service 2>/dev/null || true
    # Stop the user service for every logged-in user that is running it.
    for uid in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $1}'); do
        runuser -u "#$uid" -- systemctl --user disable --now jamsysd 2>/dev/null || true
    done
fi
exit 0
EOF

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
systemctl daemon-reload 2>/dev/null || true
if [ "$1" = "purge" ]; then
    rm -rf /run/jamsys
    echo "Per-user history and settings are left in ~/.local/share/jamsys and"
    echo "~/.config/jamsys. Remove them yourself if you want them gone."
fi
exit 0
EOF

chmod 0755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

# Compute md5sums, as a well-formed package should.
( cd "$STAGE" && find usr -type f -print0 | xargs -0 md5sum > DEBIAN/md5sums )

OUT="${BUILD_DIR:-$ROOT/build}/jamsys_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT"
echo "==> $OUT"
dpkg-deb --info "$OUT" | head -20
