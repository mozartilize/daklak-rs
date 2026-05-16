use crate::{KeyState, OutputSink, ShadowBuffer};

/// Linux evdev keycode for Backspace.
pub const KEY_BACKSPACE: u32 = 14;

/// Execute a delete+commit via synthetic Backspace keys on
/// `zwp_virtual_keyboard_v1` + `commit_string` (Tier 2 —
/// BackspaceMethod::ForwardKey).
///
/// Wire ordering: Wayland message queues are FIFO within a single connection
/// (plan0.md:240-251) so the BS events leave the socket before commit_string.
/// On the app side, though, `wl_keyboard.key` and `zwp_text_input_v3` events
/// dispatch to separate handlers and may race — chromium specifically drops
/// the first synthetic BS in a new IM session if the commit_string arrives
/// before BS has been delivered to its wl_keyboard handler (text_input_v3's
/// edit session isn't active yet, so BS gets filtered as out-of-session).
/// Mirror the Tier 3 uinput causal-ordering fix: sleep briefly after the BS
/// burst before sending commit_string so the compositor processes them as
/// distinct passes, not a single batch.
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
    if backspaces > 0 {
        std::thread::sleep(std::time::Duration::from_millis(3));
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
