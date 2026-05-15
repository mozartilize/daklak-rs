use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

/// IPC server — binds $XDG_RUNTIME_DIR/viet-ime.sock.
/// Stage 3: stub that accepts connections, sends a hello banner, and discards
/// input. GTK/Qt adapters (Stages 6–7) will use this socket for real IPC.
pub struct IpcServer {
    listener: UnixListener,
    pub path: PathBuf,
}

impl IpcServer {
    pub async fn bind() -> Option<Self> {
        let path = socket_path()?;

        // Remove stale socket if it exists
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

pub async fn handle_connection(mut stream: UnixStream) {
    // Stage 3 stub: send banner, drain input
    let _ = stream.write_all(b"viet-ime/0.1\n").await;
    let (mut r, _w) = stream.into_split();
    let _ = tokio::io::copy(&mut r, &mut tokio::io::sink()).await;
}

fn socket_path() -> Option<PathBuf> {
    let xrd = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(PathBuf::from(xrd).join("viet-ime.sock"))
}
