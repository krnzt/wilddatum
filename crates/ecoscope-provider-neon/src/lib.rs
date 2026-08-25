//! Typed boundary around the NEON Data API.

use std::{collections::BTreeMap, path::PathBuf};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use ecoscope_core::{
    AssetId, CatalogEntry, CatalogQuery, Checksum, CitationMetadata, CredentialRef, DatasetId,
    DatasetManifest, DatasetPlan, DatasetRequest, EcoScopeError, LicenseMetadata, Modality,
    PlannedFile, ProviderCapability, ProviderKind, ProviderManifest, ProviderStatus, ResourceKind,
    ResourceRecord, Result, SourceFile,
};
use ecoscope_provider_api::EcologicalDataProvider;
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;

const DEFAULT_BASE_URL: &str = "https://data.neonscience.org/api/v0";

#[derive(Clone)]
pub struct NeonProvider {
    client: Client,
    base_url: String,
    token: Option<String>,
    object_dir: Option<PathBuf>,
}

impl NeonProvider {
    pub fn new(token: Option<String>) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("ecoscope/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| EcoScopeError::Internal(error.to_string()))?;
        Ok(Self {
            client,
            base_url: DEFAULT_BASE_URL.into(),
            token,
            object_dir: None,
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    pub fn with_object_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.object_dir = Some(path.into());
        self
    }

    fn authenticated(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            request.header("X-API-Token", token)
        } else {
            request
        }
    }

    async fn json(&self, request: reqwest::RequestBuilder) -> Result<Value> {
        let response =
            self.authenticated(request).send().await.map_err(|error| {
                EcoScopeError::Internal(format!("NEON request failed: {error}"))
            })?;
        let status = response.status();
        let rate_remaining = response
            .headers()
            .get("X-RateLimit-Remaining")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response
            .text()
            .await
            .map_err(|error| EcoScopeError::Internal(error.to_string()))?;
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(EcoScopeError::CredentialsRequired("neon".into()));
        }
        if !status.is_success() {
            return Err(EcoScopeError::Internal(format!(
                "NEON returned {status}: {}",
                text.chars().take(500).collect::<String>()
            )));
        }
        if rate_remaining.as_deref() == Some("0") {
            tracing::warn!("NEON rate limit exhausted by successful request");
        }
        serde_json::from_str(&text).map_err(EcoScopeError::from)
    }

    async fn product_value(&self, product_code: &str) -> Result<Value> {
        let value = self
            .json(
                self.client
                    .get(format!("{}/products/{product_code}", self.base_url)),
            )
            .await?;
        value
            .get("data")
            .cloned()
            .ok_or_else(|| EcoScopeError::NotFound(product_code.into()))
    }

