#!/bin/sh
# Called by Meson custom_target to build the daemon binary via Cargo.
set -eu

cargo="$1"
target_dir="$2"
profile="$3"
output="$4"
source_root="$5"
features="${6:-}"
builtin_hook_dir="${7:-}"
builtin_xkb_dir="${8:-}"

export CARGO_TARGET_DIR="$target_dir"
if [ -n "$builtin_hook_dir" ]; then
    export DAKLAK_BUILTIN_HOOK_DIR="$builtin_hook_dir"
fi
if [ -n "$builtin_xkb_dir" ]; then
    export DAKLAK_BUILTIN_XKB_DIR="$builtin_xkb_dir"
fi

"$cargo" build --locked \
    --manifest-path "$source_root/Cargo.toml" \
    --package viet-ime-daemon \
    --bin daklak \
    $( [ "$profile" = "release" ] && echo --release ) \
    $( [ -n "$features" ] && echo "--features $features" )

# Copy via a temp file + atomic rename. A plain `cp` truncates the destination,
# which fails with ETXTBSY ("Text file busy") when the previous daklak binary is
# still running. rename() swaps the directory entry without touching the busy
# inode, so a running daklak keeps its old file and the new build lands cleanly.
tmp="$output.tmp.$$"
cp "$target_dir/$profile/daklak" "$tmp"
mv -f "$tmp" "$output"
