#!/usr/bin/env bash
# Remove JamSys completely, including data if asked.
set -euo pipefail
PREFIX="${PREFIX:-$HOME/.local}"

echo "==> stopping the user service"
systemctl --user disable --now jamsysd 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/jamsysd.service"
systemctl --user daemon-reload 2>/dev/null || true

echo "==> removing user-installed files"
rm -f  "$PREFIX/bin/jamsysd" "$PREFIX/bin/jamsys" "$PREFIX/bin/jamsys-cluster"
rm -rf "$PREFIX/lib/jamsys_ui"
rm -f  "$PREFIX/share/applications/org.jamsys.Monitor.desktop"
rm -f  "$PREFIX/share/applications/org.jamsys.Cluster.desktop"
rm -rf "$HOME/.local/share/gnome-shell/extensions/jamsys@jamsys.org"

if command -v dpkg >/dev/null && dpkg -s jamsys >/dev/null 2>&1; then
    echo "==> a jamsys .deb is also installed; remove it with:"
    echo "      sudo apt remove jamsys        # keeps your history"
    echo "      sudo apt purge  jamsys        # also removes /run state"
fi

if systemctl list-unit-files 2>/dev/null | grep -q jamsys-helper; then
    echo "==> the privileged helper is installed; disable it with:"
    echo "      sudo systemctl disable --now jamsys-helper.timer"
fi

echo
read -r -p "Also delete monitoring history and settings? [y/N] " a
if [ "${a:-N}" = "y" ] || [ "${a:-N}" = "Y" ]; then
    rm -rf "$HOME/.local/share/jamsys" "$HOME/.config/jamsys"
    echo "history and settings removed"
else
    echo "kept: ~/.local/share/jamsys and ~/.config/jamsys"
fi
echo "done"
