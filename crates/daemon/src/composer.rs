//! The composition brain. Owns engine + strategy + raw_word + the
//! surrounding-text reseed gate, transport-neutral. Each transport
//! (wayland / ibus / evdev) is thin glue that translates its wire events
//! into `Composer` calls; the composition logic lives here once.
//!
//! Evolved from the old `WindowState` (state) + the composition methods that
//! used to live on `Daemon` (behavior). State and behavior now sit together.

use std::time::{Duration, Instant};

use viet_ime_edit_strategy::{BackspaceMethod, ModifierState, Strategy};
use viet_ime_engine::{EngineState, InputMethod};
use viet_ime_wayland_adapter::KeyDecision;

/// Surrounding-text cursor expressed in **bytes** (wayland text_input_v3).
pub struct ByteCursor(pub u32);
/// Surrounding-text cursor expressed in **chars** (IBus).
#[cfg(feature = "ibus")]
pub struct CharCursor(pub u32);

pub(crate) struct EditModel {
    strategy: Strategy,
    prev_text: String,
    prev_cursor: u32,
    prev_anchor: u32,
}

impl EditModel {
    fn new(method: BackspaceMethod) -> Self {
        Self {
            strategy: Strategy::new(method),
            prev_text: String::new(),
            prev_cursor: 0,
            prev_anchor: 0,
        }
    }

    fn method(&self) -> BackspaceMethod {
        self.strategy.method()
    }

    fn set_method(&mut self, m: BackspaceMethod) {
        self.strategy.set_method(m);
    }

    fn set_modifiers(&mut self, m: ModifierState) {
        self.strategy.set_modifiers(m);
    }

    fn reset(&mut self) {
        self.strategy.reset_shadow();
        self.prev_text.clear();
        self.prev_cursor = 0;
        self.prev_anchor = 0;
    }

    fn shadow_text(&self) -> &str {
        self.strategy.shadow.text()
    }

    fn push_forwarded_char(&mut self, ch: char) {
        self.strategy.shadow.text_mut().push(ch);
    }

    fn pop_forwarded_char(&mut self) {
        self.strategy.shadow.text_mut().pop();
    }

    fn on_surrounding_text(&mut self, text: &str, cursor: u32, anchor: u32) {
        self.strategy.on_surrounding_text(text, cursor, anchor);
    }

    fn record_surrounding(&mut self, text: &str, cursor: u32, anchor: u32) {
        self.prev_text = text.to_owned();
        self.prev_cursor = cursor;
        self.prev_anchor = anchor;
    }

    fn one_char_insertion_since_prev(&self, text: &str, cursor: u32) -> bool {
        detect_one_char_insertion(&self.prev_text, self.prev_cursor, text, cursor)
    }

    fn is_duplicate_frame(&self, text: &str, cursor: u32, anchor: u32) -> bool {
        text == self.prev_text && cursor == self.prev_cursor && anchor == self.prev_anchor
    }

    fn clear_prev_surrounding(&mut self) {
        self.prev_text.clear();
        self.prev_cursor = 0;
        self.prev_anchor = 0;
    }

    fn prev_surrounding_for_trace(&self) -> (&str, u32) {
        (&self.prev_text, self.prev_cursor)
    }

    fn apply<S: viet_ime_edit_strategy::OutputSink>(
        &mut self,
        backspaces: usize,
        commit: &str,
        serial: u32,
        time: u32,
        sink: &mut S,
    ) {
        self.strategy.apply(backspaces, commit, serial, time, sink);
    }

    #[cfg(test)]
    fn pop_delete_span_for_test(&mut self, backspaces: usize) -> (u32, u32, u32, u32) {
        self.strategy.shadow.pop_delete_span(backspaces)
    }
}

/// Per-text-input composition state + behavior. On wlroots/Sway, one instance
/// at a time (compositor sends deactivate before new activate); ibus/evdev
/// create one per active session.
pub struct Composer {
    pub engine: EngineState,
    edit: EditModel,
    pub method: BackspaceMethod,
    pub last_keystroke_at: Instant,

    /// Last printable user keystroke fed to `feed_key`. Tracks user
    /// intent independently of the shadow buffer — the shadow can drop
    /// just-forwarded chars when a stale surrounding_text echo arrives
    /// before the compositor commits the new state. Used to gate the
    /// word-boundary seed: skip seeding when the previous keystroke was
    /// a separator (whitespace/punct/etc.), because a new word is
    /// starting and seeding from the prior word would poison the
    /// engine state.
    pub last_input_char: Option<char>,

    /// v1/KWin path: original Telex chars typed in the current word
    /// (ASCII). Reset on word boundary. Used to seed the engine on every
    /// keystroke so multi-char tone rules see the full raw context
    /// (engine forgets internal state after returning a transform, so
    /// `tieengs`'s sắc tone only fires when fed `tieeng` not `tiêng`).
    pub raw_word: String,
    /// v1/KWin path: number of raw_word entries that produced each visible
    /// screen char in the current word. Invariant: sum(raw_word_screen_widths)
    /// == raw_word.len(). Used by feed_backspace to pop the correct number
    /// of raw keystrokes when a single screen char was produced by multiple
    /// raw chars (e.g. Telex 'u'+'s' → 'ú': width=2, so BS over 'ú' must
    /// pop both 'u' and 's' from raw_word, not just one).
    pub raw_word_screen_widths: Vec<u8>,
    raw_word_from_surrounding: bool,

