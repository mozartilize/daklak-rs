//! X11 keysym / IBus key translation.
//!
//! IBus delivers X11 keysyms (`keyval`) and X11 keycodes (`keycode = evdev + 8`).
//! We need:
//!   - keyval → Option<char>  (for engine composition)
//!   - keycode → evdev code   (for KEY_BACKSPACE etc. matching)
//!   - evdev → keyval         (for ForwardKeyEvent from OutputSink::vk_key)
//!   - IBus state mask → ModifierState

use viet_ime_edit_strategy::ModifierState;

// X11 keysym constants
pub const XK_BACKSPACE: u32 = 0xff08;
pub const XK_TAB: u32 = 0xff09;
pub const XK_RETURN: u32 = 0xff0d;
pub const XK_ESCAPE: u32 = 0xff1b;
pub const XK_DELETE: u32 = 0xffff;

// IBus modifier mask bits (X11 convention)
pub const IBUS_SHIFT_MASK: u32 = 1 << 0;
pub const IBUS_LOCK_MASK: u32 = 1 << 1;
pub const IBUS_CONTROL_MASK: u32 = 1 << 2;
pub const IBUS_MOD1_MASK: u32 = 1 << 3; // Alt
pub const IBUS_MOD4_MASK: u32 = 1 << 6; // Super / Win
pub const IBUS_RELEASE_MASK: u32 = 1 << 30;

/// Convert an X11 keysym to a Unicode char for engine consumption.
///
/// Latin-1 keysyms (0x0020–0x00ff) map directly to Unicode.
/// Latin extended (0x0100–0x02ff) map directly.
/// Unicode keysyms (0x01000000–0x0110ffff) encode code point in low 21 bits.
/// Everything else → None (special key, not a printable char).
pub fn keyval_to_char(keyval: u32) -> Option<char> {
    // Unicode-range keysyms: 0x01xxxxxx
    if keyval & 0xff000000 == 0x01000000 {
        return char::from_u32(keyval & 0x00ffffff);
    }
    // Direct Latin / ISO 8859: printable range below 0x1000
    if (0x0020..=0x00ff).contains(&keyval) || (0x0100..=0x02ff).contains(&keyval) {
        return char::from_u32(keyval);
    }
    None
}

/// X11 keycode → Linux evdev keycode (subtract 8).
pub fn x11_to_evdev(x11_kc: u32) -> u32 {
    x11_kc.saturating_sub(8)
}

/// Map an IBus key event to the Linux evdev keycode daklak's core expects.
///
/// IBus keycodes are NOT reliably X11 keycodes: under X11 they are X11 codes
/// (evdev + 8), but under Wayland/Mutter (our GNOME target) they are raw evdev
/// codes. Blindly subtracting 8 turned evdev `u` (22) into 14 = KEY_BACKSPACE,
/// so every word containing `u` had its `u` swallowed as a backspace — which
/// then corrupted the raw_word seed and mis-toned the syllable (hieeu+s → hiêí
/// instead of hiếu).
///
/// The keysym (`keyval`) is unambiguous, so identify the special keys daklak's
/// core branches on — backspace and navigation — from it directly, and pass the
/// hardware keycode through unchanged for everything else. Printable keys only
/// need a code that is neither KEY_BACKSPACE nor a NAV key; their composition is
/// driven entirely by the translated `char`, not the keycode.
pub fn keyval_to_evdev(keyval: u32, keycode: u32) -> u32 {
    match keyval {
        XK_BACKSPACE => 14, // KEY_BACKSPACE
        0xff51 => 105,      // Left  → KEY_LEFT
        0xff53 => 106,      // Right → KEY_RIGHT
        0xff52 => 103,      // Up    → KEY_UP
        0xff54 => 108,      // Down  → KEY_DOWN
        0xff50 => 102,      // Home  → KEY_HOME
        0xff57 => 107,      // End   → KEY_END
        0xff55 => 104,      // PageUp   → KEY_PAGEUP
        0xff56 => 109,      // PageDown → KEY_PAGEDOWN
        _ => keycode,
    }
}

/// Linux evdev keycode → X11 keyval (keysym) for common keys used in
/// `ForwardKeyEvent` (backspace, letters a–z).
pub fn evdev_to_keyval(evdev_kc: u32) -> u32 {
    match evdev_kc {
        14 => XK_BACKSPACE,
        // Letters a(30)–z(44, 45, ...) map to keysym 'a'(0x61)–'z'(0x7a).
        // This is only needed for ForwardKey backspace path (evdev 14).
        // Other evdev codes from vk_key are letters passed through; for those
        // the caller (sink) uses the original char if available.
        _ => {
            // ASCII letters: evdev 30=a, 48=b(no)... not a simple formula.
            // For IBus the only vk_key backspace (evdev 14) is what matters.
            0 // unknown → caller should use commit_string instead
        }
    }
}

/// Map IBus/X11 modifier state to daklak `ModifierState`.
pub fn ibus_state_to_modifiers(state: u32) -> ModifierState {
    let mut m = ModifierState::empty();
    if state & IBUS_SHIFT_MASK != 0 {
        m |= ModifierState::SHIFT;
    }
    if state & IBUS_CONTROL_MASK != 0 {
        m |= ModifierState::CTRL;
    }
    if state & IBUS_MOD1_MASK != 0 {
        m |= ModifierState::ALT;
    }
    if state & IBUS_MOD4_MASK != 0 {
        m |= ModifierState::SUPER;
    }
    m
}
