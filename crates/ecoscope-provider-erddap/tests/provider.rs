use std::collections::BTreeMap;

use axum::{Json, Router, routing::get};
use ecoscope_core::{DatasetRequest, ProviderKind, ResourceQuery};
use ecoscope_provider_api::EcologicalDataProvider;
use ecoscope_provider_erddap::{ErddapProvider, config::ErddapConfig};
use serde_json::Value;

async fn fixture_server() -> ErddapConfig {
    let search: Value = serde_json::from_str(include_str!("fixtures/search.json")).unwrap();
    let info: Value = serde_json::from_str(include_str!("fixtures/info.json")).unwrap();
    let app = Router::new()
        .route(
            "/erddap/search/index.json",
            get(move || {
                let search = search.clone();
                async move { Json(search) }
            }),
        )
        .route(
            "/erddap/info/ArgoFloats/index.json",
            get(move || {
                let info = info.clone();
                async move { Json(info) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    ErddapConfig {
        provider_id: "fixture-erddap".into(),
        name: "Fixture ERDDAP".into(),
        base_url: format!("http://{address}/erddap"),
        allowed_origin: format!("http://{address}"),
        homepage: "https://example.test/".into(),
        catalog_scope: None,
    }
}

#[tokio::test]
async fn catalog_search_and_resolution_are_provider_neutral() {
    let provider = ErddapProvider::new(fixture_server().await).unwrap();
    let resources = provider
        .search_resources(ResourceQuery {
            text: "temperature".into(),
            kinds: vec![],
            modalities: vec![],
            spatial_filter: None,
            temporal_start: None,
            temporal_end: None,
            provider_filters: BTreeMap::new(),
            limit: 10,
        })
        .await
        .unwrap();

    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].resource_id, "ArgoFloats");
    assert_eq!(resources[0].provider_id, "fixture-erddap");

    let resource = provider.resolve_resource("ArgoFloats").await.unwrap();
    assert_eq!(
        resource.provider_extensions["cdm_data_type"],
        "TrajectoryProfile"
    );
    assert_eq!(
        resource.provider_extensions["variables"]["temp"]["attributes"]["units"],
        "degree_C"
    );
    assert_eq!(
        resource.temporal_start.as_deref(),
        Some("2025-01-01T00:00:00Z")
    );
}

#[tokio::test]
async fn catalog_metadata_limit_is_enforced_before_deserialization() {
    let app = Router::new().route(
        "/erddap/search/index.json",
        get(|| async { "x".repeat(128) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = ErddapProvider::new(ErddapConfig {
        provider_id: "bounded-erddap".into(),
        name: "Bounded ERDDAP".into(),
        base_url: format!("http://{address}/erddap"),
        allowed_origin: format!("http://{address}"),
        homepage: "https://example.test/".into(),
        catalog_scope: None,
    })
    .unwrap()
    .with_metadata_limit_bytes(32);

    let error = provider
        .search_resources(ResourceQuery {
            text: String::new(),
            kinds: vec![],
            modalities: vec![],
            spatial_filter: None,
            temporal_start: None,
            temporal_end: None,
            provider_filters: BTreeMap::new(),
            limit: 10,
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("32 byte limit"));
}

#[tokio::test]
async fn plan_builds_a_validated_public_subset() {
    let provider = ErddapProvider::new(fixture_server().await).unwrap();
    let provider_options = serde_json::from_value(serde_json::json!({
        "protocol": "tabledap",
        "output_format": "csv",
        "constraints": [
            {"variable": "time", "op": "gte", "value": "2025-01-01T00:00:00Z"},
            {"variable": "time", "op": "lte", "value": "2025-01-02T00:00:00Z"}
        ]
    }))
    .unwrap();
    let plan = provider
        .plan_dataset(DatasetRequest {
            provider: ProviderKind::Other("fixture-erddap".into()),
            resource_id: "ArgoFloats".into(),
            locations: vec![],
            temporal_start: None,
            temporal_end: None,
            spatial_filter: None,
            variables: vec![
                "time".into(),
                "latitude".into(),
                "longitude".into(),
                "temp".into(),
            ],
            release: None,
            package: "basic".into(),
            include_provisional: false,
            provider_options,
        })
        .await
        .unwrap();

    assert_eq!(plan.file_count, 1);
    assert!(!plan.requires_credentials);
    assert!(!plan.plan_hash.is_empty());
    let file = &plan.files[0];
    assert_eq!(file.provider_id, "fixture-erddap");
    assert_eq!(file.name, "ArgoFloats.csv");
    assert_eq!(
        file.metadata["decoded_query"],
        "time,latitude,longitude,temp&time>=2025-01-01T00:00:00Z&time<=2025-01-02T00:00:00Z"
    );
    assert!(
        file.download_url
            .as_deref()
            .unwrap()
            .contains("/erddap/tabledap/ArgoFloats.csv?")
    );
}

#[tokio::test]
async fn plan_rejects_unknown_variables() {
    let provider = ErddapProvider::new(fixture_server().await).unwrap();
    let error = provider
        .plan_dataset(DatasetRequest {
            provider: ProviderKind::Other("fixture-erddap".into()),
            resource_id: "ArgoFloats".into(),
            locations: vec![],
            temporal_start: None,
            temporal_end: None,
            spatial_filter: None,
            variables: vec!["not_a_variable".into()],
            release: None,
            package: "basic".into(),
            include_provisional: false,
            provider_options: serde_json::from_value(serde_json::json!({
                "protocol": "tabledap"
            }))
            .unwrap(),
        })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("unknown ERDDAP variable"));
}
