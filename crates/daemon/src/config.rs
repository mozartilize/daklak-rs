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
}

impl Config {
    /// Load config from $XDG_CONFIG_HOME/viet-ime/config.toml, with env
    /// override VIET_IME_METHOD={telex|vni|viqr}.
    pub fn load() -> Result<Self> {
        let mut cfg = Self::load_file().unwrap_or_default();

        if let Ok(m) = std::env::var("VIET_IME_METHOD") {
            cfg.method = match m.to_lowercase().as_str() {
                "vni" => MethodConfig::Vni,
                "viqr" => MethodConfig::Viqr,
                _ => MethodConfig::Telex,
            };
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
