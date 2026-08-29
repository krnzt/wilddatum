//! Validation and persistence for the linked trajectory/profile recipe.

use std::{collections::BTreeSet, path::Path};

use ecoscope_core::{
    EcoScopeError, EcoViewSpec, Modality, ProfileTrajectoryRecipeV1, Result,
    SourceRowSelectionMapping,
};
use serde_json::json;

use super::EcoScopeService;

impl EcoScopeService {
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
            return Err(EcoScopeError::Conflict(format!(
                "view revision is {}, expected {expected_revision}",
                view.revision
            )));
        }
        let layer = view
            .layers
            .iter_mut()
            .find(|layer| layer.id == layer_id)
            .ok_or_else(|| EcoScopeError::NotFound(format!("layer {layer_id}")))?;
        if !matches!(layer.modality, Modality::Tabular | Modality::TimeSeries) {
            return Err(EcoScopeError::Invalid(format!(
                "layer {layer_id} is not tabular or time-series data"
            )));
        }
        if !recipe.value.accepted_qc.is_empty() && recipe.value.qc_field.is_none() {
            return Err(EcoScopeError::Invalid(
                "accepted_qc requires value.qc_field".into(),
            ));
        }

        let manifest = self.get_manifest(&layer.dataset_id.0)?;
        let source = manifest
            .source_files
            .first()
            .ok_or_else(|| EcoScopeError::Invalid("dataset has no source files".into()))?;
        let path = self.source_path_for_renderer(&manifest, source)?;
        validate_delimited_recipe(&path, &source.original_name, &recipe)?;

        let mut encoding = recipe.encoding();
        encoding.insert(
            "selection_mapping".into(),
            json!(SourceRowSelectionMapping::profile_trajectory_v1()),
        );
        layer.encoding = encoding;
        view.revision += 1;
        self.save_view(&view)?;
        Ok(view)
    }
}

fn validate_delimited_recipe(
    path: &Path,
    original_name: &str,
    recipe: &ProfileTrajectoryRecipeV1,
) -> Result<()> {
    let extension = Path::new(original_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let delimiter = match extension.as_str() {
        "csv" => b',',
        "tsv" => b'\t',
        _ => {
            return Err(EcoScopeError::Invalid(format!(
                "profile/trajectory views currently support CSV and TSV sources; got {extension}"
            )));
        }
    };
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(false)
        .from_path(path)
        .map_err(csv_error)?;
    let headers = reader.headers().map_err(csv_error)?.clone();
    let configured = [
        recipe.trajectory_id_field.as_str(),
        recipe.profile_id_field.as_str(),
        recipe.latitude_field.as_str(),
        recipe.longitude_field.as_str(),
        recipe.vertical.field.as_str(),
        recipe.value.field.as_str(),
    ]
    .into_iter()
    .chain(recipe.time_field.as_deref())
    .chain(recipe.value.qc_field.as_deref())
    .collect::<BTreeSet<_>>();
    for field in &configured {
        if field.is_empty() || !headers.iter().any(|header| header == *field) {
            return Err(EcoScopeError::Invalid(format!(
                "profile/trajectory field {field:?} is absent from {original_name}"
            )));
        }
    }
    let position = |field: &str| {
        headers
            .iter()
            .position(|header| header == field)
            .expect("configured header validated above")
    };
    let numeric_fields = [
        ("latitude", position(&recipe.latitude_field), &[][..]),
        ("longitude", position(&recipe.longitude_field), &[][..]),
        (
            "vertical",
            position(&recipe.vertical.field),
            recipe.vertical.fill_values.as_slice(),
        ),
        (
            "value",
            position(&recipe.value.field),
            recipe.value.fill_values.as_slice(),
        ),
    ];
    let mut found_numeric = [false; 4];
    for record in reader.records() {
        let record = record.map_err(csv_error)?;
        for (slot, (_, column, fill_values)) in numeric_fields.iter().enumerate() {
            found_numeric[slot] |= record
                .get(*column)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .filter(|value| !fill_values.iter().any(|fill| fill.trim() == *value))
                .and_then(|value| value.parse::<f64>().ok())
                .is_some_and(f64::is_finite);
        }
        if found_numeric.into_iter().all(|found| found) {
            break;
        }
    }
    let invalid = numeric_fields
        .iter()
        .zip(found_numeric)
        .filter_map(|((role, _, _), found)| (!found).then_some(*role))
        .collect::<Vec<_>>();
    if !invalid.is_empty() {
        return Err(EcoScopeError::Invalid(format!(
            "profile/trajectory numeric fields have no finite values for: {}",
            invalid.join(", ")
        )));
    }
    Ok(())
}

fn csv_error(error: csv::Error) -> EcoScopeError {
    EcoScopeError::Invalid(format!(
        "cannot validate profile/trajectory source: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use ecoscope_core::{
        PINNED_RERUN_VERSION, ProfileValueSpec, VerticalAxisSpec, VerticalDirection,
    };

    use super::*;
    use crate::ServicePaths;

    fn service() -> (tempfile::TempDir, EcoScopeService) {
        let directory = tempfile::tempdir().unwrap();
        let service = EcoScopeService::open(ServicePaths::under(
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
        }
    }

    async fn valid_view(directory: &tempfile::TempDir, service: &EcoScopeService) -> EcoViewSpec {
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
            Err(EcoScopeError::Conflict(_))
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
                .contains("not tabular or time-series")
        );
    }
}
