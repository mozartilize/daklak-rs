#!/bin/sh
# Check generated Meson launch metadata for release vs. debug installs.
set -eu

mode="$1"
build_dir="$2"
source_root="${3:-$(pwd)}"
prefix="${4:-/usr/local}"
bindir="${5:-bin}"

desktop="$build_dir/daklak.desktop"
ibus_xml="$build_dir/daklak.xml"

case "$mode" in
  debug)
    expected_desktop_exec="$source_root/daklak-wrap.sh"
    expected_ibus_exec="$source_root/daklak-ibus-wrap.sh"
    ;;
  release)
    expected_desktop_exec="$prefix/$bindir/daklak"
    expected_ibus_exec="$prefix/$bindir/daklak --ibus"
    ;;
  *)
    echo "usage: $0 debug|release BUILD_DIR [SOURCE_ROOT] [PREFIX] [BINDIR]" >&2
    exit 2
    ;;
esac

grep -F "Exec=$expected_desktop_exec" "$desktop" >/dev/null
grep -F "<exec>$expected_ibus_exec</exec>" "$ibus_xml" >/dev/null
