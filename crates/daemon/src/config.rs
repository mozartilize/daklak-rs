use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use viet_ime_engine::InputMethod;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MethodConfig {
    Telex,
    Vni,
    Viqr,
}

impl Default for MethodConfig {
    fn default() -> Self {
        MethodConfig::Telex
    }
}

impl MethodConfig {
    pub fn to_engine(&self) -> InputMethod {
        match self {
            MethodConfig::Telex => InputMethod::Telex,
            MethodConfig::Vni => InputMethod::Vni,
            MethodConfig::Viqr => InputMethod::Viqr,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub method: MethodConfig,

    /// When false, skip Wayland IME setup and fall through to the optional
    /// evdev-grab loop if the daemon was built with `evdev_grab`.
    #[serde(default = "default_enable_wayland")]
    pub enable_wayland: bool,

    /// Minimum log level for all targets unless overridden by `log_modules`.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Log destination. Defaults to `/dev/stdout`.
    #[serde(default = "default_log_path")]
    pub log_path: String,

    /// Per-target logging directives, e.g. `daklak=debug`.
    #[serde(default)]
    pub log_modules: Vec<String>,

    /// Master switch for evdev-grab mode. When true, daklak opens
    /// `/dev/input/event*` devices and takes a kernel-level `EVIOCGRAB`
    /// at startup, acquiring all physical keyboards for exclusive use.
    /// Keys are then routed through the evdev event loop (bypassing the
    /// compositor's grab), composed by the engine, and re-emitted via
    /// uinput as the corresponding Vietnamese output. Env override
    /// `DAKLAK_ENABLE_EVDEV_GRAB` (truthy heuristic).
    #[serde(default)]
    pub enable_evdev_grab: bool,

    /// Telex-only: enable `[`/`]`/`{`/`}` shortcuts for `ơ`/`ư`/`Ơ`/`Ư`.
    /// Default false so terminal bindings like tmux Ctrl+B+[ are preserved.
    /// Env override `DAKLAK_BRACKET_SHORTCUTS` (truthy heuristic).
    #[serde(default)]
    pub bracket_shortcuts: bool,

    /// Enable GNOME / IBus engine mode. Connects to ibus-daemon and registers
    /// as `org.freedesktop.IBus.Daklak`. Requires the `ibus` Cargo feature.
    /// Set automatically when daklak is launched via `--ibus` or by ibus-daemon.
    /// Env override `DAKLAK_ENABLE_IBUS` (truthy heuristic).
    #[serde(default)]
    pub enable_ibus: bool,

    /// Modern-style tone placement on `oa`/`oe`/`uy` diphthongs.
    /// `true` (default) → `oà`, `false` → `òa` (legacy).
    /// Override: `DAKLAK_MODERN_STYLE` env var (truthy heuristic).
    #[serde(default = "default_modern_style")]
    pub modern_style: bool,

    /// Ordered evdev keymap hook names. Each name resolves to
    /// `$XDG_CONFIG_HOME/daklak/hooks/<name>-set` and `<name>-unset`.
    /// Hooks may self-filter by desktop/session and exit 10 when not applicable.
    #[serde(default)]
    pub evdev_grab_hooks: Vec<String>,

    /// Path where this config was loaded from. `None` when using defaults.
    /// Populated by `Config::load()` / `Config::load_from()`. Used by the
    /// tray menu to write method/modern_style changes back to the file.
    #[serde(skip)]
    pub config_path: Option<PathBuf>,
}

fn default_enable_wayland() -> bool {
    true
}

fn default_modern_style() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            method: MethodConfig::default(),
            enable_wayland: default_enable_wayland(),
            log_level: default_log_level(),
            log_path: default_log_path(),
            log_modules: Vec::new(),
            enable_evdev_grab: false,
            bracket_shortcuts: false,
            enable_ibus: false,
            evdev_grab_hooks: Vec::new(),
            modern_style: default_modern_style(),
            config_path: None,
        }
    }
}

