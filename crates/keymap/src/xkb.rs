use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use xkbcommon::xkb::{
    Context as XkbContext, Keycode, Keymap, Keysym, State, CONTEXT_NO_FLAGS,
    KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1, MOD_INVALID,
};

/// xkbcommon wrapper for hardware keycode → UTF-8 char conversion.
/// Initialized from the Keymap fd received via the keyboard grab
/// (ZwpInputMethodKeyboardGrabV2 Keymap event).
pub struct XkbState {
    keymap: Keymap,
    state: State,
}

/// Modifier masks derived from canonical xkb modifier names in the active keymap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CanonicalModifierMasks {
    pub shift: u32,
    pub control: u32,
    pub alt: u32,
    pub logo: u32,
}

impl XkbState {
    /// Build from the keymap fd sent by the compositor on the keyboard grab.
    /// `size` is the byte length of the keymap data.
    ///
    /// # Safety
    /// `fd` must be a valid, readable file descriptor containing the keymap
    /// in XKB_KEYMAP_FORMAT_TEXT_V1 format. The compositor guarantees this.
    pub fn from_fd(fd: OwnedFd, size: u32) -> Result<Self> {
        let ctx = XkbContext::new(CONTEXT_NO_FLAGS);
        let keymap = unsafe {
            Keymap::new_from_fd(
                &ctx,
                fd,
                size as usize,
                KEYMAP_FORMAT_TEXT_V1,
                KEYMAP_COMPILE_NO_FLAGS,
            )
        }
        .context("xkb: keymap_new_from_fd failed")?
        .context("xkb: keymap was None")?;

        let state = State::new(&keymap);
        Ok(Self { keymap, state })
    }

    /// Update modifier state from a Modifiers event on the keyboard grab.
    /// Mirror to `zwp_virtual_keyboard_v1::modifiers` in caller.
    pub fn update_modifiers(
        &mut self,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) {
        self.state
            .update_mask(mods_depressed, mods_latched, mods_locked, group, 0, 0);
    }

    /// Build from an already-compiled keymap. Used by the evdev path
    /// where there's no compositor to send a keymap fd.
    pub fn from_keymap(keymap: xkbcommon::xkb::Keymap) -> Self {
        let state = xkbcommon::xkb::State::new(&keymap);
        Self { keymap, state }
    }

    /// A real `us` layout `XkbState`, for downstream-crate tests that need a
    /// genuine keymap (e.g. to exercise `base_char`). Enabled by `test-util`.
    #[cfg(feature = "test-util")]
    pub fn us_for_test() -> Self {
        let ctx = XkbContext::new(CONTEXT_NO_FLAGS);
        let keymap = Keymap::new_from_names(&ctx, "", "", "us", "", None, KEYMAP_COMPILE_NO_FLAGS)
            .expect("build us test keymap");
        Self::from_keymap(keymap)
    }

    /// Return masks for canonical modifiers in this keymap.
    ///
    /// The Wayland keyboard `modifiers` event carries xkb modifier masks, whose
    /// bit positions are defined by the keymap's modifier indices. Do not assume
    /// that Shift/Control/Mod1/Mod4 always occupy the conventional bit values;
    /// ask xkbcommon for each modifier index and convert it to a mask bit.
    ///
    /// `Alt` and `Logo` are virtual modifier names some keymaps expose in
    /// addition to the standard core modifier names `Mod1` and `Mod4`, so each
    /// logical field ORs all known aliases present in the keymap.
    pub fn canonical_modifier_masks(&self) -> CanonicalModifierMasks {
        CanonicalModifierMasks {
            shift: self.mod_mask_for_names(&["Shift"]),
            control: self.mod_mask_for_names(&["Control"]),
            alt: self.mod_mask_for_names(&["Mod1", "Alt"]),
            logo: self.mod_mask_for_names(&["Mod4", "Logo"]),
        }
    }

    fn mod_mask_for_names(&self, names: &[&str]) -> u32 {
        names.iter().fold(0, |mask, name| {
            let idx = self.keymap.mod_get_index(*name);
            if idx == MOD_INVALID || idx >= u32::BITS {
                mask
            } else {
                mask | (1_u32 << idx)
            }
        })
    }

    /// Whether the active keymap marks this key as auto-repeatable.
    pub fn key_repeats(&self, evdev_code: u32) -> bool {
        self.keymap.key_repeats(Keycode::new(evdev_code + 8))
    }

