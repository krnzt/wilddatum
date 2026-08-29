//! Versioned semantics for linked geographic trajectories and vertical profiles.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PINNED_RERUN_VERSION;

pub const PROFILE_TRAJECTORY_VIEW_KIND: &str = "profile_trajectory_v1";
pub const PROFILE_TRAJECTORY_OBSERVATION_SUFFIXES: [&str; 2] =
    ["map_observations", "profile_observations"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerticalDirection {
    PositiveDown,
    PositiveUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerticalAxisSpec {
    pub field: String,
    pub direction: VerticalDirection,
    pub unit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileValueSpec {
    pub field: String,
    pub unit: Option<String>,
    pub qc_field: Option<String>,
    #[serde(default)]
    pub accepted_qc: Vec<String>,
}

/// User-controlled scientific semantics for a linked trajectory/profile view.
/// Source-row selection mappings are intentionally not part of this input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileTrajectoryRecipeV1 {
    pub trajectory_id_field: String,
    pub profile_id_field: String,
    pub time_field: Option<String>,
    pub latitude_field: String,
    pub longitude_field: String,
    pub vertical: VerticalAxisSpec,
    pub value: ProfileValueSpec,
}

impl ProfileTrajectoryRecipeV1 {
    pub fn encoding(&self) -> BTreeMap<String, Value> {
        let mut encoding = BTreeMap::from([
            ("view_kind".into(), json!(PROFILE_TRAJECTORY_VIEW_KIND)),
            (
                "trajectory_id_field".into(),
                json!(self.trajectory_id_field),
            ),
            ("profile_id_field".into(), json!(self.profile_id_field)),
            ("latitude_field".into(), json!(self.latitude_field)),
            ("longitude_field".into(), json!(self.longitude_field)),
            ("vertical".into(), json!(self.vertical)),
            ("value".into(), json!(self.value)),
        ]);
        if let Some(time_field) = &self.time_field {
            encoding.insert("time_field".into(), json!(time_field));
        }
        encoding
    }
}

/// Service-authored proof that a Rerun observation instance can be resolved to
/// a source record. Clients may inspect this value but cannot configure it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceRowSelectionMapping {
    pub kind: String,
    pub entity_suffixes: Vec<String>,
    pub stride: u64,
    pub rerun_version: String,
}

impl SourceRowSelectionMapping {
    pub fn profile_trajectory_v1() -> Self {
        Self {
            kind: "source_row_index".into(),
            entity_suffixes: PROFILE_TRAJECTORY_OBSERVATION_SUFFIXES
                .into_iter()
                .map(str::to_owned)
                .collect(),
            stride: 1,
            rerun_version: PINNED_RERUN_VERSION.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_trajectory_encoding_is_versioned_and_mapping_free() {
        let recipe = ProfileTrajectoryRecipeV1 {
            trajectory_id_field: "platform".into(),
            profile_id_field: "cycle".into(),
            time_field: Some("time".into()),
            latitude_field: "latitude".into(),
            longitude_field: "longitude".into(),
            vertical: VerticalAxisSpec {
                field: "pressure".into(),
                direction: VerticalDirection::PositiveDown,
                unit: Some("decibar".into()),
            },
            value: ProfileValueSpec {
                field: "temperature".into(),
                unit: Some("degree_Celsius".into()),
                qc_field: Some("temperature_qc".into()),
                accepted_qc: vec!["1".into(), "2".into()],
            },
        };
        let encoding = recipe.encoding();
        assert_eq!(encoding["view_kind"], PROFILE_TRAJECTORY_VIEW_KIND);
        assert_eq!(encoding["vertical"]["direction"], "positive_down");
        assert!(!encoding.contains_key("selection_mapping"));
        assert!(
            serde_json::from_value::<VerticalDirection>(json!("down"))
                .unwrap_err()
                .to_string()
                .contains("unknown variant")
        );
    }

    #[test]
    fn source_row_mapping_is_service_constant() {
        let mapping = SourceRowSelectionMapping::profile_trajectory_v1();
        assert_eq!(mapping.kind, "source_row_index");
        assert_eq!(mapping.stride, 1);
        assert_eq!(mapping.rerun_version, PINNED_RERUN_VERSION);
        assert_eq!(
            mapping.entity_suffixes,
            ["map_observations", "profile_observations"]
        );
    }
}
