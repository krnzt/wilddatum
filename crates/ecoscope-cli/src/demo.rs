use std::{collections::BTreeMap, io::Read, path::Path};

use anyhow::{Context, Result, bail};
use ecoscope_core::{
    DatasetManifest, ProfileTrajectoryRecipeV1, ProfileValueSpec, VerticalAxisSpec,
    VerticalDirection,
};
use ecoscope_service::EcoScopeService;
use futures::StreamExt;
use hdf5_metno as hdf5;
use las::{Builder, Color, Point, Writer, point::Classification};
use ndarray::Array3;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const SYNTHETIC_LAS: &str = "ecoscope-canopy.las";
const SYNTHETIC_HDF5: &str = "ecoscope-reflectance.h5";
const PROFILE_TRAJECTORY_CSV: &str = "ecoscope-profile-trajectory.csv";
const PROFILE_TRAJECTORY_FIXTURE: &str =
    include_str!("../../ecoscope-rerun/tests/fixtures/profile_trajectory.csv");
const NEON_LAS: RemoteFixture = RemoteFixture {
    filename: "neon-aop-teaching.las",
    url: "https://ndownloader.figshare.com/files/7024955",
    size: 185_075_623,
    sha256: "9bab74666a6f9a5767f23323b16076ce61ed9f03e8fa26c514474e1c38193d66",
};
const NEON_HDF5: RemoteFixture = RemoteFixture {
    filename: "neon-sjer-reflectance.h5",
    url: "https://ndownloader.figshare.com/files/21754221",
    size: 50_119_130,
    sha256: "7a693d9d750f06e74c84ce161f9e308f38b4460727fabd2529895e110b5cb776",
};

#[derive(Debug, Serialize)]
pub struct DemoResult {
    pub kind: &'static str,
    pub view_id: String,
    pub dataset_ids: Vec<String>,
    pub recording: String,
    pub next: String,
}

#[derive(Debug, Clone, Copy)]
struct RemoteFixture {
    filename: &'static str,
    url: &'static str,
    size: u64,
    sha256: &'static str,
}

struct DemoDefinition<'a> {
    kind: &'static str,
    name: &'a str,
    point_path: &'a Path,
    cube_path: &'a Path,
    cube_array: &'a str,
    wavelength_dataset: Option<&'a str>,
    rgb: [u32; 3],
}

pub async fn synthetic(service: &EcoScopeService) -> Result<DemoResult> {
    let source_dir = service.paths().data_dir.join("demos/synthetic-v1");
    std::fs::create_dir_all(&source_dir)?;
    let point_path = source_dir.join(SYNTHETIC_LAS);
    let cube_path = source_dir.join(SYNTHETIC_HDF5);
    if !point_path.is_file() {
        write_canopy_las(&point_path)?;
    }
    if !cube_path.is_file() {
        write_reflectance_cube(&cube_path)?;
    }
    build_demo(
        service,
        DemoDefinition {
            kind: "synthetic",
            name: "EcoScope synthetic canopy and reflectance",
            point_path: &point_path,
            cube_path: &cube_path,
            cube_array: "/EcoScope/Reflectance",
            wavelength_dataset: Some("/EcoScope/Wavelength"),
            rgb: [5, 3, 1],
        },
    )
    .await
}

pub async fn official_neon(service: &EcoScopeService, accept_download: bool) -> Result<DemoResult> {
    let source_dir = service.paths().data_dir.join("demos/neon-teaching-v1");
    std::fs::create_dir_all(&source_dir)?;
    let point_path = source_dir.join(NEON_LAS.filename);
    let cube_path = source_dir.join(NEON_HDF5.filename);
    let missing = [(&point_path, NEON_LAS), (&cube_path, NEON_HDF5)]
        .into_iter()
        .filter(|(path, _)| !path.is_file())
        .collect::<Vec<_>>();
    if !missing.is_empty() && !accept_download {
        let bytes = missing.iter().map(|(_, fixture)| fixture.size).sum::<u64>();
        bail!(
            "the official NEON demo needs to download {bytes} bytes; rerun with --accept-download"
        );
    }
    for (path, fixture) in missing {
        download_verified(path, fixture).await?;
    }
    verify_file(&point_path, NEON_LAS)?;
    verify_file(&cube_path, NEON_HDF5)?;
    build_demo(
        service,
        DemoDefinition {
            kind: "neon",
            name: "Official NEON LiDAR and hyperspectral teaching subsets",
            point_path: &point_path,
            cube_path: &cube_path,
            cube_array: "/SJER/Reflectance/Reflectance_Data",
            wavelength_dataset: Some("/SJER/Reflectance/Metadata/Spectral_Data/Wavelength"),
            rgb: [14, 9, 5],
        },
    )
    .await
}

