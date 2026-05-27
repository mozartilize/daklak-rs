#!/bin/sh
# Called by Meson custom_target to build the daemon binary via Cargo.
set -eu

cargo="$1"
target_dir="$2"
profile="$3"
output="$4"
source_root="$5"
features="${6:-}"

export CARGO_TARGET_DIR="$target_dir"

"$cargo" build --locked \
    --manifest-path "$source_root/Cargo.toml" \
    --package viet-ime-daemon \
    --bin daklak \
    $( [ "$profile" = "release" ] && echo --release ) \
    $( [ -n "$features" ] && echo "--features $features" )

cp "$target_dir/$profile/daklak" "$output"
