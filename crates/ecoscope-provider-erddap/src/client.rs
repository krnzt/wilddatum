use ecoscope_core::{EcoScopeError, Result};
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use url::{Host, Url};

use crate::table::{InfoMetadata, SearchRecord, parse_info, parse_search};

pub const DEFAULT_METADATA_LIMIT_BYTES: usize = 4 * 1024 * 1024;

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

fn validate_base_url(url: &Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EcoScopeError::Invalid(
            "ERDDAP base URL must not contain credentials".into(),
        ));
    }
    let secure = url.scheme() == "https";
    let loopback = url.scheme() == "http"
        && match url.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            None => false,
        };
    if !secure && !loopback {
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
