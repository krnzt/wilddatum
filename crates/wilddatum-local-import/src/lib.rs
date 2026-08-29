//! Safe, user-mediated local source inspection.

use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use serde_json::json;
use wilddatum_core::{
    AssetId, AxisRole, Checksum, CubeAxis, CubeDescriptor, LocalAssetInspection, Modality, Result,
    WildDatumError,
};

const MAX_HDF5_DATASETS: usize = 256;
const MAX_COORDINATE_VALUES: usize = 1_024;

#[derive(Debug, Clone)]
pub struct Hdf5Structure {
    pub dimensions: Vec<u64>,
    pub dataset_paths: Vec<String>,
    pub datasets: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ScientificMetadataInspection {
    pub format: String,
    pub modalities: Vec<Modality>,
    pub dimensions: Vec<u64>,
    pub fields: Vec<String>,
    pub crs: Option<String>,
    pub warnings: Vec<String>,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

pub async fn inspect_path(path: &Path) -> Result<LocalAssetInspection> {
    let extension = extension(path);
    let is_zarr_directory = path.is_dir() && extension == "zarr";
    if !path.is_file() && !is_zarr_directory {
        return Err(WildDatumError::Invalid(
            "local imports must be a selected file or a .zarr directory".into(),
        ));
    }

    let path_for_hash = path.to_path_buf();
    let (fingerprint, size_bytes) =
        tokio::task::spawn_blocking(move || fingerprint_path_details(&path_for_hash))
            .await
            .map_err(|error| {
                WildDatumError::Internal(format!("fingerprint task failed: {error}"))
            })??;
    let mut inspection = LocalAssetInspection {
        asset_id: AssetId::new(),
        display_name: LocalAssetInspection::display_name_for(path),
        size_bytes,
        fingerprint,
        media_type: if is_zarr_directory {
            Some("application/vnd+zarr".into())
        } else {
            mime_guess::from_path(path).first_raw().map(str::to_owned)
        },
        modalities: modalities_for_extension(&extension),
        format: extension.clone(),
        dimensions: vec![],
        fields: vec![],
        crs: None,
        requires_mapping: false,
        warnings: vec![],
        metadata: BTreeMap::new(),
    };

    inspect_file_structure(path, &extension, is_zarr_directory, &mut inspection)?;

    Ok(inspection)
}

/// Inspect scientific structure without hashing the source again. Provider
/// materializers use this after download checksums have already been verified.
pub fn inspect_scientific_metadata(
    path: &Path,
    source_name: &str,
) -> Result<ScientificMetadataInspection> {
    let extension = extension(Path::new(source_name));
    let is_zarr_directory = path.is_dir() && extension == "zarr";
    if !path.is_file() && !is_zarr_directory {
        return Err(WildDatumError::Invalid(
            "scientific metadata inspection requires an existing file or .zarr directory".into(),
        ));
    }
    let mut inspection = LocalAssetInspection {
        asset_id: AssetId::new(),
        display_name: LocalAssetInspection::display_name_for(path),
        size_bytes: 0,
        fingerprint: Checksum {
            algorithm: "not_recomputed".into(),
            value: String::new(),
        },
        media_type: None,
        modalities: modalities_for_extension(&extension),
        format: extension.clone(),
        dimensions: vec![],
        fields: vec![],
        crs: None,
        requires_mapping: false,
        warnings: vec![],
        metadata: BTreeMap::new(),
    };
    inspect_file_structure(path, &extension, is_zarr_directory, &mut inspection)?;
    Ok(ScientificMetadataInspection {
        format: inspection.format,
        modalities: inspection.modalities,
        dimensions: inspection.dimensions,
        fields: inspection.fields,
        crs: inspection.crs,
        warnings: inspection.warnings,
        metadata: inspection.metadata,
    })
}

fn inspect_file_structure(
    path: &Path,
    extension: &str,
    is_zarr_directory: bool,
    inspection: &mut LocalAssetInspection,
) -> Result<()> {
    match extension {
        "csv" | "tsv" => inspect_delimited(path, extension == "tsv", inspection)?,
        "parquet" | "geoparquet" => inspect_parquet(path, inspection)?,
        "h5" | "hdf5" => {
            inspect_hdf5(path, inspection)?;
            inspection.requires_mapping = true;
            inspection.warnings.push(
                "Confirm spatial axes, spectral axis, wavelength coordinates, scale, no-data, and georeferencing"
                    .into(),
            );
        }
        "nc" | "nc4" => {
            inspect_netcdf(path, inspection)?;
            inspection.requires_mapping = true;
            inspection.warnings.push(
                "Multidimensional axes, wavelengths, and georeferencing require confirmation"
                    .into(),
            );
        }
        "las" | "laz" | "copc" => {
            inspect_lidar(path, inspection)?;
            inspection
                .metadata
                .insert("recommended_internal_format".into(), json!("copc"));
        }
        "tif" | "tiff" => {
            inspect_geotiff(path, inspection)?;
            inspection.metadata.insert(
                "recommended_internal_format".into(),
                json!("cloud_optimized_geotiff"),
            );
        }
        "geojson" | "json" => inspect_geojson(path, inspection)?,
        "shp" => inspect_shapefile(path, inspection)?,
        "fgb" => inspect_flatgeobuf(path, inspection)?,
        "png" | "jpg" | "jpeg" | "webp" => inspect_image(path, inspection)?,
        "zarr" => {
            inspect_zarr(path, inspection)?;
            inspection.requires_mapping = true;
            inspection.metadata.insert(
                "storage".into(),
                json!(if is_zarr_directory {
                    "directory"
                } else {
                    "archive"
                }),
            );
        }
        _ => {}
    }

    Ok(())
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Compute a stable streaming fingerprint without loading scientific assets into memory.
pub fn fingerprint_path(path: &Path) -> Result<Checksum> {
    fingerprint_path_details(path).map(|(checksum, _)| checksum)
}

fn fingerprint_path_details(path: &Path) -> Result<(Checksum, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut size_bytes = 0_u64;
    if path.is_file() && extension(path) == "shp" {
        hasher.update(b"wilddatum-shapefile-set-v1\0");
        for companion_extension in ["shp", "shx", "dbf", "prj", "cpg"] {
            let companion = path.with_extension(companion_extension);
            if companion.is_file() {
                hasher.update(&(companion_extension.len() as u64).to_le_bytes());
                hasher.update(companion_extension.as_bytes());
                hash_file(&companion, &mut hasher, &mut size_bytes)?;
            }
        }
    } else if path.is_file() {
        hash_file(path, &mut hasher, &mut size_bytes)?;
    } else if path.is_dir() && extension(path) == "zarr" {
        hasher.update(b"wilddatum-zarr-directory-v1\0");
        let mut files = walkdir::WalkDir::new(path)
            .follow_links(false)
            .into_iter()
            .map(|entry| {
                entry
                    .map_err(|error| WildDatumError::Invalid(format!("invalid Zarr tree: {error}")))
            })
            .collect::<Result<Vec<_>>>()?;
        files.sort_by_key(|entry| entry.path().to_path_buf());
        for entry in files {
            if entry.file_type().is_symlink() {
                return Err(WildDatumError::Invalid(
                    "Zarr imports may not contain symbolic links".into(),
                ));
            }
            if entry.file_type().is_file() {
                let relative = entry.path().strip_prefix(path).map_err(|error| {
                    WildDatumError::Internal(format!("cannot fingerprint Zarr path: {error}"))
                })?;
                let relative = relative.to_string_lossy();
                hasher.update(&(relative.len() as u64).to_le_bytes());
                hasher.update(relative.as_bytes());
                hash_file(entry.path(), &mut hasher, &mut size_bytes)?;
            }
        }
    } else {
        return Err(WildDatumError::Invalid(
            "asset is not a regular file or .zarr directory".into(),
        ));
    }
    Ok((
        Checksum {
            algorithm: "blake3".into(),
            value: hasher.finalize().to_hex().to_string(),
        },
        size_bytes,
    ))
}

fn hash_file(path: &Path, hasher: &mut blake3::Hasher, size_bytes: &mut u64) -> Result<()> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        *size_bytes += read as u64;
    }
    Ok(())
}

fn inspect_image(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    let reader = image::ImageReader::open(path)
        .map_err(WildDatumError::from)?
        .with_guessed_format()
        .map_err(WildDatumError::from)?;
    let (width, height) = reader.into_dimensions().map_err(|error| {
        WildDatumError::Invalid(format!("cannot read image dimensions: {error}"))
    })?;
    inspection.dimensions = vec![height as u64, width as u64];
    inspection.metadata.insert("width".into(), json!(width));
    inspection.metadata.insert("height".into(), json!(height));
    Ok(())
}

fn inspect_parquet(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let reader = SerializedFileReader::new(File::open(path)?).map_err(|error| {
        WildDatumError::Invalid(format!("cannot read Parquet metadata: {error}"))
    })?;
    let metadata = reader.metadata();
    inspection.dimensions = vec![metadata.file_metadata().num_rows() as u64];
    inspection.fields = metadata
        .file_metadata()
        .schema_descr()
        .columns()
        .iter()
        .map(|column| column.path().string())
        .collect();
    inspection.metadata.insert(
        "row_count".into(),
        json!(metadata.file_metadata().num_rows()),
    );
    inspection
        .metadata
        .insert("row_groups".into(), json!(metadata.num_row_groups()));
    let geo = metadata
        .file_metadata()
        .key_value_metadata()
        .and_then(|entries| entries.iter().find(|entry| entry.key == "geo"))
        .and_then(|entry| entry.value.as_deref())
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok());
    if let Some(geo) = geo {
        if !inspection.modalities.contains(&Modality::Vector) {
            inspection.modalities.push(Modality::Vector);
        }
        inspection.metadata.insert("geoparquet".into(), geo.clone());
        if let Some(primary) = geo
            .get("primary_column")
            .and_then(serde_json::Value::as_str)
        {
            inspection
                .metadata
                .insert("primary_geometry".into(), json!(primary));
            if let Some(crs) = geo
                .get("columns")
                .and_then(|columns| columns.get(primary))
                .and_then(|column| column.get("crs"))
            {
                inspection
                    .metadata
                    .insert("crs_projjson".into(), crs.clone());
            }
        }
    }
    Ok(())
}

