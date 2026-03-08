use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub civitai: CivitaiConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CivitaiConfig {
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsConfig {
    pub models_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub update_interval_hours: u64,
    pub max_concurrent_downloads: usize,
    pub socket_path: PathBuf,
    /// Skip model versions marked as EarlyAccess when selecting the latest version.
    #[serde(default = "default_true")]
    pub skip_early_access: bool,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            models_dir: xdg_data_home().join("comfyui").join("models"),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let uid = unsafe { libc::getuid() };
        Self {
            update_interval_hours: 24,
            max_concurrent_downloads: 1,
            skip_early_access: true,
            socket_path: PathBuf::from(format!("/run/user/{}/comfyui-downloader.sock", uid)),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).context("parsing config.toml")
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising config")?;
        std::fs::write(&path, text).with_context(|| format!("writing config {}", path.display()))
    }

    pub fn config_path() -> PathBuf {
        config_path()
    }
}

fn default_true() -> bool {
    true
}

fn config_path() -> PathBuf {
    xdg_config_home()
        .join("comfyui-downloader")
        .join("config.toml")
}

/// Returns `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`.
pub fn xdg_config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

/// Returns `$XDG_DATA_HOME`, falling back to `$HOME/.local/share`.
pub fn xdg_data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}
