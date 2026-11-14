//! Plasma toplevel driver. Metadata changes are immediate, without WLR's Done.

use std::collections::HashMap;
use wayland_client::{backend::ObjectId, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_plasma::plasma_window_management::client::{
    org_kde_plasma_window::{self, OrgKdePlasmaWindow},
    org_kde_plasma_window_management::{self, OrgKdePlasmaWindowManagement},
};
use super::WaylandToplevel;
use crate::{AdapterHandler, WaylandAdapter};

pub(super) struct KdeState {
    _manager: OrgKdePlasmaWindowManagement,
    handles: HashMap<ObjectId, OrgKdePlasmaWindow>,
}

impl KdeState {
    pub fn new(manager: OrgKdePlasmaWindowManagement) -> Self {
        Self { _manager: manager, handles: HashMap::new() }
    }
}

impl Drop for KdeState {
    fn drop(&mut self) {
        for handle in self.handles.values() {
            handle.destroy();
        }
    }
}

impl<H: AdapterHandler> Dispatch<OrgKdePlasmaWindowManagement, ()> for WaylandAdapter<H> {
    fn event(state: &mut Self, manager: &OrgKdePlasmaWindowManagement, event: org_kde_plasma_window_management::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let Some(focus) = &mut state.state.focus else { return };
        let WaylandToplevel::Kde(driver) = &mut focus.wayland else { return };
        if let org_kde_plasma_window_management::Event::WindowWithUuid { uuid, .. } = event {
            let window = manager.get_window_by_uuid(uuid, qh, ());
            driver.handles.insert(window.id(), window);
        }
    }
}

impl<H: AdapterHandler> Dispatch<OrgKdePlasmaWindow, ()> for WaylandAdapter<H> {
    fn event(state: &mut Self, handle: &OrgKdePlasmaWindow, event: org_kde_plasma_window::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(focus) = &mut state.state.focus else { return };
        let WaylandToplevel::Kde(driver) = &mut focus.wayland else { return };
        let id = handle.id();
        if !driver.handles.contains_key(&id) { return; }
        let mut window = focus.state.windows.get(&id).cloned().unwrap_or_default();
        match event {
            org_kde_plasma_window::Event::AppIdChanged { app_id } => window.app_id = Some(app_id),
            org_kde_plasma_window::Event::TitleChanged { title } => window.title = Some(title),
            org_kde_plasma_window::Event::StateChanged { flags } => window.activated = (flags & 1) != 0,
            org_kde_plasma_window::Event::Unmapped => {
                driver.handles.remove(&id);
                focus.state.remove(&id);
                handle.destroy();
                return;
            }
            _ => return,
        }
        focus.state.set(id, window);
    }
}
