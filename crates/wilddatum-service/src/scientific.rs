use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use wilddatum_core::{
    AxisRole, CoordinateSummary, DatasetId, DatasetManifest, InferenceConfidence, LinkExactness,
    LocalAssetInspection, Modality, ProviderKind, Result, SCIENTIFIC_INVENTORY_VERSION,
    ScientificAxis, ScientificComponent, ScientificComponentKind, ScientificInventory,
    ScientificRole, SemanticEvidence, SuggestedLink, SuggestedPanel, SuggestedPanelKind,
    VIEW_SUGGESTION_VERSION, ViewSuggestion, ViewSuggestionSet, WildDatumError,
};

use crate::WildDatumService;

const MAX_COMPONENTS: usize = 256;
const MAX_DATASETS_PER_SUGGESTION_CALL: usize = 8;
const MAX_SUGGESTIONS: usize = 12;

impl WildDatumService {
    pub fn scientific_inventory(&self, dataset_id: &str) -> Result<ScientificInventory> {
        let manifest = self.get_manifest(dataset_id)?;
        let mut warnings = Vec::new();
        let local_inspection = if manifest.provider == ProviderKind::Local {
            manifest
                .source_files
                .first()
                .map(|source| self.inspect_asset(&source.asset_id.0))
                .transpose()
                .map_err(|error| {
                    WildDatumError::Invalid(format!(
                        "cannot inspect local scientific metadata for {dataset_id}: {error}"
                    ))
                })?
        } else {
            None
        };

        if let Some(inspection) = &local_inspection {
            warnings.extend(inspection.warnings.iter().cloned());
        }

        let mut components = Vec::new();
        append_provider_fields(&manifest, &mut components);
        if let Some(inspection) = &local_inspection {
            append_local_fields(inspection, &mut components);
        }
        append_point_positions(&manifest, &mut components);
        append_cubes(&manifest, &mut components);
        append_generic_components(&manifest, &mut components);

        components.sort_by(|left, right| left.id.cmp(&right.id));
        components.dedup_by(|left, right| left.id == right.id);
        if components.len() > MAX_COMPONENTS {
            components.truncate(MAX_COMPONENTS);
            warnings.push(format!(
                "Scientific inventory was truncated to {MAX_COMPONENTS} components"
            ));
        }

        add_field_relationships(&mut components);
        let mut unresolved_decisions = Vec::new();
        if manifest.modalities.contains(&Modality::PointCloud)
            && manifest.spatial_reference.is_none()
        {
            unresolved_decisions.push(
                "Confirm the point-cloud coordinate reference system before spatially linking it to another dataset"
                    .into(),
            );
        }
        for component in &components {
            if component.kind == ScientificComponentKind::Array {
                let roles = component
                    .axes
                    .iter()
                    .map(|axis| &axis.role)
                    .collect::<Vec<_>>();
                if !roles.contains(&&ScientificRole::HorizontalX)
                    || !roles.contains(&&ScientificRole::HorizontalY)
                {
                    unresolved_decisions.push(format!(
                        "Confirm horizontal axes for array {}",
                        component.label
                    ));
                }
                if roles.contains(&&ScientificRole::Spectral)
                    && component
                        .axes
                        .iter()
                        .find(|axis| axis.role == ScientificRole::Spectral)
                        .is_some_and(|axis| axis.coordinate_summary.is_none())
                {
                    unresolved_decisions.push(format!(
                        "Confirm wavelength coordinates for array {}",
                        component.label
                    ));
                }
            }
        }
        unresolved_decisions.sort();
        unresolved_decisions.dedup();
        warnings.sort();
        warnings.dedup();

        Ok(ScientificInventory {
            version: SCIENTIFIC_INVENTORY_VERSION,
            dataset_id: manifest.dataset_id,
            provider: manifest.provider,
            resource_id: manifest.resource_id,
            format: manifest.format.map(|format| format.name),
            modalities: manifest.modalities,
            components,
            spatial_reference: manifest.spatial_reference,
            unresolved_decisions,
            warnings,
        })
    }

    pub fn suggest_views(&self, dataset_ids: &[String]) -> Result<ViewSuggestionSet> {
        if dataset_ids.is_empty() {
            return Err(WildDatumError::Invalid(
                "suggest_views requires at least one dataset ID".into(),
            ));
        }
        if dataset_ids.len() > MAX_DATASETS_PER_SUGGESTION_CALL {
            return Err(WildDatumError::Invalid(format!(
                "suggest_views accepts at most {MAX_DATASETS_PER_SUGGESTION_CALL} dataset IDs"
            )));
        }
        let unique = dataset_ids.iter().collect::<BTreeSet<_>>();
        if unique.len() != dataset_ids.len() {
            return Err(WildDatumError::Invalid(
                "suggest_views dataset IDs must be unique".into(),
            ));
        }

        let inventories = dataset_ids
            .iter()
            .map(|dataset_id| self.scientific_inventory(dataset_id))
            .collect::<Result<Vec<_>>>()?;
        let mut suggestions = individual_suggestions(&inventories);
        suggestions.extend(multimodal_suggestions(&inventories));
        for suggestion in &mut suggestions {
            suggestion.suggestion_id = suggestion_id(suggestion);
        }
        suggestions.sort_by(|left, right| {
            right
                .confidence
                .cmp(&left.confidence)
                .then_with(|| right.panels.len().cmp(&left.panels.len()))
                .then_with(|| left.suggestion_id.cmp(&right.suggestion_id))
        });
        suggestions.truncate(MAX_SUGGESTIONS);

        Ok(ViewSuggestionSet {
            version: VIEW_SUGGESTION_VERSION,
            dataset_ids: inventories
                .iter()
                .map(|inventory| inventory.dataset_id.clone())
                .collect(),
            suggestions,
        })
    }
}