    fn product_entry(value: &Value) -> Option<CatalogEntry> {
        let code = value.get("productCode")?.as_str()?.to_owned();
        let name = value
            .get("productName")
            .and_then(Value::as_str)
            .unwrap_or(&code)
            .to_owned();
        let description = value
            .get("productDescription")
            .or_else(|| value.get("productAbstract"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let searchable = format!("{} {}", name, description.as_deref().unwrap_or_default());
        let site_records = value
            .get("siteCodes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut sites = site_records
            .iter()
            .filter_map(|item| {
                item.as_str().or_else(|| {
                    item.as_object()
                        .and_then(|object| string_field(object, &["siteCode", "site"]))
                })
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        sites.sort();
        sites.dedup();
        let mut available_months = value
            .get("availableMonths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(
                site_records
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(|object| object.get("availableMonths"))
                    .filter_map(Value::as_array)
                    .flatten()
                    .filter_map(Value::as_str),
            )
            .map(str::to_owned)
            .collect::<Vec<_>>();
        available_months.sort();
        available_months.dedup();
        let mut metadata = BTreeMap::new();
        for key in [
            "productStatus",
            "productCategory",
            "productScienceTeam",
            "productPublicationFormatType",
            "productHasExpanded",
        ] {
            if let Some(field) = value.get(key) {
                metadata.insert(key.to_owned(), field.clone());
            }
        }
        Some(CatalogEntry {
            provider: ProviderKind::Neon,
            id: code,
            name,
            description,
            modalities: infer_modalities(&searchable),
            sites,
            date_start: available_months.first().cloned(),
            date_end: available_months.last().cloned(),
            metadata,
        })
    }

    async fn exact_plan(&self, request: &DatasetRequest) -> Result<Vec<PlannedFile>> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Query<'a> {
            product_code: &'a str,
            site_codes: &'a [String],
            start_date_month: &'a str,
            end_date_month: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            release: &'a Option<String>,
            package: &'a str,
            include_provisional: bool,
        }

        let start = request
            .start_month
            .as_deref()
            .ok_or_else(|| EcoScopeError::Invalid("start_month is required for NEON".into()))?;
        let end = request
            .end_month
            .as_deref()
            .ok_or_else(|| EcoScopeError::Invalid("end_month is required for NEON".into()))?;
        if request.sites.is_empty() {
            return Err(EcoScopeError::Invalid(
                "at least one NEON site is required".into(),
            ));
        }
        let value = self
            .json(
                self.client
                    .post(format!("{}/data/query", self.base_url))
                    .json(&Query {
                        product_code: &request.product_code,
                        site_codes: &request.sites,
                        start_date_month: start,
                        end_date_month: end,
                        release: &request.release,
                        package: &request.package,
                        include_provisional: request.include_provisional,
                    }),
            )
            .await?;
        let mut files = Vec::new();
        collect_files(&value, None, None, &mut files);
        files.sort_by(|left, right| left.name.cmp(&right.name));
        files.dedup_by(|left, right| left.download_url == right.download_url);
        Ok(files)
    }

    #[cfg(test)]
    async fn download_file(&self, file: &PlannedFile) -> Result<SourceFile> {
        self.download_file_with_cancel(file, &|| false).await
    }

    async fn download_file_with_cancel<F>(
        &self,
        file: &PlannedFile,
        should_cancel: &F,
    ) -> Result<SourceFile>
    where
        F: Fn() -> bool + Send + Sync,
    {
        if should_cancel() {
            return Err(EcoScopeError::Conflict("materialization cancelled".into()));
        }
        let url = file
            .download_url
            .as_deref()
            .ok_or_else(|| EcoScopeError::Invalid(format!("{} has no download URL", file.name)))?;
        let response = self
            .authenticated(self.client.get(url))
            .send()
            .await
            .map_err(|error| EcoScopeError::Internal(format!("download failed: {error}")))?;
        if !response.status().is_success() {
            return Err(EcoScopeError::Internal(format!(
                "download of {} returned {}",
                file.name,
                response.status()
            )));
        }
        let object_dir = self.object_dir.as_ref().ok_or_else(|| {
            EcoScopeError::Internal("NEON object directory was not configured".into())
        })?;
        tokio::fs::create_dir_all(object_dir).await?;
        let asset_id = AssetId::new();
        let temporary = object_dir.join(format!(".{}.partial", asset_id.0));
        let mut output = tokio::fs::File::create(&temporary).await?;
        let mut blake3 = blake3::Hasher::new();
        let mut md5 = md5::Context::new();
        let mut crc32c = 0_u32;
        let mut size_bytes = 0_u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if should_cancel() {
                drop(output);
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(EcoScopeError::Conflict("materialization cancelled".into()));
            }
            let chunk = chunk
                .map_err(|error| EcoScopeError::Internal(format!("download failed: {error}")))?;
            output.write_all(&chunk).await?;
            blake3.update(&chunk);
            md5.consume(&chunk);
            crc32c = crc32c::crc32c_append(crc32c, &chunk);
            size_bytes += chunk.len() as u64;
        }
        output.flush().await?;
        output.sync_all().await?;
        drop(output);

        let validation = if let Some(expected_size) = file.size_bytes
            && size_bytes != expected_size
        {
            Err(EcoScopeError::Invalid(format!(
                "downloaded {size_bytes} bytes for {}, expected {expected_size}",
                file.name
            )))
        } else if let Some(checksum) = &file.checksum {
            verify_provider_digests(
                &file.name,
                checksum,
                &format!("{:x}", md5.finalize()),
                crc32c,
            )
        } else {
            Ok(())
        };
        if let Err(error) = validation {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }

        let digest = blake3.finalize().to_hex().to_string();
        let destination = object_dir.join(&digest);
        if tokio::fs::metadata(&destination).await.is_ok() {
            tokio::fs::remove_file(&temporary).await?;
        } else {
            tokio::fs::rename(&temporary, &destination).await?;
        }
        let mut metadata = BTreeMap::new();
        if let Some(checksum) = &file.checksum {
            metadata.insert("provider_checksum".into(), serde_json::to_value(checksum)?);
        }
        Ok(SourceFile {
            asset_id,
            original_name: file.name.clone(),
            source_uri: format!("neon://{}", file.provider_id),
            local_object: Some(digest.clone()),
            size_bytes,
            checksum: Checksum {
                algorithm: "blake3".into(),
                value: digest,
            },
            media_type: None,
            site: file.site.clone(),
            month: file.month.clone(),
            metadata,
        })
    }

    /// Materialize with cooperative cancellation and file-level progress.
    /// Partial objects are deleted before a cancellation result is returned.
    pub async fn materialize_with_control<F, P>(
        &self,
        plan: DatasetPlan,
        credentials: Option<CredentialRef>,
        should_cancel: F,
        on_progress: P,
    ) -> Result<DatasetManifest>
    where
        F: Fn() -> bool + Send + Sync,
        P: Fn(usize, usize) + Send + Sync,
    {
        if self.token.is_none() || credentials.is_none() {
            return Err(EcoScopeError::CredentialsRequired("neon".into()));
        }
        if plan.approved_at.is_none() {
            return Err(EcoScopeError::Invalid(
                "the exact dataset plan must be approved before materialization".into(),
            ));
        }
        let total = plan.files.len();
        let mut source_files = Vec::with_capacity(total);
        for (index, file) in plan.files.iter().enumerate() {
            if should_cancel() {
                return Err(EcoScopeError::Conflict("materialization cancelled".into()));
            }
            source_files.push(self.download_file_with_cancel(file, &should_cancel).await?);
            on_progress(index + 1, total);
        }
        if should_cancel() {
            return Err(EcoScopeError::Conflict("materialization cancelled".into()));
        }
        let modalities = self
            .inspect_product(&plan.request.product_code)
            .await
            .map(|product| product.modalities)
            .unwrap_or_else(|_| vec![Modality::Unknown]);
        Ok(DatasetManifest {
            dataset_id: DatasetId::new(),
            provider: ProviderKind::Neon,
            product_code: plan.request.product_code.clone(),
            product_revision: None,
            modalities,
            sites: plan.request.sites.clone(),
            start_month: plan.request.start_month.clone(),
            end_month: plan.request.end_month.clone(),
            release: plan.request.release.clone(),
            package: Some(plan.request.package.clone()),
            include_provisional: plan.request.include_provisional,
            source_files,
            transformations: vec![],
            format: None,
            spatial_reference: None,
            cube: None,
            cubes: vec![],
            license: Some(LicenseMetadata {
                name: "CC BY 4.0".into(),
                url: Some("https://creativecommons.org/licenses/by/4.0/".into()),
                attribution_required: true,
            }),
            citation: Some(CitationMetadata {
                text: format!(
                    "National Ecological Observatory Network (NEON), {}{}",
                    plan.request.product_code,
                    plan.request
                        .release
                        .as_deref()
                        .map(|release| format!(", {release}"))
                        .unwrap_or_default()
                ),
                doi: None,
                url: Some(format!(
                    "https://data.neonscience.org/data-products/{}",
                    plan.request.product_code
                )),
            }),
            created_at: Utc::now(),
        })
    }
}

fn verify_provider_digests(
    name: &str,
    expected: &Checksum,
    md5_hex: &str,
    crc32c: u32,
) -> Result<()> {
    let matches = match expected.algorithm.to_ascii_lowercase().as_str() {
        "md5" => md5_hex.eq_ignore_ascii_case(&expected.value),
        "crc32c" => {
            let expected_value = expected.value.trim();
            format!("{crc32c:08x}").eq_ignore_ascii_case(expected_value)
                || crc32c.to_string() == expected_value
                || BASE64.encode(crc32c.to_be_bytes()) == expected_value
        }
        algorithm => {
            return Err(EcoScopeError::Invalid(format!(
                "cannot verify {name}: unsupported provider checksum {algorithm}"
            )));
        }
    };
    if matches {
        Ok(())
    } else {
        Err(EcoScopeError::Invalid(format!(
            "provider checksum mismatch for {name}"
        )))
    }
}

#[async_trait]
impl EcologicalDataProvider for NeonProvider {
    fn provider_id(&self) -> &str {
        "neon"
    }

    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            schema_version: 1,
            provider_id: "neon".into(),
            name: "NSF National Ecological Observatory Network".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            status: ProviderStatus::BuiltIn,
            capabilities: vec![
                ProviderCapability::CatalogSearch,
                ProviderCapability::ResourceResolve,
                ProviderCapability::AssetPlan,
                ProviderCapability::AssetFetch,
                ProviderCapability::CitationResolve,
                ProviderCapability::PolicyEvaluate,
            ],
            allowed_network_origins: vec![
                "https://data.neonscience.org".into(),
                "https://storage.googleapis.com".into(),
            ],
            authentication: vec!["api_token_header".into()],
            standards: vec![
                "EML 2.2".into(),
                "ISO 19115-2".into(),
                "DataCite".into(),
                "CC BY 4.0".into(),
            ],
            homepage: Some("https://www.neonscience.org/".into()),
            support_url: Some("https://www.neonscience.org/about/contact-us".into()),
        }
    }

