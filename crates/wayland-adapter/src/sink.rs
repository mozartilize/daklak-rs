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
        let Some(spec) = crate::keymap::char_to_emit(c) else {
            tracing::trace!(tier = 4, char = %c, "vk_commit_char: char not in synthetic keymap");
            return false;
        };
        let dance = plan_mod_dance(self.raw_mods.0, spec.mods);
        let (_, lat, lock, group) = self.raw_mods;

        // Path A (XWayland tail-char-drop fix). If the user is currently
        // holding a key whose keycode equals the one we're about to press,
        // X's input thread still has that keycode in the DOWN state and
        // will silently no-op our synthetic press as a duplicate. Emit a
        // synthetic release first to transition X to UP, then proceed.
        // When the user eventually releases physically, our daemon forwards
        // that release through `on_key_released` → XWayland sees a release
        // for an already-up key, which is harmless (no-op in X).
        if self.held_user_kc == Some(spec.keycode) {
            tracing::debug!(
                tier = 4,
                keycode = spec.keycode,
                char = %c,
                "vk_commit_char: prelude release for still-held user key (Path A)"
            );
            self.vk.key(time, spec.keycode, 0);
        }

        if let Some((emit_mask, _)) = dance {
            // OR in the level-selecting bits on top of whatever the user
            // is physically holding. Compositor merges seat keyboards
            // so XWayland's wl_keyboard.modifiers reflects the union —
            // x clients see Shift / AltGr / Shift+AltGr as appropriate
            // and translate the keycode at the right xkb level.
            self.vk.modifiers(emit_mask, lat, lock, group);
            *self.synthetic_mods_pending = self.synthetic_mods_pending.saturating_add(1);
            *self.synthetic_mods_emitted_at = Some(Instant::now());
        }
        self.vk.key(time, spec.keycode, 1);
        self.vk.key(time, spec.keycode, 0);
        if let Some((_, restore_mask)) = dance {
            // Restore the user's physical mods so the next forwarded key
            // (or next char in a multi-char commit) gets the right state.
            self.vk.modifiers(restore_mask, lat, lock, group);
            *self.synthetic_mods_pending = self.synthetic_mods_pending.saturating_add(1);
            *self.synthetic_mods_emitted_at = Some(Instant::now());
        }
        tracing::trace!(
            tier = 4,
            char = %c,
            keycode = spec.keycode,
            dep_mods = format!("{:#x}", self.raw_mods.0),
            spec_mods = format!("{:#x}", spec.mods),
            danced = dance.is_some(),
            "vk_commit_char emitted"
        );
        true
    }
}

/// Plan the modifier dance for one `vk_commit_char` invocation.
///
/// - `dep` is the user's currently-depressed modifier mask (from the latest
///   `grab.Modifiers` event).
/// - `spec_mods` is the level-selecting mask the synthetic keymap demands
///   for the target char (e.g. Shift for L2, AltGr for L3, Shift+AltGr for L4).
///
/// Returns `Some((emit_mask, restore_mask))` if a dance is needed (we have to
/// override the modifier state for the emit, then restore the user's physical
/// mask). Returns `None` if the char sits at L1 of its key — no override
/// needed.
fn plan_mod_dance(dep: u32, spec_mods: u32) -> Option<(u32, u32)> {
    if spec_mods == 0 {
        None
    } else {
        Some((dep | spec_mods, dep))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT: u32 = 0x01;
    const CTRL: u32 = 0x04;
    const ALT: u32 = 0x08;
    const ALTGR: u32 = 0x80; // Mod5 — synthetic keymap uses for L3
    const SHIFT_ALTGR: u32 = SHIFT | ALTGR;

    #[test]
    fn no_dance_when_spec_mods_zero() {
        assert_eq!(plan_mod_dance(0, 0), None);
        assert_eq!(plan_mod_dance(SHIFT, 0), None);
        assert_eq!(plan_mod_dance(CTRL | ALT, 0), None);
    }

    #[test]
    fn dance_l2_shift_with_no_user_mods() {
        assert_eq!(plan_mod_dance(0, SHIFT), Some((SHIFT, 0)));
    }

    #[test]
    fn dance_l3_altgr_with_no_user_mods() {
        assert_eq!(plan_mod_dance(0, ALTGR), Some((ALTGR, 0)));
    }

    #[test]
    fn dance_l4_shift_altgr_with_no_user_mods() {
        assert_eq!(plan_mod_dance(0, SHIFT_ALTGR), Some((SHIFT_ALTGR, 0)));
    }

    #[test]
    fn dance_or_combines_user_shift_with_spec_altgr() {
        assert_eq!(plan_mod_dance(SHIFT, ALTGR), Some((SHIFT_ALTGR, SHIFT)));
    }

    #[test]
    fn dance_preserves_ctrl_in_emit_and_restore() {
        assert_eq!(
            plan_mod_dance(CTRL, SHIFT_ALTGR),
            Some((CTRL | SHIFT_ALTGR, CTRL))
        );
    }

    #[test]
    fn dance_ctrl_shift_held_with_spec_altgr() {
        assert_eq!(
            plan_mod_dance(CTRL | SHIFT, ALTGR),
            Some((CTRL | SHIFT | ALTGR, CTRL | SHIFT))
        );
    }

    #[test]
    fn dance_alt_held_with_spec_shift() {
        assert_eq!(plan_mod_dance(ALT, SHIFT), Some((ALT | SHIFT, ALT)));
    }

    #[test]
    fn dance_user_already_holding_spec_mods_still_dances() {
        assert_eq!(
            plan_mod_dance(SHIFT_ALTGR, SHIFT_ALTGR),
            Some((SHIFT_ALTGR, SHIFT_ALTGR))
        );
    }
}
