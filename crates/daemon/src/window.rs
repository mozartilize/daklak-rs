use std::time::Instant;

use viet_ime_edit_strategy::{BackspaceMethod, Strategy};
use viet_ime_engine::{EngineState, InputMethod};

/// Per-text-input-object state. On wlroots/Sway, one instance at a time
/// (compositor sends deactivate before new activate).
pub struct WindowState {
    pub engine: EngineState,
    pub strategy: Strategy,
    pub method: BackspaceMethod,
    pub last_keystroke_at: Instant,

    /// Last printable user keystroke fed to `handle_char`. Tracks user
    /// intent independently of the shadow buffer — the shadow can drop
    /// just-forwarded chars when a stale surrounding_text echo arrives
    /// before the compositor commits the new state. Used to gate the
    /// word-boundary seed: skip seeding when the previous keystroke was
    /// a separator (whitespace/punct/etc.), because a new word is
    /// starting and seeding from the prior word would poison the
    /// engine state.
    pub last_input_char: Option<char>,

    // ── Surrounding-text diff tracking (v1 IM / KWin path) ──────────────
    /// Previous surrounding_text from the last on_done_frame. Used to
    /// detect character insertions by diff, so we can drive the engine
    /// without intercepting physical key events.
    pub prev_text: String,
    /// Byte cursor position matching prev_text.
    pub prev_cursor: u32,
    /// v1/KWin path: after daklak emits delete+commit, kate sends a flurry
    /// of intermediate SurroundingText echoes (pre-delete, post-delete,
    /// post-commit). Time-based gating dropped fast user keystrokes that
    /// arrived in that window. Instead, track the EXPECTED post-apply
    /// (text, cursor) — skip frames until that target is matched (echo
    /// resync) OR text grows past the target (user typed ahead, resync to
    /// target and process the user diff against it).
    pub pending_apply_target: Option<(String, u32)>,
    /// v1/KWin path: original Telex chars typed in the current word
    /// (ASCII). Reset on word boundary. Used to seed the engine on every
    /// keystroke so multi-char tone rules see the full raw context
    /// (engine forgets internal state after returning a transform, so
    /// `tieengs`'s sắc tone only fires when fed `tieeng` not `tiêng`).
    pub raw_word: String,

    /// When true, `delete_surrounding_text` on the V1Kde sink emits a
    /// CHAR count rather than the spec-compliant byte count. Set at
    /// activate when `app_id` matches `force_chars_delete_apps` (firefox
    /// by default). Other v3 clients (chromium/Qt/GTK) honor bytes per
    /// spec — flipping universally would break them. See
    /// crates/edit-strategy/src/surrounding.rs and the firefox bug
    /// (` ơr`→`ở`).
    pub chars_for_delete: bool,
}

impl WindowState {
    pub fn new(input_method: InputMethod, backspace_method: BackspaceMethod) -> Self {
        Self {
            engine: EngineState::new(input_method),
            strategy: Strategy::new(backspace_method),
            method: backspace_method,
            last_keystroke_at: Instant::now(),
            last_input_char: None,
            prev_text: String::new(),
            prev_cursor: 0,
            pending_apply_target: None,
            raw_word: String::new(),
            chars_for_delete: false,
        }
    }

    /// Reset compose state — call on deactivate / navigation key /
    /// modifier shortcut / external cursor movement. Wipes everything
    /// that tracks an in-progress word: engine, shadow, raw_word,
    /// last_input_char, and the v1 surrounding-text diff bookkeeping
    /// (prev_text, prev_cursor, pending_apply_target).
    pub fn full_reset(&mut self) {
        self.engine.reset();
        self.strategy.reset_shadow();
        self.last_input_char = None;
        self.raw_word.clear();
        self.prev_text.clear();
        self.prev_cursor = 0;
        self.pending_apply_target = None;
    }

    /// Check 2-second idle heuristic. Returns true (and resets engine) if
    /// the gap since last keystroke exceeds 2s — user may have clicked mouse.
    ///
    /// Resets engine only, NOT shadow: the killer-feature seed at word
    /// boundary (handle_char) reads from shadow to recover word context for
    /// retroactive composition. e.g. user types `la`, waits 5s, types `f` —
    /// shadow still holds "la" so engine gets seeded → `là` composes. If
    /// the cursor moved during idle, the next surrounding_text frame
    /// resyncs shadow via observe_surrounding.
    pub fn check_idle_reset(&mut self) -> bool {
        if self.last_keystroke_at.elapsed().as_secs() >= 2 {
            self.engine.reset();
            return true;
        }
        false
    }
}
