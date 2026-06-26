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

#[derive(Debug, Deserialize, Serialize)]
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

    /// Apps whose `app_id` (case-insensitive) forces Tier 3 UInput routing
    /// regardless of `purpose` or other capability signals. Use this for
    /// apps confirmed broken on both Tier 2 ForwardKey and Tier 1
    /// SurroundingText — e.g. chromium drops first-compose vk_key BS AND
    /// drops every delete_surrounding_text. Env override
    /// `DAKLAK_FORCE_UINPUT_APPS` (comma-separated) replaces this list.
    #[serde(default)]
    pub force_uinput_apps: Vec<String>,

    /// Apps that never advertise `zwp_text_input_v3` (Qt5,
    /// XWayland-via-virtual-keyboard, etc.) — daklak synthesizes an
    /// "activate" via Sway IPC focus polling and routes them through
    /// Tier 4 VkOnly: all output via `vk_key` using daklak's
    /// synthesized Vietnamese keymap. Match is case-insensitive on
    /// `app_id`. Env override `DAKLAK_FORCE_VK_ONLY_APPS` replaces this
    /// list.
    #[serde(default)]
    pub force_vk_only_apps: Vec<String>,

    /// When true, the Sway IPC focus poller treats any XWayland-backed
    /// focused window as if it were on `force_vk_only_apps` — bootstrap
    /// a synthetic VkOnly session for it. Useful as a blanket policy
    /// when most XWayland apps benefit from Tier 4 routing (OnlyOffice,
    /// XWayland-bridged Qt5, JetBrains IDEs in X mode, etc.) without
    /// the user having to enumerate every WM_CLASS. `force_uinput_apps`
    /// still wins on conflict — chromium-class XWayland apps remain
    /// routable to Tier 3 via that list to avoid the evdev-200+ render
    /// crash. Env override
    /// `DAKLAK_AUTO_VK_ONLY_XWAYLAND` (any non-empty/non-"0"/non-"false"
    /// value enables).
    #[serde(default)]
    pub auto_vk_only_for_xwayland: bool,

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
            force_uinput_apps: Vec::new(),
            force_vk_only_apps: Vec::new(),
            auto_vk_only_for_xwayland: true,
            enable_evdev_grab: false,
            bracket_shortcuts: false,
            enable_ibus: false,
            modern_style: default_modern_style(),
            config_path: None,
        }
    }
}

impl Config {
    /// Load config from $XDG_CONFIG_HOME/daklak/config.toml, with env
    /// overrides:
    /// - `DAKLAK_METHOD={telex|vni|viqr}` overrides `method`.
    /// - `DAKLAK_FORCE_UINPUT_APPS=app1,app2,...` replaces `force_uinput_apps`
    ///   (empty string clears the list).
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

        if let Ok(apps) = std::env::var("DAKLAK_FORCE_UINPUT_APPS") {
            cfg.force_uinput_apps = parse_app_list(&apps);
        }

        if let Ok(apps) = std::env::var("DAKLAK_FORCE_VK_ONLY_APPS") {
            cfg.force_vk_only_apps = parse_app_list(&apps);
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

        if let Ok(v) = std::env::var("DAKLAK_AUTO_VK_ONLY_XWAYLAND") {
            cfg.auto_vk_only_for_xwayland = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
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

        cfg.force_uinput_apps = canonicalize_app_list(cfg.force_uinput_apps);
        cfg.force_vk_only_apps = canonicalize_app_list(cfg.force_vk_only_apps);

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

/// Parse a comma-separated env-var list into canonical app_id entries.
fn parse_app_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Parse a comma-separated list of logging directives.
fn parse_directive_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Normalize a user-supplied list (TOML): trim, drop empties, lowercase.
fn canonicalize_app_list(list: Vec<String>) -> Vec<String> {
    list.into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
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
    fn parse_app_list_trims_and_lowercases() {
        let r = parse_app_list("  Chromium , com.MITCHELLH.ghostty  ,, KeePassXC");
        assert_eq!(r, vec!["chromium", "com.mitchellh.ghostty", "keepassxc"]);
    }

    #[test]
    fn parse_app_list_empty_input_yields_empty() {
        assert!(parse_app_list("").is_empty());
        assert!(parse_app_list("   ").is_empty());
        assert!(parse_app_list(",,,").is_empty());
    }

    #[test]
    fn canonicalize_app_list_trims_and_lowercases() {
        let r = canonicalize_app_list(vec![
            "  Chromium  ".to_owned(),
            "".to_owned(),
            "ONLYOFFICE".to_owned(),
            "\tkeepassxc ".to_owned(),
        ]);
        assert_eq!(r, vec!["chromium", "onlyoffice", "keepassxc"]);
    }

    #[test]
    fn auto_vk_only_for_xwayland_defaults_enabled() {
        let cfg = Config::default();
        assert!(cfg.auto_vk_only_for_xwayland);
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
