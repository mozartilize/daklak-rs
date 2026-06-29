pub mod capability;
pub mod shadow;

mod forward_key;
mod surrounding;
pub mod uinput_device;
mod vk_only;

pub use capability::{detect_method, CapabilityProbe};
pub use shadow::ShadowBuffer;

use bitflags::bitflags;

/// Which edit mechanism a given text-input object gets.
/// One per text_input_object, NOT per window — Firefox has separate objects
/// for address bar vs page content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackspaceMethod {
    SurroundingText, // Tier 1 — delete_surrounding_text
    ForwardKey,      // Tier 2 — zwp_virtual_keyboard_v1 synthetic BS
    /// Tier 4 (VkOnly) — everything via `zwp_virtual_keyboard_v1::key()`,
    /// using daklak's synthesized xkb keymap that maps spare evdev slots
    /// (200+) to Vietnamese precomposed chars. No `commit_string`, no
    /// `zwp_text_input_v3` — usable for clients that never advertise
    /// text-input-v3 (Qt5/XWayland-via-vk).
    VkOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteUnit {
    Bytes,
    Chars,
}

/// Daemon's decision after processing a single key press. Returned from
/// the handler back into either adapter (wayland or evdev) so the adapter
/// can dispatch correctly.
pub enum KeyDecision {
    /// Engine consumed the key; no emit needed.
    Consumed,
    /// Engine did not consume; adapter forwards the key as-is (vk.key
    /// press on wayland, raw uinput passthrough on evdev) and stamps
    /// `last_forwarded_key` where applicable.
    ForwardRaw,
    /// Engine consumed and produced an edit. Adapter computes `held_user_kc`
    /// (tail-char-drop fix) before passing it to the handler.
    Apply {
        method: BackspaceMethod,
        backspaces: usize,
        commit: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
}

bitflags! {
    /// Bitmask of currently held modifier keys, extracted from the
    /// `mods_depressed` field of zwp_input_method_keyboard_grab_v2 Modifiers
    /// events (kime pattern: state.rs:694-721).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct ModifierState: u32 {
        const SHIFT = 0x01;
        const CTRL  = 0x04;
        const ALT   = 0x08;
        const SUPER = 0x40;
    }
}

/// The sink the daemon implements. Each method maps to one transport-level
/// edit or key-emission operation. The order matters and is fixed by Strategy.
pub trait OutputSink {
    // Tier 1
    /// Delete text around the cursor. `before_bytes`/`after_bytes` are
    /// UTF-8 byte counts (what wlroots v2/v3 IM and all spec-compliant
    /// v3 clients want); `before_chars`/`after_chars` are Unicode scalar
    /// counts (what firefox's v3 client wants on its KWin v1↔v3 path —
    /// see `force_chars_delete_apps` config). The sink picks whichever
    /// unit its backend + the per-window app match dictates.
    fn delete_surrounding_text(
        &mut self,
        before_bytes: u32,
        before_chars: u32,
        after_bytes: u32,
        after_chars: u32,
    );
    // All tiers
    fn commit_string(&mut self, text: &str);
    fn commit(&mut self, serial: u32);
    // Tier 2 — zwp_virtual_keyboard_v1
    fn vk_key(&mut self, time: u32, key_code: u32, state: KeyState);
    fn vk_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32);
    /// Tier 4 — emit `c` via `vk_key()` using daklak's synthesized
    /// Vietnamese keymap. Returns `false` if `c` isn't in the keymap
    /// inventory (caller should fall back).
    fn vk_commit_char(&mut self, time: u32, c: char) -> bool;

    /// Emit `text` as a sequence of `(press, release)` keysym pairs.
    /// Only implemented on ImV1 via `zwp_input_method_context_v1::keysym`,
    /// which KWin synthesises into real `wl_keyboard.key` events (using a
    /// temporary keymap with `unmappedKeyCode=247` for Unicode keysyms
    /// that don't exist in the system layout). This is the only path that
    /// delivers Vietnamese precomposed chars to **terminal** clients on
    /// KWin: terminals don't honor `commit_string` (text-input-v3 →
    /// editable widget) but do honor `wl_keyboard.key` (→ PTY UTF-8).
    ///
    /// Returns `true` if emitted; `false` if the backend has no keysym
    /// path (caller falls back to `commit_string` + `commit`).
    fn commit_via_keysym(&mut self, _serial: u32, _time: u32, _text: &str) -> bool {
        false
    }
}

/// Per-window edit state. The daemon owns one `Strategy` per
/// `text_input_object` it is tracking.
pub struct Strategy {
    method: BackspaceMethod,
    pub shadow: ShadowBuffer,
    modifiers: ModifierState,
}

impl Strategy {
    pub fn new(method: BackspaceMethod) -> Self {
        Self {
            method,
            shadow: ShadowBuffer::new(),
            modifiers: ModifierState::empty(),
        }
    }

