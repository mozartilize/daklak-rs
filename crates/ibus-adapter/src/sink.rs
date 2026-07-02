//! Buffered OutputSink for IBus.
//!
//! IBus signal emission is async, but OutputSink is sync. Strategy:
//! IbusSink buffers all operations synchronously; after strategy.apply()
//! returns the caller drains the buffers and emits signals in order BEFORE
//! returning the ProcessKeyEvent bool. D-Bus message ordering on a single
//! connection guarantees signals arrive before the method reply.

use viet_ime_edit_strategy::{KeyState, OutputSink};

use crate::keyval::{evdev_to_keyval, IBUS_FORWARD_MASK, IBUS_RELEASE_MASK, XK_BACKSPACE};

/// A pending delete_surrounding_text operation.
/// offset: chars before cursor to delete (negative = before).
/// n_chars: total chars to delete.
#[derive(Debug)]
pub struct PendingDelete {
    pub offset: i32,
    pub n_chars: u32,
}

/// A pending ForwardKeyEvent.
#[derive(Debug)]
pub struct PendingForward {
    pub keyval: u32,
    pub keycode: u32,
    pub state: u32,
}

/// Buffered sink — collects all output during strategy.apply() for async emission.
#[derive(Debug, Default)]
pub struct IbusSink {
    pub commits: Vec<String>,
    pub deletes: Vec<PendingDelete>,
    pub forwards: Vec<PendingForward>,
}

impl IbusSink {
    pub fn new() -> Self {
        Self::default()
    }
}

impl OutputSink for IbusSink {
    fn delete_surrounding_text(
        &mut self,
        _before_bytes: u32,
        before_chars: u32,
        _after_bytes: u32,
        after_chars: u32,
    ) {
        // IBus DeleteSurroundingText uses Unicode char counts.
        let offset = -(before_chars as i32);
        let n_chars = before_chars + after_chars;
        self.deletes.push(PendingDelete { offset, n_chars });
    }

    fn commit_string(&mut self, text: &str) {
        if !text.is_empty() {
            self.commits.push(text.to_owned());
        }
    }

    fn commit(&mut self, _serial: u32) {
        // No-op: IBus doesn't have a serial/commit handshake.
    }

    fn vk_key(&mut self, _time: u32, key_code: u32, state: KeyState) {
        // Only backspace is emitted via vk_key in the ForwardKey path.
        let keyval = if key_code == 14 {
            XK_BACKSPACE
        } else {
            evdev_to_keyval(key_code)
        };
        if keyval == 0 {
            return;
        }
        // IBUS_FORWARD_MASK keeps the client from round-tripping our synthetic
        // BackSpace back into ProcessKeyEvent (Firefox queue-overflow freeze).
        let s = IBUS_FORWARD_MASK
            | match state {
                KeyState::Pressed => 0u32,
                KeyState::Released => IBUS_RELEASE_MASK,
            };
        // ForwardKeyEvent keycode follows ProcessKeyEvent input semantics: evdev.
        // IBus frontends that need X11 keycode do the +8 conversion themselves.
        self.forwards.push(PendingForward {
            keyval,
            keycode: key_code,
            state: s,
        });
    }

    fn vk_modifiers(&mut self, _depressed: u32, _latched: u32, _locked: u32, _group: u32) {
        // IBus engines don't control the compositor keymap; no-op.
    }

    fn vk_commit_char(&mut self, _time: u32, _c: char) -> bool {
        false
    }

    fn commit_key_channel_text(&mut self, _serial: u32, _time: u32, _text: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_key_uses_evdev_keycode_for_forward_event() {
        let mut sink = IbusSink::default();
        sink.vk_key(0, 14, KeyState::Pressed);
        sink.vk_key(0, 14, KeyState::Released);

        assert_eq!(sink.forwards.len(), 2);
        assert_eq!(sink.forwards[0].keycode, 14);
        assert_eq!(sink.forwards[0].keyval, XK_BACKSPACE);
        // Press carries IBUS_FORWARD_MASK so the client won't re-inject it.
        assert_eq!(sink.forwards[0].state, IBUS_FORWARD_MASK);
        assert_eq!(sink.forwards[1].keycode, 14);
        assert_eq!(sink.forwards[1].keyval, XK_BACKSPACE);
        assert_eq!(
            sink.forwards[1].state,
            IBUS_FORWARD_MASK | IBUS_RELEASE_MASK
        );
    }

    #[test]
    fn ibus_does_not_claim_key_channel_commit_support() {
        let mut sink = IbusSink::default();

        assert!(!sink.vk_commit_char(0, 'ậ'));
        assert!(!sink.commit_key_channel_text(0, 0, "ập"));
        assert!(sink.commits.is_empty());
    }

    #[test]
    fn ibus_forward_key_replacement_is_one_whole_commit_text() {
        let mut sink = IbusSink::default();
        let mut strategy = viet_ime_edit_strategy::Strategy::new(
            viet_ime_edit_strategy::BackspaceMethod::ForwardKey,
        );

        strategy.apply(
            2,
            "ập",
            0,
            0,
            &mut sink,
            viet_ime_edit_strategy::DeleteUnit::Bytes,
        );

        assert_eq!(sink.forwards.len(), 4);
        assert_eq!(sink.commits, vec!["ập".to_owned()]);
    }
}
