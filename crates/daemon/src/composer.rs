//! The composition brain. Owns engine + strategy + shadow + the
//! surrounding-text reseed gate, transport-neutral. Each transport
//! (wayland / ibus / evdev) is thin glue that translates its wire events
//! into `Composer` calls; the composition logic lives here once.
//!
//! Evolved from the old `WindowState` (state) + the composition methods that
//! used to live on `Daemon` (behavior). State and behavior now sit together.

use std::time::{Duration, Instant};

use viet_ime_edit_strategy::{BackspaceMethod, DeleteUnit, KeyDecision, ModifierState, Strategy};
use viet_ime_engine::{EngineState, InputMethod};

use crate::quirks::firefox::FirefoxContenteditableQuirk;
#[cfg(feature = "ibus")]
use crate::quirks::ibus::IbusSurroundingQuirk;

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

    /// Reset IM-owned text while preserving the last application snapshot.
    /// Composition/navigation actions must not erase surrounding provenance:
    /// unchanged frames emitted before an action takes effect stay detectable
    /// as duplicates instead of being mistaken for destination context.
    fn reset_composition(&mut self) {
        self.strategy.reset_shadow();
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

    fn deletion_since_prev(&self, text: &str, cursor: u32) -> bool {
        detect_deletion(&self.prev_text, self.prev_cursor, text, cursor)
    }

    /// Prove that `text` is the last confirmed application text plus exactly
    /// `ch` immediately before `cursor`. Cursor movement is irrelevant; the
    /// unchanged prefix+suffix proves this key, rather than an existing equal
    /// character, produced the frame.
    fn confirms_inserted_char(&self, text: &str, cursor: u32, ch: char) -> bool {
        let cursor = (cursor as usize).min(text.len());
        if !text.is_char_boundary(cursor) {
            return false;
        }
        let before = &text[..cursor];
        let Some((start, actual)) = before.char_indices().next_back() else {
            return false;
        };
        actual == ch
            && self.prev_text.len() + ch.len_utf8() == text.len()
            && self.prev_text.as_bytes().get(..start) == text.as_bytes().get(..start)
            && self.prev_text.as_bytes().get(start..) == text.as_bytes().get(cursor..)
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
        delete_unit: DeleteUnit,
    ) {
        self.strategy
            .apply(backspaces, commit, serial, time, sink, delete_unit);
    }

    fn apply_forward_key<S: viet_ime_edit_strategy::OutputSink>(
        &mut self,
        backspaces: usize,
        commit: &str,
        serial: u32,
        time: u32,
        sink: &mut S,
    ) {
        self.strategy
            .apply_forward_key(backspaces, commit, serial, time, sink);
    }

    #[cfg(test)]
    fn pop_delete_span_for_test(&mut self, backspaces: usize) -> (u32, u32, u32, u32) {
        self.strategy.shadow.pop_delete_span(backspaces)
    }
}

/// Per-text-input composition state + behavior. On wlroots/Sway, one instance
/// at a time (compositor sends deactivate before new activate); ibus/evdev
/// create one per active session.

/// Consecutive "dead surrounding" frames (empty text + cursor 0 while our
/// shadow holds committed content) that trip the SurroundingText→ForwardKey
/// runtime downgrade. Two absorbs a one-off race; a functional client never
/// produces even one (its surrounding always reflects at least our commits).
///
/// ACCEPTED COST: on an always-dead client (Google Docs, Firefox
/// contenteditable) the first correction of a focus session fires before
/// strike 2, so its delete is silently dropped and the word doubles once
/// (`Tiếng` → `Tieêngếng`) before the tier downgrades. Alternatives
/// considered and rejected:
///
/// - Threshold 1: a single transient empty frame (the one-off race the
///   current limit absorbs) would permanently downgrade a healthy widget to
///   ForwardKey.
/// - Activate-time no-op probe (`delete_surrounding_text(0,0)` + watch for
///   the echo): adds a round-trip on every focus for every healthy client,
///   and text-input-v3 gives no reply to a no-op delete — absence of an echo
///   is indistinguishable from a slow client, so the probe cannot conclude
///   anything before the user starts typing anyway.
/// - Per-app-id threshold 1 (e.g. known-broken Google Docs): app_id
///   identifies the browser, not the widget — it would misfire on every
///   healthy `<input>`/`<textarea>` in the same browser session.
const SURROUNDING_DEAD_STRIKE_LIMIT: u32 = 2;

pub struct Composer {
    pub engine: EngineState,
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

    /// Whether the focused client actually applies text-input-v3
    /// `commit_string`. Defaults `true`. Flipped to `false` at the ST→FK
    /// downgrade: an app that advertised surrounding-text but reports it dead
    /// (Google Docs / Firefox contenteditable: `text="" cursor=0` forever)
    /// ignores the whole text-input-v3 server-event contract — `commit_string`
    /// silently fails too (common cause, not feature coupling). When `false`,
    /// the ForwardKey commit routes through the virtual keyboard (`vk_commit_char`
    /// on v2/sway) or `ctx.keysym` (v1/KWin) instead of `commit_string`. There
    /// is no connect-time probe for this — text-input-v3 has no per-feature
    /// capability bit and the breakage is per-widget, so the ST-liveness symptom
    /// is the only observable signal.
    ///
    /// One-way ratchet BY DESIGN: nothing (not even `full_reset`) flips it
    /// back to `true`. Recovery happens because every activate creates a
    /// fresh `Composer` (wayland activate / synthetic activate /
    /// `activate_ibus` / `activate_evdev` all construct one), so the flag's
    /// lifetime is a single focus session. If Composer reuse across
    /// activations is ever introduced, this field needs a restore path.
    pub commit_string_functional: bool,

    /// Timestamp of the last user-keystroke action — used to distinguish
    /// "compositor echo of our action" (recent) from "user clicked mid-word"
    /// (not recent) in surrounding_text frames. Moved here from `Daemon`:
    /// the reseed gate is the only reader, and it lives on `Composer` now.
    last_action_at: Instant,

    /// Consecutive surrounding_text frames that looked "dead" (empty text +
    /// cursor 0 despite a non-empty shadow). Drives the runtime downgrade from
    /// SurroundingText to ForwardKey for clients that advertise surrounding
    /// support but never honor `delete_surrounding_text` (Google Docs /
    /// contenteditable in Firefox). See `note_surrounding_liveness`.
    surrounding_dead_strikes: u32,

    /// IBus surrounding-text liveness tracking.
    #[cfg(feature = "ibus")]
    ibus: IbusSurroundingQuirk,

    /// True after the engine was seeded from already-visible context instead of
    /// live keystrokes. Retroactive edits should track the latest visible word;
    /// otherwise repeated tone toggles accumulate reconstructed raw history that
    /// leaks on a later raw restore.
    retroactive_context: bool,

    /// Firefox contenteditable stale-echo workaround state.
    firefox: FirefoxContenteditableQuirk,

    /// Whether `[`/`]`/`{`/`}` Telex shortcuts for ơ/ư/Ơ/Ư are enabled.
    /// Stored so `set_input_method` can recreate the engine with it.
    bracket_shortcuts: bool,

    /// Whether retroactive SurroundingText edits must first be confirmed by a
    /// client frame containing the raw key at its real insertion point. Only
    /// Wayland drains frame-triggered repairs; IBus keeps immediate edits.
    confirm_retroactive_edits: bool,

    /// Raw key awaiting confirmation from surrounding text. While present,
    /// daklak must not issue a cursor-relative delete from its shadow: the app
    /// frame containing this character is the authority for the actual cursor.
    pending_raw_key: Option<char>,
    /// False when another key arrived before the pending key was confirmed;
    /// overlapping frames may synchronize state but cannot authorize repair.
    pending_raw_key_repairable: bool,
    /// The engine edit computed live for a confirm-deferred key. The engine
    /// keeps its post-key state (revert history intact); if the confirming
    /// frame proves the key landed on the same base word, this edit is emitted
    /// directly instead of re-deriving it from a reseed — a reseed can fail
    /// the round-trip gate ("kư") and forget that ư came from 'w'.
    pending_engine_edit: Option<PendingEngineEdit>,
    /// True when the engine's word context was derived from a client
    /// surrounding-text frame (cursor jump / focus / confirmed repair) rather
    /// than from our own live keystrokes. Only frame-derived context is
    /// cursor-uncertain and requires the confirm-deferred transaction;
    /// live composition must stay immediate (no flash, engine revert intact).
    context_from_frame: bool,
    /// One-shot: armed by a forwarded nav/editing action. The next printable
    /// key has no trustworthy context at all (shadow was reset, no destination
    /// frame arrived — gedit) and must be confirm-deferred even though no
    /// frame context exists yet. Cleared by the next key or trusted frame.
    await_frame_context: bool,
    edit: EditModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SurroundingDecision {
    trust: bool,
    reseed: bool,
}

/// A retroactive repair emitted from a surrounding_text frame (not a
/// keystroke). The frame proves where the raw key actually landed; the repair
/// is therefore calculated from client ground truth rather than shadow cursor.
#[derive(Debug, Clone)]
pub(crate) struct RetroEdit {
    pub(crate) backspaces: usize,
    pub(crate) commit: String,
}

/// Live engine edit stashed while a confirm-deferred key awaits its client
/// frame. `base_word` is the on-screen word the engine believed it was
/// editing; a confirming frame whose base differs (cursor moved) invalidates
/// the stash and falls back to reseed-and-replay from frame ground truth.
#[derive(Debug, Clone)]
struct PendingEngineEdit {
    backspaces: usize,
    commit: String,
    base_word: String,
}

// Generic surrounding-frame trust/reseed policy. This is not in `quirks/`
// because it is shared session behavior, not an app/protocol workaround.
struct SurroundingObserver;

impl SurroundingObserver {
    fn observe(
        recent_action: bool,
        one_char_typed: bool,
        deletion: bool,
        force_reseed: bool,
        shadow_confirmed: bool,
    ) -> SurroundingDecision {
        // Within the recent-action window every frame is some echo of our own
        // edit; the question is which to keep. `shadow_confirmed` (before-cursor
        // text == our shadow) is the discriminator:
        //
        //  • A frame that MATCHES our shadow is the post-commit echo / a genuine
        //    autocomplete-selection frame whose prefix is what the user typed
        //    (Chromium "tra"→"tra|nslate"). Trust it — it syncs the selection
        //    span the Tier-1 fallback needs and clears any stale one.
        //  • A frame that does NOT match is junk: our intermediate
        //    delete_surrounding_text echo (shorter than shadow — would reset the
        //    engine mid-word and drop the first word "word"→"ửod"), a VSCode
        //    duplicated-text stale echo, or a STALE Chromium-omnibox autocomplete
        //    selection describing a prefix we already typed past (`haf`→`à`).
        //    Drop it.
        //
        // `deletion` (incl. shadow-confirmed) drives only `reseed` + the reset
        // branch below; it must NOT relax `trust`, and `has_selection` must NOT
        // either (that blanket-trusted stale omnibox selections). Genuine
        // external edits arrive `!recent_action` and are always trusted.
        let trust = !(recent_action && !one_char_typed && !shadow_confirmed && !force_reseed);
        let reseed = force_reseed || (!one_char_typed && !recent_action && !deletion);
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
            last_keystroke_at: Instant::now(),
            last_input_char: None,
            commit_string_functional: true,
            last_action_at: Instant::now() - Duration::from_secs(60),
            surrounding_dead_strikes: 0,
            #[cfg(feature = "ibus")]
            ibus: IbusSurroundingQuirk::new(),
            retroactive_context: false,
            firefox: FirefoxContenteditableQuirk::new(),
            bracket_shortcuts,
            confirm_retroactive_edits: false,
            pending_raw_key: None,
            pending_raw_key_repairable: false,
            pending_engine_edit: None,
            context_from_frame: false,
            await_frame_context: false,
        }
    }

    // ── action-clock (reseed echo gate) ────────────────────────────────

    /// Mark a composing user action just happened (gates the reseed echo).
    pub fn mark_action(&mut self) {
        self.last_action_at = Instant::now();
    }

    /// Require client-frame confirmation before cursor-relative retroactive
    /// edits (Wayland only). Idempotent; safe to call on every activate.
    pub fn set_surrounding_confirmation(&mut self, on: bool) {
        self.confirm_retroactive_edits = on;
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
        if m != BackspaceMethod::SurroundingText {
            // A non-ST tier has no confirmation transaction: no client frame
            // will ever confirm (or clear) a pending raw key. Leaving it set
            // would make every subsequent feed_key reset the engine (ghostty:
            // composition dead until a non-printable key cleared it).
            self.pending_raw_key = None;
            self.pending_raw_key_repairable = false;
            self.pending_engine_edit = None;
        }
    }

