# Format support

WildDatum separates physical encoding from scientific modality. Importing a
container records what is observable without guessing; a versioned mapping adds
axis roles, wavelengths, CRS, no-data, scaling, or display bands when those
semantics are ambiguous.

| Source | Inspect/import | Scientific query | Rerun rendering | Important bounds or caveats |
|---|---:|---:|---:|---|
| CSV/TSV | yes | yes | yes | Arrow/DataFusion projection, predicates, grouping, ordering, and aggregation; bounded previews |
| Parquet | yes | yes | tabular | Arrow/DataFusion with row-group pruning supplied by the engine |
| Arrow IPC/Feather | yes | yes | tabular | Record and stream IPC are accepted by the query adapter |
| PNG/JPEG/WebP | yes | pixel | yes | Decoded locally; channel values are queryable |
| TIFF/GeoTIFF | yes | pixel/region/statistics | yes | Pure-Rust decode, affine source coordinates, selected bands, COG export with overviews |
| GeoJSON | yes | exact spatial predicate | yes | Structure inspection is capped; large collections should use an indexed format |
| FlatGeobuf | yes | indexed bbox + exact predicate | yes | Spatial index is used before exact geometry filtering |
| Shapefile | yes | exact spatial predicate | yes | Sidecar metadata is inspected where available; shapes are converted where supported |
| GeoParquet | yes | exact `ST_Intersects` | not yet | GeoArrow metadata is restored and queried with GeoDataFusion; direct Rerun geometry logging remains open |
| GeoPackage | generic import | not yet | not yet | Recognized as vector modality but has no SQLite/geometry adapter yet |
| LAS/LAZ | yes | bbox/class/elevation or verified source row | yes | Sequential spatial scan; rendering samples at most 1,000,000 points and records the stride |
| Provider-authored COPC | yes | indexed octree bbox + level/resolution | yes | Native COPC hierarchy supports LOD selection |
| WildDatum-derived COPC | yes | indexed full-resolution bbox | yes | Immutable derivative of LAS/LAZ; current writer cannot author provider-quality LOD hierarchy |
| HDF5/NetCDF-4 | hierarchy + arrays | bounded N-D hyperslab | mapped band/RGB | Arbitrary rank is queryable; rendering currently expects one mapped rank-3 image cube |
| NetCDF-3 | variables + dimensions | bounded N-D slice | mapped band/RGB | Pure-Rust reader currently decodes the whole source variable and rejects variables over the safety budget |
| Zarr v2/v3 directory | hierarchy + arrays | bounded N-D slice | mapped band/RGB | Chunk-aware bounding reads; symbolic links are rejected during fingerprinting |
| ERDDAP tabledap CSV | remote metadata + approved subset | yes after materialization | tabular/time-series or linked profile/trajectory | Requested variables and six comparison operators are validated; generated subsets have no reliable byte estimate |
| ERDDAP tabledap/griddap NetCDF | remote metadata + approved subset | bounded N-D slice after materialization | mapped band/RGB where axes are configured | Index/value axis slices are validated; upstream is live and the local BLAKE3 object is the reproducible byte identity |

All query responses are bounded and persisted as opaque results. Large data are
not serialized into an MCP response. CSV, Parquet, and RO-Crate exports preserve
the result query and transformations; raster regions can also be exported as a
tiled, compressed Cloud-Optimized GeoTIFF.

## Cubes and hyperspectral data

`DatasetManifest.cubes` inventories every detected multidimensional array.
`DatasetManifest.cube` is the optional confirmed scientific interpretation.
Each axis records its role (`x`, `y`, `z`, `time`, `spectral`, `channel`, or
`other`), length, unit, and coordinate path when known.

WildDatum infers common NEON reflectance conventions only when a rank-3
reflectance array and a compatible wavelength coordinate are unambiguous.
Otherwise configure the layer explicitly:

```bash
wilddatum configure-cube view_... \
  --layer-id layer_1 \
  --cube-array /SITE/Reflectance/Reflectance_Data \
  --y-axis 0 --x-axis 1 --spectral-axis 2 \
  --wavelength-dataset /SITE/Reflectance/Metadata/Spectral_Data/Wavelength \
  --red-band 14 --green-band 9 --blue-band 5
```

The equivalent MCP tool is `configure_cube_view`. It supports a single band or
an RGB triple plus optional no-data, scale/offset, bad-band, and explicit display
range values. Without an explicit range, the renderer uses a 2nd–98th
percentile stretch for the derived preview only; source values are unchanged.