    /// When true, `delete_surrounding_text` emits a CHAR count rather than the
    /// spec-compliant byte count. Set at activate when `app_id` matches
    /// `force_chars_delete_apps` (firefox by default). Other v3 clients honor
    /// bytes per spec. Independent of `debounce_barrier` (plan82 #4) — a future
    /// app may need one without the other.
    pub delete_in_chars: bool,
    /// When true, the Wayland apply loop forces a flush + 30 ms sleep after each
    /// apply so firefox's v1↔v3 bridge echoes post-commit surrounding_text and
    /// can't batch consecutive delete+commit pairs. A *timing* quirk, unrelated
    /// to the delete unit above; firefox needs both, hence they were once fused.
    pub debounce_barrier: bool,

    /// Timestamp of the last user-keystroke action — used to distinguish
    /// "compositor echo of our action" (recent) from "user clicked mid-word"
    /// (not recent) in surrounding_text frames. Moved here from `Daemon`:
    /// the reseed gate is the only reader, and it lives on `Composer` now.
    last_action_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SurroundingDecision {
    trust: bool,
    reseed: bool,
}

struct SurroundingObserver;

impl SurroundingObserver {
    fn observe(
        recent_action: bool,
        one_char_typed: bool,
        force_reseed: bool,
        has_selection: bool,
    ) -> SurroundingDecision {
        let trust = !(recent_action && !one_char_typed && !force_reseed && !has_selection);
        let reseed = force_reseed || (!one_char_typed && !recent_action);
        SurroundingDecision { trust, reseed }
    }
}

impl Composer {
    pub fn new(
        input_method: InputMethod,
        backspace_method: BackspaceMethod,
        bracket_shortcuts: bool,
    ) -> Self {
        Self {
            engine: EngineState::new_with_options(input_method, bracket_shortcuts),
            edit: EditModel::new(backspace_method),
            method: backspace_method,
            last_keystroke_at: Instant::now(),
            last_input_char: None,
            raw_word: String::new(),
            raw_word_screen_widths: Vec::new(),
            raw_word_from_surrounding: false,
            delete_in_chars: false,
            debounce_barrier: false,
            last_action_at: Instant::now() - Duration::from_secs(60),
        }
    }

    // ── action-clock (reseed echo gate) ────────────────────────────────

    /// Mark a composing user action just happened (gates the reseed echo).
    pub fn mark_action(&mut self) {
        self.last_action_at = Instant::now();
    }

    /// Roll the action clock far back so the NEXT surrounding frame bypasses
    /// the recent-action gate and re-seeds. Called on NAV / modifier-shortcut /
    /// word-boundary — the cursor is now elsewhere and the next char should
    /// compose against whatever word the cursor lands on.
    pub fn defer_action(&mut self) {
        self.last_action_at = Instant::now() - Duration::from_secs(60);
    }

    // ── config / probing ────────────────────────────────────────────────

    pub fn method(&self) -> BackspaceMethod {
        self.edit.method()
    }

    pub fn set_method(&mut self, m: BackspaceMethod) {
        self.edit.set_method(m);
        self.method = m;
    }

    pub fn set_modifiers(&mut self, m: ModifierState) {
        self.edit.set_modifiers(m);
    }

    /// Set the per-window delete/debounce quirks. They are independent (#4);
    /// firefox happens to need both, so callers may pass the same value twice.
    pub fn set_window_quirks(&mut self, delete_in_chars: bool, debounce_barrier: bool) {
        self.delete_in_chars = delete_in_chars;
        self.debounce_barrier = debounce_barrier;
    }

    // ── lifecycle ─────────────────────────────────────────────────────────

    /// Reset compose state — call on deactivate / navigation key /
    /// modifier shortcut / external cursor movement / word boundary
    /// (space/Enter/Tab). Wipes everything tracking an in-progress word:
    /// engine, shadow, raw_word, last_input_char, and the v1 surrounding-text
    /// diff bookkeeping (prev_text, prev_cursor).
    pub fn full_reset(&mut self) {
        self.engine.reset();
        self.edit.reset();
        self.last_input_char = None;
        self.raw_word.clear();
        self.raw_word_screen_widths.clear();
        self.raw_word_from_surrounding = false;
    }

    /// Check 2-second idle heuristic. Returns true (and resets engine) if
    /// the gap since last keystroke exceeds 2s — user may have clicked mouse.
    ///
    /// Resets engine only, NOT shadow: the killer-feature seed at word
    /// boundary reads from shadow to recover word context for retroactive
    /// composition. e.g. user types `la`, waits 5s, types `f` — shadow still
    /// holds "la" so engine gets seeded → `là` composes. If the cursor moved
    /// during idle, the next surrounding_text frame resyncs shadow via
    /// `observe_surrounding_*`.
    pub fn check_idle_reset(&mut self) -> bool {
        if self.last_keystroke_at.elapsed().as_secs() >= 2 {
            self.engine.reset();
            return true;
        }
        false
    }

