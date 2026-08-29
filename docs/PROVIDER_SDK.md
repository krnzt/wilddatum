# Provider SDK and subprocess protocol

EcoScope's provider boundary is capability-based. Contributors describe what a
research infrastructure can do instead of reproducing NEON endpoints or adding
a provider-specific MCP tool family.

NEON is built in. Community providers can be standalone executables written in
Rust, Python, R, Go, or another language. They exchange bounded newline-delimited
JSON-RPC 2.0 over stdin/stdout. The shared domain records are plain JSON; the
Rust trait is an internal convenience, not the extension ABI.

## Install and negotiate

A local configuration identifies the expected provider and executable. Both the
command and interpreted-script arguments should be absolute because EcoScope
clears the subprocess environment, including `PATH`.

```json
{
  "schema_version": 1,
  "protocol_version": 2,
  "expected_provider_id": "my-ri",
  "command": "/usr/bin/python3",
  "args": ["/absolute/path/my_ri_provider.py"],
  "timeout_ms": 30000,
  "response_limit_bytes": 4194304
}
```

The configuration schema is
[`provider-process-config-v1.schema.json`](../schemas/provider-process-config-v1.schema.json).
Install and inspect it with:

```bash
ecoscope provider install ./my-ri.json
ecoscope provider list
```

Installation starts the executable, performs a handshake, validates its
identity/capabilities/origins, then copies the configuration into the private
EcoScope data directory. Replacement requires `--force`. `provider list`
renegotiates each installed executable and reports unavailable providers without
hiding the healthy ones. The same negotiated providers appear from the MCP
`list_providers` tool.

## Handshake and manifest

EcoScope sends one JSON object per line and expects exactly one response line
with the same request ID:

```json
{"jsonrpc":"2.0","id":1,"method":"provider.handshake","params":{"protocol_version":2,"client":"ecoscope","credential_transport":"none"}}
```

