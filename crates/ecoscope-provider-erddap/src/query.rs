use std::collections::BTreeSet;

use ecoscope_core::{EcoScopeError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ErddapOptions {
    pub protocol: Protocol,
    #[serde(default = "default_output_format")]
    pub output_format: OutputFormat,
    #[serde(default)]
    pub constraints: Vec<Constraint>,
    #[serde(default)]
    pub arrays: Vec<ArraySelection>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Tabledap,
    Griddap,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tabledap => "tabledap",
            Self::Griddap => "griddap",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Csv,
    Netcdf,
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Netcdf => "nc",
        }
    }
}

fn default_output_format() -> OutputFormat {
    OutputFormat::Csv
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    pub variable: String,
    pub op: String,
    pub value: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArraySelection {
    pub variable: String,
    pub slices: Vec<AxisSlice>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AxisSlice {
    pub start: String,
    pub stop: String,
    #[serde(default = "default_stride")]
    pub stride: u64,
    #[serde(default)]
    pub by_value: bool,
}

fn default_stride() -> u64 {
    1
}

#[derive(Debug, Clone)]
pub struct SubsetQuery {
    pub url: Url,
    pub filename: String,
    pub expression: String,
}

pub fn build_subset(
    base_url: &Url,
    dataset_id: &str,
    requested_variables: &[String],
    available_variables: &BTreeSet<String>,
    options: &ErddapOptions,
) -> Result<SubsetQuery> {
    validate_dataset_id(dataset_id)?;
    let expression = match options.protocol {
        Protocol::Tabledap => build_table_expression(
            requested_variables,
            available_variables,
            &options.constraints,
            &options.arrays,
        )?,
        Protocol::Griddap => build_grid_expression(
            requested_variables,
            available_variables,
            &options.constraints,
            &options.arrays,
        )?,
    };
    let filename = format!("{dataset_id}.{}", options.output_format.extension());
    let mut url = base_url.clone();
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| EcoScopeError::Invalid("ERDDAP base URL cannot be a base".into()))?;
        path.pop_if_empty();
        path.push(options.protocol.as_str());
        path.push(&filename);
    }
    let encoded = url::form_urlencoded::byte_serialize(expression.as_bytes()).collect::<String>();
    url.set_query(Some(&encoded));
    Ok(SubsetQuery {
        url,
        filename,
        expression,
    })
}

fn build_table_expression(
    requested_variables: &[String],
    available_variables: &BTreeSet<String>,
    constraints: &[Constraint],
    arrays: &[ArraySelection],
) -> Result<String> {
    if !arrays.is_empty() {
        return Err(EcoScopeError::Invalid(
            "array slices are valid only for griddap".into(),
        ));
    }
    if requested_variables.is_empty() {
        return Err(EcoScopeError::Invalid(
            "tabledap requires at least one variable".into(),
        ));
    }
    validate_variables(requested_variables, available_variables)?;
    let mut expression = requested_variables.join(",");
    for constraint in constraints {
        validate_variable(&constraint.variable, available_variables)?;
        let operator = match constraint.op.as_str() {
            "eq" => "=",
            "ne" => "!=",
            "lt" => "<",
            "lte" => "<=",
            "gt" => ">",
            "gte" => ">=",
            op => {
                return Err(EcoScopeError::Invalid(format!(
                    "unsupported ERDDAP constraint operator {op}"
                )));
            }
        };
        expression.push('&');
        expression.push_str(&constraint.variable);
        expression.push_str(operator);
        expression.push_str(&constraint_value(constraint)?);
    }
    Ok(expression)
}

fn build_grid_expression(
    requested_variables: &[String],
    available_variables: &BTreeSet<String>,
    constraints: &[Constraint],
    arrays: &[ArraySelection],
) -> Result<String> {
    if !constraints.is_empty() {
        return Err(EcoScopeError::Invalid(
            "row constraints are valid only for tabledap".into(),
        ));
    }
    if arrays.is_empty() {
        return Err(EcoScopeError::Invalid(
            "griddap requires at least one array selection".into(),
        ));
    }
    if !requested_variables.is_empty() {
        validate_variables(requested_variables, available_variables)?;
    }
    arrays
        .iter()
        .map(|array| {
            validate_variable(&array.variable, available_variables)?;
            if array.slices.is_empty() {
                return Err(EcoScopeError::Invalid(format!(
                    "griddap array {} requires at least one axis slice",
                    array.variable
                )));
            }
            let mut expression = array.variable.clone();
            for slice in &array.slices {
                if slice.stride == 0 {
                    return Err(EcoScopeError::Invalid(
                        "griddap axis stride must be greater than zero".into(),
                    ));
                }
                if slice.by_value {
                    validate_axis_value(&slice.start)?;
                    validate_axis_value(&slice.stop)?;
                    expression.push_str(&format!(
                        "[({}):{}:({})]",
                        slice.start, slice.stride, slice.stop
                    ));
                } else {
                    let start = slice.start.parse::<u64>().map_err(|_| {
                        EcoScopeError::Invalid("griddap index start must be an integer".into())
                    })?;
                    let stop = slice.stop.parse::<u64>().map_err(|_| {
                        EcoScopeError::Invalid("griddap index stop must be an integer".into())
                    })?;
                    expression.push_str(&format!("[{start}:{}:{stop}]", slice.stride));
                }
            }
            Ok(expression)
        })
        .collect::<Result<Vec<_>>>()
        .map(|expressions| expressions.join(","))
}

