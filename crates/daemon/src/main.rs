mod composer;
mod config;
mod control;
mod handler;
mod ipc;
mod main_loop;
mod quirks;
mod transport;
mod tray;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use tracing_subscriber::{filter::EnvFilter, fmt};

use viet_ime_wayland_adapter::connect;

fn print_help() {
    println!("Usage: daklak [SUBCOMMAND]");
    println!();
    println!("Subcommands:");
    println!("  toggle       Toggle the input method on or off.");
    println!("  enable       Turn the input method on.");
    println!("  disable      Turn the input method off.");
    println!("  status       Print 'on' or 'off' and exit.");
    println!(
        "  gen-keymap   Print daklak's synthetic xkb keymap to stdout and exit.\n\
         \x20              Pipe to a file then load it into your compositor manually:\n\
         \x20                daklak gen-keymap > /tmp/daklak.xkb\n\
         \x20                swaymsg input <viet-ime-id> xkb_file /tmp/daklak.xkb"
    );
    println!();
    println!("With no subcommand, runs the input-method daemon.");
    println!();
    println!("Sway keybind example:");
    println!("  bindsym $mod+space exec daklak toggle");
}

/// Connect to the running daemon socket and send a one-line command.
/// Returns the daemon's reply ("on" / "off" / "err ...").
fn ipc_send(cmd: &str) -> Result<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let path = ipc::socket_path().ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR not set"))?;

    let mut stream = UnixStream::connect(&path)
        .map_err(|e| anyhow::anyhow!("cannot connect to daklak socket {}: {e}", path.display()))?;

    writeln!(stream, "{cmd}")?;
    stream.flush()?;

    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply.trim().to_owned())
}

fn main() -> Result<()> {
    // CLI subcommands — parsed before logging init so output stays clean.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "toggle" | "enable" | "disable" | "status" => {
                let reply = ipc_send(&arg)?;
                println!("{reply}");
                return Ok(());
            }
            "gen-keymap" => {
                print!("{}", viet_ime_keymap::keymap_text());
                return Ok(());
            }
            "--help" | "-h" | "help" => {
                print_help();
                return Ok(());
            }
            "--ibus" => {
                // ibus-daemon spawned us via the component <exec> line.
                // Fall through to main with enable_ibus = true.
                // Config::load() will see DAKLAK_ENABLE_IBUS if set; we also
                // force the flag here so no env var is needed.
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

    let mut config = config::Config::load()?;
    // --ibus flag forces ibus mode regardless of config file.
    if std::env::args().any(|a| a == "--ibus") {
        config.enable_ibus = true;
    }
    tracing::info!("input method: {:?}", config.method);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async move {
        // --- shared control plane (works in every mode) ---
        let enabled = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = control::channel();
        let (state_tx, state_rx) = tokio::sync::watch::channel(true);

        control::spawn(cmd_rx, enabled.clone(), state_tx);

        if let Some(server) = ipc::IpcServer::bind().await {
            let tx = cmd_tx.clone();
            tokio::spawn(async move {
                loop {
                    match server.accept().await {
                        Ok(stream) => {
                            let tx = tx.clone();
                            tokio::spawn(ipc::handle_connection(stream, tx));
                        }
                        Err(e) => {
                            tracing::warn!(?e, "IPC accept errored — IPC task exiting");
                            break;
                        }
                    }
                }
            });
        }

        tray::spawn_tray(cmd_tx.clone(), state_rx);

        // --- mode-specific run loop ---

        #[cfg(feature = "ibus")]
        if config.enable_ibus {
            let daemon = handler::Daemon::new(config, enabled.clone());
            return viet_ime_ibus_adapter::IbusAdapter::run(daemon, enabled).await;
        }

        if config.enable_wayland {
            let daemon = handler::Daemon::new(config, enabled.clone());
            let mut wayland = connect(daemon)?;
            return crate::main_loop::core_loop_with_wayland(&mut wayland).await;
        }

        #[cfg(feature = "evdev_grab")]
        if config.enable_evdev_grab {
            let mut daemon = handler::Daemon::new(config, enabled.clone());
            daemon.activate_evdev();
            let mut evdev = viet_ime_evdev_adapter::EvdevAdapter::open()?;
            return evdev.run(&mut daemon).await;
        }

        #[cfg(not(feature = "evdev_grab"))]
        if config.enable_evdev_grab {
            return Err(anyhow::anyhow!(
                "evdev_grab mode requested, but the daemon was built without the evdev_grab feature"
            ));
        }

        anyhow::bail!("no input backend enabled; set enable_wayland=true or enable_evdev_grab=true")
    })
}
