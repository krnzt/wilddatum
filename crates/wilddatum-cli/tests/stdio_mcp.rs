use std::{process::Command, time::Duration};

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Value, json};

#[derive(Debug, Clone, Default)]
struct SmokeClient;

impl rmcp::ClientHandler for SmokeClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

#[tokio::test]
async fn released_process_completes_demo_selection_loop_over_stdio() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let data_dir = directory.path().join("data");
    let cache_dir = directory.path().join("cache");
    let executable = env!("CARGO_BIN_EXE_wilddatum");

    let demo_output = Command::new(executable)
        .env("WILDDATUM_DATA_DIR", &data_dir)
        .env("WILDDATUM_CACHE_DIR", &cache_dir)
        .args(["demo", "synthetic", "--no-open"])
        .output()?;
    anyhow::ensure!(
        demo_output.status.success(),
        "demo failed: {}",
        String::from_utf8_lossy(&demo_output.stderr)
    );
    let demo: Value = serde_json::from_slice(&demo_output.stdout)?;
    let view_id = demo["view_id"].as_str().expect("view ID");
    let point_dataset_id = demo["dataset_ids"][0].as_str().expect("point dataset ID");
    let cube_dataset_id = demo["dataset_ids"][1].as_str().expect("cube dataset ID");

    let inventory_output = Command::new(executable)
        .env("WILDDATUM_DATA_DIR", &data_dir)
        .env("WILDDATUM_CACHE_DIR", &cache_dir)
        .args(["inventory", cube_dataset_id])
        .output()?;
    anyhow::ensure!(inventory_output.status.success());
    let inventory: Value = serde_json::from_slice(&inventory_output.stdout)?;
    anyhow::ensure!(inventory["version"] == 1);
    anyhow::ensure!(
        !String::from_utf8_lossy(&inventory_output.stdout)
            .contains(directory.path().to_string_lossy().as_ref()),
        "scientific inventory exposed the private data directory"
    );

    let suggestion_output = Command::new(executable)
        .env("WILDDATUM_DATA_DIR", &data_dir)
        .env("WILDDATUM_CACHE_DIR", &cache_dir)
        .args(["suggest-views", point_dataset_id, cube_dataset_id])
        .output()?;
    anyhow::ensure!(suggestion_output.status.success());
    let suggestion: Value = serde_json::from_slice(&suggestion_output.stdout)?;
    anyhow::ensure!(
        suggestion["suggestions"]
            .as_array()
            .is_some_and(|suggestions| {
                suggestions
                    .iter()
                    .any(|candidate| candidate["recipe"] == "point_cloud_spectral_cube_v1")
            })
    );
    let suggestion_id = suggestion["suggestions"]
        .as_array()
        .and_then(|suggestions| {
            suggestions
                .iter()
                .find(|candidate| candidate["recipe"] == "point_cloud_spectral_cube_v1")
        })
        .and_then(|candidate| candidate["suggestion_id"].as_str())
        .expect("multimodal suggestion ID");
    let accepted_output = Command::new(executable)
        .env("WILDDATUM_DATA_DIR", &data_dir)
        .env("WILDDATUM_CACHE_DIR", &cache_dir)
        .args([
            "create-suggested-view",
            suggestion_id,
            "--name",
            "Accepted multimodal workspace",
            point_dataset_id,
            cube_dataset_id,
        ])
        .output()?;
    anyhow::ensure!(
        accepted_output.status.success(),
        "suggestion acceptance failed: {}",
        String::from_utf8_lossy(&accepted_output.stderr)
    );
    let accepted: Value = serde_json::from_slice(&accepted_output.stdout)?;
    anyhow::ensure!(accepted["version"] == 2);
    anyhow::ensure!(
        accepted["panels"]
            .as_array()
            .is_some_and(|panels| panels.len() == 3)
    );
    anyhow::ensure!(
        accepted["link_rules"]
            .as_array()
            .is_some_and(|links| links.iter().any(|link| {
                link["resolver"] == "world_to_raster_pixel" && link["exactness"] == "unavailable"
            }))
    );
    let accepted_recording = directory.path().join("accepted-multimodal.rrd");
    let render_output = Command::new(executable)
        .env("WILDDATUM_DATA_DIR", &data_dir)
        .env("WILDDATUM_CACHE_DIR", &cache_dir)
        .args([
            "render",
            accepted["view_id"].as_str().expect("accepted view ID"),
            "--output",
            accepted_recording.to_str().expect("recording path"),
        ])
        .output()?;
    anyhow::ensure!(
        render_output.status.success(),
        "accepted view render failed: {}",
        String::from_utf8_lossy(&render_output.stderr)
    );
    anyhow::ensure!(accepted_recording.metadata()?.len() > 0);

    let transport = TokioChildProcess::new(tokio::process::Command::new(executable).configure(
        |command| {
            command
                .env("WILDDATUM_DATA_DIR", &data_dir)
                .env("WILDDATUM_CACHE_DIR", &cache_dir)
                .env("NEON_API_TOKEN", "stdio-smoke-test-token")
                .arg("mcp");
        },
    ))?;
    let mut client = SmokeClient.serve(transport).await?;
    let tools = client.list_all_tools().await?;
    anyhow::ensure!(tools.iter().any(|tool| tool.name == "inspect_view"));
    anyhow::ensure!(
        tools
            .iter()
            .any(|tool| tool.name == "inspect_scientific_inventory")
    );
    anyhow::ensure!(tools.iter().any(|tool| tool.name == "suggest_views"));
    anyhow::ensure!(
        tools
            .iter()
            .any(|tool| tool.name == "create_view_from_suggestion")
    );
    let plan_tool = tools
        .iter()
        .find(|tool| tool.name == "plan_materialization")
        .expect("plan_materialization tool");
    let properties = plan_tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("plan_materialization input properties");
    for field in [
        "resource_id",
        "variables",
        "spatial_filter",
        "provider_options",
    ] {
        anyhow::ensure!(
            properties.contains_key(field),
            "missing {field} input field"
        );
    }
    anyhow::ensure!(!properties.contains_key("selection"));

    let health = client
        .call_tool(CallToolRequestParams::new("health"))
        .await?;
    anyhow::ensure!(health.is_error != Some(true));
    let listed = client
        .call_tool(CallToolRequestParams::new("list_providers"))
        .await?;
    let providers = listed
        .structured_content
        .as_ref()
        .and_then(|value| value.get("providers"))
        .and_then(Value::as_array)
        .expect("provider list");
    for provider_id in ["emso", "icos-erddap", "euro-argo"] {
        let provider = providers
            .iter()
            .find(|provider| provider["provider_id"] == provider_id)
            .unwrap_or_else(|| panic!("missing built-in provider {provider_id}"));
        for capability in [
            "catalog_search",
            "resource_resolve",
            "asset_plan",
            "asset_fetch",
            "citation_resolve",
            "policy_evaluate",
        ] {
            anyhow::ensure!(
                provider["capabilities"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item == capability)),
                "{provider_id} is missing {capability}"
            );
        }
    }
    let inspected = client
        .call_tool(
            CallToolRequestParams::new("inspect_view")
                .with_arguments(json!({"view_id": view_id}).as_object().unwrap().clone()),
        )
        .await?;
    anyhow::ensure!(inspected.is_error != Some(true));

    let inventory = client
        .call_tool(
            CallToolRequestParams::new("inspect_scientific_inventory").with_arguments(
                json!({"dataset_id": cube_dataset_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    anyhow::ensure!(inventory.is_error != Some(true));
    anyhow::ensure!(
        inventory
            .structured_content
            .as_ref()
            .and_then(|value| value.get("components"))
            .and_then(Value::as_array)
            .is_some_and(|components| !components.is_empty())
    );

    let suggestions = client
        .call_tool(
            CallToolRequestParams::new("suggest_views").with_arguments(
                json!({"dataset_ids": demo["dataset_ids"]})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    anyhow::ensure!(suggestions.is_error != Some(true));
    anyhow::ensure!(
        suggestions
            .structured_content
            .as_ref()
            .and_then(|value| value.get("suggestions"))
            .and_then(Value::as_array)
            .is_some_and(|suggestions| {
                suggestions
                    .iter()
                    .any(|suggestion| suggestion["recipe"] == "point_cloud_spectral_cube_v1")
            })
    );

    let recorded = client
        .call_tool(
            CallToolRequestParams::new("record_selection").with_arguments(
                json!({
                    "view_id": view_id,
                    "selection": {
                        "type": "cube_pixel",
                        "dataset_id": cube_dataset_id,
                        "array_path": "/WildDatum/Reflectance",
                        "x": 16,
                        "y": 16,
                        "x_axis": 1,
                        "y_axis": 0,
                        "spectral_axis": 2,
                        "displayed_bands": [5, 3, 1]
                    },
                    "summary": {"source": "release_stdio_smoke"}
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;
    anyhow::ensure!(recorded.is_error != Some(true));
    let selection_id = recorded
        .structured_content
        .as_ref()
        .and_then(|value| value.get("selection_id"))
        .and_then(Value::as_str)
        .expect("selection ID");
    let queried = client
        .call_tool(
            CallToolRequestParams::new("query_selection").with_arguments(
                json!({
                    "selection_id": selection_id,
                    "dataset_id": cube_dataset_id,
                    "point_limit": 1000
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await?;
    anyhow::ensure!(queried.is_error != Some(true));
    anyhow::ensure!(
        queried
            .structured_content
            .as_ref()
            .and_then(|value| value.get("row_count"))
            .and_then(Value::as_u64)
            == Some(16)
    );

    let shutdown = client.close_with_timeout(Duration::from_secs(15)).await?;
    anyhow::ensure!(shutdown.is_some(), "MCP child did not shut down within 15s");
    Ok(())
}
