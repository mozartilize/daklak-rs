//! viet-ime-engine — thin wrapper around vnkey-engine.
//!
//! Exposes a clean API for the daemon: feed keys (chars), get back the bytes
//! to commit and the count of preceding chars to delete. Hides the internal
//! state machine of vnkey-engine.

mod telex;
mod vni;

use vnkey_engine::{Engine, InputMethod as VnkeyIm};

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
    /// Kept so the render-gate can spin up a throwaway engine with the same
    /// configuration to round-trip-check a reconstructed seed.
    method: InputMethod,
    bracket_shortcuts: bool,
}

impl EngineState {
    pub fn new(method: InputMethod) -> Self {
        Self::new_with_options(method, false)
    }

    pub fn new_with_options(method: InputMethod, bracket_shortcuts: bool) -> Self {
        let mut engine = Engine::new();
        engine.set_input_method(method.to_vnkey());
        if method == InputMethod::Telex && !bracket_shortcuts {
            let key_map = telex::telex_key_map(false);
            engine.input.set_user_key_map(&key_map);
        }
        engine.set_viet_mode(true);
        Self {
            engine,
            method,
            bracket_shortcuts,
        }
    }

    /// Render the visible string produced by typing `raw` from a clean state,
    /// using this engine's configuration. Pure: runs on a throwaway engine and
    /// does NOT touch `self`. Used by the render-gate to verify a reconstructed
    /// telex seed actually round-trips back to the glyphs it came from.
    fn render(&self, raw: &str) -> String {
        let mut scratch = Self::new_with_options(self.method, self.bracket_shortcuts);
        let mut screen = String::new();
        for ch in raw.chars() {
            let r = scratch.process_key(ch);
            if r.consumed {
                for _ in 0..r.backspaces {
                    screen.pop();
                }
                screen.push_str(&r.commit);
            } else {
                screen.push(ch);
            }
        }
        screen
    }