fn inspect_geotiff(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    let raster = geotiff_reader::GeoTiffFile::open(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot read GeoTIFF: {error}")))?;
    inspection.dimensions = vec![
        raster.height() as u64,
        raster.width() as u64,
        raster.band_count() as u64,
    ];
    inspection
        .metadata
        .insert("width".into(), json!(raster.width()));
    inspection
        .metadata
        .insert("height".into(), json!(raster.height()));
    inspection
        .metadata
        .insert("band_count".into(), json!(raster.band_count()));
    inspection
        .metadata
        .insert("geo_bounds".into(), json!(raster.geo_bounds()));
    inspection
        .metadata
        .insert("nodata".into(), json!(raster.nodata()));
    inspection
        .metadata
        .insert("overview_count".into(), json!(raster.overview_count()));
    if let Some(transform) = raster.transform() {
        inspection.metadata.insert(
            "affine_transform".into(),
            json!([
                transform.origin_x,
                transform.pixel_width,
                transform.skew_x,
                transform.origin_y,
                transform.skew_y,
                transform.pixel_height
            ]),
        );
    }
    if let Some(epsg) = raster.epsg() {
        inspection.crs = Some(format!("EPSG:{epsg}"));
    }
    Ok(())
}

fn inspect_geojson(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    const MAX_GEOJSON_INSPECTION_BYTES: u64 = 256 * 1024 * 1024;
    if inspection.size_bytes > MAX_GEOJSON_INSPECTION_BYTES {
        inspection.requires_mapping = true;
        inspection.warnings.push(format!(
            "GeoJSON structure inspection is limited to {MAX_GEOJSON_INSPECTION_BYTES} bytes; convert this asset to FlatGeobuf or GeoParquet for indexed access"
        ));
        return Ok(());
    }
    let document: geojson::GeoJson = serde_json::from_reader(File::open(path)?)
        .map_err(|error| WildDatumError::Invalid(format!("invalid GeoJSON: {error}")))?;
    let mut fields = std::collections::BTreeSet::new();
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    let feature_count = match &document {
        geojson::GeoJson::FeatureCollection(collection) => {
            for feature in &collection.features {
                if let Some(properties) = &feature.properties {
                    fields.extend(properties.keys().cloned());
                }
                if let Some(geometry) = &feature.geometry {
                    collect_geojson_value_bounds(&geometry.value, &mut bounds);
                }
            }
            collection.features.len() as u64
        }
        geojson::GeoJson::Feature(feature) => {
            if let Some(properties) = &feature.properties {
                fields.extend(properties.keys().cloned());
            }
            if let Some(geometry) = &feature.geometry {
                collect_geojson_value_bounds(&geometry.value, &mut bounds);
            }
            1
        }
        geojson::GeoJson::Geometry(geometry) => {
            collect_geojson_value_bounds(&geometry.value, &mut bounds);
            1
        }
    };
    inspection.dimensions = vec![feature_count];
    inspection.fields = fields.into_iter().collect();
    inspection
        .metadata
        .insert("feature_count".into(), json!(feature_count));
    if bounds.iter().all(|value| value.is_finite()) {
        inspection.metadata.insert("bounds".into(), json!(bounds));
    }
    Ok(())
}

fn inspect_shapefile(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    let reader = shapefile::ShapeReader::from_path(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot read Shapefile: {error}")))?;
    let header = reader.header();
    inspection.dimensions = reader
        .shape_count()
        .map(|count| vec![count as u64])
        .unwrap_or_default();
    inspection.metadata.insert(
        "bounds".into(),
        json!([
            header.bbox.min.x,
            header.bbox.min.y,
            header.bbox.max.x,
            header.bbox.max.y
        ]),
    );
    inspection.metadata.insert(
        "shape_type".into(),
        json!(format!("{:?}", header.shape_type)),
    );
    inspection.metadata.insert(
        "companion_files".into(),
        json!([".shp", ".shx", ".dbf", ".prj", ".cpg"]),
    );
    let projection_path = path.with_extension("prj");
    if projection_path.is_file() {
        let wkt = std::fs::read_to_string(projection_path)?;
        inspection
            .metadata
            .insert("crs_wkt".into(), json!(wkt.trim()));
    }
    Ok(())
}

fn inspect_flatgeobuf(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    let input = std::io::BufReader::new(File::open(path)?);
    let reader = flatgeobuf::FgbReader::open(input)
        .map_err(|error| WildDatumError::Invalid(format!("cannot read FlatGeobuf: {error}")))?;
    let header = reader.header();
    inspection.dimensions = vec![header.features_count()];
    inspection.fields = header
        .columns()
        .map(|columns| {
            columns
                .iter()
                .map(|column| column.name().to_owned())
                .collect()
        })
        .unwrap_or_default();
    inspection
        .metadata
        .insert("feature_count".into(), json!(header.features_count()));
    if let Some(envelope) = header.envelope() {
        inspection
            .metadata
            .insert("bounds".into(), json!(envelope.iter().collect::<Vec<_>>()));
    }
    inspection
        .metadata
        .insert("spatial_index".into(), json!(header.index_node_size() > 0));
    if let Some(crs) = header.crs() {
        inspection.metadata.insert(
            "crs".into(),
            json!({
                "authority": crs.org(),
                "code": crs.code(),
                "code_string": crs.code_string(),
                "name": crs.name(),
                "wkt": crs.wkt()
            }),
        );
        if let Some(authority) = crs.org() {
            let code = crs
                .code_string()
                .map(str::to_owned)
                .unwrap_or_else(|| crs.code().to_string());
            inspection.crs = Some(format!("{authority}:{code}"));
        }
    }
    Ok(())
}

fn collect_geojson_value_bounds(value: &geojson::Value, bounds: &mut [f64; 4]) {
    fn visit(coordinates: &serde_json::Value, bounds: &mut [f64; 4]) {
        let Some(values) = coordinates.as_array() else {
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
            visit(child, bounds);
        }
    }

    if let Ok(value) = serde_json::to_value(value)
        && let Some(coordinates) = value.get("coordinates")
    {
        visit(coordinates, bounds);
    }
}

fn inspect_lidar(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    let reader = las::Reader::from_path(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot read LAS/LAZ header: {error}")))?;
    let header = reader.header();
    let bounds = header.bounds();
    inspection.dimensions = vec![header.number_of_points()];
    inspection
        .metadata
        .insert("point_count".into(), json!(header.number_of_points()));
    inspection.metadata.insert(
        "bounds".into(),
        json!({
            "min": [bounds.min.x, bounds.min.y, bounds.min.z],
            "max": [bounds.max.x, bounds.max.y, bounds.max.z]
        }),
    );
    let transforms = header.transforms();
    inspection.metadata.insert(
        "coordinate_scale".into(),
        json!([transforms.x.scale, transforms.y.scale, transforms.z.scale]),
    );
    inspection.metadata.insert(
        "coordinate_offset".into(),
        json!([
            transforms.x.offset,
            transforms.y.offset,
            transforms.z.offset
        ]),
    );
    if let Some((crs, source, wkt)) = las_spatial_reference(header) {
        inspection.crs = Some(crs);
        inspection
            .metadata
            .insert("crs_source".into(), json!(source));
        if let Some(wkt) = wkt {
            inspection.metadata.insert("crs_wkt".into(), json!(wkt));
        }
    }
    Ok(())
}

fn las_spatial_reference(header: &las::Header) -> Option<(String, &'static str, Option<String>)> {
    let projection_vlrs = header
        .all_vlrs()
        .filter(|vlr| vlr.user_id.trim_end_matches('\0') == "LASF_Projection")
        .collect::<Vec<_>>();
    for vlr in &projection_vlrs {
        if matches!(vlr.record_id, 2111 | 2112) {
            let wkt = String::from_utf8_lossy(&vlr.data)
                .trim_matches(['\0', ' '])
                .to_owned();
            if let Some(epsg) = epsg_from_wkt(&wkt) {
                return Some((format!("EPSG:{epsg}"), "las_wkt_vlr", Some(wkt)));
            }
        }
    }
    for vlr in projection_vlrs {
        if vlr.record_id == 34735
            && let Some(epsg) = epsg_from_geokey_directory(&vlr.data)
        {
            return Some((format!("EPSG:{epsg}"), "las_geokey_directory", None));
        }
    }
    None
}

fn epsg_from_wkt(wkt: &str) -> Option<u32> {
    let upper = wkt.to_ascii_uppercase();
    let mut epsg = None;
    let mut offset = 0;
    while let Some(relative) = upper[offset..].find("EPSG") {
        let start = offset + relative + 4;
        let digits = upper[start..]
            .chars()
            .skip_while(|character| !character.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        if let Ok(value) = digits.parse::<u32>() {
            epsg = Some(value);
        }
        offset = start;
    }
    epsg
}

fn epsg_from_geokey_directory(data: &[u8]) -> Option<u16> {
    if data.len() < 8 || !data.len().is_multiple_of(2) {
        return None;
    }
    let words = data
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    if words[0] != 1 {
        return None;
    }
    let key_count = words[3] as usize;
    let keys = words.get(4..4_usize.checked_add(key_count.checked_mul(4)?)?)?;
    [3072_u16, 2048_u16].into_iter().find_map(|wanted| {
        keys.chunks_exact(4).find_map(|key| {
            (key[0] == wanted && key[1] == 0 && key[2] == 1 && key[3] != 32_767).then_some(key[3])
        })
    })
}

fn inspect_hdf5(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    let structure = inspect_hdf5_structure(path)?;
    inspection.dimensions = structure.dimensions;
    apply_hdf5_georeferencing(&structure.datasets, inspection);
    inspection.fields = structure.dataset_paths;
    inspection
        .metadata
        .insert("hdf5_datasets".into(), structure.datasets);
    let descriptors = cube_descriptors_from_inventory(
        inspection.metadata["hdf5_datasets"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default(),
    );
    inspection
        .metadata
        .insert("cube_descriptors".into(), json!(descriptors));
    Ok(())
}

fn inspect_netcdf(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    let mut magic = [0_u8; 8];
    let read = File::open(path)?.read(&mut magic)?;
    if read == magic.len() && magic == *b"\x89HDF\r\n\x1a\n" {
        inspect_hdf5(path, inspection)?;
        inspection
            .metadata
            .insert("netcdf_container".into(), json!("netcdf4_hdf5"));
        return Ok(());
    }
    if read < 4 || &magic[..3] != b"CDF" {
        return Err(WildDatumError::Invalid(
            "the .nc source is neither NetCDF-3 nor NetCDF-4/HDF5".into(),
        ));
    }
    let reader = netcdf3::FileReader::open(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open NetCDF-3: {error}")))?;
    let mut inventory = Vec::new();
    let mut descriptors = Vec::new();
    for variable in reader
        .data_set()
        .get_vars()
        .into_iter()
        .take(MAX_HDF5_DATASETS)
    {
        let dimensions = variable.get_dims();
        let shape = dimensions
            .iter()
            .map(|dimension| dimension.size() as u64)
            .collect::<Vec<_>>();
        let names = variable.dim_names();
        let attributes = variable
            .get_attr_names()
            .into_iter()
            .map(|name| (name.clone(), netcdf3_attribute(variable, &name)))
            .collect::<BTreeMap<_, _>>();
        inventory.push(json!({
            "path": variable.name(),
            "shape": shape,
            "datatype": format!("{:?}", variable.data_type()),
            "dimensions": names,
            "attributes": attributes
        }));
        if shape.len() >= 2 {
            descriptors.push(CubeDescriptor {
                array_path: variable.name().to_owned(),
                data_type: format!("{:?}", variable.data_type()),
                axes: names
                    .iter()
                    .zip(&shape)
                    .map(|(name, length)| cube_axis(name.clone(), *length))
                    .collect(),
                chunk_shape: vec![],
                scale_factor: first_numeric_attribute(variable, "scale_factor"),
                add_offset: first_numeric_attribute(variable, "add_offset"),
                no_data: first_numeric_attribute(variable, "_FillValue")
                    .or_else(|| first_numeric_attribute(variable, "missing_value")),
                attributes,
            });
        }
    }
    descriptors.sort_by(|left, right| left.array_path.cmp(&right.array_path));
    inventory.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    inspection.dimensions = descriptors
        .iter()
        .max_by_key(|descriptor| {
            descriptor
                .axes
                .iter()
                .map(|axis| axis.length)
                .product::<u64>()
        })
        .map(|descriptor| descriptor.axes.iter().map(|axis| axis.length).collect())
        .unwrap_or_default();
    inspection.fields = reader.data_set().get_var_names();
    inspection
        .metadata
        .insert("netcdf_container".into(), json!("netcdf3"));
    inspection
        .metadata
        .insert("netcdf_variables".into(), json!(inventory));
    inspection
        .metadata
        .insert("cube_descriptors".into(), json!(descriptors));
    Ok(())
}

fn netcdf3_attribute(variable: &netcdf3::Variable, name: &str) -> serde_json::Value {
    if let Some(value) = variable.get_attr_as_string(name) {
        return json!(value);
    }
    macro_rules! numeric_attribute {
        ($method:ident) => {
            if let Some(values) = variable.$method(name) {
                return json!(values);
            }
        };
    }
    numeric_attribute!(get_attr_i8);
    numeric_attribute!(get_attr_u8);
    numeric_attribute!(get_attr_i16);
    numeric_attribute!(get_attr_i32);
    numeric_attribute!(get_attr_f32);
    numeric_attribute!(get_attr_f64);
    serde_json::Value::Null
}

fn first_numeric_attribute(variable: &netcdf3::Variable, name: &str) -> Option<f64> {
    variable
        .get_attr_i8(name)
        .and_then(|values| values.first())
        .map(|value| *value as f64)
        .or_else(|| {
            variable
                .get_attr_u8(name)
                .and_then(|values| values.first())
                .map(|value| *value as f64)
        })
        .or_else(|| {
            variable
                .get_attr_i16(name)
                .and_then(|values| values.first())
                .map(|value| *value as f64)
        })
        .or_else(|| {
            variable
                .get_attr_i32(name)
                .and_then(|values| values.first())
                .map(|value| *value as f64)
        })
        .or_else(|| {
            variable
                .get_attr_f32(name)
                .and_then(|values| values.first())
                .map(|value| *value as f64)
        })
        .or_else(|| {
            variable
                .get_attr_f64(name)
                .and_then(|values| values.first())
                .copied()
        })
}

fn inspect_zarr(path: &Path, inspection: &mut LocalAssetInspection) -> Result<()> {
    use std::sync::Arc;
    use zarrs::{array::Array, filesystem::FilesystemStore, group::Group, node::NodeMetadata};

    let store =
        Arc::new(FilesystemStore::new(path).map_err(|error| {
            WildDatumError::Invalid(format!("cannot open Zarr store: {error}"))
        })?);
    let mut arrays = Vec::new();
    if let Ok(array) = Array::open(store.clone(), "/") {
        arrays.push(array);
    } else {
        let group = Group::open(store.clone(), "/").map_err(|error| {
            WildDatumError::Invalid(format!("cannot open Zarr hierarchy: {error}"))
        })?;
        for (node_path, metadata) in group.traverse().map_err(|error| {
            WildDatumError::Invalid(format!("cannot inspect Zarr hierarchy: {error}"))
        })? {
            if let NodeMetadata::Array(metadata) = metadata {
                arrays.push(
                    Array::new_with_metadata(store.clone(), node_path.as_str(), metadata).map_err(
                        |error| WildDatumError::Invalid(format!("invalid Zarr array: {error}")),
                    )?,
                );
            }
        }
    }
    arrays.sort_by(|left, right| left.path().as_str().cmp(right.path().as_str()));
    let mut inventory = Vec::new();
    let mut descriptors = Vec::new();
    for array in arrays.into_iter().take(MAX_HDF5_DATASETS) {
        let shape = array.shape().to_vec();
        let names_value = serde_json::to_value(array.dimension_names()).unwrap_or_default();
        let names = names_value
            .as_array()
            .map(|names| {
                names
                    .iter()
                    .enumerate()
                    .map(|(axis, name)| {
                        name.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("axis_{axis}"))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                (0..shape.len())
                    .map(|axis| format!("axis_{axis}"))
                    .collect()
            });
        let chunk_shape = array
            .chunk_shape(&vec![0; shape.len()])
            .map(|shape| shape.iter().map(|value| value.get()).collect::<Vec<_>>())
            .unwrap_or_default();
        let attributes = array
            .attributes()
            .clone()
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        inventory.push(json!({
            "path": array.path().as_str(),
            "shape": shape,
            "datatype": format!("{:?}", array.data_type()),
            "dimensions": names,
            "chunk": chunk_shape,
            "attributes": attributes
        }));
        if shape.len() >= 2 {
            descriptors.push(CubeDescriptor {
                array_path: array.path().as_str().to_owned(),
                data_type: format!("{:?}", array.data_type()),
                axes: names
                    .iter()
                    .zip(&shape)
                    .map(|(name, length)| cube_axis(name.clone(), *length))
                    .collect(),
                chunk_shape,
                scale_factor: json_first_f64(attributes.get("scale_factor")),
                add_offset: json_first_f64(attributes.get("add_offset")),
                no_data: json_first_f64(attributes.get("_FillValue"))
                    .or_else(|| json_first_f64(attributes.get("missing_value"))),
                attributes,
            });
        }
    }
    inspection.dimensions = descriptors
        .iter()
        .max_by_key(|descriptor| {
            descriptor
                .axes
                .iter()
                .map(|axis| axis.length)
                .product::<u64>()
        })
        .map(|descriptor| descriptor.axes.iter().map(|axis| axis.length).collect())
        .unwrap_or_default();
    inspection.fields = descriptors
        .iter()
        .map(|descriptor| descriptor.array_path.clone())
        .collect();
    inspection
        .metadata
        .insert("zarr_arrays".into(), json!(inventory));
    inspection
        .metadata
        .insert("cube_descriptors".into(), json!(descriptors));
    Ok(())
}

fn cube_descriptors_from_inventory(inventory: &[serde_json::Value]) -> Vec<CubeDescriptor> {
    let mut descriptors = inventory
        .iter()
        .filter_map(|entry| {
            let shape = entry["shape"]
                .as_array()?
                .iter()
                .map(serde_json::Value::as_u64)
                .collect::<Option<Vec<_>>>()?;
            let attributes = entry
                .get("attribute_values")
                .and_then(serde_json::Value::as_object)
                .map(|attributes| {
                    attributes
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            (shape.len() >= 2).then(|| CubeDescriptor {
                array_path: entry["path"].as_str().unwrap_or_default().to_owned(),
                data_type: entry["datatype"].as_str().unwrap_or("unknown").to_owned(),
                axes: shape
                    .iter()
                    .enumerate()
                    .map(|(axis, length)| cube_axis(format!("axis_{axis}"), *length))
                    .collect(),
                chunk_shape: entry["chunk"]
                    .as_array()
                    .map(|shape| shape.iter().filter_map(serde_json::Value::as_u64).collect())
                    .unwrap_or_default(),
                scale_factor: hdf5_scale_factor(&attributes),
                add_offset: hdf5_attribute_f64(&attributes, &["add_offset"]),
                no_data: hdf5_attribute_f64(
                    &attributes,
                    &["data_ignore_value", "_fillvalue", "missing_value"],
                ),
                attributes,
            })
        })
        .collect::<Vec<_>>();
    infer_hdf5_cube_conventions(inventory, &mut descriptors);
    descriptors
}

/// Infer only conventions with an unambiguous coordinate match and a named
/// scientific array convention. Generic rank-3 arrays remain unmapped.
fn infer_hdf5_cube_conventions(
    inventory: &[serde_json::Value],
    descriptors: &mut [CubeDescriptor],
) {
    let wavelength_coordinates = inventory
        .iter()
        .filter_map(|entry| {
            let path = entry["path"].as_str()?;
            let shape = entry["shape"].as_array()?;
            (path.to_ascii_lowercase().contains("wavelength") && shape.len() == 1).then(|| {
                (
                    path.to_owned(),
                    shape[0].as_u64().unwrap_or_default(),
                    entry.get("coordinate_values").cloned(),
                )
            })
        })
        .collect::<Vec<_>>();
    for descriptor in descriptors {
        let path = descriptor.array_path.to_ascii_lowercase();
        if descriptor.axes.len() != 3
            || !(path.contains("reflectance_data") || path.ends_with("/reflectance"))
        {
            continue;
        }
        let candidates = wavelength_coordinates
            .iter()
            .filter_map(|(coordinate_path, length, values)| {
                let matching_axes = descriptor
                    .axes
                    .iter()
                    .enumerate()
                    .filter_map(|(axis, descriptor_axis)| {
                        (descriptor_axis.length == *length).then_some(axis)
                    })
                    .collect::<Vec<_>>();
                (matching_axes.len() == 1)
                    .then(|| (matching_axes[0], coordinate_path.clone(), values.clone()))
            })
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            continue;
        }
        let (spectral_axis, coordinate_path, coordinate_values) = &candidates[0];
        descriptor.axes[*spectral_axis].name = "wavelength".into();
        descriptor.axes[*spectral_axis].role = AxisRole::Spectral;
        descriptor.axes[*spectral_axis].coordinate_path = Some(coordinate_path.clone());
        descriptor.axes[*spectral_axis].unit = coordinate_values
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .filter(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_f64)
                    .all(|value| (100.0..=100_000.0).contains(&value))
            })
            .map(|_| "nm".into());
        let spatial_axes = (0..3)
            .filter(|axis| axis != spectral_axis)
            .collect::<Vec<_>>();
        descriptor.axes[spatial_axes[0]].name = "y".into();
        descriptor.axes[spatial_axes[0]].role = AxisRole::Y;
        descriptor.axes[spatial_axes[1]].name = "x".into();
        descriptor.axes[spatial_axes[1]].role = AxisRole::X;
    }
}

fn cube_axis(name: String, length: u64) -> CubeAxis {
    let lower = name.to_ascii_lowercase();
    let role = if matches!(lower.as_str(), "x" | "lon" | "longitude" | "easting") {
        AxisRole::X
    } else if matches!(lower.as_str(), "y" | "lat" | "latitude" | "northing") {
        AxisRole::Y
    } else if lower == "z"
        || lower.contains("height")
        || lower.contains("elevation")
        || lower.contains("depth")
    {
        AxisRole::Z
    } else if lower.contains("time") || lower.contains("date") {
        AxisRole::Time
    } else if lower.contains("wave") || lower.contains("spectral") || lower == "band" {
        AxisRole::Spectral
    } else if lower.contains("channel") {
        AxisRole::Channel
    } else {
        AxisRole::Other
    };
    CubeAxis {
        name,
        role,
        length,
        unit: None,
        coordinate_path: None,
        regular_start: None,
        regular_step: None,
    }
}

fn json_first_f64(value: Option<&serde_json::Value>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_array()?.first()?.as_f64())
    })
}

pub fn inspect_hdf5_structure(path: &Path) -> Result<Hdf5Structure> {
    let file = hdf5_metno::File::open(path)
        .map_err(|error| WildDatumError::Invalid(format!("cannot open HDF5: {error}")))?;
    let root = file
        .group("/")
        .map_err(|error| WildDatumError::Invalid(format!("cannot read HDF5 root: {error}")))?;
    let mut datasets = Vec::new();
    collect_hdf5_datasets(&root, &mut datasets)?;
    datasets.sort_by(|left, right| {
        left.get("path")
            .and_then(serde_json::Value::as_str)
            .cmp(&right.get("path").and_then(serde_json::Value::as_str))
    });
    let dimensions = datasets
        .iter()
        .filter_map(|dataset| dataset.get("shape"))
        .filter_map(serde_json::Value::as_array)
        .max_by_key(|shape| {
            shape
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .product::<u64>()
        })
        .map(|shape| shape.iter().filter_map(serde_json::Value::as_u64).collect())
        .unwrap_or_default();
    let dataset_paths = datasets
        .iter()
        .filter_map(|dataset| dataset.get("path").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    Ok(Hdf5Structure {
        dimensions,
        dataset_paths,
        datasets: json!(datasets),
    })
}

fn collect_hdf5_datasets(
    group: &hdf5_metno::Group,
    output: &mut Vec<serde_json::Value>,
) -> Result<()> {
    if output.len() >= MAX_HDF5_DATASETS {
        return Ok(());
    }
    for dataset in group
        .datasets()
        .map_err(|error| WildDatumError::Invalid(format!("cannot list HDF5 datasets: {error}")))?
    {
        let datatype = dataset
            .dtype()
            .and_then(|datatype| datatype.to_descriptor())
            .map(|descriptor| format!("{descriptor:?}"))
            .unwrap_or_else(|_| "unknown".into());
        let mut description = json!({
            "path": dataset.name(),
            "shape": dataset.shape(),
            "datatype": datatype,
            "chunk": dataset.chunk(),
            "attributes": dataset.attr_names().unwrap_or_default()
        });
        let attribute_values = hdf5_attribute_previews(&dataset);
        if !attribute_values.is_empty() {
            description["attribute_values"] = json!(attribute_values);
        }
        if let Some(value) = hdf5_scalar_string(&dataset) {
            description["value"] = json!(value);
        }
        if let Some(values) = hdf5_coordinate_preview(&dataset) {
            description["coordinate_values"] = values;
        }
        output.push(description);
        if output.len() >= MAX_HDF5_DATASETS {
            break;
        }
    }
    for child in group
        .groups()
        .map_err(|error| WildDatumError::Invalid(format!("cannot list HDF5 groups: {error}")))?
    {
        collect_hdf5_datasets(&child, output)?;
    }
    Ok(())
}

fn hdf5_coordinate_preview(dataset: &hdf5_metno::Dataset) -> Option<serde_json::Value> {
    let shape = dataset.shape();
    let name = dataset.name().to_ascii_lowercase();
    if shape.len() != 1
        || shape[0] > MAX_COORDINATE_VALUES
        || !(name.contains("wavelength") || name.contains("coordinate"))
    {
        return None;
    }
    let datatype = dataset.dtype().ok()?;
    macro_rules! read_values {
        ($type:ty) => {
            dataset.read_raw::<$type>().ok().map(|values| json!(values))
        };
    }
    if datatype.is::<u16>() {
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
        None
    }
}

fn hdf5_scalar_string(dataset: &hdf5_metno::Dataset) -> Option<String> {
    use hdf5_metno::types::{FixedAscii, TypeDescriptor, VarLenAscii, VarLenUnicode};

    let name = dataset.name().to_ascii_lowercase();
    if dataset.size() != 1
        || ![
            "coordinate_system_string",
            "epsg code",
            "map_info",
            "proj4",
            "spatial_ref",
            "crs_wkt",
        ]
        .iter()
        .any(|candidate| name.ends_with(candidate))
    {
        return None;
    }
    let datatype = dataset.dtype().ok()?;
    let descriptor = datatype.to_descriptor().ok()?;
    let value = match descriptor {
        TypeDescriptor::VarLenAscii => dataset
            .read_raw::<VarLenAscii>()
            .ok()?
            .first()?
            .as_str()
            .to_owned(),
        TypeDescriptor::VarLenUnicode => dataset
            .read_raw::<VarLenUnicode>()
            .ok()?
            .first()?
            .as_str()
            .to_owned(),
        TypeDescriptor::FixedAscii(length) => {
            macro_rules! read_fixed {
                ($length:literal) => {
                    dataset
                        .read_raw::<FixedAscii<$length>>()
                        .ok()?
                        .first()?
                        .as_str()
                        .to_owned()
                };
            }
            match length {
                1 => read_fixed!(1),
                6 => read_fixed!(6),
                16 => read_fixed!(16),
                32 => read_fixed!(32),
                63 => read_fixed!(63),
                64 => read_fixed!(64),
                128 => read_fixed!(128),
                256 => read_fixed!(256),
                395 => read_fixed!(395),
                512 => read_fixed!(512),
                1_024 => read_fixed!(1024),
                _ => return None,
            }
        }
        _ => return None,
    };
    let value = value.trim_matches(['\0', ' ']).to_owned();
    (!value.is_empty() && value.len() <= 4_096).then_some(value)
}

fn hdf5_attribute_previews(dataset: &hdf5_metno::Dataset) -> BTreeMap<String, serde_json::Value> {
    let mut values = BTreeMap::new();
    for name in dataset
        .attr_names()
        .unwrap_or_default()
        .into_iter()
        .take(64)
    {
        let lower = name.to_ascii_lowercase();
        if ![
            "scale_factor",
            "add_offset",
            "data_ignore_value",
            "_fillvalue",
            "missing_value",
            "spatial_extent_meters",
            "spatial_resolution_x_y",
            "units_valid_range",
            "valid_range",
            "dimensions",
            "dim",
        ]
        .contains(&lower.as_str())
        {
            continue;
        }
        let Ok(attribute) = dataset.attr(&name) else {
            continue;
        };
        if attribute.size() > 64 {
            continue;
        }
        let Ok(datatype) = attribute.dtype() else {
            continue;
        };
        macro_rules! numeric_attribute {
            ($type:ty) => {{
                attribute.read_raw::<$type>().ok().map(|raw| {
                    if raw.len() == 1 {
                        json!(raw[0])
                    } else {
                        json!(raw)
                    }
                })
            }};
        }
        let value = if datatype.is::<u8>() {
            numeric_attribute!(u8)
        } else if datatype.is::<u16>() {
            numeric_attribute!(u16)
        } else if datatype.is::<u32>() {
            numeric_attribute!(u32)
        } else if datatype.is::<u64>() {
            numeric_attribute!(u64)
        } else if datatype.is::<i8>() {
            numeric_attribute!(i8)
        } else if datatype.is::<i16>() {
            numeric_attribute!(i16)
        } else if datatype.is::<i32>() {
            numeric_attribute!(i32)
        } else if datatype.is::<i64>() {
            numeric_attribute!(i64)
        } else if datatype.is::<f32>() {
            numeric_attribute!(f32)
        } else if datatype.is::<f64>() {
            numeric_attribute!(f64)
        } else {
            None
        };
        if let Some(value) = value {
            values.insert(name, value);
        }
    }
    values
}

fn hdf5_attribute_f64(
    attributes: &BTreeMap<String, serde_json::Value>,
    names: &[&str],
) -> Option<f64> {
    attributes.iter().find_map(|(name, value)| {
        names
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
            .then(|| json_first_f64(Some(value)))
            .flatten()
    })
}

fn hdf5_scale_factor(attributes: &BTreeMap<String, serde_json::Value>) -> Option<f64> {
    if let Some(divisor) = attributes.get("Scale_Factor").and_then(|value| {
        json_first_f64(Some(value)).filter(|value| value.is_finite() && *value != 0.0)
    }) {
        // NEON imaging-spectrometer HDF5 stores a divisor named
        // `Scale_Factor`; CubeDescriptor stores the multiplicative operation.
        return Some(1.0 / divisor);
    }
    hdf5_attribute_f64(attributes, &["scale_factor"])
}

fn apply_hdf5_georeferencing(inventory: &serde_json::Value, inspection: &mut LocalAssetInspection) {
    let Some(entries) = inventory.as_array() else {
        return;
    };
    let epsg = entries.iter().find_map(|entry| {
        let path = entry.get("path")?.as_str()?.to_ascii_lowercase();
        let value = entry.get("value")?.as_str()?;
        path.ends_with("/coordinate_system/epsg code")
            .then(|| value.trim().parse::<u32>().ok())
            .flatten()
    });
    if let Some(epsg) = epsg {
        inspection.crs = Some(format!("EPSG:{epsg}"));
        inspection
            .metadata
            .insert("crs_source".into(), json!("hdf5_epsg_code"));
    }
    if let Some(wkt) = entries.iter().find_map(|entry| {
        let path = entry.get("path")?.as_str()?.to_ascii_lowercase();
        path.ends_with("/coordinate_system/coordinate_system_string")
            .then(|| entry.get("value")?.as_str())
            .flatten()
    }) {
        inspection.metadata.insert("crs_wkt".into(), json!(wkt));
    }
    let map_info = entries.iter().find_map(|entry| {
        let path = entry.get("path")?.as_str()?.to_ascii_lowercase();
        path.ends_with("/coordinate_system/map_info")
            .then(|| entry.get("value")?.as_str())
            .flatten()
    });
    let Some((affine, map_fields)) = map_info.and_then(parse_envi_map_info) else {
        return;
    };
    inspection
        .metadata
        .insert("map_info".into(), json!(map_info));
    let height = inspection.dimensions.first().copied().unwrap_or_default() as f64;
    let width = inspection.dimensions.get(1).copied().unwrap_or_default() as f64;
    let computed_bounds = [
        affine[0],
        affine[3] + height * affine[5],
        affine[0] + width * affine[1],
        affine[3],
    ];
    if let Some(spatial_extent) = hdf5_reflectance_spatial_extent(entries) {
        inspection
            .metadata
            .insert("spatial_extent_meters".into(), json!(spatial_extent));
        let tolerance = affine[1].abs().max(affine[5].abs()) * 0.01;
        if computed_bounds
            .iter()
            .zip(spatial_extent)
            .any(|(computed, reported)| (computed - reported).abs() > tolerance)
        {
            inspection
                .metadata
                .insert("map_info_affine_candidate".into(), json!(affine));
            inspection.warnings.push(
                "HDF5 Map_Info and Reflectance_Data Spatial_Extent_meters disagree; world-to-pixel linking is disabled"
                    .into(),
            );
            return;
        }
    }
    inspection
        .metadata
        .insert("affine_transform".into(), json!(affine));
    inspection.metadata.insert(
        "world_to_pixel".into(),
        json!({
            "kind": "north_up_affine_v1",
            "pixel_reference": "upper_left_corner",
            "x_axis": 1,
            "y_axis": 0,
            "x_rounding": "floor",
            "y_rounding": "floor",
            "source": "hdf5_map_info",
            "reference_pixel": map_fields.reference_pixel,
        }),
    );
    if height > 0.0 && width > 0.0 {
        inspection
            .metadata
            .insert("geo_bounds".into(), json!(computed_bounds));
    }
}

fn hdf5_reflectance_spatial_extent(entries: &[serde_json::Value]) -> Option<[f64; 4]> {
    entries.iter().find_map(|entry| {
        let path = entry.get("path")?.as_str()?.to_ascii_lowercase();
        if !(path.contains("reflectance") && path.ends_with("reflectance_data")) {
            return None;
        }
        let values = entry
            .get("attribute_values")?
            .get("Spatial_Extent_meters")?
            .as_array()?;
        if values.len() != 4 {
            return None;
        }
        let source = values
            .iter()
            .map(serde_json::Value::as_f64)
            .collect::<Option<Vec<_>>>()?;
        // NEON stores [x_min, x_max, y_min, y_max]. WildDatum bounds use
        // [x_min, y_min, x_max, y_max].
        Some([source[0], source[2], source[1], source[3]])
    })
}

struct EnviMapFields {
    reference_pixel: [f64; 2],
}

fn parse_envi_map_info(value: &str) -> Option<([f64; 6], EnviMapFields)> {
    let parts = value.split(',').map(str::trim).collect::<Vec<_>>();
    if parts.len() < 7 || !parts[0].eq_ignore_ascii_case("UTM") {
        return None;
    }
    if parts.iter().skip(7).any(|part| {
        part.strip_prefix("rotation=")
            .or_else(|| part.strip_prefix("Rotation="))
            .and_then(|value| value.trim().parse::<f64>().ok())
            .is_some_and(|rotation| rotation.abs() > f64::EPSILON)
    }) {
        return None;
    }
    let reference_x = parts[1].parse::<f64>().ok()?;
    let reference_y = parts[2].parse::<f64>().ok()?;
    let map_x = parts[3].parse::<f64>().ok()?;
    let map_y = parts[4].parse::<f64>().ok()?;
    let pixel_width = parts[5].parse::<f64>().ok()?;
    let pixel_height = parts[6].parse::<f64>().ok()?;
    if ![
        reference_x,
        reference_y,
        map_x,
        map_y,
        pixel_width,
        pixel_height,
    ]
    .into_iter()
    .all(f64::is_finite)
        || pixel_width <= 0.0
        || pixel_height <= 0.0
    {
        return None;
    }
    let origin_x = map_x - (reference_x - 1.0) * pixel_width;
    let origin_y = map_y + (reference_y - 1.0) * pixel_height;
    Some((
        [origin_x, pixel_width, 0.0, origin_y, 0.0, -pixel_height],
        EnviMapFields {
            reference_pixel: [reference_x, reference_y],
        },
    ))
}

fn inspect_delimited(
    path: &Path,
    tab_separated: bool,
    inspection: &mut LocalAssetInspection,
) -> Result<()> {
    let mut builder = csv::ReaderBuilder::new();
    builder.delimiter(if tab_separated { b'\t' } else { b',' });
    let mut reader = builder
        .from_path(path)
        .map_err(|error| WildDatumError::Invalid(format!("invalid delimited file: {error}")))?;
    inspection.fields = reader
        .headers()
        .map_err(|error| WildDatumError::Invalid(format!("cannot read table header: {error}")))?
        .iter()
        .map(str::to_owned)
        .collect();

    let field_names = inspection
        .fields
        .iter()
        .map(|field| field.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if field_names
        .iter()
        .any(|field| field.contains("time") || field.contains("date"))
    {
        inspection.modalities.push(Modality::TimeSeries);
    }
    inspection.requires_mapping = inspection.fields.is_empty();
    Ok(())
}

pub fn modalities_for_extension(extension: &str) -> Vec<Modality> {
    match extension {
        "csv" | "tsv" | "parquet" | "geoparquet" | "arrow" | "ipc" => {
            vec![Modality::Tabular]
        }
        "tif" | "tiff" | "cog" => vec![Modality::Raster],
        "h5" | "hdf5" | "nc" | "nc4" | "zarr" => {
            vec![Modality::Hyperspectral, Modality::Tensor]
        }
        "las" | "laz" | "copc" | "ply" => vec![Modality::PointCloud],
        "geojson" | "json" | "fgb" | "gpkg" | "shp" => vec![Modality::Vector],
        "png" | "jpg" | "jpeg" | "webp" => vec![Modality::Image],
        _ => vec![Modality::Unknown],
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[tokio::test]
    async fn inspects_csv_without_exposing_path() {
        let mut file = tempfile::Builder::new().suffix(".csv").tempfile().unwrap();
        writeln!(file, "timestamp,site,value").unwrap();
        writeln!(file, "2025-01-01T00:00:00Z,HARV,1.2").unwrap();
        let inspection = inspect_path(file.path()).await.unwrap();
        assert_eq!(inspection.fields, vec!["timestamp", "site", "value"]);
        assert!(inspection.modalities.contains(&Modality::TimeSeries));
        assert!(
            !serde_json::to_string(&inspection)
                .unwrap()
                .contains(file.path().to_str().unwrap())
        );
    }

    #[tokio::test]
    async fn fingerprints_zarr_directories_deterministically() {
        use std::sync::Arc;

        let directory = tempfile::tempdir().unwrap();
        let zarr = directory.path().join("cube.zarr");
        std::fs::create_dir(&zarr).unwrap();
        let store = Arc::new(zarrs::filesystem::FilesystemStore::new(&zarr).unwrap());
        let array = zarrs::array::ArrayBuilder::new(
            vec![2, 3, 4],
            vec![2, 3, 4],
            zarrs::array::data_type::uint8(),
            0_u8,
        )
        .dimension_names(["y", "x", "wavelength"].into())
        .build(store, "/")
        .unwrap();
        array.store_metadata().unwrap();
        array
            .store_chunk(&[0, 0, 0], (0_u8..24).collect::<Vec<_>>())
            .unwrap();
        let first = inspect_path(&zarr).await.unwrap();
        let second = inspect_path(&zarr).await.unwrap();
        assert_eq!(first.fingerprint.value, second.fingerprint.value);
        assert!(first.size_bytes > 24);
        assert!(first.modalities.contains(&Modality::Hyperspectral));
        assert_eq!(
            first.metadata["cube_descriptors"][0]["axes"][2]["role"],
            "spectral"
        );

        array
            .store_chunk(&[0, 0, 0], (1_u8..=24).collect::<Vec<_>>())
            .unwrap();
        let changed = inspect_path(&zarr).await.unwrap();
        assert_ne!(first.fingerprint.value, changed.fingerprint.value);
    }

    #[tokio::test]
    async fn discovers_netcdf3_dimensions_and_cf_roles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("temperature.nc");
        let mut definition = netcdf3::DataSet::new();
        definition.add_fixed_dim("latitude", 2).unwrap();
        definition.add_fixed_dim("longitude", 3).unwrap();
        definition
            .add_var_f32("temperature", &["latitude", "longitude"])
            .unwrap();
        let mut writer = netcdf3::FileWriter::create_new(&path).unwrap();
        writer
            .set_def(&definition, netcdf3::Version::Classic, 0)
            .unwrap();
        writer
            .write_var_f32("temperature", &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
            .unwrap();
        writer.close().unwrap();

        let inspection = inspect_path(&path).await.unwrap();
        assert_eq!(inspection.dimensions, vec![2, 3]);
        assert_eq!(inspection.metadata["netcdf_container"], "netcdf3");
        assert_eq!(
            inspection.metadata["cube_descriptors"][0]["axes"][0]["role"],
            "y"
        );
        assert_eq!(
            inspection.metadata["cube_descriptors"][0]["axes"][1]["role"],
            "x"
        );
    }

    #[tokio::test]
    async fn discovers_neon_hdf5_reflectance_axes_from_wavelength_coordinates() {
        use hdf5_metno::types::FixedAscii;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reflectance.h5");
        let file = hdf5_metno::File::create(&path).unwrap();
        let group = file.create_group("HARV/Reflectance").unwrap();
        let dataset = group
            .new_dataset::<u16>()
            .shape([2, 3, 4])
            .create("Reflectance_Data")
            .unwrap();
        dataset.write_raw(&(0_u16..24).collect::<Vec<_>>()).unwrap();
        dataset
            .new_attr::<f64>()
            .create("Scale_Factor")
            .unwrap()
            .write_scalar(&10_000.0)
            .unwrap();
        dataset
            .new_attr::<i16>()
            .create("Data_Ignore_Value")
            .unwrap()
            .write_scalar(&-9_999)
            .unwrap();
        dataset
            .new_attr::<f64>()
            .shape([4])
            .create("Spatial_Extent_meters")
            .unwrap()
            .write_raw(&[256000.0, 256003.0, 4111998.0, 4112000.0])
            .unwrap();
        let wavelengths = group
            .new_dataset::<f32>()
            .shape([4])
            .create("Wavelength")
            .unwrap();
        wavelengths
            .write_raw(&[450.0_f32, 550.0, 650.0, 850.0])
            .unwrap();
        let coordinate_system = group.create_group("Metadata/Coordinate_System").unwrap();
        coordinate_system
            .new_dataset::<FixedAscii<6>>()
            .shape([1])
            .create("EPSG Code")
            .unwrap()
            .write_raw(&[FixedAscii::<6>::from_ascii(b"32611").unwrap()])
            .unwrap();
        coordinate_system
            .new_dataset::<FixedAscii<128>>()
            .shape([1])
            .create("Map_Info")
            .unwrap()
            .write_raw(&[FixedAscii::<128>::from_ascii(
                b"UTM, 1.000, 1.000, 256000.000, 4112000.000, 1.000000e+00, 1.000000e+00, 11, North, WGS-84, units=Meters, 0",
            )
            .unwrap()])
            .unwrap();
        drop(file);

        let inspection = inspect_path(&path).await.unwrap();
        assert_eq!(inspection.dimensions, vec![2, 3, 4]);
        assert_eq!(inspection.crs.as_deref(), Some("EPSG:32611"));
        assert_eq!(
            inspection.metadata["affine_transform"],
            json!([256000.0, 1.0, 0.0, 4112000.0, 0.0, -1.0])
        );
        assert_eq!(
            inspection.metadata["world_to_pixel"]["pixel_reference"],
            "upper_left_corner"
        );
        assert!(inspection.requires_mapping);
        assert!(
            inspection
                .fields
                .iter()
                .any(|field| field.ends_with("Reflectance_Data"))
        );
        let datasets = inspection.metadata["hdf5_datasets"].as_array().unwrap();
        assert!(datasets.iter().any(|dataset| {
            dataset.get("coordinate_values") == Some(&json!([450.0_f32, 550.0, 650.0, 850.0]))
        }));
        let descriptor = &inspection.metadata["cube_descriptors"][0];
        assert_eq!(descriptor["axes"][0]["role"], "y");
        assert_eq!(descriptor["axes"][1]["role"], "x");
        assert_eq!(descriptor["axes"][2]["role"], "spectral");
        assert_eq!(descriptor["scale_factor"], 0.0001);
        assert_eq!(descriptor["no_data"], -9_999.0);
        assert_eq!(
            descriptor["axes"][2]["coordinate_path"],
            "/HARV/Reflectance/Wavelength"
        );

        let file = hdf5_metno::File::open_rw(&path).unwrap();
        file.dataset("/HARV/Reflectance/Reflectance_Data")
            .unwrap()
            .attr("Spatial_Extent_meters")
            .unwrap()
            .write_raw(&[256500.0, 256503.0, 4111998.0, 4112000.0])
            .unwrap();
        drop(file);
        let inconsistent = inspect_path(&path).await.unwrap();
        assert!(!inconsistent.metadata.contains_key("world_to_pixel"));
        assert!(!inconsistent.metadata.contains_key("affine_transform"));
        assert!(
            inconsistent
                .warnings
                .iter()
                .any(|warning| warning.contains("Spatial_Extent_meters disagree"))
        );
    }

    #[tokio::test]
    async fn discovers_las_epsg_from_the_geokey_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("points.las");
        let mut header = las::Builder::default();
        let words = [1_u16, 1, 0, 1, 3072, 0, 1, 32611];
        header.vlrs.push(las::Vlr {
            user_id: "LASF_Projection".into(),
            record_id: 34735,
            description: "GeoKeyDirectoryTag".into(),
            data: words.into_iter().flat_map(u16::to_le_bytes).collect(),
        });
        let mut writer = las::Writer::from_path(&path, header.into_header().unwrap()).unwrap();
        writer
            .write_point(las::Point {
                x: 1.0,
                y: 2.0,
                z: 3.0,
                ..Default::default()
            })
            .unwrap();
        writer.close().unwrap();

        let inspection = inspect_path(&path).await.unwrap();
        assert_eq!(inspection.crs.as_deref(), Some("EPSG:32611"));
        assert_eq!(inspection.metadata["crs_source"], "las_geokey_directory");
    }
}
