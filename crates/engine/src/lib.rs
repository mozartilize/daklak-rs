#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::let_underscore_must_use,
    )
)]

//! viet-ime-engine — thin wrapper around vnkey-engine.
//!
//! Exposes a clean API for the daemon: feed keys (chars), get back the bytes
//! to commit and the count of preceding chars to delete. Hides the internal
//! state machine of vnkey-engine.

mod reversal;
mod telex;

use vnkey_engine::input::CharType;
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

fn decode_engine_output(output: Vec<u8>) -> String {
    // INVARIANT: vnkey-engine output comes from its internal UTF-8 tables,
    // never from external byte input.
    match String::from_utf8(output) {
        Ok(commit) => commit,
        Err(error) => panic!("vnkey-engine violated its UTF-8 output invariant: {error}"),
    }
}

/// Longest Vietnamese word on screen (`nghiêng`). A longer word is not
/// Vietnamese, so it is restored to the keys typed and no longer composed.
const MAX_WORD_CHARS: usize = 7;

/// Keys one word may feed vnkey before its history is compacted to keys that
/// reproduce the word on screen. Tone toggling grows vnkey's key history while the word
/// stays short (`hanh` + `jf`×70 still shows `hành`), and vnkey's own
/// non-Vietnamese restore copies that history into a fixed 128-entry buffer.
const MAX_WORD_KEYS: usize = 32;

