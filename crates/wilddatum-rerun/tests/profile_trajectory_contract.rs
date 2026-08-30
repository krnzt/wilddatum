use std::path::Path;

use serde_json::json;
use wilddatum_core::{
    DatasetId, NumericRange, ProfileTrajectoryRecipeV1, ProfileValueSpec, VerticalAxisSpec,
    VerticalDirection,
};
use wilddatum_service::{ServicePaths, WildDatumService};

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/profile_trajectory.csv")
}

fn recipe() -> ProfileTrajectoryRecipeV1 {
    ProfileTrajectoryRecipeV1 {
        trajectory_id_field: "platform_number".into(),
        profile_id_field: "cycle_number".into(),
        time_field: Some("time".into()),
        latitude_field: "latitude".into(),
        longitude_field: "longitude".into(),
        vertical: VerticalAxisSpec {
            field: "pres".into(),
            direction: VerticalDirection::PositiveDown,
            unit: Some("decibar".into()),
            fill_values: vec![],
        },
        value: ProfileValueSpec {
            field: "temp_adjusted".into(),
            unit: Some("degree_Celsius".into()),
            qc_field: Some("temp_adjusted_qc".into()),
            accepted_qc: vec!["1".into(), "2".into()],
            fill_values: vec![],
        },
        additional_values: vec![],
        vertical_range: None,
        max_points_per_profile: None,
    }
}

#[tokio::test]
async fn writes_native_map_and_profile_recording() {
    let directory = tempfile::tempdir().unwrap();
    let service = WildDatumService::open(ServicePaths::under(
        directory.path().join("data"),
        directory.path().join("cache"),
    ))
    .unwrap();
    let manifest = service.import_local_file(&fixture()).await.unwrap();
    let view = service
        .create_view(
            "Synthetic profile and trajectory".into(),
            vec![DatasetId(manifest.dataset_id.0.clone())],
        )
        .unwrap();
    let view = service
        .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", recipe())
        .unwrap();

    let recording = directory.path().join("profile-trajectory.rrd");
    wilddatum_rerun::write_recording(&service, &view.view_id.0, &recording).unwrap();
    let bytes = std::fs::read(&recording).unwrap();
    assert!(bytes.len() > 5_000, "recording should contain both panels");
    for entity in [
        b"map_observations".as_slice(),
        b"trajectory_lines".as_slice(),
        b"profile_observations".as_slice(),
        b"profile_lines".as_slice(),
        b"profile_trajectory_info".as_slice(),
    ] {
        assert!(
            bytes.windows(entity.len()).any(|window| window == entity),
            "recording should contain {}",
            String::from_utf8_lossy(entity)
        );
    }
}

#[tokio::test]
async fn invalid_recipe_logs_an_adapter_notice_instead_of_crashing() {
    let directory = tempfile::tempdir().unwrap();
    let service = WildDatumService::open(ServicePaths::under(
        directory.path().join("data"),
        directory.path().join("cache"),
    ))
    .unwrap();
    let manifest = service.import_local_file(&fixture()).await.unwrap();
    let view = service
        .create_view("Invalid recipe".into(), vec![manifest.dataset_id])
        .unwrap();
    let configured = service
        .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", recipe())
        .unwrap();
    let mut invalid = configured.clone();
    invalid.layers[0].encoding.get_mut("vertical").unwrap()["direction"] = json!("sideways");
    invalid.revision += 1;
    service.save_view(&invalid).unwrap();

    let recording = directory.path().join("invalid-profile-trajectory.rrd");
    wilddatum_rerun::write_recording(&service, &invalid.view_id.0, &recording).unwrap();
    let bytes = std::fs::read(recording).unwrap();
    assert!(
        bytes
            .windows(b"adapter_notice".len())
            .any(|window| window == b"adapter_notice")
    );
    assert!(
        bytes
            .windows(b"sideways".len())
            .any(|window| window == b"sideways")
    );
}

#[tokio::test]
async fn writes_multiple_profile_panels_with_range_and_sampling_contracts() {
    let directory = tempfile::tempdir().unwrap();
    let service = WildDatumService::open(ServicePaths::under(
        directory.path().join("data"),
        directory.path().join("cache"),
    ))
    .unwrap();
    let manifest = service.import_local_file(&fixture()).await.unwrap();
    let view = service
        .create_view("Multi-value profiles".into(), vec![manifest.dataset_id])
        .unwrap();
    let mut recipe = recipe();
    recipe.additional_values = vec![ProfileValueSpec {
        field: "psal_adjusted".into(),
        unit: Some("1e-3".into()),
        qc_field: Some("psal_adjusted_qc".into()),
        accepted_qc: vec!["1".into(), "2".into()],
        fill_values: vec![],
    }];
    recipe.vertical_range = Some(NumericRange {
        minimum: 10.0,
        maximum: 100.0,
    });
    recipe.max_points_per_profile = Some(3);
    let view = service
        .configure_profile_trajectory_view(&view.view_id.0, 1, "layer_1", recipe)
        .unwrap();
    let recording = directory.path().join("multi-profile.rrd");
    wilddatum_rerun::write_recording(&service, &view.view_id.0, &recording).unwrap();
    let bytes = std::fs::read(recording).unwrap();
    for entity in [
        b"profile_observations_psal_adjusted".as_slice(),
        b"profile_lines_psal_adjusted".as_slice(),
    ] {
        assert!(
            bytes.windows(entity.len()).any(|window| window == entity),
            "recording should contain {}",
            String::from_utf8_lossy(entity)
        );
    }
}
