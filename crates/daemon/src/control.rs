use tokio::sync::{mpsc, oneshot, watch};

use crate::backend::InputBackend;
use crate::config::MethodConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlReply {
    Enabled(bool),
    Backend(InputBackend),
    Ok(String),
    Error(String),
}

impl ControlReply {
    pub fn as_ipc_line(&self) -> String {
        match self {
            Self::Enabled(true) => "on".to_owned(),
            Self::Enabled(false) => "off".to_owned(),
            Self::Backend(backend) => format!("{backend}"),
            Self::Ok(msg) => format!("ok {msg}"),
            Self::Error(msg) => format!("err {msg}"),
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdKind {
    Toggle,
    Enable,
    Disable,
    Status,
    Quit,
    BackendStatus,
    SetBackend(crate::backend::BackendTarget),
}

#[derive(Debug)]
pub struct Command {
    pub kind: CmdKind,
    pub resp: oneshot::Sender<ControlReply>,
}

pub type CmdTx = mpsc::Sender<Command>;
pub type StateRx = watch::Receiver<bool>;

pub fn channel() -> (CmdTx, mpsc::Receiver<Command>) {
    mpsc::channel(32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::InputBackend;

    #[test]
    fn control_reply_formats_ipc_lines() {
        assert_eq!(ControlReply::Enabled(true).as_ipc_line(), "on");
        assert_eq!(ControlReply::Enabled(false).as_ipc_line(), "off");
        assert_eq!(ControlReply::Backend(InputBackend::Wayland).as_ipc_line(), "wayland");
        assert_eq!(ControlReply::Ok("backend evdev".into()).as_ipc_line(), "ok backend evdev");
        assert_eq!(
            ControlReply::Error("backend ibus unavailable".into()).as_ipc_line(),
            "err backend ibus unavailable"
        );
    }
}
