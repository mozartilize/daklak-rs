#!/usr/bin/env bash
source "$(dirname "$0")/tool.sh"
set -euo pipefail

mkdir -pv "$DAKLAK_OUT"

DAKLAK_FEATURES="${DAKLAK_FEATURES:-}"

set_release() {
    TARGET_DIR=release
    CARGO_ARGS="--release"
}

set_debug() {
    TARGET_DIR=debug
    CARGO_ARGS=""
}

set_release

while getopts "hrdf:" opt; do
    case $opt in
        h)
            echo "build.sh"
            echo "-r: release mode (default)"
            echo "-d: debug mode"
            echo "-f: cargo features (comma-separated, e.g. kde,ibus,evdev_grab)"
            exit 0
            ;;
        r) set_release ;;
        d) set_debug ;;
        f) DAKLAK_FEATURES="$OPTARG" ;;
    esac
done

FEATURES_FLAG=""
if [ -n "$DAKLAK_FEATURES" ]; then
    FEATURES_FLAG="--features $DAKLAK_FEATURES"
fi

cargo build $CARGO_ARGS --locked --package viet-ime-daemon --bin daklak $FEATURES_FLAG

cp "target/$TARGET_DIR/daklak" "$DAKLAK_OUT/"
