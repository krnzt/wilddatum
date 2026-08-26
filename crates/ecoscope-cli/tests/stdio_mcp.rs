use std::process::Command;

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
    let executable = env!("CARGO_BIN_EXE_ecoscope");

    let demo_output = Command::new(executable)
        .env("ECOSCOPE_DATA_DIR", &data_dir)
        .env("ECOSCOPE_CACHE_DIR", &cache_dir)
        .args(["demo", "synthetic", "--no-open"])
        .output()?;
    anyhow::ensure!(
        demo_output.status.success(),
        "demo failed: {}",
        String::from_utf8_lossy(&demo_output.stderr)
    );
    let demo: Value = serde_json::from_slice(&demo_output.stdout)?;
    let view_id = demo["view_id"].as_str().expect("view ID");
    let cube_dataset_id = demo["dataset_ids"][1].as_str().expect("cube dataset ID");

    let transport = TokioChildProcess::new(tokio::process::Command::new(executable).configure(
        |command| {
            command
                .env("ECOSCOPE_DATA_DIR", &data_dir)
                .env("ECOSCOPE_CACHE_DIR", &cache_dir)
                .arg("mcp");
        },
    ))?;
    let client = SmokeClient.serve(transport).await?;
    let tools = client.list_all_tools().await?;
    anyhow::ensure!(tools.iter().any(|tool| tool.name == "inspect_view"));

    let health = client
        .call_tool(CallToolRequestParams::new("health"))
        .await?;
    anyhow::ensure!(health.is_error != Some(true));
    let inspected = client
        .call_tool(
            CallToolRequestParams::new("inspect_view")
                .with_arguments(json!({"view_id": view_id}).as_object().unwrap().clone()),
        )
        .await?;
    anyhow::ensure!(inspected.is_error != Some(true));

    let recorded = client
        .call_tool(
            CallToolRequestParams::new("record_selection").with_arguments(
                json!({
                    "view_id": view_id,
                    "selection": {
                        "type": "cube_pixel",
                        "dataset_id": cube_dataset_id,
                        "array_path": "/EcoScope/Reflectance",
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

    client.cancel().await?;
    Ok(())
}