    // ── composition ───────────────────────────────────────────────────────

    /// Feed one printable char on the **v1/raw_word path** (client already
    /// inserted the char; we observe surrounding text). This is the only path
    /// production uses — shared by KWin/v1 and IBus. The v2/wlroots key-grab
    /// path (shadow does NOT yet contain the char) is reachable via
    /// `feed_key_inner(.., false)` and is exercised only by the characterization
    /// tests.
    pub fn feed_key(&mut self, ch: char) -> KeyDecision {
        self.feed_key_inner(ch, true)
    }

    /// `shadow_already_has_ch`: `true` for the v1/KWin + IBus surrounding-text
    /// path — the client already inserted the char, so shadow reflects
    /// post-insertion text. The word-boundary seed uses raw_word instead.
    /// `false` for the v2/wlroots key-grab path — shadow does NOT contain the
    /// char yet, so the engine seeds from shadow at the word boundary.
    pub(crate) fn feed_key_inner(&mut self, ch: char, shadow_already_has_ch: bool) -> KeyDecision {
        let prev_was_separator = matches!(
            self.last_input_char,
            Some(c) if !c.is_ascii_alphabetic()
        );
        self.last_input_char = Some(ch);

        // v1/KWin path: maintain raw_word and use it as the engine seed on
        // EVERY keystroke. Engine forgets vowel-cluster context after
        // returning a transform (e.g. after `ee → ê` engine no longer
        // recognizes `iê` as a vowel cluster when 's' tone arrives later).
        // Feeding the original raw ASCII chars sidesteps that.
        if shadow_already_has_ch {
            // Word boundary: reset raw_word.
            if !ch.is_ascii_alphabetic() {
                self.raw_word.clear();
                self.raw_word_screen_widths.clear();
                self.raw_word_from_surrounding = false;
            }
            let prefix = self.raw_word.clone();
            self.engine.reset();
            if !prefix.is_empty() {
                tracing::debug!(prefix, "seed engine from raw_word (v1 path)");
                self.engine.feed_context_for_key(&prefix, ch);
            }
            // Append `ch` AFTER seeding (engine's process_key adds it). For a
            // word seeded from surrounding composed text, raw_word tracks the
            // current visible word; transforms edit that visible word below,
            // while no-edit keys append there.
            if ch.is_ascii_alphabetic() && !self.raw_word_from_surrounding {
                self.raw_word.push(ch);
                self.raw_word_from_surrounding = false;
            }
        } else {
            // v2/wlroots path: original shadow-based seed.
            if self.engine.at_word_beginning() && !prev_was_separator {
                let shadow_text = self.edit.shadow_text().to_owned();
                let raw_word = current_word_before_cursor(&shadow_text, shadow_text.len() as u32);
                if !raw_word.is_empty() && raw_word.chars().all(|c| c.is_ascii_lowercase()) {
                    tracing::debug!(word = raw_word, "seed engine from shadow at word boundary");
                    self.engine.feed_context(raw_word);
                }
            }
        }

        let r = self.engine.process_key(ch);

        self.last_keystroke_at = Instant::now();

        tracing::debug!(
            ch = %ch,
            consumed = r.consumed,
            bs = r.backspaces,
            commit = %r.commit,
            shadow = %self.edit.shadow_text(),
            "engine.process_key"
        );

        // v1/KWin path: maintain raw_word_screen_widths in sync with raw_word.
        // raw_word_screen_widths[i] = how many raw chars produced screen char i.
        // Invariant: sum(raw_word_screen_widths) == raw_word.len().
        let v1_identity_output = shadow_already_has_ch
            && r.consumed
            && r.backspaces == 0
            && r.commit.chars().eq(std::iter::once(ch));
        if shadow_already_has_ch && ch.is_ascii_alphabetic() {
            if r.consumed && !v1_identity_output && self.raw_word_from_surrounding {
                for _ in 0..r.backspaces {
                    self.raw_word.pop();
                    self.raw_word_screen_widths.pop();
                }
                self.raw_word.push_str(&r.commit);
                self.raw_word_screen_widths
                    .extend(std::iter::repeat(1u8).take(r.commit.chars().count()));
            } else if r.consumed && !v1_identity_output {
                // Engine deleted r.backspaces screen chars and emitted r.commit.
                // Pop r.backspaces widths (sum = s); the new raw chars for all
                // commit screen chars together cost s + 1 (the current ch).
                let s: usize = (0..r.backspaces)
                    .map(|_| self.raw_word_screen_widths.pop().unwrap_or(1) as usize)
                    .sum();
                let total = s + 1; // raw chars to distribute across commit chars
                let m = r.commit.chars().count().max(1);
                // Push 1 for the first m-1 commit chars; all remaining raw
                // chars go to the last one (ensures sum == total == raw_word
                // growth since last clear).
                for _ in 0..m.saturating_sub(1) {
                    self.raw_word_screen_widths.push(1);
                }
                let last_width = total.saturating_sub(m.saturating_sub(1)).max(1) as u8;
                self.raw_word_screen_widths.push(last_width);
            } else {
                if self.raw_word_from_surrounding && ch.is_ascii_alphabetic() {
                    self.raw_word.push(ch);
                    self.raw_word_screen_widths.push(1);
                }
                // ForwardRaw: one raw char → one screen char.
                if !self.raw_word_from_surrounding {
                    self.raw_word_screen_widths.push(1);
                }
            }
        }

        if r.consumed && !v1_identity_output {
            let method = self.method;
            KeyDecision::Apply {
                method,
                backspaces: r.backspaces,
                commit: r.commit,
            }
        } else {
            self.edit.push_forwarded_char(ch);
            KeyDecision::ForwardRaw
        }
    }

