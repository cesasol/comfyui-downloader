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
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::{debug, info};

/// Attribute value marking the keyring items owned by this program.
const APPLICATION: &str = "comfyui-downloader";

/// How long `Store::open_ready` keeps retrying the connection while the Secret
/// Service is still coming up at login. The unlock prompt it may raise is not
/// covered: that blocks until the user answers or dismisses it.
pub const STARTUP_WAIT: Duration = Duration::from_secs(30);

/// How long a late [`Cache::refresh`] waits for the keyring before giving up:
/// short, because a request is blocked while it runs.
pub const REFRESH_WAIT: Duration = Duration::from_secs(5);

/// Minimum gap between two late refreshes, so a credential that is genuinely
/// absent cannot make every request open a D-Bus session.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(60);

/// Retry back-off while waiting for the Secret Service: first delay, then cap.
const RETRY_INITIAL: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(2);

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

/// The credentials this process is using right now.
///
/// The daemon seeds this once at startup and shares clones of the `Arc` with
/// the HTTP clients, so a keyring that only comes up after startup — or a
/// credential stored later with `comfyui-dl set-key` — is picked up without a
/// restart. Reads and writes are plain memory operations and therefore
/// synchronous; only [`Cache::refresh`] touches D-Bus.
#[derive(Debug, Default)]
pub struct Cache {
    civitai_api_key: RwLock<Option<String>>,
    huggingface_token: RwLock<Option<String>>,
    /// When the keyring was last asked, so a credential that is genuinely
    /// absent cannot make every caller open a D-Bus session.
    last_refresh: Mutex<Option<Instant>>,
}

