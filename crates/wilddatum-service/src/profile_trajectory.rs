//! Validation and persistence for the linked trajectory/profile recipe.

use std::{collections::BTreeSet, path::Path};

use serde_json::{Value, json};
use wilddatum_core::{
    EcoViewSpec, MAX_PROFILE_TRAJECTORY_ROWS, Modality, ProfileTrajectoryRecipeV1, Result,
    SourceRowSelectionMapping, WildDatumError,
};

use super::WildDatumService;

impl WildDatumService {
    /// Validate scientific field semantics against the authoritative source and
    /// atomically replace one layer's profile/trajectory encoding.
    pub fn configure_profile_trajectory_view(
        &self,
        view_id: &str,
        expected_revision: u64,
        layer_id: &str,
        recipe: ProfileTrajectoryRecipeV1,
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
        if !matches!(
            layer.modality,
            Modality::Tabular | Modality::TimeSeries | Modality::Vector
        ) {
            return Err(WildDatumError::Invalid(format!(
                "layer {layer_id} is not tabular, time-series, or trajectory data"
            )));
        }
        if recipe.values().count() > 8 {
            return Err(WildDatumError::Invalid(
                "profile/trajectory views support at most eight value fields".into(),
            ));
        }
        let mut value_fields = BTreeSet::new();
        for value in recipe.values() {
            if !value_fields.insert(value.field.as_str()) {
                return Err(WildDatumError::Invalid(format!(
                    "profile/trajectory value field {} is duplicated",
                    value.field
                )));
            }
            if !value.accepted_qc.is_empty() && value.qc_field.is_none() {
                return Err(WildDatumError::Invalid(format!(
                    "accepted_qc requires a qc_field for value {}",
                    value.field
                )));
            }
        }
        if let Some(range) = recipe.vertical_range
            && (!range.minimum.is_finite()
                || !range.maximum.is_finite()
                || range.minimum > range.maximum)
        {
            return Err(WildDatumError::Invalid(
                "vertical_range requires finite minimum <= maximum".into(),
            ));
        }
        if recipe.max_points_per_profile == Some(0) {
            return Err(WildDatumError::Invalid(
                "max_points_per_profile must be positive".into(),
            ));
        }

        let manifest = self.get_manifest(&layer.dataset_id.0)?;
        let source = manifest
            .source_files
            .first()
            .ok_or_else(|| WildDatumError::Invalid("dataset has no source files".into()))?;
        let path = self.source_path_for_renderer(&manifest, source)?;
        let source_identity = validate_profile_source(&path, &source.original_name, &recipe)?;

        let mut encoding = recipe.encoding();
        encoding.insert("source_row_identity".into(), json!(source_identity));
        encoding.insert(
            "selection_mapping".into(),
            json!(
                SourceRowSelectionMapping::profile_trajectory_v1_with_values(
                    recipe.values().map(|value| value.field.as_str()),
                )
            ),
        );
        layer.encoding = encoding;
        view.revision += 1;
        self.save_view(&view)?;
        Ok(view)
    }
}

