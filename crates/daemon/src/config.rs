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

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub method: MethodConfig,

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
}

impl Config {
    /// Load config from $XDG_CONFIG_HOME/viet-ime/config.toml, with env
    /// overrides:
    /// - `VIET_IME_METHOD={telex|vni|viqr}` overrides `method`.
    /// - `DAKLAK_FORCE_UINPUT_APPS=app1,app2,...` replaces `force_uinput_apps`
    ///   (empty string clears the list).
    pub fn load() -> Result<Self> {
        let mut cfg = Self::load_file().unwrap_or_default();

        if let Ok(m) = std::env::var("VIET_IME_METHOD") {
            cfg.method = match m.to_lowercase().as_str() {
                "vni" => MethodConfig::Vni,
                "viqr" => MethodConfig::Viqr,
                _ => MethodConfig::Telex,
            };
        }

        if let Ok(apps) = std::env::var("DAKLAK_FORCE_UINPUT_APPS") {
            cfg.force_uinput_apps = apps
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
        }

        if let Ok(apps) = std::env::var("DAKLAK_FORCE_VK_ONLY_APPS") {
            cfg.force_vk_only_apps = apps
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
        }

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

        let path = config_dir.join("viet-ime").join("config.toml");
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }
}
