import {WebViewer, type EntityItem, type SelectionChangeEvent} from "@rerun-io/web-viewer";
import "./style.css";

const token = new URLSearchParams(window.location.search).get("token");
if (!token) {
  throw new Error("This EcoScope explorer link is missing its launch token.");
}

const status = element("status");
const viewName = element("view-name");
const viewDetails = element("view-details");
const selectionOutput = element("selection");
const viewerContainer = element("viewer");

const apiUrl = (path: string): string => {
  const url = new URL(path, window.location.origin);
  url.searchParams.set("token", token);
  return url.href;
};
const view = await fetchJson(apiUrl("/api/view"));
viewName.textContent = String(view.name ?? "Ecological explorer");
viewDetails.innerHTML = `
  <dt>View</dt><dd>${escapeHtml(String(view.view_id))}</dd>
  <dt>Revision</dt><dd>${escapeHtml(String(view.revision))}</dd>
  <dt>Datasets</dt><dd>${escapeHtml(String(view.dataset_ids?.length ?? 0))}</dd>
  <dt>Layers</dt><dd>${escapeHtml(String(view.layers?.length ?? 0))}</dd>
`;

const viewer = new WebViewer();
await viewer.start(apiUrl("/api/recording.rrd"), viewerContainer, {
  hide_welcome_screen: true,
  width: "100%",
  height: "100%",
  render_backend: "webgl"
});
status.textContent = "Connected · Rerun semantic bridge active";

viewer.on("selection_change", async (event: SelectionChangeEvent) => {
  const selection = semanticSelection(event, view);
  const record = await fetchJson(apiUrl("/api/selection"), {
    method: "POST",
    headers: {"Content-Type": "application/json"},
    body: JSON.stringify({
      selection,
      summary: {source: "rerun_web_viewer", semantic: selection, raw_event: event}
    })
  });
  selectionOutput.textContent = JSON.stringify(record, null, 2);
});

const existingSelection = await fetchJson(apiUrl("/api/selection"));
if (existingSelection.selection) {
  selectionOutput.textContent = JSON.stringify(existingSelection, null, 2);
}

function element(id: string): HTMLElement {
  const value = document.getElementById(id);
  if (!value) throw new Error(`Missing #${id}`);
  return value;
}

async function fetchJson(url: string, init?: RequestInit): Promise<any> {
  const response = await fetch(url, init);
  if (!response.ok) throw new Error(`${response.status}: ${await response.text()}`);
  return response.json();
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>'"]/g, character => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;"
  })[character] ?? character);
}

function semanticSelection(event: SelectionChangeEvent, view: any): Record<string, unknown> {
  const entities = event.items.filter((item): item is EntityItem => item.type === "entity");
  const primary = entities.find(item => item.position !== undefined) ?? entities[0];
  if (!primary) {
    return {
      type: "entities",
      entity_paths: [],
      instance_ids: []
    };
  }
  const layer = (view.layers ?? []).find((candidate: any) =>
    primary.entity_path.includes(`/${String(candidate.id)}/`) ||
    primary.entity_path.endsWith(`/${String(candidate.id)}`)
  );
  const datasetId = String(layer?.dataset_id ?? view.dataset_ids?.[0] ?? "");
  const modality = String(layer?.modality ?? "unknown");
  const position = primary.position;

  if (position && ["hyperspectral", "tensor"].includes(modality)) {
    const encoding = layer?.encoding ?? {};
    const stride = Array.isArray(encoding.preview_stride) ? encoding.preview_stride : [1, 1];
    const x = Math.max(0, Math.floor(position[0] * Number(stride[1] ?? 1)));
    const y = Math.max(0, Math.floor(position[1] * Number(stride[0] ?? 1)));
    const bandIndices = [encoding.band, encoding.red_band, encoding.green_band, encoding.blue_band]
      .filter((band): band is number => Number.isInteger(band));
    return {
      type: "cube_pixel",
      dataset_id: datasetId,
      array_path: String(encoding.cube_array ?? encoding.hdf5_dataset ?? ""),
      x,
      y,
      x_axis: Number(encoding.x_axis ?? 1),
      y_axis: Number(encoding.y_axis ?? 0),
      spectral_axis: Number(encoding.spectral_axis ?? 2),
      displayed_bands: [...new Set(bandIndices)]
    };
  }

  if (position && ["raster", "image"].includes(modality)) {
    const encoding = layer?.encoding ?? {};
    const stride = Array.isArray(encoding.preview_stride) ? encoding.preview_stride : [1, 1];
    const x = Math.max(0, Math.floor(position[0] * Number(stride[1] ?? 1)));
    const y = Math.max(0, Math.floor(position[1] * Number(stride[0] ?? 1)));
    const affine = Array.isArray(encoding.affine_transform)
      ? encoding.affine_transform.map(Number)
      : null;
    return {
      type: "raster_region",
      pixel_bounds: [x, y, x + 1, y + 1],
      world_geometry: affine ? {geojson: pixelPolygon(x, y, affine)} : null,
      band_indices: []
    };
  }

  if (position && modality === "vector") {
    const origin = Array.isArray(layer?.encoding?.coordinate_origin)
      ? layer.encoding.coordinate_origin.map(Number)
      : [0, 0];
    return {
      type: "map_region",
      geometry: {
        geojson: {
          type: "Point",
          coordinates: [position[0] + (origin[0] ?? 0), position[1] + (origin[1] ?? 0)]
        }
      },
      crs: String(layer?.encoding?.crs ?? "source")
    };
  }

  if (position && modality === "point_cloud") {
    const origin = Array.isArray(layer?.encoding?.coordinate_origin)
      ? layer.encoding.coordinate_origin.map(Number)
      : [0, 0, 0];
    const sourcePosition = position.map((coordinate, index) => coordinate + (origin[index] ?? 0));
    const mapping = layer?.encoding?.instance_id_mapping;
    const verifiedSourceIndex = mapping?.kind === "source_stream_stride" &&
      Number.isInteger(primary.instance_id)
      ? Number(primary.instance_id) * Math.max(1, Number(mapping.stride ?? 1))
      : null;
    return {
      type: "point_set",
      dataset_id: datasetId,
      spatial_query: {
        geometry: {type: "Point", coordinates: sourcePosition},
        entity_path: primary.entity_path,
        instance_id: primary.instance_id ?? null,
        source_indices: verifiedSourceIndex === null ? [] : [verifiedSourceIndex],
        coordinate_scale: layer?.encoding?.coordinate_scale ?? null,
        display_sampling_stride: Number(layer?.encoding?.sampling_stride ?? 1),
        source_index_verified: verifiedSourceIndex !== null,
        coordinate_space: String(layer?.encoding?.coordinate_space ?? "rerun_recording"),
        position_semantics: "viewer pick position; exact source row is identified separately when source_index_verified is true",
        recording_position: position,
        coordinate_origin: origin
      },
      estimated_points: 1
    };
  }

  return {
    type: "entities",
    entity_paths: entities.map(item => item.entity_path),
    instance_ids: entities
      .map(item => item.instance_id)
      .filter((id): id is number => id !== undefined)
      .map(String)
  };
}

function pixelPolygon(x: number, y: number, affine: number[]): Record<string, unknown> {
  const world = (column: number, row: number): [number, number] => [
    affine[0] + column * affine[1] + row * affine[2],
    affine[3] + column * affine[4] + row * affine[5]
  ];
  const corners = [world(x, y), world(x + 1, y), world(x + 1, y + 1), world(x, y + 1)];
  corners.push(corners[0]);
  return {type: "Polygon", coordinates: [corners]};
}
