# Provider SDK and subprocess protocol

WildDatum's provider boundary is capability-based. Contributors describe what a
research infrastructure can do instead of reproducing NEON endpoints or adding
a provider-specific MCP tool family.

NEON and the maintained `emso`, `icos-erddap`, and `euro-argo` presets are built
in. The latter three share one generic ERDDAP implementation rather than
institution-specific MCP tools. Community providers can be standalone
executables written in Rust, Python, R, Go, or another language. They exchange
bounded newline-delimited JSON-RPC 2.0 over stdin/stdout. The shared domain
records are plain JSON; the Rust trait is an internal convenience, not the
extension ABI.

## Install and negotiate

A local configuration identifies the expected provider and executable. Both the
command and interpreted-script arguments should be absolute because WildDatum
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
wilddatum provider install ./my-ri.json
wilddatum provider list
```

Installation starts the executable, performs a handshake, validates its
identity/capabilities/origins, then copies the configuration into the private
WildDatum data directory. Replacement requires `--force`. `provider list`
renegotiates each installed executable and reports unavailable providers without
hiding the healthy ones. The same negotiated providers appear from the MCP
`list_providers` tool.

## Handshake and manifest

WildDatum sends one JSON object per line and expects exactly one response line
with the same request ID:

```json
{"jsonrpc":"2.0","id":1,"method":"provider.handshake","params":{"protocol_version":2,"client":"wilddatum","credential_transport":"none"}}
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

WildDatum rejects invalid JSON, a mismatched version or request ID, a missing
result, responses over the configured byte limit, and calls exceeding the
configured timeout. Calls on one provider process are serialized.

The configuration file remains schema v1, but `protocol_version` is negotiated
independently and must be 2. Protocol-v1 executables are not accepted. WildDatum
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
and whether credentials are required. WildDatum finalizes the plan to create its
stable BLAKE3 approval hash and rejects every planned URL outside the manifest's
exact origin allowlist.

`datasets.plan` receives a request matching
[`dataset-request-v2.schema.json`](../schemas/dataset-request-v2.schema.json).
Provider-native selection state belongs in `provider_options`; the shared
fields are not renamed to match a particular RI:

```json
{
  "provider": {"other": "emso"},
  "resource_id": "OBSEA_seabed_station_TS_L1c",
  "locations": [],
  "temporal_start": "2025-01-01T00:00:00Z",
  "temporal_end": "2025-01-31T23:59:59Z",
  "spatial_filter": null,
  "variables": ["time", "TEMP"],
  "release": null,
  "package": "csv",
  "include_provisional": false,
  "provider_options": {
    "protocol": "tabledap",
    "output_format": "csv",
    "constraints": [
      {"variable": "TEMP", "op": "gte", "value": 0}
    ]
  }
}
```

Adapters translate only concepts their service defines unambiguously. For
example, only the NEON adapter knows that `resource_id` becomes `productCode`
and `locations` become `siteCodes`. The generic ERDDAP adapter translates
neutral temporal bounds to tabledap `time` constraints but requires explicit
constraints for station/location dimensions because their names differ across
datasets. It rejects unsupported shared fields instead of silently ignoring
them.

Community subprocess materialization is currently limited to public plans. The
subprocess receives the approved plan but no credential values. If
`requires_credentials` is true, WildDatum stops with an error. An authenticated
community provider needs an WildDatum-owned credential broker so the model and
provider process receive only an opaque connection reference.

A returned `DatasetManifest` must preserve source checksums, provider/release
version, license, citation, native QC and units, format/cube/spatial metadata,
and every named transformation. Expiring URLs are origins, never durable
dataset identity.

## Maintained ERDDAP adapter and presets

The in-tree ERDDAP provider owns one parser, HTTP client, query planner, and
materializer. Presets provide only immutable identity and policy:

The query model follows the official
[tabledap](https://coastwatch.pfeg.noaa.gov/erddap/tabledap/documentation.html)
and
[griddap](https://coastwatch.pfeg.noaa.gov/erddap/griddap/documentation.html)
contracts; WildDatum deliberately exposes a validated subset of each rather
than accepting arbitrary query fragments.

| ID | Base URL | Optional catalog scope |
|---|---|---|
| `emso` | `https://erddap.emso.eu/erddap` | none |
| `icos-erddap` | `https://erddap.icos-cp.eu/erddap` | none |
| `euro-argo` | `https://erddap.ifremer.fr/erddap` | `Argo` |

Catalog and `info` responses use ERDDAP's JSON table envelope. Columns are
looked up by name rather than position, every row must match the declared
width, and metadata responses are capped at 4 MiB before deserialization.
Search results retain institution and protocol URLs; resource resolution
retains the original info table, global attributes, variable types/units,
coverage, and feature type.

`provider_options` is deny-unknown-fields and supports:

- `protocol`: `tabledap` or `griddap`;
- `output_format`: `csv` or `netcdf`;
- tabledap operators `eq`, `ne`, `lt`, `lte`, `gt`, and `gte`;
- griddap axis slices `{start, stop, stride, by_value}` with a nonzero stride.

The planner validates the dataset and every requested, constraint, or array
variable against inspected metadata. Tabledap cannot contain array slices;
griddap cannot contain row constraints. Requests are encoded once as one
ERDDAP DAP expression and the decoded expression remains in the plan.

Only the preset's exact HTTPS origin may receive the initial download request.
Some federated services—notably EMSO—redirect a subset to a regional ERDDAP.
WildDatum disables automatic HTTP redirects, probes the chain during planning,
requires every target to keep the exact subset path/query and use HTTPS, and
stores the complete chain in the approval hash. Materialization follows only
that approved chain and fails closed if any redirect changes. Loopback HTTP is
accepted solely by deterministic tests.

Downloads stream through `.partial-{asset_id}`, compute BLAKE3 incrementally,
flush and sync before atomic rename, and remove partial files on cancellation or
failure. The manifest captures the final source URL, query, redirect chain,
ERDDAP version, ETag, Last-Modified, access time, globals, license, citation,
and locally frozen checksum. A live ERDDAP query is not presented as a fixed
upstream release.

To propose another maintained preset, contribute all of the following:

1. A stable public HTTPS `/erddap` base URL and authoritative RI homepage.
2. A unique lowercase provider ID and any narrowly justified catalog scope.
3. Shuffled-column search and representative info fixtures that are safe to
   redistribute.
4. Deterministic catalog, plan, redirect/origin, materialization, and cleanup
   tests.
5. An ignored live search/inspect drift test against the public service.

Do not fork the provider merely to rename fields or change branding. A separate
adapter is warranted only when service semantics cannot be represented by the
shared ERDDAP contract.

## Security model

Provider installation is an explicit decision to execute trusted local code.
The subprocess protocol is not an OS sandbox.

WildDatum does apply defense-in-depth:

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

    // Optional for in-tree providers: object-safe cooperative cancellation and
    // progress used by the generic MCP materialization router.
    async fn materialize_controlled(/* plan, credentials, callbacks */)
        -> Result<DatasetManifest> { /* delegate to streaming implementation */ }
}
```

The executable `wilddatum-provider-fixture` and its integration test form the v2
wire-protocol conformance example. Provider tests should cover handshake
validation, representative metadata, a dry-run plan, out-of-allowlist URL
rejection, checksum failure, and one successful materialization without live
credentials.
