//! The composition brain. Owns engine + strategy + shadow + the
//! surrounding-text reseed gate, transport-neutral. Each transport
//! (wayland / ibus / evdev) is thin glue that translates its wire events
//! into `Composer` calls; the composition logic lives here once.
//!
//! Evolved from the old `WindowState` (state) + the composition methods that
//! used to live on `Daemon` (behavior). State and behavior now sit together.

use std::time::{Duration, Instant};

use viet_ime_edit_strategy::{BackspaceMethod, DeleteUnit, KeyDecision, ModifierState, Strategy};
use viet_ime_engine::{EngineState, InputMethod, ProcessResult};

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
        char_inserted_before_cursor(&self.prev_text, text, cursor, ch)
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

// Per-text-input composition state + behavior. On wlroots/Sway, one instance
// at a time (compositor sends deactivate before new activate); ibus/evdev
// create one per active session.

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

    /// The client's most recent surrounding-text report and the one before
    /// it, as `(text, cursor)` (Wayland only). Every report counts, including
    /// duplicates and echoes the reseed gate skips, so unlike the recorded
    /// diff baseline these never go stale:
    /// - text-input-v3 clients convert `delete_surrounding_text` byte lengths
    ///   to characters against the latest report (`client_byte_delete_agrees`);
    /// - consecutive reports prove where a raw key landed, and whether a frame
    ///   really repeats the previous one.
    client_report: Option<(String, u32)>,
    previous_client_report: Option<(String, u32)>,

    /// Whether `[`/`]`/`{`/`}` Telex shortcuts for ơ/ư/Ơ/Ư are enabled.
    /// Stored so `set_input_method` can recreate the engine with it.
    bracket_shortcuts: bool,

    /// Whether retroactive SurroundingText edits must first be confirmed by a
    /// client frame containing the raw key at its real insertion point. Only
    /// Wayland drains frame-triggered repairs; IBus keeps immediate edits.
    confirm_retroactive_edits: bool,

    /// Complete client-frame confirmation state. Keeping provenance, raw key,
    /// repairability, and speculative engine edit in one enum prevents invalid
    /// combinations of independent flags.
    confirmation: ConfirmationState,
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

#[derive(Debug, Clone)]
enum ConfirmationState {
    Live,
    LiveConfirmed,
    FrameDerived,
    AwaitingFrame,
    PendingRaw(PendingRaw),
}

#[derive(Debug, Clone)]
struct PendingRaw {
    ch: char,
    repairable: bool,
    edit: Option<PendingEngineEdit>,
    from_frame: bool,
}

impl ConfirmationState {
    fn pending_raw(&self) -> Option<&PendingRaw> {
        match self {
            Self::PendingRaw(pending) => Some(pending),
            Self::Live | Self::LiveConfirmed | Self::FrameDerived | Self::AwaitingFrame => None,
        }
    }

    fn is_uncertain(&self) -> bool {
        !matches!(self, Self::Live | Self::LiveConfirmed)
    }

    fn is_frame_derived(&self) -> bool {
        match self {
            Self::FrameDerived => true,
            Self::PendingRaw(pending) => pending.from_frame,
            Self::Live | Self::LiveConfirmed | Self::AwaitingFrame => false,
        }
    }

    #[cfg(test)]
    fn replacement_pending(&self, ch: char, edit: Option<PendingEngineEdit>) -> Self {
        Self::PendingRaw(PendingRaw {
            ch,
            repairable: !matches!(self, Self::PendingRaw(_)),
            edit,
            from_frame: self.is_frame_derived(),
        })
    }

    #[cfg(test)]
    fn variant_name(&self) -> &'static str {
        match self {
            Self::Live => "Live",
            Self::LiveConfirmed => "LiveConfirmed",
            Self::FrameDerived => "FrameDerived",
            Self::AwaitingFrame => "AwaitingFrame",
            Self::PendingRaw(_) => "PendingRaw",
        }
    }
}

#[derive(Debug, Clone)]
struct FeedKeyFacts {
    ch: char,
    method: BackspaceMethod,
    confirmation_enabled: bool,
    confirmation: ConfirmationState,
    previous_was_separator: bool,
    engine_at_word_beginning: bool,
    shadow_word: String,
}

impl FeedKeyFacts {
    fn capture(composer: &Composer, ch: char) -> Self {
        let shadow = composer.edit.shadow_text();
        let shadow_word = current_word_before_cursor(shadow, shadow.len() as u32).to_owned();
        Self {
            ch,
            method: composer.method(),
            confirmation_enabled: composer.confirm_retroactive_edits,
            confirmation: composer.confirmation.clone(),
            previous_was_separator: matches!(
                composer.last_input_char,
                Some(previous) if !previous.is_ascii_alphabetic()
            ),
            engine_at_word_beginning: composer.engine.at_word_beginning(),
            shadow_word,
        }
    }
}

#[derive(Debug, Clone)]
struct KeyStartPlan {
    context_uncertain: bool,
    reset_speculation: bool,
    clear_non_alpha_context: bool,
    seed_word: Option<String>,
}

enum KeyFinishPlan {
    Apply { backspaces: usize, commit: String },
    Forward { pending: Option<PendingRaw> },
}

fn plan_key_start(facts: &FeedKeyFacts) -> KeyStartPlan {
    let prior_pending = matches!(&facts.confirmation, ConfirmationState::PendingRaw(_));
    let context_uncertain = facts.confirmation_enabled
        && facts.method == BackspaceMethod::SurroundingText
        && facts.confirmation.is_uncertain();
    let seed_word = (!prior_pending
        && facts.engine_at_word_beginning
        && !facts.previous_was_separator
        && !facts.shadow_word.is_empty())
    .then(|| facts.shadow_word.clone());
    KeyStartPlan {
        context_uncertain,
        reset_speculation: prior_pending,
        clear_non_alpha_context: !facts.ch.is_ascii_alphabetic(),
        seed_word,
    }
}

