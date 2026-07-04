#!/bin/sh
# Generate daklak xkb artifacts.
set -eu

build_dir="$1"
output="$2"
mode="${3:-symbols}"

case "$mode" in
    keymap)
        "$build_dir/daklak" gen-keymap >"$output"
        ;;
    symbols)
        "$build_dir/daklak" gen-keymap --symbols >"$output"
        ;;
    rules)
        "$build_dir/daklak" gen-keymap --rules >"$output"
        ;;
    *)
        echo "unknown xkb artifact mode: $mode" >&2
        exit 2
        ;;
esac
