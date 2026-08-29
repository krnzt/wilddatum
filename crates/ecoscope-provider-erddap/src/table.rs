use std::collections::BTreeMap;

use ecoscope_core::{EcoScopeError, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct TableEnvelope {
    pub table: TableData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableData {
    pub column_names: Vec<String>,
    #[serde(default)]
    pub rows: Vec<Vec<Value>>,
}

impl TableData {
    pub fn row(&self, values: &[Value]) -> Result<BTreeMap<String, Value>> {
        if values.len() != self.column_names.len() {
            return Err(EcoScopeError::Invalid(format!(
                "ERDDAP row has {} values for {} columns",
                values.len(),
                self.column_names.len()
            )));
        }
        Ok(self
            .column_names
            .iter()
            .cloned()
            .zip(values.iter().cloned())
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRecord {
    pub dataset_id: String,
    pub title: String,
    pub summary: Option<String>,
    pub institution: Option<String>,
    pub tabledap_url: Option<String>,
    pub griddap_url: Option<String>,
    pub accessible: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariableMetadata {
    pub name: String,
    pub data_type: String,
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InfoMetadata {
    pub globals: BTreeMap<String, Value>,
    pub variables: BTreeMap<String, VariableMetadata>,
    pub raw_metadata: Value,
}

pub fn parse_search(raw_metadata: &Value) -> Result<Vec<SearchRecord>> {
    let envelope: TableEnvelope = serde_json::from_value(raw_metadata.clone())?;
    envelope
        .table
        .rows
        .iter()
        .map(|values| {
            let row = envelope.table.row(values)?;
            Ok(SearchRecord {
                dataset_id: required_string(&row, "Dataset ID")?,
                title: required_string(&row, "Title")?,
                summary: optional_string(&row, "Summary"),
                institution: optional_string(&row, "Institution"),
                tabledap_url: optional_string(&row, "tabledap"),
                griddap_url: optional_string(&row, "griddap"),
                accessible: optional_string(&row, "Accessible").unwrap_or_default(),
            })
        })
        .collect()
}

pub fn parse_info(raw_metadata: &Value) -> Result<InfoMetadata> {
    let envelope: TableEnvelope = serde_json::from_value(raw_metadata.clone())?;
    let mut globals = BTreeMap::new();
    let mut variables = BTreeMap::<String, VariableMetadata>::new();
    for values in &envelope.table.rows {
        let row = envelope.table.row(values)?;
        let row_type = required_string(&row, "Row Type")?;
        let variable_name = required_string(&row, "Variable Name")?;
        match row_type.as_str() {
            "variable" => {
                variables
                    .entry(variable_name.clone())
                    .and_modify(|variable| {
                        variable.data_type = optional_string(&row, "Data Type").unwrap_or_default()
                    })
                    .or_insert_with(|| VariableMetadata {
                        name: variable_name,
                        data_type: optional_string(&row, "Data Type").unwrap_or_default(),
                        attributes: BTreeMap::new(),
                    });
            }
            "attribute" => {
                let attribute_name = required_string(&row, "Attribute Name")?;
                let value = row.get("Value").cloned().unwrap_or(Value::Null);
                if variable_name == "NC_GLOBAL" {
                    globals.insert(attribute_name, value);
                } else {
                    variables
                        .entry(variable_name.clone())
                        .or_insert_with(|| VariableMetadata {
                            name: variable_name,
                            data_type: String::new(),
                            attributes: BTreeMap::new(),
                        })
                        .attributes
                        .insert(attribute_name, value);
                }
            }
            _ => {}
        }
    }
    Ok(InfoMetadata {
        globals,
        variables,
        raw_metadata: raw_metadata.clone(),
    })
}

fn required_string(row: &BTreeMap<String, Value>, column: &str) -> Result<String> {
    optional_string(row, column).ok_or_else(|| {
        EcoScopeError::Invalid(format!("ERDDAP row is missing required column {column}"))
    })
}

fn optional_string(row: &BTreeMap<String, Value>, column: &str) -> Option<String> {
    row.get(column)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_rows_by_column_name() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/search.json")).unwrap();

        let records = parse_search(&fixture).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].dataset_id, "ArgoFloats");
        assert_eq!(records[0].title, "Argo Float Measurements");
        assert_eq!(records[0].institution.as_deref(), Some("Argo"));
        assert_eq!(
            records[0].tabledap_url.as_deref(),
            Some("https://example.test/erddap/tabledap/ArgoFloats")
        );
        assert!(records[0].griddap_url.is_none());
    }

    #[test]
    fn collects_info_globals_variables_units_and_raw_metadata() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/info.json")).unwrap();

        let info = parse_info(&fixture).unwrap();

        assert_eq!(info.globals["cdm_data_type"], "TrajectoryProfile");
        assert_eq!(info.globals["license"], "CC BY 4.0");
        assert_eq!(info.variables["temp"].data_type, "float");
        assert_eq!(info.variables["temp"].attributes["units"], "degree_C");
        assert_eq!(info.variables["pres"].attributes["units"], "dbar");
        assert_eq!(info.raw_metadata, fixture);
    }
}