    /// Watchdog for clients that advertise surrounding-text support but never
    /// actually maintain it. Google Docs (and any contenteditable widget in
    /// Firefox) reports `text="" cursor=0` on every frame and silently no-ops
    /// `delete_surrounding_text`, so on the SurroundingText tier each
    /// correction's `commit_string` lands without its paired delete and the
    /// word doubles (`Tiếng` → `Tieêngếng`).
    ///
    /// The signature is unambiguous: an empty surrounding frame *while our
    /// shadow already holds committed content*. A functional client's
    /// surrounding always reflects at least what we committed, so it never
    /// produces this; a genuinely empty field leaves our shadow empty too.
    /// After [`SURROUNDING_DEAD_STRIKE_LIMIT`] consecutive such frames, return
    /// `true` so the caller can downgrade the tier to ForwardKey — whose real
    /// Backspace keystrokes these clients do honor (raw key passthrough already
    /// works on the same path). Any frame that does reflect content resets the
    /// count. Call once per surrounding frame, *before* the duplicate-frame
    /// guard (every dead frame is byte-identical, so the guard would otherwise
    /// hide all but the first).
    pub fn note_surrounding_liveness(&mut self, text: &str, cursor: u32) -> bool {
        let dead = text.is_empty() && cursor == 0 && !self.edit.shadow_text().is_empty();
        if dead {
            self.surrounding_dead_strikes += 1;
            self.surrounding_dead_strikes >= SURROUNDING_DEAD_STRIKE_LIMIT
        } else {
            if !text.is_empty() {
                self.surrounding_dead_strikes = 0;
            }
            false
        }
    }

    /// Record that a surrounding_text frame arrived (any content). On the
    /// SurroundingText tier this is the client echoing our edit back, i.e.
    /// proof it honored `delete_surrounding_text`. Marks the echo window open
    /// for the in-flight correction so `note_surrounding_correction` won't
    /// strike it. Call once per received frame, before any early-return guard.
    #[cfg(feature = "ibus")]
    pub fn mark_surrounding_frame_seen(&mut self) {
        self.ibus.mark_surrounding_frame_seen();
    }

    /// Echo-based SurroundingText→ForwardKey downgrade. A functional client
    /// round-trips every delete+commit as a fresh SetSurroundingText (gedit
    /// echoes `kh`→`khô`); a client that advertises surrounding but silently
    /// no-ops the delete (Google Docs under IBus) emits nothing back, so each
    /// correction's commit lands undeleted and the word doubles
    /// (`Tiếng` → `Tieêngếng`). Capability bits can't tell them apart — Docs
    /// flaps caps=9/caps=41 in one focus sequence — but the echo can.
    ///
    /// Call once per SurroundingText correction that issues a delete
    /// (`backspaces > 0`), *before* applying it. If no frame arrived since the
    /// previous correction, that previous correction went unechoed → count a
    /// strike; on the first such echo-less strike return `true` so
    /// the caller downgrades to ForwardKey (whose real Backspaces these clients
    /// do honor). Any echo resets the count. The first correction never strikes
    /// (no predecessor to judge); it then arms the window for the next one.
    #[cfg(feature = "ibus")]
    pub fn note_surrounding_correction(&mut self) -> bool {
        self.ibus.note_correction_and_should_downgrade()
    }

    pub fn set_modifiers(&mut self, m: ModifierState) {
        self.edit.set_modifiers(m);
    }

    /// Change input method at runtime. Resets all composition state.
    pub fn set_input_method(&mut self, method: InputMethod) {
        let modern_style = self.engine.modern_style();
        self.full_reset();
        self.engine = EngineState::new_with_options(method, self.bracket_shortcuts);
        self.engine.set_modern_style(modern_style);
    }

    /// Toggle modern-style tone placement. `false` = legacy `òa` instead of `oà`.
    pub fn set_modern_style(&mut self, enabled: bool) {
        self.engine.set_modern_style(enabled);
    }

    // ── lifecycle ─────────────────────────────────────────────────────────

    /// Reset compose state — call on deactivate / navigation key /
    /// modifier shortcut / external cursor movement / word boundary
    /// (space/Enter/Tab). Wipes everything tracking an in-progress word:
    /// engine, shadow, last_input_char, and the surrounding-text diff
    /// bookkeeping (prev_text, prev_cursor).
    pub fn full_reset(&mut self) {
        self.reset_composition_preserving_surrounding();
        self.edit.clear_prev_surrounding();
        #[cfg(feature = "ibus")]
        self.ibus.reset();
    }

    /// Reset IM-owned composition without discarding the last confirmed client
    /// snapshot. Forwarded cursor/editing actions are asynchronous: KWin may
    /// re-emit the unchanged pre-action frame before gedit processes the key.
    /// Keeping `prev_text` makes that frame a duplicate instead of destination
    /// context. Mouse navigation needs no special handling; its changed frame
    /// is consumed normally by `apply_surrounding`.
    pub fn reset_composition_preserving_surrounding(&mut self) {
        self.engine.reset();
        self.edit.reset_composition();
        self.last_input_char = None;
        self.retroactive_context = false;
        self.context_from_frame = false;
        self.await_frame_context = true;
        self.pending_raw_key = None;
        self.pending_raw_key_repairable = false;
        self.pending_engine_edit = None;
        self.firefox.clear();
        self.firefox.reset_forward_sticky();
    }

