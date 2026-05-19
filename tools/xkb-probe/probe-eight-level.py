#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["evdev"]
# ///
"""
EIGHT_LEVEL + Mod3/LevelFive probe under XWayland.

Builds on probe-four-level.py. The new dimension is the LevelFive virtual
modifier — bound to Mod3 via KEY_HENKAN (kc 92) in probe-eight-level.xkb.
If all 8 levels resolve, daklak's B3 design (17 low-kc EIGHT_LEVEL slots,
4 viet pairs per slot) is viable for XWayland.

xkb maps AC01 (kc 30) to EIGHT_LEVEL [ à À á Á â Â ä Ä ].
Probe emits 8 sequences (ENTER-paced, 2s focus window to xev):
  L1: A                          expect à  (0x00E0)
  L2: Shift + A                  expect À  (0x00C0)
  L3: RAlt + A                   expect á  (0x00E1)
  L4: Shift + RAlt + A           expect Á  (0x00C1)
  L5: HENK + A                   expect â  (0x00E2)
  L6: Shift + HENK + A           expect Â  (0x00C2)
  L7: RAlt + HENK + A            expect ä  (0x00E4)
  L8: Shift + RAlt + HENK + A    expect Ä  (0x00C4)

Pass criterion: xev shows the expected keysym for each level under the
expected `state 0x..` mask:
  L1  state 0x00
  L2  state 0x01 (Shift)
  L3  state 0x80 (Mod5  = LevelThree)
  L4  state 0x81
  L5  state 0x20 (Mod3  = LevelFive)
  L6  state 0x21
  L7  state 0xa0
  L8  state 0xa1

Failure modes:
  - L5-L8 produce L1-L4 chars → Mod3 binding to LevelFive not honored by
    XWayland's seat-merged keymap. EIGHT_LEVEL unusable on this stack.
  - L5-L8 produce nothing → HENK keypress dropped before reaching xkb
    state (libinput may filter kc 92 for some device classes).
  - L1-L4 wrong → regression vs the four-level probe, recheck xkb file.

Run order:
  1. sudo -E uv run probe-eight-level.py
  2. swaymsg input <ID-of-daklak-xkb-probe8> xkb_file \\
       /home/mozart/Documents/workspace/daklak-rs/tools/xkb-probe/probe-eight-level.xkb
  3. env -u WAYLAND_DISPLAY xev -event keyboard
  4. ENTER 8 times in probe shell, focus xev within 2s of each.
"""

import time
from evdev import UInput, ecodes as e

KC_A      = e.KEY_A          # 30
KC_LSHIFT = e.KEY_LEFTSHIFT  # 42
KC_RALT   = e.KEY_RIGHTALT   # 100
KC_HENKAN = e.KEY_HENKAN     # 92

cap = {e.EV_KEY: [KC_A, KC_LSHIFT, KC_RALT, KC_HENKAN]}
ui = UInput(cap, name="daklak-xkb-probe8", version=0x3)
print(f"uinput created: {ui.device.path}  name={ui.device.name!r}")
print("Bind probe-eight-level.xkb via swaymsg, focus xev, then proceed.")
time.sleep(15)


def tap(*kcs):
    for kc in kcs:
        ui.write(e.EV_KEY, kc, 1); ui.syn()
    time.sleep(0.02)
    for kc in reversed(kcs):
        ui.write(e.EV_KEY, kc, 0); ui.syn()


print("IMPORTANT: focus xev WINDOW after each ENTER; 2s grace before emit.")
for label, expected, kcs in [
    ("L1 plain          ", "à 0x00E0 state 0x00", (KC_A,)),
    ("L2 Shift          ", "À 0x00C0 state 0x01", (KC_LSHIFT, KC_A)),
    ("L3 RAlt           ", "á 0x00E1 state 0x80", (KC_RALT, KC_A)),
    ("L4 Sh+RAlt        ", "Á 0x00C1 state 0x81", (KC_LSHIFT, KC_RALT, KC_A)),
    ("L5 HENK           ", "â 0x00E2 state 0x20", (KC_HENKAN, KC_A)),
    ("L6 Sh+HENK        ", "Â 0x00C2 state 0x21", (KC_LSHIFT, KC_HENKAN, KC_A)),
    ("L7 RAlt+HENK      ", "ä 0x00E4 state 0xa0", (KC_RALT, KC_HENKAN, KC_A)),
    ("L8 Sh+RAlt+HENK   ", "Ä 0x00C4 state 0xa1", (KC_LSHIFT, KC_RALT, KC_HENKAN, KC_A)),
]:
    _ = input(f"ENTER, focus xev ({label} → {expected}): ")
    time.sleep(2.0)
    tap(*kcs)
    time.sleep(0.2)

print("Done. Compare xev keysym + state vs expected column.")
time.sleep(1)
ui.close()
