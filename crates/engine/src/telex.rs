//! Telex input method: key-map construction.

use vnkey_engine::input::{KeyEvType, TELEX_MAPPING, VNE_COUNT};

pub(super) fn telex_key_map(bracket_shortcuts: bool) -> [i32; 256] {
    let mut key_map = [KeyEvType::Normal as i32; 256];
    for entry in TELEX_MAPPING {
        if !bracket_shortcuts && matches!(entry.key, b'[' | b']' | b'{' | b'}') {
            continue;
        }
        key_map[entry.key as usize] = entry.action;
        if entry.action < VNE_COUNT {
            let ch = entry.key;
            if ch.is_ascii_lowercase() {
                key_map[ch.to_ascii_uppercase() as usize] = entry.action;
            } else if ch.is_ascii_uppercase() {
                key_map[ch.to_ascii_lowercase() as usize] = entry.action;
            }
        }
    }
    key_map
}