    /// Process a single character keystroke (ASCII printable expected for
    /// composition; arbitrary chars are forwarded to vnkey-engine which
    /// will treat non-ASCII as word-break).
    pub fn process_key(&mut self, ch: char) -> ProcessResult {
        let r = self.engine.process(ch as u32);
        let commit = String::from_utf8(r.output).expect("vnkey-engine output must be valid UTF-8");
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
        let commit = String::from_utf8(r.output).expect("vnkey-engine output must be valid UTF-8");
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
    ///
    /// NOTE: this takes method keystrokes OR composed glyphs interchangeably
    /// (`context_from_surrounding` passes ASCII through and reverses
    /// diacritics for the engine's active input method), so callers seeding
    /// from raw accumulated keystrokes use it directly. Callers seeding from
    /// *composed surrounding text* should use [`feed_context_gated`], which
    /// round-trip-checks the reconstruction.
    pub fn feed_context(&mut self, text: &str) -> bool {
        let (context, _) = context_from_surrounding(text, self.method);
        self.engine.feed_context(&context)
    }

    /// Seed from **composed** surrounding text, with a render-gate.
    ///
    /// The seed is reconstructed by reversing composed glyphs back into
    /// keystrokes for the engine's active input method, which is lossy for a
    /// minority of inputs (e.g. multi-syllable runs with no space, Telex:
    /// "ơiời" → "owiowfi"). Feeding a reconstruction that does NOT render
    /// back to the original glyphs loads garbage engine state and corrupts
    /// the next keystroke, so we round-trip-check first and refuse to seed on
    /// mismatch — the engine stays untouched and the caller treats the word
    /// as foreign text (retroactive editing disabled for it). For real
    /// Vietnamese single syllables the reconstruction is faithful and the gate
    /// is inert. Returns `true` only if the seed round-tripped AND was fed.
    pub fn feed_context_gated(&mut self, text: &str) -> bool {
        let (context, _) = context_from_surrounding(text, self.method);
        if self.render(&context) != text {
            return false;
        }
        self.engine.feed_context(&context)
    }

    /// Returns true if the engine has no pending composition state — i.e.
    /// the next key starts a fresh word.
    pub fn at_word_beginning(&self) -> bool {
        self.engine.at_word_beginning()
    }
}

/// Reconstruct input-method keystrokes from composed surrounding text for the
/// Telex method. Returns the keystroke string and a per-glyph byte-width vector.
pub fn telex_context_from_surrounding(text: &str) -> (String, Vec<u8>) {
    context_from_surrounding(text, InputMethod::Telex)
}

/// Reconstruct input-method keystrokes from composed surrounding text for the
/// VNI method. Returns the keystroke string and a per-glyph byte-width vector.
pub fn vni_context_from_surrounding(text: &str) -> (String, Vec<u8>) {
    context_from_surrounding(text, InputMethod::Vni)
}

fn context_from_surrounding(text: &str, method: InputMethod) -> (String, Vec<u8>) {
    let mut result = String::with_capacity(text.len());
    let mut widths = Vec::with_capacity(text.chars().count());
    for ch in text.chars() {
        let keys = chars_from_composed(ch, method);
        if !keys.is_empty() {
            widths.push(keys.len() as u8);
            result.push_str(keys);
        }
    }
    (result, widths)
}

fn chars_from_composed(ch: char, method: InputMethod) -> &'static str {
    match method {
        InputMethod::Telex => telex::telex_chars_from_composed(ch),
        InputMethod::Vni => vni::vni_chars_from_composed(ch),
        InputMethod::Viqr => telex::telex_chars_from_composed(ch),
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
    fn telex_hieeus_is_hieu() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "hieeus"), "hiếu");
    }

    /// Pins that `feed_context` rebuilds a vowel cluster from raw ASCII: feeding
    /// the accumulated prefix before each key and then the key, a tone after a
    /// cluster transform (`ee`→ê then `s`) must still land on the cluster vowel —
    /// `hiếu`, not `hiêí`. The continuous word-start seed relies on this
    /// reconstruction being faithful.
    #[test]
    fn feed_context_per_key_preserves_vowel_cluster() {
        let mut eng = EngineState::new(InputMethod::Telex);
        let mut raw = String::new();
        let mut screen = String::new();
        for ch in "hieeus".chars() {
            let prefix = raw.clone();
            eng.reset();
            if !prefix.is_empty() {
                eng.feed_context(&prefix);
            }
            raw.push(ch);
            let r = eng.process_key(ch);
            if r.consumed {
                for _ in 0..r.backspaces {
                    screen.pop();
                }
                screen.push_str(&r.commit);
            } else {
                screen.push(ch);
            }
        }
        assert_eq!(screen, "hiếu");
    }

    #[test]
    fn feed_context_accepts_composed_vietnamese_for_tone_replacement() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(eng.feed_context("nó"));

        let r = eng.process_key('r');

        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "ỏ");
    }

    #[test]
    fn feed_context_accepts_composed_vietnamese_for_raw_restore() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(eng.feed_context("ră"));

        let r = eng.process_key('w');

        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "aw");
    }

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

    #[test]
    fn vni_a61_to_acircumflex_acute() {
        let mut eng = EngineState::new(InputMethod::Vni);
        assert_eq!(type_str(&mut eng, "a61"), "ấ");
    }

    #[test]
    fn vni_o61_to_ocircumflex_acute() {
        let mut eng = EngineState::new(InputMethod::Vni);
        assert_eq!(type_str(&mut eng, "o61"), "ố");
    }

    #[test]
    fn vni_word_tieng() {
        let mut eng = EngineState::new(InputMethod::Vni);
        assert_eq!(type_str(&mut eng, "tie61ng"), "tiếng");
    }

    // ===== VNI feed_context (retroactive editing) =====

    #[test]
    fn vni_feed_context_per_key_preserves_vowel_cluster() {
        let mut eng = EngineState::new(InputMethod::Vni);
        let mut raw = String::new();
        let mut screen = String::new();
        for ch in "hie61u".chars() {
            let prefix = raw.clone();
            eng.reset();
            if !prefix.is_empty() {
                eng.feed_context(&prefix);
            }
            raw.push(ch);
            let r = eng.process_key(ch);
            if r.consumed {
                for _ in 0..r.backspaces {
                    screen.pop();
                }
                screen.push_str(&r.commit);
            } else {
                screen.push(ch);
            }
        }
        assert_eq!(screen, "hiếu");
    }

    #[test]
    fn vni_feed_context_accepts_composed_vietnamese_for_tone_replacement() {
        let mut eng = EngineState::new(InputMethod::Vni);
        assert!(eng.feed_context("nó"));

        let r = eng.process_key('3');

        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "ỏ");
    }

    #[test]
    fn vni_feed_context_accepts_composed_vietnamese_inside_word() {
        let mut eng = EngineState::new(InputMethod::Vni);

        assert!(eng.feed_context("pho"));
        let r = eng.process_key('7');
        assert!(r.consumed);
        assert_eq!(r.commit, "ơ");

        eng.reset();
        assert!(eng.feed_context("phơ"));
        let r = eng.process_key('3');
        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "ở");
    }

    #[test]
    fn vni_feed_context_accepts_faithful_reconstruction() {
        let mut e = EngineState::new(InputMethod::Vni);
        assert!(e.feed_context_gated("tiê"));
        let r = e.process_key('1');
        assert_eq!((r.backspaces, r.commit.as_str()), (1, "ế"));
    }

    #[test]
    fn vni_feed_context_rejects_lossy_reconstruction() {
        let mut e = EngineState::new(InputMethod::Vni);
        assert!(!e.feed_context_gated("ơiời"));
        assert!(
            e.at_word_beginning(),
            "rejected seed must leave the engine untouched"
        );
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
    fn feed_context_accepts_composed_vietnamese_inside_word() {
        let mut eng = EngineState::new(InputMethod::Telex);

        assert!(eng.feed_context("pho"));
        let r = eng.process_key('w');
        assert!(r.consumed);
        assert_eq!(r.commit, "ơ");

        eng.reset();
        assert!(eng.feed_context("phơ"));
        let r = eng.process_key('r');
        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "ở");
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
        // Vietnamese phonology, and auto-restore reverts the whole
        // word when the composition becomes invalid.
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

    #[test]
    fn feed_context_accepts_faithful_reconstruction() {
        // A real single syllable reverse-telexes faithfully ("tiê" → "tiee" →
        // renders "tiê"), so the gate passes and the engine is seeded: a tone
        // key then edits the existing vowel instead of starting fresh.
        let mut e = EngineState::new(InputMethod::Telex);
        assert!(e.feed_context_gated("tiê"));
        let r = e.process_key('s');
        assert_eq!((r.backspaces, r.commit.as_str()), (1, "ế"));
    }

    #[test]
    fn feed_context_rejects_lossy_reconstruction() {
        // "ơiời" reverse-telexes to "owiowfi", which renders back to "owiowfi"
        // (NOT "ơiời") — a lossy reconstruction. The render-gate must refuse to
        // seed and leave the engine pristine, so a following key starts a fresh
        // word rather than composing against corrupt state.
        let mut e = EngineState::new(InputMethod::Telex);
        assert!(!e.feed_context_gated("ơiời"));
        assert!(
            e.at_word_beginning(),
            "rejected seed must leave the engine untouched"
        );
    }
}
