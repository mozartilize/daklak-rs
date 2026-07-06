#!/usr/bin/env bash
source "$(dirname "$0")/tool.sh"
set -euo pipefail

if [ -z "${1:-}" ]; then
    echo "Usage: $0 <install-prefix>"
    exit 1
fi

PREFIX="$1"
BIN_DIR="${DAKLAK_BIN_DIR:-usr/bin}"
DATA_DIR="${DAKLAK_DATA_DIR:-usr/share/daklak}"
ICON_DIR="${DAKLAK_ICON_DIR:-usr/share/icons/hicolor/scalable/apps}"
DESKTOP_DIR="${DAKLAK_DESKTOP_DIR:-usr/share/applications}"
XKB_DIR="${DAKLAK_XKB_DIR:-usr/share/X11/xkb/symbols}"
HOOK_DIR="${DAKLAK_HOOK_DIR:-usr/libexec/daklak/hooks}"
IBUS_DIR="${DAKLAK_IBUS_DIR:-usr/share/ibus/component}"
LIBEXEC_DIR="${DAKLAK_LIBEXEC_DIR:-usr/libexec/daklak}"

install_bin() {
    install -Dm755 "$DAKLAK_OUT/$1" -t "$PREFIX/$BIN_DIR"
}

install_data() {
    install -Dm644 "$DAKLAK_OUT/$1" -t "$PREFIX/$DATA_DIR"
}

install_optional() {
    local src="$DAKLAK_OUT/$1"
    if [ -f "$src" ]; then
        install -Dm644 "$src" "$PREFIX/$2"
    fi
}

install_bin daklak

install_optional config.toml.example "$DATA_DIR/config.toml.example"
install -Dm644 res/99-daklak-input.rules "$PREFIX/$DATA_DIR/99-daklak-input.rules"

if [ -f "$DAKLAK_OUT/daklak-native.svg" ]; then
    install -Dm644 "$DAKLAK_OUT/daklak-native.svg" "$PREFIX/$ICON_DIR/daklak-native.svg"
fi
if [ -f "$DAKLAK_OUT/daklak-evdev.svg" ]; then
    install -Dm644 "$DAKLAK_OUT/daklak-evdev.svg" "$PREFIX/$ICON_DIR/daklak-evdev.svg"
fi

if [ -f "$DAKLAK_OUT/daklak.desktop" ]; then
    install -Dm644 "$DAKLAK_OUT/daklak.desktop" "$PREFIX/$DESKTOP_DIR/daklak.desktop"
fi

if [ -f "$DAKLAK_OUT/daklak_vn" ]; then
    install -Dm644 "$DAKLAK_OUT/daklak_vn" "$PREFIX/$XKB_DIR/daklak_vn"
fi

if [ -f "$DAKLAK_OUT/daklak.xml" ]; then
    install -Dm644 "$DAKLAK_OUT/daklak.xml" "$PREFIX/$IBUS_DIR/daklak.xml"
fi

for hook in "$DAKLAK_OUT"/hooks/*; do
    [ -f "$hook" ] || continue
    install -Dm755 "$hook" "$PREFIX/$HOOK_DIR/$(basename "$hook")"
done