impl Cache {
    /// Reads a credential. `None` means unset, never empty.
    pub fn get(&self, cred: Credential) -> Option<String> {
        self.slot(cred)
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Records a credential, replacing any previous value. A blank value clears
    /// it, matching `config.toml` where blank means unset.
    pub fn set(&self, cred: Credential, value: &str) {
        *self
            .slot(cred)
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = non_empty(value);
    }

    /// Asks the keyring again for every credential still missing, at most once
    /// per [`REFRESH_COOLDOWN`]. Returns whether anything was filled in.
    ///
    /// This is the recovery path for a keyring that was not ready at startup:
    /// whoever needs a credential calls it, and a value that has appeared since
    /// is used for the rest of the process's life.
    pub async fn refresh(&self, timeout: Duration) -> bool {
        let missing: Vec<Credential> = ALL_CREDENTIALS
            .into_iter()
            .filter(|cred| self.get(*cred).is_none())
            .collect();
        if missing.is_empty() || !self.begin_refresh() {
            return false;
        }

        let Ok(store) = Store::open_ready(timeout).await else {
            debug!("Keyring still unavailable; keeping the credentials already in use");
            return false;
        };

        let mut filled = false;
        for cred in missing {
            match store.get(cred).await {
                Ok(Some(secret)) => {
                    info!("Picked up {} from the keyring", cred.config_field());
                    self.set(cred, &secret);
                    filled = true;
                }
                Ok(None) => {}
                Err(e) => debug!("Keyring read of {} failed: {e:#}", cred.id()),
            }
        }
        filled
    }

    /// Rate-limits refreshes. `true` means this caller may proceed.
    fn begin_refresh(&self) -> bool {
        let mut last = self
            .last_refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.is_some_and(|at| at.elapsed() < REFRESH_COOLDOWN) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    fn slot(&self, cred: Credential) -> &RwLock<Option<String>> {
        match cred {
            Credential::CivitaiApiKey => &self.civitai_api_key,
            Credential::HuggingfaceToken => &self.huggingface_token,
        }
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

    /// Opens the store for a login-time process, retrying the connection until
    /// `timeout` elapses.
    ///
    /// A systemd user service starts alongside the graphical session, before
    /// gnome-keyring-daemon (or another provider) has claimed
    /// `org.freedesktop.secrets` and before the login keyring is unlocked. A
    /// single attempt loses that race and leaves the process without
    /// credentials, so wait for the service to appear and ask for the keyring
    /// to be unlocked. `timeout` bounds only the connection retries: asking
    /// blocks until the unlock prompt is answered or dismissed. Machines with
    /// no session bus are not waited on: no provider can appear there, so the
    /// caller's plaintext fallback is used.
    pub async fn open_ready(timeout: Duration) -> Result<Self> {
        if !session_bus_present() {
            return Self::open().await;
        }
        retry_until(timeout, is_transient, || async {
            let store = Self::open().await?;
            store.unlock_default_collection().await?;
            Ok(store)
        })
        .await
    }

    /// Asks the Secret Service to unlock the default collection when it is
    /// locked. This is what raises the keyring's unlock prompt.
    async fn unlock_default_collection(&self) -> Result<()> {
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
            info!("The keyring is locked; asking the Secret Service to unlock it");
            collection
                .unlock()
                .await
                .context("unlocking the default keyring collection")?;
        }
        Ok(())
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

/// Whether a failed keyring attempt is worth retrying.
///
/// A provider that has not claimed `org.freedesktop.secrets` yet is transient
/// and will come up on its own. A user who dismissed the unlock prompt is not,
/// and `Locked` means an item stayed sealed: retrying either would only
/// re-raise the prompt the user just declined.
fn is_transient(error: &anyhow::Error) -> bool {
    !matches!(
        error.downcast_ref::<secret_service::Error>(),
        Some(
            secret_service::Error::Prompt
                | secret_service::Error::PromptDisconnected
                | secret_service::Error::Locked
                | secret_service::Error::Crypto(_)
                | secret_service::Error::Zvariant(_)
        )
    )
}

/// Retries `attempt` with exponential back-off until it succeeds, `retryable`
/// rejects the error, or `timeout` elapses.
async fn retry_until<T, F, Fut>(
    timeout: Duration,
    retryable: fn(&anyhow::Error) -> bool,
    mut attempt: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let deadline = Instant::now() + timeout;
    let mut delay = RETRY_INITIAL;
    loop {
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(e) => e,
        };
        if !retryable(&error) {
            debug!("Not retrying: {error:#}");
            return Err(error);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(error);
        }
        let pause = delay.min(deadline - now);
        debug!("Secret Service not ready yet, retrying in {pause:?}: {error:#}");
        sleep(pause).await;
        delay = (delay * 2).min(RETRY_MAX);
    }
}

/// Whether a session bus could exist, so the startup wait does not stall a
/// machine that has none (where no Secret Service provider could ever appear).
fn session_bus_present() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| PathBuf::from(dir).join("bus").exists())
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

/// Rejects a value that cannot be a credential of this kind, before it reaches
/// the keyring.
///
/// The usual mistake is pasting a whole `config.toml` or JSON line, which wraps
/// the key in quotes and leaves a trailing comma. That value is stored as-is and
/// every later request fails with HTTP 401, so name the stray characters.
pub fn check_format(cred: Credential, value: &str) -> Result<()> {
    let stray: String = value
        .chars()
        .filter(|c| matches!(c, '"' | '\'' | ',' | '=') || c.is_whitespace())
        .collect();
    if !stray.is_empty() {
        anyhow::bail!(
            "the {} credential contains {stray:?}; paste only the key itself, without quotes, commas or spaces",
            cred.service_name()
        );
    }
    match cred {
        Credential::CivitaiApiKey => {
            if value.len() != 32 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
                anyhow::bail!(
                    "a CivitAI API key is 32 hexadecimal characters; this one has {} characters",
                    value.chars().count()
                );
            }
        }
        Credential::HuggingfaceToken => {
            let body = value.strip_prefix("hf_").unwrap_or_default();
            if body.is_empty() || !body.chars().all(|c| c.is_ascii_alphanumeric()) {
                anyhow::bail!(
                    "a HuggingFace token starts with `hf_` followed by letters and digits"
                );
            }
        }
    }
    Ok(())
}

/// What the service said about a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accepted,
    /// HTTP 401 or 403: the service does not recognise the credential.
    Rejected(u16),
}

