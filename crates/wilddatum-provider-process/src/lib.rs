//! Language-neutral provider subprocess runtime.
//!
//! Community providers communicate over bounded newline-delimited JSON-RPC.
//! Credential values are never sent to a child process. Network origin
//! declarations are validated on plans, while installation of an executable
//! remains an explicit trust decision by the user.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};
use url::Url;
use wilddatum_core::{
    CatalogEntry, CatalogQuery, CredentialRef, DatasetManifest, DatasetPlan, DatasetRequest,
    ProviderManifest, ResourceQuery, ResourceRecord, Result, WildDatumError,
};
use wilddatum_provider_api::{
    EcologicalDataProvider, PROVIDER_PROTOCOL_VERSION, validate_manifest,
};

pub const DEFAULT_RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const PROVIDER_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessProviderConfig {
    #[serde(default = "default_config_version")]
    pub schema_version: u32,
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
    pub expected_provider_id: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_response_limit")]
    pub response_limit_bytes: usize,
}

fn default_config_version() -> u32 {
    PROVIDER_CONFIG_VERSION
}

fn default_protocol_version() -> u32 {
    PROVIDER_PROTOCOL_VERSION
}

fn default_timeout_ms() -> u64 {
    DEFAULT_REQUEST_TIMEOUT.as_millis() as u64
}

fn default_response_limit() -> usize {
    DEFAULT_RESPONSE_LIMIT
}

impl ProcessProviderConfig {
    pub fn from_file(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_reader(std::fs::File::open(path)?)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.command.is_absolute() {
            return Err(WildDatumError::Invalid(format!(
                "provider command must be an absolute path: {}",
                self.command.display()
            )));
        }
        if self.schema_version != PROVIDER_CONFIG_VERSION {
            return Err(WildDatumError::Invalid(format!(
                "provider configuration uses unsupported schema version {}",
                self.schema_version
            )));
        }
        if self.protocol_version != PROVIDER_PROTOCOL_VERSION {
            return Err(WildDatumError::Invalid(format!(
                "provider configuration requests unsupported protocol version {}",
                self.protocol_version
            )));
        }
        if self.timeout_ms == 0 || self.response_limit_bytes < 1024 {
            return Err(WildDatumError::Invalid(
                "provider timeout must be positive and response limit at least 1024 bytes".into(),
            ));
        }
        Ok(())
    }
}

pub fn discover_configs(directory: &Path) -> Result<Vec<ProcessProviderConfig>> {
    if !directory.is_dir() {
        return Ok(vec![]);
    }
    let mut paths = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    paths
        .into_iter()
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .map(|path| ProcessProviderConfig::from_file(&path))
        .collect()
}

pub fn find_config(directory: &Path, provider_id: &str) -> Result<ProcessProviderConfig> {
    discover_configs(directory)?
        .into_iter()
        .find(|config| config.expected_provider_id == provider_id)
        .ok_or_else(|| {
            WildDatumError::NotFound(format!(
                "community provider {provider_id} is not installed in {}",
                directory.display()
            ))
        })
}

