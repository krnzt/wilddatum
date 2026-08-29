//! Stable EcoScope-to-Rerun boundary.
//!
//! Rerun recordings are derived visualization artifacts. EcoViewSpec and
//! DatasetManifest remain the authoritative state.

mod tabular;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use ecoscope_core::{
    DatasetManifest, EcoScopeError, MAX_RENDERED_POINT_CLOUD_POINTS, Modality, Result, ViewLayout,
};
use ecoscope_service::EcoScopeService;
use rerun::blueprint::{
    Blueprint, BlueprintActivation, ContainerLike, Grid, Horizontal, MapView, Spatial2DView,
    Spatial3DView, Tabs, TextDocumentView, TimeSeriesView, Vertical,
};
use serde_json::Value;

const MAX_HYPERSPECTRAL_EDGE: usize = 1_024;
const MAX_VECTOR_FEATURES: usize = 100_000;

pub use ecoscope_core::PINNED_RERUN_VERSION;

#[derive(Debug)]
struct BlueprintLayer {
    name: String,
    entity_root: String,
    kind: BlueprintLayerKind,
}

#[derive(Debug)]
enum BlueprintLayerKind {
    Modality(Modality),
    ProfileTrajectory { value_field: String },
}

pub fn write_recording(
    service: &EcoScopeService,
    view_id: &str,
    output: impl AsRef<Path>,
) -> Result<PathBuf> {
    let view = service.get_view(view_id)?;
    let output = output.as_ref().to_path_buf();
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let application_id =
        rerun::ApplicationId::try_new(format!("ecoscope-{}", view.view_id)).map_err(rerun_error)?;
    let recording = rerun::RecordingStreamBuilder::new(application_id)
        .save(&output)
        .map_err(rerun_error)?;

    let view_json = serde_json::to_string_pretty(&view)?;
    recording
        .log_static(
            "ecoscope/view_spec",
            &rerun::TextDocument::new(view_json).with_media_type(rerun::MediaType::markdown()),
        )
        .map_err(rerun_error)?;

    let mut blueprint_layers = Vec::new();
    for layer in &view.layers {
        let manifest = service.get_manifest(&layer.dataset_id.0)?;
        let entity_root = format!("datasets/{}/{}", manifest.dataset_id, safe_name(&layer.id));
        let is_profile_trajectory = tabular::is_profile_trajectory(&layer.encoding);
        blueprint_layers.push(BlueprintLayer {
            name: layer.name.clone(),
            entity_root: entity_root.clone(),
            kind: if is_profile_trajectory {
                BlueprintLayerKind::ProfileTrajectory {
                    value_field: tabular::profile_value_field(&layer.encoding)
                        .unwrap_or("value")
                        .to_owned(),
                }
            } else {
                BlueprintLayerKind::Modality(layer.modality.clone())
            },
        });
        recording
            .log_static(
                format!("{entity_root}/manifest"),
                &rerun::TextDocument::new(serde_json::to_string_pretty(&manifest)?),
            )
            .map_err(rerun_error)?;

        let source_path = source_path(service, &manifest);
        let extension = manifest
            .source_files
            .first()
            .and_then(|source| Path::new(&source.original_name).extension())
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        let original_name = manifest
            .source_files
            .first()
            .map(|source| source.original_name.as_str())
            .unwrap_or_default();

        if is_profile_trajectory {
            match source_path.and_then(|path| {
                tabular::log_profile_trajectory(
                    &recording,
                    &entity_root,
                    &path,
                    original_name,
                    &layer.encoding,
                )
            }) {
                Ok(()) => {}
                Err(error) => log_adapter_notice(&recording, &entity_root, error.to_string())?,
            }
        } else if matches!(
            extension.as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "tif" | "tiff"
        ) {
            match source_path.and_then(|path| log_image(&recording, &entity_root, &path)) {
                Ok(()) => {}
                Err(error) => log_adapter_notice(&recording, &entity_root, error.to_string())?,
            }
        } else if matches!(extension.as_str(), "las" | "laz" | "copc") {
            match source_path.and_then(|path| log_point_cloud(&recording, &entity_root, &path)) {
                Ok(()) => {}
                Err(error) => log_adapter_notice(&recording, &entity_root, error.to_string())?,
            }
        } else if matches!(extension.as_str(), "h5" | "hdf5" | "nc" | "nc4" | "zarr") {
            match source_path.and_then(|path| {
                log_hyperspectral(&recording, &entity_root, &path, &layer.encoding)
            }) {
                Ok(()) => {}
                Err(error) => log_adapter_notice(&recording, &entity_root, error.to_string())?,
            }
        } else if matches!(extension.as_str(), "geojson" | "json" | "fgb" | "shp") {
            match source_path
                .and_then(|path| log_vector(&recording, &entity_root, &path, &extension))
            {
                Ok(()) => {}
                Err(error) => log_adapter_notice(&recording, &entity_root, error.to_string())?,
            }
        } else if matches!(layer.modality, Modality::Tabular | Modality::TimeSeries)
            && let Ok(preview) = service.preview_dataset(&manifest.dataset_id.0, 2_000)
            && let Some(rows) = preview.get("rows").and_then(serde_json::Value::as_array)
        {
            for (row_index, row) in rows.iter().enumerate() {
                recording.set_time_sequence("row", row_index as i64);
                if let Some(fields) = row.as_object() {
                    for (field, value) in fields {
                        if let Some(number) =
                            value.as_str().and_then(|value| value.parse::<f64>().ok())
                        {
                            recording
                                .log(
                                    format!("{entity_root}/series/{}", safe_name(field)),
                                    &rerun::Scalars::single(number),
                                )
                                .map_err(rerun_error)?;
                        }
                    }
                }
            }
        } else if matches!(
            layer.modality,
            Modality::Hyperspectral | Modality::Tensor | Modality::Raster | Modality::Vector
        ) {
            log_adapter_notice(
                &recording,
                &entity_root,
                format!(
                    "{} is catalogued as {:?}. Set an explicit axis/band mapping before a derived preview is rendered.",
                    manifest.resource_id, layer.modality
                ),
            )?;
        }
    }

    send_blueprint(
        &recording,
        &view.layout,
        blueprint_layers,
        view.provenance_visible,
    )?;

    recording.flush_blocking().map_err(rerun_error)?;
    Ok(output)
}

