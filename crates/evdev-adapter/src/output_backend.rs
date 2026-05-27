//! Output backend abstraction for evdev-only mode.
//!
//! The evdev adapter receives raw keystrokes (grabbed from
//! `/dev/input/event*`), runs them through the daklak engine, then
//! needs to deliver the engine's output (raw pass-through or Vietnamese
//! commits) back to the focused Wayland client. *How* that delivery
//! happens is the only thing that varies by backend.
//!
//! Backends implement the unified `viet_ime_key_emitter::KeyEmitter`
//! trait — the same trait `viet-ime-wayland-adapter` uses for its
//! `zwp_virtual_keyboard_v1` emit path.
//!
//! **`UinputEmitter`**: creates `/dev/uinput` device, writes
//! daklak's synthetic xkb keymap to `$XDG_RUNTIME_DIR/daklak/keymap.xkb`,
//! tells the compositor (sway/scroll today) to load the keymap on the
//! uinput device via a `~/.config/sway/config.d/*.conf` snippet. Works
//! on every wlroots compositor + KDE if user installs the snippet.

pub use viet_ime_key_emitter::{KeyEmitter, UinputEmitter};

// Backwards-compatible aliases (drop in a follow-up cleanup pass).
/// Alias for the unified emit trait. New code should import
/// `viet_ime_key_emitter::KeyEmitter` directly.
pub type OutputBackend = dyn KeyEmitter + Send;

/// Alias for the unified uinput backend. New code should use
/// `viet_ime_key_emitter::UinputEmitter`.
pub type UinputBackend = UinputEmitter;
