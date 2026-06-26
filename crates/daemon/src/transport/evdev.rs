//! Evdev transport glue: `EvdevHandler` impl. Thin delegations to the
//! `Daemon` routing surface + `Composer` brain.

#[cfg(feature = "evdev_grab")]
use viet_ime_edit_strategy::KeyDecision;

use crate::handler::Daemon;

#[cfg(feature = "evdev_grab")]
impl viet_ime_evdev_adapter::EvdevHandler for Daemon {
    fn handle_char(&mut self, _code: u32, ch: char) -> KeyDecision {
        Daemon::handle_char(self, ch)
    }

    fn handle_backspace(&mut self) -> KeyDecision {
        Daemon::handle_backspace(self)
    }

    fn clear_session(&mut self) {
        self.router.current_active = false;
        self.router.synthetic_active = false;
        self.router.composer = None;
        self.router.focused_app_id = None;
    }

    fn clear_last_input_char(&mut self) {
        if let Some(w) = self.router.composer.as_mut() {
            w.last_input_char = None;
        }
    }

    fn full_reset_window(&mut self) {
        if let Some(w) = self.router.composer.as_mut() {
            w.full_reset();
        }
    }

    fn check_idle_reset_window(&mut self) {
        if let Some(w) = self.router.composer.as_mut() {
            w.check_idle_reset();
        }
    }
}
