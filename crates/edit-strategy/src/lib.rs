pub mod capability;
pub mod shadow;

mod surrounding;
mod forward_key;
mod uinput_backspace;
pub mod uinput_device;
mod vk_only;

pub use capability::{CapabilityProbe, SurroundingFrame, detect_method};
pub use shadow::ShadowBuffer;

use bitflags::bitflags;

/// Which Wayland/uinput mechanism a given text-input object gets.
/// One per text_input_object, NOT per window — Firefox has separate objects
/// for address bar vs page content (plan0.md:354-369).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackspaceMethod {
    SurroundingText, // Tier 1 — delete_surrounding_text
    ForwardKey,      // Tier 2 — zwp_virtual_keyboard_v1 synthetic BS
    UInput,          // Tier 3 — /dev/uinput synthetic BS with modifier guard
    /// Tier 4 (Path C) — everything via `zwp_virtual_keyboard_v1::key()`,
    /// using daklak's synthesized xkb keymap that maps spare evdev slots
    /// (200+) to Vietnamese precomposed chars. No `commit_string`, no
    /// `zwp_text_input_v3` — usable for clients that never advertise
    /// text-input-v3 (Qt5/XWayland-via-vk).
    VkOnly,
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
    /// Engine consumed and produced an edit. Adapter wraps `apply_pending`
    /// in the Tier-3 grab dance (when method == UInput) and computes
    /// `held_user_kc` (Path A) before passing both to the handler.
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

impl ModifierState {
    /// Iterator over the individual set bits — used by uinput_backspace to
    /// release/restore each modifier individually.
    pub fn all_bits() -> [ModifierState; 4] {
        [
            ModifierState::SHIFT,
            ModifierState::CTRL,
            ModifierState::ALT,
            ModifierState::SUPER,
        ]
    }
}

