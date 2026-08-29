use std::path::Path;

use ecoscope_core::{EcoScopeError, Result};
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use url::{Host, Url};

use crate::table::{InfoMetadata, SearchRecord, parse_info, parse_search};

pub const DEFAULT_METADATA_LIMIT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DownloadMetadata {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub media_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub digest: String,
    pub size_bytes: u64,
    pub metadata: DownloadMetadata,
    pub final_url: Url,
}

#[derive(Clone)]
pub struct ErddapClient {
    client: Client,
    base_url: Url,
    metadata_limit_bytes: usize,
}

impl ErddapClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let base_url = Url::parse(base_url)
            .map_err(|error| EcoScopeError::Invalid(format!("invalid ERDDAP URL: {error}")))?;
        validate_base_url(&base_url)?;
        let client = Client::builder()
            .user_agent(concat!("ecoscope/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| EcoScopeError::Internal(error.to_string()))?;
        Ok(Self {
            client,
            base_url,
            metadata_limit_bytes: DEFAULT_METADATA_LIMIT_BYTES,
        })
    }

    pub fn with_metadata_limit_bytes(mut self, limit: usize) -> Self {
        self.metadata_limit_bytes = limit;
        self
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchRecord>> {
        let mut url = self.endpoint(&["search", "index.json"])?;
        url.query_pairs_mut()
            .append_pair("page", "1")
            .append_pair("itemsPerPage", &limit.clamp(1, 1_000).to_string())
            .append_pair("searchFor", query);
        let raw = self.get_json(url).await?;
        parse_search(&raw)
    }

    pub async fn info(&self, dataset_id: &str) -> Result<InfoMetadata> {
        let url = self.endpoint(&["info", dataset_id, "index.json"])?;
        let raw = self.get_json(url).await?;
        parse_info(&raw)
    }

    pub async fn server_version(&self) -> Result<String> {
        let url = self.endpoint(&["version"])?;
        let response =
            self.client.get(url).send().await.map_err(|error| {
                EcoScopeError::Internal(format!("ERDDAP request failed: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(EcoScopeError::Internal(format!(
                "ERDDAP version request returned {}",
                response.status()
            )));
        }
        if response.content_length().is_some_and(|size| size > 65_536) {
            return Err(EcoScopeError::Invalid(
                "ERDDAP version response is too large".into(),
            ));
        }
        let text = response
            .text()
            .await
            .map_err(|error| EcoScopeError::Internal(error.to_string()))?;
        let version = text
            .trim()
            .strip_prefix("ERDDAP_version=")
            .unwrap_or(text.trim())
            .trim();
        if version.is_empty() || version.len() > 65_536 {
            return Err(EcoScopeError::Invalid(
                "ERDDAP returned an invalid server version".into(),
            ));
        }
        Ok(version.to_owned())
    }

    pub async fn resolve_download_chain(&self, url: &Url) -> Result<Vec<Url>> {
        let mut chain = vec![url.clone()];
        for _ in 0..5 {
            let current = chain.last().expect("download chain is never empty");
            let response = self
                .client
                .head(current.clone())
                .send()
                .await
                .map_err(|error| {
                    EcoScopeError::Internal(format!("ERDDAP subset probe failed: {error}"))
                })?;
            if response.status().is_redirection() {
                let next = redirect_location(current, response.headers())?;
                validate_redirect_target(url, &next)?;
                chain.push(next);
                continue;
            }
            if response.status().is_success() || response.status() == StatusCode::METHOD_NOT_ALLOWED
            {
                return Ok(chain);
            }
            return Err(EcoScopeError::Internal(format!(
                "ERDDAP subset probe returned {}",
                response.status()
            )));
        }
        Err(EcoScopeError::Invalid(
            "ERDDAP subset exceeded five redirects".into(),
        ))
    }

    pub async fn download_to_partial<F>(
        &self,
        redirect_chain: &[Url],
        partial: &Path,
        should_cancel: &F,
    ) -> Result<DownloadResult>
    where
        F: Fn() -> bool + Send + Sync,
    {
        if should_cancel() {
            return Err(EcoScopeError::Conflict("materialization cancelled".into()));
        }
        let initial = redirect_chain
            .first()
            .ok_or_else(|| EcoScopeError::Invalid("ERDDAP redirect chain is empty".into()))?;
        let mut successful_response = None;
        for (index, url) in redirect_chain.iter().enumerate() {
            let response = self.client.get(url.clone()).send().await.map_err(|error| {
                EcoScopeError::Internal(format!("ERDDAP download failed: {error}"))
            })?;
            if response.status().is_redirection() {
                let actual = redirect_location(url, response.headers())?;
                let expected = redirect_chain.get(index + 1).ok_or_else(|| {
                    EcoScopeError::Conflict(
                        "ERDDAP returned an unapproved redirect; create a new plan".into(),
                    )
                })?;
                if &actual != expected {
                    return Err(EcoScopeError::Conflict(
                        "ERDDAP redirect changed after approval; create a new plan".into(),
                    ));
                }
                continue;
            }
            if !response.status().is_success() {
                return Err(EcoScopeError::Internal(format!(
                    "ERDDAP download returned {}",
                    response.status()
                )));
            }
            if index + 1 != redirect_chain.len() {
                return Err(EcoScopeError::Conflict(
                    "ERDDAP redirect chain changed after approval; create a new plan".into(),
                ));
            }
            successful_response = Some(response);
        }
        let response = successful_response.ok_or_else(|| {
            EcoScopeError::Conflict("ERDDAP download did not reach an approved endpoint".into())
        })?;
        let metadata = DownloadMetadata {
            etag: header_string(response.headers(), reqwest::header::ETAG),
            last_modified: header_string(response.headers(), reqwest::header::LAST_MODIFIED),
            media_type: header_string(response.headers(), reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.split(';').next().map(str::to_owned)),
        };
        let result = async {
            let mut output = tokio::fs::File::create(partial).await?;
            let mut hasher = blake3::Hasher::new();
            let mut size_bytes = 0_u64;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                if should_cancel() {
                    return Err(EcoScopeError::Conflict("materialization cancelled".into()));
                }
                let chunk = chunk.map_err(|error| {
                    EcoScopeError::Internal(format!("ERDDAP download failed: {error}"))
                })?;
                output.write_all(&chunk).await?;
                hasher.update(&chunk);
                size_bytes = size_bytes.checked_add(chunk.len() as u64).ok_or_else(|| {
                    EcoScopeError::Invalid("ERDDAP download size overflow".into())
                })?;
            }
            output.flush().await?;
            output.sync_all().await?;
            drop(output);
            Ok(DownloadResult {
                digest: hasher.finalize().to_hex().to_string(),
                size_bytes,
                metadata,
                final_url: redirect_chain
                    .last()
                    .cloned()
                    .unwrap_or_else(|| initial.clone()),
            })
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(partial).await;
        }
        result
    }

    pub fn info_url(&self, dataset_id: &str) -> Result<Url> {
        self.endpoint(&["info", dataset_id, "index.html"])
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.base_url.clone();
        url.set_query(None);
        url.set_fragment(None);
        let mut path = url
            .path_segments_mut()
            .map_err(|_| EcoScopeError::Invalid("ERDDAP base URL cannot be a base".into()))?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
        drop(path);
        Ok(url)
    }

    async fn get_json(&self, url: Url) -> Result<Value> {
        let response =
            self.client.get(url.clone()).send().await.map_err(|error| {
                EcoScopeError::Internal(format!("ERDDAP request failed: {error}"))
            })?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Err(EcoScopeError::NotFound(url.to_string()));
        }
        if !status.is_success() {
            return Err(EcoScopeError::Internal(format!(
                "ERDDAP returned {status} for {url}"
            )));
        }
        if response
            .content_length()
            .is_some_and(|size| size > self.metadata_limit_bytes as u64)
        {
            return Err(EcoScopeError::Invalid(format!(
                "ERDDAP metadata exceeds the {} byte limit",
                self.metadata_limit_bytes
            )));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                EcoScopeError::Internal(format!("ERDDAP response failed: {error}"))
            })?;
            if body
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > self.metadata_limit_bytes)
            {
                return Err(EcoScopeError::Invalid(format!(
                    "ERDDAP metadata exceeds the {} byte limit",
                    self.metadata_limit_bytes
                )));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(EcoScopeError::from)
    }
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn redirect_location(current: &Url, headers: &reqwest::header::HeaderMap) -> Result<Url> {
    let location = headers
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| EcoScopeError::Invalid("ERDDAP redirect omitted Location".into()))?;
    current
        .join(location)
        .map_err(|error| EcoScopeError::Invalid(format!("invalid ERDDAP redirect: {error}")))
}

