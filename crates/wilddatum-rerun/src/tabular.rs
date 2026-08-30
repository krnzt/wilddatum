use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde_json::{Value, json};
use wilddatum_core::{
    MAX_PROFILE_TRAJECTORY_ROWS, PROFILE_TRAJECTORY_VIEW_KIND, ProfileTrajectoryRecipeV1,
    ProfileValueSpec, Result, VerticalDirection, WildDatumError, profile_line_suffix,
    profile_observation_suffix,
};

use crate::rerun_error;

#[derive(Debug)]
struct ProfileTrajectoryData {
    source_rows: usize,
    map_positions: Vec<[f64; 2]>,
    map_colors: Vec<rerun::Color>,
    map_radii: Vec<rerun::Radius>,
    trajectory_lines: Vec<Vec<[f64; 2]>>,
    value_series: Vec<ProfileValueData>,
    coordinate_valid_rows: usize,
    trajectory_count: usize,
    raw_vertical_range: Option<[f64; 2]>,
    source_row_identity: &'static str,
}

#[derive(Debug)]
struct ProfileValueData {
    field: String,
    profile_positions: Vec<[f32; 2]>,
    profile_colors: Vec<rerun::Color>,
    profile_radii: Vec<rerun::Radius>,
    profile_lines: Vec<Vec<[f32; 2]>>,
    value_valid_rows: usize,
    displayed_rows: usize,
    downsampled_rows: usize,
    range_filtered_rows: usize,
    accepted_qc_rows: usize,
    rejected_qc_rows: usize,
    missing_qc_rows: usize,
    profile_count: usize,
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
    values: Vec<ProfileValueObservation>,
}

#[derive(Debug)]
struct ProfileValueObservation {
    value: Option<f64>,
    qc: Option<String>,
}

pub(crate) fn is_profile_trajectory(encoding: &BTreeMap<String, Value>) -> bool {
    encoding.get("view_kind").and_then(Value::as_str) == Some(PROFILE_TRAJECTORY_VIEW_KIND)
}

pub(crate) fn profile_value_fields(encoding: &BTreeMap<String, Value>) -> Vec<&str> {
    encoding
        .get("value")
        .and_then(Value::as_object)
        .and_then(|value| value.get("field"))
        .and_then(Value::as_str)
        .into_iter()
        .chain(
            encoding
                .get("additional_values")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|value| value.get("field").and_then(Value::as_str)),
        )
        .collect()
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
    for (index, series) in data.value_series.iter().enumerate() {
        let observations = profile_observation_suffix(index, &series.field);
        recording
            .log_static(
                format!("{entity_root}/{observations}"),
                &rerun::Points2D::new(series.profile_positions.iter().copied())
                    .with_colors(series.profile_colors.iter().copied())
                    .with_radii(series.profile_radii.iter().copied()),
            )
            .map_err(rerun_error)?;
        if !series.profile_lines.is_empty() {
            recording
                .log_static(
                    format!(
                        "{entity_root}/{}",
                        profile_line_suffix(index, &series.field)
                    ),
                    &rerun::LineStrips2D::new(series.profile_lines.iter().cloned())
                        .with_colors([rerun::Color::from_rgb(99, 221, 157)])
                        .with_radii([rerun::Radius::new_ui_points(2.0)]),
                )
                .map_err(rerun_error)?;
        }
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
        "trajectory_count": data.trajectory_count,
        "value_series": data.value_series.iter().map(|series| json!({
            "field": series.field,
            "value_valid_rows": series.value_valid_rows,
            "displayed_rows": series.displayed_rows,
            "downsampled_rows": series.downsampled_rows,
            "range_filtered_rows": series.range_filtered_rows,
            "accepted_qc_rows": series.accepted_qc_rows,
            "rejected_qc_rows": series.rejected_qc_rows,
            "missing_qc_rows": series.missing_qc_rows,
            "profile_count": series.profile_count,
            "raw_value_range": series.raw_value_range,
        })).collect::<Vec<_>>(),
        "recipe": recipe,
        "raw_vertical_range": data.raw_vertical_range,
        "source_row_identity": data.source_row_identity,
        "truncated": false,
        "error": null,
        "display_transform": display_transform,
        "instance_contract": "observation array position equals zero-based source row; invalid observations retain transparent in-range placeholders",
        "rerun_version": wilddatum_core::PINNED_RERUN_VERSION,
    })
}

