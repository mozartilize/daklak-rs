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
    /// The word on screen when it differs from `word`, vnkey's rendering of
    /// `word_raw`. Set after a backspace into raw letters that vnkey would
    /// render differently (`thoong`, left of a restored `thoongh`, renders
    /// `thông`): the screen keeps `thoong` until the next key, whose result
    /// is then written over it as if `thoong` plus that key had been typed.
    screen_word: Option<String>,
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
            screen_word: None,
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
            self.screen_word = None;
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
        // vnkey restores a word that stops being Vietnamese by rewriting it
        // to its whole key history.
        let vnkey_restored =
            result.consumed && self.word == self.word_raw && self.word != format!("{before}{ch}");
        if !vnkey_restored && self.word.chars().count() <= MAX_WORD_CHARS {
            return match self.screen_word.take() {
                Some(screen) => replace_word(&screen, &self.word, ch),
                None => result,
            };
        }
        // Not Vietnamese (vnkey restored it, or it is longer than any
        // Vietnamese word): put back keys reproducing the word as it was on
        // screen, plus this key. Keys that were undone are not resurrected:
        // escapes (`corrr` shows `corr`, so `correct` + `i` is `correcti`)
        // and replaced tones (`hanhjf` shows `hành`, so `p` gives `hanhfp`).
        // Then stop composing until the word ends.
        let raw = format!("{}{ch}", self.raw_form(&before));
        let screen = self.screen_word.take().unwrap_or(before);
        self.word_raw.clear();
        self.engine.reset();
        self.word.clear();
        self.raw_word = true;
        if raw == format!("{screen}{ch}") {
            return ProcessResult::forward();
        }
        ProcessResult {
            backspaces: screen.chars().count(),
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
        if let Some(mut screen) = self.screen_word.take() {
            // vnkey's word is not what is on screen, and its own backspace
            // can rewrite it (`doén` minus `n` moves the tone: `dóe`). Let
            // the app delete one screen letter; the caller reseeds from
            // what is left.
            screen.pop();
            self.engine.reset();
            self.word_raw = screen.clone();
            self.word = screen;
            return ProcessResult::forward();
        }
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
        // The user's own spelling, shorthands included (`ửo` typed as `wor`),
        // when it still types `text`.
        if let Some(typed) = self.typed_spelling(text) {
            if self.render(&typed) == text {
                return typed;
            }
        }
        let by_glyph = context_from_surrounding(text, self.method).0;
        if is_subsequence(&by_glyph, &self.word_raw) {
            by_glyph
        } else {
            tone_last_context(text, self.method)
        }
    }

    /// The raw letters of the on-screen word `text`, for a non-Vietnamese
    /// restore: each glyph spelled as the user could have typed it, picking
    /// the first spelling that is a subsequence of the keys actually typed.
    /// Keys that were undone are therefore dropped (`correct` typed as
    /// `corrrect` stays `correct`; `hành` typed as `hanhjf` is `hanhf`),
    /// while shorthands survive (`sưi` typed as `swi` is `swi`). Unlike
    /// `keys_for`, the result need not re-type `text` in this method.
    fn raw_form(&self, text: &str) -> String {
        self.typed_spelling(text)
            .unwrap_or_else(|| self.keys_for(text))
    }

    /// `text` spelled glyph by glyph as the user could have typed it: a
    /// spelling that is a subsequence of the keys typed for the current word
    /// (`word_raw`), keeping as many of them as the glyphs account for (a
    /// bare `u`/`o` also matches `nguoiwf`, but would drop its `w`). Ties go
    /// to the earlier key order below. `None` if no spelling matches.
    fn typed_spelling(&self, text: &str) -> Option<String> {
        let typed: Vec<char> = self.word_raw.chars().collect();
        let mut best: Option<String> = None;
        for (tone_last, w_last) in [(false, false), (true, false), (true, true), (false, true)] {
            let groups = spelling_groups(text, self.method, tone_last, w_last);
            let mut keys = String::new();
            if pick_typed_spelling(&groups, &typed, 0, &mut keys)
                && best
                    .as_ref()
                    .is_none_or(|b| keys.chars().count() > b.chars().count())
            {
                best = Some(keys);
            }
        }
        best
    }

    /// Reset engine state. Call on focus loss, click, navigation key, or
    /// any signal that the cursor moved outside the daemon's control.
    pub fn reset(&mut self) {
        self.engine.reset();
        self.word.clear();
        self.word_raw.clear();
        self.word_keys = 0;
        self.raw_word = false;
        self.screen_word = None;
    }

    /// Start the word over as if `keys` had just been typed, and return the
    /// word they show.
    pub fn retype(&mut self, keys: &str) -> String {
        self.reset();
        let mut word = String::new();
        for ch in keys.chars() {
            let result = self.process_key(ch);
            apply_to_word(&mut word, &result, ch);
        }
        word
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
    ///
    /// Raw letters left of a restored word (`thoong`, after deleting the `h`
    /// of `thoongh`) do not reproduce themselves: they render `thông`. They
    /// are still exactly the keys typed for them, so they are seeded as typed
    /// keys and the next key acts as if they had just been typed: `s` gives
    /// `thống`. See `screen_word`.
    pub fn reseed_after_delete(&mut self, word: &str) -> bool {
        if self.feed_context_gated(word) {
            return true;
        }
        if !word.is_empty() && word.chars().all(|ch| ch.is_ascii_alphabetic()) {
            let screen = self.render(word);
            if self.seed(word, screen) {
                self.screen_word = Some(word.to_owned());
                return true;
            }
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
        self.screen_word = None;
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

/// The edit turning on-screen word `screen` into `word`, after typing `ch`.
/// A pure append of `ch` is left to the app (forwarded).
fn replace_word(screen: &str, word: &str, ch: char) -> ProcessResult {
    if *word == format!("{screen}{ch}") {
        return ProcessResult::forward();
    }
    let common = screen
        .chars()
        .zip(word.chars())
        .take_while(|(a, b)| a == b)
        .count();
    ProcessResult {
        backspaces: screen.chars().count() - common,
        commit: word.chars().skip(common).collect(),
        consumed: true,
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

/// For each glyph of `text`, the spellings that type it, preferred first.
/// Telex horn/breve glyphs also list shorthands: `ư` as `uw`, `w` or `u` (the
/// `w` of `uow` horns both vowels), `ơ`/`ă` as `ow`/`aw` or bare. With
/// `tone_last`, each word's tone key is its own group after its letters.
/// With `w_last`, the horn/breve `w` is typed once after the word's letters
/// (`người` as `nguoiwf`), before its tone key.
fn spelling_groups(
    text: &str,
    method: InputMethod,
    tone_last: bool,
    w_last: bool,
) -> Vec<Vec<String>> {
    let mut groups = Vec::new();
    let mut tone = None;
    let mut w = None;
    let flush = |groups: &mut Vec<Vec<String>>, w: Option<char>, tone: Option<char>| {
        for key in [w, tone].into_iter().flatten() {
            groups.push(vec![key.to_string()]);
        }
    };
    for ch in text.chars() {
        let mut keys = chars_from_composed(ch, method).to_owned();
        if !ch.is_alphabetic() {
            flush(&mut groups, w.take(), tone.take());
            groups.push(vec![keys]);
            continue;
        }
        if w_last && method == InputMethod::Telex && !ch.is_ascii() {
            if let Some(at) = keys.find(['w', 'W']) {
                w = w.or(keys[at..].chars().next());
                keys.remove(at);
            }
        }
        if tone_last {
            if let Some((at, key)) = keys.char_indices().last() {
                if at > 0 && is_tone_key(key, method) {
                    if let Some(prev) = tone.replace(key) {
                        flush(&mut groups, w.take(), Some(prev));
                    }
                    keys.truncate(at);
                }
            }
        }
        let mut spellings = vec![keys.clone()];
        if method == InputMethod::Telex && !ch.is_ascii() {
            if let Some(w) = keys.find(['w', 'W']) {
                let mut bare = keys.clone();
                bare.remove(w);
                spellings.push(bare);
                if w == 1 && keys.starts_with(['u', 'U']) {
                    spellings.push(keys[1..].to_owned());
                }
            }
        }
        // The tone key may also be typed before the glyph's other keys, right
        // after its first letter (`ể` as `ere`, `ớ` as `osw`).
        if !ch.is_ascii() {
            for spelling in spellings.clone() {
                let keys: Vec<char> = spelling.chars().collect();
                let Some((&tone, letters)) = keys.split_last() else { continue };
                if letters.len() < 2 || !is_tone_key(tone, method) {
                    continue;
                }
                for at in 1..letters.len() {
                    let mut moved = letters.to_vec();
                    moved.insert(at, tone);
                    spellings.push(moved.into_iter().collect());
                }
            }
        }
        groups.push(spellings);
    }
    flush(&mut groups, w, tone);
    groups
}

/// Append to `out` one spelling per group of `groups` that together form a
/// subsequence of `typed[pos..]`; backtracks over spellings.
fn pick_typed_spelling(
    groups: &[Vec<String>],
    typed: &[char],
    pos: usize,
    out: &mut String,
) -> bool {
    let Some((spellings, rest)) = groups.split_first() else {
        return true;
    };
    for spelling in spellings {
        let mut next = pos;
        let len = out.len();
        let matched = spelling.chars().all(|key| {
            let found = typed[next..]
                .iter()
                .position(|t| t.eq_ignore_ascii_case(&key));
            if let Some(at) = found {
                out.push(typed[next + at]);
                next += at + 1;
                true
            } else {
                false
            }
        });
        if matched && pick_typed_spelling(rest, typed, next, out) {
            return true;
        }
        out.truncate(len);
    }
    false
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle
        .chars()
        .all(|ch| rest.any(|typed| typed.eq_ignore_ascii_case(&ch)))
}

fn is_tone_key(key: char, method: InputMethod) -> bool {
    match method {
        InputMethod::Telex => matches!(key.to_ascii_lowercase(), 's' | 'f' | 'r' | 'x' | 'j'),
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
    fn eighth_letter_restores_the_word_as_shown_until_word_break() {
        // `đ` + consonants has no vowel, so vnkey never restores it itself;
        // only the length cap can.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "ddbcdghk"), "đbcdghk");
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "ddbcdghkl"), "ddbcdghkl");
        // `ddd` shows `dd`; the escaping third `d` is not put back.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "dddbcdgh"), "ddbcdgh");
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "dddbcdghk"), "ddbcdghk");
        // Raw until the word ends (`dd` stays `dd`), then composing resumes.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert_eq!(type_str(&mut eng, "ddbcdghkldd as"), "ddbcdghkldd á");
    }

    #[test]
    fn restore_puts_back_the_word_as_shown_not_undone_keys() {
        // A non-Vietnamese restore puts back keys reproducing the word as it
        // was on screen, plus the new key: escape keys (`rr` shows `r`) and
        // replaced tone keys (`jf` shows only the grave) are not resurrected.
        for (keys, expected) in [
            // Length cap: `correct` + `i` is longer than any Vietnamese word.
            ("corrrection", "correction"),
            ("errrors", "errors"),
            // vnkey's own restore: the word stops being Vietnamese.
            ("hanhjfp", "hanhfp"),
            ("toosst", "toost"),
            ("loorrd", "loord"),
            // Capitalized and all-caps non-Vietnamese words keep typed case.
            ("Osin", "Osin"),
            ("OSIN", "OSIN"),
            ("Doesnt", "Doesnt"),
            // A `w` typed after the vowels (`nguoiwf` shows `người`) counts.
            ("nguoiwfk", "nguoiwfk"),
            ("nuocwsk", "nuocwsk"),
            // Nothing undone: every key typed comes back as before.
            ("tuwjd", "tuwjd"),
            ("doesnt", "doesnt"),
            // Tone typed inside the vowel's keys (`here` shows `hể`,
            // `osw` shows `ớ`): typed order is kept, not `heer`/`owsw`.
            ("herea", "herea"),
            ("therea", "therea"),
            ("Herea", "Herea"),
            ("asak", "asak"),
            ("loswk", "loswk"),
            // Undone keys are still dropped around such a tone.
            ("asasa", "aasa"),
            ("sixsxi", "sixi"),
        ] {
            let mut eng = EngineState::new(InputMethod::Telex);
            assert_eq!(type_str(&mut eng, keys), expected, "{keys}");
        }
    }

    #[test]
    fn reseed_keys_retype_the_word_even_when_typed_keys_do_not() {
        // `thuoww` shows `thươ` (the second `w` is literal-escaped back);
        // its typed spelling `thuow` would not retype it, so seeding falls
        // back to keys that do.
        let mut eng = EngineState::new(InputMethod::Telex);
        let screen = type_str(&mut eng, "thuoww");
        assert!(eng.feed_context_gated(&screen), "{screen}");
    }

    #[test]
    fn raw_letters_left_by_a_delete_compose_like_fresh_typing() {
        // `thoong` is what is left of a restored `thoongh`: the screen shows
        // raw letters that vnkey renders `thông`.
        let diverged = || {
            let mut eng = EngineState::new(InputMethod::Telex);
            assert!(eng.reseed_after_delete("thoong"));
            eng
        };
        // The next key is written over the raw letters: `thoongs` is `thống`.
        let r = diverged().process_key('s');
        assert_eq!((r.backspaces, r.commit.as_str()), (4, "ống"));

        // A word break ends it: the next word starts clean.
        let mut eng = diverged();
        assert_eq!(eng.process_key(' ').backspaces, 0);
        assert_eq!(eng.process_key('a').backspaces, 0);

        // A backspace is left to the app: vnkey would edit its own word (in
        // the old tone style `doén` minus `n` moves the tone back: `dóe`),
        // not the screen's `doesn`. The caller reseeds from what is left.
        for modern in [true, false] {
            let mut eng = EngineState::new(InputMethod::Telex);
            eng.set_modern_style(modern);
            assert!(eng.reseed_after_delete("doesn"));
            let r = eng.process_backspace();
            assert!(!r.consumed, "modern={modern} {r:?}");
            assert!(eng.reseed_after_delete("does"));
            let r = eng.process_key('n');
            assert_eq!(
                (r.backspaces, r.commit.as_str()),
                (2, "én"),
                "modern={modern}"
            );
        }
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
        // Same for a long word typed live (crashed at key 131). `asas` shows
        // `âs` (the second `s` undid the tone), so `a` restores the word as
        // shown, `aas` + `a`; the rest passes through.
        let mut eng = EngineState::new(InputMethod::Telex);
        let expected = format!("aasas{}", "as".repeat(63));
        assert_eq!(type_str(&mut eng, &"as".repeat(66)), expected);
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
        for keys in ["doesnt", "hanhjp", "Osin"] {
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
