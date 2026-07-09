# Evdev setup

[← Back to index](../README.md)

This is the deep-dive companion to the [evdev transport](transports.md#evdev)
overview. It covers the synthetic keymap, why it's needed, and the
platform-specific steps to install it.

## Contents

- [What evdev mode is](#what-evdev-mode-is)
- [Why a synthetic keymap is needed](#why-a-synthetic-keymap-is-needed)
- [Generating the keymap](#generating-the-keymap)
- [sway / scroll (per-device)](#sway--scroll-per-device)
- [KDE Plasma, GNOME, X11](#kde-plasma-gnome-x11)
- [SIGKILL recovery](#sigkill-recovery)
- [Troubleshooting](#troubleshooting)

## What evdev mode is

Daklak's evdev mode (`enable_evdev_grab = true`) grabs every keyboard via
`/dev/input/event*`, runs the engine on raw keycodes, and emits both
pass-through ASCII and Vietnamese precomposed characters through a
daklak-owned `daklak` uinput device.

Evdev mode can be the **startup backend** (`enable_wayland = false`,
`enable_evdev_grab = true`) or switched to at **runtime** from the native
desktop backend (IBus/Wayland):

```
daklak backend evdev       # switch to evdev at runtime
daklak backend native      # switch back
```

The tray icon also shows an "Enable evdev" checkbox when evdev-grab support
is compiled in.

Compared to Wayland mode, no `zwp_input_method_v2` is involved — it works on any
compositor (sway, scroll, KWin, Mutter, X11) as long as keyboard-class
`/dev/input/event*` is readable by the daklak user (`input` group membership)
and `/dev/uinput` is writable (`uinput` group membership).

## Security model

Evdev mode is a high-trust fallback. While active, Daklak reads keyboard events
from `/dev/input/event*`, holds exclusive grabs on the physical keyboards, and
injects replacement/pass-through events through `/dev/uinput`. That is the same
class of privilege as a local key remapper: it can observe keystroke metadata and
can inject keystrokes into the active session. Daklak separates those permissions
where possible: the `input` group gates physical keyboard access, while the
`uinput` group gates synthetic input injection.

Use evdev mode only for sessions where you trust the Daklak process and its
configuration. Prefer the native Wayland/IBus backend when it works for your
applications.

`daklak disable` turns Vietnamese composition off but does not release the evdev
grab; keys are forwarded raw through Daklak. To release the physical keyboard
grab, switch back to the native backend:

```sh
daklak backend native
```

Avoid leaving `RUST_LOG=trace` or equivalent trace-level module logging enabled
while typing sensitive input. Trace logs can include keycodes and input-state
metadata useful for reconstructing keystrokes.

## Device permissions

Evdev mode needs two Linux input permissions:

- `input` group access for `/dev/input/event*` so Daklak can read and grab
  physical keyboards.
- `uinput` group access for `/dev/uinput` so Daklak can create its synthetic
  keyboard.

Set them up manually (no install script touches these — they are system-wide
privileges you control):

```sh
# 1. Make sure the uinput group exists
sudo groupadd --system uinput

# 2. Add your user to the input and uinput groups
sudo usermod -aG input,uinput $USER

# 3. Install the udev rule so /dev/uinput gets the right permissions
#    (copy from the installed reference or create it directly)
sudo cp /usr/share/daklak/99-daklak-input.rules /etc/udev/rules.d/99-daklak.rules

# 4. Reload udev and trigger the rule
sudo udevadm control --reload-rules
sudo udevadm trigger --name-match=uinput

# 5. Load the uinput kernel module
sudo modprobe uinput
```

Log out and back in after changing group membership (or use `newgrp` /
`sg` in the session that starts daklak). Verify `/dev/uinput` is owned by
`root:uinput` with mode `crw-rw----`.

Daklak installs the rule as a reference to `/usr/share/daklak/99-daklak-input.rules`
(or `~/.local/share/daklak/99-daklak-input.rules` for user-local installs).
Copy it to `/etc/udev/rules.d/99-daklak.rules` to activate — daklak never
writes to `/etc` on your behalf.

## Why a synthetic keymap is needed

The engine commits precomposed characters (`ấ`, `ầ`, …) that don't exist in the
`us` xkb layout. Uinput emits raw keycodes; the compositor maps them through its
active layout. Without a layout that knows Vietnamese precomposed chars at the
slot keycodes daklak picks, those commits would render as the keycode's default
keysym (a Japanese IME function, an F13+ keysym, etc).

Daklak generates the same synthetic keymap the Wayland ForwardKey
virtual-keyboard channel uploads to `zwp_virtual_keyboard_v1` — 17 `EIGHT_LEVEL`
custom slots over evdev keycodes ≤ 255 so XWayland clients receive them too:

- **IME zone** (kc 85,86,89-94 with 92 reserved): `<ZEHA>`, `<LSGT>`, `<AB11>`,
  `<KATA>`, `<HIRA>`, `<HKTG>`, `<MUHE>`, `<AE13>` (KEY_YEN) — 8 slots.
- **F13-F19** (kc 183-189): `<FK13>..<FK19>` — 7 slots.
- **Korean IME** (kc 122, 123): `<HNGL>` (KEY_HANGEUL), `<HJCV>` (KEY_HANJA) —
  2 slots.

`<AE13>` (KEY_YEN, kc 124) carries slot 7 instead of `<JPCM>` (KEY_KPJPCOMMA,
kc 95): Chromium/Electron clients (e.g. VSCode) have no DomCode for evdev 95,
so keysyms bound there never turn into text in those apps — the i-family (ì í ỉ
ĩ) would silently vanish while every other composition rendered. KEY_YEN maps
to Chromium's live `IntlYen` DomCode, so it works in Chromium and native clients
alike.

F20/F21 (kc 190/191) are deliberately skipped — they were observed silently
filtered on at least one xfce4 X11 session (the event reached the uinput cap and
the xkb keymap had the right keysym bound, but `xev` never saw the KeyPress).
The source of the filter wasn't pinned down, so we route around it.

Slot 92 (`KEY_HENKAN`, `<HENK>`) is reserved as the `ISO_Level5_Shift` modifier
carrier so the keymap can address levels 5-8 via Mod3. Daklak never physically
presses `<HENK>` — it sets the Mod3 bit synthetically before pressing the slot
keycode. The keymap applies **per-device** to daklak's own uinput device so the
user's other keyboards are unaffected.

The slot keycodes and names live in
[`crates/keymap/src/lib.rs`](../crates/keymap/src/lib.rs) (`SAFE_KEYCODES` /
`SAFE_KEYCODE_NAMES`, with the L5 carrier as `LEVEL5_SHIFT_EVDEV`).

### XWayland compatibility

Slot keycodes deliberately stay ≤ 255 so the X11 8-bit keycode field can carry
them. Firefox-X11, JetBrains IDEs, and any other Xorg-only client receives
Vietnamese precomposed chars the same way native Wayland clients (foot,
Chromium-Wayland, kate-Wayland) do. Each emit triggers an X11 `MappingNotify`
storm as the compositor re-pushes the seat keymap on device switch — observable
in `xev` but harmless in practice (clients typically lazy-cache xkb state).

### Known limitation: EIGHT_LEVEL and old X11 apps

The synth keymap uses `EIGHT_LEVEL` xkb type to pack 4 Vietnamese
character pairs per keycode (17 keycodes for 134 keysyms). Levels 5-8
require `ISO_Level5_Shift` (bound to `<HENK>`/Mod3). **Old X11 apps
that only support up to 4 xkb levels** — notably **urxvt** — silently
ignore levels 5-8 and produce the level-1 character instead, yielding
wrong output for roughly half the Vietnamese inventory.

Native Wayland clients (foot, ghostty, Firefox Wayland, kate) and
modern XWayland clients (Chromium, Firefox X11, JetBrains, OnlyOffice,
Steam) handle all 8 levels correctly.

Switching to `FOUR_LEVEL` (2 pairs per key) would fix urxvt but
requires 34 keycodes instead of 17. Finding 17 additional safe
keycodes with Chromium DomCode support is not feasible — only ~13
candidates exist (see `plans/four-level-keymap-migration.md`). Since
xkb ties Shift to level selection (Shift IS level 2, not an
independent case modifier), uppercase cannot be decoupled from the
level count — each character pair (lower + Shift→upper) always
consumes 2 levels.

This is accepted as a known limitation: old X11 terminal emulators
that lack full xkb level support are not a target platform. Users on
urxvt can switch to a Wayland-native terminal or use the Wayland
transport (text-input-v3 / input-method-v1) instead of evdev grab.

## Generating the keymap

Daklak's `gen-keymap` subcommand prints xkb keymap material to stdout. The
daemon itself never writes a file or calls into the compositor — keymap setup
is the user's responsibility (one `swaymsg` / `xkbcomp` / `kwriteconfig6` /
`gsettings` invocation, easily wrapped in a systemd unit). Three output forms:

- `daklak gen-keymap` — the **full keymap blob** (`xkb_keymap { … }`). For the
  per-device / session-wide blob paths (`swaymsg … xkb_file`, `xkbcomp`).
- `daklak gen-keymap --symbols` — the standalone `xkb_symbols` fragment, with
  **two sections**: `basic` (self-contained: includes `pc+us+inet(evdev)` then
  applies the daklak overrides — for selecting `daklak_vn` as a layout) and
  `overlay` (the daklak overrides *only*, no base include — for applying daklak
  as an xkb **option** on KDE/GNOME Wayland; see below).
- `daklak gen-keymap --rules` — the user-local **rules overlay** that pulls in
  the stock `evdev` ruleset and registers the `daklak:vn` option mapping to
  `daklak_vn(overlay)`.

```
daklak gen-keymap          > /tmp/daklak.xkb                      # blob
daklak gen-keymap --symbols > ~/.config/xkb/symbols/daklak_vn     # option path (Wayland)
daklak gen-keymap --rules   > ~/.config/xkb/rules/evdev           # option path (Wayland)
```

For manual setup, the **blob** paths (sway/X11) load the generated blob directly.
The **option** paths (KDE/GNOME Wayland) put the two files under
`~/.config/xkb/` (user-local, no root, no system-file edits); libxkbcommon
searches `~/.config/xkb` first, so they shadow the stock ruleset cleanly.

Meson installs all three generated artifacts when configured with
`-Devdev_grab=true` and `sudo meson install`: `daklak.xkb`, `daklak_vn`, and
`evdev` under `${datadir}/daklak/xkb` (typically `/usr/share/daklak/xkb`). It
also installs `daklak_vn` system-wide under `/usr/share/X11/xkb/symbols/` for
the `setxkbmap daklak_vn` *layout* path. The built-in hooks consume these
installed files instead of running `daklak gen-keymap` at runtime.

Verify it parses cleanly (optional — the slot/name table is checked at compile
time via a `const` assertion, so this is just for hand-editing or CI):

```
xkbcomp -I/usr/share/X11/xkb -xkb /tmp/daklak.xkb /tmp/daklak.xkm
```

Harmless warnings to expect (both `xkbcomp -xkb …` and `xkbcomp … $DISPLAY`
flows):

- `Keycodes above 256 (e.g. <I256>) are not supported by X and are ignored` —
  X11 wire keycodes cap at 8 bits. Daklak slots all sit under 200, but
  `evdev+aliases(qwerty)` declares keys past that.
- `<I###> not found in evdev_daklak keycodes` / `No symbols defined for <I###>`
  — same root cause. The include declares names daklak doesn't bind symbols for.
  Non-fatal.
- `Multiple symbols for level 1/group 1 on key <FK23>` / `Symbol map for key
  <FK23> redefined` — pre-existing collision between `pc+us+inet(evdev)`'s
  `XF86TouchpadOff` and `evdev`'s `F23` on X11 kc 201. Unrelated to daklak. The
  include ships this way.

Real failures look different: parse errors, `Maximum code (X) must be >= …`, or
"not found" warnings naming `<FK13..21>`, `<ZEHA>`, `<HKTG>`, `<LSGT>`,
`<KATA>`, `<HIRA>`, `<MUHE>`, `<AE13>`, `<AB11>`, or `<HENK>` — those are
daklak's own slot names. If you see those, the keycode naming in
[`crates/keymap/src/lib.rs`](../crates/keymap/src/lib.rs) (`SAFE_KEYCODE_NAMES`)
has drifted from the system's evdev keycodes file.

## sway / scroll (per-device)

Generate the keymap, then point sway at daklak's uinput device:

```
daklak gen-keymap > /tmp/daklak.xkb
swaymsg input "56001:44033:daklak" xkb_file /tmp/daklak.xkb
```

The numeric prefix is the decimal `vendor:product` of daklak's uinput device —
`0xdac1:0xac01`, see
[`crates/edit-strategy/src/uinput_device.rs`](../crates/edit-strategy/src/uinput_device.rs)
(`DAKLAK_UINPUT_VENDOR` / `DAKLAK_UINPUT_PRODUCT`) for the constants.

Per-device — physicals and other keyboards untouched. Sway forgets the rule when
the uinput device disappears on daklak exit, so no cleanup needed (SIGKILL-safe).

If you want this driven automatically by daklak's lifecycle, wire it through a
user systemd unit, e.g.:

```ini
# ~/.config/systemd/user/daklak.service
[Service]
ExecStartPre=/bin/sh -c 'daklak gen-keymap > %t/daklak.xkb'
ExecStart=daklak
ExecStartPost=/bin/sh -c 'swaymsg input "56001:44033:daklak" xkb_file %t/daklak.xkb'
```

(`%t` expands to `$XDG_RUNTIME_DIR` under user units. The ordering between
`ExecStartPost` and daklak fully creating its uinput device isn't guaranteed — if
the swaymsg call races, retry in a loop or use a small sleep.)

## KDE Plasma, GNOME, X11

Per-device raw-keymap APIs aren't standard on these. Daklak itself still runs
and emits as before — only the compositor-side keymap mapping is applied
manually, session-wide.

Applying daklak's slots session-wide is harmless even though it touches every
device's keymap: daklak grabs the physical keyboards, so the compositor only
ever sees daklak's own uinput emits traverse the session keymap. And the overlay
is purely *additive* — the base layout (`us`, …) is untouched for ordinary keys;
daklak only redefines the high IME / F13-F19 / Korean slot keycodes that normal
typing never produces.

### Why an xkb *option*, not a *layout* (Wayland)

The intuitive approach — install `daklak_vn` as a layout and select it — is
broken on KDE/GNOME by xkb rules composition. Selecting it as a layout composes
`pc+daklak_vn+inet(evdev)`, and `inet(evdev)` is appended **last**, re-defining 8
of daklak's 17 slots (`<ZEHA>` → the `à/á/ả/ã` family, and `<FK13>`-`<FK19>` →
the `o`/`u` families). Result: `hà` → `h;2~`, `hơn` → `hn`.

Applied as an xkb **option** instead, daklak's symbols are merged *after*
`inet(evdev)`, so the daklak overrides win and all 17 slots survive. That's what
the `overlay` symbols variant + the `--rules` file are for. Both compositors
read `~/.config/xkb` first (KWin via libxkbcommon defaults; Mutter explicitly —
see [`meta-keymap-utils.c`](https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/936)),
so no system files are edited.

Drop the two user-local files once (shared by KDE and GNOME):

```
daklak gen-keymap --symbols > ~/.config/xkb/symbols/daklak_vn
daklak gen-keymap --rules   > ~/.config/xkb/rules/evdev
```

### KDE / KWin

Set the `daklak:vn` option in `kxkbrc`, using `--notify` so KWin reloads it live:

```
kwriteconfig6 --notify --file kxkbrc --group Layout --key Options daklak:vn
kwriteconfig6 --notify --file kxkbrc --group Layout --key ResetOldOptions true
```

**The `--notify` flag is mandatory for a live reload.** KWin recompiles the
seat keymap only when its keyboard `KConfigWatcher` receives the
`org.kde.kconfig.notify` D-Bus signal. A plain `kwriteconfig6` write does **not**
emit that signal, so the edit sits on disk unread until the next login. `--notify`
emits it, and KWin recompiles the keymap immediately (no layout-list toggling, no
logout). Note that `qdbus6 org.kde.KWin /KWin reconfigure` does **not** apply the
keymap — it reloads `kwinrc`/compositor state, not the xkb layout, so it is not
part of this recipe.

**`ResetOldOptions=true` is also mandatory.** Without it KWin ignores the
`Options` key entirely (it reads `Options` only when `ResetOldOptions` is true,
otherwise falling back to the `XKB_DEFAULT_OPTIONS` environment default) — so the
keymap won't pick up `daklak:vn` no matter how the reload is triggered.

`ResetOldOptions=true` makes KWin **replace** the entire xkb option set with
exactly what's in `Options` on each apply. If you rely on other options (e.g.
`compose:ralt`, a `grp:` layout-switch key), list them all, comma-separated:
`Options=daklak:vn,compose:ralt`.

Revert (again `--notify` for the live reload):

```
kwriteconfig6 --notify --file kxkbrc --group Layout --key Options ""
kwriteconfig6 --notify --file kxkbrc --group Layout --key ResetOldOptions true
```

### GNOME / Mutter

Set the option via `gsettings` — Mutter watches the schema and recompiles the
keymap **live**, so there's no reconfigure step:

```
gsettings set org.gnome.desktop.input-sources xkb-options "['daklak:vn']"
```

Requires GNOME/Mutter **≥ 3.38** — the `$XDG_CONFIG_HOME/xkb` include path landed
in mutter MR !936 (merged Jun 2020). Like KDE's `ResetOldOptions`, `xkb-options`
is always the **complete** option list, so include any others you need:
`"['daklak:vn', 'compose:ralt']"`. GNOME passes the raw option string straight to
libxkbcommon and does not filter unregistered options (the Settings GUI just
won't *list* `daklak:vn`, which doesn't matter — you set it directly here).

Revert:

```
gsettings reset org.gnome.desktop.input-sources xkb-options
```

### X11

Session-wide via `xkbcomp`:

```
daklak gen-keymap > /tmp/daklak.xkb
xkbcomp /tmp/daklak.xkb $DISPLAY
```

**Order matters**: run `xkbcomp` **after** daklak starts, so daklak's uinput
device exists and inherits the new keymap. X11 hot-plugs new slave keyboards with
the *current* core keymap — if you `xkbcomp` then launch daklak, the new device
may get the previous (pre-daklak) keymap. Symptom: typing produces nothing or
odd characters even though the master keyboard's keymap is correct.

If the order was wrong, re-run `xkbcomp` after daklak is running, or target
daklak's device explicitly:

```
ID=$(xinput list --short | awk '/daklak/{ for(i=1;i<=NF;i++) if($i ~ /^id=/){sub("id=","",$i); print $i; exit}}')
xkbcomp -i "$ID" /tmp/daklak.xkb $DISPLAY
```

Verify daklak's slave keyboard actually has the right keymap (look for
`EIGHT_LEVEL` Vietnamese keysyms on `<ZEHA>` / `<LSGT>` etc.):

```
xkbcomp -i "$ID" $DISPLAY /tmp/daklak_device.xkb
grep -A2 "key <ZEHA>" /tmp/daklak_device.xkb   # expect agrave at level 1
```

This `xkbcomp` flow changes the layout for **every** input device. Physical
typing is also affected — but daklak grabs the physicals, so the WM only sees
daklak's uinput events anyway.

Revert after daklak exit:

```
setxkbmap us
```

(or whatever your prior layout was)

#### Other IMEs intercept daklak's keystrokes

Apps that route input through a second IME (ibus, fcitx, …) will silently
swallow daklak's commits. Symptom: `xev -event keyboard` shows the correct UTF-8
keysym (e.g. `keysym 0x10001b0, uhorn` + `(c6 b0) "ư"`), but gedit / kitty /
xfce4-terminal / GTK / Qt apps render nothing.

Cause: GTK/Qt run the keystroke through their IM module first. ibus/fcitx don't
recognize daklak's Vietnamese keysyms at non-trivial mod states (Mod3+Mod5 for
Level 5+, etc.) and drop them. `xev` uses raw Xlib + `XmbLookupString` — no IM
module — which is why it shows the char fine while real apps don't.

Quick diagnosis:

```
env | grep -E "GTK_IM_MODULE|QT_IM_MODULE|XMODIFIERS"
ps -A | grep -E "ibus|fcitx"
```

Run an app with IM disabled to confirm:

```
GTK_IM_MODULE=none QT_IM_MODULE=none XMODIFIERS=@im=none gedit
```

If Vietnamese renders correctly there, the other IME was the interceptor.
Long-term fix: don't run two IMEs simultaneously. Stop ibus/fcitx for the
session that uses daklak (`pkill -x ibus-daemon`, disable fcitx autostart in
xfce session settings, etc.). Or unset the `*_IM_MODULE` env vars in your
daklak-using session's profile.

#### Why not `setxkbmap`?

`setxkbmap` does **not** load raw `.xkb` keymaps. It composes a keymap from the
xkb rules tree (`rules/`, `symbols/`, `types/`, `compat/` under
`/usr/share/X11/xkb/` or `$XDG_CONFIG_HOME/xkb/`) and asks the X server to switch
to that composition. `xkbcomp` is the only path for loading a complete keymap
blob directly.

If you really want `setxkbmap daklak_vn` to work — e.g. so the layout shows up
in the GNOME/KDE keyboard switcher list — you have to install daklak's
`xkb_symbols` section as a layout entry and update the rules. Outline:

1. Extract the `xkb_symbols "pc+us+daklak" { … };` block from `daklak gen-keymap`
   output. Drop the surrounding `xkb_keymap { … }` wrapper.
2. Save as `$XDG_CONFIG_HOME/xkb/symbols/daklak_vn` (user-local) or
   `/usr/share/X11/xkb/symbols/daklak_vn` (system-wide, needs sudo).
3. Register the layout in the rules tree so `setxkbmap` can resolve the name —
   see the xkb rules `evdev.lst` / `evdev.xml` format. The user-local equivalent
   is `$XDG_CONFIG_HOME/xkb/rules/evdev.lst` with a matching `! layout` entry.
4. Then:

   ```
   setxkbmap -I$XDG_CONFIG_HOME/xkb daklak_vn
   ```

This is significantly more work than `xkbcomp` and gains you nothing extra in the
evdev daklak flow (daklak isn't a layout the user manually selects — it's
tied to the uinput device). The `xkbcomp` one-shot above is the documented path.

## SIGKILL recovery

sway/scroll: automatic. Per-device setting died with the uinput.

X11 (if you applied `xkbcomp` manually): `setxkbmap us` to reset.

KDE/GNOME (option overlay): nothing to recover — the `daklak:vn` option persists
in `kxkbrc` / `gsettings` across a daklak crash, and is harmless while daklak is
stopped (it only redefines high IME slot keycodes that normal typing never
emits). Re-launch daklak and it works again; clear the option with the revert
command above if you want it gone.

## Setup hooks

When switching from a native desktop backend (IBus/Wayland) to evdev at
runtime, daklak can run **setup hooks** — shell commands that prepare the
environment (e.g. apply the xkb keymap, toggle `xkbcomp`, or configure an
xkb option). On switch-back to the native backend daklak runs corresponding
**cleanup hooks**.

Security note: hooks are executable programs, not passive config. A user override
such as `$XDG_CONFIG_HOME/daklak/hooks/sway-set` runs as your user with Daklak's
environment when evdev mode activates. Treat the hooks directory like
`~/.bashrc`, `~/.config/autostart`, or a user systemd unit: do not make it
writable by other users or untrusted tools.

Packaged installs built with `-Devdev_grab=true` include built-in hooks named
`sway`, `kde`, `gnome`, and `x11`. Each hook name resolves at runtime to a
`<name>-set` / `<name>-unset` script pair, searched in order:

1. `$XDG_CONFIG_HOME/daklak/hooks` (or `~/.config/daklak/hooks`) — user overrides
2. `~/.local/libexec/daklak/hooks` — per-user install
3. `/usr/libexec/daklak/hooks` — system install

First directory with a complete pair wins, so a user override shadows the
packaged copy. Runtime discovery means a directly-built (cargo) binary finds
installed hooks without a compile-time path. `/var/lib` is never used — these
are static helper programs, not mutable state.

The keymap files the hooks consume — `daklak.xkb`, `daklak_vn`, `evdev` — are
found the same way, in the first existing of `$XDG_CONFIG_HOME/daklak/xkb`,
`~/.local/share/daklak/xkb`, then `/usr/share/daklak/xkb` (Meson installs them
to `${datadir}/daklak/xkb`). Daklak exports that directory to hooks as
`DAKLAK_XKB_DIR`; the hooks read or copy the files and never call
`daklak gen-keymap` while running.

Hooks are configured by name in the config file:

```toml
evdev_grab_hooks = [
    # Built-in hooks self-skip with exit 10 when their desktop is not active.
    "sway",
    "kde",
    "gnome",
    "x11",
]
```

Hook names must contain only ASCII alphanumeric characters, underscores, and
hyphens — no whitespace, path separators, or shell syntax. The hooks are
resolved to `<name>-set` / `<name>-unset` executable pairs and run in order via
`std::process::Command` (never through a shell).

A configured hook whose `<name>-set` / `<name>-unset` scripts are not installed
(in `$XDG_CONFIG_HOME/daklak/hooks` or the packaged libexec dir) is **skipped
with a warning**, not treated as an error. This lets a single
`evdev_grab_hooks = ["sway", "kde", "gnome", "x11"]` list work across machines
where only one desktop's scripts exist. A missing script never aborts evdev
activation — important because on KDE daklak may run as KWin's managed
virtual-keyboard input method, where a non-zero exit would make KWin terminate
the daklak process.

Daklak runs setup hooks after its uinput device exists **and after physical
keyboards are grabbed**. This ordering matters for X11: the daklak uinput slave
must already exist before the hook loads the XKB keymap, otherwise the slave can
inherit the previous keymap and Vietnamese slot keysyms will not resolve.

| Exit code | Meaning |
|---|---|
| `0` | Applied successfully |
| `10` | Skipped (not applicable — no error) |
| other | Failure — already-applied hooks are rolled back and evdev activation stops |

### Rollback recovery

If `daklak` is killed (SIGKILL, power loss) while evdev hooks are active, a
**rollback marker** file is left on disk. On the next startup daklak detects
the stale marker and runs the cleanup hooks automatically.

**Daklak exits with "no keyboards grabbed".** Check `input` group membership:
`groups | grep input`. If absent: `sudo usermod -aG input $USER` and re-login.

**Daklak cannot open `/dev/uinput`.** Check that the `uinput` group exists and
that `/dev/uinput` is owned by it:

```sh
getent group uinput || sudo groupadd --system uinput
sudo usermod -aG uinput $USER
sudo cp /usr/share/daklak/99-daklak-input.rules /etc/udev/rules.d/99-daklak.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --name-match=uinput
sudo modprobe uinput
ls -l /dev/uinput   # expected: root uinput, mode crw-rw----
```

Log out and back in after changing group membership.

**Vietnamese commits appear as F13/F14… or Japanese IME glyphs.** Keymap not
applied — slots fell through to their default evdev keysym. Check `swaymsg -t
get_inputs | grep -A5 daklak` for `xkb_active_layout_name = "Daklak Vietnamese"`.
If absent, the `swaymsg input … xkb_file` step in your setup didn't run or failed
— re-run it manually.

**xev shows the Vietnamese keysym but apps render nothing (X11).** Another input
method (ibus, fcitx) running in the session is intercepting daklak's keystrokes
at the GTK/Qt IM layer. xev uses raw Xlib + `XmbLookupString` and bypasses the IM
module, which is why it sees the char while real apps don't. Disable the other
IME, or test with `GTK_IM_MODULE=none QT_IM_MODULE=none XMODIFIERS=@im=none
<app>` to confirm. See the X11 section for full details.

**Vietnamese commits drop on X11 even though core keymap looks right.** You ran
`xkbcomp /tmp/daklak.xkb $DISPLAY` before launching daklak. X11 assigns the
current core keymap to each hot-plugged slave, so daklak's freshly created uinput
device inherited the *old* layout. Fix by either running `xkbcomp` after daklak,
or by targeting daklak's device id directly with `xkbcomp -i <id>
/tmp/daklak.xkb $DISPLAY` (see the X11 section).

**keyd remap stops working when daklak runs.** If daklak starts before keyd,
daklak grabs the physical keyboards first and keyd's `EVIOCGRAB` fails with
`EBUSY` — keyd is silently neutered. Force order with systemd: in
`~/.config/systemd/user/daklak.service`, add `After=keyd.service` so keyd grabs
the physicals first and daklak grabs keyd's virtual output (recommended setup).

**keyd config uses `[ids] *`.** Wildcard means keyd also grabs daklak's `daklak`
uinput device. Every daklak pass-through emit re-enters daklak via keyd's virtual
output → feedback loop → kernel input buffer overflow → global kbd freeze.
Restrict keyd to specific vendor/product ids — daklak's are `0xdac1:0xac01`,
exclude that pair from keyd's `[ids]` block.

## Next

- [Transports](transports.md#evdev) — the evdev mode in the context of the other
  two transports.
- [Getting started](getting-started.md#evdev-grab) — enabling evdev grab and the
  config switches.
- [Backspace tiers](backspace-tiers.md) — how retroactive edits reach the app
  across all modes.
