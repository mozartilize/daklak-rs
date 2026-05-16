use crate::{KeyState, OutputSink, ShadowBuffer};

/// Linux evdev keycode for Backspace.
pub const KEY_BACKSPACE: u32 = 14;

/// Execute a delete+commit via synthetic Backspace keys on
/// `zwp_virtual_keyboard_v1` + `commit_string` (Tier 2 —
/// BackspaceMethod::ForwardKey).
///
/// Wayland message queues are FIFO within a single connection (plan0.md:240-251),
/// so the app sees N×BS events before the commit_string — no sleep needed.
///
/// Precondition: the daemon must have called `vk.keymap(...)` once before any
/// `vk_key()` calls (kime pattern: state.rs:620-626). Stage 3 owns that.
pub fn apply(
    shadow: &mut ShadowBuffer,
    backspaces: usize,
    commit: &str,
    serial: u32,
    time: u32,
    sink: &mut impl OutputSink,
) {
    tracing::debug!(bs = backspaces, commit = %commit, serial,
        "forward_key tier: emit vk_key BS + commit_string");
    for _ in 0..backspaces {
        sink.vk_key(time, KEY_BACKSPACE, KeyState::Pressed);
        sink.vk_key(time, KEY_BACKSPACE, KeyState::Released);
    }
    sink.commit_string(commit);
    sink.commit(serial);

    // Shadow tracks char-level view (used by cursor-delta detection).
    // We don't know what the app deleted — pop by char count.
    for _ in 0..backspaces {
        shadow.text_mut().pop();
    }
    shadow.append(commit);
    shadow.pending_commit = true;
}
