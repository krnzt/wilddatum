use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use ecoscope_core::{
    EcoScopeError, PROFILE_TRAJECTORY_VIEW_KIND, ProfileTrajectoryRecipeV1, Result,
    VerticalDirection,
};
use serde_json::{Value, json};

use crate::rerun_error;

const MAX_PROFILE_TRAJECTORY_ROWS: usize = 100_000;

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
    raw_vertical_range: Option<[f64; 2]>,
    raw_value_range: Option<[f64; 2]>,
}

#[derive(Debug)]
struct ProfileTrajectoryObservation {
    source_index: u64,
    trajectory_id: String,
    profile_id: String,
    latitude: Option<f64>,
    longitude: Option<f64>,
    vertical: Option<f64>,
    value: Option<f64>,
    qc: Option<String>,
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

    let display_transform = if recipe.vertical.direction == VerticalDirection::PositiveDown {
        "display_vertical = -source_vertical"
    } else {
        "display_vertical = source_vertical"
    };
    recording
        .log_static(
            format!("{entity_root}/profile_trajectory_info"),
            &rerun::TextDocument::new(
                render_summary(&recipe, &data, display_transform).to_string(),
            )
            .with_media_type(rerun::MediaType::markdown()),
        )
        .map_err(rerun_error)
}

fn render_summary(
    recipe: &ProfileTrajectoryRecipeV1,
    data: &ProfileTrajectoryData,
    display_transform: &str,
) -> Value {
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
        "recipe": recipe,
        "raw_value_range": data.raw_value_range,
        "raw_vertical_range": data.raw_vertical_range,
        "truncated": false,
        "error": null,
        "display_transform": display_transform,
        "instance_contract": "observation array position equals zero-based source row; invalid observations retain transparent in-range placeholders",
        "rerun_version": ecoscope_core::PINNED_RERUN_VERSION,
    })
}

fn parse_recipe(encoding: &BTreeMap<String, Value>) -> Result<ProfileTrajectoryRecipeV1> {
    if !is_profile_trajectory(encoding) {
        return Err(EcoScopeError::Invalid(format!(
            "tabular profile renderer requires view_kind={PROFILE_TRAJECTORY_VIEW_KIND}"
        )));
    }
    let mut value = serde_json::to_value(encoding)?;
    let object = value
        .as_object_mut()
        .expect("BTreeMap always serializes as a JSON object");
    object.remove("view_kind");
    object.remove("selection_mapping");
    let recipe: ProfileTrajectoryRecipeV1 = serde_json::from_value(value).map_err(|error| {
        EcoScopeError::Invalid(format!("invalid profile/trajectory recipe: {error}"))
    })?;
    if !recipe.value.accepted_qc.is_empty() && recipe.value.qc_field.is_none() {
        return Err(EcoScopeError::Invalid(
            "profile/trajectory accepted_qc requires value.qc_field".into(),
        ));
    }
    Ok(recipe)
}

fn load_profile_trajectory(
    path: &Path,
    original_name: &str,
    recipe: &ProfileTrajectoryRecipeV1,
) -> Result<ProfileTrajectoryData> {
    let observations = parse_profile_trajectory(path, original_name, recipe)?;
    derive_profile_trajectory(observations, recipe)
}

fn parse_profile_trajectory(
    path: &Path,
    original_name: &str,
    recipe: &ProfileTrajectoryRecipeV1,
) -> Result<Vec<ProfileTrajectoryObservation>> {
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
        recipe.vertical.field.as_str(),
        recipe.value.field.as_str(),
    ];
    for field in required.into_iter().chain(recipe.value.qc_field.as_deref()) {
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
    let vertical_index = index_for(&recipe.vertical.field);
    let value_index = index_for(&recipe.value.field);
    let qc_index = recipe.value.qc_field.as_deref().map(index_for);
    let vertical_fill_values = recipe
        .vertical
        .fill_values
        .iter()
        .map(|value| value.trim())
        .collect::<BTreeSet<_>>();
    let value_fill_values = recipe
        .value
        .fill_values
        .iter()
        .map(|value| value.trim())
        .collect::<BTreeSet<_>>();
    let no_fill_values = BTreeSet::new();

    let mut observations = Vec::new();
    for (source_index, record) in reader.records().enumerate() {
        if source_index >= MAX_PROFILE_TRAJECTORY_ROWS {
            return Err(EcoScopeError::Invalid(format!(
                "profile/trajectory rendering is limited to {MAX_PROFILE_TRAJECTORY_ROWS} source rows"
            )));
        }
        let record = record.map_err(csv_error)?;
        observations.push(ProfileTrajectoryObservation {
            source_index: source_index as u64,
            trajectory_id: record
                .get(trajectory_index)
                .unwrap_or_default()
                .trim()
                .to_owned(),
            profile_id: record
                .get(profile_index)
                .unwrap_or_default()
                .trim()
                .to_owned(),
            latitude: finite_number(record.get(latitude_index), &no_fill_values),
            longitude: finite_number(record.get(longitude_index), &no_fill_values),
            vertical: finite_number(record.get(vertical_index), &vertical_fill_values),
            value: finite_number(record.get(value_index), &value_fill_values),
            qc: qc_index
                .and_then(|index| record.get(index))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        });
    }
    if observations.is_empty() {
        return Err(EcoScopeError::Invalid(
            "profile/trajectory source contains no data rows".into(),
        ));
    }
    Ok(observations)
}