fn parse_recipe(encoding: &BTreeMap<String, Value>) -> Result<ProfileTrajectoryRecipeV1> {
    if !is_profile_trajectory(encoding) {
        return Err(WildDatumError::Invalid(format!(
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
        WildDatumError::Invalid(format!("invalid profile/trajectory recipe: {error}"))
    })?;
    for value in recipe.values() {
        if !value.accepted_qc.is_empty() && value.qc_field.is_none() {
            return Err(WildDatumError::Invalid(format!(
                "profile/trajectory accepted_qc requires a qc_field for {}",
                value.field
            )));
        }
    }
    Ok(recipe)
}

fn load_profile_trajectory(
    path: &Path,
    original_name: &str,
    recipe: &ProfileTrajectoryRecipeV1,
) -> Result<ProfileTrajectoryData> {
    let (observations, source_row_identity) =
        parse_profile_trajectory(path, original_name, recipe)?;
    derive_profile_trajectory(observations, recipe, source_row_identity)
}

fn parse_profile_trajectory(
    path: &Path,
    original_name: &str,
    recipe: &ProfileTrajectoryRecipeV1,
) -> Result<(Vec<ProfileTrajectoryObservation>, &'static str)> {
    let table =
        wilddatum_query::read_source_table(path, original_name, MAX_PROFILE_TRAJECTORY_ROWS)?;
    let required = [
        recipe.trajectory_id_field.as_str(),
        recipe.profile_id_field.as_str(),
        recipe.latitude_field.as_str(),
        recipe.longitude_field.as_str(),
        recipe.vertical.field.as_str(),
    ];
    for field in required
        .into_iter()
        .chain(recipe.values().map(|value| value.field.as_str()))
        .chain(
            recipe
                .values()
                .filter_map(|value| value.qc_field.as_deref()),
        )
    {
        if !table.columns.iter().any(|header| header == field) {
            return Err(WildDatumError::Invalid(format!(
                "profile/trajectory field {field} is absent from {original_name}"
            )));
        }
    }
    let vertical_fill_values = recipe
        .vertical
        .fill_values
        .iter()
        .map(|value| value.trim())
        .collect::<BTreeSet<_>>();
    let no_fill_values = BTreeSet::new();

    let mut observations = Vec::with_capacity(table.rows.len());
    for (source_index, row) in table.rows.iter().enumerate() {
        observations.push(ProfileTrajectoryObservation {
            source_index: source_index as u64,
            trajectory_id: scalar_string(row.get(&recipe.trajectory_id_field)),
            profile_id: scalar_string(row.get(&recipe.profile_id_field)),
            latitude: finite_number(row.get(&recipe.latitude_field), &no_fill_values),
            longitude: finite_number(row.get(&recipe.longitude_field), &no_fill_values),
            vertical: finite_number(row.get(&recipe.vertical.field), &vertical_fill_values),
            values: recipe
                .values()
                .map(|value| {
                    let fill_values = value
                        .fill_values
                        .iter()
                        .map(|value| value.trim())
                        .collect::<BTreeSet<_>>();
                    ProfileValueObservation {
                        value: finite_number(row.get(&value.field), &fill_values),
                        qc: value
                            .qc_field
                            .as_ref()
                            .and_then(|field| row.get(field))
                            .map(|value| scalar_string(Some(value)))
                            .filter(|value| !value.is_empty()),
                    }
                })
                .collect(),
        });
    }
    if observations.is_empty() {
        return Err(WildDatumError::Invalid(
            "profile/trajectory source contains no data rows".into(),
        ));
    }
    Ok((observations, table.identity))
}

fn derive_profile_trajectory(
    observations: Vec<ProfileTrajectoryObservation>,
    recipe: &ProfileTrajectoryRecipeV1,
    source_row_identity: &'static str,
) -> Result<ProfileTrajectoryData> {
    let source_rows = observations.len();
    let mut map_positions = Vec::with_capacity(source_rows);
    let mut map_colors = Vec::with_capacity(source_rows);
    let mut map_radii = Vec::with_capacity(source_rows);
    let mut trajectory_groups = BTreeMap::<String, Vec<[f64; 2]>>::new();
    let mut coordinate_valid_rows = 0;
    let mut raw_vertical_range = None;
    let primary_accepted_qc = recipe
        .value
        .accepted_qc
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();

    for observation in &observations {
        if observation.source_index != map_positions.len() as u64 {
            return Err(WildDatumError::Internal(
                "profile/trajectory source observations lost source order".into(),
            ));
        }
        include_range(&mut raw_vertical_range, observation.vertical);
        let coordinate_valid = observation.latitude.is_some() && observation.longitude.is_some();
        let primary_qc = observation.values[0].qc.as_deref();
        let primary_qc_accepted = primary_accepted_qc.is_empty()
            || primary_qc.is_some_and(|value| primary_accepted_qc.contains(value));

        if coordinate_valid {
            coordinate_valid_rows += 1;
            let position = [
                observation.latitude.unwrap(),
                observation.longitude.unwrap(),
            ];
            map_positions.push(position);
            map_colors.push(if primary_qc_accepted {
                rerun::Color::from_rgb(66, 183, 124)
            } else {
                rerun::Color::from_rgb(239, 168, 68)
            });
            map_radii.push(rerun::Radius::new_ui_points(5.0));
            if primary_qc_accepted {
                trajectory_groups
                    .entry(observation.trajectory_id.clone())
                    .or_default()
                    .push(position);
            }
        } else {
            map_positions.push([f64::NAN, f64::NAN]);
            map_colors.push(rerun::Color::TRANSPARENT);
            map_radii.push(rerun::Radius::new_ui_points(0.0));
        }
    }

    replace_non_finite_positions(&mut map_positions);
    let trajectory_count = trajectory_groups.len();
    let trajectory_lines = trajectory_groups
        .into_values()
        .filter(|line| line.len() >= 2)
        .collect();
    let value_series = recipe
        .values()
        .enumerate()
        .map(|(value_index, value)| derive_profile_value(&observations, recipe, value_index, value))
        .collect::<Vec<_>>();
    Ok(ProfileTrajectoryData {
        source_rows,
        map_positions,
        map_colors,
        map_radii,
        trajectory_lines,
        value_series,
        coordinate_valid_rows,
        trajectory_count,
        raw_vertical_range,
        source_row_identity,
    })
}

fn derive_profile_value(
    observations: &[ProfileTrajectoryObservation],
    recipe: &ProfileTrajectoryRecipeV1,
    value_index: usize,
    value_spec: &ProfileValueSpec,
) -> ProfileValueData {
    let accepted_qc = value_spec
        .accepted_qc
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut candidates = BTreeMap::<(String, String), Vec<usize>>::new();
    let mut raw_value_range = None;
    let mut value_valid_rows = 0;
    let mut range_filtered_rows = 0;
    for observation in observations {
        let value = observation.values[value_index].value;
        include_range(&mut raw_value_range, value);
        let raw_valid = observation.vertical.is_some() && value.is_some();
        if raw_valid {
            value_valid_rows += 1;
        }
        let in_range = observation.vertical.is_some_and(|vertical| {
            recipe
                .vertical_range
                .is_none_or(|range| range.contains(vertical))
        });
        if raw_valid && !in_range {
            range_filtered_rows += 1;
        }
        if raw_valid && in_range {
            candidates
                .entry((
                    observation.trajectory_id.clone(),
                    observation.profile_id.clone(),
                ))
                .or_default()
                .push(observation.source_index as usize);
        }
    }
    let displayed_indices = candidates
        .values()
        .flat_map(|indices| {
            evenly_sampled_indices(
                indices,
                recipe.max_points_per_profile.map(|value| value as usize),
            )
        })
        .collect::<BTreeSet<_>>();
    let mut profile_positions = Vec::with_capacity(observations.len());
    let mut profile_colors = Vec::with_capacity(observations.len());
    let mut profile_radii = Vec::with_capacity(observations.len());
    let mut profile_groups = BTreeMap::<(String, String), Vec<[f32; 2]>>::new();
    let mut displayed_rows = 0;
    let mut accepted_qc_rows = 0;
    let mut rejected_qc_rows = 0;
    let mut missing_qc_rows = 0;
    for observation in observations {
        let value = &observation.values[value_index];
        let displayed = displayed_indices.contains(&(observation.source_index as usize));
        if value_spec.qc_field.is_some() && value.qc.is_none() {
            missing_qc_rows += 1;
        }
        if displayed {
            displayed_rows += 1;
            let vertical = observation
                .vertical
                .expect("candidate has a vertical value");
            let displayed_vertical = if recipe.vertical.direction == VerticalDirection::PositiveDown
            {
                -vertical
            } else {
                vertical
            };
            let position = [
                value.value.expect("candidate has a profile value") as f32,
                displayed_vertical as f32,
            ];
            profile_positions.push(position);
            profile_radii.push(rerun::Radius::new_ui_points(5.0));
            let qc_accepted = accepted_qc.is_empty()
                || value
                    .qc
                    .as_deref()
                    .is_some_and(|qc| accepted_qc.contains(qc));
            if qc_accepted {
                accepted_qc_rows += 1;
                profile_colors.push(rerun::Color::from_rgb(99, 221, 157));
                profile_groups
                    .entry((
                        observation.trajectory_id.clone(),
                        observation.profile_id.clone(),
                    ))
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
    }
    replace_non_finite_profile_positions(&mut profile_positions);
    let profile_count = profile_groups.len();
    let profile_lines = profile_groups
        .into_values()
        .filter(|line| line.len() >= 2)
        .collect();
    let eligible_rows = candidates.values().map(Vec::len).sum::<usize>();
    ProfileValueData {
        field: value_spec.field.clone(),
        profile_positions,
        profile_colors,
        profile_radii,
        profile_lines,
        value_valid_rows,
        displayed_rows,
        downsampled_rows: eligible_rows.saturating_sub(displayed_rows),
        range_filtered_rows,
        accepted_qc_rows,
        rejected_qc_rows,
        missing_qc_rows,
        profile_count,
        raw_value_range,
    }
}

fn evenly_sampled_indices(indices: &[usize], maximum: Option<usize>) -> Vec<usize> {
    let Some(maximum) = maximum else {
        return indices.to_vec();
    };
    if indices.len() <= maximum {
        return indices.to_vec();
    }
    if maximum == 1 {
        return vec![indices[indices.len() / 2]];
    }
    (0..maximum)
        .map(|slot| indices[slot * (indices.len() - 1) / (maximum - 1)])
        .collect()
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

fn finite_number(value: Option<&Value>, fill_values: &BTreeSet<&str>) -> Option<f64> {
    let value = value?;
    let number = if let Some(number) = value.as_f64() {
        if fill_values
            .iter()
            .filter_map(|fill| fill.parse::<f64>().ok())
            .any(|fill| fill == number)
        {
            return None;
        }
        number
    } else {
        let text = value.as_str()?.trim();
        if text.is_empty() || fill_values.contains(text) {
            return None;
        }
        text.parse().ok()?
    };
    number.is_finite().then_some(number)
}

fn scalar_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.trim().to_owned(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Bool(value)) => value.to_string(),
        _ => String::new(),
    }
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
        let series = &data.value_series[0];

        assert_eq!(data.source_rows, 16);
        assert_eq!(data.map_positions.len(), 16);
        assert_eq!(series.profile_positions.len(), 16);
        assert_eq!(series.profile_positions[3], series.profile_positions[0]);
        assert_eq!(series.profile_colors[3].to_array()[3], 0);
        assert_eq!(data.map_positions[6], data.map_positions[0]);
        assert_eq!(data.map_colors[6].to_array()[3], 0);
        assert_eq!(series.profile_positions[7], [8.21, -700.0]);
        assert_eq!(series.profile_positions[10], [17.44, -25.0]);
        assert_eq!(data.coordinate_valid_rows, 15);
        assert_eq!(series.value_valid_rows, 15);
        assert_eq!(series.accepted_qc_rows, 14);
        assert_eq!(series.rejected_qc_rows, 1);
        assert_eq!(data.trajectory_count, 1);
        assert_eq!(series.profile_count, 2);
        assert_eq!(data.trajectory_lines[0].len(), 13);
        assert_eq!(series.profile_lines.iter().map(Vec::len).sum::<usize>(), 14);
        assert_eq!(data.map_positions[10], [35.05, -75.0]);
        assert_eq!(
            data.map_colors[10].to_array(),
            rerun::Color::from_rgb(239, 168, 68).to_array()
        );
        assert_eq!(data.raw_vertical_range, Some([0.0, 700.0]));
        assert_eq!(series.raw_value_range, Some([7.94, 18.4]));
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
        assert_eq!(profile_value_fields(&encoding()), ["temp_adjusted"]);
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
        assert_eq!(data.value_series[0].profile_positions[7], [8.21, 700.0]);
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
        let series = &data.value_series[0];

        assert_eq!(data.source_rows, 6);
        assert_eq!(series.value_valid_rows, 4);
        assert_eq!(series.profile_positions[1], series.profile_positions[0]);
        assert_eq!(series.profile_colors[1].to_array()[3], 0);
        assert_eq!(series.profile_positions[2], series.profile_positions[0]);
        assert_eq!(series.profile_colors[2].to_array()[3], 0);
        assert_eq!(data.trajectory_count, 2);
        assert_eq!(series.profile_count, 2);
        assert_eq!(data.trajectory_lines.len(), 2);
        assert_eq!(series.profile_lines.len(), 2);
        assert_eq!(series.raw_value_range, Some([16.0, 20.0]));
    }

    #[test]
    fn multiple_values_ranges_and_profile_sampling_preserve_source_slots() {
        let mut encoding = encoding();
        encoding.insert(
            "additional_values".into(),
            json!([{
                "field": "psal_adjusted",
                "unit": "1e-3",
                "qc_field": "psal_adjusted_qc",
                "accepted_qc": ["1", "2"],
                "fill_values": []
            }]),
        );
        encoding.insert(
            "vertical_range".into(),
            json!({"minimum": 10.0, "maximum": 100.0}),
        );
        encoding.insert("max_points_per_profile".into(), json!(3));
        let recipe = parse_recipe(&encoding).unwrap();
        let data = load_profile_trajectory(&fixture(), "profile_trajectory.csv", &recipe).unwrap();

        assert_eq!(data.value_series.len(), 2);
        assert_eq!(
            data.value_series
                .iter()
                .map(|series| series.field.as_str())
                .collect::<Vec<_>>(),
            ["temp_adjusted", "psal_adjusted"]
        );
        for series in &data.value_series {
            assert_eq!(series.profile_positions.len(), data.source_rows);
            assert!(series.displayed_rows <= 6);
            assert!(series.downsampled_rows > 0);
            assert!(series.range_filtered_rows > 0);
            assert!(
                series
                    .profile_colors
                    .iter()
                    .filter(|color| color.to_array()[3] == 0)
                    .count()
                    > 0
            );
        }
        assert_eq!(
            profile_observation_suffix(1, "psal_adjusted"),
            "profile_observations_psal_adjusted"
        );
        let summary = render_summary(&recipe, &data, "display_vertical = -source_vertical");
        assert_eq!(summary["value_series"].as_array().unwrap().len(), 2);
        assert_eq!(summary["recipe"]["max_points_per_profile"], 3);
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
        assert!(error.contains("explicit 100000-row rendering limit"));
    }
}