    /// Translate a hardware evdev keycode to the char it produces with the
    /// current modifier state, or `None` for non-printable keys.
    ///
    /// xkb keycodes = evdev keycodes + 8 (compositor sends evdev).
    pub fn key_to_char(&self, evdev_code: u32) -> Option<char> {
        let xkb_keycode = Keycode::new(evdev_code + 8);
        let sym = self.state.key_get_one_sym(xkb_keycode);
        let utf32 = xkbcommon::xkb::keysym_to_utf32(sym);
        if utf32 == 0 || utf32 == 0xFFFF_FFFF {
            return None;
        }
        char::from_u32(utf32)
    }

    /// The char this keycode produces at its BASE level (level 0) of the
    /// active layout, ignoring all depressed/latched/locked modifiers. `None`
    /// for keys with no printable base symbol.
    ///
    /// Used to detect when the active modifier state changed the decoded char
    /// (Shift, AltGr/Level3, CapsLock, …): on the KWin v1 ForwardKey path a
    /// raw-forwarded keycode is decoded by the client at base level, so any
    /// char that differs from its base must be committed as text instead.
    pub fn base_char(&self, evdev_code: u32) -> Option<char> {
        let xkb_keycode = Keycode::new(evdev_code + 8);
        let layout = self.state.key_get_layout(xkb_keycode);
        let syms = self.keymap.key_get_syms_by_level(xkb_keycode, layout, 0);
        let sym = syms.first().copied()?;
        let utf32 = xkbcommon::xkb::keysym_to_utf32(sym);
        if utf32 == 0 || utf32 == 0xFFFF_FFFF {
            return None;
        }
        char::from_u32(utf32)
    }

    /// Reverse lookup: find an evdev keycode that produces the given character
    /// under the current modifier state. Used by the evdev path to emit
    /// composed strings via uinput.
    pub fn char_to_keycode(&self, ch: char) -> Option<u32> {
        let target_sym = xkbcommon::xkb::utf32_to_keysym(ch as u32);
        if target_sym == Keysym::NoSymbol {
            return None;
        }
        // Scan the full keymap range (8..256+8) for a matching keysym.
        for evdev_code in 8..256 {
            let xkb_keycode = Keycode::new(evdev_code + 8);
            let sym = self.state.key_get_one_sym(xkb_keycode);
            if sym == target_sym {
                return Some(evdev_code);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us() -> XkbState {
        let ctx = XkbContext::new(CONTEXT_NO_FLAGS);
        let keymap =
            Keymap::new_from_names(&ctx, "", "", "us", "", None, KEYMAP_COMPILE_NO_FLAGS).unwrap();
        XkbState::from_keymap(keymap)
    }

    #[test]
    fn canonical_modifier_masks_follow_keymap_indices() {
        let xkb = us();
        let masks = xkb.canonical_modifier_masks();
        assert_eq!(masks.shift, 0x01);
        assert_eq!(masks.control, 0x04);
        assert_ne!(masks.alt & 0x08, 0, "Mod1 contributes to the Alt mask");
        assert_eq!(masks.logo, 0x40);
    }

    #[test]
    fn base_char_ignores_modifiers() {
        let mut xkb = us();
        // evdev 38 = 'l'. Base level is lowercase regardless of state.
        assert_eq!(xkb.base_char(38), Some('l'));
        // Hold Shift (Wayland mods_depressed bit 0x1). key_to_char now sees
        // 'L', but base_char must still report the level-0 char.
        xkb.update_modifiers(0x1, 0, 0, 0);
        assert_eq!(xkb.key_to_char(38), Some('L'), "shift raises decode level");
        assert_eq!(xkb.base_char(38), Some('l'), "base_char ignores shift");
    }

    #[test]
    fn base_char_none_for_nonprintable() {
        // evdev 59 = F1 — keysym has no UTF-32 mapping.
        assert_eq!(us().base_char(59), None);
    }
}

#[cfg(all(test, feature = "test-util"))]
mod repeat_tests {
    use super::XkbState;

    #[test]
    fn key_repeat_flag_distinguishes_letters_from_modifiers() {
        let xkb = XkbState::us_for_test();
        assert!(xkb.key_repeats(24), "o repeats");
        assert!(!xkb.key_repeats(42), "left shift does not repeat");
    }
}
