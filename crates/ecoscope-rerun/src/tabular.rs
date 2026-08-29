use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use ecoscope_core::{EcoScopeError, PROFILE_TRAJECTORY_VIEW_KIND, Result};
use serde_json::{Value, json};

use crate::rerun_error;

const MAX_PROFILE_TRAJECTORY_ROWS: usize = 100_000;

#[derive(Debug, Clone)]
struct ProfileTrajectoryRecipe {
    trajectory_id_field: String,
    profile_id_field: String,
    latitude_field: String,
    longitude_field: String,
    vertical_field: String,
    vertical_positive_down: bool,
    vertical_unit: Option<String>,
    value_field: String,
    value_unit: Option<String>,
    qc_field: Option<String>,
    accepted_qc: BTreeSet<String>,
}

#[derive(Debug)]
struct ProfileTrajectoryData {
    source_rows: usize,
    map_positions: Vec<[f64; 2]>,
    map_colors: Vec<rerun::Color>,
    map_radii: Vec<rerun::Radius>,
    profile_positions: Vec<[f32; 2]>,
    profile_colors: Vec<rerun::Color>,
    profile_radii: Vec<rerun::Radius>,
    trajectory_lines: Vec<Vec<[f64; 2]>>,
    profile_lines: Vec<Vec<[f32; 2]>>,
    coordinate_valid_rows: usize,
    value_valid_rows: usize,
    accepted_qc_rows: usize,
    rejected_qc_rows: usize,
    missing_qc_rows: usize,
    trajectory_count: usize,
    profile_count: usize,
}

pub(crate) fn is_profile_trajectory(encoding: &BTreeMap<String, Value>) -> bool {
    encoding.get("view_kind").and_then(Value::as_str) == Some(PROFILE_TRAJECTORY_VIEW_KIND)
}

pub(crate) fn profile_value_field(encoding: &BTreeMap<String, Value>) -> Option<&str> {
    encoding
        .get("value")
        .and_then(Value::as_object)
        .and_then(|value| value.get("field"))
        .and_then(Value::as_str)
}

pub(crate) fn log_profile_trajectory(
    recording: &rerun::RecordingStream,
    entity_root: &str,
    path: &Path,
    original_name: &str,
    encoding: &BTreeMap<String, Value>,
) -> Result<()> {
    let recipe = parse_recipe(encoding)?;
    let data = load_profile_trajectory(path, original_name, &recipe)?;

    recording
        .log_static(
            format!("{entity_root}/map_observations"),
            &rerun::GeoPoints::from_lat_lon(data.map_positions.iter().copied())
                .with_colors(data.map_colors.iter().copied())
                .with_radii(data.map_radii.iter().copied()),
        )
        .map_err(rerun_error)?;
    if !data.trajectory_lines.is_empty() {
        recording
            .log_static(
                format!("{entity_root}/trajectory_lines"),
                &rerun::GeoLineStrings::from_lat_lon(data.trajectory_lines.iter().cloned())
                    .with_colors([rerun::Color::from_rgb(66, 183, 124)])
                    .with_radii([rerun::Radius::new_ui_points(2.0)]),
            )
            .map_err(rerun_error)?;
    }
    recording
        .log_static(
            format!("{entity_root}/profile_observations"),
            &rerun::Points2D::new(data.profile_positions.iter().copied())
                .with_colors(data.profile_colors.iter().copied())
                .with_radii(data.profile_radii.iter().copied()),
        )
        .map_err(rerun_error)?;
    if !data.profile_lines.is_empty() {
        recording
            .log_static(
                format!("{entity_root}/profile_lines"),
                &rerun::LineStrips2D::new(data.profile_lines.iter().cloned())
                    .with_colors([rerun::Color::from_rgb(99, 221, 157)])
                    .with_radii([rerun::Radius::new_ui_points(2.0)]),
            )
            .map_err(rerun_error)?;
    }

    let display_transform = if recipe.vertical_positive_down {
        "display_vertical = -source_vertical"
    } else {
        "display_vertical = source_vertical"
    };
    recording
        .log_static(
            format!("{entity_root}/profile_trajectory_info"),
            &rerun::TextDocument::new(
                json!({
                    "view_kind": PROFILE_TRAJECTORY_VIEW_KIND,
                    "source_rows": data.source_rows,
                    "coordinate_valid_rows": data.coordinate_valid_rows,
                    "value_valid_rows": data.value_valid_rows,
                    "accepted_qc_rows": data.accepted_qc_rows,
                    "rejected_qc_rows": data.rejected_qc_rows,
                    "missing_qc_rows": data.missing_qc_rows,
                    "trajectory_count": data.trajectory_count,
                    "profile_count": data.profile_count,
                    "value_field": recipe.value_field,
                    "value_unit": recipe.value_unit,
                    "vertical_field": recipe.vertical_field,
                    "vertical_unit": recipe.vertical_unit,
                    "display_transform": display_transform,
                    "instance_contract": "observation array position equals zero-based source row; invalid observations retain transparent in-range placeholders",
                    "rerun_version": ecoscope_core::PINNED_RERUN_VERSION,
                })
                .to_string(),
            )
            .with_media_type(rerun::MediaType::markdown()),
        )
        .map_err(rerun_error)
}