    pub fn feed_backspace(&mut self) -> KeyDecision {
        let r = self.engine.process_backspace();
        tracing::debug!(
            consumed = r.consumed,
            bs = r.backspaces,
            "engine.process_backspace"
        );

        // v1/KWin path: raw_word tracks raw keystrokes so backspace must
        // pop the raw entries that produced the deleted screen char.
        // raw_word_screen_widths[last] tells us how many raw chars to remove
        // (e.g. Telex 'u'+'s' produced 'ú' → width=2 → BS over 'ú' pops
        // both 's' and 'u', leaving raw_word consistent with the screen).
        let popped_width = self.raw_word_screen_widths.pop();
        {
            let width = popped_width.unwrap_or(1) as usize;
            for _ in 0..width {
                self.raw_word.pop();
            }
        }

        // If the engine restored chars (e.g. tone-undo: 'ú' → 'u', or
        // vowel-undo: 'ê' → 'ee'), push them back into raw_word so the
        // next keystroke seeds the engine with correct context.  Only do
        // this when we were actively tracking (popped_width.is_some()),
        // i.e. the v1 path — in v2 raw_word_screen_widths is always empty.
        if popped_width.is_some() && !r.commit.is_empty() {
            for ch in r.commit.chars() {
                if ch.is_ascii_alphabetic() {
                    self.raw_word.push(ch);
                    self.raw_word_screen_widths.push(1);
                }
            }
        }

        if r.consumed {
            self.last_keystroke_at = Instant::now();
            let method = self.method;
            KeyDecision::Apply {
                method,
                backspaces: r.backspaces,
                commit: r.commit,
            }
        } else {
            tracing::trace!("BS not consumed → forward");
            self.edit.pop_forwarded_char();
            self.last_keystroke_at = Instant::now();
            KeyDecision::ForwardRaw
        }
    }

    /// Apply a pending edit to an arbitrary sink. Used by transports that don't
    /// go through `AdapterCtx::with_sink` (IBus). The wayland path applies via
    /// `strategy.apply` inside `AdapterCtx::with_sink` directly.
    #[cfg(feature = "ibus")]
    pub fn apply<S: viet_ime_edit_strategy::OutputSink>(
        &mut self,
        backspaces: usize,
        commit: &str,
        time: u32,
        sink: &mut S,
    ) {
        self.edit.apply(backspaces, commit, 0, time, sink);
    }

    pub fn apply_to_sink<S: viet_ime_edit_strategy::OutputSink>(
        &mut self,
        backspaces: usize,
        commit: &str,
        serial: u32,
        time: u32,
        sink: &mut S,
    ) {
        self.edit.apply(backspaces, commit, serial, time, sink);
    }

    // ── surrounding-text / reseed gate (the ONE copy) ───────────────────────

    /// wayland text_input_v3 reports cursor in **bytes**. `force_reseed` is
    /// `true` on an activate frame (always reseed from the word at cursor).
    pub fn observe_surrounding_bytes(
        &mut self,
        text: &str,
        cursor: ByteCursor,
        anchor: ByteCursor,
        force_reseed: bool,
    ) {
        self.apply_surrounding(text, cursor.0, anchor.0, force_reseed);
    }

    /// IBus reports cursor/anchor in **chars**; everything downstream
    /// (`apply_surrounding`, `shadow.observe_surrounding`,
    /// `current_word_before_cursor`, `detect_one_char_insertion`) is
    /// byte-based, so convert at this boundary. For ASCII telex chars == bytes
    /// and this is a no-op, but once a multibyte char is committed (any
    /// Vietnamese diacritic, e.g. `í` = 2 bytes) the char offset lands
    /// mid-char when read as a byte offset, which truncated the shadow to ""
    /// and dropped the next `delete_surrounding_text` (typing `iss` → `íis`).
    /// ibus never force-reseeds (no activate frame in this path).
    #[cfg(feature = "ibus")]
    pub fn observe_surrounding_chars(
        &mut self,
        text: &str,
        cursor: CharCursor,
        anchor: CharCursor,
    ) {
        let cursor_bytes = char_to_byte_offset(text, cursor.0);
        let anchor_bytes = char_to_byte_offset(text, anchor.0);
        self.apply_surrounding(text, cursor_bytes, anchor_bytes, false);
    }

