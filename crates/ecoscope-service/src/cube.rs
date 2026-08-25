//! Format-independent, bounded multidimensional array queries.

use std::{fs::File, io::Read, ops::Range, path::Path, sync::Arc};

use ecoscope_core::{CubeRange, EcoScopeError, Result};
use serde_json::{Value, json};

const MAX_CUBE_CELLS: u64 = 100_000;
const MAX_ZARR_BOUNDING_CELLS: u64 = 1_000_000;
const MAX_NETCDF3_SOURCE_CELLS: usize = 16_000_000;

pub(crate) fn query_cube_slice(
    path: &Path,
    original_name: &str,
    array_path: &str,
    ranges: &[CubeRange],
    requested_limit: u64,
) -> Result<(Value, Option<u64>, &'static str)> {
    let limit = requested_limit.clamp(1, MAX_CUBE_CELLS);
    if path.is_dir() {
        return query_zarr_slice(path, array_path, ranges, limit);
    }
    let mut magic = [0_u8; 8];
    let read = File::open(path)?.read(&mut magic)?;
    if read == magic.len() && magic == *b"\x89HDF\r\n\x1a\n" {
        return query_hdf5_slice(path, array_path, ranges, limit);
    }
    if read >= 4 && &magic[..3] == b"CDF" {
        return query_netcdf3_slice(path, array_path, ranges, limit);
    }
    Err(EcoScopeError::Invalid(format!(
        "cube_slice supports HDF5/NetCDF-4, NetCDF-3, and Zarr; cannot identify {original_name}"
    )))
}

fn validate_ranges(shape: &[u64], ranges: &[CubeRange], limit: u64) -> Result<Vec<u64>> {
    if ranges.len() != shape.len() {
        return Err(EcoScopeError::Invalid(format!(
            "cube slice requires one range per axis; source rank is {} but {} ranges were supplied",
            shape.len(),
            ranges.len()
        )));
    }
    let mut output_shape = Vec::with_capacity(shape.len());
    let mut cells = 1_u64;
    for (axis, (range, length)) in ranges.iter().zip(shape).enumerate() {
        if range.step == 0 || range.start >= range.end || range.end > *length {
            return Err(EcoScopeError::Invalid(format!(
                "invalid range on axis {axis}: [{}, {}) step {} for length {length}",
                range.start, range.end, range.step
            )));
        }
        let count = (range.end - range.start).div_ceil(range.step);
        cells = cells
            .checked_mul(count)
            .ok_or_else(|| EcoScopeError::Invalid("cube slice cell count overflows u64".into()))?;
        output_shape.push(count);
    }
    if cells > limit {
        return Err(EcoScopeError::Invalid(format!(
            "cube slice contains {cells} cells, above the requested bounded limit {limit}; narrow the ranges or increase cell_limit up to {MAX_CUBE_CELLS}"
        )));
    }
    Ok(output_shape)
}

fn query_hdf5_slice(
    path: &Path,
    array_path: &str,
    ranges: &[CubeRange],
    limit: u64,
) -> Result<(Value, Option<u64>, &'static str)> {
    use hdf5_metno::{Hyperslab, SliceOrIndex};

    let file = hdf5_metno::File::open(path)
        .map_err(|error| EcoScopeError::Invalid(format!("cannot open HDF5/NetCDF-4: {error}")))?;
    let dataset = file.dataset(array_path).map_err(|error| {
        EcoScopeError::Invalid(format!(
            "cannot open multidimensional array {array_path}: {error}"
        ))
    })?;
    let shape = dataset
        .shape()
        .into_iter()
        .map(|value| value as u64)
        .collect::<Vec<_>>();
    let output_shape = validate_ranges(&shape, ranges, limit)?;
    let hyperslab = Hyperslab::new(
        ranges
            .iter()
            .map(|range| SliceOrIndex::SliceTo {
                start: range.start as usize,
                step: range.step as usize,
                end: range.end as usize,
                block: 1,
            })
            .collect::<Vec<_>>(),
    );
    let datatype = dataset.dtype().map_err(|error| {
        EcoScopeError::Invalid(format!("cannot inspect HDF5 datatype: {error}"))
    })?;
    macro_rules! read_values {
        ($type:ty) => {
            dataset
                .read_slice::<$type, _, ndarray::IxDyn>(hyperslab.clone())
                .map(|array| array.iter().map(|value| *value as f64).collect::<Vec<_>>())
        };
    }
    let values = if datatype.is::<u8>() {
        read_values!(u8)
    } else if datatype.is::<u16>() {
        read_values!(u16)
    } else if datatype.is::<u32>() {
        read_values!(u32)
    } else if datatype.is::<u64>() {
        read_values!(u64)
    } else if datatype.is::<i8>() {
        read_values!(i8)
    } else if datatype.is::<i16>() {
        read_values!(i16)
    } else if datatype.is::<i32>() {
        read_values!(i32)
    } else if datatype.is::<i64>() {
        read_values!(i64)
    } else if datatype.is::<f32>() {
        read_values!(f32)
    } else if datatype.is::<f64>() {
        read_values!(f64)
    } else {
        return Err(EcoScopeError::Invalid(format!(
            "cube_slice does not support HDF5 datatype {datatype:?}"
        )));
    }
    .map_err(|error| EcoScopeError::Invalid(format!("cannot read HDF5 slice: {error}")))?;
    Ok(cube_payload(
        array_path,
        &shape,
        ranges,
        &output_shape,
        values,
        "hdf5_hyperslab",
    ))
}

