use std::io::{BufRead, Write};

use chrono::Utc;
use ecoscope_core::{
    DatasetId, DatasetManifest, DatasetPlan, DatasetRequest, PlanId, ProviderCapability,
    ProviderKind, ProviderManifest, ProviderStatus, ResourceKind, ResourceRecord,
};
use serde_json::{Value, json};

fn main() {
    let input = std::io::stdin();
    let mut output = std::io::stdout().lock();
    for line in input.lock().lines() {
        let Ok(line) = line else { break };
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let id = request["id"].clone();
        let method = request["method"].as_str().unwrap_or_default();
        let params = request["params"].clone();
        let result = match method {
            "provider.handshake" => serde_json::to_value(manifest()).unwrap(),
            "resources.search" => json!([resource("fixture-dataset")]),
            "resources.resolve" => {
                json!(resource(params["id"].as_str().unwrap_or("fixture-dataset")))
            }
            "catalog.search" => json!([]),
            "catalog.inspect" => Value::Null,
            "datasets.plan" => {
                let request: DatasetRequest = serde_json::from_value(params).unwrap();
                serde_json::to_value(
                    DatasetPlan {
                        plan_id: PlanId::new(),
                        request,
                        plan_hash: String::new(),
                        file_count: 0,
                        estimated_bytes: Some(0),
                        files: vec![],
                        warnings: vec!["fixture plan".into()],
                        requires_credentials: false,
                        created_at: Utc::now(),
                        approved_at: None,
                    }
                    .finalize()
                    .unwrap(),
                )
                .unwrap()
            }
            "datasets.materialize" => {
                let plan: DatasetPlan = serde_json::from_value(params["plan"].clone()).unwrap();
                let request = plan.request;
                serde_json::to_value(DatasetManifest {
                    dataset_id: DatasetId::new(),
                    provider: request.provider,
                    resource_id: request.resource_id,
                    resource_version: None,
                    modalities: vec![],
                    locations: request.locations,
                    temporal_start: request.temporal_start,
                    temporal_end: request.temporal_end,
                    release: request.release,
                    package: Some(request.package),
                    include_provisional: request.include_provisional,
                    source_files: vec![],
                    transformations: vec![],
                    format: None,
                    spatial_reference: None,
                    cube: None,
                    cubes: vec![],
                    license: None,
                    citation: None,
                    provider_metadata: request.provider_options,
                    created_at: Utc::now(),
                })
                .unwrap()
            }
            _ => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": "method not found"}
                });
                writeln!(output, "{response}").unwrap();
                output.flush().unwrap();
                continue;
            }
        };
        let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
        writeln!(output, "{response}").unwrap();
        output.flush().unwrap();
    }
}

fn manifest() -> ProviderManifest {
    ProviderManifest {
        schema_version: 2,
        provider_id: "fixture".into(),
        name: "EcoScope conformance fixture".into(),
        version: "0.1.0".into(),
        status: ProviderStatus::Community,
        capabilities: vec![
            ProviderCapability::CatalogSearch,
            ProviderCapability::ResourceResolve,
            ProviderCapability::AssetPlan,
            ProviderCapability::AssetFetch,
        ],
        allowed_network_origins: vec!["https://example.org".into()],
        authentication: vec![],
        standards: vec!["EcoScope provider protocol v2".into()],
        homepage: None,
        support_url: None,
    }
}

fn resource(id: &str) -> ResourceRecord {
    ResourceRecord {
        provider_id: "fixture".into(),
        resource_id: id.into(),
        kind: ResourceKind::DatasetVersion,
        name: "Fixture ecological dataset".into(),
        description: Some("Language-neutral subprocess conformance data".into()),
        modalities: vec![],
        spatial_extent: None,
        temporal_start: None,
        temporal_end: None,
        relations: vec![],
        identifiers: Default::default(),
        vocabulary_terms: Default::default(),
        provider_extensions: Default::default(),
        raw_metadata: Some(
            json!({"fixture": true, "provider": ProviderKind::Other("fixture".into())}),
        ),
    }
}
