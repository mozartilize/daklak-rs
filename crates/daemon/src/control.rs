use std::sync::atomic::{AtomicBool, Ordering::Acquire, Ordering::Release};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use crate::config::MethodConfig;

#[derive(Debug, Clone, Copy)]
pub struct ConfigChange {
    pub method: MethodConfig,
    pub modern_style: bool,
}

impl ConfigChange {
    /// True when nothing has changed from the given baseline.
    pub fn no_change_from(&self, other: &Self) -> bool {
        self.method == other.method && self.modern_style == other.modern_style
    }
}

impl Default for ConfigChange {
    fn default() -> Self {
        Self {
            method: MethodConfig::Telex,
            modern_style: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CmdKind {
    Toggle,
    Enable,
    Disable,
    Status,
    Quit,
}

#[derive(Debug)]
pub struct Command {
    pub kind: CmdKind,
    pub resp: oneshot::Sender<bool>,
}

pub type CmdTx = mpsc::Sender<Command>;
pub type StateRx = watch::Receiver<bool>;

pub fn channel() -> (CmdTx, mpsc::Receiver<Command>) {
    mpsc::channel(32)
}

/// Single writer of `enabled` + state broadcaster. Spawned once, works in every mode.
pub fn spawn(
    mut rx: mpsc::Receiver<Command>,
    enabled: Arc<AtomicBool>,
    state_tx: watch::Sender<bool>,
) {
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            let new = match cmd.kind {
                CmdKind::Toggle => {
                    let v = !enabled.load(Acquire);
                    enabled.store(v, Release);
                    v
                }
                CmdKind::Enable => {
                    enabled.store(true, Release);
                    true
                }
                CmdKind::Disable => {
                    enabled.store(false, Release);
                    false
                }
                CmdKind::Status => enabled.load(Acquire),
                CmdKind::Quit => {
                    let cur = enabled.load(Acquire);
                    let _ = cmd.resp.send(cur);
                    tracing::info!("quit requested via control");
                    std::process::exit(0);
                }
            };
            let _ = state_tx.send(new);
            let _ = cmd.resp.send(new);
        }
    });
}
