/// Shadow buffer — tracks the text immediately before the cursor that the
/// daemon has committed during the current word. Used to compute the byte
/// count for `delete_surrounding_text` (Tier 1) and to detect cursor-delta
/// invalidation (see plan0.md priority-1 invalidation).
pub struct ShadowBuffer {
    text: String,
    last_cursor: Option<u32>,
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
        self.pending_commit = false;
    }

    /// Observe a surrounding_text frame from the compositor and sync the
    /// shadow to it. `text[..cursor]` becomes the authoritative content
    /// before the cursor — Tier 1 reads it to compute byte counts for
    /// `delete_surrounding_text`.
    ///
    /// Returns `true` if the cursor moved unexpectedly (no daemon action
    /// pending) — caller may use this to detect mouse clicks / arrow keys
    /// the daemon didn't catch via the keyboard grab.
    pub fn observe_surrounding(&mut self, text: &str, cursor: u32) -> bool {
        // Find a valid char boundary at or before `cursor` (compositor
        // *should* guarantee this, but be defensive).
        let cursor_usize = (cursor as usize).min(text.len());
        let cursor_boundary = (0..=cursor_usize)
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
        buf.observe_surrounding("hello world", 5);
        assert_eq!(buf.text(), "hello");
        assert_eq!(buf.last_cursor(), Some(5));
    }

    #[test]
    fn observe_surrounding_handles_multibyte_cursor() {
        // cursor in bytes — "làn" = 4 bytes (l=1, à=2, n=1)
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("lành", 4);
        assert_eq!(buf.text(), "làn");
    }

    #[test]
    fn unexpected_cursor_jump_reports_true() {
        // Cursor moved without pending_commit → unexpected (mouse click)
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("hello", 5);
        let unexpected = buf.observe_surrounding("hello", 1);
        assert!(unexpected);
        assert_eq!(buf.text(), "h");
    }

    #[test]
    fn pending_commit_marks_cursor_change_expected() {
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("trâ", 4);
        buf.pending_commit = true;
        let unexpected = buf.observe_surrounding("trầ", 4);
        assert!(!unexpected);
        assert!(!buf.pending_commit);
    }
}
