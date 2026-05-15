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
        // purpose persists across frames (sticky per text-input object)
    }
}
