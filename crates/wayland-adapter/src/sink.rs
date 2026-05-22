use std::collections::VecDeque;
use std::time::Instant;

use viet_ime_edit_strategy::{KeyState, OutputSink};

use wayland_protocols::wp::input_method::zv1::client::zwp_input_method_context_v1::ZwpInputMethodContextV1;
use wayland_protocols_misc::{
    zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2,
    zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use viet_ime_edit_strategy::uinput_device::UinputDevice;

/// Which set of Wayland proxies to use for text-commit + key-emission.
/// V2 = wlroots path (separate im + vk), V1 = KWin/Mutter path (single context).
pub(crate) enum SinkTarget<'a> {
    V2 {
        im: &'a ZwpInputMethodV2,
        vk: &'a ZwpVirtualKeyboardV1,
    },
    V1 {
        ctx: &'a ZwpInputMethodContextV1,
    },
}

/// Bridges `edit_strategy::OutputSink` to live Wayland proxy calls.
///
/// Constructed via `AdapterCtx::with_sink` — borrows proxy references from
/// `AdapterState`. The `target` field selects between v2 (wlroots) and v1
/// (KWin) backend.
pub struct AdapterSink<'a> {
    pub(crate) target: SinkTarget<'a>,
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
        match &self.target {
            SinkTarget::V2 { im, .. } => im.delete_surrounding_text(before, after),
            // v1 delete_surrounding_text takes (index: i32, length: u32).
            // index is relative to cursor (negative = before cursor),
            // length is total bytes to delete.
            SinkTarget::V1 { ctx } => {
                let index = -(before as i32);
                let length = before + after;
                ctx.delete_surrounding_text(index, length);
            }
        }
    }

    fn commit_string(&mut self, text: &str) {
        match &self.target {
            SinkTarget::V2 { im, .. } => im.commit_string(text.to_owned()),
            SinkTarget::V1 { ctx } => ctx.commit_string(self.serial, text.to_owned()),
        }
    }

    fn commit(&mut self, serial: u32) {
        match &self.target {
            SinkTarget::V2 { im, .. } => im.commit(serial),
            // v1 has no batching — commit is a no-op.
            SinkTarget::V1 { .. } => {}
        }
    }

    fn vk_key(&mut self, time: u32, key_code: u32, state: KeyState) {
        let value: u32 = match state {
            KeyState::Pressed => 1,
            KeyState::Released => 0,
        };
        match &self.target {
            SinkTarget::V2 { vk, .. } => vk.key(time, key_code, value),
            SinkTarget::V1 { ctx } => ctx.key(self.serial, time, key_code, value),
        }
    }

    fn vk_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        match &self.target {
            SinkTarget::V2 { vk, .. } => vk.modifiers(depressed, latched, locked, group),
            SinkTarget::V1 { ctx } => {
                ctx.modifiers(self.serial, depressed, latched, locked, group)
            }
        }
    }

    fn uinput_key(&mut self, key_code: u16, value: i32) {
        if let Some(u) = &mut self.uinput {
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
        match &self.target {
            SinkTarget::V2 { vk, .. } => crate::keymap::emit_char(
                vk,
                self.synthetic_mods_pending,
                self.synthetic_mods_emitted_at,
                self.raw_mods,
                self.held_user_kc,
                time,
                c,
            ),
            // VkOnly (Tier 4) is gated off on v1 (KWin). This path is
            // unreachable during normal operation — return false so the
            // caller falls back to commit_string.
            SinkTarget::V1 { .. } => {
                tracing::trace!("vk_commit_char called on v1 (unexpected — VkOnly disabled on KWin)");
                false
            }
        }
    }
}

