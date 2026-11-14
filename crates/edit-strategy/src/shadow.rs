/// Predicted text before the insertion point, excluding any selection.
/// Measures character/byte deletion spans; the caller decides which client
/// frames are trustworthy before adopting them here.
pub struct ShadowBuffer {
    text: String,
    /// Selection extent on the "before cursor" side from the latest
    /// surrounding_text frame (anchor < cursor), measured in chars.
    selected_before_chars: usize,
    /// Selection extent on the "after cursor" side from the latest
    /// surrounding_text frame (anchor > cursor), measured in bytes/chars.
    /// Tier 1 delete_surrounding_text must include this range, otherwise
    /// chromium rejects the delete when a selection exists.
    selected_after_bytes: u32,
    selected_after_chars: u32,
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
            selected_before_chars: 0,
            selected_after_bytes: 0,
            selected_after_chars: 0,
        }
    }

    pub fn append(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// Pop `n` chars from the end of the shadow. Returns `(bytes, chars)`
    /// — the byte count is what text-input-v3 wants for
    /// `delete_surrounding_text(before_length)`; the char count is what
    /// IBus wants for its char-indexed equivalent. `chars` may be less
    /// than the requested `n` if the shadow runs out.
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

    pub fn has_selection(&self) -> bool {
        self.selected_before_chars > 0 || self.selected_after_chars > 0
    }

    /// The shadow already excludes selected text in either direction.
    pub fn clear_selection(&mut self) {
        self.selected_before_chars = 0;
        self.selected_after_bytes = 0;
        self.selected_after_chars = 0;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.clear_selection();
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

    /// Adopt a trusted surrounding_text frame. Offsets are bytes; selected
    /// text is excluded from the shadow and tracked as a deletion span.
    pub fn observe_surrounding(&mut self, text: &str, cursor: u32, anchor: u32) {
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

        // Sync shadow to compositor's view of text before the insertion
        // point.  When anchor < cursor (selection before the cursor) the
        // selected text replaces on the next keystroke, so the effective
        // pre-cursor text is text[..anchor] not text[..cursor].
        let effective_boundary = cursor_boundary.min(anchor_boundary);
        self.text.clear();
        self.text.push_str(&text[..effective_boundary]);

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
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn text_mut(&mut self) -> &mut String {
        &mut self.text
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
    }

    #[test]
    fn observe_surrounding_handles_multibyte_cursor() {
        // cursor in bytes — "làn" = 4 bytes (l=1, à=2, n=1)
        let mut buf = ShadowBuffer::new();
        buf.observe_surrounding("lành", 4, 4);
        assert_eq!(buf.text(), "làn");
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