    /// The reseed gate proper — the single home of the logic that previously
    /// existed in two drifting copies (wayland `on_done_frame` and ibus
    /// `observe_surrounding`). Callers have already passed their own
    /// duplicate-frame guard and any transport-specific tier-upgrade probe.
    ///
    /// A 1-char insertion at the prior cursor is an ordinary keystroke — the
    /// engine's running composition already tracks it. Re-seeding here would
    /// clobber transient Telex state (double-letter ee→ê, a pending tone on
    /// the current syllable), which is exactly what broke "hieeus" and
    /// "phucs". So only re-seed on a genuine cursor jump (click elsewhere /
    /// focus into existing text) or an explicit `force_reseed`, never on
    /// mid-word typing or within 150 ms of our own action (echo).
    fn apply_surrounding(&mut self, text: &str, cursor: u32, anchor: u32, force_reseed: bool) {
        let recent_action = self.last_action_at.elapsed() < Duration::from_millis(150);
        let one_char_typed = self.edit.one_char_insertion_since_prev(text, cursor);
        // A frame with an active selection (anchor ≠ cursor) carries the
        // Chromium autocomplete state the Tier-1 selection fallback depends on
        // (see surrounding::apply). VSCode's stale echoes are plain duplicated
        // text with the cursor collapsed to the start (anchor == cursor), so a
        // selection distinguishes the frame we must record from the garbage we
        // must drop. Let selection frames through — `should_reseed` stays false
        // under `recent_action`, so we update the shadow without reseeding the
        // engine and clobbering Telex state.
        let has_selection = cursor != anchor;
        let decision = SurroundingObserver::observe(
            recent_action,
            one_char_typed,
            force_reseed,
            has_selection,
        );
        if !decision.trust {
            tracing::trace!(text, cursor, anchor, "skip recent surrounding_text echo");
            return;
        }

        self.edit.on_surrounding_text(text, cursor, anchor);

        if decision.reseed {
            let word = current_word_before_insertion_point(text, cursor, anchor);
            self.engine.reset();
            if !word.is_empty() && self.engine.feed_context(word) {
                tracing::debug!(word, "re-seed engine (activate or cursor jump)");
                self.raw_word_screen_widths = vec![1u8; word.chars().count()];
                self.raw_word = word.to_owned();
                self.raw_word_from_surrounding = true;
            } else {
                self.raw_word.clear();
                self.raw_word_screen_widths.clear();
                self.raw_word_from_surrounding = false;
            }
        }

        self.edit.record_surrounding(text, cursor, anchor);
    }

    /// True if (text, cursor, anchor) exactly matches the last frame — clients
    /// re-emit identical surrounding text; re-running the reseed on an
    /// unchanged frame burns engine state. Transport glue checks this before
    /// `observe_surrounding_*`.
    pub fn is_duplicate_frame(&self, text: &str, cursor: u32, anchor: u32) -> bool {
        self.edit.is_duplicate_frame(text, cursor, anchor)
    }

    /// Clear the surrounding-text diff bookkeeping (no surrounding text in
    /// this frame). Wayland-only: the v3 frame had no surrounding_text.
    pub fn clear_prev_surrounding(&mut self) {
        self.edit.clear_prev_surrounding();
    }

    pub fn prev_surrounding_for_trace(&self) -> (&str, u32) {
        self.edit.prev_surrounding_for_trace()
    }

    #[cfg(test)]
    fn shadow_text(&self) -> &str {
        self.edit.shadow_text()
    }

    #[cfg(test)]
    fn pop_shadow_delete_span_for_test(&mut self, backspaces: usize) -> (u32, u32, u32, u32) {
        self.edit.pop_delete_span_for_test(backspaces)
    }
}

// ── pure helpers the reseed gate depends on (with their unit tests) ─────────

pub fn current_word_before_insertion_point(text: &str, cursor: u32, anchor: u32) -> &str {
    current_word_before_cursor(text, cursor.min(anchor))
}

/// Extract just the word immediately before the cursor (scan back to last
/// whitespace). For retroactive editing, the engine only needs the current
/// word's context — not the entire document.
/// Convert a char offset into a byte offset within `text`. An offset equal to
/// (or past) the char count maps to `text.len()` — the cursor sits at the end.
/// IBus surrounding-text offsets are char-based; the rest of the daemon is
/// byte-based, so this is applied at the IBus boundary.
#[cfg(feature = "ibus")]
fn char_to_byte_offset(text: &str, char_idx: u32) -> u32 {
    text.char_indices()
        .nth(char_idx as usize)
        .map(|(b, _)| b as u32)
        .unwrap_or(text.len() as u32)
}

pub fn current_word_before_cursor(text: &str, cursor: u32) -> &str {
    let cursor = (cursor as usize).min(text.len());
    let cursor = (0..=cursor)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let before = &text[..cursor];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace() || *c == '\0')
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    &before[start..]
}

/// Detects whether the transition from (`prev_text`, `prev_cursor`) to
/// (`text`, `cursor`) is exactly one character inserted at `prev_cursor`.
/// Handles end-of-text typing AND mid-text typing.
pub fn detect_one_char_insertion(
    prev_text: &str,
    prev_cursor: u32,
    text: &str,
    cursor: u32,
) -> bool {
    let prev_cur = prev_cursor as usize;
    prev_cur <= prev_text.len()
        && cursor > prev_cursor
        && cursor == prev_cursor + 1
        && text.len() > prev_text.len()
        && text.len() == prev_text.len() + 1
        && text.get(..prev_cur) == prev_text.get(..prev_cur)
        && text.get((cursor as usize)..) == prev_text.get(prev_cur..)
}

