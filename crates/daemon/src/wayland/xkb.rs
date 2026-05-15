use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use xkbcommon::xkb::{
    Context as XkbContext, Keycode, Keymap, State, CONTEXT_NO_FLAGS,
    KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1,
};

/// xkbcommon wrapper for hardware keycode → UTF-8 char conversion.
/// Initialized from the Keymap fd received via the keyboard grab
/// (ZwpInputMethodKeyboardGrabV2 Keymap event).
pub struct XkbState {
    #[allow(dead_code)]
    keymap: Keymap,
    state: State,
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
}
