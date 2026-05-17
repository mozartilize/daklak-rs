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

    // Multi-thread runtime so the Tier 3 grab-dance blocking sleeps (wrapped in
    // `tokio::task::block_in_place` at their call sites) can hand the worker
    // back to the executor for the duration of the sleep — other tokio tasks
    // (focus poller spawn_blocking returns, IPC accepts, signal handler)
    // keep progressing instead of being starved. Two workers is plenty —
    // daklak is I/O bound and the second worker only carries traffic during
    // a Tier 3 compose. block_in_place requires multi-thread runtime to
    // function (it errors on current_thread).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(wayland::run_event_loop(conn, event_queue, app))
}
