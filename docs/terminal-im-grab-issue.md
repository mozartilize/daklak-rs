# Wayland terminal IME — input-method-v2 grab vs. synthetic backspace

> **Resolved 2026-05-16.** Tier 3 UInput now works for terminals via a grab release/regrab dance around each uinput emission, plus a brief sleep to force causal ordering between BS (kernel → wl_keyboard) and commit_string (input-method-v2 → text_input_v3). Confirmed: `phở bò ngon` composes cleanly in foot and ghostty. The historical analysis below is preserved as a reference for the architectural reasoning.
>
> **Routing update 2026-05-17.** Default for `purpose == 13` reverted to Tier 2 ForwardKey because foot composes correctly there (cosmetic upstream preedit underline aside) and Tier 2 avoids the Tier 3 grab-dance keystroke-leakage window. Per-app auto-escalation mechanism exists: focused `app_id` (Sway IPC) matched against `FORWARD_KEY_BROKEN_TERMINALS` in `crates/edit-strategy/src/capability.rs`. **Shipped empty** — daklak provides the mechanism but no baked-in opinion about specific terminals. Env var `DAKLAK_TERMINAL_TIER=forward|uinput|surrounding` overrides for users who need it (ghostty users without the constant populated). Rationale: no Wayland-protocol-level signal exists to runtime-detect "app dropped Tier 2" (terminals send no `surrounding_text` frames — verified empirically across the full ghostty session log), so the only auto-detect available is identity-based via the compositor IPC, and identity matching requires per-app knowledge somewhere in daklak.


Daklak-rs ([github / WIP](../)) is a native Wayland Vietnamese IME that does **no preedit** — it commits text directly and rewrites by sending Backspace+commit (Gboard / Windows Unikey pattern). It binds the `zwp_input_method_v2` protocol on the default seat, gets a `zwp_input_method_keyboard_grab_v2`, and processes every keystroke before forwarding via `zwp_virtual_keyboard_v1::key`. Composition triggers a delete+commit through the configured tier:

| Tier | Delete mechanism | Commit |
|------|------------------|--------|
| 1 SurroundingText | `zwp_input_method_v2::delete_surrounding_text(bytes, 0)` | `commit_string` + `commit(serial)` |
| 2 ForwardKey | `zwp_virtual_keyboard_v1::key(KEY_BACKSPACE)` press+release | `commit_string` + `commit(serial)` |
| 3 UInput | `/dev/uinput` evdev synthesize KEY_BACKSPACE press+release | `commit_string` + `commit(serial)` |

Capability detection routes editable widgets (gedit, Kate, Firefox address bar, Chrome) to Tier 1; non-surrounding-text apps to Tier 2; terminals (`purpose == 13`) to Tier 3. **This works correctly for all non-terminal apps.** Terminals are the problem.

## Environment

- Wayland compositor: Sway / wlroots-based (also reproduced on KDE Plasma per prior reports)
- Apps tested: foot, ghostty, chromium, gedit, thunar
- Daemon: `daklak` (Rust, GPL-3.0)

## Observed terminal behaviors

Vietnamese target: typing `phowr` (Telex) should yield `phở` — engine returns `bs=1 commit=â` on second `a`, then `bs=2 commit=ần` on `f`.

### Tier 2 (vk_key BS + commit_string) — original routing

- **foot**: typed-but-uncomposed letters (`r t p d c v` are reproducible) are rendered with a preedit underline until the next `commit_string` clears them. `commit_string` does land; the BS does land. Composition works but with visible underline artifacts on intermediate letters.
- **ghostty**: silently drops *both* the synthetic `vk.key(KEY_BACKSPACE)` AND the `commit_string` that follows it. Output: `phow bo ngon` (no Vietnamese at all). Same code path produces clean output on Chromium → daklak's protocol use is correct; ghostty filters synthetic input.

### Tier 1 (delete_surrounding_text + commit_string) — tested

- **foot**: `commit_string` lands (composed chars insert), `delete_surrounding_text` is ignored (PTY has no editable buffer). Result: `traânần vieêtết haà` — composed chars present but originals not deleted.
- **ghostty**: appears to drop the entire `done(serial)` transaction atomically when `delete_surrounding_text` can't be honored in terminal mode. `commit_string` is also dropped. Result: `phoowr boof ngonf` (no composition at all — worse than Tier 2).

### Tier 3 (uinput kernel BS + commit_string)

- **foot / ghostty**: `commit_string` lands (composed chars insert). Kernel BS round-trips through daklak's own input-method-v2 keyboard grab before reaching the terminal — confirmed by `grab.Key key=14` events arriving back at the daemon immediately after each emission. Suppression queue drops them (necessary to avoid infinite loop), but that means the terminal never sees the BS either. Result: `traânần vieêtết haà` — same partial state as foot Tier 1.

