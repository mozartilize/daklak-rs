mod config;
mod focused_app;
mod ipc;
mod protocols;
mod sink;
mod wayland;
mod window;

use anyhow::Result;
use tracing_subscriber::{filter::EnvFilter, fmt};

fn main() -> Result<()> {
    // Logging: the binary's crate name is `viet_ime` (from [[bin]] name = "viet-ime"),
    // NOT viet_ime_daemon (which is the package name — different thing).
    // Override at runtime: RUST_LOG=viet_ime=trace or RUST_LOG=debug
    fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::new("viet_ime=debug")),
        )
        .init();

    let config = config::Config::load()?;
    tracing::info!("input method: {:?}", config.method);

    let (conn, event_queue, app) = wayland::connect(config)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(wayland::run_event_loop(conn, event_queue, app))
}
