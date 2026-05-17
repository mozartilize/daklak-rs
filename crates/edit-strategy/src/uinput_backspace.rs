use crate::{ModifierState, OutputSink, ShadowBuffer};

/// Linux evdev key codes for modifier keys (left-side variants).
/// See: /usr/include/linux/input-event-codes.h and plan0.md:282-310.
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTALT: u16 = 56;
const KEY_LEFTMETA: u16 = 125;
const KEY_BACKSPACE: u16 = 14;

fn modifier_codes(m: ModifierState) -> &'static [u16] {
    // Return a fixed slice per modifier; caller iterates.
    // We use left-side variants to mirror what was likely pressed, matching
    // the modifier bitmask from zwp_input_method_v2 key events.
    match m {
        ModifierState::SHIFT => &[KEY_LEFTSHIFT],
        ModifierState::CTRL => &[KEY_LEFTCTRL],
        ModifierState::ALT => &[KEY_LEFTALT],
        ModifierState::SUPER => &[KEY_LEFTMETA],
        _ => &[],
    }
}

/// Execute a delete+commit via uinput synthetic Backspace + `commit_string`
/// (Tier 3 — BackspaceMethod::UInput).
///
/// Modifier guard (plan0.md:282-310): if any modifier is held, release it
/// before the BS events and restore it after. This prevents Sway from
/// merging the held modifier state from the physical keyboard with our
/// synthetic events, which would produce e.g. Shift+Backspace (= "delete
/// word" in vim).
///
/// Sleep/pacing: 12ms inter-key delay (ydotool convention) is NOT done here —
/// the calls are synchronous from Strategy's perspective. Stage 3's Tokio task
/// inserts the sleeps between calls so the daemon loop isn't blocked.
pub fn apply(
    shadow: &mut ShadowBuffer,
    backspaces: usize,
    commit: &str,
    serial: u32,
    modifiers: ModifierState,
    sink: &mut impl OutputSink,
) {
    // Phase 1: release all held modifiers
    for bit in ModifierState::all_bits() {
        if modifiers.contains(bit) {
            for &code in modifier_codes(bit) {
                sink.uinput_key(code, 0); // release
            }
        }
    }

    // Phase 2: send N×(press+release) Backspace
    for _ in 0..backspaces {
        sink.uinput_key(KEY_BACKSPACE, 1);
        sink.uinput_key(KEY_BACKSPACE, 0);
    }

    // Phase 3: restore modifiers
    for bit in ModifierState::all_bits() {
        if modifiers.contains(bit) {
            for &code in modifier_codes(bit) {
                sink.uinput_key(code, 1); // press (restore)
            }
        }
    }

    // Force causal ordering at the compositor: kernel BS events go through
    // libinput → wl_keyboard; the upcoming commit_string goes through
    // input-method-v2 → text_input_v3. They're separate channels and apps
    // that process them in independent handlers (e.g. ghostty) can apply
    // them out of order, yielding wrong text. Brief sleep lets the
    // compositor drain the BS events before the wayland commit hits the
    // socket.
    //
    // FIXME: this is a blocking sleep on the daemon's tokio runtime thread.
    // The outer Tier 3 sleeps in daemon/src/wayland/mod.rs are wrapped in
    // `tokio::task::block_in_place` so they hand the worker back to the
    // executor. This one isn't, because edit-strategy has no tokio dep —
    // adding one for a single 3ms hint would pull tokio's full feature
    // surface into the strategy crate. Two cleaner fixes (deferred):
    // (a) hoist this sleep out into the daemon caller (split `apply` into
    // emit_backspaces + emit_commit so the daemon paces between them), or
    // (b) thread an opaque "pause hint" callback in from the daemon.
    std::thread::sleep(std::time::Duration::from_millis(3));

    // Commit text still goes through Wayland — uinput has no Unicode path.
    tracing::debug!(commit = %commit, serial, "uinput tier: emit commit_string + commit");
    sink.commit_string(commit);
    sink.commit(serial);

    // Update shadow
    for _ in 0..backspaces {
        shadow.text_mut().pop();
    }
    shadow.append(commit);
    shadow.pending_commit = true;
}
