#!/bin/sh
# Installs SecondScrn from this folder.
#
#   sudo ./install.sh              # system-wide into /usr/local (recommended)
#   ./install.sh --user            # just for you, into ~/.local (no pen tablet:
#                                  # the udev rule needs root)
#   sudo ./install.sh --uninstall  # (or ./install.sh --user --uninstall)
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
app_id="dev.secondscrn.SecondScrn"
user=false
uninstall=false
for a in "$@"; do
    case "$a" in
        --user) user=true ;;
        --uninstall) uninstall=true ;;
        *) echo "unknown option $a"; exit 2 ;;
    esac
done
if $user; then
    bin="$HOME/.local/bin"
    share="${XDG_DATA_HOME:-$HOME/.local/share}"
else
    [ "$(id -u)" = 0 ] || { echo "Run with sudo, or pass --user."; exit 1; }
    bin=/usr/local/bin
    share=/usr/local/share
fi
files="applications/$app_id.desktop metainfo/$app_id.metainfo.xml icons/hicolor/256x256/apps/$app_id.png"
if $uninstall; then
    rm -f "$bin/secondscrn" "$bin/secondscrn-cli"
    for f in $files; do rm -f "$share/$f"; done
    $user || rm -f /etc/udev/rules.d/70-secondscrn-uinput.rules
    echo "SecondScrn removed (your pairings and settings stay in ~/.local/share/SecondScrn)."
    exit 0
fi
install -Dm755 "$here/usr/bin/secondscrn" "$bin/secondscrn"
install -Dm755 "$here/usr/bin/secondscrn-cli" "$bin/secondscrn-cli"
for f in $files; do install -Dm644 "$here/usr/share/$f" "$share/$f"; done
if $user; then
    # The desktop entry must find the binary even when ~/.local/bin isn't on PATH.
    sed -i "s|^Exec=secondscrn|Exec=$bin/secondscrn|" "$share/applications/$app_id.desktop"
    echo "Installed for $(id -un). For the pen tablet (pressure, tilt), also run as root:"
    echo "  install -Dm644 $here/usr/lib/udev/rules.d/70-secondscrn-uinput.rules /etc/udev/rules.d/"
else
    install -Dm644 "$here/usr/lib/udev/rules.d/70-secondscrn-uinput.rules" /etc/udev/rules.d/70-secondscrn-uinput.rules
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --subsystem-match=misc --sysname-match=uinput 2>/dev/null || true
    echo "Installed. Start SecondScrn from the application menu."
fi
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t -f "$share/icons/hicolor" 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$share/applications" 2>/dev/null || true
