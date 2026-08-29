//! Generic ERDDAP provider with maintained research-infrastructure presets.

use std::collections::BTreeMap;

use async_trait::async_trait;
use ecoscope_core::{
    CatalogEntry, CatalogQuery, CredentialRef, DatasetManifest, DatasetPlan, DatasetRequest,
    EcoScopeError, GeoGeometry, Modality, ProviderCapability, ProviderKind, ProviderManifest,
    ProviderStatus, ResourceKind, ResourceQuery, ResourceRecord, Result,
};
use ecoscope_provider_api::{EcologicalDataProvider, PROVIDER_PROTOCOL_VERSION};
use serde_json::{Value, json};

use crate::{
    client::ErddapClient,
    config::ErddapConfig,
    table::{InfoMetadata, SearchRecord},
};

pub mod client;
pub mod config;
pub mod table;

#[derive(Clone)]
pub struct ErddapProvider {
    config: ErddapConfig,
    client: ErddapClient,
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
        Ok(Self { config, client })
    }

    pub fn with_metadata_limit_bytes(mut self, limit: usize) -> Self {
        self.client = self.client.with_metadata_limit_bytes(limit);
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

    async fn plan_dataset(&self, _request: DatasetRequest) -> Result<DatasetPlan> {
        Err(EcoScopeError::Invalid(
            "ERDDAP subset planning is not available yet".into(),
        ))
    }

    async fn materialize(
        &self,
        _plan: DatasetPlan,
        _credentials: Option<CredentialRef>,
    ) -> Result<DatasetManifest> {
        Err(EcoScopeError::Invalid(
            "ERDDAP materialization is not available yet".into(),
        ))
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