#[cfg(test)]
mod tests {
    use super::{current_word_before_cursor, current_word_before_insertion_point};
    use super::{ByteCursor, Composer};
    use viet_ime_edit_strategy::{BackspaceMethod, KeyState, OutputSink};
    use viet_ime_engine::InputMethod;
    use viet_ime_wayland_adapter::KeyDecision;

    #[derive(Default)]
    struct DeleteCaptureSink {
        deletes: Vec<(u32, u32, u32, u32)>,
        vk_keys: Vec<(u32, u32, KeyState)>,
        commits: Vec<String>,
    }

    impl OutputSink for DeleteCaptureSink {
        fn delete_surrounding_text(
            &mut self,
            before_bytes: u32,
            before_chars: u32,
            after_bytes: u32,
            after_chars: u32,
        ) {
            self.deletes
                .push((before_bytes, before_chars, after_bytes, after_chars));
        }

        fn commit_string(&mut self, text: &str) {
            self.commits.push(text.to_owned());
        }

        fn commit(&mut self, _serial: u32) {}

        fn vk_key(&mut self, time: u32, key_code: u32, state: KeyState) {
            self.vk_keys.push((time, key_code, state));
        }

        fn vk_modifiers(&mut self, _depressed: u32, _latched: u32, _locked: u32, _group: u32) {}

        fn uinput_key(&mut self, _code: u16, _value: i32) {}

        fn vk_commit_char(&mut self, _time: u32, _ch: char) -> bool {
            false
        }
    }

    #[test]
    fn extracts_word_at_end_of_line() {
        assert_eq!(current_word_before_cursor("phow", 4), "phow");
    }

    #[test]
    fn extracts_word_in_middle_of_line() {
        assert_eq!(current_word_before_cursor("hello phow", 10), "phow");
    }

    #[test]
    fn extracts_partial_word_at_cursor() {
        assert_eq!(current_word_before_cursor("phow", 3), "pho");
    }

    #[test]
    fn empty_text_returns_empty() {
        assert_eq!(current_word_before_cursor("", 0), "");
    }

    #[test]
    fn cursor_at_start_returns_empty() {
        assert_eq!(current_word_before_cursor("hello", 0), "");
    }

    #[test]
    fn handles_multibyte_chars_at_char_boundary() {
        assert_eq!(current_word_before_cursor("trâ", 4), "trâ");
    }

    #[test]
    fn handles_cursor_inside_multibyte_char() {
        let r = current_word_before_cursor("trâ", 3);
        assert_eq!(r, "tr");
    }

    #[test]
    fn cursor_beyond_text_clamps() {
        assert_eq!(current_word_before_cursor("hi", 99), "hi");
    }

    #[test]
    fn space_separates_words() {
        assert_eq!(current_word_before_cursor("foo bar baz", 11), "baz");
    }

    #[test]
    fn tab_separates_words() {
        assert_eq!(current_word_before_cursor("foo\tbar", 7), "bar");
    }

    #[test]
    fn newline_separates_words() {
        assert_eq!(current_word_before_cursor("line1\nline2", 11), "line2");
    }

    #[test]
    fn selection_after_word_prefix_seeds_prefix_for_both_directions() {
        let text = "the vietnamese";
        let selection_start = "the viet".len() as u32;
        let selection_end = "the vietnamese".len() as u32;

        assert_eq!(
            current_word_before_insertion_point(text, selection_start, selection_end),
            "viet"
        );
        assert_eq!(
            current_word_before_insertion_point(text, selection_end, selection_start),
            "viet"
        );
    }

    #[test]
    fn selection_at_word_start_seeds_nothing_for_both_directions() {
        let text = "the vietnamese";
        let selection_start = "the ".len() as u32;
        let selection_end = "the viet".len() as u32;

        assert_eq!(
            current_word_before_insertion_point(text, selection_start, selection_end),
            ""
        );
        assert_eq!(
            current_word_before_insertion_point(text, selection_end, selection_start),
            ""
        );
    }

    #[test]
    fn cursor_inside_word_ignores_suffix_after_cursor() {
        let text = "tiếng viet vui vẻ";
        let cursor = "tiếng vie".len() as u32;

        assert_eq!(
            current_word_before_insertion_point(text, cursor, cursor),
            "vie"
        );
    }

    #[test]
    fn cursor_at_end_of_word_seeds_full_ascii_suffix_after_vietnamese_text() {
        let text = "tiếng viet vui vẻ";
        let cursor = "tiếng viet".len() as u32;

        assert_eq!(
            current_word_before_insertion_point(text, cursor, cursor),
            "viet"
        );
    }

    #[test]
    fn cursor_jump_after_composed_word_allows_next_tone_key_to_replace_tone() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let text = "chắc nó chừa mình ra";
        let cursor = "chắc nó".len() as u32;

