use ecoscope_core::{DatasetRequest, ProviderKind, ResourceQuery};
use ecoscope_provider_api::EcologicalDataProvider;
use ecoscope_provider_process::{ProcessProvider, ProcessProviderConfig};

#[tokio::test]
async fn negotiates_and_calls_a_language_neutral_provider_process() {
    let provider = ProcessProvider::spawn(ProcessProviderConfig {
        schema_version: 1,
        expected_provider_id: "fixture".into(),
        command: std::path::PathBuf::from(env!("CARGO_BIN_EXE_ecoscope-provider-fixture")),
        args: vec![],
        timeout_ms: 5_000,
        response_limit_bytes: 1_048_576,
    })
    .await
    .unwrap();
    assert_eq!(provider.provider_id(), "fixture");
    let resources = provider
        .search_resources(ResourceQuery {
            text: "fixture".into(),
            kinds: vec![],
            modalities: vec![],
            spatial_filter: None,
            temporal_start: None,
            temporal_end: None,
            provider_filters: Default::default(),
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(resources[0].resource_id, "fixture-dataset");
    let plan = provider
        .plan_dataset(DatasetRequest {
            provider: ProviderKind::Other("fixture".into()),
            product_code: "fixture-dataset".into(),
            sites: vec![],
            start_month: None,
            end_month: None,
            release: None,
            package: "basic".into(),
            include_provisional: false,
        })
        .await
        .unwrap();
    assert!(!plan.plan_hash.is_empty());
    let dataset = provider.materialize(plan, None).await.unwrap();
    assert_eq!(dataset.provider, ProviderKind::Other("fixture".into()));
    assert_eq!(dataset.product_code, "fixture-dataset");
}