fn derive_profile_trajectory(
    observations: Vec<ProfileTrajectoryObservation>,
    recipe: &ProfileTrajectoryRecipeV1,
) -> Result<ProfileTrajectoryData> {
    let source_rows = observations.len();
    let accepted_qc = recipe
        .value
        .accepted_qc
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();

    let mut map_positions = Vec::with_capacity(source_rows);
    let mut map_colors = Vec::with_capacity(source_rows);
    let mut map_radii = Vec::with_capacity(source_rows);
    let mut profile_positions = Vec::with_capacity(source_rows);
    let mut profile_colors = Vec::with_capacity(source_rows);
    let mut profile_radii = Vec::with_capacity(source_rows);
    let mut trajectory_groups = BTreeMap::<String, Vec<[f64; 2]>>::new();
    let mut profile_groups = BTreeMap::<(String, String), Vec<[f32; 2]>>::new();
    let mut coordinate_valid_rows = 0;
    let mut value_valid_rows = 0;
    let mut accepted_qc_rows = 0;
    let mut rejected_qc_rows = 0;
    let mut missing_qc_rows = 0;
    let mut raw_vertical_range = None;
    let mut raw_value_range = None;

    for observation in observations {
        if observation.source_index != map_positions.len() as u64 {
            return Err(EcoScopeError::Internal(
                "profile/trajectory source observations lost source order".into(),
            ));
        }
        let ProfileTrajectoryObservation {
            trajectory_id,
            profile_id,
            latitude,
            longitude,
            vertical,
            value,
            qc,
            ..
        } = observation;
        include_range(&mut raw_vertical_range, vertical);
        include_range(&mut raw_value_range, value);
        let coordinate_valid = latitude.is_some() && longitude.is_some();
        let profile_valid = vertical.is_some() && value.is_some();
        let qc_text = qc.as_deref();
        let qc_accepted =
            accepted_qc.is_empty() || qc_text.is_some_and(|value| accepted_qc.contains(value));

        if coordinate_valid {
            coordinate_valid_rows += 1;
            let position = [latitude.unwrap(), longitude.unwrap()];
            map_positions.push(position);
            map_colors.push(if qc_accepted {
                rerun::Color::from_rgb(66, 183, 124)
            } else {
                rerun::Color::from_rgb(239, 168, 68)
            });
            map_radii.push(rerun::Radius::new_ui_points(5.0));
            if qc_accepted {
                trajectory_groups
                    .entry(trajectory_id.clone())
                    .or_default()
                    .push(position);
            }
        } else {
            map_positions.push([f64::NAN, f64::NAN]);
            map_colors.push(rerun::Color::TRANSPARENT);
            map_radii.push(rerun::Radius::new_ui_points(0.0));
        }

        let displayed_vertical = vertical.map(|vertical| {
            if recipe.vertical.direction == VerticalDirection::PositiveDown {
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
        if recipe.value.qc_field.is_some() && qc_text.is_none() {
            missing_qc_rows += 1;
        }
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
        raw_vertical_range,
        raw_value_range,
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

fn finite_number(value: Option<&str>, fill_values: &BTreeSet<&str>) -> Option<f64> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| !fill_values.contains(value))
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn include_range(range: &mut Option<[f64; 2]>, value: Option<f64>) {
    let Some(value) = value else {
        return;
    };
    match range {
        Some([minimum, maximum]) => {
            *minimum = minimum.min(value);
            *maximum = maximum.max(value);
        }
        None => *range = Some([value, value]),
    }
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
        assert_eq!(data.trajectory_lines[0].len(), 13);
        assert_eq!(data.profile_lines.iter().map(Vec::len).sum::<usize>(), 14);
        assert_eq!(data.map_positions[10], [35.05, -75.0]);
        assert_eq!(
            data.map_colors[10].to_array(),
            rerun::Color::from_rgb(239, 168, 68).to_array()
        );
        assert_eq!(data.raw_vertical_range, Some([0.0, 700.0]));
        assert_eq!(data.raw_value_range, Some([7.94, 18.4]));
        let summary = render_summary(&recipe, &data, "display_vertical = -source_vertical");
        assert_eq!(summary["recipe"]["value"]["unit"], "degree_Celsius");
        assert_eq!(summary["recipe"]["vertical"]["unit"], "decibar");
        assert_eq!(summary["raw_vertical_range"], json!([0.0, 700.0]));
        assert_eq!(
            summary["display_transform"],
            "display_vertical = -source_vertical"
        );
    }

    #[test]
    fn recipe_exposes_the_profile_value_field() {
        assert_eq!(profile_value_field(&encoding()), Some("temp_adjusted"));
    }

    #[test]
    fn positive_up_preserves_raw_vertical_sign() {
        let mut encoding = encoding();
        encoding
            .get_mut("vertical")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("direction".into(), json!("positive_up"));
        let recipe = parse_recipe(&encoding).unwrap();
        let data = load_profile_trajectory(&fixture(), "profile_trajectory.csv", &recipe).unwrap();
        assert_eq!(data.profile_positions[7], [8.21, 700.0]);
    }

    #[test]
    fn configured_fill_values_leave_transparent_source_slots() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fills.csv");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "platform_number,cycle_number,time,latitude,longitude,pres,temp_adjusted,temp_adjusted_qc"
        )
        .unwrap();
        writeln!(file, "P,1,t0,1,2,0,18,1").unwrap();
        writeln!(file, "P,1,t1,2,3,10,-9999,1").unwrap();
        writeln!(file, "P,1,t2,3,4,20,NaN,1").unwrap();
        writeln!(file, "P,1,t3,4,5,30,16,1").unwrap();
        writeln!(file, "Q,1,t4,5,6,0,20,1").unwrap();
        writeln!(file, "Q,1,t5,6,7,10,19,1").unwrap();
        let mut encoding = encoding();
        encoding
            .get_mut("value")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("fill_values".into(), json!(["-9999"]));

        let recipe = parse_recipe(&encoding).unwrap();
        let data = load_profile_trajectory(&path, "fills.csv", &recipe).unwrap();

        assert_eq!(data.source_rows, 6);
        assert_eq!(data.value_valid_rows, 4);
        assert_eq!(data.profile_positions[1], data.profile_positions[0]);
        assert_eq!(data.profile_colors[1].to_array()[3], 0);
        assert_eq!(data.profile_positions[2], data.profile_positions[0]);
        assert_eq!(data.profile_colors[2].to_array()[3], 0);
        assert_eq!(data.trajectory_count, 2);
        assert_eq!(data.profile_count, 2);
        assert_eq!(data.trajectory_lines.len(), 2);
        assert_eq!(data.profile_lines.len(), 2);
        assert_eq!(data.raw_value_range, Some([16.0, 20.0]));
    }

    #[test]
    fn row_limit_is_a_hard_error_not_silent_truncation() {
        use std::io::{BufWriter, Write};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("too-many.csv");
        let mut file = BufWriter::new(std::fs::File::create(&path).unwrap());
        writeln!(
            file,
            "platform_number,cycle_number,time,latitude,longitude,pres,temp_adjusted,temp_adjusted_qc"
        )
        .unwrap();
        for index in 0..=MAX_PROFILE_TRAJECTORY_ROWS {
            writeln!(file, "P,1,{index},1,2,{index},18,1").unwrap();
        }
        drop(file);
        let recipe = parse_recipe(&encoding()).unwrap();
        let error = load_profile_trajectory(&path, "too-many.csv", &recipe)
            .unwrap_err()
            .to_string();
        assert!(error.contains("limited to 100000 source rows"));
    }
}