        c.observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false);

        match c.feed_key('r') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 1);
                assert_eq!(commit, "ỏ");
            }
            _ => panic!("expected tone replacement edit"),
        }
    }

    #[test]
    fn window_quirks_store_delete_units_and_debounce_independently() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

        c.set_window_quirks(true, false);
        assert!(c.delete_in_chars);
        assert!(!c.debounce_barrier);

        c.set_window_quirks(false, true);
        assert!(!c.delete_in_chars);
        assert!(c.debounce_barrier);
    }

    #[test]
    fn cursor_jump_after_composed_vowel_keeps_plain_consonant_continuation_raw() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let text = "raw khôn";
        let cursor = text.len() as u32;

        c.observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false);

        assert!(matches!(c.feed_key('g'), KeyDecision::ForwardRaw));
        assert_eq!(c.raw_word, "không");
    }

    #[test]
    fn cursor_jump_after_toned_vowel_allows_live_vowel_update() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let text = "cái lòn gì";
        let cursor = "cái lòn".len() as u32;

        c.observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false);

        match c.feed_key('o') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 2);
                assert_eq!(commit, "ồn");
            }
            _ => panic!("expected live vowel update edit"),
        }
    }

    #[test]
    fn retroactive_composed_word_tracks_visible_state_between_tone_updates() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let text = "nguyễn sĩ thanh";
        let cursor = "nguyễn sĩ".len() as u32;

        c.observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false);

        match c.feed_key('s') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 1);
                assert_eq!(commit, "í");
            }
            _ => panic!("expected tone update"),
        }

        match c.feed_key('x') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 1);
                assert_eq!(commit, "ĩ");
            }
            _ => panic!("expected tone update"),
        }

        match c.feed_key('i') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 2);
                assert_eq!(commit, "sixi");
            }
            _ => panic!("expected raw restore"),
        }
    }

    #[test]
    fn active_word_w_then_tone_keeps_final_consonant_order() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

        assert!(matches!(c.feed_key('h'), KeyDecision::ForwardRaw));
        assert!(matches!(c.feed_key('o'), KeyDecision::ForwardRaw));
        assert!(matches!(c.feed_key('n'), KeyDecision::ForwardRaw));

        match c.feed_key('w') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 2);
                assert_eq!(commit, "ơn");
            }
            _ => panic!("expected vowel shape update"),
        }

        match c.feed_key('s') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 2);
                assert_eq!(commit, "ớn");
            }
            _ => panic!("expected tone update"),
        }
    }

    #[test]
    fn v1_identity_vowel_output_forwards_raw_instead_of_committing_duplicate() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

        assert!(matches!(c.feed_key('i'), KeyDecision::ForwardRaw));
        assert_eq!(c.raw_word, "i");
        assert_eq!(c.raw_word_screen_widths, vec![1]);
        assert_eq!(c.shadow_text(), "i");
    }

    #[test]
    fn recent_implausible_surrounding_echo_does_not_clobber_shadow() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

        c.mark_action();
        assert!(matches!(c.feed_key('i'), KeyDecision::ForwardRaw));
        c.observe_surrounding_bytes("i", ByteCursor(1), ByteCursor(1), false);
        c.observe_surrounding_bytes("ii", ByteCursor(0), ByteCursor(0), false);

        assert_eq!(c.shadow_text(), "i");
        match c.feed_key('s') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 1);
                assert_eq!(commit, "í");
            }
            _ => panic!("expected tone transform"),
        }
    }

    #[test]
    fn recent_selection_surrounding_frame_reaches_shadow() {
        // Chromium omnibox autocomplete: user types into "tra", the omnibox
        // expands to "translate" with "nslate" selected (cursor=3, anchor=9).
        // That frame arrives within the post-keystroke `recent_action` window
        // and is not a one-char insertion, but it carries the selection the
        // Tier-1 fallback (surrounding::apply) needs. The stale-echo gate must
        // NOT drop it. Regression for the bug reintroduced by the VSCode
        // stale-echo guard.
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

        c.mark_action();
        c.observe_surrounding_bytes("translate", ByteCursor(3), ByteCursor(9), false);

        // Frame reached the shadow: before-cursor text is "tra" and the
        // after-cursor selection "nslate" is recorded, so pop_delete_span
        // yields after_bytes > 0 and the ForwardKey fallback would fire.
        assert_eq!(c.shadow_text(), "tra");
        let (_, _, after_bytes, _) = c.pop_shadow_delete_span_for_test(1);
        assert!(after_bytes > 0, "selection-after must be recorded");
    }

    #[test]
    fn surrounding_observer_trusts_mid_word_one_char_without_reseed() {
        let decision = super::SurroundingObserver::observe(true, true, false, false);

        assert_eq!(decision, super::SurroundingDecision { trust: true, reseed: false });
    }

    #[test]
    fn surrounding_observer_trusts_recent_selection_without_reseed() {
        let decision = super::SurroundingObserver::observe(true, false, false, true);

        assert_eq!(decision, super::SurroundingDecision { trust: true, reseed: false });
    }

    #[test]
    fn edit_model_owns_shadow_and_tier_apply() {
        let mut edit = super::EditModel::new(BackspaceMethod::SurroundingText);
        edit.push_forwarded_char('a');
        assert_eq!(edit.shadow_text(), "a");

        let mut sink = DeleteCaptureSink::default();
        edit.apply(1, "â", 1, 0, &mut sink);

        assert_eq!(sink.deletes.len(), 1);
        assert_eq!(sink.commits, vec!["â".to_owned()]);
        assert_eq!(edit.shadow_text(), "â");
    }

    // ── detect_one_char_insertion ──────────────────────────────────────

    use super::detect_one_char_insertion as oci;

    #[test]
    fn oci_empty_to_first_char() {
        assert!(oci("", 0, "t", 1));
    }

    #[test]
    fn oci_append_ascii() {
        assert!(oci("ti", 2, "tie", 3));
    }

    #[test]
    fn oci_append_after_vietnamese() {
        // shadow "tiê" (4 bytes, 3 chars). cursor at byte 4. Type 'n' →
        // "tiên" (5 bytes), cursor at byte 5. Must detect as keystroke.
        assert!(oci("tiê", 4, "tiên", 5));
    }

    #[test]
    fn oci_mid_text_insert() {
        // "abcd" cursor=2 ("ab|cd"). Type 'X' → "abXcd" cursor=3.
        assert!(oci("abcd", 2, "abXcd", 3));
    }

    #[test]
    fn oci_multi_char_paste_rejected() {
        assert!(!oci("ab", 2, "abcde", 5));
    }

    #[test]
    fn oci_backspace_rejected() {
        assert!(!oci("abc", 3, "ab", 2));
    }

    #[test]
    fn oci_same_text_rejected() {
        // Duplicate frame: text and cursor unchanged.
        assert!(!oci("abc", 3, "abc", 3));
    }

    #[test]
    fn oci_cursor_jump_rejected() {
        // Same text, cursor moved (user clicked elsewhere).
        assert!(!oci("abcde", 5, "abcde", 2));
    }

    #[test]
    fn oci_replace_with_same_length_rejected() {
        // ê (2 bytes) replaced with two ASCII: same byte count, no growth.
        assert!(!oci("tiê", 4, "tiab", 4));
    }

    #[test]
    fn oci_cursor_mismatch_rejected() {
        // text grew by 1 byte but cursor jumped by 2 (impossible for single
        // keystroke insert at cursor).
        assert!(!oci("ab", 2, "abc", 3) == false);
        // also: cursor at 0 in larger text (not at insertion point).
        assert!(!oci("ab", 2, "abc", 1));
    }

    #[test]
    fn oci_post_apply_echo_rejected() {
        // After daklak's `bs=2 commit="ê"` on "tiee" cursor=4:
        // kate text becomes "tiê" cursor=4. text shrank by 0 in chars but
        // -1 byte (4→4 bytes? actually tiê=4 bytes, tiee=4 bytes, same len).
        // Should NOT be one_char_typed.
        assert!(!oci("tiee", 4, "tiê", 4));
    }

    #[test]
    fn oci_duong_line_break_to_capital_d() {
        // Regression: gedit on Enter resets surrounding text. User had
        // "đường" then pressed Enter and typed 'D'. text="D" cursor=1 vs
        // prev_text="đường" prev_cursor=9. Cursor went DOWN — must NOT be
        // detected as a 1-char keystroke (would feed 'D' into engine
        // without resetting raw_word).
        assert!(!oci("đường", 9, "D", 1));
    }

    #[test]
    fn oci_second_capital_d_on_new_line() {
        // After cursor jump above, prev_text becomes "D" prev_cursor=1.
        // User types second 'D' → text="DD" cursor=2. MUST be detected as
        // 1-char keystroke so handle_char fires and `DD→Đ` rule runs.
        assert!(oci("D", 1, "DD", 2));
    }
}