    /// Re-seed the engine after a retroactive edit was applied. Must mirror
    /// the initial word-boundary seed in `feed_key`: feed only the CURRENT
    /// word before the cursor, never the whole shadow. The render gate in
    /// `feed_context_gated` round-trips the seed, and a whole shadow
    /// containing an English word fails it (Telex misreads "wor" as "wỏ"),
    /// which would silently drop retroactive context after the first tone
    /// toggle in mixed text like "wor ở".
    fn refresh_retroactive_context_after_apply(&mut self, backspaces: usize) {
        if backspaces == 0 || !self.retroactive_context {
            return;
        }

        let shadow = self.edit.shadow_text();
        let word = current_word_before_cursor(shadow, shadow.len() as u32);
        self.engine.reset();
        self.retroactive_context = !word.is_empty() && self.engine.feed_context_gated(word);
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
            // The cursor may have moved silently during the pause (gedit
            // sends no frame for mouse clicks). The next key's context is
            // cursor-uncertain and must be confirm-deferred on ST.
            self.await_frame_context = true;
            return true;
        }
        false
    }

    // ── composition ───────────────────────────────────────────────────────

    /// Feed one printable char. The engine runs **continuously** — each key
    /// builds on the prior engine state (no per-key reset), so vowel-cluster
    /// context survives transforms (`tieengs`'s sắc tone lands on `iê` because
    /// the engine still holds the cluster). Used by every transport: wayland
    /// (KWin v1 + wlroots v2), IBus, and evdev.
    ///
    /// When starting a fresh word, the engine is seeded from the word before the
    /// cursor in `shadow` (retroactive editing). The seed is **render-gated** — a
    /// shadow word whose reverse-telex doesn't round-trip (e.g. English "wor",
    /// which telex-misreads as "ở") must NOT seed, or the next key composes
    /// against garbage (`wor`+`d` → a bogus delete-2 edit instead of plain
    /// "word"). On gate failure the engine stays fresh and the key starts anew.
    pub fn feed_key(&mut self, ch: char) -> KeyDecision {
        let prev_was_separator = matches!(
            self.last_input_char,
            Some(c) if !c.is_ascii_alphabetic()
        );
        // Defence in depth vs. set_method: a pending key is only meaningful
        // while frame confirmation is possible (SurroundingText tier).
        let prior_key_unconfirmed = self.pending_raw_key.is_some()
            && self.method() == BackspaceMethod::SurroundingText;
        // Only cursor-uncertain context defers to frame confirmation:
        // a forwarded nav/editing action or idle pause (await_frame_context),
        // engine context reconstructed from a client frame at a possibly-
        // stale cursor (context_from_frame), or an in-flight confirmation
        // transaction (prior_key_unconfirmed). Live composition from our own
        // keystroke stream is never deferred — no flash, and the engine's
        // continuous state handles reverts (kww→kw) and auto-restore
        // (kwin stays raw) natively.
        let context_uncertain = self.confirm_retroactive_edits
            && self.method() == BackspaceMethod::SurroundingText
            && (self.await_frame_context || self.context_from_frame || prior_key_unconfirmed);
        self.await_frame_context = false;
        if prior_key_unconfirmed {
            // More than one key arrived before surrounding confirmation. Never
            // compose against the speculative shadow; degrade to raw and let
            // the newest key's frame resynchronize the transaction.
            self.engine.reset();
            self.retroactive_context = false;
            self.pending_engine_edit = None;
        }
        if !ch.is_ascii_alphabetic() {
            self.retroactive_context = false;
            self.context_from_frame = false;
            self.firefox.clear();
        }
        self.last_input_char = Some(ch);

        if !prior_key_unconfirmed && self.engine.at_word_beginning() && !prev_was_separator {
            let shadow = self.edit.shadow_text();
            let raw_word = current_word_before_cursor(shadow, shadow.len() as u32);
            if !raw_word.is_empty() {
                tracing::debug!(word = raw_word, "seed engine from shadow at word boundary");
                self.retroactive_context = self.engine.feed_context_gated(raw_word);
            }
        }

        // A context reconstructed from surrounding text may describe a stale
        // cursor. Its positional edit is unsafe until the client reports where
        // this key actually landed. Live composition remains immediate.
        let confirm_before_positional_edit = context_uncertain;
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

        if r.consumed && !confirm_before_positional_edit {
            let method = self.method();
            KeyDecision::Apply {
                method,
                backspaces: r.backspaces,
                commit: r.commit,
            }
        } else {
            if r.consumed {
                // Keep the engine's post-key state — it holds the revert
                // history a reseed cannot reconstruct ("kư" fails the gate).
                // Stash the computed edit; the confirming frame decides whether
                // it may be emitted (same base word) or must be re-derived.
                let base = {
                    let shadow = self.edit.shadow_text();
                    current_word_before_cursor(shadow, shadow.len() as u32).to_owned()
                };
                self.pending_engine_edit = Some(PendingEngineEdit {
                    backspaces: r.backspaces,
                    commit: r.commit.clone(),
                    base_word: base,
                });
            }
            self.edit.push_forwarded_char(ch);
            if confirm_before_positional_edit {
                // Only a cursor-uncertain key opens a confirmation
                // transaction. Plain unconsumed live keys (English words,
                // word-initial consonants) just extend the shadow as before.
                self.pending_raw_key = Some(ch);
                self.pending_raw_key_repairable = !prior_key_unconfirmed;
            }
            // A forwarded key changes the shadow after the stale echo that
            // armed char-count mode, so that one-shot assumption no longer
            // applies to the next correction.
            self.firefox.clear();
            KeyDecision::ForwardRaw
        }
    }

    pub fn unrecord_forwarded_char(&mut self) {
        self.edit.pop_forwarded_char();
        self.pending_raw_key = None;
        self.pending_raw_key_repairable = false;
        if self.pending_engine_edit.take().is_some() {
            // Engine held speculative post-key state the app never applied.
            self.engine.reset();
            self.retroactive_context = false;
        }
    }

    pub fn feed_backspace(&mut self) -> KeyDecision {
        let r = self.engine.process_backspace();
        tracing::debug!(
            consumed = r.consumed,
            bs = r.backspaces,
            "engine.process_backspace"
        );

        if r.consumed {
            self.last_keystroke_at = Instant::now();
            let method = self.method();
            KeyDecision::Apply {
                method,
                backspaces: r.backspaces,
                commit: r.commit,
            }
        } else {
            tracing::trace!("BS not consumed → forward");
            let deleted_visible = self.edit.shadow_text().chars().last();
            self.edit.pop_forwarded_char();
            // `processed=false` can still carry a positive backspace count:
            // vnkey recognized that an owned glyph was deleted but did not emit
            // an edit for the daemon. The app will perform that delete through
            // the forwarded Backspace; when the deleted visible glyph is a
            // composed non-ASCII char (e.g. `u`+`w` -> `ư`), drop stale raw
            // history before a later raw restore resurrects the deleted prefix
            // (`sưi`+`t` -> `uswit`). Plain ASCII English backspaces keep their
            // engine history so retyping `doesnt` stays raw.
            if r.backspaces > 0 && matches!(deleted_visible, Some(ch) if !ch.is_ascii()) {
                self.engine.reset();
                self.retroactive_context = false;
            }
            self.last_keystroke_at = Instant::now();
            self.firefox.clear();
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
        self.edit
            .apply(backspaces, commit, 0, time, sink, DeleteUnit::Bytes);
        self.refresh_retroactive_context_after_apply(backspaces);
    }

    pub fn apply_to_sink<S: viet_ime_edit_strategy::OutputSink>(
        &mut self,
        backspaces: usize,
        commit: &str,
        serial: u32,
        time: u32,
        sink: &mut S,
    ) {
        self.apply_to_sink_inner(backspaces, commit, serial, time, sink, false);
    }

    fn apply_to_sink_inner<S: viet_ime_edit_strategy::OutputSink>(
        &mut self,
        backspaces: usize,
        commit: &str,
        serial: u32,
        time: u32,
        sink: &mut S,
        preserve_engine: bool,
    ) {
        let before = self.edit.shadow_text().to_owned();
        let delete_echo = prefix_after_char_delete(&before, backspaces);
        let delete_unit = self.firefox.delete_unit();

        // Firefox contenteditable quirk: once a stale correction echo is
        // observed, force delete through ForwardKey until a fresh correction
        // echo clears the quirk state.
        //
        // delete_unit == Chars is still logged as the immediate stale signal.
        // `firefox.use_forward_delete()` reflects the quirk's current stale
        // assessment, while `firefox.forward_sticky()` keeps delete on
        // ForwardKey for this text-input object once stale mode has been
        // observed at least once.
        let quirk_requests_forward = self.firefox.use_forward_delete();
        if quirk_requests_forward {
            self.firefox.arm_forward_sticky();
        }
        // Firefox contenteditable byte-vs-char bug (Bug 1905481 family):
        // delete_surrounding_text uses byte counts per the text-input-v3 spec,
        // but Firefox contenteditable may misinterpret them as character counts
        // when the deletion spans multibyte UTF-8 and the composition is being
        // REVERTED to raw ASCII (e.g. Telex ww undoes ư→w). Normal tone
        // replacement (à→ả) commits another multibyte char so the byte
        // arithmetic is self-correcting; raw reverts commit ASCII (1 byte per
        // char), exposing Firefox's mis-measurement of the preceding delete.
        // Use ForwardKey (physical Backspace) which always removes exactly
        // 1 character regardless of byte width.
        let multibyte_retroactive_revert = self.retroactive_context
            && backspaces > 0
            && commit.is_ascii()
            && backspaces < before.chars().count() // partial revert, not full auto-restore
            && before.chars().rev().take(backspaces).any(|c| c.len_utf8() > 1);
        let used_forward_key =
            quirk_requests_forward || self.firefox.forward_sticky() || multibyte_retroactive_revert;
        tracing::trace!(
            ?delete_unit,
            quirk_requests_forward,
            sticky_forward = self.firefox.forward_sticky(),
            used_forward_key,
            pending_echo = self.firefox.has_pending_echo(),
            retroactive_context = self.retroactive_context,
            "firefox delete-channel decision"
        );

        // Firefox contenteditable quirk: when delete_unit == Chars, the quirk
        // detected a stale surrounding-text echo, meaning Firefox didn't update
        // its text-input-v3 state after our last correction. Using
        // delete_surrounding_text in this state is unreliable — Firefox may
        // delete the wrong range or skip the delete entirely, causing forward
        // deletion into the next word. Emit physical Backspace key events
        // (ForwardKey) instead.
        if used_forward_key {
            tracing::info!(
                backspaces,
                shadow_before_bytes = before.len(),
                shadow_before_chars = before.chars().count(),
                commit_bytes = commit.len(),
                commit_chars = commit.chars().count(),
                "firefox quirk active: using ForwardKey instead of delete_surrounding_text"
            );
            self.edit
                .apply_forward_key(backspaces, commit, serial, time, sink);
        } else {
            self.edit
                .apply(backspaces, commit, serial, time, sink, delete_unit);
            self.firefox.reset_delete_unit_after_use();
        }

        if !preserve_engine {
            self.refresh_retroactive_context_after_apply(backspaces);
        }
        if backspaces > 0 && self.method() == BackspaceMethod::SurroundingText {
            self.firefox
                .record_expected_echo(self.edit.shadow_text().to_owned(), delete_echo);
        }
    }

    /// Apply a repair calculated from the frame that confirmed a raw key's
    /// actual insertion point. KWin may repeat the pre-edit frame; that is part
    /// of this generic transaction, not evidence of Firefox stale state.
    pub fn apply_confirmed_repair_to_sink<S: viet_ime_edit_strategy::OutputSink>(
        &mut self,
        backspaces: usize,
        commit: &str,
        serial: u32,
        time: u32,
        sink: &mut S,
    ) {
        // The engine already holds the authoritative live composition (the
        // confirmed base word replayed through the raw key) and knows the
        // key that produced each transform. Reseeding from the on-screen word
        // would destroy revert history: after 'w'→"kư", the seed gate rejects
        // "kư" (no unique round-trip), the engine resets, and the second 'w'
        // composes a fresh "ư" ("kưư") instead of reverting to "kw".
        self.apply_to_sink_inner(backspaces, commit, serial, time, sink, true);
        self.firefox.forget_expected_echo();
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
    ) -> Option<RetroEdit> {
        self.apply_surrounding(text, cursor.0, anchor.0, force_reseed)
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
        // IBus ignores frame-triggered repairs (confirmation stays off), so
        // `apply_surrounding` never returns one here — drop the Option.
        let _ = self.apply_surrounding(text, cursor_bytes, anchor_bytes, false);
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
    fn apply_surrounding(
        &mut self,
        text: &str,
        cursor: u32,
        anchor: u32,
        force_reseed: bool,
    ) -> Option<RetroEdit> {
        // text-input-v3 caps surrounding text at 4000 bytes; IBus has no
        // protocol limit. Log if an app sends more than ~1 KiB — that's
        // already far more context than we ever use (one word before cursor).
        if text.len() > 1024 {
            tracing::warn!(
                text_bytes = text.len(),
                cursor,
                anchor,
                "surrounding text is unexpectedly long (>1 KiB)"
            );
        }
        let recent_action = self.last_action_at.elapsed() < Duration::from_millis(150);
        let one_char_typed = self.edit.one_char_insertion_since_prev(text, cursor);
        let before_cursor = text_before_cursor(text, cursor);

        // A raw key awaiting confirmation is stronger evidence than the 150ms
        // echo heuristic: daklak issued no positional edit for this key, and a
        // frame containing it immediately before the reported cursor tells us
        // where the client actually inserted it. Handle this before Firefox or
        // generic stale-echo classification.
        if self.pending_raw_key.is_some() && cursor == anchor {
            if let Some(repair) = self.confirm_pending_raw_key(text, cursor, anchor, before_cursor) {
                return Some(repair);
            }
            if self.pending_raw_key.is_none() {
                return None;
            }
        }

        let shadow_confirmed = before_cursor == self.edit.shadow_text();
        let deletion = self.edit.deletion_since_prev(text, cursor) || shadow_confirmed;
        // `shadow_confirmed` (not `has_selection`) gates trust within the
        // recent-action window. A genuine Chromium autocomplete-selection frame
        // has a before-cursor prefix equal to what the user typed (== shadow),
        // so it is shadow_confirmed and trusted — the Tier-1 fallback still gets
        // its selection span via `on_surrounding_text`. A STALE omnibox
        // selection frame describes a prefix we already typed past, so it is NOT
        // confirmed and is dropped (previously `has_selection` blanket-trusted
        // it, re-arming a stale selection → `haf` committed `à`).
        let decision = SurroundingObserver::observe(
            recent_action,
            one_char_typed,
            deletion,
            force_reseed,
            shadow_confirmed,
        );

        self.firefox
            .observe_surrounding(before_cursor, recent_action, self.retroactive_context);
        if self.firefox.use_forward_delete() || self.firefox.forward_sticky() {
            self.commit_string_functional = false;
        }

        if !decision.trust {
            tracing::trace!(text, cursor, anchor, "skip recent surrounding_text echo");
            return None;
        }

        let deletion_already_applied = shadow_confirmed;
        self.edit.on_surrounding_text(text, cursor, anchor);
        // A trusted frame supplies destination context; frame-derived seeds
        // below carry the uncertainty via `context_from_frame` instead.
        self.await_frame_context = false;
        self.pending_raw_key = None;
        self.pending_raw_key_repairable = false;
        if self.pending_engine_edit.take().is_some() {
            // A trusted frame superseded the confirmation transaction; the
            // engine's speculative post-key state was never applied on screen.
            self.engine.reset();
            self.retroactive_context = false;
        }

        if deletion {
            if !deletion_already_applied {
                self.engine.reset();
                self.retroactive_context = false;
                self.context_from_frame = false;
                // The engine now reflects the app, not the keyboard. A stale
                // last_input_char (e.g. the ' ' that ended the previous word)
                // would keep prev_was_separator true and block the
                // word-boundary seed for the word the cursor now touches
                // (external delete of "la "→"la", then 'f' must compose "là",
                // not forward raw).
                self.last_input_char = None;
            }
        } else if decision.reseed {
            let word = current_word_before_insertion_point(text, cursor, anchor);
            self.engine.reset();
            self.retroactive_context = !word.is_empty() && self.engine.feed_context_gated(word);
            // Engine context now describes a frame-reported cursor which may
            // go stale before the next key; positional edits from it must be
            // confirm-deferred.
            self.context_from_frame = self.retroactive_context;
            // Same invalidation as the deletion arm: after a genuine cursor
            // jump the last typed key describes the previous locale.
            self.last_input_char = None;
            if self.retroactive_context {
                tracing::debug!(word, "re-seed engine (activate or cursor jump)");
            }
        }

        self.edit.record_surrounding(text, cursor, anchor);
        None
    }

    /// Finish a raw-key transaction from client ground truth. The only required
    /// proof is the tracked key immediately before the reported cursor; how the
    /// cursor moved there is irrelevant. The frame is always recorded, ending
    /// the `prev_text` starvation loop even when replay produces no edit.
    fn confirm_pending_raw_key(
        &mut self,
        text: &str,
        cursor: u32,
        anchor: u32,
        before_cursor: &str,
    ) -> Option<RetroEdit> {
        let ch = self.pending_raw_key?;
        let Some((raw_index, actual_ch)) = before_cursor.char_indices().next_back() else {
            return None;
        };
        if actual_ch != ch {
            return None;
        }

        let insertion_proven = self.edit.confirms_inserted_char(text, cursor, ch);
        let repairable = self.pending_raw_key_repairable;
        let stripped = &before_cursor[..raw_index];
        let base_word = current_word_before_cursor(stripped, stripped.len() as u32).to_owned();
        let actual_word = current_word_before_cursor(before_cursor, before_cursor.len() as u32)
            .to_owned();

        if !insertion_proven || !repairable {
            // The frame changed by more than this key (paste/delete/external
            // edit). It is still the newest ground truth, but cannot justify a
            // repair. Synchronize and continue from its visible word.
            self.edit.on_surrounding_text(text, cursor, anchor);
            self.edit.record_surrounding(text, cursor, anchor);
            self.pending_raw_key = None;
            self.pending_raw_key_repairable = false;
            self.pending_engine_edit = None;
            self.last_input_char = Some(ch);
            self.engine.reset();
            self.retroactive_context = !actual_word.is_empty()
                && self.engine.feed_context_gated(&actual_word);
            self.context_from_frame = self.retroactive_context;
            return None;
        }

        // Preferred path: the live engine already processed this key against
        // the same base word the frame now proves. Emit its stashed edit and
        // KEEP the engine state — it holds revert history a reseed loses
        // ("kư" fails the round-trip gate, so reseeding forgets ư came from
        // 'w' and a second 'w' would compose "kưư" instead of reverting).
        if let Some(stash) = self.pending_engine_edit.take() {
            if stash.base_word == base_word {
                self.edit.on_surrounding_text(text, cursor, anchor);
                self.edit.record_surrounding(text, cursor, anchor);
                self.pending_raw_key = None;
                self.pending_raw_key_repairable = false;
                self.last_input_char = Some(ch);
                self.retroactive_context = true;
                self.context_from_frame = true;
                // The screen shows base_word + raw ch; the engine wants
                // base_word minus `backspaces` plus `commit`. Deleting the raw
                // char costs one extra backspace.
                return Some(RetroEdit {
                    backspaces: stash.backspaces + 1,
                    commit: stash.commit,
                });
            }
            // Cursor moved: the key landed on a different word than the one
            // the engine composed against. Drop the stash and re-derive from
            // frame ground truth below.
        }

        self.engine.reset();
        let seeded = !base_word.is_empty() && self.engine.feed_context_gated(&base_word);
        let replay = seeded.then(|| self.engine.process_key(ch));

        // This frame is authoritative regardless of whether replay composes.
        self.edit.on_surrounding_text(text, cursor, anchor);
        self.edit.record_surrounding(text, cursor, anchor);
        self.pending_raw_key = None;
        self.pending_raw_key_repairable = false;
        self.last_input_char = Some(ch);

        // For boundary-valued IM keys (`[`/`]` Telex shortcuts), extracting the
        // current word after the raw key yields empty. The proven on-screen word
        // is instead exactly the base word plus that inserted key.
        let screen_word = format!("{base_word}{ch}");

        let Some(replay) = replay else {
            // Fresh word or foreign base: keep the raw key, but seed the full
            // confirmed word so subsequent keys start from real client state.
            self.engine.reset();
            self.retroactive_context = !screen_word.is_empty()
                && self.engine.feed_context_gated(&screen_word);
            self.context_from_frame = self.retroactive_context;
            return None;
        };

        self.retroactive_context = true;
        self.context_from_frame = true;
        if !replay.consumed {
            // The seeded engine already incorporated the raw key and matches the
            // screen; no visible correction is needed.
            return None;
        }

        let mut desired = base_word;
        for _ in 0..replay.backspaces {
            desired.pop();
        }
        desired.push_str(&replay.commit);

        let prefix_chars = screen_word
            .chars()
            .zip(desired.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let backspaces = screen_word.chars().count() - prefix_chars;
        let commit: String = desired.chars().skip(prefix_chars).collect();

        if backspaces == 0 && commit.is_empty() {
            None
        } else {
            Some(RetroEdit { backspaces, commit })
        }
    }

    /// True if (text, cursor, anchor) exactly matches the last frame — clients
    /// re-emit identical surrounding text; re-running the reseed on an
    /// unchanged frame burns engine state. Transport glue checks this before
    /// `observe_surrounding_*`.
    pub fn is_duplicate_frame(&self, text: &str, cursor: u32, anchor: u32) -> bool {
        self.edit.is_duplicate_frame(text, cursor, anchor)
    }

    /// Duplicate surrounding frames are normally safe to skip, but not while a
    /// SurroundingText correction is awaiting its echo: Firefox contenteditable
    /// can report the stale pre-edit text unchanged, and that duplicate is the
    /// only signal to switch the next correction to char-count delete.
    pub fn should_skip_surrounding_frame(
        &self,
        text: &str,
        cursor: u32,
        anchor: u32,
        activate: bool,
        deactivate: bool,
    ) -> bool {
        !activate
            && !deactivate
            && !self.firefox.has_pending_echo()
            && self.is_duplicate_frame(text, cursor, anchor)
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
    pub(crate) fn shadow_text(&self) -> &str {
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
    let before = text_before_cursor(text, cursor);
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| is_word_boundary(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    &before[start..]
}

/// Hyphen is intentionally NOT a word boundary: Vietnamese uses hyphens
/// within compound words and proper nouns (e.g. `bán-nguyệt`, `Ê-đê`).
/// Breaking at `-` would prevent surrounding-text reseeding from
/// reconstructing the full compound for the engine.
fn is_word_boundary(c: char) -> bool {
    c.is_whitespace()
        || c == '\0'
        || matches!(
            c,
            '.' | ','
                | ';'
                | ':'
                | '!'
                | '?'
                | '"'
                | '\''
                | '“'
                | '”'
                | '‘'
                | '’'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '/'
                | '\\'
                | '|'
                | '…'
        )
}

fn text_before_cursor(text: &str, cursor: u32) -> &str {
    let cursor = (cursor as usize).min(text.len());
    let cursor = (0..=cursor)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    &text[..cursor]
}

/// Expected "delete-phase echo": the shadow with `backspaces` chars removed
/// from the END. This is deliberately the ONLY intermediate echo shape the
/// Firefox quirk recognizes — our corrections are always delete-tail +
/// commit, so a compliant client passing through the intermediate state can
/// only show an end-deletion. Firefox's stale cache (Bug 1905481) can report
/// other shapes (pre-edit text, truncations); those deliberately do NOT get
/// their own patterns. See `FirefoxContenteditableQuirk::observe_surrounding`
/// for why unrecognized shapes degrade to ForwardKey instead.
fn prefix_after_char_delete(text: &str, backspaces: usize) -> String {
    let keep = text.chars().count().saturating_sub(backspaces);
    text.chars().take(keep).collect()
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
    let cur = cursor as usize;
    if prev_cur > prev_text.len()
        || cur > text.len()
        || prev_cur >= cur
        || !prev_text.is_char_boundary(prev_cur)
        || !text.is_char_boundary(cur)
        || !text.is_char_boundary(prev_cur)
    {
        return false;
    }

    text.get(..prev_cur) == prev_text.get(..prev_cur)
        && text.get(cur..) == prev_text.get(prev_cur..)
        && text[prev_cur..cur].chars().count() == 1
}

/// Detects whether text between the previous and current cursor was deleted.
/// Handles end-of-text and mid-text deletions while preserving surrounding text.
pub fn detect_deletion(prev_text: &str, prev_cursor: u32, text: &str, cursor: u32) -> bool {
    let prev_cur = prev_cursor as usize;
    let cur = cursor as usize;
    if cur > prev_cur
        || cur > text.len()
        || prev_cur > prev_text.len()
        || !text.is_char_boundary(cur)
        || !prev_text.is_char_boundary(cur)
        || !prev_text.is_char_boundary(prev_cur)
    {
        return false;
    }

    text.len() < prev_text.len()
        && text.get(..cur) == prev_text.get(..cur)
        && text.get(cur..) == prev_text.get(prev_cur..)
        && !prev_text[cur..prev_cur].is_empty()
}

#[cfg(test)]
mod tests {
    use super::{current_word_before_cursor, current_word_before_insertion_point};
    use super::{ByteCursor, Composer};
    use viet_ime_edit_strategy::{BackspaceMethod, DeleteUnit, KeyState, OutputSink};
    use viet_ime_engine::InputMethod;
    use viet_ime_edit_strategy::KeyDecision;

    // Re-exports for nested test modules
    pub(super) use super::{EditModel, SurroundingDecision, SurroundingObserver};
    pub(super) use super::detect_deletion as del;
    pub(super) use super::detect_one_char_insertion as oci;

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

        fn vk_commit_char(&mut self, _time: u32, _ch: char) -> bool {
            false
        }
    }

    mod firefox_contenteditable {
        use super::*;

        #[test]
        fn stale_surrounding_echo_arms_char_delete_for_composed_chars() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), true);
            c.mark_action();
            c.apply_to_sink(1, "ư", 1, 0, &mut sink);
            assert_eq!(c.shadow_text(), "tư");

            // Firefox contenteditable stale cache (Bug 1905481): after tu+w -> tư,
            // the echo still reports the old shorter text. A byte-count delete would
            // over-delete and eat the preceding consonant (the `tự` first-char drop),
            // so arm char-count deletion for the next correction.
            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), false);

            c.mark_action();
            c.apply_to_sink(1, "ự", 2, 0, &mut sink);

            // Stale echo now bypasses delete_surrounding_text and emits one
            // ForwardKey Backspace pair instead.
            assert_eq!(sink.deletes.len(), 1);
            assert_eq!(sink.vk_keys.len(), 2);
            assert_eq!(sink.vk_keys[0], (0, 14, KeyState::Pressed));
            assert_eq!(sink.vk_keys[1], (0, 14, KeyState::Released));
            assert_eq!(c.shadow_text(), "tự");
        }

        #[test]
        fn duplicate_stale_echo_is_not_skipped_while_waiting_for_correction_echo() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), true);
            c.mark_action();
            c.apply_to_sink(1, "ư", 1, 0, &mut sink);

            assert!(c.is_duplicate_frame("tu", 2, 2));
            assert!(!c.should_skip_surrounding_frame("tu", 2, 2, false, false));

            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), false);
            c.mark_action();
            c.apply_to_sink(1, "ự", 2, 0, &mut sink);

            // Duplicate stale echo still reaches the correction path, but
            // the correction now uses ForwardKey Backspace instead of
            // delete_surrounding_text.
            assert_eq!(sink.deletes.len(), 1);
            assert_eq!(sink.vk_keys.len(), 2);
            assert_eq!(sink.vk_keys[0], (0, 14, KeyState::Pressed));
            assert_eq!(sink.vk_keys[1], (0, 14, KeyState::Released));
        }

        #[test]
        fn stale_mode_keeps_using_forward_key_until_a_fresh_echo() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), true);
            c.mark_action();
            c.apply_to_sink(1, "ư", 1, 0, &mut sink);

            // Stale echo arms firefox quirk mode.
            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), false);

            c.mark_action();
            c.apply_to_sink(1, "ự", 2, 0, &mut sink);
            c.mark_action();
            c.apply_to_sink(1, "ữ", 3, 0, &mut sink);

            // Initial correction uses surrounding delete; stale-mode corrections
            // keep using ForwardKey until a healthy echo arrives.
            assert_eq!(sink.deletes.len(), 1);
            assert_eq!(sink.vk_keys.len(), 4);
            assert_eq!(sink.vk_keys[0], (0, 14, KeyState::Pressed));
            assert_eq!(sink.vk_keys[1], (0, 14, KeyState::Released));
            assert_eq!(sink.vk_keys[2], (0, 14, KeyState::Pressed));
            assert_eq!(sink.vk_keys[3], (0, 14, KeyState::Released));
        }

        #[test]
        fn fresh_surrounding_echo_keeps_byte_delete() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), true);
            c.mark_action();
            c.apply_to_sink(1, "ư", 1, 0, &mut sink);

            c.observe_surrounding_bytes("tư", ByteCursor(3), ByteCursor(3), false);
            c.mark_action();
            c.apply_to_sink(1, "ự", 2, 0, &mut sink);

            // Fresh echo preserves byte-count delete → before_bytes = 2.
            assert_eq!(sink.deletes[1], (2, 1, 0, 0));
        }

        #[test]
        fn space_after_stale_echo_forwards_raw_and_sticky_mode_persists() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), true);
            c.mark_action();
            c.apply_to_sink(1, "ư", 1, 0, &mut sink);
            c.observe_surrounding_bytes("tu", ByteCursor(2), ByteCursor(2), false);

            // Space is forwarded raw. It clears the one-shot char-delete state
            // (`firefox.clear()`), but `forward_sticky` deliberately survives
            // word boundaries: a widget that produced one stale echo is assumed
            // stale for its whole lifetime, so later corrections keep using
            // ForwardKey. Sticky mode only resets with the Composer (recreated
            // on activate) via `full_reset`.
            assert!(matches!(c.feed_key(' '), KeyDecision::ForwardRaw));
        }

        #[test]
        fn forwarded_char_after_stale_echo_resets_char_delete_before_next_correction() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("d", ByteCursor(1), ByteCursor(1), true);
            for (ch, text) in [('o', "do"), ('e', "doe")] {
                c.mark_action();
                assert!(matches!(c.feed_key(ch), KeyDecision::ForwardRaw));
                c.observe_surrounding_bytes(
                    text,
                    ByteCursor(text.len() as u32),
                    ByteCursor(text.len() as u32),
                    false,
                );
            }

            c.mark_action();
            // doe + s in Telex: backspaces=1, commit="é" → shadow becomes "doé"
            match c.feed_key('s') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => c.apply_to_sink(backspaces, &commit, 1, 0, &mut sink),
                _ => panic!("expected stale-echo setup tone edit"),
            }
            c.observe_surrounding_bytes("doe", ByteCursor(3), ByteCursor(3), false);

            // A forwarded raw key changes the buffer, so the one-shot char-count
            // assumption no longer applies — reset to byte deletes.
            c.mark_action();
            assert!(matches!(c.feed_key('n'), KeyDecision::ForwardRaw));

            c.mark_action();
            match c.feed_key('t') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 4);
                    assert_eq!(commit, "doesnt");
                    c.apply_to_sink(backspaces, &commit, 2, 0, &mut sink);
                }
                _ => panic!("expected deconversion to raw English word"),
            }

            assert_eq!(sink.deletes.last(), Some(&(5, 4, 0, 0)));
        }

        #[test]
        fn retroactive_stale_echo_keeps_byte_delete_for_multibyte_tone_updates() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();
            let text = "cà phê";
            let cursor = "cà".len() as u32;

            c.observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false);

            c.mark_action();
            match c.feed_key('r') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ả");
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                _ => panic!("expected retroactive tone replacement"),
            }

            // Firefox contenteditable can echo the old surrounding buffer with the
            // cursor shifted past the edit. This must not switch retroactive edits
            // to char-count deletion, because the v1 path still needs byte counts
            // to delete the multibyte Vietnamese character before committing again.
            c.observe_surrounding_bytes(text, ByteCursor("cà ".len() as u32), ByteCursor("cà ".len() as u32), false);

            c.mark_action();
            match c.feed_key('j') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ạ");
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                _ => panic!("expected second retroactive tone replacement"),
            }

            assert_eq!(sink.deletes[1], (3, 1, 0, 0));
        }

        #[test]
        fn retroactive_stale_cursor_into_next_word_uses_forward_key_delete() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            // Cursor moved to the end of the first word in a two-word phrase.
            c.observe_surrounding_bytes(
                "là lạ",
                ByteCursor("là".len() as u32),
                ByteCursor("là".len() as u32),
                false,
            );

            // First retroactive tone replacement on the first word: là -> lạ.
            c.mark_action();
            match c.feed_key('j') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ạ");
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                _ => panic!("expected first retroactive tone replacement"),
            }
            assert_eq!(c.shadow_text(), "lạ");

            // Firefox stale echo can shift cursor into the next word while
            // still reporting old surrounding text. Historically this armed
            // char-count delete and caused forward deletion into the second
            // word. We now bypass delete_surrounding_text and emit ForwardKey BS.
            c.observe_surrounding_bytes(
                "lạ lạ",
                ByteCursor("lạ l".len() as u32),
                ByteCursor("lạ l".len() as u32),
                false,
            );

            c.mark_action();
            match c.feed_key('r') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ả");
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                _ => panic!("expected second retroactive tone replacement"),
            }

            // First correction used SurroundingText delete; second correction
            // (after stale cursor shift) must use ForwardKey delete instead.
            assert_eq!(sink.deletes.len(), 1);
            assert_eq!(sink.vk_keys.len(), 2);
            assert_eq!(sink.vk_keys[0], (0, 14, KeyState::Pressed));
            assert_eq!(sink.vk_keys[1], (0, 14, KeyState::Released));
            assert_eq!(c.shadow_text(), "lả");
        }

        #[test]
        fn stale_firefox_echo_marks_commit_string_unusable_before_forward_key() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("la", ByteCursor(2), ByteCursor(2), true);
            c.mark_action();
            c.apply_to_sink(1, "ả", 1, 0, &mut sink);
            assert!(c.commit_string_functional);

            c.observe_surrounding_bytes(
                "lạ l",
                ByteCursor("lạ l".len() as u32),
                ByteCursor("lạ l".len() as u32),
                false,
            );

            assert!(c.firefox.use_forward_delete());
            assert!(!c.commit_string_functional);
        }

        #[test]
        fn full_reset_preserves_commit_string_ratchet_for_composer_lifetime() {
            // `commit_string_functional` is a ONE-WAY ratchet: once a widget
            // proves it ignores the text-input-v3 server-event contract, no
            // in-session event may flip it back — the breakage is per-widget
            // and permanent, and one flapped correction doubles a word.
            // Recovery is scoped to Composer recreation on activate. This pin
            // fails if someone "fixes" full_reset to restore the flag; that
            // change must instead be paired with removing the
            // Composer-per-activate invariant documented on the field.
            let mut c =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("la", ByteCursor(2), ByteCursor(2), true);
            c.mark_action();
            c.apply_to_sink(1, "ả", 1, 0, &mut sink);
            c.observe_surrounding_bytes(
                "lạ l",
                ByteCursor("lạ l".len() as u32),
                ByteCursor("lạ l".len() as u32),
                false,
            );
            assert!(!c.commit_string_functional);

            c.full_reset();

            assert!(
                !c.commit_string_functional,
                "ratchet must survive full_reset; recovery is Composer recreation only"
            );
        }

        #[test]
        fn sticky_forward_mode_keeps_delete_channel_after_first_stale_hit() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            c.observe_surrounding_bytes("la", ByteCursor(2), ByteCursor(2), true);

            // Arm Firefox quirk stale mode once, as seen in field logs where
            // one correction flips to ForwardKey and following corrections
            // previously flapped back to delete_surrounding_text.
            c.firefox.record_expected_echo("lả".to_owned(), "l".to_owned());
            c.firefox.observe_surrounding("lạ l", true, false);
            assert!(c.firefox.use_forward_delete());

            c.mark_action();
            c.apply_to_sink(1, "ả", 1, 0, &mut sink);

            // Quirk can transiently clear after unrelated surrounding frames,
            // but sticky mode must keep delete on ForwardKey for this object.
            c.firefox.clear();
            c.mark_action();
            c.apply_to_sink(1, "ạ", 2, 0, &mut sink);

            assert_eq!(sink.deletes.len(), 0);
            assert_eq!(sink.vk_keys.len(), 4);
            assert_eq!(sink.vk_keys[0], (0, 14, KeyState::Pressed));
            assert_eq!(sink.vk_keys[1], (0, 14, KeyState::Released));
            assert_eq!(sink.vk_keys[2], (0, 14, KeyState::Pressed));
            assert_eq!(sink.vk_keys[3], (0, 14, KeyState::Released));
        }

        #[test]
        fn recent_implausible_surrounding_echo_does_not_clobber_shadow() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

            c.mark_action();
            // First key 'i' (shape is path-dependent; this test is about the echo,
            // not the key). The implausible "ii" cursor=0 frame must be dropped so
            // shadow stays "i" and the next tone key composes against it.
            let _ = c.feed_key('i');
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
            // Chromium omnibox autocomplete: user types "tra", the omnibox expands
            // to "translate" with "nslate" selected (cursor=3, anchor=9). That frame
            // arrives within the post-keystroke `recent_action` window and is not a
            // one-char insertion, but its before-cursor prefix "tra" == our shadow,
            // so it is shadow_confirmed and must be trusted — it carries the
            // selection the Tier-1 fallback (surrounding::apply) needs.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

            // Type "tra" so the shadow holds the prefix the autocomplete selects past.
            for ch in "tra".chars() {
                c.mark_action();
                assert!(matches!(c.feed_key(ch), KeyDecision::ForwardRaw));
            }
            assert_eq!(c.shadow_text(), "tra");

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
        fn recent_external_delete_reseeds_before_next_key() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

            c.observe_surrounding_bytes("work", ByteCursor(4), ByteCursor(4), true);
            // An EXTERNAL delete is by definition not our own recent action — the
            // user deleted via some other channel. Model that with `defer_action`
            // (rolls the action clock back) so the delete frame is trusted and the
            // engine resets. A delete frame inside the recent-action window
            // is instead our own delete_surrounding_text echo and must be dropped.
            c.defer_action();
            c.observe_surrounding_bytes("wor", ByteCursor(3), ByteCursor(3), false);

            assert_eq!(c.shadow_text(), "wor");
            // After the external delete, 'd' continues the English word: the shadow
            // word "wor" is render-gated (telex-misreads, so no seed) and 'd' is
            // forwarded raw → "word", not recomposed into the deleted word.
            match c.feed_key('d') {
                KeyDecision::ForwardRaw => {}
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    panic!("expected raw 'd', got edit bs={backspaces} commit={commit:?}");
                }
                KeyDecision::Consumed => panic!("expected raw 'd', got consumed"),
            }
        }

        /// Reproduces the Firefox contenteditable bug: cursor mid-word in
        /// "sword", user deletes 'w' then types 'ww' to get raw 'w' back.
        /// First 'w' composes ư (seeded from "s"), second 'w' reverts via
        /// bs=1 → delete_surrounding_text targets a multibyte char (ư = 2
        /// bytes). Firefox misinterprets the byte-based delete as character-
        /// based, eating 2 chars ("sư" = 3 bytes) instead of 2 bytes ("ư").
        ///
        /// The fix: when the delete backspaces over a multibyte char produced
        /// from retroactive context seeding, use ForwardKey instead of
        /// delete_surrounding_text — physical Backspace always removes
        /// exactly 1 character regardless of byte width.
        #[test]
        fn ww_undo_after_retroactive_seed_uses_forward_key() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();

            // Simulate: text="sword " cursor=1 (cursor between 's' and 'word')
            // User deleted 'w' externally, arriving as text="sord" cursor=1
            c.observe_surrounding_bytes("sord ", ByteCursor(1), ByteCursor(1), true);

            // First 'w': engine seeds from shadow "s", produces ư
            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 0);
                    assert_eq!(commit, "ư");
                    c.apply_to_sink(backspaces, &commit, 1, 0, &mut sink);
                }
                other => panic!("expected Apply for first 'w', got {:?}", std::mem::discriminant(&other)),
            }

            // Second 'w': engine reverts ư→w (Telex ww escape)
            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1, "engine wants to delete 1 char (ư)");
                    assert_eq!(commit, "w");
                    c.apply_to_sink(backspaces, &commit, 2, 0, &mut sink);

                    // The delete targeted ư (2 bytes, 1 char). On Firefox
                    // contenteditable this MUST use ForwardKey, not
                    // delete_surrounding_text, because Firefox mis-counts
                    // byte offsets as characters.
                    assert!(
                        !sink.vk_keys.is_empty(),
                        "second 'w' revert must use ForwardKey (physical BS), \
                         not delete_surrounding_text — Firefox contenteditable \
                         misinterprets byte offsets as char offsets for multibyte \
                         deletions, eating the seeded context 's' as well"
                    );
                }
                other => panic!("expected Apply for second 'w', got {:?}", std::mem::discriminant(&other)),
            }
        }

    }

    mod surrounding_words {
        use super::*;

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
        fn comma_separates_vietnamese_syllables() {
            let text = "xin,chào";
            assert_eq!(current_word_before_cursor(text, text.len() as u32), "chào");
        }

        #[test]
        fn delimiters_separate_current_vietnamese_word() {
            assert_eq!(
                current_word_before_cursor("(tiếng", "(tiếng".len() as u32),
                "tiếng"
            );
            assert_eq!(
                current_word_before_cursor("anh/chị", "anh/chị".len() as u32),
                "chị"
            );
            assert_eq!(
                current_word_before_cursor("“đẹp", "“đẹp".len() as u32),
                "đẹp"
            );
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

    }

    mod retroactive_context {
        use super::*;

        // Kate, Telex: live typing is continuous composition — never
        // confirm-deferred, no raw-key flash. "kww" composes ư then reverts
        // to "kw"; "kwin" auto-restores to raw via normal engine state.
        #[test]
        fn kate_kww_reverts_to_kw() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);

            c.observe_surrounding_bytes("", ByteCursor(0), ByteCursor(0), false);

            c.mark_action();
            assert!(matches!(c.feed_key('k'), KeyDecision::ForwardRaw));
            assert!(c
                .observe_surrounding_bytes("k", ByteCursor(1), ByteCursor(1), false)
                .is_none());

            // 'w' composes ư immediately: live typing, no deferral.
            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply { backspaces, commit, .. } => {
                    assert_eq!(backspaces, 0);
                    assert_eq!(commit, "ư");
                }
                _ => panic!("live 'w' after k must compose ư immediately"),
            }
            let ku = "kư";
            c.observe_surrounding_bytes(ku, ByteCursor(ku.len() as u32), ByteCursor(ku.len() as u32), false);

            // Second 'w' reverts ư → w through continuous engine state.
            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply { backspaces, commit, .. } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "w");
                }
                _ => panic!("second 'w' must revert kư to kw"),
            }
        }

        // Live "kwin" must end raw: ư composes on 'w', then the invalid
        // Vietnamese sequence triggers the engine's raw auto-restore. No
        // deferral, no flash — same behavior as before the gedit fix.
        #[test]
        fn live_kwin_auto_restores_to_raw() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);

            c.observe_surrounding_bytes("", ByteCursor(0), ByteCursor(0), false);
            c.mark_action();
            assert!(matches!(c.feed_key('k'), KeyDecision::ForwardRaw));
            c.observe_surrounding_bytes("k", ByteCursor(1), ByteCursor(1), false);

            c.mark_action();
            assert!(matches!(c.feed_key('w'), KeyDecision::Apply { .. }));
            let ku = "kư";
            c.observe_surrounding_bytes(ku, ByteCursor(ku.len() as u32), ByteCursor(ku.len() as u32), false);

            // 'i' then 'n': the engine restores raw "kwin".
            c.mark_action();
            let mut screen = String::from("kư");
            for ch in ['i', 'n'] {
                c.mark_action();
                match c.feed_key(ch) {
                    KeyDecision::Apply { backspaces, commit, .. } => {
                        for _ in 0..backspaces {
                            screen.pop();
                        }
                        screen.push_str(&commit);
                    }
                    KeyDecision::ForwardRaw => screen.push(ch),
                    _ => panic!("unexpected decision for {ch}"),
                }
                let cur = screen.len() as u32;
                c.observe_surrounding_bytes(&screen, ByteCursor(cur), ByteCursor(cur), false);
            }
            assert_eq!(screen, "kwin");
        }

        // Ghostty advertises surrounding-text but every frame is empty; the
        // liveness watchdog downgrades ST→FK after the first keys. A raw key
        // forwarded on the ST tier is still pending at that moment. The
        // downgrade must clear it — no frame will ever confirm it — or every
        // subsequent feed_key sees prior_key_unconfirmed and resets the
        // engine, killing composition until a non-printable key (Ctrl+C).
        #[test]
        fn tier_downgrade_clears_pending_raw_key_so_composition_survives() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);

            // First keys forwarded raw on the ST tier → pending set.
            c.mark_action();
            assert!(matches!(c.feed_key('t'), KeyDecision::ForwardRaw));

            // Watchdog fires: empty frames despite commits.
            c.set_method(BackspaceMethod::ForwardKey);

            // Composition must work immediately: "taa" → â on second 'a'.
            c.feed_key('a');
            match c.feed_key('a') {
                KeyDecision::Apply { backspaces, commit, .. } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "â");
                }
                _ => panic!("second 'a' must compose â after tier downgrade"),
            }
        }

        

        #[test]
        fn backspace_on_surrounding_context_forwards_instead_of_deleting_at_shadow_cursor() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let text = "tiếng việt";
            let cursor = "tiếng".len() as u32;
            c.observe_surrounding_bytes(
                text,
                ByteCursor(cursor),
                ByteCursor(cursor),
                false,
            );

            assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
        }

        #[test]
        fn overlapping_raw_keys_degrade_to_sync_without_repair() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let baseline = "tiếng";
            let cursor = baseline.len() as u32;
            c.observe_surrounding_bytes(
                baseline,
                ByteCursor(cursor),
                ByteCursor(cursor),
                false,
            );

            c.mark_action();
            assert!(matches!(c.feed_key('r'), KeyDecision::ForwardRaw));
            c.mark_action();
            assert!(matches!(c.feed_key('r'), KeyDecision::ForwardRaw));

            // This can only be the first key's frame. Because another key is
            // already in flight, it may synchronize but must not repair.
            let first = "tiếngr";
            let first_cursor = first.len() as u32;
            assert!(c
                .observe_surrounding_bytes(
                    first,
                    ByteCursor(first_cursor),
                    ByteCursor(first_cursor),
                    false,
                )
                .is_none());
            assert_eq!(c.prev_surrounding_for_trace(), (first, first_cursor));
        }

        #[test]
        fn equal_trailing_character_without_insertion_never_triggers_repair() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let text = "bar car";
            c.observe_surrounding_bytes(text, ByteCursor(3), ByteCursor(3), false);
            c.reset_composition_preserving_surrounding();

            c.mark_action();
            assert!(matches!(c.feed_key('r'), KeyDecision::ForwardRaw));

            // Cursor moved to another existing word ending in the same letter,
            // but the document contains no inserted key. Suffix equality alone
            // must not authorize a positional repair.
            assert!(c
                .observe_surrounding_bytes(text, ByteCursor(7), ByteCursor(7), false)
                .is_none());
            assert_eq!(c.prev_surrounding_for_trace(), (text, 7));
        }

        #[test]
        fn telex_bracket_shortcut_composes_live_when_frame_word_fails_gate() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, true);
            c.set_surrounding_confirmation(true);
            // Frame word "t" fails the round-trip seed gate, so no frame
            // context exists; '[' is live composition and must apply
            // immediately (ơ) with no confirm-deferral flash.
            c.observe_surrounding_bytes("t", ByteCursor(1), ByteCursor(1), false);

            c.mark_action();
            match c.feed_key('[') {
                KeyDecision::Apply { backspaces, commit, .. } => {
                    assert_eq!(backspaces, 0);
                    assert_eq!(commit, "ơ");
                }
                _ => panic!("live '[' must compose ơ immediately"),
            }
        }

        #[test]
        fn vni_tone_key_on_surrounding_context_waits_for_client_confirmation() {
            let mut c = Composer::new(InputMethod::Vni, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let text = "tiếng";
            let cursor = text.len() as u32;
            c.observe_surrounding_bytes(
                text,
                ByteCursor(cursor),
                ByteCursor(cursor),
                false,
            );

            c.mark_action();
            assert!(matches!(c.feed_key('3'), KeyDecision::ForwardRaw));
            let raw = "tiếng3";
            let raw_cursor = raw.len() as u32;
            assert!(c
                .observe_surrounding_bytes(
                    raw,
                    ByteCursor(raw_cursor),
                    ByteCursor(raw_cursor),
                    false,
                )
                .is_some());
        }

        #[test]
        fn word_boundary_seed_established_during_key_still_requires_confirmation() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let text = "tiếng";
            let cursor = text.len() as u32;
            c.observe_surrounding_bytes(
                text,
                ByteCursor(cursor),
                ByteCursor(cursor),
                false,
            );

            // Model an uncertain idle reset that retained synchronized shadow.
            // The key itself performs the word-boundary seed; confirmation must
            // be decided after that seed, not from the pre-key flag alone.
            c.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(c.check_idle_reset());
            c.retroactive_context = false;
            c.mark_action();
            assert!(matches!(c.feed_key('r'), KeyDecision::ForwardRaw));

            let raw = "tiếngr";
            let raw_cursor = raw.len() as u32;
            let repair = c
                .observe_surrounding_bytes(
                    raw,
                    ByteCursor(raw_cursor),
                    ByteCursor(raw_cursor),
                    false,
                )
                .expect("confirmed raw tone key should repair from retained shadow");
            assert_eq!(repair.backspaces, 4);
            assert_eq!(repair.commit, "ểng");
        }

        #[test]
        fn unchanged_pre_action_frame_cannot_seed_a_positional_edit() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let baseline = "tiệng việt mĩ miều";
            let old_cursor = "tiệng".len() as u32;

            c.observe_surrounding_bytes(
                baseline,
                ByteCursor(old_cursor),
                ByteCursor(old_cursor),
                false,
            );

            // A forwarded cursor/editing action resets composition but keeps
            // the last confirmed application snapshot. KWin may re-emit that
            // unchanged snapshot before gedit processes the forwarded action;
            // it must remain a duplicate, never destination context.
            c.reset_composition_preserving_surrounding();
            c.defer_action();
            assert!(c.should_skip_surrounding_frame(
                baseline,
                old_cursor,
                old_cursor,
                false,
                false,
            ));

            // No destination frame arrived. Even though stale "tiệng" is the
            // last confirmed word, a positional tone edit is unsafe: forward
            // the key so gedit reports its actual insertion point.
            c.mark_action();
            assert!(matches!(c.feed_key('r'), KeyDecision::ForwardRaw));

            // The raw key actually landed after the second word. This frame is
            // authoritative; any repair must target "việt", not "tiệng".
            let actual = "tiệng việtr mĩ miều";
            let actual_cursor = "tiệng việtr".len() as u32;
            let repair = c.observe_surrounding_bytes(
                actual,
                ByteCursor(actual_cursor),
                ByteCursor(actual_cursor),
                false,
            );
            let repair = repair.expect("actual insertion frame should finish the transaction");
            assert_eq!(repair.backspaces, 3);
            assert_eq!(repair.commit, "eejtr");
            assert_eq!(c.prev_surrounding_for_trace(), (actual, actual_cursor));
        }

        #[test]
        fn stale_ctrl_backspace_frame_does_not_poison_retyped_vietnamese_word() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let mut sink = DeleteCaptureSink::default();
            let stale = "tiệng vieetjr mĩ miều";
            let stale_cursor = "tiệng vieetjr".len() as u32;

            c.observe_surrounding_bytes(
                stale,
                ByteCursor(stale_cursor),
                ByteCursor(stale_cursor),
                false,
            );
            c.reset_composition_preserving_surrounding();
            c.defer_action();
            assert!(c.should_skip_surrounding_frame(
                stale,
                stale_cursor,
                stale_cursor,
                false,
                false,
            ));

            // Ctrl+Backspace has already removed "vieetjr" in gedit. The first
            // retyped letter proves the real insertion point and replaces the
            // stale snapshot as ground truth.
            c.mark_action();
            assert!(matches!(c.feed_key('v'), KeyDecision::ForwardRaw));
            let v_text = "tiệng v mĩ miều";
            let v_cursor = "tiệng v".len() as u32;
            assert!(c
                .observe_surrounding_bytes(
                    v_text,
                    ByteCursor(v_cursor),
                    ByteCursor(v_cursor),
                    false,
                )
                .is_none());
            assert_eq!(c.shadow_text(), "tiệng v");

            // Continue from surrounding-confirmed context. Raw letters sync;
            // transformations are inserted raw first, then repaired from their
            // confirming frame so later cursor movement cannot race an edit.
            for (ch, text) in [('i', "tiệng vi mĩ miều"), ('e', "tiệng vie mĩ miều")] {
                c.mark_action();
                assert!(matches!(c.feed_key(ch), KeyDecision::ForwardRaw));
                let cursor = text.split(" mĩ").next().unwrap().len() as u32;
                assert!(c
                    .observe_surrounding_bytes(
                        text,
                        ByteCursor(cursor),
                        ByteCursor(cursor),
                        false,
                    )
                    .is_none());
            }

            c.mark_action();
            assert!(matches!(c.feed_key('e'), KeyDecision::ForwardRaw));
            let raw_ee = "tiệng viee mĩ miều";
            let raw_ee_cursor = "tiệng viee".len() as u32;
            let circumflex = c
                .observe_surrounding_bytes(
                    raw_ee,
                    ByteCursor(raw_ee_cursor),
                    ByteCursor(raw_ee_cursor),
                    false,
                )
                .expect("confirmed raw e should repair viee to viê");
            c.apply_confirmed_repair_to_sink(
                circumflex.backspaces,
                &circumflex.commit,
                0,
                0,
                &mut sink,
            );
            let composed_e = "tiệng viê mĩ miều";
            let composed_e_cursor = "tiệng viê".len() as u32;
            c.observe_surrounding_bytes(
                composed_e,
                ByteCursor(composed_e_cursor),
                ByteCursor(composed_e_cursor),
                false,
            );

            c.mark_action();
            assert!(matches!(c.feed_key('t'), KeyDecision::ForwardRaw));
            let raw_t = "tiệng viêt mĩ miều";
            let raw_t_cursor = "tiệng viêt".len() as u32;
            assert!(c
                .observe_surrounding_bytes(
                    raw_t,
                    ByteCursor(raw_t_cursor),
                    ByteCursor(raw_t_cursor),
                    false,
                )
                .is_none());

            c.mark_action();
            assert!(matches!(c.feed_key('j'), KeyDecision::ForwardRaw));
            let raw_j = "tiệng viêtj mĩ miều";
            let raw_j_cursor = "tiệng viêtj".len() as u32;
            let tone = c
                .observe_surrounding_bytes(
                    raw_j,
                    ByteCursor(raw_j_cursor),
                    ByteCursor(raw_j_cursor),
                    false,
                )
                .expect("confirmed raw j should repair viêtj to việt");
            c.apply_confirmed_repair_to_sink(
                tone.backspaces,
                &tone.commit,
                0,
                0,
                &mut sink,
            );
            assert_eq!(c.shadow_text(), "tiệng việt");
        }

        // gedit under KWin (text-input-v1, SurroundingText tier) sends NO
        // surrounding_text frame for a pure cursor move (Ctrl+Right to the end
        // of "tiếng|"). The frame only arrives fused with — and one wire event
        // AFTER — the first typed key, which daklak has already forwarded raw
        // because the engine was never seeded ("tiếng" + raw 'r' = "tiếngr").
        // Without recovery this closes a loop: every skipped frame starves
        // prev_text so the engine never resyncs and tone keys pile up as raw
        // text ("tiếngrjrjrj…"). The revealed frame is the ground truth; when it
        // is exactly our forwarded run glued to a real word, retro-compose it.
        #[test]
        fn gedit_late_fused_frame_retro_composes_forwarded_tone_key() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let mut sink = DeleteCaptureSink::default();

            // Last confirmed frame was near the start of the word. Ctrl+Right
            // resets composition but preserves that application snapshot; no
            // destination frame follows.
            let baseline = "tiếng việt mĩ miều";
            c.observe_surrounding_bytes(
                baseline,
                ByteCursor(1),
                ByteCursor(1),
                false,
            );
            c.reset_composition_preserving_surrounding();
            c.defer_action();

            // The 'r' keystroke marks an action, then forwards raw (no seed).
            c.mark_action();
            assert!(matches!(c.feed_key('r'), KeyDecision::ForwardRaw));
            assert_eq!(c.shadow_text(), "r");

            // The fused frame lands ~1 wire event later, already containing the
            // forwarded 'r' at the end of "tiếng": text="tiếngr…" cursor=8.
            let text = "tiếngr việt mĩ miều";
            let cursor = "tiếngr".len() as u32; // 8
            let edit = c
                .observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false)
                .expect("late fused frame should yield a retro-compose repair");
            // Delete the already-forwarded "ếngr" (4 chars) and commit "ểng":
            // "tiếngr" → "tiểng" (hỏi tone on "tiếng").
            assert_eq!(edit.backspaces, 4);
            assert_eq!(edit.commit, "ểng");

            // The transport applies the repair; shadow becomes "tiểng…".
            c.apply_confirmed_repair_to_sink(
                edit.backspaces,
                &edit.commit,
                0,
                0,
                &mut sink,
            );
            assert!(
                !c.firefox.has_pending_echo(),
                "KWin repair echoes must not enter Firefox stale classification"
            );
            let corrected = "tiểng việt mĩ miều";
            let corrected_cursor = "tiểng".len() as u32;
            c.observe_surrounding_bytes(
                corrected,
                ByteCursor(corrected_cursor),
                ByteCursor(corrected_cursor),
                false,
            );

            // Every surrounding-derived positional edit follows the same safe
            // transaction. The raw key may flash for one client frame, but the
            // repair cannot land at an unreported mouse/keyboard cursor.
            c.mark_action();
            assert!(matches!(c.feed_key('j'), KeyDecision::ForwardRaw));
            let raw_j = "tiểngj việt mĩ miều";
            let raw_j_cursor = "tiểngj".len() as u32;
            let tone = c
                .observe_surrounding_bytes(
                    raw_j,
                    ByteCursor(raw_j_cursor),
                    ByteCursor(raw_j_cursor),
                    false,
                )
                .expect("confirmed raw j should yield a tone repair");
            assert_eq!(tone.backspaces, 4);
            assert_eq!(tone.commit, "ệng");
        }

        // The same late-fused-frame shape must NOT fabricate an edit when the
        // revealed word is English that Telex can't round-trip: navigating into
        // "wor|" and typing 'd' reveals "word". The gate rejects "wor", so we
        // only resync the shadow (ending the starvation loop) and forward 'd'
        // raw — no phantom delete/commit.
        #[test]
        fn gedit_late_fused_frame_leaves_english_word_raw() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);

            c.full_reset();
            c.defer_action();
            c.mark_action();
            assert!(matches!(c.feed_key('d'), KeyDecision::ForwardRaw));

            let text = "word";
            let cursor = text.len() as u32;
            let edit =
                c.observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false);
            assert!(edit.is_none(), "English word must not be retro-composed");
            // Shadow resynced to ground truth so the loop can't keep starving.
            assert_eq!(c.shadow_text(), "word");
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
        fn idle_reset_seeds_composed_shadow_for_tone_replacement() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let text = "là";

            c.observe_surrounding_bytes(
                text,
                ByteCursor(text.len() as u32),
                ByteCursor(text.len() as u32),
                true,
            );
            c.last_keystroke_at -= std::time::Duration::from_secs(3);

            assert!(c.check_idle_reset());
            match c.feed_key('s') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "á");
                }
                _ => panic!("expected tone replacement after idle reset"),
            }
        }

        #[test]
        fn cursor_jump_after_composed_vowel_keeps_plain_consonant_continuation_raw() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let text = "raw khôn";
            let cursor = text.len() as u32;

            c.observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false);

            // 'g' is a plain continuation of "khôn" → forwarded raw. The engine
            // keeps "không" as running context, so a following tone key still edits
            // it (proving the word survived the jump-in without a raw trail).
            assert!(matches!(c.feed_key('g'), KeyDecision::ForwardRaw));
            match c.feed_key('s') {
                KeyDecision::Apply { commit, .. } => assert_eq!(commit, "ống"),
                _ => panic!("expected tone edit on kept 'không'"),
            }
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

            // 'i' makes the syllable un-composable → raw restore. On the continuous
            // engine the restore is the full keystroke history (seed "six" + typed
            // "sxi" = "sixsxi"), not the reset-per-key path's cleaner "sixi". Both
            // are just a literal echo of a nonsense sequence; the continuous form is
            // what IBus/wayland now produce.
            match c.feed_key('i') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 2);
                    assert_eq!(commit, "sixsxi");
                }
                _ => panic!("expected raw restore"),
            }
        }

        #[test]
        fn forwarded_backspace_after_telex_u_w_drops_deleted_raw_context() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();
            let mut visible = "đoạn trước ".to_owned();
            let cursor = visible.len() as u32;
            c.observe_surrounding_bytes(&visible, ByteCursor(cursor), ByteCursor(cursor), true);

            for ch in ['u', 'w'] {
                c.mark_action();
                match c.feed_key(ch) {
                    KeyDecision::Apply {
                        backspaces, commit, ..
                    } => {
                        for _ in 0..backspaces {
                            visible.pop();
                        }
                        visible.push_str(&commit);
                        c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                    }
                    _ => panic!("expected Telex edit for {ch:?}"),
                }
            }
            assert!(visible.ends_with('ư'));

            c.mark_action();
            assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
            visible.pop();
            assert_eq!(visible, "đoạn trước ");

            c.mark_action();
            assert!(matches!(c.feed_key('s'), KeyDecision::ForwardRaw));
            visible.push('s');

            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    for _ in 0..backspaces {
                        visible.pop();
                    }
                    visible.push_str(&commit);
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                _ => panic!("expected Telex edit for 'w'"),
            }
            assert_eq!(visible, "đoạn trước sư");

            c.mark_action();
            assert!(matches!(c.feed_key('i'), KeyDecision::ForwardRaw));
            visible.push('i');

            c.mark_action();
            match c.feed_key('t') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 3);
                    assert_eq!(commit, "swit");
                }
                _ => panic!("expected raw restore to English word"),
            }
        }

        #[test]
        fn retroactive_tone_toggle_then_consonant_restores_last_raw_form_only() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();
            let suffix = " lá la";
            let mut word = "là".to_owned();

            c.observe_surrounding_bytes(
                "là lá la",
                ByteCursor(word.len() as u32),
                ByteCursor(word.len() as u32),
                false,
            );

            for (ch, expected_word) in [
                ('r', "lả"),
                ('j', "lạ"),
                ('f', "là"),
                ('j', "lạ"),
                ('r', "lả"),
                ('s', "lá"),
                ('j', "lạ"),
                ('r', "lả"),
                ('f', "là"),
            ] {
                c.mark_action();
                match c.feed_key(ch) {
                    KeyDecision::Apply {
                        backspaces, commit, ..
                    } => {
                        assert_eq!(backspaces, 1);
                        c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                        word = expected_word.to_owned();
                        let text = format!("{word}{suffix}");
                        c.observe_surrounding_bytes(
                            &text,
                            ByteCursor(word.len() as u32),
                            ByteCursor(word.len() as u32),
                            false,
                        );
                    }
                    _ => panic!("expected tone update for {ch:?}"),
                }
            }

            c.mark_action();
            match c.feed_key('d') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 2);
                    assert_eq!(commit, "lafd");
                }
                _ => panic!("expected raw restore for final consonant"),
            }
        }

        #[test]
        fn external_deletion_clears_stale_separator_so_next_tone_key_seeds() {
            // User types "la " (trailing space → last_input_char = ' '), the
            // client echoes it, then the user externally deletes the space
            // (mouse select + cut — no Backspace through the IME). The
            // deletion frame resets the engine; the keyboard history must be
            // invalidated with it. If a stale separator survives, the
            // word-boundary seed in feed_key is skipped and the next tone key
            // forwards raw ("laf") instead of composing against the word the
            // cursor now touches ("là").
            let mut c =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

            c.observe_surrounding_bytes("", ByteCursor(0), ByteCursor(0), true);
            for ch in ['l', 'a', ' '] {
                assert!(matches!(c.feed_key(ch), KeyDecision::ForwardRaw));
            }
            assert_eq!(c.shadow_text(), "la ");

            // Client echoes the typed text (outside the recent-action window;
            // no mark_action in this test). Shadow-confirmed → trusted, no
            // engine change.
            c.observe_surrounding_bytes("la ", ByteCursor(3), ByteCursor(3), false);

            // External deletion of the trailing space: text shrank → engine
            // reset — and the stale ' ' in last_input_char must go with it.
            c.observe_surrounding_bytes("la", ByteCursor(2), ByteCursor(2), false);

            match c.feed_key('f') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "à");
                }
                _ => panic!("expected tone edit on 'la'; stale separator blocked the seed"),
            }
        }

        #[test]
        fn forwarded_ascii_backspace_on_seeded_word_stays_consistent() {
            // Cursor placed after "la" in "la world"; the engine is seeded
            // retroactively from "la". A forwarded ASCII backspace (deleting
            // 'a') keeps engine history by design — vnkey's process_backspace
            // decrements its internal buffer in the same pass, so the engine
            // now holds "l", matching the popped shadow. The next 'f' has no
            // vowel to tone and must forward raw (visible text "lf world"),
            // NOT compose against a stale "la" seed (which would emit a bogus
            // delete+"à" edit).
            let mut c =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

            c.observe_surrounding_bytes("la world", ByteCursor(2), ByteCursor(2), true);
            assert!(c.retroactive_context);

            c.mark_action();
            assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
            assert_eq!(c.shadow_text(), "l");

            c.mark_action();
            assert!(
                matches!(c.feed_key('f'), KeyDecision::ForwardRaw),
                "'f' after deleting 'a' must forward raw, not tone a stale seed"
            );
        }

        #[test]
        fn refresh_after_apply_seeds_current_word_not_whole_shadow() {
            // User clicks after "ơ" in existing text "wor ơ x" (an English word
            // precedes the Vietnamese one) and toggles tones on "ơ". After the
            // first toggle the refresh reseed must feed ONLY the current word
            // ("ở") like the initial seed does — feeding the whole shadow
            // ("wor ở") fails the render gate ("wor" telex-misreads as "wỏ"),
            // wrongly dropping retroactive_context. The Firefox quirk then
            // misclassifies the next stale echo (cursor drifted onto the space)
            // as in-word staleness and routes the next correction through
            // ForwardKey backspaces instead of a byte-count
            // delete_surrounding_text.
            let mut c =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();
            let text = "wor ơ x";
            let cursor = "wor ơ".len() as u32;

            c.observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), true);

            c.mark_action();
            match c.feed_key('r') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ở");
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                _ => panic!("expected retroactive tone edit"),
            }
            assert_eq!(c.shadow_text(), "wor ở");
            assert!(
                c.retroactive_context,
                "retroactive context must survive the refresh reseed"
            );

            // Firefox stale echo: old text, cursor shifted past the edit onto
            // the following space. With retroactive_context intact this keeps
            // byte deletes; if the refresh wrongly dropped it, the quirk arms
            // char-count delete + ForwardKey here.
            c.observe_surrounding_bytes(
                text,
                ByteCursor("wor ơ ".len() as u32),
                ByteCursor("wor ơ ".len() as u32),
                false,
            );

            c.mark_action();
            match c.feed_key('j') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ợ");
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                _ => panic!("expected second retroactive tone edit"),
            }

            // Both corrections must go through delete_surrounding_text. The
            // second delete is 3 bytes / 1 char (ở).
            assert_eq!(
                sink.deletes.len(),
                2,
                "second correction must use delete_surrounding_text, not ForwardKey"
            );
            assert_eq!(sink.deletes[1], (3, 1, 0, 0));
            assert!(sink.vk_keys.is_empty(), "no ForwardKey backspaces expected");
        }
    }

    mod surrounding_liveness {
        use super::*;

        #[test]
        fn dead_surrounding_frames_signal_forward_key_downgrade() {
            // Google Docs / Firefox contenteditable: advertises surrounding-text
            // but every frame is text="" cursor=0 and delete_surrounding_text is a
            // no-op, so SurroundingText commits double the word. The watchdog must
            // flag the downgrade once shadow holds content but frames stay empty.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

            // A genuinely empty widget (empty shadow) is NOT a strike.
            assert!(!c.note_surrounding_liveness("", 0));

            // Client reflected one commit, so our shadow now holds content.
            c.observe_surrounding_bytes("T", ByteCursor(1), ByteCursor(1), false);
            assert_eq!(c.shadow_text(), "T");

            // Now it goes dead: empty frames despite committed shadow.
            assert!(!c.note_surrounding_liveness("", 0)); // strike 1
            assert!(c.note_surrounding_liveness("", 0)); // strike 2 → downgrade
        }

        #[test]
        fn live_surrounding_frame_resets_dead_strikes() {
            // A functional client reflects our commits; one such frame must clear
            // the strike counter so a lone transient empty frame never downgrades.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.observe_surrounding_bytes("ph", ByteCursor(2), ByteCursor(2), false);

            assert!(!c.note_surrounding_liveness("", 0)); // strike 1
            assert!(!c.note_surrounding_liveness("pho", 3)); // content seen → reset
            assert!(!c.note_surrounding_liveness("", 0)); // back to strike 1, no downgrade
        }

        #[cfg(feature = "ibus")]
        #[test]
        fn unechoed_surrounding_corrections_signal_forward_key_downgrade() {
            // Google Docs under IBus: advertises surrounding-text but silently
            // no-ops the delete and echoes nothing back. The first correction has
            // no predecessor to judge (no strike); the second, still echo-less,
            // signals the downgrade.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            assert!(!c.note_surrounding_correction()); // first correction: no strike yet
            assert!(c.note_surrounding_correction()); // second, still no echo → downgrade
        }

        #[cfg(feature = "ibus")]
        #[test]
        fn echoed_surrounding_corrections_never_downgrade() {
            // A functional client (gedit) echoes every edit back before the next
            // correction; each echo resets the strike, so no downgrade ever fires.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.mark_surrounding_frame_seen();
            assert!(!c.note_surrounding_correction()); // echoed
            c.mark_surrounding_frame_seen();
            assert!(!c.note_surrounding_correction()); // echoed again
            c.mark_surrounding_frame_seen();
            assert!(!c.note_surrounding_correction());
        }

    }

    mod surrounding_policy {

        #[test]
        fn surrounding_observer_trusts_mid_word_one_char_without_reseed() {
            let decision = super::SurroundingObserver::observe(true, true, false, false, false);

            assert_eq!(
                decision,
                super::SurroundingDecision {
                    trust: true,
                    reseed: false
                }
            );
        }

        #[test]
        fn surrounding_observer_trusts_recent_shadow_confirmed_without_reseed() {
            // A recent frame whose before-cursor text matches our shadow (post-commit
            // echo, or a Chromium autocomplete selection whose prefix is what we
            // typed) is trusted — syncs the shadow/selection without reseeding.
            let decision = super::SurroundingObserver::observe(true, false, false, false, true);

            assert_eq!(
                decision,
                super::SurroundingDecision {
                    trust: true,
                    reseed: false
                }
            );
        }

        #[test]
        fn surrounding_observer_drops_recent_unconfirmed_selection() {
            // A STALE Chromium-omnibox autocomplete selection (recent, not a one-char
            // insert, before-cursor does NOT match shadow) must be dropped — trusting
            // it re-armed a stale selection span and broke `haf`→`à`.
            let decision = super::SurroundingObserver::observe(true, false, false, false, false);

            assert_eq!(
                decision,
                super::SurroundingDecision {
                    trust: false,
                    reseed: false
                }
            );
        }

        #[test]
        fn surrounding_observer_drops_recent_delete_echo() {
            // A deletion within the recent-action window is our own
            // delete_surrounding_text echo, not an external edit — drop it so the
            // shadow and in-progress composition survive until the commit echo.
            let decision = super::SurroundingObserver::observe(true, false, true, false, false);

            assert_eq!(
                decision,
                super::SurroundingDecision {
                    trust: false,
                    reseed: false
                }
            );
        }

        #[test]
        fn surrounding_observer_trusts_external_delete_for_reset() {
            // The same deletion outside the recent-action window IS an external
            // edit — trust it so the deletion branch can reset the engine.
            let decision = super::SurroundingObserver::observe(false, false, true, false, false);

            assert_eq!(
                decision,
                super::SurroundingDecision {
                    trust: true,
                    reseed: false
                }
            );
        }

    }

    mod edit_model_behavior {
        use super::*;

        #[test]
        fn edit_model_owns_shadow_and_tier_apply() {
            let mut edit = super::EditModel::new(BackspaceMethod::SurroundingText);
            edit.push_forwarded_char('a');
            assert_eq!(edit.shadow_text(), "a");

            let mut sink = DeleteCaptureSink::default();
            edit.apply(1, "â", 1, 0, &mut sink, DeleteUnit::Bytes);

            assert_eq!(sink.deletes.len(), 1);
            assert_eq!(sink.commits, vec!["â".to_owned()]);
            assert_eq!(edit.shadow_text(), "â");
        }

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
        fn oci_append_direct_multibyte_scalar() {
            // Direct app-visible insertion of one Vietnamese scalar advances the
            // byte cursor by that scalar's UTF-8 width, not by exactly one byte.
            assert!(oci("ti", 2, "tiế", "tiế".len() as u32));
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
            // detected as a 1-char keystroke (would feed 'D' into the engine
            // without resetting it).
            assert!(!oci("đường", 9, "D", 1));
        }

        #[test]
        fn oci_second_capital_d_on_new_line() {
            // After cursor jump above, prev_text becomes "D" prev_cursor=1.
            // User types second 'D' → text="DD" cursor=2. MUST be detected as
            // 1-char keystroke so handle_char fires and `DD→Đ` rule runs.
            assert!(oci("D", 1, "DD", 2));
        }

        #[test]
        fn deletion_rejects_cursor_inside_multibyte_char() {
            // Cursor offsets are byte offsets. When both offsets land inside 'ế',
            // slicing returns None on both sides; that must not be accepted as a
            // real external deletion.
            assert!(!del("aếb", 2, "aế", 2));
        }

        #[test]
        fn deletion_accepts_multibyte_char_span() {
            assert!(del("aếb", "aế".len() as u32, "ab", 1));
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
    fn forwarded_backspace_echo_does_not_reseed_deleted_word() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

        c.observe_surrounding_bytes("work", ByteCursor(4), ByteCursor(4), true);
        c.mark_action();
        c.observe_surrounding_bytes("work", ByteCursor(4), ByteCursor(4), false);
        assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
        c.observe_surrounding_bytes("wor", ByteCursor(3), ByteCursor(3), false);

        assert_eq!(c.shadow_text(), "wor");
        match c.feed_key('d') {
            KeyDecision::ForwardRaw => {}
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                panic!("expected raw 'd', got edit bs={backspaces} commit={commit:?}");
            }
            KeyDecision::Consumed => panic!("expected raw 'd', got consumed"),
        }
    }

    #[test]
    fn forwarded_backspace_echo_with_stale_prev_text_does_not_reseed_deleted_word() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let mut sink = DeleteCaptureSink::default();

        c.mark_action();
        match c.feed_key('w') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
            }
            _ => panic!("expected w transform"),
        }
        c.observe_surrounding_bytes("work", ByteCursor(4), ByteCursor(4), false);
        assert_eq!(c.shadow_text(), "ư");

        c.mark_action();
        assert!(matches!(c.feed_key('o'), KeyDecision::ForwardRaw));
        c.observe_surrounding_bytes("ưo", ByteCursor(3), ByteCursor(3), false);
        c.mark_action();
        match c.feed_key('r') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
            }
            _ => panic!("expected r transform"),
        }
        c.observe_surrounding_bytes("ửo", ByteCursor(4), ByteCursor(4), false);
        c.mark_action();
        match c.feed_key('k') {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
            }
            _ => panic!("expected k transform"),
        }
        c.observe_surrounding_bytes("work", ByteCursor(4), ByteCursor(4), false);
        assert_eq!(c.shadow_text(), "work");

        c.defer_action();
        assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
        c.observe_surrounding_bytes("wor", ByteCursor(3), ByteCursor(3), false);
        assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
        c.observe_surrounding_bytes("wo", ByteCursor(2), ByteCursor(2), false);

        c.mark_action();
        assert!(matches!(c.feed_key('r'), KeyDecision::ForwardRaw));
        c.observe_surrounding_bytes("wor", ByteCursor(3), ByteCursor(3), false);
        c.mark_action();
        match c.feed_key('d') {
            KeyDecision::ForwardRaw => {}
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                panic!("expected raw 'd', got edit bs={backspaces} commit={commit:?}");
            }
            KeyDecision::Consumed => panic!("expected raw 'd', got consumed"),
        }
    }

    #[test]
    fn retyping_english_after_plain_backspaces_stays_raw() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);

        c.observe_surrounding_bytes("doesnt", ByteCursor(6), ByteCursor(6), true);

        for expected in ["doesn", "does", "doe"] {
            assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
            c.observe_surrounding_bytes(
                expected,
                ByteCursor(expected.len() as u32),
                ByteCursor(expected.len() as u32),
                false,
            );
        }

        for ch in ['s', 'n', 't'] {
            c.mark_action();
            match c.feed_key(ch) {
                KeyDecision::ForwardRaw => {}
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    panic!("expected raw {ch:?}, got edit bs={backspaces} commit={commit:?}");
                }
                KeyDecision::Consumed => panic!("expected raw {ch:?}, got consumed"),
            }
            let text = c.shadow_text().to_owned();
            c.observe_surrounding_bytes(
                &text,
                ByteCursor(text.len() as u32),
                ByteCursor(text.len() as u32),
                false,
            );
        }

        assert_eq!(c.shadow_text(), "doesnt");
    }

    #[test]
    fn retyping_typed_english_after_plain_backspaces_stays_raw() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let mut sink = DeleteCaptureSink::default();

        for ch in "doesnt".chars() {
            c.mark_action();
            match c.feed_key(ch) {
                KeyDecision::ForwardRaw => {}
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                KeyDecision::Consumed => {}
            }
            let text = c.shadow_text().to_owned();
            c.observe_surrounding_bytes(
                &text,
                ByteCursor(text.len() as u32),
                ByteCursor(text.len() as u32),
                false,
            );
        }

        for expected in ["doesn", "does", "doe"] {
            assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
            c.observe_surrounding_bytes(
                expected,
                ByteCursor(expected.len() as u32),
                ByteCursor(expected.len() as u32),
                false,
            );
        }

        for ch in ['s', 'n', 't'] {
            c.mark_action();
            match c.feed_key(ch) {
                KeyDecision::ForwardRaw => {}
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    panic!("expected raw {ch:?}, got edit bs={backspaces} commit={commit:?}");
                }
                KeyDecision::Consumed => panic!("expected raw {ch:?}, got consumed"),
            }
            let text = c.shadow_text().to_owned();
            c.observe_surrounding_bytes(
                &text,
                ByteCursor(text.len() as u32),
                ByteCursor(text.len() as u32),
                false,
            );
        }

        assert_eq!(c.shadow_text(), "doesnt");
    }

    #[test]
    fn first_typed_english_word_stays_raw_on_reliable_surrounding_clients() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let mut sink = DeleteCaptureSink::default();

        let mut visible = String::new();
        for ch in "doesnt".chars() {
            c.mark_action();
            match c.feed_key(ch) {
                KeyDecision::ForwardRaw => visible.push(ch),
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    for _ in 0..backspaces {
                        visible.pop();
                    }
                    visible.push_str(&commit);
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                KeyDecision::Consumed => {}
            }
            c.observe_surrounding_bytes(
                &visible,
                ByteCursor(visible.len() as u32),
                ByteCursor(visible.len() as u32),
                false,
            );
        }

        assert_eq!(visible, "doesnt");
        assert_eq!(c.shadow_text(), "doesnt");
    }

    #[test]
    fn first_typed_english_word_stays_raw_across_ibus_caps_upgrade() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::ForwardKey, false);
        let mut sink = DeleteCaptureSink::default();

        let mut visible = String::new();
        for (idx, ch) in "word".chars().enumerate() {
            c.mark_action();
            match c.feed_key(ch) {
                KeyDecision::ForwardRaw => visible.push(ch),
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    for _ in 0..backspaces {
                        visible.pop();
                    }
                    visible.push_str(&commit);
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                KeyDecision::Consumed => {}
            }

            if idx == 0 {
                c.set_method(BackspaceMethod::SurroundingText);
            }
            c.observe_surrounding_bytes(
                &visible,
                ByteCursor(visible.len() as u32),
                ByteCursor(visible.len() as u32),
                false,
            );
        }

        assert_eq!(visible, "word");
        assert_eq!(c.shadow_text(), "word");
    }

    #[test]
    fn activation_stale_document_text_does_not_seed_new_first_word() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let mut sink = DeleteCaptureSink::default();

        c.observe_surrounding_bytes("ửod word word ", ByteCursor(16), ByteCursor(16), true);
        c.observe_surrounding_bytes("ửod word word ", ByteCursor(0), ByteCursor(0), false);

        let mut visible = String::new();
        for ch in "word".chars() {
            c.mark_action();
            match c.feed_key(ch) {
                KeyDecision::ForwardRaw => visible.push(ch),
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    for _ in 0..backspaces {
                        visible.pop();
                    }
                    visible.push_str(&commit);
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                KeyDecision::Consumed => {}
            }
            c.observe_surrounding_bytes(
                &visible,
                ByteCursor(visible.len() as u32),
                ByteCursor(visible.len() as u32),
                false,
            );
        }

        assert_eq!(visible, "word");
        assert_eq!(c.shadow_text(), "word");
    }

    #[test]
    fn v2_first_word_invalid_syllable_deconverts_despite_delete_echo() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let mut sink = DeleteCaptureSink::default();
        let mut visible = String::new();

        // Activate frame: empty document, force reseed.
        c.observe_surrounding_bytes("", ByteCursor(0), ByteCursor(0), true);

        // Helper: feed a key the way the wayland glue does, update `visible`,
        // and apply the edit to the shadow via the sink.
        let press = |c: &mut Composer,
                     sink: &mut DeleteCaptureSink,
                     visible: &mut String,
                     ch: char|
         -> KeyDecision {
            c.mark_action();
            let d = c.feed_key(ch);
            match &d {
                KeyDecision::ForwardRaw => visible.push(ch),
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    for _ in 0..*backspaces {
                        visible.pop();
                    }
                    visible.push_str(commit);
                    c.apply_to_sink(*backspaces, commit, 0, 0, sink);
                }
                KeyDecision::Consumed => {}
            }
            d
        };

        // w → ư
        press(&mut c, &mut sink, &mut visible, 'w');
        c.observe_surrounding_bytes("ư", ByteCursor(2), ByteCursor(2), false);
        // o → ưo (forwarded raw)
        press(&mut c, &mut sink, &mut visible, 'o');
        c.observe_surrounding_bytes("ưo", ByteCursor(3), ByteCursor(3), false);
        // r → ửo (delete "ưo" + commit "ửo"). The compositor echoes the
        // intermediate delete frame ("" at cursor 0) BEFORE the commit frame.
        press(&mut c, &mut sink, &mut visible, 'r');
        assert_eq!(visible, "ửo");
        c.observe_surrounding_bytes("", ByteCursor(0), ByteCursor(0), false); // delete echo
        c.observe_surrounding_bytes("ửo", ByteCursor(4), ByteCursor(4), false); // commit echo

        // d → invalid syllable, engine deconverts whole word to raw "word".
        let d = press(&mut c, &mut sink, &mut visible, 'd');
        match d {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(
                    backspaces, 2,
                    "must delete the 2 composed screen chars 'ửo'"
                );
                assert_eq!(commit, "word");
            }
            KeyDecision::ForwardRaw => {
                panic!("expected deconversion to 'word', got ForwardRaw (regression: 'ửod')")
            }
            KeyDecision::Consumed => panic!("expected deconversion to 'word', got Consumed"),
        }
        assert_eq!(visible, "word");
        // The delete must remove "ửo" (4 bytes / 2 chars), not 0 — proves the
        // shadow survived the delete echo.
        let (before_bytes, before_chars, _, _) = *sink.deletes.last().unwrap();
        assert_eq!((before_bytes, before_chars), (4, 2));
    }

    #[test]
    fn chromium_omnibox_stale_selection_does_not_eat_first_word() {
        let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
        let mut sink = DeleteCaptureSink::default();
        let mut visible = String::new();

        // The omnibox autocompletes 'h' → a URL beginning with 'h'; cursor sits
        // after the typed 'h' (=1) with the rest selected to anchor=N.
        let url = "https://invent.kde.org";
        let n = url.len() as u32;

        let press = |c: &mut Composer,
                     sink: &mut DeleteCaptureSink,
                     visible: &mut String,
                     ch: char|
         -> KeyDecision {
            c.mark_action();
            let d = c.feed_key(ch);
            match &d {
                KeyDecision::ForwardRaw => visible.push(ch),
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    for _ in 0..*backspaces {
                        visible.pop();
                    }
                    visible.push_str(commit);
                    c.apply_to_sink(*backspaces, commit, 0, 0, sink);
                }
                KeyDecision::Consumed => {}
            }
            d
        };

        // h → raw 'h'; omnibox echoes its autocomplete selection (prefix "h"
        // still matches shadow here, so it is confirmed and trusted).
        press(&mut c, &mut sink, &mut visible, 'h');
        c.observe_surrounding_bytes(url, ByteCursor(1), ByteCursor(n), false);
        // a → raw 'a'. Now a STALE "h…" selection frame re-arrives (prefix "h"
        // no longer matches shadow "ha") and must be dropped, then the real
        // "ha" frame clears the selection.
        press(&mut c, &mut sink, &mut visible, 'a');
        c.observe_surrounding_bytes(url, ByteCursor(1), ByteCursor(n), false); // stale → dropped
        c.observe_surrounding_bytes("ha", ByteCursor(2), ByteCursor(2), false); // real → clears sel
                                                                                // f → huyền tone on a: bs=1 commit "à". Must use the plain
                                                                                // delete_surrounding_text path, NOT the selection-active ForwardKey-BS
                                                                                // fallback (which would delete "ha" and leave only "à").
        let d = press(&mut c, &mut sink, &mut visible, 'f');
        match d {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => {
                assert_eq!(backspaces, 1);
                assert_eq!(commit, "à");
            }
            other => panic!(
                "expected tone Apply bs=1 'à', got {}",
                match other {
                    KeyDecision::ForwardRaw => "ForwardRaw",
                    KeyDecision::Consumed => "Consumed",
                    _ => "?",
                }
            ),
        }
        assert_eq!(visible, "hà");
        // No virtual-keyboard backspaces: the stale selection was not honored.
        assert!(
            sink.vk_keys.is_empty(),
            "stale selection must not trigger ForwardKey-BS fallback"
        );
        assert_eq!(*sink.deletes.last().unwrap(), (1, 1, 0, 0));
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