fn send_blueprint(
    recording: &rerun::RecordingStream,
    layout: &ViewLayout,
    layers: Vec<BlueprintLayer>,
    provenance_visible: bool,
) -> Result<()> {
    let mut contents = Vec::new();
    for layer in layers {
        let name = layer.name;
        let root = layer.entity_root;
        match layer.kind {
            BlueprintLayerKind::ProfileTrajectory { value_field } => {
                let map = ContainerLike::from(
                    MapView::new(format!("{name} map"))
                        .with_origin(root.clone())
                        .with_contents([
                            format!("{root}/map_observations"),
                            format!("{root}/trajectory_lines"),
                        ]),
                );
                let profile = ContainerLike::from(
                    Spatial2DView::new(format!("{value_field} profile"))
                        .with_origin(root.clone())
                        .with_contents([
                            format!("{root}/profile_observations"),
                            format!("{root}/profile_lines"),
                        ]),
                );
                contents.push(ContainerLike::from(Horizontal::new([map, profile])));
            }
            BlueprintLayerKind::Modality(modality) => {
                let layer_contents = [format!("{root}/**")];
                contents.push(match modality {
                    Modality::PointCloud => ContainerLike::from(
                        Spatial3DView::new(name)
                            .with_origin(root)
                            .with_contents(layer_contents),
                    ),
                    Modality::TimeSeries | Modality::Tabular => ContainerLike::from(
                        TimeSeriesView::new(name)
                            .with_origin(root)
                            .with_contents(layer_contents),
                    ),
                    Modality::Raster
                    | Modality::Hyperspectral
                    | Modality::Image
                    | Modality::Tensor
                    | Modality::Vector => ContainerLike::from(
                        Spatial2DView::new(name)
                            .with_origin(root)
                            .with_contents(layer_contents),
                    ),
                    Modality::Unknown => ContainerLike::from(
                        TextDocumentView::new(name).with_origin(format!("{root}/adapter_notice")),
                    ),
                });
            }
        }
    }
    if provenance_visible {
        contents.push(ContainerLike::from(
            TextDocumentView::new("EcoScope provenance").with_origin("ecoscope"),
        ));
    }

    let root: ContainerLike = match layout {
        ViewLayout::Single | ViewLayout::Tabs => Tabs::new(contents).into(),
        ViewLayout::Horizontal => Horizontal::new(contents).into(),
        ViewLayout::Vertical => Vertical::new(contents).into(),
        ViewLayout::Grid { columns } => Grid::new(contents)
            .with_grid_columns((*columns).max(1))
            .into(),
    };
    Blueprint::new(root)
        .with_auto_views(false)
        .with_auto_layout(false)
        .send(recording, BlueprintActivation::default())
        .map_err(rerun_error)
}

fn source_path(service: &EcoScopeService, manifest: &DatasetManifest) -> Result<PathBuf> {
    let source = manifest
        .source_files
        .first()
        .ok_or_else(|| EcoScopeError::Invalid("dataset has no source files".into()))?;
    service.source_path_for_renderer(manifest, source)
}

fn log_image(recording: &rerun::RecordingStream, entity_root: &str, path: &Path) -> Result<()> {
    let image = image::open(path)
        .map_err(|error| EcoScopeError::Invalid(format!("cannot decode image/raster: {error}")))?;
    let image = rerun::Image::from_image(image)
        .map_err(|error| EcoScopeError::Invalid(format!("cannot convert image: {error}")))?;
    recording
        .log_static(format!("{entity_root}/raster"), &image)
        .map_err(rerun_error)
}

fn log_point_cloud(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    path: &Path,
) -> Result<()> {
    let mut reader = las::Reader::from_path(path)
        .map_err(|error| EcoScopeError::Invalid(format!("cannot decode LAS/LAZ: {error}")))?;
    let bounds = reader.header().bounds();
    let total_points = reader.header().number_of_points();
    let stride = point_sampling_stride(total_points);
    let origin = [bounds.min.x, bounds.min.y, bounds.min.z];
    let mut positions =
        Vec::with_capacity(total_points.min(MAX_RENDERED_POINT_CLOUD_POINTS) as usize);
    let mut colors = Vec::with_capacity(positions.capacity());
    for (index, point) in reader.points().enumerate() {
        if !(index as u64).is_multiple_of(stride) {
            continue;
        }
        let point = point
            .map_err(|error| EcoScopeError::Invalid(format!("cannot decode LAS point: {error}")))?;
        positions.push([
            (point.x - origin[0]) as f32,
            (point.y - origin[1]) as f32,
            (point.z - origin[2]) as f32,
        ]);
        colors.push(point_color(&point));
    }
    recording
        .log_static(
            format!("{entity_root}/points"),
            &rerun::Points3D::new(positions)
                .with_colors(colors)
                .with_radii([rerun::Radius::new_ui_points(2.5)]),
        )
        .map_err(rerun_error)?;
    recording
        .log_static(
            format!("{entity_root}/point_cloud_info"),
            &rerun::TextDocument::new(
                serde_json::json!({
                    "source_points": total_points,
                    "rendered_points": total_points.div_ceil(stride),
                    "sampling_stride": stride,
                    "coordinate_origin": origin,
                    "coordinate_note": "Positions are offset from the source minimum to retain f32 precision; source coordinates remain in the manifest."
                })
                .to_string(),
            )
            .with_media_type(rerun::MediaType::markdown()),
        )
        .map_err(rerun_error)
}

