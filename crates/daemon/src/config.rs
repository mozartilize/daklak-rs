use anyhow::Result;
use serde::Deserialize;
use viet_ime_engine::InputMethod;

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub method: MethodConfig,

    /// When false, skip Wayland IME setup and fall through to the evdev-only
    /// placeholder loop. Must match a compiled-in `wayland` feature.
    #[serde(default = "default_enable_wayland")]
    pub enable_wayland: bool,

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
    /// Tier 4 VkOnly (Path C): all output via `vk_key` using daklak's
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
    /// crash (see [project_path_c_vkonly.md]). Env override
    /// `DAKLAK_AUTO_VK_ONLY_XWAYLAND` (any non-empty/non-"0"/non-"false"
    /// value enables).
    #[serde(default)]
    pub auto_vk_only_for_xwayland: bool,

    /// Apps whose `app_id` (case-insensitive) make daklak emit
    /// `delete_surrounding_text` as a CHARACTER count instead of a byte
    /// count on the V1Kde sink. Firefox's text-input-v3 client (or the
    /// KWin v1↔v3 bridge for it) interprets `before_length` as Unicode
    /// scalar count rather than bytes, so daklak passing bytes
    /// over-deletes for multi-byte vowels (` ơr`→`ở` — leading space
    /// eaten because daklak said "2 bytes" but firefox heard "2 chars").
    /// Other v3 clients (chromium/Qt/GTK) honor the spec and require
    /// bytes — flipping universally breaks them. Override via
    /// `DAKLAK_FORCE_CHARS_DELETE_APPS` env var (comma-separated).
    /// Default: firefox / firefox-dev / navigator. No effect on v2/wlroots.
    #[serde(default = "default_force_chars_delete_apps")]
    pub force_chars_delete_apps: Vec<String>,

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
}

fn default_enable_wayland() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            method: MethodConfig::default(),
            enable_wayland: default_enable_wayland(),
            force_uinput_apps: Vec::new(),
            force_vk_only_apps: Vec::new(),
            auto_vk_only_for_xwayland: true,
            force_chars_delete_apps: default_force_chars_delete_apps(),
            enable_evdev_grab: false,
            bracket_shortcuts: false,
            enable_ibus: false,
        }
    }
}

fn default_force_chars_delete_apps() -> Vec<String> {
    // Firefox's text-input-v3 (or KWin's v1↔v3 bridge for it) interprets
    // delete_surrounding_text `before_length` as chars, not bytes — over-
    // deletes multi-byte vowels (` ơr`→`ở`, `tự`→`ự`). Other v3 clients
    // honor the spec (bytes). Override via DAKLAK_FORCE_CHARS_DELETE_APPS.
    vec![
        "firefox".to_string(),
        "firefox-dev".to_string(),
        // "navigator".to_string(),
    ]
}

impl Config {
    /// Load config from $XDG_CONFIG_HOME/daklak/config.toml, with env
    /// overrides:
    /// - `DAKLAK_METHOD={telex|vni|viqr}` overrides `method`.
    /// - `DAKLAK_FORCE_UINPUT_APPS=app1,app2,...` replaces `force_uinput_apps`
    ///   (empty string clears the list).
    pub fn load() -> Result<Self> {
        let mut cfg = Self::load_file().unwrap_or_default();

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

        if let Ok(apps) = std::env::var("DAKLAK_FORCE_CHARS_DELETE_APPS") {
            cfg.force_chars_delete_apps = parse_app_list(&apps);
        }

        if let Ok(v) = std::env::var("DAKLAK_ENABLE_WAYLAND") {
            cfg.enable_wayland = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
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

        if let Ok(v) = std::env::var("DAKLAK_ENABLE_IBUS") {
            cfg.enable_ibus = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }

        cfg.force_uinput_apps = canonicalize_app_list(cfg.force_uinput_apps);
        cfg.force_vk_only_apps = canonicalize_app_list(cfg.force_vk_only_apps);
        cfg.force_chars_delete_apps = canonicalize_app_list(cfg.force_chars_delete_apps);

        Ok(cfg)
    }

    fn load_file() -> Option<Self> {
        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| std::path::PathBuf::from(h).join(".config"))
            })?;

        let path = config_dir.join("daklak").join("config.toml");
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
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

/// Normalize a user-supplied list (TOML): trim, drop empties, lowercase.
fn canonicalize_app_list(list: Vec<String>) -> Vec<String> {
    list.into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
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

}
