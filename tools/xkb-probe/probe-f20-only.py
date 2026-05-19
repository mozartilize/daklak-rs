#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["evdev"]
# ///
"""
Isolate the kc 190 (KEY_F20) delivery question.

Creates a uinput device with ONLY KEY_F20 in its capability bitmap,
sleeps 10s for you to focus xev (or apply the daklak per-device keymap
if you want to verify EIGHT_LEVEL decoding), then emits one F20 tap.

Pass: xev shows a KeyPress at X11 kc 198. Then daklak's missing-FK20 bug
is in daklak's path (uinput cap setup or emit ordering).
Fail: xev silent. kc 198 is being filtered by the X server / kernel /
xfce somewhere downstream of uinput, independent of daklak.

Run:
  sudo -E uv run probe-f20-only.py
  # in another shell: xev -event keyboard, focus the xev window.
"""
import time
from evdev import UInput, ecodes as e

ui = UInput({e.EV_KEY: [e.KEY_F20]}, name="f20-only-probe")
print(f"uinput created: {ui.device.path}  name={ui.device.name!r}")
print("Focus xev (or any app) within 10s.")
time.sleep(10)
ui.write(e.EV_KEY, e.KEY_F20, 1); ui.syn()
time.sleep(0.05)
ui.write(e.EV_KEY, e.KEY_F20, 0); ui.syn()
print("Emitted kc 190 (KEY_F20) tap.")
time.sleep(2)
ui.close()