/// Asks the service whether it accepts `value`, using an endpoint that only
/// identifies the caller. An `Err` means the question could not be asked
/// (offline, timeout, server error), not that the credential is bad.
pub async fn verify_online(cred: Credential, value: &str) -> Result<Verdict> {
    let url = match cred {
        Credential::CivitaiApiKey => "https://civitai.com/api/v1/me",
        Credential::HuggingfaceToken => "https://huggingface.co/api/whoami-v2",
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building the HTTP client")?;
    let status = http
        .get(url)
        .bearer_auth(value)
        .send()
        .await
        .with_context(|| {
            format!(
                "asking {url} whether the {} credential is valid",
                cred.service_name()
            )
        })?
        .status();
    match status {
        s if s.is_success() => Ok(Verdict::Accepted),
        s @ (reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => {
            Ok(Verdict::Rejected(s.as_u16()))
        }
        s => anyhow::bail!("{url} answered HTTP {s}"),
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
    fn test_check_format_accepts_a_civitai_key() {
        assert!(check_format(Credential::CivitaiApiKey, &"aB".repeat(16)).is_ok());
    }

    #[test]
    fn test_check_format_names_quotes_and_commas_pasted_from_toml() {
        let error = check_format(
            Credential::CivitaiApiKey,
            &format!("\"{}\",", "a".repeat(32)),
        )
        .expect_err("a quoted key must be rejected");
        assert!(error.to_string().contains("quotes"), "{error}");
    }

    #[test]
    fn test_check_format_rejects_a_civitai_key_of_the_wrong_length() {
        assert!(check_format(Credential::CivitaiApiKey, &"a".repeat(16)).is_err());
    }

    #[test]
    fn test_check_format_requires_the_huggingface_prefix() {
        assert!(check_format(Credential::HuggingfaceToken, "hf_AbC123").is_ok());
        assert!(check_format(Credential::HuggingfaceToken, "AbC123").is_err());
        assert!(check_format(Credential::HuggingfaceToken, "hf_").is_err());
    }

    #[test]
    fn test_non_empty_trims_and_discards_blanks() {
        assert_eq!(non_empty("  key  "), Some("key".to_string()));
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty(""), None);
    }

    #[tokio::test]
    async fn test_retry_until_returns_the_first_success_and_stops() {
        let mut attempts = 0;
        let value = retry_until(
            Duration::from_secs(5),
            |_| true,
            || {
                attempts += 1;
                let attempt = attempts;
                async move {
                    if attempt < 3 {
                        anyhow::bail!("not ready")
                    }
                    Ok(attempt)
                }
            },
        )
        .await
        .expect("the third attempt succeeds");

        assert_eq!(value, 3);
        assert_eq!(attempts, 3, "no attempt is made after a success");
    }

    #[tokio::test]
    async fn test_retry_until_gives_up_and_returns_the_last_error() {
        let mut attempts = 0;
        let error = retry_until(
            Duration::from_millis(50),
            |_| true,
            || {
                attempts += 1;
                async { Err::<(), _>(anyhow::anyhow!("still down")) }
            },
        )
        .await
        .expect_err("an attempt that never succeeds must give up");

        assert!(attempts >= 2, "expected retries, made {attempts}");
        assert!(error.to_string().contains("still down"));
    }

    #[tokio::test]
    async fn test_retry_until_stops_at_once_when_the_error_is_not_retryable() {
        let mut attempts = 0;
        let error = retry_until(
            Duration::from_secs(30),
            |_| false,
            || {
                attempts += 1;
                async { Err::<(), _>(anyhow::anyhow!("dismissed")) }
            },
        )
        .await
        .expect_err("a terminal error is returned");

        assert_eq!(attempts, 1, "a terminal error must not be retried");
        assert!(error.to_string().contains("dismissed"));
    }

    #[test]
    fn test_transient_keyring_failures_are_retried_and_refusals_are_not() {
        assert!(is_transient(&secret_service::Error::Unavailable.into()));
        assert!(is_transient(&secret_service::Error::NoResult.into()));
        assert!(!is_transient(&secret_service::Error::Locked.into()));
        assert!(!is_transient(&secret_service::Error::Prompt.into()));
        assert!(!is_transient(
            &secret_service::Error::PromptDisconnected.into()
        ));
    }

    #[test]
    fn test_a_context_layer_does_not_hide_the_keyring_error() {
        let contextual: anyhow::Error = secret_service::Error::Prompt.into();
        let contextual = contextual.context("connecting to the Secret Service");

        assert!(
            !is_transient(&contextual),
            "a dismissed prompt must stay terminal through anyhow context"
        );
    }

    #[test]
    fn test_cache_returns_what_was_stored_and_blank_clears_it() {
        let cache = Cache::default();
        cache.set(Credential::CivitaiApiKey, "  key  ");
        cache.set(Credential::HuggingfaceToken, "token");

        assert_eq!(
            cache.get(Credential::CivitaiApiKey),
            Some("key".to_string())
        );
        assert_eq!(
            cache.get(Credential::HuggingfaceToken),
            Some("token".to_string())
        );

        cache.set(Credential::CivitaiApiKey, "   ");
        assert_eq!(cache.get(Credential::CivitaiApiKey), None);
        assert_eq!(
            cache.get(Credential::HuggingfaceToken),
            Some("token".to_string())
        );
    }

    #[tokio::test]
    async fn test_cache_refresh_skips_the_keyring_when_nothing_is_missing() {
        let cache = Cache::default();
        cache.set(Credential::CivitaiApiKey, "key");
        cache.set(Credential::HuggingfaceToken, "token");

        assert!(
            !cache.refresh(Duration::from_millis(1)).await,
            "nothing is missing, so the keyring must not be contacted"
        );
    }

    #[test]
    fn test_cache_refreshes_are_rate_limited() {
        let cache = Cache::default();
        assert!(cache.begin_refresh(), "the first attempt is allowed");
        assert!(
            !cache.begin_refresh(),
            "a second attempt inside the cooldown is not"
        );
    }
}