The result is a manifest matching
[`provider-manifest-v2.schema.json`](../schemas/provider-manifest-v2.schema.json):

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "schema_version": 2,
    "provider_id": "my-ri",
    "name": "My Research Infrastructure",
    "version": "0.1.0",
    "status": "community",
    "capabilities": ["catalog_search", "resource_resolve", "asset_plan"],
    "allowed_network_origins": ["https://data.example.org"],
    "authentication": [],
    "standards": ["Darwin Core", "DataCite"],
    "homepage": "https://example.org",
    "support_url": "https://example.org/help"
  }
}
```

`provider_id` must match `expected_provider_id`. IDs contain lowercase ASCII
letters, digits, and hyphens. Capabilities must be unique. Network origins are
exact HTTPS origins with no path or trailing slash.

## RPC methods

| Method | Params | Result |
|---|---|---|
| `provider.handshake` | protocol/client/credential declaration | `ProviderManifest` |
| `catalog.search` | `CatalogQuery` | `CatalogEntry[]` |
| `catalog.inspect` | `{ "id": string }` | `CatalogEntry` |
| `resources.search` | `ResourceQuery` | `ResourceRecord[]` |
| `resources.resolve` | `{ "id": string }` | `ResourceRecord` |
| `datasets.plan` | `DatasetRequest` | finalized `DatasetPlan` |
| `datasets.materialize` | `{ "plan": DatasetPlan }` | `DatasetManifest` |

The executable may implement the resource methods directly or map its native
catalog into them. `ResourceRecord` is broader than “data product”: it can
represent collections, dataset versions, assets, sites, stations, visits,
instruments, sensors, observations, occurrences, taxa, samples, agents, and
vocabulary terms. Relationships are explicit edges. Fields that do not fit the
shared model belong in `provider_extensions` and `raw_metadata`, not a lossy
description string.

A JSON-RPC error uses the standard shape:

```json
{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"method not found"}}
```

EcoScope rejects invalid JSON, a mismatched version or request ID, a missing
result, responses over the configured byte limit, and calls exceeding the
configured timeout. Calls on one provider process are serialized.

The configuration file remains schema v1, but `protocol_version` is negotiated
independently and must be 2. Protocol-v1 executables are not accepted. EcoScope
continues to read persisted alpha-era request, plan, source-file, and manifest
JSON through explicit aliases; that storage compatibility does not make v1 a
supported provider wire protocol.

## Capability contract

| Capability | Expected operation |
|---|---|
| `catalog_search` | Search provider resources without materializing assets |
| `resource_resolve` | Resolve one stable provider resource and native metadata |
| `asset_plan` | Produce a deterministic, inspectable transfer plan |
| `asset_fetch` | Materialize an approved public plan |
| `observations_query` | Query a provider observation service directly |
| `samples_query` | Resolve sample/visit/specimen relationships |
| `spatial_search` | Apply provider-side spatial constraints |
| `stream_subscribe` | Subscribe to a live or append-only stream |
| `citation_resolve` | Produce authoritative citation metadata |
| `policy_evaluate` | Surface licenses, embargoes, attribution, and access rules |

Do not advertise an operation that only returns “not implemented.” A discovery
and planning-only provider is valid and immediately useful.

## Planning and materialization

Planning must be deterministic and non-mutating. A provider returns file names,
sizes, checksums, public download URLs, temporal/location information, warnings,
and whether credentials are required. EcoScope finalizes the plan to create its
stable BLAKE3 approval hash and rejects every planned URL outside the manifest's
exact origin allowlist.

`datasets.plan` receives a request matching
[`dataset-request-v2.schema.json`](../schemas/dataset-request-v2.schema.json).
Provider-native selection state belongs in `provider_options`; the shared
fields are not renamed to match a particular RI:

```json
{
  "provider": {"other": "emso"},
  "resource_id": "OBSEA_moored_buoy_BGC_L1c",
  "locations": ["OBSEA"],
  "temporal_start": "2025-01-01T00:00:00Z",
  "temporal_end": "2025-01-31T23:59:59Z",
  "spatial_filter": null,
  "variables": ["time", "temperature"],
  "release": null,
  "package": "csv",
  "include_provisional": false,
  "provider_options": {
    "protocol": "tabledap",
    "constraints": []
  }
}
```

Adapters translate `resource_id`, locations, temporal bounds, spatial geometry,
variables, and options into provider-native parameters. For example, only the
NEON adapter knows that `resource_id` becomes `productCode` and `locations`
become `siteCodes`.

Community subprocess materialization is currently limited to public plans. The
subprocess receives the approved plan but no credential values. If
`requires_credentials` is true, EcoScope stops with an error. An authenticated
community provider needs an EcoScope-owned credential broker so the model and
provider process receive only an opaque connection reference.

A returned `DatasetManifest` must preserve source checksums, provider/release
version, license, citation, native QC and units, format/cube/spatial metadata,
and every named transformation. Expiring URLs are origins, never durable
dataset identity.

## Security model

Provider installation is an explicit decision to execute trusted local code.
The subprocess protocol is not an OS sandbox.

EcoScope does apply defense-in-depth:

- absolute commands and an identity-checked handshake;
- a cleared environment and no credential transport;
- piped stdin/stdout, discarded stderr, and kill-on-drop;
- per-call timeout and bounded request/response sizes;
- exact HTTPS-origin validation for every planned download URL.

These controls constrain the protocol but cannot prevent an executable from
using the user's filesystem or network permissions on its own. Package signing,
WASI isolation, and a brokered remote runner are future distribution layers.

## Rust adapter

An in-tree provider can implement `EcologicalDataProvider` directly:

```rust,ignore
#[async_trait]
impl EcologicalDataProvider for MyInfrastructure {
    fn provider_id(&self) -> &str { "my-ri" }

    fn manifest(&self) -> ProviderManifest { /* capability contract */ }

    async fn search_catalog(&self, query: CatalogQuery)
        -> Result<Vec<CatalogEntry>> { /* compatibility catalog */ }

    async fn inspect_product(&self, id: &str)
        -> Result<CatalogEntry> { /* compatibility catalog */ }

    async fn search_resources(&self, query: ResourceQuery)
        -> Result<Vec<ResourceRecord>> { /* preferred discovery */ }

    async fn resolve_resource(&self, id: &str)
        -> Result<ResourceRecord> { /* preserve native metadata */ }

    async fn plan_dataset(&self, request: DatasetRequest)
        -> Result<DatasetPlan> { /* deterministic, no download */ }

    async fn materialize(&self, plan: DatasetPlan, credentials: Option<CredentialRef>)
        -> Result<DatasetManifest> { /* immutable verified assets */ }
}
```

The executable `ecoscope-provider-fixture` and its integration test form the v2
wire-protocol conformance example. Provider tests should cover handshake
validation, representative metadata, a dry-run plan, out-of-allowlist URL
rejection, checksum failure, and one successful materialization without live
credentials.
