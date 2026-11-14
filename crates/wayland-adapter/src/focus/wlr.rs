//! WLR toplevel driver. Publish only complete `Done` batches.

use std::collections::HashMap;
use wayland_client::{backend::ObjectId, event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::{WaylandToplevel, Window};
use crate::{AdapterHandler, WaylandAdapter};

#[derive(Default)]
struct PendingWindow {
    app_id: Option<String>,
    title: Option<String>,
    activated: Option<bool>,
}

impl PendingWindow {
    fn commit(&mut self, window: &mut Window) {
        if let Some(app_id) = self.app_id.take() { window.app_id = Some(app_id); }
        if let Some(title) = self.title.take() { window.title = Some(title); }
        if let Some(activated) = self.activated.take() { window.activated = activated; }
    }
}

pub(super) struct WlrState {
    manager: ZwlrForeignToplevelManagerV1,
    pending: HashMap<ObjectId, (ZwlrForeignToplevelHandleV1, PendingWindow)>,
}

impl WlrState {
    pub fn new(manager: ZwlrForeignToplevelManagerV1) -> Self {
        Self { manager, pending: HashMap::new() }
    }
}

impl Drop for WlrState {
    fn drop(&mut self) {
        for (handle, _) in self.pending.values() {
            handle.destroy();
        }
        // The manager has no destroy request; stop lets the compositor finish it.
        self.manager.stop();
    }
}

fn activated(bytes: &[u8]) -> bool {
    // Wayland arrays carry native-endian u32 values; Activated is enum value 2.
    bytes.as_chunks::<4>().0.iter().any(|b| u32::from_ne_bytes(*b) == 2)
}

impl<H: AdapterHandler> Dispatch<ZwlrForeignToplevelManagerV1, ()> for WaylandAdapter<H> {
    fn event(state: &mut Self, _: &ZwlrForeignToplevelManagerV1, event: zwlr_foreign_toplevel_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(focus) = &mut state.state.focus else { return };
        #[allow(irrefutable_let_patterns)]
        let WaylandToplevel::Wlr(driver) = &mut focus.wayland else { return };
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                driver.pending.insert(toplevel.id(), (toplevel, PendingWindow::default()));
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                // Clear the shared snapshot and notify before closing the feed.
                focus.finish();
                state.state.focus = None;
            }
            _ => {}
        }
    }

    event_created_child!(WaylandAdapter<H>, ZwlrForeignToplevelManagerV1, [
        0 => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl<H: AdapterHandler> Dispatch<ZwlrForeignToplevelHandleV1, ()> for WaylandAdapter<H> {
    fn event(state: &mut Self, handle: &ZwlrForeignToplevelHandleV1, event: zwlr_foreign_toplevel_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(focus) = &mut state.state.focus else { return };
        #[allow(irrefutable_let_patterns)]
        let WaylandToplevel::Wlr(driver) = &mut focus.wayland else { return };
        let id = handle.id();
        let Some((_, pending)) = driver.pending.get_mut(&id) else { return };
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => pending.app_id = Some(app_id),
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => pending.title = Some(title),
            zwlr_foreign_toplevel_handle_v1::Event::State { state: bytes } => pending.activated = Some(activated(&bytes)),
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                let mut window = focus.state.windows.get(&id).cloned().unwrap_or_default();
                pending.commit(&mut window);
                focus.state.set(id, window);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                driver.pending.remove(&id);
                focus.state.remove(&id);
                handle.destroy();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::FocusState;

    #[test]
    fn pending_fields_stay_invisible_until_done_and_unset_fields_stay_sticky() {
        let (mut state, mut feed) = FocusState::new(None, "test");
        let mut window = Window::default();
        let mut pending = PendingWindow { app_id: Some("editor".into()), title: Some("first".into()), activated: Some(true) };
        assert!(feed.rx.try_recv().is_err());
        pending.commit(&mut window);
        state.set(1, window.clone());
        assert_eq!(feed.rx.try_recv().unwrap().app_id.as_deref(), Some("editor"));
        pending.app_id = Some("browser".into());
        assert!(feed.rx.try_recv().is_err());
        assert_eq!(window.app_id.as_deref(), Some("editor"));
        pending.commit(&mut window);
        state.set(1, window.clone());
        assert_eq!(feed.rx.try_recv().unwrap().app_id.as_deref(), Some("browser"));
        assert_eq!(window.title.as_deref(), Some("first"));
        assert!(window.activated);
        pending.activated = Some(false);
        pending.commit(&mut window);
        state.set(1, window);
        assert_eq!(feed.rx.try_recv().unwrap(), crate::focus::FocusEvent::default());
    }

    #[test]
    fn state_array_contains_activated_not_a_bitmask() {
        let bytes: Vec<_> = [1u32, 3, 2].into_iter().flat_map(u32::to_ne_bytes).collect();
        assert!(activated(&bytes));
        assert!(!activated(&3u32.to_ne_bytes())); // maximized/minimized are not Active
        assert!(!activated(&[]));
    }
}
