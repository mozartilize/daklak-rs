mod config;
mod handler;
mod ipc;
mod window;

use anyhow::Result;
use tracing_subscriber::{filter::EnvFilter, fmt};

fn main() -> Result<()> {
    // Logging: the binary's crate name is `viet_ime` (from [[bin]] name).
    // Override at runtime: RUST_LOG=viet_ime=trace or RUST_LOG=debug
    fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::new("viet_ime=debug")),
        )
        .init();

    let config = config::Config::load()?;
    tracing::info!("input method: {:?}", config.method);

    // Multi-thread runtime so the Tier 3 grab-dance blocking sleeps (wrapped
    // in `tokio::task::block_in_place` inside the adapter) can hand the
    // worker back to the executor for the duration of the sleep. Two
    // workers is plenty — daklak is I/O bound and the second worker only
    // carries traffic during a Tier 3 compose.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let daemon = handler::Daemon::new(config);

        // IPC server (Stage 3 stub) — spawned independently so the adapter
        // event loop stays focused on Wayland + focus poller + ctrl-c.
        if let Some(server) = ipc::IpcServer::bind().await {
            tokio::spawn(async move {
                loop {
                    match server.accept().await {
                        Ok(stream) => {
                            tokio::spawn(ipc::handle_connection(stream));
                        }
                        Err(e) => {
                            tracing::warn!(?e, "IPC accept errored — IPC task exiting");
                            break;
                        }
                    }
                }
            });
        }

        viet_ime_wayland_adapter::run(daemon).await
    })
}
