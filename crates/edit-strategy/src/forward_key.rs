use crate::{KeyState, OutputSink, ShadowBuffer};

/// Linux evdev keycode for Backspace.
pub const KEY_BACKSPACE: u32 = 14;

/// Execute a delete+commit via synthetic Backspace keys on
/// `zwp_virtual_keyboard_v1` + `commit_string` (Tier 2 —
/// BackspaceMethod::ForwardKey).
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
    prefer_key_channel_commit: bool,
) {
    tracing::debug!(bs = backspaces, commit = %commit, serial,
        "forward_key tier: emit vk_key BS + commit_string");
    for _ in 0..backspaces {
        sink.vk_key(time, KEY_BACKSPACE, KeyState::Pressed);
        sink.vk_key(time, KEY_BACKSPACE, KeyState::Released);
    }

    // Firefox/contenteditable on wlroots can reorder cross-channel edits
    // (vk-key delete + text-input commit_string). When requested, keep the
    // replacement on the key channel too: first try vk_commit_char per-char,
    // then keysym (v1), then commit_string fallback for unmapped chars.
    if prefer_key_channel_commit {
        let mut fallback = String::new();
        for c in commit.chars() {
            if !sink.vk_commit_char(time, c) {
                fallback.push(c);
            }
        }
        if !fallback.is_empty() {
            if !sink.commit_via_keysym(serial, time, &fallback) {
                sink.commit_string(&fallback);
                sink.commit(serial);
            }
        }
    } else if !sink.commit_via_keysym(serial, time, commit) {
        // Prefer per-char keysym emission on ImV1 — real wl_keyboard.key
        // events via KWin's forwardKeySym + temporary-keymap synthesis.
        // Terminals like foot ignore commit_string but honor wl_keyboard.
        // Other backends return false → fall through to commit_string.
        sink.commit_string(commit);
        sink.commit(serial);
    }

    // Shadow tracks char-level view (used by cursor-delta detection).
    // We don't know what the app deleted — pop by char count.
    for _ in 0..backspaces {
        shadow.text_mut().pop();
    }
    shadow.append(commit);
    shadow.pending_commit = true;
}
