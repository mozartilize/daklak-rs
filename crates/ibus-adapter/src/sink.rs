//! Buffered OutputSink for IBus.
//!
//! IBus signal emission is async, but OutputSink is sync. Strategy:
//! IbusSink buffers all operations synchronously; after strategy.apply()
//! returns the caller drains the buffers and emits signals in order BEFORE
//! returning the ProcessKeyEvent bool. D-Bus message ordering on a single
//! connection guarantees signals arrive before the method reply.

use viet_ime_edit_strategy::{KeyState, OutputSink};

use crate::keyval::{evdev_to_keyval, XK_BACKSPACE, IBUS_FORWARD_MASK, IBUS_RELEASE_MASK};

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
    /// Use char counts for delete_surrounding_text (always true for IBus
    /// since the IBus protocol measures in Unicode scalars, not bytes).
    pub chars_for_delete: bool,
}

impl IbusSink {
    pub fn new(chars_for_delete: bool) -> Self {
        Self {
            chars_for_delete,
            ..Default::default()
        }
    }
}

impl OutputSink for IbusSink {
    fn delete_surrounding_text(
        &mut self,
        before_bytes: u32,
        before_chars: u32,
        after_bytes: u32,
        after_chars: u32,
    ) {
        // IBus DeleteSurroundingText uses Unicode char counts.
        // before_chars / after_chars are correct regardless of chars_for_delete flag —
        // chars_for_delete only affects the wayland v1 sink (firefox byte/char issue).
        let _ = (before_bytes, after_bytes);
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
        self.forwards.push(PendingForward { keyval, keycode: key_code, state: s });
    }

    fn vk_modifiers(&mut self, _depressed: u32, _latched: u32, _locked: u32, _group: u32) {
        // IBus engines don't control the compositor keymap; no-op.
    }

    fn uinput_key(&mut self, key_code: u16, value: i32) {
        // UInput path shouldn't fire for GNOME apps (SurroundingText is used),
        // but handle backspace gracefully just in case.
        let kc = key_code as u32;
        let keyval = if kc == 14 { XK_BACKSPACE } else { return };
        let s = IBUS_FORWARD_MASK | if value == 0 { IBUS_RELEASE_MASK } else { 0 };
        self.forwards.push(PendingForward { keyval, keycode: kc, state: s });
    }

    fn vk_commit_char(&mut self, _time: u32, c: char) -> bool {
        self.commits.push(c.to_string());
        true
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
        assert_eq!(sink.forwards[1].state, IBUS_FORWARD_MASK | IBUS_RELEASE_MASK);
    }

    #[test]
    fn uinput_key_uses_evdev_keycode_for_forward_event() {
        let mut sink = IbusSink::default();
        sink.uinput_key(14, 1);
        sink.uinput_key(14, 0);

        assert_eq!(sink.forwards.len(), 2);
        assert_eq!(sink.forwards[0].keycode, 14);
        assert_eq!(sink.forwards[0].keyval, XK_BACKSPACE);
        assert_eq!(sink.forwards[0].state, IBUS_FORWARD_MASK);
        assert_eq!(sink.forwards[1].keycode, 14);
        assert_eq!(sink.forwards[1].keyval, XK_BACKSPACE);
        assert_eq!(sink.forwards[1].state, IBUS_FORWARD_MASK | IBUS_RELEASE_MASK);
    }
}
