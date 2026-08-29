//! Bounded scientific queries and durable, reproducible result artifacts.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use chrono::Utc;
use image::GenericImageView;
use ndarray::s;
use parquet::arrow::ArrowWriter;
use serde_json::{Map, Value, json};
use wilddatum_core::{
    ArtifactDescriptor, DatasetQuery, ExportFormat, ExportRecord, ExportRequest, Result, ResultId,
    ResultRecord, WildDatumError,
};

use super::WildDatumService;

const MAX_POINT_RESULT_ROWS: usize = 100_000;
const PREVIEW_ROWS: usize = 200;

type TabularRows = Vec<Map<String, Value>>;

impl WildDatumService {
    /// Execute a bounded query and persist both its exact specification and its
    /// result artifact. Callers receive an opaque artifact URI, never a path.
    pub async fn query_dataset(
        &self,
        dataset_id: &str,
        query: DatasetQuery,
    ) -> Result<ResultRecord> {
        let started = Instant::now();
        let manifest = self.get_manifest(dataset_id)?;
        let source = manifest
            .source_files
            .first()
            .ok_or_else(|| WildDatumError::Invalid("dataset has no source files".into()))?;
        let path = self.source_path_for_renderer(&manifest, source)?;
        let mut table_batches = None;
        let (payload, row_count, media_type) = match &query {
            DatasetQuery::Preview { limit } => {
                let value = self.preview_dataset(dataset_id, (*limit).min(2_000))?;
                let count = value.get("returned_rows").and_then(Value::as_u64);
                (value, count, "application/json")
            }
            DatasetQuery::SourceRows {
                source_indices,
                select,
            } => {
                let output = wilddatum_query::execute_source_rows(
                    &path,
                    &source.original_name,
                    source_indices,
                    select,
                )?;
                let row_count = output.matched_rows;
                (output.payload, Some(row_count), "application/json")
            }
            DatasetQuery::Table {
                select,
                filters,
                group_by,
                aggregates,
                order_by,
                limit,
            } => {
                let output = wilddatum_query::execute_table_query(
                    &path,
                    &source.original_name,
                    wilddatum_query::TableQuerySpec {
                        select,
                        filters,
                        group_by,
                        aggregates,
                        order_by,
                        limit: *limit,
                    },
                )
                .await?;
                let row_count = output.matched_rows;
                table_batches = Some(output.batches);
                (output.payload, Some(row_count), "application/json")
            }
            DatasetQuery::Spectrum {
                x,
                y,
                dataset_path,
                wavelength_dataset,
                spectral_axis,
                wavelength_start_nm,
                wavelength_end_nm,
                scale_factor,
                add_offset,
                no_data,
                bad_bands,
            } => query_spectrum(
                &path,
                source.metadata.get("hdf5_datasets"),
                *x,
                *y,
                dataset_path.as_deref(),
                wavelength_dataset.as_deref(),
                *spectral_axis,
                *wavelength_start_nm,
                *wavelength_end_nm,
                scale_factor.unwrap_or(1.0),
                add_offset.unwrap_or(0.0),
                *no_data,
                bad_bands,
            )?,
            DatasetQuery::CubeSlice {
                array_path,
                ranges,
                cell_limit,
            } => super::cube::query_cube_slice(
                &path,
                &source.original_name,
                array_path,
                ranges,
                *cell_limit,
            )?,
            DatasetQuery::PointCloudRegion {
                geometry,
                crs,
                source_indices,
                classifications,
                elevation_min,
                elevation_max,
                resolution,
                level,
                point_limit,
            } => {
                let (query_path, query_name) = self.resolve_point_cloud_query_source(
                    dataset_id,
                    &path,
                    &source.original_name,
                    &source.checksum.value,
                    *resolution,
                    *level,
                )?;
                query_point_cloud(
                    &query_path,
                    &query_name,
                    PointCloudQuerySpec {
                        geometry: &geometry.geojson,
                        crs,
                        source_indices,
                        classifications,
                        elevation_min: *elevation_min,
                        elevation_max: *elevation_max,
                        resolution: *resolution,
                        level: *level,
                        limit: *point_limit,
                    },
                )?
            }
            DatasetQuery::RasterPixel { x, y, bands } => query_raster_pixel(&path, *x, *y, bands)?,
            DatasetQuery::RasterRegion {
                geometry,
                crs,
                bands,
                statistics,
            } => query_raster_region(&path, &geometry.geojson, crs, bands, statistics)?,
            DatasetQuery::VectorRegion { geometry, crs } => {
                query_vector_region_dispatch(&path, &source.original_name, &geometry.geojson, crs)
                    .await?
            }
        };
        let record = self.persist_result(
            dataset_id,
            query,
            payload,
            row_count,
            media_type,
            table_batches.as_deref(),
        )?;
        self.connection()?
            .execute(
                "INSERT OR REPLACE INTO query_stats(
                    result_id, scanned_rows, returned_rows, elapsed_ms, json, created_at
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    record.result_id.0,
                    row_count.map(|value| value as i64),
                    record
                        .preview
                        .get("returned_rows")
                        .and_then(Value::as_u64)
                        .map(|value| value as i64),
                    started.elapsed().as_millis() as i64,
                    serde_json::to_string(&json!({"engine": "wilddatum-query"}))?,
                    record.created_at.to_rfc3339(),
                ],
            )
            .map_err(|error| WildDatumError::Internal(format!("database error: {error}")))?;
        Ok(record)
    }

    fn persist_result(
        &self,
        dataset_id: &str,
        query: DatasetQuery,
        payload: Value,
        row_count: Option<u64>,
        media_type: &str,
        table_batches: Option<&[RecordBatch]>,
    ) -> Result<ResultRecord> {
        let result_id = ResultId::new();
        let path = self
            .paths()
            .results_dir
            .join(format!("{}.json", result_id.0));
        serde_json::to_writer(File::create(&path)?, &payload)?;
        let checksum = wilddatum_local_import::fingerprint_path(&path)?;
        let mut artifacts = vec![ArtifactDescriptor {
            uri: format!("wilddatum://results/{}/data", result_id.0),
            format: "json".into(),
            media_type: media_type.into(),
            size_bytes: path.metadata()?.len(),
            checksum: checksum.clone(),
        }];
        if let Some(batches) = table_batches.filter(|batches| !batches.is_empty()) {
            let arrow_path = self
                .paths()
                .results_dir
                .join(format!("{}.arrow", result_id.0));
            wilddatum_query::write_arrow_ipc(&arrow_path, batches)?;
            let arrow_checksum = wilddatum_local_import::fingerprint_path(&arrow_path)?;
            artifacts.push(ArtifactDescriptor {
                uri: format!("wilddatum://results/{}/arrow", result_id.0),
                format: "arrow_ipc".into(),
                media_type: "application/vnd.apache.arrow.file".into(),
                size_bytes: arrow_path.metadata()?.len(),
                checksum: arrow_checksum,
            });
        }
        let preview = bounded_preview(&payload);
        let record = ResultRecord {
            result_id: result_id.clone(),
            dataset_id: wilddatum_core::DatasetId(dataset_id.to_owned()),
            source_selection: None,
            query,
            row_count,
            preview,
            artifact: Some(format!("wilddatum://results/{}/data", result_id.0)),
            media_type: Some(media_type.into()),
            checksum: Some(checksum),
            artifacts,
            transformations: vec![],
            created_at: Utc::now(),
        };
        self.put_json(
            "results",
            &record.result_id.0,
            &record,
            record.created_at.to_rfc3339(),
        )?;
        Ok(record)
    }

    pub fn get_result(&self, result_id: &str) -> Result<ResultRecord> {
        self.get_json("results", result_id)
    }

    pub fn export_result(&self, request: ExportRequest) -> Result<ExportRecord> {
        let result = self.get_result(&request.result_id.0)?;
        let payload: Value =
            serde_json::from_reader(File::open(self.result_path(&result.result_id))?)?;
        let export_id = format!("export_{}", uuid::Uuid::now_v7().simple());
        let extension = match request.format {
            ExportFormat::Csv => "csv",
            ExportFormat::Parquet => "parquet",
            ExportFormat::RoCrate => "json",
            ExportFormat::GeoJson => "geojson",
            ExportFormat::GeoTiff => "tif",
            ExportFormat::GeoParquet | ExportFormat::Copc => {
                return Err(WildDatumError::Invalid(format!(
                    "{:?} export requires its format-specific geospatial adapter",
                    request.format
                )));
            }
        };
        let output = self
            .paths()
            .exports_dir
            .join(format!("{export_id}.{extension}"));
        match request.format {
            ExportFormat::Csv => write_csv_export(&output, &payload)?,
            ExportFormat::Parquet => write_parquet_export(&output, &payload)?,
            ExportFormat::GeoJson => write_geojson_export(&output, &payload)?,
            ExportFormat::GeoTiff => self.write_geotiff_export(&result, &payload, &output)?,
            ExportFormat::RoCrate => {
                let manifest = self.get_manifest(&result.dataset_id.0)?;
                let crate_document = json!({
                    "@context": "https://w3id.org/ro/crate/1.1/context",
                    "@graph": [
                        {
                            "@id": "ro-crate-metadata.json",
                            "@type": "CreativeWork",
                            "about": {"@id": "./"}
                        },
                        {
                            "@id": "./",
                            "@type": "Dataset",
                            "name": manifest.resource_id,
                            "identifier": result.dataset_id,
                            "citation": manifest.citation,
                            "license": manifest.license,
                            "wilddatum:query": result.query,
                            "wilddatum:result": payload
                        }
                    ]
                });
                serde_json::to_writer_pretty(File::create(&output)?, &crate_document)?;
            }
            _ => unreachable!("unsupported formats returned above"),
        }
        let checksum = wilddatum_local_import::fingerprint_path(&output)?;
        let manifest_artifact = if request.include_provenance {
            let manifest_path = self
                .paths()
                .exports_dir
                .join(format!("{export_id}.manifest.json"));
            let manifest = self.get_manifest(&result.dataset_id.0)?;
            serde_json::to_writer_pretty(
                File::create(&manifest_path)?,
                &json!({
                    "schema_version": 1,
                    "export_id": export_id,
                    "dataset": manifest,
                    "result": result,
                    "format": request.format,
                    "reproduction": if request.include_reproduction_code {
                        json!({
                            "mcp_tool": "query_dataset",
                            "arguments": {"dataset_id": result.dataset_id, "query": result.query}
                        })
                    } else {
                        Value::Null
                    }
                }),
            )?;
            Some(format!("wilddatum://exports/{export_id}/manifest"))
        } else {
            None
        };
        let record = ExportRecord {
            export_id: export_id.clone(),
            result_id: result.result_id,
            format: request.format,
            artifact: format!("wilddatum://exports/{export_id}/data"),
            checksum,
            manifest_artifact,
            created_at: Utc::now(),
        };
        self.put_json(
            "exports",
            &record.export_id,
            &record,
            record.created_at.to_rfc3339(),
        )?;
        Ok(record)
    }

    fn result_path(&self, result_id: &ResultId) -> PathBuf {
        self.paths()
            .results_dir
            .join(format!("{}.json", result_id.0))
    }

    fn write_geotiff_export(
        &self,
        result: &ResultRecord,
        payload: &Value,
        output: &Path,
    ) -> Result<()> {
        let DatasetQuery::RasterRegion { bands, .. } = &result.query else {
            return Err(WildDatumError::Invalid(
                "GeoTIFF export requires a raster_region result".into(),
            ));
        };
        let bounds = payload
            .get("pixel_bounds")
            .and_then(Value::as_array)
            .filter(|bounds| bounds.len() == 4)
            .ok_or_else(|| WildDatumError::Invalid("raster result has no pixel bounds".into()))?;
        let col_start = bounds[0].as_u64().unwrap_or_default() as usize;
        let row_start = bounds[1].as_u64().unwrap_or_default() as usize;
        let col_end = bounds[2].as_u64().unwrap_or_default() as usize;
        let row_end = bounds[3].as_u64().unwrap_or_default() as usize;
        let rows = row_end.saturating_sub(row_start);
        let cols = col_end.saturating_sub(col_start);
        if rows == 0 || cols == 0 {
            return Err(WildDatumError::Invalid(
                "raster result window is empty".into(),
            ));
        }
        let manifest = self.get_manifest(&result.dataset_id.0)?;
        let source = manifest
            .source_files
            .first()
            .ok_or_else(|| WildDatumError::Invalid("dataset has no source files".into()))?;
        let source_path = self.source_path_for_renderer(&manifest, source)?;
        let raster = geotiff_reader::GeoTiffFile::open(&source_path)
            .map_err(|error| WildDatumError::Invalid(format!("cannot open GeoTIFF: {error}")))?;
        let selected = raster_bands(raster.band_count(), bands)?;
        let by_band = selected
            .iter()
            .map(|band| {
                read_geotiff_band_window(&raster, *band as usize, row_start, col_start, rows, cols)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut interleaved = Vec::with_capacity(rows * cols * selected.len());
        for pixel in 0..rows * cols {
            for values in &by_band {
                interleaved.push(values[pixel]);
            }
        }
        let data = ndarray::Array3::from_shape_vec((rows, cols, selected.len()), interleaved)
            .map_err(|error| {
                WildDatumError::Internal(format!("cannot shape raster export: {error}"))
            })?;
        let source_transform = raster.transform().ok_or_else(|| {
            WildDatumError::Invalid("source GeoTIFF has no affine transform".into())
        })?;
        let (origin_x, origin_y) =
            source_transform.pixel_to_geo(col_start as f64, row_start as f64);
        let export_transform = geotiff_writer::GeoTransform {
            origin_x,
            origin_y,
            pixel_width: source_transform.pixel_width,
            pixel_height: source_transform.pixel_height,
            skew_x: source_transform.skew_x,
            skew_y: source_transform.skew_y,
        };
        let mut builder = geotiff_writer::GeoTiffBuilder::new(cols as u32, rows as u32)
            .bands(selected.len() as u32)
            .tile_size(256, 256)
            .compression(geotiff_writer::Compression::Deflate)
            .transform(export_transform);
        if let Some(epsg) = raster.epsg().and_then(|epsg| u16::try_from(epsg).ok()) {
            builder = builder.epsg(epsg);
        }
        if let Some(nodata) = raster.nodata() {
            builder = builder.nodata(nodata);
        }
        let mut overview_levels = Vec::new();
        let mut level = 2_u32;
        while rows / level as usize >= 1 && cols / level as usize >= 1 {
            overview_levels.push(level);
            level = level.saturating_mul(2);
            if level == 0 {
                break;
            }
        }
        geotiff_writer::CogBuilder::new(builder)
            .overview_levels(overview_levels)
            .resampling(geotiff_writer::Resampling::Average)
            .write_3d(output, data.view())
            .map_err(|error| WildDatumError::Internal(format!("cannot write COG export: {error}")))
    }
}

fn json_scalar(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
#[allow(clippy::too_many_arguments)]
fn query_spectrum(
    path: &Path,
    inspected_datasets: Option<&Value>,
    x: u64,
    y: u64,
    requested_dataset: Option<&str>,
    requested_wavelengths: Option<&str>,
    spectral_axis: u32,
    wavelength_start: Option<f64>,
    wavelength_end: Option<f64>,
    scale_factor: f64,
    add_offset: f64,
    no_data: Option<f64>,
    bad_bands: &[u32],
) -> Result<(Value, Option<u64>, &'static str)> {
    if spectral_axis != 2 {
        return Err(WildDatumError::Invalid(
            "spectrum queries currently require spectral_axis=2 ([y, x, band])".into(),
        ));
    }
    let file = hdf5_metno::File::open(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open HDF5: {error}")))?;
    let dataset_path = requested_dataset
        .map(str::to_owned)
        .or_else(|| unique_hdf5_path(inspected_datasets, |shape, _| shape.len() == 3))
        .ok_or_else(|| {
            WildDatumError::Invalid(
                "dataset_path is required because the HDF5 cube is ambiguous".into(),
            )
        })?;
    let dataset = file.dataset(&dataset_path).map_err(|error| {
        WildDatumError::Invalid(format!("cannot open HDF5 dataset {dataset_path}: {error}"))
    })?;
    let shape = dataset.shape();
    if shape.len() != 3 || y as usize >= shape[0] || x as usize >= shape[1] {
        return Err(WildDatumError::Invalid(format!(
            "pixel [{x}, {y}] is outside cube shape {shape:?}"
        )));
    }
    let raw = read_hdf5_spectrum(&dataset, y as usize, x as usize)?;
    let wavelength_path = requested_wavelengths.map(str::to_owned).or_else(|| {
        unique_hdf5_path(inspected_datasets, |shape, path| {
            shape == [raw.len()] && path.to_ascii_lowercase().contains("wavelength")
        })
    });
    let wavelengths = wavelength_path
        .as_deref()
        .map(|path| read_hdf5_vector(&file, path))
        .transpose()?;
    if (wavelength_start.is_some() || wavelength_end.is_some()) && wavelengths.is_none() {
        return Err(WildDatumError::Invalid(
            "wavelength filtering requires wavelength_dataset".into(),
        ));
    }
    if wavelengths
        .as_ref()
        .is_some_and(|values| values.len() != raw.len())
    {
        return Err(WildDatumError::Invalid(
            "wavelength coordinate length does not match the spectral dimension".into(),
        ));
    }
    let bad_bands = bad_bands.iter().copied().collect::<BTreeSet<_>>();
    let mut rows = Vec::new();
    for (band, raw_value) in raw.into_iter().enumerate() {
        let wavelength = wavelengths.as_ref().map(|values| values[band]);
        if bad_bands.contains(&(band as u32))
            || no_data == Some(raw_value)
            || wavelength_start.is_some_and(|start| wavelength.is_some_and(|value| value < start))
            || wavelength_end.is_some_and(|end| wavelength.is_some_and(|value| value > end))
        {
            continue;
        }
        rows.push(json!({
            "band": band,
            "wavelength_nm": wavelength,
            "value": raw_value * scale_factor + add_offset
        }));
    }
    let count = rows.len() as u64;
    Ok((
        json!({
            "dataset_path": dataset_path,
            "wavelength_dataset": wavelength_path,
            "pixel": {"x": x, "y": y},
            "columns": ["band", "wavelength_nm", "value"],
            "rows": rows,
            "returned_rows": count,
            "scale_factor": scale_factor,
            "add_offset": add_offset
        }),
        Some(count),
        "application/json",
    ))
}

fn unique_hdf5_path(
    inspected: Option<&Value>,
    predicate: impl Fn(&[usize], &str) -> bool,
) -> Option<String> {
    let matches = inspected?
        .as_array()?
        .iter()
        .filter_map(|entry| {
            let path = entry.get("path")?.as_str()?;
            let shape = entry
                .get("shape")?
                .as_array()?
                .iter()
                .map(|value| value.as_u64().map(|value| value as usize))
                .collect::<Option<Vec<_>>>()?;
            predicate(&shape, path).then(|| path.to_owned())
        })
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0].clone())
}

fn read_hdf5_spectrum(dataset: &hdf5_metno::Dataset, y: usize, x: usize) -> Result<Vec<f64>> {
    let datatype = dataset
        .dtype()
        .map_err(|error| WildDatumError::Invalid(format!("cannot inspect HDF5 type: {error}")))?;
    macro_rules! read_values {
        ($type:ty) => {
            dataset
                .read_slice_1d::<$type, _>(s![y, x, ..])
                .map(|values| values.iter().map(|value| *value as f64).collect())
        };
    }
    let values = if datatype.is::<u8>() {
        read_values!(u8)
    } else if datatype.is::<u16>() {
        read_values!(u16)
    } else if datatype.is::<u32>() {
        read_values!(u32)
    } else if datatype.is::<i16>() {
        read_values!(i16)
    } else if datatype.is::<i32>() {
        read_values!(i32)
    } else if datatype.is::<f32>() {
        read_values!(f32)
    } else if datatype.is::<f64>() {
        read_values!(f64)
    } else {
        return Err(WildDatumError::Invalid(format!(
            "unsupported HDF5 sample datatype {datatype:?}"
        )));
    };
    values.map_err(|error| WildDatumError::Invalid(format!("cannot read spectrum: {error}")))
}

fn read_hdf5_vector(file: &hdf5_metno::File, path: &str) -> Result<Vec<f64>> {
    let dataset = file.dataset(path).map_err(|error| {
        WildDatumError::Invalid(format!("cannot open wavelength dataset {path}: {error}"))
    })?;
    if dataset.shape().len() != 1 {
        return Err(WildDatumError::Invalid(format!(
            "wavelength dataset {path} must be rank 1"
        )));
    }
    let datatype = dataset
        .dtype()
        .map_err(|error| WildDatumError::Invalid(format!("cannot inspect wavelengths: {error}")))?;
    macro_rules! read_values {
        ($type:ty) => {
            dataset
                .read_raw::<$type>()
                .map(|values| values.into_iter().map(|value| value as f64).collect())
        };
    }
    let values = if datatype.is::<u16>() {
        read_values!(u16)
    } else if datatype.is::<u32>() {
        read_values!(u32)
    } else if datatype.is::<i16>() {
        read_values!(i16)
    } else if datatype.is::<i32>() {
        read_values!(i32)
    } else if datatype.is::<f32>() {
        read_values!(f32)
    } else if datatype.is::<f64>() {
        read_values!(f64)
    } else {
        return Err(WildDatumError::Invalid(
            "unsupported wavelength datatype".into(),
        ));
    };
    values.map_err(|error| WildDatumError::Invalid(format!("cannot read wavelengths: {error}")))
}

struct PointCloudQuerySpec<'a> {
    geometry: &'a Value,
    crs: &'a str,
    source_indices: &'a [u64],
    classifications: &'a [u8],
    elevation_min: Option<f64>,
    elevation_max: Option<f64>,
    resolution: Option<f64>,
    level: Option<i32>,
    limit: u64,
}