fn validate_profile_source(
    path: &Path,
    original_name: &str,
    recipe: &ProfileTrajectoryRecipeV1,
) -> Result<&'static str> {
    let table =
        wilddatum_query::read_source_table(path, original_name, MAX_PROFILE_TRAJECTORY_ROWS)?;
    if table.rows.is_empty() {
        return Err(WildDatumError::Invalid(
            "profile/trajectory source contains no data rows".into(),
        ));
    }
    let configured = [
        recipe.trajectory_id_field.as_str(),
        recipe.profile_id_field.as_str(),
        recipe.latitude_field.as_str(),
        recipe.longitude_field.as_str(),
        recipe.vertical.field.as_str(),
    ]
    .into_iter()
    .chain(recipe.time_field.as_deref())
    .chain(recipe.values().map(|value| value.field.as_str()))
    .chain(
        recipe
            .values()
            .filter_map(|value| value.qc_field.as_deref()),
    )
    .collect::<BTreeSet<_>>();
    for field in &configured {
        if field.is_empty() || !table.columns.iter().any(|header| header == *field) {
            return Err(WildDatumError::Invalid(format!(
                "profile/trajectory field {field:?} is absent from {original_name}"
            )));
        }
    }
    let mut numeric_fields = vec![
        (
            "latitude".to_owned(),
            recipe.latitude_field.as_str(),
            &[][..],
        ),
        (
            "longitude".to_owned(),
            recipe.longitude_field.as_str(),
            &[][..],
        ),
        (
            "vertical".to_owned(),
            recipe.vertical.field.as_str(),
            recipe.vertical.fill_values.as_slice(),
        ),
    ];
    numeric_fields.extend(recipe.values().map(|value| {
        (
            format!("value {}", value.field),
            value.field.as_str(),
            value.fill_values.as_slice(),
        )
    }));
    let mut found_numeric = vec![false; numeric_fields.len()];
    for row in &table.rows {
        for (slot, (_, field, fill_values)) in numeric_fields.iter().enumerate() {
            found_numeric[slot] |= row
                .get(*field)
                .and_then(|value| finite_source_number(value, fill_values))
                .is_some();
        }
        if found_numeric.iter().all(|found| *found) {
            break;
        }
    }
    let invalid = numeric_fields
        .iter()
        .zip(found_numeric)
        .filter_map(|((role, _, _), found)| (!found).then_some(role.as_str()))
        .collect::<Vec<_>>();
    if !invalid.is_empty() {
        return Err(WildDatumError::Invalid(format!(
            "profile/trajectory numeric fields have no finite values for: {}",
            invalid.join(", ")
        )));
    }
    Ok(table.identity)
}

