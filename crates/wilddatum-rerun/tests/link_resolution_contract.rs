use hdf5_metno::types::FixedAscii;
use serde_json::json;
use wilddatum_core::SemanticSelection;
use wilddatum_service::{ServicePaths, WildDatumService};

#[tokio::test]
async fn renders_exact_pixel_and_linked_spectrum_inside_scientific_panels() {
    let directory = tempfile::tempdir().unwrap();
    let service = WildDatumService::open(ServicePaths::under(
        directory.path().join("data"),
        directory.path().join("cache"),
    ))
    .unwrap();
    let path = directory.path().join("reflectance.h5");
    let file = hdf5_metno::File::create(&path).unwrap();
    let group = file.create_group("SITE/Reflectance").unwrap();
    group
        .new_dataset::<u16>()
        .shape([2, 3, 4])
        .create("Reflectance_Data")
        .unwrap()
        .write_raw(&(0_u16..24).collect::<Vec<_>>())
        .unwrap();
    group
        .new_dataset::<f32>()
        .shape([4])
        .create("Wavelength")
        .unwrap()
        .write_raw(&[450.0_f32, 550.0, 650.0, 850.0])
        .unwrap();
    let coordinates = group.create_group("Metadata/Coordinate_System").unwrap();
    coordinates
        .new_dataset::<FixedAscii<6>>()
        .shape([1])
        .create("EPSG Code")
        .unwrap()
        .write_raw(&[FixedAscii::<6>::from_ascii(b"32611").unwrap()])
        .unwrap();
    drop(file);

    let manifest = service.import_local_file(&path).await.unwrap();
    let suggestions = service
        .suggest_views(std::slice::from_ref(&manifest.dataset_id.0))
        .unwrap();
    let suggestion = suggestions
        .suggestions
        .iter()
        .find(|suggestion| suggestion.recipe == "spectral_cube_v1")
        .unwrap();
    let view = service
        .create_view_from_suggestion(
            &suggestion.suggestion_id,
            std::slice::from_ref(&manifest.dataset_id.0),
            None,
        )
        .unwrap();
    let rgb = view
        .panels
        .iter()
        .find(|panel| panel.representation == "rgb")
        .unwrap();
    let selection = service
        .save_selection(
            &view.view_id.0,
            SemanticSelection::CubePixel {
                dataset_id: manifest.dataset_id,
                array_path: rgb.encoding["cube_array"].as_str().unwrap().into(),
                x: 1,
                y: 1,
                x_axis: 1,
                y_axis: 0,
                spectral_axis: 2,
                displayed_bands: vec![2, 1, 0],
            },
            json!({"source": "rerun_contract_test"}),
        )
        .unwrap();
    let resolution = service
        .resolve_selection_links(&selection.selection_id.0)
        .await
        .unwrap();
    let output = directory.path().join("linked-spectrum.rrd");
    wilddatum_rerun::write_recording_with_link_resolution(
        &service,
        &view.view_id.0,
        &output,
        Some(&resolution),
    )
    .unwrap();

    let bytes = std::fs::read(output).unwrap();
    for entity in [
        b"source_selection".as_slice(),
        b"spectrum_line".as_slice(),
        b"spectrum_samples".as_slice(),
        b"link_resolutions".as_slice(),
    ] {
        assert!(
            bytes.windows(entity.len()).any(|window| window == entity),
            "recording should contain {}",
            String::from_utf8_lossy(entity)
        );
    }
}
