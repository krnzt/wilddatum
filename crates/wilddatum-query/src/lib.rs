//! Typed, bounded tabular query execution for local and provider materialized assets.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
    sync::Arc,
};

use arrow_array::RecordBatch;
use arrow_ipc::{reader::FileReader, writer::FileWriter};
use arrow_json::WriterBuilder;
use datafusion::{
    datasource::MemTable,
    execution::SessionStateBuilder,
    prelude::{CsvReadOptions, ParquetReadOptions, SessionConfig, SessionContext},
};
use serde_json::{Map, Value, json};
use wilddatum_core::{AggregateSpec, QueryFilter, Result, SortSpec, WildDatumError};

pub const MAX_RESULT_ROWS: usize = 100_000;
pub const MAX_SOURCE_ROWS: usize = 10_000;

#[derive(Debug, Clone)]
pub struct TabularQueryOutput {
    pub batches: Vec<RecordBatch>,
    pub matched_rows: u64,
    pub payload: Value,
}

/// Read exact zero-based records from the original delimited source in one
/// streaming pass. Values remain strings so identifiers, sentinel values, and
/// provider-native QC encodings survive without type coercion.
pub fn execute_source_rows(
    path: &Path,
    original_name: &str,
    source_indices: &[u64],
    select: &[String],
) -> Result<TabularQueryOutput> {
    if source_indices.is_empty() {
        return Err(WildDatumError::Invalid(
            "source_rows requires at least one source index".into(),
        ));
    }
    if source_indices.len() > MAX_SOURCE_ROWS {
        return Err(WildDatumError::Invalid(format!(
            "source_rows accepts at most {MAX_SOURCE_ROWS} indices"
        )));
    }
    let unique = source_indices.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != source_indices.len() {
        return Err(WildDatumError::Invalid(
            "source_rows indices must be unique".into(),
        ));
    }

    let extension = Path::new(original_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let delimiter = match extension.as_str() {
        "csv" => b',',
        "tsv" => b'\t',
        _ => {
            return Err(WildDatumError::Invalid(format!(
                "source_rows currently supports original CSV and TSV sources; got {extension}"
            )));
        }
    };
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .from_path(path)
        .map_err(csv_error)?;
    let headers = reader.headers().map_err(csv_error)?.clone();
    let header_names = headers.iter().map(str::to_owned).collect::<Vec<_>>();
    if header_names.iter().collect::<BTreeSet<_>>().len() != header_names.len() {
        return Err(WildDatumError::Invalid(
            "source_rows cannot represent duplicate CSV/TSV header names".into(),
        ));
    }
    let projected = if select.is_empty() {
        header_names.clone()
    } else {
        select.to_vec()
    };
    let header_positions = header_names
        .iter()
        .enumerate()
        .map(|(position, name)| (name.as_str(), position))
        .collect::<BTreeMap<_, _>>();
    let projected_positions = projected
        .iter()
        .map(|field| {
            header_positions
                .get(field.as_str())
                .copied()
                .ok_or_else(|| {
                    WildDatumError::Invalid(format!("unknown source_rows field {field}"))
                })
        })
        .collect::<Result<Vec<_>>>()?;
    if projected.iter().collect::<BTreeSet<_>>().len() != projected.len() {
        return Err(WildDatumError::Invalid(
            "source_rows selected fields must be unique".into(),
        ));
    }

    let wanted = source_indices
        .iter()
        .enumerate()
        .map(|(request_position, source_index)| (*source_index, request_position))
        .collect::<BTreeMap<_, _>>();
    let largest = *wanted
        .last_key_value()
        .expect("non-empty source indices checked above")
        .0;
    let mut found = vec![None; source_indices.len()];
    let mut found_count = 0_usize;
    for (source_index, record) in reader.records().enumerate() {
        let source_index = source_index as u64;
        if source_index > largest || found_count == found.len() {
            break;
        }
        let Some(&request_position) = wanted.get(&source_index) else {
            continue;
        };
        let record = record.map_err(csv_error)?;
        let values = projected
            .iter()
            .zip(&projected_positions)
            .map(|(field, position)| {
                (
                    field.clone(),
                    Value::String(record.get(*position).unwrap_or_default().to_owned()),
                )
            })
            .collect::<Map<_, _>>();
        found[request_position] = Some(json!({
            "source_index": source_index,
            "values": values
        }));
        found_count += 1;
    }
    if found_count != found.len() {
        let missing = source_indices
            .iter()
            .zip(&found)
            .filter_map(|(index, row)| row.is_none().then_some(*index))
            .collect::<Vec<_>>();
        return Err(WildDatumError::Invalid(format!(
            "source_rows indices are outside the source: {missing:?}"
        )));
    }
    let rows = found
        .into_iter()
        .map(|row| row.expect("all requested source rows were found"))
        .collect::<Vec<_>>();
    let returned_rows = rows.len();
    Ok(TabularQueryOutput {
        batches: vec![],
        matched_rows: returned_rows as u64,
        payload: json!({
            "columns": projected,
            "row_envelope": {
                "source_index": "zero-based data-record position in the original source",
                "values": "uncoerced source field values"
            },
            "rows": rows,
            "returned_rows": returned_rows,
            "matched_rows": returned_rows,
            "truncated": false,
            "deterministic_order": true,
            "engine": "wilddatum-source-rows-1"
        }),
    })
}

fn csv_error(error: csv::Error) -> WildDatumError {
    WildDatumError::Invalid(format!("cannot read delimited source: {error}"))
}

/// Execute an exact spatial predicate against a GeoParquet source after its
/// GeoArrow extension metadata has been restored by the GeoParquet reader.
pub async fn execute_geoparquet_region(
    path: &Path,
    query_wkt: &str,
    requested_limit: u64,
) -> Result<TabularQueryOutput> {
    let state = SessionStateBuilder::new()
        .with_default_features()
        .with_file_formats(vec![Arc::new(
            geodatafusion_geoparquet::file_format::GeoParquetFormatFactory::default(),
        )])
        .build();
    let context = SessionContext::new_with_state(state).enable_url_table();
    geodatafusion::register(&context);
    let path_text = path
        .to_str()
        .ok_or_else(|| WildDatumError::Invalid("GeoParquet path is not valid UTF-8".into()))?;
    let source = sql_string(path_text);
    let dataframe = context
        .sql(&format!("SELECT * FROM {source}"))
        .await
        .map_err(query_error)?;
    let fields = dataframe.schema().fields();
    let geometry = fields
        .iter()
        .find(|field| {
            field
                .extension_type_name()
                .is_some_and(|name| name.starts_with("geoarrow."))
        })
        .ok_or_else(|| {
            WildDatumError::Invalid(
                "Parquet source has no GeoParquet primary geometry extension metadata".into(),
            )
        })?;
    let geometry_name = geometry.name();
    let query_geometry = format!("ST_GeomFromText({})", sql_string(query_wkt));
    let predicate = format!(
        "ST_Intersects({}, {query_geometry})",
        quote_identifier(geometry_name)
    );
    let count_batches = context
        .sql(&format!(
            "SELECT COUNT(*) AS matched_rows FROM {source} WHERE {predicate}"
        ))
        .await
        .map_err(query_error)?
        .collect()
        .await
        .map_err(query_error)?;
    let matched_rows = extract_count(&count_batches)?;
    let mut projection = fields
        .iter()
        .filter(|field| field.name() != geometry_name)
        .map(|field| quote_identifier(field.name()))
        .collect::<Vec<_>>();
    projection.push(format!(
        "ST_AsText({}) AS __wilddatum_geometry_wkt",
        quote_identifier(geometry_name)
    ));
    let limit = requested_limit.clamp(1, MAX_RESULT_ROWS as u64);
    let batches = context
        .sql(&format!(
            "SELECT {} FROM {source} WHERE {predicate} LIMIT {limit}",
            projection.join(", ")
        ))
        .await
        .map_err(query_error)?
        .collect()
        .await
        .map_err(query_error)?;
    let returned_rows = batches.iter().map(RecordBatch::num_rows).sum::<usize>();
    let rows = batches_to_json(&batches)?;
    Ok(TabularQueryOutput {
        batches,
        matched_rows,
        payload: json!({
            "rows": rows,
            "returned_rows": returned_rows,
            "matched_rows": matched_rows,
            "truncated": matched_rows > returned_rows as u64,
            "geometry_column": geometry_name,
            "engine": "geodatafusion-0.5",
            "predicate": "st_intersects"
        }),
    })
}

pub struct TableQuerySpec<'a> {
    pub select: &'a [String],
    pub filters: &'a [QueryFilter],
    pub group_by: &'a [String],
    pub aggregates: &'a [AggregateSpec],
    pub order_by: &'a [SortSpec],
    pub limit: u32,
}

