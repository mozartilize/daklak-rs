//! Evdev transport glue: `EvdevHandler` impl. Thin delegations to the
//! `Daemon` routing surface + `Composer` brain.

#[cfg(feature = "evdev_grab")]
use viet_ime_edit_strategy::KeyDecision;

use crate::handler::Daemon;

#[cfg(feature = "evdev_grab")]
impl viet_ime_evdev_adapter::EvdevHandler for Daemon {
    fn handle_char(&mut self, _code: u32, ch: char) -> KeyDecision {
        // Gate on the enabled flag: while daklak is toggled off, forward the
        // key raw instead of composing. The evdev adapter passes `ForwardRaw`
        // straight to `passthrough`, so the original keystroke reaches the app
        // unmodified. Without this, the grabbed device kept composing even when
        // the IME was "off" (CLI toggle / tray menu had no effect).
        if !self.sync_enabled_edge() {
            return KeyDecision::ForwardRaw;
        }
        Daemon::handle_char(self, ch)
    }

    fn handle_backspace(&mut self) -> KeyDecision {
        if !self.sync_enabled_edge() {
            return KeyDecision::ForwardRaw;
        }
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

#[cfg(all(test, feature = "evdev_grab"))]
mod tests {
    use super::*;
    use crate::composer::Composer;
    use crate::config::Config;
    use crate::handler::noop_config_rx;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use viet_ime_edit_strategy::BackspaceMethod;
    use viet_ime_engine::InputMethod;
    use viet_ime_evdev_adapter::EvdevHandler;

    fn daemon() -> Daemon {
        let mut d = Daemon::new(
            Config::default(),
            Arc::new(AtomicBool::new(true)),
            noop_config_rx(),
        );
        d.router.current_active = true;
        d.router.composer = Some(Composer::new(
            InputMethod::Telex,
            BackspaceMethod::SurroundingText,
            false,
        ));
        d
    }

    // Regression: a grabbed evdev device must stop composing once daklak is
    // toggled off. Before the enabled-gate, the evdev path fed every key into
    // the composer regardless of state, so "off" still produced Vietnamese.
    // Every key (and backspace) must now forward raw while disabled.
    #[test]
    fn disabled_forwards_raw_never_composes() {
        let mut d = daemon();
        d.enabled.store(false, Ordering::Release);
        assert!(matches!(
            EvdevHandler::handle_char(&mut d, 35, 'h'),
            KeyDecision::ForwardRaw
        ));
        assert!(matches!(
            EvdevHandler::handle_char(&mut d, 24, 'o'),
            KeyDecision::ForwardRaw
        ));
        assert!(matches!(
            EvdevHandler::handle_char(&mut d, 17, 'w'),
            KeyDecision::ForwardRaw
        ));
        assert!(matches!(
            EvdevHandler::handle_backspace(&mut d),
            KeyDecision::ForwardRaw
        ));
    }

    // Toggling off mid-word resets the in-flight composition via the on→off
    // edge, so re-enabling starts from a clean word context.
    #[test]
    fn toggling_off_resets_inflight_composition() {
        let mut d = daemon();
        let _ = EvdevHandler::handle_char(&mut d, 35, 'h');
        let _ = EvdevHandler::handle_char(&mut d, 24, 'o');
        assert!(d.router.composer.as_ref().unwrap().last_input_char.is_some());
        d.enabled.store(false, Ordering::Release);
        // First key after disable forwards raw AND fires the reset edge.
        let _ = EvdevHandler::handle_char(&mut d, 49, 'n');
        assert!(
            d.router.composer.as_ref().unwrap().last_input_char.is_none(),
            "on→off edge must clear the in-flight word"
        );
    }
}