A browser image click is translated into a `CubePixel` containing the source X
and Y index and mapped array path. `query_selection` then generates an N-D slice
that fixes the two spatial axes and returns every spectral cell. This is the
agent-readable spectrum behind the displayed pixel, not a value inferred from
the RGB canvas.

## Point clouds and display precision

LAS stores scaled integer coordinates while Rerun renders `f32` positions.
WildDatum subtracts a source-coordinate origin before logging points and records
the origin, LAS scale/offset, sampling stride, and instance mapping in
`EcoViewSpec`.

For an WildDatum-authored sequential point batch, a Rerun instance ID maps to the
zero-based source row by `instance_id × sampling_stride`. The service accepts
that exact-row path only after independently checking the mapping kind, pinned
Rerun version, stride, supplied instance ID, and supplied source index. Rerun's
reported pick position remains useful spatial context, but it is not treated as
the exact logged point coordinate. Unknown/community Rerun layers fall back to
a source-precision spatial envelope.

## Linked trajectories and vertical profiles

`profile_trajectory_v1` is a versioned visualization recipe over a delimited
source, not a new `Modality`. It currently accepts CSV and TSV datasets whose
view layer is tabular, time-series, or trajectory/vector data. A recipe names:

- trajectory and profile identifier fields;
- optional time plus required latitude and longitude fields;
- one vertical field, `positive_up` or `positive_down`, unit, and textual fill
  values; and
- one displayed value field, unit, optional native QC field, accepted QC codes,
  and textual fill values.

Configuration reads the authoritative source, verifies every named column, and
requires finite latitude, longitude, vertical, and value observations. Empty,
configured fill, and non-finite values are omitted. QC-rejected observations
remain visible and selectable in amber but do not participate in the green
trajectory/profile line geometry. Native QC fields are never normalized away.

The map uses Rerun `MapView` with source longitude/latitude in EPSG:4326. The
profile uses the raw vertical and value coordinates in a `Spatial2DView`; a
positive-down axis is displayed downward without changing the stored source
value. Rerun currently applies an equal raw-coordinate aspect, so a temperature
range of a few degrees against thousands of decibars can look like a nearly
vertical line. WildDatum preserves the honest axes instead of inventing a visual
scale transform.

Each finite observation is logged with its zero-based delimited source-record
index as the Rerun instance ID. A browser pick supplies only the entity path,
instance ID, mapping kind, and Rerun version. The service verifies the view,
dataset, layer, entity suffix, mapping stride, pinned version, and source bounds
before creating an exact `SourceRows` query. Results retain original strings in
an envelope shaped like `{source_index, values}`; they do not coerce identifiers,
QC codes, or provider unit rows.

Rendering rejects inputs above 100,000 source records. One viewer pick queries
one row; direct exact-row queries accept at most 10,000 unique indices. The same
RRD and exact-row contract are used by native Rerun and Rerun Web Viewer. General
depth/interval brushing, multiple displayed values, Parquet row identity, and
profile-aware downsampling are not implemented yet.

## Local-source privacy and identity

Files are fingerprinted with streaming BLAKE3. Zarr trees are fingerprinted
from ordered relative paths and file contents. Raw sources remain immutable;
derived COPC/COG assets record their source fingerprint and transformation.
Local absolute paths stay in the private SQLite registry and are never returned
through MCP or the browser API.

## ERDDAP subsets and modalities

ERDDAP is a remote access protocol, not a new local container format. WildDatum
materializes an approved tabledap or griddap expression as CSV or NetCDF and
then uses the same query and Rerun adapters listed above. The source filename in
the manifest preserves `.csv` or `.nc` even though the immutable object-store
name is its extensionless BLAKE3 digest.

ERDDAP `cdm_data_type` metadata supplies conservative modality hints:

| `cdm_data_type` | WildDatum modalities |
|---|---|
| `Grid` | raster, tensor |
| `Trajectory`, `Profile`, `TrajectoryProfile`, `TimeSeriesProfile` | vector, time-series, tabular |
| `TimeSeries` | time-series, tabular |
| `Point` | vector, tabular |
| other/unknown | tabular |

These hints do not invent a visualization grammar. Grid NetCDF uses explicit
cube/axis configuration when metadata is ambiguous. Trajectory and profile CSV
can be queried immediately and configured into the linked recipe above. CF
`cf_role`, `standard_name`, units, axes, and fill values are preserved as
variable-level manifest metadata, but WildDatum does not silently choose a value
or institutional QC policy. Native variables, QC columns, global attributes,
license, and citation remain available rather than being flattened into the
rendered scene.
