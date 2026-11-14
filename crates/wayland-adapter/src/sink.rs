use viet_ime_edit_strategy::{KeyState, OutputSink};
use viet_ime_key_emitter::{HeldKeys, KeyEmitter};
use viet_ime_keymap::xkb::XkbState;

use wayland_protocols::wp::input_method::zv1::client::zwp_input_method_context_v1::ZwpInputMethodContextV1;
use wayland_protocols_misc::{
    zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2,
    zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use crate::state::ModifierEcho;

/// Backend selection for the text-input-v3-shaped operations (the ones
/// `KeyEmitter` doesn't cover): `commit_string`, `commit(serial)`,
/// `delete_surrounding_text`. Key emission is delegated to `dyn KeyEmitter`.
pub(crate) enum TextOpsTarget<'a> {
    V2 {
        im: &'a ZwpInputMethodV2,
        acked: &'a mut bool,
    },
    V1 {
        ctx: &'a ZwpInputMethodContextV1,
        serial: u32,
    },
}

/// Key routing is independent of the text protocol. A VK may accompany IMv1.
#[derive(Clone, Copy)]
pub(crate) enum KeyOutput<'a> {
    VirtualKeyboard { vk: &'a ZwpVirtualKeyboardV1, synthetic_keymap: bool },
    V1Context { ctx: &'a ZwpInputMethodContextV1, serial: u32 },
}

impl KeyOutput<'_> {
    fn has_synthetic_keymap(&self) -> bool {
        matches!(self, Self::VirtualKeyboard { synthetic_keymap: true, .. })
    }
}

impl KeyEmitter for KeyOutput<'_> {
    fn emit_key(&mut self, time: u32, key: u32, value: u32) {
        match self {
            Self::VirtualKeyboard { vk, .. } => vk.key(time, key, value),
            Self::V1Context { ctx, serial } => ctx.key(*serial, time, key, value),
        }
    }

    fn emit_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        match self {
            Self::VirtualKeyboard { vk, .. } => vk.modifiers(depressed, latched, locked, group),
            Self::V1Context { ctx, serial } => ctx.modifiers(*serial, depressed, latched, locked, group),
        }
    }

    fn modifier_echo_through_grab(&self) -> bool {
        matches!(self, Self::V1Context { .. })
    }
}

// V2 serial checks can discard an edit when a client update overtakes it.
// Preflight before deleting: never mix key deletion with text insertion here.
fn prefer_v2_key_replacement(is_v2: bool, has_synthetic_keyboard: bool, text: &str) -> bool {
    is_v2 && has_synthetic_keyboard
        && text.chars().all(|ch| viet_ime_keymap::char_to_emit(ch).is_some())
}

fn v1_keysym_needs_modifier_guard(
    is_unmapped: bool,
    commit_string_functional: bool,
    raw_mods: (u32, u32, u32, u32),
) -> bool {
    let (depressed, latched, locked, _) = raw_mods;
    !is_unmapped && !commit_string_functional && (depressed | latched | locked) != 0
}

fn forward_backspace_needs_modifier_guard(key_code: u32, raw_mods: (u32, u32, u32, u32)) -> bool {
    let (depressed, latched, locked, _) = raw_mods;
    key_code == 14 && (depressed | latched | locked) != 0
}

fn flush_connection(conn: &wayland_client::Connection, operation: &'static str) -> bool {
    match conn.flush() {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, operation, "Wayland emission flush failed");
            false
        }
    }
}

