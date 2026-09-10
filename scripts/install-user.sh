#!/usr/bin/env bash
# Install JamSys for the current user only, from a source tree, without root.
# Useful for development or on a machine where you would rather not install a .deb.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
TARGET="${CARGO_TARGET_DIR:-$ROOT/jamsys-daemon/target}"

echo "==> building"
( cd "$ROOT/jamsys-daemon" && cargo build --release )

mkdir -p "$PREFIX/bin" "$PREFIX/lib/jamsys_ui" "$PREFIX/share/applications" \
         "$HOME/.config/systemd/user"
install -m 0755 "$TARGET/release/jamsysd" "$PREFIX/bin/jamsysd"
install -m 0644 "$ROOT"/jamsys-ui/jamsys_ui/*.py "$PREFIX/lib/jamsys_ui/"
sed "s|^sys.path.insert.*|sys.path.insert(0, '$PREFIX/lib')|" \
    "$ROOT/jamsys-ui/bin/jamsys" > "$PREFIX/bin/jamsys"
chmod 0755 "$PREFIX/bin/jamsys"
install -m 0755 "$ROOT/jamsys-ui/bin/jamsys-cluster" "$PREFIX/bin/jamsys-cluster"

sed "s|/usr/bin/jamsysd|$PREFIX/bin/jamsysd|" \
    "$ROOT/packaging/systemd/jamsysd.service" > "$HOME/.config/systemd/user/jamsysd.service"
sed "s|^Exec=jamsys$|Exec=$PREFIX/bin/jamsys|" \
    "$ROOT/packaging/desktop/org.jamsys.Monitor.desktop" > "$PREFIX/share/applications/org.jamsys.Monitor.desktop"
sed "s|^Exec=jamsys-cluster$|Exec=$PREFIX/bin/jamsys-cluster|" \
    "$ROOT/packaging/desktop/org.jamsys.Cluster.desktop" > "$PREFIX/share/applications/org.jamsys.Cluster.desktop"

# GNOME Shell extension. Installed to the per-user directory; GNOME will not load a
# *newly added* extension until the session restarts, which on Wayland means logging
# out. That is a GNOME constraint, not something this script can work around.
EXT_SRC="$ROOT/gnome-extension/jamsys@jamsys.org"
EXT_DST="$HOME/.local/share/gnome-shell/extensions/jamsys@jamsys.org"
if [ -d "$EXT_SRC" ]; then
    mkdir -p "$EXT_DST/schemas"
    # *.js covers extension.js, prefs.js, format.js, gauges.js and standalone.js —
    # the standalone window lives beside the extension because it shares gauges.js.
    install -m 0644 "$EXT_SRC"/*.js "$EXT_SRC/metadata.json" "$EXT_SRC/stylesheet.css" "$EXT_DST/"
    install -m 0644 "$EXT_SRC"/schemas/*.gschema.xml "$EXT_DST/schemas/"
    glib-compile-schemas "$EXT_DST/schemas" 2>/dev/null || true
    echo "==> GNOME Shell extension installed to $EXT_DST"
fi

systemctl --user daemon-reload
systemctl --user enable jamsysd
# `enable --now` starts a stopped unit but does nothing to a running one, which on a
# reinstall leaves the old process executing the replaced-and-now-deleted inode. The
# files on disk look current while the running daemon is not, which is exactly the
# kind of difference nobody thinks to check. Always restart.
systemctl --user restart jamsysd
echo "==> installed. Status:"
systemctl --user --no-pager status jamsysd | head -8
echo
JD_PID="$(systemctl --user show -p MainPID --value jamsysd)"
if [ -n "$JD_PID" ] && [ "$JD_PID" != "0" ]; then
    if readlink "/proc/$JD_PID/exe" 2>/dev/null | grep -q '(deleted)'; then
        echo "==> WARNING: the running daemon is still on a replaced binary"
    else
        echo "==> daemon is running the freshly installed binary (pid $JD_PID)"
    fi
fi

echo "Open the interface with:  $PREFIX/bin/jamsys"
echo "Show the cluster now with: $PREFIX/bin/jamsys-cluster"
if [ -d "$EXT_DST" ]; then
    echo
    echo "The corner readout needs a session restart before GNOME will see it."
    echo "Log out and back in, then:"
    echo "    gnome-extensions enable jamsys@jamsys.org"
    echo "    gnome-extensions prefs  jamsys@jamsys.org"
fi
