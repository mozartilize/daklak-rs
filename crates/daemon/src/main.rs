mod composer;
mod config;
mod control;
mod handler;
mod ipc;
mod logging;
mod main_loop;
mod quirks;
mod transport;
mod tray;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;

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
         \x20                swaymsg input <viet-ime-id> xkb_file /tmp/daklak.xkb\n\
         \x20              Add --symbols to print the installable xkb_symbols fragment."
    );
    println!();
    println!("With no subcommand, runs the input-method daemon.");
    println!();
    println!("Sway keybind example:");
    println!("  bindsym $mod+space exec daklak toggle");
    println!();
    println!("Config file:");
    println!("  -c, --config <path>       Use an alternate config file.");
    println!("  DAKLAK_CONFIG=<path>      Same as --config (env override).");
    println!("                            Default: $XDG_CONFIG_HOME/daklak/config.toml");
    println!();
    println!("Logging flags:");
    println!("  --log-level <error|info|debug>  Set the base log level (trace aliases debug).");
    println!("  --log-path <path>               Write logs to this path (default /dev/stdout).");
    println!("  --log-module <target=level>      Add a per-target directive; repeatable.");
    println!("  --log-modules <a,b,c>           Comma-separated per-target directives.");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Toggle,
    Enable,
    Disable,
    Status,
    GenKeymap,
    Help,
}

#[derive(Debug, Default)]
struct CliOverrides {
    ibus: bool,
    config_path: Option<PathBuf>,
    log_level: Option<String>,
    log_path: Option<String>,
    log_modules: Vec<String>,
}

#[derive(Debug, Default)]
struct Cli {
    command: Option<Command>,
    overrides: CliOverrides,
    gen_keymap_symbols: bool,
}

fn parse_cli() -> Result<Cli> {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1).peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ibus" => cli.overrides.ibus = true,
            "--help" | "-h" | "help" => cli.command = Some(Command::Help),
            "toggle" => set_command(&mut cli.command, Command::Toggle, &arg)?,
            "enable" => set_command(&mut cli.command, Command::Enable, &arg)?,
            "disable" => set_command(&mut cli.command, Command::Disable, &arg)?,
            "status" => set_command(&mut cli.command, Command::Status, &arg)?,
            "gen-keymap" => set_command(&mut cli.command, Command::GenKeymap, &arg)?,
            "--symbols" => cli.gen_keymap_symbols = true,
            "--config" | "-c" => {
                cli.overrides.config_path = Some(PathBuf::from(next_value(&mut args, "--config")?));
            }
            _ if arg.starts_with("--config=") || arg.starts_with("-c=") => {
                cli.overrides.config_path = Some(PathBuf::from(value_after_equals(&arg, "--config")?));
            }
            "--log-level" => cli.overrides.log_level = Some(next_value(&mut args, "--log-level")?),
            _ if arg.starts_with("--log-level=") => {
                cli.overrides.log_level = Some(value_after_equals(&arg, "--log-level")?);
            }
            "--log-path" => cli.overrides.log_path = Some(next_value(&mut args, "--log-path")?),
            _ if arg.starts_with("--log-path=") => {
                cli.overrides.log_path = Some(value_after_equals(&arg, "--log-path")?);
            }
            "--log-module" => cli.overrides.log_modules.push(next_value(&mut args, "--log-module")?),
            _ if arg.starts_with("--log-module=") => {
                cli.overrides.log_modules.push(value_after_equals(&arg, "--log-module")?);
            }
            "--log-modules" => cli
                .overrides
                .log_modules
                .extend(parse_csv_list(&next_value(&mut args, "--log-modules")?)),
            _ if arg.starts_with("--log-modules=") => {
                cli.overrides
                    .log_modules
                    .extend(parse_csv_list(&value_after_equals(&arg, "--log-modules")?));
            }
            other if other.starts_with('-') => {
                return Err(anyhow::anyhow!("daklak: unknown option {other:?}"));
            }
            other => {
                return Err(anyhow::anyhow!("daklak: unknown subcommand {other:?}"));
            }
        }
    }

    if cli.gen_keymap_symbols && !matches!(cli.command, Some(Command::GenKeymap)) {
        return Err(anyhow::anyhow!("daklak: --symbols is only valid with gen-keymap"));
    }

    Ok(cli)
}

fn set_command(slot: &mut Option<Command>, command: Command, arg: &str) -> Result<()> {
    if let Some(prev) = slot {
        if *prev != command {
            return Err(anyhow::anyhow!("daklak: multiple subcommands provided"));
        }
        return Ok(());
    }

    *slot = Some(command);
    if arg == "help" {
        *slot = Some(Command::Help);
    }
    Ok(())
}

fn next_value(args: &mut std::iter::Peekable<impl Iterator<Item = String>>, flag: &str) -> Result<String> {
    args.next().ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))
}

fn value_after_equals(arg: &str, flag: &str) -> Result<String> {
    arg.split_once('=')
        .map(|(_, value)| value.to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))
}

fn parse_csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn apply_overrides(config: &mut config::Config, overrides: CliOverrides) {
    if let Some(level) = overrides.log_level {
        config.log_level = level;
    }
    if let Some(path) = overrides.log_path {
        config.log_path = path;
    }
    if !overrides.log_modules.is_empty() {
        config.log_modules = overrides.log_modules;
    }
    if overrides.ibus {
        config.enable_ibus = true;
    }
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
    let cli = parse_cli()?;

    match cli.command {
        Some(Command::Help) => {
            print_help();
            return Ok(());
        }
        Some(Command::Toggle) => {
            let reply = ipc_send("toggle")?;
            println!("{reply}");
            return Ok(());
        }
        Some(Command::Enable) => {
            let reply = ipc_send("enable")?;
            println!("{reply}");
            return Ok(());
        }
        Some(Command::Disable) => {
            let reply = ipc_send("disable")?;
            println!("{reply}");
            return Ok(());
        }
        Some(Command::Status) => {
            let reply = ipc_send("status")?;
            println!("{reply}");
            return Ok(());
        }
        Some(Command::GenKeymap) => {
            if cli.gen_keymap_symbols {
                print!("{}", viet_ime_keymap::symbols_text());
            } else {
                print!("{}", viet_ime_keymap::keymap_text());
            }
            return Ok(());
        }
        None => {}
    }

    let config_path = cli
        .overrides
        .config_path
        .clone()
        .or_else(|| std::env::var_os("DAKLAK_CONFIG").map(PathBuf::from));
    let mut config = match config_path {
        Some(path) => config::Config::load_from(Some(path))?,
        None => config::Config::load()?,
    };
    apply_overrides(&mut config, cli.overrides);
    logging::init(&config)?;
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
