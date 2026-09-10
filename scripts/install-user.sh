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

sed "s|/usr/bin/jamsysd|$PREFIX/bin/jamsysd|" \
    "$ROOT/packaging/systemd/jamsysd.service" > "$HOME/.config/systemd/user/jamsysd.service"
sed "s|^Exec=jamsys$|Exec=$PREFIX/bin/jamsys|" \
    "$ROOT/packaging/desktop/org.jamsys.Monitor.desktop" > "$PREFIX/share/applications/org.jamsys.Monitor.desktop"

# GNOME Shell extension. Installed to the per-user directory; GNOME will not load a
# *newly added* extension until the session restarts, which on Wayland means logging
# out. That is a GNOME constraint, not something this script can work around.
EXT_SRC="$ROOT/gnome-extension/jamsys@jamsys.org"
EXT_DST="$HOME/.local/share/gnome-shell/extensions/jamsys@jamsys.org"
if [ -d "$EXT_SRC" ]; then
    mkdir -p "$EXT_DST/schemas"
    install -m 0644 "$EXT_SRC"/*.js "$EXT_SRC/metadata.json" "$EXT_SRC/stylesheet.css" "$EXT_DST/"
    install -m 0644 "$EXT_SRC"/schemas/*.gschema.xml "$EXT_DST/schemas/"
    glib-compile-schemas "$EXT_DST/schemas" 2>/dev/null || true
    echo "==> GNOME Shell extension installed to $EXT_DST"
fi

systemctl --user daemon-reload
systemctl --user enable --now jamsysd
echo "==> installed. Status:"
systemctl --user --no-pager status jamsysd | head -8
echo
echo "Open the interface with:  $PREFIX/bin/jamsys"
if [ -d "$EXT_DST" ]; then
    echo
    echo "The corner readout needs a session restart before GNOME will see it."
    echo "Log out and back in, then:"
    echo "    gnome-extensions enable jamsys@jamsys.org"
    echo "    gnome-extensions prefs  jamsys@jamsys.org"
fi
