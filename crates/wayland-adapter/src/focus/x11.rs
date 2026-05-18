//! X11 bridge for XWayland detection.
//!
//! When `$DISPLAY` is set, XWayland is running and exposes its toplevels via
//! an X server we can connect to as a plain X client. This module enumerates
//! those toplevels and keeps the set live via `SubstructureNotify` +
//! `PropertyNotify` events on the root window. The wlr focus dispatch (or a
//! composite layer) consults `matches(app_id, title)` per focus change to
//! decide whether the focused Wayland toplevel is XWayland-backed.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, Window,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Default, Clone)]
pub(crate) struct X11Window {
    pub wm_class_instance: Option<String>,
    pub wm_class_class: Option<String>,
    pub net_wm_name: Option<String>,
    pub net_wm_pid: Option<u32>,
}

#[derive(Clone)]
pub struct X11Bridge {
    inner: Arc<RwLock<HashMap<Window, X11Window>>>,
}

impl X11Bridge {
    /// Connect to `$DISPLAY` and spawn a blocking thread that keeps the
    /// toplevel map live. Returns `None` if `$DISPLAY` is not set or the
    /// connection fails.
    pub fn spawn() -> Option<Self> {
        if std::env::var_os("DISPLAY").is_none() {
            tracing::debug!("DISPLAY unset; X11 bridge disabled");
            return None;
        }
        match Self::try_spawn() {
            Ok(b) => {
                tracing::info!("X11 bridge connected");
                Some(b)
            }
            Err(e) => {
                tracing::warn!("X11 bridge connect failed: {e:#}");
                None
            }
        }
    }

    fn try_spawn() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None).context("x11rb connect")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let inner: Arc<RwLock<HashMap<Window, X11Window>>> = Arc::new(RwLock::new(HashMap::new()));

        // Subscribe to substructure + property events on the root window so we
        // see CreateNotify/DestroyNotify for new toplevels and PropertyNotify
        // for property changes on existing ones.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )
        .context("change_window_attributes root")?
        .check()
        .context("subscribe root events")?;

        // Build initial snapshot.
        let tree = conn.query_tree(root)?.reply()?;
        {
            let mut guard = inner.write().unwrap();
            for win in tree.children {
                // Also listen for property changes on each child.
                let _ = conn.change_window_attributes(
                    win,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                );
                if let Some(entry) = fetch_window(&conn, win) {
                    guard.insert(win, entry);
                }
            }
        }
        let _ = conn.flush();

        // Detached OS thread (not tokio's blocking pool): `wait_for_event`
        // blocks indefinitely; tokio runtime shutdown waits on its blocking
        // pool threads, so using `spawn_blocking` here would deadlock Ctrl-C.
        let inner_for_thread = inner.clone();
        std::thread::Builder::new()
            .name("daklak-x11-bridge".into())
            .spawn(move || run_event_loop(conn, root, inner_for_thread))
            .context("spawn X11 bridge thread")?;

        Ok(Self { inner })
    }

    /// True iff any tracked X toplevel has a `WM_CLASS.class` (case-insensitive)
    /// matching `app_id` OR a `_NET_WM_NAME` matching `title`.
    pub fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        let g = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return false,
        };
        for (_, w) in g.iter() {
            if let (Some(needle), Some(cls)) = (app_id, w.wm_class_class.as_deref()) {
                if needle.eq_ignore_ascii_case(cls) {
                    return true;
                }
            }
            if let (Some(needle), Some(cls)) = (app_id, w.wm_class_instance.as_deref()) {
                if needle.eq_ignore_ascii_case(cls) {
                    return true;
                }
            }
            if let (Some(needle), Some(name)) = (title, w.net_wm_name.as_deref()) {
                if needle == name {
                    return true;
                }
            }
        }
        false
    }
}

fn fetch_window(conn: &RustConnection, win: Window) -> Option<X11Window> {
    let mut entry = X11Window::default();
    // WM_CLASS — STRING, "instance\0class\0"
    if let Ok(cookie) = conn.get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
    {
        if let Ok(r) = cookie.reply() {
            if !r.value.is_empty() {
                let parts: Vec<&[u8]> = r.value.split(|&b| b == 0).collect();
                if let Some(instance) = parts.first().and_then(|b| std::str::from_utf8(b).ok()) {
                    if !instance.is_empty() {
                        entry.wm_class_instance = Some(instance.to_owned());
                    }
                }
                if let Some(class) = parts.get(1).and_then(|b| std::str::from_utf8(b).ok()) {
                    if !class.is_empty() {
                        entry.wm_class_class = Some(class.to_owned());
                    }
                }
            }
        }
    }
    // _NET_WM_NAME — UTF8_STRING (interned)
    if let Ok(name) = intern_atom(conn, b"_NET_WM_NAME") {
        if let Ok(cookie) = conn.get_property(false, win, name, AtomEnum::ANY, 0, 1024) {
            if let Ok(r) = cookie.reply() {
                if let Ok(s) = std::str::from_utf8(&r.value) {
                    if !s.is_empty() {
                        entry.net_wm_name = Some(s.to_owned());
                    }
                }
            }
        }
    }
    // _NET_WM_PID — CARDINAL
    if let Ok(pid_atom) = intern_atom(conn, b"_NET_WM_PID") {
        if let Ok(cookie) =
            conn.get_property(false, win, pid_atom, AtomEnum::CARDINAL, 0, 1)
        {
            if let Ok(r) = cookie.reply() {
                if r.value.len() >= 4 {
                    let pid = u32::from_ne_bytes([r.value[0], r.value[1], r.value[2], r.value[3]]);
                    entry.net_wm_pid = Some(pid);
                }
            }
        }
    }
    if entry.wm_class_class.is_none()
        && entry.wm_class_instance.is_none()
        && entry.net_wm_name.is_none()
    {
        None
    } else {
        Some(entry)
    }
}

fn intern_atom(conn: &RustConnection, name: &[u8]) -> Result<u32, x11rb::errors::ReplyError> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn run_event_loop(
    conn: RustConnection,
    root: Window,
    inner: Arc<RwLock<HashMap<Window, X11Window>>>,
) {
    loop {
        let event = match conn.wait_for_event() {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("X11 bridge: connection error: {e}");
                break;
            }
        };
        match event {
            XEvent::CreateNotify(ev) if ev.parent == root => {
                // Listen for property changes on the new window, then fetch
                // initial properties (may be empty if not yet set).
                let _ = conn.change_window_attributes(
                    ev.window,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                );
                if let Some(entry) = fetch_window(&conn, ev.window) {
                    if let Ok(mut g) = inner.write() {
                        g.insert(ev.window, entry);
                    }
                }
            }
            XEvent::DestroyNotify(ev) => {
                if let Ok(mut g) = inner.write() {
                    g.remove(&ev.window);
                }
            }
            XEvent::PropertyNotify(ev) => {
                if let Some(entry) = fetch_window(&conn, ev.window) {
                    if let Ok(mut g) = inner.write() {
                        g.insert(ev.window, entry);
                    }
                }
            }
            _ => {}
        }
    }
}