    async fn search_catalog(&self, query: CatalogQuery) -> Result<Vec<CatalogEntry>> {
        let value = self
            .json(self.client.get(format!("{}/products", self.base_url)))
            .await?;
        let products = value
            .get("data")
            .and_then(|data| data.get("products").or(Some(data)))
            .and_then(Value::as_array)
            .ok_or_else(|| EcoScopeError::Internal("unexpected NEON products response".into()))?;
        let needle = query.text.to_ascii_lowercase();
        let mut entries = products
            .iter()
            .filter_map(Self::product_entry)
            .filter(|entry| {
                let modality_text = entry
                    .modalities
                    .iter()
                    .map(|modality| format!("{modality:?}"))
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_ascii_lowercase();
                (needle.is_empty()
                    || entry.id.to_ascii_lowercase().contains(&needle)
                    || entry.name.to_ascii_lowercase().contains(&needle)
                    || entry
                        .description
                        .as_deref()
                        .unwrap_or_default()
                        .to_ascii_lowercase()
                        .contains(&needle)
                    || modality_text.contains(&needle))
                    && (query.modalities.is_empty()
                        || query
                            .modalities
                            .iter()
                            .any(|modality| entry.modalities.contains(modality)))
                    && query.sites.iter().all(|site| {
                        entry
                            .sites
                            .iter()
                            .any(|available| available.eq_ignore_ascii_case(site))
                    })
                    && query.start_month.as_ref().is_none_or(|start| {
                        entry
                            .date_end
                            .as_ref()
                            .is_some_and(|available_end| available_end >= start)
                    })
                    && query.end_month.as_ref().is_none_or(|end| {
                        entry
                            .date_start
                            .as_ref()
                            .is_some_and(|available_start| available_start <= end)
                    })
            })
            .take(query.limit.min(100) as usize)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(entries)
    }

