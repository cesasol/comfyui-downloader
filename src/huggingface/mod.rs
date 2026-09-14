//! HuggingFace Hub client: URL parsing, file metadata (size + sha256), download auth.

use anyhow::{Context, Result};
use reqwest::{Client, RequestBuilder, StatusCode, Url, header};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

const BASE_URL: &str = "https://huggingface.co";

/// A single file inside a HuggingFace repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfFileRef {
    pub repo: String,
    pub revision: String,
    pub path: String,
}

impl HfFileRef {
    pub fn resolve_url(&self) -> String {
        format!(
            "{BASE_URL}/{}/resolve/{}/{}",
            self.repo, self.revision, self.path
        )
    }

    pub fn file_name(&self) -> &str {
        self.path
            .rsplit_once('/')
            .map_or(&self.path, |(_, name)| name)
    }

    pub fn dir(&self) -> &str {
        self.path.rsplit_once('/').map_or("", |(dir, _)| dir)
    }
}

fn is_hf_host(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("huggingface.co" | "hf.co" | "www.huggingface.co" | "www.hf.co")
    )
}

pub fn is_huggingface_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| is_hf_host(&url))
}

pub fn parse_hf_url(url: &str) -> Option<HfFileRef> {
    let url = Url::parse(url).ok()?;
    if !is_hf_host(&url) {
        return None;
    }
    let mut segments = url.path().strip_prefix('/')?.splitn(5, '/');
    let owner = segments.next()?;
    let name = segments.next()?;
    let action = segments.next()?;
    let revision = segments.next()?;
    let path = segments.next()?;
    if owner.is_empty()
        || matches!(owner, "datasets" | "spaces")
        || name.is_empty()
        || !matches!(action, "resolve" | "blob")
        || revision.is_empty()
        || path.split('/').any(str::is_empty)
    {
        return None;
    }
    Some(HfFileRef {
        repo: format!("{owner}/{name}"),
        revision: revision.into(),
        path: path.into(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfFileMeta {
    pub path: String,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum TreeEntry {
    File {
        path: String,
        size: u64,
        lfs: Option<LfsInfo>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct LfsInfo {
    oid: String,
}

fn parse_tree(body: &str) -> Result<Vec<HfFileMeta>> {
    let entries: Vec<TreeEntry> =
        serde_json::from_str(body).context("parsing HuggingFace tree response")?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            TreeEntry::File { path, size, lfs } => Some(HfFileMeta {
                path,
                size,
                sha256: lfs.map(|lfs| lfs.oid),
            }),
            TreeEntry::Other => None,
        })
        .collect())
}

fn tree_url(repo: &str, revision: &str, dir: &str) -> Result<Url> {
    let mut url = Url::parse(BASE_URL).context("parsing HuggingFace API base URL")?;
    let mut path = format!("/api/models/{repo}/tree/{revision}");
    if !dir.is_empty() {
        path.push('/');
        path.push_str(dir);
    }
    // set_path escapes path delimiters such as '?' without double-encoding '%'.
    url.set_path(&path);
    url.query_pairs_mut().append_pair("recursive", "false");
    Ok(url)
}

fn next_page(url: &Url, headers: &header::HeaderMap) -> Result<Option<Url>> {
    for value in headers.get_all(header::LINK) {
        for link in value
            .to_str()
            .context("reading HuggingFace Link header")?
            .split(',')
        {
            let mut parts = link.split(';');
            let target = parts
                .next()
                .context("missing HuggingFace pagination target")?
                .trim();
            let is_next = parts.any(|part| {
                part.trim().split_once('=').is_some_and(|(name, value)| {
                    name.trim() == "rel"
                        && value
                            .trim()
                            .trim_matches('"')
                            .split_whitespace()
                            .any(|rel| rel == "next")
                })
            });
            if is_next {
                let target = target
                    .strip_prefix('<')
                    .and_then(|s| s.strip_suffix('>'))
                    .context("invalid HuggingFace pagination target")?;
                let next = url
                    .join(target)
                    .context("parsing HuggingFace pagination URL")?;
                // Never forward the configured bearer token to another origin.
                if next.origin() != url.origin() {
                    anyhow::bail!("HuggingFace pagination URL points to another origin");
                }
                return Ok(Some(next));
            }
        }
    }
    Ok(None)
}

pub struct HfClient {
    http: Client,
    token: Option<String>,
}

impl HfClient {
    pub fn new(token: Option<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HuggingFace HTTP client")?;
        Ok(Self { http, token })
    }

    /// Adds bearer authorization when a token is configured, otherwise passes through.
    pub fn authorize(&self, req: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    /// Lists files in a directory, following the tree API's pagination links.
    pub async fn list_dir(&self, repo: &str, revision: &str, dir: &str) -> Result<Vec<HfFileMeta>> {
        let mut url = tree_url(repo, revision, dir)?;
        let mut files = Vec::new();
        let mut attempts = 0u32;
        loop {
            let response = self
                .authorize(self.http.get(url.clone()))
                .send()
                .await
                .with_context(|| {
                    format!("requesting HuggingFace tree for {repo}/{dir} at {revision}")
                })?;
            match response.status() {
                StatusCode::TOO_MANY_REQUESTS => {
                    attempts = (attempts + 1).min(6);
                    let delay = Duration::from_secs(2u64.pow(attempts));
                    warn!("HuggingFace rate limited; retrying in {}s", delay.as_secs());
                    sleep(delay).await;
                }
                status if status.is_success() => {
                    let next = next_page(&url, response.headers())?;
                    let body = response
                        .text()
                        .await
                        .context("reading HuggingFace tree response")?;
                    files.extend(parse_tree(&body)?);
                    match next {
                        Some(next) => {
                            url = next;
                            attempts = 0;
                        }
                        None => return Ok(files),
                    }
                }
                status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                    anyhow::bail!(
                        "HuggingFace HTTP {status} for {repo}/{dir}: the repo may be gated and a token may be required (set huggingface.token in config.toml)"
                    );
                }
                StatusCode::NOT_FOUND => {
                    anyhow::bail!(
                        "HuggingFace repo/path not found: {repo}/{dir} at revision {revision} (HTTP 404)"
                    );
                }
                status => anyhow::bail!("HuggingFace API error for {repo}/{dir}: {status}"),
            }
        }
    }

    /// Finds a file's metadata in its parent directory; errors if the file is absent.
    pub async fn file_meta(&self, file: &HfFileRef) -> Result<HfFileMeta> {
        self.list_dir(&file.repo, &file.revision, file.dir())
            .await
            .with_context(|| {
                format!(
                    "looking up HuggingFace file {}/{} at {}",
                    file.repo, file.path, file.revision
                )
            })?
            .into_iter()
            .find(|meta| meta.path == file.path)
            .with_context(|| {
                format!(
                    "HuggingFace file not found: {}/{} at revision {}",
                    file.repo, file.path, file.revision
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = r#"[
        {"type":"file","path":"weights/model.safetensors","size":335304388,"oid":"git-oid",
         "lfs":{"oid":"0123456789abcdef","size":335304388}},
        {"type":"file","path":"weights/config.json","size":42,"oid":"plain-git-oid"},
        {"type":"directory","path":"weights/nested","size":0,"oid":"directory-oid"}
    ]"#;

    fn expected_file(revision: &str, path: &str) -> HfFileRef {
        HfFileRef {
            repo: "owner/model".into(),
            revision: revision.into(),
            path: path.into(),
        }
    }

    #[test]
    fn test_parse_hf_url_resolve() {
        assert_eq!(
            parse_hf_url("https://huggingface.co/owner/model/resolve/main/model.safetensors"),
            Some(expected_file("main", "model.safetensors"))
        );
    }

    #[test]
    fn test_parse_hf_url_blob() {
        assert_eq!(
            parse_hf_url("https://huggingface.co/owner/model/blob/main/model.safetensors"),
            Some(expected_file("main", "model.safetensors"))
        );
    }

    #[test]
    fn test_parse_hf_url_query_and_fragment() {
        assert_eq!(
            parse_hf_url(
                "https://huggingface.co/owner/model/resolve/main/model.safetensors?download=true#details"
            ),
            Some(expected_file("main", "model.safetensors"))
        );
    }

    #[test]
    fn test_parse_hf_url_nested_path() {
        assert_eq!(
            parse_hf_url("https://huggingface.co/owner/model/resolve/main/a/b/model.safetensors"),
            Some(expected_file("main", "a/b/model.safetensors"))
        );
    }

    #[test]
    fn test_parse_hf_url_hf_alias() {
        assert_eq!(
            parse_hf_url("https://hf.co/owner/model/resolve/main/model.safetensors"),
            Some(expected_file("main", "model.safetensors"))
        );
    }

    #[test]
    fn test_parse_hf_url_www_aliases() {
        for host in ["www.huggingface.co", "www.hf.co"] {
            assert_eq!(
                parse_hf_url(&format!(
                    "https://{host}/owner/model/resolve/main/model.safetensors"
                )),
                Some(expected_file("main", "model.safetensors"))
            );
        }
    }

    #[test]
    fn test_parse_hf_url_revision() {
        assert_eq!(
            parse_hf_url("https://huggingface.co/owner/model/resolve/v2/model.safetensors"),
            Some(expected_file("v2", "model.safetensors"))
        );
    }

    #[test]
    fn test_parse_hf_url_keeps_encoded_path() {
        assert_eq!(
            parse_hf_url("https://huggingface.co/owner/model/resolve/main/a%20b/model.safetensors"),
            Some(expected_file("main", "a%20b/model.safetensors"))
        );
    }

    #[test]
    fn test_parse_hf_url_rejects_unsupported_urls() {
        for url in [
            "https://civitai.com/owner/model/resolve/main/model.safetensors",
            "https://huggingface.co/datasets/owner/model/resolve/main/data.json",
            "https://huggingface.co/spaces/owner/model/resolve/main/app.py",
            "https://huggingface.co/owner/model",
            "https://huggingface.co/owner/model/resolve/main",
            "https://huggingface.co/owner/model/resolve/main/",
            "https://huggingface.co/owner/model/resolve/main/folder/",
            "https://huggingface.co/owner/model/tree/main/model.safetensors",
            "https://huggingface.co/owner/model/resolve//model.safetensors",
            "not a URL",
        ] {
            assert_eq!(parse_hf_url(url), None, "{url}");
        }
    }

    #[test]
    fn test_is_huggingface_url_hosts() {
        for host in ["huggingface.co", "hf.co", "www.huggingface.co", "www.hf.co"] {
            assert!(is_huggingface_url(&format!("https://{host}/owner/model")));
        }
        for url in [
            "https://huggingface.co.evil.invalid/model",
            "https://evil.invalid/huggingface.co",
            "https://huggingface.co@evil.invalid/model",
            "https://civitai.com/model",
            "not a URL",
        ] {
            assert!(!is_huggingface_url(url), "{url}");
        }
    }

    #[test]
    fn test_resolve_url_round_trip() {
        let url = "https://huggingface.co/owner/model/resolve/v2/a%20b/model.safetensors";
        let file = parse_hf_url(url).unwrap();
        assert_eq!(file.resolve_url(), url);
        assert_eq!(parse_hf_url(&file.resolve_url()), Some(file));
    }

    #[test]
    fn test_file_name() {
        for path in ["model.safetensors", "a/b/model.safetensors"] {
            assert_eq!(expected_file("main", path).file_name(), "model.safetensors");
        }
    }

    #[test]
    fn test_dir_nested() {
        assert_eq!(expected_file("main", "a/b/model.safetensors").dir(), "a/b");
    }

    #[test]
    fn test_dir_root() {
        assert_eq!(expected_file("main", "model.safetensors").dir(), "");
    }

    #[test]
    fn test_parse_tree_lfs_sha256() {
        let files = parse_tree(TREE).unwrap();
        assert_eq!(
            files[0],
            HfFileMeta {
                path: "weights/model.safetensors".into(),
                size: 335304388,
                sha256: Some("0123456789abcdef".into()),
            }
        );
    }

    #[test]
    fn test_parse_tree_plain_git_blob() {
        let files = parse_tree(TREE).unwrap();
        assert_eq!(
            files[1],
            HfFileMeta {
                path: "weights/config.json".into(),
                size: 42,
                sha256: None,
            }
        );
    }

    #[test]
    fn test_parse_tree_skips_directories() {
        let files = parse_tree(TREE).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.path != "weights/nested"));
    }

    #[test]
    fn test_parse_tree_invalid_json() {
        assert!(parse_tree("not JSON").is_err());
        assert!(parse_tree(r#"[{"type":"file","path":"model.safetensors"}]"#).is_err());
    }

    #[test]
    fn test_tree_url_root_has_no_trailing_slash() {
        let url = tree_url("owner/model", "v2", "").unwrap();
        assert_eq!(
            url.as_str(),
            "https://huggingface.co/api/models/owner/model/tree/v2?recursive=false"
        );
    }

    #[test]
    fn test_tree_url_nested_path_encoding() {
        let url = tree_url("owner/model", "main", "a%20b/c d/#?").unwrap();
        assert_eq!(
            url.path(),
            "/api/models/owner/model/tree/main/a%20b/c%20d/%23%3F"
        );
        assert_eq!(url.query(), Some("recursive=false"));
    }

    #[test]
    fn test_next_page_link_cursor() {
        let url = tree_url("owner/model", "main", "weights").unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            "<?cursor=previous>; rel=\"prev\", <?recursive=false&cursor=abc>; rel=\"next\""
                .parse()
                .unwrap(),
        );
        let next = next_page(&url, &headers).unwrap().unwrap();
        assert_eq!(next.path(), url.path());
        assert_eq!(next.query(), Some("recursive=false&cursor=abc"));
    }

    #[test]
    fn test_next_page_without_link() {
        let url = tree_url("owner/model", "main", "").unwrap();
        assert_eq!(
            next_page(&url, &reqwest::header::HeaderMap::new()).unwrap(),
            None
        );
    }

    #[test]
    fn test_next_page_rejects_cross_origin() {
        let url = tree_url("owner/model", "main", "").unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            "<https://example.invalid/?cursor=abc>; rel=\"next\""
                .parse()
                .unwrap(),
        );
        assert!(next_page(&url, &headers).is_err());
    }

    #[test]
    fn test_client_new_without_token() {
        assert!(HfClient::new(None).is_ok());
    }

    #[test]
    fn test_client_new_with_token() {
        assert!(HfClient::new(Some("test-token".into())).is_ok());
    }

    #[test]
    fn test_authorize_without_token() {
        let client = HfClient::new(None).unwrap();
        let request = client
            .authorize(reqwest::Client::new().get("https://example.invalid"))
            .build()
            .unwrap();
        assert!(
            !request
                .headers()
                .contains_key(reqwest::header::AUTHORIZATION)
        );
    }

    #[test]
    fn test_authorize_with_token() {
        let client = HfClient::new(Some("test-token".into())).unwrap();
        let request = client
            .authorize(reqwest::Client::new().get("https://example.invalid"))
            .build()
            .unwrap();
        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer test-token"
        );
    }

    #[test]
    fn test_authorize_without_token_preserves_existing_header() {
        let client = HfClient::new(None).unwrap();
        let request = client
            .authorize(
                reqwest::Client::new()
                    .get("https://example.invalid")
                    .bearer_auth("existing"),
            )
            .build()
            .unwrap();
        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer existing"
        );
    }
}