fn log_vector(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    path: &Path,
    extension: &str,
) -> Result<()> {
    let features = load_vector_features(path, extension)?;
    let source_feature_count = features.len();
    let mut points = Vec::<[f64; 2]>::new();
    let mut strips = Vec::<Vec<[f64; 2]>>::new();
    for feature in features.into_iter().take(MAX_VECTOR_FEATURES) {
        if let Some(geometry) = feature.geometry {
            collect_vector_primitives(&geometry.value, &mut points, &mut strips);
        }
    }
    let origin_x = points
        .iter()
        .map(|point| point[0])
        .chain(strips.iter().flatten().map(|point| point[0]))
        .fold(f64::INFINITY, f64::min);
    let origin_y = points
        .iter()
        .map(|point| point[1])
        .chain(strips.iter().flatten().map(|point| point[1]))
        .fold(f64::INFINITY, f64::min);
    let origin = if origin_x.is_finite() && origin_y.is_finite() {
        [origin_x, origin_y]
    } else {
        [0.0, 0.0]
    };
    if !points.is_empty() {
        recording
            .log_static(
                format!("{entity_root}/vector_points"),
                &rerun::Points2D::new(
                    points.iter().map(|point| {
                        [(point[0] - origin[0]) as f32, (point[1] - origin[1]) as f32]
                    }),
                ),
            )
            .map_err(rerun_error)?;
    }
    if !strips.is_empty() {
        recording
            .log_static(
                format!("{entity_root}/vector_lines"),
                &rerun::LineStrips2D::new(strips.iter().map(|strip| {
                    strip
                        .iter()
                        .map(|point| [(point[0] - origin[0]) as f32, (point[1] - origin[1]) as f32])
                        .collect::<Vec<_>>()
                })),
            )
            .map_err(rerun_error)?;
    }
    recording
        .log_static(
            format!("{entity_root}/vector_info"),
            &rerun::TextDocument::new(
                serde_json::json!({
                    "source_features": source_feature_count,
                    "rendered_feature_budget": MAX_VECTOR_FEATURES,
                    "rendered_points": points.len(),
                    "rendered_line_strips": strips.len(),
                    "coordinate_origin": origin,
                    "coordinate_note": "Positions are offset from the source minimum to retain f32 precision; selections and queries use source coordinates."
                })
                .to_string(),
            )
            .with_media_type(rerun::MediaType::markdown()),
        )
        .map_err(rerun_error)
}

fn load_vector_features(path: &Path, extension: &str) -> Result<Vec<geojson::Feature>> {
    match extension {
        "geojson" | "json" => {
            let document: geojson::GeoJson = serde_json::from_reader(std::fs::File::open(path)?)
                .map_err(|error| EcoScopeError::Invalid(format!("invalid GeoJSON: {error}")))?;
            Ok(match document {
                geojson::GeoJson::FeatureCollection(collection) => collection.features,
                geojson::GeoJson::Feature(feature) => vec![feature],
                geojson::GeoJson::Geometry(geometry) => vec![geojson::Feature {
                    geometry: Some(geometry),
                    ..Default::default()
                }],
            })
        }
        "fgb" => {
            use flatgeobuf::FallibleStreamingIterator;
            use geozero::ToJson;

            let input = std::io::BufReader::new(std::fs::File::open(path)?);
            let mut reader = flatgeobuf::FgbReader::open(input)
                .and_then(flatgeobuf::FgbReader::select_all)
                .map_err(|error| {
                    EcoScopeError::Invalid(format!("cannot open FlatGeobuf: {error}"))
                })?;
            let mut output = Vec::new();
            while output.len() < MAX_VECTOR_FEATURES {
                let Some(feature) = reader.next().map_err(|error| {
                    EcoScopeError::Invalid(format!("cannot read FlatGeobuf: {error}"))
                })?
                else {
                    break;
                };
                let text = feature.to_json().map_err(|error| {
                    EcoScopeError::Invalid(format!("cannot decode FlatGeobuf: {error}"))
                })?;
                output.push(text.parse().map_err(|error| {
                    EcoScopeError::Invalid(format!("invalid FlatGeobuf feature: {error}"))
                })?);
            }
            Ok(output)
        }
        "shp" => {
            let mut reader = shapefile::Reader::from_path(path).map_err(|error| {
                EcoScopeError::Invalid(format!("cannot open Shapefile: {error}"))
            })?;
            reader
                .iter_shapes_and_records()
                .take(MAX_VECTOR_FEATURES)
                .map(|entry| {
                    let (shape, _) = entry.map_err(|error| {
                        EcoScopeError::Invalid(format!("cannot read Shapefile: {error}"))
                    })?;
                    let geometry: geo::Geometry<f64> = shape.try_into().map_err(|error| {
                        EcoScopeError::Invalid(format!("unsupported Shapefile shape: {error}"))
                    })?;
                    Ok(geojson::Feature {
                        geometry: Some(geojson::Geometry::new(geojson::Value::from(&geometry))),
                        ..Default::default()
                    })
                })
                .collect()
        }
        _ => Err(EcoScopeError::Invalid(format!(
            "unsupported vector renderer for {extension}"
        ))),
    }
}

