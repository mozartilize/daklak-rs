use std::collections::VecDeque;
use std::time::Instant;

use viet_ime_edit_strategy::{KeyState, OutputSink};

use wayland_protocols_misc::{
    zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2,
    zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use viet_ime_edit_strategy::uinput_device::UinputDevice;

/// Bridges `edit_strategy::OutputSink` to live Wayland proxy calls.
///
/// Constructed via `AdapterCtx::with_sink` — borrows proxy references from
/// `AdapterState`. Borrow checker allows this because `im`, `vk`, `uinput`,
/// and `pending_self_emits` are distinct fields.
pub struct AdapterSink<'a> {
    pub(crate) im: &'a ZwpInputMethodV2,
    pub(crate) vk: &'a ZwpVirtualKeyboardV1,
    pub(crate) uinput: Option<&'a mut UinputDevice>,
    /// Queue daklak's own uinput emissions go into so the grab handler can
    /// match and drop their round-trips. AdapterState owns it.
    pub(crate) pending_self_emits: &'a mut VecDeque<(u16, i32, Instant)>,
    /// Counter of `vk.modifiers` calls daklak has emitted but not yet seen
    /// echoed back through the IM grab's `Modifiers` event. Incremented per
    /// emit in `vk_commit_char`; AdapterState's `on_modifiers` handler decrements
    /// and skips its own `self.modifiers` update so daklak's modifier state
    /// tracking isn't transiently corrupted by its own dance.
    pub(crate) synthetic_mods_pending: &'a mut u32,
    /// Stamp of the most recent `synthetic_mods_pending` increment — read by
    /// `on_modifiers` as a TTL safety net (force-reset the counter if no
    /// echo arrives within 50ms, in case the compositor coalesced events).
    pub(crate) synthetic_mods_emitted_at: &'a mut Option<Instant>,
    pub(crate) serial: u32,
    /// Snapshot of the user's physical modifier state at the time of the
    /// daemon action. `vk_commit_char` may temporarily override these to
    /// address xkb level 2/3/4 of the daklak custom keymap, then restores.
    /// Tuple = (depressed, latched, locked, group) as in
    /// `zwp_input_method_keyboard_grab_v2::Modifiers`.
    pub(crate) raw_mods: (u32, u32, u32, u32),
    /// Tail-char-drop fix (Path A). When the user is currently holding a
    /// key whose keycode equals the one daklak is about to re-emit via
    /// `vk_commit_char`, the XWayland X-server input thread silently
    /// drops the synthetic press because that keycode is already
    /// marked DOWN in its state table. Solution: emit a prelude
    /// release for that keycode before the normal press/release pair,
    /// transitioning X's state to UP first.
    pub(crate) held_user_kc: Option<u32>,
}

impl OutputSink for AdapterSink<'_> {
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
            // Only record for self-emit suppression if the kernel actually
            // accepted the event. /dev/uinput failures (perm revoke, device
            // disappearance, compositor input reset) used to enqueue
            // phantoms here, swallowing the next real Backspace.
            match u.emit(key_code, value) {
                Ok(()) => {
                    self.pending_self_emits
                        .push_back((key_code, value, Instant::now()));
                }
                Err(e) => {
                    tracing::warn!(?e, key_code, value, "uinput emit failed");
                }
            }
        }
    }

    fn vk_commit_char(&mut self, time: u32, c: char) -> bool {
        crate::keymap::emit_char(
            self.vk,
            self.synthetic_mods_pending,
            self.synthetic_mods_emitted_at,
            self.raw_mods,
            self.held_user_kc,
            time,
            c,
        )
    }
}

