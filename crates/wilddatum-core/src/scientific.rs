use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DatasetId, Modality, ProviderKind, SpatialReference};

pub const SCIENTIFIC_INVENTORY_VERSION: u32 = 1;
pub const VIEW_SUGGESTION_VERSION: u32 = 1;

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum InferenceConfidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScientificRole {
    Time,
    Latitude,
    Longitude,
    HorizontalX,
    HorizontalY,
    Vertical,
    Spectral,
    Channel,
    Identifier,
    Value,
    QualityControl,
    Uncertainty,
    Geometry,
    PointPosition,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScientificComponentKind {
    Field,
    Array,
    Geometry,
    PointPositions,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SemanticEvidence {
    pub source: String,
    pub pointer: Option<String>,
    pub statement: String,
    pub confidence: InferenceConfidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct CoordinateSummary {
    pub count: u64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ScientificAxis {
    pub index: u32,
    pub name: String,
    pub role: ScientificRole,
    pub length: u64,
    pub unit: Option<String>,
    pub coordinate_path: Option<String>,
    pub coordinate_summary: Option<CoordinateSummary>,
    #[serde(default)]
    pub evidence: Vec<SemanticEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ScientificComponent {
    pub id: String,
    pub label: String,
    pub kind: ScientificComponentKind,
    pub source_pointer: Option<String>,
    pub data_type: Option<String>,
    #[serde(default)]
    pub shape: Vec<u64>,
    #[serde(default)]
    pub roles: Vec<ScientificRole>,
    #[serde(default)]
    pub axes: Vec<ScientificAxis>,
    pub unit: Option<String>,
    pub coordinate_summary: Option<CoordinateSummary>,
    #[serde(default)]
    pub relationships: BTreeMap<String, String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub evidence: Vec<SemanticEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScientificInventory {
    pub version: u32,
    pub dataset_id: DatasetId,
    pub provider: ProviderKind,
    pub resource_id: String,
    pub format: Option<String>,
    #[serde(default)]
    pub modalities: Vec<Modality>,
    #[serde(default)]
    pub components: Vec<ScientificComponent>,
    pub spatial_reference: Option<SpatialReference>,
    #[serde(default)]
    pub unresolved_decisions: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuggestedPanelKind {
    Map,
    Spatial2d,
    Spatial3d,
    TimeSeries,
    Profile,
    Heatmap,
    Table,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct SuggestedPanel {
    pub id: String,
    pub kind: SuggestedPanelKind,
    pub dataset_id: DatasetId,
    pub component_id: Option<String>,
    pub representation: String,
    #[serde(default)]
    pub encoding: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkExactness {
    Exact,
    Bounded,
    Approximate,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SuggestedLink {
    pub source_panel: String,
    pub source_selection: String,
    pub target_panel: String,
    pub resolver: String,
    pub exactness: LinkExactness,
    pub explanation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ViewSuggestion {
    pub suggestion_id: String,
    pub recipe: String,
    pub title: String,
    pub description: String,
    pub dataset_ids: Vec<DatasetId>,
    pub confidence: InferenceConfidence,
    #[serde(default)]
    pub panels: Vec<SuggestedPanel>,
    #[serde(default)]
    pub links: Vec<SuggestedLink>,
    #[serde(default)]
    pub evidence: Vec<SemanticEvidence>,
    #[serde(default)]
    pub unresolved_decisions: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ViewSuggestionSet {
    pub version: u32,
    pub dataset_ids: Vec<DatasetId>,
    pub suggestions: Vec<ViewSuggestion>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_scientific_contracts_with_stable_names() {
        let inventory = ScientificInventory {
            version: SCIENTIFIC_INVENTORY_VERSION,
            dataset_id: DatasetId("ds_fixture".into()),
            provider: ProviderKind::Local,
            resource_id: "reflectance.h5".into(),
            format: Some("hdf5".into()),
            modalities: vec![Modality::Hyperspectral],
            components: vec![ScientificComponent {
                id: "array_reflectance".into(),
                label: "Reflectance".into(),
                kind: ScientificComponentKind::Array,
                source_pointer: Some("/SITE/Reflectance".into()),
                data_type: Some("u16".into()),
                shape: vec![10, 20, 30],
                roles: vec![ScientificRole::Value],
                axes: vec![ScientificAxis {
                    index: 2,
                    name: "wavelength".into(),
                    role: ScientificRole::Spectral,
                    length: 30,
                    unit: Some("nm".into()),
                    coordinate_path: Some("/SITE/Wavelength".into()),
                    coordinate_summary: Some(CoordinateSummary {
                        count: 30,
                        minimum: Some(400.0),
                        maximum: Some(900.0),
                        unit: Some("nm".into()),
                    }),
                    evidence: vec![],
                }],
                unit: None,
                coordinate_summary: None,
                relationships: BTreeMap::new(),
                metadata: BTreeMap::new(),
                evidence: vec![SemanticEvidence {
                    source: "file_metadata".into(),
                    pointer: Some("/SITE/Reflectance".into()),
                    statement: "A wavelength coordinate identifies the spectral axis".into(),
                    confidence: InferenceConfidence::High,
                }],
            }],
            spatial_reference: None,
            unresolved_decisions: vec!["Confirm georeferencing".into()],
            warnings: vec![],
        };

        let value = serde_json::to_value(inventory).unwrap();
        assert_eq!(value["components"][0]["kind"], "array");
        assert_eq!(value["components"][0]["axes"][0]["role"], "spectral");
        assert_eq!(value["components"][0]["evidence"][0]["confidence"], "high");
        assert!(value.to_string().contains("ds_fixture"));
        assert!(!value.to_string().contains("/Users/"));
    }

    #[test]
    fn serializes_suggestion_links_and_panel_kinds() {
        let suggestion = ViewSuggestion {
            suggestion_id: "suggest_fixture".into(),
            recipe: "spectral_cube_v1".into(),
            title: "RGB and spectrum".into(),
            description: "Inspect a cube pixel and its spectrum".into(),
            dataset_ids: vec![DatasetId("ds_cube".into())],
            confidence: InferenceConfidence::High,
            panels: vec![SuggestedPanel {
                id: "rgb".into(),
                kind: SuggestedPanelKind::Spatial2d,
                dataset_id: DatasetId("ds_cube".into()),
                component_id: Some("array_reflectance".into()),
                representation: "rgb".into(),
                encoding: BTreeMap::new(),
            }],
            links: vec![SuggestedLink {
                source_panel: "rgb".into(),
                source_selection: "cube_pixel".into(),
                target_panel: "spectrum".into(),
                resolver: "cube_pixel_to_spectrum".into(),
                exactness: LinkExactness::Exact,
                explanation: "Axes and source array are explicit".into(),
            }],
            evidence: vec![],
            unresolved_decisions: vec![],
            warnings: vec![],
        };

        let value = serde_json::to_value(suggestion).unwrap();
        assert_eq!(value["panels"][0]["kind"], "spatial2d");
        assert_eq!(value["links"][0]["exactness"], "exact");
    }
}