fn collect_vector_primitives(
    geometry: &geojson::Value,
    points: &mut Vec<[f64; 2]>,
    strips: &mut Vec<Vec<[f64; 2]>>,
) {
    fn coordinate(value: &[f64]) -> Option<[f64; 2]> {
        (value.len() >= 2 && value[0].is_finite() && value[1].is_finite())
            .then_some([value[0], value[1]])
    }
    match geometry {
        geojson::Value::Point(point) => points.extend(coordinate(point)),
        geojson::Value::MultiPoint(values) => {
            points.extend(values.iter().filter_map(|point| coordinate(point)));
        }
        geojson::Value::LineString(line) => {
            strips.push(line.iter().filter_map(|point| coordinate(point)).collect());
        }
        geojson::Value::MultiLineString(lines) | geojson::Value::Polygon(lines) => {
            strips.extend(
                lines
                    .iter()
                    .map(|line| line.iter().filter_map(|point| coordinate(point)).collect()),
            );
        }
        geojson::Value::MultiPolygon(polygons) => {
            for polygon in polygons {
                strips.extend(
                    polygon
                        .iter()
                        .map(|line| line.iter().filter_map(|point| coordinate(point)).collect()),
                );
            }
        }
        geojson::Value::GeometryCollection(geometries) => {
            for geometry in geometries {
                collect_vector_primitives(&geometry.value, points, strips);
            }
        }
    }
}

fn point_sampling_stride(total_points: u64) -> u64 {
    total_points
        .div_ceil(MAX_RENDERED_POINT_CLOUD_POINTS)
        .max(1)
}

fn log_hyperspectral(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    path: &Path,
    encoding: &BTreeMap<String, Value>,
) -> Result<()> {
    let dataset_path = encoding
        .get("cube_array")
        .or_else(|| encoding.get("hdf5_dataset"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            EcoScopeError::Invalid(
                "hyperspectral rendering requires layer.encoding.cube_array and an explicit axis/band mapping"
                    .into(),
            )
        })?;
    if path.is_dir() {
        return log_zarr_hyperspectral(recording, entity_root, path, dataset_path, encoding);
    }
    let mut magic = [0_u8; 8];
    use std::io::Read;
    let read = std::fs::File::open(path)?.read(&mut magic)?;
    if read == magic.len() && magic == *b"\x89HDF\r\n\x1a\n" {
        return log_hdf5_hyperspectral(recording, entity_root, path, dataset_path, encoding);
    }
    if read >= 4 && &magic[..3] == b"CDF" {
        return log_netcdf3_hyperspectral(recording, entity_root, path, dataset_path, encoding);
    }
    Err(EcoScopeError::Invalid(
        "hyperspectral renderer supports HDF5/NetCDF-4, NetCDF-3, and Zarr".into(),
    ))
}

fn log_hdf5_hyperspectral(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    path: &Path,
    dataset_path: &str,
    encoding: &BTreeMap<String, Value>,
) -> Result<()> {
    let file = hdf5_metno::File::open(path)
        .map_err(|error| EcoScopeError::Invalid(format!("cannot open HDF5/NetCDF-4: {error}")))?;
    let dataset = file.dataset(dataset_path).map_err(|error| {
        EcoScopeError::Invalid(format!("cannot open HDF5 dataset {dataset_path}: {error}"))
    })?;
    let shape = dataset.shape();
    render_hyperspectral(
        recording,
        entity_root,
        dataset_path,
        &shape,
        encoding,
        |band, y_stride, x_stride, y_axis, x_axis, spectral_axis| {
            read_hdf5_band(
                &dataset,
                band,
                y_stride,
                x_stride,
                y_axis,
                x_axis,
                spectral_axis,
            )
        },
    )
}

fn log_zarr_hyperspectral(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    path: &Path,
    dataset_path: &str,
    encoding: &BTreeMap<String, Value>,
) -> Result<()> {
    use std::sync::Arc;
    use zarrs::{array::Array, filesystem::FilesystemStore};

    let store = Arc::new(
        FilesystemStore::new(path)
            .map_err(|error| EcoScopeError::Invalid(format!("cannot open Zarr store: {error}")))?,
    );
    let array = Array::open(store, dataset_path).map_err(|error| {
        EcoScopeError::Invalid(format!("cannot open Zarr array {dataset_path}: {error}"))
    })?;
    let shape = array
        .shape()
        .iter()
        .map(|value| *value as usize)
        .collect::<Vec<_>>();
    render_hyperspectral(
        recording,
        entity_root,
        dataset_path,
        &shape,
        encoding,
        |band, y_stride, x_stride, y_axis, x_axis, spectral_axis| {
            read_zarr_band(
                &array,
                band,
                y_stride,
                x_stride,
                y_axis,
                x_axis,
                spectral_axis,
            )
        },
    )
}

