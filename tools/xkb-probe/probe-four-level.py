#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["evdev"]
# ///
"""
FOUR_LEVEL + mod-dance probe under XWayland.

Builds on probe.py (which confirmed per-device keymap reaches XWayland).
This one tests whether all four xkb levels — including the AltGr (Mod5)
binding daklak relies on for Vietnamese L3/L4 — also resolve correctly
when the source is a non-primary uinput device.

xkb maps AC01 (kc 30) to FOUR_LEVEL [ à, À, á, Á ].
Probe emits these sequences, one per ENTER:
  L1: A                       expect à  (agrave  0x00E0)
  L2: Shift + A               expect À  (Agrave  0x00C0)
  L3: RAlt  + A               expect á  (aacute  0x00E1)
  L4: Shift + RAlt + A        expect Á  (Aacute  0x00C1)

Pass criterion: xev shows expected keysym for each level. Failure modes
worth flagging:
  - L3/L4 produce a/A → Mod5 binding lost in seat-merge; mod tracking
    is seat-global and real-kb keymap (no Mod5 on RALT) wins.
  - L3/L4 produce keysym from "Alt+a" hotkey path → RALT still bound
    to Mod1 in XWayland's view; per-device modifier_map ignored.
  - Any L produces nothing → grab/routing issue (separate problem).

Run order:
  1. sudo -E uv run probe-four-level.py
  2. In another shell, while probe sleeps. Use the FULL absolute path
     to probe.xkb — sway's error path string tells you exactly what it
     tried to open (e.g. "...daklak-rs/probe.xkb" = wrong, file lives
     under tools/xkb-probe/):
       ID=$(swaymsg -t get_inputs | jq -r \
           '.[] | select(.name=="daklak-xkb-probe4").identifier')
       swaymsg input "$ID" xkb_file \
           /home/mozart/Documents/workspace/daklak-rs/tools/xkb-probe/probe-for-level.xkb
       # Verify sway accepted it (no error). Optional:
       swaymsg -t get_inputs | jq '.[] | select(.name=="daklak-xkb-probe4")'
     Note: sway/xkbcommon reports "Inappropriate ioctl for device" for
     several failure modes (missing file, unreadable include, parse
     issue). The path in the error is the literal arg sway received.
  3. env -u WAYLAND_DISPLAY xev -event keyboard   (focus it)
  4. ENTER four times in probe shell — one per level.
"""

import time
from evdev import UInput, ecodes as e

KC_A      = e.KEY_A          # 30
KC_LSHIFT = e.KEY_LEFTSHIFT  # 42
KC_RALT   = e.KEY_RIGHTALT   # 100

cap = {e.EV_KEY: [KC_A, KC_LSHIFT, KC_RALT]}
ui = UInput(cap, name="daklak-xkb-probe4", version=0x3)
print(f"uinput created: {ui.device.path}  name={ui.device.name!r}")
print("Bind probe-four-level.xkb via swaymsg, focus xev, then proceed.")
time.sleep(15)


def tap(*kcs):
    """Press kcs in order, release in reverse. One SYN per transition."""
    for kc in kcs:
        ui.write(e.EV_KEY, kc, 1); ui.syn()
    time.sleep(0.02)
    for kc in reversed(kcs):
        ui.write(e.EV_KEY, kc, 0); ui.syn()


print("IMPORTANT: focus xev WINDOW before each ENTER, not the shell.")
print("Probe waits 2s after ENTER before emitting so you can switch focus.")
for label, expected, kcs in [
    ("L1 plain     ", "à 0x00E0", (KC_A,)),
    ("L2 Shift+A   ", "À 0x00C0", (KC_LSHIFT, KC_A)),
    ("L3 RAlt+A    ", "á 0x00E1", (KC_RALT, KC_A)),
    ("L4 Sh+RAlt+A ", "Á 0x00C1", (KC_LSHIFT, KC_RALT, KC_A)),
]:
    _ = input(f"ENTER then focus xev within 2s ({label} → {expected}): ")
    time.sleep(2.0)
    tap(*kcs)
    time.sleep(0.2)

print("Done. Compare xev keysyms against expected column above.")
time.sleep(1)
ui.close()
