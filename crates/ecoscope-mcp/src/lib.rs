//! EcoScope MCP server: a scientific interface, not a mirror of provider APIs.

use std::collections::BTreeMap;

use chrono::Utc;
use ecoscope_core::{
    CredentialRef, DatasetId, DatasetQuery, DatasetRequest, EcoScopeError, ExportFormat,
    ExportRequest, JobStatus, MAX_MCP_RESULT_BYTES, ProviderCapability, ProviderKind,
    ProviderManifest, ProviderStatus, ResourceQuery, ResultId, SemanticSelection,
};
use ecoscope_provider_api::{EcologicalDataProvider, PROVIDER_PROTOCOL_VERSION, validate_manifest};
use ecoscope_provider_erddap::{ErddapProvider, config as erddap_config};
use ecoscope_provider_neon::NeonProvider;
use ecoscope_provider_process::{ProcessProvider, discover_configs, find_config};
use ecoscope_service::EcoScopeService;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

const KEYRING_SERVICE: &str = "org.ecoscope.EcoScope";
const KEYRING_NEON_USER: &str = "neon-api-token";

pub fn built_in_provider_manifests() -> std::result::Result<Vec<ProviderManifest>, EcoScopeError> {
    let neon = NeonProvider::new(None)?.manifest();
    let local = ProviderManifest {
        schema_version: PROVIDER_PROTOCOL_VERSION,
        provider_id: "local".into(),
        name: "Local scientific files".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        status: ProviderStatus::BuiltIn,
        capabilities: vec![
            ProviderCapability::ResourceResolve,
            ProviderCapability::AssetFetch,
            ProviderCapability::ObservationsQuery,
            ProviderCapability::SpatialSearch,
            ProviderCapability::PolicyEvaluate,
        ],
        allowed_network_origins: vec![],
        authentication: vec!["operating_system_file_access".into()],
        standards: vec![
            "Arrow".into(),
            "Parquet".into(),
            "GeoJSON".into(),
            "GeoTIFF".into(),
            "HDF5".into(),
            "NetCDF".into(),
            "Zarr".into(),
            "LAS/LAZ/COPC".into(),
        ],
        homepage: None,
        support_url: None,
    };
    let mut manifests = vec![neon, local];
    for preset in erddap_config::presets() {
        manifests.push(ErddapProvider::new((*preset).into())?.manifest());
    }
    for manifest in &manifests {
        validate_manifest(manifest)?;
    }
    Ok(manifests)
}

async fn routed_provider(
    service: &EcoScopeService,
    provider_id: &str,
    neon_token: Option<String>,
) -> std::result::Result<Box<dyn EcologicalDataProvider>, EcoScopeError> {
    if provider_id.eq_ignore_ascii_case("neon") {
        return Ok(Box::new(NeonProvider::new(neon_token)?.with_object_dir(
            service.paths().provider_objects_dir(&ProviderKind::Neon),
        )));
    }
    let normalized = provider_id.to_ascii_lowercase();
    if let Some(preset) = erddap_config::preset(&normalized) {
        let provider_kind = ProviderKind::Other(preset.provider_id.into());
        return Ok(Box::new(
            ErddapProvider::new(preset.into())?
                .with_object_dir(service.paths().provider_objects_dir(&provider_kind)),
        ));
    }
    let config = find_config(&service.paths().providers_dir, provider_id)?;
    Ok(Box::new(ProcessProvider::spawn(config).await?))
}

#[derive(Clone)]
pub struct EcoScopeMcp {
    service: EcoScopeService,
    tool_router: ToolRouter<Self>,
}

impl EcoScopeMcp {
    pub fn new(service: EcoScopeService) -> Self {
        Self {
            service,
            tool_router: Self::tool_router(),
        }
    }

