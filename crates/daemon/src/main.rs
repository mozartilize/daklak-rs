mod config;
mod handler;
mod ipc;
mod main_loop;
mod window;

use anyhow::Result;
use tracing_subscriber::{filter::EnvFilter, fmt};

#[cfg(feature = "wayland")]
use viet_ime_wayland_adapter::connect;

fn print_help() {
    println!("Usage: daklak [SUBCOMMAND]");
    println!();
    println!("Subcommands:");
    println!(
        "  gen-keymap   Print daklak's synthetic xkb keymap to stdout and exit.\n\
         \x20              Pipe to a file then load it into your compositor manually:\n\
         \x20                daklak gen-keymap > /tmp/daklak.xkb\n\
         \x20                swaymsg input <viet-ime-id> xkb_file /tmp/daklak.xkb"
    );
    println!();
    println!("With no subcommand, runs the input-method daemon.");
}

fn main() -> Result<()> {
    // CLI subcommands — parsed before logging init so output stays clean.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "gen-keymap" => {
                print!("{}", viet_ime_keymap::keymap_text());
                return Ok(());
            }
            "--help" | "-h" | "help" => {
                print_help();
                return Ok(());
            }
            other => {
                eprintln!("daklak: unknown subcommand {other:?}\n");
                print_help();
                std::process::exit(2);
            }
        }
    }

    fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::new("viet_ime=debug")),
        )
        .init();

    let config = config::Config::load()?;
    tracing::info!("input method: {:?}", config.method);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async move {
        #[cfg(feature = "wayland")]
        if config.enable_wayland {
            let daemon = handler::Daemon::new(config);
            let mut wayland = connect(daemon)?;

            // IPC server — spawned independently
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

            crate::main_loop::core_loop_with_wayland(&mut wayland).await
        } else {
            let mut daemon = handler::Daemon::new(config);
            daemon.activate_evdev();
            let mut evdev = viet_ime_evdev_adapter::EvdevAdapter::open()?;
            evdev.run(&mut daemon).await
        }
    })
}
