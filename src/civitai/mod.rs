pub mod types;

use crate::civitai::types::{ModelVersion, ModelInfo};
use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

const BASE_URL: &str = "https://civitai.com/api/v1";

pub struct CivitaiClient {
    http: Client,
    api_key: Option<String>,
}

impl CivitaiClient {
    pub fn new(api_key: Option<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, api_key })
    }

    pub async fn get_model(&self, model_id: u64) -> Result<ModelInfo> {
        let url = format!("{BASE_URL}/models/{model_id}");
        self.get_json(&url).await
    }

    pub async fn get_model_version(&self, version_id: u64) -> Result<ModelVersion> {
        let url = format!("{BASE_URL}/model-versions/{version_id}");
        self.get_json(&url).await
    }

    pub async fn get_model_version_by_hash(&self, sha256: &str) -> Result<ModelVersion> {
        let url = format!("{BASE_URL}/model-versions/by-hash/{sha256}");
        self.get_json(&url).await
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let key = self.api_key.as_deref()
            .ok_or_else(|| anyhow::anyhow!("CivitAI API key is not configured (set civitai.api_key in config.toml)"))?;
        let mut attempts = 0u32;
        loop {
            let resp = self.http.get(url).bearer_auth(key).send().await.context("sending request")?;

            match resp.status() {
                StatusCode::TOO_MANY_REQUESTS => {
                    attempts += 1;
                    let delay = Duration::from_secs(2u64.pow(attempts.min(6)));
                    warn!("Rate limited; retrying in {}s", delay.as_secs());
                    sleep(delay).await;
                }
                s if s.is_success() => {
                    return resp.json::<T>().await.context("deserialising response");
                }
                s => {
                    anyhow::bail!("CivitAI API error: {s}");
                }
            }
        }
    }
}
