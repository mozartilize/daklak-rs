/// Accumulates the per-frame state events (surrounding_text, text_change_cause,
/// content_type) that arrive between activate and done. Per spec: state updates
/// become active at the done() boundary.
///
/// Mirrors the DoneFrame pattern in tools/probe/src/main.rs.
#[derive(Default)]
pub struct DoneFrame {
    pub pending_activate: bool,
    pub pending_deactivate: bool,
    pub surrounding_text: Option<SurroundingText>,
    pub change_cause: Option<u32>,
    pub purpose: u32, // from content_type hint/purpose
}

#[derive(Debug, Clone)]
pub struct SurroundingText {
    pub text: String,
    pub cursor: u32,
    pub anchor: u32,
}

impl DoneFrame {
    pub fn reset(&mut self) {
        self.pending_activate = false;
        self.pending_deactivate = false;
        self.surrounding_text = None;
        self.change_cause = None;
        // purpose stays sticky WITHIN a session; cleared on session boundary
    }

    /// Called when a deactivate frame applies — the next activate is a fresh
    /// text-input session and must not inherit the previous app's purpose.
    /// Without this, e.g. focusing chromium right after foot keeps purpose=13
    /// (PURPOSE_TERMINAL) and mis-routes chromium through the terminal arm of
    /// `detect_method`.
    pub fn end_session(&mut self) {
        self.purpose = 0; // PURPOSE_NORMAL default
    }
}
