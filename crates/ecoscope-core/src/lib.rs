//! Stable domain model shared by EcoScope providers, storage, MCP tools, and viewers.

use std::{collections::BTreeMap, fmt, path::Path};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// The Rerun release against which EcoScope's semantic event mappings are tested.
pub const PINNED_RERUN_VERSION: &str = "0.36.2";

pub const MAX_MCP_RESULT_BYTES: usize = 256 * 1024;
/// Shared contract between the Rerun sampler and semantic instance mapping.
pub const MAX_RENDERED_POINT_CLOUD_POINTS: u64 = 1_000_000;
pub const DEFAULT_PREVIEW_ROWS: u32 = 200;

macro_rules! opaque_id {
    ($name:ident, $prefix:literal) => {
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::now_v7().simple()))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

opaque_id!(DatasetId, "ds");
opaque_id!(PlanId, "plan");
opaque_id!(JobId, "job");
opaque_id!(ViewId, "view");
opaque_id!(SelectionId, "sel");
opaque_id!(AssetId, "asset");
opaque_id!(CredentialRef, "conn");
opaque_id!(ResultId, "result");
opaque_id!(ResourceId, "resource");

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Neon,
    Local,
    Other(String),
}

impl ProviderKind {
    /// Stable, filesystem-safe namespace for provider-owned content-addressed objects.
    pub fn object_namespace(&self) -> String {
        match self {
            Self::Neon => "neon".into(),
            Self::Local => "local".into(),
            Self::Other(provider_id) => {
                let digest = blake3::hash(provider_id.as_bytes()).to_hex();
                format!("provider-{}", &digest[..16])
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Tabular,
    TimeSeries,
    Raster,
    Hyperspectral,
    PointCloud,
    Vector,
    Tensor,
    Image,
    Unknown,
}

/// Capabilities are negotiated per provider rather than implied by a single
/// NEON-shaped trait. Providers only advertise operations they actually
/// implement.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    CatalogSearch,
    ResourceResolve,
    AssetPlan,
    AssetFetch,
    ObservationsQuery,
    SamplesQuery,
    SpatialSearch,
    StreamSubscribe,
    CitationResolve,
    PolicyEvaluate,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    BuiltIn,
    Community,
    ProviderReviewed,
    ProviderMaintained,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderManifest {
    pub schema_version: u32,
    pub provider_id: String,
    pub name: String,
    pub version: String,
    pub status: ProviderStatus,
    pub capabilities: Vec<ProviderCapability>,
    #[serde(default)]
    pub allowed_network_origins: Vec<String>,
    #[serde(default)]
    pub authentication: Vec<String>,
    #[serde(default)]
    pub standards: Vec<String>,
    pub homepage: Option<String>,
    pub support_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Collection,
    DatasetVersion,
    Asset,
    Site,
    Station,
    Visit,
    Instrument,
    Sensor,
    Observation,
    Occurrence,
    Taxon,
    Sample,
    Agent,
    VocabularyTerm,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResourceRelation {
    pub predicate: String,
    pub target_provider_id: String,
    pub target_resource_id: String,
}

/// Provider-neutral discovery record. `provider_extensions` and
/// `raw_metadata` deliberately preserve information that the shared model
/// cannot represent without flattening provider semantics.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResourceRecord {
    pub provider_id: String,
    pub resource_id: String,
    pub kind: ResourceKind,
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub modalities: Vec<Modality>,
    pub spatial_extent: Option<GeoGeometry>,
    pub temporal_start: Option<String>,
    pub temporal_end: Option<String>,
    #[serde(default)]
    pub relations: Vec<ResourceRelation>,
    #[serde(default)]
    pub identifiers: BTreeMap<String, String>,
    #[serde(default)]
    pub vocabulary_terms: BTreeMap<String, String>,
    #[serde(default)]
    pub provider_extensions: BTreeMap<String, Value>,
    pub raw_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResourceQuery {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub kinds: Vec<ResourceKind>,
    #[serde(default)]
    pub modalities: Vec<Modality>,
    pub spatial_filter: Option<GeoGeometry>,
    pub temporal_start: Option<String>,
    pub temporal_end: Option<String>,
    #[serde(default)]
    pub provider_filters: BTreeMap<String, Value>,
    #[serde(default = "default_catalog_limit")]
    pub limit: u32,
}

impl From<CatalogEntry> for ResourceRecord {
    fn from(entry: CatalogEntry) -> Self {
        let provider_id = match &entry.provider {
            ProviderKind::Neon => "neon".to_owned(),
            ProviderKind::Local => "local".to_owned(),
            ProviderKind::Other(id) => id.clone(),
        };
        Self {
            provider_id,
            resource_id: entry.id,
            kind: ResourceKind::Collection,
            name: entry.name,
            description: entry.description,
            modalities: entry.modalities,
            spatial_extent: None,
            temporal_start: entry.date_start,
            temporal_end: entry.date_end,
            relations: Vec::new(),
            identifiers: BTreeMap::new(),
            vocabulary_terms: BTreeMap::new(),
            provider_extensions: entry.metadata,
            raw_metadata: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CatalogQuery {
    pub text: String,
    #[serde(default)]
    pub modalities: Vec<Modality>,
    #[serde(default)]
    pub sites: Vec<String>,
    pub start_month: Option<String>,
    pub end_month: Option<String>,
    #[serde(default = "default_catalog_limit")]
    pub limit: u32,
}

fn default_catalog_limit() -> u32 {
    25
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CatalogEntry {
    pub provider: ProviderKind,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub modalities: Vec<Modality>,
    pub sites: Vec<String>,
    pub date_start: Option<String>,
    pub date_end: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DatasetRequest {
    pub provider: ProviderKind,
    #[serde(alias = "product_code")]
    pub resource_id: String,
    #[serde(default, alias = "sites")]
    pub locations: Vec<String>,
    #[serde(default, alias = "start_month")]
    pub temporal_start: Option<String>,
    #[serde(default, alias = "end_month")]
    pub temporal_end: Option<String>,
    #[serde(default)]
    pub spatial_filter: Option<GeoGeometry>,
    #[serde(default)]
    pub variables: Vec<String>,
    pub release: Option<String>,
    #[serde(default = "default_package")]
    pub package: String,
    #[serde(default)]
    pub include_provisional: bool,
    #[serde(default)]
    pub provider_options: BTreeMap<String, Value>,
}

fn default_package() -> String {
    "basic".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlannedFile {
    pub provider_id: String,
    pub name: String,
    pub size_bytes: Option<u64>,
    pub checksum: Option<Checksum>,
    pub download_url: Option<String>,
    #[serde(default, alias = "site")]
    pub location: Option<String>,
    #[serde(default, alias = "month")]
    pub temporal_partition: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DatasetPlan {
    pub plan_id: PlanId,
    pub request: DatasetRequest,
    pub plan_hash: String,
    pub file_count: u64,
    pub estimated_bytes: Option<u64>,
    pub files: Vec<PlannedFile>,
    pub warnings: Vec<String>,
    pub requires_credentials: bool,
    pub created_at: DateTime<Utc>,
    pub approved_at: Option<DateTime<Utc>>,
}

impl DatasetPlan {
    pub fn finalize(mut self) -> Result<Self> {
        self.plan_hash.clear();
        // Approval is an attestation about the plan, not part of the immutable plan itself.
        // Excluding it keeps the hash the user approved stable after approval.
        let approved_at = self.approved_at.take();
        let canonical = serde_json::to_vec(&self)?;
        self.plan_hash = blake3::hash(&canonical).to_hex().to_string();
        self.approved_at = approved_at;
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Checksum {
    pub algorithm: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceFile {
    pub asset_id: AssetId,
    pub original_name: String,
    pub source_uri: String,
    pub local_object: Option<String>,
    pub size_bytes: u64,
    pub checksum: Checksum,
    pub media_type: Option<String>,
    #[serde(default, alias = "site")]
    pub location: Option<String>,
    #[serde(default, alias = "month")]
    pub temporal_partition: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Transformation {
    pub name: String,
    pub version: String,
    pub parameters: Value,
    pub created_at: DateTime<Utc>,
}

/// The physical encoding of a scientific asset, separate from its ecological
/// modality. This stays extensible so community formats do not require a core enum change.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FormatDescriptor {
    pub name: String,
    pub version: Option<String>,
    pub profile: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SpatialReference {
    pub authority: Option<String>,
    pub code: Option<String>,
    pub wkt: Option<String>,
    #[serde(default)]
    pub axis_order: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AxisRole {
    X,
    Y,
    Z,
    Time,
    Spectral,
    Channel,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CubeAxis {
    pub name: String,
    pub role: AxisRole,
    pub length: u64,
    pub unit: Option<String>,
    pub coordinate_path: Option<String>,
    pub regular_start: Option<f64>,
    pub regular_step: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CubeDescriptor {
    pub array_path: String,
    pub data_type: String,
    pub axes: Vec<CubeAxis>,
    #[serde(default)]
    pub chunk_shape: Vec<u64>,
    pub scale_factor: Option<f64>,
    pub add_offset: Option<f64>,
    pub no_data: Option<f64>,
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DatasetManifest {
    pub dataset_id: DatasetId,
    pub provider: ProviderKind,
    #[serde(alias = "product_code")]
    pub resource_id: String,
    #[serde(default, alias = "product_revision")]
    pub resource_version: Option<String>,
    pub modalities: Vec<Modality>,
    #[serde(default, alias = "sites")]
    pub locations: Vec<String>,
    #[serde(default, alias = "start_month")]
    pub temporal_start: Option<String>,
    #[serde(default, alias = "end_month")]
    pub temporal_end: Option<String>,
    pub release: Option<String>,
    pub package: Option<String>,
    pub include_provisional: bool,
    pub source_files: Vec<SourceFile>,
    pub transformations: Vec<Transformation>,
    #[serde(default)]
    pub format: Option<FormatDescriptor>,
    #[serde(default)]
    pub spatial_reference: Option<SpatialReference>,
    #[serde(default)]
    pub cube: Option<CubeDescriptor>,
    /// All multidimensional arrays discovered in the source. `cube` is the
    /// optional confirmed scientific mapping selected from this inventory.
    #[serde(default)]
    pub cubes: Vec<CubeDescriptor>,
    pub license: Option<LicenseMetadata>,
    pub citation: Option<CitationMetadata>,
    #[serde(default)]
    pub provider_metadata: BTreeMap<String, Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LicenseMetadata {
    pub name: String,
    pub url: Option<String>,
    pub attribution_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CitationMetadata {
    pub text: String,
    pub doi: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobRecord {
    pub job_id: JobId,
    pub kind: String,
    pub status: JobStatus,
    pub progress: f32,
    pub message: Option<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EcoViewSpec {
    pub version: u32,
    pub view_id: ViewId,
    pub revision: u64,
    pub name: String,
    pub dataset_ids: Vec<DatasetId>,
    pub layout: ViewLayout,
    pub layers: Vec<EcoLayer>,
    pub filters: Vec<Filter>,
    pub linked_groups: Vec<LinkedGroup>,
    pub camera: Option<CameraState>,
    pub active_timeline: Option<TimelineState>,
    pub provenance_visible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewLayout {
    Single,
    Horizontal,
    Vertical,
    Grid { columns: u32 },
    Tabs,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EcoLayer {
    pub id: String,
    pub dataset_id: DatasetId,
    pub name: String,
    pub modality: Modality,
    pub visible: bool,
    pub opacity: f32,
    #[serde(default)]
    pub encoding: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Filter {
    pub field: String,
    pub op: String,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkedGroup {
    pub id: String,
    pub layer_ids: Vec<String>,
    pub dimensions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CameraState {
    pub kind: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimelineState {
    pub name: String,
    pub current: f64,
    pub start: Option<f64>,
    pub end: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GeoGeometry {
    pub geojson: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SemanticSelection {
    Rows {
        dataset_id: DatasetId,
        predicate: Value,
        row_count: u64,
    },
    TimeInterval {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        entities: Vec<String>,
    },
    MapRegion {
        geometry: GeoGeometry,
        crs: String,
    },
    RasterRegion {
        pixel_bounds: [u64; 4],
        world_geometry: Option<GeoGeometry>,
        band_indices: Vec<u32>,
    },
    SpectralRange {
        wavelength_start_nm: f64,
        wavelength_end_nm: f64,
        spatial_region: Option<GeoGeometry>,
    },
    CubePixel {
        dataset_id: DatasetId,
        array_path: String,
        x: u64,
        y: u64,
        x_axis: u32,
        y_axis: u32,
        spectral_axis: u32,
        #[serde(default)]
        displayed_bands: Vec<u32>,
    },
    PointSet {
        dataset_id: DatasetId,
        spatial_query: Value,
        estimated_points: u64,
    },
    Entities {
        entity_paths: Vec<String>,
        instance_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SelectionRecord {
    pub selection_id: SelectionId,
    pub view_id: ViewId,
    pub revision: u64,
    pub selection: SemanticSelection,
    pub summary: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueryFilter {
    pub field: String,
    pub op: String,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AggregateSpec {
    pub field: String,
    pub function: String,
    pub alias: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SortSpec {
    pub field: String,
    #[serde(default)]
    pub descending: bool,
    #[serde(default)]
    pub nulls_first: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct CubeRange {
    /// Inclusive zero-based start index.
    pub start: u64,
    /// Exclusive zero-based end index.
    pub end: u64,
    #[serde(default = "default_cube_step")]
    pub step: u64,
}

/// A bounded, deterministic scientific query. Every variant produces a
/// durable `ResultRecord`; MCP only receives its bounded preview.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DatasetQuery {
    Preview {
        #[serde(default = "default_preview_rows")]
        limit: u32,
    },
    Table {
        #[serde(default)]
        select: Vec<String>,
        #[serde(default)]
        filters: Vec<QueryFilter>,
        #[serde(default)]
        group_by: Vec<String>,
        #[serde(default)]
        aggregates: Vec<AggregateSpec>,
        #[serde(default)]
        order_by: Vec<SortSpec>,
        #[serde(default = "default_preview_rows")]
        limit: u32,
    },
    RasterPixel {
        x: u64,
        y: u64,
        #[serde(default)]
        bands: Vec<u32>,
    },
    RasterRegion {
        geometry: GeoGeometry,
        crs: String,
        #[serde(default)]
        bands: Vec<u32>,
        #[serde(default)]
        statistics: Vec<String>,
    },
    Spectrum {
        x: u64,
        y: u64,
        /// Explicit rank-3 reflectance dataset. If omitted, EcoScope only
        /// proceeds when the manifest contains exactly one unambiguous cube.
        dataset_path: Option<String>,
        /// Optional rank-1 wavelength coordinate dataset.
        wavelength_dataset: Option<String>,
        #[serde(default = "default_spectral_axis")]
        spectral_axis: u32,
        wavelength_start_nm: Option<f64>,
        wavelength_end_nm: Option<f64>,
        scale_factor: Option<f64>,
        add_offset: Option<f64>,
        no_data: Option<f64>,
        #[serde(default)]
        bad_bands: Vec<u32>,
    },
    /// A format-independent, bounded N-dimensional slice. One range is
    /// required per source axis and the result is returned in row-major order.
    CubeSlice {
        array_path: String,
        ranges: Vec<CubeRange>,
        #[serde(default = "default_cube_cell_limit")]
        cell_limit: u64,
    },
    PointCloudRegion {
        geometry: GeoGeometry,
        crs: String,
        /// Exact zero-based source-stream positions. A viewer adapter may use
        /// these only after the service verifies its instance-to-source mapping;
        /// display pick coordinates are never treated as scientific equality.
        #[serde(default)]
        source_indices: Vec<u64>,
        #[serde(default)]
        classifications: Vec<u8>,
        elevation_min: Option<f64>,
        elevation_max: Option<f64>,
        /// COPC point spacing in source coordinate units. Higher values select
        /// coarser octree levels. Omit for full resolution.
        resolution: Option<f64>,
        /// Exact COPC octree level. Mutually exclusive with `resolution`.
        level: Option<i32>,
        #[serde(default = "default_point_limit")]
        point_limit: u64,
    },
    VectorRegion {
        geometry: GeoGeometry,
        crs: String,
    },
}

fn default_preview_rows() -> u32 {
    DEFAULT_PREVIEW_ROWS
}

fn default_point_limit() -> u64 {
    1_000_000
}

fn default_spectral_axis() -> u32 {
    2
}

fn default_cube_step() -> u64 {
    1
}

fn default_cube_cell_limit() -> u64 {
    100_000
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResultRecord {
    pub result_id: ResultId,
    pub dataset_id: DatasetId,
    #[serde(default)]
    pub source_selection: Option<SelectionId>,
    pub query: DatasetQuery,
    pub row_count: Option<u64>,
    pub preview: Value,
    pub artifact: Option<String>,
    pub media_type: Option<String>,
    pub checksum: Option<Checksum>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactDescriptor>,
    #[serde(default)]
    pub transformations: Vec<Transformation>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactDescriptor {
    pub uri: String,
    pub format: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub checksum: Checksum,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DerivedAssetRecord {
    pub derived_id: String,
    pub dataset_id: DatasetId,
    pub kind: String,
    pub source_fingerprint: Checksum,
    pub artifact: ArtifactDescriptor,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Csv,
    Parquet,
    GeoParquet,
    GeoJson,
    GeoTiff,
    Copc,
    RoCrate,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExportRequest {
    pub result_id: ResultId,
    pub format: ExportFormat,
    #[serde(default = "default_true")]
    pub include_provenance: bool,
    #[serde(default = "default_true")]
    pub include_reproduction_code: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExportRecord {
    pub export_id: String,
    pub result_id: ResultId,
    pub format: ExportFormat,
    pub artifact: String,
    pub checksum: Checksum,
    pub manifest_artifact: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LocalAssetInspection {
    pub asset_id: AssetId,
    pub display_name: String,
    pub size_bytes: u64,
    pub fingerprint: Checksum,
    pub media_type: Option<String>,
    pub modalities: Vec<Modality>,
    pub format: String,
    pub dimensions: Vec<u64>,
    pub fields: Vec<String>,
    pub crs: Option<String>,
    pub requires_mapping: bool,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl LocalAssetInspection {
    pub fn display_name_for(path: &Path) -> String {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("local asset")
            .to_owned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EcoScopeError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("credentials required for provider {0}")]
    CredentialsRequired(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, EcoScopeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_request_uses_provider_neutral_v2_names() {
        let request = DatasetRequest {
            provider: ProviderKind::Other("emso".into()),
            resource_id: "OBSEA_moored_buoy_BGC_L1c".into(),
            locations: vec!["OBSEA".into()],
            temporal_start: Some("2025-01-01T00:00:00Z".into()),
            temporal_end: Some("2025-01-31T23:59:59Z".into()),
            spatial_filter: None,
            variables: vec!["time".into(), "temperature".into()],
            release: None,
            package: "csv".into(),
            include_provisional: false,
            provider_options: BTreeMap::from([(
                "protocol".into(),
                Value::String("tabledap".into()),
            )]),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["resource_id"], "OBSEA_moored_buoy_BGC_L1c");
        assert_eq!(value["locations"][0], "OBSEA");
        assert!(value.get("product_code").is_none());
        assert!(value.get("sites").is_none());
        assert!(value.get("start_month").is_none());
    }

    #[test]
    fn dataset_request_reads_v1_alpha_names() {
        let request: DatasetRequest = serde_json::from_value(serde_json::json!({
            "provider": "neon",
            "product_code": "DP1.00094.001",
            "sites": ["HARV"],
            "start_month": "2024-01",
            "end_month": "2024-02",
            "release": "RELEASE-2025",
            "package": "basic",
            "include_provisional": false
        }))
        .unwrap();
        assert_eq!(request.resource_id, "DP1.00094.001");
        assert_eq!(request.locations, vec!["HARV"]);
        assert_eq!(request.temporal_start.as_deref(), Some("2024-01"));
        assert!(request.variables.is_empty());
        assert!(request.provider_options.is_empty());
    }

    #[test]
    fn ids_are_prefixed_and_unique() {
        let first = DatasetId::new();
        let second = DatasetId::new();
        assert!(first.0.starts_with("ds_"));
        assert_ne!(first, second);
    }

    #[test]
    fn provider_object_namespaces_are_stable_and_path_safe() {
        assert_eq!(ProviderKind::Neon.object_namespace(), "neon");
        let namespace = ProviderKind::Other("example/ri/../unsafe".into()).object_namespace();
        assert!(namespace.starts_with("provider-"));
        assert!(!namespace.contains('/'));
        assert_eq!(
            namespace,
            ProviderKind::Other("example/ri/../unsafe".into()).object_namespace()
        );
    }

    #[test]
    fn finalized_plan_hash_is_stable() {
        let plan = DatasetPlan {
            plan_id: PlanId("plan_fixed".into()),
            request: DatasetRequest {
                provider: ProviderKind::Neon,
                resource_id: "DP1.00094.001".into(),
                locations: vec!["HARV".into()],
                temporal_start: Some("2024-01".into()),
                temporal_end: Some("2024-02".into()),
                spatial_filter: None,
                variables: vec![],
                release: Some("RELEASE-2025".into()),
                package: "basic".into(),
                include_provisional: false,
                provider_options: BTreeMap::new(),
            },
            plan_hash: String::new(),
            file_count: 0,
            estimated_bytes: Some(0),
            files: vec![],
            warnings: vec![],
            requires_credentials: true,
            created_at: DateTime::from_timestamp(0, 0).unwrap(),
            approved_at: None,
        };
        assert_eq!(
            plan.clone().finalize().unwrap().plan_hash,
            plan.finalize().unwrap().plan_hash
        );
    }

    #[test]
    fn approval_timestamp_does_not_change_plan_hash() {
        let mut plan = DatasetPlan {
            plan_id: PlanId("plan_fixed".into()),
            request: DatasetRequest {
                provider: ProviderKind::Neon,
                resource_id: "DP3.30006.002".into(),
                locations: vec!["HARV".into()],
                temporal_start: Some("2024-01".into()),
                temporal_end: Some("2024-02".into()),
                spatial_filter: None,
                variables: vec![],
                release: Some("RELEASE-2025".into()),
                package: "basic".into(),
                include_provisional: false,
                provider_options: BTreeMap::new(),
            },
            plan_hash: String::new(),
            file_count: 0,
            estimated_bytes: Some(0),
            files: vec![],
            warnings: vec![],
            requires_credentials: true,
            created_at: DateTime::from_timestamp(0, 0).unwrap(),
            approved_at: None,
        }
        .finalize()
        .unwrap();
        let original_hash = plan.plan_hash.clone();
        plan.approved_at = Some(DateTime::from_timestamp(1, 0).unwrap());
        assert_eq!(plan.finalize().unwrap().plan_hash, original_hash);
    }
}