fn append_provider_fields(manifest: &DatasetManifest, components: &mut Vec<ScientificComponent>) {
    let Some(variables) = manifest
        .provider_metadata
        .get("variables")
        .and_then(Value::as_object)
    else {
        return;
    };
    let mut names = variables.keys().collect::<Vec<_>>();
    names.sort();
    for name in names.into_iter().take(MAX_COMPONENTS) {
        let variable = &variables[name];
        let attributes = variable
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let (roles, evidence) = roles_from_authoritative_metadata(name, &attributes);
        let unit = attribute_string(&attributes, "units");
        let summary = attribute_string(&attributes, "actual_range")
            .and_then(|range| numeric_range(&range))
            .map(|(minimum, maximum)| CoordinateSummary {
                count: 0,
                minimum: Some(minimum),
                maximum: Some(maximum),
                unit: unit.clone(),
            });
        let mut relationships = BTreeMap::new();
        if let Some(ancillary) = attribute_string(&attributes, "ancillary_variables") {
            relationships.insert("quality_control".into(), component_id("field", &ancillary));
        }
        let label = attribute_string(&attributes, "long_name").unwrap_or_else(|| name.clone());
        let mut metadata = selected_field_metadata(&attributes);
        metadata.insert("source_name".into(), json!(name));
        components.push(ScientificComponent {
            id: component_id("field", name),
            label,
            kind: ScientificComponentKind::Field,
            source_pointer: Some(format!("provider_metadata.variables.{name}")),
            data_type: variable
                .get("data_type")
                .and_then(Value::as_str)
                .map(str::to_owned),
            shape: vec![],
            roles,
            axes: vec![],
            unit,
            coordinate_summary: summary,
            relationships,
            metadata,
            evidence,
        });
    }
}

fn append_local_fields(
    inspection: &LocalAssetInspection,
    components: &mut Vec<ScientificComponent>,
) {
    for field in inspection.fields.iter().take(MAX_COMPONENTS) {
        if components.iter().any(|component| {
            component.kind == ScientificComponentKind::Field && component.label == *field
        }) {
            continue;
        }
        let (roles, evidence) = roles_from_field_name(field);
        components.push(ScientificComponent {
            id: component_id("field", field),
            label: field.clone(),
            kind: ScientificComponentKind::Field,
            source_pointer: Some(format!("fields.{field}")),
            data_type: None,
            shape: inspection.dimensions.first().copied().into_iter().collect(),
            roles,
            axes: vec![],
            unit: None,
            coordinate_summary: None,
            relationships: BTreeMap::new(),
            metadata: BTreeMap::new(),
            evidence,
        });
    }
}

fn append_point_positions(manifest: &DatasetManifest, components: &mut Vec<ScientificComponent>) {
    if !manifest.modalities.contains(&Modality::PointCloud) {
        return;
    }
    let point_count = source_metadata_value(manifest, "point_count").and_then(Value::as_u64);
    let bounds = source_metadata_value(manifest, "bounds").cloned();
    let mut evidence = vec![SemanticEvidence {
        source: "manifest_modality".into(),
        pointer: Some("modalities.point_cloud".into()),
        statement: "The dataset is explicitly classified as a point cloud".into(),
        confidence: InferenceConfidence::High,
    }];
    if point_count.is_some() || bounds.is_some() {
        evidence.push(SemanticEvidence {
            source: "file_header".into(),
            pointer: Some("source_files.metadata".into()),
            statement: "Point count and coordinate bounds were read from source metadata".into(),
            confidence: InferenceConfidence::High,
        });
    }
    let mut metadata = BTreeMap::new();
    if let Some(point_count) = point_count {
        metadata.insert("point_count".into(), json!(point_count));
    }
    if let Some(bounds) = bounds {
        metadata.insert("bounds".into(), bounds);
    }
    components.push(ScientificComponent {
        id: "point_positions".into(),
        label: "Point positions".into(),
        kind: ScientificComponentKind::PointPositions,
        source_pointer: Some("point_cloud.points".into()),
        data_type: None,
        shape: point_count.into_iter().collect(),
        roles: vec![
            ScientificRole::PointPosition,
            ScientificRole::HorizontalX,
            ScientificRole::HorizontalY,
            ScientificRole::Vertical,
        ],
        axes: vec![],
        unit: None,
        coordinate_summary: None,
        relationships: BTreeMap::new(),
        metadata,
        evidence,
    });
}