    async fn provider(
        &self,
        provider_id: &str,
    ) -> std::result::Result<Box<dyn EcologicalDataProvider>, EcoScopeError> {
        let neon_token = provider_id
            .eq_ignore_ascii_case("neon")
            .then(load_neon_token)
            .flatten();
        routed_provider(&self.service, provider_id, neon_token).await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchCatalogInput {
    /// Stable provider ID. `neon` is the production remote provider in v0.2.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Natural-language terms, a product code, or an ecological variable.
    pub query: String,
    /// Optional modality filters such as hyperspectral, point_cloud, or raster.
    #[serde(default)]
    pub modalities: Vec<ecoscope_core::Modality>,
    /// Optional NEON site codes.
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectProductInput {
    /// Provider product identifier, for example DP3.30006.002.
    pub product_code: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectResourceInput {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub resource_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanDatasetInput {
    /// Provider name. NEON is the supported remote provider in v0.1.
    #[serde(default = "default_provider")]
    pub provider: String,
    pub product_code: String,
    pub sites: Vec<String>,
    pub start_month: String,
    pub end_month: String,
    pub release: Option<String>,
    #[serde(default = "default_package")]
    pub package: String,
    #[serde(default)]
    pub include_provisional: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanMaterializationInput {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub resource_id: String,
    #[serde(default)]
    pub locations: Vec<String>,
    pub temporal_start: Option<String>,
    pub temporal_end: Option<String>,
    pub spatial_filter: Option<ecoscope_core::GeoGeometry>,
    #[serde(default)]
    pub variables: Vec<String>,
    pub release: Option<String>,
    #[serde(default = "default_package")]
    pub package: String,
    #[serde(default)]
    pub include_provisional: bool,
    /// Provider-native options preserved verbatim in the plan and manifest.
    #[serde(default)]
    pub provider_options: BTreeMap<String, Value>,
}

fn default_provider() -> String {
    "neon".into()
}

fn default_package() -> String {
    "basic".into()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApprovePlanInput {
    pub plan_id: String,
    /// The exact plan_hash returned by plan_dataset.
    pub plan_hash: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanIdInput {
    pub plan_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobIdInput {
    pub job_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DatasetIdInput {
    pub dataset_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryDatasetInput {
    pub dataset_id: String,
    /// Scientific query. Omit for a backwards-compatible bounded preview.
    pub query: Option<DatasetQuery>,
    /// Legacy preview limit used only when query is omitted.
    #[serde(default = "default_preview_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResultIdInput {
    pub result_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportResultInput {
    pub result_id: String,
    pub format: ExportFormat,
    #[serde(default = "default_true")]
    pub include_provenance: bool,
    #[serde(default = "default_true")]
    pub include_reproduction_code: bool,
}

fn default_true() -> bool {
    true
}

fn default_preview_limit() -> u32 {
    200
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateViewInput {
    pub name: String,
    pub dataset_ids: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PatchViewInput {
    pub view_id: String,
    pub expected_revision: u64,
    /// RFC 7396 JSON Merge Patch applied to the EcoViewSpec.
    pub patch: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ViewIdInput {
    pub view_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConfigureHyperspectralInput {
    pub view_id: String,
    pub expected_revision: u64,
    pub layer_id: String,
    /// HDF5 dataset path discovered in the source manifest, e.g. /HARV/Reflectance/Reflectance_Data.
    pub hdf5_dataset: String,
    /// Optional wavelength coordinate dataset for semantic spectral queries.
    pub wavelength_dataset: Option<String>,
    /// Wavelength unit stored in the coordinate dataset, normally `nm`.
    pub wavelength_unit: Option<String>,
    /// The spectral dimension. v0.1 rendering supports axis 2 ([y, x, band]).
    #[serde(default = "default_spectral_axis")]
    pub spectral_axis: u32,
    /// Render one zero-based band as grayscale.
    pub band: Option<u32>,
    /// Render three zero-based bands as RGB. All three are required together.
    pub red_band: Option<u32>,
    pub green_band: Option<u32>,
    pub blue_band: Option<u32>,
    pub display_min: Option<f64>,
    pub display_max: Option<f64>,
    pub no_data: Option<f64>,
    pub scale_factor: Option<f64>,
    pub add_offset: Option<f64>,
    #[serde(default)]
    pub bad_bands: Vec<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConfigureCubeInput {
    pub view_id: String,
    pub expected_revision: u64,
    pub layer_id: String,
    /// Array path from DatasetManifest.cubes. Works for HDF5/NetCDF-4,
    /// NetCDF-3 variables, and Zarr v2/v3 arrays.
    pub cube_array: String,
    #[serde(default = "default_y_axis")]
    pub y_axis: u32,
    #[serde(default = "default_x_axis")]
    pub x_axis: u32,
    #[serde(default = "default_spectral_axis")]
    pub spectral_axis: u32,
    pub wavelength_dataset: Option<String>,
    pub wavelength_unit: Option<String>,
    pub band: Option<u32>,
    pub red_band: Option<u32>,
    pub green_band: Option<u32>,
    pub blue_band: Option<u32>,
    pub display_min: Option<f64>,
    pub display_max: Option<f64>,
    pub no_data: Option<f64>,
    pub scale_factor: Option<f64>,
    pub add_offset: Option<f64>,
    #[serde(default)]
    pub bad_bands: Vec<u32>,
}

fn default_y_axis() -> u32 {
    0
}

fn default_x_axis() -> u32 {
    1
}

fn default_spectral_axis() -> u32 {
    2
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordSelectionInput {
    pub view_id: String,
    pub selection: SemanticSelection,
    #[serde(default)]
    pub summary: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuerySelectionInput {
    pub selection_id: String,
    /// Required only when the selection intersects more than one compatible
    /// layer in a linked multimodal view.
    pub dataset_id: Option<String>,
    #[serde(default = "default_point_limit")]
    pub point_limit: u64,
}

fn default_point_limit() -> u64 {
    100_000
}

#[tool_router]
impl EcoScopeMcp {
    #[tool(description = "Check EcoScope storage, protocol, provider, and viewer readiness")]
    async fn health(&self) -> CallToolResult {
        let mut health = match self.service.health() {
            Ok(value) => value,
            Err(error) => return tool_error(error),
        };
        health["mcp_spec"] = json!("2026-07-28");
        health["rerun_version"] = json!(ecoscope_rerun::PINNED_RERUN_VERSION);
        health["neon_connected"] = json!(load_neon_token().is_some());
        bounded_result(health)
    }

    #[tool(description = "List ecological providers and their negotiated capabilities")]
    async fn list_providers(&self) -> CallToolResult {
        let mut providers = match built_in_provider_manifests() {
            Ok(providers) => providers,
            Err(error) => return tool_error(error),
        };
        let mut unavailable = Vec::new();
        let configs = match discover_configs(&self.service.paths().providers_dir) {
            Ok(configs) => configs,
            Err(error) => return tool_error(error),
        };
        for config in configs {
            let provider_id = config.expected_provider_id.clone();
            match ProcessProvider::spawn(config).await {
                Ok(provider) => providers.push(provider.manifest()),
                Err(error) => unavailable.push(json!({
                    "provider_id": provider_id,
                    "error": error.to_string()
                })),
            }
        }
        bounded_result(json!({
            "count": providers.len(),
            "providers": providers,
            "unavailable": unavailable
        }))
    }

    #[tool(
        description = "Search ecological provider resources by concept, modality, site, and dates without downloading data"
    )]
    async fn search_catalog(
        &self,
        Parameters(input): Parameters<SearchCatalogInput>,
    ) -> CallToolResult {
        let query = ResourceQuery {
            text: input.query,
            kinds: vec![],
            modalities: input.modalities,
            spatial_filter: None,
            temporal_start: input.start_month,
            temporal_end: input.end_month,
            provider_filters: BTreeMap::from([("sites".into(), json!(input.sites))]),
            limit: input.limit.min(100),
        };
        let provider = match self.provider(&input.provider).await {
            Ok(provider) => provider,
            Err(error) => return tool_error(error),
        };
        match provider.search_resources(query).await {
            Ok(entries) => bounded_result(json!({"resources": entries, "count": entries.len()})),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Resolve one provider resource with native metadata preserved")]
    async fn inspect_resource(
        &self,
        Parameters(input): Parameters<InspectResourceInput>,
    ) -> CallToolResult {
        let provider = match self.provider(&input.provider).await {
            Ok(provider) => provider,
            Err(error) => return tool_error(error),
        };
        match provider.resolve_resource(&input.resource_id).await {
            Ok(resource) => bounded_serializable(resource),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Inspect one NEON product, including inferred modalities and scientific metadata"
    )]
    async fn inspect_product(
        &self,
        Parameters(input): Parameters<InspectProductInput>,
    ) -> CallToolResult {
        let provider = match self.provider("neon").await {
            Ok(provider) => provider,
            Err(error) => return tool_error(error),
        };
        match provider.inspect_product(&input.product_code).await {
            Ok(product) => bounded_serializable(product),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Report whether a NEON API token is securely connected; never returns the token"
    )]
    async fn get_connection_status(&self) -> CallToolResult {
        bounded_result(json!({
            "provider": "neon",
            "connected": load_neon_token().is_some(),
            "credential_storage": "operating_system_keychain"
        }))
    }

    #[tool(
        description = "Return a safe out-of-band command for connecting a NEON token outside model context"
    )]
    async fn open_connection_setup(&self) -> CallToolResult {
        bounded_result(json!({
            "provider": "neon",
            "status": if load_neon_token().is_some() { "connected" } else { "connection_required" },
            "command": "ecoscope connect-neon",
            "warning": "Run this command in a terminal. Never paste the token into chat or a tool argument."
        }))
    }

    #[tool(description = "Construct and persist a reproducible, non-downloading dataset plan")]
    async fn plan_dataset(
        &self,
        Parameters(input): Parameters<PlanDatasetInput>,
    ) -> CallToolResult {
        let is_neon = input.provider.eq_ignore_ascii_case("neon");
        let provider_id = input.provider.clone();
        let request = DatasetRequest {
            provider: if is_neon {
                ProviderKind::Neon
            } else {
                ProviderKind::Other(provider_id.clone())
            },
            resource_id: input.product_code,
            locations: input.sites,
            temporal_start: Some(input.start_month),
            temporal_end: Some(input.end_month),
            spatial_filter: None,
            variables: vec![],
            release: input.release,
            package: input.package,
            include_provisional: input.include_provisional,
            provider_options: BTreeMap::new(),
        };
        let provider = match self.provider(&provider_id).await {
            Ok(provider) => provider,
            Err(error) => return tool_error(error),
        };
        match provider.plan_dataset(request).await {
            Ok(plan) => {
                if let Err(error) = self.service.save_plan(&plan) {
                    return tool_error(error);
                }
                bounded_serializable(plan)
            }
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Construct a provider-neutral, reproducible materialization plan without downloading data"
    )]
    async fn plan_materialization(
        &self,
        Parameters(input): Parameters<PlanMaterializationInput>,
    ) -> CallToolResult {
        let is_neon = input.provider.eq_ignore_ascii_case("neon");
        let provider_id = input.provider.clone();
        if is_neon && (input.temporal_start.is_none() || input.temporal_end.is_none()) {
            return tool_error(EcoScopeError::Invalid(
                "NEON materialization requires temporal_start and temporal_end".into(),
            ));
        }
        let request = DatasetRequest {
            provider: if is_neon {
                ProviderKind::Neon
            } else {
                ProviderKind::Other(provider_id.clone())
            },
            resource_id: input.resource_id,
            locations: input.locations,
            temporal_start: input.temporal_start,
            temporal_end: input.temporal_end,
            spatial_filter: input.spatial_filter,
            variables: input.variables,
            release: input.release,
            package: input.package,
            include_provisional: input.include_provisional,
            provider_options: input.provider_options,
        };
        let provider = match self.provider(&provider_id).await {
            Ok(provider) => provider,
            Err(error) => return tool_error(error),
        };
        match provider.plan_dataset(request).await {
            Ok(plan) => {
                if let Err(error) = self.service.save_plan(&plan) {
                    return tool_error(error);
                }
                bounded_serializable(plan)
            }
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Inspect a previously persisted dataset plan")]
    async fn inspect_plan(&self, Parameters(input): Parameters<PlanIdInput>) -> CallToolResult {
        match self.service.get_plan(&input.plan_id) {
            Ok(plan) => bounded_serializable(plan),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Approve the exact immutable plan hash before any download begins")]
    async fn approve_plan(
        &self,
        Parameters(input): Parameters<ApprovePlanInput>,
    ) -> CallToolResult {
        match self.service.approve_plan(&input.plan_id, &input.plan_hash) {
            Ok(plan) => bounded_serializable(plan),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Start materializing an approved provider plan; returns a durable job handle immediately"
    )]
    async fn materialize_dataset(
        &self,
        Parameters(input): Parameters<PlanIdInput>,
    ) -> CallToolResult {
        let plan = match self.service.get_plan(&input.plan_id) {
            Ok(plan) => plan,
            Err(error) => return tool_error(error),
        };
        if plan.approved_at.is_none() {
            return tool_error(EcoScopeError::Invalid(
                "approve the plan before materialization".into(),
            ));
        }
        let (provider_id, token, credentials) = match &plan.request.provider {
            ProviderKind::Neon => match load_neon_token() {
                Some(token) => (
                    "neon".to_owned(),
                    Some(token),
                    Some(CredentialRef("conn_neon_keychain".into())),
                ),
                None => {
                    return tool_error(EcoScopeError::CredentialsRequired("neon".into()));
                }
            },
            ProviderKind::Other(provider_id) => {
                if erddap_config::preset(provider_id).is_none()
                    && let Err(error) =
                        find_config(&self.service.paths().providers_dir, provider_id)
                {
                    return tool_error(error);
                }
                (provider_id.clone(), None, None)
            }
            ProviderKind::Local => {
                return tool_error(EcoScopeError::Invalid(
                    "local files are imported outside MCP rather than materialized from plans"
                        .into(),
                ));
            }
        };
        let job_kind = format!("materialize_{}", plan.request.provider.object_namespace());
        let job = match self.service.create_job(&job_kind) {
            Ok(job) => job,
            Err(error) => return tool_error(error),
        };
        let service = self.service.clone();
        let job_id = job.job_id.clone();
        tokio::spawn(async move {
            let mut running = match service.get_job(&job_id.0) {
                Ok(job) => job,
                Err(_) => return,
            };
            running.status = JobStatus::Running;
            running.message = Some("Materializing and verifying provider assets".into());
            running.updated_at = Utc::now();
            let _ = service.save_job(&running);
            let outcome: Result<_, EcoScopeError> = async {
                let provider = routed_provider(&service, &provider_id, token).await?;
                let cancellation_service = service.clone();
                let cancellation_job_id = job_id.0.clone();
                let should_cancel = move || {
                    cancellation_service
                        .get_job(&cancellation_job_id)
                        .is_ok_and(|job| job.status == JobStatus::Cancelled)
                };
                let progress_service = service.clone();
                let progress_job_id = job_id.0.clone();
                let on_progress = move |completed, total| {
                    if let Ok(mut job) = progress_service.get_job(&progress_job_id)
                        && job.status != JobStatus::Cancelled
                    {
                        job.progress = if total == 0 {
                            1.0
                        } else {
                            completed as f32 / total as f32
                        };
                        job.message = Some(format!(
                            "Downloaded and verified {completed} of {total} files"
                        ));
                        job.updated_at = Utc::now();
                        let _ = progress_service.save_job(&job);
                    }
                };
                provider
                    .materialize_controlled(plan, credentials, &should_cancel, &on_progress)
                    .await
            }
            .await;
            match outcome {
                Ok(mut manifest) => {
                    if service
                        .get_job(&job_id.0)
                        .is_ok_and(|job| job.status == JobStatus::Cancelled)
                    {
                        return;
                    }
                    service.enrich_manifest_metadata(&mut manifest);
                    if let Err(error) = service.save_manifest(&manifest) {
                        fail_job(&service, &job_id.0, error.to_string());
                        return;
                    }
                    if let Ok(mut complete) = service.get_job(&job_id.0) {
                        complete.status = JobStatus::Succeeded;
                        complete.progress = 1.0;
                        complete.message = Some("Dataset materialized".into());
                        complete.result = Some(json!({"dataset_id": manifest.dataset_id}));
                        complete.updated_at = Utc::now();
                        let _ = service.save_job(&complete);
                    }
                }
                Err(error) => fail_job(&service, &job_id.0, error.to_string()),
            }
        });
        bounded_serializable(job)
    }

    #[tool(description = "Inspect progress and results for a durable EcoScope job")]
    async fn get_job(&self, Parameters(input): Parameters<JobIdInput>) -> CallToolResult {
        match self.service.get_job(&input.job_id) {
            Ok(job) => bounded_serializable(job),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Request cancellation of a queued or running EcoScope job")]
    async fn cancel_job(&self, Parameters(input): Parameters<JobIdInput>) -> CallToolResult {
        match self.service.cancel_job(&input.job_id) {
            Ok(job) => bounded_serializable(job),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Return safe instructions for importing a local scientific file through a user-controlled picker or CLI"
    )]
    async fn open_importer(&self) -> CallToolResult {
        bounded_result(json!({
            "status": "user_action_required",
            "command": "ecoscope import <file>",
            "supported": ["csv", "tsv", "parquet", "arrow", "geotiff", "hdf5", "netcdf4", "zarr", "las", "laz", "copc", "geojson"],
            "security": "Local paths are selected outside model context and replaced by opaque asset IDs."
        }))
    }

    #[tool(description = "List materialized provider datasets and locally imported datasets")]
    async fn list_datasets(&self) -> CallToolResult {
        match self.service.list_manifests() {
            Ok(datasets) => bounded_result(json!({"datasets": datasets, "count": datasets.len()})),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Start building a derived COPC octree for full-resolution spatial queries on a local or materialized LAS/LAZ dataset; returns a durable job handle"
    )]
    async fn prepare_point_cloud_index(
        &self,
        Parameters(input): Parameters<DatasetIdInput>,
    ) -> CallToolResult {
        let job = match self.service.create_job("derive_copc_spatial_index") {
            Ok(job) => job,
            Err(error) => return tool_error(error),
        };
        let service = self.service.clone();
        let job_id = job.job_id.clone();
        tokio::spawn(async move {
            if let Ok(mut running) = service.get_job(&job_id.0) {
                running.status = JobStatus::Running;
                running.progress = 0.05;
                running.message = Some("Building a full-resolution COPC spatial index".into());
                running.updated_at = Utc::now();
                let _ = service.save_job(&running);
            }
            let indexing_service = service.clone();
            let dataset_id = input.dataset_id;
            let outcome = tokio::task::spawn_blocking(move || {
                indexing_service.derive_copc_index(&dataset_id)
            })
            .await;
            match outcome {
                Ok(Ok(derived)) => {
                    if let Ok(mut complete) = service.get_job(&job_id.0) {
                        if complete.status == JobStatus::Cancelled {
                            return;
                        }
                        complete.status = JobStatus::Succeeded;
                        complete.progress = 1.0;
                        complete.message = Some("COPC spatial index ready".into());
                        complete.result = Some(json!({"derived_asset": derived}));
                        complete.updated_at = Utc::now();
                        let _ = service.save_job(&complete);
                    }
                }
                Ok(Err(error)) => fail_job(&service, &job_id.0, error.to_string()),
                Err(error) => fail_job(&service, &job_id.0, error.to_string()),
            }
        });
        bounded_serializable(job)
    }

    #[tool(
        description = "Inspect an immutable dataset manifest, sources, transformations, license, and citation"
    )]
    async fn inspect_dataset(
        &self,
        Parameters(input): Parameters<DatasetIdInput>,
    ) -> CallToolResult {
        match self.service.get_manifest(&input.dataset_id) {
            Ok(manifest) => bounded_serializable(manifest),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Run a bounded tabular, raster, hyperspectral, point-cloud, or vector query and return a durable result handle"
    )]
    async fn query_dataset(
        &self,
        Parameters(input): Parameters<QueryDatasetInput>,
    ) -> CallToolResult {
        let query = input.query.unwrap_or(DatasetQuery::Preview {
            limit: input.limit.min(2_000),
        });
        match self.service.query_dataset(&input.dataset_id, query).await {
            Ok(result) => bounded_serializable(result),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Inspect a durable scientific query result and its bounded preview")]
    async fn inspect_result(&self, Parameters(input): Parameters<ResultIdInput>) -> CallToolResult {
        match self.service.get_result(&input.result_id) {
            Ok(result) => bounded_serializable(result),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Export a durable query result as CSV, Parquet, or RO-Crate with provenance"
    )]
    async fn export_result(
        &self,
        Parameters(input): Parameters<ExportResultInput>,
    ) -> CallToolResult {
        match self.service.export_result(ExportRequest {
            result_id: ResultId(input.result_id),
            format: input.format,
            include_provenance: input.include_provenance,
            include_reproduction_code: input.include_reproduction_code,
        }) {
            Ok(export) => bounded_serializable(export),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Create a durable declarative multimodal view over one or more datasets")]
    async fn create_view(&self, Parameters(input): Parameters<CreateViewInput>) -> CallToolResult {
        match self.service.create_view(
            input.name,
            input.dataset_ids.into_iter().map(DatasetId).collect(),
        ) {
            Ok(view) => bounded_serializable(view),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Patch an existing EcoViewSpec using optimistic revision control")]
    async fn patch_view(&self, Parameters(input): Parameters<PatchViewInput>) -> CallToolResult {
        match self
            .service
            .patch_view(&input.view_id, input.expected_revision, input.patch)
        {
            Ok(view) => bounded_serializable(view),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Configure an HDF5 hyperspectral layer using explicit dataset, axis, band, no-data, and display-range semantics"
    )]
    async fn configure_hyperspectral_view(
        &self,
        Parameters(input): Parameters<ConfigureHyperspectralInput>,
    ) -> CallToolResult {
        let has_single = input.band.is_some();
        let rgb_count = [input.red_band, input.green_band, input.blue_band]
            .into_iter()
            .flatten()
            .count();
        if has_single == (rgb_count == 3) || (!has_single && rgb_count != 3) {
            return tool_error(EcoScopeError::Invalid(
                "provide either band or all of red_band, green_band, and blue_band".into(),
            ));
        }
        if input.display_min.is_some() != input.display_max.is_some() {
            return tool_error(EcoScopeError::Invalid(
                "display_min and display_max must be supplied together".into(),
            ));
        }
        if input.spectral_axis != 2 {
            return tool_error(EcoScopeError::Invalid(
                "v0.1 rendering supports spectral_axis=2 ([y, x, band])".into(),
            ));
        }
        if let (Some(minimum), Some(maximum)) = (input.display_min, input.display_max)
            && minimum >= maximum
        {
            return tool_error(EcoScopeError::Invalid(
                "display_min must be less than display_max".into(),
            ));
        }
        let mut encoding = BTreeMap::from([
            ("hdf5_dataset".into(), json!(input.hdf5_dataset.clone())),
            ("cube_array".into(), json!(input.hdf5_dataset)),
            ("y_axis".into(), json!(0)),
            ("x_axis".into(), json!(1)),
            ("spectral_axis".into(), json!(input.spectral_axis)),
        ]);
        for (key, value) in [
            ("wavelength_dataset", input.wavelength_dataset),
            ("wavelength_unit", input.wavelength_unit),
        ] {
            if let Some(value) = value {
                encoding.insert(key.into(), json!(value));
            }
        }
        for (key, value) in [
            ("band", input.band),
            ("red_band", input.red_band),
            ("green_band", input.green_band),
            ("blue_band", input.blue_band),
        ] {
            if let Some(value) = value {
                encoding.insert(key.into(), json!(value));
            }
        }
        for (key, value) in [
            ("display_min", input.display_min),
            ("display_max", input.display_max),
            ("no_data", input.no_data),
            ("scale_factor", input.scale_factor),
            ("add_offset", input.add_offset),
        ] {
            if let Some(value) = value {
                encoding.insert(key.into(), json!(value));
            }
        }
        if !input.bad_bands.is_empty() {
            encoding.insert("bad_bands".into(), json!(input.bad_bands));
        }
        match self.service.configure_layer_encoding(
            &input.view_id,
            input.expected_revision,
            &input.layer_id,
            encoding,
        ) {
            Ok(view) => bounded_serializable(view),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Configure an HDF5, NetCDF, or Zarr rank-3 cube layer using explicit spatial/spectral axes, bands, no-data, and display semantics"
    )]
    async fn configure_cube_view(
        &self,
        Parameters(input): Parameters<ConfigureCubeInput>,
    ) -> CallToolResult {
        let has_single = input.band.is_some();
        let rgb_count = [input.red_band, input.green_band, input.blue_band]
            .into_iter()
            .flatten()
            .count();
        if has_single == (rgb_count == 3) || (!has_single && rgb_count != 3) {
            return tool_error(EcoScopeError::Invalid(
                "provide either band or all of red_band, green_band, and blue_band".into(),
            ));
        }
        if input.display_min.is_some() != input.display_max.is_some() {
            return tool_error(EcoScopeError::Invalid(
                "display_min and display_max must be supplied together".into(),
            ));
        }
        if input.y_axis == input.x_axis
            || input.y_axis == input.spectral_axis
            || input.x_axis == input.spectral_axis
        {
            return tool_error(EcoScopeError::Invalid(
                "y_axis, x_axis, and spectral_axis must be distinct".into(),
            ));
        }
        if let (Some(minimum), Some(maximum)) = (input.display_min, input.display_max)
            && minimum >= maximum
        {
            return tool_error(EcoScopeError::Invalid(
                "display_min must be less than display_max".into(),
            ));
        }
        let mut encoding = BTreeMap::from([
            ("cube_array".into(), json!(input.cube_array)),
            ("y_axis".into(), json!(input.y_axis)),
            ("x_axis".into(), json!(input.x_axis)),
            ("spectral_axis".into(), json!(input.spectral_axis)),
        ]);
        for (key, value) in [
            ("wavelength_dataset", input.wavelength_dataset),
            ("wavelength_unit", input.wavelength_unit),
        ] {
            if let Some(value) = value {
                encoding.insert(key.into(), json!(value));
            }
        }
        for (key, value) in [
            ("band", input.band),
            ("red_band", input.red_band),
            ("green_band", input.green_band),
            ("blue_band", input.blue_band),
        ] {
            if let Some(value) = value {
                encoding.insert(key.into(), json!(value));
            }
        }
        for (key, value) in [
            ("display_min", input.display_min),
            ("display_max", input.display_max),
            ("no_data", input.no_data),
            ("scale_factor", input.scale_factor),
            ("add_offset", input.add_offset),
        ] {
            if let Some(value) = value {
                encoding.insert(key.into(), json!(value));
            }
        }
        if !input.bad_bands.is_empty() {
            encoding.insert("bad_bands".into(), json!(input.bad_bands));
        }
        match self.service.configure_layer_encoding(
            &input.view_id,
            input.expected_revision,
            &input.layer_id,
            encoding,
        ) {
            Ok(view) => bounded_serializable(view),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Inspect the authoritative semantic state of a visualization")]
    async fn inspect_view(&self, Parameters(input): Parameters<ViewIdInput>) -> CallToolResult {
        match self.service.get_view(&input.view_id) {
            Ok(view) => bounded_serializable(view),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Record a structured human/viewer selection for subsequent agent reasoning"
    )]
    async fn record_selection(
        &self,
        Parameters(input): Parameters<RecordSelectionInput>,
    ) -> CallToolResult {
        match self
            .service
            .save_selection(&input.view_id, input.selection, input.summary)
        {
            Ok(selection) => bounded_serializable(selection),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Inspect the latest exact human selection in a visualization")]
    async fn inspect_selection(
        &self,
        Parameters(input): Parameters<ViewIdInput>,
    ) -> CallToolResult {
        match self.service.latest_selection(&input.view_id) {
            Ok(selection) => bounded_serializable(selection),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Convert an exact durable human/viewer selection into a bounded scientific dataset query and provenance-linked result"
    )]
    async fn query_selection(
        &self,
        Parameters(input): Parameters<QuerySelectionInput>,
    ) -> CallToolResult {
        match self
            .service
            .query_selection(
                &input.selection_id,
                input.dataset_id.as_deref(),
                input.point_limit,
            )
            .await
        {
            Ok(result) => bounded_serializable(result),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Clear all structured selections associated with a visualization")]
    async fn clear_selection(&self, Parameters(input): Parameters<ViewIdInput>) -> CallToolResult {
        match self.service.clear_selections(&input.view_id) {
            Ok(count) => bounded_result(json!({"view_id": input.view_id, "removed": count})),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Create a bounded, regenerable Rerun recording for a semantic EcoScope view"
    )]
    async fn render_view(&self, Parameters(input): Parameters<ViewIdInput>) -> CallToolResult {
        let destination = self
            .service
            .paths()
            .views_dir
            .join(format!("{}.rrd", input.view_id));
        match ecoscope_rerun::write_recording(&self.service, &input.view_id, &destination) {
            Ok(_) => bounded_result(json!({
                "view_id": input.view_id,
                "artifact": format!("ecoscope://views/{}/recording", input.view_id),
                "renderer": "rerun",
                "status": "ready"
            })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Open a loopback-only browser explorer with the Rerun Web Viewer and semantic selection bridge"
    )]
    async fn open_view(&self, Parameters(input): Parameters<ViewIdInput>) -> CallToolResult {
        if let Err(error) = self.service.get_view(&input.view_id) {
            return tool_error(error);
        }
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => return tool_error(EcoScopeError::Io(error)),
        };
        let child = std::process::Command::new(executable)
            .args(["serve", &input.view_id, "--port", "0", "--open"])
            .env("ECOSCOPE_DATA_DIR", &self.service.paths().data_dir)
            .env("ECOSCOPE_CACHE_DIR", &self.service.paths().cache_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match child {
            Ok(_) => bounded_result(json!({
                "view_id": input.view_id,
                "status": "opening",
                "renderer": "rerun_web_viewer",
                "selection_bridge": "active"
            })),
            Err(error) => tool_error(EcoScopeError::Internal(format!(
                "could not start browser explorer: {error}"
            ))),
        }
    }

    #[tool(description = "Render and open an EcoScope view in the installed native Rerun viewer")]
    async fn open_native_view(&self, Parameters(input): Parameters<ViewIdInput>) -> CallToolResult {
        let destination = self
            .service
            .paths()
            .views_dir
            .join(format!("{}.rrd", input.view_id));
        if let Err(error) =
            ecoscope_rerun::write_recording(&self.service, &input.view_id, &destination)
        {
            return tool_error(error);
        }
        match ecoscope_rerun::open_recording(&destination) {
            Ok(()) => bounded_result(json!({
                "view_id": input.view_id,
                "status": "opened",
                "renderer": "rerun"
            })),
            Err(error) => tool_error(error),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EcoScopeMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "EcoScope discovers, plans, materializes, queries, visualizes, selects, and cites ecological data. Always plan and inspect before materializing. Never request credentials or local file paths in conversation.",
        )
    }
}

pub async fn run_stdio(service: EcoScopeService) -> anyhow::Result<()> {
    let server = EcoScopeMcp::new(service)
        .serve(rmcp::transport::stdio())
        .await?;
    server.waiting().await?;
    Ok(())
}

fn bounded_serializable(value: impl serde::Serialize) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(value) => bounded_result(value),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(format!(
            "serialization failed: {error}"
        ))]),
    }
}

fn bounded_result(value: Value) -> CallToolResult {
    match serde_json::to_vec(&value) {
        Ok(bytes) if bytes.len() <= MAX_MCP_RESULT_BYTES => CallToolResult::structured(value),
        Ok(bytes) => CallToolResult::error(vec![ContentBlock::text(format!(
            "result is {} bytes, above EcoScope's {} byte MCP limit; narrow the query or use an artifact handle",
            bytes.len(),
            MAX_MCP_RESULT_BYTES
        ))]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
    }
}

fn tool_error(error: EcoScopeError) -> CallToolResult {
    let code = match error {
        EcoScopeError::CredentialsRequired(_) => "NEON_CONNECTION_REQUIRED",
        EcoScopeError::NotFound(_) => "NOT_FOUND",
        EcoScopeError::Conflict(_) => "CONFLICT",
        EcoScopeError::Invalid(_) => "INVALID_REQUEST",
        _ => "ECOSCOPE_ERROR",
    };
    CallToolResult::error(vec![ContentBlock::text(format!("{code}: {error}"))])
}

fn load_neon_token() -> Option<String> {
    if let Ok(token) = std::env::var("NEON_API_TOKEN")
        && !token.trim().is_empty()
    {
        return Some(token);
    }
    // Unit tests must never trigger an operating-system credential prompt.
    // Tests that need a connection can opt in with NEON_API_TOKEN.
    #[cfg(test)]
    return None;

    #[cfg(not(test))]
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_NEON_USER)
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .filter(|token| !token.trim().is_empty())
}

fn fail_job(service: &EcoScopeService, job_id: &str, error: String) {
    if let Ok(mut job) = service.get_job(job_id) {
        if job.status == JobStatus::Cancelled {
            return;
        }
        job.status = JobStatus::Failed;
        job.error = Some(error);
        job.updated_at = Utc::now();
        let _ = service.save_job(&job);
    }
}

pub fn store_neon_token(token: &str) -> Result<(), EcoScopeError> {
    if token.trim().len() < 8 {
        return Err(EcoScopeError::Invalid(
            "the NEON token appears to be empty or incomplete".into(),
        ));
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_NEON_USER)
        .map_err(|error| EcoScopeError::Internal(format!("keychain error: {error}")))?;
    entry
        .set_password(token.trim())
        .map_err(|error| EcoScopeError::Internal(format!("keychain error: {error}")))
}

pub fn remove_neon_token() -> Result<(), EcoScopeError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_NEON_USER)
        .map_err(|error| EcoScopeError::Internal(format!("keychain error: {error}")))?;
    entry
        .delete_credential()
        .map_err(|error| EcoScopeError::Internal(format!("keychain error: {error}")))
}

pub fn neon_connected() -> bool {
    load_neon_token().is_some()
}

#[cfg(test)]
mod tests {
    use ecoscope_service::ServicePaths;
    use rmcp::{
        ClientHandler,
        model::{CallToolRequestParams, ClientInfo},
    };

    use super::*;

    #[derive(Debug, Clone, Default)]
    struct SmokeClient;

    impl ClientHandler for SmokeClient {
        fn get_info(&self) -> ClientInfo {
            ClientInfo::default()
        }
    }

    #[tokio::test]
    async fn registered_client_can_discover_and_call_tools() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let service = EcoScopeService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))?;
        let registry = EcoScopeMcp::new(service.clone());
        for provider_id in ["emso", "icos-erddap", "euro-argo"] {
            let provider = registry.provider(provider_id).await?;
            assert_eq!(provider.provider_id(), provider_id);
        }
        let table_path = directory.path().join("observations.csv");
        std::fs::write(&table_path, "site,value\nHARV,1\nHARV,3\nABBY,8\n")?;
        let dataset = service.import_local_file(&table_path).await?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            EcoScopeMcp::new(service)
                .serve(server_transport)
                .await?
                .waiting()
                .await?;
            anyhow::Ok(())
        });
        let client = SmokeClient.serve(client_transport).await?;

        let listed = client.list_tools(None).await?;
        let names = listed
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        for expected in [
            "health",
            "list_providers",
            "search_catalog",
            "inspect_resource",
            "plan_dataset",
            "plan_materialization",
            "materialize_dataset",
            "query_dataset",
            "inspect_result",
            "export_result",
            "create_view",
            "inspect_view",
            "inspect_selection",
            "query_selection",
            "render_view",
            "open_view",
            "configure_hyperspectral_view",
            "configure_cube_view",
        ] {
            assert!(names.contains(&expected), "missing MCP tool {expected}");
        }

        let health = client
            .call_tool(CallToolRequestParams::new("health"))
            .await?;
        assert_ne!(health.is_error, Some(true));
        assert_eq!(
            health
                .structured_content
                .as_ref()
                .and_then(|value| value.get("mcp_spec"))
                .and_then(serde_json::Value::as_str),
            Some("2026-07-28")
        );

        let providers = client
            .call_tool(CallToolRequestParams::new("list_providers"))
            .await?;
        assert_ne!(providers.is_error, Some(true));
        let listed_providers = providers
            .structured_content
            .as_ref()
            .and_then(|value| value.get("providers"))
            .and_then(Value::as_array)
            .expect("provider list");
        for provider_id in ["emso", "icos-erddap", "euro-argo"] {
            let provider = listed_providers
                .iter()
                .find(|provider| provider["provider_id"] == provider_id)
                .unwrap_or_else(|| panic!("missing built-in provider {provider_id}"));
            for capability in [
                "catalog_search",
                "resource_resolve",
                "asset_plan",
                "asset_fetch",
                "citation_resolve",
                "policy_evaluate",
            ] {
                assert!(
                    provider["capabilities"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(|item| item == capability)),
                    "{provider_id} is missing {capability}"
                );
            }
        }
        assert!(
            listed_providers.iter().all(|provider| {
                provider.get("schema_version").and_then(Value::as_u64) == Some(2)
            })
        );

        let query = client
            .call_tool(
                CallToolRequestParams::new("query_dataset").with_arguments(
                    json!({
                        "dataset_id": dataset.dataset_id,
                        "query": {
                            "kind": "table",
                            "group_by": ["site"],
                            "aggregates": [{
                                "field": "value",
                                "function": "mean",
                                "alias": "mean_value"
                            }],
                            "limit": 100
                        }
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            )
            .await?;
        assert_ne!(query.is_error, Some(true));
        let result_id = query
            .structured_content
            .as_ref()
            .and_then(|value| value.get("result_id"))
            .and_then(Value::as_str)
            .expect("query result ID");
        let exact_rows = client
            .call_tool(
                CallToolRequestParams::new("query_dataset").with_arguments(
                    json!({
                        "dataset_id": dataset.dataset_id,
                        "query": {
                            "kind": "source_rows",
                            "source_indices": [2, 0],
                            "select": ["site", "value"]
                        }
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            )
            .await?;
        assert_ne!(exact_rows.is_error, Some(true));
        let exact_preview = exact_rows
            .structured_content
            .as_ref()
            .and_then(|value| value.get("preview"))
            .expect("exact source rows preview");
        assert_eq!(exact_preview["rows"][0]["source_index"], 2);
        assert_eq!(exact_preview["rows"][0]["values"]["site"], "ABBY");
        assert_eq!(exact_preview["rows"][1]["source_index"], 0);
        assert_eq!(exact_preview["rows"][1]["values"]["value"], "1");
        let export = client
            .call_tool(
                CallToolRequestParams::new("export_result").with_arguments(
                    json!({
                        "result_id": result_id,
                        "format": "parquet"
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            )
            .await?;
        assert_ne!(export.is_error, Some(true));
        assert!(
            export
                .structured_content
                .as_ref()
                .and_then(|value| value.get("artifact"))
                .and_then(Value::as_str)
                .is_some_and(|artifact| artifact.starts_with("ecoscope://exports/"))
        );

        client.cancel().await?;
        server.await??;
        Ok(())
    }
}