struct Runtime {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

pub struct ProcessProvider {
    provider_id: String,
    manifest: ProviderManifest,
    runtime: Arc<Mutex<Runtime>>,
    call_timeout: Duration,
    response_limit: usize,
}

impl ProcessProvider {
    pub async fn spawn(config: ProcessProviderConfig) -> Result<Self> {
        config.validate()?;
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .env_clear()
            .env("NO_COLOR", "1");
        let mut child = command.spawn().map_err(|error| {
            WildDatumError::Invalid(format!(
                "cannot start provider {}: {error}",
                config.command.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| WildDatumError::Internal("provider stdin is unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| WildDatumError::Internal("provider stdout is unavailable".into()))?;
        let runtime = Arc::new(Mutex::new(Runtime {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            next_id: 1,
        }));
        let call_timeout = Duration::from_millis(config.timeout_ms);
        let manifest: ProviderManifest = rpc_call(
            &runtime,
            call_timeout,
            config.response_limit_bytes,
            "provider.handshake",
            json!({
                "protocol_version": config.protocol_version,
                "client": "wilddatum",
                "credential_transport": "none"
            }),
        )
        .await?;
        validate_manifest(&manifest)?;
        if manifest.provider_id != config.expected_provider_id {
            return Err(WildDatumError::Invalid(format!(
                "provider identity mismatch: configuration expected {}, executable reported {}",
                config.expected_provider_id, manifest.provider_id
            )));
        }
        let provider_id = manifest.provider_id.clone();
        Ok(Self {
            provider_id,
            manifest,
            runtime,
            call_timeout,
            response_limit: config.response_limit_bytes,
        })
    }

    async fn call<P: Serialize, T: DeserializeOwned>(&self, method: &str, params: P) -> Result<T> {
        rpc_call(
            &self.runtime,
            self.call_timeout,
            self.response_limit,
            method,
            serde_json::to_value(params)?,
        )
        .await
    }

    fn validate_plan_origins(&self, plan: &DatasetPlan) -> Result<()> {
        for file in &plan.files {
            let Some(download_url) = &file.download_url else {
                continue;
            };
            let url = Url::parse(download_url).map_err(|error| {
                WildDatumError::Invalid(format!("provider returned invalid download URL: {error}"))
            })?;
            let host = url.host_str().ok_or_else(|| {
                WildDatumError::Invalid("provider download URL has no host".into())
            })?;
            let origin = match url.port() {
                Some(port) => format!("{}://{host}:{port}", url.scheme()),
                None => format!("{}://{host}", url.scheme()),
            };
            if !self
                .manifest
                .allowed_network_origins
                .iter()
                .any(|allowed| allowed == &origin)
            {
                return Err(WildDatumError::Invalid(format!(
                    "provider plan returned URL outside its declared origin allowlist: {origin}"
                )));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl EcologicalDataProvider for ProcessProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn manifest(&self) -> ProviderManifest {
        self.manifest.clone()
    }

    async fn search_catalog(&self, query: CatalogQuery) -> Result<Vec<CatalogEntry>> {
        self.call("catalog.search", query).await
    }

    async fn inspect_product(&self, id: &str) -> Result<CatalogEntry> {
        self.call("catalog.inspect", json!({"id": id})).await
    }

    async fn search_resources(&self, query: ResourceQuery) -> Result<Vec<ResourceRecord>> {
        self.call("resources.search", query).await
    }

    async fn resolve_resource(&self, id: &str) -> Result<ResourceRecord> {
        self.call("resources.resolve", json!({"id": id})).await
    }

    async fn plan_dataset(&self, request: DatasetRequest) -> Result<DatasetPlan> {
        let plan: DatasetPlan = self.call("datasets.plan", request).await?;
        self.validate_plan_origins(&plan)?;
        plan.finalize()
    }

    async fn materialize(
        &self,
        plan: DatasetPlan,
        credentials: Option<CredentialRef>,
    ) -> Result<DatasetManifest> {
        if credentials.is_some() || plan.requires_credentials {
            return Err(WildDatumError::Invalid(
                "community subprocess providers cannot receive credential values; authenticated fetching must be implemented by an WildDatum-owned credential broker"
                    .into(),
            ));
        }
        self.call("datasets.materialize", json!({"plan": plan}))
            .await
    }
}

#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    jsonrpc: String,
    id: u64,
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

async fn rpc_call<T: DeserializeOwned>(
    runtime: &Arc<Mutex<Runtime>>,
    call_timeout: Duration,
    response_limit: usize,
    method: &str,
    params: Value,
) -> Result<T> {
    let mut runtime = runtime.lock().await;
    let id = runtime.next_id;
    runtime.next_id += 1;
    let request = serde_json::to_vec(&RpcRequest {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })?;
    if request.len() > response_limit {
        return Err(WildDatumError::Invalid(format!(
            "provider request exceeds {response_limit} bytes"
        )));
    }
    runtime.stdin.write_all(&request).await?;
    runtime.stdin.write_all(b"\n").await?;
    runtime.stdin.flush().await?;
    let line = match timeout(
        call_timeout,
        read_bounded_line(&mut runtime.stdout, response_limit),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            let _ = runtime.child.kill().await;
            return Err(WildDatumError::Invalid(format!(
                "provider call {method} exceeded {} ms and the process was terminated",
                call_timeout.as_millis()
            )));
        }
    };
    let response: RpcResponse = serde_json::from_slice(&line).map_err(|error| {
        WildDatumError::Invalid(format!("provider returned invalid JSON-RPC: {error}"))
    })?;
    if response.jsonrpc != "2.0" || response.id != id {
        return Err(WildDatumError::Invalid(
            "provider returned a mismatched JSON-RPC version or request ID".into(),
        ));
    }
    if let Some(error) = response.error {
        return Err(WildDatumError::Invalid(format!(
            "provider error {}: {}{}",
            error.code,
            error.message,
            error
                .data
                .map(|data| format!(" ({data})"))
                .unwrap_or_default()
        )));
    }
    let result = response
        .result
        .ok_or_else(|| WildDatumError::Invalid("provider response has no result".into()))?;
    serde_json::from_value(result).map_err(|error| {
        WildDatumError::Invalid(format!(
            "provider result does not match the contract: {error}"
        ))
    })
}

async fn read_bounded_line(reader: &mut BufReader<ChildStdout>, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Err(WildDatumError::Invalid(
                "provider exited before returning a response".into(),
            ));
        }
        let take = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |position| position + 1);
        if output.len() + take > limit {
            return Err(WildDatumError::Invalid(format!(
                "provider response exceeds {limit} bytes"
            )));
        }
        output.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if output.last() == Some(&b'\n') {
            output.pop();
            if output.last() == Some(&b'\r') {
                output.pop();
            }
            return Ok(output);
        }
    }
}