pub async fn profile_trajectory(service: &EcoScopeService) -> Result<DemoResult> {
    let source_dir = service.paths().data_dir.join("demos/profile-trajectory-v1");
    std::fs::create_dir_all(&source_dir)?;
    let source_path = source_dir.join(PROFILE_TRAJECTORY_CSV);
    if !source_path.is_file() {
        std::fs::write(&source_path, PROFILE_TRAJECTORY_FIXTURE)?;
    }
    let manifest = import_or_reuse(service, &source_path).await?;
    let view = service.create_view(
        "EcoScope synthetic profile and trajectory".into(),
        vec![manifest.dataset_id.clone()],
    )?;
    let configured = service.configure_profile_trajectory_view(
        &view.view_id.0,
        view.revision,
        "layer_1",
        ProfileTrajectoryRecipeV1 {
            trajectory_id_field: "platform_number".into(),
            profile_id_field: "cycle_number".into(),
            time_field: Some("time".into()),
            latitude_field: "latitude".into(),
            longitude_field: "longitude".into(),
            vertical: VerticalAxisSpec {
                field: "pres".into(),
                direction: VerticalDirection::PositiveDown,
                unit: Some("decibar".into()),
                fill_values: vec![],
            },
            value: ProfileValueSpec {
                field: "temp_adjusted".into(),
                unit: Some("degree_Celsius".into()),
                qc_field: Some("temp_adjusted_qc".into()),
                accepted_qc: vec!["1".into(), "2".into()],
                fill_values: vec![],
            },
        },
    )?;
    let view = service.patch_view(
        &configured.view_id.0,
        configured.revision,
        json!({"layout": "single", "provenance_visible": false}),
    )?;
    let recording = service
        .paths()
        .views_dir
        .join(format!("{}.rrd", view.view_id));
    ecoscope_rerun::write_recording(service, &view.view_id.0, &recording)?;
    Ok(DemoResult {
        kind: "profile_trajectory",
        view_id: view.view_id.0,
        dataset_ids: vec![manifest.dataset_id.0],
        recording: recording.display().to_string(),
        next: "Open the linked Rerun map/profile view and click an observation to prove the browser instance-selection contract.".into(),
    })
}

async fn build_demo(
    service: &EcoScopeService,
    definition: DemoDefinition<'_>,
) -> Result<DemoResult> {
    let point = import_or_reuse(service, definition.point_path).await?;
    let cube = import_or_reuse(service, definition.cube_path).await?;
    let view = service.create_view(
        definition.name.into(),
        vec![point.dataset_id.clone(), cube.dataset_id.clone()],
    )?;
    let mut encoding = BTreeMap::from([
        ("cube_array".into(), json!(definition.cube_array)),
        ("y_axis".into(), json!(0)),
        ("x_axis".into(), json!(1)),
        ("spectral_axis".into(), json!(2)),
        ("red_band".into(), json!(definition.rgb[0])),
        ("green_band".into(), json!(definition.rgb[1])),
        ("blue_band".into(), json!(definition.rgb[2])),
    ]);
    if let Some(path) = definition.wavelength_dataset {
        encoding.insert("wavelength_dataset".into(), json!(path));
    }
    let configured =
        service.configure_layer_encoding(&view.view_id.0, view.revision, "layer_2", encoding)?;
    let recording = service
        .paths()
        .views_dir
        .join(format!("{}.rrd", configured.view_id));
    ecoscope_rerun::write_recording(service, &configured.view_id.0, &recording)?;
    Ok(DemoResult {
        kind: definition.kind,
        view_id: configured.view_id.0,
        dataset_ids: vec![point.dataset_id.0, cube.dataset_id.0],
        recording: recording.display().to_string(),
        next: "Open the view, select a LiDAR return or cube pixel, then ask the agent to inspect and query the selection.".into(),
    })
}

