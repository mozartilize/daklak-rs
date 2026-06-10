//! IBus transport glue: `IbusHandler` impl. Thin delegations to the `Daemon`
//! routing surface + `Composer` brain. Only compiled with the `ibus` feature.

use viet_ime_wayland_adapter::KeyDecision;

use crate::handler::Daemon;

impl viet_ime_ibus_adapter::IbusHandler for Daemon {
    fn process_key(&mut self, evdev: u32, ch: Option<char>) -> KeyDecision {
        Daemon::process_key(self, evdev, ch)
    }
    fn apply_with_sink(
        &mut self,
        backspaces: usize,
        commit: &str,
        time: u32,
        sink: &mut viet_ime_ibus_adapter::sink::IbusSink,
    ) {
        Daemon::apply_with_sink(self, backspaces, commit, time, sink);
    }
    fn observe_surrounding(&mut self, text: &str, cursor: u32, anchor: u32) {
        Daemon::observe_surrounding(self, text, cursor, anchor);
    }
    fn set_modifiers(&mut self, m: viet_ime_edit_strategy::ModifierState) {
        self.modifiers = m;
        if let Some(w) = self.composer.as_mut() {
            w.set_modifiers(m);
        }
    }
    fn activate_ibus(
        &mut self,
        method: viet_ime_edit_strategy::BackspaceMethod,
        chars_for_delete: bool,
    ) {
        Daemon::activate_ibus(self, method, chars_for_delete);
    }
    fn deactivate_ibus(&mut self) {
        Daemon::deactivate_ibus(self);
    }
    fn update_method(&mut self, method: viet_ime_edit_strategy::BackspaceMethod) {
        Daemon::update_ibus_method(self, method);
    }
    fn full_reset(&mut self) {
        if let Some(w) = self.composer.as_mut() {
            w.full_reset();
        }
    }
}