fn log_netcdf3_hyperspectral(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    path: &Path,
    dataset_path: &str,
    encoding: &BTreeMap<String, Value>,
) -> Result<()> {
    let mut reader = netcdf3::FileReader::open(path)
        .map_err(|error| EcoScopeError::Invalid(format!("cannot open NetCDF-3: {error}")))?;
    let variable = reader
        .data_set()
        .get_var(dataset_path)
        .ok_or_else(|| EcoScopeError::NotFound(format!("NetCDF-3 variable {dataset_path}")))?;
    let shape = variable
        .get_dims()
        .iter()
        .map(|dimension| dimension.size())
        .collect::<Vec<_>>();
    if variable.len() > 16_000_000 {
        return Err(EcoScopeError::Invalid(
            "NetCDF-3 preview is limited to variables containing at most 16,000,000 cells because the pure-Rust reader does not yet support subset I/O"
                .into(),
        ));
    }
    let values = match reader.read_var(dataset_path).map_err(|error| {
        EcoScopeError::Invalid(format!("cannot read NetCDF-3 variable: {error}"))
    })? {
        netcdf3::DataVector::I8(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::U8(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::I16(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::I32(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::F32(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::F64(values) => values,
    };
    render_hyperspectral(
        recording,
        entity_root,
        dataset_path,
        &shape,
        encoding,
        |band, y_stride, x_stride, y_axis, x_axis, spectral_axis| {
            read_memory_band(
                &values,
                &shape,
                band,
                y_stride,
                x_stride,
                y_axis,
                x_axis,
                spectral_axis,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn render_hyperspectral(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    dataset_path: &str,
    shape: &[usize],
    encoding: &BTreeMap<String, Value>,
    mut read_band: impl FnMut(usize, usize, usize, usize, usize, usize) -> Result<Vec<f64>>,
) -> Result<()> {
    let spectral_axis = encoding
        .get("spectral_axis")
        .and_then(Value::as_u64)
        .unwrap_or(2) as usize;
    let y_axis = encoding.get("y_axis").and_then(Value::as_u64).unwrap_or(0) as usize;
    let x_axis = encoding.get("x_axis").and_then(Value::as_u64).unwrap_or(1) as usize;
    if shape.len() != 3
        || y_axis >= shape.len()
        || x_axis >= shape.len()
        || spectral_axis >= shape.len()
        || y_axis == x_axis
        || y_axis == spectral_axis
        || x_axis == spectral_axis
    {
        return Err(EcoScopeError::Invalid(
            "hyperspectral rendering requires a rank-3 cube and distinct valid y_axis, x_axis, and spectral_axis values"
                .into(),
        ));
    }
    if shape.contains(&0) {
        return Err(EcoScopeError::Invalid(format!(
            "cube {dataset_path} must have a non-empty shape, got {shape:?}"
        )));
    }
    let y_stride = shape[y_axis].div_ceil(MAX_HYPERSPECTRAL_EDGE).max(1);
    let x_stride = shape[x_axis].div_ceil(MAX_HYPERSPECTRAL_EDGE).max(1);
    let width = shape[x_axis].div_ceil(x_stride);
    let height = shape[y_axis].div_ceil(y_stride);
    let no_data = encoding.get("no_data").and_then(Value::as_f64);

    let single_band = encoding
        .get("band")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let rgb_bands = ["red_band", "green_band", "blue_band"].map(|key| {
        encoding
            .get(key)
            .and_then(Value::as_u64)
            .map(|value| value as usize)
    });
    if let Some(band) = single_band {
        validate_band(band, shape[spectral_axis])?;
        let values = read_band(band, y_stride, x_stride, y_axis, x_axis, spectral_axis)?;
        let (minimum, maximum) = configured_or_robust_range(encoding, &values, no_data);
        let bytes = values
            .iter()
            .map(|value| scale_sample(*value, minimum, maximum, no_data))
            .collect::<Vec<_>>();
        recording
            .log_static(
                format!("{entity_root}/hyperspectral_band"),
                &rerun::Image::from_l8(bytes, [width as u32, height as u32]),
            )
            .map_err(rerun_error)?;
    } else if let [Some(red_band), Some(green_band), Some(blue_band)] = rgb_bands {
        for band in [red_band, green_band, blue_band] {
            validate_band(band, shape[spectral_axis])?;
        }
        let red = read_band(red_band, y_stride, x_stride, y_axis, x_axis, spectral_axis)?;
        let green = read_band(
            green_band,
            y_stride,
            x_stride,
            y_axis,
            x_axis,
            spectral_axis,
        )?;
        let blue = read_band(blue_band, y_stride, x_stride, y_axis, x_axis, spectral_axis)?;
        let mut combined = Vec::with_capacity(red.len() + green.len() + blue.len());
        combined.extend(red.iter().copied());
        combined.extend(green.iter().copied());
        combined.extend(blue.iter().copied());
        let (minimum, maximum) = configured_or_robust_range(encoding, &combined, no_data);
        let mut bytes = Vec::with_capacity(red.len() * 3);
        for index in 0..red.len() {
            bytes.extend([
                scale_sample(red[index], minimum, maximum, no_data),
                scale_sample(green[index], minimum, maximum, no_data),
                scale_sample(blue[index], minimum, maximum, no_data),
            ]);
        }
        recording
            .log_static(
                format!("{entity_root}/hyperspectral_rgb"),
                &rerun::Image::from_rgb24(bytes, [width as u32, height as u32]),
            )
            .map_err(rerun_error)?;
    } else {
        return Err(EcoScopeError::Invalid(
            "set either layer.encoding.band or all of red_band, green_band, and blue_band".into(),
        ));
    }
    recording
        .log_static(
            format!("{entity_root}/hyperspectral_mapping"),
            &rerun::TextDocument::new(
                serde_json::json!({
                    "dataset": dataset_path,
                    "source_shape": shape,
                    "preview_shape": [height, width],
                    "stride": [y_stride, x_stride],
                    "axes": {"y": y_axis, "x": x_axis, "spectral": spectral_axis},
                    "encoding": encoding
                })
                .to_string(),
            )
            .with_media_type(rerun::MediaType::markdown()),
        )
        .map_err(rerun_error)
}

fn validate_band(band: usize, available_bands: usize) -> Result<()> {
    if band < available_bands {
        Ok(())
    } else {
        Err(EcoScopeError::Invalid(format!(
            "band {band} is outside 0..{available_bands}"
        )))
    }
}

fn read_hdf5_band(
    dataset: &hdf5_metno::Dataset,
    band: usize,
    y_stride: usize,
    x_stride: usize,
    y_axis: usize,
    x_axis: usize,
    spectral_axis: usize,
) -> Result<Vec<f64>> {
    use hdf5_metno::{Hyperslab, SliceOrIndex};

    let shape = dataset.shape();
    let selection = Hyperslab::new(
        shape
            .iter()
            .enumerate()
            .map(|(axis, length)| {
                if axis == spectral_axis {
                    SliceOrIndex::SliceTo {
                        start: band,
                        step: 1,
                        end: band + 1,
                        block: 1,
                    }
                } else {
                    SliceOrIndex::SliceTo {
                        start: 0,
                        step: if axis == y_axis {
                            y_stride
                        } else if axis == x_axis {
                            x_stride
                        } else {
                            1
                        },
                        end: *length,
                        block: 1,
                    }
                }
            })
            .collect::<Vec<_>>(),
    );
    let datatype = dataset.dtype().map_err(|error| {
        EcoScopeError::Invalid(format!("cannot inspect HDF5 datatype: {error}"))
    })?;
    macro_rules! read_as_f64 {
        ($type:ty) => {{
            dataset
                .read_slice::<$type, _, ndarray::IxDyn>(selection.clone())
                .map(|values| {
                    let sampled_shape = values.shape().to_vec();
                    let values = values.iter().map(|value| *value as f64).collect::<Vec<_>>();
                    read_memory_band(
                        &values,
                        &sampled_shape,
                        0,
                        1,
                        1,
                        y_axis,
                        x_axis,
                        spectral_axis,
                    )
                })
        }};
    }
    let values = if datatype.is::<u8>() {
        read_as_f64!(u8)
    } else if datatype.is::<u16>() {
        read_as_f64!(u16)
    } else if datatype.is::<u32>() {
        read_as_f64!(u32)
    } else if datatype.is::<i16>() {
        read_as_f64!(i16)
    } else if datatype.is::<i32>() {
        read_as_f64!(i32)
    } else if datatype.is::<f32>() {
        read_as_f64!(f32)
    } else if datatype.is::<f64>() {
        read_as_f64!(f64)
    } else {
        return Err(EcoScopeError::Invalid(format!(
            "unsupported HDF5 sample datatype {datatype:?}"
        )));
    };
    values.map_err(|error| EcoScopeError::Invalid(format!("cannot read HDF5 band: {error}")))?
}

#[allow(clippy::too_many_arguments)]
fn read_zarr_band<TStorage: ?Sized + zarrs::storage::ReadableStorageTraits + 'static>(
    array: &zarrs::array::Array<TStorage>,
    band: usize,
    y_stride: usize,
    x_stride: usize,
    y_axis: usize,
    x_axis: usize,
    spectral_axis: usize,
) -> Result<Vec<f64>> {
    use std::ops::Range;
    use zarrs::array::data_type;

    let shape = array.shape();
    let ranges = shape
        .iter()
        .enumerate()
        .map(|(axis, length)| {
            if axis == spectral_axis {
                Range {
                    start: band as u64,
                    end: band as u64 + 1,
                }
            } else {
                Range {
                    start: 0,
                    end: *length,
                }
            }
        })
        .collect::<Vec<_>>();
    let subset_cells = ranges
        .iter()
        .map(|range| range.end - range.start)
        .product::<u64>();
    if subset_cells > 16_000_000 {
        return Err(EcoScopeError::Invalid(
            "Zarr hyperspectral band preview exceeds the 16,000,000-cell decode budget".into(),
        ));
    }
    macro_rules! retrieve_as_f64 {
        ($type:ty) => {
            array
                .retrieve_array_subset::<Vec<$type>>(&ranges)
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>()
                })
        };
    }
    let values = if array.data_type() == &data_type::uint8() {
        retrieve_as_f64!(u8)
    } else if array.data_type() == &data_type::uint16() {
        retrieve_as_f64!(u16)
    } else if array.data_type() == &data_type::uint32() {
        retrieve_as_f64!(u32)
    } else if array.data_type() == &data_type::uint64() {
        retrieve_as_f64!(u64)
    } else if array.data_type() == &data_type::int8() {
        retrieve_as_f64!(i8)
    } else if array.data_type() == &data_type::int16() {
        retrieve_as_f64!(i16)
    } else if array.data_type() == &data_type::int32() {
        retrieve_as_f64!(i32)
    } else if array.data_type() == &data_type::int64() {
        retrieve_as_f64!(i64)
    } else if array.data_type() == &data_type::float32() {
        retrieve_as_f64!(f32)
    } else if array.data_type() == &data_type::float64() {
        retrieve_as_f64!(f64)
    } else {
        return Err(EcoScopeError::Invalid(format!(
            "unsupported Zarr sample datatype {:?}",
            array.data_type()
        )));
    }
    .map_err(|error| EcoScopeError::Invalid(format!("cannot read Zarr band: {error}")))?;
    let subset_shape = shape
        .iter()
        .enumerate()
        .map(|(axis, length)| {
            if axis == spectral_axis {
                1
            } else {
                *length as usize
            }
        })
        .collect::<Vec<_>>();
    read_memory_band(
        &values,
        &subset_shape,
        0,
        y_stride,
        x_stride,
        y_axis,
        x_axis,
        spectral_axis,
    )
}

#[allow(clippy::too_many_arguments)]
fn read_memory_band(
    values: &[f64],
    shape: &[usize],
    band: usize,
    y_stride: usize,
    x_stride: usize,
    y_axis: usize,
    x_axis: usize,
    spectral_axis: usize,
) -> Result<Vec<f64>> {
    if shape.len() != 3 || band >= shape[spectral_axis] {
        return Err(EcoScopeError::Invalid(
            "invalid rank or band for hyperspectral preview".into(),
        ));
    }
    let mut output =
        Vec::with_capacity(shape[y_axis].div_ceil(y_stride) * shape[x_axis].div_ceil(x_stride));
    for y in (0..shape[y_axis]).step_by(y_stride) {
        for x in (0..shape[x_axis]).step_by(x_stride) {
            let mut indices = [0_usize; 3];
            indices[y_axis] = y;
            indices[x_axis] = x;
            indices[spectral_axis] = band;
            let flat = indices
                .iter()
                .zip(shape)
                .fold(0_usize, |flat, (index, length)| flat * length + index);
            output.push(values[flat]);
        }
    }
    Ok(output)
}

fn configured_or_robust_range(
    encoding: &BTreeMap<String, Value>,
    values: &[f64],
    no_data: Option<f64>,
) -> (f64, f64) {
    if let (Some(minimum), Some(maximum)) = (
        encoding.get("display_min").and_then(Value::as_f64),
        encoding.get("display_max").and_then(Value::as_f64),
    ) {
        return (minimum, maximum);
    }
    let mut finite = values
        .iter()
        .copied()
        .filter(|value| value.is_finite() && no_data != Some(*value))
        .collect::<Vec<_>>();
    if finite.is_empty() {
        return (0.0, 1.0);
    }
    finite.sort_by(f64::total_cmp);
    let low = finite.len().saturating_sub(1) * 2 / 100;
    let high = finite.len().saturating_sub(1) * 98 / 100;
    let minimum = finite[low];
    let maximum = finite[high];
    if maximum > minimum {
        (minimum, maximum)
    } else {
        (minimum, minimum + 1.0)
    }
}

fn scale_sample(value: f64, minimum: f64, maximum: f64, no_data: Option<f64>) -> u8 {
    if !value.is_finite() || no_data == Some(value) {
        return 0;
    }
    (((value - minimum) / (maximum - minimum)).clamp(0.0, 1.0) * 255.0).round() as u8
}

fn point_color(point: &las::Point) -> [u8; 3] {
    if let Some(color) = point.color {
        return [
            scale_las_color(color.red),
            scale_las_color(color.green),
            scale_las_color(color.blue),
        ];
    }
    match u8::from(point.classification) {
        2 => [166, 124, 82],  // ground
        3 => [152, 211, 121], // low vegetation
        4 => [74, 168, 80],   // medium vegetation
        5 => [18, 112, 52],   // high vegetation
        6 => [198, 86, 64],   // building
        9 => [65, 132, 220],  // water
        17 => [130, 130, 140],
        _ => [210, 214, 220],
    }
}

fn scale_las_color(value: u16) -> u8 {
    if value <= u8::MAX as u16 {
        value as u8
    } else {
        (value >> 8) as u8
    }
}

fn log_adapter_notice(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    message: String,
) -> Result<()> {
    recording
        .log_static(
            format!("{entity_root}/adapter_notice"),
            &rerun::TextDocument::new(message).with_media_type(rerun::MediaType::markdown()),
        )
        .map_err(rerun_error)
}

pub fn open_recording(path: &Path) -> Result<()> {
    let status = std::process::Command::new("rerun")
        .arg(path)
        .spawn()
        .map_err(|error| {
            EcoScopeError::Internal(format!(
                "could not start the Rerun viewer; install `rerun` or open {} manually: {error}",
                path.display()
            ))
        })?;
    drop(status);
    Ok(())
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn rerun_error(error: impl std::fmt::Display) -> EcoScopeError {
    EcoScopeError::Internal(format!("Rerun recording error: {error}"))
}

#[cfg(test)]
mod tests {
    use ecoscope_core::DatasetId;
    use ecoscope_service::ServicePaths;

    use super::*;

    #[test]
    fn sanitizes_entity_paths() {
        assert_eq!(safe_name("canopy height (m)"), "canopy_height__m_");
    }

    #[test]
    fn bounds_large_neon_point_clouds_to_one_million_points() {
        let source_points = 6_609_829;
        let stride = point_sampling_stride(source_points);
        assert_eq!(stride, 7);
        assert!(source_points.div_ceil(stride) <= MAX_RENDERED_POINT_CLOUD_POINTS);
        assert_eq!(point_sampling_stride(999_999), 1);
    }

    #[test]
    fn reads_neon_style_unsigned_hdf5_bands() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reflectance.h5");
        let file = hdf5_metno::File::create(&path).unwrap();
        let dataset = file
            .new_dataset::<u32>()
            .shape([2, 3, 4])
            .create("reflectance")
            .unwrap();
        dataset.write_raw(&(0_u32..24).collect::<Vec<_>>()).unwrap();

        let band = read_hdf5_band(&dataset, 2, 1, 1, 0, 1, 2).unwrap();
        assert_eq!(band, vec![2.0, 6.0, 10.0, 14.0, 18.0, 22.0]);
    }

    #[tokio::test]
    async fn writes_image_lidar_and_hyperspectral_recordings() {
        use std::sync::Arc;

        let directory = tempfile::tempdir().unwrap();
        let service = EcoScopeService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();

        let image_path = directory.path().join("canopy.png");
        image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8, y as u8, 64]))
            .save(&image_path)
            .unwrap();
        let image_manifest = service.import_local_file(&image_path).await.unwrap();

        let lidar_path = directory.path().join("canopy.las");
        let mut writer = las::Writer::from_path(&lidar_path, las::Header::default()).unwrap();
        for index in 0..8 {
            writer
                .write_point(las::Point {
                    x: index as f64,
                    y: (index % 3) as f64,
                    z: (index * 2) as f64,
                    ..Default::default()
                })
                .unwrap();
        }
        writer.close().unwrap();
        let lidar_manifest = service.import_local_file(&lidar_path).await.unwrap();

        let hdf5_path = directory.path().join("reflectance.h5");
        let file = hdf5_metno::File::create(&hdf5_path).unwrap();
        let dataset = file
            .new_dataset::<u32>()
            .shape([4, 5, 3])
            .create("reflectance")
            .unwrap();
        dataset.write_raw(&(0_u32..60).collect::<Vec<_>>()).unwrap();
        drop(file);
        let hdf5_manifest = service.import_local_file(&hdf5_path).await.unwrap();

        let zarr_path = directory.path().join("reflectance.zarr");
        std::fs::create_dir(&zarr_path).unwrap();
        let store = Arc::new(zarrs::filesystem::FilesystemStore::new(&zarr_path).unwrap());
        let zarr_array = zarrs::array::ArrayBuilder::new(
            vec![3, 4, 5],
            vec![1, 2, 3],
            zarrs::array::data_type::uint16(),
            0_u16,
        )
        .dimension_names(["wavelength", "y", "x"].into())
        .build(store, "/")
        .unwrap();
        zarr_array.store_metadata().unwrap();
        zarr_array
            .store_array_subset(&[0..3, 0..4, 0..5], (0_u16..60).collect::<Vec<_>>())
            .unwrap();
        let zarr_manifest = service.import_local_file(&zarr_path).await.unwrap();

        let vector_path = directory.path().join("plots.geojson");
        serde_json::to_writer(
            std::fs::File::create(&vector_path).unwrap(),
            &serde_json::json!({
                "type": "FeatureCollection",
                "features": [
                    {"type": "Feature", "properties": {"plot": "A"}, "geometry": {"type": "Point", "coordinates": [1000.0, 2000.0]}},
                    {"type": "Feature", "properties": {"plot": "B"}, "geometry": {"type": "Polygon", "coordinates": [[[1001.0, 2001.0], [1003.0, 2001.0], [1003.0, 2003.0], [1001.0, 2003.0], [1001.0, 2001.0]]]}}
                ]
            }),
        )
        .unwrap();
        let vector_manifest = service.import_local_file(&vector_path).await.unwrap();

        let mut view = service
            .create_view(
                "Multimodal fixture".into(),
                vec![
                    DatasetId(image_manifest.dataset_id.0),
                    DatasetId(lidar_manifest.dataset_id.0),
                    DatasetId(hdf5_manifest.dataset_id.0),
                    DatasetId(zarr_manifest.dataset_id.0),
                    DatasetId(vector_manifest.dataset_id.0),
                ],
            )
            .unwrap();
        view.layers[2]
            .encoding
            .insert("hdf5_dataset".into(), serde_json::json!("/reflectance"));
        view.layers[2]
            .encoding
            .insert("red_band".into(), serde_json::json!(2));
        view.layers[2]
            .encoding
            .insert("green_band".into(), serde_json::json!(1));
        view.layers[2]
            .encoding
            .insert("blue_band".into(), serde_json::json!(0));
        view.layers[3]
            .encoding
            .insert("cube_array".into(), serde_json::json!("/"));
        view.layers[3]
            .encoding
            .insert("spectral_axis".into(), serde_json::json!(0));
        view.layers[3]
            .encoding
            .insert("y_axis".into(), serde_json::json!(1));
        view.layers[3]
            .encoding
            .insert("x_axis".into(), serde_json::json!(2));
        view.layers[3]
            .encoding
            .insert("band".into(), serde_json::json!(1));
        service.save_view(&view).unwrap();
        let recording = directory.path().join("multimodal.rrd");
        write_recording(&service, &view.view_id.0, &recording).unwrap();
        assert!(std::fs::metadata(recording).unwrap().len() > 1_000);
    }
}
