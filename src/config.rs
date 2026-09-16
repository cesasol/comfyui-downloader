use crate::secrets::{ALL_CREDENTIALS, Credential, Store};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub civitai: CivitaiConfig,
    #[serde(default)]
    pub huggingface: HuggingfaceConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub gpu: GpuConfig,
    /// Credentials read from the Secret Service. Never serialised, so they
    /// cannot leak back into `config.toml`.
    #[serde(skip)]
    pub secrets: ResolvedSecrets,
}

/// Credentials resolved for this process, keyed by credential rather than by
/// config section because their storage is the keyring, not the file.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSecrets {
    pub civitai_api_key: Option<String>,
    pub huggingface_token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CivitaiConfig {
    /// Deprecated plaintext API key. Migrated into the keyring on daemon
    /// startup; prefer `comfyui-dl set-key`.
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HuggingfaceConfig {
    /// Deprecated plaintext access token for gated or private repositories.
    /// Migrated into the keyring on daemon startup; public files need none.
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuConfig {
    /// Overrides the detected VRAM capacity (bytes) used to judge whether a
    /// model bundle can run. Set this when detection is wrong or when sizing
    /// downloads for another machine.
    pub vram_bytes: Option<u64>,
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
    /// Enable system tray icon (requires `tray-icon` feature and GUI environment).
    #[serde(default)]
    pub enable_tray_icon: bool,
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
            enable_tray_icon: true,
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

    /// The CivitAI API key for this process: the keyring value when resolved,
    /// otherwise the deprecated plaintext field.
    pub fn civitai_api_key(&self) -> Option<&str> {
        self.secrets
            .civitai_api_key
            .as_deref()
            .or(self.plaintext(Credential::CivitaiApiKey))
    }

    /// The HuggingFace token for this process, resolved like the API key.
    pub fn huggingface_token(&self) -> Option<&str> {
        self.secrets
            .huggingface_token
            .as_deref()
            .or(self.plaintext(Credential::HuggingfaceToken))
    }

    /// Loads credentials from the Secret Service and moves any plaintext ones
    /// out of `config.toml`.
    ///
    /// A value already in the keyring wins; a value still in the file is
    /// migrated into the keyring and stripped from disk. If the Secret Service
    /// is unreachable the plaintext values keep working so downloads do not
    /// break on a headless machine, and the reason is logged.
    pub async fn resolve_credentials(&mut self) -> Result<()> {
        let store = match Store::open().await {
            Ok(store) => store,
            Err(e) => {
                warn!("Secret Service unavailable, using config.toml credentials: {e:#}");
                return Ok(());
            }
        };

        let mut stripped = false;
        for cred in ALL_CREDENTIALS {
            let plaintext = self.plaintext(cred).map(str::to_owned);
            let resolved = match store.get(cred).await {
                Ok(Some(secret)) => {
                    if plaintext.is_some() {
                        warn!(
                            "{} is in the keyring as well as config.toml; dropping the plaintext copy",
                            cred.config_field()
                        );
                    }
                    Some(secret)
                }
                Ok(None) => match plaintext.clone() {
                    Some(value) => match store.set(cred, &value).await {
                        Ok(()) => {
                            info!(
                                "Moved {} from config.toml into the keyring",
                                cred.config_field()
                            );
                            Some(value)
                        }
                        Err(e) => {
                            warn!(
                                "Could not move {} into the keyring, leaving it in config.toml: {e:#}",
                                cred.config_field()
                            );
                            continue;
                        }
                    },
                    None => continue,
                },
                Err(e) => {
                    warn!(
                        "Reading {} from the keyring failed, using config.toml: {e:#}",
                        cred.config_field()
                    );
                    continue;
                }
            };

            *self.resolved_mut(cred) = resolved;
            if self.plaintext_mut(cred).take().is_some() {
                stripped = true;
            }
        }

        if stripped {
            self.save()
                .context("rewriting config.toml without plaintext credentials")?;
        }
        Ok(())
    }

    /// Clears a plaintext credential from `config.toml`, returning whether the
    /// file had to be rewritten.
    pub fn forget_plaintext(&mut self, cred: Credential) -> Result<bool> {
        if self.plaintext_mut(cred).take().is_none() {
            return Ok(false);
        }
        self.save()
            .context("rewriting config.toml without the plaintext credential")?;
        Ok(true)
    }

    fn plaintext(&self, cred: Credential) -> Option<&str> {
        match cred {
            Credential::CivitaiApiKey => self.civitai.api_key.as_deref(),
            Credential::HuggingfaceToken => self.huggingface.token.as_deref(),
        }
        .map(str::trim)
        .filter(|s| !s.is_empty())
    }

    fn plaintext_mut(&mut self, cred: Credential) -> &mut Option<String> {
        match cred {
            Credential::CivitaiApiKey => &mut self.civitai.api_key,
            Credential::HuggingfaceToken => &mut self.huggingface.token,
        }
    }

    fn resolved_mut(&mut self, cred: Credential) -> &mut Option<String> {
        match cred {
            Credential::CivitaiApiKey => &mut self.secrets.civitai_api_key,
            Credential::HuggingfaceToken => &mut self.secrets.huggingface_token,
        }
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

/// Returns `$XDG_CACHE_HOME`, falling back to `$HOME/.cache`.
pub fn xdg_cache_home() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keyring_credential_wins_over_plaintext() {
        let mut config = Config::default();
        config.civitai.api_key = Some("from-file".into());
        config.secrets.civitai_api_key = Some("from-keyring".into());
        assert_eq!(config.civitai_api_key(), Some("from-keyring"));
    }

    #[test]
    fn test_plaintext_credential_is_the_fallback() {
        let mut config = Config::default();
        config.huggingface.token = Some("from-file".into());
        assert_eq!(config.huggingface_token(), Some("from-file"));
    }

    #[test]
    fn test_blank_plaintext_credential_counts_as_unset() {
        let mut config = Config::default();
        config.civitai.api_key = Some("   ".into());
        config.huggingface.token = Some(String::new());
        assert_eq!(config.civitai_api_key(), None);
        assert_eq!(config.huggingface_token(), None);
    }

    #[test]
    fn test_resolved_credentials_are_not_serialised() {
        let mut config = Config::default();
        config.secrets.civitai_api_key = Some("top-secret".into());
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(!text.contains("top-secret"), "config.toml text: {text}");
    }
}
