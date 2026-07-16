use viet_ime_edit_strategy::DeleteUnit;

#[derive(Debug)]
pub(crate) struct FirefoxContenteditableQuirk {
    expected_echo: Option<PendingEcho>,
    delete_unit: DeleteUnit,
    use_forward_delete: bool,
    forward_sticky: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEcho {
    expected: String,
    delete_echo: String,
}

impl FirefoxContenteditableQuirk {
    pub(crate) fn new() -> Self {
        Self {
            expected_echo: None,
            delete_unit: DeleteUnit::Bytes,
            use_forward_delete: false,
            forward_sticky: false,
        }
    }

    pub(crate) fn delete_unit(&self) -> DeleteUnit {
        self.delete_unit
    }

    pub(crate) fn use_forward_delete(&self) -> bool {
        self.use_forward_delete
    }

    pub(crate) fn forward_sticky(&self) -> bool {
        self.forward_sticky
    }

    pub(crate) fn arm_forward_sticky(&mut self) {
        self.forward_sticky = true;
    }

    pub(crate) fn reset_forward_sticky(&mut self) {
        self.forward_sticky = false;
    }

    pub(crate) fn reset_delete_unit_after_use(&mut self) {
        if self.delete_unit == DeleteUnit::Chars {
            self.delete_unit = DeleteUnit::Bytes;
        }
    }

    pub(crate) fn record_expected_echo(
        &mut self,
        expected: String,
        delete_echo: String,
    ) {
        self.expected_echo = Some(PendingEcho {
            expected,
            delete_echo,
        });
    }

    pub(crate) fn clear(&mut self) {
        self.expected_echo = None;
        self.delete_unit = DeleteUnit::Bytes;
        self.use_forward_delete = false;
    }

    /// A correction derived from a client-confirmed raw-key frame has its own
    /// surrounding transaction. Do not let its KWin pre-edit duplicate enter
    /// Firefox-specific stale-echo classification; preserve any sticky policy
    /// already learned for this text-input object.
    pub(crate) fn forget_expected_echo(&mut self) {
        self.expected_echo = None;
    }

    /// Classify the surrounding frame against the correction we just issued.
    ///
    /// DESIGN: recognize narrowly, degrade safely. Exactly three frame shapes
    /// are recognized as healthy — the final echo (`expected`), the
    /// intermediate delete-phase echo (`delete_echo`, an end-deletion — see
    /// `prefix_after_char_delete`), and a non-recent mismatch (user moved on;
    /// clear and forget). Every other recent shape — Firefox's stale pre-edit
    /// cache (Bug 1905481), truncations, cursor drift — lands in one "stale"
    /// bucket that arms char-count delete / ForwardKey.
    ///
    /// Widening the healthy set (matching more stale shapes as benign) is the
    /// dangerous direction: misreading a genuine external edit as our own
    /// echo desyncs the shadow and corrupts visible text on the next
    /// correction. Misclassifying a healthy echo as stale merely costs speed
    /// — ForwardKey backspaces always work, just less atomically. So
    /// unrecognized shapes deliberately take the slow-but-correct path
    /// instead of getting their own patterns.
    pub(crate) fn observe_surrounding(
        &mut self,
        before_cursor: &str,
        recent_action: bool,
        retroactive_context: bool,
    ) {
        let Some(expected_echo) = self.expected_echo.as_ref() else {
            return;
        };

        if before_cursor == expected_echo.expected {
            self.reset_delete_unit();
        } else if before_cursor != expected_echo.delete_echo && recent_action {
            let retroactive_cursor_left_word = retroactive_context
                && before_cursor
                    .chars()
                    .last()
                    .map(is_word_boundary)
                    .unwrap_or(false);
            self.delete_unit = if retroactive_cursor_left_word {
                DeleteUnit::Bytes
            } else {
                DeleteUnit::Chars
            };
            self.use_forward_delete = self.delete_unit == DeleteUnit::Chars;
            self.expected_echo = None;
        } else if before_cursor != expected_echo.delete_echo {
            self.expected_echo = None;
            self.delete_unit = DeleteUnit::Bytes;
            self.use_forward_delete = false;
        }
    }

    pub(crate) fn has_pending_echo(&self) -> bool {
        self.expected_echo.is_some()
    }

    fn reset_delete_unit(&mut self) {
        self.expected_echo = None;
        self.delete_unit = DeleteUnit::Bytes;
        self.use_forward_delete = false;
    }
}

fn is_word_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_echo_resets_to_byte_delete() {
        let mut quirk = FirefoxContenteditableQuirk::new();
        quirk.record_expected_echo("tự".to_owned(), "t".to_owned());

        quirk.observe_surrounding("tự", true, false);

        assert_eq!(quirk.delete_unit(), DeleteUnit::Bytes);
        assert!(!quirk.has_pending_echo());
    }

    #[test]
    fn delete_only_echo_keeps_waiting() {
        let mut quirk = FirefoxContenteditableQuirk::new();
        quirk.record_expected_echo("tự".to_owned(), "t".to_owned());

        quirk.observe_surrounding("t", true, false);

        assert_eq!(quirk.delete_unit(), DeleteUnit::Bytes);
        assert!(quirk.has_pending_echo());
    }

    #[test]
    fn stale_recent_echo_arms_char_delete() {
        let mut quirk = FirefoxContenteditableQuirk::new();
        quirk.record_expected_echo("tự".to_owned(), "t".to_owned());

        quirk.observe_surrounding("tư", true, false);

        assert_eq!(quirk.delete_unit(), DeleteUnit::Chars);
        assert!(!quirk.has_pending_echo());
    }

    #[test]
    fn retroactive_cursor_on_boundary_keeps_byte_delete() {
        let mut quirk = FirefoxContenteditableQuirk::new();
        quirk.record_expected_echo("tự".to_owned(), "t".to_owned());

        quirk.observe_surrounding("tư ", true, true);

        assert_eq!(quirk.delete_unit(), DeleteUnit::Bytes);
        assert!(!quirk.has_pending_echo());
    }

    #[test]
    fn non_recent_mismatch_clears_pending_echo() {
        let mut quirk = FirefoxContenteditableQuirk::new();
        quirk.record_expected_echo("tự".to_owned(), "t".to_owned());

        quirk.observe_surrounding("other", false, false);

        assert_eq!(quirk.delete_unit(), DeleteUnit::Bytes);
        assert!(!quirk.has_pending_echo());
    }
}