fn plan_key_finish(
    ch: char,
    context_uncertain: bool,
    prior_pending: bool,
    from_frame: bool,
    result: ProcessResult,
    base_word: String,
) -> KeyFinishPlan {
    let ProcessResult {
        backspaces,
        commit,
        consumed,
    } = result;
    if consumed && !context_uncertain {
        return KeyFinishPlan::Apply { backspaces, commit };
    }
    let pending = context_uncertain.then(|| PendingRaw {
        ch,
        repairable: !prior_pending,
        edit: consumed.then_some(PendingEngineEdit {
            backspaces,
            commit,
            base_word,
        }),
        from_frame,
    });
    KeyFinishPlan::Forward { pending }
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
            client_report: None,
            previous_client_report: None,
            bracket_shortcuts,
            confirm_retroactive_edits: false,
            confirmation: ConfirmationState::Live,
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

    #[cfg(test)]
    fn confirmation_variant(&self) -> &'static str {
        self.confirmation.variant_name()
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
            self.confirmation = ConfirmationState::Live;
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
        self.confirmation = ConfirmationState::AwaitingFrame;
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
        // A successful seed replaces the engine itself. If the visible word
        // cannot round-trip (e.g. literal `anw` after `anww`), keep the live
        // engine's escape history instead of resetting it before rejection.
        self.retroactive_context = !word.is_empty() && self.engine.feed_context_gated(word);
    }

    /// Apply the 2-second idle policy. Returns true when the gap since the
    /// last keystroke exceeds 2 seconds.
    ///
    /// ForwardKey has no surrounding frames to detect a cursor move, so idle
    /// resets its engine and unverifiable shadow. SurroundingText retains
    /// continuous state and uses frame revalidation instead: a click during
    /// the pause arrives as a frame that revokes or reseeds the composition.
    /// Idle keeps live composition, confirmed or not (Firefox does not echo
    /// daklak's edits until the next input, but does report a click at once),
    /// and only defers context derived from a frame or a forwarded edit key.
    pub fn check_idle_reset(&mut self) -> bool {
        if self.last_keystroke_at.elapsed().as_secs() < 2 {
            return false;
        }
        if self.method() == BackspaceMethod::SurroundingText {
            if !matches!(self.confirmation, ConfirmationState::LiveConfirmed) {
                self.confirmation = ConfirmationState::AwaitingFrame;
            }
        } else {
            // ForwardKey receives no cursor frames. After an idle pause its
            // shadow cannot be trusted: a silent click may have moved the
            // application cursor while the shadow still names the old word.
            self.engine.reset();
            self.edit.reset_composition();
            self.last_input_char = None;
            self.retroactive_context = false;
        }
        true
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
        let facts = FeedKeyFacts::capture(self, ch);
        let start = plan_key_start(&facts);

        if start.reset_speculation {
            self.engine.reset();
            self.retroactive_context = false;
        }
        if start.clear_non_alpha_context {
            self.retroactive_context = false;
        }
        if let Some(word) = start.seed_word.as_deref() {
            tracing::debug!(word, "seed engine from shadow at word boundary");
            self.retroactive_context = self.engine.feed_context_gated(word);
        }

        self.last_input_char = Some(ch);
        let result = self.engine.process_key(ch);
        self.last_keystroke_at = Instant::now();
        self.trace_process_key(ch, &result);

        let base_word = self.current_shadow_word().to_owned();
        let prior_pending = matches!(&facts.confirmation, ConfirmationState::PendingRaw(_));
        match plan_key_finish(
            ch,
            start.context_uncertain,
            prior_pending,
            facts.confirmation.is_frame_derived(),
            result,
            base_word,
        ) {
            KeyFinishPlan::Apply { backspaces, commit } => {
                self.confirmation = ConfirmationState::Live;
                KeyDecision::Apply {
                    method: self.method(),
                    backspaces,
                    commit,
                }
            }
            KeyFinishPlan::Forward { pending } => {
                self.edit.push_forwarded_char(ch);
                self.confirmation = pending
                    .map(ConfirmationState::PendingRaw)
                    .unwrap_or(ConfirmationState::Live);
                KeyDecision::ForwardRaw
            }
        }
    }

    fn trace_process_key(&self, ch: char, result: &ProcessResult) {
        tracing::debug!(
            ch = %ch,
            consumed = result.consumed,
            bs = result.backspaces,
            commit = %result.commit,
            shadow = %self.edit.shadow_text(),
            "engine.process_key"
        );
    }

    fn current_shadow_word(&self) -> &str {
        let shadow = self.edit.shadow_text();
        current_word_before_cursor(shadow, shadow.len() as u32)
    }

    pub fn unrecord_forwarded_char(&mut self) {
        self.edit.pop_forwarded_char();
        let pending = std::mem::replace(&mut self.confirmation, ConfirmationState::Live);
        if matches!(
            pending,
            ConfirmationState::PendingRaw(PendingRaw { edit: Some(_), .. })
        ) {
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
            if self.method() == BackspaceMethod::ForwardKey {
                // ForwardKey clients (foot and other terminals that advertise
                // surrounding-text but never reflect it, so the liveness
                // watchdog downgrades ST→FK) never deliver a post-backspace
                // frame to resync engine state. After a raw restore (e.g.
                // `tự`+`d` → `tuwjd`), vnkey holds a NonVn buffer; forwarded
                // backspaces only decrement it, so it stays non-composable and
                // a re-typed tone key (`tu`+`w`) forwards raw instead of
                // composing `ư`. Reseed the engine from the word the backspace
                // just re-exposed. The render gate keeps English/foreign words
                // raw — they fail the round-trip and leave the engine empty —
                // and seeding from the popped shadow (not the pre-delete
                // buffer) never resurrects the deleted glyph.
                let word = self.current_shadow_word().to_owned();
                self.engine.reset();
                self.retroactive_context =
                    !word.is_empty() && self.engine.feed_context_gated(&word);
            } else if r.backspaces > 0 && matches!(deleted_visible, Some(ch) if !ch.is_ascii()) {
                // SurroundingText: `processed=false` can still carry a positive
                // backspace count — vnkey recognized an owned glyph was deleted
                // but emitted no edit. When that glyph is a composed non-ASCII
                // char (e.g. `u`+`w` -> `ư`), drop stale raw history before a
                // later raw restore resurrects the deleted prefix (`sưi`+`t` ->
                // `uswit`). Plain ASCII English backspaces keep their engine
                // history so retyping `doesnt` stays raw. (The post-backspace
                // surrounding frame re-seeds when the cursor context warrants.)
                self.engine.reset();
                self.retroactive_context = false;
            }
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
        // The client converts the byte length of a `delete_surrounding_text`
        // to characters against the text it last reported, not its live
        // buffer (GTK: `g_utf8_pointer_to_offset` over the cached
        // surrounding text). When that report lags our previous commit, the
        // same bytes cover a different number of characters: Firefox still
        // reporting `anse|r` after `ans` + `ư` turns "delete `ư`" (2 bytes)
        // into "delete 2 chars", eating the `s`. Physical Backspaces count
        // characters at the real caret, so use them whenever the report does
        // not end with the exact text being deleted.
        let client_view_disagrees = backspaces > 0
            && self.method() == BackspaceMethod::SurroundingText
            && self.client_report.as_ref().is_some_and(|(text, cursor)| {
                !client_byte_delete_agrees(text_before_cursor(text, *cursor), &before, backspaces)
            });
        let used_forward_key = client_view_disagrees;
        tracing::trace!(
            used_forward_key,
            client_view_disagrees,
            "delete-channel decision"
        );
        if used_forward_key {
            tracing::info!(
                backspaces,
                shadow_before_bytes = before.len(),
                shadow_before_chars = before.chars().count(),
                commit_bytes = commit.len(),
                commit_chars = commit.chars().count(),
                "using ForwardKey instead of delete_surrounding_text"
            );
            self.edit
                .apply_forward_key(backspaces, commit, serial, time, sink);
        } else {
            self.edit
                .apply(backspaces, commit, serial, time, sink, DeleteUnit::Bytes);
        }

        if !preserve_engine {
            self.refresh_retroactive_context_after_apply(backspaces);
        }
    }

    /// Apply a repair calculated from the frame that confirmed a raw key's
    /// actual insertion point. KWin may repeat the pre-edit frame while this
    /// client-confirmation transaction completes.
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
        // The Wayland transport notes each report before its duplicate gate;
        // only note here for callers that did not (tests, direct use).
        if self.client_report.as_ref() != Some(&(text.to_owned(), cursor.0)) {
            self.note_client_surrounding(text, cursor.0);
        }
        self.apply_surrounding(text, cursor.0, anchor.0, force_reseed)
    }

    /// Record a client surrounding-text report. Called for every Wayland
    /// frame, before the duplicate gate and including echoes the reseed gate
    /// ignores. See `client_report`.
    pub fn note_client_surrounding(&mut self, text: &str, cursor: u32) {
        self.previous_client_report = self.client_report.take();
        self.client_report = Some((text.to_owned(), cursor));
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

    /// Downgrade a confirmed live composition when a report shows the caret
    /// is no longer where our edits left it.
    ///
    /// A report can disagree with the shadow in text alone: Firefox updates
    /// its caret before the text it reports, so right after an edit it sends
    /// the new cursor with the previous text. Such a report puts the caret
    /// exactly where the shadow says it is (byte offset == shadow length), so
    /// it is not evidence of a cursor move; deletes stay safe because
    /// `client_view_disagrees` sends Backspaces at the real caret whenever the
    /// reported text differs. A report outside the echo window that changes
    /// the text is trusted by `apply_surrounding`, which reseeds from it.
    fn revalidate_live_confirmation(&mut self, text: &str, cursor: u32, anchor: u32) {
        if !matches!(self.confirmation, ConfirmationState::LiveConfirmed) {
            return;
        }
        let shadow = self.edit.shadow_text();
        let text_matches = text_before_cursor(text, cursor) == shadow;
        let caret_matches = cursor == anchor && cursor as usize == shadow.len();
        if !text_matches && !caret_matches {
            self.confirmation = ConfirmationState::AwaitingFrame;
        }
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
        self.revalidate_live_confirmation(text, cursor, anchor);

        // A raw key awaiting confirmation is stronger evidence than the 150ms
        // echo heuristic: daklak issued no positional edit for this key, and a
        // frame containing it immediately before the reported cursor tells us
        // where the client actually inserted it. Handle this before generic
        // stale-frame filtering.
        if self.confirmation.pending_raw().is_some() && cursor == anchor {
            if let Some(repair) = self.confirm_pending_raw_key(text, cursor, anchor, before_cursor) {
                return Some(repair);
            }
            self.confirmation.pending_raw()?;
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

        if !decision.trust {
            tracing::trace!(text, cursor, anchor, "skip recent surrounding_text echo");
            return None;
        }

        let deletion_already_applied = shadow_confirmed;
        self.edit.on_surrounding_text(text, cursor, anchor);
        // A trusted frame supplies destination context; a frame-derived seed
        // below carries any remaining uncertainty in `confirmation`.
        let preserve_frame_derived = self.confirmation.is_frame_derived();
        let confirm_live =
            shadow_confirmed && !matches!(self.confirmation, ConfirmationState::PendingRaw(_));
        let had_pending_edit = self
            .confirmation
            .pending_raw()
            .and_then(|pending| pending.edit.as_ref())
            .is_some();
        self.confirmation = if preserve_frame_derived {
            ConfirmationState::FrameDerived
        } else if confirm_live {
            ConfirmationState::LiveConfirmed
        } else {
            ConfirmationState::Live
        };
        if had_pending_edit {
            // A trusted frame superseded the confirmation transaction; the
            // engine's speculative post-key state was never applied on screen.
            self.engine.reset();
            self.retroactive_context = false;
        }

        if deletion {
            if !deletion_already_applied {
                self.engine.reset();
                self.retroactive_context = false;
                self.confirmation = ConfirmationState::Live;
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
            self.confirmation = if self.retroactive_context {
                ConfirmationState::FrameDerived
            } else {
                ConfirmationState::Live
            };
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
        let pending = self.confirmation.pending_raw()?.clone();
        let ch = pending.ch;
        let (raw_index, actual_ch) = before_cursor.char_indices().next_back()?;
        if actual_ch != ch {
            return None;
        }

        // A delayed client may combine the prior committed transform and this
        // raw key into one frame, so `prev_text` can lag by more than one edit.
        // Shadow equality alone also matches a cursor move over an adjacent
        // identical character; a real insertion changes the document.
        let document_changed = text != self.edit.prev_surrounding_for_trace().0;
        // The recorded baseline can be stale (echoes skipped since), so the
        // client's own previous report is checked too: going from it to this
        // frame by exactly the raw key at the caret proves where it landed.
        let client_step_inserted = self
            .previous_client_report
            .as_ref()
            .is_some_and(|(prev, _)| char_inserted_before_cursor(prev, text, cursor, ch));
        let insertion_proven = self.edit.confirms_inserted_char(text, cursor, ch)
            || client_step_inserted
            || (before_cursor == self.edit.shadow_text() && document_changed);
        let repairable = pending.repairable;
        let stripped = &before_cursor[..raw_index];
        let base_word = current_word_before_cursor(stripped, stripped.len() as u32).to_owned();
        let actual_word = current_word_before_cursor(before_cursor, before_cursor.len() as u32)
            .to_owned();

        if !insertion_proven || !repairable {
            // The frame changed by more than this key (paste/delete/external
            // edit), or an overlapping key already invalidated the stashed
            // repair. Either way it is the newest ground truth but cannot
            // justify emitting a repair. Synchronize and continue from its
            // visible word.
            self.edit.on_surrounding_text(text, cursor, anchor);
            self.edit.record_surrounding(text, cursor, anchor);
            self.last_input_char = Some(ch);
            self.engine.reset();
            self.retroactive_context =
                !actual_word.is_empty() && self.engine.feed_context_gated(&actual_word);
            // When the frame PROVED this pending key landed at the reported
            // cursor (`insertion_proven`) but a later overlapping key made it
            // unrepairable, the cursor and latest character are known — end the
            // deferral instead of re-arming FrameDerived. Leaving it frame-
            // derived was sticky: every following key kept forwarding raw and
            // dropping its commit. Observed in LibreOffice Writer typing "hơn":
            // a stale post-backspace frame (arriving after the recent-action
            // window because forwarded backspaces do not mark an action)
            // reseeded to FrameDerived, then two overlapping keys pinned
            // `repairable=false`, so "ho"+w never composed ơ and raw "how"/
            // "howj" was left on screen. Only the unproven case stays
            // conservative — it may be a genuine cursor jump.
            self.confirmation = if insertion_proven {
                ConfirmationState::LiveConfirmed
            } else if self.retroactive_context {
                ConfirmationState::FrameDerived
            } else {
                ConfirmationState::Live
            };
            return None;
        }

        // Preferred path: the live engine already processed this key against
        // the same base word the frame now proves. Emit its stashed edit and
        // KEEP the engine state — it holds revert history a reseed loses
        // ("kư" fails the round-trip gate, so reseeding forgets ư came from
        // 'w' and a second 'w' would compose "kưư" instead of reverting).
        if let Some(stash) = pending.edit {
            if stash.base_word == base_word {
                self.edit.on_surrounding_text(text, cursor, anchor);
                self.edit.record_surrounding(text, cursor, anchor);
                self.last_input_char = Some(ch);
                self.retroactive_context = true;
                // The frame just PROVED our own keystroke landed where the
                // engine composed it, so the cursor is known again — return to
                // live composition instead of re-arming FrameDerived. Leaving
                // it frame-derived made the deferral sticky: it survived word
                // boundaries (a backspace-reseed then leaked into every later
                // tone key, flashing raw + repairing via the frame until an
                // external Ctrl+A→Backspace reset). A genuine later cursor move
                // is still caught — `revalidate_live_confirmation` downgrades
                // LiveConfirmed→AwaitingFrame on a shadow mismatch, and idle
                // re-defers after 2s.
                self.confirmation = ConfirmationState::LiveConfirmed;
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
        self.last_input_char = Some(ch);

        // For boundary-valued IM keys (`[`/`]` Telex shortcuts), extracting the
        // current word after the raw key yields empty. The proven on-screen word
        // is instead exactly the base word plus that inserted key.
        let screen_word = format!("{base_word}{ch}");

        let Some(replay) = replay else {
            // Fresh word or foreign base: keep the raw key, but seed the full
            // confirmed word so subsequent keys start from real client state.
            self.engine.reset();
            self.retroactive_context =
                !screen_word.is_empty() && self.engine.feed_context_gated(&screen_word);
            self.confirmation = if self.retroactive_context {
                ConfirmationState::FrameDerived
            } else {
                ConfirmationState::Live
            };
            return None;
        };

        self.retroactive_context = true;
        // As in the stash path above: the frame proved where this key landed,
        // so the caret is known and the next key may edit directly. Leaving
        // it frame-derived defers the next key until a report proves it too,
        // which Firefox's caret-first reports may never do before Enter.
        self.confirmation = ConfirmationState::LiveConfirmed;
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

    /// Skip exact repeats of the last recorded frame. A repeat carries no new
    /// evidence, so it must not revoke confirmation either: clients resend
    /// the frame that confirmed a raw key after daklak has already repaired
    /// it, and that frame naturally disagrees with the repaired shadow.
    /// Wayland records the client's byte-delete view before this gate, so
    /// deletes stay safe without inspecting the repeat.
    pub fn should_skip_surrounding_frame(
        &mut self,
        text: &str,
        cursor: u32,
        anchor: u32,
        activate: bool,
        deactivate: bool,
    ) -> bool {
        // The recorded baseline goes stale while echoes are skipped, so a
        // frame equal to it is only a repeat if it also equals the client's
        // previous report. (The current report is this frame, noted already.)
        let repeats_client = self
            .previous_client_report
            .as_ref()
            .is_none_or(|(prev, prev_cursor)| prev == text && *prev_cursor == cursor);
        let duplicate = !activate
            && !deactivate
            && self.is_duplicate_frame(text, cursor, anchor)
            && repeats_client;
        if !duplicate {
            self.revalidate_live_confirmation(text, cursor, anchor);
        }
        duplicate
    }

    /// Clear the surrounding-text diff bookkeeping (no surrounding text in
    /// this frame). Wayland-only: the v3 frame had no surrounding_text.
    pub fn clear_prev_surrounding(&mut self) {
        self.edit.clear_prev_surrounding();
        self.client_report = None;
        self.previous_client_report = None;
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

/// Whether a text-input delete of the last `backspaces` characters of
/// `shadow`, sent as a byte length, removes exactly that many characters on a
/// client whose last report before its cursor was `client_before`. The
/// client measures the byte span backwards from its reported cursor, so it is
/// exact only when the report ends with those same bytes.
fn client_byte_delete_agrees(client_before: &str, shadow: &str, backspaces: usize) -> bool {
    let Some((start, _)) = shadow
        .char_indices()
        .rev()
        .nth(backspaces.saturating_sub(1))
    else {
        return false;
    };
    client_before.ends_with(&shadow[start..])
}

/// Whether `text` is `prev` with `ch` inserted immediately before `cursor`.
fn char_inserted_before_cursor(prev: &str, text: &str, cursor: u32, ch: char) -> bool {
    let cursor = (cursor as usize).min(text.len());
    if !text.is_char_boundary(cursor) {
        return false;
    }
    let Some((start, actual)) = text[..cursor].char_indices().next_back() else {
        return false;
    };
    actual == ch
        && prev.len() + ch.len_utf8() == text.len()
        && prev.as_bytes().get(..start) == text.as_bytes().get(..start)
        && prev.as_bytes().get(start..) == text.as_bytes().get(cursor..)
}

fn text_before_cursor(text: &str, cursor: u32) -> &str {
    let cursor = (cursor as usize).min(text.len());
    let cursor = (0..=cursor)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    &text[..cursor]
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
    use viet_ime_edit_strategy::KeyDecision;
    use viet_ime_edit_strategy::{BackspaceMethod, DeleteUnit, KeyState, OutputSink};
    use viet_ime_engine::InputMethod;

    // Re-exports for nested test modules
    pub(super) use super::detect_deletion as del;
    pub(super) use super::detect_one_char_insertion as oci;
    pub(super) use super::{
        plan_key_finish, plan_key_start, ConfirmationState, EditModel, FeedKeyFacts, KeyFinishPlan,
        PendingRaw, SurroundingDecision, SurroundingObserver,
    };

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

    /// A text-input-v3 client modelled on GTK's im-wayland module: a byte
    /// delete is converted to characters against the last reported text,
    /// then applied at the live caret. Key Backspaces delete one live scalar.
    /// These tests cover collapsed selections and byte spans on UTF-8
    /// boundaries; unsupported spans fail rather than guessing a char count.
    struct CachedReportClient {
        text: String,
        caret: usize,
        report: String,
        report_cursor: usize,
        pending_delete_chars: usize,
        pending_commit: String,
    }

    impl CachedReportClient {
        fn new(text: &str, caret: usize) -> Self {
            assert!(text.is_char_boundary(caret));
            Self {
                text: text.to_owned(),
                caret,
                report: text.to_owned(),
                report_cursor: caret,
                pending_delete_chars: 0,
                pending_commit: String::new(),
            }
        }

        fn report_live(&mut self) -> (String, u32) {
            self.report = self.text.clone();
            self.report_cursor = self.caret;
            (self.report.clone(), self.caret as u32)
        }

        fn report_stale(&mut self, text: &str, cursor: usize) -> (String, u32) {
            assert!(text.is_char_boundary(cursor));
            self.report = text.to_owned();
            self.report_cursor = cursor;
            (self.report.clone(), cursor as u32)
        }

        /// Follow the transport's duplicate-frame gate, still recording every
        /// client report for byte-delete conversion before the gate runs.
        fn deliver_report(&self, c: &mut Composer, activate: bool) {
            let cursor = self.report_cursor as u32;
            c.note_client_surrounding(&self.report, cursor);
            if !c.should_skip_surrounding_frame(&self.report, cursor, cursor, activate, false) {
                assert!(
                    c.observe_surrounding_bytes(
                        &self.report,
                        ByteCursor(cursor),
                        ByteCursor(cursor),
                        activate
                    )
                    .is_none(),
                    "this fixture does not model confirmation-deferred repairs"
                );
            }
        }

        fn delete_chars_before_caret(&mut self, n: usize) {
            for _ in 0..n {
                if let Some(ch) = self.text[..self.caret].chars().next_back() {
                    self.caret -= ch.len_utf8();
                    self.text.remove(self.caret);
                }
            }
        }

        fn insert(&mut self, text: &str) {
            self.text.insert_str(self.caret, text);
            self.caret += text.len();
        }
    }

    impl OutputSink for CachedReportClient {
        fn delete_surrounding_text(&mut self, before_bytes: u32, _: u32, after_bytes: u32, _: u32) {
            assert_eq!(
                after_bytes, 0,
                "this fixture models collapsed selections only"
            );
            let start = self
                .report_cursor
                .checked_sub(before_bytes as usize)
                .expect("delete exceeds the cached report");
            self.pending_delete_chars = self
                .report
                .get(start..self.report_cursor)
                .expect("fixture byte span must end on UTF-8 boundaries")
                .chars()
                .count();
        }

        fn commit_string(&mut self, text: &str) {
            self.pending_commit.push_str(text);
        }

        fn commit(&mut self, _serial: u32) {
            let n = std::mem::take(&mut self.pending_delete_chars);
            self.delete_chars_before_caret(n);
            let text = std::mem::take(&mut self.pending_commit);
            self.insert(&text);
        }

        fn vk_key(&mut self, _time: u32, key_code: u32, state: KeyState) {
            if key_code == 14 && state == KeyState::Pressed {
                self.delete_chars_before_caret(1);
            }
        }

        fn vk_modifiers(&mut self, _: u32, _: u32, _: u32, _: u32) {}

        fn vk_commit_char(&mut self, _time: u32, _ch: char) -> bool {
            false
        }
    }

    fn type_key(c: &mut Composer, client: &mut CachedReportClient, ch: char) {
        c.mark_action();
        match c.feed_key(ch) {
            KeyDecision::Apply {
                backspaces, commit, ..
            } => c.apply_to_sink(backspaces, &commit, 0, 0, client),
            KeyDecision::ForwardRaw => client.insert(&ch.to_string()),
            KeyDecision::Consumed => {}
        }
    }

    /// Like `deliver_report`, but applies a frame-confirmed repair the way the
    /// Wayland transport does (no new action mark; the key already set it).
    fn deliver_report_with_repairs(
        c: &mut Composer,
        client: &mut CachedReportClient,
        activate: bool,
    ) {
        let (report, cursor) = (client.report.clone(), client.report_cursor as u32);
        c.note_client_surrounding(&report, cursor);
        if c.should_skip_surrounding_frame(&report, cursor, cursor, activate, false) {
            return;
        }
        if let Some(repair) =
            c.observe_surrounding_bytes(&report, ByteCursor(cursor), ByteCursor(cursor), activate)
        {
            c.apply_confirmed_repair_to_sink(repair.backspaces, &repair.commit, 0, 0, client);
        }
    }

    fn apply_edit(
        c: &mut Composer,
        client: &mut CachedReportClient,
        backspaces: usize,
        commit: &str,
    ) {
        c.mark_action();
        c.apply_to_sink(backspaces, commit, 0, 0, client);
    }

    #[test]
    fn cached_report_client_distinguishes_stale_and_live_byte_measurement() {
        // Hand-worked: stale `anse|r` measures two ASCII chars, whereas live
        // `ansư|er` measures one 2-byte scalar. Neither calculation uses the
        // composer's delete-span helpers.
        let mut stale = CachedReportClient::new("ansưer", 5);
        stale.report_stale("anser", 4);
        stale.delete_surrounding_text(2, 1, 0, 0);
        stale.commit_string("w");
        stale.commit(0);
        assert_eq!(stale.text, "anwer");

        let mut live = CachedReportClient::new("ansưer", 5);
        live.delete_surrounding_text(2, 1, 0, 0);
        live.commit_string("w");
        live.commit(0);
        assert_eq!(live.text, "answer");

        let mut keyed = CachedReportClient::new("ansưer", 5);
        keyed.report_stale("anser", 4);
        keyed.vk_key(0, 14, KeyState::Pressed);
        keyed.vk_key(0, 14, KeyState::Released);
        keyed.commit_string("w");
        keyed.commit(0);
        assert_eq!(keyed.text, "answer");
    }

    mod confirmation_with_lagging_reports {
        use super::*;

        /// Firefox reports its caret before its text: each report carries the
        /// new cursor with the text as it was one edit earlier.
        fn lagging(client: &mut CachedReportClient, text: &str) {
            let cursor = client.caret;
            client.report_stale(text, cursor);
        }

        #[test]
        fn ww_after_confirmed_repair_survives_caret_first_report() {
            // `sord`, caret moved after `s`, then `ww`: expected `sword`.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let mut client = CachedReportClient::new("sord", 1);
            client.deliver_report(&mut c, true);

            // First w: frame-derived context, so it goes raw and the frame
            // showing the inserted `w` confirms it and repairs it to `ư`.
            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "sword");
            client.report_live();
            deliver_report_with_repairs(&mut c, &mut client, false);
            assert_eq!(client.text, "sưord");

            // Firefox: caret after `ư` (byte 3), text still `sword`.
            lagging(&mut client, "sword");
            deliver_report_with_repairs(&mut c, &mut client, false);

            // Second w reverts `ư` to a literal `w`.
            // Firefox sent no up-to-date report before the line ended, so a
            // deferred key would never be confirmed.
            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "sword");
            assert_eq!(client.caret, 2);
        }

        #[test]
        fn repeated_pre_repair_report_does_not_defer_next_key() {
            // Fresh line in a rich editor: `sw` is confirmed and repaired to
            // `sư`, then the client repeats the very report that triggered
            // the repair. That repeat is old news, not a cursor move: the
            // second w must revert to a literal `w` straight away.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let mut client = CachedReportClient::new("s", 1);
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'w');
            client.report_live();
            deliver_report_with_repairs(&mut c, &mut client, false);
            assert_eq!(client.text, "sư");

            deliver_report_with_repairs(&mut c, &mut client, false); // `sw|` again
            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "sw");
        }

        /// `sord`, three fast Lefts (no reports in between), then `w`. Firefox
        /// reports the moved caret only after the key: `sord|1` inside the echo
        /// window (skipped), then `sword|2`.
        fn sord_then_fast_lefts_and_w(c: &mut Composer, client: &mut CachedReportClient) {
            for _ in 0..3 {
                c.reset_composition_preserving_surrounding();
                c.defer_action();
            }
            client.caret = 1;
            type_key(c, client, 'w');
            assert_eq!(client.text, "sword");
            client.report_stale("sord", 1);
            deliver_report_with_repairs(c, client, false);
            client.report_live();
            deliver_report_with_repairs(c, client, false);
        }

        #[test]
        fn skipped_caret_report_still_proves_raw_key_insertion() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            // `sor` + `d` restores `sỏ` to raw `sord`; Firefox keeps reporting
            // the old `sỏ|4`, which stays the recorded baseline.
            let mut client = CachedReportClient::new("sỏ", "sỏ".len());
            client.deliver_report(&mut c, true);
            apply_edit(&mut c, &mut client, 2, "sord");
            client.deliver_report(&mut c, false);
            sord_then_fast_lefts_and_w(&mut c, &mut client);
            // `sord|1` -> `sword|2` is exactly the raw w at the caret.
            assert_eq!(client.text, "sưord");

            // The repair proved the caret position. With only caret-first
            // reports afterwards (`sword|3`, then nothing before Enter), the
            // second w must still revert at once.
            lagging(&mut client, "sword");
            deliver_report_with_repairs(&mut c, &mut client, false);
            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "sword");
        }

        #[test]
        fn report_equal_to_stale_baseline_is_not_skipped_as_duplicate() {
            // The recorded baseline is `sword|2` from the previous line (the
            // clear and this line's reports were all skipped as echoes). The
            // new `sword|2` matches that baseline but not the client's
            // previous report, so it is new evidence and must be processed.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let mut client = CachedReportClient::new("sword", 2);
            client.deliver_report(&mut c, true);
            client.text = "sord".to_owned();
            client.caret = 4;
            client.report_live();
            c.mark_action(); // this report lands inside an echo window
            client.deliver_report(&mut c, false);
            sord_then_fast_lefts_and_w(&mut c, &mut client);
            assert_eq!(client.text, "sưord");
        }

        #[test]
        fn caret_elsewhere_after_repair_still_defers_next_edit() {
            // Control: a report whose caret is NOT where our edit put it is a
            // real move (click). The next key must not edit at the old spot.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let mut client = CachedReportClient::new("sord la", 1);
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'w');
            client.report_live();
            deliver_report_with_repairs(&mut c, &mut client, false);
            assert_eq!(client.text, "sưord la");

            // Click after `la` within the echo window.
            client.caret = client.text.len();
            client.report_live();
            deliver_report_with_repairs(&mut c, &mut client, false);
            type_key(&mut c, &mut client, 'f');
            client.report_live();
            deliver_report_with_repairs(&mut c, &mut client, false);
            // Composing `là` or leaving a raw `f` are both safe here; editing
            // with the stale `sư` context (`lừ`, `sừord`) is the bug.
            assert!(
                ["sưord là", "sưord laf"].contains(&client.text.as_str()),
                "{:?}",
                client.text
            );
        }
    }

    mod firefox_contenteditable {
        use super::*;

        #[test]
        fn ww_revert_mid_word_with_stale_report_keeps_preceding_char() {
            // `anser`, caret moved after `s`; Telex `ww` must give `answer`.
            // Firefox reports the first `w`'s result late: old text `anser `
            // with the cursor already one step on (`anse|r`). Measured
            // against that report, the 2-byte delete of `ư` spans `se`,
            // two characters, which also ate the `s` (`anwer`).
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("anser ", 3);
            let (text, cursor) = client.report_live();
            c.observe_surrounding_bytes(&text, ByteCursor(cursor), ByteCursor(cursor), true);

            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "ansưer ");
            let (text, cursor) = client.report_stale("anser ", 4);
            c.observe_surrounding_bytes(&text, ByteCursor(cursor), ByteCursor(cursor), false);

            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "answer ");
        }

        #[test]
        fn ww_revert_with_fresh_report_still_uses_byte_delete() {
            // Same edit with an up-to-date report: the byte delete measures
            // exactly `ư`, so the text-input channel stays in use.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("anser ", 3);
            let (text, cursor) = client.report_live();
            c.observe_surrounding_bytes(&text, ByteCursor(cursor), ByteCursor(cursor), true);

            type_key(&mut c, &mut client, 'w');
            let (text, cursor) = client.report_live();
            c.observe_surrounding_bytes(&text, ByteCursor(cursor), ByteCursor(cursor), false);

            let mut sink = DeleteCaptureSink::default();
            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink),
                _ => panic!("expected ww revert"),
            }
            assert_eq!(sink.deletes, vec![(2, 1, 0, 0)]);
            assert!(sink.vk_keys.is_empty());
        }

        #[test]
        fn tone_on_stale_report_with_queued_raw_key_keeps_word() {
            // `nhieeuf` typed fast in Firefox: when `f` arrives the last
            // report is `nhie|` (neither `ê` nor the raw `u` reported yet).
            // A 3-byte delete of `êu` measured there spans `hie`.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("x ", 2);
            let (text, cursor) = client.report_live();
            c.observe_surrounding_bytes(&text, ByteCursor(cursor), ByteCursor(cursor), true);

            for ch in "nhiee".chars() {
                type_key(&mut c, &mut client, ch);
            }
            assert_eq!(client.text, "x nhiê");
            let (text, cursor) = client.report_stale("x nhie", 6);
            c.observe_surrounding_bytes(&text, ByteCursor(cursor), ByteCursor(cursor), false);
            type_key(&mut c, &mut client, 'u');
            type_key(&mut c, &mut client, 'f');
            assert_eq!(client.text, "x nhiều");
        }

        #[test]
        fn stale_byte_measurement_preserves_preceding_consonant() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("tu suffix", 2);
            client.deliver_report(&mut c, true);
            apply_edit(&mut c, &mut client, 1, "ư");
            assert_eq!(client.text, "tư suffix");

            // The report still measures two bytes as `tu`, not the live `ư`.
            client.deliver_report(&mut c, false);
            apply_edit(&mut c, &mut client, 1, "ự");
            assert_eq!(client.text, "tự suffix");
            assert_eq!(client.caret, "tự".len());
        }

        #[test]
        fn delete_phase_report_does_not_corrupt_the_next_correction() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("prefix tu suffix", "prefix tu".len());
            client.deliver_report(&mut c, true);
            apply_edit(&mut c, &mut client, 1, "ư");
            // A delayed intermediate report contains only the delete phase;
            // the live application has already applied the replacement.
            client.report_stale("prefix t suffix", "prefix t".len());
            client.deliver_report(&mut c, false);
            apply_edit(&mut c, &mut client, 1, "ự");
            assert_eq!(client.text, "prefix tự suffix");
        }

        #[test]
        fn external_edit_after_correction_uses_new_word_not_old_echo() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("tu suffix", 2);
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "tư suffix");

            // External edit after the correction: its pending echo cannot
            // authorize editing the old word instead of this new destination.
            client.text = "la tail".to_owned();
            client.caret = 2;
            client.report_live();
            c.defer_action();
            client.deliver_report(&mut c, false);
            type_key(&mut c, &mut client, 'f');
            assert_eq!(client.text, "là tail");
        }

        #[test]
        fn duplicate_stale_reports_do_not_change_correction_result() {
            for duplicates in [1, 3] {
                let mut c =
                    Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
                let mut client = CachedReportClient::new("tu suffix", 2);
                client.deliver_report(&mut c, true);
                apply_edit(&mut c, &mut client, 1, "ư");
                for _ in 0..duplicates {
                    client.deliver_report(&mut c, false);
                }
                apply_edit(&mut c, &mut client, 1, "ự");
                assert_eq!(client.text, "tự suffix", "{duplicates} duplicate reports");
            }
        }

        #[test]
        fn repeated_corrections_preserve_text_when_reports_lag_then_catch_up() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("tu suffix", 2);
            client.deliver_report(&mut c, true);
            apply_edit(&mut c, &mut client, 1, "ư");
            client.deliver_report(&mut c, false);

            for (glyph, want) in [("ự", "tự suffix"), ("ữ", "tữ suffix")] {
                apply_edit(&mut c, &mut client, 1, glyph);
                assert_eq!(client.text, want);
            }
            client.report_live();
            client.deliver_report(&mut c, false);
            apply_edit(&mut c, &mut client, 1, "ư");
            assert_eq!(client.text, "tư suffix");
        }

        #[test]
        fn stale_report_does_not_corrupt_the_next_word() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("tu", 2);
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'w');
            client.deliver_report(&mut c, false);
            type_key(&mut c, &mut client, 'j');
            assert_eq!(client.text, "tự");
            for ch in " hanhj ".chars() {
                type_key(&mut c, &mut client, ch);
                client.report_live();
                client.deliver_report(&mut c, false);
            }
            assert_eq!(client.text, "tự hạnh ");
        }

        #[test]
        fn english_restore_after_forwarded_char_preserves_surrounding_text() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("d suffix", 1);
            client.deliver_report(&mut c, true);
            for ch in "oe".chars() {
                type_key(&mut c, &mut client, ch);
                client.report_live();
                client.deliver_report(&mut c, false);
            }
            type_key(&mut c, &mut client, 's');
            assert_eq!(client.text, "doé suffix");
            client.deliver_report(&mut c, false); // still `doe| suffix`
            type_key(&mut c, &mut client, 'n');
            client.report_live();
            client.deliver_report(&mut c, false);
            type_key(&mut c, &mut client, 't');
            assert_eq!(client.text, "doesnt suffix");
            assert_eq!(client.caret, "doesnt".len());
        }

        #[test]
        fn stale_cursor_after_word_preserves_multibyte_tone_and_suffix() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("cà phê", "cà".len());
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'r');
            assert_eq!(client.text, "cả phê");

            // Three bytes back from cached `cà |phê` covers `à ` (two
            // scalars), but the real caret is still before the space.
            client.report_stale("cà phê", "cà ".len());
            client.deliver_report(&mut c, false);
            type_key(&mut c, &mut client, 'j');
            assert_eq!(client.text, "cạ phê");
            assert_eq!(client.caret, "cạ".len());
        }

        #[test]
        fn stale_cursor_in_next_word_does_not_edit_that_word() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("là lạ", "là".len());
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'j');
            assert_eq!(client.text, "lạ lạ");

            // The cached cursor drifts into the suffix; the live caret stays
            // at the end of the first word throughout both corrections.
            client.report_stale("lạ lạ", "lạ l".len());
            client.deliver_report(&mut c, false);
            type_key(&mut c, &mut client, 'r');
            assert_eq!(client.text, "lả lạ");
            assert_eq!(client.caret, "lả".len());
        }

        #[test]
        fn mixed_expanding_revert_preserves_text_with_fresh_or_stale_report() {
            // `tuwj` -> `tự`, then w removes the horn but keeps the tone:
            // `tụw`. The former gedit fallback must not dictate a channel;
            // only the resulting text and insertion point are the contract.
            for stale in [false, true] {
                let mut c =
                    Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
                let mut client = CachedReportClient::new("x  suffix", 2);
                client.deliver_report(&mut c, true);
                for ch in "tuwj".chars() {
                    type_key(&mut c, &mut client, ch);
                    client.report_live();
                    client.deliver_report(&mut c, false);
                }
                assert_eq!(client.text, "x tự suffix");
                if stale {
                    client.report_stale("x tư suffix", "x tư".len());
                }
                client.deliver_report(&mut c, false);
                type_key(&mut c, &mut client, 'w');
                assert_eq!(client.text, "x tụw suffix", "stale={stale}");
                assert_eq!(client.caret, "x tụw".len());
            }
        }

        #[test]
        fn tone_replacement_with_fresh_reports_preserves_surrounding_text() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("tu suffix", 2);
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "tư suffix");
            client.report_live();
            client.deliver_report(&mut c, false);
            type_key(&mut c, &mut client, 'j');
            assert_eq!(client.text, "tự suffix");
            assert_eq!(client.caret, "tự".len());
        }

        #[test]
        fn stale_delete_report_does_not_disable_working_commits() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("la tail", 2);
            client.deliver_report(&mut c, true);
            apply_edit(&mut c, &mut client, 1, "ả");
            assert_eq!(client.text, "lả tail");

            client.report_stale("la tail", 3);
            client.deliver_report(&mut c, false);
            apply_edit(&mut c, &mut client, 1, "ạ");
            assert_eq!(client.text, "lạ tail");
            // A stale byte-delete view is not evidence that commits are dead.
            assert!(c.commit_string_functional);
        }

        #[test]
        fn full_reset_preserves_known_dead_commit_channel() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::ForwardKey, false);
            // Transport liveness detection (or a no-text-input activation)
            // marked this channel dead, independently of stale delete reports.
            c.commit_string_functional = false;
            c.full_reset();
            assert!(!c.commit_string_functional);
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

        #[test]
        fn ww_undo_mid_word_preserves_preceding_letter_and_suffix() {
            // Delete w from `sword`, then type ww after s to restore it.
            // The last report predates both w keys; a two-byte delete must
            // not be applied as two live characters.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("sord ", 1);
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "sưord ");
            type_key(&mut c, &mut client, 'w');
            assert_eq!(client.text, "sword ");
            assert_eq!(client.caret, 2);
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

        // A confirmed raw key (stash-hit) proves where our own keystroke
        // landed, so the cursor is known again — the deferral must NOT persist
        // into the following live composition. Regression for a
        // backspace-triggered reseed (killer feature) that left confirmation
        // stuck in FrameDerived: on gedit/KWin every later tone key then
        // flashed raw and repaired via the frame until an external reset
        // (Ctrl+A → Backspace). After a stash-hit repair, the next key from our
        // own stream must compose live.
        #[test]
        fn stash_confirmed_raw_key_does_not_leak_deferral_into_next_key() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);

            // Cursor-jump reseed into an existing word "ho" → FrameDerived
            // (no recent action, not a one-char insertion, not a deletion).
            c.observe_surrounding_bytes("ho", ByteCursor(2), ByteCursor(2), false);

            // 'w' is cursor-uncertain here → forwarded raw and confirm-deferred.
            c.mark_action();
            assert!(matches!(c.feed_key('w'), KeyDecision::ForwardRaw));

            // The client frame proves 'w' landed after "ho": stash-hit repair.
            let repair = c
                .observe_surrounding_bytes("how", ByteCursor(3), ByteCursor(3), false)
                .expect("frame confirming the raw 'w' must repair to ơ");
            assert_eq!((repair.backspaces, repair.commit.as_str()), (2, "ơ"));

            // The cursor is now proven. The next composing key from our own
            // keystroke stream must apply live (ớ), never defer again.
            c.mark_action();
            match c.feed_key('s') {
                KeyDecision::Apply { backspaces, commit, .. } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ớ");
                }
                _ => panic!("live 's' after a confirmed 'w' must compose ớ, not defer"),
            }
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

            // The frame PROVED the (unrepairable) pending key's insertion point,
            // so the deferral must end — confirmation returns to a certain
            // state, not sticky FrameDerived. Otherwise every following key
            // would keep forwarding raw and dropping its commit (see the
            // LibreOffice "hơn" cascade test below).
            assert_eq!(c.confirmation_variant(), "LiveConfirmed");
        }

        // End-to-end shape of the LibreOffice Writer "hơn" drop: an uncertain
        // confirmation state (here induced by idle; in the field by a stale
        // post-backspace frame reseeding to FrameDerived) plus two overlapping
        // raw keys pins the second key `repairable=false`. The authoritative
        // frame then proves that key's insertion point. Before the fix the
        // deferral stayed sticky (FrameDerived), so the next composing key
        // forwarded raw and dropped its commit — leaving raw "how"/"howj"
        // instead of "hơn". It must instead recover to live composition.
        #[test]
        fn unrepairable_but_proven_frame_recovers_to_live_composition() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);

            // Idle >2s on SurroundingText arms an uncertain state (AwaitingFrame)
            // — the same uncertain class a stale-frame reseed produces.
            c.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(c.check_idle_reset());

            // Two overlapping raw keys: 'h' defers (repairable), then 'o' defers
            // while 'h' is still pending → 'o' is marked unrepairable.
            c.mark_action();
            assert!(matches!(c.feed_key('h'), KeyDecision::ForwardRaw));
            c.mark_action();
            assert!(matches!(c.feed_key('o'), KeyDecision::ForwardRaw));

            // The client frame proves 'o' landed after 'h' ("ho"). It cannot
            // emit a repair (unrepairable), but it proves the cursor — so the
            // deferral must end.
            assert!(c
                .observe_surrounding_bytes("ho", ByteCursor(2), ByteCursor(2), false)
                .is_none());
            assert_eq!(c.confirmation_variant(), "LiveConfirmed");

            // The next composing key must apply live (ho + w → ơ), never defer
            // and drop its commit.
            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply { backspaces, commit, .. } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ơ");
                }
                _ => panic!("composing 'w' after a proven-insertion sync must compose ơ, not defer"),
            }
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
        fn idle_on_existing_word_still_requires_insertion_confirmation() {
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

            // Idle must not authorize an edit at an unconfirmed insertion
            // point, irrespective of how word context is represented.
            c.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(c.check_idle_reset());
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

        // After a stale Ctrl+Backspace snapshot, the first retyped key forwards
        // raw. Once its frame proves the insertion point, the cursor is known
        // and later keys compose LIVE (vie→viê, viêt→việt) instead of
        // re-deferring — no per-key raw flash. The retyped word is still not
        // poisoned: the final shadow anchors "tiệng việt".
        #[test]
        fn stale_ctrl_backspace_prefix_composes_live_after_confirmed_repair() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let mut sink = DeleteCaptureSink::default();

            seed_stale_ctrl_backspace_frame(&mut c);
            type_confirmed_raw_prefix(&mut c);
            repair_confirmed_circumflex(&mut c, &mut sink);
            live_tone_after_confirmed_prefix(&mut c, &mut sink);

            assert_eq!(c.shadow_text(), "tiệng việt");
        }

        fn seed_stale_ctrl_backspace_frame(c: &mut Composer) {
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
                false
            ));
        }

        #[test]
        fn first_line_confirmed_repair_keeps_literal_w_history() {
            // A fresh rich-text editor in Firefox reports a placeholder newline.
            // Type `anwwser`: the first w is forwarded then repaired to `ăn`;
            // the second w must escape to literal `anw` and keep `ser` raw.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            c.set_surrounding_confirmation(true);
            let mut sink = DeleteCaptureSink::default();
            // Start at the frame-derived `an` before the first w. This also
            // covers placing the caret after an existing `an`.
            c.observe_surrounding_bytes("an", ByteCursor(2), ByteCursor(2), true);
            c.mark_action();
            assert!(matches!(c.feed_key('w'), KeyDecision::ForwardRaw));
            let repair = c
                .observe_surrounding_bytes("anw", ByteCursor(3), ByteCursor(3), false)
                .expect("first w should be repaired after insertion is confirmed");
            assert_eq!((repair.backspaces, repair.commit.as_str()), (3, "ăn"));
            c.apply_confirmed_repair_to_sink(repair.backspaces, &repair.commit, 0, 0, &mut sink);
            c.observe_surrounding_bytes("ăn", ByteCursor(3), ByteCursor(3), false);

            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!((backspaces, commit.as_str()), (2, "anw"));
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                }
                _ => panic!("second w must revert to literal w"),
            }
            // Firefox repeats the pre-edit report before the next key.
            c.observe_surrounding_bytes("ăn", ByteCursor(3), ByteCursor(3), false);
            for ch in "ser".chars() {
                c.mark_action();
                assert!(
                    matches!(c.feed_key(ch), KeyDecision::ForwardRaw),
                    "unexpected edit for {ch}"
                );
            }
            assert_eq!(c.shadow_text(), "anwser");
        }

        fn type_confirmed_raw_prefix(c: &mut Composer) {
            // Ctrl+Backspace has already removed "vieetjr" in gedit. The first
            // retyped letter proves the real insertion point and replaces the
            // stale snapshot as ground truth.
            assert_forwarded_char_confirmed(c, 'v', "tiệng v mĩ miều");
            assert_eq!(c.shadow_text(), "tiệng v");

            // Continue from surrounding-confirmed context. Raw letters sync;
            // transformations are inserted raw first, then repaired from their
            // confirming frame so later cursor movement cannot race an edit.
            assert_forwarded_char_confirmed(c, 'i', "tiệng vi mĩ miều");
            assert_forwarded_char_confirmed(c, 'e', "tiệng vie mĩ miều");
        }

        fn assert_forwarded_char_confirmed(c: &mut Composer, ch: char, text: &str) {
            c.mark_action();
            assert!(matches!(c.feed_key(ch), KeyDecision::ForwardRaw));
            let cursor = text.split(" mĩ").next().unwrap().len() as u32;
            assert!(c
                .observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false)
                .is_none());
        }

        fn repair_confirmed_circumflex(c: &mut Composer, sink: &mut DeleteCaptureSink) {
            // The confirmed prefix lets the circumflex compose live.
            c.mark_action();
            match c.feed_key('e') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!((backspaces, commit.as_str()), (1, "ê"));
                    c.apply_to_sink(backspaces, &commit, 0, 0, sink);
                }
                _ => panic!("circumflex after a confirmed prefix must compose live"),
            }

            let composed_e = "tiệng viê mĩ miều";
            let composed_e_cursor = "tiệng viê".len() as u32;
            c.observe_surrounding_bytes(
                composed_e,
                ByteCursor(composed_e_cursor),
                ByteCursor(composed_e_cursor),
                false,
            );
            assert_forwarded_char_confirmed(c, 't', "tiệng viêt mĩ miều");
        }

        fn live_tone_after_confirmed_prefix(c: &mut Composer, sink: &mut DeleteCaptureSink) {
            // The circumflex frame already proved the insertion point, so the
            // tone key composes live (viêt → việt) with no raw flash.
            c.mark_action();
            match c.feed_key('j') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 2);
                    assert_eq!(commit, "ệt");
                    c.apply_to_sink(backspaces, &commit, 0, 0, sink);
                }
                _ => panic!("tone 'j' after a stash-hit-confirmed prefix must compose việt live"),
            }
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

            seed_gedit_late_fused_frame(&mut c);
            assert_late_fused_repair(&mut c, &mut sink);
            assert_late_fused_next_tone(&mut c);
        }

        fn seed_gedit_late_fused_frame(c: &mut Composer) {
            // Last confirmed frame was near the start of the word. Ctrl+Right
            // resets composition but preserves that application snapshot; no
            // destination frame follows.
            let baseline = "tiếng việt mĩ miều";
            c.observe_surrounding_bytes(baseline, ByteCursor(1), ByteCursor(1), false);
            c.reset_composition_preserving_surrounding();
            c.defer_action();

            // The 'r' keystroke marks an action, then forwards raw (no seed).
            c.mark_action();
            assert!(matches!(c.feed_key('r'), KeyDecision::ForwardRaw));
            assert_eq!(c.shadow_text(), "r");
        }

        fn assert_late_fused_repair(c: &mut Composer, sink: &mut DeleteCaptureSink) {
            // The fused frame lands ~1 wire event later, already containing the
            // forwarded 'r' at the end of "tiếng": text="tiếngr…" cursor=8.
            let text = "tiếngr việt mĩ miều";
            let cursor = "tiếngr".len() as u32;
            let edit = c
                .observe_surrounding_bytes(text, ByteCursor(cursor), ByteCursor(cursor), false)
                .expect("late fused frame should yield a retro-compose repair");
            assert_eq!(edit.backspaces, 4);
            assert_eq!(edit.commit, "ểng");

            c.apply_confirmed_repair_to_sink(edit.backspaces, &edit.commit, 0, 0, sink);
            assert_eq!(c.shadow_text(), "tiểng");
            let corrected = "tiểng việt mĩ miều";
            let corrected_cursor = "tiểng".len() as u32;
            c.observe_surrounding_bytes(
                corrected,
                ByteCursor(corrected_cursor),
                ByteCursor(corrected_cursor),
                false,
            );
        }

        fn assert_late_fused_next_tone(c: &mut Composer) {
            // The fused frame proved where `r` landed and the corrected echo
            // matched, so the caret is known: the next tone composes live
            // instead of flashing raw and waiting for another frame.
            c.mark_action();
            match c.feed_key('j') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => assert_eq!((backspaces, commit.as_str()), (3, "ệng")),
                _ => panic!("tone after a proven repair must compose live"),
            }
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
        fn idle_reset_still_resets_forward_key_engine() {
            let mut composer =
                Composer::new(InputMethod::Telex, BackspaceMethod::ForwardKey, false);
            assert!(matches!(composer.feed_key('k'), KeyDecision::ForwardRaw));
            assert!(matches!(composer.feed_key('w'), KeyDecision::Apply { .. }));
            assert!(!composer.engine.at_word_beginning());

            composer.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(composer.check_idle_reset());
            assert!(composer.engine.at_word_beginning());
        }

        #[test]
        fn forward_key_idle_discards_shadow_before_unreported_cursor_move() {
            let mut composer =
                Composer::new(InputMethod::Telex, BackspaceMethod::ForwardKey, false);
            let mut sink = DeleteCaptureSink::default();
            let mut visible = String::new();

            for ch in "hoom nay muwa".chars() {
                match composer.feed_key(ch) {
                    KeyDecision::ForwardRaw => visible.push(ch),
                    KeyDecision::Apply {
                        backspaces, commit, ..
                    } => {
                        for _ in 0..backspaces {
                            visible.pop();
                        }
                        visible.push_str(&commit);
                        composer.apply_to_sink(backspaces, &commit, 0, 0, &mut sink);
                    }
                    KeyDecision::Consumed => {}
                }
            }
            assert_eq!(visible, "hôm nay mưa");

            composer.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(composer.check_idle_reset());
            let mut clicked_prefix = "hôm".to_owned();
            match composer.feed_key('r') {
                KeyDecision::ForwardRaw => clicked_prefix.push('r'),
                _ => panic!("stale pre-click word must not receive the tone key"),
            }
            assert_eq!(clicked_prefix, "hômr");
        }

        #[test]
        fn idle_does_not_reset_surrounding_text_engine() {
            let mut composer =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            assert!(matches!(composer.feed_key('k'), KeyDecision::ForwardRaw));
            assert!(matches!(composer.feed_key('w'), KeyDecision::Apply { .. }));
            assert!(!composer.engine.at_word_beginning());

            composer.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(composer.check_idle_reset());
            assert!(!composer.engine.at_word_beginning());
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
        fn confirmed_live_word_applies_tone_immediately_after_idle() {
            let mut composer =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();
            composer.set_surrounding_confirmation(true);

            for (ch, text) in [('m', "m"), ('i', "mi")] {
                composer.mark_action();
                assert!(matches!(composer.feed_key(ch), KeyDecision::ForwardRaw));
                let cursor = text.len() as u32;
                composer.observe_surrounding_bytes(
                    text,
                    ByteCursor(cursor),
                    ByteCursor(cursor),
                    false,
                );
            }

            composer.mark_action();
            let KeyDecision::Apply {
                backspaces, commit, ..
            } = composer.feed_key('x')
            else {
                panic!("x must compose mĩ");
            };
            assert_eq!((backspaces, commit.as_str()), (1, "ĩ"));
            composer.apply_to_sink(backspaces, &commit, 1, 0, &mut sink);
            composer.observe_surrounding_bytes("mĩ", ByteCursor(3), ByteCursor(3), false);

            composer.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(composer.check_idle_reset());
            composer.mark_action();
            match composer.feed_key('s') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => assert_eq!((backspaces, commit.as_str()), (1, "í")),
                _ => panic!("confirmed live word must not flash a raw tone key"),
            }
        }

        #[test]
        fn mismatching_frame_revokes_live_confirmation_even_when_rejected() {
            let mut composer =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();
            composer.set_surrounding_confirmation(true);

            for (ch, text) in [('m', "m"), ('i', "mi")] {
                composer.mark_action();
                assert!(matches!(composer.feed_key(ch), KeyDecision::ForwardRaw));
                let cursor = text.len() as u32;
                composer.observe_surrounding_bytes(
                    text,
                    ByteCursor(cursor),
                    ByteCursor(cursor),
                    false,
                );
            }

            composer.mark_action();
            let KeyDecision::Apply {
                backspaces, commit, ..
            } = composer.feed_key('x')
            else {
                panic!("x must compose mĩ");
            };
            composer.apply_to_sink(backspaces, &commit, 1, 0, &mut sink);
            composer.observe_surrounding_bytes("mĩ", ByteCursor(3), ByteCursor(3), false);

            // A delayed cursor frame can arrive inside the recent-action
            // rejection window. Even when ignored for reseeding, it disproves
            // the earlier live confirmation.
            composer.mark_action();
            composer.observe_surrounding_bytes("other", ByteCursor(5), ByteCursor(5), false);
            composer.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(composer.check_idle_reset());
            composer.mark_action();
            assert!(matches!(composer.feed_key('s'), KeyDecision::ForwardRaw));
        }

        #[test]
        fn delayed_live_echo_forwards_then_repairs_tone_key() {
            let mut composer =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut sink = DeleteCaptureSink::default();
            composer.set_surrounding_confirmation(true);

            for (ch, text) in [('m', "m"), ('i', "mi")] {
                composer.mark_action();
                assert!(matches!(composer.feed_key(ch), KeyDecision::ForwardRaw));
                let cursor = text.len() as u32;
                composer.observe_surrounding_bytes(
                    text,
                    ByteCursor(cursor),
                    ByteCursor(cursor),
                    false,
                );
            }

            composer.mark_action();
            let KeyDecision::Apply {
                backspaces, commit, ..
            } = composer.feed_key('x')
            else {
                panic!("x must compose mĩ");
            };
            composer.apply_to_sink(backspaces, &commit, 1, 0, &mut sink);

            composer.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(composer.check_idle_reset());
            composer.mark_action();
            assert!(matches!(composer.feed_key('s'), KeyDecision::ForwardRaw));

            let repair = composer
                .observe_surrounding_bytes("mĩs", ByteCursor(4), ByteCursor(4), false)
                .expect("combined delayed frame must repair the raw tone key");
            assert_eq!((repair.backspaces, repair.commit.as_str()), (2, "í"));
        }

        #[test]
        fn cursor_move_over_existing_suffix_does_not_confirm_raw_key() {
            let mut composer =
                Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            composer.set_surrounding_confirmation(true);
            composer.observe_surrounding_bytes("mĩs", ByteCursor(3), ByteCursor(3), false);

            composer.last_keystroke_at -= std::time::Duration::from_secs(3);
            assert!(composer.check_idle_reset());
            composer.mark_action();
            assert!(matches!(composer.feed_key('s'), KeyDecision::ForwardRaw));

            // The document did not change; only the cursor moved over an
            // existing `s`. Shadow equality must not authorize a repair.
            assert!(composer
                .observe_surrounding_bytes("mĩs", ByteCursor(4), ByteCursor(4), false)
                .is_none());
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
        fn deleting_seeded_vowel_does_not_tone_the_deleted_letter() {
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("la world", 2);
            client.deliver_report(&mut c, true);
            c.mark_action();
            match c.feed_backspace() {
                KeyDecision::ForwardRaw => client.delete_chars_before_caret(1),
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    c.apply_to_sink(backspaces, &commit, 0, 0, &mut client);
                }
                KeyDecision::Consumed => {}
            }
            assert_eq!(client.text, "l world");
            type_key(&mut c, &mut client, 'f');
            assert_eq!(client.text, "lf world");
        }

        #[test]
        fn forwardkey_backspace_after_raw_restore_reseeds_so_tone_recomposes() {
            // foot and other terminals advertise surrounding-text but send only
            // empty frames, so the liveness watchdog downgrades ST→FK and no
            // frame ever resyncs engine state. Field repro (typing "tự"):
            //   tuwj → tự, then 'd' raw-restores the invalid syllable to "tuwjd",
            //   three forwarded Backspaces bring the visible text to "tu", and
            //   the next 'w' must re-compose ư. Before the reseed the engine held
            //   a stale NonVn buffer and 'w' forwarded raw, leaving "tuw".
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::ForwardKey, false);
            let mut sink = DeleteCaptureSink::default();
            let mut visible = String::new();

            for ch in ['t', 'u', 'w', 'j'] {
                press_visible(&mut c, &mut sink, &mut visible, ch);
            }
            assert_eq!(visible, "tự");

            // 'd' → invalid syllable → raw restore to "tuwjd".
            press_visible(&mut c, &mut sink, &mut visible, 'd');
            assert_eq!(visible, "tuwjd");

            // Forwarded Backspaces back down to "tu".
            for _ in 0..3 {
                c.mark_action();
                assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
                visible.pop();
            }
            assert_eq!(visible, "tu");

            // The re-typed tone must compose ư now, not forward raw.
            c.mark_action();
            match c.feed_key('w') {
                KeyDecision::Apply {
                    backspaces, commit, ..
                } => {
                    assert_eq!(backspaces, 1);
                    assert_eq!(commit, "ư");
                }
                _ => panic!("'w' on re-exposed \"tu\" must compose ư, not forward raw"),
            }
        }

        #[test]
        fn forwardkey_backspace_keeps_english_word_raw() {
            // The reseed must not turn English into Vietnamese. Type "him"
            // (no Telex tone/modifier keys), Backspace to "hi", then retype 'm'.
            // "hi" round-trips the render gate and seeds the engine, but 'm' has
            // no vowel to tone, so it forwards raw — "him" stays "him".
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::ForwardKey, false);
            let mut sink = DeleteCaptureSink::default();
            let mut visible = String::new();

            for ch in ['h', 'i', 'm'] {
                assert!(matches!(
                    press_visible(&mut c, &mut sink, &mut visible, ch),
                    KeyDecision::ForwardRaw
                ));
            }
            assert_eq!(visible, "him");

            c.mark_action();
            assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
            visible.pop();
            assert_eq!(visible, "hi");

            assert!(
                matches!(
                    press_visible(&mut c, &mut sink, &mut visible, 'm'),
                    KeyDecision::ForwardRaw
                ),
                "retyped 'm' after an English backspace must stay raw"
            );
            assert_eq!(visible, "him");
        }

        #[test]
        fn forwardkey_backspace_of_vowel_does_not_tone_a_stale_seed() {
            // The ForwardKey analog of the SurroundingText
            // `forwarded_ascii_backspace_on_seeded_word_stays_consistent`
            // guard. Type "la", Backspace the vowel to "l", then 'f'. The
            // reseed feeds "l" (which round-trips) but it has no vowel, so 'f'
            // must forward raw ("lf"), NOT compose a stale "l"+a→"là" edit.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::ForwardKey, false);
            let mut sink = DeleteCaptureSink::default();
            let mut visible = String::new();

            for ch in ['l', 'a'] {
                assert!(matches!(
                    press_visible(&mut c, &mut sink, &mut visible, ch),
                    KeyDecision::ForwardRaw
                ));
            }
            assert_eq!(visible, "la");

            c.mark_action();
            assert!(matches!(c.feed_backspace(), KeyDecision::ForwardRaw));
            visible.pop();
            assert_eq!(visible, "l");

            assert!(
                matches!(
                    press_visible(&mut c, &mut sink, &mut visible, 'f'),
                    KeyDecision::ForwardRaw
                ),
                "'f' after deleting the vowel must forward raw, not tone a stale seed"
            );
            assert_eq!(visible, "lf");
        }

        #[test]
        fn consecutive_tone_updates_preserve_other_words_despite_stale_cursor() {
            // The English prefix must not affect corrections of the word at
            // the caret, whether its state is retained or reconstructed.
            let mut c = Composer::new(InputMethod::Telex, BackspaceMethod::SurroundingText, false);
            let mut client = CachedReportClient::new("wor ơ x", "wor ơ".len());
            client.deliver_report(&mut c, true);
            type_key(&mut c, &mut client, 'r');
            assert_eq!(client.text, "wor ở x");

            client.report_stale("wor ơ x", "wor ơ ".len());
            client.deliver_report(&mut c, false);
            type_key(&mut c, &mut client, 'j');
            assert_eq!(client.text, "wor ợ x");
            assert_eq!(client.caret, "wor ợ".len());
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

        #[test]
        fn confirmation_state_only_pending_variant_contains_raw_key() {
            let pending = super::ConfirmationState::PendingRaw(super::PendingRaw {
                ch: 's',
                repairable: true,
                edit: None,
                from_frame: false,
            });

            assert_eq!(pending.pending_raw().map(|pending| pending.ch), Some('s'));
            assert!(super::ConfirmationState::Live.pending_raw().is_none());
            assert!(super::ConfirmationState::LiveConfirmed
                .pending_raw()
                .is_none());
            assert!(super::ConfirmationState::FrameDerived
                .pending_raw()
                .is_none());
            assert!(super::ConfirmationState::AwaitingFrame
                .pending_raw()
                .is_none());
        }

        #[test]
        fn overlapping_pending_key_becomes_non_repairable() {
            let state = super::ConfirmationState::PendingRaw(super::PendingRaw {
                ch: 's',
                repairable: true,
                edit: None,
                from_frame: false,
            });

            let replacement = state.replacement_pending('f', None);
            let pending = replacement
                .pending_raw()
                .expect("pending replacement state");

            assert_eq!(pending.ch, 'f');
            assert!(!pending.repairable);
        }

        #[test]
        fn selected_frame_preserves_pending_frame_provenance() {
            let mut composer = super::Composer::new(
                super::InputMethod::Telex,
                super::BackspaceMethod::SurroundingText,
                false,
            );
            composer.set_surrounding_confirmation(true);
            composer.edit.on_surrounding_text("foo", 3, 3);
            composer.edit.record_surrounding("foo", 3, 3);
            composer.confirmation = super::ConfirmationState::PendingRaw(super::PendingRaw {
                ch: 'x',
                repairable: true,
                edit: None,
                from_frame: true,
            });

            assert!(composer.apply_surrounding("foobar", 3, 6, false).is_none());
            assert!(matches!(
                composer.confirmation,
                super::ConfirmationState::FrameDerived
            ));
        }

        #[test]
        fn key_start_plan_requires_confirmation_for_every_uncertain_state() {
            for confirmation in [
                super::ConfirmationState::FrameDerived,
                super::ConfirmationState::AwaitingFrame,
                super::ConfirmationState::PendingRaw(super::PendingRaw {
                    ch: 'r',
                    repairable: true,
                    edit: None,
                    from_frame: false,
                }),
            ] {
                let facts = super::FeedKeyFacts {
                    ch: 's',
                    method: super::BackspaceMethod::SurroundingText,
                    confirmation_enabled: true,
                    confirmation,
                    previous_was_separator: false,
                    engine_at_word_beginning: false,
                    shadow_word: String::new(),
                };

                assert!(super::plan_key_start(&facts).context_uncertain);
            }
        }

        #[test]
        fn key_finish_plan_never_applies_from_uncertain_context() {
            let result = viet_ime_engine::ProcessResult {
                consumed: true,
                backspaces: 1,
                commit: "á".to_owned(),
            };

            let plan = super::plan_key_finish('s', true, false, false, result, "a".to_owned());

            assert!(matches!(plan, super::KeyFinishPlan::Forward { .. }));
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
            assert!(oci("ab", 2, "abc", 3));
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

    fn press_visible(
        c: &mut Composer,
        sink: &mut DeleteCaptureSink,
        visible: &mut String,
        ch: char,
    ) -> KeyDecision {
        c.mark_action();
        let decision = c.feed_key(ch);
        match &decision {
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
        decision
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

        press_visible(&mut c, &mut sink, &mut visible, 'h');
        c.observe_surrounding_bytes(url, ByteCursor(1), ByteCursor(n), false);

        press_visible(&mut c, &mut sink, &mut visible, 'a');
        c.observe_surrounding_bytes(url, ByteCursor(1), ByteCursor(n), false);
        c.observe_surrounding_bytes("ha", ByteCursor(2), ByteCursor(2), false);

        let decision = press_visible(&mut c, &mut sink, &mut visible, 'f');
        match decision {
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
                    KeyDecision::Apply { .. } => "Apply",
                }
            ),
        }
        assert_eq!(visible, "hà");
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
