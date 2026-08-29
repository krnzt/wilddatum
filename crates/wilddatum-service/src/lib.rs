//! Persistent local state and scientific asset catalog.

mod cube;
mod point_cloud;
mod profile_trajectory;
mod query;
mod selection;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::Utc;
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use wilddatum_core::{
    CitationMetadata, DatasetId, DatasetManifest, DatasetPlan, EcoLayer, EcoViewSpec, JobId,
    JobRecord, JobStatus, LicenseMetadata, LinkedGroup, LocalAssetInspection,
    MAX_RENDERED_POINT_CLOUD_POINTS, Modality, PINNED_RERUN_VERSION, ProviderKind, Result,
    SelectionId, SelectionRecord, SemanticSelection, SourceFile, ViewId, ViewLayout,
    WildDatumError,
};

#[derive(Debug, Clone)]
pub struct ServicePaths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_db: PathBuf,
    pub objects_dir: PathBuf,
    pub views_dir: PathBuf,
    pub results_dir: PathBuf,
    pub exports_dir: PathBuf,
    pub derived_dir: PathBuf,
    /// Explicitly installed language-neutral provider subprocess contracts.
    pub providers_dir: PathBuf,
}

impl ServicePaths {
    pub fn discover() -> Result<Self> {
        if let Some(data_dir) =
            std::env::var_os("WILDDATUM_DATA_DIR").or_else(|| std::env::var_os("ECOSCOPE_DATA_DIR"))
        {
            let data_dir = PathBuf::from(data_dir);
            let cache_dir = std::env::var_os("WILDDATUM_CACHE_DIR")
                .or_else(|| std::env::var_os("ECOSCOPE_CACHE_DIR"))
                .map(PathBuf::from)
                .unwrap_or_else(|| data_dir.join("cache"));
            return Ok(Self::under(data_dir, cache_dir));
        }
        let dirs = ProjectDirs::from("org", "WildDatum", "WildDatum").ok_or_else(|| {
            WildDatumError::Internal("cannot determine user data directory".into())
        })?;
        let legacy = ProjectDirs::from("org", "EcoScope", "EcoScope");
        if !dirs.data_dir().join("state.db").is_file()
            && let Some(legacy) = legacy
            && legacy.data_dir().join("state.db").is_file()
        {
            return Ok(Self::under(legacy.data_dir(), legacy.cache_dir()));
        }
        Ok(Self::under(dirs.data_dir(), dirs.cache_dir()))
    }

    pub fn provider_objects_dir(&self, provider: &ProviderKind) -> PathBuf {
        self.objects_dir.join(provider.object_namespace())
    }

    pub fn under(data_dir: impl AsRef<Path>, cache_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref().to_path_buf();
        let cache_dir = cache_dir.as_ref().to_path_buf();
        Self {
            state_db: data_dir.join("state.db"),
            objects_dir: data_dir.join("objects"),
            views_dir: data_dir.join("views"),
            results_dir: data_dir.join("results"),
            exports_dir: data_dir.join("exports"),
            derived_dir: data_dir.join("derived"),
            providers_dir: data_dir.join("providers"),
            data_dir,
            cache_dir,
        }
    }

