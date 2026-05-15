use crate::{OutputSink, ShadowBuffer};

/// Execute a delete+commit via `delete_surrounding_text` + `commit_string`
/// (Tier 1 — BackspaceMethod::SurroundingText).
///
/// Byte accounting: `delete_surrounding_text` takes bytes, not char counts
/// (zwp-input-method-unstable-v2.xml:262-281). `shadow.pop_chars(n)` returns
/// the UTF-8 byte count of the chars removed — that is what we pass.
pub fn apply(
    shadow: &mut ShadowBuffer,
    backspaces: usize,
    commit: &str,
    serial: u32,
    sink: &mut impl OutputSink,
) {
    let before_bytes = shadow.pop_chars(backspaces);
    sink.delete_surrounding_text(before_bytes, 0);
    sink.commit_string(commit);
    sink.commit(serial);
    shadow.append(commit);
    shadow.pending_commit = true;
}
