# Format support

EcoScope separates physical encoding from scientific modality. Importing a
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
| EcoScope-derived COPC | yes | indexed full-resolution bbox | yes | Immutable derivative of LAS/LAZ; current writer cannot author provider-quality LOD hierarchy |
| HDF5/NetCDF-4 | hierarchy + arrays | bounded N-D hyperslab | mapped band/RGB | Arbitrary rank is queryable; rendering currently expects one mapped rank-3 image cube |
| NetCDF-3 | variables + dimensions | bounded N-D slice | mapped band/RGB | Pure-Rust reader currently decodes the whole source variable and rejects variables over the safety budget |
| Zarr v2/v3 directory | hierarchy + arrays | bounded N-D slice | mapped band/RGB | Chunk-aware bounding reads; symbolic links are rejected during fingerprinting |

All query responses are bounded and persisted as opaque results. Large data are
not serialized into an MCP response. CSV, Parquet, and RO-Crate exports preserve
the result query and transformations; raster regions can also be exported as a
tiled, compressed Cloud-Optimized GeoTIFF.

## Cubes and hyperspectral data

`DatasetManifest.cubes` inventories every detected multidimensional array.
`DatasetManifest.cube` is the optional confirmed scientific interpretation.
Each axis records its role (`x`, `y`, `z`, `time`, `spectral`, `channel`, or
`other`), length, unit, and coordinate path when known.

EcoScope infers common NEON reflectance conventions only when a rank-3
reflectance array and a compatible wavelength coordinate are unambiguous.
Otherwise configure the layer explicitly:

```bash
ecoscope configure-cube view_... \
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
EcoScope subtracts a source-coordinate origin before logging points and records
the origin, LAS scale/offset, sampling stride, and instance mapping in
`EcoViewSpec`.

For an EcoScope-authored sequential point batch, a Rerun instance ID maps to the
zero-based source row by `instance_id × sampling_stride`. The service accepts
that exact-row path only after independently checking the mapping kind, pinned
Rerun version, stride, supplied instance ID, and supplied source index. Rerun's
reported pick position remains useful spatial context, but it is not treated as
the exact logged point coordinate. Unknown/community Rerun layers fall back to
a source-precision spatial envelope.

## Local-source privacy and identity

Files are fingerprinted with streaming BLAKE3. Zarr trees are fingerprinted
from ordered relative paths and file contents. Raw sources remain immutable;
derived COPC/COG assets record their source fingerprint and transformation.
Local absolute paths stay in the private SQLite registry and are never returned
through MCP or the browser API.