/// The sink the daemon implements. Each method maps to exactly one Wayland
/// request or one uinput emit. The order matters and is fixed by Strategy.
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
    // Tier 3 — /dev/uinput
    fn uinput_key(&mut self, key_code: u16, value: i32); // 1=press, 0=release
    /// Tier 4 — emit `c` via `vk_key()` using daklak's synthesized
    /// Vietnamese keymap. Returns `false` if `c` isn't in the keymap
    /// inventory (caller should fall back).
    fn vk_commit_char(&mut self, time: u32, c: char) -> bool;

    /// Emit `text` as a sequence of `(press, release)` keysym pairs.
    /// Only implemented on V1Kde via `zwp_input_method_context_v1::keysym`,
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
    ) {
        match self.method {
            BackspaceMethod::SurroundingText => {
                surrounding::apply(&mut self.shadow, backspaces, commit, serial, sink);
            }
            BackspaceMethod::ForwardKey => {
                forward_key::apply(&mut self.shadow, backspaces, commit, serial, time, sink);
            }
            BackspaceMethod::UInput => {
                uinput_backspace::apply(
                    &mut self.shadow,
                    backspaces,
                    commit,
                    serial,
                    self.modifiers,
                    sink,
                );
            }
            BackspaceMethod::VkOnly => {
                vk_only::apply(&mut self.shadow, backspaces, commit, time, sink);
            }
        }
    }

    /// Observe a surrounding_text frame from the compositor. Resets shadow on
    /// cursor delta (plan0.md priority-1 invalidation).
    pub fn on_surrounding_text(&mut self, text: &str, cursor: u32) {
        self.shadow.observe_surrounding(text, cursor);
    }

    /// Reset shadow on focus loss or navigation key.
    pub fn reset_shadow(&mut self) {
        self.shadow.clear();
    }

    /// Update modifier state from a Modifiers event on the keyboard grab.
    /// Called by the daemon; used by Tier 3's modifier guard.
    pub fn set_modifiers(&mut self, m: ModifierState) {
        self.modifiers = m;
    }

    pub fn method(&self) -> BackspaceMethod {
        self.method
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
        UinputKey(u16, i32),
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
            self.calls.push(Call::VkModifiers(depressed, latched, locked, group));
        }
        fn uinput_key(&mut self, key_code: u16, value: i32) {
            self.calls.push(Call::UinputKey(key_code, value));
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
        s.apply(1, "â", 1, 0, &mut sink);
        assert_eq!(sink.calls, vec![
            Call::DeleteSurroundingText(1, 1, 0, 0), // "a" = 1 byte = 1 char
            Call::CommitString("â".to_owned()),
            Call::Commit(1),
        ]);
        assert_eq!(s.shadow.text(), "â");
    }

    #[test]
    fn tier1_multibyte_delete() {
        // shadow = "â" (2 bytes, 1 char), pop 1 char → delete 2 bytes / 1 char.
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.shadow.append("â");
        let mut sink = MockSink::default();
        s.apply(1, "ầ", 1, 0, &mut sink);
        assert_eq!(sink.calls[0], Call::DeleteSurroundingText(2, 1, 0, 0));
        assert_eq!(sink.calls[1], Call::CommitString("ầ".to_owned()));
        assert_eq!(s.shadow.text(), "ầ");
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
        s.apply(1, "ở", 1, 0, &mut sink);
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
        s.apply(4, "tiến", 1, 0, &mut sink);
        assert_eq!(sink.calls[0], Call::DeleteSurroundingText(5, 4, 0, 0));
        assert_eq!(sink.calls[1], Call::CommitString("tiến".to_owned()));
    }

    // ── Tier 2 — ForwardKey ───────────────────────────────────────────────────

    #[test]
    fn tier2_single_backspace() {
        let mut s = Strategy::new(BackspaceMethod::ForwardKey);
        s.shadow.append("a");
        let mut sink = MockSink::default();
        s.apply(1, "â", 1, 0, &mut sink);
        assert_eq!(sink.calls, vec![
            Call::VkKey(0, 14, KeyState::Pressed),
            Call::VkKey(0, 14, KeyState::Released),
            Call::CommitString("â".to_owned()),
            Call::Commit(1),
        ]);
    }

    #[test]
    fn tier2_three_backspaces() {
        let mut s = Strategy::new(BackspaceMethod::ForwardKey);
        s.shadow.append("abc");
        let mut sink = MockSink::default();
        s.apply(3, "x", 2, 5, &mut sink);
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
        s.apply(1, "â", 1, 0, &mut sink);
        assert_eq!(s.shadow.text(), "abâ");
    }

    // ── Tier 3 — UInput ───────────────────────────────────────────────────────

    #[test]
    fn tier3_no_mods_single_backspace() {
        let mut s = Strategy::new(BackspaceMethod::UInput);
        s.set_modifiers(ModifierState::empty());
        s.shadow.append("a");
        let mut sink = MockSink::default();
        s.apply(1, "â", 1, 0, &mut sink);
        assert_eq!(sink.calls, vec![
            // No mod release (no mods held)
            Call::UinputKey(14, 1), // BS press
            Call::UinputKey(14, 0), // BS release
            // No mod restore
            Call::CommitString("â".to_owned()),
            Call::Commit(1),
        ]);
    }

    #[test]
    fn tier3_shift_held_single_backspace() {
        // Shift held: release Shift, BS, restore Shift
        let mut s = Strategy::new(BackspaceMethod::UInput);
        s.set_modifiers(ModifierState::SHIFT);
        s.shadow.append("a");
        let mut sink = MockSink::default();
        s.apply(1, "â", 1, 0, &mut sink);
        assert_eq!(sink.calls, vec![
            Call::UinputKey(42, 0), // LEFTSHIFT release
            Call::UinputKey(14, 1), // BS press
            Call::UinputKey(14, 0), // BS release
            Call::UinputKey(42, 1), // LEFTSHIFT restore
            Call::CommitString("â".to_owned()),
            Call::Commit(1),
        ]);
    }

    #[test]
    fn tier3_ctrl_shift_held() {
        let mut s = Strategy::new(BackspaceMethod::UInput);
        s.set_modifiers(ModifierState::SHIFT | ModifierState::CTRL);
        s.shadow.append("a");
        let mut sink = MockSink::default();
        s.apply(1, "x", 1, 0, &mut sink);
        let calls = &sink.calls;
        // Release phase: SHIFT(42)=0, CTRL(29)=0 — order by ModifierState::all_bits()
        assert!(calls.contains(&Call::UinputKey(42, 0)));
        assert!(calls.contains(&Call::UinputKey(29, 0)));
        // BS
        assert!(calls.contains(&Call::UinputKey(14, 1)));
        assert!(calls.contains(&Call::UinputKey(14, 0)));
        // Restore phase: SHIFT(42)=1, CTRL(29)=1
        assert!(calls.contains(&Call::UinputKey(42, 1)));
        assert!(calls.contains(&Call::UinputKey(29, 1)));
        // Commit at the end
        assert_eq!(*calls.last().unwrap(), Call::Commit(1));
    }

    #[test]
    fn tier3_two_backspaces_mods_touched_once() {
        // Modifier release/restore wraps ALL backspaces — not once per BS.
        let mut s = Strategy::new(BackspaceMethod::UInput);
        s.set_modifiers(ModifierState::SHIFT);
        s.shadow.append("ab");
        let mut sink = MockSink::default();
        s.apply(2, "x", 1, 0, &mut sink);
        // Count UinputKey(42, ...) calls — should be exactly 2 (1 release + 1 restore)
        let shift_calls: Vec<_> = sink.calls.iter()
            .filter(|c| matches!(c, Call::UinputKey(42, _)))
            .collect();
        assert_eq!(shift_calls.len(), 2);
        assert_eq!(shift_calls[0], &Call::UinputKey(42, 0)); // release
        assert_eq!(shift_calls[1], &Call::UinputKey(42, 1)); // restore
        // BS×2 = 4 events
        let bs_calls: Vec<_> = sink.calls.iter()
            .filter(|c| matches!(c, Call::UinputKey(14, _)))
            .collect();
        assert_eq!(bs_calls.len(), 4);
    }

    // ── Tier 4 — VkOnly (Path C) ──────────────────────────────────────────────

    #[test]
    fn tier4_single_backspace_and_commit() {
        let mut s = Strategy::new(BackspaceMethod::VkOnly);
        s.shadow.append("a");
        let mut sink = MockSink::default();
        s.apply(1, "â", 7, 42, &mut sink);
        assert_eq!(sink.calls, vec![
            Call::VkKey(42, 14, KeyState::Pressed),
            Call::VkKey(42, 14, KeyState::Released),
            Call::VkCommitChar(42, 'â'),
        ]);
        assert!(!sink.calls.iter().any(|c| matches!(c, Call::CommitString(_))));
        assert!(!sink.calls.iter().any(|c| matches!(c, Call::Commit(_))));
        assert_eq!(s.shadow.text(), "â");
    }

    #[test]
    fn tier4_multichar_commit_each_via_vk() {
        let mut s = Strategy::new(BackspaceMethod::VkOnly);
        s.shadow.append("ph");
        let mut sink = MockSink::default();
        s.apply(0, "ởn", 0, 5, &mut sink);
        assert_eq!(sink.calls, vec![
            Call::VkCommitChar(5, 'ở'),
            Call::VkCommitChar(5, 'n'),
        ]);
        assert_eq!(s.shadow.text(), "phởn");
    }

    #[test]
    fn tier4_three_backspaces() {
        let mut s = Strategy::new(BackspaceMethod::VkOnly);
        s.shadow.append("abc");
        let mut sink = MockSink::default();
        s.apply(3, "x", 1, 0, &mut sink);
        assert_eq!(sink.calls, vec![
            Call::VkKey(0, 14, KeyState::Pressed),
            Call::VkKey(0, 14, KeyState::Released),
            Call::VkKey(0, 14, KeyState::Pressed),
            Call::VkKey(0, 14, KeyState::Released),
            Call::VkKey(0, 14, KeyState::Pressed),
            Call::VkKey(0, 14, KeyState::Released),
            Call::VkCommitChar(0, 'x'),
        ]);
        assert_eq!(s.shadow.text(), "x");
    }

    // ── Shadow invalidation ───────────────────────────────────────────────────

    #[test]
    fn surrounding_text_syncs_shadow() {
        // Shadow is synced to compositor's text[..cursor] — that's how
        // Tier 1 gets correct byte counts for delete_surrounding_text.
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.on_surrounding_text("cha", 3);
        assert_eq!(s.shadow.text(), "cha");
    }

    #[test]
    fn tier1_uses_surrounding_text_bytes() {
        // Daemon receives surrounding_text "châ" cursor=4 from compositor.
        // Engine returns bs=1 commit="ầ". Tier 1 should delete the â
        // (2 bytes / 1 char) and commit "ầ".
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.on_surrounding_text("châ", 4); // "châ" = 4 bytes
        let mut sink = MockSink::default();
        s.apply(1, "ầ", 1, 0, &mut sink);
        assert_eq!(sink.calls[0], Call::DeleteSurroundingText(2, 1, 0, 0));
        assert_eq!(sink.calls[1], Call::CommitString("ầ".to_owned()));
    }

    #[test]
    fn reset_shadow_clears() {
        let mut s = Strategy::new(BackspaceMethod::SurroundingText);
        s.shadow.append("hello");
        s.reset_shadow();
        assert_eq!(s.shadow.text(), "");
    }
}
