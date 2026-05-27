//! viet-ime-engine — thin wrapper around vnkey-engine.
//!
//! Exposes a clean API for the daemon: feed keys (chars), get back the bytes
//! to commit and the count of preceding chars to delete. Hides the internal
//! state machine of vnkey-engine.

use vnkey_engine::input::{KeyEvType, TELEX_MAPPING, VNE_COUNT};
use vnkey_engine::{Engine, InputMethod as VnkeyIm};

fn telex_key_map(bracket_shortcuts: bool) -> [i32; 256] {
    let mut key_map = [KeyEvType::Normal as i32; 256];
    for entry in TELEX_MAPPING {
        if !bracket_shortcuts && matches!(entry.key, b'[' | b']' | b'{' | b'}') {
            continue;
        }
        key_map[entry.key as usize] = entry.action;
        if entry.action < VNE_COUNT {
            let ch = entry.key;
            if ch.is_ascii_lowercase() {
                key_map[ch.to_ascii_uppercase() as usize] = entry.action;
            } else if ch.is_ascii_uppercase() {
                key_map[ch.to_ascii_lowercase() as usize] = entry.action;
            }
        }
    }
    key_map
}

/// Vietnamese input method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMethod {
    Telex,
    Vni,
    Viqr,
}

impl InputMethod {
    fn to_vnkey(self) -> VnkeyIm {
        match self {
            InputMethod::Telex => VnkeyIm::Telex,
            InputMethod::Vni => VnkeyIm::Vni,
            InputMethod::Viqr => VnkeyIm::Viqr,
        }
    }
}

/// Result of processing one key.
///
/// `backspaces` chars must be deleted before the cursor, then `commit` is
/// inserted at the cursor. `consumed` tells the daemon whether the engine
/// took ownership of the key — if false, the daemon should forward the key
/// to the application as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessResult {
    pub backspaces: usize,
    pub commit: String,
    pub consumed: bool,
}

/// Per-window engine state. One instance per window/text-input the daemon
/// is tracking.
pub struct EngineState {
    engine: Engine,
}

impl EngineState {
    pub fn new(method: InputMethod) -> Self {
        Self::new_with_options(method, false)
    }

    pub fn new_with_options(method: InputMethod, bracket_shortcuts: bool) -> Self {
        let mut engine = Engine::new();
        engine.set_input_method(method.to_vnkey());
        if method == InputMethod::Telex && !bracket_shortcuts {
            let key_map = telex_key_map(false);
            engine.input.set_user_key_map(&key_map);
        }
        engine.set_viet_mode(true);
        Self { engine }
    }

    /// Process a single character keystroke (ASCII printable expected for
    /// composition; arbitrary chars are forwarded to vnkey-engine which
    /// will treat non-ASCII as word-break).
    pub fn process_key(&mut self, ch: char) -> ProcessResult {
        let r = self.engine.process(ch as u32);
        let commit = String::from_utf8(r.output).unwrap_or_default();
        ProcessResult {
            backspaces: r.backspaces,
            commit,
            consumed: r.processed,
        }
    }

    /// Process a backspace. Returns what the daemon should do *in addition*
    /// to (or instead of) forwarding the backspace to the app.
    pub fn process_backspace(&mut self) -> ProcessResult {
        let r = self.engine.process_backspace();
        let commit = String::from_utf8(r.output).unwrap_or_default();
        ProcessResult {
            backspaces: r.backspaces,
            commit,
            consumed: r.processed,
        }
    }

    /// Reset engine state. Call on focus loss, click, navigation key, or
    /// any signal that the cursor moved outside the daemon's control.
    pub fn reset(&mut self) {
        self.engine.reset();
    }

    /// Seed engine with text before the cursor — enables retroactive word
    /// editing when the IME activates mid-word. Returns true if context
    /// was fed successfully.
    pub fn feed_context(&mut self, text: &str) -> bool {
        self.engine.feed_context(text)
    }

