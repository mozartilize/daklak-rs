use crate::{KeyState, OutputSink, ShadowBuffer};

/// Linux evdev keycode for Backspace.
pub const KEY_BACKSPACE: u32 = 14;

/// Execute a delete+commit entirely via `zwp_virtual_keyboard_v1::key()`
/// (Tier 4 — `BackspaceMethod::VkOnly`).
///
/// Used for clients that don't advertise `zwp_text_input_v3` and thus never
/// fire `commit_string` for daklak (Qt5, XWayland-via-vk, raw-tui in some
/// contexts). The compositor uses daklak's synthesized keymap (chars
/// `à…Đ` at evdev 200+) to translate `vk_key(custom_keycode)` into the
/// matching Unicode keysym, which then reaches the focused app as a
/// regular keypress.
///
/// If the engine somehow emits a char that's not in daklak's inventory
/// (rare — vnkey-engine's output set is a strict subset), the sink falls
/// back to `commit_string` for that char. Apps without text_input_v3
/// won't see it, but daklak doesn't silently swallow it either.
pub fn apply(
    shadow: &mut ShadowBuffer,
    backspaces: usize,
    commit: &str,
    time: u32,
    sink: &mut impl OutputSink,
) {
    tracing::debug!(bs = backspaces, commit = %commit,
        "vk_only tier: vk_key BS + vk_key Vietnamese-keymap chars");

    for _ in 0..backspaces {
        sink.vk_key(time, KEY_BACKSPACE, KeyState::Pressed);
        sink.vk_key(time, KEY_BACKSPACE, KeyState::Released);
    }

    for c in commit.chars() {
        if !sink.vk_commit_char(time, c) {
            // Fallback for chars not in the daklak keymap. Apps without
            // text_input_v3 ignore this; logging is enough to flag the
            // inventory gap.
            tracing::warn!(c = ?c,
                "vk_only: char not in daklak keymap → commit_string fallback");
            let mut buf = [0u8; 4];
            sink.commit_string(c.encode_utf8(&mut buf));
        }
    }
    // Deliberately NO `sink.commit(serial)` — that's a text_input_v3 op,
    // and clients on this tier don't expose text_input_v3.

    for _ in 0..backspaces {
        shadow.text_mut().pop();
    }
    shadow.append(commit);
    // pending_commit stays false: there's no surrounding_text echo to
    // wait for (no text_input_v3 session).
}
