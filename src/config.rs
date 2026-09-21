use crate::placement::AliasConfigError;
use crate::secrets::{ALL_CREDENTIALS, Credential, Store};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    #[serde(default)]
    pub model_families: ModelFamiliesConfig,
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

/// Browsing-label aliases. Once initialised this is a user-owned snapshot: new
/// releases never overlay it at runtime, only an explicit merge changes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelFamiliesConfig {
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// The defaults this snapshot was seeded from. `None` means the section was
    /// never initialised; an empty map means every default was removed on
    /// purpose. The distinction is what stops a merge resurrecting deletions.
    #[serde(default)]
    pub baseline: Option<BTreeMap<String, String>>,
}

impl ModelFamiliesConfig {
    /// Build the validated lookup used when resolving placements.
    pub fn alias_table(&self) -> Result<crate::placement::AliasTable, AliasConfigError> {
        crate::placement::AliasTable::new(
            self.aliases.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        )
    }
}

/// One default whose value disagrees with the user's snapshot. Reported, never
/// applied: a user edit outranks a newly released default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AliasConflict {
    pub source_label: String,
    pub user_value: String,
    pub default_value: String,
}

/// Whether a merge may resurrect aliases the user deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreRemoved {
    Yes,
    No,
}

/// What an alias merge would change, for review before anything is written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AliasMergePlan {
    /// Defaults that never appeared in this snapshot's baseline.
    pub additions: BTreeMap<String, String>,
    /// Defaults the user has since edited.
    pub conflicts: Vec<AliasConflict>,
    /// Baseline entries the user deleted. Restoring one needs approval.
    pub previously_removed: BTreeMap<String, String>,
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

    /// Seed the alias snapshot when it has never been initialised. Returns
    /// whether anything was written, and never touches other sections.
    pub fn initialise_model_families(&mut self, defaults: &[(&str, &str)]) -> bool {
        if self.model_families.baseline.is_some() {
            return false;
        }
        let seeded: BTreeMap<String, String> = defaults
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        self.model_families.aliases = seeded.clone();
        self.model_families.baseline = Some(seeded);
        true
    }

    /// Classify a set of defaults against the current snapshot without writing.
    pub fn preview_alias_merge(&self, defaults: &[(&str, &str)]) -> AliasMergePlan {
        let baseline = self.model_families.baseline.clone().unwrap_or_default();
        let current = &self.model_families.aliases;
        let mut plan = AliasMergePlan::default();
        for (source_label, default_value) in defaults {
            match current.get(*source_label) {
                Some(user_value) if user_value != default_value => {
                    plan.conflicts.push(AliasConflict {
                        source_label: (*source_label).to_string(),
                        user_value: user_value.clone(),
                        default_value: (*default_value).to_string(),
                    });
                }
                Some(_) => {}
                None if baseline.contains_key(*source_label) => {
                    plan.previously_removed
                        .insert((*source_label).to_string(), (*default_value).to_string());
                }
                None => {
                    plan.additions
                        .insert((*source_label).to_string(), (*default_value).to_string());
                }
            }
        }
        plan
    }

    /// Write a reviewed plan into the snapshot. Conflicts keep the user's value
    /// always; deletions return only when `restore` says so.
    pub fn apply_alias_merge(&mut self, plan: &AliasMergePlan, restore: RestoreRemoved) {
        let families = &mut self.model_families;
        families
            .aliases
            .extend(plan.additions.iter().map(|(k, v)| (k.clone(), v.clone())));
        if restore == RestoreRemoved::Yes {
            families.aliases.extend(
                plan.previously_removed
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        let baseline = families.baseline.get_or_insert_with(BTreeMap::new);
        baseline.extend(plan.additions.iter().map(|(k, v)| (k.clone(), v.clone())));
        baseline.extend(
            plan.previously_removed
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        for conflict in &plan.conflicts {
            baseline.insert(
                conflict.source_label.clone(),
                conflict.default_value.clone(),
            );
        }
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
    fn test_initialising_aliases_leaves_unrelated_settings_alone() {
        let text = r#"
[paths]
models_dir = "/srv/models"

[daemon]
update_interval_hours = 6
max_concurrent_downloads = 3
socket_path = "/run/user/1000/x.sock"
"#;
        let mut config: Config = toml::from_str(text).expect("fixture config parses");

        let populated = config.initialise_model_families(&[("SD1.5", "SD 1.5")]);

        assert!(populated);
        assert_eq!(config.paths.models_dir, PathBuf::from("/srv/models"));
        assert_eq!(config.daemon.update_interval_hours, 6);
        assert_eq!(config.daemon.max_concurrent_downloads, 3);
        assert_eq!(
            config
                .model_families
                .aliases
                .get("SD1.5")
                .map(String::as_str),
            Some("SD 1.5")
        );
    }

    #[test]
    fn test_a_populated_alias_snapshot_is_never_overlaid_by_new_defaults() {
        let mut config = Config::default();
        config.initialise_model_families(&[("SD1.5", "SD 1.5")]);
        config.model_families.aliases.remove("SD1.5");

        let populated =
            config.initialise_model_families(&[("SD1.5", "SD 1.5"), ("LTX-2", "LTXV2")]);

        assert!(!populated);
        assert!(config.model_families.aliases.is_empty());
    }

    #[test]
    fn test_merge_preview_separates_additions_conflicts_and_removals() {
        let mut config = Config::default();
        config.initialise_model_families(&[("SD1.5", "SD 1.5"), ("LTX-2", "LTXV2")]);
        config
            .model_families
            .aliases
            .insert("SD1.5".to_string(), "Stable Diffusion 1.5".to_string());
        config.model_families.aliases.remove("LTX-2");

        let plan = config.preview_alias_merge(&[
            ("SD1.5", "SD 1.5"),
            ("LTX-2", "LTXV2"),
            ("Wan2.2", "Wan Video 2.2"),
        ]);

        assert_eq!(
            plan.additions.get("Wan2.2").map(String::as_str),
            Some("Wan Video 2.2")
        );
        assert_eq!(plan.additions.len(), 1);
        assert_eq!(
            plan.conflicts,
            vec![AliasConflict {
                source_label: "SD1.5".to_string(),
                user_value: "Stable Diffusion 1.5".to_string(),
                default_value: "SD 1.5".to_string(),
            }]
        );
        assert_eq!(
            plan.previously_removed.get("LTX-2").map(String::as_str),
            Some("LTXV2")
        );
    }

    fn snapshot_with_an_edit_and_a_deletion() -> Config {
        let mut config = Config::default();
        config.initialise_model_families(&[("SD1.5", "SD 1.5"), ("LTX-2", "LTXV2")]);
        config
            .model_families
            .aliases
            .insert("SD1.5".to_string(), "Stable Diffusion 1.5".to_string());
        config.model_families.aliases.remove("LTX-2");
        config
    }

    const LATER_DEFAULTS: &[(&str, &str)] = &[
        ("SD1.5", "SD 1.5"),
        ("LTX-2", "LTXV2"),
        ("Wan2.2", "Wan Video 2.2"),
    ];

    #[test]
    fn test_applying_a_merge_adds_defaults_without_undoing_user_choices() {
        let mut config = snapshot_with_an_edit_and_a_deletion();
        let plan = config.preview_alias_merge(LATER_DEFAULTS);

        config.apply_alias_merge(&plan, RestoreRemoved::No);

        let aliases = &config.model_families.aliases;
        assert_eq!(
            aliases.get("Wan2.2").map(String::as_str),
            Some("Wan Video 2.2")
        );
        assert_eq!(
            aliases.get("SD1.5").map(String::as_str),
            Some("Stable Diffusion 1.5")
        );
        assert!(!aliases.contains_key("LTX-2"));
        assert!(
            config
                .model_families
                .baseline
                .as_ref()
                .expect("baseline stays initialised")
                .contains_key("Wan2.2")
        );
    }

    #[test]
    fn test_a_merge_touches_nothing_but_the_aliases() {
        let text = r#"
[paths]
models_dir = "/srv/models"

[daemon]
update_interval_hours = 6
max_concurrent_downloads = 3
socket_path = "/run/user/1000/x.sock"

[gpu]
vram_bytes = 12000000000
"#;
        let mut config: Config = toml::from_str(text).expect("fixture config parses");
        config.initialise_model_families(&[("SD1.5", "SD 1.5")]);
        let plan = config.preview_alias_merge(LATER_DEFAULTS);

        config.apply_alias_merge(&plan, RestoreRemoved::No);

        assert_eq!(config.paths.models_dir, PathBuf::from("/srv/models"));
        assert_eq!(config.daemon.update_interval_hours, 6);
        assert_eq!(config.daemon.max_concurrent_downloads, 3);
        assert_eq!(config.gpu.vram_bytes, Some(12_000_000_000));
    }

    #[test]
    fn test_a_removed_alias_returns_only_when_approved() {
        let mut config = snapshot_with_an_edit_and_a_deletion();
        let plan = config.preview_alias_merge(LATER_DEFAULTS);

        config.apply_alias_merge(&plan, RestoreRemoved::Yes);

        assert_eq!(
            config
                .model_families
                .aliases
                .get("LTX-2")
                .map(String::as_str),
            Some("LTXV2")
        );
    }

    #[test]
    fn test_alias_table_rejects_an_ambiguous_configured_mapping() {
        let mut config = Config::default();
        config
            .model_families
            .aliases
            .insert("Wan2.2".to_string(), "Wan Video".to_string());
        config
            .model_families
            .aliases
            .insert("Wan Video".to_string(), "Wan".to_string());

        config
            .model_families
            .alias_table()
            .expect_err("a chained configured alias must be reported, not silently ordered");
    }

    #[test]
    fn test_alias_table_reflects_the_configured_mapping() {
        let mut config = Config::default();
        config
            .model_families
            .aliases
            .insert("SD1.5".to_string(), "SD 1.5".to_string());

        let table = config.model_families.alias_table().expect("valid aliases");

        assert_eq!(table.browsing_label("SD1.5"), "SD 1.5");
        assert_eq!(table.browsing_label("Flux.2 Klein"), "Flux.2 Klein");
    }

    #[test]
    fn test_resolved_credentials_are_not_serialised() {
        let mut config = Config::default();
        config.secrets.civitai_api_key = Some("top-secret".into());
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(!text.contains("top-secret"), "config.toml text: {text}");
    }
}