pub async fn execute_table_query(
    path: &Path,
    original_name: &str,
    query: TableQuerySpec<'_>,
) -> Result<TabularQueryOutput> {
    let context = SessionContext::new_with_config(
        SessionConfig::new()
            .with_target_partitions(1)
            .with_batch_size(8_192),
    );
    register_source(&context, path, original_name).await?;
    let dataframe = context.table("dataset").await.map_err(query_error)?;
    let fields = dataframe
        .schema()
        .fields()
        .iter()
        .map(|field| field.name().to_owned())
        .collect::<Vec<_>>();
    validate_query(
        &fields,
        query.select,
        query.filters,
        query.group_by,
        query.aggregates,
        query.order_by,
    )?;

    let where_clause = filters_to_sql(query.filters)?;
    let matched_sql = format!("SELECT COUNT(*) AS matched_rows FROM dataset{where_clause}");
    let matched_batches = context
        .sql(&matched_sql)
        .await
        .map_err(query_error)?
        .collect()
        .await
        .map_err(query_error)?;
    let matched_rows = extract_count(&matched_batches)?;

    let limit = (query.limit as usize).clamp(1, MAX_RESULT_ROWS);
    let query_sql = build_sql(
        &fields,
        query.select,
        &where_clause,
        query.group_by,
        query.aggregates,
        query.order_by,
        limit,
    )?;
    let batches = context
        .sql(&query_sql)
        .await
        .map_err(query_error)?
        .collect()
        .await
        .map_err(query_error)?;
    let returned_rows = batches.iter().map(RecordBatch::num_rows).sum::<usize>();
    let columns = batches
        .first()
        .map(|batch| {
            batch
                .schema()
                .fields()
                .iter()
                .map(|field| field.name().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| result_columns(&fields, query.select, query.group_by, query.aggregates));
    let rows = batches_to_json(&batches)?;
    let schema = batches.first().map_or_else(Vec::new, |batch| {
        batch
            .schema()
            .fields()
            .iter()
            .map(|field| {
                json!({
                    "name": field.name(),
                    "data_type": format!("{:?}", field.data_type()),
                    "nullable": field.is_nullable()
                })
            })
            .collect()
    });
    let deterministic =
        !query.order_by.is_empty() || (!query.aggregates.is_empty() && query.group_by.is_empty());
    Ok(TabularQueryOutput {
        batches,
        matched_rows,
        payload: json!({
            "columns": columns,
            "schema": schema,
            "rows": rows,
            "returned_rows": returned_rows,
            "matched_rows": matched_rows,
            "truncated": if query.aggregates.is_empty() {
                matched_rows > returned_rows as u64
            } else {
                returned_rows == limit
            },
            "deterministic_order": deterministic,
            "engine": "datafusion-54.1"
        }),
    })
}

pub fn write_arrow_ipc(path: &Path, batches: &[RecordBatch]) -> Result<()> {
    let schema = batches
        .first()
        .map(RecordBatch::schema)
        .ok_or_else(|| WildDatumError::Invalid("cannot write an empty Arrow result".into()))?;
    let mut writer = FileWriter::try_new(File::create(path)?, &schema)
        .map_err(|error| WildDatumError::Internal(format!("cannot create Arrow IPC: {error}")))?;
    for batch in batches {
        writer.write(batch).map_err(|error| {
            WildDatumError::Internal(format!("cannot write Arrow IPC: {error}"))
        })?;
    }
    writer
        .finish()
        .map_err(|error| WildDatumError::Internal(format!("cannot finish Arrow IPC: {error}")))
}

async fn register_source(context: &SessionContext, path: &Path, original_name: &str) -> Result<()> {
    let path_text = path
        .to_str()
        .ok_or_else(|| WildDatumError::Invalid("table path is not valid UTF-8".into()))?;
    let extension = Path::new(original_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "csv" | "tsv" => context
            .register_csv(
                "dataset",
                path_text,
                CsvReadOptions::new().delimiter(if extension == "tsv" { b'\t' } else { b',' }),
            )
            .await
            .map_err(query_error),
        "parquet" | "geoparquet" => context
            .register_parquet("dataset", path_text, ParquetReadOptions::default())
            .await
            .map_err(query_error),
        "arrow" | "ipc" | "feather" => {
            let reader = FileReader::try_new(File::open(path)?, None).map_err(|error| {
                WildDatumError::Invalid(format!("cannot read Arrow IPC file: {error}"))
            })?;
            let schema = reader.schema();
            let batches = reader
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| WildDatumError::Invalid(format!("invalid Arrow IPC: {error}")))?;
            let table = MemTable::try_new(schema, vec![batches]).map_err(query_error)?;
            context
                .register_table("dataset", Arc::new(table))
                .map_err(query_error)?;
            Ok(())
        }
        _ => Err(WildDatumError::Invalid(format!(
            "table queries support CSV, TSV, Parquet, GeoParquet, Arrow IPC, and Feather; got {extension}"
        ))),
    }
}

fn build_sql(
    fields: &[String],
    select: &[String],
    where_clause: &str,
    group_by: &[String],
    aggregates: &[AggregateSpec],
    order_by: &[SortSpec],
    limit: usize,
) -> Result<String> {
    let projection = if aggregates.is_empty() {
        let projected = if select.is_empty() { fields } else { select };
        projected
            .iter()
            .map(|field| quote_identifier(field))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        let mut expressions = group_by
            .iter()
            .map(|field| quote_identifier(field))
            .collect::<Vec<_>>();
        for aggregate in aggregates {
            let function = match aggregate.function.to_ascii_lowercase().as_str() {
                "count" => "COUNT",
                "sum" => "SUM",
                "mean" | "avg" => "AVG",
                "min" => "MIN",
                "max" => "MAX",
                function => {
                    return Err(WildDatumError::Invalid(format!(
                        "unsupported aggregate {function}; use count, sum, mean, min, or max"
                    )));
                }
            };
            let field = if aggregate.field == "*" {
                "*".to_owned()
            } else {
                quote_identifier(&aggregate.field)
            };
            expressions.push(format!(
                "{function}({field}) AS {}",
                quote_identifier(&aggregate.alias)
            ));
        }
        expressions.join(", ")
    };
    let group_clause = if group_by.is_empty() {
        String::new()
    } else {
        format!(
            " GROUP BY {}",
            group_by
                .iter()
                .map(|field| quote_identifier(field))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let order_clause = if order_by.is_empty() {
        String::new()
    } else {
        format!(
            " ORDER BY {}",
            order_by
                .iter()
                .map(|sort| format!(
                    "{} {} NULLS {}",
                    quote_identifier(&sort.field),
                    if sort.descending { "DESC" } else { "ASC" },
                    if sort.nulls_first { "FIRST" } else { "LAST" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Ok(format!(
        "SELECT {projection} FROM dataset{where_clause}{group_clause}{order_clause} LIMIT {limit}"
    ))
}

fn filters_to_sql(filters: &[QueryFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(String::new());
    }
    let mut predicates = Vec::with_capacity(filters.len());
    for filter in filters {
        let field = quote_identifier(&filter.field);
        let op = filter.op.to_ascii_lowercase();
        let predicate = match op.as_str() {
            "eq" | "=" if filter.value.is_null() => format!("{field} IS NULL"),
            "ne" | "!=" if filter.value.is_null() => format!("{field} IS NOT NULL"),
            "eq" | "=" => format!("{field} = {}", sql_literal(&filter.value)?),
            "ne" | "!=" => format!("{field} <> {}", sql_literal(&filter.value)?),
            "lt" | "<" => format!("{field} < {}", sql_literal(&filter.value)?),
            "lte" | "<=" => format!("{field} <= {}", sql_literal(&filter.value)?),
            "gt" | ">" => format!("{field} > {}", sql_literal(&filter.value)?),
            "gte" | ">=" => format!("{field} >= {}", sql_literal(&filter.value)?),
            "contains" => format!(
                "strpos(CAST({field} AS VARCHAR), {}) > 0",
                sql_literal(&filter.value)?
            ),
            "in" => {
                let values = filter.value.as_array().ok_or_else(|| {
                    WildDatumError::Invalid("an in filter requires an array value".into())
                })?;
                if values.is_empty() {
                    "FALSE".into()
                } else {
                    format!(
                        "{field} IN ({})",
                        values
                            .iter()
                            .map(sql_literal)
                            .collect::<Result<Vec<_>>>()?
                            .join(", ")
                    )
                }
            }
            _ => {
                return Err(WildDatumError::Invalid(format!(
                    "unsupported filter operator {}; use eq, ne, lt, lte, gt, gte, contains, or in",
                    filter.op
                )));
            }
        };
        predicates.push(predicate);
    }
    Ok(format!(" WHERE {}", predicates.join(" AND ")))
}

fn validate_query(
    fields: &[String],
    select: &[String],
    filters: &[QueryFilter],
    group_by: &[String],
    aggregates: &[AggregateSpec],
    order_by: &[SortSpec],
) -> Result<()> {
    if aggregates.is_empty() && !group_by.is_empty() {
        return Err(WildDatumError::Invalid(
            "group_by requires at least one aggregate".into(),
        ));
    }
    let aggregate_aliases = aggregates
        .iter()
        .map(|aggregate| aggregate.alias.as_str())
        .collect::<Vec<_>>();
    for field in select
        .iter()
        .chain(group_by)
        .chain(filters.iter().map(|filter| &filter.field))
        .chain(
            aggregates
                .iter()
                .filter(|aggregate| aggregate.field != "*")
                .map(|aggregate| &aggregate.field),
        )
    {
        if !fields.contains(field) {
            return Err(WildDatumError::Invalid(format!(
                "unknown table field {field}"
            )));
        }
    }
    for sort in order_by {
        if !fields.contains(&sort.field) && !aggregate_aliases.contains(&sort.field.as_str()) {
            return Err(WildDatumError::Invalid(format!(
                "unknown order_by field {}",
                sort.field
            )));
        }
    }
    Ok(())
}

fn sql_literal(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("NULL".into()),
        Value::Bool(value) => Ok(if *value { "TRUE" } else { "FALSE" }.into()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) => Ok(format!("'{}'", value.replace('\'', "''"))),
        _ => Err(WildDatumError::Invalid(
            "filter values must be scalar, except for the in operator".into(),
        )),
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn batches_to_json(batches: &[RecordBatch]) -> Result<Vec<Map<String, Value>>> {
    if batches.is_empty() {
        return Ok(vec![]);
    }
    let mut writer = WriterBuilder::new()
        .with_explicit_nulls(true)
        .build::<_, arrow_json::writer::JsonArray>(Vec::new());
    let references = batches.iter().collect::<Vec<_>>();
    writer.write_batches(&references).map_err(|error| {
        WildDatumError::Internal(format!("cannot serialize Arrow result: {error}"))
    })?;
    writer.finish().map_err(|error| {
        WildDatumError::Internal(format!("cannot finish Arrow JSON result: {error}"))
    })?;
    serde_json::from_slice(&writer.into_inner()).map_err(WildDatumError::from)
}

fn extract_count(batches: &[RecordBatch]) -> Result<u64> {
    let rows = batches_to_json(batches)?;
    rows.first()
        .and_then(|row| row.get("matched_rows"))
        .and_then(Value::as_u64)
        .ok_or_else(|| WildDatumError::Internal("DataFusion count returned no value".into()))
}

fn result_columns(
    fields: &[String],
    select: &[String],
    group_by: &[String],
    aggregates: &[AggregateSpec],
) -> Vec<String> {
    if aggregates.is_empty() {
        if select.is_empty() {
            fields.to_vec()
        } else {
            select.to_vec()
        }
    } else {
        group_by
            .iter()
            .cloned()
            .chain(aggregates.iter().map(|aggregate| aggregate.alias.clone()))
            .collect()
    }
}

fn query_error(error: impl std::fmt::Display) -> WildDatumError {
    WildDatumError::Invalid(format!("tabular query failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use wilddatum_core::SortSpec;

    #[tokio::test]
    async fn csv_queries_are_typed_filtered_sorted_and_aggregated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observations.csv");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "site,value,qc").unwrap();
        writeln!(file, "HARV,3.5,pass").unwrap();
        writeln!(file, "ABBY,2.0,pass").unwrap();
        writeln!(file, "HARV,4.5,fail").unwrap();
        let filters = [QueryFilter {
            field: "qc".into(),
            op: "eq".into(),
            value: json!("pass"),
        }];
        let group_by = ["site".into()];
        let aggregates = [AggregateSpec {
            field: "value".into(),
            function: "mean".into(),
            alias: "mean_value".into(),
        }];
        let order_by = [SortSpec {
            field: "site".into(),
            descending: false,
            nulls_first: false,
        }];
        let output = execute_table_query(
            &path,
            "observations.csv",
            TableQuerySpec {
                select: &[],
                filters: &filters,
                group_by: &group_by,
                aggregates: &aggregates,
                order_by: &order_by,
                limit: 100,
            },
        )
        .await
        .unwrap();
        assert_eq!(output.matched_rows, 2);
        assert_eq!(output.payload["returned_rows"], 2);
        assert_eq!(output.payload["rows"][0]["site"], "ABBY");
        assert_eq!(output.payload["rows"][1]["mean_value"], 3.5);
        let ipc = directory.path().join("result.arrow");
        write_arrow_ipc(&ipc, &output.batches).unwrap();
        assert!(ipc.metadata().unwrap().len() > 0);
    }

    #[test]
    fn source_rows_stream_once_preserve_order_and_do_not_coerce_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("opaque-object");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "site\tvalue\tnative_qc\tnote").unwrap();
        writeln!(file, "HARV\t01.00\tpass\tfirst").unwrap();
        writeln!(file, "ABBY\t-9999\tmissing\tsecond").unwrap();
        writeln!(file, "OSBS\t18.72\tsuspect\tthird").unwrap();
        writeln!(file, "KONZ\t12.50\tpass\tfourth").unwrap();

        let output = execute_source_rows(&path, "observations.tsv", &[3, 1], &[]).unwrap();

        assert!(output.batches.is_empty());
        assert_eq!(output.matched_rows, 2);
        assert_eq!(output.payload["rows"][0]["source_index"], 3);
        assert_eq!(output.payload["rows"][0]["values"]["native_qc"], "pass");
        assert_eq!(output.payload["rows"][1]["source_index"], 1);
        assert_eq!(output.payload["rows"][1]["values"]["value"], "-9999");
        assert_eq!(output.payload["columns"][3], "note");
    }

    #[test]
    fn source_rows_validate_identity_projection_and_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "site,value").unwrap();
        writeln!(file, "HARV,1").unwrap();

        let duplicate = execute_source_rows(&path, "observations.csv", &[0, 0], &[])
            .unwrap_err()
            .to_string();
        assert!(duplicate.contains("unique"));
        let empty = execute_source_rows(&path, "observations.csv", &[], &[])
            .unwrap_err()
            .to_string();
        assert!(empty.contains("at least one"));
        let excessive = execute_source_rows(
            &path,
            "observations.csv",
            &(0..=MAX_SOURCE_ROWS as u64).collect::<Vec<_>>(),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(excessive.contains("at most 10000"));
        let unknown = execute_source_rows(&path, "observations.csv", &[0], &["qc".into()])
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("unknown source_rows field qc"));
        let outside = execute_source_rows(&path, "observations.csv", &[4], &[])
            .unwrap_err()
            .to_string();
        assert!(outside.contains("outside the source: [4]"));
        let unsupported = execute_source_rows(&path, "observations.parquet", &[0], &[])
            .unwrap_err()
            .to_string();
        assert!(unsupported.contains("original CSV and TSV"));
    }

    #[tokio::test]
    async fn geoparquet_queries_use_geoarrow_metadata_and_exact_intersection() {
        use std::collections::HashMap;

        use arrow_array::{ArrayRef, BinaryArray, StringArray};
        use arrow_schema::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;

        fn point_wkb(x: f64, y: f64) -> Vec<u8> {
            let mut bytes = vec![1];
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
            bytes
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plots.parquet");
        let metadata = HashMap::from([(
            "geo".to_owned(),
            json!({
                "version": "1.1.0",
                "primary_column": "geometry",
                "columns": {
                    "geometry": {
                        "encoding": "WKB",
                        "geometry_types": ["Point"],
                        "crs": null,
                        "edges": "planar",
                        "bbox": [1.0, 1.0, 10.0, 10.0]
                    }
                }
            })
            .to_string(),
        )]);
        let schema = Arc::new(Schema::new_with_metadata(
            vec![
                Field::new("plot", DataType::Utf8, false),
                Field::new("geometry", DataType::Binary, false),
            ],
            metadata,
        ));
        let geometries = [point_wkb(1.0, 1.0), point_wkb(10.0, 10.0)];
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["inside", "outside"])) as ArrayRef,
                Arc::new(BinaryArray::from_iter_values(
                    geometries.iter().map(Vec::as_slice),
                )) as ArrayRef,
            ],
        )
        .unwrap();
        let mut writer = ArrowWriter::try_new(File::create(&path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let output = execute_geoparquet_region(&path, "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))", 100)
            .await
            .unwrap();
        assert_eq!(output.matched_rows, 1);
        assert_eq!(output.payload["rows"][0]["plot"], "inside");
        assert_eq!(output.payload["engine"], "geodatafusion-0.5");
    }
}