fn query_point_cloud(
    path: &Path,
    original_name: &str,
    query: PointCloudQuerySpec<'_>,
) -> Result<(Value, Option<u64>, &'static str)> {
    let PointCloudQuerySpec {
        geometry,
        crs,
        source_indices,
        classifications,
        elevation_min,
        elevation_max,
        resolution,
        level,
        limit: requested_limit,
    } = query;
    if resolution.is_some() && level.is_some() {
        return Err(WildDatumError::Invalid(
            "point-cloud resolution and level are mutually exclusive".into(),
        ));
    }
    if !source_indices.is_empty() {
        return query_point_cloud_source_indices(
            path,
            crs,
            source_indices,
            classifications,
            elevation_min,
            elevation_max,
            requested_limit,
        );
    }
    if original_name.to_ascii_lowercase().contains(".copc.") {
        return query_copc_point_cloud(
            path,
            geometry,
            crs,
            classifications,
            elevation_min,
            elevation_max,
            resolution,
            level,
            requested_limit,
        );
    }
    if resolution.is_some() || level.is_some() {
        return Err(WildDatumError::Invalid(
            "resolution and level selection require a COPC source; derive a COPC index first"
                .into(),
        ));
    }
    let [min_x, min_y, max_x, max_y] = geometry_bounds(geometry).ok_or_else(|| {
        WildDatumError::Invalid(
            "point cloud geometry must contain valid GeoJSON coordinates".into(),
        )
    })?;
    let mut reader = las::Reader::from_path(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open LAS/LAZ: {error}")))?;
    let limit = requested_limit.clamp(1, MAX_POINT_RESULT_ROWS as u64) as usize;
    let classes = classifications.iter().copied().collect::<BTreeSet<_>>();
    let mut points = Vec::new();
    let mut matched = 0_u64;
    let mut z_min: Option<f64> = None;
    let mut z_max: Option<f64> = None;
    let mut z_sum = 0.0;
    let mut class_counts = BTreeMap::<u8, u64>::new();
    for point in reader.points() {
        let point = point
            .map_err(|error| WildDatumError::Invalid(format!("cannot read LAS point: {error}")))?;
        let class = u8::from(point.classification);
        if point.x < min_x
            || point.x > max_x
            || point.y < min_y
            || point.y > max_y
            || elevation_min.is_some_and(|minimum| point.z < minimum)
            || elevation_max.is_some_and(|maximum| point.z > maximum)
            || (!classes.is_empty() && !classes.contains(&class))
        {
            continue;
        }
        matched += 1;
        z_min = Some(z_min.map_or(point.z, |value| value.min(point.z)));
        z_max = Some(z_max.map_or(point.z, |value| value.max(point.z)));
        z_sum += point.z;
        *class_counts.entry(class).or_default() += 1;
        if points.len() < limit {
            points.push(json!({
                "x": point.x,
                "y": point.y,
                "z": point.z,
                "classification": class
            }));
        }
    }
    Ok((
        json!({
            "crs": crs,
            "bounds": [min_x, min_y, max_x, max_y],
            "columns": ["x", "y", "z", "classification"],
            "rows": points,
            "returned_rows": points.len(),
            "matched_points": matched,
            "truncated": matched > points.len() as u64,
            "statistics": {
                "elevation_min": z_min,
                "elevation_max": z_max,
                "elevation_mean": (matched > 0).then(|| z_sum / matched as f64),
                "classification_counts": class_counts
            }
        }),
        Some(matched),
        "application/json",
    ))
}

fn query_point_cloud_source_indices(
    path: &Path,
    crs: &str,
    source_indices: &[u64],
    classifications: &[u8],
    elevation_min: Option<f64>,
    elevation_max: Option<f64>,
    requested_limit: u64,
) -> Result<(Value, Option<u64>, &'static str)> {
    let indices = source_indices.iter().copied().collect::<BTreeSet<_>>();
    let maximum_index = indices.iter().next_back().copied().unwrap_or_default();
    let classes = classifications.iter().copied().collect::<BTreeSet<_>>();
    let limit = requested_limit.clamp(1, MAX_POINT_RESULT_ROWS as u64) as usize;
    let mut reader = las::Reader::from_path(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open LAS/LAZ: {error}")))?;
    let mut points = Vec::new();
    let mut matched = 0_u64;
    for (index, point) in reader.points().enumerate() {
        let index = index as u64;
        if index > maximum_index {
            break;
        }
        if !indices.contains(&index) {
            continue;
        }
        let point = point
            .map_err(|error| WildDatumError::Invalid(format!("cannot read LAS point: {error}")))?;
        let class = u8::from(point.classification);
        if elevation_min.is_some_and(|minimum| point.z < minimum)
            || elevation_max.is_some_and(|maximum| point.z > maximum)
            || (!classes.is_empty() && !classes.contains(&class))
        {
            continue;
        }
        matched += 1;
        if points.len() < limit {
            points.push(json!({
                "source_index": index,
                "x": point.x,
                "y": point.y,
                "z": point.z,
                "classification": class
            }));
        }
    }
    Ok((
        json!({
            "crs": crs,
            "selection_mode": "source_indices",
            "source_indices": source_indices,
            "columns": ["source_index", "x", "y", "z", "classification"],
            "rows": points,
            "returned_rows": points.len(),
            "matched_points": matched,
            "truncated": matched > points.len() as u64
        }),
        Some(matched),
        "application/json",
    ))
}

#[allow(clippy::too_many_arguments)]
fn query_copc_point_cloud(
    path: &Path,
    geometry: &Value,
    crs: &str,
    classifications: &[u8],
    elevation_min: Option<f64>,
    elevation_max: Option<f64>,
    resolution: Option<f64>,
    level: Option<i32>,
    requested_limit: u64,
) -> Result<(Value, Option<u64>, &'static str)> {
    let [min_x, min_y, max_x, max_y] = geometry_bounds(geometry).ok_or_else(|| {
        WildDatumError::Invalid(
            "point cloud geometry must contain valid GeoJSON coordinates".into(),
        )
    })?;
    let mut reader = copc_rs::CopcReader::from_path(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open COPC: {error}")))?;
    let header_bounds = reader.header().bounds();
    let bounds = copc_rs::Bounds {
        min: copc_rs::Vector {
            x: min_x,
            y: min_y,
            z: elevation_min.unwrap_or(header_bounds.min.z),
        },
        max: copc_rs::Vector {
            x: max_x,
            y: max_y,
            z: elevation_max.unwrap_or(header_bounds.max.z),
        },
    };
    let lod = if let Some(resolution) = resolution {
        if !resolution.is_finite() || resolution <= 0.0 {
            return Err(WildDatumError::Invalid(
                "COPC resolution must be a positive finite number".into(),
            ));
        }
        copc_rs::LodSelection::Resolution(resolution)
    } else if let Some(level) = level {
        if level < 0 {
            return Err(WildDatumError::Invalid(
                "COPC level must be non-negative".into(),
            ));
        }
        copc_rs::LodSelection::Level(level)
    } else {
        copc_rs::LodSelection::All
    };
    let mut points = reader
        .points(lod, copc_rs::BoundsSelection::Within(bounds))
        .map_err(|error| WildDatumError::Invalid(format!("cannot query COPC octree: {error}")))?;
    let limit = requested_limit.clamp(1, MAX_POINT_RESULT_ROWS as u64) as usize;
    let classes = classifications.iter().copied().collect::<BTreeSet<_>>();
    let mut rows = Vec::with_capacity(limit.min(32_768));
    let mut z_min: Option<f64> = None;
    let mut z_max: Option<f64> = None;
    let mut z_sum = 0.0;
    let mut class_counts = BTreeMap::<u8, u64>::new();
    let mut truncated = false;
    for point in points.by_ref() {
        let class = u8::from(point.classification);
        if !classes.is_empty() && !classes.contains(&class) {
            continue;
        }
        if rows.len() == limit {
            truncated = true;
            break;
        }
        z_min = Some(z_min.map_or(point.z, |value| value.min(point.z)));
        z_max = Some(z_max.map_or(point.z, |value| value.max(point.z)));
        z_sum += point.z;
        *class_counts.entry(class).or_default() += 1;
        rows.push(json!({
            "x": point.x,
            "y": point.y,
            "z": point.z,
            "classification": class
        }));
    }
    let returned = rows.len() as u64;
    Ok((
        json!({
            "crs": crs,
            "bounds": [min_x, min_y, max_x, max_y],
            "columns": ["x", "y", "z", "classification"],
            "rows": rows,
            "returned_rows": returned,
            "matched_points_lower_bound": returned + u64::from(truncated),
            "truncated": truncated,
            "engine": "copc_octree",
            "lod": {"resolution": resolution, "level": level, "full_resolution": resolution.is_none() && level.is_none()},
            "statistics_scope": "returned_lod_points",
            "statistics": {
                "elevation_min": z_min,
                "elevation_max": z_max,
                "elevation_mean": (returned > 0).then(|| z_sum / returned as f64),
                "classification_counts": class_counts
            }
        }),
        Some(returned),
        "application/json",
    ))
}

fn geometry_bounds(value: &Value) -> Option<[f64; 4]> {
    if let Some(bbox) = value.get("bbox").and_then(Value::as_array)
        && bbox.len() >= 4
    {
        return Some([
            bbox[0].as_f64()?,
            bbox[1].as_f64()?,
            bbox[2].as_f64()?,
            bbox[3].as_f64()?,
        ]);
    }
    let coordinates = value
        .get("geometry")
        .and_then(|geometry| geometry.get("coordinates"))
        .or_else(|| value.get("coordinates"))?;
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    collect_coordinate_bounds(coordinates, &mut bounds);
    bounds
        .iter()
        .all(|value| value.is_finite())
        .then_some(bounds)
}

fn collect_coordinate_bounds(value: &Value, bounds: &mut [f64; 4]) {
    let Some(values) = value.as_array() else {
        return;
    };
    if values.len() >= 2
        && let (Some(x), Some(y)) = (values[0].as_f64(), values[1].as_f64())
    {
        bounds[0] = bounds[0].min(x);
        bounds[1] = bounds[1].min(y);
        bounds[2] = bounds[2].max(x);
        bounds[3] = bounds[3].max(y);
        return;
    }
    for child in values {
        collect_coordinate_bounds(child, bounds);
    }
}

fn query_raster_pixel(
    path: &Path,
    x: u64,
    y: u64,
    bands: &[u32],
) -> Result<(Value, Option<u64>, &'static str)> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "tif" | "tiff" | "cog") {
        return query_geotiff_pixel(path, x, y, bands);
    }
    let image = image::open(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot decode raster: {error}")))?;
    if x >= image.width() as u64 || y >= image.height() as u64 {
        return Err(WildDatumError::Invalid(format!(
            "pixel [{x}, {y}] is outside raster dimensions [{}, {}]",
            image.width(),
            image.height()
        )));
    }
    let channels = image.get_pixel(x as u32, y as u32).0;
    let chosen = if bands.is_empty() {
        (0..channels.len() as u32).collect::<Vec<_>>()
    } else {
        bands.to_vec()
    };
    let mut values = Vec::new();
    for band in chosen {
        let value = channels.get(band as usize).ok_or_else(|| {
            WildDatumError::Invalid(format!(
                "band {band} is outside the decoded raster channels"
            ))
        })?;
        values.push(json!({"band": band, "value": value}));
    }
    Ok((
        json!({
            "pixel": {"x": x, "y": y},
            "rows": values,
            "returned_rows": values.len()
        }),
        Some(values.len() as u64),
        "application/json",
    ))
}

fn query_geotiff_pixel(
    path: &Path,
    x: u64,
    y: u64,
    bands: &[u32],
) -> Result<(Value, Option<u64>, &'static str)> {
    let raster = geotiff_reader::GeoTiffFile::open(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open GeoTIFF: {error}")))?;
    if x >= raster.width() as u64 || y >= raster.height() as u64 {
        return Err(WildDatumError::Invalid(format!(
            "pixel [{x}, {y}] is outside raster dimensions [{}, {}]",
            raster.width(),
            raster.height()
        )));
    }
    let chosen = raster_bands(raster.band_count(), bands)?;
    let mut values = Vec::with_capacity(chosen.len());
    for band in chosen {
        let sample =
            read_geotiff_band_window(&raster, band as usize, y as usize, x as usize, 1, 1)?
                .into_iter()
                .next()
                .ok_or_else(|| WildDatumError::Internal("GeoTIFF pixel window was empty".into()))?;
        values.push(json!({"band": band, "value": sample}));
    }
    let world = raster
        .pixel_to_geo(x as f64 + 0.5, y as f64 + 0.5)
        .map(|(world_x, world_y)| json!({"x": world_x, "y": world_y}));
    Ok((
        json!({
            "pixel": {"x": x, "y": y},
            "world_coordinate": world,
            "crs": raster.epsg().map(|epsg| format!("EPSG:{epsg}")),
            "nodata": raster.nodata(),
            "rows": values,
            "returned_rows": values.len()
        }),
        Some(values.len() as u64),
        "application/json",
    ))
}

fn query_raster_region(
    path: &Path,
    geometry: &Value,
    crs: &str,
    bands: &[u32],
    requested_statistics: &[String],
) -> Result<(Value, Option<u64>, &'static str)> {
    const MAX_RASTER_REGION_PIXELS: usize = 4_000_000;
    let raster = geotiff_reader::GeoTiffFile::open(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open GeoTIFF: {error}")))?;
    if crs != "source"
        && raster
            .epsg()
            .is_some_and(|epsg| !crs.eq_ignore_ascii_case(&format!("EPSG:{epsg}")))
    {
        return Err(WildDatumError::Invalid(format!(
            "raster_region CRS {crs} does not match source EPSG:{}; reprojection is not implicit",
            raster.epsg().unwrap_or_default()
        )));
    }
    let [min_x, min_y, max_x, max_y] = geometry_bounds(geometry).ok_or_else(|| {
        WildDatumError::Invalid("raster geometry must contain valid GeoJSON coordinates".into())
    })?;
    let corners = [
        (min_x, min_y),
        (min_x, max_y),
        (max_x, min_y),
        (max_x, max_y),
    ];
    let pixels = corners
        .iter()
        .map(|(world_x, world_y)| {
            raster.geo_to_pixel(*world_x, *world_y).ok_or_else(|| {
                WildDatumError::Invalid("GeoTIFF transform is singular or unavailable".into())
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let col_start = pixels
        .iter()
        .map(|(col, _)| *col)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as usize;
    let row_start = pixels
        .iter()
        .map(|(_, row)| *row)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as usize;
    let col_end = pixels
        .iter()
        .map(|(col, _)| *col)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(raster.width() as f64) as usize;
    let row_end = pixels
        .iter()
        .map(|(_, row)| *row)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(raster.height() as f64) as usize;
    if col_start >= col_end || row_start >= row_end {
        return Err(WildDatumError::Invalid(
            "requested geometry does not overlap the raster".into(),
        ));
    }
    let rows = row_end - row_start;
    let cols = col_end - col_start;
    if rows.saturating_mul(cols) > MAX_RASTER_REGION_PIXELS {
        return Err(WildDatumError::Invalid(format!(
            "raster window contains {} pixels, above the {MAX_RASTER_REGION_PIXELS}-pixel safety budget; narrow the geometry",
            rows.saturating_mul(cols)
        )));
    }
    let chosen = raster_bands(raster.band_count(), bands)?;
    let nodata = raster.nodata().and_then(|value| value.parse::<f64>().ok());
    let requested = requested_statistics
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let include_all = requested.is_empty();
    let mut band_statistics = Vec::with_capacity(chosen.len());
    for band in chosen {
        let values =
            read_geotiff_band_window(&raster, band as usize, row_start, col_start, rows, cols)?;
        let valid = values
            .iter()
            .copied()
            .filter(|value| value.is_finite() && nodata != Some(*value))
            .collect::<Vec<_>>();
        let count = valid.len() as u64;
        let sum = valid.iter().sum::<f64>();
        let mean = (count > 0).then(|| sum / count as f64);
        let minimum = valid.iter().copied().reduce(f64::min);
        let maximum = valid.iter().copied().reduce(f64::max);
        let standard_deviation = mean.map(|mean| {
            (valid
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / count as f64)
                .sqrt()
        });
        let mut statistics = Map::new();
        if include_all || requested.contains("count") {
            statistics.insert("count".into(), json!(count));
        }
        if include_all || requested.contains("min") || requested.contains("minimum") {
            statistics.insert("min".into(), json!(minimum));
        }
        if include_all || requested.contains("max") || requested.contains("maximum") {
            statistics.insert("max".into(), json!(maximum));
        }
        if include_all || requested.contains("mean") || requested.contains("avg") {
            statistics.insert("mean".into(), json!(mean));
        }
        if include_all || requested.contains("stddev") || requested.contains("standard_deviation") {
            statistics.insert("standard_deviation".into(), json!(standard_deviation));
        }
        statistics.insert("nodata_count".into(), json!(values.len() as u64 - count));
        band_statistics.push(json!({"band": band, "statistics": statistics}));
    }
    let pixel_count = rows.saturating_mul(cols) as u64;
    Ok((
        json!({
            "crs": raster.epsg().map(|epsg| format!("EPSG:{epsg}")),
            "world_bounds": [min_x, min_y, max_x, max_y],
            "pixel_bounds": [col_start, row_start, col_end, row_end],
            "window_shape": [rows, cols],
            "pixel_count": pixel_count,
            "nodata": raster.nodata(),
            "rows": band_statistics,
            "returned_rows": band_statistics.len()
        }),
        Some(pixel_count),
        "application/json",
    ))
}

fn raster_bands(band_count: u32, bands: &[u32]) -> Result<Vec<u32>> {
    let selected = if bands.is_empty() {
        (0..band_count).collect::<Vec<_>>()
    } else {
        bands.to_vec()
    };
    if let Some(invalid) = selected.iter().find(|band| **band >= band_count) {
        return Err(WildDatumError::Invalid(format!(
            "band {invalid} is outside the raster's {band_count} bands"
        )));
    }
    Ok(selected)
}

fn read_geotiff_band_window(
    raster: &geotiff_reader::GeoTiffFile,
    band: usize,
    row: usize,
    col: usize,
    rows: usize,
    cols: usize,
) -> Result<Vec<f64>> {
    macro_rules! try_sample {
        ($sample:ty) => {
            if let Ok(values) = raster.read_band_window::<$sample>(band, row, col, rows, cols) {
                return Ok(values.iter().map(|value| *value as f64).collect::<Vec<_>>());
            }
        };
    }
    try_sample!(u8);
    try_sample!(i8);
    try_sample!(u16);
    try_sample!(i16);
    try_sample!(u32);
    try_sample!(i32);
    try_sample!(u64);
    try_sample!(i64);
    try_sample!(f32);
    try_sample!(f64);
    Err(WildDatumError::Invalid(
        "unsupported GeoTIFF sample type for numeric analysis".into(),
    ))
}

fn query_vector_region(
    path: &Path,
    original_name: &str,
    geometry: &Value,
    crs: &str,
) -> Result<(Value, Option<u64>, &'static str)> {
    const MAX_VECTOR_RESULTS: usize = 100_000;
    let extension = Path::new(original_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "fgb" {
        return query_flatgeobuf_region(path, geometry, crs);
    }
    if extension == "shp" {
        return query_shapefile_region(path, geometry, crs);
    }
    if !matches!(extension.as_str(), "geojson" | "json") {
        return Err(WildDatumError::Invalid(format!(
            "vector_region currently accepts GeoJSON; {extension} requires its indexed vector adapter"
        )));
    }
    let query_geometry = geojson_geometry(geometry)?;
    let query_geometry: geo::Geometry<f64> = (&query_geometry.value)
        .try_into()
        .map_err(|error| WildDatumError::Invalid(format!("invalid query geometry: {error}")))?;
    let document: geojson::GeoJson = serde_json::from_reader(File::open(path)?)
        .map_err(|error| WildDatumError::Invalid(format!("invalid GeoJSON source: {error}")))?;
    let features = match document {
        geojson::GeoJson::FeatureCollection(collection) => collection.features,
        geojson::GeoJson::Feature(feature) => vec![feature],
        geojson::GeoJson::Geometry(geometry) => vec![geojson::Feature {
            geometry: Some(geometry),
            ..Default::default()
        }],
    };
    let mut matched = 0_u64;
    let mut rows = Vec::new();
    for (index, feature) in features.into_iter().enumerate() {
        let intersects = feature.geometry.as_ref().is_some_and(|geometry| {
            let candidate: std::result::Result<geo::Geometry<f64>, _> =
                (&geometry.value).try_into();
            candidate
                .is_ok_and(|candidate| geo::Intersects::intersects(&candidate, &query_geometry))
        });
        if !intersects {
            continue;
        }
        matched += 1;
        if rows.len() < MAX_VECTOR_RESULTS {
            rows.push(json!({
                "feature_index": index,
                "id": feature.id,
                "properties": feature.properties,
                "geometry": feature.geometry
            }));
        }
    }
    Ok((
        json!({
            "crs": crs,
            "columns": ["feature_index", "id", "properties", "geometry"],
            "rows": rows,
            "returned_rows": rows.len(),
            "matched_features": matched,
            "truncated": matched > rows.len() as u64
        }),
        Some(matched),
        "application/geo+json",
    ))
}

async fn query_vector_region_dispatch(
    path: &Path,
    original_name: &str,
    geometry: &Value,
    crs: &str,
) -> Result<(Value, Option<u64>, &'static str)> {
    use wkt::ToWkt;

    let extension = Path::new(original_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "parquet" | "geoparquet") {
        let query_geometry = geojson_geometry(geometry)?;
        let query_geometry: geo::Geometry<f64> = (&query_geometry.value)
            .try_into()
            .map_err(|error| WildDatumError::Invalid(format!("invalid query geometry: {error}")))?;
        let output = wilddatum_query::execute_geoparquet_region(
            path,
            &query_geometry.to_wkt().to_string(),
            100_000,
        )
        .await?;
        let rows = output.payload["rows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, row)| geoparquet_row_to_feature(index, row))
            .collect::<Result<Vec<_>>>()?;
        return Ok((
            json!({
                "crs": crs,
                "columns": ["feature_index", "id", "properties", "geometry"],
                "rows": rows,
                "returned_rows": rows.len(),
                "matched_features": output.matched_rows,
                "truncated": output.matched_rows > rows.len() as u64,
                "engine": "geodatafusion-0.5",
                "predicate": "st_intersects"
            }),
            Some(output.matched_rows),
            "application/geo+json",
        ));
    }
    query_vector_region(path, original_name, geometry, crs)
}

fn geoparquet_row_to_feature(index: usize, row: Value) -> Result<Value> {
    let mut properties = row
        .as_object()
        .cloned()
        .ok_or_else(|| WildDatumError::Internal("GeoParquet result row is not an object".into()))?;
    let geometry_text = properties
        .remove("__wilddatum_geometry_wkt")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| WildDatumError::Internal("GeoParquet row has no decoded geometry".into()))?;
    let parsed: wkt::Wkt<f64> = geometry_text
        .parse()
        .map_err(|error| WildDatumError::Invalid(format!("invalid GeoParquet WKT: {error}")))?;
    let geometry: geo::Geometry<f64> = parsed.try_into().map_err(|error| {
        WildDatumError::Invalid(format!("unsupported GeoParquet geometry: {error}"))
    })?;
    let id = properties.get("id").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "feature_index": index,
        "id": id,
        "properties": properties,
        "geometry": geojson::Geometry::new(geojson::Value::from(&geometry))
    }))
}

fn query_flatgeobuf_region(
    path: &Path,
    geometry: &Value,
    crs: &str,
) -> Result<(Value, Option<u64>, &'static str)> {
    use flatgeobuf::FallibleStreamingIterator;
    use geozero::ToJson;

    const MAX_VECTOR_RESULTS: usize = 100_000;
    let [min_x, min_y, max_x, max_y] = geometry_bounds(geometry).ok_or_else(|| {
        WildDatumError::Invalid("vector geometry must contain valid GeoJSON coordinates".into())
    })?;
    let query_geometry = geojson_geometry(geometry)?;
    let query_geometry: geo::Geometry<f64> = (&query_geometry.value)
        .try_into()
        .map_err(|error| WildDatumError::Invalid(format!("invalid query geometry: {error}")))?;
    let input = std::io::BufReader::new(File::open(path)?);
    let reader = flatgeobuf::FgbReader::open(input)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open FlatGeobuf: {error}")))?;
    let mut features = reader
        .select_bbox(min_x, min_y, max_x, max_y)
        .map_err(|error| {
            WildDatumError::Invalid(format!("cannot query FlatGeobuf index: {error}"))
        })?;
    let mut rows = Vec::new();
    let mut matched = 0_u64;
    let mut index = 0_u64;
    while let Some(feature) = features
        .next()
        .map_err(|error| WildDatumError::Invalid(format!("cannot read FlatGeobuf: {error}")))?
    {
        let text = feature.to_json().map_err(|error| {
            WildDatumError::Invalid(format!("cannot decode FlatGeobuf feature: {error}"))
        })?;
        let feature: geojson::Feature = text.parse().map_err(|error| {
            WildDatumError::Invalid(format!("invalid FlatGeobuf geometry: {error}"))
        })?;
        let intersects = feature.geometry.as_ref().is_some_and(|geometry| {
            let candidate: std::result::Result<geo::Geometry<f64>, _> =
                (&geometry.value).try_into();
            candidate
                .is_ok_and(|candidate| geo::Intersects::intersects(&candidate, &query_geometry))
        });
        if intersects {
            matched += 1;
            if rows.len() < MAX_VECTOR_RESULTS {
                rows.push(json!({
                    "feature_index": index,
                    "id": feature.id,
                    "properties": feature.properties,
                    "geometry": feature.geometry
                }));
            }
        }
        index += 1;
    }
    Ok(vector_region_payload(
        rows,
        matched,
        crs,
        "flatgeobuf_rtree",
    ))
}

fn query_shapefile_region(
    path: &Path,
    geometry: &Value,
    crs: &str,
) -> Result<(Value, Option<u64>, &'static str)> {
    const MAX_VECTOR_RESULTS: usize = 100_000;
    let query_geometry = geojson_geometry(geometry)?;
    let query_geometry: geo::Geometry<f64> = (&query_geometry.value)
        .try_into()
        .map_err(|error| WildDatumError::Invalid(format!("invalid query geometry: {error}")))?;
    let mut reader = shapefile::Reader::from_path(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open Shapefile: {error}")))?;
    let mut rows = Vec::new();
    let mut matched = 0_u64;
    for (index, entry) in reader.iter_shapes_and_records().enumerate() {
        let (shape, record) = entry
            .map_err(|error| WildDatumError::Invalid(format!("cannot read Shapefile: {error}")))?;
        let candidate: geo::Geometry<f64> = shape.try_into().map_err(|error| {
            WildDatumError::Invalid(format!("unsupported Shapefile shape: {error}"))
        })?;
        if !geo::Intersects::intersects(&candidate, &query_geometry) {
            continue;
        }
        matched += 1;
        if rows.len() < MAX_VECTOR_RESULTS {
            let geometry = geojson::Geometry::new(geojson::Value::from(&candidate));
            let properties = record
                .into_iter()
                .map(|(name, value)| (name, dbase_value_to_json(value)))
                .collect::<Map<_, _>>();
            rows.push(json!({
                "feature_index": index,
                "id": null,
                "properties": properties,
                "geometry": geometry
            }));
        }
    }
    Ok(vector_region_payload(rows, matched, crs, "shapefile_scan"))
}

fn vector_region_payload(
    rows: Vec<Value>,
    matched: u64,
    crs: &str,
    engine: &str,
) -> (Value, Option<u64>, &'static str) {
    let returned = rows.len();
    (
        json!({
            "crs": crs,
            "columns": ["feature_index", "id", "properties", "geometry"],
            "rows": rows,
            "returned_rows": returned,
            "matched_features": matched,
            "truncated": matched > returned as u64,
            "engine": engine
        }),
        Some(matched),
        "application/geo+json",
    )
}

fn dbase_value_to_json(value: shapefile::dbase::FieldValue) -> Value {
    use shapefile::dbase::FieldValue;
    match value {
        FieldValue::Character(value) => json!(value),
        FieldValue::Memo(value) => json!(value),
        FieldValue::Numeric(value) => json!(value),
        FieldValue::Logical(value) => json!(value),
        FieldValue::Date(value) => json!(value.map(|value| value.to_string())),
        FieldValue::Float(value) => json!(value),
        FieldValue::Integer(value) => json!(value),
        FieldValue::Currency(value) | FieldValue::Double(value) => json!(value),
        FieldValue::DateTime(value) => json!(format!("{value:?}")),
    }
}

fn geojson_geometry(value: &Value) -> Result<geojson::Geometry> {
    let geometry = value
        .get("geometry")
        .cloned()
        .unwrap_or_else(|| value.clone());
    serde_json::from_value(geometry)
        .map_err(|error| WildDatumError::Invalid(format!("invalid GeoJSON geometry: {error}")))
}

fn bounded_preview(payload: &Value) -> Value {
    let mut preview = payload.clone();
    if let Some(rows) = preview.get_mut("rows").and_then(Value::as_array_mut) {
        rows.truncate(PREVIEW_ROWS);
        preview["preview_rows"] = Value::from(rows.len() as u64);
    }
    preview
}

fn table_parts(payload: &Value) -> Result<(Vec<String>, TabularRows)> {
    let enveloped = payload.get("row_envelope").is_some();
    let columns = payload
        .get("columns")
        .and_then(Value::as_array)
        .ok_or_else(|| WildDatumError::Invalid("result is not tabular".into()))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| WildDatumError::Invalid("invalid result column".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| WildDatumError::Invalid("result is not tabular".into()))?
        .iter()
        .map(|row| {
            (if enveloped {
                row.get("values").unwrap_or(&Value::Null)
            } else {
                row
            })
            .as_object()
            .cloned()
            .ok_or_else(|| WildDatumError::Invalid("invalid result row".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((columns, rows))
}

fn write_csv_export(path: &Path, payload: &Value) -> Result<()> {
    let (columns, rows) = table_parts(payload)?;
    let mut writer = csv::Writer::from_path(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot create CSV export: {error}")))?;
    writer
        .write_record(&columns)
        .map_err(|error| WildDatumError::Invalid(format!("cannot write CSV header: {error}")))?;
    for row in rows {
        writer
            .write_record(
                columns
                    .iter()
                    .map(|column| row.get(column).map(json_scalar).unwrap_or_default()),
            )
            .map_err(|error| WildDatumError::Invalid(format!("cannot write CSV row: {error}")))?;
    }
    writer
        .flush()
        .map_err(|error| WildDatumError::Invalid(format!("cannot finish CSV export: {error}")))
}

fn write_parquet_export(path: &Path, payload: &Value) -> Result<()> {
    let (columns, rows) = table_parts(payload)?;
    let schema = Arc::new(Schema::new(
        columns
            .iter()
            .map(|column| Field::new(column, DataType::Utf8, true))
            .collect::<Vec<_>>(),
    ));
    let arrays = columns
        .iter()
        .map(|column| {
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.get(column).map(json_scalar))
                    .collect::<Vec<_>>(),
            )) as ArrayRef
        })
        .collect::<Vec<_>>();
    let batch = RecordBatch::try_new(schema.clone(), arrays)
        .map_err(|error| WildDatumError::Invalid(format!("cannot build Arrow batch: {error}")))?;
    let mut writer = ArrowWriter::try_new(File::create(path)?, schema, None).map_err(|error| {
        WildDatumError::Internal(format!("cannot create Parquet export: {error}"))
    })?;
    writer.write(&batch).map_err(|error| {
        WildDatumError::Internal(format!("cannot write Parquet export: {error}"))
    })?;
    writer.close().map_err(|error| {
        WildDatumError::Internal(format!("cannot finish Parquet export: {error}"))
    })?;
    Ok(())
}

fn write_geojson_export(path: &Path, payload: &Value) -> Result<()> {
    let rows = payload
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| WildDatumError::Invalid("result is not a vector feature result".into()))?;
    let features = rows
        .iter()
        .map(|row| {
            let geometry = row
                .get("geometry")
                .cloned()
                .filter(|value| !value.is_null())
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| {
                    WildDatumError::Invalid(format!("invalid result geometry: {error}"))
                })?;
            let properties = row.get("properties").and_then(Value::as_object).cloned();
            let id = row.get("id").and_then(|value| match value {
                Value::String(value) => Some(geojson::feature::Id::String(value.clone())),
                Value::Number(value) => Some(geojson::feature::Id::Number(value.clone())),
                _ => None,
            });
            Ok(geojson::Feature {
                bbox: None,
                geometry,
                id,
                properties,
                foreign_members: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let collection = geojson::FeatureCollection {
        bbox: None,
        features,
        foreign_members: None,
    };
    serde_json::to_writer_pretty(File::create(path)?, &collection)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use wilddatum_core::{
        AggregateSpec, CubeRange, DatasetQuery, ExportFormat, ExportRequest, GeoGeometry,
    };

    use super::*;
    use crate::ServicePaths;

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
    async fn table_queries_are_durable_and_exportable() {
        let (directory, service) = service();
        let path = directory.path().join("observations.csv");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "site,value").unwrap();
        writeln!(file, "HARV,1").unwrap();
        writeln!(file, "HARV,3").unwrap();
        writeln!(file, "ABBY,8").unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        let result = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::Table {
                    select: vec![],
                    filters: vec![],
                    group_by: vec!["site".into()],
                    aggregates: vec![AggregateSpec {
                        field: "value".into(),
                        function: "mean".into(),
                        alias: "mean_value".into(),
                    }],
                    order_by: vec![],
                    limit: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.row_count, Some(3));
        assert_eq!(result.preview["rows"].as_array().unwrap().len(), 2);
        let export = service
            .export_result(ExportRequest {
                result_id: result.result_id,
                format: ExportFormat::Parquet,
                include_provenance: true,
                include_reproduction_code: true,
            })
            .unwrap();
        assert!(export.artifact.starts_with("wilddatum://exports/"));
        assert!(export.manifest_artifact.is_some());
    }

    #[tokio::test]
    async fn source_rows_preserve_exact_indices_order_and_native_fields() {
        let (directory, service) = service();
        let path = directory.path().join("native-observations.tsv");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "platform\tpressure\tpres_qc\ttemperature\ttemp_qc\tsalinity\tpsal_qc"
        )
        .unwrap();
        for index in 0..8 {
            writeln!(
                file,
                "FLOAT_001\t{}\t1\t{}\t{}\t{}\t1",
                index * 10,
                if index == 0 { "01.00" } else { "18.72" },
                if index == 7 { "4" } else { "1" },
                35.0 + index as f64 / 100.0,
            )
            .unwrap();
        }
        let manifest = service.import_local_file(&path).await.unwrap();

        let result = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::SourceRows {
                    source_indices: vec![7, 0],
                    select: vec![],
                },
            )
            .await
            .unwrap();

        assert_eq!(result.row_count, Some(2));
        assert_eq!(result.preview["rows"][0]["source_index"], 7);
        assert_eq!(result.preview["rows"][0]["values"]["pres_qc"], "1");
        assert_eq!(result.preview["rows"][0]["values"]["temp_qc"], "4");
        assert_eq!(result.preview["rows"][0]["values"]["psal_qc"], "1");
        assert_eq!(result.preview["rows"][1]["source_index"], 0);
        assert_eq!(result.preview["rows"][1]["values"]["temperature"], "01.00");
        assert_eq!(result.artifacts.len(), 1);

        let export = service
            .export_result(ExportRequest {
                result_id: result.result_id,
                format: ExportFormat::Csv,
                include_provenance: true,
                include_reproduction_code: true,
            })
            .unwrap();
        assert!(export.manifest_artifact.is_some());
        let exported = std::fs::read_to_string(
            service
                .paths()
                .exports_dir
                .join(format!("{}.csv", export.export_id)),
        )
        .unwrap();
        assert!(
            exported.starts_with("platform,pressure,pres_qc,temperature,temp_qc,salinity,psal_qc")
        );
        assert!(exported.contains("FLOAT_001,70,1,18.72,4,35.07,1"));
    }

    #[tokio::test]
    async fn queries_hyperspectral_pixels_with_wavelengths() {
        let (directory, service) = service();
        let path = directory.path().join("cube.h5");
        let file = hdf5_metno::File::create(&path).unwrap();
        file.new_dataset::<u16>()
            .shape([2, 3, 4])
            .create("reflectance")
            .unwrap()
            .write_raw(&(0_u16..24).collect::<Vec<_>>())
            .unwrap();
        file.new_dataset::<f64>()
            .shape([4])
            .create("wavelength")
            .unwrap()
            .write_raw(&[400.0, 500.0, 600.0, 700.0])
            .unwrap();
        drop(file);
        let manifest = service.import_local_file(&path).await.unwrap();
        let result = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::Spectrum {
                    x: 1,
                    y: 1,
                    dataset_path: Some("/reflectance".into()),
                    wavelength_dataset: Some("/wavelength".into()),
                    spectral_axis: 2,
                    wavelength_start_nm: Some(500.0),
                    wavelength_end_nm: Some(650.0),
                    scale_factor: Some(0.01),
                    add_offset: None,
                    no_data: None,
                    bad_bands: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(result.row_count, Some(2));
        assert_eq!(result.preview["rows"][0]["wavelength_nm"], 500.0);
    }

    #[tokio::test]
    async fn queries_general_hdf5_zarr_and_netcdf3_cube_slices() {
        use std::sync::Arc;

        let (directory, service) = service();

        let hdf5_path = directory.path().join("arbitrary.h5");
        let file = hdf5_metno::File::create(&hdf5_path).unwrap();
        file.new_dataset::<i16>()
            .shape([2, 3, 4])
            .create("values")
            .unwrap()
            .write_raw(&(0_i16..24).collect::<Vec<_>>())
            .unwrap();
        drop(file);
        let hdf5 = service.import_local_file(&hdf5_path).await.unwrap();
        assert_eq!(hdf5.cubes.len(), 1);
        let hdf5_result = service
            .query_dataset(
                &hdf5.dataset_id.0,
                DatasetQuery::CubeSlice {
                    array_path: "/values".into(),
                    ranges: vec![
                        CubeRange {
                            start: 1,
                            end: 2,
                            step: 1,
                        },
                        CubeRange {
                            start: 0,
                            end: 3,
                            step: 2,
                        },
                        CubeRange {
                            start: 1,
                            end: 4,
                            step: 2,
                        },
                    ],
                    cell_limit: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(hdf5_result.row_count, Some(4));
        assert_eq!(hdf5_result.preview["rows"][0]["value"], 13.0);
        assert_eq!(hdf5_result.preview["rows"][3]["value"], 23.0);
        assert_eq!(hdf5_result.preview["engine"], "hdf5_hyperslab");

        let zarr_path = directory.path().join("arbitrary.zarr");
        std::fs::create_dir(&zarr_path).unwrap();
        let store = Arc::new(zarrs::filesystem::FilesystemStore::new(&zarr_path).unwrap());
        zarrs::group::GroupBuilder::new()
            .build(store.clone(), "/")
            .unwrap()
            .store_metadata()
            .unwrap();
        let array = zarrs::array::ArrayBuilder::new(
            vec![2, 3, 4],
            vec![1, 2, 2],
            zarrs::array::data_type::float32(),
            f32::NAN,
        )
        .dimension_names(["time", "y", "x"].into())
        .build(store, "/temperature")
        .unwrap();
        array.store_metadata().unwrap();
        array
            .store_array_subset(
                &[0..2, 0..3, 0..4],
                (0..24).map(|value| value as f32).collect::<Vec<_>>(),
            )
            .unwrap();
        let zarr = service.import_local_file(&zarr_path).await.unwrap();
        assert_eq!(zarr.cubes[0].axes[0].role, wilddatum_core::AxisRole::Time);
        let zarr_result = service
            .query_dataset(
                &zarr.dataset_id.0,
                DatasetQuery::CubeSlice {
                    array_path: "/temperature".into(),
                    ranges: vec![
                        CubeRange {
                            start: 0,
                            end: 2,
                            step: 1,
                        },
                        CubeRange {
                            start: 1,
                            end: 3,
                            step: 1,
                        },
                        CubeRange {
                            start: 0,
                            end: 4,
                            step: 2,
                        },
                    ],
                    cell_limit: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(zarr_result.row_count, Some(8));
        assert_eq!(zarr_result.preview["rows"][0]["value"], 4.0);
        assert_eq!(zarr_result.preview["engine"], "zarr_chunk_subset");

        let netcdf_path = directory.path().join("arbitrary.nc");
        let mut definition = netcdf3::DataSet::new();
        definition.add_fixed_dim("y", 2).unwrap();
        definition.add_fixed_dim("x", 3).unwrap();
        definition.add_var_f64("temperature", &["y", "x"]).unwrap();
        let mut writer = netcdf3::FileWriter::create_new(&netcdf_path).unwrap();
        writer
            .set_def(&definition, netcdf3::Version::Classic, 0)
            .unwrap();
        writer
            .write_var_f64("temperature", &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0])
            .unwrap();
        writer.close().unwrap();
        let netcdf = service.import_local_file(&netcdf_path).await.unwrap();
        let netcdf_result = service
            .query_dataset(
                &netcdf.dataset_id.0,
                DatasetQuery::CubeSlice {
                    array_path: "temperature".into(),
                    ranges: vec![
                        CubeRange {
                            start: 0,
                            end: 2,
                            step: 1,
                        },
                        CubeRange {
                            start: 1,
                            end: 3,
                            step: 1,
                        },
                    ],
                    cell_limit: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(netcdf_result.row_count, Some(4));
        assert_eq!(netcdf_result.preview["rows"][3]["value"], 5.0);
        assert_eq!(netcdf_result.preview["engine"], "netcdf3_bounded_variable");
    }

    #[tokio::test]
    async fn queries_a_point_cloud_region() {
        let (directory, service) = service();
        let path = directory.path().join("points.las");
        let mut header = las::Builder::from((1, 4));
        header.has_wkt_crs = true;
        header.point_format = las::point::Format::new(6).unwrap();
        header.vlrs.push(las::Vlr {
            user_id: "LASF_Projection".into(),
            record_id: 2112,
            description: "WGS 84".into(),
            data: b"GEOGCS[\"WGS 84\",AUTHORITY[\"EPSG\",\"4326\"]]".to_vec(),
        });
        let mut writer = las::Writer::from_path(&path, header.into_header().unwrap()).unwrap();
        for index in 0..10 {
            writer
                .write_point(las::Point {
                    x: index as f64,
                    y: index as f64,
                    z: index as f64 * 2.0,
                    gps_time: Some(index as f64),
                    ..Default::default()
                })
                .unwrap();
        }
        writer.close().unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        let result = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::PointCloudRegion {
                    geometry: GeoGeometry {
                        geojson: json!({
                            "type": "Polygon",
                            "coordinates": [[[2.0, 2.0], [5.0, 2.0], [5.0, 5.0], [2.0, 5.0], [2.0, 2.0]]]
                        }),
                    },
                    crs: "source".into(),
                    source_indices: vec![],
                    classifications: vec![],
                    elevation_min: None,
                    elevation_max: None,
                    resolution: None,
                    level: None,
                    point_limit: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.row_count, Some(4));
        assert_eq!(result.preview["statistics"]["elevation_mean"], 7.0);
        let selected = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::PointCloudRegion {
                    geometry: GeoGeometry {
                        geojson: json!({"type": "Point", "coordinates": [0.0, 0.0]}),
                    },
                    crs: "source".into(),
                    source_indices: vec![7],
                    classifications: vec![],
                    elevation_min: None,
                    elevation_max: None,
                    resolution: None,
                    level: None,
                    point_limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(selected.row_count, Some(1));
        assert_eq!(selected.preview["rows"][0]["source_index"], 7);
        assert_eq!(selected.preview["rows"][0]["x"], 7.0);
        let derived = service.derive_copc_index(&manifest.dataset_id.0).unwrap();
        assert_eq!(derived.kind, "copc_spatial_index");
        assert_eq!(derived.metadata["representative_lod"], false);
        let indexed = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::PointCloudRegion {
                    geometry: GeoGeometry {
                        geojson: json!({
                            "type": "Polygon",
                            "coordinates": [[[2.0, 2.0], [5.0, 2.0], [5.0, 5.0], [2.0, 5.0], [2.0, 2.0]]]
                        }),
                    },
                    crs: "source".into(),
                    source_indices: vec![],
                    classifications: vec![],
                    elevation_min: None,
                    elevation_max: None,
                    resolution: None,
                    level: None,
                    point_limit: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(indexed.preview["engine"], "copc_octree");
        assert_eq!(indexed.row_count, Some(4));
    }

    #[tokio::test]
    async fn queries_georeferenced_raster_regions_and_pixels() {
        let (directory, service) = service();
        let path = directory.path().join("elevation.tif");
        let values =
            ndarray::Array2::from_shape_vec((4, 4), (0_u16..16).collect::<Vec<_>>()).unwrap();
        geotiff_writer::GeoTiffBuilder::new(4, 4)
            .epsg(4326)
            .pixel_scale(1.0, 1.0)
            .origin(0.0, 4.0)
            .nodata("65535")
            .write_2d(&path, values.view())
            .unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        assert_eq!(
            manifest.spatial_reference.as_ref().unwrap().code.as_deref(),
            Some("4326")
        );
        let pixel = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::RasterPixel {
                    x: 2,
                    y: 1,
                    bands: vec![0],
                },
            )
            .await
            .unwrap();
        assert_eq!(pixel.preview["rows"][0]["value"], 6.0);
        assert_eq!(pixel.preview["world_coordinate"]["x"], 2.5);

        let region = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::RasterRegion {
                    geometry: GeoGeometry {
                        geojson: json!({
                            "type": "Polygon",
                            "coordinates": [[[1.0, 1.0], [3.0, 1.0], [3.0, 3.0], [1.0, 3.0], [1.0, 1.0]]]
                        }),
                    },
                    crs: "EPSG:4326".into(),
                    bands: vec![0],
                    statistics: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(region.preview["pixel_count"], 4);
        assert_eq!(region.preview["rows"][0]["statistics"]["mean"], 7.5);
        let export = service
            .export_result(ExportRequest {
                result_id: region.result_id,
                format: ExportFormat::GeoTiff,
                include_provenance: true,
                include_reproduction_code: true,
            })
            .unwrap();
        let exported_path = service
            .paths()
            .exports_dir
            .join(format!("{}.tif", export.export_id));
        let exported = geotiff_reader::GeoTiffFile::open(exported_path).unwrap();
        assert_eq!((exported.width(), exported.height()), (2, 2));
        assert_eq!(exported.epsg(), Some(4326));
    }

    #[tokio::test]
    async fn filters_and_exports_geojson_features() {
        let (directory, service) = service();
        let path = directory.path().join("plots.geojson");
        serde_json::to_writer(
            File::create(&path).unwrap(),
            &json!({
                "type": "FeatureCollection",
                "features": [
                    {"type": "Feature", "properties": {"plot": "inside"}, "geometry": {"type": "Point", "coordinates": [1.0, 1.0]}},
                    {"type": "Feature", "properties": {"plot": "outside"}, "geometry": {"type": "Point", "coordinates": [10.0, 10.0]}}
                ]
            }),
        )
        .unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        assert_eq!(manifest.source_files[0].metadata["feature_count"], 2);
        let result = service
            .query_dataset(
                &manifest.dataset_id.0,
                DatasetQuery::VectorRegion {
                    geometry: GeoGeometry {
                        geojson: json!({
                            "type": "Polygon",
                            "coordinates": [[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0], [0.0, 0.0]]]
                        }),
                    },
                    crs: "source".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(result.row_count, Some(1));
        assert_eq!(result.preview["rows"][0]["properties"]["plot"], "inside");
        let export = service
            .export_result(ExportRequest {
                result_id: result.result_id,
                format: ExportFormat::GeoJson,
                include_provenance: true,
                include_reproduction_code: true,
            })
            .unwrap();
        assert!(export.artifact.ends_with("/data"));
    }
}
