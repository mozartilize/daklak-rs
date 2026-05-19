#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["evdev"]
# ///
"""
XWayland per-device-keymap probe.

Question: when sway loads `xkb_file` for ONE input device, does that keymap
reach XWayland (per-device), or does XWayland keep using the seat-primary
keyboard's keymap (seat-scoped)?

If per-device → daklak's "2 uinput + 2 xkb_file" plan can deliver Vietnamese
to XWayland clients with kc ≤ 255. If seat-scoped → blocked.

Run order:
  1. sudo -E uv run probe.py     (or: sudo ./probe.py)
     Creates uinput device "daklak-xkb-probe", sleeps 15s.
  2. In another shell, while probe sleeps. Use the FULL absolute path
     to probe.xkb — sway's error path string tells you exactly what it
     tried to open (e.g. "...daklak-rs/probe.xkb" = wrong, file lives
     under tools/xkb-probe/):
       ID=$(swaymsg -t get_inputs | jq -r \
           '.[] | select(.name=="daklak-xkb-probe").identifier')
       swaymsg input "$ID" xkb_file \
           /home/mozart/Documents/workspace/daklak-rs/tools/xkb-probe/probe.xkb
       # Verify sway accepted it (no error). Optional:
       swaymsg -t get_inputs | jq '.[] | select(.name=="daklak-xkb-probe")'
     Note: sway/xkbcommon reports "Inappropriate ioctl for device" for
     several failure modes (missing file, unreadable include, parse
     issue). The path in the error is the literal arg sway received.
  3. Launch an XWayland xev (NOT a Wayland-native one):
       env -u WAYLAND_DISPLAY xev -event keyboard
     Focus the xev window.
  4. Return to probe shell, hit ENTER. Probe emits kc 30 once.
  5. Read xev output:
       keysym 0x07e1 (Greek_alpha, α)  → WIN — per-device keymap honored.
       keysym 0x0061 (a)               → BLOCKED — seat-scoped, idea dead.
       no event at all                 → grab/routing issue, separate problem.

Requires: uv (PEP 723 runner — auto-installs evdev), jq, sway, xev built
against Xlib (xorg-xev pkg).
"""

import time
from evdev import UInput, ecodes as e

ui = UInput({e.EV_KEY: [e.KEY_A]}, name="daklak-xkb-probe", version=0x3)
print(f"uinput created: {ui.device.path}  name={ui.device.name!r}")
print("Now run the swaymsg step from the docstring, then launch xev.")
print("Sleeping 15s before accepting ENTER...")
time.sleep(15)

input("xev focused under XWayland? Press ENTER to emit kc 30 (KEY_A): ")

ui.write(e.EV_KEY, e.KEY_A, 1); ui.syn()
time.sleep(0.05)
ui.write(e.EV_KEY, e.KEY_A, 0); ui.syn()

print("Emitted. Check xev. α=0x07e1 → win. a=0x0061 → blocked.")
time.sleep(2)
ui.close()