    /// Apply an engine ProcessResult through the configured tier.
    /// `backspaces` is a char count (from vnkey-engine); conversion to bytes
    /// for Tier 1 happens inside `surrounding::apply`.
    pub fn apply(
        &mut self,
        backspaces: usize,
        commit: &str,
        serial: u32,
        time: u32,
        sink: &mut impl OutputSink,
        delete_unit: DeleteUnit,
    ) {
        match self.method {
            BackspaceMethod::SurroundingText => {
                surrounding::apply(
                    &mut self.shadow,
                    backspaces,
                    commit,
                    serial,
                    time,
                    sink,
                    delete_unit,
                );
            }
            BackspaceMethod::ForwardKey => {
                forward_key::apply(
                    &mut self.shadow,
                    backspaces,
                    commit,
                    serial,
                    time,
                    sink,
                    false,
                );
            }
            BackspaceMethod::VkOnly => {
                vk_only::apply(&mut self.shadow, backspaces, commit, time, sink);
            }
        }
    }

    /// Like `apply`, but uses ForwardKey (virtual keyboard Backspace keys)
    /// regardless of the current method. Does NOT change `self.method`.
    /// Used by the Firefox contenteditable quirk to bypass
    /// `delete_surrounding_text` which Firefox handles unreliably.
    pub fn apply_forward_key(
        &mut self,
        backspaces: usize,
        commit: &str,
        serial: u32,
        time: u32,
        sink: &mut impl OutputSink,
    ) {
        forward_key::apply(
            &mut self.shadow,
            backspaces,
            commit,
            serial,
            time,
            sink,
            true,
        );
    }

    /// Observe a surrounding_text frame from the compositor. Resets shadow on
    /// cursor delta (the priority-1 invalidation signal).
    pub fn on_surrounding_text(&mut self, text: &str, cursor: u32, anchor: u32) {
        self.shadow.observe_surrounding(text, cursor, anchor);
    }

    /// Reset shadow on focus loss or navigation key.
    pub fn reset_shadow(&mut self) {
        self.shadow.clear();
    }

    /// Update modifier state from a Modifiers event on the keyboard grab.
    pub fn set_modifiers(&mut self, m: ModifierState) {
        self.modifiers = m;
    }

    pub fn method(&self) -> BackspaceMethod {
        self.method
    }

    /// Runtime tier switch. Two callers, opposite directions:
    ///   • Late upgrade — the activate frame had no surrounding info (Tier 1
    ///     demoted to ForwardKey) but a later frame proves the app supports
    ///     surrounding-text.
    ///   • Late downgrade — the app advertised surrounding-text but never
    ///     honors it (Google Docs / Firefox contenteditable), so Tier 1 is
    ///     abandoned for ForwardKey after repeated dead frames.
    /// Either way shadow state is preserved; ForwardKey derives its backspace
    /// count from the engine, not the shadow, so a stale shadow is harmless.
    pub fn set_method(&mut self, m: BackspaceMethod) {
        self.method = m;
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── MockSink ──────────────────────────────────────────────────────────────

    #[derive(Debug, Default)]
    struct MockSink {
        calls: Vec<Call>,
    }

    #[derive(Debug, PartialEq)]
    enum Call {
        DeleteSurroundingText(u32, u32, u32, u32),
        CommitString(String),
        Commit(u32),
        VkKey(u32, u32, KeyState),
        VkModifiers(u32, u32, u32, u32),
        VkCommitChar(u32, char),
    }

    impl OutputSink for MockSink {
        fn delete_surrounding_text(
            &mut self,
            before_bytes: u32,
            before_chars: u32,
            after_bytes: u32,
            after_chars: u32,
        ) {
            self.calls.push(Call::DeleteSurroundingText(
                before_bytes,
                before_chars,
                after_bytes,
                after_chars,
            ));
        }
        fn commit_string(&mut self, text: &str) {
            self.calls.push(Call::CommitString(text.to_owned()));
        }
        fn commit(&mut self, serial: u32) {
            self.calls.push(Call::Commit(serial));
        }
        fn vk_key(&mut self, time: u32, key_code: u32, state: KeyState) {
            self.calls.push(Call::VkKey(time, key_code, state));
        }
        fn vk_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
            self.calls
                .push(Call::VkModifiers(depressed, latched, locked, group));
        }
        fn vk_commit_char(&mut self, time: u32, c: char) -> bool {
            self.calls.push(Call::VkCommitChar(time, c));
            true
        }
    }

    // ── Tier 1 — SurroundingText ──────────────────────────────────────────────