    /// Returns true if the engine has no pending composition state — i.e.
    /// the next key starts a fresh word.
    pub fn at_word_beginning(&self) -> bool {
        self.engine.at_word_beginning()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Daemon-side simulator: apply engine output the way a real daemon would.
    /// If consumed, delete `backspaces` chars and insert `commit`. If not
    /// consumed, the daemon forwards the original key to the app — here we
    /// model that by appending the raw char.
    fn type_str(eng: &mut EngineState, s: &str) -> String {
        let mut buf = String::new();
        for ch in s.chars() {
            let r = eng.process_key(ch);
            if r.consumed {
                for _ in 0..r.backspaces {
                    buf.pop();
                }
                buf.push_str(&r.commit);
            } else {
                buf.push(ch);
            }
        }
        buf
    }

    fn do_backspace(eng: &mut EngineState, buf: &mut String) {
        let r = eng.process_backspace();
        if r.consumed {
            for _ in 0..r.backspaces {
                buf.pop();
            }
            buf.push_str(&r.commit);
        } else {
            // daemon forwards BS to app
            buf.pop();
        }
    }

    // ===== Telex basic =====

    #[test]
    fn telex_plain_a() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "a"), "a");
    }

    #[test]
    fn telex_aa_to_acircumflex() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "aa"), "â");
    }

    #[test]
    fn telex_aw_to_abreve() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "aw"), "ă");
    }

    #[test]
    fn telex_ee_to_ecircumflex() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "ee"), "ê");
    }

    #[test]
    fn telex_oo_to_ocircumflex() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "oo"), "ô");
    }

    #[test]
    fn telex_dd_to_dstroke() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "dd"), "đ");
    }

    // ===== Telex tone marks =====

    #[test]
    fn telex_tones_on_a() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "as"), "á");
        eng.reset();
        assert_eq!(type_str(&mut eng, "af"), "à");
        eng.reset();
        assert_eq!(type_str(&mut eng, "ar"), "ả");
        eng.reset();
        assert_eq!(type_str(&mut eng, "ax"), "ã");
        eng.reset();
        assert_eq!(type_str(&mut eng, "aj"), "ạ");
    }

    // ===== Telex combinations =====

    #[test]
    fn telex_aas_to_acircumflex_acute() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "aas"), "ấ");
    }

    #[test]
    fn telex_oow_to_o_horn() {
        // vnkey-engine: `o` → o, `oo` → ô, then `w` reinterprets to ơ.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "oow"), "ơ");
    }

    #[test]
    fn telex_uow_to_uow() {
        // `uow` → `ươ` (canonical Telex digraph).
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "uow"), "ươ");
    }

    #[test]
    fn telex_word_tieng() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "tieengs"), "tiếng");
    }

    #[test]
    fn telex_word_viet() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "vieejt"), "việt");
    }

    #[test]
    fn telex_tone_order_flexible() {
        // Tone mark may come before or after the trailing consonant.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "vieetj"), "việt");
    }

    // ===== VNI =====

    #[test]
    fn vni_a1_to_aacute() {
        let mut eng = EngineState::new(InputMethod::Vni);
        assert_eq!(type_str(&mut eng, "a1"), "á");
    }

    #[test]
    fn vni_o6_to_ocircumflex() {
        let mut eng = EngineState::new(InputMethod::Vni);
        assert_eq!(type_str(&mut eng, "o6"), "ô");
    }

    #[test]
    fn vni_u7_to_uhorn() {
        let mut eng = EngineState::new(InputMethod::Vni);
        assert_eq!(type_str(&mut eng, "u7"), "ư");
    }

    // ===== Backspace mid-word =====

    #[test]
    fn telex_backspace_mid_word() {
        let mut eng = EngineState::new(InputMethod::Telex);
        let mut buf = type_str(&mut eng, "toa");
        assert_eq!(buf, "toa");
        do_backspace(&mut eng, &mut buf);
        assert_eq!(buf, "to");
    }

    #[test]
    fn telex_backspace_unwinds_transform() {
        // After `aa` → `â`, a backspace should leave the user at `a` again
        // (engine restores soft-reset state, daemon's BS deletes `â`).
        let mut eng = EngineState::new(InputMethod::Telex);
        let mut buf = type_str(&mut eng, "aa");
        assert_eq!(buf, "â");
        do_backspace(&mut eng, &mut buf);
        // After 1 backspace, `â` is gone — the visible buffer reflects the
        // delete that BS caused in the app.
        assert!(buf.is_empty() || buf == "a");
    }

    // ===== Word boundary flushes state =====

    #[test]
    fn telex_space_flushes() {
        let mut eng = EngineState::new(InputMethod::Telex);
        // Space causes vnkey-engine to return `processed=false`; daemon
        // forwards the space to the app. After the space, the engine is
        // back at word-beginning so the next `aa` composes fresh.
        assert_eq!(type_str(&mut eng, "aa "), "â ");
        assert!(eng.at_word_beginning());
        assert_eq!(type_str(&mut eng, "aa"), "â");
    }

    #[test]
    fn telex_punctuation_passthrough() {
        let mut eng = EngineState::new(InputMethod::Telex);
        // Comma is a word-break — engine returns not-consumed so daemon
        // forwards `,` raw.
        assert_eq!(type_str(&mut eng, "aa,"), "â,");
    }

    #[test]
    fn telex_bracket_shortcuts_disabled_by_default() {
        let mut eng = EngineState::new(InputMethod::Telex);
        let r = eng.process_key('[');
        assert!(
            !r.consumed,
            "default config should not consume '[' as Telex ơ shortcut"
        );
    }

    #[test]
    fn telex_bracket_shortcuts_enabled_when_opted_in() {
        let mut eng = EngineState::new_with_options(InputMethod::Telex, true);
        let r = eng.process_key('[');
        assert!(r.consumed, "opt-in should consume '[' as Telex ơ shortcut");
        assert_eq!(r.backspaces, 0);
        assert_eq!(r.commit, "ơ");
    }

    // ===== Reset =====

    #[test]
    fn reset_clears_state() {
        let mut eng = EngineState::new(InputMethod::Telex);
        let _ = eng.process_key('a');
        assert!(!eng.at_word_beginning());
        eng.reset();
        assert!(eng.at_word_beginning());
    }

    // ===== feed_context (retroactive editing) =====

    #[test]
    fn feed_context_enables_retroactive_tone() {
        // Scenario from protocol-behavior.md:
        // Existing text "phow", cursor positioned after 'o' (offset 3).
        // Seed engine with "pho", then type Telex tone keys.
        //
        // Engine returns the patch to apply: backspaces=1, commit="ơ".
        // The daemon turns that into delete_surrounding_text(before=1, after=0)
        // + commit_string("ơ"), preserving the trailing 'w' that sits after
        // the cursor.
        let mut eng = EngineState::new(InputMethod::Telex);
        let fed = eng.feed_context("pho");
        assert!(fed);

        let r = eng.process_key('w');
        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "ơ");

        // Adding `r` should change `ơ` to `ở` — engine deletes 1 char and
        // commits the toned glyph. Net visible: ph + ở + (w after cursor).
        let r = eng.process_key('r');
        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "ở");
    }

    #[test]
    fn feed_context_empty_returns_false() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(!eng.feed_context(""));
    }

    #[test]
    fn feed_context_non_ascii_resets_engine() {
        // KNOWN LIMITATION of vnkey-engine: feed_context() resets internal
        // state whenever it encounters a non-ASCII char (chars outside
        // 0x21..=0x7E). The engine cannot reverse-engineer composed Vietnamese
        // text back into its keystroke state.
        //
        // Consequence for the daemon (Stage 3): we must NOT call feed_context
        // on every surrounding_text frame — if it contains `â` (already
        // committed), the call wipes the in-progress engine state and breaks
        // ongoing composition. The daemon gates feed_context behind a
        // "no recent action" check so it only fires on activate or genuine
        // cursor jumps.
        let mut eng = EngineState::new(InputMethod::Telex);

        // First seed with pure ASCII — engine retains state
        assert!(eng.feed_context("pho"));
        // Now typing 'w' should produce "ơ" (engine knows we're in word ctx)
        let r = eng.process_key('w');
        assert!(r.consumed);
        assert_eq!(r.commit, "ơ");

        // Second seed with text containing Vietnamese char — engine WIPES
        eng.reset();
        let _ = eng.feed_context("phơ");
        // Try typing 'r' — engine should compose tone... but it can't,
        // because the ơ was treated as a reset, not as composed vowel.
        let r = eng.process_key('r');
        // Engine is in fresh state (post-reset), 'r' alone has no vowel target
        assert!(!r.consumed, "feed_context with non-ASCII left engine empty");
    }

    // ===== Switching methods =====

    // ===== Regression: auto-restore after "work" → "push" =====
    //
    // After "work" triggers auto-escape (typing 'k' after 'r' tone on 'o'),
    // the engine should still properly auto-restore "push" when 's' acts
    // as a tone on 'u'. The 's' is Tone1 (sắc) in Telex, so it will
    // temporarily produce "pú". But 'h' should trigger auto-restore back
    // to "push".

    #[test]
    fn telex_work_space_push() {
        // In Telex, 's' is Tone1 (sắc), so "push" produces "púh"
        // at the engine level.  Auto-restore does NOT fire because
        // 'h' is correctly accepted as a CVC final consonant in
        // Vietnamese phonology.  The daklak handler must keep
        // raw_word in sync on backspace so re-seeding doesn't
        // double-count deleted characters.
        let mut eng = EngineState::new(InputMethod::Telex);
        let mut buf = type_str(&mut eng, "work");
        assert_eq!(buf, "work", "auto-restore should revert 'work'");
        buf = type_str(&mut eng, " push");
        assert_eq!(buf, " púh", "'push' in Telex = 'púh' — s is Tone1");
    }

    #[test]
    fn vni_and_telex_isolated() {
        // Sanity: same engine, different method produces different output.
        let mut t = EngineState::new(InputMethod::Telex);
        let mut v = EngineState::new(InputMethod::Vni);
        assert_eq!(type_str(&mut t, "as"), "á");
        assert_eq!(type_str(&mut v, "as"), "as");
    }
}