fn parse_recipe(encoding: &BTreeMap<String, Value>) -> Result<ProfileTrajectoryRecipe> {
    if !is_profile_trajectory(encoding) {
        return Err(EcoScopeError::Invalid(format!(
            "tabular profile renderer requires view_kind={PROFILE_TRAJECTORY_VIEW_KIND}"
        )));
    }
    let vertical = object(encoding.get("vertical"), "vertical")?;
    let value = object(encoding.get("value"), "value")?;
    let direction = string(vertical.get("direction"), "vertical.direction")?;
    let vertical_positive_down = match direction {
        "positive_down" => true,
        "positive_up" => false,
        _ => {
            return Err(EcoScopeError::Invalid(
                "vertical.direction must be positive_down or positive_up".into(),
            ));
        }
    };
    let accepted_qc = value
        .get("accepted_qc")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(scalar_text)
        .collect::<BTreeSet<_>>();
    Ok(ProfileTrajectoryRecipe {
        trajectory_id_field: top_level_string(encoding, "trajectory_id_field")?,
        profile_id_field: top_level_string(encoding, "profile_id_field")?,
        latitude_field: top_level_string(encoding, "latitude_field")?,
        longitude_field: top_level_string(encoding, "longitude_field")?,
        vertical_field: string(vertical.get("field"), "vertical.field")?.into(),
        vertical_positive_down,
        vertical_unit: optional_string(vertical.get("unit")),
        value_field: string(value.get("field"), "value.field")?.into(),
        value_unit: optional_string(value.get("unit")),
        qc_field: optional_string(value.get("qc_field")),
        accepted_qc,
    })
}

