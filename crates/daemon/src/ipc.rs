use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::backend::BackendTarget;
use crate::control::{CmdKind, CmdTx, Command};

pub struct IpcServer {
    listener: UnixListener,
    pub path: PathBuf,
}

impl IpcServer {
    pub async fn bind() -> Option<Self> {
        let path = socket_path()?;
        let _ = std::fs::remove_file(&path);
        match UnixListener::bind(&path) {
            Ok(listener) => {
                tracing::info!("IPC socket bound at {}", path.display());
                Some(Self { listener, path })
            }
            Err(e) => {
                tracing::warn!("IPC socket bind failed ({e}) — continuing without IPC");
                None
            }
        }
    }

    pub async fn accept(&self) -> std::io::Result<UnixStream> {
        let (stream, _addr) = self.listener.accept().await?;
        Ok(stream)
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn parse_ipc_command(line: &str) -> Result<CmdKind, String> {
    let trimmed = line.trim();
    let mut parts = trimmed.split_whitespace();
    let Some(cmd) = parts.next() else {
        return Err("empty command".to_owned());
    };
    let cmd = cmd.to_ascii_lowercase();
    match cmd.as_str() {
        "toggle" => Ok(CmdKind::Toggle),
        "enable" => Ok(CmdKind::Enable),
        "disable" => Ok(CmdKind::Disable),
        "status" => Ok(CmdKind::Status),
        "quit" => Ok(CmdKind::Quit),
        "backend" => match parts.next() {
            None => Ok(CmdKind::BackendStatus),
            Some(raw) => {
                let target = BackendTarget::parse(raw).ok_or_else(|| {
                    if matches!(raw, "ibus" | "wayland") {
                        format!("direct {raw} switching is not supported; use native or evdev")
                    } else {
                        format!("unknown backend: {raw}")
                    }
                })?;
                if parts.next().is_some() {
                    return Err("backend takes at most one argument".to_owned());
                }
                Ok(CmdKind::SetBackend(target))
            }
        },
        other => Err(format!("unknown command: {other}")),
    }
}

pub async fn handle_connection(stream: UnixStream, cmd_tx: CmdTx) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }

    let kind = match parse_ipc_command(&line) {
        Ok(kind) => kind,
        Err(e) => {
            let _ = writer.write_all(format!("err {e}\n").as_bytes()).await;
            return;
        }
    };

    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    if cmd_tx.send(Command { kind, resp: resp_tx }).await.is_err() {
        let _ = writer.write_all(b"err daemon unavailable\n").await;
        return;
    }

    match resp_rx.await {
        Ok(reply) => {
            let _ = writer.write_all(format!("{}\n", reply.as_ipc_line()).as_bytes()).await;
        }
        Err(_) => {
            let _ = writer.write_all(b"err no reply\n").await;
        }
    }
}

pub fn socket_path() -> Option<PathBuf> {
    let xrd = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(PathBuf::from(xrd).join("daklak.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendTarget;

    #[test]
    fn parses_existing_commands() {
        assert!(matches!(parse_ipc_command("toggle"), Ok(CmdKind::Toggle)));
        assert!(matches!(parse_ipc_command("enable"), Ok(CmdKind::Enable)));
        assert!(matches!(parse_ipc_command("disable"), Ok(CmdKind::Disable)));
        assert!(matches!(parse_ipc_command("status"), Ok(CmdKind::Status)));
        assert!(matches!(parse_ipc_command("quit"), Ok(CmdKind::Quit)));
    }

    #[test]
    fn parses_backend_commands() {
        assert!(matches!(parse_ipc_command("backend"), Ok(CmdKind::BackendStatus)));
        assert!(matches!(
            parse_ipc_command("backend evdev"),
            Ok(CmdKind::SetBackend(BackendTarget::Evdev))
        ));
        assert!(matches!(
            parse_ipc_command("backend native"),
            Ok(CmdKind::SetBackend(BackendTarget::Native))
        ));
        assert!(matches!(
            parse_ipc_command("backend auto"),
            Ok(CmdKind::SetBackend(BackendTarget::Native))
        ));
    }

    #[test]
    fn rejects_direct_ibus_wayland_backend_switches() {
        assert_eq!(
            parse_ipc_command("backend ibus"),
            Err("direct ibus switching is not supported; use native or evdev".to_owned())
        );
        assert_eq!(
            parse_ipc_command("backend wayland"),
            Err("direct wayland switching is not supported; use native or evdev".to_owned())
        );
    }

    #[test]
    fn rejects_bad_backend_commands() {
        assert_eq!(parse_ipc_command("backend potato"), Err("unknown backend: potato".to_owned()));
        assert_eq!(
            parse_ipc_command("backend evdev extra"),
            Err("backend takes at most one argument".to_owned())
        );
    }
}
