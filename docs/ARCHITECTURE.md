# Architecture

## Invariants

1. `DatasetManifest` is the source of truth for data and provenance.
2. `EcoViewSpec` is the source of truth for visualization state.
3. `SemanticSelection` is the source of truth for human interaction.
4. Raw sources are immutable. Every conversion is a named transformation.
5. Rerun recordings are bounded, regenerable rendering artifacts.
6. Remote credentials and local filesystem paths never enter model context.
7. MCP responses remain small; large data move by local artifact handles.
8. Provider capabilities are negotiated and provider-native metadata is never
   discarded merely because the shared model does not recognize it.

## Runtime

```text
MCP host ──stdio──> ecoscope mcp
                       │
                       ├── SQLite/WAL state
                       ├── content-addressed objects
                       ├── built-in NEON provider
                       ├── negotiated community provider subprocesses
                       ├── local import adapters
                       ├── Arrow/DataFusion + spatial/cube/point queries
                       ├── durable result/export/derived artifacts
                       └── EcoViewSpec -> explicit Rerun blueprint + RRD
                                         │
                       browser <──loopback token──┘
```

The current executable opens SQLite directly from each process. WAL mode and
optimistic view revisions permit safe sharing between the MCP server and
browser/CLI processes. A dedicated IPC daemon can replace this without changing
public interfaces when concurrent processing requirements justify it.

## Rerun boundary

EcoScope pins Rerun 0.36 because RRD and deep viewer extension APIs evolve with
Rerun releases. Only `ecoscope-rerun` may depend on Rerun. Upgrades must pass:

- RRD generation tests.
- semantic event round trips.
- browser loading.
- representative tabular, raster, tensor, and point-cloud fixtures.

The TypeScript browser package only boots the Rerun Wasm viewer and translates
viewer positions through mappings already stored in `EcoViewSpec`. It does not
invent scientific schemas, transforms, queries, or provenance.

Rerun remains the rendering and interaction engine; EcoScope does not fork it
or build a replacement canvas. Native Rerun and the Rerun Web Viewer consume the
same bounded RRD. EcoScope adds a side panel and loopback API so viewer events
become durable `SemanticSelection` records that an MCP agent can inspect and
query.

The boundary is deliberately honest about what the viewer API exposes:

- Entity/instance picks include an entity path, optional instance ID, and a
  viewer pick position. The position is not guaranteed to be the exact logged
  point component.
- EcoScope-authored point batches record a sequential instance-to-source-stride
  mapping. The service recomputes and verifies that mapping against the pinned
  Rerun version before issuing an exact source-row query.
- Image positions are mapped through the recorded preview stride to source
  cube pixels; the agent then reads the source array, not canvas colors.
- Rerun does not currently provide EcoScope a universal brush/interval event
  protocol across every view type. Explicit time, map, raster, spectral, and
  row selections can also enter through `record_selection`; those use the same
  durable state and `query_selection` path.
- Community-authored RRD entities receive no implicit exact-row promise. If a
  verified mapping is absent, point picks become source-precision spatial
  queries.

## Data lifecycle

```text
source -> inspection -> plan -> approval -> materialization -> manifest
       -> bounded query -> durable result -> explicit blueprint + RRD
       -> human selection in source coordinates -> agent query -> export
```

An exact remote plan has a BLAKE3 hash. Approval requires that exact hash. NEON
downloads stream into a partial object while BLAKE3, MD5, and CRC32C are
computed, then are verified and atomically promoted.
Durable source identity never depends on an expiring download URL.

## Provider contract

Providers advertise granular capabilities such as catalog search, resource
resolution, asset planning/fetching, observation queries, sample queries,
spatial search, citation resolution, and policy evaluation. Provider-native
fields survive in `provider_extensions` and `raw_metadata`. Additional Research
Infrastructures therefore extend the provider registry without creating a new
MCP tool family or pretending that every RI has NEON's product/site/month model.

The built-in NEON adapter is Rust because it owns credential brokerage,
streaming downloads, and checksum validation. Community adapters can instead be
executables written in any language. They negotiate a v1 manifest and exchange
bounded newline-delimited JSON-RPC over stdin/stdout. Requests are serialized,
time-limited, response-size-limited, and identity-checked. Planned download
URLs are rejected unless their exact HTTPS origin appears in the negotiated
manifest.

This is an interoperability boundary, not an operating-system sandbox. Installing
a provider executable is an explicit decision to run trusted local code.
EcoScope clears its environment before launch and never passes credentials, but
the executable still has the permissions of the user account. A future WASI or
brokered remote runner can add stronger isolation without changing the JSON
domain contract.

## Security boundary

- MCP is stdio-only in v0.1.
- Browser services bind to `127.0.0.1` and require an unguessable launch token.
- Mutating browser requests validate same-loopback Origin and Host.
- Local imports originate in a user-controlled CLI/picker.
- The NEON token is stored in the OS keychain; `NEON_API_TOKEN` is a documented
  headless fallback.
- Credential values are never sent to community provider subprocesses.
- Provider output is bounded and planned URLs are checked against declared
  exact HTTPS origins; these checks do not make an installed executable safe.
- Exact point-source indices are recomputed from trusted view state instead of
  trusting a browser-supplied verification flag.
