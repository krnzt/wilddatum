# EcoScope roadmap

EcoScope is building an agent-native ecological data workbench: one standard
MCP interface for finding, materializing, querying, visualizing, selecting, and
citing ecological data from Research Infrastructures and local files.

This roadmap describes intended outcomes, not release dates. Priorities can
move when Research Infrastructure maintainers contribute representative data,
metadata, or operational constraints. Work is accepted when it has bounded
agent responses, source-level provenance, deterministic tests, and a real
Rerun browser verification where visualization is involved.

## Shipped: public multimodal alpha

- Standard local MCP server registered as `io.github.krnzt/ecoscope`.
- NEON discovery, approval, authenticated materialization, checksums, license,
  citation, and immutable manifests.
- Local CSV, Parquet, Arrow, imagery, raster, vector, LAS/LAZ/COPC,
  HDF5/NetCDF, and Zarr ingestion paths.
- Native and browser Rerun rendering for tabular observations, imagery,
  point clouds, vectors, and mapped scientific cubes.
- Exact point-row and hyperspectral-pixel selections exposed as structured MCP
  state.
- Language-neutral community-provider subprocess protocol.

## Next: interoperable public Research Infrastructures

The next release line establishes one standards adapter that can serve several
RIs instead of adding institution-specific MCP tools.

- [x] Migrate plans and manifests from NEON field names to a provider-neutral
  v2 contract, with read compatibility for existing alpha state.
- [x] Add a reusable public ERDDAP provider with bounded catalog search,
  metadata inspection, tabledap and griddap planning, streaming
  materialization, license, citation, and provenance capture.
- [x] Ship maintained presets for EMSO ERIC, the public ICOS ERDDAP surface,
  and Euro-Argo/Argo data served by Ifremer.
- [ ] Add linked trajectory-map and vertical-profile Rerun views with exact
  source-row selection and QC-aware configuration.
- [x] Add synthetic fixtures and opt-in live smoke tests for all three public
  services; normal CI remains deterministic and offline.

## Following: authenticated ecological repositories

- EcoScope-owned credential broker using opaque connection references; models
  and community-provider processes never receive credential values.
- SAEON discovery and Parquet/CSV observation materialization through its
  OpenAPI/OData service.
- EDI and US LTER package discovery, EML relationships, immutable package
  versions, and citation-aware materialization.
- Provider conformance kit with recorded HTTP fixtures, schema validation,
  origin-policy tests, checksum-failure tests, and contributor documentation.

## Later: federated multimodal infrastructures

- STAC and OGC API adapters for imagery, rasters, collections, and spatial
  subsetting.
- THREDDS/OPeNDAP support for provider-hosted multidimensional arrays.
- TERN facility adapters built on those standards, beginning with one coherent
  imagery or remote-sensing vertical instead of treating every TERN portal as
  one API.
- eLTER site, platform, and sensor graph integration alongside the eLTER DAR
  dataset service.
- Remote Streamable HTTP MCP deployment with OAuth, while retaining local stdio
  for local files, operating-system credentials, and high-volume rendering.

## Continuing engineering tracks

These are release-independent quality commitments:

- Keep Rerun behind the `ecoscope-rerun` boundary and verify every upgrade with
  real browser rendering and semantic-selection tests.
- Preserve native QC flags, units, CRS, provider metadata, access policy,
  transformations, and citations without flattening them into prose.
- Prefer immutable source objects and explicit versions; when a provider serves
  live data, record the exact query, response metadata, access time, and local
  content checksum.
- Keep large data and private paths out of MCP responses.
- Improve accessibility, installation, notarization, platform coverage, and
  contributor-owned RI fixtures as the community grows.

## How to contribute

The most useful early contributions are small representative fixtures and
metadata interpretations from RI maintainers. Open an issue before starting a
large provider or viewer change and identify:

1. the authoritative discovery and download interfaces;
2. stable identifiers, versions, licenses, citations, units, and QC semantics;
3. one small redistributable or synthetic fixture;
4. the scientific selection users need to make in the visualization; and
5. the exact source records that selection must resolve back to.

See [CONTRIBUTING.md](CONTRIBUTING.md) and the
[provider SDK](docs/PROVIDER_SDK.md) for the current extension contract.