async fn import_or_reuse(service: &EcoScopeService, path: &Path) -> Result<DatasetManifest> {
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("demo source has no filename")?;
    for manifest in service.list_manifests()? {
        if manifest
            .source_files
            .first()
            .is_some_and(|source| source.original_name == filename)
            && manifest.source_files.first().is_some_and(|source| {
                service
                    .validate_local_asset(&source.asset_id.0)
                    .unwrap_or(false)
            })
        {
            return Ok(manifest);
        }
    }
    service.import_local_file(path).await.map_err(Into::into)
}

fn write_canopy_las(path: &Path) -> Result<()> {
    let mut builder = Builder::from((1, 4));
    builder.point_format = las::point::Format::new(2)?;
    builder.system_identifier = "EcoScope synthetic demo".into();
    builder.generating_software = format!("EcoScope {}", env!("CARGO_PKG_VERSION"));
    let header = builder.into_header()?;
    let mut writer = Writer::from_path(path, header)?;
    for y in 0..32 {
        for x in 0..32 {
            let x = f64::from(x);
            let y = f64::from(y);
            writer.write_point(Point {
                x,
                y,
                z: 0.15 * (x / 5.0).sin() + 0.1 * (y / 7.0).cos(),
                intensity: 250,
                return_number: 1,
                number_of_returns: 1,
                classification: Classification::Ground,
                color: Some(Color::new(8_000, 10_000, 6_000)),
                ..Default::default()
            })?;
            let canopy = canopy_height(x, y);
            if canopy > 1.0 {
                writer.write_point(Point {
                    x: x + 0.18 * (y * 0.7).sin(),
                    y: y + 0.18 * (x * 0.6).cos(),
                    z: canopy,
                    intensity: (1_000.0 + canopy * 300.0) as u16,
                    return_number: 1,
                    number_of_returns: 2,
                    classification: if canopy > 5.0 {
                        Classification::HighVegetation
                    } else {
                        Classification::MediumVegetation
                    },
                    color: Some(Color::new(
                        (7_000.0 + canopy * 500.0) as u16,
                        (16_000.0 + canopy * 1_600.0) as u16,
                        (5_000.0 + canopy * 350.0) as u16,
                    )),
                    ..Default::default()
                })?;
            }
        }
    }
    writer.close()?;
    Ok(())
}

fn canopy_height(x: f64, y: f64) -> f64 {
    [
        (9.0, 10.0, 12.0, 5.0),
        (22.0, 20.0, 16.0, 6.5),
        (10.0, 25.0, 9.0, 4.0),
    ]
    .into_iter()
    .map(|(cx, cy, height, spread)| {
        let radius = ((x - cx).powi(2) + (y - cy).powi(2)) / (2.0 * spread * spread);
        height * (-radius).exp()
    })
    .fold(0.0, f64::max)
}