    async fn inspect_product(&self, id: &str) -> Result<CatalogEntry> {
        let value = self.product_value(id).await?;
        Self::product_entry(&value).ok_or_else(|| EcoScopeError::NotFound(id.into()))
    }

    async fn resolve_resource(&self, id: &str) -> Result<ResourceRecord> {
        let raw = self.product_value(id).await?;
        let entry = Self::product_entry(&raw).ok_or_else(|| EcoScopeError::NotFound(id.into()))?;
        let mut record = ResourceRecord::from(entry);
        record.kind = ResourceKind::Collection;
        record
            .identifiers
            .insert("neon_product_code".into(), id.into());
        record.raw_metadata = Some(raw);
        Ok(record)
    }

    async fn plan_dataset(&self, request: DatasetRequest) -> Result<DatasetPlan> {
        let (files, mut warnings) = if self.token.is_some() {
            (self.exact_plan(&request).await?, Vec::new())
        } else {
            (
                Vec::new(),
                vec![
                    "Connect a NEON API token to resolve the exact file list and byte estimate"
                        .into(),
                ],
            )
        };
        if request.release.is_none() {
            warnings.push(
                "No fixed release selected; choose a release before approval for reproducibility"
                    .into(),
            );
        }
        let estimated_bytes = if files.is_empty() {
            None
        } else {
            Some(files.iter().filter_map(|file| file.size_bytes).sum())
        };
        DatasetPlan {
            plan_id: ecoscope_core::PlanId::new(),
            request,
            plan_hash: String::new(),
            file_count: files.len() as u64,
            estimated_bytes,
            files,
            warnings,
            requires_credentials: true,
            created_at: Utc::now(),
            approved_at: None,
        }
        .finalize()
    }

