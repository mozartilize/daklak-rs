#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::let_underscore_must_use,
    )
)]

mod backend;
mod composer;
mod config;
mod control;
#[cfg(feature = "evdev_grab")]
mod evdev_hooks;
#[cfg(feature = "evdev_grab")]
mod evdev_preflight;
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

fn print_help() {
    println!("Usage: daklak [SUBCOMMAND]");
    println!();
    println!("Subcommands:");
    println!("  toggle       Toggle the input method on or off.");
    println!("  enable       Turn the input method on.");
    println!("  disable      Turn the input method off.");
    println!("  status       Print 'on' or 'off' and exit.");
    println!("  backend [native|auto|evdev|toggle]  Print/switch/toggle active backend.");
    println!(
        "  gen-keymap   Print daklak's synthetic xkb keymap to stdout and exit.\n\
         \x20              Pipe to a file then load it into your compositor manually:\n\
         \x20                daklak gen-keymap > /tmp/daklak.xkb\n\
         \x20                swaymsg input <viet-ime-id> xkb_file /tmp/daklak.xkb\n\
         \x20              Add --symbols to print the installable xkb_symbols fragment
         \x20              (basic + overlay variants), or --rules to print the
         \x20              ~/.config/xkb/rules/evdev overlay that registers the
         \x20              daklak:vn option for KDE/Wayland."
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
    if cfg!(feature = "ibus") {
        println!();
        println!("IBus integration:");
        println!("  --ibus                       Run as an IBus engine (spawned by ibus-daemon).");
        println!();
    }
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
    Backend,
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
    gen_keymap_rules: bool,
    backend_arg: Option<backend::BackendTarget>,
}

fn parse_cli() -> Result<Cli> {
    parse_cli_from(std::env::args().skip(1))
}

fn parse_cli_from<I>(raw_args: I) -> Result<Cli>
where
    I: IntoIterator<Item = String>,
{
    let mut cli = Cli::default();
    let mut args = raw_args.into_iter().peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ibus" => cli.overrides.ibus = true,
            "--help" | "-h" | "help" => cli.command = Some(Command::Help),
            "toggle" => set_command(&mut cli.command, Command::Toggle, &arg)?,
            "enable" => set_command(&mut cli.command, Command::Enable, &arg)?,
            "disable" => set_command(&mut cli.command, Command::Disable, &arg)?,
            "status" => set_command(&mut cli.command, Command::Status, &arg)?,
            "gen-keymap" => set_command(&mut cli.command, Command::GenKeymap, &arg)?,
            "backend" => {
                set_command(&mut cli.command, Command::Backend, &arg)?;
                if let Some(next) = args.peek() {
                    if !next.starts_with('-') {
                        let Some(raw) = args.next() else {
                            return Err(anyhow::anyhow!(
                                "daklak: backend argument disappeared during parsing"
                            ));
                        };
                        cli.backend_arg =
                            Some(backend::BackendTarget::parse(&raw).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "daklak: unknown backend {raw:?}; use native, auto, evdev, or toggle"
                                )
                            })?);
                    }
                }
            }
            "--symbols" => cli.gen_keymap_symbols = true,
            "--rules" => cli.gen_keymap_rules = true,
            "--config" | "-c" => {
                cli.overrides.config_path = Some(PathBuf::from(next_value(&mut args, "--config")?));
            }
            _ if arg.starts_with("--config=") || arg.starts_with("-c=") => {
                cli.overrides.config_path =
                    Some(PathBuf::from(value_after_equals(&arg, "--config")?));
            }
            "--log-level" => cli.overrides.log_level = Some(next_value(&mut args, "--log-level")?),
            _ if arg.starts_with("--log-level=") => {
                cli.overrides.log_level = Some(value_after_equals(&arg, "--log-level")?);
            }
            "--log-path" => cli.overrides.log_path = Some(next_value(&mut args, "--log-path")?),
            _ if arg.starts_with("--log-path=") => {
                cli.overrides.log_path = Some(value_after_equals(&arg, "--log-path")?);
            }
            "--log-module" => cli
                .overrides
                .log_modules
                .push(next_value(&mut args, "--log-module")?),
            _ if arg.starts_with("--log-module=") => {
                cli.overrides
                    .log_modules
                    .push(value_after_equals(&arg, "--log-module")?);
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
        return Err(anyhow::anyhow!(
            "daklak: --symbols is only valid with gen-keymap"
        ));
    }
    if cli.gen_keymap_rules && !matches!(cli.command, Some(Command::GenKeymap)) {
        return Err(anyhow::anyhow!(
            "daklak: --rules is only valid with gen-keymap"
        ));
    }
    if cli.gen_keymap_symbols && cli.gen_keymap_rules {
        return Err(anyhow::anyhow!(
            "daklak: --symbols and --rules are mutually exclusive"
        ));
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

fn next_value(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))
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
        if cfg!(feature = "ibus") {
            config.ibus_requested = true;
        } else {
            tracing::warn!("--ibus was passed but this build lacks the `ibus` feature; ignoring");
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundTask {
    Ipc,
    Tray,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundEvent {
    Exited(BackgroundTask),
    Failed(BackgroundTask),
    Cancelled,
    Panicked,
}

fn classify_background_join(
    joined: Result<(BackgroundTask, anyhow::Result<()>), tokio::task::JoinError>,
) -> BackgroundEvent {
    match joined {
        Ok((task, Ok(()))) => {
            tracing::info!(?task, "optional background subsystem stopped");
            BackgroundEvent::Exited(task)
        }
        Ok((task, Err(error))) => {
            tracing::warn!(?task, %error, "optional background subsystem disabled");
            BackgroundEvent::Failed(task)
        }
        Err(error) if error.is_cancelled() => {
            tracing::debug!(%error, "background task cancelled");
            BackgroundEvent::Cancelled
        }
        Err(error) => {
            tracing::error!(%error, "background task panicked; daemon continuing");
            BackgroundEvent::Panicked
        }
    }
}

async fn run_services(config: config::Config) -> Result<()> {
    let enabled = Arc::new(AtomicBool::new(true));
    let (cmd_tx, cmd_rx) = control::channel();
    let (state_tx, state_rx) = tokio::sync::watch::channel(true);
    let (backend_tx, backend_rx) = tokio::sync::watch::channel(backend::InputBackend::Auto);
    let (config_change_tx, config_change_rx) =
        tokio::sync::watch::channel(control::ConfigChange::default());
    let mut background = tokio::task::JoinSet::new();

    if let Some(server) = ipc::IpcServer::bind().await {
        let tx = cmd_tx.clone();
        background.spawn(async move { (BackgroundTask::Ipc, ipc::serve(server, tx).await) });
    }
    let tray_config = config.clone();
    background.spawn(async move {
        let result =
            tray::run_tray(cmd_tx, state_rx, backend_rx, config_change_tx, tray_config).await;
        (BackgroundTask::Tray, result)
    });

    let supervisor = transport::supervisor::Supervisor::new(
        config,
        enabled,
        state_tx,
        backend_tx,
        config_change_rx,
    );
    let supervisor_run = supervisor.run(cmd_rx);
    tokio::pin!(supervisor_run);
    let result = loop {
        tokio::select! {
            result = &mut supervisor_run => break result,
            joined = background.join_next(), if !background.is_empty() => {
                if let Some(joined) = joined {
                    classify_background_join(joined);
                }
            }
        }
    };

    background.abort_all();
    while let Some(joined) = background.join_next().await {
        classify_background_join(joined);
    }
    result
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
        Some(Command::Backend) => {
            let cmd = match cli.backend_arg {
                Some(target) => format!("backend {target}"),
                None => "backend".to_owned(),
            };
            let reply = ipc_send(&cmd)?;
            println!("{reply}");
            return Ok(());
        }
        Some(Command::GenKeymap) => {
            if cli.gen_keymap_rules {
                print!("{}", viet_ime_keymap::rules_text());
            } else if cli.gen_keymap_symbols {
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

    rt.block_on(run_services(config))
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use crate::backend::BackendTarget;

    fn parse_from(args: &[&str]) -> Result<Cli> {
        parse_cli_from(args.iter().copied().map(str::to_owned))
    }

    #[test]
    fn parses_backend_status_and_switch() {
        let cli = parse_from(&["backend"]).unwrap();
        assert_eq!(cli.command, Some(Command::Backend));
        assert_eq!(cli.backend_arg, None);

        let cli = parse_from(&["backend", "evdev"]).unwrap();
        assert_eq!(cli.command, Some(Command::Backend));
        assert_eq!(cli.backend_arg, Some(BackendTarget::Evdev));

        let cli = parse_from(&["backend", "native"]).unwrap();
        assert_eq!(cli.backend_arg, Some(BackendTarget::Native));

        let cli = parse_from(&["backend", "toggle"]).unwrap();
        assert_eq!(cli.backend_arg, Some(BackendTarget::Toggle));
    }

    #[tokio::test]
    async fn background_task_panic_is_observed_as_nonfatal_failure() {
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async {
            panic!("background task panic for classifier test");
            #[allow(unreachable_code)]
            (BackgroundTask::Tray, Ok(()))
        });
        let joined = tasks.join_next().await.unwrap();

        assert_eq!(classify_background_join(joined), BackgroundEvent::Panicked);
    }
}
