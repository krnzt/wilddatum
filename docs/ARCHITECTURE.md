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
9. Scientific inventories and view suggestions are derived, evidence-backed
   advice. They never replace immutable manifests or silently confer trusted
   source/spatial identity.

## Runtime

```text
MCP host ──stdio──> wilddatum mcp
                       │
                       ├── SQLite/WAL state
                       ├── content-addressed objects
                       ├── built-in NEON provider
                       ├── negotiated community provider subprocesses
                       ├── local import adapters
                       ├── Arrow/DataFusion + spatial/cube/point queries
                       ├── scientific inventory + view suggestion compiler
                       ├── durable result/export/derived artifacts
                       └── EcoViewSpec -> explicit Rerun blueprint + RRD
                                         │
                       browser <──loopback token──┘
```

The current executable opens SQLite directly from each process. WAL mode and
optimistic view revisions permit safe sharing between the MCP server and
browser/CLI processes. A dedicated IPC daemon can replace this without changing
public interfaces when concurrent processing requirements justify it.

## Scientific inventory and suggestion boundary

`DatasetManifest` remains source truth. `ScientificInventory` is a bounded,
regenerable interpretation of an existing manifest plus local inspection or
provider metadata. It exposes fields, array axes, units, QC relationships,
coordinate summaries, evidence, and unresolved decisions without returning
local absolute paths or full coordinate vectors.

Inference follows an explicit precedence:

```text
user-confirmed mapping
    > provider/file metadata such as CF, CRS, LAS, or GeoArrow
    > strong format convention
    > field-name heuristic
```

Every inferred role carries its source and confidence. Weak field-name
heuristics can support a suggestion but cannot establish CRS compatibility,
source-row identity, or cross-dataset registration.

`suggest_views` recognizes a bounded set of scientific recipes and returns
ranked, deterministic `ViewSuggestion` objects. A suggestion contains proposed
panels, encodings, links, link exactness, evidence, and unresolved decisions.
It is advisory and side-effect free: it does not persist a view, start Rerun, or
download data. At most eight unique dataset handles enter one request and at
most twelve suggestions leave it; the normal MCP response-size ceiling still
applies.

`create_view_from_suggestion` is the only suggestion-acceptance boundary. It
accepts an opaque suggestion ID plus the original dataset handles, recomputes
the bounded suggestion from current immutable manifests, and rejects IDs that
do not match. The resulting `EcoViewSpec` v2 snapshots explicit panels,
panel-local encodings, selection link rules, link exactness, and the source
suggestion ID. A v2 RGB panel also configures the underlying spectral layer for
the existing Rerun renderer. Missing v2 fields use empty defaults so persisted
v1 views continue to deserialize and render.

`resolve_selection_links` evaluates only rules that match the durable source
selection and the source panel's dataset. Exact cube-pixel → spectrum mapping
stores the source selection and link rule in result provenance, and the browser
presents the bounded spectrum preview after a real Rerun pick. Exact
point-return → image-pixel resolution first verifies identical authoritative
CRS, an internally consistent north-up affine transform, and footprint bounds.
It records the derived `CubePixel`, then chains that structured selection
through the existing spectrum resolver. Rules marked `unavailable` are returned
as structured state without executing a query. Unknown resolvers, non-exact
executable rules, mismatched arrays, out-of-bounds coordinates, and selections
recorded against an older view revision fail closed.

The first multimodal recipe combines point-cloud, wavelength-aware RGB, and
spectrum panels. RGB bands are selected from inspected wavelength coordinates,
not institution-specific indices. Cube-pixel-to-spectrum can be exact from
array axes alone. Point-to-pixel linking becomes exact only when source CRS,
cube world-to-pixel transform, transform consistency, and spatial overlap are
authoritative; otherwise the suggested link explains why it is unavailable.

## Rerun boundary

WildDatum pins Rerun 0.36 because RRD and deep viewer extension APIs evolve with
Rerun releases. Only `wilddatum-rerun` may depend on Rerun. Upgrades must pass:

- RRD generation tests.
- semantic event round trips.
- browser loading.
- representative tabular, raster, tensor, and point-cloud fixtures.

The TypeScript browser package only boots the Rerun Wasm viewer and translates
viewer positions through mappings already stored in `EcoViewSpec`. It does not
invent scientific schemas, transforms, queries, or provenance.

When a link resolves, Rust regenerates the complete RRD with source and derived
selection markers plus bounded linked-result series under the target panel's
entity tree. The loopback server publishes the completed file atomically, and
the TypeScript shell reconnects the viewer with a cache-busted recording URL.
The overlay is a rendering of `SelectionLinkResolution`, not a second source of
scientific state.

Rerun remains the rendering and interaction engine; WildDatum does not fork it
or build a replacement canvas. Native Rerun and the Rerun Web Viewer consume the
same bounded RRD. WildDatum adds a side panel and loopback API so viewer events
become durable `SemanticSelection` records that an MCP agent can inspect and
query.

