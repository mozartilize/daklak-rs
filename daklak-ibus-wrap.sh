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

# Keep handler at debug while leaving the rest at error.
# export WAYLAND_DEBUG=1
export DAKLAK_LOG_PATH=/tmp/daklak.log
export DAKLAK_LOG_LEVEL=error
export DAKLAK_LOG_MODULES=daklak=debug,viet_ime_ibus_adapter=info,viet_ime_edit_strategy=info,viet_ime_engine=debug
#export DAKLAK_ENABLE_EVDEV_GRAB=0
#export DAKLAK_ENABLE_WAYLAND=1
# export DAKLAK_FORCE_VK_ONLY_APPS=org.keepassxc.KeePassXC,ONLYOFFICE,steam,xfce4-terminal,com.mitchellh.ghostty

exec ./target/debug/daklak --ibus
