//! Thin re-export over `viet-ime-key-emitter`'s keymap helpers.
//!
//! Kept as a module path so existing call sites
//! (`use viet_ime_wayland_adapter::keymap::{char_to_emit, …}`) continue
//! to compile. New code should depend on `viet-ime-key-emitter` directly.

pub use viet_ime_key_emitter::{
    build, char_to_emit, emit_char, keymap_text, plan_mod_dance, vn_pairs, DaklakKeymap,
    EmitSpec, BASE_EVDEV, MOD_LEVEL3, MOD_SHIFT,
};