/// Bridges `edit_strategy::OutputSink` to live Wayland proxy calls.
///
/// Constructed via `AdapterCtx::with_sink` — borrows proxy references from
/// `AdapterState`.
///
/// Text operations and key operations borrow independently selected channels.
/// All key output, including passthrough and replacement text, uses `keys`.
pub struct AdapterSink<'a> {
    pub(crate) text_ops: TextOpsTarget<'a>,
    pub(crate) keys: KeyOutput<'a>,
    pub(crate) echo: &'a mut ModifierEcho,
    /// Snapshot of the user's physical modifier state at the time of the
    /// daemon action. `vk_commit_char` may temporarily override these to
    /// address xkb level 2/3/4 of the daklak custom keymap, then restores.
    /// Tuple = (depressed, latched, locked, group) as in
    /// `zwp_input_method_keyboard_grab_v2::Modifiers`.
    pub(crate) raw_mods: (u32, u32, u32, u32),
    /// Tail-char-drop fix. User keys the focused client sees as held. A
    /// synthetic press on one of their keycodes (`vk_commit_char`, or the v1
    /// keysym KWin maps to that keycode) would be dropped as a duplicate, so
    /// the sink emits a prelude release first and removes the key here.
    pub(crate) held: &'a mut HeldKeys,
    /// Wayland connection — used by the key-channel commit to flush after
    /// the batch of emissions so they reach the compositor in one round.
    pub(crate) conn: &'a wayland_client::Connection,
    /// Reverse lookup for held-key releases and the IMv1 modifier guard.
    pub(crate) xkb: Option<&'a XkbState>,
    /// Whether the focused client applies text-input-v3 `commit_string`.
    /// `true` by default; flipped `false` at the ST→FK downgrade when the
    /// client's surrounding-text proved dead (Google Docs / Firefox
    /// contenteditable) — the same dead text-input-v3 contract drops
    /// `commit_string` too. Also controls the IMv1 mapped-keysym modifier guard.
    /// A key-channel replacement is always preflighted as a whole.
    pub(crate) commit_string_functional: bool,
}

impl OutputSink for AdapterSink<'_> {
    fn prefer_key_channel_replacement(&self, text: &str) -> bool {
        prefer_v2_key_replacement(
            matches!(&self.text_ops, TextOpsTarget::V2 { .. }),
            self.keys.has_synthetic_keymap(),
            text,
        )
    }

    fn delete_surrounding_text(
        &mut self,
        before_bytes: u32,
        _before_chars: u32,
        after_bytes: u32,
        _after_chars: u32,
    ) {
        match &self.text_ops {
            // v2/wlroots and v1/KWin both use UTF-8 byte counts per the
            // text-input-v3 spec.
            TextOpsTarget::V2 { im, .. } => {
                im.delete_surrounding_text(before_bytes, after_bytes);
            }
            TextOpsTarget::V1 { ctx, .. } => {
                let index = -(before_bytes as i32);
                let length = before_bytes + after_bytes;
                tracing::trace!(
                    index,
                    length,
                    "delete_surrounding_text emit (text-input channel)"
                );
                ctx.delete_surrounding_text(index, length);
            }
        }
    }

    fn commit_string(&mut self, text: &str) {
        tracing::trace!(text = %text, "commit_string emit (text-input channel)");
        match &self.text_ops {
            TextOpsTarget::V2 { im, .. } => im.commit_string(text.to_owned()),
            TextOpsTarget::V1 { ctx, serial } => ctx.commit_string(*serial, text.to_owned()),
        }
    }

    fn commit(&mut self, serial: u32) {
        match &mut self.text_ops {
            TextOpsTarget::V2 { im, acked } => {
                im.commit(serial);
                **acked = true;
            }
            // v1 has no batching — commit is a no-op.
            TextOpsTarget::V1 { .. } => {}
        }
    }

    fn vk_key(&mut self, time: u32, key_code: u32, state: KeyState) {
        let value: u32 = match state {
            KeyState::Pressed => 1,
            KeyState::Released => 0,
        };
        // Backspace and replacement text must use the same keyboard channel.
        let guard_modifiers = forward_backspace_needs_modifier_guard(key_code, self.raw_mods);
        if guard_modifiers && state == KeyState::Pressed {
            tracing::trace!(
                raw_mods = ?self.raw_mods,
                "vk_key Backspace modifier guard: clear modifiers"
            );
            self.keys.emit_modifiers(0, 0, 0, self.raw_mods.3);
        }
        if state == KeyState::Pressed {
            // A synthetic Backspace while the user's own Backspace is still
            // down (fast delete-then-retype) would otherwise be dropped.
            self.held
                .release_before_press(&mut self.keys, time, key_code);
        }
        tracing::trace!(key_code, value, "vk_key emit (forward channel)");
        self.keys.emit_key(time, key_code, value);
        if guard_modifiers && state == KeyState::Released {
            tracing::trace!(
                raw_mods = ?self.raw_mods,
                "vk_key Backspace modifier guard: restore modifiers"
            );
            self.keys.emit_modifiers(
                self.raw_mods.0,
                self.raw_mods.1,
                self.raw_mods.2,
                self.raw_mods.3,
            );
        }
    }

    fn commit_key_channel_text(&mut self, _serial: u32, time: u32, text: &str) -> bool {
        // Never emit a supported prefix and fall back with the remainder: that
        // reorders characters across keyboard and text-input delivery queues.
        match self.keys {
            // Preflight the entire text before emitting any key.
            KeyOutput::VirtualKeyboard { .. } => {
                if !self.keys.has_synthetic_keymap() || !text.chars().all(|c| {
                    viet_ime_keymap::char_to_emit(c).is_some()
                }) {
                    return false;
                }
                for c in text.chars() {
                    if !self.vk_commit_char(time, c) {
                        return false;
                    }
                    if !flush_connection(self.conn, "flush key-channel character") {
                        // Output was attempted; false would replay the edit.
                        return true;
                    }
                }
                true
            }
            // ImV1: delegate to whole-keysym path.
            KeyOutput::V1Context { ctx, serial } => {
                self.commit_via_keysym_v1(ctx, serial, time, text)
            }
        }
    }
}

