//! Credential storage in the freedesktop Secret Service.
//!
//! API keys used to sit in plaintext in `config.toml`. They now live in the
//! session keyring — gnome-keyring, KWallet or any other implementation of the
//! D-Bus `org.freedesktop.secrets` interface — and `config.toml` keeps only
//! non-secret settings. Items are found by their attributes, so the same
//! credential can be read back regardless of the label a keyring manager
//! shows.

use anyhow::{Context, Result};
use secret_service::{EncryptionType, SecretService};
use std::collections::HashMap;

/// Attribute value marking the keyring items owned by this program.
const APPLICATION: &str = "comfyui-downloader";

/// A credential this program needs at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    CivitaiApiKey,
    HuggingfaceToken,
}

pub const ALL_CREDENTIALS: [Credential; 2] =
    [Credential::CivitaiApiKey, Credential::HuggingfaceToken];

impl Credential {
    /// Stable attribute value used to find the item again.
    pub fn id(self) -> &'static str {
        match self {
            Credential::CivitaiApiKey => "civitai-api-key",
            Credential::HuggingfaceToken => "huggingface-token",
        }
    }

    /// Label shown by keyring managers such as Seahorse or KWalletManager.
    pub fn label(self) -> &'static str {
        match self {
            Credential::CivitaiApiKey => "CivitAI API key (comfyui-downloader)",
            Credential::HuggingfaceToken => "HuggingFace token (comfyui-downloader)",
        }
    }

    /// The `config.toml` field that held this credential before the keyring.
    pub fn config_field(self) -> &'static str {
        match self {
            Credential::CivitaiApiKey => "civitai.api_key",
            Credential::HuggingfaceToken => "huggingface.token",
        }
    }

    /// Name accepted by `comfyui-dl set-key --service`.
    pub fn service_name(self) -> &'static str {
        match self {
            Credential::CivitaiApiKey => "civitai",
            Credential::HuggingfaceToken => "huggingface",
        }
    }

    fn attributes(self) -> HashMap<&'static str, &'static str> {
        HashMap::from([("application", APPLICATION), ("credential", self.id())])
    }
}

/// An open session with the Secret Service.
///
/// Opening the store negotiates the D-Bus connection and an encrypted session,
/// so callers that touch several credentials should reuse one instance.
pub struct Store {
    service: SecretService<'static>,
}

impl Store {
    pub async fn open() -> Result<Self> {
        let service = SecretService::connect(EncryptionType::Dh)
            .await
            .context("connecting to the Secret Service (org.freedesktop.secrets) over D-Bus")?;
        Ok(Self { service })
    }

    /// Reads a credential. Returns `None` when the keyring holds no item for it.
    pub async fn get(&self, cred: Credential) -> Result<Option<String>> {
        let found = self
            .service
            .search_items(cred.attributes())
            .await
            .with_context(|| format!("searching the keyring for {}", cred.id()))?;

        let item = match found.unlocked.first() {
            Some(item) => item,
            None => match found.locked.first() {
                Some(locked) => {
                    locked
                        .unlock()
                        .await
                        .with_context(|| format!("unlocking keyring item {}", cred.id()))?;
                    locked
                }
                None => return Ok(None),
            },
        };

        let bytes = item
            .get_secret()
            .await
            .with_context(|| format!("reading keyring item {}", cred.id()))?;
        let value = String::from_utf8(bytes)
            .with_context(|| format!("keyring item {} is not valid UTF-8", cred.id()))?;
        Ok(non_empty(&value))
    }

    /// Writes a credential, replacing any previous item for the same credential.
    pub async fn set(&self, cred: Credential, secret: &str) -> Result<()> {
        let collection = self
            .service
            .get_default_collection()
            .await
            .context("opening the default keyring collection")?;
        if collection
            .is_locked()
            .await
            .context("checking whether the default keyring collection is locked")?
        {
            collection
                .unlock()
                .await
                .context("unlocking the default keyring collection")?;
        }
        collection
            .create_item(
                cred.label(),
                cred.attributes(),
                secret.as_bytes(),
                true,
                "text/plain",
            )
            .await
            .with_context(|| format!("storing {} in the keyring", cred.id()))?;
        Ok(())
    }
}

/// Trims a credential and discards blank values: an empty string in either the
/// keyring or `config.toml` means "unset", not "authenticate with nothing".
pub fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Resolves a `--service` value to a credential.
pub fn parse_service(name: &str) -> Option<Credential> {
    match name.trim().to_ascii_lowercase().as_str() {
        "civitai" => Some(Credential::CivitaiApiKey),
        "huggingface" | "hf" => Some(Credential::HuggingfaceToken),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credential_attributes_identify_the_application_and_credential() {
        let attrs = Credential::CivitaiApiKey.attributes();
        assert_eq!(attrs.get("application"), Some(&"comfyui-downloader"));
        assert_eq!(attrs.get("credential"), Some(&"civitai-api-key"));
    }

    #[test]
    fn test_credential_attributes_differ_per_credential() {
        assert_ne!(
            Credential::CivitaiApiKey.attributes(),
            Credential::HuggingfaceToken.attributes()
        );
    }

    #[test]
    fn test_parse_service_accepts_known_names() {
        assert_eq!(parse_service("civitai"), Some(Credential::CivitaiApiKey));
        assert_eq!(
            parse_service(" HuggingFace "),
            Some(Credential::HuggingfaceToken)
        );
        assert_eq!(parse_service("hf"), Some(Credential::HuggingfaceToken));
        assert_eq!(parse_service("openai"), None);
    }

    #[test]
    fn test_non_empty_trims_and_discards_blanks() {
        assert_eq!(non_empty("  key  "), Some("key".to_string()));
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty(""), None);
    }
}