## Root cause — input-method-v2 grab is exclusive

While `zwp_input_method_keyboard_grab_v2` is held, the compositor routes **all** keyboard events for that seat to the IM exclusively. The focused surface receives no `wl_keyboard` events. `/dev/uinput` events flow into the same seat's keyboard via libinput → compositor → IM grab — daklak sees them as `grab.Key` events; terminal sees nothing.

Trace excerpt (Tier 3, foot, typing the second `a` of `traa`):

```
strategy.apply (char) method=UInput
uinput tier: emit commit_string + commit commit=â serial=6
grab.Key key=14 key_state=1     ← our own uinput BS round-tripped to daklak
self-emit suppressed key=14 value=1
grab.Key key=14 key_state=0
self-emit suppressed key=14 value=0
im_v2: Done frame serial=7
```

The `self-emit suppressed` lines confirm the BS arrived at daklak's grab, not at foot. If suppression were removed, the loop would infinitely re-emit BS — so the suppression is necessary, but it equally proves the BS doesn't reach the application.

## Reproducer logs

- [ghostty-uinput-bug.log](../ghostty-uinput-bug.log) — Tier 3 routing, foot session inside (Sway). Shows BS round-trip + suppression.
- [chrome-bug.log](../chrome-bug.log) — Tier 2 routing, chromium address bar regression where `purpose=13` was inherited from previous focus (separate sticky-purpose bug, now fixed in daklak).
- [thunar-bug.log](../thunar-bug.log) — Tier 1 path, engine seed pollution with capitalized English word (fixed via lowercase-only seed gate).

Key daklak files for context:
- `crates/edit-strategy/src/capability.rs` — `detect_method` routing rules
- `crates/edit-strategy/src/{surrounding,forward_key,uinput_backspace}.rs` — three tier implementations
- `crates/daemon/src/wayland/mod.rs` `AppState.pending_self_emits` + `suppress_self_emit` — kernel round-trip handling
- `crates/daemon/src/sink.rs` `WaylandSink::uinput_key` — push to suppression queue

## Conclusion

There appears to be **no Wayland-protocol-level path** for an IME to delete text in a terminal whose `text_input_v3` client filters synthetic Backspace:

- `delete_surrounding_text` via input-method-v2 — terminals ignore in PTY mode
- `vk.key(KEY_BACKSPACE)` via virtual_keyboard — filtered by terminal's text_input_v3
- `/dev/uinput` kernel BS — either round-trips through the IM's own grab, or escapes and gets filtered at the app layer same as virtual_keyboard

The IME's only remaining option is to intercept input **before** the terminal's text_input_v3 binding sees anything — i.e. an in-process toolkit IM module (GTK4 `GTK_IM_MODULE`, Qt input plugin), bypassing Wayland entirely. This is how kime achieves Korean composition in ghostty.

## Question

Options considered:

1. **Release grab → uinput emit → re-grab.** Implemented + tested. The grab release/regrab works partially — sometimes the kernel BS escapes the grab window and reaches the app's `wl_keyboard`, sometimes it races and gets intercepted (see mixed `grab.Key key=14 → self-emit suppressed` pattern across the log). But **the surprising finding: even when BS reliably escapes the grab and lands at the app's wl_keyboard, output is unchanged.** Foot and ghostty both filter synthetic Backspace at the application layer when `text_input_v3` is active with `purpose=terminal` — they treat any wl_keyboard BS as IM activity and discard it, expecting the IME to use `delete_surrounding_text` instead (which they also don't honor in PTY mode). So the grab is not the only line of defense; app-layer filtering is the deeper issue.

2. **Separate seat for the uinput device.** Compositors do per-seat grab handling — if daklak's uinput were on a seat the grab doesn't cover, events would not round-trip through the grab. Open questions: do mainstream wlroots/KDE Wayland compositors support multi-seat for a single user session? Will the compositor even route a non-`seat0` keyboard's events to a `seat0`-focused surface? (Initial reading says no — seats are isolated, focus is per-seat.)

3. **Wayland protocol extension** like wlroots' `wlr_keyboard_group` or a hypothetical "tag self-emit to skip own grab" extension. None standard.

4. **In-process IM module per toolkit** — GTK4 module loaded via `GTK_IM_MODULE=viet-ime` intercepts at the GTK widget layer before PTY (this is how kime achieves Korean composition in GTK4 terminals like ghostty). Mirrors `vendors/kime/src/frontends/gtk4`. Not Wayland-protocol-level but the only protocol-respecting path we see. Will eventually ship but is a big chunk of work — wanted to check there's no simpler Wayland-level fix first.

5. **Accept the partial state on Tier 3** and document. Composition lands, deletions don't — usable for typing fresh Vietnamese but bad for backspace-and-retype flows.

If anyone in the Wayland / foot / ghostty / sway communities has dealt with this before, pointers welcome.
