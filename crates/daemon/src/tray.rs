//! Tray icon / SNI app indicator with method and modern-style controls.
//!
//! The tray menu has:
//! - An on/off toggle (replaces `daklak toggle` / IPC enable|disable).
//! - A `Mode` submenu with Telex / VNI / ViQR radio items.
//! - A `Legacy tone placement` checkmark item (`òa` vs `oà`).
//! - A `Quit` item.
//!
//! Mode and legacy-tone changes are written back to the config file when
//! daklak was loaded from a file, and forwarded to the daemon at runtime
//! through a `watch::Sender<ConfigChange>`.

use std::path::{Path, PathBuf};

use ksni::TrayMethods;

use tokio::sync::watch;

use crate::backend::{BackendTarget, InputBackend};
use crate::config::{Config, MethodConfig};
use crate::control::{CmdKind, CmdTx, Command, ConfigChange, StateRx};

pub struct DaklakTray {
    cmd_tx: CmdTx,
    config_change_tx: tokio::sync::watch::Sender<ConfigChange>,
    config_path: Option<PathBuf>,
    pub enabled: bool,
    pub method: MethodConfig,
    pub modern_style: bool,
    pub backend: InputBackend,
}

impl ksni::Tray for DaklakTray {
    fn id(&self) -> String {
        "daklak".into()
    }

    fn icon_name(&self) -> String {
        if self.enabled {
            "input-keyboard".into()
        } else {
            "input-keyboard-symbolic".into()
        }
    }

    fn title(&self) -> String {
        format!("daklak — {}", if self.enabled { "on" } else { "off" })
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};

        let (label, target) = if self.enabled {
            ("Off", CmdKind::Disable)
        } else {
            ("On", CmdKind::Enable)
        };

        // Mode radio group index for the current method.
        let selected = match self.method {
            MethodConfig::Telex => 0,
            MethodConfig::Vni => 1,
            MethodConfig::Viqr => 2,
        };

        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem {
                label: label.into(),
                activate: Box::new(move |t: &mut Self| fire(&t.cmd_tx, target)),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Mode".into(),
                submenu: vec![
                    RadioGroup {
                        selected,
                        select: Box::new(|tray: &mut Self, idx: usize| {
                            let method = match idx {
                                0 => MethodConfig::Telex,
                                1 => MethodConfig::Vni,
                                2 => MethodConfig::Viqr,
                                _ => return,
                            };
                            tray.method = method;
                            tray.apply(ConfigChange {
                                method,
                                modern_style: tray.modern_style,
                            });
                        }),
                        options: vec![
                            RadioItem {
                                label: "Telex".into(),
                                ..Default::default()
                            },
                            RadioItem {
                                label: "VNI".into(),
                                ..Default::default()
                            },
                            RadioItem {
                                label: "VIQR".into(),
                                ..Default::default()
                            },
                        ],
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Legacy tone placement (òa vs oà)".into(),
                checked: !self.modern_style,
                activate: Box::new(|tray: &mut Self| {
                    let modern_style = !tray.modern_style;
                    tray.modern_style = modern_style;
                    tray.apply(ConfigChange {
                        method: tray.method,
                        modern_style,
                    });
                }),
                ..Default::default()
            }
            .into(),
        ];
        if cfg!(feature = "evdev_grab") {
            let evdev_active = self.backend == InputBackend::Evdev;
            let (evdev_label, evdev_target) = if evdev_active {
                ("Disable evdev", CmdKind::SetBackend(BackendTarget::Native))
            } else {
                ("Enable evdev", CmdKind::SetBackend(BackendTarget::Evdev))
            };
            items.push(
                CheckmarkItem {
                    label: evdev_label.into(),
                    checked: evdev_active,
                    activate: Box::new(move |t: &mut Self| fire(&t.cmd_tx, evdev_target)),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| fire(&t.cmd_tx, CmdKind::Quit)),
                ..Default::default()
            }
            .into(),
        );
        items
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        tracing::debug!(?reason, "SNI watcher offline — tray icon unavailable");
        false // keep running, just without a visible icon
    }
}

impl DaklakTray {
    /// Send the new config to the daemon and persist to disk.
    fn apply(&mut self, change: ConfigChange) {
        let _ = self.config_change_tx.send(change);
        if let Some(path) = &self.config_path {
            save_config(path, change);
        }
    }
}

fn fire(tx: &CmdTx, kind: CmdKind) {
    let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
    let _ = tx.try_send(Command {
        kind,
        resp: resp_tx,
    });
}

/// Read the config file at `path`, update method + modern_style, and write
/// back. Logs a warning on any I/O or parse failure instead of crashing.
fn save_config(path: &Path, change: ConfigChange) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "tray: failed to read config for save");
            return;
        }
    };
    let mut cfg: Config = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "tray: failed to parse config for save");
            return;
        }
    };
    cfg.method = change.method;
    cfg.modern_style = change.modern_style;
    let out = match toml::to_string_pretty(&cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "tray: failed to serialize config for save");
            return;
        }
    };
    if let Err(e) = std::fs::write(path, &out) {
        tracing::warn!(path = %path.display(), %e, "tray: failed to write config");
    }
    tracing::debug!(path = %path.display(), ?change, "tray saved config");
}

/// Spawn the tray icon best-effort. Logs a warning on D-Bus failure and returns
/// without aborting the daemon — IPC/CLI still work without a session bus.
pub fn spawn_tray(
    cmd_tx: CmdTx,
    mut state_rx: StateRx,
    mut backend_rx: watch::Receiver<InputBackend>,
    config_change_tx: tokio::sync::watch::Sender<ConfigChange>,
    config: &Config,
) {
    let method = config.method;
    let modern_style = config.modern_style;
    let config_path = config.config_path.clone();

    tokio::spawn(async move {
        let tray = DaklakTray {
            cmd_tx,
            config_change_tx,
            config_path,
            enabled: *state_rx.borrow_and_update(),
            method,
            modern_style,
            backend: *backend_rx.borrow(),
        };
        let handle = match tray.spawn().await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("app indicator unavailable (no session D-Bus or SNI host?): {e}");
                return;
            }
        };
        // Repaint on every state or backend change.
        loop {
            tokio::select! {
                biased;
                changed = state_rx.changed() => {
                    if changed.is_err() { break; }
                    let on = *state_rx.borrow_and_update();
                    handle.update(|t| t.enabled = on).await;
                }
                changed = backend_rx.changed() => {
                    if changed.is_err() { break; }
                    let b = *backend_rx.borrow_and_update();
                    handle.update(|t| t.backend = b).await;
                }
            }
        }
    });
}