impl AdapterSink<'_> {
    /// Emit `c` through the virtual keyboard using daklak's synthesized
    /// Vietnamese keymap. Returns `false` if there is no synthetic keymap or
    /// `c` is not in its inventory.
    pub(crate) fn vk_commit_char(&mut self, time: u32, c: char) -> bool {
        if !self.keys.has_synthetic_keymap() {
            return false;
        }
        viet_ime_key_emitter::emit_char(
            &mut self.keys,
            &mut self.echo.emission(),
            viet_ime_key_emitter::EmitCharParams {
                raw_mods: self.raw_mods,
                held: self.held,
                time,
                c,
            },
        )
    }

    /// ImV1 keysym-path commit. Emits the whole replacement through the
    /// keysym channel to avoid non-atomic split-channel edits (keysym for
    /// Vietnamese chars + commit_string for ASCII tails), which terminals
    /// like Ghostty and xfce4-terminal render incorrectly.
    fn commit_via_keysym_v1(
        &mut self,
        ctx: &ZwpInputMethodContextV1,
        serial: u32,
        time: u32,
        text: &str,
    ) -> bool {
        const PRESSED: u32 = 1;
        const RELEASED: u32 = 0;

        if !text.chars().all(|c| viet_ime_keymap::xkb::commit_keysym(c).is_some()) {
            return false;
        }

        tracing::trace!(text = %text, "commit_via_keysym_v1: enter (v1 keysym path)");

        for c in text.chars() {
            let is_unmapped = match self.xkb {
                Some(xkb) => xkb.char_to_keycode(c).is_none(),
                None => true,
            };
            // Whole replacement stays on the keysym channel. No per-char
            // commit_string split — mixed key/text-input replacement is not
            // a coherent edit for KWin terminals.

            // Latin-2+ legacy keysyms (e.g. ă=0x1E3, đ=0x1F0) are sent in
            // Unicode form so KWin resolves them to daklak's own slot (see
            // `commit_keysym`). Latin-1 chars keep their legacy keysyms.
            let Some(sym) = viet_ime_keymap::xkb::commit_keysym(c) else {
                tracing::debug!(c = %c, "commit_via_keysym_v1: no keysym for char, falling back");
                return false;
            };

            // Tail-char-drop fix: KWin ignores a press for a keycode it
            // already considers pressed. If the keycode KWin will pick for
            // this keysym (any level: `ô` lives on level 5 of KEY_O) is a
            // physical key the user is still holding, release it through
            // the forward channel first so the synthetic press is not
            // dropped.
            if let Some(kc) = self.xkb.and_then(|xkb| xkb.keysym_keycode(sym)) {
                self.held
                    .release_before_press(&mut self.keys, time, kc);
            }

            // Keep the mapped-key modifier guard and per-character flush on
            // the context channel; do not apply this workaround to VK.
            let guard_modifiers = v1_keysym_needs_modifier_guard(
                is_unmapped,
                self.commit_string_functional,
                self.raw_mods,
            );
            if guard_modifiers {
                self.keys.emit_modifiers(0, 0, 0, self.raw_mods.3);
            }
            ctx.keysym(serial, time, sym.raw(), PRESSED, 0);
            ctx.keysym(serial, time, sym.raw(), RELEASED, 0);
            if guard_modifiers {
                self.keys.emit_modifiers(
                    self.raw_mods.0,
                    self.raw_mods.1,
                    self.raw_mods.2,
                    self.raw_mods.3,
                );
            }
            if !flush_connection(self.conn, "flush input-method keysym") {
                return true;
            }
            tracing::trace!(
                c = %c,
                sym = sym.raw(),
                "commit_via_keysym_v1: keysym emit + barrier (unmapped)"
            );
        }

        // A failed final flush is observed above, but this channel was still
        // attempted and must not trigger a second-channel replay.
        let _flushed = flush_connection(self.conn, "flush input-method replacement");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_key_replacement_requires_the_whole_inventory_and_backend() {
        assert!(prefer_v2_key_replacement(true, true, "ình"));
        assert!(prefer_v2_key_replacement(true, true, "ĐạA"));
        assert!(prefer_v2_key_replacement(true, true, "")); // deletion only
        assert!(!prefer_v2_key_replacement(false, true, "ình"));
        assert!(!prefer_v2_key_replacement(true, false, "ình"));
        // Supported prefixes must not permit a partial key-channel edit.
        assert!(!prefer_v2_key_replacement(true, true, "ình🙂"));
        assert!(!prefer_v2_key_replacement(true, true, "a\u{0301}"));
    }

    #[test]
    fn v1_mapped_keysym_under_depressed_modifier_needs_modifier_guard() {
        assert!(v1_keysym_needs_modifier_guard(false, false, (1, 0, 0, 0)));
        assert!(!v1_keysym_needs_modifier_guard(false, true, (1, 0, 0, 0)));
        assert!(!v1_keysym_needs_modifier_guard(true, false, (1, 0, 0, 0)));
        assert!(!v1_keysym_needs_modifier_guard(false, false, (0, 0, 0, 0)));
        assert!(!v1_keysym_needs_modifier_guard(false, false, (0, 0, 0, 1)));
    }

    #[test]
    fn forward_backspace_under_depressed_modifier_needs_modifier_guard() {
        assert!(forward_backspace_needs_modifier_guard(14, (1, 0, 0, 0)));
        assert!(!forward_backspace_needs_modifier_guard(14, (0, 0, 0, 0)));
        assert!(!forward_backspace_needs_modifier_guard(14, (0, 0, 0, 1)));
        assert!(!forward_backspace_needs_modifier_guard(30, (1, 0, 0, 0)));
    }

    /// Verifies that Latin-2+ legacy keysyms get Unicode encoding to
    /// bypass KWin's mapped forwardKeySym path, while Latin-1 keysyms
    /// stay as-is (mapped path works fine for them, temp-keymap doubles).
    #[test]
    fn latin2_uses_unicode_keysym_latin1_stays_legacy() {
        // ă has legacy Latin-2 keysym 0x1E3 — must be overridden.
        let legacy_a_breve = xkbcommon::xkb::utf32_to_keysym('\u{0103}' as u32);
        assert_eq!(legacy_a_breve.raw(), 0x1E3, "xkbcommon returns legacy keysym for ă");
        // Our logic: legacy > 0xFF → force Unicode
        assert!(legacy_a_breve.raw() > 0xFF && legacy_a_breve.raw() < 0x0100_0000);
        let unicode_a_breve = xkbcommon::xkb::Keysym::new(0x0100_0000 | '\u{0103}' as u32);
        assert_eq!(unicode_a_breve.raw(), 0x0100_0103);

        // đ has legacy Latin-2 keysym 0x1F0 — must be overridden.
        let legacy_dstroke = xkbcommon::xkb::utf32_to_keysym('\u{0111}' as u32);
        assert_eq!(legacy_dstroke.raw(), 0x1F0);
        assert!(legacy_dstroke.raw() > 0xFF && legacy_dstroke.raw() < 0x0100_0000);

        // ú (Latin-1, keysym 0xFA) — must NOT be overridden.
        let latin1_u_acute = xkbcommon::xkb::utf32_to_keysym('\u{00FA}' as u32);
        assert_eq!(latin1_u_acute.raw(), 0xFA, "ú must keep Latin-1 keysym");
        assert!(latin1_u_acute.raw() <= 0xFF, "Latin-1 keysym must stay below override threshold");

        // ư has no legacy keysym — already Unicode, no override needed.
        let unicode_uhorn = xkbcommon::xkb::utf32_to_keysym('\u{01B0}' as u32);
        assert_eq!(unicode_uhorn.raw(), 0x0100_01B0);
        assert!(unicode_uhorn.raw() >= 0x0100_0000, "ư is already Unicode keysym");
    }
}
