use std::collections::VecDeque;
use std::time::Instant;

use viet_ime_edit_strategy::uinput_device::UinputDevice;
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
///   `vk_commit_char` (Tier 4 VkOnly). Only `vk_v2` qualifies — v1 has
///   no `vk_commit_char` parity.
///   `None` on backends where Tier 4 is unsupported (KWin v1).
pub struct AdapterSink<'a> {
    pub(crate) text_ops: TextOpsTarget<'a>,
    pub(crate) forward_emitter: &'a mut dyn KeyEmitter,
    pub(crate) synth_keymap_emitter: Option<&'a mut dyn KeyEmitter>,
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
    /// Per-window override: when true, `delete_surrounding_text` emits a CHAR
    /// count rather than the spec-compliant byte count. Independent from the
    /// post-apply debounce barrier; firefox happens to need both.
    pub(crate) delete_in_chars: bool,
    /// Wayland connection — used by `commit_via_keysym` to flush after
    /// the batch of emissions so they reach the compositor in one round.
    pub(crate) conn: Option<&'a wayland_client::Connection>,
    /// xkb state for char→keycode reverse lookup. Used by
    /// `commit_via_keysym` to emit standard ASCII chars via `ctx.key()`
    /// (layout-aware) instead of `ctx.keysym()` which would go through
    /// KWin's forwardKeySym + kc 247 temp keymap and trigger foot's xkb
    /// recompile race.
    pub(crate) xkb: Option<&'a XkbState>,
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
        before_chars: u32,
        after_bytes: u32,
        after_chars: u32,
    ) {
        match &self.text_ops {
            // v2/wlroots: spec says bytes, but firefox's v3 client
            // counts in chars/ASCII-units and stops at the first
            // multibyte boundary (e.g. del 3 bytes over "án"=3 bytes
            // only removes the trailing ASCII 'n', leaving 'á' →
            // result "tráans" instead of "trans"). Gated per-window
            // via delete_in_chars (config: force_chars_delete_apps).
            TextOpsTarget::V2 { im } => {
                let (before, after) = select_delete_units(
                    self.delete_in_chars,
                    before_bytes,
                    before_chars,
                    after_bytes,
                    after_chars,
                );
                im.delete_surrounding_text(before, after);
            }
            // v1: spec is bytes (text-input-unstable-v1.xml), but
            // firefox's v3 client on KWin's bridge expects chars —
            // gated per-window via delete_in_chars.
            TextOpsTarget::V1 { ctx, .. } => {
                let (before, after) = select_delete_units(
                    self.delete_in_chars,
                    before_bytes,
                    before_chars,
                    after_bytes,
                    after_chars,
                );
                let index = -(before as i32);
                let length = before + after;
                ctx.delete_surrounding_text(index, length);
            }
        }
    }

    fn commit_string(&mut self, text: &str) {
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
        self.forward_emitter.emit_key(time, key_code, value);
    }

    fn vk_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        // Modifier echo around forwards / Tier 3 mod-guard → forward_emitter.
        // Tier 4's modifier dance runs inside emit_char on synth_keymap_emitter
        // and is wired directly there.
        self.forward_emitter
            .emit_modifiers(depressed, latched, locked, group);
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
        // Tier 4 (VkOnly) needs daklak's synthesised keymap. Only the
        // synth_keymap_emitter knows how to drive it — KWin v1 leaves
        // this slot `None` and the caller falls back to `commit_string`.
        let Some(emitter) = self.synth_keymap_emitter.as_deref_mut() else {
            tracing::trace!(
                "vk_commit_char: no synth_keymap_emitter available (Tier 4 unsupported on this backend)"
            );
            return false;
        };
        viet_ime_key_emitter::emit_char(
            emitter,
            self.synthetic_mods_pending,
            self.synthetic_mods_emitted_at,
            self.raw_mods,
            self.held_user_kc,
            time,
            c,
        )
    }

    fn commit_via_keysym(&mut self, serial: u32, time: u32, text: &str) -> bool {
        // Only ImV1 implements this; V2 has no zwp_input_method_v2::keysym
        // equivalent (it has commit_string + commit batching).
        let TextOpsTarget::V1 { ctx, .. } = &self.text_ops else {
            return false;
        };
        // wl_keyboard_key_state: 1=Pressed, 0=Released.
        const PRESSED: u32 = 1;
        const RELEASED: u32 = 0;

        // Collect consecutive ASCII chars into buf and flush them as a
        // single ctx.commit_string(). This goes through text-input-v3,
        // which has NO dedup (unlike ctx.key() / ctx.keysym() → MAPPED
        // forwardKeySym, both of which hit KWin's KeyboardInterface::
        // updateKey pressedKeys check). When the physical keycode for
        // an ASCII char in the commit is still held by the user (e.g. 'g'
        // while typing "ếng"), KWin drops the synthetic press because that
        // keycode is already in pressedKeys from the forwarded physical
        // press. commit_string() via text-input-v3 bypasses this
        // deduplication entirely.
        let mut ascii_buf = String::new();
        let flush_ascii = |buf: &mut String, ctx: &ZwpInputMethodContextV1, serial: u32| {
            if !buf.is_empty() {
                ctx.commit_string(serial, std::mem::take(buf));
            }
        };

        for c in text.chars() {
            let is_unmapped = match self.xkb {
                Some(xkb) => xkb.char_to_keycode(c).is_none(),
                None => true,
            };
            if !is_unmapped {
                ascii_buf.push(c);
                continue;
            }

            // Vietnamese (unmapped) char — flush pending ASCII first so
            // commit_string arrives before the keysym's keymap dance.
            flush_ascii(&mut ascii_buf, ctx, serial);

            let sym = xkbcommon::xkb::utf32_to_keysym(c as u32);
            if sym.raw() == 0 {
                tracing::debug!(c = %c, "commit_via_keysym: no keysym for char, falling back");
                return false;
            }
            // Per-char keysym barrier: KWin's forwardKeySym installs a
            // per-sym temp keymap (kc 247) for unmapped keysyms → foot
            // recompiles xkb. Flush + sleep so foot finishes before the
            // next char or forwarded physical key arrives.
            ctx.keysym(serial, time, sym.raw(), PRESSED, 0);
            ctx.keysym(serial, time, sym.raw(), RELEASED, 0);
            if let Some(conn) = self.conn {
                let _ = conn.flush();
            }
            tracing::trace!(c = %c, sym = sym.raw(), "commit_via_keysym: keysym emit + barrier (unmapped)");
        }

        // Flush any trailing ASCII.
        flush_ascii(&mut ascii_buf, ctx, serial);

        if let Some(conn) = self.conn {
            let _ = conn.flush();
        }
        true
    }
}

fn select_delete_units(
    delete_in_chars: bool,
    before_bytes: u32,
    before_chars: u32,
    after_bytes: u32,
    after_chars: u32,
) -> (u32, u32) {
    if delete_in_chars {
        (before_chars, after_chars)
    } else {
        (before_bytes, after_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::select_delete_units;

    #[test]
    fn delete_in_chars_selects_char_counts_without_implying_debounce() {
        assert_eq!(select_delete_units(true, 6, 2, 3, 1), (2, 1));
    }

    #[test]
    fn byte_delete_keeps_spec_byte_counts() {
        assert_eq!(select_delete_units(false, 6, 2, 3, 1), (6, 3));
    }
}
