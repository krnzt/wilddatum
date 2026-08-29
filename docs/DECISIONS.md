# Key implementation decisions

## Standard MCP first

EcoScope is a normal MCP 2026-07-28 stdio server. Codex and Claude Code launch
the same native binary and discover its tools through MCP. No host-specific
plugin is required for the scientific interface. MCPB is an additional release
artifact, not the runtime architecture.

The initial release stays local because local scientific files, large downloads,
native viewers, and operating-system credentials all have a clear security
boundary there. A later remote Streamable HTTP deployment can add OAuth without
changing provider, manifest, view, or selection models.

## Extend Rerun instead of building a viewer from scratch

Rerun already supplies a high-performance native viewer, a browser WebAssembly
viewer, spatial and temporal entity models, point clouds, images, scalar plots,
selection events, and a versioned recording format. EcoScope owns the ecological
semantics around it: discovery, source mapping, provenance, reproducible view
state, and conversion of human selections into MCP-readable records.

Rerun is intentionally isolated in `ecoscope-rerun` and pinned. An upgrade is a
tested adapter change rather than a repository-wide rewrite. `EcoViewSpec` and
`SemanticSelection` remain authoritative; an RRD is a regenerable artifact.

## Rust core, minimal TypeScript shell

Rust owns providers, streaming I/O, checksums, HDF5/LAS/image adapters, SQLite,
MCP, view state, and Rerun recordings. TypeScript is limited to bootstrapping the
Rerun Web Viewer and forwarding its structured selection events to the local
service. Browser lifecycle and DOM integration are where TypeScript is useful;
scientific logic is not duplicated there.

## Explicit hyperspectral semantics

HDF5 containers may contain many arrays, and an arbitrary three-dimensional
array does not establish which dimension is wavelength or which bands should be
displayed. EcoScope therefore inventories datasets and shapes, then requires an
explicit layer mapping before rendering. The mapping travels with view state and
can be cited as a transformation.

Common NEON reflectance containers are inferred only when a single compatible
reflectance array and wavelength coordinate make the axis roles unambiguous.
The same cube contract now drives HDF5/NetCDF-4, NetCDF-3, and Zarr queries and
Rerun previews; `configure_cube_view` is the provider-neutral path.

## Language-neutral community providers

The shared provider contract is JSON, not a Rust dynamic-library ABI. Built-in
providers may implement the Rust trait directly. Community providers are
explicitly installed subprocesses using bounded newline-delimited JSON-RPC and
a negotiated capability manifest. This lets RI contributors work in their
normal ecosystem without coupling EcoScope to Python embedding or an unstable
Rust ABI.

Subprocess installation is a code-trust decision. Origin validation, cleared
environment variables, timeouts, and response limits reduce accidental scope;
they are not an OS sandbox. Credentials remain in EcoScope-owned brokers and
are never transported to a community subprocess.

## Provider-neutral materialization vocabulary

The canonical request and manifest do not use NEON product, site, or month
names. They use resource identity/version, locations, temporal bounds, spatial
geometry, variables, and an explicit provider-options map. Provider-native
terms such as NEON `productCode`, ERDDAP `datasetID`, or an RI-specific package
selector are translated only inside the responsible adapter.

This prevents the first provider from defining every future provider's data
model while still preserving native state. Provider options and metadata remain
structured JSON, source records retain native checksums and QC metadata, and
the MCP tool exposes the shared fields as a typed schema. Persisted alpha JSON
is read through aliases, but all newly serialized records and subprocess
protocol-v2 messages use the neutral vocabulary.
