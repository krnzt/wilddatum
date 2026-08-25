//! Derived point-cloud indexes and safe internal resolution.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

use chrono::Utc;
use ecoscope_core::{ArtifactDescriptor, DatasetId, DerivedAssetRecord, EcoScopeError, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::EcoScopeService;

const COPC_KIND: &str = "copc_spatial_index";

impl EcoScopeService {
    /// Build a COPC octree used for full-resolution spatial filtering. The
    /// current copc-rs writer does not guarantee representative lower LODs, so
    /// derived indexes are explicitly marked as unsuitable for resolution queries.
    pub fn derive_copc_index(&self, dataset_id: &str) -> Result<DerivedAssetRecord> {
        let manifest = self.get_manifest(dataset_id)?;
        let source = manifest
            .source_files
            .first()
            .ok_or_else(|| EcoScopeError::Invalid("dataset has no source files".into()))?;
        let lower_name = source.original_name.to_ascii_lowercase();
        if lower_name.contains(".copc.") {
            return Err(EcoScopeError::Invalid(
                "the dataset is already a COPC source".into(),
            ));
        }
        if !lower_name.ends_with(".las") && !lower_name.ends_with(".laz") {
            return Err(EcoScopeError::Invalid(
                "COPC derivation requires a LAS or LAZ source".into(),
            ));
        }
        if let Some(existing) = self.find_derived_asset(dataset_id, COPC_KIND)?
            && self.derived_asset_path(&existing).is_file()
            && existing.source_fingerprint.value == source.checksum.value
        {
            return Ok(existing);
        }

        let source_path = self.source_path_for_renderer(&manifest, source)?;
        let mut reader = las::Reader::from_path(&source_path)
            .map_err(|error| EcoScopeError::Invalid(format!("cannot open LAS/LAZ: {error}")))?;
        let header = reader.header().clone();
        let point_count = i32::try_from(header.number_of_points()).map_err(|_| {
            EcoScopeError::Invalid(
                "copc-rs currently supports deriving indexes for at most i32::MAX points".into(),
            )
        })?;
        let derived_id = format!("derived_{}", uuid::Uuid::now_v7().simple());
        let temporary = tempfile::Builder::new()
            .prefix("ecoscope-")
            .suffix(".copc.laz")
            .tempfile_in(&self.paths().derived_dir)?;
        let temporary_path = temporary.path().to_path_buf();
        let read_error = Rc::new(RefCell::new(None::<String>));
        let read_error_for_iterator = read_error.clone();
        let points = reader.points().map_while(move |point| match point {
            Ok(point) => Some(point),
            Err(error) => {
                *read_error_for_iterator.borrow_mut() = Some(error.to_string());
                None
            }
        });
        let mut writer = copc_rs::CopcWriter::from_path(&temporary_path, header, 256, 16_384)
            .map_err(|error| {
                EcoScopeError::Internal(format!("cannot create COPC index: {error}"))
            })?;
        writer.write(points, point_count).map_err(|error| {
            EcoScopeError::Invalid(format!("cannot derive COPC index: {error}"))
        })?;
        drop(writer);
        if let Some(error) = read_error.borrow().as_ref() {
            return Err(EcoScopeError::Invalid(format!(
                "cannot derive COPC because the LAS/LAZ stream failed: {error}"
            )));
        }
        let output = self
            .paths()
            .derived_dir
            .join(format!("{derived_id}.copc.laz"));
        temporary
            .persist_noclobber(&output)
            .map_err(|error| EcoScopeError::Io(error.error))?;
        let checksum = ecoscope_local_import::fingerprint_path(&output)?;
        let record = DerivedAssetRecord {
            derived_id: derived_id.clone(),
            dataset_id: DatasetId(dataset_id.to_owned()),
            kind: COPC_KIND.into(),
            source_fingerprint: source.checksum.clone(),
            artifact: ArtifactDescriptor {
                uri: format!("ecoscope://derived/{derived_id}/data"),
                format: "copc".into(),
                media_type: "application/vnd.laszip".into(),
                size_bytes: output.metadata()?.len(),
                checksum,
            },
            metadata: [
                ("spatial_queries".into(), json!("full_resolution")),
                (
                    "representative_lod".into(),
                    json!(false),
                ),
                (
                    "warning".into(),
                    json!("The copc-rs 0.5 writer does not guarantee representative lower LOD point distributions; EcoScope will not use resolution/level queries on this derived index."),
                ),
            ]
            .into_iter()
            .collect(),
            created_at: Utc::now(),
        };
        self.connection()?
            .execute(
                "INSERT OR REPLACE INTO derived_assets(
                    id, dataset_id, kind, source_fingerprint, artifact_uri, json, created_at
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    record.derived_id,
                    record.dataset_id.0,
                    record.kind,
                    record.source_fingerprint.value,
                    record.artifact.uri,
                    serde_json::to_string(&record)?,
                    record.created_at.to_rfc3339(),
                ],
            )
            .map_err(|error| EcoScopeError::Internal(format!("database error: {error}")))?;
        Ok(record)
    }

    pub fn find_derived_asset(
        &self,
        dataset_id: &str,
        kind: &str,
    ) -> Result<Option<DerivedAssetRecord>> {
        let text = self
            .connection()?
            .query_row(
                "SELECT json FROM derived_assets
                 WHERE dataset_id=?1 AND kind=?2 ORDER BY created_at DESC LIMIT 1",
                params![dataset_id, kind],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| EcoScopeError::Internal(format!("database error: {error}")))?;
        text.map(|text| serde_json::from_str(&text).map_err(EcoScopeError::from))
            .transpose()
    }

    pub(crate) fn resolve_point_cloud_query_source(
        &self,
        dataset_id: &str,
        source_path: &Path,
        original_name: &str,
        source_fingerprint: &str,
        resolution: Option<f64>,
        level: Option<i32>,
    ) -> Result<(PathBuf, String)> {
        if original_name.to_ascii_lowercase().contains(".copc.") {
            return Ok((source_path.to_path_buf(), original_name.to_owned()));
        }
        if let Some(derived) = self.find_derived_asset(dataset_id, COPC_KIND)?
            && derived.source_fingerprint.value == source_fingerprint
            && derived.metadata.get("representative_lod") == Some(&Value::Bool(false))
        {
            if resolution.is_some() || level.is_some() {
                return Err(EcoScopeError::Invalid(
                    "this derived COPC index is valid for full-resolution spatial queries only; resolution/level selection requires a provider-authored COPC"
                        .into(),
                ));
            }
            let path = self.derived_asset_path(&derived);
            if path.is_file() {
                return Ok((path, format!("{}.copc.laz", derived.derived_id)));
            }
        }
        Ok((source_path.to_path_buf(), original_name.to_owned()))
    }

    fn derived_asset_path(&self, record: &DerivedAssetRecord) -> PathBuf {
        self.paths()
            .derived_dir
            .join(format!("{}.copc.laz", record.derived_id))
    }
}
