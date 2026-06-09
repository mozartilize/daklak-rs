#!/usr/bin/env bash
set -euo pipefail

# Log invocation BEFORE anything else so we can confirm kwin spawned us.
echo "[daklak-wrap] invoked $(date -Iseconds) pid=$$ pwd=$(pwd) args=$*" >>/tmp/daklak.log
echo "[daklak-wrap] env: WAYLAND_SOCKET=${WAYLAND_SOCKET:-unset} WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-unset}" >>/tmp/daklak.log

cd "$(dirname "$(readlink -f "$0")")"

# cargo build --workspace --features kde
# Build outside wrapper before invoking kwin — long builds during exec may
# trip kwin's IM-readiness timeout.
# if [[ ! -x ./target/debug/daklak ]]; then
#     echo "daklak-wrap: binary missing — run 'cargo build -p viet-ime-daemon' first" >&2
#     exit 1
# fi

# Keep handler at trace (composition logic) but quiet the raw protocol
# event dump from dispatch_v1 (3-5 events per keystroke = noise).
# export WAYLAND_DEBUG=1
export RUST_LOG=daklak=trace,viet_ime_ibus_adapter=debug,viet_ime_edit_strategy=debug,viet_ime_engine=trace
#export DAKLAK_ENABLE_EVDEV_GRAB=0
#export DAKLAK_ENABLE_WAYLAND=1
# export DAKLAK_FORCE_VK_ONLY_APPS=org.keepassxc.KeePassXC,ONLYOFFICE,steam,xfce4-terminal,com.mitchellh.ghostty

exec ./target/debug/daklak --ibus >>/tmp/daklak.log 2>&1