fn query_zarr_slice(
    path: &Path,
    array_path: &str,
    ranges: &[CubeRange],
    limit: u64,
) -> Result<(Value, Option<u64>, &'static str)> {
    use zarrs::{
        array::{Array, data_type},
        filesystem::FilesystemStore,
    };

    let store = Arc::new(
        FilesystemStore::new(path)
            .map_err(|error| EcoScopeError::Invalid(format!("cannot open Zarr store: {error}")))?,
    );
    let array = Array::open(store, array_path).map_err(|error| {
        EcoScopeError::Invalid(format!("cannot open Zarr array {array_path}: {error}"))
    })?;
    let shape = array.shape().to_vec();
    let output_shape = validate_ranges(&shape, ranges, limit)?;
    let bounding_cells = ranges
        .iter()
        .try_fold(1_u64, |cells, range| {
            cells.checked_mul(range.end - range.start)
        })
        .ok_or_else(|| EcoScopeError::Invalid("Zarr bounding slice size overflows u64".into()))?;
    if bounding_cells > MAX_ZARR_BOUNDING_CELLS {
        return Err(EcoScopeError::Invalid(format!(
            "the requested strided Zarr bounding window contains {bounding_cells} cells, above the safe decode budget {MAX_ZARR_BOUNDING_CELLS}; narrow the ranges"
        )));
    }
    let contiguous = ranges
        .iter()
        .map(|range| Range {
            start: range.start,
            end: range.end,
        })
        .collect::<Vec<_>>();
    macro_rules! retrieve_values {
        ($type:ty) => {
            array
                .retrieve_array_subset::<Vec<$type>>(&contiguous)
                .map(|values| {
                    values
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>()
                })
        };
    }
    let values = if array.data_type() == &data_type::uint8() {
        retrieve_values!(u8)
    } else if array.data_type() == &data_type::uint16() {
        retrieve_values!(u16)
    } else if array.data_type() == &data_type::uint32() {
        retrieve_values!(u32)
    } else if array.data_type() == &data_type::uint64() {
        retrieve_values!(u64)
    } else if array.data_type() == &data_type::int8() {
        retrieve_values!(i8)
    } else if array.data_type() == &data_type::int16() {
        retrieve_values!(i16)
    } else if array.data_type() == &data_type::int32() {
        retrieve_values!(i32)
    } else if array.data_type() == &data_type::int64() {
        retrieve_values!(i64)
    } else if array.data_type() == &data_type::float32() {
        retrieve_values!(f32)
    } else if array.data_type() == &data_type::float64() {
        retrieve_values!(f64)
    } else {
        return Err(EcoScopeError::Invalid(format!(
            "cube_slice does not support Zarr datatype {:?}",
            array.data_type()
        )));
    }
    .map_err(|error| EcoScopeError::Invalid(format!("cannot decode Zarr slice: {error}")))?;
    let bounding_shape = ranges
        .iter()
        .map(|range| range.end - range.start)
        .collect::<Vec<_>>();
    let sampled = if ranges.iter().all(|range| range.step == 1) {
        values
    } else {
        sample_bounding_values(values, &bounding_shape, ranges)
    };
    Ok(cube_payload(
        array_path,
        &shape,
        ranges,
        &output_shape,
        sampled,
        "zarr_chunk_subset",
    ))
}