    pub fn initialize(&self) -> Result<()> {
        for path in [
            &self.data_dir,
            &self.cache_dir,
            &self.objects_dir,
            &self.views_dir,
            &self.results_dir,
            &self.exports_dir,
            &self.derived_dir,
            &self.providers_dir,
        ] {
            std::fs::create_dir_all(path)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct WildDatumService {
    paths: ServicePaths,
    connection: Arc<Mutex<Connection>>,
}

impl WildDatumService {
    pub fn open(paths: ServicePaths) -> Result<Self> {
        paths.initialize()?;
        let connection = Connection::open(&paths.state_db).map_err(db_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(db_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(db_error)?;
        let service = Self {
            paths,
            connection: Arc::new(Mutex::new(connection)),
        };
        service.migrate()?;
        Ok(service)
    }

    pub fn discover() -> Result<Self> {
        Self::open(ServicePaths::discover()?)
    }

    pub fn paths(&self) -> &ServicePaths {
        &self.paths
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| WildDatumError::Internal("state database lock was poisoned".into()))
    }

    fn migrate(&self) -> Result<()> {
        self.connection()?
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS plans (
                    id TEXT PRIMARY KEY,
                    json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS datasets (
                    id TEXT PRIMARY KEY,
                    json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS local_assets (
                    id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    inspection_json TEXT NOT NULL,
                    fingerprint TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS jobs (
                    id TEXT PRIMARY KEY,
                    json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS views (
                    id TEXT PRIMARY KEY,
                    revision INTEGER NOT NULL,
                    json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS selections (
                    id TEXT PRIMARY KEY,
                    view_id TEXT NOT NULL,
                    revision INTEGER NOT NULL,
                    json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS selections_by_view
                    ON selections(view_id, revision DESC);
                CREATE TABLE IF NOT EXISTS results (
                    id TEXT PRIMARY KEY,
                    json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS exports (
                    id TEXT PRIMARY KEY,
                    json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS derived_assets (
                    id TEXT PRIMARY KEY,
                    dataset_id TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    source_fingerprint TEXT NOT NULL,
                    artifact_uri TEXT NOT NULL,
                    json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS derived_assets_by_dataset
                    ON derived_assets(dataset_id, kind);
                CREATE TABLE IF NOT EXISTS format_mappings (
                    id TEXT PRIMARY KEY,
                    dataset_id TEXT NOT NULL,
                    json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS query_stats (
                    result_id TEXT PRIMARY KEY,
                    scanned_rows INTEGER,
                    returned_rows INTEGER,
                    elapsed_ms INTEGER NOT NULL,
                    json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                INSERT INTO metadata(key, value) VALUES('schema_version', '3')
                    ON CONFLICT(key) DO UPDATE SET value=excluded.value;
                ",
            )
            .map_err(db_error)
    }

    pub fn save_plan(&self, plan: &DatasetPlan) -> Result<()> {
        self.put_json("plans", &plan.plan_id.0, plan, plan.created_at.to_rfc3339())
    }

    pub fn get_plan(&self, plan_id: &str) -> Result<DatasetPlan> {
        self.get_json("plans", plan_id)
    }

    pub fn approve_plan(&self, plan_id: &str, expected_hash: &str) -> Result<DatasetPlan> {
        let mut plan = self.get_plan(plan_id)?;
        if plan.plan_hash != expected_hash {
            return Err(WildDatumError::Conflict(
                "plan hash changed; inspect the plan again before approval".into(),
            ));
        }
        if plan.files.is_empty() {
            return Err(WildDatumError::Invalid(
                "the plan has no resolved files; connect credentials and plan again".into(),
            ));
        }
        plan.approved_at = Some(Utc::now());
        self.save_plan(&plan)?;
        Ok(plan)
    }

    pub fn save_manifest(&self, manifest: &DatasetManifest) -> Result<()> {
        self.put_json(
            "datasets",
            &manifest.dataset_id.0,
            manifest,
            manifest.created_at.to_rfc3339(),
        )
    }

    pub fn enrich_manifest_metadata(&self, manifest: &mut DatasetManifest) {
        for source in &mut manifest.source_files {
            let extension = Path::new(&source.original_name)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !matches!(extension.as_str(), "h5" | "hdf5") {
                continue;
            }
            let Some(object) = &source.local_object else {
                continue;
            };
            let path = match self.provider_object_path(&manifest.provider, object) {
                Ok(path) => path,
                Err(error) => {
                    source.metadata.insert(
                        "structure_inspection_error".into(),
                        json!(error.to_string()),
                    );
                    continue;
                }
            };
            match wilddatum_local_import::inspect_hdf5_structure(&path) {
                Ok(structure) => {
                    source
                        .metadata
                        .insert("dimensions".into(), json!(structure.dimensions));
                    source
                        .metadata
                        .insert("hdf5_datasets".into(), structure.datasets);
                }
                Err(error) => {
                    source.metadata.insert(
                        "structure_inspection_error".into(),
                        json!(error.to_string()),
                    );
                }
            }
        }
    }

    pub fn get_manifest(&self, dataset_id: &str) -> Result<DatasetManifest> {
        self.get_json("datasets", dataset_id)
    }

    pub fn list_manifests(&self) -> Result<Vec<DatasetManifest>> {
        self.list_json("datasets", "created_at DESC")
    }

    pub async fn import_local_file(&self, path: &Path) -> Result<DatasetManifest> {
        let inspection = wilddatum_local_import::inspect_path(path).await?;
        self.store_local_asset(path, &inspection)?;
        let manifest = DatasetManifest {
            dataset_id: DatasetId::new(),
            provider: ProviderKind::Local,
            resource_id: inspection.display_name.clone(),
            resource_version: Some(inspection.fingerprint.value.clone()),
            modalities: inspection.modalities.clone(),
            locations: vec![],
            temporal_start: None,
            temporal_end: None,
            release: None,
            package: None,
            include_provisional: false,
            source_files: vec![SourceFile {
                asset_id: inspection.asset_id.clone(),
                original_name: inspection.display_name.clone(),
                source_uri: format!("local://{}", inspection.asset_id),
                local_object: None,
                size_bytes: inspection.size_bytes,
                checksum: inspection.fingerprint.clone(),
                media_type: inspection.media_type.clone(),
                location: None,
                temporal_partition: None,
                metadata: inspection.metadata.clone(),
            }],
            transformations: vec![],
            format: Some(wilddatum_core::FormatDescriptor {
                name: inspection.format.clone(),
                version: None,
                profile: inspection
                    .metadata
                    .get("recommended_internal_format")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                options: BTreeMap::new(),
            }),
            spatial_reference: inspection.crs.as_ref().map(|crs| {
                let (authority, code) = crs
                    .split_once(':')
                    .map_or((None, Some(crs.clone())), |(authority, code)| {
                        (Some(authority.to_owned()), Some(code.to_owned()))
                    });
                wilddatum_core::SpatialReference {
                    authority,
                    code,
                    wkt: None,
                    axis_order: vec![],
                }
            }),
            cube: None,
            cubes: inspection
                .metadata
                .get("cube_descriptors")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default(),
            license: Some(LicenseMetadata {
                name: "User supplied".into(),
                url: None,
                attribution_required: false,
            }),
            citation: Some(CitationMetadata {
                text: format!("Local source: {}", inspection.display_name),
                doi: None,
                url: None,
            }),
            provider_metadata: BTreeMap::new(),
            created_at: Utc::now(),
        };
        self.save_manifest(&manifest)?;
        Ok(manifest)
    }

    fn store_local_asset(&self, path: &Path, inspection: &LocalAssetInspection) -> Result<()> {
        let json = serde_json::to_string(inspection)?;
        self.connection()?
            .execute(
                "INSERT INTO local_assets(id, path, inspection_json, fingerprint, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                   path=excluded.path,
                   inspection_json=excluded.inspection_json,
                   fingerprint=excluded.fingerprint",
                params![
                    inspection.asset_id.0,
                    path.to_string_lossy(),
                    json,
                    inspection.fingerprint.value,
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn inspect_asset(&self, asset_id: &str) -> Result<LocalAssetInspection> {
        let text = self
            .connection()?
            .query_row(
                "SELECT inspection_json FROM local_assets WHERE id=?1",
                params![asset_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| WildDatumError::NotFound(format!("asset {asset_id}")))?;
        serde_json::from_str(&text).map_err(WildDatumError::from)
    }

    pub fn validate_local_asset(&self, asset_id: &str) -> Result<bool> {
        let (path, expected): (String, String) = self
            .connection()?
            .query_row(
                "SELECT path, fingerprint FROM local_assets WHERE id=?1",
                params![asset_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| WildDatumError::NotFound(format!("asset {asset_id}")))?;
        let fingerprint = wilddatum_local_import::fingerprint_path(Path::new(&path))?;
        Ok(fingerprint.value == expected)
    }

    /// Resolve an opaque local asset for a trusted in-process renderer.
    ///
    /// This path must never be serialized into an MCP result or browser response.
    pub fn local_asset_path_for_renderer(&self, asset_id: &str) -> Result<PathBuf> {
        self.connection()?
            .query_row(
                "SELECT path FROM local_assets WHERE id=?1",
                params![asset_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?
            .map(PathBuf::from)
            .ok_or_else(|| WildDatumError::NotFound(format!("asset {asset_id}")))
    }

    /// Resolve a source for a trusted in-process adapter without exposing its path
    /// through MCP or the browser API.
    pub fn source_path_for_renderer(
        &self,
        manifest: &DatasetManifest,
        source: &SourceFile,
    ) -> Result<PathBuf> {
        if let Some(object) = &source.local_object {
            return self.provider_object_path(&manifest.provider, object);
        }
        self.local_asset_path_for_renderer(&source.asset_id.0)
    }

    fn provider_object_path(&self, provider: &ProviderKind, object: &str) -> Result<PathBuf> {
        let mut components = Path::new(object).components();
        let safe_name = match (components.next(), components.next()) {
            (Some(std::path::Component::Normal(name)), None) => name,
            _ => {
                return Err(WildDatumError::Invalid(
                    "provider object reference must be one opaque filename".into(),
                ));
            }
        };
        Ok(self.paths.provider_objects_dir(provider).join(safe_name))
    }

    pub fn preview_dataset(&self, dataset_id: &str, limit: u32) -> Result<Value> {
        let manifest = self.get_manifest(dataset_id)?;
        let source = manifest
            .source_files
            .first()
            .ok_or_else(|| WildDatumError::Invalid("dataset has no source files".into()))?;
        let path = self.source_path_for_renderer(&manifest, source)?;
        let delimiter = if source.original_name.to_ascii_lowercase().ends_with(".tsv") {
            b'\t'
        } else {
            b','
        };
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .from_path(&path)
            .map_err(|error| WildDatumError::Invalid(error.to_string()))?;
        let headers = reader
            .headers()
            .map_err(|error| WildDatumError::Invalid(error.to_string()))?
            .iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        for record in reader.records().take(limit.min(2000) as usize) {
            let record = record.map_err(|error| WildDatumError::Invalid(error.to_string()))?;
            rows.push(
                headers
                    .iter()
                    .zip(record.iter())
                    .map(|(key, value)| (key.clone(), Value::String(value.into())))
                    .collect::<serde_json::Map<_, _>>(),
            );
        }
        Ok(json!({
            "dataset_id": dataset_id,
            "columns": headers,
            "rows": rows,
            "returned_rows": rows.len(),
            "truncated": rows.len() == limit.min(2000) as usize
        }))
    }

    pub fn create_job(&self, kind: impl Into<String>) -> Result<JobRecord> {
        let now = Utc::now();
        let job = JobRecord {
            job_id: JobId::new(),
            kind: kind.into(),
            status: JobStatus::Queued,
            progress: 0.0,
            message: None,
            result: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        self.save_job(&job)?;
        Ok(job)
    }

    pub fn save_job(&self, job: &JobRecord) -> Result<()> {
        self.put_json("jobs", &job.job_id.0, job, job.updated_at.to_rfc3339())
    }

    pub fn get_job(&self, job_id: &str) -> Result<JobRecord> {
        self.get_json("jobs", job_id)
    }

    pub fn cancel_job(&self, job_id: &str) -> Result<JobRecord> {
        let mut job = self.get_job(job_id)?;
        if matches!(job.status, JobStatus::Succeeded | JobStatus::Failed) {
            return Err(WildDatumError::Conflict(
                "completed jobs cannot be cancelled".into(),
            ));
        }
        job.status = JobStatus::Cancelled;
        job.updated_at = Utc::now();
        self.save_job(&job)?;
        Ok(job)
    }

    pub fn create_view(&self, name: String, dataset_ids: Vec<DatasetId>) -> Result<EcoViewSpec> {
        for dataset_id in &dataset_ids {
            self.get_manifest(&dataset_id.0)?;
        }
        let mut layers = Vec::new();
        for (index, dataset_id) in dataset_ids.iter().enumerate() {
            let manifest = self.get_manifest(&dataset_id.0)?;
            let modality = primary_view_modality(&manifest.modalities);
            let mut encoding = BTreeMap::new();
            if matches!(modality, Modality::PointCloud)
                && let Some(origin) = manifest
                    .source_files
                    .first()
                    .and_then(|source| source.metadata.get("bounds"))
                    .and_then(|bounds| bounds.get("min"))
            {
                encoding.insert("coordinate_origin".into(), origin.clone());
                encoding.insert("coordinate_space".into(), json!("source"));
                if let Some(scale) = manifest
                    .source_files
                    .first()
                    .and_then(|source| source.metadata.get("coordinate_scale"))
                {
                    encoding.insert("coordinate_scale".into(), scale.clone());
                }
                if let Some(point_count) = manifest
                    .source_files
                    .first()
                    .and_then(|source| source.metadata.get("point_count"))
                    .and_then(Value::as_u64)
                {
                    let stride = point_count.div_ceil(MAX_RENDERED_POINT_CLOUD_POINTS).max(1);
                    encoding.insert("sampling_stride".into(), json!(stride));
                    encoding.insert(
                        "instance_id_mapping".into(),
                        json!({
                            "kind": "source_stream_stride",
                            "stride": stride,
                        "rerun_version": PINNED_RERUN_VERSION
                        }),
                    );
                }
            }
            if modality == Modality::Vector
                && let Some(bounds) = manifest
                    .source_files
                    .first()
                    .and_then(|source| source.metadata.get("bounds"))
                    .and_then(Value::as_array)
                && bounds.len() >= 2
            {
                encoding.insert(
                    "coordinate_origin".into(),
                    json!([bounds[0].clone(), bounds[1].clone()]),
                );
                encoding.insert("coordinate_space".into(), json!("source"));
            }
            if modality == Modality::Raster
                && let Some(transform) = manifest
                    .source_files
                    .first()
                    .and_then(|source| source.metadata.get("affine_transform"))
            {
                encoding.insert("affine_transform".into(), transform.clone());
                encoding.insert("preview_stride".into(), json!([1, 1]));
            }
            if matches!(modality, Modality::Raster | Modality::Vector)
                && let Some(spatial_reference) = &manifest.spatial_reference
            {
                let crs = match (&spatial_reference.authority, &spatial_reference.code) {
                    (Some(authority), Some(code)) => format!("{authority}:{code}"),
                    (_, Some(code)) => code.clone(),
                    _ => "source".into(),
                };
                encoding.insert("crs".into(), json!(crs));
            }
            if matches!(modality, Modality::Hyperspectral | Modality::Tensor)
                && manifest.cubes.len() == 1
                && manifest.cubes[0].axes.len() == 3
            {
                let descriptor = &manifest.cubes[0];
                let axis_for = |role: wilddatum_core::AxisRole| {
                    descriptor.axes.iter().position(|axis| axis.role == role)
                };
                if let (Some(y_axis), Some(x_axis), Some(spectral_axis)) = (
                    axis_for(wilddatum_core::AxisRole::Y),
                    axis_for(wilddatum_core::AxisRole::X),
                    axis_for(wilddatum_core::AxisRole::Spectral),
                ) {
                    encoding.insert("cube_array".into(), json!(descriptor.array_path));
                    encoding.insert("y_axis".into(), json!(y_axis));
                    encoding.insert("x_axis".into(), json!(x_axis));
                    encoding.insert("spectral_axis".into(), json!(spectral_axis));
                }
            }
            layers.push(EcoLayer {
                id: format!("layer_{}", index + 1),
                dataset_id: dataset_id.clone(),
                name: manifest.resource_id,
                modality,
                visible: true,
                opacity: 1.0,
                encoding,
            });
        }
        let spatial_layers = layers
            .iter()
            .filter(|layer| {
                matches!(
                    layer.modality,
                    Modality::PointCloud
                        | Modality::Raster
                        | Modality::Vector
                        | Modality::Hyperspectral
                        | Modality::Image
                )
            })
            .map(|layer| layer.id.clone())
            .collect::<Vec<_>>();
        let temporal_layers = layers
            .iter()
            .filter(|layer| layer.modality == Modality::TimeSeries)
            .map(|layer| layer.id.clone())
            .collect::<Vec<_>>();
        let mut linked_groups = Vec::new();
        if spatial_layers.len() > 1 {
            linked_groups.push(LinkedGroup {
                id: "linked_space".into(),
                layer_ids: spatial_layers,
                dimensions: vec!["x".into(), "y".into()],
            });
        }
        if temporal_layers.len() > 1 {
            linked_groups.push(LinkedGroup {
                id: "linked_time".into(),
                layer_ids: temporal_layers,
                dimensions: vec!["time".into()],
            });
        }
        let view = EcoViewSpec {
            version: 1,
            view_id: ViewId::new(),
            revision: 1,
            name,
            dataset_ids,
            layout: ViewLayout::Grid { columns: 2 },
            layers,
            filters: vec![],
            linked_groups,
            camera: None,
            active_timeline: None,
            provenance_visible: true,
        };
        self.save_view(&view)?;
        Ok(view)
    }

    pub fn save_view(&self, view: &EcoViewSpec) -> Result<()> {
        let text = serde_json::to_string(view)?;
        self.connection()?
            .execute(
                "INSERT INTO views(id, revision, json, updated_at) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                   revision=excluded.revision,
                   json=excluded.json,
                   updated_at=excluded.updated_at",
                params![
                    view.view_id.0,
                    view.revision as i64,
                    text,
                    Utc::now().to_rfc3339()
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn get_view(&self, view_id: &str) -> Result<EcoViewSpec> {
        self.get_json("views", view_id)
    }

    pub fn patch_view(
        &self,
        view_id: &str,
        expected_revision: u64,
        patch: Value,
    ) -> Result<EcoViewSpec> {
        let current = self.get_view(view_id)?;
        if current.revision != expected_revision {
            return Err(WildDatumError::Conflict(format!(
                "view revision is {}, expected {expected_revision}",
                current.revision
            )));
        }
        let mut value = serde_json::to_value(&current)?;
        merge_patch(&mut value, patch);
        value["view_id"] = Value::String(current.view_id.0.clone());
        value["version"] = Value::from(current.version);
        value["revision"] = Value::from(current.revision + 1);
        let updated: EcoViewSpec = serde_json::from_value(value).map_err(|error| {
            WildDatumError::Invalid(format!("view patch does not produce a valid spec: {error}"))
        })?;
        self.save_view(&updated)?;
        Ok(updated)
    }

    pub fn configure_layer_encoding(
        &self,
        view_id: &str,
        expected_revision: u64,
        layer_id: &str,
        mut encoding: BTreeMap<String, Value>,
    ) -> Result<EcoViewSpec> {
        let mut view = self.get_view(view_id)?;
        if view.revision != expected_revision {
            return Err(WildDatumError::Conflict(format!(
                "view revision is {}, expected {expected_revision}",
                view.revision
            )));
        }
        let layer = view
            .layers
            .iter_mut()
            .find(|layer| layer.id == layer_id)
            .ok_or_else(|| WildDatumError::NotFound(format!("layer {layer_id}")))?;
        if !matches!(layer.modality, Modality::Hyperspectral | Modality::Tensor) {
            return Err(WildDatumError::Invalid(format!(
                "layer {layer_id} is not hyperspectral or tensor data"
            )));
        }
        if let Some(dataset_path) = encoding
            .get("cube_array")
            .or_else(|| encoding.get("hdf5_dataset"))
            .and_then(Value::as_str)
            && let Some(descriptor) = self
                .get_manifest(&layer.dataset_id.0)?
                .cubes
                .into_iter()
                .find(|descriptor| descriptor.array_path == dataset_path)
            && descriptor.axes.len() == 3
        {
            let y_axis = encoding.get("y_axis").and_then(Value::as_u64).unwrap_or(0) as usize;
            let x_axis = encoding.get("x_axis").and_then(Value::as_u64).unwrap_or(1) as usize;
            let shape = descriptor
                .axes
                .iter()
                .map(|axis| axis.length)
                .collect::<Vec<_>>();
            let y = shape.get(y_axis).copied().unwrap_or(0);
            let x = shape.get(x_axis).copied().unwrap_or(0);
            const MAX_PREVIEW_EDGE: u64 = 1_024;
            encoding.insert("source_shape".into(), json!(shape));
            encoding.insert(
                "preview_stride".into(),
                json!([
                    y.div_ceil(MAX_PREVIEW_EDGE).max(1),
                    x.div_ceil(MAX_PREVIEW_EDGE).max(1)
                ]),
            );
        }
        layer.encoding = encoding;
        view.revision += 1;
        self.save_view(&view)?;
        Ok(view)
    }

    pub fn save_selection(
        &self,
        view_id: &str,
        selection: SemanticSelection,
        summary: Value,
    ) -> Result<SelectionRecord> {
        let view = self.get_view(view_id)?;
        let record = SelectionRecord {
            selection_id: SelectionId::new(),
            view_id: view.view_id,
            revision: view.revision,
            selection,
            summary,
            created_at: Utc::now(),
        };
        let text = serde_json::to_string(&record)?;
        self.connection()?
            .execute(
                "INSERT INTO selections(id, view_id, revision, json, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    record.selection_id.0,
                    record.view_id.0,
                    record.revision as i64,
                    text,
                    record.created_at.to_rfc3339()
                ],
            )
            .map_err(db_error)?;
        Ok(record)
    }

    pub fn latest_selection(&self, view_id: &str) -> Result<SelectionRecord> {
        let text = self
            .connection()?
            .query_row(
                "SELECT json FROM selections WHERE view_id=?1
                 ORDER BY created_at DESC LIMIT 1",
                params![view_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| WildDatumError::NotFound(format!("selection for view {view_id}")))?;
        serde_json::from_str(&text).map_err(WildDatumError::from)
    }

    pub fn clear_selections(&self, view_id: &str) -> Result<u64> {
        let count = self
            .connection()?
            .execute("DELETE FROM selections WHERE view_id=?1", params![view_id])
            .map_err(db_error)?;
        Ok(count as u64)
    }

    pub fn health(&self) -> Result<Value> {
        let schema_version: String = self
            .connection()?
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        Ok(json!({
            "status": "ok",
            "schema_version": schema_version,
            "data_dir": self.paths.data_dir,
            "objects_dir": self.paths.objects_dir,
        }))
    }

    fn put_json<T: Serialize>(
        &self,
        table: &str,
        id: &str,
        value: &T,
        timestamp: String,
    ) -> Result<()> {
        debug_assert!(matches!(
            table,
            "plans" | "datasets" | "jobs" | "results" | "exports"
        ));
        let timestamp_column = if table == "jobs" {
            "updated_at"
        } else {
            "created_at"
        };
        let sql = format!(
            "INSERT INTO {table}(id, json, {timestamp_column}) VALUES(?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET json=excluded.json, {timestamp_column}=excluded.{timestamp_column}"
        );
        self.connection()?
            .execute(&sql, params![id, serde_json::to_string(value)?, timestamp])
            .map_err(db_error)?;
        Ok(())
    }

    fn get_json<T: DeserializeOwned>(&self, table: &str, id: &str) -> Result<T> {
        debug_assert!(matches!(
            table,
            "plans" | "datasets" | "jobs" | "views" | "results" | "exports"
        ));
        let sql = format!("SELECT json FROM {table} WHERE id=?1");
        let text = self
            .connection()?
            .query_row(&sql, params![id], |row| row.get::<_, String>(0))
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| WildDatumError::NotFound(format!("{table} {id}")))?;
        serde_json::from_str(&text).map_err(WildDatumError::from)
    }

    fn list_json<T: DeserializeOwned>(&self, table: &str, order: &str) -> Result<Vec<T>> {
        debug_assert!(matches!(table, "datasets"));
        let sql = format!("SELECT json FROM {table} ORDER BY {order}");
        let connection = self.connection()?;
        let mut statement = connection.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db_error)?;
        let mut values = Vec::new();
        for row in rows {
            values.push(serde_json::from_str(&row.map_err(db_error)?)?);
        }
        Ok(values)
    }
}

fn primary_view_modality(modalities: &[Modality]) -> Modality {
    [
        Modality::PointCloud,
        Modality::Hyperspectral,
        Modality::Raster,
        Modality::Vector,
        Modality::Image,
        Modality::TimeSeries,
        Modality::Tabular,
        Modality::Tensor,
    ]
    .into_iter()
    .find(|candidate| modalities.contains(candidate))
    .unwrap_or(Modality::Unknown)
}

fn merge_patch(target: &mut Value, patch: Value) {
    match patch {
        Value::Object(patch_object) => {
            if !target.is_object() {
                *target = Value::Object(Default::default());
            }
            let target_object = target.as_object_mut().expect("object assigned above");
            for (key, value) in patch_object {
                if value.is_null() {
                    target_object.remove(&key);
                } else {
                    merge_patch(target_object.entry(key).or_insert(Value::Null), value);
                }
            }
        }
        replacement => *target = replacement,
    }
}

fn db_error(error: rusqlite::Error) -> WildDatumError {
    WildDatumError::Internal(format!("state database error: {error}"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn service() -> (tempfile::TempDir, WildDatumService) {
        let directory = tempfile::tempdir().unwrap();
        let service = WildDatumService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        (directory, service)
    }

    #[tokio::test]
    async fn imports_and_previews_a_local_table() {
        let (directory, service) = service();
        let path = directory.path().join("observations.csv");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "timestamp,site,value").unwrap();
        writeln!(file, "2025-01-01,HARV,1.5").unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        let preview = service.preview_dataset(&manifest.dataset_id.0, 10).unwrap();
        assert_eq!(preview["returned_rows"], 1);
        assert_eq!(service.list_manifests().unwrap().len(), 1);
    }

    #[test]
    fn reads_alpha_manifest_json_from_the_state_database() {
        let (_directory, service) = service();
        let legacy = json!({
            "dataset_id": "ds_legacy",
            "provider": "local",
            "product_code": "observations.csv",
            "product_revision": "blake3-alpha",
            "modalities": ["tabular"],
            "sites": [],
            "start_month": null,
            "end_month": null,
            "release": null,
            "package": null,
            "include_provisional": false,
            "source_files": [],
            "transformations": [],
            "format": null,
            "spatial_reference": null,
            "cube": null,
            "cubes": [],
            "license": null,
            "citation": null,
            "created_at": "2026-01-01T00:00:00Z"
        });
        service
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO datasets(id, json, created_at) VALUES(?1, ?2, ?3)",
                params!["ds_legacy", legacy.to_string(), "2026-01-01T00:00:00Z"],
            )
            .unwrap();

        let manifest = service.get_manifest("ds_legacy").unwrap();
        assert_eq!(manifest.resource_id, "observations.csv");
        assert!(manifest.locations.is_empty());
        assert!(manifest.provider_metadata.is_empty());
    }

    #[tokio::test]
    async fn previews_provider_objects_through_the_manifest_namespace() {
        let (directory, service) = service();
        let path = directory.path().join("observations.csv");
        std::fs::write(&path, "timestamp,site,value\n2025-01-01,HARV,1.5\n").unwrap();
        let mut manifest = service.import_local_file(&path).await.unwrap();
        manifest.provider = ProviderKind::Other("example-ri".into());
        manifest.source_files[0].local_object = Some("fixture-object".into());
        let object_dir = service.paths().provider_objects_dir(&manifest.provider);
        std::fs::create_dir_all(&object_dir).unwrap();
        std::fs::copy(&path, object_dir.join("fixture-object")).unwrap();
        service.save_manifest(&manifest).unwrap();

        let preview = service.preview_dataset(&manifest.dataset_id.0, 10).unwrap();
        assert_eq!(preview["returned_rows"], 1);
        assert_eq!(preview["rows"][0]["site"], "HARV");
    }

    #[tokio::test]
    async fn patches_views_with_revision_checks() {
        let (directory, service) = service();
        let path = directory.path().join("observations.csv");
        std::fs::write(&path, "value\n1\n").unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        let view = service
            .create_view("test".into(), vec![manifest.dataset_id])
            .unwrap();
        let patched = service
            .patch_view(&view.view_id.0, 1, json!({"name": "updated"}))
            .unwrap();
        assert_eq!(patched.revision, 2);
        assert!(matches!(
            service.patch_view(&view.view_id.0, 1, json!({"name": "stale"})),
            Err(WildDatumError::Conflict(_))
        ));
    }
}
