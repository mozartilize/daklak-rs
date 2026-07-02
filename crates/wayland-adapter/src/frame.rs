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
    /// Nothing routes on `purpose` any more, but it is still logged per frame;
    /// clearing it here keeps that diagnostic accurate (e.g. focusing chromium
    /// right after foot must not keep the stale purpose=13 terminal hint).
    pub fn end_session(&mut self) {
        self.purpose = 0; // PURPOSE_NORMAL default
    }
}

/// Protocol-agnostic view of the input-method events that mutate
/// `DoneFrame` between activate and done. Each backend dispatcher
/// (`dispatch.rs` for v2/wlroots, `dispatch_v1.rs` for v1/KWin)
/// translates its native protocol Event into this enum and calls
/// `AdapterState::apply_event` — keeping the per-field mutation
/// logic, tracing, and v1-only `pending_commit` bookkeeping in a
/// single place. Adding a new variant fails to compile in
/// `apply_event` until handled, blocking silent drift.
#[derive(Debug)]
pub enum FrameEvent {
    Activate,
    Deactivate,
    SurroundingText { text: String, cursor: u32, anchor: u32 },
    /// v3-numbered content-type purpose (`PURPOSE_*`). v1 dispatcher
    /// translates v1 numbering → v3 before constructing this variant.
    Purpose(u32),
    /// v2-only; v1's `zwp_input_method_context_v1` has no text-change-cause.
    ChangeCause(u32),
    /// v2-only; sent when another IM is already registered with the
    /// compositor. Triggers daemon exit.
    Unavailable,
}