fn append_cubes(manifest: &DatasetManifest, components: &mut Vec<ScientificComponent>) {
    for cube in manifest.cubes.iter().take(64) {
        let mut relationships = BTreeMap::new();
        let axes = cube
            .axes
            .iter()
            .enumerate()
            .map(|(index, axis)| {
                let role = role_from_axis(&axis.role);
                let confidence = if axis.role == AxisRole::Other {
                    InferenceConfidence::Low
                } else {
                    InferenceConfidence::High
                };
                let coordinate_summary = coordinate_summary(manifest, axis);
                if role == ScientificRole::Spectral
                    && let Some(path) = &axis.coordinate_path
                {
                    relationships.insert("spectral_coordinate".into(), path.clone());
                }
                ScientificAxis {
                    index: index as u32,
                    name: axis.name.clone(),
                    role,
                    length: axis.length,
                    unit: axis.unit.clone(),
                    coordinate_path: axis.coordinate_path.clone(),
                    coordinate_summary,
                    evidence: vec![SemanticEvidence {
                        source: "array_metadata".into(),
                        pointer: axis.coordinate_path.clone(),
                        statement: format!("Axis {} is classified as {:?}", axis.name, axis.role),
                        confidence,
                    }],
                }
            })
            .collect::<Vec<_>>();
        let mut metadata = BTreeMap::new();
        metadata.insert("chunk_shape".into(), json!(cube.chunk_shape));
        if let Some(value) = cube.scale_factor {
            metadata.insert("scale_factor".into(), json!(value));
        }
        if let Some(value) = cube.add_offset {
            metadata.insert("add_offset".into(), json!(value));
        }
        if let Some(value) = cube.no_data {
            metadata.insert("no_data".into(), json!(value));
        }
        if let Some(spectral_axis) = axes
            .iter()
            .find(|axis| axis.role == ScientificRole::Spectral)
            && let Some(bands) = rgb_bands_from_manifest(manifest, spectral_axis)
        {
            metadata.insert("suggested_rgb_bands".into(), json!(bands));
            metadata.insert(
                "suggested_rgb_wavelengths_nm".into(),
                json!([650.0, 550.0, 450.0]),
            );
        }
        components.push(ScientificComponent {
            id: component_id("array", &cube.array_path),
            label: cube
                .array_path
                .rsplit('/')
                .find(|part| !part.is_empty())
                .unwrap_or(&cube.array_path)
                .to_owned(),
            kind: ScientificComponentKind::Array,
            source_pointer: Some(cube.array_path.clone()),
            data_type: Some(cube.data_type.clone()),
            shape: cube.axes.iter().map(|axis| axis.length).collect(),
            roles: vec![ScientificRole::Value],
            axes,
            unit: cube
                .attributes
                .get("Units")
                .or_else(|| cube.attributes.get("units"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            coordinate_summary: None,
            relationships,
            metadata,
            evidence: vec![SemanticEvidence {
                source: "array_inventory".into(),
                pointer: Some(cube.array_path.clone()),
                statement: "Array shape and axes were inspected from the source container".into(),
                confidence: InferenceConfidence::High,
            }],
        });
    }
}

fn append_generic_components(
    manifest: &DatasetManifest,
    components: &mut Vec<ScientificComponent>,
) {
    let has_array = components
        .iter()
        .any(|component| component.kind == ScientificComponentKind::Array);
    let has_geometry = components
        .iter()
        .any(|component| component.kind == ScientificComponentKind::Geometry);
    if manifest.modalities.contains(&Modality::Raster) && !has_array {
        components.push(generic_component(
            "raster_pixels",
            "Raster pixels",
            ScientificComponentKind::Array,
            vec![
                ScientificRole::Value,
                ScientificRole::HorizontalX,
                ScientificRole::HorizontalY,
            ],
            "manifest_modality",
        ));
    }
    if manifest.modalities.contains(&Modality::Vector) && !has_geometry {
        components.push(generic_component(
            "vector_geometry",
            "Vector geometry",
            ScientificComponentKind::Geometry,
            vec![ScientificRole::Geometry],
            "manifest_modality",
        ));
    }
}

fn generic_component(
    id: &str,
    label: &str,
    kind: ScientificComponentKind,
    roles: Vec<ScientificRole>,
    source: &str,
) -> ScientificComponent {
    ScientificComponent {
        id: id.into(),
        label: label.into(),
        kind,
        source_pointer: None,
        data_type: None,
        shape: vec![],
        roles,
        axes: vec![],
        unit: None,
        coordinate_summary: None,
        relationships: BTreeMap::new(),
        metadata: BTreeMap::new(),
        evidence: vec![SemanticEvidence {
            source: source.into(),
            pointer: None,
            statement: format!("{label} are declared by the dataset modality"),
            confidence: InferenceConfidence::Medium,
        }],
    }
}

fn add_field_relationships(components: &mut [ScientificComponent]) {
    let fields = components
        .iter()
        .filter(|component| component.kind == ScientificComponentKind::Field)
        .map(|component| (component.label.to_ascii_lowercase(), component.id.clone()))
        .collect::<BTreeMap<_, _>>();
    for component in components {
        if component.kind != ScientificComponentKind::Field {
            continue;
        }
        let lower = component.label.to_ascii_lowercase();
        if component.roles.contains(&ScientificRole::QualityControl) {
            let base = lower
                .strip_suffix("_qc")
                .or_else(|| lower.strip_suffix("qc"))
                .map(|base| base.trim_end_matches('_'));
            if let Some(base) = base.and_then(|base| fields.get(base)) {
                component
                    .relationships
                    .insert("quality_for".into(), base.clone());
            }
        } else if let Some(qc) = fields
            .get(&format!("{lower}_qc"))
            .or_else(|| fields.get(&format!("{lower}qc")))
        {
            component
                .relationships
                .insert("quality_control".into(), qc.clone());
        }
    }
}

fn roles_from_authoritative_metadata(
    name: &str,
    attributes: &Map<String, Value>,
) -> (Vec<ScientificRole>, Vec<SemanticEvidence>) {
    let mut roles = Vec::new();
    let mut statements = Vec::new();
    let standard_name = attribute_string(attributes, "standard_name")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let axis = attribute_string(attributes, "axis")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let coordinate_axis = attribute_string(attributes, "_CoordinateAxisType")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let variable_type = attribute_string(attributes, "variable_type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if standard_name == "time" || axis == "T" || coordinate_axis == "time" {
        push_role(&mut roles, ScientificRole::Time);
        statements.push("CF/axis metadata identifies time");
    }
    if standard_name == "latitude" || coordinate_axis == "lat" {
        push_role(&mut roles, ScientificRole::Latitude);
        statements.push("CF metadata identifies latitude");
    }
    if standard_name == "longitude" || coordinate_axis == "lon" {
        push_role(&mut roles, ScientificRole::Longitude);
        statements.push("CF metadata identifies longitude");
    }
    if standard_name == "depth"
        || standard_name.contains("pressure")
        || axis == "Z"
        || coordinate_axis == "height"
    {
        push_role(&mut roles, ScientificRole::Vertical);
        statements.push("CF/axis metadata identifies a vertical coordinate");
    }
    if variable_type == "quality_control" || name.to_ascii_lowercase().ends_with("_qc") {
        push_role(&mut roles, ScientificRole::QualityControl);
        statements.push("Provider metadata identifies a quality-control field");
    }
    if attributes.contains_key("cf_role") {
        push_role(&mut roles, ScientificRole::Identifier);
        statements.push("CF role metadata identifies a sampling-geometry identifier");
    }
    if variable_type == "environmental"
        || (!standard_name.is_empty()
            && !roles.iter().any(|role| {
                matches!(
                    role,
                    ScientificRole::Time
                        | ScientificRole::Latitude
                        | ScientificRole::Longitude
                        | ScientificRole::Vertical
                        | ScientificRole::Identifier
                        | ScientificRole::QualityControl
                )
            }))
    {
        push_role(&mut roles, ScientificRole::Value);
        statements.push("Provider metadata identifies a measured scientific value");
    }
    if roles.is_empty() {
        let (fallback, fallback_evidence) = roles_from_field_name(name);
        return (fallback, fallback_evidence);
    }
    if !roles.iter().any(|role| {
        matches!(
            role,
            ScientificRole::Time
                | ScientificRole::Latitude
                | ScientificRole::Longitude
                | ScientificRole::Vertical
                | ScientificRole::Identifier
                | ScientificRole::QualityControl
        )
    }) {
        push_role(&mut roles, ScientificRole::Value);
    }
    let evidence = statements
        .into_iter()
        .map(|statement| SemanticEvidence {
            source: "provider_metadata".into(),
            pointer: Some(format!("variables.{name}.attributes")),
            statement: statement.into(),
            confidence: InferenceConfidence::High,
        })
        .collect();
    (roles, evidence)
}

fn roles_from_field_name(field: &str) -> (Vec<ScientificRole>, Vec<SemanticEvidence>) {
    let lower = field.to_ascii_lowercase();
    let normalized = lower.trim_matches(|character: char| !character.is_ascii_alphanumeric());
    let (role, confidence, statement) = if matches!(
        normalized,
        "time" | "timestamp" | "datetime" | "date"
    ) || lower.ends_with("_time")
    {
        (
            ScientificRole::Time,
            InferenceConfidence::Medium,
            "Field name suggests a temporal coordinate",
        )
    } else if matches!(normalized, "lat" | "latitude") {
        (
            ScientificRole::Latitude,
            InferenceConfidence::Medium,
            "Field name suggests latitude",
        )
    } else if matches!(normalized, "lon" | "lng" | "longitude") {
        (
            ScientificRole::Longitude,
            InferenceConfidence::Medium,
            "Field name suggests longitude",
        )
    } else if matches!(
        normalized,
        "depth" | "pressure" | "pres" | "height" | "elevation" | "altitude"
    ) {
        (
            ScientificRole::Vertical,
            InferenceConfidence::Medium,
            "Field name suggests a vertical coordinate",
        )
    } else if lower.ends_with("_qc") || lower.contains("quality_control") || normalized == "qc" {
        (
            ScientificRole::QualityControl,
            InferenceConfidence::Medium,
            "Field name suggests a quality-control flag",
        )
    } else if lower.contains("uncert") || lower.ends_with("_error") || lower.ends_with("_std") {
        (
            ScientificRole::Uncertainty,
            InferenceConfidence::Low,
            "Field name suggests uncertainty",
        )
    } else if lower.ends_with("_id")
        || lower.contains("identifier")
        || ["site", "station", "platform", "profile", "cycle"]
            .iter()
            .any(|term| lower.contains(term))
    {
        (
            ScientificRole::Identifier,
            InferenceConfidence::Low,
            "Field name suggests an identifier",
        )
    } else {
        (
            ScientificRole::Value,
            InferenceConfidence::Low,
            "Field is a candidate scientific value; no authoritative semantic metadata is available",
        )
    };
    (
        vec![role],
        vec![SemanticEvidence {
            source: "field_name_heuristic".into(),
            pointer: Some(format!("fields.{field}")),
            statement: statement.into(),
            confidence,
        }],
    )
}

fn role_from_axis(role: &AxisRole) -> ScientificRole {
    match role {
        AxisRole::X => ScientificRole::HorizontalX,
        AxisRole::Y => ScientificRole::HorizontalY,
        AxisRole::Z => ScientificRole::Vertical,
        AxisRole::Time => ScientificRole::Time,
        AxisRole::Spectral => ScientificRole::Spectral,
        AxisRole::Channel => ScientificRole::Channel,
        AxisRole::Other => ScientificRole::Other("unresolved_axis".into()),
    }
}

fn coordinate_summary(
    manifest: &DatasetManifest,
    axis: &wilddatum_core::CubeAxis,
) -> Option<CoordinateSummary> {
    if let Some(path) = &axis.coordinate_path
        && let Some(values) = coordinate_values(manifest, path)
    {
        let mut minimum = f64::INFINITY;
        let mut maximum = f64::NEG_INFINITY;
        let mut count = 0_u64;
        for value in values {
            if let Some(value) = value.as_f64()
                && value.is_finite()
            {
                minimum = minimum.min(value);
                maximum = maximum.max(value);
                count += 1;
            }
        }
        if count > 0 {
            return Some(CoordinateSummary {
                count,
                minimum: Some(minimum),
                maximum: Some(maximum),
                unit: axis.unit.clone(),
            });
        }
    }
    axis.regular_start.map(|start| {
        let maximum = axis
            .regular_step
            .map(|step| start + step * axis.length.saturating_sub(1) as f64)
            .unwrap_or(start);
        CoordinateSummary {
            count: axis.length,
            minimum: Some(start.min(maximum)),
            maximum: Some(start.max(maximum)),
            unit: axis.unit.clone(),
        }
    })
}

fn coordinate_values<'a>(manifest: &'a DatasetManifest, path: &str) -> Option<&'a Vec<Value>> {
    for source in &manifest.source_files {
        for inventory_key in ["hdf5_datasets", "netcdf_variables"] {
            let Some(entries) = source.metadata.get(inventory_key).and_then(Value::as_array) else {
                continue;
            };
            if let Some(values) = entries.iter().find_map(|entry| {
                (entry.get("path").and_then(Value::as_str) == Some(path))
                    .then(|| entry.get("coordinate_values").and_then(Value::as_array))
                    .flatten()
            }) {
                return Some(values);
            }
        }
    }
    None
}

