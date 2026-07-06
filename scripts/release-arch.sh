#!/usr/bin/env bash
source "$(dirname "$0")/tool.sh"
set -euo pipefail

if [ -z "${1:-}" ]; then
    echo "Usage: $0 <out-path>"
    exit 1
fi

if ! command -v makepkg >/dev/null 2>&1; then
    echo "makepkg is required to build an Arch package" >&2
    exit 1
fi

if [ "$(id -u)" -eq 0 ]; then
    echo "makepkg cannot be run as root; run $0 as a normal user" >&2
    exit 1
fi

TARGET_PATH="$1"
mkdir -pv "$TARGET_PATH"
TARGET_PATH="$(cd "$TARGET_PATH" && pwd)"

VER="$(sed -nE "s/^\s*version:\s*'(.*)',/\1/p" meson.build)"
PKGREL="${DAKLAK_PKGREL:-1}"
ARCH="${CARCH:-$(uname -m)}"
TMP_PATH="$(mktemp -d)"
ROOT_PATH="$PWD"
OUT_PATH="$DAKLAK_OUT"

cleanup() {
    rm -rf "$TMP_PATH"
}
trap cleanup EXIT

printf -v ROOT_PATH_Q "%q" "$ROOT_PATH"
printf -v OUT_PATH_Q "%q" "$OUT_PATH"

cat > "$TMP_PATH/PKGBUILD" <<PKGBUILD
pkgname=daklak
pkgver=$VER
pkgrel=$PKGREL
pkgdesc='Vietnamese Input Method'
arch=('$ARCH')
url='https://github.com/mozartilize/daklak-rs'
license=('GPL-3.0-or-later')
depends=('libxkbcommon' 'libxcb')
options=('!debug')
source=()
sha256sums=()

_daklak_root=$ROOT_PATH_Q
_daklak_out=$OUT_PATH_Q

package() {
    cd "\$_daklak_root"
    DAKLAK_OUT="\$_daklak_out" scripts/install.sh "\$pkgdir"
}
PKGBUILD

(
    cd "$TMP_PATH"
    PKGDEST="$TARGET_PATH" makepkg --force --nodeps
)
