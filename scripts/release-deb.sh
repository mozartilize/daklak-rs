#!/usr/bin/env bash
source "$(dirname "$0")/tool.sh"
set -euo pipefail

if [ -z "${1:-}" ]; then
    echo "Usage: $0 <out-path>"
    exit 1
fi

TARGET_PATH="$1"
TMP_PATH="$(mktemp -d)"
ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m)"
VER="$(sed -nE "s/^\s*version:\s*'(.*)',/\1/p" meson.build)"
DISTRO="${DAKLAK_DISTRO:-}"
DISTRO_SUFFIX="${DISTRO:+_${DISTRO}}"

mkdir -pv "$TMP_PATH/DEBIAN"

sed "s/%VER%/$VER/g; s/%ARCH%/$ARCH/g" scripts/control.in > "$TMP_PATH/DEBIAN/control"

DAKLAK_OUT="$DAKLAK_OUT" scripts/install.sh "$TMP_PATH"

dpkg-deb --root-owner-group --build "$TMP_PATH" "${TARGET_PATH}/daklak_${VER}${DISTRO_SUFFIX}_${ARCH}.deb"
rm -rf "$TMP_PATH"