#[cfg(all(test, feature = "ibus"))]
mod char_to_byte_offset_tests {
    use super::char_to_byte_offset as c2b;

    #[test]
    fn ascii_char_offset_equals_byte_offset() {
        assert_eq!(c2b("is", 0), 0);
        assert_eq!(c2b("is", 1), 1);
        assert_eq!(c2b("is", 2), 2); // end
    }

    #[test]
    fn multibyte_cursor_at_end_maps_to_byte_len() {
        // "í" = U+00ED = 2 bytes, 1 char. IBus reports cursor_pos=1 (chars);
        // this must map to byte 2 (end), NOT byte 1 (mid-char) — the bug that
        // truncated the shadow to "" and produced "íis" when typing "iss".
        assert_eq!(c2b("í", 1), 2);
    }

    #[test]
    fn mixed_multibyte_offsets() {
        // "íis" = í(2) + i(1) + s(1) = 4 bytes, 3 chars.
        assert_eq!(c2b("íis", 0), 0);
        assert_eq!(c2b("íis", 1), 2); // after í
        assert_eq!(c2b("íis", 2), 3); // after i
        assert_eq!(c2b("íis", 3), 4); // end
    }

    #[test]
    fn offset_past_end_clamps_to_byte_len() {
        assert_eq!(c2b("í", 99), 2);
    }
}
