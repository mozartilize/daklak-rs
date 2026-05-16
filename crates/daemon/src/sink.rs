use std::collections::VecDeque;
use std::time::Instant;

use viet_ime_edit_strategy::{KeyState, OutputSink};

use crate::protocols::{
    input_method_v2::zwp_input_method_v2::ZwpInputMethodV2,
    virtual_keyboard_v1::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use viet_ime_edit_strategy::uinput_device::UinputDevice;

/// Bridges `edit_strategy::OutputSink` to live Wayland proxy calls.
///
/// Borrows proxy references from `AppState` — borrow checker allows this
/// because `im`, `vk`, `uinput`, and `pending_self_emits` are distinct fields.
pub struct WaylandSink<'a> {
    pub im: &'a ZwpInputMethodV2,
    pub vk: &'a ZwpVirtualKeyboardV1,
    pub uinput: Option<&'a mut UinputDevice>,
    /// Queue daklak's own uinput emissions go into so the grab handler can
    /// match and drop their round-trips. AppState owns it.
    pub pending_self_emits: &'a mut VecDeque<(u16, i32, Instant)>,
    pub serial: u32,
    /// Snapshot of the user's physical modifier state at the time of the
    /// daemon action. `vk_commit_char` may temporarily override these to
    /// address xkb level 2/3/4 of the daklak custom keymap, then restores.
    /// Tuple = (depressed, latched, locked, group) as in
    /// `zwp_input_method_keyboard_grab_v2::Modifiers`.
    pub raw_mods: (u32, u32, u32, u32),
}

impl OutputSink for WaylandSink<'_> {
    fn delete_surrounding_text(&mut self, before: u32, after: u32) {
        self.im.delete_surrounding_text(before, after);
    }

    fn commit_string(&mut self, text: &str) {
        self.im.commit_string(text.to_owned());
    }

    fn commit(&mut self, serial: u32) {
        self.im.commit(serial);
    }

    fn vk_key(&mut self, time: u32, key_code: u32, state: KeyState) {
        let value: u32 = match state {
            KeyState::Pressed => 1,
            KeyState::Released => 0,
        };
        self.vk.key(time, key_code, value);
    }

    fn vk_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.vk.modifiers(depressed, latched, locked, group);
    }

    fn uinput_key(&mut self, key_code: u16, value: i32) {
        if let Some(u) = &mut self.uinput {
            let _ = u.emit(key_code, value);
            self.pending_self_emits
                .push_back((key_code, value, Instant::now()));
        }
    }

    fn vk_commit_char(&mut self, time: u32, c: char) -> bool {
        let Some(spec) = crate::wayland::keymap::char_to_emit(c) else {
            return false;
        };
        let (dep, lat, lock, group) = self.raw_mods;
        let need_mod_dance = spec.mods != 0;
        if need_mod_dance {
            // OR in the level-selecting bits on top of whatever the user
            // is physically holding. Compositor merges seat keyboards
            // so XWayland's wl_keyboard.modifiers reflects the union —
            // x clients see Shift / AltGr / Shift+AltGr as appropriate
            // and translate the keycode at the right xkb level.
            self.vk.modifiers(dep | spec.mods, lat, lock, group);
        }
        self.vk.key(time, spec.keycode, 1);
        self.vk.key(time, spec.keycode, 0);
        if need_mod_dance {
            // Restore the user's physical mods so the next forwarded key
            // (or next char in a multi-char commit) gets the right state.
            self.vk.modifiers(dep, lat, lock, group);
        }
        true
    }
}
