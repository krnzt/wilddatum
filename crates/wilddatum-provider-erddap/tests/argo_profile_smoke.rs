use std::{path::Path, time::Duration};

use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
use wilddatum_core::{
    DatasetRequest, ProfileTrajectoryRecipeV1, ProfileValueSpec, ProviderKind, ResourceRecord,
    Result, VerticalAxisSpec, VerticalDirection, WildDatumError,
};
use wilddatum_provider_api::EcologicalDataProvider;
use wilddatum_provider_erddap::{ErddapProvider, config};
use wilddatum_service::{ServicePaths, WildDatumService};

const DATASET_ID: &str = "ArgoFloats";
const MAX_DOWNLOAD_BYTES: u64 = 5 * 1024 * 1024;
const MAX_PROFILE_ROWS: usize = 10_000;

#[derive(Debug)]
struct ProfileFields {
    trajectory: String,
    profile: String,
    time: String,
    latitude: String,
    longitude: String,
    vertical: String,
    value: String,
    qc: Option<String>,
    vertical_unit: Option<String>,
    value_unit: Option<String>,
    vertical_fills: Vec<String>,
    value_fills: Vec<String>,
}

#[derive(Debug)]
struct ProfileKey {
    trajectory: String,
    profile: i64,
}

#[tokio::test]
#[ignore = "bounded public Euro-Argo materialization and Rerun render; requires network"]
async fn euro_argo_profile_materializes_with_cf_metadata_and_renders() {
    tokio::time::timeout(Duration::from_secs(180), live_smoke())
        .await
        .expect("Euro-Argo smoke exceeded its 180 second deadline")
        .expect("Euro-Argo smoke failed");
}