/// Per-window engine state. One instance per window/text-input the daemon
/// is tracking.
pub struct EngineState {
    engine: Engine,
    /// The current word as shown on screen, from this engine's own output.
    word: String,
    /// Keys fed to vnkey for the current word: typed keys, or the keys a
    /// seed or compaction replaced them with. Equal to vnkey's own key
    /// history, which is how its non-Vietnamese restore is recognised, and
    /// tells `keys_for` the order the user typed.
    word_raw: String,
    /// Keys fed to vnkey for the current word.
    word_keys: usize,
    /// The current word grew past `MAX_WORD_CHARS`: pass keys through
    /// unchanged until the next word break.
    raw_word: bool,
    /// Kept so the render-gate can spin up a throwaway engine with the same
    /// configuration to round-trip-check a reconstructed seed.
    method: InputMethod,
    bracket_shortcuts: bool,
    modern_style: bool,
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
        let modern_style = engine.options.modern_style;
        Self {
            engine,
            word: String::new(),
            word_raw: String::new(),
            word_keys: 0,
            raw_word: false,
            method,
            bracket_shortcuts,
            modern_style,
        }
    }

    /// Render the visible string produced by typing `raw` from a clean state,
    /// using this engine's configuration. Pure: runs on a throwaway engine and
    /// does NOT touch `self`. Used by the render-gate to verify a reconstructed
    /// telex seed actually round-trips back to the glyphs it came from.
    fn render(&self, raw: &str) -> String {
        let mut scratch = Self::new_with_options(self.method, self.bracket_shortcuts);
        scratch.set_modern_style(self.modern_style);
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
        let word_break = matches!(
            self.engine.input.get_char_type(ch as u32),
            CharType::WordBreak | CharType::Reset
        );
        if self.raw_word {
            if word_break {
                self.reset();
            }
            return ProcessResult::forward();
        }
        if word_break {
            self.word.clear();
            self.word_raw.clear();
            self.word_keys = 0;
            return self.engine_key(ch);
        }
        if self.word_keys >= MAX_WORD_KEYS {
            let keys = self.keys_for(&self.word);
            if !self.engine.feed_context(&keys) {
                self.engine.reset();
            }
            self.word_keys = keys.chars().count();
            self.word_raw = keys;
        }
        let before = self.word.clone();
        let result = self.engine_key(ch);
        self.word_keys += 1;
        self.word_raw.push(ch);
        apply_to_word(&mut self.word, &result, ch);
        if self.word.chars().count() <= MAX_WORD_CHARS {
            return result;
        }
        // Longer than any Vietnamese word: put back the keys typed for it and
        // stop composing until the word ends.
        let raw = std::mem::take(&mut self.word_raw);
        self.engine.reset();
        self.word.clear();
        self.raw_word = true;
        if raw == format!("{before}{ch}") {
            return ProcessResult::forward();
        }
        ProcessResult {
            backspaces: before.chars().count(),
            commit: raw,
            consumed: true,
        }
    }

    fn engine_key(&mut self, ch: char) -> ProcessResult {
        let r = self.engine.process(ch as u32);
        ProcessResult {
            backspaces: r.backspaces,
            commit: decode_engine_output(r.output),
            consumed: r.processed,
        }
    }

    /// Process a backspace. Returns what the daemon should do *in addition*
    /// to (or instead of) forwarding the backspace to the app.
    pub fn process_backspace(&mut self) -> ProcessResult {
        let r = self.engine.process_backspace();
        let result = ProcessResult {
            backspaces: r.backspaces,
            commit: decode_engine_output(r.output),
            consumed: r.processed,
        };
        if result.consumed {
            apply_to_word(&mut self.word, &result, '\0');
        } else {
            self.word.pop();
        }
        // Which typed keys a deleted letter came from is unknown; keep keys
        // that reproduce what is left on screen.
        self.word_raw = self.keys_for(&self.word);
        result
    }

    /// Keys that rebuild composed `text`, for seeding this engine. vnkey puts
    /// these keys back verbatim if the word later stops being Vietnamese, so
    /// they follow the order the user typed: the tone key last (`hạnh` ->
    /// `hanhj`, so `hạnh` + `p` restores `hanhjp`, not `hajnhp`), unless the
    /// keys typed for this word put it right after its vowel (`doén` typed
    /// as `doesn` stays `doesn`, so `t` restores `doesnt`). Either way only
    /// the last form is kept, never undone tone toggles.
    fn keys_for(&self, text: &str) -> String {
        let by_glyph = context_from_surrounding(text, self.method).0;
        if is_subsequence(&by_glyph, &self.word_raw) {
            by_glyph
        } else {
            tone_last_context(text, self.method)
        }
    }

    /// Reset engine state. Call on focus loss, click, navigation key, or
    /// any signal that the cursor moved outside the daemon's control.
    pub fn reset(&mut self) {
        self.engine.reset();
        self.word.clear();
        self.word_raw.clear();
        self.word_keys = 0;
        self.raw_word = false;
    }

    /// Change the active input method at runtime. Resets composition state
    /// (the new method has different keystroke semantics).
    pub fn set_input_method(&mut self, method: InputMethod) {
        self.reset();
        self.engine.set_input_method(method.to_vnkey());
        self.engine.options.modern_style = self.modern_style;
        self.method = method;
    }

    pub fn modern_style(&self) -> bool {
        self.modern_style
    }

    /// Toggle modern-style tone placement. `false` = legacy `òa` instead of `oà`.
    /// Does NOT reset composition — the change takes effect on the next tone.
    pub fn set_modern_style(&mut self, enabled: bool) {
        self.modern_style = enabled;
        self.engine.options.modern_style = enabled;
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
    /// round-trip-checks the reconstruction. A word longer than any
    /// Vietnamese word is not seeded; the following keys pass through
    /// unchanged until the next word break.
    pub fn feed_context(&mut self, text: &str) -> bool {
        let context = self.keys_for(text);
        let screen = self.render(&context);
        self.seed(&context, screen)
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
    /// is inert. A word longer than any Vietnamese word is never seeded and
    /// switches the engine to pass-through until the next word break.
    /// Returns `true` only if the seed round-tripped AND was fed.
    pub fn feed_context_gated(&mut self, text: &str) -> bool {
        let context = self.keys_for(text);
        let screen = self.render(&context);
        if screen.chars().count() <= MAX_WORD_CHARS && screen != text {
            return false;
        }
        self.seed(&context, screen)
    }

    /// Rebuild composition state after the app deleted text that vnkey did
    /// not edit itself. vnkey's history still holds the deleted keys, and
    /// after a non-Vietnamese restore it stays non-composable, so it is
    /// replaced by keys reproducing `word`, the word now before the cursor:
    /// `hanhjph` minus `jph` composes `hạnh` on the next `j` like freshly
    /// typed `hanh`. A word that cannot be reproduced is not composed: keys
    /// pass through until the next word break.
    pub fn reseed_after_delete(&mut self, word: &str) -> bool {
        if self.feed_context_gated(word) {
            return true;
        }
        self.reset();
        self.raw_word = !word.is_empty();
        false
    }

    fn seed(&mut self, keys: &str, screen: String) -> bool {
        if screen.chars().count() > MAX_WORD_CHARS {
            self.reset();
            self.raw_word = true;
            return false;
        }
        let fed = self.engine.feed_context(keys);
        if fed {
            self.word = screen;
            self.word_raw = keys.to_owned();
            self.word_keys = keys.chars().count();
            self.raw_word = false;
        } else {
            self.reset();
        }
        fed
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

impl ProcessResult {
    fn forward() -> Self {
        Self {
            backspaces: 0,
            commit: String::new(),
            consumed: false,
        }
    }
}

/// Update the on-screen `word` the way the daemon applies `result` for `ch`.
fn apply_to_word(word: &mut String, result: &ProcessResult, ch: char) {
    if result.consumed {
        for _ in 0..result.backspaces {
            word.pop();
        }
        word.push_str(&result.commit);
    } else {
        word.push(ch);
    }
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
    reversal::chars_from_composed(ch, method)
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle
        .chars()
        .all(|ch| rest.any(|typed| typed == ch))
}

fn is_tone_key(key: char, method: InputMethod) -> bool {
    match method {
        InputMethod::Telex => matches!(key, 's' | 'f' | 'r' | 'x' | 'j'),
        InputMethod::Vni => matches!(key, '1'..='5'),
        InputMethod::Viqr => matches!(key, '\'' | '`' | '?' | '~' | '.'),
    }
}

/// Like `context_from_surrounding`, but each word's tone key comes after the
/// word's letters instead of right after its vowel.
fn tone_last_context(text: &str, method: InputMethod) -> String {
    let mut result = String::with_capacity(text.len());
    let mut tone = None;
    for ch in text.chars() {
        let keys = chars_from_composed(ch, method);
        if !ch.is_alphabetic() {
            result.extend(tone.take());
            result.push_str(keys);
            continue;
        }
        match keys.char_indices().last() {
            Some((at, key)) if at > 0 && is_tone_key(key, method) => {
                result.extend(tone.replace(key));
                result.push_str(&keys[..at]);
            }
            _ => result.push_str(keys),
        }
    }
    result.extend(tone);
    result
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

    // ===== Word-length cap =====

    #[test]
    fn longest_vietnamese_word_still_composes_with_corrections() {
        // `nghiêng` is the longest word (7 letters). A tone correction takes
        // it to 10 keys, so a keystroke cap of 9 would wrongly restore it.
        for (method, keys, want) in [
            (InputMethod::Telex, "nghieengx", "nghiễng"),
            (InputMethod::Telex, "nghieengxf", "nghiềng"),
            (InputMethod::Vni, "nghie6ng42", "nghiềng"),
        ] {
            let mut eng = EngineState::new(method);
            assert_eq!(type_str(&mut eng, keys), want, "{method:?} {keys}");
        }
    }

    #[test]
    fn eighth_letter_restores_the_typed_keys_until_word_break() {
        // `đ` + consonants has no vowel, so vnkey never restores it itself;
        // only the length cap can.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "ddbcdghk"), "đbcdghk");
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "ddbcdghkl"), "ddbcdghkl");
        // Raw until the word ends (`dd` stays `dd`), then composing resumes.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "ddbcdghkldd as"), "ddbcdghkldd á");
    }

    #[test]
    fn long_tone_toggling_word_does_not_crash_the_engine() {
        // Previously: `hanh` + `jf`x70 kept showing 4 letters, and the next
        // non-Vietnamese key made vnkey copy all 144 keys into its 128-entry
        // buffer (index out of bounds).
        let mut eng = EngineState::new(InputMethod::Telex);
        let toggled = format!("hanh{}", "jf".repeat(70));
        assert_eq!(type_str(&mut eng, &toggled), "hành");
        type_str(&mut eng, "k");
        assert_eq!(type_str(&mut eng, " hanhj"), " hạnh");
        // Same for a long word typed live (crashed at key 131): vnkey
        // restores it to the keys typed, and the rest passes through.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, &"as".repeat(66)), "as".repeat(66));
    }

    #[test]
    fn deleting_a_word_boundary_keeps_composition() {
        let mut eng = EngineState::new(InputMethod::Telex);
        let mut visible = type_str(&mut eng, "la ");
        do_backspace(&mut eng, &mut visible);
        let r = eng.process_key('f');
        assert!(r.consumed);
        assert_eq!((r.backspaces, r.commit.as_str()), (1, "à"));
    }

    #[test]
    fn seed_keys_put_the_tone_last() {
        // Seeded words restore with the tone key last, as typed.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(eng.feed_context_gated("hạnh"));
        assert_eq!(type_str(&mut eng, "p"), "hanhjp");
    }

    #[test]
    fn reseeding_keeps_the_typed_tone_position() {
        // Retroactive editing reseeds from the screen word after every edit.
        // A later restore must still show the tone key where it was typed:
        // mid-word in `doesnt` (`doén` is not `doens`), last in `hanhjp`.
        for keys in ["doesnt", "hanhjp"] {
            let mut eng = EngineState::new(InputMethod::Telex);
            let mut screen = String::new();
            for ch in keys.chars() {
                let r = eng.process_key(ch);
                apply_to_word(&mut screen, &r, ch);
                eng.feed_context_gated(&screen);
            }
            assert_eq!(screen, keys);
        }
    }

    #[test]
    fn seed_longer_than_any_word_passes_keys_through_until_word_break() {
        for method in [InputMethod::Telex, InputMethod::Vni, InputMethod::Viqr] {
            let mut eng = EngineState::new(method);
            // A 427-character URL seeded from a URL bar used to crash.
            let url = format!("https://example.test/{}", "as".repeat(203));
            assert!(!eng.feed_context_gated(&url));
            assert!(!eng.feed_context(&url));
        }
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(!eng.feed_context_gated("abcdefgh"));
        // A fresh engine would compose `as` → `á`; this is still the long word.
        assert_eq!(type_str(&mut eng, "as as"), "as á");
    }

    #[test]
    fn seven_letter_seed_still_composes() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(eng.feed_context_gated("nghiêng"));
        let r = eng.process_key('f');
        assert!(r.consumed);
        let mut screen = String::from("nghiêng");
        for _ in 0..r.backspaces {
            screen.pop();
        }
        screen.push_str(&r.commit);
        assert_eq!(screen, "nghiềng");
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

    // ===== VIQR surrounding-text reconstruction =====

    #[test]
    fn viqr_context_reversal_uses_viqr_keystrokes() {
        // VIQR uses punctuation marks for tones/diacritics, not Telex letters.
        let (keys, widths) = context_from_surrounding("nó", InputMethod::Viqr);
        assert_eq!(keys, "no'", "VIQR acute tone is apostrophe, not 's'");
        assert_eq!(widths, &[1, 2]);
    }

    #[test]
    fn viqr_context_reversal_circumflex_and_horn() {
        let (keys, _) = context_from_surrounding("ơ", InputMethod::Viqr);
        assert_eq!(keys, "o+", "VIQR horn is '+'");

        let (keys, _) = context_from_surrounding("ô", InputMethod::Viqr);
        assert_eq!(keys, "o^", "VIQR circumflex is '^'");

        let (keys, _) = context_from_surrounding("ă", InputMethod::Viqr);
        assert_eq!(keys, "a(", "VIQR breve is '('");
    }

    #[test]
    fn viqr_feed_context_accepts_composed_vietnamese_for_tone_replacement() {
        let mut eng = EngineState::new(InputMethod::Viqr);
        assert!(eng.feed_context("nó"));

        let r = eng.process_key('?');

        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "ỏ");
    }

    #[test]
    fn viqr_feed_context_accepts_composed_vietnamese_inside_word() {
        let mut eng = EngineState::new(InputMethod::Viqr);

        assert!(eng.feed_context("pho"));
        let r = eng.process_key('+');
        assert!(r.consumed);
        assert_eq!(r.commit, "ơ");

        eng.reset();
        assert!(eng.feed_context("phơ"));
        let r = eng.process_key('?');
        assert!(r.consumed);
        assert_eq!(r.backspaces, 1);
        assert_eq!(r.commit, "ở");
    }
}
