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
}

impl WindowState {
    pub fn new(input_method: InputMethod, backspace_method: BackspaceMethod) -> Self {
        Self {
            engine: EngineState::new(input_method),
            strategy: Strategy::new(backspace_method),
            method: backspace_method,
            last_keystroke_at: Instant::now(),
        }
    }

    /// Reset both engine and shadow — call on deactivate / navigation key /
    /// external cursor movement.
    pub fn full_reset(&mut self) {
        self.engine.reset();
        self.strategy.reset_shadow();
    }

    /// Check 2-second idle heuristic. Returns true (and resets engine) if
    /// the gap since last keystroke exceeds 2s — user may have clicked mouse.
    ///
    /// Resets engine only, NOT shadow: the killer-feature seed at word
    /// boundary (handle_char) reads from shadow to recover word context for
    /// retroactive composition. e.g. user types `ha`, waits 5s, types `f` —
    /// shadow still holds "ha" so engine gets seeded → `hà` composes. If
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