fn query_netcdf3_slice(
    path: &Path,
    array_path: &str,
    ranges: &[CubeRange],
    limit: u64,
) -> Result<(Value, Option<u64>, &'static str)> {
    let mut reader = netcdf3::FileReader::open(path)
        .map_err(|error| EcoScopeError::Invalid(format!("cannot open NetCDF-3: {error}")))?;
    let variable = reader
        .data_set()
        .get_var(array_path)
        .ok_or_else(|| EcoScopeError::NotFound(format!("NetCDF-3 variable {array_path}")))?;
    let shape = variable
        .get_dims()
        .iter()
        .map(|dimension| dimension.size() as u64)
        .collect::<Vec<_>>();
    let source_cells = variable.len();
    if source_cells > MAX_NETCDF3_SOURCE_CELLS {
        return Err(EcoScopeError::Invalid(format!(
            "NetCDF-3 variable {array_path} has {source_cells} cells; the pure-Rust reader cannot perform subset I/O yet, so EcoScope limits full-variable reads to {MAX_NETCDF3_SOURCE_CELLS} cells"
        )));
    }
    let output_shape = validate_ranges(&shape, ranges, limit)?;
    let data = reader.read_var(array_path).map_err(|error| {
        EcoScopeError::Invalid(format!("cannot read NetCDF-3 variable: {error}"))
    })?;
    let values = match data {
        netcdf3::DataVector::I8(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::U8(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::I16(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::I32(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::F32(values) => values.into_iter().map(|value| value as f64).collect(),
        netcdf3::DataVector::F64(values) => values,
    };
    let sampled = sample_source_values(values, &shape, ranges);
    Ok(cube_payload(
        array_path,
        &shape,
        ranges,
        &output_shape,
        sampled,
        "netcdf3_bounded_variable",
    ))
}

fn sample_bounding_values(
    values: Vec<f64>,
    bounding_shape: &[u64],
    ranges: &[CubeRange],
) -> Vec<f64> {
    let output_shape = ranges
        .iter()
        .map(|range| (range.end - range.start).div_ceil(range.step))
        .collect::<Vec<_>>();
    (0..output_shape.iter().product::<u64>())
        .map(|flat| {
            let output_indices = unravel_index(flat, &output_shape);
            let bounding_indices = output_indices
                .iter()
                .zip(ranges)
                .map(|(index, range)| index * range.step)
                .collect::<Vec<_>>();
            values[ravel_index(&bounding_indices, bounding_shape)]
        })
        .collect()
}

fn sample_source_values(values: Vec<f64>, source_shape: &[u64], ranges: &[CubeRange]) -> Vec<f64> {
    let output_shape = ranges
        .iter()
        .map(|range| (range.end - range.start).div_ceil(range.step))
        .collect::<Vec<_>>();
    (0..output_shape.iter().product::<u64>())
        .map(|flat| {
            let output_indices = unravel_index(flat, &output_shape);
            let source_indices = output_indices
                .iter()
                .zip(ranges)
                .map(|(index, range)| range.start + index * range.step)
                .collect::<Vec<_>>();
            values[ravel_index(&source_indices, source_shape)]
        })
        .collect()
}

fn cube_payload(
    array_path: &str,
    source_shape: &[u64],
    ranges: &[CubeRange],
    output_shape: &[u64],
    values: Vec<f64>,
    engine: &str,
) -> (Value, Option<u64>, &'static str) {
    let rows = values
        .into_iter()
        .enumerate()
        .map(|(flat, value)| {
            let output_indices = unravel_index(flat as u64, output_shape);
            let source_indices = output_indices
                .iter()
                .zip(ranges)
                .map(|(index, range)| range.start + index * range.step)
                .collect::<Vec<_>>();
            json!({"indices": source_indices, "value": value})
        })
        .collect::<Vec<_>>();
    let count = rows.len() as u64;
    (
        json!({
            "array_path": array_path,
            "source_shape": source_shape,
            "output_shape": output_shape,
            "ranges": ranges,
            "columns": ["indices", "value"],
            "rows": rows,
            "returned_rows": count,
            "engine": engine,
            "order": "c_row_major"
        }),
        Some(count),
        "application/json",
    )
}

fn unravel_index(mut flat: u64, shape: &[u64]) -> Vec<u64> {
    let mut indices = vec![0; shape.len()];
    for axis in (0..shape.len()).rev() {
        indices[axis] = flat % shape[axis];
        flat /= shape[axis];
    }
    indices
}

fn ravel_index(indices: &[u64], shape: &[u64]) -> usize {
    indices
        .iter()
        .zip(shape)
        .fold(0_u64, |flat, (index, length)| flat * length + index) as usize
}
