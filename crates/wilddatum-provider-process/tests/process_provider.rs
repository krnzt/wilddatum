use wilddatum_core::{DatasetRequest, ProviderKind, ResourceQuery};
use wilddatum_provider_api::EcologicalDataProvider;
use wilddatum_provider_process::{ProcessProvider, ProcessProviderConfig};

#[tokio::test]
async fn negotiates_and_calls_a_language_neutral_provider_process() {
    let provider = ProcessProvider::spawn(ProcessProviderConfig {
        schema_version: 1,
        protocol_version: 2,
        expected_provider_id: "fixture".into(),
        command: std::path::PathBuf::from(env!("CARGO_BIN_EXE_wilddatum-provider-fixture")),
        args: vec![],
        timeout_ms: 5_000,
        response_limit_bytes: 1_048_576,
    })
    .await
    .unwrap();
    assert_eq!(provider.provider_id(), "fixture");
    assert_eq!(provider.manifest().schema_version, 2);
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
            resource_id: "fixture-dataset".into(),
            locations: vec![],
            temporal_start: None,
            temporal_end: None,
            spatial_filter: None,
            variables: vec![],
            release: None,
            package: "basic".into(),
            include_provisional: false,
            provider_options: Default::default(),
        })
        .await
        .unwrap();
    assert!(!plan.plan_hash.is_empty());
    let dataset = provider.materialize(plan, None).await.unwrap();
    assert_eq!(dataset.provider, ProviderKind::Other("fixture".into()));
    assert_eq!(dataset.resource_id, "fixture-dataset");
}
