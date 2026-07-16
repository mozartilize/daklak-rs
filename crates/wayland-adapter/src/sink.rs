use std::time::Instant;

use viet_ime_edit_strategy::{KeyState, OutputSink};
use viet_ime_key_emitter::KeyEmitter;
use viet_ime_keymap::xkb::XkbState;

use wayland_protocols::wp::input_method::zv1::client::zwp_input_method_context_v1::ZwpInputMethodContextV1;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2;

/// Backend selection for the text-input-v3-shaped operations (the ones
/// `KeyEmitter` doesn't cover): `commit_string`, `commit(serial)`,
/// `delete_surrounding_text`. Key emission is delegated to `dyn KeyEmitter`.
pub(crate) enum TextOpsTarget<'a> {
    V2 {
        im: &'a ZwpInputMethodV2,
    },
    V1 {
        ctx: &'a ZwpInputMethodContextV1,
        serial: u32,
    },
}

/// Routing decision for an entire KWin IMv1 replacement string.
/// Always emit whole replacement through keysym on KWin; splitting a single
/// logical replacement between keysym and commit_string creates non-atomic
/// edits that terminals like Ghostty and xfce4-terminal render incorrectly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum V1ReplacementRoute {
    WholeKeysym,
}

fn v1_replacement_route(_text: &str, _commit_string_functional: bool) -> V1ReplacementRoute {
    V1ReplacementRoute::WholeKeysym
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

fn flush_connection(conn: Option<&wayland_client::Connection>, operation: &'static str) -> bool {
    let Some(conn) = conn else {
        return true;
    };
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
/// - `text_ops` selects v2 (wlroots) vs v1 (KWin) for text-input-v3-shaped
///   events (`commit_string`, `delete_surrounding_text`, `commit(serial)`).
/// - `forward_emitter` emits **standard keycodes** against the
///   compositor's existing keymap. Drives `vk_key` (Tier 2 BS, nav) and
///   `forward_press` (user-typed letters daklak passes through).
///   `vk_v1` on KWin / `vk_v2` everywhere else.
/// - `synth_keymap_emitter` emits **daklak's synthesised keymap slots**
///   (Vietnamese precomposed chars at evdev kc ≤ 191). Drives
///   `vk_commit_char` (the key-channel Vietnamese commit). Only `vk_v2`
///   qualifies — v1 has no `vk_commit_char` parity.
///   `None` on backends where the vk synthetic-keymap commit is
///   unsupported (KWin v1).
pub struct AdapterSink<'a> {
    pub(crate) text_ops: TextOpsTarget<'a>,
    pub(crate) forward_emitter: &'a mut dyn KeyEmitter,
    pub(crate) synth_keymap_emitter: Option<&'a mut dyn KeyEmitter>,
    /// Counter of `vk.modifiers` calls daklak has emitted but not yet seen
    /// echoed back through the IM grab's `Modifiers` event. Incremented per
    /// emit in `vk_commit_char`; AdapterState's `on_modifiers` handler decrements
    /// and skips its own `self.modifiers` update so daklak's modifier state
    /// tracking isn't transiently corrupted by its own dance.
    pub(crate) synthetic_mods_pending: &'a mut u32,
    /// Exact synthetic modifier masks expected to echo back through the IM grab.
    pub(crate) synthetic_mods_expected: &'a mut std::collections::VecDeque<(u32, u32, u32, u32)>,
    /// Stamp of the most recent `synthetic_mods_pending` increment — read by
    /// `on_modifiers` as a TTL safety net (force-reset the counter if no
    /// echo arrives within 50ms, in case the compositor coalesced events).
    pub(crate) synthetic_mods_emitted_at: &'a mut Option<Instant>,
    /// Snapshot of the user's physical modifier state at the time of the
    /// daemon action. `vk_commit_char` may temporarily override these to
    /// address xkb level 2/3/4 of the daklak custom keymap, then restores.
    /// Tuple = (depressed, latched, locked, group) as in
    /// `zwp_input_method_keyboard_grab_v2::Modifiers`.
    pub(crate) raw_mods: (u32, u32, u32, u32),
    /// Tail-char-drop fix. When the user is currently holding a
    /// key whose keycode equals the one daklak is about to re-emit via
    /// `vk_commit_char`, the XWayland X-server input thread silently
    /// drops the synthetic press because that keycode is already
    /// marked DOWN in its state table. Solution: emit a prelude
    /// release for that keycode before the normal press/release pair,
    /// transitioning X's state to UP first.
    pub(crate) held_user_kc: Option<u32>,
    /// Wayland connection — used by `commit_via_keysym` to flush after
    /// the batch of emissions so they reach the compositor in one round.
    pub(crate) conn: Option<&'a wayland_client::Connection>,
    /// xkb state for char→keycode reverse lookup. Used by
    /// `commit_via_keysym` to emit standard ASCII chars via `ctx.key()`
    /// (layout-aware) instead of `ctx.keysym()` which would go through
    /// KWin's forwardKeySym + kc 247 temp keymap and trigger foot's xkb
    /// recompile race.
    pub(crate) xkb: Option<&'a XkbState>,
    /// Whether the focused client applies text-input-v3 `commit_string`.
    /// `true` by default; flipped `false` at the ST→FK downgrade when the
    /// client's surrounding-text proved dead (Google Docs / Firefox
    /// contenteditable) — the same dead text-input-v3 contract drops
    /// `commit_string` too. When `false`, `commit_via_keysym` routes the whole
    /// commit through `vk_commit_char` (v2/sway) or `ctx.keysym` (v1/KWin)
    /// instead of `commit_string`. KWin v1 always keeps replacements on the
    /// single keysym channel; v2 with working commit_string still falls back to
    /// one whole commit_string on the normal path.
    pub(crate) commit_string_functional: bool,
    /// V2 only: flipped to true by `commit()` so `apply_done_frame` can
    /// detect whether daklak already acked the compositor's done event,
    /// and emit a bare heartbeat commit otherwise. See AdapterState field
    /// of the same name for rationale.
    pub(crate) pending_im_commit_ack: &'a mut bool,
}

impl OutputSink for AdapterSink<'_> {
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
            TextOpsTarget::V2 { im } => {
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
            TextOpsTarget::V2 { im } => im.commit_string(text.to_owned()),
            TextOpsTarget::V1 { ctx, serial } => ctx.commit_string(*serial, text.to_owned()),
        }
    }

    fn commit(&mut self, serial: u32) {
        match &self.text_ops {
            TextOpsTarget::V2 { im } => {
                im.commit(serial);
                *self.pending_im_commit_ack = true;
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
        // Standard keycode (BS / nav / forward) → forward_emitter.
        // TRACE the wire order: on v1 this is ctx.key (wl_keyboard channel),
        // emitted interleaved with commit_via_keysym's keysym (wl_keyboard) and
        // commit_string (text-input-v3) channels. Lets a live log confirm
        // whether ForwardKey backspaces actually fire and in what order vs the
        // commit on a contenteditable that reorders the two channels.
        let guard_modifiers = forward_backspace_needs_modifier_guard(key_code, self.raw_mods);
        if guard_modifiers && state == KeyState::Pressed {
            tracing::trace!(
                raw_mods = ?self.raw_mods,
                "vk_key Backspace modifier guard: clear modifiers"
            );
            self.forward_emitter.emit_modifiers(0, 0, 0, self.raw_mods.3);
        }
        tracing::trace!(key_code, value, "vk_key emit (forward channel)");
        self.forward_emitter.emit_key(time, key_code, value);
        if guard_modifiers && state == KeyState::Released {
            tracing::trace!(
                raw_mods = ?self.raw_mods,
                "vk_key Backspace modifier guard: restore modifiers"
            );
            self.forward_emitter.emit_modifiers(
                self.raw_mods.0,
                self.raw_mods.1,
                self.raw_mods.2,
                self.raw_mods.3,
            );
        }
    }

    fn vk_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        // Modifier echo around forwards → forward_emitter. The key-channel
        // commit's modifier dance runs inside emit_char on
        // synth_keymap_emitter and is wired directly there.
        self.forward_emitter
            .emit_modifiers(depressed, latched, locked, group);
    }

    fn vk_commit_char(&mut self, time: u32, c: char) -> bool {
        // The key-channel Vietnamese commit needs daklak's synthesised
        // keymap. Only the
        // synth_keymap_emitter knows how to drive it — KWin v1 leaves
        // this slot `None` and the caller falls back to `commit_string`.
        let Some(emitter) = self.synth_keymap_emitter.as_deref_mut() else {
            tracing::trace!(
                "vk_commit_char: no synth_keymap_emitter available (vk synthetic-keymap commit unsupported on this backend)"
            );
            return false;
        };
        let mut synthetic_mods = viet_ime_key_emitter::SyntheticMods {
            pending: self.synthetic_mods_pending,
            expected: self.synthetic_mods_expected,
            emitted_at: self.synthetic_mods_emitted_at,
        };
        viet_ime_key_emitter::emit_char(
            emitter,
            &mut synthetic_mods,
            viet_ime_key_emitter::EmitCharParams {
                raw_mods: self.raw_mods,
                held_user_kc: self.held_user_kc,
                time,
                c,
            },
        )
    }

    fn commit_via_keysym(&mut self, serial: u32, time: u32, text: &str) -> bool {
        match &self.text_ops {
            // ── ImV2 path: emit via virtual keyboard ──────────────────────
            //
            // ImV2 has no ctx.keysym(). Non-terminal clients (Firefox /
            // Google Docs) ignore text-input-v3 commit_string, so the
            // ForwardKey fallback to commit_string silently drops the
            // entire commit. Route every char through the virtual
            // keyboard instead: vk_commit_char uses the synth keymap
            // (Vietnamese precomposed chars at levels 2-4, ASCII at
            // base level) and already includes the held_user_kc
            // prelude-release fix from emit_char_impl.
            //
            // Clients with a working commit_string (commit_string_functional
            // = true, e.g. foot) keep the commit_string fallback below — they
            // honor text-input-v3 and don't need the synth keymap path.
            TextOpsTarget::V2 { .. } if !self.commit_string_functional => {
                tracing::trace!(text = %text, "commit_via_keysym: enter (v2 vk path)");
                let mut fallback_buf = String::new();
                for c in text.chars() {
                    if self.vk_commit_char(time, c) {
                        if !flush_connection(self.conn, "flush synthetic character") {
                            // Emission was attempted. Returning false would replay
                            // the replacement through commit_string and risk duplicates.
                            return true;
                        }
                        continue;
                    }
                    // Char not in synth keymap — buffer for commit_string.
                    fallback_buf.push(c);
                }
                if !fallback_buf.is_empty() {
                    tracing::trace!(
                        text = %fallback_buf,
                        "commit_via_keysym: fallback commit_string for chars outside synth keymap"
                    );
                    self.commit_string(&fallback_buf);
                    self.commit(serial);
                }
                true
            }

            // ── ImV1 path: emit via ctx.keysym() ─────────────────────────
            TextOpsTarget::V1 { ctx, .. } => self.commit_via_keysym_v1(ctx, serial, time, text),
            // ImV2 with working commit_string: no keysym path, let
            // forward_key fall back to commit_string.
            TextOpsTarget::V2 { .. } => false,
        }
    }

    fn commit_key_channel_text(&mut self, _serial: u32, time: u32, text: &str) -> bool {
        match &self.text_ops {
            // ImV2: preflight all chars are in the synth keymap, then emit.
            TextOpsTarget::V2 { .. } => {
                if !text.chars().all(|c| {
                    viet_ime_keymap::char_to_emit(c).is_some()
                }) {
                    return false;
                }
                for c in text.chars() {
                    if !self.vk_commit_char(time, c) {
                        return false;
                    }
                    if !flush_connection(self.conn, "flush key-channel character") {
                        return true;
                    }
                }
                true
            }
            // ImV1: delegate to whole-keysym path.
            TextOpsTarget::V1 { ctx, .. } => {
                self.commit_via_keysym_v1(ctx, _serial, time, text)
            }
        }
    }
}

