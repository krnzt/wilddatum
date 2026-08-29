use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    http::Response,
    routing::get,
};
use chrono::Utc;
use ecoscope_core::{DatasetPlan, DatasetRequest, ProviderKind, ResourceQuery};
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
        )
        .route(
            "/erddap/tabledap/ArgoFloats.csv",
            get(|| async {
                let chunks = futures::stream::iter([
                    Ok::<_, std::convert::Infallible>(Bytes::from_static(b"time,temp\n")),
                    Ok(Bytes::from_static(b"2025-01-01T00:00:00Z,12.5\n")),
                    Ok(Bytes::from_static(b"2025-01-01T01:00:00Z,12.8\n")),
                ]);
                Response::builder()
                    .header("content-type", "text/csv")
                    .header("etag", "fixture-etag")
                    .header("last-modified", "Thu, 28 Aug 2026 12:00:00 GMT")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }),
        )
        .route("/erddap/version", get(|| async { "2.28" }));
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

fn materialization_request() -> DatasetRequest {
    DatasetRequest {
        provider: ProviderKind::Other("fixture-erddap".into()),
        resource_id: "ArgoFloats".into(),
        locations: vec![],
        temporal_start: None,
        temporal_end: None,
        spatial_filter: None,
        variables: vec!["time".into(), "temp".into()],
        release: None,
        package: "basic".into(),
        include_provisional: false,
        provider_options: serde_json::from_value(serde_json::json!({
            "protocol": "tabledap",
            "output_format": "csv"
        }))
        .unwrap(),
    }
}

async fn approved_plan(provider: &ErddapProvider) -> DatasetPlan {
    let mut plan = provider
        .plan_dataset(materialization_request())
        .await
        .unwrap();
    plan.approved_at = Some(Utc::now());
    plan
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

#[tokio::test]
async fn materialize_streams_an_approved_subset_into_an_immutable_object() {
    const CSV: &[u8] = b"time,temp\n2025-01-01T00:00:00Z,12.5\n2025-01-01T01:00:00Z,12.8\n";

    let objects = tempfile::tempdir().unwrap();
    let provider = ErddapProvider::new(fixture_server().await)
        .unwrap()
        .with_object_dir(objects.path());
    let plan = approved_plan(&provider).await;
    assert!(!plan.requires_credentials);

    let manifest = provider.materialize(plan, None).await.unwrap();

    let digest = blake3::hash(CSV).to_hex().to_string();
    assert_eq!(manifest.source_files.len(), 1);
    assert_eq!(manifest.source_files[0].checksum.algorithm, "blake3");
    assert_eq!(manifest.source_files[0].checksum.value, digest);
    assert!(objects.path().join(&digest).is_file());
    assert_eq!(manifest.transformations[0].name, "erddap_subset");
    assert_eq!(manifest.transformations[0].version, "2.28");
    assert_eq!(manifest.license.as_ref().unwrap().name, "CC BY 4.0");
    assert_eq!(manifest.provider_metadata["response_etag"], "fixture-etag");
}

#[test]
fn rejects_non_loopback_http_origins() {
    let error = ErddapProvider::new(ErddapConfig {
        provider_id: "unsafe-erddap".into(),
        name: "Unsafe ERDDAP".into(),
        base_url: "http://example.test/erddap".into(),
        allowed_origin: "http://example.test".into(),
        homepage: "https://example.test/".into(),
        catalog_scope: None,
    })
    .err()
    .unwrap();

    assert!(error.to_string().contains("requires HTTPS"));
}

#[tokio::test]
async fn materialize_requires_approval_and_an_untampered_plan() {
    let objects = tempfile::tempdir().unwrap();
    let provider = ErddapProvider::new(fixture_server().await)
        .unwrap()
        .with_object_dir(objects.path());
    let plan = provider
        .plan_dataset(materialization_request())
        .await
        .unwrap();
    let error = provider.materialize(plan.clone(), None).await.unwrap_err();
    assert!(error.to_string().contains("must be approved"));

    let mut tampered = plan;
    tampered.approved_at = Some(Utc::now());
    tampered.files[0].name = "changed.csv".into();
    let error = provider.materialize(tampered, None).await.unwrap_err();
    assert!(error.to_string().contains("changed after"));
}

#[tokio::test]
async fn materialize_rejects_an_approved_url_from_another_origin() {
    let objects = tempfile::tempdir().unwrap();
    let provider = ErddapProvider::new(fixture_server().await)
        .unwrap()
        .with_object_dir(objects.path());
    let mut plan = provider
        .plan_dataset(materialization_request())
        .await
        .unwrap();
    plan.files[0].download_url =
        Some("https://example.test/erddap/tabledap/ArgoFloats.csv?time%2Ctemp".into());
    plan = plan.finalize().unwrap();
    plan.approved_at = Some(Utc::now());

    let error = provider.materialize(plan, None).await.unwrap_err();

    assert!(error.to_string().contains("origin is not allowed"));
    assert_eq!(std::fs::read_dir(objects.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn materialize_cleans_partial_files_when_cancelled_mid_stream() {
    let objects = tempfile::tempdir().unwrap();
    let provider = ErddapProvider::new(fixture_server().await)
        .unwrap()
        .with_object_dir(objects.path());
    let plan = approved_plan(&provider).await;
    let checks = AtomicUsize::new(0);

    let error = provider
        .materialize_with_control(
            plan,
            || checks.fetch_add(1, Ordering::SeqCst) >= 2,
            |_, _| {},
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("cancelled"));
    assert_eq!(std::fs::read_dir(objects.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn materialize_leaves_no_object_after_an_http_failure() {
    let objects = tempfile::tempdir().unwrap();
    let provider = ErddapProvider::new(fixture_server().await)
        .unwrap()
        .with_object_dir(objects.path());
    let mut plan = provider
        .plan_dataset(materialization_request())
        .await
        .unwrap();
    let original = plan.files[0].download_url.as_ref().unwrap();
    plan.files[0].download_url = Some(original.replace("ArgoFloats.csv", "Missing.csv"));
    plan = plan.finalize().unwrap();
    plan.approved_at = Some(Utc::now());

    let error = provider.materialize(plan, None).await.unwrap_err();

    assert!(error.to_string().contains("404"));
    assert_eq!(std::fs::read_dir(objects.path()).unwrap().count(), 0);
}
