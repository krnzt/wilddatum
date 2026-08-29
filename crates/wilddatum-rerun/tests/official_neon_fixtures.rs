//! Opt-in regression test using NEON's published teaching subsets.
//!
//! Fixtures:
//! - https://ndownloader.figshare.com/files/7024955 (sample AOP LAS)
//! - https://ndownloader.figshare.com/files/21754221 (reduced SJER HDF5 cube)

use std::collections::BTreeMap;

use serde_json::json;
use wilddatum_core::{DatasetId, DatasetQuery, GeoGeometry};
use wilddatum_service::{ServicePaths, WildDatumService};

#[tokio::test]
#[ignore = "requires NEON_POINT_CLOUD_FIXTURE and NEON_HYPERSPECTRAL_FIXTURE"]
async fn renders_official_neon_point_cloud_and_hyperspectral_subsets() {
    let point_cloud = std::env::var("NEON_POINT_CLOUD_FIXTURE")
        .expect("set NEON_POINT_CLOUD_FIXTURE to NEON's sample LAS file");
    let hyperspectral = std::env::var("NEON_HYPERSPECTRAL_FIXTURE")
        .expect("set NEON_HYPERSPECTRAL_FIXTURE to NEON's reduced SJER HDF5 cube");
    let directory = tempfile::tempdir().unwrap();
    let service = WildDatumService::open(ServicePaths::under(
        directory.path().join("data"),
        directory.path().join("cache"),
    ))
    .unwrap();

    let point_manifest = service
        .import_local_file(std::path::Path::new(&point_cloud))
        .await
        .unwrap();
    assert_eq!(
        point_manifest.source_files[0].metadata["point_count"],
        6_609_829
    );
    assert_eq!(
        point_manifest
            .spatial_reference
            .as_ref()
            .unwrap()
            .code
            .as_deref(),
        Some("32611")
    );
    assert_eq!(
        point_manifest.source_files[0].metadata["crs_source"],
        "las_geokey_directory"
    );
    let point_view = service
        .create_view(
            "Official NEON point-cloud fixture".into(),
            vec![DatasetId(point_manifest.dataset_id.0.clone())],
        )
        .unwrap();
    let point_rrd = directory.path().join("point-cloud.rrd");
    wilddatum_rerun::write_recording(&service, &point_view.view_id.0, &point_rrd).unwrap();
    assert!(std::fs::metadata(point_rrd).unwrap().len() > 10_000_000);
    let bounds = &point_manifest.source_files[0].metadata["bounds"];
    let min_x = bounds["min"][0].as_f64().unwrap();
    let min_y = bounds["min"][1].as_f64().unwrap();
    let point_result = service
        .query_dataset(
            &point_manifest.dataset_id.0,
            DatasetQuery::PointCloudRegion {
                geometry: GeoGeometry {
                    geojson: json!({
                        "type": "Polygon",
                        "coordinates": [[
                            [min_x, min_y],
                            [min_x + 100.0, min_y],
                            [min_x + 100.0, min_y + 100.0],
                            [min_x, min_y + 100.0],
                            [min_x, min_y]
                        ]]
                    }),
                },
                crs: "source".into(),
                source_indices: vec![],
                classifications: vec![],
                elevation_min: None,
                elevation_max: None,
                resolution: None,
                level: None,
                point_limit: 1_000,
            },
        )
        .await
        .unwrap();
    assert!(point_result.row_count.unwrap() > 0);

    let hyperspectral_manifest = service
        .import_local_file(std::path::Path::new(&hyperspectral))
        .await
        .unwrap();
    let datasets = hyperspectral_manifest.source_files[0].metadata["hdf5_datasets"]
        .as_array()
        .unwrap();
    assert!(datasets.iter().any(|dataset| {
        dataset["path"] == "/SJER/Reflectance/Reflectance_Data"
            && dataset["shape"] == json!([500, 500, 107])
    }));
    assert_eq!(
        hyperspectral_manifest
            .spatial_reference
            .as_ref()
            .unwrap()
            .code
            .as_deref(),
        Some("32611")
    );
    assert_eq!(hyperspectral_manifest.cubes[0].scale_factor, Some(0.0001));
    assert_eq!(hyperspectral_manifest.cubes[0].no_data, Some(-9999.0));
    assert!(
        !hyperspectral_manifest.source_files[0]
            .metadata
            .contains_key("world_to_pixel")
    );
    assert!(
        service
            .scientific_inventory(&hyperspectral_manifest.dataset_id.0)
            .unwrap()
            .warnings
            .iter()
            .any(|warning| warning.contains("Spatial_Extent_meters disagree"))
    );
    let hyperspectral_view = service
        .create_view(
            "Official NEON hyperspectral fixture".into(),
            vec![DatasetId(hyperspectral_manifest.dataset_id.0.clone())],
        )
        .unwrap();
    let encoding = BTreeMap::from([
        (
            "hdf5_dataset".into(),
            json!("/SJER/Reflectance/Reflectance_Data"),
        ),
        ("spectral_axis".into(), json!(2)),
        ("red_band".into(), json!(14)),
        ("green_band".into(), json!(9)),
        ("blue_band".into(), json!(5)),
    ]);
    service
        .configure_layer_encoding(&hyperspectral_view.view_id.0, 1, "layer_1", encoding)
        .unwrap();
    let hyperspectral_rrd = directory.path().join("hyperspectral.rrd");
    wilddatum_rerun::write_recording(&service, &hyperspectral_view.view_id.0, &hyperspectral_rrd)
        .unwrap();
    assert!(std::fs::metadata(hyperspectral_rrd).unwrap().len() > 500_000);
    let spectrum = service
        .query_dataset(
            &hyperspectral_manifest.dataset_id.0,
            DatasetQuery::Spectrum {
                x: 250,
                y: 250,
                dataset_path: Some("/SJER/Reflectance/Reflectance_Data".into()),
                wavelength_dataset: Some(
                    "/SJER/Reflectance/Metadata/Spectral_Data/Wavelength".into(),
                ),
                spectral_axis: 2,
                wavelength_start_nm: Some(500.0),
                wavelength_end_nm: Some(700.0),
                scale_factor: Some(0.0001),
                add_offset: None,
                no_data: Some(-9999.0),
                bad_bands: vec![],
            },
        )
        .await
        .unwrap();
    assert!(spectrum.row_count.is_some_and(|count| count >= 9));
}
