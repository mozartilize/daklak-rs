use crate::{OutputSink, ShadowBuffer};

/// Execute a delete+commit via `delete_surrounding_text` + `commit_string`
/// (Tier 1 — BackspaceMethod::SurroundingText).
///
/// Pass both byte and char counts: per the v2/v3 spec
/// (zwp-input-method-unstable-v2.xml:262-281) `before_length` is bytes,
/// but firefox's v3 client on the KWin v1↔v3 path interprets it as
/// chars. The sink picks the right unit based on
/// `force_chars_delete_apps`.
pub fn apply(
    shadow: &mut ShadowBuffer,
    backspaces: usize,
    commit: &str,
    serial: u32,
    sink: &mut impl OutputSink,
) {
    let (before_bytes, before_chars, after_bytes, after_chars) = shadow.pop_delete_span(backspaces);
    sink.delete_surrounding_text(before_bytes, before_chars, after_bytes, after_chars);
    sink.commit_string(commit);
    sink.commit(serial);
    shadow.append(commit);
    shadow.pending_commit = true;
}
