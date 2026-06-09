use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

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

pub async fn handle_connection(stream: UnixStream, cmd_tx: CmdTx) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }

    let kind = match line.trim().to_ascii_lowercase().as_str() {
        "toggle"  => CmdKind::Toggle,
        "enable"  => CmdKind::Enable,
        "disable" => CmdKind::Disable,
        "status"  => CmdKind::Status,
        "quit"    => CmdKind::Quit,
        other => {
            let _ = writer.write_all(format!("err unknown command: {other}\n").as_bytes()).await;
            return;
        }
    };

    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    if cmd_tx.send(Command { kind, resp: resp_tx }).await.is_err() {
        let _ = writer.write_all(b"err daemon unavailable\n").await;
        return;
    }

    match resp_rx.await {
        Ok(true)  => { let _ = writer.write_all(b"on\n").await; }
        Ok(false) => { let _ = writer.write_all(b"off\n").await; }
        Err(_)    => { let _ = writer.write_all(b"err no reply\n").await; }
    }
}

pub fn socket_path() -> Option<PathBuf> {
    let xrd = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(PathBuf::from(xrd).join("daklak.sock"))
}
