#!/usr/bin/env bash
# probe-xwayland.sh — every XWayland toplevel daklak's X11 bridge will track.
set -euo pipefail
: "${DISPLAY:?DISPLAY not set — no XWayland on this session, bridge will be skipped}"

# Option A: wmctrl (cleanest output if installed: pacman -S wmctrl)
if command -v wmctrl >/dev/null; then
    echo "== wmctrl -lpx =="
    wmctrl -lpx   # win-id  desktop  pid  WM_CLASS  hostname  title
    exit 0
fi

# Option B: pure xprop/xwininfo (always available with libX11)
echo "== xprop fallback =="
xwininfo -root -children -tree \
  | awk '/^ +0x[0-9a-f]+/{print $1}' \
  | while read -r win; do
        cls=$(xprop -id "$win" WM_CLASS 2>/dev/null | sed -n 's/^WM_CLASS(STRING) = //p')
        name=$(xprop -id "$win" _NET_WM_NAME 2>/dev/null | sed -n 's/^_NET_WM_NAME[^=]*= //p')
        pid=$(xprop -id "$win" _NET_WM_PID 2>/dev/null | sed -n 's/^_NET_WM_PID(CARDINAL) = //p')
        [[ -n "$cls$name" ]] && printf '%-12s  pid=%-6s  class=%-40s  name=%s\n' "$win" "${pid:--}" "${cls:--}" "${name:--}"
    done