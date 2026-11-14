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

use reversal::chars_from_composed;
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

impl ProcessResult {
    fn forward() -> Self {
        Self {
            backspaces: 0,
            commit: String::new(),
            consumed: false,
        }
    }

    fn from_vnkey(result: vnkey_engine::engine::ProcessResult) -> Self {
        // INVARIANT: vnkey-engine output comes from its internal UTF-8 tables,
        // never from external byte input.
        let commit = match String::from_utf8(result.output) {
            Ok(commit) => commit,
            Err(error) => panic!("vnkey-engine violated its UTF-8 output invariant: {error}"),
        };
        Self {
            backspaces: result.backspaces,
            commit,
            consumed: result.processed,
        }
    }
}

/// Maximum Vietnamese syllable length on screen (`nghiêng`). A longer
/// composition is restored to the keys typed and no longer composed.
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
    /// Current composed word from the engine's output, not the client's
    /// surrounding text. `screen_word` records when the two differ after a seed.
    word: String,
    /// Current word's typing order, replaced by reconstructed keys on a seed
    /// or backspace. Matches vnkey's history while typing continuously, so a
    /// rewrite to these keys identifies vnkey's non-Vietnamese restore.
    word_raw: String,
    /// Keys fed since the last reset/reseed. Backspace can shorten `word_raw`
    /// without rebuilding vnkey's history, so its length is not this budget.
    word_keys: usize,
    /// Composition was restored or could not be seeded: pass keys through
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
            word: String::new(),
            word_raw: String::new(),
            word_keys: 0,
            raw_word: false,
            screen_word: None,
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
        scratch.set_modern_style(self.modern_style());
        scratch.type_keys(raw)
    }

    /// Feed `keys` one by one and return what they show.
    fn type_keys(&mut self, keys: &str) -> String {
        let mut word = String::new();
        for ch in keys.chars() {
            let result = self.process_key(ch);
            apply_to_word(&mut word, &result, ch);
        }
        word
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
            self.forget_word();
            return self.engine_key(ch);
        }
        self.compose_key(ch)
    }

    /// Add the key `ch` to the current word.
    fn compose_key(&mut self, ch: char) -> ProcessResult {
        if self.word_keys >= MAX_WORD_KEYS && !self.rebuild_key_history() {
            return ProcessResult::forward();
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
        let vietnamese = !vnkey_restored && self.word.chars().count() <= MAX_WORD_CHARS;
        match self.screen_word.take() {
            None if vietnamese => result,
            Some(screen) if vietnamese && !self.word.is_ascii() => {
                replace_word(&screen, &self.word, ch)
            }
            // Not Vietnamese (vnkey restored it, or it is longer than any
            // Vietnamese word). Or the key undid a diacritic only vnkey had
            // (`af` seeded as `à`, then `f`): the raw letters on screen must
            // not shrink, so they are kept and the key appended, like a
            // restore.
            screen => self.restore_typed_keys(screen.unwrap_or(before), ch),
        }
    }

    fn engine_key(&mut self, ch: char) -> ProcessResult {
        ProcessResult::from_vnkey(self.engine.process(ch as u32))
    }

    /// Replace vnkey's key history with keys that retype the current word.
    /// Its own history cannot be kept once it is too long (`MAX_WORD_KEYS`)
    /// or vnkey has dropped a key from it (a backspace it handled itself).
    /// A word no keys retype, such as the literal `af` left by undoing a
    /// tone, is no longer composed: keys pass through until it ends.
    fn rebuild_key_history(&mut self) -> bool {
        let keys = self.keys_for(&self.word);
        if self.render(&keys) != self.word || !self.engine.feed_context(&keys) {
            let word_left = !self.word.is_empty();
            self.reset();
            self.raw_word = word_left;
            // An empty word starts fresh; only a remaining literal word
            // needs pass-through until its next separator.
            return !word_left;
        }
        self.word_keys = keys.chars().count();
        self.word_raw = keys;
        true
    }

    /// Put back keys reproducing `screen`, the word as it was on screen, plus
    /// the key `ch` that made it stop being Vietnamese. Keys that were undone
    /// are not resurrected: escapes (`corrr` shows `corr`, so `correct` + `i`
    /// is `correcti`) and replaced tones (`hanhjf` shows `hành`, so `p` gives
    /// `hanhfp`). Then stop composing until the word ends.
    fn restore_typed_keys(&mut self, screen: String, ch: char) -> ProcessResult {
        // Spell the screen from the keys typed before this one: matching the
        // new key too lets it stand in for an earlier letter (`bòa` + `a`).
        self.word_raw.pop();
        let raw = format!("{}{ch}", self.raw_form(&screen));
        self.reset();
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
        let result = ProcessResult::from_vnkey(self.engine.process_backspace());
        if result.consumed {
            // vnkey rewrote the word itself (`hoàn` minus `n` is `hòa` in the
            // legacy style), dropping a key of its own choosing.
            apply_to_word(&mut self.word, &result, '\0');
            self.rebuild_key_history();
        } else {
            // The app deletes the letter. Which typed keys it came from is
            // unknown; keep keys that reproduce what is left on screen.
            self.word.pop();
            self.word_raw = self.keys_for(&self.word);
        }
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
        let inline = canonical_keys(text, self.method, KeyOrder::INLINE);
        if is_subsequence(&inline, &self.word_raw) {
            inline
        } else {
            canonical_keys(text, self.method, KeyOrder::TONE_LAST)
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
        // No glyph-by-glyph spelling is in typed order (`âc` typed `aca`):
        // the typed keys are, so never reorder them into canonical keys.
        self.typed_spelling(text)
            .unwrap_or_else(|| self.word_raw.clone())
    }

    /// `text` spelled glyph by glyph as the user could have typed it: a
    /// spelling that is a subsequence of the keys typed for the current word
    /// (`word_raw`), keeping as many of them as the glyphs account for (a
    /// bare `u`/`o` also matches `nguoiwf`, but would drop its `w`). Ties go
    /// to the earlier of `KeyOrder::TYPED`. `None` if no spelling matches.
    fn typed_spelling(&self, text: &str) -> Option<String> {
        let typed: Vec<char> = self.word_raw.chars().collect();
        // `max_by_key` keeps the last of equals, so walk the orders backwards.
        KeyOrder::TYPED
            .into_iter()
            .rev()
            .filter_map(|order| {
                let groups = spelling_groups(text, self.method, order);
                let mut keys = String::new();
                pick_typed_spelling(&groups, &typed, 0, &mut keys).then_some(keys)
            })
            .max_by_key(|keys| keys.chars().count())
    }

    /// Reset engine state. Call on focus loss, click, navigation key, or
    /// any signal that the cursor moved outside the daemon's control.
    pub fn reset(&mut self) {
        self.engine.reset();
        self.forget_word();
        self.raw_word = false;
    }

    /// Forget the word tracked so far; vnkey's own state is left alone.
    fn forget_word(&mut self) {
        self.word.clear();
        self.word_raw.clear();
        self.word_keys = 0;
        self.screen_word = None;
    }

    /// Start the word over as if `keys` had just been typed, and return the
    /// word they show.
    pub fn retype(&mut self, keys: &str) -> String {
        self.reset();
        self.type_keys(keys)
    }

    /// Change the active input method at runtime. Drops the word in progress
    /// (the new method gives its keys other meanings); the other options stay.
    pub fn set_input_method(&mut self, method: InputMethod) {
        let modern_style = self.modern_style();
        *self = Self::new_with_options(method, self.bracket_shortcuts);
        self.set_modern_style(modern_style);
    }

    pub fn modern_style(&self) -> bool {
        self.engine.options.modern_style
    }

    /// Toggle modern-style tone placement. `false` = legacy `òa` instead of `oà`.
    /// Does NOT reset composition — the change takes effect on the next tone.
    pub fn set_modern_style(&mut self, enabled: bool) {
        self.engine.options.modern_style = enabled;
    }

    /// Seed the engine from **composed** text before the cursor, so the next
    /// key edits that word (retroactive editing when the IME activates
    /// mid-word). Render-gated.
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
            // This fallback recovers composed Vietnamese (`thoong` -> `thông`),
            // not an escape replay that only removes literal letters (`surr`
            // -> `sur`). Such a seed would silently eat the last r on the
            // next key even though the screen still contains both r's.
            if !screen.is_ascii() && self.seed(word, screen) {
                self.screen_word = Some(word.to_owned());
                return true;
            }
        }
        self.reset();
        self.raw_word = !word.is_empty();
        false
    }

    /// Load `keys`, which show `screen`, as the current word.
    fn seed(&mut self, keys: &str, screen: String) -> bool {
        if screen.chars().count() > MAX_WORD_CHARS {
            self.reset();
            self.raw_word = true;
            return false;
        }
        if !self.engine.feed_context(keys) {
            self.reset();
            return false;
        }
        self.word = screen;
        self.word_raw = keys.to_owned();
        self.word_keys = keys.chars().count();
        self.raw_word = false;
        self.screen_word = None;
        true
    }

    /// Returns true if the engine has no pending composition state — i.e.
    /// the next key starts a fresh word.
    pub fn at_word_beginning(&self) -> bool {
        self.engine.at_word_beginning()
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

/// Where the user typed a word's tone key and, in Telex, the `w` of its horn
/// or breve: right after the vowel, or after the word's letters.
#[derive(Clone, Copy)]
struct KeyOrder {
    tone_last: bool,
    w_last: bool,
}

impl KeyOrder {
    /// Every glyph's keys together: `người` as `nguwowfi`.
    const INLINE: Self = Self {
        tone_last: false,
        w_last: false,
    };
    /// The tone key after the word's letters: `nguwowif`.
    const TONE_LAST: Self = Self {
        tone_last: true,
        w_last: false,
    };
    /// One `w` after the word's letters, then the tone key: `nguoiwf`.
    const W_AND_TONE_LAST: Self = Self {
        tone_last: true,
        w_last: true,
    };
    /// The tone key with its vowel, one `w` after the word's letters:
    /// `nguofiw`.
    const W_LAST: Self = Self {
        tone_last: false,
        w_last: true,
    };
    /// The orders a typed word is matched against, preferred first.
    const TYPED: [Self; 4] = [
        Self::INLINE,
        Self::TONE_LAST,
        Self::W_AND_TONE_LAST,
        Self::W_LAST,
    ];
}

/// The keys of one glyph: its letter, the key that marks the letter (`â` `ă`
/// `ơ` `ư` `đ`), and its tone key. `as` is a letter and a tone, `aw` a letter
/// and a mark, `aws` all three.
fn glyph_keys(ch: char, method: InputMethod) -> [Option<char>; 3] {
    let mut keys = chars_from_composed(ch, method).chars();
    let (letter, mut mark, mut tone) = (keys.next(), keys.next(), keys.next());
    if tone.is_none() && mark.is_some_and(|key| is_tone_key(key, method)) {
        tone = mark.take();
    }
    [letter, mark, tone]
}

/// The spellings that type one glyph, preferred first, given its keys
/// (see `glyph_keys`) less any typed after the word's letters.
fn glyph_spellings(
    [letter, mark, tone]: [Option<char>; 3],
    method: InputMethod,
) -> Vec<String> {
    let spell = |keys: [Option<char>; 3]| keys.into_iter().flatten().collect::<String>();
    let mut spellings = vec![spell([letter, mark, tone])];
    if is_telex_w(mark, method) {
        // Telex shorthands: one `w` horns both vowels of `uow`, leaving the
        // other bare, and a lone `w` is `ư`.
        spellings.push(spell([letter, None, tone]));
        if matches!(letter, Some('u' | 'U')) {
            spellings.push(spell([None, mark, tone]));
        }
    }
    if mark.is_some() && tone.is_some() {
        // The tone key typed before the mark (`ể` as `ere`, `ớ` as `osw`).
        spellings.push(spell([letter, tone, mark]));
    }
    spellings
}

/// Whether `mark` is the Telex `w` of a horn or breve.
fn is_telex_w(mark: Option<char>, method: InputMethod) -> bool {
    method == InputMethod::Telex && matches!(mark, Some('w' | 'W'))
}

/// For each glyph of `text`, the spellings that type it, preferred first.
/// With `order.tone_last`, each word's tone key is its own group after its
/// letters. With `order.w_last`, the Telex horn/breve `w` is typed once
/// after the word's letters (`người` as `nguoiwf`), before its tone key.
fn spelling_groups(text: &str, method: InputMethod, order: KeyOrder) -> Vec<Vec<String>> {
    let mut groups = Vec::new();
    // Keys held back until the word's letters are done.
    let mut word_w = None;
    let mut word_tone = None;
    let end_word = |groups: &mut Vec<Vec<String>>, w: Option<char>, tone: Option<char>| {
        for key in [w, tone].into_iter().flatten() {
            groups.push(vec![key.to_string()]);
        }
    };
    for ch in text.chars() {
        if !ch.is_alphabetic() {
            end_word(&mut groups, word_w.take(), word_tone.take());
            groups.push(vec![chars_from_composed(ch, method).to_owned()]);
            continue;
        }
        let [letter, mut mark, mut tone] = glyph_keys(ch, method);
        if order.w_last && is_telex_w(mark, method) {
            word_w = word_w.or(mark.take());
        }
        if order.tone_last {
            if let Some(key) = tone.take() {
                // A second tone means a second syllable with no break.
                if let Some(earlier) = word_tone.replace(key) {
                    end_word(&mut groups, word_w.take(), Some(earlier));
                }
            }
        }
        groups.push(glyph_spellings([letter, mark, tone], method));
    }
    end_word(&mut groups, word_w, word_tone);
    groups
}

/// The keys that type `text` with every glyph spelled out in full.
fn canonical_keys(text: &str, method: InputMethod, order: KeyOrder) -> String {
    spelling_groups(text, method, order)
        .iter()
        .filter_map(|spellings| spellings.first())
        .map(String::as_str)
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Daemon-side simulator: apply engine output the way a real daemon would.
    /// If consumed, delete `backspaces` chars and insert `commit`. If not
    /// consumed, the daemon forwards the original key to the app — here we
    /// model that by appending the raw char.
    fn type_str(eng: &mut EngineState, s: &str) -> String {
        let mut screen = String::new();
        for ch in s.chars() {
            let result = eng.process_key(ch);
            if result.consumed {
                for _ in 0..result.backspaces {
                    screen.pop();
                }
                screen.push_str(&result.commit);
            } else {
                screen.push(ch);
            }
        }
        screen
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
        // `nghiêng` has the maximum syllable length (7 letters). A tone correction takes
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
            // Also with the horn `w` in the vowels' keys, and when one `w`
            // horned both vowels.
            ("muwowfsib", "muwowsib"),
            ("nguowifsk", "nguowisk"),
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
    fn reseed_after_delete_does_not_replay_literal_escapes() {
        for word in ["surr", "Surr", "SURR"] {
            let mut eng = EngineState::new(InputMethod::Telex);
            assert!(!eng.reseed_after_delete(word));
            for ch in "os".chars() {
                assert!(!eng.process_key(ch).consumed, "{word} + {ch}");
            }
            // Only this unreproducible raw word is passed through.
            eng.process_key(' ');
            assert_eq!(type_str(&mut eng, "as"), "á");
        }
    }

    #[test]
    fn restoring_english_keeps_typed_letter_order() {
        // Each word restores to exactly its typed keys. Before the fix the
        // glyph-order spelling reordered them (`academy` -> `aacdemy`: the
        // `â` of `aca` was spelled `aa`), and the restored key itself
        // stood in for an earlier letter (legacy `boà`/`bòa` + `a` ->
        // `bofaa`).
        for word in ["academy", "acacia", "airing", "boafa"] {
            for modern in [true, false] {
                let mut eng = EngineState::new(InputMethod::Telex);
                eng.set_modern_style(modern);
                assert_eq!(type_str(&mut eng, word), word, "modern={modern}");
            }
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
        // Compacting the key history must not disturb the word: a repeated
        // tone key still undoes the tone, and the word is then literal.
        // Whichever key the compaction falls on, the undo included.
        for toggles in [13, 14, 15, 16, 17, 20] {
            let mut eng = EngineState::new(InputMethod::Telex);
            let toggled = format!("a{}", "sf".repeat(toggles));
            assert_eq!(type_str(&mut eng, &toggled), "à", "{toggles}");
            assert_eq!(type_str(&mut eng, "fs"), "afs", "{toggles}");
        }
    }

    #[test]
    fn deleting_last_glyph_at_compaction_boundary_starts_fresh() {
        // 32 keys show one glyph. Deleting it must start a new composition,
        // not forward the next `w` merely because the old history was full.
        let mut eng = EngineState::new(InputMethod::Telex);
        let mut screen = type_str(&mut eng, &format!("a{}s", "sf".repeat(15)));
        assert_eq!(screen, "á");
        do_backspace(&mut eng, &mut screen);
        assert_eq!(screen, "");
        assert_eq!(type_str(&mut eng, "w"), "ư");
    }

    #[test]
    fn backspace_that_moves_the_tone_is_tracked() {
        // Legacy placement: `hoàn` minus `n` is `hòa`, an edit vnkey makes
        // itself. The word is then `hòa`: a restore must not bring `n` back.
        let mut eng = EngineState::new(InputMethod::Telex);
        eng.set_modern_style(false);
        let mut buf = type_str(&mut eng, "hoanf");
        assert_eq!(buf, "hoàn");
        do_backspace(&mut eng, &mut buf);
        assert_eq!(buf, "hòa");
        let r = eng.process_key('k');
        apply_to_word(&mut buf, &r, 'k');
        assert_eq!(buf, "hoafk");
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
    fn seed_follows_the_tone_placement_style() {
        // `hoà` is the modern placement and `hòa` the legacy one. Each seeds
        // only in its own style, and the next tone lands where that style
        // puts it.
        for (modern, seed, other, toned) in
            [(true, "hoà", "hòa", "hoá"), (false, "hòa", "hoà", "hóa")]
        {
            let mut eng = EngineState::new(InputMethod::Telex);
            eng.set_modern_style(modern);
            assert!(!eng.feed_context_gated(other), "modern={modern}");
            assert!(eng.feed_context_gated(seed), "modern={modern}");
            let mut screen = seed.to_owned();
            let r = eng.process_key('s');
            apply_to_word(&mut screen, &r, 's');
            assert_eq!(screen, toned, "modern={modern}");
        }
    }

    #[test]
    fn seed_spells_marked_and_toned_vowels() {
        // `ờ` takes three keys (`owf`); the tone key goes last in the word.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(eng.feed_context_gated("người"));
        assert_eq!(type_str(&mut eng, "k"), "nguwowifk");
    }

    #[test]
    fn new_seed_replaces_raw_letters_left_by_a_delete() {
        let mut eng = EngineState::new(InputMethod::Telex);
        // `thoong` stays on screen as raw letters...
        assert!(eng.reseed_after_delete("thoong"));
        // ...until the caret lands on another word, which is then the one
        // the next key edits.
        assert!(eng.feed_context_gated("hanh"));
        let r = eng.process_key('j');
        assert_eq!((r.backspaces, r.commit.as_str()), (3, "ạnh"));
    }

    #[test]
    fn reseed_after_delete_without_a_composable_word_stops_composing() {
        // Nothing left of the word: start over.
        let mut eng = EngineState::new(InputMethod::Telex);
        type_str(&mut eng, "ab");
        assert!(!eng.reseed_after_delete(""));
        assert!(eng.at_word_beginning());
        assert_eq!(type_str(&mut eng, "as"), "á");
        // A literal `a1` (VNI renders those keys `á`) is not Vietnamese
        // left over from a restore: keys pass through until the word ends.
        let mut eng = EngineState::new(InputMethod::Vni);
        assert!(!eng.reseed_after_delete("a1"));
        assert_eq!(type_str(&mut eng, "2 a2"), "2 à");
    }

    #[test]
    fn seed_longer_than_any_word_passes_keys_through_until_word_break() {
        for method in [InputMethod::Telex, InputMethod::Vni, InputMethod::Viqr] {
            let mut eng = EngineState::new(method);
            // A 427-character URL seeded from a URL bar used to crash.
            let url = format!("https://example.test/{}", "as".repeat(203));
            assert!(!eng.feed_context_gated(&url));
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

    // ===== Typing =====

    #[test]
    fn keys_compose_vietnamese() {
        use InputMethod::{Telex, Viqr, Vni};
        for (method, keys, want) in [
            (Telex, "aa", "â"),
            (Telex, "aw", "ă"),
            (Telex, "ee", "ê"),
            (Telex, "oo", "ô"),
            (Telex, "dd", "đ"),
            (Telex, "as", "á"),
            (Telex, "af", "à"),
            (Telex, "ar", "ả"),
            (Telex, "ax", "ã"),
            (Telex, "aj", "ạ"),
            (Telex, "aas", "ấ"),
            // `o` → o, `oo` → ô, then `w` reinterprets to ơ.
            (Telex, "oow", "ơ"),
            // One `w` horns both vowels.
            (Telex, "uow", "ươ"),
            (Telex, "hieeus", "hiếu"),
            (Telex, "tieengs", "tiếng"),
            // The tone key may come before or after the final consonant.
            (Telex, "vieejt", "việt"),
            (Telex, "vieetj", "việt"),
            (Vni, "a1", "á"),
            (Vni, "o6", "ô"),
            (Vni, "u7", "ư"),
            (Vni, "a61", "ấ"),
            (Vni, "o61", "ố"),
            (Vni, "tie61ng", "tiếng"),
            // Telex keys mean nothing in VNI.
            (Vni, "as", "as"),
            (Viqr, "a'", "á"),
            (Viqr, "o+", "ơ"),
            (Viqr, "dda~", "đã"),
            (Viqr, "vie^.t", "việt"),
        ] {
            let mut eng = EngineState::new(method);
            assert_eq!(type_str(&mut eng, keys), want, "{method:?} {keys}");
        }
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
    fn telex_backspace_deletes_the_whole_glyph() {
        // `aa` shows one glyph, `â`; a backspace removes it, not one key.
        let mut eng = EngineState::new(InputMethod::Telex);
        let mut buf = type_str(&mut eng, "aa");
        assert_eq!(buf, "â");
        do_backspace(&mut eng, &mut buf);
        assert_eq!(buf, "");
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

    // ===== Switching input method =====

    #[test]
    fn switching_method_changes_what_keys_mean() {
        let mut eng = EngineState::new(InputMethod::Telex);
        // A word in progress is dropped: the `a` typed as Telex must not take
        // the VNI tone.
        let _ = eng.process_key('a');
        eng.set_input_method(InputMethod::Vni);
        assert!(eng.at_word_beginning());
        assert_eq!(type_str(&mut eng, "1"), "1");
        assert_eq!(type_str(&mut eng, " a1 as"), " á as");
        // Seeds are spelled in the new method's keys too.
        assert!(eng.feed_context_gated("nó"));
        let r = eng.process_key('3');
        assert_eq!((r.backspaces, r.commit.as_str()), (1, "ỏ"));
    }

    #[test]
    fn switching_method_keeps_the_other_options() {
        for brackets in [false, true] {
            for (modern, expected) in [(false, " hòa"), (true, " hoà")] {
                let mut eng = EngineState::new_with_options(InputMethod::Telex, brackets);
                eng.set_modern_style(modern);
                eng.set_input_method(InputMethod::Vni);
                eng.set_input_method(InputMethod::Telex);
                assert_eq!(type_str(&mut eng, "["), if brackets { "ơ" } else { "[" });
                assert_eq!(type_str(&mut eng, " hoaf"), expected);
            }
        }
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

    // ===== Seeding from composed text (retroactive editing) =====

    #[test]
    fn seeded_word_is_edited_by_the_next_key() {
        use InputMethod::{Telex, Viqr, Vni};
        // The key rewrites the last glyph of the seeded word: one backspace,
        // then the new glyph.
        for (method, seed, key, commit) in [
            // A tone replaces the tone already there.
            (Telex, "nó", 'r', "ỏ"),
            (Vni, "nó", '3', "ỏ"),
            (Viqr, "nó", '?', "ỏ"),
            // A mark lands on the bare vowel, then a tone on the marked one.
            (Telex, "pho", 'w', "ơ"),
            (Vni, "pho", '7', "ơ"),
            (Viqr, "pho", '+', "ơ"),
            (Telex, "phơ", 'r', "ở"),
            (Vni, "phơ", '3', "ở"),
            (Viqr, "phơ", '?', "ở"),
            // The tone goes on the cluster's marked vowel.
            (Telex, "tiê", 's', "ế"),
            (Vni, "tiê", '1', "ế"),
            // A second `w` undoes the breve and stays as a letter.
            (Telex, "ră", 'w', "aw"),
        ] {
            let mut eng = EngineState::new(method);
            assert!(eng.feed_context_gated(seed), "{method:?} {seed}");
            let r = eng.process_key(key);
            assert!(r.consumed, "{method:?} {seed} + {key}");
            assert_eq!(
                (r.backspaces, r.commit.as_str()),
                (1, commit),
                "{method:?} {seed} + {key}"
            );
        }
    }

    #[test]
    fn seeded_word_keeps_composing_key_after_key() {
        // Existing text "phow", cursor after 'o'. Seed with "pho", then type
        // Telex keys: each returns the patch for the glyph before the cursor,
        // so the 'w' that sits after the cursor is preserved.
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(eng.feed_context_gated("pho"));

        let r = eng.process_key('w');
        assert!(r.consumed);
        assert_eq!((r.backspaces, r.commit.as_str()), (1, "ơ"));

        let r = eng.process_key('r');
        assert!(r.consumed);
        assert_eq!((r.backspaces, r.commit.as_str()), (1, "ở"));
    }

    #[test]
    fn seed_that_does_not_round_trip_is_refused() {
        // "ơiời" reverse-telexes to "owiowfi", which renders back to "owiowfi"
        // (NOT "ơiời") — a lossy reconstruction. The render-gate must refuse to
        // seed and leave the engine pristine, so a following key starts a fresh
        // word rather than composing against corrupt state.
        for method in [InputMethod::Telex, InputMethod::Vni] {
            let mut e = EngineState::new(method);
            assert!(!e.feed_context_gated("ơiời"), "{method:?}");
            assert!(
                e.at_word_beginning(),
                "{method:?}: rejected seed must leave the engine untouched"
            );
        }
    }

    #[test]
    fn empty_seed_is_refused() {
        let mut eng = EngineState::new(InputMethod::Telex);
        assert!(!eng.feed_context_gated(""));
        // It still ends the word in progress: nothing is before the caret.
        type_str(&mut eng, "a");
        assert!(!eng.feed_context_gated(""));
        assert!(eng.at_word_beginning());
    }

    #[test]
    fn telex_work_space_push() {
        // vnkey accepts the intermediate spelling `púh` rather than restoring
        // `push`. This records its behavior, not a valid Vietnamese syllable:
        // bare `h` is not a Vietnamese final consonant.
        let mut eng = EngineState::new(InputMethod::Telex);
        let mut buf = type_str(&mut eng, "work");
        assert_eq!(buf, "work", "auto-restore should revert 'work'");
        buf = type_str(&mut eng, " push");
        assert_eq!(buf, " púh", "'push' in Telex = 'púh' — s is Tone1");
    }
}