pub fn validate_redirect_target(initial: &Url, target: &Url) -> Result<()> {
    if !target.username().is_empty() || target.password().is_some() {
        return Err(EcoScopeError::Invalid(
            "ERDDAP redirect must not contain credentials".into(),
        ));
    }
    if !secure_or_loopback(target) {
        return Err(EcoScopeError::Invalid(
            "ERDDAP redirect must use HTTPS".into(),
        ));
    }
    if target.path() != initial.path() || target.query() != initial.query() {
        return Err(EcoScopeError::Invalid(
            "ERDDAP redirect changed the approved subset path or query".into(),
        ));
    }
    Ok(())
}

fn validate_base_url(url: &Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EcoScopeError::Invalid(
            "ERDDAP base URL must not contain credentials".into(),
        ));
    }
    if !secure_or_loopback(url) {
        return Err(EcoScopeError::Invalid(
            "ERDDAP requires HTTPS; HTTP is allowed only for loopback tests".into(),
        ));
    }
    if !url.path().trim_end_matches('/').ends_with("/erddap") {
        return Err(EcoScopeError::Invalid(
            "ERDDAP base URL must end with /erddap".into(),
        ));
    }
    Ok(())
}

fn secure_or_loopback(url: &Url) -> bool {
    let secure = url.scheme() == "https";
    let loopback = url.scheme() == "http"
        && match url.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            None => false,
        };
    secure || loopback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn federation_redirects_preserve_the_approved_subset() {
        let initial = Url::parse(
            "https://erddap.example/erddap/tabledap/data.csv?time%2CTEMP%26time%3E%3D2025-01-01",
        )
        .unwrap();
        let regional = Url::parse(
            "https://regional.example/erddap/tabledap/data.csv?time%2CTEMP%26time%3E%3D2025-01-01",
        )
        .unwrap();
        validate_redirect_target(&initial, &regional).unwrap();

        for rejected in [
            "http://regional.example/erddap/tabledap/data.csv?time%2CTEMP%26time%3E%3D2025-01-01",
            "https://user:secret@regional.example/erddap/tabledap/data.csv?time%2CTEMP%26time%3E%3D2025-01-01",
            "https://regional.example/erddap/tabledap/other.csv?time%2CTEMP%26time%3E%3D2025-01-01",
            "https://regional.example/erddap/tabledap/data.csv?time%2CTEMP%26time%3E%3D2026-01-01",
        ] {
            assert!(
                validate_redirect_target(&initial, &Url::parse(rejected).unwrap()).is_err(),
                "accepted unsafe redirect {rejected}"
            );
        }
    }
}