fn validate_dataset_id(dataset_id: &str) -> Result<()> {
    if dataset_id.is_empty()
        || dataset_id == "."
        || dataset_id == ".."
        || !dataset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(EcoScopeError::Invalid(
            "invalid ERDDAP dataset identifier".into(),
        ));
    }
    Ok(())
}

fn validate_variables(variables: &[String], available_variables: &BTreeSet<String>) -> Result<()> {
    let mut unique = BTreeSet::new();
    for variable in variables {
        validate_variable(variable, available_variables)?;
        if !unique.insert(variable) {
            return Err(EcoScopeError::Invalid(format!(
                "ERDDAP variable {variable} was requested more than once"
            )));
        }
    }
    Ok(())
}

fn validate_variable(variable: &str, available_variables: &BTreeSet<String>) -> Result<()> {
    if variable.is_empty() || !available_variables.contains(variable) {
        return Err(EcoScopeError::Invalid(format!(
            "unknown ERDDAP variable {variable}"
        )));
    }
    Ok(())
}

fn constraint_value(constraint: &Constraint) -> Result<String> {
    match &constraint.value {
        Value::String(value) if constraint.variable.eq_ignore_ascii_case("time") => {
            validate_axis_value(value)?;
            Ok(value.clone())
        }
        Value::String(value) => serde_json::to_string(value).map_err(EcoScopeError::from),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        _ => Err(EcoScopeError::Invalid(
            "ERDDAP constraints accept only string, number, or boolean values".into(),
        )),
    }
}

fn validate_axis_value(value: &str) -> Result<()> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'[' | b']' | b'&'))
    {
        return Err(EcoScopeError::Invalid("invalid ERDDAP axis value".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;
    use url::Url;

    use super::*;

    fn available(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn builds_encoded_tabledap_expression_once() {
        let options = ErddapOptions {
            protocol: Protocol::Tabledap,
            output_format: OutputFormat::Csv,
            constraints: vec![
                Constraint {
                    variable: "time".into(),
                    op: "gte".into(),
                    value: json!("2025-01-01T00:00:00Z"),
                },
                Constraint {
                    variable: "time".into(),
                    op: "lte".into(),
                    value: json!("2025-01-02T00:00:00Z"),
                },
            ],
            arrays: vec![],
        };
        let subset = build_subset(
            &Url::parse("https://example.test/erddap").unwrap(),
            "ArgoFloats",
            &["time", "latitude", "longitude", "temp"]
                .map(str::to_owned)
                .to_vec(),
            &available(&["time", "latitude", "longitude", "temp"]),
            &options,
        )
        .unwrap();

        let decoded = url::form_urlencoded::parse(subset.url.query().unwrap().as_bytes())
            .next()
            .unwrap()
            .0;
        assert_eq!(
            decoded,
            "time,latitude,longitude,temp&time>=2025-01-01T00:00:00Z&time<=2025-01-02T00:00:00Z"
        );
        assert_eq!(subset.filename, "ArgoFloats.csv");
    }

    #[test]
    fn builds_griddap_index_slices() {
        let options = ErddapOptions {
            protocol: Protocol::Griddap,
            output_format: OutputFormat::Netcdf,
            constraints: vec![],
            arrays: vec![ArraySelection {
                variable: "temperature".into(),
                slices: vec![
                    AxisSlice {
                        start: "0".into(),
                        stop: "23".into(),
                        stride: 1,
                        by_value: false,
                    },
                    AxisSlice {
                        start: "10".into(),
                        stop: "30".into(),
                        stride: 2,
                        by_value: false,
                    },
                ],
            }],
        };
        let subset = build_subset(
            &Url::parse("https://example.test/erddap").unwrap(),
            "GridData",
            &[],
            &available(&["temperature"]),
            &options,
        )
        .unwrap();

        let decoded = url::form_urlencoded::parse(subset.url.query().unwrap().as_bytes())
            .next()
            .unwrap()
            .0;
        assert_eq!(decoded, "temperature[0:1:23][10:2:30]");
        assert_eq!(subset.filename, "GridData.nc");
    }

    #[test]
    fn rejects_unknown_variables_and_operators() {
        let mut options: ErddapOptions = serde_json::from_value(json!({
            "protocol": "tabledap",
            "constraints": [{"variable": "time", "op": "contains", "value": "x"}]
        }))
        .unwrap();
        assert!(
            build_subset(
                &Url::parse("https://example.test/erddap").unwrap(),
                "data",
                &["time".into()],
                &available(&["time"]),
                &options,
            )
            .is_err()
        );
        options.constraints.clear();
        assert!(
            build_subset(
                &Url::parse("https://example.test/erddap").unwrap(),
                "data",
                &["unknown".into()],
                &available(&["time"]),
                &options,
            )
            .is_err()
        );
    }
}
