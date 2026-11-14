use crate::{KeyState, OutputSink, ShadowBuffer};

/// Linux evdev keycode for Backspace.
pub const KEY_BACKSPACE: u32 = 14;

/// Execute a delete+commit via synthetic Backspace keys, then emit the
/// replacement as one whole string through the sink's key channel, or one
/// whole `commit_string` when the sink has none (Tier 2 —
/// BackspaceMethod::ForwardKey).
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
    // The first Backspace clears a selection, not an engine character.
    let key_deletes = backspaces + usize::from(shadow.has_selection());
    shadow.pop_chars(backspaces);
    shadow.clear_selection();
    for _ in 0..key_deletes {
        sink.vk_key(time, KEY_BACKSPACE, KeyState::Pressed);
        sink.vk_key(time, KEY_BACKSPACE, KeyState::Released);
    }

    // Never split one logical replacement across channels: the key channel
    // (im-v1 keysym, im-v2 virtual keyboard) takes the whole string or none.
    if !sink.commit_key_channel_text(serial, time, commit) {
        sink.commit_string(commit);
        sink.commit(serial);
    }

    shadow.append(commit);
}