    #[test]
    fn tier1_single_ascii_backspace() {
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.shadow.append("a");
        let mut sink = MockSink::default();
        s.apply(1, "â", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(
            sink.calls,
            vec![
                Call::DeleteSurroundingText(1, 1, 0, 0), // "a" = 1 byte = 1 char
                Call::CommitString("â".to_owned()),
                Call::Commit(1),
            ]
        );
        assert_eq!(s.shadow.text(), "â");
    }

    #[test]
    fn tier1_multibyte_delete() {
        // shadow = "â" (2 bytes, 1 char), pop 1 char → delete 2 bytes / 1 char.
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.shadow.append("â");
        let mut sink = MockSink::default();
        s.apply(1, "ầ", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(sink.calls[0], Call::DeleteSurroundingText(2, 1, 0, 0));
        assert_eq!(sink.calls[1], Call::CommitString("ầ".to_owned()));
        assert_eq!(s.shadow.text(), "ầ");
    }

    #[test]
    fn tier1_multibyte_delete_can_emit_char_count_primary_length() {
        // Firefox's stale ContentCacheInParent can interpret the primary
        // delete length against stale ASCII text. Char mode emits 1 for the
        // visible scalar even though the shadow char is 2 bytes.
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.shadow.append("ư");
        let mut sink = MockSink::default();
        s.apply(1, "ự", 1, 0, &mut sink, DeleteUnit::Chars);
        assert_eq!(sink.calls[0], Call::DeleteSurroundingText(1, 1, 0, 0));
        assert_eq!(sink.calls[1], Call::CommitString("ự".to_owned()));
        assert_eq!(s.shadow.text(), "ự");
    }

    #[test]
    fn tier1_retroactive_pho_horn_to_pho_hook() {
        // Retroactive editing scenario from docs/protocol-behavior.md:
        // shadow seeded with "phơ" after feed_context + one keypress.
        // Engine returns bs=1 (the "ơ"), commit="ở".
        // "ph" = 2 bytes, "ơ" = 2 bytes, pop 1 char (ơ) → (2 bytes, 1 char).
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.shadow.append("phơ");
        let mut sink = MockSink::default();
        s.apply(1, "ở", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(sink.calls[0], Call::DeleteSurroundingText(2, 1, 0, 0));
        assert_eq!(sink.calls[1], Call::CommitString("ở".to_owned()));
        assert_eq!(s.shadow.text(), "phở");
    }

    #[test]
    fn tier1_multi_char_delete() {
        // shadow = "tieê", apply bs=4 → pops 4 chars
        // t=1, i=1, e=1, ê=2 → total 5 bytes / 4 chars
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.shadow.append("tieê");
        let mut sink = MockSink::default();
        s.apply(4, "tiến", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(sink.calls[0], Call::DeleteSurroundingText(5, 4, 0, 0));
        assert_eq!(sink.calls[1], Call::CommitString("tiến".to_owned()));
    }

    // ── Tier 2 — ForwardKey ───────────────────────────────────────────────────

    #[test]
    fn tier2_single_backspace() {
        let mut s = Strategy::new(BackspaceMethod::ForwardKey);
        s.shadow.append("a");
        let mut sink = MockSink::default();
        s.apply(1, "â", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(
            sink.calls,
            vec![
                Call::VkKey(0, 14, KeyState::Pressed),
                Call::VkKey(0, 14, KeyState::Released),
                Call::CommitString("â".to_owned()),
                Call::Commit(1),
            ]
        );
    }

    #[test]
    fn tier2_three_backspaces() {
        let mut s = Strategy::new(BackspaceMethod::ForwardKey);
        s.shadow.append("abc");
        let mut sink = MockSink::default();
        s.apply(3, "x", 2, 5, &mut sink, DeleteUnit::Bytes);
        // 3×(Pressed,Released) = 6 calls, then CommitString, Commit
        assert_eq!(sink.calls.len(), 8);
        assert_eq!(sink.calls[0], Call::VkKey(5, 14, KeyState::Pressed));
        assert_eq!(sink.calls[1], Call::VkKey(5, 14, KeyState::Released));
        assert_eq!(sink.calls[2], Call::VkKey(5, 14, KeyState::Pressed));
        assert_eq!(sink.calls[3], Call::VkKey(5, 14, KeyState::Released));
        assert_eq!(sink.calls[4], Call::VkKey(5, 14, KeyState::Pressed));
        assert_eq!(sink.calls[5], Call::VkKey(5, 14, KeyState::Released));
        assert_eq!(sink.calls[6], Call::CommitString("x".to_owned()));
        assert_eq!(sink.calls[7], Call::Commit(2));
    }

    #[test]
    fn tier2_shadow_updated() {
        let mut s = Strategy::new(BackspaceMethod::ForwardKey);
        s.shadow.append("abc");
        let mut sink = MockSink::default();
        s.apply(1, "â", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(s.shadow.text(), "abâ");
    }

    // ── Tier 4 — VkOnly ──────────────────────────────────────────────

    #[test]
    fn tier4_single_backspace_and_commit() {
        let mut s = Strategy::new(BackspaceMethod::VkOnly);
        s.shadow.append("a");
        let mut sink = MockSink::default();
        s.apply(1, "â", 7, 42, &mut sink, DeleteUnit::Bytes);
        assert_eq!(
            sink.calls,
            vec![
                Call::VkKey(42, 14, KeyState::Pressed),
                Call::VkKey(42, 14, KeyState::Released),
                Call::VkCommitChar(42, 'â'),
            ]
        );
        assert!(!sink
            .calls
            .iter()
            .any(|c| matches!(c, Call::CommitString(_))));
        assert!(!sink.calls.iter().any(|c| matches!(c, Call::Commit(_))));
        assert_eq!(s.shadow.text(), "â");
    }

    #[test]
    fn tier4_multichar_commit_each_via_vk() {
        let mut s = Strategy::new(BackspaceMethod::VkOnly);
        s.shadow.append("ph");
        let mut sink = MockSink::default();
        s.apply(0, "ởn", 0, 5, &mut sink, DeleteUnit::Bytes);
        assert_eq!(
            sink.calls,
            vec![Call::VkCommitChar(5, 'ở'), Call::VkCommitChar(5, 'n'),]
        );
        assert_eq!(s.shadow.text(), "phởn");
    }

    #[test]
    fn tier4_three_backspaces() {
        let mut s = Strategy::new(BackspaceMethod::VkOnly);
        s.shadow.append("abc");
        let mut sink = MockSink::default();
        s.apply(3, "x", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(
            sink.calls,
            vec![
                Call::VkKey(0, 14, KeyState::Pressed),
                Call::VkKey(0, 14, KeyState::Released),
                Call::VkKey(0, 14, KeyState::Pressed),
                Call::VkKey(0, 14, KeyState::Released),
                Call::VkKey(0, 14, KeyState::Pressed),
                Call::VkKey(0, 14, KeyState::Released),
                Call::VkCommitChar(0, 'x'),
            ]
        );
        assert_eq!(s.shadow.text(), "x");
    }

    // ── Shadow invalidation ───────────────────────────────────────────────────

    #[test]
    fn surrounding_text_syncs_shadow() {
        // Shadow is synced to compositor's text[..cursor] — that's how
        // Tier 1 gets correct byte counts for delete_surrounding_text.
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.on_surrounding_text("cha", 3, 3);
        assert_eq!(s.shadow.text(), "cha");
    }

    #[test]
    fn tier1_uses_surrounding_text_bytes() {
        // Daemon receives surrounding_text "châ" cursor=4 from compositor.
        // Engine returns bs=1 commit="ầ". Tier 1 should delete the â
        // (2 bytes / 1 char) and commit "ầ".
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.on_surrounding_text("châ", 4, 4); // "châ" = 4 bytes
        let mut sink = MockSink::default();
        s.apply(1, "ầ", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(sink.calls[0], Call::DeleteSurroundingText(2, 1, 0, 0));
        assert_eq!(sink.calls[1], Call::CommitString("ầ".to_owned()));
    }

    #[test]
    fn tier1_selection_falls_back_to_forward_key() {
        // Chromium may keep an active selection (anchor > cursor) on first
        // focused word. Using delete_surrounding_text races with the
        // wl_keyboard path (key release arrives before our text edit),
        // causing Chrome to change its autocomplete state and reject the
        // delete.
        //
        // Fix: when selection is present, fall back to ForwardKey (virtual
        // keyboard backspaces). One BS clears the selection, then engine-
        // requested BSes delete individual chars. Chrome handles this
        // deterministically regardless of selection state.
        //
        // Scenario: Chromium omnibox + Google search provider inline-
        // autocomplete injects "translate" with cursor=3, anchor=9.
        // Engine says bs=1 (delete 'a'), commit "â".
        // Expected: 2 BS (1 for selection + 1 for 'a') + commit "â".
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.on_surrounding_text("translate", 3, 9);
        let mut sink = MockSink::default();
        s.apply(1, "â", 1, 0, &mut sink, DeleteUnit::Bytes);
        assert_eq!(
            sink.calls,
            vec![
                Call::VkKey(0, 14, KeyState::Pressed), // BS 1: clears selection "nslate"
                Call::VkKey(0, 14, KeyState::Released),
                Call::VkKey(0, 14, KeyState::Pressed), // BS 2: deletes 'a'
                Call::VkKey(0, 14, KeyState::Released),
                Call::CommitString("â".to_owned()),
                Call::Commit(1),
            ]
        );
        assert_eq!(s.shadow.text(), "trâ");
    }

    #[test]
    fn reset_shadow_clears() {
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.shadow.append("hello");
        s.reset_shadow();
        assert_eq!(s.shadow.text(), "");
    }
}