fn load_profile_trajectory(
    path: &Path,
    original_name: &str,
    recipe: &ProfileTrajectoryRecipe,
) -> Result<ProfileTrajectoryData> {
    let delimiter = match Path::new(original_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("csv") => b',',
        Some("tsv") => b'\t',
        _ => {
            return Err(EcoScopeError::Invalid(
                "profile/trajectory rendering currently supports CSV and TSV assets".into(),
            ));
        }
    };
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(false)
        .from_path(path)
        .map_err(csv_error)?;
    let headers = reader.headers().map_err(csv_error)?.clone();
    let required = [
        recipe.trajectory_id_field.as_str(),
        recipe.profile_id_field.as_str(),
        recipe.latitude_field.as_str(),
        recipe.longitude_field.as_str(),
        recipe.vertical_field.as_str(),
        recipe.value_field.as_str(),
    ];
    for field in required.into_iter().chain(recipe.qc_field.as_deref()) {
        if !headers.iter().any(|header| header == field) {
            return Err(EcoScopeError::Invalid(format!(
                "profile/trajectory field {field} is absent from {original_name}"
            )));
        }
    }
    let index_for = |field: &str| {
        headers
            .iter()
            .position(|header| header == field)
            .expect("validated profile/trajectory header")
    };
    let trajectory_index = index_for(&recipe.trajectory_id_field);
    let profile_index = index_for(&recipe.profile_id_field);
    let latitude_index = index_for(&recipe.latitude_field);
    let longitude_index = index_for(&recipe.longitude_field);
    let vertical_index = index_for(&recipe.vertical_field);
    let value_index = index_for(&recipe.value_field);
    let qc_index = recipe.qc_field.as_deref().map(index_for);

    let mut map_positions = Vec::new();
    let mut map_colors = Vec::new();
    let mut map_radii = Vec::new();
    let mut profile_positions = Vec::new();
    let mut profile_colors = Vec::new();
    let mut profile_radii = Vec::new();
    let mut trajectory_groups = BTreeMap::<String, Vec<[f64; 2]>>::new();
    let mut profile_groups = BTreeMap::<(String, String), Vec<[f32; 2]>>::new();
    let mut coordinate_valid_rows = 0;
    let mut value_valid_rows = 0;
    let mut accepted_qc_rows = 0;
    let mut rejected_qc_rows = 0;
    let mut missing_qc_rows = 0;

    for (source_index, record) in reader.records().enumerate() {
        if source_index >= MAX_PROFILE_TRAJECTORY_ROWS {
            return Err(EcoScopeError::Invalid(format!(
                "profile/trajectory rendering is limited to {MAX_PROFILE_TRAJECTORY_ROWS} source rows"
            )));
        }
        let record = record.map_err(csv_error)?;
        let trajectory_id = record
            .get(trajectory_index)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let profile_id = record
            .get(profile_index)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let latitude = finite_number(record.get(latitude_index));
        let longitude = finite_number(record.get(longitude_index));
        let vertical = finite_number(record.get(vertical_index));
        let value = finite_number(record.get(value_index));
        let coordinate_valid = latitude.is_some() && longitude.is_some();
        let profile_valid = vertical.is_some() && value.is_some();
        let qc_text = qc_index
            .and_then(|index| record.get(index))
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let qc_accepted = recipe.accepted_qc.is_empty()
            || qc_text.is_some_and(|value| recipe.accepted_qc.contains(value));

        if coordinate_valid {
            coordinate_valid_rows += 1;
            let position = [latitude.unwrap(), longitude.unwrap()];
            map_positions.push(position);
            map_colors.push(rerun::Color::from_rgb(66, 183, 124));
            map_radii.push(rerun::Radius::new_ui_points(5.0));
            trajectory_groups
                .entry(trajectory_id.clone())
                .or_default()
                .push(position);
        } else {
            map_positions.push([f64::NAN, f64::NAN]);
            map_colors.push(rerun::Color::TRANSPARENT);
            map_radii.push(rerun::Radius::new_ui_points(0.0));
        }

        let displayed_vertical = vertical.map(|vertical| {
            if recipe.vertical_positive_down {
                -vertical
            } else {
                vertical
            }
        });
        if profile_valid {
            value_valid_rows += 1;
            let position = [value.unwrap() as f32, displayed_vertical.unwrap() as f32];
            profile_positions.push(position);
            profile_radii.push(rerun::Radius::new_ui_points(5.0));
            if qc_accepted {
                accepted_qc_rows += 1;
                profile_colors.push(rerun::Color::from_rgb(99, 221, 157));
                profile_groups
                    .entry((trajectory_id, profile_id))
                    .or_default()
                    .push(position);
            } else {
                rejected_qc_rows += 1;
                profile_colors.push(rerun::Color::from_rgb(239, 168, 68));
            }
        } else {
            profile_positions.push([f32::NAN, f32::NAN]);
            profile_colors.push(rerun::Color::TRANSPARENT);
            profile_radii.push(rerun::Radius::new_ui_points(0.0));
        }
        if qc_index.is_some() && qc_text.is_none() {
            missing_qc_rows += 1;
        }
    }

    let source_rows = map_positions.len();
    if source_rows == 0 {
        return Err(EcoScopeError::Invalid(
            "profile/trajectory source contains no data rows".into(),
        ));
    }
    replace_non_finite_positions(&mut map_positions);
    replace_non_finite_profile_positions(&mut profile_positions);
    let trajectory_count = trajectory_groups.len();
    let profile_count = profile_groups.len();
    let trajectory_lines = trajectory_groups
        .into_values()
        .filter(|line| line.len() >= 2)
        .collect();
    let profile_lines = profile_groups
        .into_values()
        .filter(|line| line.len() >= 2)
        .collect();
    Ok(ProfileTrajectoryData {
        source_rows,
        map_positions,
        map_colors,
        map_radii,
        profile_positions,
        profile_colors,
        profile_radii,
        trajectory_lines,
        profile_lines,
        coordinate_valid_rows,
        value_valid_rows,
        accepted_qc_rows,
        rejected_qc_rows,
        missing_qc_rows,
        trajectory_count,
        profile_count,
    })
}

