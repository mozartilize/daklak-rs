use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::backend::BackendTarget;
use crate::control::{CmdKind, CmdTx, Command};

pub(crate) const MAX_IPC_LINE_BYTES: usize = 4096;

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
                if let Err(e) = restrict_socket_permissions(&path) {
                    tracing::warn!(path = %path.display(), error = %e, "IPC socket chmod failed");
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
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
        #[cfg(target_os = "linux")]
        {
            let peer = peer_uid(&stream)?;
            validate_peer_uid(peer, current_euid())?;
        }
        Ok(stream)
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn restrict_socket_permissions(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn current_euid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut cred = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { cred.assume_init() }.uid)
}

#[cfg(target_os = "linux")]
fn validate_peer_uid(peer: u32, expected: u32) -> std::io::Result<()> {
    if peer == expected {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("IPC peer uid {peer} does not match daemon uid {expected}"),
        ))
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

async fn read_ipc_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<String, String> {
    let mut buf = Vec::with_capacity(128);
    let mut byte = [0_u8; 1];

    loop {
        let n = reader
            .read(&mut byte)
            .await
            .map_err(|e| format!("read command: {e}"))?;
        if n == 0 {
            if buf.is_empty() {
                return Err("empty command".to_owned());
            }
            return Err("command missing newline".to_owned());
        }

        buf.push(byte[0]);
        if buf.len() > MAX_IPC_LINE_BYTES {
            return Err(format!("command too long (max {MAX_IPC_LINE_BYTES} bytes)"));
        }
        if byte[0] == b'\n' {
            break;
        }
    }

    String::from_utf8(buf).map_err(|_| "command must be UTF-8".to_owned())
}

pub async fn handle_connection(stream: UnixStream, cmd_tx: CmdTx) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let line = match read_ipc_line(&mut reader).await {
        Ok(line) => line,
        Err(e) => {
            let _ = writer.write_all(format!("err {e}\n").as_bytes()).await;
            return;
        }
    };

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

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_restricts_socket_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "daklak-ipc-mode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &root);

        let server = IpcServer::bind().await.expect("bind IPC server");
        let mode = std::fs::metadata(&server.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "IPC socket must not be group/world accessible");

        drop(server);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn validate_peer_uid_rejects_other_users() {
        let current = current_euid();
        let other = if current == 0 { 1 } else { 0 };
        let err = validate_peer_uid(other, current).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn validate_peer_uid_accepts_current_user() {
        let current = current_euid();
        validate_peer_uid(current, current).unwrap();
    }

    #[tokio::test]
    async fn overlong_ipc_command_returns_error_without_dispatch() {
        let (cmd_tx, mut cmd_rx) = crate::control::channel();
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let task = tokio::spawn(handle_connection(server, cmd_tx));
        let long_command = format!("{}\n", "status".repeat(MAX_IPC_LINE_BYTES));
        let mut client = client;
        client.write_all(long_command.as_bytes()).await.unwrap();

        let mut reader = BufReader::new(client);
        let mut reply = String::new();
        reader.read_line(&mut reply).await.unwrap();

        assert!(reply.starts_with("err command too long"), "reply was {reply:?}");
        assert!(cmd_rx.try_recv().is_err(), "overlong command must not dispatch");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn normal_ipc_command_still_dispatches() {
        let (cmd_tx, mut cmd_rx) = crate::control::channel();
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();

        let task = tokio::spawn(handle_connection(server, cmd_tx));
        client.write_all(b"status\n").await.unwrap();

        let cmd = cmd_rx.recv().await.expect("command dispatched");
        assert!(matches!(cmd.kind, CmdKind::Status));
        let _ = cmd.resp.send(crate::control::ControlReply::Enabled(true));

        let mut reader = BufReader::new(client);
        let mut reply = String::new();
        reader.read_line(&mut reply).await.unwrap();
        assert_eq!(reply, "on\n");
        task.await.unwrap();
    }
}
