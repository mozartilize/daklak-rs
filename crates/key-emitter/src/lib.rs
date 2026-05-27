//! Unified key-emission backends for daklak.
//!
//! Three sites emit a single Wayland-shaped key event today:
//!
//! - `zwp_virtual_keyboard_v1` (wlroots / IM v2 path).
//! - `zwp_input_method_context_v1.key()` (KWin / IM v1 path).
//! - `/dev/uinput` (evdev-only mode, plus Tier 3 backspace).
//!
//! They all reduce to `key(time, keycode, value)` + optional
//! `modifiers(dep, lat, lock, group)`. This crate is the single trait
//! every backend implements; callers (`viet-ime-wayland-adapter`,
//! `viet-ime-evdev-adapter`) pick the impl per-session and route
//! through `&mut dyn KeyEmitter`.
//!
//! ### Hazard — Chromium-class apps
//!
//! Chromium has its own hard-coded `LinuxKeyCode → DomCode` table for
//! keyboard introspection. Evdev codes 200+ are KEY_KBDILLUMUP/KEY_FN_F*
//! in that table — feeding them Unicode keysyms crashes the renderer.
//! Use `force_uinput_apps` for chromium-class instead.

mod emit_char_impl;
mod keymap;
mod uinput;
mod vk_v1;
mod vk_v2;

pub use emit_char_impl::emit_char;
pub use keymap::{build, DaklakKeymap};
pub use uinput::UinputEmitter;
pub use vk_v1::VkV1Emitter;
pub use vk_v2::VkV2Emitter;

// Re-export shared data + helpers from the data crate so external call
// sites can `use viet_ime_key_emitter::{char_to_emit, …}` without also
// depending on `viet-ime-keymap` directly.
pub use viet_ime_keymap::{
    char_to_emit, keymap_text, plan_mod_dance, vn_pairs, EmitSpec, BASE_EVDEV, MOD_LEVEL3,
    MOD_SHIFT,
};

/// Single-key emit surface.
///
/// `time` is the wl_keyboard-shaped monotonic timestamp in milliseconds.
/// Uinput coerces to a 16-bit keycode and ignores `time`.
///
/// `value`: 1 = press, 0 = release. Uinput also recognises 2 = autorepeat.
///
/// Modifier echo path semantics vary per impl:
///
/// - `VkV2Emitter` / `VkV1Emitter`: events round-trip through the IM
///   keyboard grab. Callers that mutate the grab-visible modifier mask
///   (`emit_char`) must track a "synthetic echo pending" counter so the
///   round-tripped `Modifiers` event isn't double-applied to local
///   state.
/// - `UinputEmitter`: kernel-level. No grab round-trip. No counter.
pub trait KeyEmitter {
    fn emit_key(&mut self, time: u32, keycode: u32, value: u32);
    fn emit_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32);

    /// True iff emits from this backend round-trip through the IME's
    /// own `zwp_input_method_keyboard_grab_v2`. Drives the
    /// synthetic-mods-pending bookkeeping inside `emit_char`.
    fn modifier_echo_through_grab(&self) -> bool {
        false
    }

    /// Block until every emit issued on this backend so far has been
    /// processed by the compositor. Default no-op; `LibeiEmitter`
    /// overrides with `ei_connection.sync`.
    fn sync_barrier(&mut self) {}
}