fn replace_non_finite_positions<const N: usize>(positions: &mut [[f64; N]]) {
    let fallback = positions
        .iter()
        .copied()
        .find(|position| position.iter().all(|value| value.is_finite()))
        .unwrap_or([0.0; N]);
    for position in positions {
        if !position.iter().all(|value| value.is_finite()) {
            *position = fallback;
        }
    }
}

fn replace_non_finite_profile_positions(positions: &mut [[f32; 2]]) {
    let fallback = positions
        .iter()
        .copied()
        .find(|position| position.iter().all(|value| value.is_finite()))
        .unwrap_or([0.0; 2]);
    for position in positions {
        if !position.iter().all(|value| value.is_finite()) {
            *position = fallback;
        }
    }
}

fn top_level_string(encoding: &BTreeMap<String, Value>, field: &str) -> Result<String> {
    Ok(string(encoding.get(field), field)?.into())
}

fn object<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a serde_json::Map<String, Value>> {
    value.and_then(Value::as_object).ok_or_else(|| {
        EcoScopeError::Invalid(format!(
            "profile/trajectory encoding requires object {field}"
        ))
    })
}

fn string<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str> {
    value.and_then(Value::as_str).ok_or_else(|| {
        EcoScopeError::Invalid(format!(
            "profile/trajectory encoding requires string {field}"
        ))
    })
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn finite_number(value: Option<&str>) -> Option<f64> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn csv_error(error: csv::Error) -> EcoScopeError {
    EcoScopeError::Invalid(format!("cannot read profile/trajectory table: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/profile_trajectory.csv")
    }

    fn encoding() -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("view_kind".into(), json!(PROFILE_TRAJECTORY_VIEW_KIND)),
            ("trajectory_id_field".into(), json!("platform_number")),
            ("profile_id_field".into(), json!("cycle_number")),
            ("time_field".into(), json!("time")),
            ("latitude_field".into(), json!("latitude")),
            ("longitude_field".into(), json!("longitude")),
            (
                "vertical".into(),
                json!({"field": "pres", "direction": "positive_down", "unit": "decibar"}),
            ),
            (
                "value".into(),
                json!({
                    "field": "temp_adjusted",
                    "unit": "degree_Celsius",
                    "qc_field": "temp_adjusted_qc",
                    "accepted_qc": ["1", "2"]
                }),
            ),
        ])
    }

    #[test]
    fn source_order_survives_missing_values_and_qc() {
        let encoding = encoding();
        let recipe = parse_recipe(&encoding).unwrap();
        let data = load_profile_trajectory(&fixture(), "profile_trajectory.csv", &recipe).unwrap();

        assert_eq!(data.source_rows, 16);
        assert_eq!(data.map_positions.len(), 16);
        assert_eq!(data.profile_positions.len(), 16);
        assert_eq!(data.profile_positions[3], data.profile_positions[0]);
        assert_eq!(data.profile_colors[3].to_array()[3], 0);
        assert_eq!(data.map_positions[6], data.map_positions[0]);
        assert_eq!(data.map_colors[6].to_array()[3], 0);
        assert_eq!(data.profile_positions[7], [8.21, -700.0]);
        assert_eq!(data.profile_positions[10], [17.44, -25.0]);
        assert_eq!(data.coordinate_valid_rows, 15);
        assert_eq!(data.value_valid_rows, 15);
        assert_eq!(data.accepted_qc_rows, 14);
        assert_eq!(data.rejected_qc_rows, 1);
        assert_eq!(data.trajectory_count, 1);
        assert_eq!(data.profile_count, 2);
        assert_eq!(data.trajectory_lines[0].len(), 15);
        assert_eq!(data.profile_lines.iter().map(Vec::len).sum::<usize>(), 14);
    }

    #[test]
    fn recipe_exposes_the_profile_value_field() {
        assert_eq!(profile_value_field(&encoding()), Some("temp_adjusted"));
    }
}
