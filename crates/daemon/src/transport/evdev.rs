//! Evdev transport glue: `EvdevHandler` impl. Thin delegations to the
//! `Daemon` routing surface + `Composer` brain.

use viet_ime_wayland_adapter::KeyDecision;

use crate::handler::Daemon;

impl viet_ime_evdev_adapter::EvdevHandler for Daemon {
    fn handle_char(&mut self, code: u32, ch: char) -> KeyDecision {
        Daemon::handle_char(self, code, ch)
    }

    fn handle_backspace(&mut self) -> KeyDecision {
        Daemon::handle_backspace(self)
    }

    fn clear_session(&mut self) {
        self.current_active = false;
        self.synthetic_active = false;
        self.composer = None;
        self.focused_app_id = None;
    }

    fn clear_last_input_char(&mut self) {
        if let Some(w) = self.composer.as_mut() {
            w.last_input_char = None;
        }
    }

    fn full_reset_window(&mut self) {
        if let Some(w) = self.composer.as_mut() {
            w.full_reset();
        }
    }

    fn check_idle_reset_window(&mut self) {
        if let Some(w) = self.composer.as_mut() {
            w.check_idle_reset();
        }
    }
}