    async fn materialize(
        &self,
        plan: DatasetPlan,
        credentials: Option<CredentialRef>,
    ) -> Result<DatasetManifest> {
        self.materialize_with_control(plan, credentials, || false, |_, _| {})
            .await
    }
}

fn infer_modalities(text: &str) -> Vec<Modality> {
    let text = text.to_ascii_lowercase();
    let mut modalities = Vec::new();
    if text.contains("hyperspectral") || text.contains("spectrometer") {
        modalities.extend([Modality::Hyperspectral, Modality::Raster]);
    }
    if text.contains("lidar") || text.contains("point cloud") {
        modalities.push(Modality::PointCloud);
    }
    if text.contains("geotiff") || text.contains("raster") || text.contains("elevation") {
        modalities.push(Modality::Raster);
    }
    if text.contains("time series") || text.contains("sensor") || text.contains("temperature") {
        modalities.extend([Modality::Tabular, Modality::TimeSeries]);
    }
    if modalities.is_empty() {
        modalities.push(Modality::Tabular);
    }
    modalities.sort_by_key(|modality| format!("{modality:?}"));
    modalities.dedup();
    modalities
}

fn collect_files(
    value: &Value,
    inherited_site: Option<&str>,
    inherited_month: Option<&str>,
    output: &mut Vec<PlannedFile>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_files(item, inherited_site, inherited_month, output);
            }
        }
        Value::Object(object) => {
            let site = string_field(object, &["siteCode", "site"])
                .or(inherited_site)
                .map(str::to_owned);
            let month = string_field(object, &["month", "dateMonth", "startDateMonth"])
                .or(inherited_month)
                .map(str::to_owned);
            if let (Some(name), Some(url)) = (
                string_field(object, &["name", "fileName"]),
                string_field(object, &["url", "downloadUrl"]),
            ) {
                let checksum = string_field(object, &["md5"])
                    .map(|value| Checksum {
                        algorithm: "md5".into(),
                        value: value.into(),
                    })
                    .or_else(|| {
                        string_field(object, &["crc32c"]).map(|value| Checksum {
                            algorithm: "crc32c".into(),
                            value: value.into(),
                        })
                    });
                output.push(PlannedFile {
                    provider_id: name.into(),
                    name: name.into(),
                    size_bytes: object.get("size").and_then(Value::as_u64),
                    checksum,
                    download_url: Some(url.into()),
                    site,
                    month,
                    expires_at: None,
                });
                return;
            }
            for child in object.values() {
                collect_files(child, site.as_deref(), month.as_deref(), output);
            }
        }
        _ => {}
    }
}

