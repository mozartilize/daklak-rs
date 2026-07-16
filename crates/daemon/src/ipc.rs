use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::{JoinError, JoinSet};

use anyhow::Context;

use crate::backend::BackendTarget;
use crate::control::{CmdKind, CmdTx, Command};

pub(crate) const MAX_IPC_LINE_BYTES: usize = 4096;

pub struct IpcServer {
    listener: UnixListener,
    pub path: PathBuf,
}

fn remove_socket(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "remove IPC socket failed");
        }
    }
}

impl IpcServer {
    pub async fn bind() -> Option<Self> {
        let path = socket_path()?;
        remove_socket(&path);
        match UnixListener::bind(&path) {
            Ok(listener) => {
                if let Err(e) = restrict_socket_permissions(&path) {
                    tracing::warn!(path = %path.display(), error = %e, "IPC socket chmod failed");
                    remove_socket(&path);
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
        loop {
            let (stream, _addr) = self.listener.accept().await?;
            #[cfg(target_os = "linux")]
            {
                let peer_result =
                    peer_uid(&stream).and_then(|peer| validate_peer_uid(peer, current_euid()));
                if let Err(error) = peer_result {
                    tracing::warn!(%error, "rejected IPC peer");
                    continue;
                }
            }
            return Ok(stream);
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        remove_socket(&self.path);
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
                        format!("direct {raw} switching is not supported; use native, evdev, or toggle")
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

pub async fn handle_connection(stream: UnixStream, cmd_tx: CmdTx) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let line = match read_ipc_line(&mut reader).await {
        Ok(line) => line,
        Err(error) => {
            writer
                .write_all(format!("err {error}\n").as_bytes())
                .await
                .context("write IPC input error")?;
            return Ok(());
        }
    };

    let kind = match parse_ipc_command(&line) {
        Ok(kind) => kind,
        Err(error) => {
            writer
                .write_all(format!("err {error}\n").as_bytes())
                .await
                .context("write IPC command error")?;
            return Ok(());
        }
    };

    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(Command {
            kind,
            resp: resp_tx,
        })
        .await
        .is_err()
    {
        writer
            .write_all(b"err daemon unavailable\n")
            .await
            .context("write unavailable IPC response")?;
        anyhow::bail!("daemon command channel closed");
    }

    match resp_rx.await {
        Ok(reply) => writer
            .write_all(format!("{}\n", reply.as_ipc_line()).as_bytes())
            .await
            .context("write IPC response")?,
        Err(error) => {
            writer
                .write_all(b"err no reply\n")
                .await
                .context("write missing IPC response")?;
            return Err(error).context("daemon dropped IPC response");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskDisposition {
    Continue,
    Shutdown,
}

fn classify_connection_join(joined: Result<anyhow::Result<()>, JoinError>) -> TaskDisposition {
    match joined {
        Ok(Ok(())) => TaskDisposition::Continue,
        Ok(Err(error)) => {
            tracing::debug!(%error, "IPC connection ended with error");
            TaskDisposition::Continue
        }
        Err(error) if error.is_cancelled() => {
            tracing::debug!(%error, "IPC connection task cancelled");
            TaskDisposition::Shutdown
        }
        Err(error) => {
            tracing::error!(%error, "IPC connection task panicked");
            TaskDisposition::Continue
        }
    }
}

pub async fn serve(server: IpcServer, cmd_tx: CmdTx) -> anyhow::Result<()> {
    const ACCEPT_DELAYS: [std::time::Duration; 2] = [
        std::time::Duration::from_millis(100),
        std::time::Duration::from_millis(500),
    ];
    let mut accept_failures = 0_usize;
    let mut connections = JoinSet::new();
    let result = loop {
        tokio::select! {
            accepted = server.accept() => match accepted {
                Ok(stream) => {
                    accept_failures = 0;
                    connections.spawn(handle_connection(stream, cmd_tx.clone()));
                }
                Err(error) => {
                    if accept_failures == ACCEPT_DELAYS.len() {
                        break Err(error).context(
                            "IPC accept failed three consecutive times",
                        );
                    }
                    let delay = ACCEPT_DELAYS[accept_failures];
                    accept_failures += 1;
                    tracing::warn!(%error, ?delay, "IPC accept failed; retrying");
                    tokio::time::sleep(delay).await;
                }
            },
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(joined) = joined {
                    if classify_connection_join(joined) == TaskDisposition::Shutdown {
                        break Ok(());
                    }
                }
            }
        }
    };

    connections.abort_all();
    while let Some(joined) = connections.join_next().await {
        classify_connection_join(joined);
    }
    result
}

pub fn socket_path() -> Option<PathBuf> {
    let xrd = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(PathBuf::from(xrd).join("daklak.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendTarget;
    use tokio::io::AsyncBufReadExt;

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
        assert!(matches!(
            parse_ipc_command("backend toggle"),
            Ok(CmdKind::SetBackend(BackendTarget::Toggle))
        ));
    }

    #[test]
    fn rejects_direct_ibus_wayland_backend_switches() {
        assert_eq!(
            parse_ipc_command("backend ibus"),
            Err("direct ibus switching is not supported; use native, evdev, or toggle".to_owned())
        );
        assert_eq!(
            parse_ipc_command("backend wayland"),
            Err("direct wayland switching is not supported; use native, evdev, or toggle".to_owned())
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

        assert!(
            reply.starts_with("err command too long"),
            "reply was {reply:?}"
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "overlong command must not dispatch"
        );
        task.await.unwrap().unwrap();
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
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn failed_ipc_connection_does_not_stop_later_connections() {
        let root = std::env::temp_dir().join(format!("daklak-ipc-serve-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("daklak.sock");
        let server = IpcServer {
            listener: UnixListener::bind(&path).unwrap(),
            path: path.clone(),
        };
        let (cmd_tx, mut cmd_rx) = crate::control::channel();
        let server_task = tokio::spawn(serve(server, cmd_tx));

        let mut bad = UnixStream::connect(&path).await.unwrap();
        let mut overlong = vec![b'x'; MAX_IPC_LINE_BYTES + 1];
        overlong.push(b'\n');
        bad.write_all(&overlong).await.unwrap();
        let mut bad_reply = String::new();
        BufReader::new(bad).read_line(&mut bad_reply).await.unwrap();
        assert!(bad_reply.starts_with("err command too long"));

        let mut good = UnixStream::connect(&path).await.unwrap();
        good.write_all(b"status\n").await.unwrap();
        let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
            .await
            .unwrap()
            .expect("valid command dispatched after failed connection");
        assert!(matches!(cmd.kind, CmdKind::Status));
        cmd.resp
            .send(crate::control::ControlReply::Enabled(true))
            .unwrap();
        let mut good_reply = String::new();
        BufReader::new(good)
            .read_line(&mut good_reply)
            .await
            .unwrap();
        assert_eq!(good_reply, "on\n");

        server_task.abort();
        assert!(server_task.await.unwrap_err().is_cancelled());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn connection_join_panic_is_observed_and_listener_continues() {
        let joined = tokio::spawn(async {
            panic!("connection task panic for classifier test");
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        })
        .await;

        assert_eq!(classify_connection_join(joined), TaskDisposition::Continue);
    }
}