fn finite_source_number(value: &Value, fill_values: &[String]) -> Option<f64> {
    let number = if let Some(number) = value.as_f64() {
        if fill_values
            .iter()
            .filter_map(|fill| fill.trim().parse::<f64>().ok())
            .any(|fill| fill == number)
        {
            return None;
        }
        number
    } else {
        let text = value.as_str()?.trim();
        if text.is_empty() || fill_values.iter().any(|fill| fill.trim() == text) {
            return None;
        }
        text.parse().ok()?
    };
    number.is_finite().then_some(number)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use wilddatum_core::{
        PINNED_RERUN_VERSION, ProfileValueSpec, VerticalAxisSpec, VerticalDirection,
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

    fn recipe() -> ProfileTrajectoryRecipeV1 {
        ProfileTrajectoryRecipeV1 {
            trajectory_id_field: "platform".into(),
            profile_id_field: "cycle".into(),
            time_field: Some("time".into()),
            latitude_field: "latitude".into(),
            longitude_field: "longitude".into(),
            vertical: VerticalAxisSpec {
                field: "pressure".into(),
                direction: VerticalDirection::PositiveDown,
                unit: Some("decibar".into()),
                fill_values: vec![],
            },
            value: ProfileValueSpec {
                field: "temperature".into(),
                unit: Some("degree_Celsius".into()),
                qc_field: Some("temperature_qc".into()),
                accepted_qc: vec!["1".into(), "2".into()],
                fill_values: vec![],
            },
            additional_values: vec![],
            vertical_range: None,
            max_points_per_profile: None,
        }
    }

    async fn valid_view(directory: &tempfile::TempDir, service: &WildDatumService) -> EcoViewSpec {
        let path = directory.path().join("profile.tsv");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "platform\tcycle\ttime\tlatitude\tlongitude\tpressure\ttemperature\ttemperature_qc"
        )
        .unwrap();
        writeln!(
            file,
            "FLOAT_1\t1\t2026-01-01T00:00:00Z\t34.8\t-75.5\t0\t01.00\t1"
        )
        .unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        service
            .create_view("profile".into(), vec![manifest.dataset_id])
            .unwrap()
    }

    #[tokio::test]
    async fn configure_profile_trajectory_writes_validated_service_mapping() {
        let (directory, service) = service();
        let view = valid_view(&directory, &service).await;
        let source_path = directory.path().join("profile.tsv");
        let source_before = std::fs::read_to_string(&source_path).unwrap();
        let configured = service
            .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", recipe())
            .unwrap();

        assert_eq!(configured.revision, 2);
        let encoding = &configured.layers[0].encoding;
        assert_eq!(encoding["view_kind"], "profile_trajectory_v1");
        assert_eq!(encoding["vertical"]["direction"], "positive_down");
        assert_eq!(encoding["vertical"]["unit"], "decibar");
        assert_eq!(encoding["value"]["unit"], "degree_Celsius");
        assert_eq!(encoding["selection_mapping"]["kind"], "source_row_index");
        assert_eq!(encoding["selection_mapping"]["stride"], 1);
        assert_eq!(
            encoding["selection_mapping"]["entity_suffixes"],
            json!(["map_observations", "profile_observations"])
        );
        assert_eq!(
            encoding["selection_mapping"]["rerun_version"],
            PINNED_RERUN_VERSION
        );
        assert_eq!(std::fs::read_to_string(source_path).unwrap(), source_before);
        assert!(matches!(
            service.configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", recipe()),
            Err(WildDatumError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn configure_profile_trajectory_validates_fields_numeric_data_and_qc() {
        let (directory, service) = service();
        let view = valid_view(&directory, &service).await;

        let mut missing = recipe();
        missing.latitude_field = "lat_missing".into();
        assert!(
            service
                .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", missing)
                .unwrap_err()
                .to_string()
                .contains("lat_missing")
        );
        let mut no_qc_field = recipe();
        no_qc_field.value.qc_field = None;
        assert!(
            service
                .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", no_qc_field)
                .unwrap_err()
                .to_string()
                .contains("accepted_qc requires")
        );
        let mut no_filter = recipe();
        no_filter.value.qc_field = None;
        no_filter.value.accepted_qc.clear();
        let configured = service
            .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", no_filter)
            .unwrap();
        assert_eq!(
            configured.layers[0].encoding["value"]["accepted_qc"],
            json!([])
        );
    }

    #[tokio::test]
    async fn configure_profile_trajectory_rejects_non_numeric_and_wrong_layer_targets() {
        let (directory, service) = service();
        let path = directory.path().join("bad.csv");
        std::fs::write(
            &path,
            "platform,cycle,time,latitude,longitude,pressure,temperature,temperature_qc\nFLOAT,1,t,north,-75,0,18,1\n",
        )
        .unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        let view = service
            .create_view("bad".into(), vec![manifest.dataset_id.clone()])
            .unwrap();
        assert!(
            service
                .configure_profile_trajectory_view(&view.view_id.0, 1, "missing", recipe())
                .unwrap_err()
                .to_string()
                .contains("layer missing")
        );
        assert!(
            service
                .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", recipe())
                .unwrap_err()
                .to_string()
                .contains("latitude")
        );

        let mut incompatible = service.get_manifest(&manifest.dataset_id.0).unwrap();
        incompatible.modalities = vec![Modality::Raster];
        service.save_manifest(&incompatible).unwrap();
        let incompatible_view = service
            .create_view("raster".into(), vec![manifest.dataset_id])
            .unwrap();
        assert!(
            service
                .configure_profile_trajectory_view(
                    &incompatible_view.view_id.0,
                    1,
                    "layer_1",
                    recipe()
                )
                .unwrap_err()
                .to_string()
                .contains("not tabular, time-series, or trajectory")
        );
    }

    #[tokio::test]
    async fn parquet_and_arrow_profiles_keep_exact_rows_across_multiple_values() {
        use std::{fs::File, sync::Arc};

        use arrow_array::{ArrayRef, Float64Array, RecordBatch, StringArray};
        use arrow_ipc::writer::FileWriter;
        use arrow_schema::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;
        use serde_json::json;
        use wilddatum_core::{NumericRange, SemanticSelection};

        let schema = Arc::new(Schema::new(vec![
            Field::new("platform", DataType::Utf8, false),
            Field::new("cycle", DataType::Int64, false),
            Field::new("time", DataType::Utf8, false),
            Field::new("latitude", DataType::Float64, false),
            Field::new("longitude", DataType::Float64, false),
            Field::new("pressure", DataType::Float64, false),
            Field::new("temperature", DataType::Float64, false),
            Field::new("temperature_qc", DataType::Utf8, false),
            Field::new("salinity", DataType::Float64, false),
            Field::new("salinity_qc", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["F1", "F1", "F1", "F1"])) as ArrayRef,
                Arc::new(arrow_array::Int64Array::from(vec![1, 1, 1, 1])) as ArrayRef,
                Arc::new(StringArray::from(vec!["t0", "t1", "t2", "t3"])) as ArrayRef,
                Arc::new(Float64Array::from(vec![34.0, 34.1, 34.2, 34.3])) as ArrayRef,
                Arc::new(Float64Array::from(vec![-75.0, -75.1, -75.2, -75.3])) as ArrayRef,
                Arc::new(Float64Array::from(vec![0.0, 10.0, 20.0, 30.0])) as ArrayRef,
                Arc::new(Float64Array::from(vec![18.0, 17.0, 16.0, 15.0])) as ArrayRef,
                Arc::new(StringArray::from(vec!["1", "1", "1", "1"])) as ArrayRef,
                Arc::new(Float64Array::from(vec![35.0, 35.1, 35.2, 35.3])) as ArrayRef,
                Arc::new(StringArray::from(vec!["1", "1", "2", "1"])) as ArrayRef,
            ],
        )
        .unwrap();

        for extension in ["parquet", "arrow"] {
            let (directory, service) = service();
            let path = directory.path().join(format!("profile.{extension}"));
            if extension == "parquet" {
                let mut writer =
                    ArrowWriter::try_new(File::create(&path).unwrap(), schema.clone(), None)
                        .unwrap();
                writer.write(&batch).unwrap();
                writer.close().unwrap();
            } else {
                let mut writer =
                    FileWriter::try_new(File::create(&path).unwrap(), &schema).unwrap();
                writer.write(&batch).unwrap();
                writer.finish().unwrap();
            }
            let manifest = service.import_local_file(&path).await.unwrap();
            let view = service
                .create_view(
                    "structured profile".into(),
                    vec![manifest.dataset_id.clone()],
                )
                .unwrap();
            let mut recipe = recipe();
            recipe.additional_values = vec![wilddatum_core::ProfileValueSpec {
                field: "salinity".into(),
                unit: Some("1e-3".into()),
                qc_field: Some("salinity_qc".into()),
                accepted_qc: vec!["1".into(), "2".into()],
                fill_values: vec![],
            }];
            recipe.vertical_range = Some(NumericRange {
                minimum: 5.0,
                maximum: 25.0,
            });
            recipe.max_points_per_profile = Some(2);
            let configured = service
                .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", recipe)
                .unwrap();
            let encoding = &configured.layers[0].encoding;
            assert_eq!(
                encoding["selection_mapping"]["entity_suffixes"][2],
                "profile_observations_salinity"
            );
            assert_eq!(
                encoding["source_row_identity"],
                if extension == "parquet" {
                    "parquet_physical_row_v1"
                } else {
                    "arrow_file_batch_row_v1"
                }
            );
            let selection = service
                .save_selection(
                    &configured.view_id.0,
                    SemanticSelection::Rows {
                        dataset_id: manifest.dataset_id,
                        predicate: json!({
                            "entity_path": format!(
                                "datasets/{}/layer_1/profile_observations_salinity",
                                configured.dataset_ids[0]
                            ),
                            "instance_id": 2,
                            "mapping_kind": "source_row_index",
                            "rerun_version": wilddatum_core::PINNED_RERUN_VERSION,
                        }),
                        row_count: 1,
                    },
                    json!({"source": "structured_profile_test"}),
                )
                .unwrap();
            let exact = service
                .query_selection(&selection.selection_id.0, None, 100)
                .await
                .unwrap();
            assert_eq!(exact.preview["rows"][0]["source_index"], 2);
            assert_eq!(exact.preview["rows"][0]["values"]["salinity"], 35.2);
        }
    }
}