The boundary is deliberately honest about what the viewer API exposes:

- Entity/instance picks include an entity path, optional instance ID, and a
  viewer pick position. The position is not guaranteed to be the exact logged
  point component.
- WildDatum-authored point batches record a sequential instance-to-source-stride
  mapping. The service recomputes and verifies that mapping against the pinned
  Rerun version before issuing an exact source-row query.
- Image positions are mapped through the recorded preview stride to source
  cube pixels; the agent then reads the source array, not canvas colors.
- Rerun does not currently provide WildDatum a universal brush/interval event
  protocol across every view type. Explicit time, map, raster, spectral, and
  row selections can also enter through `record_selection`; those use the same
  durable state and `query_selection` path.
- Community-authored RRD entities receive no implicit exact-row promise. If a
  verified mapping is absent, point picks become source-precision spatial
  queries.

### Profile/trajectory recipe boundary

Linked geographic trajectories and vertical profiles deliberately reuse this
boundary. `configure_profile_trajectory_view` validates a typed
`profile_trajectory_v1` recipe against an immutable CSV/TSV, Parquet, or Arrow
source, then the service—not the client—adds a `source_row_index` selection
mapping with the allowed entity suffix for every value panel. The Rerun adapter
parses deterministic physical record order, groups line geometry by
trajectory/profile identifiers, keeps native QC on pickable observations, and
logs physical source-record indices as instance IDs. Profile-aware sampling and
vertical ranges hide observations with transparent in-range placeholders rather
than compacting those instance arrays.

The web shell does not derive or transmit a trusted source index. It emits the
selected observation entity and instance together with the mapping kind and
pinned Rerun version. `query_selection` reopens authoritative view and manifest
state, verifies that exact mapping, checks multiplication/bounds, and reads the
requested physical record directly from the delimited, Parquet, or Arrow source.
This makes a pick in the map or any value profile reproducible without asking an
agent to interpret pixels or trust browser-authored scientific state.

## Data lifecycle

```text
source -> inspection -> plan -> approval -> materialization -> manifest
       -> scientific inventory -> explained view suggestions
       -> bounded query -> durable result -> explicit blueprint + RRD
       -> human selection in source coordinates -> agent query -> export
```

An exact remote plan has a BLAKE3 hash. Approval requires that exact hash. NEON
downloads stream into a partial object while BLAKE3, MD5, and CRC32C are
computed, then are verified and atomically promoted.
Durable source identity never depends on an expiring download URL.

Generic ERDDAP downloads use the same partial-object discipline with BLAKE3 and
a configurable streaming byte ceiling. Catalog resolution preserves global and
variable-level CF metadata; materialization copies both into the durable
manifest so recipes can be constructed from `cf_role`, `standard_name`, units,
axes, and fill values without coupling Rerun to Argo or another institution.

## MCP profile workflow

Local files enter out of band through `wilddatum import`; remote data use the
normal catalog, plan, approval, and materialization tools. From the resulting
opaque dataset ID, every MCP host follows the same agent-native sequence:

```text
create_view(dataset_id)
  -> configure_profile_trajectory_view(view_id, expected_revision, fields/QC)
  -> open_view(view_id)
  -> human selects an observation in Rerun
  -> inspect_view(view_id)
  -> query_selection(selection_id)
  -> exact source row + provenance-linked result handle
```

The tool is provider-neutral. A local CTD cast and Euro-Argo `ArgoFloats` CSV
use identical recipe and selection semantics; only materialization provenance
and native field names differ.

## Provider contract

Providers advertise granular capabilities such as catalog search, resource
resolution, asset planning/fetching, observation queries, sample queries,
spatial search, citation resolution, and policy evaluation. Provider-native
fields survive in `provider_extensions` and `raw_metadata`. Additional Research
Infrastructures therefore extend the provider registry without creating a new
MCP tool family or pretending that every RI has NEON's product/site/month model.

Canonical materialization records use `resource_id`, `resource_version`,
`locations`, temporal bounds, spatial geometry, variables, and
`provider_options`. Native identifiers such as NEON `productCode` or ERDDAP
`datasetID` exist only inside their adapters. The executable contract is defined
by the [dataset request v2](../schemas/dataset-request-v2.schema.json) and
[provider manifest v2](../schemas/provider-manifest-v2.schema.json) schemas.
Serde aliases keep persisted alpha JSON readable without emitting the old names
or accepting protocol-v1 provider executables.

The built-in NEON adapter is Rust because it owns credential brokerage,
streaming downloads, and checksum validation. Community adapters can instead be
executables written in any language. They negotiate a v2 manifest and exchange
bounded newline-delimited JSON-RPC over stdin/stdout. Requests are serialized,
time-limited, response-size-limited, and identity-checked. Planned download
URLs are rejected unless their exact HTTPS origin appears in the negotiated
manifest.

This is an interoperability boundary, not an operating-system sandbox. Installing
a provider executable is an explicit decision to run trusted local code.
WildDatum clears its environment before launch and never passes credentials, but
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
