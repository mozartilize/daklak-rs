//! Optional focused-window tracking, independent of the input-method channel.
//! Protocol drivers normalize Wayland toplevels; X11 only enriches their metadata.

use std::{collections::HashMap, hash::Hash};
use async_trait::async_trait;
use tokio::sync::mpsc;
use wayland_client::{backend::ObjectId, globals::GlobalList, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;

pub use viet_ime_focus::{FocusBackend, FocusEvent};
pub use viet_ime_focus::x11;

#[cfg(feature = "kde")]
pub mod kde;
pub mod wlr;

use crate::{AdapterHandler, WaylandAdapter};

pub(crate) struct FocusTracker {
    wayland: WaylandToplevel,
    state: FocusState<ObjectId>,
}

enum WaylandToplevel {
    Wlr(wlr::WlrState),
    #[cfg(feature = "kde")]
    Kde(kde::KdeState),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Window {
    app_id: Option<String>,
    title: Option<String>,
    activated: bool,
}

/// Normalized reducer. Keys are opaque window identities, not application IDs.
struct FocusState<Id> {
    windows: HashMap<Id, Window>,
    active: Option<Id>,
    xwayland: Option<x11::X11Bridge>,
    tx: mpsc::UnboundedSender<FocusEvent>,
    current: Option<FocusEvent>,
}

struct FocusFeed {
    name: &'static str,
    rx: mpsc::UnboundedReceiver<FocusEvent>,
}

impl<Id: Eq + Hash + Clone> FocusState<Id> {
    fn new(xwayland: Option<x11::X11Bridge>, name: &'static str) -> (Self, FocusFeed) {
        let (tx, rx) = mpsc::unbounded_channel();
        let feed = FocusFeed { name, rx };
        (Self { windows: HashMap::new(), active: None, xwayland, tx, current: None }, feed)
    }

    fn set(&mut self, id: Id, window: Window) {
        let old_active = self.active.clone();
        let was_active = self.windows.get(&id).is_some_and(|w| w.activated);
        if window.activated && !was_active {
            self.active = Some(id.clone());
        } else if !window.activated && self.active.as_ref() == Some(&id) {
            self.active = None;
        }
        self.windows.insert(id, window);
        self.publish(self.active != old_active);
    }

    fn remove(&mut self, id: &Id) {
        self.windows.remove(id);
        let active_changed = self.active.as_ref() == Some(id);
        if active_changed {
            self.active = None;
        }
        self.publish(active_changed);
    }

    fn publish(&mut self, active_changed: bool) {
        let event = match self.active.as_ref().and_then(|id| self.windows.get(id)) {
            Some(w) => {
                let app_id = w.app_id.as_ref().filter(|id| !id.trim().is_empty()).cloned();
                let is_xwayland = self.xwayland.as_ref()
                    .is_some_and(|bridge| bridge.matches(app_id.as_deref(), w.title.as_deref()));
                FocusEvent { app_id, is_xwayland }
            }
            None => FocusEvent::default(),
        };
        if !active_changed && self.current.as_ref() == Some(&event) {
            return;
        }
        self.current = Some(event.clone());
        tracing::trace!(?event, "focused window changed");
        if self.tx.send(event).is_err() {
            tracing::debug!("focus observer dropped");
        }
    }
}

impl FocusTracker {
    pub fn probe<H: AdapterHandler>(globals: &GlobalList, qh: &QueueHandle<WaylandAdapter<H>>) -> (Option<Self>, Option<Box<dyn FocusBackend>>) {
        let wayland = match globals.bind::<ZwlrForeignToplevelManagerV1, _, _>(qh, 1..=3, ()) {
            Ok(manager) => Some(WaylandToplevel::Wlr(wlr::WlrState::new(manager))),
            Err(error) => {
                tracing::debug!(%error, "wlr-foreign-toplevel unavailable");
                None
            }
        };
        #[cfg(feature = "kde")]
        let wayland = wayland.or_else(|| {
            use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window_management::OrgKdePlasmaWindowManagement;
            match globals.bind::<OrgKdePlasmaWindowManagement, _, _>(qh, 1..=18, ()) {
                Ok(manager) => Some(WaylandToplevel::Kde(kde::KdeState::new(manager))),
                Err(error) => {
                    tracing::debug!(%error, "Plasma window management unavailable");
                    None
                }
            }
        });
        let Some(wayland) = wayland else {
            tracing::warn!("no toplevel source; focus-driven composition disabled");
            return (None, None);
        };
        let name = match &wayland {
            WaylandToplevel::Wlr(_) => "wlr-foreign-toplevel",
            #[cfg(feature = "kde")]
            WaylandToplevel::Kde(_) => "kde-plasma-window-management",
        };
        let (state, feed) = FocusState::new(x11::X11Bridge::spawn(), name);
        tracing::info!(name, "focus backend selected");
        (Some(Self { wayland, state }), Some(Box::new(feed)))
    }

    pub fn current(&self) -> Option<FocusEvent> {
        self.state.current.clone()
    }

    pub fn finish(&mut self) {
        self.state.windows.clear();
        self.state.active = None;
        self.state.publish(true);
    }
}

#[async_trait]
impl FocusBackend for FocusFeed {
    async fn next_event(&mut self) -> Option<FocusEvent> { self.rx.recv().await }
    fn name(&self) -> &'static str { self.name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(app: &str, activated: bool) -> Window {
        Window { app_id: Some(app.into()), activated, ..Default::default() }
    }

    #[test]
    fn new_activation_wins_before_old_deactivation_and_background_updates() {
        let (mut state, mut feed) = FocusState::new(None, "test");
        state.set(11, window("editor", true));
        assert_eq!(feed.rx.try_recv().unwrap().app_id.as_deref(), Some("editor"));
        state.set(22, window("browser", true));
        assert_eq!(feed.rx.try_recv().unwrap().app_id.as_deref(), Some("browser"));
        // Old window still says activated until its own event arrives. A
        // metadata update must not choose it via HashMap iteration order.
        state.set(11, window("old-editor-renamed", true));
        assert!(feed.rx.try_recv().is_err());
        state.set(11, window("old-editor-renamed", false));
        state.remove(&11);
        assert_eq!(state.active, Some(22));
        assert!(feed.rx.try_recv().is_err());
        state.remove(&22);
        assert_eq!(feed.rx.try_recv().unwrap(), FocusEvent::default());
        assert_eq!(state.current, Some(FocusEvent::default()));
    }

    #[test]
    fn late_app_id_is_published_without_a_second_activation() {
        let (mut state, mut feed) = FocusState::new(None, "test");
        state.set(7, window("   ", true));
        assert_eq!(feed.rx.try_recv().unwrap(), FocusEvent::default());
        state.set(7, window("org.example.Editor", true));
        let expected = FocusEvent { app_id: Some("org.example.Editor".into()), is_xwayland: false };
        assert_eq!(feed.rx.try_recv().unwrap(), expected);
        assert_eq!(state.current, Some(expected));
        state.set(7, window("org.example.Editor", true));
        assert!(feed.rx.try_recv().is_err());
    }

    #[test]
    fn distinct_windows_with_the_same_app_still_emit_focus_change() {
        let (mut state, mut feed) = FocusState::new(None, "test");
        state.set(1, window("editor", true));
        feed.rx.try_recv().unwrap();
        state.set(2, window("editor", true));
        assert_eq!(state.active, Some(2));
        assert_eq!(feed.rx.try_recv().unwrap().app_id.as_deref(), Some("editor"));
    }
}