fn source_metadata_value<'a>(manifest: &'a DatasetManifest, key: &str) -> Option<&'a Value> {
    manifest
        .source_files
        .iter()
        .find_map(|source| source.metadata.get(key))
}

fn attribute_string(attributes: &Map<String, Value>, key: &str) -> Option<String> {
    attributes.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn numeric_range(value: &str) -> Option<(f64, f64)> {
    let values = value
        .split([',', ' '])
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    Some((*values.first()?, *values.last()?))
}

fn selected_field_metadata(attributes: &Map<String, Value>) -> BTreeMap<String, Value> {
    [
        "standard_name",
        "long_name",
        "cf_role",
        "axis",
        "positive",
        "variable_type",
        "ancillary_variables",
        "flag_values",
        "flag_meanings",
    ]
    .into_iter()
    .filter_map(|key| {
        attributes
            .get(key)
            .cloned()
            .map(|value| (key.into(), value))
    })
    .collect()
}

fn push_role(roles: &mut Vec<ScientificRole>, role: ScientificRole) {
    if !roles.contains(&role) {
        roles.push(role);
    }
}

fn component_id(prefix: &str, value: &str) -> String {
    let digest = blake3::hash(value.as_bytes()).to_hex();
    format!("{prefix}_{}", &digest[..16])
}

fn individual_suggestions(inventories: &[ScientificInventory]) -> Vec<ViewSuggestion> {
    let mut suggestions = Vec::new();
    for inventory in inventories {
        for component in &inventory.components {
            match component.kind {
                ScientificComponentKind::PointPositions => {
                    suggestions.push(point_cloud_suggestion(inventory, component));
                }
                ScientificComponentKind::Array if spectral_axes(component).is_some() => {
                    suggestions.push(spectral_cube_suggestion(inventory, component));
                }
                ScientificComponentKind::Geometry => {
                    suggestions.push(vector_suggestion(inventory, component));
                }
                _ => {}
            }
        }
        if inventory
            .components
            .iter()
            .any(|component| component.kind == ScientificComponentKind::Field)
        {
            suggestions.push(table_suggestion(inventory));
        }
    }
    suggestions
}

fn point_cloud_suggestion(
    inventory: &ScientificInventory,
    component: &ScientificComponent,
) -> ViewSuggestion {
    ViewSuggestion {
        suggestion_id: String::new(),
        recipe: "point_cloud_3d_v1".into(),
        title: format!("{} point cloud", inventory.resource_id),
        description: "Render bounded point positions in a Rerun 3D spatial panel with exact source-instance mapping where supported".into(),
        dataset_ids: vec![inventory.dataset_id.clone()],
        confidence: InferenceConfidence::High,
        panels: vec![SuggestedPanel {
            id: panel_id("point_cloud", &inventory.dataset_id),
            kind: SuggestedPanelKind::Spatial3d,
            dataset_id: inventory.dataset_id.clone(),
            component_id: Some(component.id.clone()),
            representation: "point_cloud".into(),
            encoding: BTreeMap::new(),
        }],
        links: vec![],
        evidence: component.evidence.clone(),
        unresolved_decisions: inventory.unresolved_decisions.clone(),
        warnings: inventory.warnings.clone(),
    }
}

fn spectral_cube_suggestion(
    inventory: &ScientificInventory,
    component: &ScientificComponent,
) -> ViewSuggestion {
    let (y_axis, x_axis, spectral_axis) = spectral_axes(component).expect("checked above");
    let bands = rgb_bands(component);
    let mut rgb_encoding = BTreeMap::from([
        (
            "cube_array".into(),
            json!(component.source_pointer.clone().unwrap_or_default()),
        ),
        ("y_axis".into(), json!(y_axis.index)),
        ("x_axis".into(), json!(x_axis.index)),
        ("spectral_axis".into(), json!(spectral_axis.index)),
    ]);
    let mut unresolved = inventory.unresolved_decisions.clone();
    let confidence = if let Some([red, green, blue]) = bands {
        rgb_encoding.insert("red_band".into(), json!(red));
        rgb_encoding.insert("green_band".into(), json!(green));
        rgb_encoding.insert("blue_band".into(), json!(blue));
        InferenceConfidence::High
    } else {
        unresolved.push(format!(
            "Choose RGB or single-band display indices for {}",
            component.label
        ));
        InferenceConfidence::Medium
    };
    if let Some(path) = &spectral_axis.coordinate_path {
        rgb_encoding.insert("wavelength_dataset".into(), json!(path));
    }
    unresolved.sort();
    unresolved.dedup();
    let rgb_panel = panel_id("rgb", &inventory.dataset_id);
    let spectrum_panel = panel_id("spectrum", &inventory.dataset_id);
    ViewSuggestion {
        suggestion_id: String::new(),
        recipe: "spectral_cube_v1".into(),
        title: format!("{} RGB and spectrum", inventory.resource_id),
        description: "Render a wavelength-aware image and query the exact source spectrum behind a selected cube pixel".into(),
        dataset_ids: vec![inventory.dataset_id.clone()],
        confidence,
        panels: vec![
            SuggestedPanel {
                id: rgb_panel.clone(),
                kind: SuggestedPanelKind::Spatial2d,
                dataset_id: inventory.dataset_id.clone(),
                component_id: Some(component.id.clone()),
                representation: "rgb".into(),
                encoding: rgb_encoding,
            },
            SuggestedPanel {
                id: spectrum_panel.clone(),
                kind: SuggestedPanelKind::Profile,
                dataset_id: inventory.dataset_id.clone(),
                component_id: Some(component.id.clone()),
                representation: "spectrum".into(),
                encoding: BTreeMap::from([
                    ("spectral_axis".into(), json!(spectral_axis.index)),
                    (
                        "wavelength_dataset".into(),
                        json!(spectral_axis.coordinate_path),
                    ),
                ]),
            },
        ],
        links: vec![SuggestedLink {
            source_panel: rgb_panel,
            source_selection: "cube_pixel".into(),
            target_panel: spectrum_panel,
            resolver: "cube_pixel_to_spectrum".into(),
            exactness: LinkExactness::Exact,
            explanation: "The selected pixel fixes the two explicit spatial axes and queries every cell on the explicit spectral axis".into(),
        }],
        evidence: component.evidence.clone(),
        unresolved_decisions: unresolved,
        warnings: inventory.warnings.clone(),
    }
}

fn vector_suggestion(
    inventory: &ScientificInventory,
    component: &ScientificComponent,
) -> ViewSuggestion {
    ViewSuggestion {
        suggestion_id: String::new(),
        recipe: "vector_map_v1".into(),
        title: format!("{} map", inventory.resource_id),
        description: "Render source geometry in a map or spatial panel".into(),
        dataset_ids: vec![inventory.dataset_id.clone()],
        confidence: if inventory.spatial_reference.is_some() {
            InferenceConfidence::High
        } else {
            InferenceConfidence::Medium
        },
        panels: vec![SuggestedPanel {
            id: panel_id("map", &inventory.dataset_id),
            kind: SuggestedPanelKind::Map,
            dataset_id: inventory.dataset_id.clone(),
            component_id: Some(component.id.clone()),
            representation: "geometry".into(),
            encoding: BTreeMap::new(),
        }],
        links: vec![],
        evidence: component.evidence.clone(),
        unresolved_decisions: inventory.unresolved_decisions.clone(),
        warnings: inventory.warnings.clone(),
    }
}

fn table_suggestion(inventory: &ScientificInventory) -> ViewSuggestion {
    let time = inventory.components.iter().find(|component| {
        component.kind == ScientificComponentKind::Field
            && component.roles.contains(&ScientificRole::Time)
    });
    let values = inventory
        .components
        .iter()
        .filter(|component| {
            component.kind == ScientificComponentKind::Field
                && component.roles.contains(&ScientificRole::Value)
        })
        .take(8)
        .map(field_source_name)
        .collect::<Vec<_>>();
    let mut encoding = BTreeMap::new();
    let (kind, representation, confidence, description) = if let Some(time) = time {
        encoding.insert("time_field".into(), json!(field_source_name(time)));
        encoding.insert("value_fields".into(), json!(values));
        (
            SuggestedPanelKind::TimeSeries,
            "time_series",
            InferenceConfidence::Medium,
            "Plot candidate value fields against the inferred time coordinate",
        )
    } else {
        (
            SuggestedPanelKind::Table,
            "table",
            InferenceConfidence::High,
            "Inspect a bounded table preview without inferring an analytical axis",
        )
    };
    ViewSuggestion {
        suggestion_id: String::new(),
        recipe: format!("{representation}_v1"),
        title: format!(
            "{} {}",
            inventory.resource_id,
            representation.replace('_', " ")
        ),
        description: description.into(),
        dataset_ids: vec![inventory.dataset_id.clone()],
        confidence,
        panels: vec![SuggestedPanel {
            id: panel_id(representation, &inventory.dataset_id),
            kind,
            dataset_id: inventory.dataset_id.clone(),
            component_id: time.map(|component| component.id.clone()),
            representation: representation.into(),
            encoding,
        }],
        links: vec![],
        evidence: time
            .map(|component| component.evidence.clone())
            .unwrap_or_default(),
        unresolved_decisions: inventory.unresolved_decisions.clone(),
        warnings: inventory.warnings.clone(),
    }
}

fn field_source_name(component: &ScientificComponent) -> String {
    component
        .metadata
        .get("source_name")
        .and_then(Value::as_str)
        .unwrap_or(&component.label)
        .to_owned()
}

fn multimodal_suggestions(inventories: &[ScientificInventory]) -> Vec<ViewSuggestion> {
    let point_candidates = inventories
        .iter()
        .flat_map(|inventory| {
            inventory
                .components
                .iter()
                .filter(|component| component.kind == ScientificComponentKind::PointPositions)
                .map(move |component| (inventory, component))
        })
        .collect::<Vec<_>>();
    let cube_candidates = inventories
        .iter()
        .flat_map(|inventory| {
            inventory
                .components
                .iter()
                .filter(|component| spectral_axes(component).is_some())
                .map(move |component| (inventory, component))
        })
        .collect::<Vec<_>>();
    let mut suggestions = Vec::new();
    for (point_inventory, point_component) in point_candidates {
        for (cube_inventory, cube_component) in &cube_candidates {
            if point_inventory.dataset_id == cube_inventory.dataset_id {
                continue;
            }
            let point = point_cloud_suggestion(point_inventory, point_component);
            let cube = spectral_cube_suggestion(cube_inventory, cube_component);
            let point_panel = point.panels[0].clone();
            let rgb_panel = cube.panels[0].clone();
            let spectrum_panel = cube.panels[1].clone();
            let spatial_link_verified = point_inventory.spatial_reference.is_some()
                && cube_inventory.spatial_reference.is_some()
                && cube_component.metadata.contains_key("world_to_pixel");
            let mut unresolved = point.unresolved_decisions;
            unresolved.extend(cube.unresolved_decisions);
            if !spatial_link_verified {
                unresolved.push(
                    "Verify compatible point-cloud CRS and cube world-to-pixel affine metadata before enabling point-to-pixel linking"
                        .into(),
                );
            }
            unresolved.sort();
            unresolved.dedup();
            let mut warnings = point.warnings;
            warnings.extend(cube.warnings);
            warnings.sort();
            warnings.dedup();
            let mut evidence = point.evidence;
            evidence.extend(cube.evidence);
            suggestions.push(ViewSuggestion {
                suggestion_id: String::new(),
                recipe: "point_cloud_spectral_cube_v1".into(),
                title: "Linked point cloud, RGB cube, and spectrum".into(),
                description: "Compare 3D point positions with wavelength-aware imagery and inspect exact cube-pixel spectra; spatial cross-dataset linking activates only after georegistration is verified".into(),
                dataset_ids: vec![
                    point_inventory.dataset_id.clone(),
                    cube_inventory.dataset_id.clone(),
                ],
                // The three panels are independently supported with high
                // confidence. The unavailable link below, rather than the
                // suggestion confidence, communicates missing registration.
                confidence: InferenceConfidence::High,
                panels: vec![point_panel.clone(), rgb_panel.clone(), spectrum_panel],
                links: vec![
                    cube.links[0].clone(),
                    SuggestedLink {
                        source_panel: point_panel.id,
                        source_selection: "world_point".into(),
                        target_panel: rgb_panel.id,
                        resolver: "world_to_raster_pixel".into(),
                        exactness: if spatial_link_verified {
                            LinkExactness::Exact
                        } else {
                            LinkExactness::Unavailable
                        },
                        explanation: if spatial_link_verified {
                            "Verified CRS and affine metadata define the point-to-pixel transform"
                                .into()
                        } else {
                            "A point-cloud CRS and cube world-to-pixel affine transform have not both been verified"
                                .into()
                        },
                    },
                ],
                evidence,
                unresolved_decisions: unresolved,
                warnings,
            });
        }
    }
    suggestions
}

fn spectral_axes(
    component: &ScientificComponent,
) -> Option<(&ScientificAxis, &ScientificAxis, &ScientificAxis)> {
    if component.kind != ScientificComponentKind::Array {
        return None;
    }
    let y = component
        .axes
        .iter()
        .find(|axis| axis.role == ScientificRole::HorizontalY)?;
    let x = component
        .axes
        .iter()
        .find(|axis| axis.role == ScientificRole::HorizontalX)?;
    let spectral = component
        .axes
        .iter()
        .find(|axis| axis.role == ScientificRole::Spectral)?;
    Some((y, x, spectral))
}

fn rgb_bands(component: &ScientificComponent) -> Option<[u32; 3]> {
    let values = component
        .metadata
        .get("suggested_rgb_bands")
        .and_then(Value::as_array)?;
    let [red, green, blue] = values.as_slice() else {
        return None;
    };
    Some([
        u32::try_from(red.as_u64()?).ok()?,
        u32::try_from(green.as_u64()?).ok()?,
        u32::try_from(blue.as_u64()?).ok()?,
    ])
}

fn rgb_bands_from_manifest(
    manifest: &DatasetManifest,
    spectral_axis: &ScientificAxis,
) -> Option<[u32; 3]> {
    let path = spectral_axis.coordinate_path.as_deref()?;
    let values = coordinate_values(manifest, path)?;
    let factor = wavelength_factor_to_nm(spectral_axis.unit.as_deref()?)?;
    let values = values
        .iter()
        .map(Value::as_f64)
        .collect::<Option<Vec<_>>>()?;
    Some([
        nearest_index(&values, 650.0 / factor)?,
        nearest_index(&values, 550.0 / factor)?,
        nearest_index(&values, 450.0 / factor)?,
    ])
}

fn wavelength_factor_to_nm(unit: &str) -> Option<f64> {
    let normalized = unit.trim().to_ascii_lowercase();
    if matches!(normalized.as_str(), "nm" | "nanometer" | "nanometers") {
        Some(1.0)
    } else if matches!(
        normalized.as_str(),
        "um" | "µm" | "micrometer" | "micrometers"
    ) {
        Some(1_000.0)
    } else {
        None
    }
}

fn nearest_index(values: &[f64], target: f64) -> Option<u32> {
    values
        .iter()
        .enumerate()
        .filter(|(_, value)| value.is_finite())
        .min_by(|(_, left), (_, right)| (*left - target).abs().total_cmp(&(*right - target).abs()))
        .and_then(|(index, _)| u32::try_from(index).ok())
}

fn panel_id(prefix: &str, dataset_id: &DatasetId) -> String {
    let digest = blake3::hash(format!("{prefix}:{}", dataset_id.0).as_bytes()).to_hex();
    format!("{prefix}_{}", &digest[..12])
}

fn suggestion_id(suggestion: &ViewSuggestion) -> String {
    let mut seed = String::new();
    seed.push_str(&suggestion.recipe);
    for dataset_id in &suggestion.dataset_ids {
        seed.push(':');
        seed.push_str(&dataset_id.0);
    }
    for panel in &suggestion.panels {
        seed.push(':');
        seed.push_str(&panel.id);
        if let Some(component_id) = &panel.component_id {
            seed.push(':');
            seed.push_str(component_id);
        }
    }
    let digest = blake3::hash(seed.as_bytes()).to_hex();
    format!("suggest_{}", &digest[..24])
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::Utc;
    use wilddatum_core::{
        AssetId, Checksum, CubeAxis, CubeDescriptor, FormatDescriptor, SourceFile,
    };

    use super::*;
    use crate::ServicePaths;

    fn service() -> (tempfile::TempDir, WildDatumService) {
        let directory = tempfile::tempdir().unwrap();
        let service = WildDatumService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        (directory, service)
    }

    fn source_file(name: &str, metadata: BTreeMap<String, Value>) -> SourceFile {
        SourceFile {
            asset_id: AssetId::new(),
            original_name: name.into(),
            source_uri: format!("fixture://{name}"),
            local_object: None,
            size_bytes: 1,
            checksum: Checksum {
                algorithm: "blake3".into(),
                value: "fixture".into(),
            },
            media_type: None,
            location: None,
            temporal_partition: None,
            metadata,
        }
    }

    fn manifest(
        dataset_id: &str,
        resource_id: &str,
        modalities: Vec<Modality>,
        source: SourceFile,
        cubes: Vec<CubeDescriptor>,
    ) -> DatasetManifest {
        DatasetManifest {
            dataset_id: DatasetId(dataset_id.into()),
            provider: ProviderKind::Other("fixture".into()),
            resource_id: resource_id.into(),
            resource_version: Some("fixture-v1".into()),
            modalities,
            locations: vec![],
            temporal_start: None,
            temporal_end: None,
            release: None,
            package: None,
            include_provisional: false,
            source_files: vec![source],
            transformations: vec![],
            format: Some(FormatDescriptor {
                name: Path::new(resource_id)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("fixture")
                    .into(),
                version: None,
                profile: None,
                options: BTreeMap::new(),
            }),
            spatial_reference: None,
            cube: None,
            cubes,
            license: None,
            citation: None,
            provider_metadata: BTreeMap::new(),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn local_inventory_infers_fields_without_exposing_the_path() {
        let (directory, service) = service();
        let path = directory.path().join("private-observations.csv");
        std::fs::write(
            &path,
            "time,latitude,longitude,depth,temp,temp_qc\n2026-01-01,1,2,3,4,1\n",
        )
        .unwrap();
        let dataset = service.import_local_file(&path).await.unwrap();

        let inventory = service.scientific_inventory(&dataset.dataset_id.0).unwrap();
        let json = serde_json::to_string(&inventory).unwrap();
        assert!(!json.contains(path.to_str().unwrap()));
        assert!(inventory.components.iter().any(|component| {
            component.label == "time" && component.roles.contains(&ScientificRole::Time)
        }));
        let temperature = inventory
            .components
            .iter()
            .find(|component| component.label == "temp")
            .unwrap();
        assert!(temperature.relationships.contains_key("quality_control"));
    }

    #[test]
    fn authoritative_provider_metadata_preserves_source_fields_and_qc_links() {
        let (_directory, service) = service();
        let mut dataset = manifest(
            "ds_table",
            "observations.csv",
            vec![Modality::Tabular, Modality::TimeSeries],
            source_file("observations.csv", BTreeMap::new()),
            vec![],
        );
        dataset.provider_metadata.insert(
            "variables".into(),
            json!({
                "time": {
                    "data_type": "double",
                    "attributes": {"standard_name": "time", "axis": "T", "units": "seconds since 1970-01-01"}
                },
                "TEMP": {
                    "data_type": "double",
                    "attributes": {
                        "standard_name": "sea_water_temperature",
                        "long_name": "sea water temperature",
                        "variable_type": "environmental",
                        "units": "degC",
                        "ancillary_variables": "TEMP_QC"
                    }
                },
                "TEMP_QC": {
                    "data_type": "ubyte",
                    "attributes": {"variable_type": "quality_control"}
                }
            }),
        );
        service.save_manifest(&dataset).unwrap();

        let inventory = service.scientific_inventory("ds_table").unwrap();
        let temperature = inventory
            .components
            .iter()
            .find(|component| component.metadata["source_name"] == "TEMP")
            .unwrap();
        assert!(temperature.roles.contains(&ScientificRole::Value));
        assert_eq!(temperature.unit.as_deref(), Some("degC"));
        assert_eq!(
            temperature.relationships["quality_control"],
            component_id("field", "TEMP_QC")
        );
        assert!(
            temperature
                .evidence
                .iter()
                .all(|evidence| evidence.confidence == InferenceConfidence::High)
        );

        let suggestions = service.suggest_views(&["ds_table".into()]).unwrap();
        let time_series = suggestions
            .suggestions
            .iter()
            .find(|suggestion| suggestion.recipe == "time_series_v1")
            .unwrap();
        assert_eq!(time_series.panels[0].encoding["time_field"], "time");
        assert_eq!(
            time_series.panels[0].encoding["value_fields"],
            json!(["TEMP"])
        );
    }

    #[test]
    fn suggests_neon_shaped_point_cloud_rgb_and_spectrum_deterministically() {
        let (_directory, service) = service();
        let point = manifest(
            "ds_point",
            "tile.las",
            vec![Modality::PointCloud],
            source_file(
                "tile.las",
                BTreeMap::from([
                    ("point_count".into(), json!(6_609_829)),
                    (
                        "bounds".into(),
                        json!({"min": [256000.0, 4111000.0, 384.0], "max": [257000.0, 4112000.0, 511.0]}),
                    ),
                ]),
            ),
            vec![],
        );
        let wavelength_path = "/SITE/Reflectance/Metadata/Wavelength";
        let reflectance_path = "/SITE/Reflectance/Reflectance_Data";
        let cube = manifest(
            "ds_cube",
            "reflectance.h5",
            vec![Modality::Hyperspectral, Modality::Tensor],
            source_file(
                "reflectance.h5",
                BTreeMap::from([(
                    "hdf5_datasets".into(),
                    json!([{
                        "path": wavelength_path,
                        "coordinate_values": [400.0, 450.0, 550.0, 650.0, 800.0]
                    }]),
                )]),
            ),
            vec![CubeDescriptor {
                array_path: reflectance_path.into(),
                data_type: "u16".into(),
                axes: vec![
                    CubeAxis {
                        name: "y".into(),
                        role: AxisRole::Y,
                        length: 500,
                        unit: None,
                        coordinate_path: None,
                        regular_start: None,
                        regular_step: None,
                    },
                    CubeAxis {
                        name: "x".into(),
                        role: AxisRole::X,
                        length: 500,
                        unit: None,
                        coordinate_path: None,
                        regular_start: None,
                        regular_step: None,
                    },
                    CubeAxis {
                        name: "wavelength".into(),
                        role: AxisRole::Spectral,
                        length: 5,
                        unit: Some("nm".into()),
                        coordinate_path: Some(wavelength_path.into()),
                        regular_start: None,
                        regular_step: None,
                    },
                ],
                chunk_shape: vec![500, 500, 5],
                scale_factor: None,
                add_offset: None,
                no_data: None,
                attributes: BTreeMap::new(),
            }],
        );
        service.save_manifest(&point).unwrap();
        service.save_manifest(&cube).unwrap();

        let point_inventory = service.scientific_inventory("ds_point").unwrap();
        assert_eq!(
            point_inventory.components[0].kind,
            ScientificComponentKind::PointPositions
        );
        assert!(point_inventory.unresolved_decisions[0].contains("coordinate reference system"));
        let cube_inventory = service.scientific_inventory("ds_cube").unwrap();
        let spectral = cube_inventory.components[0]
            .axes
            .iter()
            .find(|axis| axis.role == ScientificRole::Spectral)
            .unwrap();
        assert_eq!(spectral.coordinate_summary.as_ref().unwrap().count, 5);
        assert_eq!(rgb_bands_from_manifest(&cube, spectral), Some([3, 2, 1]));

        let first = service
            .suggest_views(&["ds_point".into(), "ds_cube".into()])
            .unwrap();
        let second = service
            .suggest_views(&["ds_point".into(), "ds_cube".into()])
            .unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        assert_eq!(first.suggestions[0].recipe, "point_cloud_spectral_cube_v1");
        assert_eq!(first.suggestions[0].panels.len(), 3);
        let rgb = first.suggestions[0]
            .panels
            .iter()
            .find(|panel| panel.representation == "rgb")
            .unwrap();
        assert_eq!(rgb.encoding["red_band"], 3);
        assert_eq!(rgb.encoding["green_band"], 2);
        assert_eq!(rgb.encoding["blue_band"], 1);
        assert!(first.suggestions[0].links.iter().any(|link| {
            link.resolver == "cube_pixel_to_spectrum" && link.exactness == LinkExactness::Exact
        }));
        assert!(first.suggestions[0].links.iter().any(|link| {
            link.resolver == "world_to_raster_pixel" && link.exactness == LinkExactness::Unavailable
        }));
    }

    #[test]
    fn suggestion_input_is_bounded_and_unique() {
        let (_directory, service) = service();
        assert!(service.suggest_views(&[]).is_err());
        assert!(
            service
                .suggest_views(&["ds_duplicate".into(), "ds_duplicate".into()])
                .is_err()
        );
        assert!(
            service
                .suggest_views(
                    &(0..9)
                        .map(|index| format!("ds_{index}"))
                        .collect::<Vec<_>>()
                )
                .is_err()
        );
    }
}