impl Config {
    /// Load config from $XDG_CONFIG_HOME/daklak/config.toml, with env
    /// overrides:
    /// - `DAKLAK_METHOD={telex|vni|viqr}` overrides `method`.
    ///
    /// `load_from(Some(path))` loads that file directly and errors loudly on
    /// missing or invalid input.
    pub fn load() -> Result<Self> {
        Self::load_from(None)
    }

    pub fn load_from(path: Option<PathBuf>) -> Result<Self> {
        let mut cfg = match path {
            Some(ref p) => Self::load_file(p)?,
            None => Self::load_default_file().unwrap_or_default(),
        };

        // Populate config_path now so that tray / save() know where to write.
        // load_file / load_default_file set it internally.
        if let Some(p) = path {
            if cfg.config_path.is_none() {
                cfg.config_path = Some(p);
            }
        }

        if let Ok(m) = std::env::var("DAKLAK_METHOD") {
            cfg.method = match m.to_lowercase().as_str() {
                "vni" => MethodConfig::Vni,
                "viqr" => MethodConfig::Viqr,
                _ => MethodConfig::Telex,
            };
        }

        if let Ok(v) = std::env::var("DAKLAK_ENABLE_WAYLAND") {
            cfg.enable_wayland = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }

        if let Ok(v) = std::env::var("DAKLAK_LOG_LEVEL") {
            cfg.log_level = v;
        }

        if let Ok(v) = std::env::var("DAKLAK_LOG_PATH") {
            cfg.log_path = v;
        }

        if let Ok(v) = std::env::var("DAKLAK_LOG_MODULES") {
            cfg.log_modules = parse_directive_list(&v);
        }

        if let Ok(v) = std::env::var("DAKLAK_ENABLE_EVDEV_GRAB") {
            cfg.enable_evdev_grab = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }

        if let Ok(v) = std::env::var("DAKLAK_BRACKET_SHORTCUTS") {
            cfg.bracket_shortcuts = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }

        if let Ok(v) = std::env::var("DAKLAK_MODERN_STYLE") {
            cfg.modern_style = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }

        if let Ok(v) = std::env::var("DAKLAK_ENABLE_IBUS") {
            cfg.enable_ibus = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }

        Ok(cfg)
    }

    fn load_default_file() -> Option<Self> {
        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| std::path::PathBuf::from(h).join(".config"))
            })?;

        let path = config_dir.join("daklak").join("config.toml");
        let text = std::fs::read_to_string(&path).ok()?;
        let mut cfg: Self = toml::from_str(&text).ok()?;
        cfg.config_path = Some(path);
        Some(cfg)
    }

    fn load_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        cfg.config_path = Some(path.to_owned());
        Ok(cfg)
    }
}

/// Parse a comma-separated list of logging directives.
fn parse_directive_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn default_log_level() -> String {
    "error".to_owned()
}

fn default_log_path() -> String {
    "/dev/stdout".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_directive_list_trims_and_preserves_case() {
        let r = parse_directive_list("  daklak=Debug , viet_ime=info  ,,");
        assert_eq!(r, vec!["daklak=Debug", "viet_ime=info"]);
    }

    #[test]
    fn load_from_explicit_path_reads_that_file() {
        let path = std::env::temp_dir().join(format!(
            "daklak-config-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));

        std::fs::write(
            &path,
            r#"method = "vni"
log_level = "info"
log_path = "/tmp/daklak.log"
"#,
        )
        .unwrap();

        let cfg = Config::load_from(Some(path.clone())).unwrap();
        assert_eq!(cfg.method.to_engine(), InputMethod::Vni);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.log_path, "/tmp/daklak.log");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parses_evdev_grab_hooks_array() {
        let cfg: Config = toml::from_str(r#"
            enable_evdev_grab = true
            evdev_grab_hooks = ["sway", "kde", "x11"]
        "#).unwrap();
        assert_eq!(cfg.evdev_grab_hooks, vec!["sway", "kde", "x11"]);
    }

    #[test]
    fn load_from_explicit_path_missing_errors() {
        let path = std::env::temp_dir().join(format!(
            "daklak-missing-config-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));

        let err = Config::load_from(Some(path)).unwrap_err();
        assert!(err.to_string().contains("failed to read config file"));
    }
}
