use ksni::TrayMethods;

use crate::control::{CmdKind, CmdTx, Command, StateRx};

pub struct DaklakTray {
    cmd_tx: CmdTx,
    pub enabled: bool,
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
        use ksni::menu::StandardItem;
        let (label, target) = if self.enabled {
            ("Off", CmdKind::Disable)
        } else {
            ("On", CmdKind::Enable)
        };
        vec![
            StandardItem {
                label: label.into(),
                activate: Box::new(move |t: &mut Self| fire(&t.cmd_tx, target)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| fire(&t.cmd_tx, CmdKind::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        tracing::debug!(?reason, "SNI watcher offline — tray icon unavailable");
        false // keep running, just without a visible icon
    }
}

fn fire(tx: &CmdTx, kind: CmdKind) {
    let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
    let _ = tx.try_send(Command { kind, resp: resp_tx });
}

/// Spawn the tray icon best-effort. Logs a warning on D-Bus failure and returns
/// without aborting the daemon — IPC/CLI still work without a session bus.
pub fn spawn_tray(cmd_tx: CmdTx, mut state_rx: StateRx) {
    let enabled = *state_rx.borrow_and_update();
    tokio::spawn(async move {
        let tray = DaklakTray { cmd_tx, enabled };
        let handle = match tray.spawn().await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("app indicator unavailable (no session D-Bus or SNI host?): {e}");
                return;
            }
        };
        // Repaint on every state change so external `daklak toggle` updates the icon/label.
        while state_rx.changed().await.is_ok() {
            let on = *state_rx.borrow_and_update();
            handle.update(|t| t.enabled = on).await;
        }
    });
}