async fn live_smoke() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let service = WildDatumService::open(ServicePaths::under(
        directory.path().join("data"),
        directory.path().join("cache"),
    ))?;
    let provider_kind = ProviderKind::Other("euro-argo".into());
    let provider = ErddapProvider::new(config::preset("euro-argo").unwrap().into())?
        .with_object_dir(service.paths().provider_objects_dir(&provider_kind))
        .with_download_limit_bytes(MAX_DOWNLOAD_BYTES);

    let resource = provider.resolve_resource(DATASET_ID).await?;
    if resource
        .provider_extensions
        .get("cdm_data_type")
        .and_then(Value::as_str)
        != Some("TrajectoryProfile")
    {
        return Err(WildDatumError::Invalid(
            "Euro-Argo no longer advertises ArgoFloats as a TrajectoryProfile".into(),
        ));
    }
    let fields = ProfileFields::discover(&resource)?;

    let start = (Utc::now() - ChronoDuration::days(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let discovery_request = request(
        vec![
            fields.trajectory.clone(),
            fields.profile.clone(),
            fields.time.clone(),
            fields.vertical.clone(),
        ],
        Some(start),
        vec![json!({
            "variable": fields.vertical,
            "op": "lte",
            "value": 1.0
        })],
    )?;
    let discovery = materialize(&provider, discovery_request).await?;
    assert_bounded(&discovery)?;
    let discovery_path = materialized_path(&service, &provider_kind, &discovery)?;
    let key = first_profile_key(&discovery_path, &fields)?;

    let mut variables = vec![
        fields.trajectory.clone(),
        fields.profile.clone(),
        fields.time.clone(),
        fields.latitude.clone(),
        fields.longitude.clone(),
        fields.vertical.clone(),
        fields.value.clone(),
    ];
    if let Some(qc) = &fields.qc {
        variables.push(qc.clone());
    }
    let exact_request = request(
        variables,
        None,
        vec![
            json!({
                "variable": fields.trajectory,
                "op": "eq",
                "value": key.trajectory
            }),
            json!({
                "variable": fields.profile,
                "op": "eq",
                "value": key.profile
            }),
        ],
    )?;
    let manifest = materialize(&provider, exact_request).await?;
    assert_bounded(&manifest)?;
    assert_cf_metadata(&manifest, &fields)?;
    let exact_path = materialized_path(&service, &provider_kind, &manifest)?;
    let rows = csv::Reader::from_path(&exact_path)
        .map_err(csv_error)?
        .records()
        .count();
    if rows == 0 || rows > MAX_PROFILE_ROWS {
        return Err(WildDatumError::Invalid(format!(
            "Euro-Argo profile returned {rows} rows; expected 1..={MAX_PROFILE_ROWS}"
        )));
    }

    service.save_manifest(&manifest)?;
    let view = service.create_view(
        format!("Euro-Argo {} cycle {}", key.trajectory, key.profile),
        vec![manifest.dataset_id.clone()],
    )?;
    let view = service.configure_profile_trajectory_view(
        &view.view_id.0,
        view.revision,
        "layer_1",
        ProfileTrajectoryRecipeV1 {
            trajectory_id_field: fields.trajectory,
            profile_id_field: fields.profile,
            time_field: Some(fields.time),
            latitude_field: fields.latitude,
            longitude_field: fields.longitude,
            vertical: VerticalAxisSpec {
                field: fields.vertical,
                direction: VerticalDirection::PositiveDown,
                unit: fields.vertical_unit,
                fill_values: fields.vertical_fills,
            },
            value: ProfileValueSpec {
                field: fields.value,
                unit: fields.value_unit,
                qc_field: fields.qc,
                accepted_qc: vec!["1".into(), "2".into()],
                fill_values: fields.value_fills,
            },
        },
    )?;
    let recording = directory.path().join("euro-argo-profile.rrd");
    wilddatum_rerun::write_recording(&service, &view.view_id.0, &recording)?;
    let bytes = std::fs::read(recording)?;
    if bytes.len() <= 5_000 {
        return Err(WildDatumError::Invalid(
            "Euro-Argo Rerun recording did not contain a substantive view".into(),
        ));
    }
    for entity in [
        b"map_observations".as_slice(),
        b"trajectory_lines".as_slice(),
        b"profile_observations".as_slice(),
        b"profile_lines".as_slice(),
        b"profile_trajectory_info".as_slice(),
    ] {
        if !bytes.windows(entity.len()).any(|window| window == entity) {
            return Err(WildDatumError::Invalid(format!(
                "Euro-Argo recording omitted {}",
                String::from_utf8_lossy(entity)
            )));
        }
    }
    Ok(())
}

impl ProfileFields {
    fn discover(resource: &ResourceRecord) -> Result<Self> {
        let variables = variables(resource)?;
        let trajectory = find_by_attribute(variables, "cf_role", "trajectory_id", |_| true)?;
        let profile = find_by_attribute(variables, "cf_role", "profile_id", |_| true)?;
        let time = find_by_attribute(variables, "standard_name", "time", |_| true)?;
        let latitude = find_by_attribute(variables, "standard_name", "latitude", |_| true)?;
        let longitude = find_by_attribute(variables, "standard_name", "longitude", |_| true)?;
        let vertical =
            find_by_attribute(variables, "standard_name", "sea_water_pressure", |name| {
                !name.contains("adjusted")
            })?;
        let value = find_by_attribute(
            variables,
            "standard_name",
            "sea_water_temperature",
            |name| !name.contains("adjusted"),
        )?;
        let expected_qc = format!("{value}_qc");
        let qc = variables.contains_key(&expected_qc).then_some(expected_qc);
        Ok(Self {
            trajectory,
            profile,
            time,
            latitude,
            longitude,
            vertical_unit: attribute(variables, &vertical, "units"),
            value_unit: attribute(variables, &value, "units"),
            vertical_fills: attribute(variables, &vertical, "_FillValue")
                .into_iter()
                .collect(),
            value_fills: attribute(variables, &value, "_FillValue")
                .into_iter()
                .collect(),
            vertical,
            value,
            qc,
        })
    }
}

fn variables(resource: &ResourceRecord) -> Result<&Map<String, Value>> {
    resource
        .provider_extensions
        .get("variables")
        .and_then(Value::as_object)
        .ok_or_else(|| WildDatumError::Invalid("ERDDAP resource omitted variable metadata".into()))
}

fn find_by_attribute(
    variables: &Map<String, Value>,
    attribute_name: &str,
    expected: &str,
    preferred: impl Fn(&str) -> bool,
) -> Result<String> {
    let mut matches = variables
        .keys()
        .filter(|name| attribute(variables, name, attribute_name).as_deref() == Some(expected))
        .cloned()
        .collect::<Vec<_>>();
    matches.sort_by_key(|name| !preferred(name));
    matches.into_iter().next().ok_or_else(|| {
        WildDatumError::Invalid(format!(
            "ERDDAP metadata has no variable with {attribute_name}={expected}"
        ))
    })
}

fn attribute(variables: &Map<String, Value>, variable: &str, name: &str) -> Option<String> {
    let value = variables.get(variable)?.get("attributes")?.get(name)?;
    value.as_str().map(str::to_owned).or_else(|| {
        value
            .as_f64()
            .map(|number| number.to_string())
            .or_else(|| value.as_i64().map(|number| number.to_string()))
    })
}

fn request(
    variables: Vec<String>,
    temporal_start: Option<String>,
    constraints: Vec<Value>,
) -> Result<DatasetRequest> {
    Ok(DatasetRequest {
        provider: ProviderKind::Other("euro-argo".into()),
        resource_id: DATASET_ID.into(),
        locations: vec![],
        temporal_start,
        temporal_end: None,
        spatial_filter: None,
        variables,
        release: None,
        package: "basic".into(),
        include_provisional: false,
        provider_options: serde_json::from_value(json!({
            "protocol": "tabledap",
            "output_format": "csv",
            "constraints": constraints
        }))?,
    })
}

async fn materialize(
    provider: &ErddapProvider,
    request: DatasetRequest,
) -> Result<wilddatum_core::DatasetManifest> {
    let mut plan = provider.plan_dataset(request).await?;
    plan.approved_at = Some(Utc::now());
    provider.materialize(plan, None).await
}

fn materialized_path(
    service: &WildDatumService,
    provider: &ProviderKind,
    manifest: &wilddatum_core::DatasetManifest,
) -> Result<std::path::PathBuf> {
    let object = manifest
        .source_files
        .first()
        .and_then(|source| source.local_object.as_deref())
        .ok_or_else(|| WildDatumError::Invalid("materialized asset has no local object".into()))?;
    Ok(service.paths().provider_objects_dir(provider).join(object))
}

fn first_profile_key(path: &Path, fields: &ProfileFields) -> Result<ProfileKey> {
    let mut reader = csv::Reader::from_path(path).map_err(csv_error)?;
    let headers = reader.headers().map_err(csv_error)?.clone();
    let trajectory = headers
        .iter()
        .position(|name| name == fields.trajectory)
        .ok_or_else(|| WildDatumError::Invalid("discovery omitted trajectory field".into()))?;
    let profile = headers
        .iter()
        .position(|name| name == fields.profile)
        .ok_or_else(|| WildDatumError::Invalid("discovery omitted profile field".into()))?;
    for record in reader.records() {
        let record = record.map_err(csv_error)?;
        let trajectory = record.get(trajectory).unwrap_or_default().trim();
        let profile = record
            .get(profile)
            .unwrap_or_default()
            .trim()
            .parse::<i64>();
        if !trajectory.is_empty()
            && let Ok(profile) = profile
        {
            return Ok(ProfileKey {
                trajectory: trajectory.into(),
                profile,
            });
        }
    }
    Err(WildDatumError::Invalid(
        "bounded discovery returned no usable Euro-Argo profile".into(),
    ))
}

fn assert_bounded(manifest: &wilddatum_core::DatasetManifest) -> Result<()> {
    let bytes = manifest
        .source_files
        .iter()
        .map(|file| file.size_bytes)
        .sum::<u64>();
    if bytes == 0 || bytes > MAX_DOWNLOAD_BYTES {
        return Err(WildDatumError::Invalid(format!(
            "Euro-Argo materialization used {bytes} bytes; expected 1..={MAX_DOWNLOAD_BYTES}"
        )));
    }
    Ok(())
}

fn assert_cf_metadata(
    manifest: &wilddatum_core::DatasetManifest,
    fields: &ProfileFields,
) -> Result<()> {
    let variables = manifest
        .provider_metadata
        .get("variables")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            WildDatumError::Invalid("manifest omitted ERDDAP variable metadata".into())
        })?;
    if attribute(variables, &fields.trajectory, "cf_role").as_deref() != Some("trajectory_id")
        || attribute(variables, &fields.profile, "cf_role").as_deref() != Some("profile_id")
        || attribute(variables, &fields.vertical, "standard_name").as_deref()
            != Some("sea_water_pressure")
        || attribute(variables, &fields.value, "standard_name").as_deref()
            != Some("sea_water_temperature")
    {
        return Err(WildDatumError::Invalid(
            "manifest did not preserve the CF profile/trajectory semantics".into(),
        ));
    }
    if manifest
        .citation
        .as_ref()
        .is_none_or(|citation| citation.text.trim().is_empty())
    {
        return Err(WildDatumError::Invalid(
            "Euro-Argo manifest omitted a citation".into(),
        ));
    }
    Ok(())
}

fn csv_error(error: csv::Error) -> WildDatumError {
    WildDatumError::Invalid(format!("cannot read ERDDAP CSV: {error}"))
}
