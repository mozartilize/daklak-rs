#!/usr/bin/env bash
source "$(dirname "$0")/tool.sh"
set -euo pipefail

if [ -z "${1:-}" ]; then
    echo "Usage: $0 <out-path>"
    exit 1
fi

VER="$(sed -nE "s/^\s*version:\s*'(.*)',/\1/p" meson.build)"

tar -cvf - -C "$DAKLAK_OUT" . | zstd -T0 -15 -o "${1}/daklak-${VER}.tar.zst"
