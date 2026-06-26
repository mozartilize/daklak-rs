#!/bin/sh
# Generate the installable xkb_symbols fragment for daklak.
set -eu

build_dir="$1"
output="$2"

"$build_dir/daklak" gen-keymap --symbols >"$output"
