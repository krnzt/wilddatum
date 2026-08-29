//! Generic ERDDAP provider with maintained research-infrastructure presets.

use std::{collections::BTreeMap, path::PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use ecoscope_core::{
    AssetId, CatalogEntry, CatalogQuery, Checksum, CitationMetadata, CredentialRef, DatasetId,
    DatasetManifest, DatasetPlan, DatasetRequest, EcoScopeError, FormatDescriptor, GeoGeometry,
    LicenseMetadata, Modality, PlanId, PlannedFile, ProviderCapability, ProviderKind,
    ProviderManifest, ProviderStatus, ResourceKind, ResourceQuery, ResourceRecord, Result,
    SourceFile, Transformation,
};
use ecoscope_provider_api::{EcologicalDataProvider, PROVIDER_PROTOCOL_VERSION};
use serde_json::{Value, json};

use crate::{
    client::{DownloadMetadata, ErddapClient, validate_redirect_target},
    config::ErddapConfig,
    query::{Constraint, ErddapOptions, Protocol, build_subset},
    table::{InfoMetadata, SearchRecord},
};

pub mod client;
pub mod config;
pub mod query;
pub mod table;

#[derive(Clone)]
pub struct ErddapProvider {
    config: ErddapConfig,
    client: ErddapClient,
    object_dir: Option<PathBuf>,
}

impl ErddapProvider {
    pub fn new(config: ErddapConfig) -> Result<Self> {
        if config.provider_id.is_empty()
            || !config
                .provider_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(EcoScopeError::Invalid(
                "ERDDAP provider_id must contain lowercase ASCII letters, digits, and hyphens"
                    .into(),
            ));
        }
        let client = ErddapClient::new(&config.base_url)?;
        Ok(Self {
            config,
            client,
            object_dir: None,
        })
    }

    pub fn with_metadata_limit_bytes(mut self, limit: usize) -> Self {
        self.client = self.client.with_metadata_limit_bytes(limit);
        self
    }

    pub fn with_object_dir(mut self, object_dir: impl Into<PathBuf>) -> Self {
        self.object_dir = Some(object_dir.into());
        self
    }

    async fn search_records(&self, text: &str, limit: u32) -> Result<Vec<SearchRecord>> {
        let query = match (&self.config.catalog_scope, text.trim()) {
            (Some(scope), "") => scope.clone(),
            (Some(scope), text) => format!("{scope} {text}"),
            (None, text) => text.to_owned(),
        };
        self.client.search(&query, limit).await
    }

    fn search_resource(&self, record: SearchRecord) -> ResourceRecord {
        let mut provider_extensions = BTreeMap::new();
        insert_optional(
            &mut provider_extensions,
            "institution",
            record.institution.clone(),
        );
        insert_optional(
            &mut provider_extensions,
            "tabledap_url",
            record.tabledap_url.clone(),
        );
        insert_optional(
            &mut provider_extensions,
            "griddap_url",
            record.griddap_url.clone(),
        );
        provider_extensions.insert("accessible".into(), Value::String(record.accessible));
        ResourceRecord {
            provider_id: self.config.provider_id.clone(),
            resource_id: record.dataset_id.clone(),
            kind: ResourceKind::Collection,
            name: record.title,
            description: record.summary,
            modalities: if record.griddap_url.is_some() {
                vec![Modality::Raster, Modality::Tensor]
            } else {
                vec![Modality::Tabular]
            },
            spatial_extent: None,
            temporal_start: None,
            temporal_end: None,
            relations: Vec::new(),
            identifiers: BTreeMap::from([("erddap_dataset_id".into(), record.dataset_id)]),
            vocabulary_terms: BTreeMap::new(),
            provider_extensions,
            raw_metadata: None,
        }
    }

    fn resolved_resource(
        &self,
        record: SearchRecord,
        info: InfoMetadata,
    ) -> Result<ResourceRecord> {
        let mut resource = self.search_resource(record);
        let cdm_data_type = info
            .globals
            .get("cdm_data_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        resource.modalities = modalities(cdm_data_type);
        resource.temporal_start = string_attribute(&info.globals, "time_coverage_start");
        resource.temporal_end = string_attribute(&info.globals, "time_coverage_end");
        resource.spatial_extent = coverage_polygon(&info.globals);
        for (name, value) in &info.globals {
            resource
                .provider_extensions
                .insert(name.clone(), value.clone());
        }
        let variables = info
            .variables
            .iter()
            .map(|(name, variable)| {
                (
                    name.clone(),
                    json!({
                        "data_type": variable.data_type,
                        "attributes": variable.attributes,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        resource
            .provider_extensions
            .insert("variables".into(), Value::Object(variables));
        resource.provider_extensions.insert(
            "info_url".into(),
            Value::String(self.client.info_url(&resource.resource_id)?.to_string()),
        );
        resource.raw_metadata = Some(info.raw_metadata);
        Ok(resource)
    }

    async fn download_file_with_cancel<F>(
        &self,
        file: &PlannedFile,
        should_cancel: &F,
    ) -> Result<(SourceFile, DownloadMetadata)>
    where
        F: Fn() -> bool + Send + Sync,
    {
        if file.provider_id != self.config.provider_id {
            return Err(EcoScopeError::Invalid(format!(
                "planned file belongs to provider {}, expected {}",
                file.provider_id, self.config.provider_id
            )));
        }
        let download_url = file
            .download_url
            .as_deref()
            .ok_or_else(|| EcoScopeError::Invalid(format!("{} has no download URL", file.name)))?;
        let url = url::Url::parse(download_url)
            .map_err(|error| EcoScopeError::Invalid(format!("invalid download URL: {error}")))?;
        self.validate_download_url(&url)?;
        let redirect_chain = planned_redirect_chain(file, &url)?;
        let object_dir = self.object_dir.as_ref().ok_or_else(|| {
            EcoScopeError::Internal("ERDDAP object directory was not configured".into())
        })?;
        tokio::fs::create_dir_all(object_dir).await?;
        let asset_id = AssetId::new();
        let partial = object_dir.join(format!(".partial-{}", asset_id.0));
        let downloaded = self
            .client
            .download_to_partial(&redirect_chain, &partial, should_cancel)
            .await?;
        let destination = object_dir.join(&downloaded.digest);
        let stored = if tokio::fs::metadata(&destination).await.is_ok() {
            tokio::fs::remove_file(&partial).await?;
            Ok(())
        } else {
            tokio::fs::rename(&partial, &destination).await
        };
        if let Err(error) = stored {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(EcoScopeError::Io(error));
        }
        let mut metadata = file.metadata.clone();
        insert_optional(
            &mut metadata,
            "response_etag",
            downloaded.metadata.etag.clone(),
        );
        insert_optional(
            &mut metadata,
            "response_last_modified",
            downloaded.metadata.last_modified.clone(),
        );
        let source = SourceFile {
            asset_id,
            original_name: file.name.clone(),
            source_uri: downloaded.final_url.to_string(),
            local_object: Some(downloaded.digest.clone()),
            size_bytes: downloaded.size_bytes,
            checksum: Checksum {
                algorithm: "blake3".into(),
                value: downloaded.digest,
            },
            media_type: downloaded.metadata.media_type.clone(),
            location: file.location.clone(),
            temporal_partition: file.temporal_partition.clone(),
            metadata,
        };
        Ok((source, downloaded.metadata))
    }

    fn validate_download_url(&self, url: &url::Url) -> Result<()> {
        if url.origin().ascii_serialization() != self.config.allowed_origin {
            return Err(EcoScopeError::Invalid(format!(
                "ERDDAP download origin is not allowed: {}",
                url.origin().ascii_serialization()
            )));
        }
        let base_path = self.client.base_url().path().trim_end_matches('/');
        if !url.path().starts_with(&format!("{base_path}/tabledap/"))
            && !url.path().starts_with(&format!("{base_path}/griddap/"))
        {
            return Err(EcoScopeError::Invalid(
                "ERDDAP download URL is outside the tabledap/griddap surfaces".into(),
            ));
        }
        Ok(())
    }

    /// Materialize with cooperative cancellation and per-file progress.
    pub async fn materialize_with_control<F, P>(
        &self,
        plan: DatasetPlan,
        should_cancel: F,
        on_progress: P,
    ) -> Result<DatasetManifest>
    where
        F: Fn() -> bool + Send + Sync,
        P: Fn(usize, usize) + Send + Sync,
    {
        if plan.approved_at.is_none() {
            return Err(EcoScopeError::Invalid(
                "the exact dataset plan must be approved before materialization".into(),
            ));
        }
        if plan.clone().finalize()?.plan_hash != plan.plan_hash {
            return Err(EcoScopeError::Conflict(
                "dataset plan changed after it was finalized".into(),
            ));
        }
        if plan.file_count != plan.files.len() as u64 || plan.files.is_empty() {
            return Err(EcoScopeError::Invalid(
                "ERDDAP plan file count is invalid".into(),
            ));
        }
        let search_record = self
            .search_records(&plan.request.resource_id, 100)
            .await?
            .into_iter()
            .find(|record| record.dataset_id == plan.request.resource_id)
            .ok_or_else(|| EcoScopeError::NotFound(plan.request.resource_id.clone()))?;
        let info = self.client.info(&plan.request.resource_id).await?;
        let server_version = self
            .client
            .server_version()
            .await
            .unwrap_or_else(|_| "unknown".into());
        let total = plan.files.len();
        let mut source_files = Vec::with_capacity(total);
        let mut response_metadata = Vec::with_capacity(total);
        for (index, file) in plan.files.iter().enumerate() {
            if should_cancel() {
                return Err(EcoScopeError::Conflict("materialization cancelled".into()));
            }
            let (source, metadata) = self.download_file_with_cancel(file, &should_cancel).await?;
            source_files.push(source);
            response_metadata.push(metadata);
            on_progress(index + 1, total);
        }
        let accessed_at = Utc::now();
        let globals = info.globals.clone();
        let info_url = self.client.info_url(&plan.request.resource_id)?.to_string();
        let license = license_metadata(&globals);
        let citation = citation_metadata(&search_record, &globals, &info_url);
        let first_response = response_metadata.first();
        let provider_metadata = BTreeMap::from([
            ("accessed_at".into(), json!(accessed_at)),
            ("server_version".into(), json!(server_version)),
            (
                "cdm_data_type".into(),
                globals.get("cdm_data_type").cloned().unwrap_or(Value::Null),
            ),
            ("global_attributes".into(), json!(globals)),
            ("info_url".into(), json!(info_url)),
            (
                "response_etag".into(),
                json!(first_response.and_then(|metadata| metadata.etag.clone())),
            ),
            (
                "response_last_modified".into(),
                json!(first_response.and_then(|metadata| metadata.last_modified.clone())),
            ),
        ]);
        let output_format = source_files
            .first()
            .and_then(|source| source.original_name.rsplit_once('.').map(|(_, ext)| ext))
            .unwrap_or("unknown")
            .to_owned();
        Ok(DatasetManifest {
            dataset_id: DatasetId::new(),
            provider: ProviderKind::Other(self.config.provider_id.clone()),
            resource_id: plan.request.resource_id.clone(),
            resource_version: None,
            modalities: modalities(
                globals
                    .get("cdm_data_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            locations: plan.request.locations.clone(),
            temporal_start: plan.request.temporal_start.clone(),
            temporal_end: plan.request.temporal_end.clone(),
            release: plan.request.release.clone(),
            package: Some(output_format.clone()),
            include_provisional: plan.request.include_provisional,
            source_files,
            transformations: vec![Transformation {
                name: "erddap_subset".into(),
                version: server_version,
                parameters: json!({
                    "request": plan.request,
                    "download_url": plan.files[0].download_url,
                    "response_etag": first_response.and_then(|metadata| metadata.etag.clone()),
                    "response_last_modified": first_response.and_then(|metadata| metadata.last_modified.clone()),
                }),
                created_at: accessed_at,
            }],
            format: Some(FormatDescriptor {
                name: output_format,
                version: None,
                profile: None,
                options: BTreeMap::new(),
            }),
            spatial_reference: None,
            cube: None,
            cubes: Vec::new(),
            license,
            citation,
            provider_metadata,
            created_at: accessed_at,
        })
    }
}

#[async_trait]
impl EcologicalDataProvider for ErddapProvider {
    fn provider_id(&self) -> &str {
        &self.config.provider_id
    }

    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            schema_version: PROVIDER_PROTOCOL_VERSION,
            provider_id: self.config.provider_id.clone(),
            name: self.config.name.clone(),
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
            allowed_network_origins: vec![self.config.allowed_origin.clone()],
            authentication: Vec::new(),
            standards: vec![
                "ERDDAP REST".into(),
                "Climate and Forecast Conventions".into(),
                "Attribute Convention for Data Discovery".into(),
            ],
            homepage: Some(self.config.homepage.clone()),
            support_url: None,
        }
    }

    async fn search_catalog(&self, query: CatalogQuery) -> Result<Vec<CatalogEntry>> {
        self.search_resources(ResourceQuery {
            text: query.text,
            kinds: Vec::new(),
            modalities: query.modalities,
            spatial_filter: None,
            temporal_start: query.start_month,
            temporal_end: query.end_month,
            provider_filters: BTreeMap::from([("sites".into(), json!(query.sites))]),
            limit: query.limit,
        })
        .await
        .map(|resources| {
            resources
                .into_iter()
                .map(|resource| CatalogEntry {
                    provider: ProviderKind::Other(resource.provider_id),
                    id: resource.resource_id,
                    name: resource.name,
                    description: resource.description,
                    modalities: resource.modalities,
                    sites: Vec::new(),
                    date_start: resource.temporal_start,
                    date_end: resource.temporal_end,
                    metadata: resource.provider_extensions,
                })
                .collect()
        })
    }

    async fn inspect_product(&self, id: &str) -> Result<CatalogEntry> {
        let resource = self.resolve_resource(id).await?;
        Ok(CatalogEntry {
            provider: ProviderKind::Other(resource.provider_id),
            id: resource.resource_id,
            name: resource.name,
            description: resource.description,
            modalities: resource.modalities,
            sites: Vec::new(),
            date_start: resource.temporal_start,
            date_end: resource.temporal_end,
            metadata: resource.provider_extensions,
        })
    }

    async fn search_resources(&self, query: ResourceQuery) -> Result<Vec<ResourceRecord>> {
        if !query.kinds.is_empty() && !query.kinds.contains(&ResourceKind::Collection) {
            return Ok(Vec::new());
        }
        let records = self.search_records(&query.text, query.limit).await?;
        Ok(records
            .into_iter()
            .map(|record| self.search_resource(record))
            .filter(|resource| {
                query.modalities.is_empty()
                    || query
                        .modalities
                        .iter()
                        .any(|modality| resource.modalities.contains(modality))
            })
            .take(query.limit as usize)
            .collect())
    }

    async fn resolve_resource(&self, id: &str) -> Result<ResourceRecord> {
        let record = self
            .search_records(id, 100)
            .await?
            .into_iter()
            .find(|record| record.dataset_id == id)
            .ok_or_else(|| EcoScopeError::NotFound(id.into()))?;
        let info = self.client.info(id).await?;
        self.resolved_resource(record, info)
    }

    async fn plan_dataset(&self, request: DatasetRequest) -> Result<DatasetPlan> {
        if request.provider != ProviderKind::Other(self.config.provider_id.clone()) {
            return Err(EcoScopeError::Invalid(format!(
                "request provider does not match {}",
                self.config.provider_id
            )));
        }
        if request.spatial_filter.is_some() {
            return Err(EcoScopeError::Invalid(
                "generic ERDDAP spatial filters must be expressed as explicit constraints".into(),
            ));
        }
        if !request.locations.is_empty() {
            return Err(EcoScopeError::Invalid(
                "generic ERDDAP locations must be expressed as explicit constraints".into(),
            ));
        }
        let mut options: ErddapOptions = serde_json::from_value(serde_json::to_value(
            &request.provider_options,
        )?)
        .map_err(|error| EcoScopeError::Invalid(format!("invalid ERDDAP options: {error}")))?;
        if options.protocol == Protocol::Tabledap
            && let Some(start) = &request.temporal_start
            && !options.constraints.iter().any(|constraint| {
                constraint.variable == "time" && matches!(constraint.op.as_str(), "gt" | "gte")
            })
        {
            options.constraints.push(Constraint {
                variable: "time".into(),
                op: "gte".into(),
                value: Value::String(start.clone()),
            });
        }
        if options.protocol == Protocol::Tabledap
            && let Some(end) = &request.temporal_end
            && !options.constraints.iter().any(|constraint| {
                constraint.variable == "time" && matches!(constraint.op.as_str(), "lt" | "lte")
            })
        {
            options.constraints.push(Constraint {
                variable: "time".into(),
                op: "lte".into(),
                value: Value::String(end.clone()),
            });
        }
        let search_record = self
            .search_records(&request.resource_id, 100)
            .await?
            .into_iter()
            .find(|record| record.dataset_id == request.resource_id)
            .ok_or_else(|| EcoScopeError::NotFound(request.resource_id.clone()))?;
        match options.protocol {
            Protocol::Tabledap if search_record.tabledap_url.is_none() => {
                return Err(EcoScopeError::Invalid(format!(
                    "{} does not advertise tabledap access",
                    request.resource_id
                )));
            }
            Protocol::Griddap if search_record.griddap_url.is_none() => {
                return Err(EcoScopeError::Invalid(format!(
                    "{} does not advertise griddap access",
                    request.resource_id
                )));
            }
            _ => {}
        }
        let info = self.client.info(&request.resource_id).await?;
        let available_variables = info.variables.keys().cloned().collect();
        let subset = build_subset(
            self.client.base_url(),
            &request.resource_id,
            &request.variables,
            &available_variables,
            &options,
        )?;
        let redirect_chain = self.client.resolve_download_chain(&subset.url).await?;
        let file = PlannedFile {
            provider_id: self.config.provider_id.clone(),
            name: subset.filename,
            size_bytes: None,
            checksum: None,
            download_url: Some(subset.url.to_string()),
            location: None,
            temporal_partition: None,
            metadata: BTreeMap::from([
                ("protocol".into(), json!(options.protocol)),
                ("output_format".into(), json!(options.output_format)),
                ("decoded_query".into(), Value::String(subset.expression)),
                (
                    "redirect_chain".into(),
                    json!(
                        redirect_chain
                            .iter()
                            .map(url::Url::as_str)
                            .collect::<Vec<_>>()
                    ),
                ),
            ]),
            expires_at: None,
        };
        let mut warnings = vec![
            "This generated ERDDAP subset may change upstream; materialization freezes the bytes with a local BLAKE3 checksum"
                .into(),
        ];
        if request.release.is_some() {
            warnings.push("ERDDAP does not apply EcoScope's release field to subset URLs".into());
        }
        DatasetPlan {
            plan_id: PlanId::new(),
            request,
            plan_hash: String::new(),
            file_count: 1,
            estimated_bytes: None,
            files: vec![file],
            warnings,
            requires_credentials: false,
            created_at: Utc::now(),
            approved_at: None,
        }
        .finalize()
    }

    async fn materialize(
        &self,
        plan: DatasetPlan,
        _credentials: Option<CredentialRef>,
    ) -> Result<DatasetManifest> {
        self.materialize_with_control(plan, || false, |_, _| {})
            .await
    }

    async fn materialize_controlled(
        &self,
        plan: DatasetPlan,
        _credentials: Option<CredentialRef>,
        should_cancel: &(dyn Fn() -> bool + Send + Sync),
        on_progress: &(dyn Fn(usize, usize) + Send + Sync),
    ) -> Result<DatasetManifest> {
        self.materialize_with_control(plan, should_cancel, on_progress)
            .await
    }
}

fn modalities(cdm_data_type: &str) -> Vec<Modality> {
    match cdm_data_type.to_ascii_lowercase().as_str() {
        "grid" => vec![Modality::Raster, Modality::Tensor],
        "trajectory" => vec![Modality::Vector, Modality::TimeSeries, Modality::Tabular],
        "profile" | "trajectoryprofile" | "timeseriesprofile" => {
            vec![Modality::Vector, Modality::TimeSeries, Modality::Tabular]
        }
        "timeseries" => vec![Modality::TimeSeries, Modality::Tabular],
        "point" => vec![Modality::Vector, Modality::Tabular],
        _ => vec![Modality::Tabular],
    }
}

fn insert_optional(target: &mut BTreeMap<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        target.insert(key.into(), Value::String(value));
    }
}

fn string_attribute(attributes: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    attributes
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn numeric_attribute(attributes: &BTreeMap<String, Value>, key: &str) -> Option<f64> {
    attributes.get(key).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn coverage_polygon(attributes: &BTreeMap<String, Value>) -> Option<GeoGeometry> {
    let west = numeric_attribute(attributes, "geospatial_lon_min")?;
    let east = numeric_attribute(attributes, "geospatial_lon_max")?;
    let south = numeric_attribute(attributes, "geospatial_lat_min")?;
    let north = numeric_attribute(attributes, "geospatial_lat_max")?;
    Some(GeoGeometry {
        geojson: json!({
            "type": "Polygon",
            "coordinates": [[[west, south], [east, south], [east, north], [west, north], [west, south]]]
        }),
    })
}

fn planned_redirect_chain(file: &PlannedFile, initial: &url::Url) -> Result<Vec<url::Url>> {
    let Some(values) = file.metadata.get("redirect_chain") else {
        return Ok(vec![initial.clone()]);
    };
    let values = values
        .as_array()
        .ok_or_else(|| EcoScopeError::Invalid("ERDDAP redirect_chain must be an array".into()))?;
    if values.is_empty() || values.len() > 6 {
        return Err(EcoScopeError::Invalid(
            "ERDDAP redirect_chain must contain between one and six URLs".into(),
        ));
    }
    let chain = values
        .iter()
        .map(|value| {
            let value = value.as_str().ok_or_else(|| {
                EcoScopeError::Invalid("ERDDAP redirect_chain values must be URLs".into())
            })?;
            url::Url::parse(value).map_err(|error| {
                EcoScopeError::Invalid(format!("invalid ERDDAP redirect URL: {error}"))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if chain.first() != Some(initial) {
        return Err(EcoScopeError::Invalid(
            "ERDDAP redirect_chain does not begin with the planned URL".into(),
        ));
    }
    for target in chain.iter().skip(1) {
        validate_redirect_target(initial, target)?;
    }
    Ok(chain)
}

fn license_metadata(globals: &BTreeMap<String, Value>) -> Option<LicenseMetadata> {
    let name = string_attribute(globals, "license")?;
    let url = string_attribute(globals, "license_url")
        .or_else(|| name.starts_with("http").then(|| name.clone()));
    let lower = name.to_ascii_lowercase();
    Some(LicenseMetadata {
        name,
        url,
        attribution_required: lower.contains("cc by") || lower.contains("attribution"),
    })
}

fn citation_metadata(
    search: &SearchRecord,
    globals: &BTreeMap<String, Value>,
    info_url: &str,
) -> Option<CitationMetadata> {
    let text = string_attribute(globals, "citation").unwrap_or_else(|| {
        let institution = string_attribute(globals, "institution")
            .or_else(|| search.institution.clone())
            .map(|institution| format!(" ({institution})"))
            .unwrap_or_default();
        format!(
            "{}{}, ERDDAP dataset {}",
            search.title, institution, search.dataset_id
        )
    });
    let doi = string_attribute(globals, "doi")
        .or_else(|| string_attribute(globals, "DOI"))
        .map(|doi| doi.trim_start_matches("doi:").trim().to_owned());
    Some(CitationMetadata {
        text,
        doi,
        url: Some(info_url.to_owned()),
    })
}
