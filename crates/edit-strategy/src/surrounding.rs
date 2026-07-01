use crate::{DeleteUnit, KeyState, OutputSink, ShadowBuffer};

/// Linux evdev keycode for Backspace.
const KEY_BACKSPACE: u32 = 14;

/// Execute a delete+commit via `delete_surrounding_text` + `commit_string`
/// (Tier 1 — BackspaceMethod::SurroundingText).
///
/// Pass both byte and char counts: per the v2/v3 spec
/// (zwp-input-method-unstable-v2.xml:262-281) `before_length` is bytes.
/// Firefox stale-echo handling can request char-count fallback for one
/// correction; otherwise sinks use the spec-compliant byte counts.
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
    delete_unit: DeleteUnit,
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

    tracing::debug!(
        before_bytes,
        before_chars,
        ?delete_unit,
        commit,
        serial,
        "surrounding tier: emit delete_surrounding_text + commit_string"
    );
    let (emit_before, emit_after) = match delete_unit {
        DeleteUnit::Bytes => (before_bytes, after_bytes),
        DeleteUnit::Chars => (before_chars, after_chars),
    };
    if delete_unit == DeleteUnit::Chars {
        tracing::info!(
            before_bytes,
            before_chars,
            after_bytes,
            after_chars,
            emit_before,
            emit_after,
            "surrounding tier: emitting char-count delete fallback"
        );
    }
    sink.delete_surrounding_text(emit_before, before_chars, emit_after, after_chars);
    sink.commit_string(commit);
    sink.commit(serial);
    shadow.append(commit);
    shadow.pending_commit = true;
}
