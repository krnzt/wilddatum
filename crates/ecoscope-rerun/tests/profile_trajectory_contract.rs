use std::{collections::BTreeMap, path::Path};

use ecoscope_core::DatasetId;
use ecoscope_service::{EcoScopeService, ServicePaths};
use serde_json::{Value, json};

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/profile_trajectory.csv")
}

fn encoding() -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("view_kind".into(), json!("profile_trajectory_v1")),
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
        (
            "selection_mapping".into(),
            json!({
                "kind": "source_row_index",
                "entity_suffixes": ["map_observations", "profile_observations"],
                "stride": 1,
                "rerun_version": ecoscope_core::PINNED_RERUN_VERSION
            }),
        ),
    ])
}

#[tokio::test]
async fn writes_native_map_and_profile_recording() {
    let directory = tempfile::tempdir().unwrap();
    let service = EcoScopeService::open(ServicePaths::under(
        directory.path().join("data"),
        directory.path().join("cache"),
    ))
    .unwrap();
    let manifest = service.import_local_file(&fixture()).await.unwrap();
    let mut view = service
        .create_view(
            "Synthetic profile and trajectory".into(),
            vec![DatasetId(manifest.dataset_id.0.clone())],
        )
        .unwrap();
    view.layers[0].encoding = encoding();
    view.revision += 1;
    service.save_view(&view).unwrap();

    let recording = directory.path().join("profile-trajectory.rrd");
    ecoscope_rerun::write_recording(&service, &view.view_id.0, &recording).unwrap();
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
