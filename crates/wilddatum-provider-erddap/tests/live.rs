use std::collections::BTreeMap;

use chrono::Utc;
use wilddatum_core::{DatasetRequest, ProviderKind, ResourceQuery, Result, WildDatumError};
use wilddatum_provider_api::EcologicalDataProvider;
use wilddatum_provider_erddap::{ErddapProvider, config};

fn resource_query(text: &str) -> ResourceQuery {
    ResourceQuery {
        text: text.into(),
        kinds: vec![],
        modalities: vec![],
        spatial_filter: None,
        temporal_start: None,
        temporal_end: None,
        provider_filters: BTreeMap::new(),
        limit: 5,
    }
}

fn live_provider(provider_id: &str) -> Result<ErddapProvider> {
    let preset =
        config::preset(provider_id).ok_or_else(|| WildDatumError::NotFound(provider_id.into()))?;
    Ok(ErddapProvider::new(preset.into())?.with_object_dir(
        std::env::temp_dir()
            .join("wilddatum-live-erddap")
            .join(provider_id),
    ))
}

#[tokio::test]
#[ignore = "public ERDDAP drift check; requires network"]
async fn public_presets_search_and_inspect() {
    for (provider_id, query) in [
        ("emso", "temperature"),
        ("icos-erddap", "trajectory"),
        ("euro-argo", "ArgoFloats"),
    ] {
        let provider = live_provider(provider_id).unwrap();
        let resources = provider
            .search_resources(resource_query(query))
            .await
            .unwrap();
        assert!(!resources.is_empty(), "{provider_id} returned no resources");
        provider
            .resolve_resource(&resources[0].resource_id)
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "public EMSO federation check; requires network"]
async fn emso_federated_subset_plans_and_materializes() {
    let objects = tempfile::tempdir().unwrap();
    let provider = ErddapProvider::new(config::preset("emso").unwrap().into())
        .unwrap()
        .with_object_dir(objects.path());
    let mut plan = provider
        .plan_dataset(DatasetRequest {
            provider: ProviderKind::Other("emso".into()),
            resource_id: "OBSEA_seabed_station_TS_L1c".into(),
            locations: vec![],
            temporal_start: Some("2025-01-01T00:00:00Z".into()),
            temporal_end: Some("2025-01-01T01:00:00Z".into()),
            spatial_filter: None,
            variables: vec!["time".into(), "TEMP".into()],
            release: None,
            package: "basic".into(),
            include_provisional: false,
            provider_options: serde_json::from_value(serde_json::json!({
                "protocol": "tabledap",
                "output_format": "csv"
            }))
            .unwrap(),
        })
        .await
        .unwrap();
    assert!(
        plan.files[0].metadata["redirect_chain"]
            .as_array()
            .is_some_and(|chain| chain.len() >= 2)
    );
    plan.approved_at = Some(Utc::now());

    let manifest = provider.materialize(plan, None).await.unwrap();

    assert_eq!(manifest.source_files.len(), 1);
    assert!(manifest.source_files[0].size_bytes > 0);
    assert!(manifest.source_files[0].source_uri.starts_with("https://"));
    assert!(
        objects
            .path()
            .join(&manifest.source_files[0].checksum.value)
            .is_file()
    );
}
