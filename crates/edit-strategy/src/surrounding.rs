use crate::{KeyState, OutputSink, ShadowBuffer};

/// Linux evdev keycode for Backspace.
const KEY_BACKSPACE: u32 = 14;

/// Execute a delete+commit via `delete_surrounding_text` + `commit_string`
/// (Tier 1 — BackspaceMethod::SurroundingText).
///
/// Pass both byte and char counts: per the v2/v3 spec
/// (zwp-input-method-unstable-v2.xml:262-281) `before_length` is bytes,
/// but firefox's v3 client on the KWin v1↔v3 path interprets it as
/// chars. The sink picks the right unit based on
/// `force_chars_delete_apps`.
///
/// When the compositor reports a selection (anchor ≠ cursor), Chromium may
/// reject `delete_surrounding_text` because the client's selection state
/// races with the done-batch delivery (the key release arrives via the fast
/// `wl_keyboard` path and triggers autocomplete state changes before the
/// slower `text-input-v3` delete arrives). In this case we fall back to
/// ForwardKey backspaces, which Chrome processes deterministically regardless
/// of selection state: one BS clears the selection, remaining BSes delete
/// individual characters.
pub fn apply(
    shadow: &mut ShadowBuffer,
    backspaces: usize,
    commit: &str,
    serial: u32,
    time: u32,
    sink: &mut impl OutputSink,
) {
    let (before_bytes, before_chars, after_bytes, after_chars) = shadow.pop_delete_span(backspaces);

    if after_bytes > 0 || after_chars > 0 {
        // Selection present — fall back to ForwardKey (virtual keyboard BS).
        // One BS clears the entire selection; then `backspaces` more BSes
        // delete the individual chars the engine requested.
        let total_bs = backspaces + 1;
        tracing::debug!(
            total_bs,
            engine_bs = backspaces,
            after_bytes,
            commit,
            "surrounding: selection active, falling back to ForwardKey BS"
        );
        for _ in 0..total_bs {
            sink.vk_key(time, KEY_BACKSPACE, KeyState::Pressed);
            sink.vk_key(time, KEY_BACKSPACE, KeyState::Released);
        }
        sink.commit_string(commit);
        sink.commit(serial);
        shadow.append(commit);
        shadow.pending_commit = true;
        return;
    }

    sink.delete_surrounding_text(before_bytes, before_chars, after_bytes, after_chars);
    sink.commit_string(commit);
    sink.commit(serial);
    shadow.append(commit);
    shadow.pending_commit = true;
}