fn string_field<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_multimodal_products() {
        assert!(infer_modalities("hyperspectral raster").contains(&Modality::Hyperspectral));
        assert!(infer_modalities("discrete return lidar").contains(&Modality::PointCloud));
    }

    #[test]
    fn parses_current_product_site_availability_shape() {
        let value = serde_json::json!({
            "productCode": "DP3.30006.002",
            "productName": "Hyperspectral surface directional reflectance",
            "siteCodes": [
                {"siteCode": "HARV", "availableMonths": ["2023-06", "2024-06"]},
                {"siteCode": "ABBY", "availableMonths": ["2022-07"]}
            ]
        });
        let product = NeonProvider::product_entry(&value).unwrap();
        assert_eq!(product.sites, vec!["ABBY", "HARV"]);
        assert_eq!(product.date_start.as_deref(), Some("2022-07"));
        assert_eq!(product.date_end.as_deref(), Some("2024-06"));
        assert!(product.modalities.contains(&Modality::Hyperspectral));
    }

    #[test]
    fn recursively_extracts_files() {
        let value = serde_json::json!({
            "data": {"sites": [{"siteCode": "HARV", "files": [{
                "name": "sample.h5", "size": 42, "md5": "abc", "url": "https://example.test/file"
            }]}]}
        });
        let mut files = vec![];
        collect_files(&value, None, None, &mut files);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].site.as_deref(), Some("HARV"));
    }

    #[test]
    fn verifies_supported_provider_checksums() {
        let bytes = b"hello";
        let md5 = format!("{:x}", md5::compute(bytes));
        let crc = crc32c::crc32c(bytes);
        verify_provider_digests(
            "hello.txt",
            &Checksum {
                algorithm: "md5".into(),
                value: "5d41402abc4b2a76b9719d911017c592".into(),
            },
            &md5,
            crc,
        )
        .unwrap();
        verify_provider_digests(
            "hello.txt",
            &Checksum {
                algorithm: "crc32c".into(),
                value: format!("{crc:08x}"),
            },
            &md5,
            crc,
        )
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires the live NEON Data API"]
    async fn live_catalog_search_returns_multimodal_availability() {
        let provider = NeonProvider::new(None).unwrap();
        let products = provider
            .search_catalog(CatalogQuery {
                text: "hyperspectral".into(),
                modalities: vec![Modality::Hyperspectral],
                sites: vec!["HARV".into()],
                start_month: Some("2023-01".into()),
                end_month: Some("2025-12".into()),
                limit: 10,
            })
            .await
            .unwrap();
        assert!(!products.is_empty());
        assert!(
            products
                .iter()
                .all(|product| product.sites.contains(&"HARV".into()))
        );
        assert!(
            products
                .iter()
                .all(|product| product.modalities.contains(&Modality::Hyperspectral))
        );
    }

    #[tokio::test]
    async fn streams_downloads_into_verified_content_addressed_objects() {
        let bytes = b"multimodal ecological source".to_vec();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/file",
            axum::routing::get({
                let bytes = bytes.clone();
                move || {
                    let bytes = bytes.clone();
                    async move { bytes }
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let provider = NeonProvider::new(None)
            .unwrap()
            .with_object_dir(directory.path());
        let source = provider
            .download_file(&PlannedFile {
                provider_id: "fixture".into(),
                name: "fixture.bin".into(),
                size_bytes: Some(bytes.len() as u64),
                checksum: Some(Checksum {
                    algorithm: "md5".into(),
                    value: format!("{:x}", md5::compute(&bytes)),
                }),
                download_url: Some(format!("http://{address}/file")),
                site: Some("HARV".into()),
                month: Some("2025-01".into()),
                expires_at: None,
            })
            .await
            .unwrap();
        let object = directory.path().join(source.local_object.unwrap());
        assert_eq!(std::fs::read(object).unwrap(), bytes);
        assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".partial")
        }));
        server.abort();
    }
}