impl AdapterSink<'_> {
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

        let _route = v1_replacement_route(text, self.commit_string_functional);

        tracing::trace!(text = %text, "commit_via_keysym: enter (v1 keysym path)");

        for c in text.chars() {
            let is_unmapped = match self.xkb {
                Some(xkb) => xkb.char_to_keycode(c).is_none(),
                None => true,
            };
            // Whole replacement stays on the keysym channel. No per-char
            // commit_string split — mixed key/text-input replacement is not
            // a coherent edit for KWin terminals.

            // Tail-char-drop fix: for mapped ASCII chars whose
            // physical keycode the user is still holding, emit a
            // prelude key-release through the forward channel so
            // KWin's forwardKeySym pressedKeys dedup doesn't drop
            // the synthetic keysym press.
            if !is_unmapped {
                if let Some(kc) = self.xkb.and_then(|xkb| xkb.char_to_keycode(c)) {
                    if self.held_user_kc == Some(kc) {
                        tracing::debug!(
                            kc,
                            char = %c,
                            "commit_via_keysym: prelude release for still-held user key (tail-char-drop fix)"
                        );
                        self.forward_emitter.emit_key(time, kc, 0);
                    }
                }
            }

            // Latin-2+ legacy keysyms (e.g. ă=0x1E3, đ=0x1F0) may exist
            // in the user's xkb layout, causing KWin's keycodeFromKeysym to
            // take the "mapped" forwardKeySym path which double-outputs on
            // Ghostty+pi due to a modifier-change + key-event race. Force
            // Unicode keysym encoding (0x01000000 | codepoint) for these so
            // KWin always takes the unmapped/temp-keymap path (kc 247).
            //
            // Latin-1 chars (ú=0xFA, ó=0xF3, etc.) MUST keep their legacy
            // keysyms (= codepoint): KWin's mapped path works correctly for
            // them, and the temp-keymap path doubles on Ghostty+pi.
            let sym = xkbcommon::xkb::utf32_to_keysym(c as u32);
            let sym = if sym.raw() > 0xFF && sym.raw() < 0x0100_0000 {
                // Legacy Latin-2+ keysym — override to Unicode encoding
                xkbcommon::xkb::Keysym::new(0x0100_0000 | c as u32)
            } else {
                sym
            };
            if sym.raw() == 0 {
                tracing::debug!(c = %c, "commit_via_keysym: no keysym for char, falling back");
                return false;
            }
            // Per-char keysym barrier: KWin's forwardKeySym installs a
            // per-sym temp keymap (kc 247) for unmapped keysyms → foot
            // recompiles xkb. Flush + sleep so foot finishes before the
            // next char or forwarded physical key arrives.
            let guard_modifiers = v1_keysym_needs_modifier_guard(
                is_unmapped,
                self.commit_string_functional,
                self.raw_mods,
            );
            if guard_modifiers {
                self.forward_emitter.emit_modifiers(0, 0, 0, self.raw_mods.3);
            }
            ctx.keysym(serial, time, sym.raw(), PRESSED, 0);
            ctx.keysym(serial, time, sym.raw(), RELEASED, 0);
            if guard_modifiers {
                self.forward_emitter.emit_modifiers(
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
                "commit_via_keysym: keysym emit + barrier (unmapped)"
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

    // ── V1ReplacementRoute tests ───────────────────────────────────────

    #[test]
    fn v1_replacement_uses_whole_keysym_even_when_commit_string_functional() {
        assert_eq!(
            v1_replacement_route("ập", true),
            V1ReplacementRoute::WholeKeysym
        );
    }

    #[test]
    fn v1_replacement_uses_whole_keysym_when_commit_string_is_not_functional() {
        assert_eq!(
            v1_replacement_route("ập", false),
            V1ReplacementRoute::WholeKeysym
        );
    }

    #[test]
    fn v1_mapped_ascii_tail_is_allowed_on_keysym_channel() {
        assert_eq!(
            v1_replacement_route("ếng", true),
            V1ReplacementRoute::WholeKeysym
        );
        assert_eq!(
            v1_replacement_route("ập", false),
            V1ReplacementRoute::WholeKeysym
        );
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
