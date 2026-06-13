/// Shadow buffer — tracks the text immediately before the cursor that the
/// daemon has committed during the current word. Used to compute the byte
/// count for `delete_surrounding_text` (Tier 1) and to detect cursor-delta
/// invalidation (cursor-delta is the priority-1 signal).
pub struct ShadowBuffer {
    text: String,
    last_cursor: Option<u32>,
    /// Selection extent on the "before cursor" side from the latest
    /// surrounding_text frame (anchor < cursor), measured in chars.
    selected_before_chars: usize,
    /// Selection extent on the "after cursor" side from the latest
    /// surrounding_text frame (anchor > cursor), measured in bytes/chars.
    /// Tier 1 delete_surrounding_text must include this range, otherwise
    /// chromium rejects the delete when a selection exists.
    selected_after_bytes: u32,
    selected_after_chars: u32,
    /// True after daemon issues a delete+commit. The compositor will echo
    /// back a surrounding_text with the new cursor position — we skip the
    /// cursor-delta check for exactly one frame.
    pub pending_commit: bool,
}

impl Default for ShadowBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ShadowBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            last_cursor: None,
            selected_before_chars: 0,
            selected_after_bytes: 0,
            selected_after_chars: 0,
            pending_commit: false,
        }
    }

    pub fn append(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// Pop `n` chars from the end of the shadow. Returns `(bytes, chars)`
    /// — the byte count is what wlroots v2/v3 IM (and most v3 clients on
    /// KWin) want for `delete_surrounding_text(before_length)`; the char
    /// count is what firefox specifically wants on its KWin v1↔v3 path
    /// (see `force_chars_delete_apps` config). `chars` may be less than
    /// the requested `n` if the shadow runs out.
    pub fn pop_chars(&mut self, n: usize) -> (u32, u32) {
        let mut byte_count: u32 = 0;
        let mut char_count: u32 = 0;
        for _ in 0..n {
            match self.text.pop() {
                Some(ch) => {
                    byte_count += ch.len_utf8() as u32;
                    char_count += 1;
                }
                None => break,
            }
        }
        (byte_count, char_count)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.last_cursor = None;
        self.selected_before_chars = 0;
        self.selected_after_bytes = 0;
        self.selected_after_chars = 0;
        self.pending_commit = false;
    }

    /// Build delete_surrounding_text lengths for Tier 1 from engine
    /// `backspaces` and the current compositor selection span.
    ///
    /// Returns `(before_bytes, before_chars, after_bytes, after_chars)`.
    /// If anchor < cursor, selected-before chars are folded into `before_*`.
    /// If anchor > cursor, selected-after bytes/chars are emitted via
    /// `after_*` so clients that require deleting the full selection (e.g.
    /// chromium) accept the operation.
    pub fn pop_delete_span(&mut self, backspaces: usize) -> (u32, u32, u32, u32) {
        let before_target = backspaces.saturating_add(self.selected_before_chars);
        let (before_bytes, before_chars) = self.pop_chars(before_target);
        let after_bytes = self.selected_after_bytes;
        let after_chars = self.selected_after_chars;
        self.selected_before_chars = 0;
        self.selected_after_bytes = 0;
        self.selected_after_chars = 0;
        (before_bytes, before_chars, after_bytes, after_chars)
    }

    /// Observe a surrounding_text frame from the compositor and sync the
    /// shadow to it. `text[..cursor]` becomes the authoritative content
    /// before the cursor — Tier 1 reads it to compute byte counts for
    /// `delete_surrounding_text`.
    ///
    /// Returns `true` if the cursor moved unexpectedly (no daemon action
    /// pending) — caller may use this to detect mouse clicks / arrow keys
    /// the daemon didn't catch via the keyboard grab.
    pub fn observe_surrounding(&mut self, text: &str, cursor: u32, anchor: u32) -> bool {
        // Find a valid char boundary at or before `cursor` (compositor
        // *should* guarantee this, but be defensive).
        let cursor_usize = (cursor as usize).min(text.len());
        let cursor_boundary = (0..=cursor_usize)
            .rev()
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(0);

        // Ditto for `anchor`.
        let anchor_usize = (anchor as usize).min(text.len());
        let anchor_boundary = (0..=anchor_usize)
            .rev()
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(0);

        let unexpected_move = !self.pending_commit
            && self
                .last_cursor
                .map(|last| last != cursor)
                .unwrap_or(false);

        // Sync shadow to compositor's view of text before cursor.
        self.text.clear();
        self.text.push_str(&text[..cursor_boundary]);

        // Track selection span relative to cursor.
        if anchor_boundary < cursor_boundary {
            let selected_before = &text[anchor_boundary..cursor_boundary];
            self.selected_before_chars = selected_before.chars().count();
            self.selected_after_bytes = 0;
            self.selected_after_chars = 0;
        } else if anchor_boundary > cursor_boundary {
            let selected_after = &text[cursor_boundary..anchor_boundary];
            self.selected_before_chars = 0;
            self.selected_after_bytes = selected_after.len() as u32;
            self.selected_after_chars = selected_after.chars().count() as u32;
        } else {
            self.selected_before_chars = 0;
            self.selected_after_bytes = 0;
            self.selected_after_chars = 0;
        }

        self.last_cursor = Some(cursor);
        self.pending_commit = false;

        unexpected_move
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn text_mut(&mut self) -> &mut String {
        &mut self.text
    }

    pub fn last_cursor(&self) -> Option<u32> {
        self.last_cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_ascii_char() {
        let mut buf = ShadowBuffer::new();
        buf.append("a");
        assert_eq!(buf.pop_chars(1), (1, 1));
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn pop_multibyte_char() {
        let mut buf = ShadowBuffer::new();
        buf.append("â"); // U+00E2, 2 bytes in UTF-8
        assert_eq!(buf.pop_chars(1), (2, 1));
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn pop_three_byte_char() {
        let mut buf = ShadowBuffer::new();
        buf.append("ầ"); // U+1EA7, 3 bytes in UTF-8
        assert_eq!(buf.pop_chars(1), (3, 1));
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn pop_partial_mixed() {
        let mut buf = ShadowBuffer::new();
        buf.append("trâ"); // t=1, r=1, â=2 → total 4 bytes; pop 1 char = 2 bytes
        assert_eq!(buf.pop_chars(1), (2, 1));
        assert_eq!(buf.text(), "tr");
    }

    #[test]
    fn pop_more_than_available() {
        let mut buf = ShadowBuffer::new();
        buf.append("a");
        assert_eq!(buf.pop_chars(5), (1, 1)); // only 1 available
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn observe_surrounding_syncs_text_before_cursor() {
        // Shadow is replaced with text[..cursor] from compositor.
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("hello world", 5, 5);
        assert_eq!(buf.text(), "hello");
        assert_eq!(buf.last_cursor(), Some(5));
    }

    #[test]
    fn observe_surrounding_handles_multibyte_cursor() {
        // cursor in bytes — "làn" = 4 bytes (l=1, à=2, n=1)
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("lành", 4, 4);
        assert_eq!(buf.text(), "làn");
    }

    #[test]
    fn unexpected_cursor_jump_reports_true() {
        // Cursor moved without pending_commit → unexpected (mouse click)
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("hello", 5, 5);
        let unexpected = buf.observe_surrounding("hello", 1, 1);
        assert!(unexpected);
        assert_eq!(buf.text(), "h");
    }

    #[test]
    fn pending_commit_marks_cursor_change_expected() {
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("trâ", 4, 4);
        buf.pending_commit = true;
        let unexpected = buf.observe_surrounding("trầ", 4, 4);
        assert!(!unexpected);
        assert!(!buf.pending_commit);
    }

    #[test]
    fn pop_delete_span_includes_selected_after_cursor() {
        // Mirrors Chromium omnibox inline-autocomplete selection shape seen
        // with Google search provider history suggestions:
        // "tra|nslate" (cursor=3, anchor=9).
        //
        // pop_delete_span returns the selection-after bytes/chars so the
        // caller (surrounding::apply) can detect the selection and fall back
        // to ForwardKey backspaces. The shadow text is updated (popped) for
        // the before-cursor chars; the after-cursor selection is reported
        // but not used for delete_surrounding_text (ForwardKey handles it).
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("translate", 3, 9);
        assert_eq!(buf.text(), "tra");

        let (before_b, before_c, after_b, after_c) = buf.pop_delete_span(1);
        assert_eq!((before_b, before_c), (1, 1));
        assert_eq!((after_b, after_c), (6, 6));
        assert_eq!(buf.text(), "tr");
    }
}