fn write_reflectance_cube(path: &Path) -> Result<()> {
    const EDGE: usize = 32;
    const BANDS: usize = 16;
    let mut values = Array3::<f32>::zeros((EDGE, EDGE, BANDS));
    for y in 0..EDGE {
        for x in 0..EDGE {
            let canopy = (canopy_height(x as f64, y as f64) / 16.0).clamp(0.0, 1.0) as f32;
            for band in 0..BANDS {
                let wavelength = 450.0 + band as f32 * 40.0;
                let green_peak = (-((wavelength - 550.0) / 65.0).powi(2)).exp();
                let red_absorption = (-((wavelength - 670.0) / 45.0).powi(2)).exp();
                let red_edge = 1.0 / (1.0 + (-(wavelength - 720.0) / 24.0).exp());
                values[[y, x, band]] = 0.04
                    + canopy * (0.08 * green_peak - 0.045 * red_absorption + 0.48 * red_edge)
                    + (x + y) as f32 * 0.0004;
            }
        }
    }
    let file = hdf5::File::create(path)?;
    let group = file.create_group("EcoScope")?;
    group
        .new_dataset_builder()
        .deflate(3)
        .with_data(&values)
        .create("Reflectance")?;
    let wavelengths = (0..BANDS)
        .map(|band| 450.0_f32 + band as f32 * 40.0)
        .collect::<Vec<_>>();
    group
        .new_dataset_builder()
        .with_data(&wavelengths)
        .create("Wavelength")?;
    Ok(())
}

async fn download_verified(path: &Path, fixture: RemoteFixture) -> Result<()> {
    let partial = path.with_extension("partial");
    let response = reqwest::Client::new()
        .get(fixture.url)
        .send()
        .await?
        .error_for_status()?;
    let mut output = tokio::fs::File::create(&partial).await?;
    let mut stream = response.bytes_stream();
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size += chunk.len() as u64;
        hash.update(&chunk);
        output.write_all(&chunk).await?;
    }
    output.flush().await?;
    drop(output);
    let actual_hash = format!("{:x}", hash.finalize());
    if size != fixture.size || actual_hash != fixture.sha256 {
        let _ = tokio::fs::remove_file(&partial).await;
        bail!(
            "download verification failed for {}: got {size} bytes and {actual_hash}",
            fixture.filename
        );
    }
    tokio::fs::rename(&partial, path).await?;
    Ok(())
}

fn verify_file(path: &Path, fixture: RemoteFixture) -> Result<()> {
    let mut input = std::fs::File::open(path)?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hash.update(&buffer[..read]);
    }
    let actual_hash = format!("{:x}", hash.finalize());
    if size != fixture.size || actual_hash != fixture.sha256 {
        bail!(
            "cached fixture {} does not match its published size and SHA-256",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ecoscope_service::ServicePaths;

    use super::*;

    #[tokio::test]
    async fn synthetic_demo_uses_real_multimodal_adapters() {
        let directory = tempfile::tempdir().unwrap();
        let service = EcoScopeService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        let demo = synthetic(&service).await.unwrap();
        assert_eq!(demo.dataset_ids.len(), 2);
        let view = service.get_view(&demo.view_id).unwrap();
        assert_eq!(view.layers.len(), 2);
        assert_eq!(
            view.layers[1].encoding["cube_array"],
            "/EcoScope/Reflectance"
        );
        assert!(std::fs::metadata(demo.recording).unwrap().len() > 10_000);

        let manifests = demo
            .dataset_ids
            .iter()
            .map(|id| service.get_manifest(id).unwrap())
            .collect::<Vec<_>>();
        assert!(manifests.iter().any(|manifest| {
            manifest
                .modalities
                .contains(&ecoscope_core::Modality::PointCloud)
        }));
        assert!(manifests.iter().any(|manifest| {
            manifest
                .cubes
                .iter()
                .any(|cube| cube.array_path == "/EcoScope/Reflectance")
        }));
    }

    #[tokio::test]
    async fn profile_trajectory_demo_uses_the_validated_service_contract() {
        let directory = tempfile::tempdir().unwrap();
        let service = EcoScopeService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();

        let demo = profile_trajectory(&service).await.unwrap();
        let view = service.get_view(&demo.view_id).unwrap();

        assert_eq!(view.revision, 3);
        assert_eq!(
            view.layers[0].encoding["view_kind"],
            "profile_trajectory_v1"
        );
        assert_eq!(
            view.layers[0].encoding["selection_mapping"]["kind"],
            "source_row_index"
        );
        assert_eq!(view.layers[0].encoding["selection_mapping"]["stride"], 1);
        assert!(std::fs::metadata(demo.recording).unwrap().len() > 10_000);
    }
}
