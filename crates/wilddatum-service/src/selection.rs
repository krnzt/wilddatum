//! Conversion of durable semantic viewer state into reproducible data queries.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use wilddatum_core::{
    DatasetManifest, DatasetQuery, EcoLayer, EcoViewSpec, GeoGeometry, LINK_RESOLUTION_VERSION,
    LinkExactness, LinkResolutionStatus, LinkedResultSummary, Modality, PINNED_RERUN_VERSION,
    PROFILE_TRAJECTORY_VIEW_KIND, QueryFilter, ResolvedViewLink, Result, ResultRecord,
    SelectionLinkResolution, SelectionRecord, SemanticSelection, Transformation, ViewLinkRule,
    WildDatumError,
};

use super::WildDatumService;

impl WildDatumService {
    pub fn get_selection(&self, selection_id: &str) -> Result<SelectionRecord> {
        let text = self
            .connection()?
            .query_row(
                "SELECT json FROM selections WHERE id=?1",
                params![selection_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| WildDatumError::Internal(format!("database error: {error}")))?
            .ok_or_else(|| WildDatumError::NotFound(format!("selection {selection_id}")))?;
        serde_json::from_str(&text).map_err(WildDatumError::from)
    }

    /// Re-run the exact structured selection against one compatible view
    /// layer. Ambiguous multimodal selections require an explicit dataset ID.
    pub async fn query_selection(
        &self,
        selection_id: &str,
        requested_dataset_id: Option<&str>,
        point_limit: u64,
    ) -> Result<ResultRecord> {
        let selection = self.get_selection(selection_id)?;
        let view = self.get_view(&selection.view_id.0)?;
        let (dataset_id, query) =
            self.selection_to_query(&view, &selection, requested_dataset_id, point_limit)?;
        let mut result = self.query_dataset(&dataset_id, query).await?;
        result.source_selection = Some(selection.selection_id.clone());
        result.transformations.push(Transformation {
            name: "semantic_selection_query".into(),
            version: "1".into(),
            parameters: json!({
                "selection_id": selection.selection_id,
                "view_id": selection.view_id,
                "view_revision": selection.revision
            }),
            created_at: Utc::now(),
        });
        self.put_json(
            "results",
            &result.result_id.0,
            &result,
            result.created_at.to_rfc3339(),
        )?;
        Ok(result)
    }

    /// Evaluate the server-authored v2 link rules applicable to one durable
    /// selection. Unavailable links are reported structurally and never run.
    pub async fn resolve_selection_links(
        &self,
        selection_id: &str,
    ) -> Result<SelectionLinkResolution> {
        let selection = self.get_selection(selection_id)?;
        let view = self.get_view(&selection.view_id.0)?;
        if view.revision != selection.revision {
            return Err(WildDatumError::Conflict(format!(
                "selection {} belongs to view revision {}, but the current view revision is {}",
                selection.selection_id.0, selection.revision, view.revision
            )));
        }
        let (source_selection, source_dataset) = selection_link_source(&selection.selection);
        let mut links = Vec::new();
        for rule in &view.link_rules {
            if rule.source_selection != source_selection
                || !link_source_matches(&view, rule, source_dataset)?
            {
                continue;
            }
            if rule.exactness == LinkExactness::Unavailable {
                links.push(ResolvedViewLink {
                    source_panel: rule.source_panel.clone(),
                    target_panel: rule.target_panel.clone(),
                    resolver: rule.resolver.clone(),
                    exactness: rule.exactness.clone(),
                    status: LinkResolutionStatus::Unavailable,
                    explanation: rule.explanation.clone(),
                    derived_selection: None,
                    result: None,
                });
                continue;
            }
            if rule.exactness != LinkExactness::Exact {
                return Err(WildDatumError::Invalid(format!(
                    "link resolver {} with exactness {:?} is not executable",
                    rule.resolver, rule.exactness
                )));
            }
            match rule.resolver.as_str() {
                "cube_pixel_to_spectrum" => {
                    let result = self
                        .resolve_cube_pixel_spectrum(&view, &selection, rule)
                        .await?;
                    links.push(resolved_result_link(rule, result));
                }
                "world_to_raster_pixel" => {
                    let Some(derived) =
                        self.resolve_world_point_to_cube_pixel(&view, &selection, rule)?
                    else {
                        links.push(ResolvedViewLink {
                            source_panel: rule.source_panel.clone(),
                            target_panel: rule.target_panel.clone(),
                            resolver: rule.resolver.clone(),
                            exactness: rule.exactness.clone(),
                            status: LinkResolutionStatus::Unavailable,
                            explanation: "The selected point is outside the target cube footprint"
                                .into(),
                            derived_selection: None,
                            result: None,
                        });
                        continue;
                    };
                    links.push(ResolvedViewLink {
                        source_panel: rule.source_panel.clone(),
                        target_panel: rule.target_panel.clone(),
                        resolver: rule.resolver.clone(),
                        exactness: rule.exactness.clone(),
                        status: LinkResolutionStatus::Resolved,
                        explanation: rule.explanation.clone(),
                        derived_selection: Some(derived.clone()),
                        result: None,
                    });
                    let derived_record = SelectionRecord {
                        selection_id: selection.selection_id.clone(),
                        view_id: selection.view_id.clone(),
                        revision: selection.revision,
                        selection: derived,
                        summary: selection.summary.clone(),
                        created_at: selection.created_at,
                    };
                    for chained_rule in view.link_rules.iter().filter(|candidate| {
                        candidate.source_panel == rule.target_panel
                            && candidate.source_selection == "cube_pixel"
                    }) {
                        if chained_rule.resolver != "cube_pixel_to_spectrum"
                            || chained_rule.exactness != LinkExactness::Exact
                        {
                            continue;
                        }
                        let result = self
                            .resolve_cube_pixel_spectrum(&view, &derived_record, chained_rule)
                            .await?;
                        links.push(resolved_result_link(chained_rule, result));
                    }
                }
                _ => {
                    return Err(WildDatumError::Invalid(format!(
                        "link resolver {} is not executable",
                        rule.resolver
                    )));
                }
            }
        }
        Ok(SelectionLinkResolution {
            version: LINK_RESOLUTION_VERSION,
            selection_id: selection.selection_id,
            view_id: selection.view_id,
            view_revision: selection.revision,
            links,
        })
    }

    fn resolve_world_point_to_cube_pixel(
        &self,
        view: &EcoViewSpec,
        selection: &SelectionRecord,
        rule: &ViewLinkRule,
    ) -> Result<Option<SemanticSelection>> {
        let SemanticSelection::PointSet { spatial_query, .. } = &selection.selection else {
            return Err(WildDatumError::Invalid(
                "world_to_raster_pixel requires a point-set selection".into(),
            ));
        };
        let coordinates = spatial_query
            .get("geometry")
            .and_then(|geometry| geometry.get("geojson").or(Some(geometry)))
            .and_then(|geometry| geometry.get("coordinates"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                WildDatumError::Invalid(
                    "point selection requires source-coordinate point geometry".into(),
                )
            })?;
        let world_x = coordinates.first().and_then(Value::as_f64).ok_or_else(|| {
            WildDatumError::Invalid("point selection x coordinate must be finite".into())
        })?;
        let world_y = coordinates.get(1).and_then(Value::as_f64).ok_or_else(|| {
            WildDatumError::Invalid("point selection y coordinate must be finite".into())
        })?;
        if !world_x.is_finite() || !world_y.is_finite() {
            return Err(WildDatumError::Invalid(
                "point selection coordinates must be finite".into(),
            ));
        }
        let target_panel = view
            .panels
            .iter()
            .find(|panel| panel.id == rule.target_panel)
            .ok_or_else(|| WildDatumError::NotFound(format!("panel {}", rule.target_panel)))?;
        let source_panel = view
            .panels
            .iter()
            .find(|panel| panel.id == rule.source_panel)
            .ok_or_else(|| WildDatumError::NotFound(format!("panel {}", rule.source_panel)))?;
        let source_layer = view
            .layers
            .iter()
            .find(|layer| layer.id == source_panel.layer_id)
            .ok_or_else(|| WildDatumError::NotFound(format!("layer {}", source_panel.layer_id)))?;
        let target_layer = view
            .layers
            .iter()
            .find(|layer| layer.id == target_panel.layer_id)
            .ok_or_else(|| WildDatumError::NotFound(format!("layer {}", target_panel.layer_id)))?;
        let source_crs = source_layer.encoding.get("crs").and_then(Value::as_str);
        let target_crs = target_layer.encoding.get("crs").and_then(Value::as_str);
        if source_crs.is_none_or(|crs| Some(crs) != target_crs) {
            return Err(WildDatumError::Invalid(
                "world_to_raster_pixel requires identical authoritative layer CRS values".into(),
            ));
        }
        if target_panel
            .encoding
            .get("world_to_pixel")
            .and_then(|mapping| mapping.get("kind"))
            .and_then(Value::as_str)
            != Some("north_up_affine_v1")
        {
            return Err(WildDatumError::Invalid(
                "target cube has no validated north-up world-to-pixel mapping".into(),
            ));
        }
        let transform = target_panel
            .encoding
            .get("affine_transform")
            .and_then(Value::as_array)
            .filter(|values| values.len() == 6)
            .ok_or_else(|| {
                WildDatumError::Invalid("target cube has no validated affine transform".into())
            })?
            .iter()
            .map(Value::as_f64)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                WildDatumError::Invalid("target affine transform must be numeric".into())
            })?;
        if transform[1] <= 0.0
            || transform[5] >= 0.0
            || transform[2].abs() > f64::EPSILON
            || transform[4].abs() > f64::EPSILON
        {
            return Err(WildDatumError::Invalid(
                "world_to_raster_pixel currently requires a north-up affine transform".into(),
            ));
        }
        let pixel_x = ((world_x - transform[0]) / transform[1]).floor();
        let pixel_y = ((world_y - transform[3]) / transform[5]).floor();
        let shape = target_panel
            .encoding
            .get("source_shape")
            .and_then(Value::as_array)
            .ok_or_else(|| WildDatumError::Invalid("target cube shape is unavailable".into()))?;
        let x_axis = target_panel
            .encoding
            .get("x_axis")
            .and_then(Value::as_u64)
            .unwrap_or(1) as usize;
        let y_axis = target_panel
            .encoding
            .get("y_axis")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let spectral_axis = target_panel
            .encoding
            .get("spectral_axis")
            .and_then(Value::as_u64)
            .unwrap_or(2) as u32;
        let width = shape
            .get(x_axis)
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let height = shape
            .get(y_axis)
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if pixel_x < 0.0 || pixel_y < 0.0 || pixel_x >= width as f64 || pixel_y >= height as f64 {
            return Ok(None);
        }
        let displayed_bands = ["red_band", "green_band", "blue_band", "band"]
            .into_iter()
            .filter_map(|key| target_panel.encoding.get(key).and_then(Value::as_u64))
            .filter_map(|value| u32::try_from(value).ok())
            .collect();
        Ok(Some(SemanticSelection::CubePixel {
            dataset_id: target_layer.dataset_id.clone(),
            array_path: target_panel
                .encoding
                .get("cube_array")
                .and_then(Value::as_str)
                .ok_or_else(|| WildDatumError::Invalid("target cube array is unavailable".into()))?
                .into(),
            x: pixel_x as u64,
            y: pixel_y as u64,
            x_axis: x_axis as u32,
            y_axis: y_axis as u32,
            spectral_axis,
            displayed_bands,
        }))
    }

    async fn resolve_cube_pixel_spectrum(
        &self,
        view: &EcoViewSpec,
        selection: &SelectionRecord,
        rule: &ViewLinkRule,
    ) -> Result<ResultRecord> {
        let SemanticSelection::CubePixel {
            dataset_id,
            array_path,
            x,
            y,
            x_axis,
            y_axis,
            spectral_axis,
            ..
        } = &selection.selection
        else {
            return Err(WildDatumError::Invalid(
                "cube_pixel_to_spectrum requires a cube-pixel selection".into(),
            ));
        };
        // Reuse the authoritative selection validator before choosing the
        // wavelength-aware common-case query below.
        let (_, fallback_query) = self.selection_to_query(view, selection, None, 100_000)?;
        let source_panel = view
            .panels
            .iter()
            .find(|panel| panel.id == rule.source_panel)
            .ok_or_else(|| WildDatumError::NotFound(format!("panel {}", rule.source_panel)))?;
        let target_panel = view
            .panels
            .iter()
            .find(|panel| panel.id == rule.target_panel)
            .ok_or_else(|| WildDatumError::NotFound(format!("panel {}", rule.target_panel)))?;
        if source_panel.layer_id != target_panel.layer_id {
            return Err(WildDatumError::Invalid(
                "cube pixel and spectrum panels must reference the same validated cube layer"
                    .into(),
            ));
        }
        if source_panel
            .encoding
            .get("cube_array")
            .and_then(Value::as_str)
            != Some(array_path)
        {
            return Err(WildDatumError::Invalid(
                "cube selection array does not match the accepted source panel".into(),
            ));
        }

        let query = if (*y_axis, *x_axis, *spectral_axis) == (0, 1, 2) {
            DatasetQuery::Spectrum {
                x: *x,
                y: *y,
                dataset_path: Some(array_path.clone()),
                wavelength_dataset: source_panel
                    .encoding
                    .get("wavelength_dataset")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                spectral_axis: *spectral_axis,
                wavelength_start_nm: None,
                wavelength_end_nm: None,
                scale_factor: source_panel
                    .encoding
                    .get("scale_factor")
                    .and_then(Value::as_f64),
                add_offset: source_panel
                    .encoding
                    .get("add_offset")
                    .and_then(Value::as_f64),
                no_data: source_panel.encoding.get("no_data").and_then(Value::as_f64),
                bad_bands: source_panel
                    .encoding
                    .get("bad_bands")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_u64)
                            .filter_map(|value| u32::try_from(value).ok())
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        } else {
            fallback_query
        };
        let mut result = self.query_dataset(&dataset_id.0, query).await?;
        result.source_selection = Some(selection.selection_id.clone());
        result.transformations.push(Transformation {
            name: "view_link_resolution".into(),
            version: LINK_RESOLUTION_VERSION.to_string(),
            parameters: json!({
                "view_id": selection.view_id,
                "view_revision": selection.revision,
                "selection_id": selection.selection_id,
                "source_panel": rule.source_panel,
                "target_panel": rule.target_panel,
                "resolver": rule.resolver,
                "exactness": rule.exactness,
            }),
            created_at: Utc::now(),
        });
        self.put_json(
            "results",
            &result.result_id.0,
            &result,
            result.created_at.to_rfc3339(),
        )?;
        Ok(result)
    }

    fn selection_to_query(
        &self,
        view: &EcoViewSpec,
        record: &SelectionRecord,
        requested_dataset_id: Option<&str>,
        point_limit: u64,
    ) -> Result<(String, DatasetQuery)> {
        match &record.selection {
            SemanticSelection::Rows {
                dataset_id,
                predicate,
                row_count,
            } => {
                ensure_dataset_in_view(view, &dataset_id.0)?;
                if let Some(requested) = requested_dataset_id
                    && requested != dataset_id.0
                {
                    return Err(WildDatumError::Invalid(format!(
                        "row selection belongs to dataset {}, not requested dataset {requested}",
                        dataset_id.0
                    )));
                }
                if let Some(query) = verified_source_row_query(
                    view,
                    &dataset_id.0,
                    predicate,
                    *row_count,
                )? {
                    return Ok((dataset_id.0.clone(), query));
                }
                let filters = parse_selection_filters(predicate)?;
                Ok((
                    dataset_id.0.clone(),
                    DatasetQuery::Table {
                        select: vec![],
                        filters,
                        group_by: vec![],
                        aggregates: vec![],
                        order_by: vec![],
                        limit: 100_000,
                    },
                ))
            }
            SemanticSelection::TimeInterval { start, end, .. } => {
                let layer = select_layer(view, requested_dataset_id, |layer| {
                    matches!(
                        layer.modality,
                        Modality::Tabular | Modality::TimeSeries | Modality::Vector
                    )
                        && layer.encoding.contains_key("time_field")
                })?;
                let time_field = layer.encoding["time_field"]
                    .as_str()
                    .ok_or_else(|| WildDatumError::Invalid("time_field must be a string".into()))?;
                Ok((
                    layer.dataset_id.0.clone(),
                    DatasetQuery::Table {
                        select: vec![],
                        filters: vec![
                            QueryFilter {
                                field: time_field.into(),
                                op: "gte".into(),
                                value: json!(start.to_rfc3339()),
                            },
                            QueryFilter {
                                field: time_field.into(),
                                op: "lte".into(),
                                value: json!(end.to_rfc3339()),
                            },
                        ],
                        group_by: vec![],
                        aggregates: vec![],
                        order_by: vec![],
                        limit: 100_000,
                    },
                ))
            }
            SemanticSelection::MapRegion { geometry, crs } => {
                let layer = select_layer(view, requested_dataset_id, |layer| {
                    matches!(
                        layer.modality,
                        Modality::Vector | Modality::Raster | Modality::PointCloud
                    )
                })?;
                let query = match layer.modality {
                    Modality::Vector => DatasetQuery::VectorRegion {
                        geometry: geometry.clone(),
                        crs: crs.clone(),
                    },
                    Modality::Raster => DatasetQuery::RasterRegion {
                        geometry: geometry.clone(),
                        crs: crs.clone(),
                        bands: vec![],
                        statistics: vec![],
                    },
                    Modality::PointCloud => DatasetQuery::PointCloudRegion {
                        geometry: geometry.clone(),
                        crs: crs.clone(),
                        source_indices: vec![],
                        classifications: vec![],
                        elevation_min: None,
                        elevation_max: None,
                        resolution: None,
                        level: None,
                        point_limit,
                    },
                    _ => unreachable!(),
                };
                Ok((layer.dataset_id.0.clone(), query))
            }
            SemanticSelection::RasterRegion {
                world_geometry,
                band_indices,
                ..
            } => {
                let layer = select_layer(view, requested_dataset_id, |layer| {
                    layer.modality == Modality::Raster
                })?;
                let geometry = world_geometry.clone().ok_or_else(|| {
                    WildDatumError::Invalid(
                        "raster selection has no source/world geometry; record the browser's semantic source-coordinate mapping before querying"
                            .into(),
                    )
                })?;
                let crs = layer
                    .encoding
                    .get("crs")
                    .and_then(Value::as_str)
                    .unwrap_or("source")
                    .to_owned();
                Ok((
                    layer.dataset_id.0.clone(),
                    DatasetQuery::RasterRegion {
                        geometry,
                        crs,
                        bands: band_indices.clone(),
                        statistics: vec![],
                    },
                ))
            }
            SemanticSelection::SpectralRange {
                wavelength_start_nm,
                wavelength_end_nm,
                ..
            } => {
                let layer = select_layer(view, requested_dataset_id, |layer| {
                    matches!(layer.modality, Modality::Hyperspectral | Modality::Tensor)
                })?;
                let x = record.summary.get("x").and_then(Value::as_u64).ok_or_else(|| {
                    WildDatumError::Invalid("spectral selection summary requires source pixel x".into())
                })?;
                let y = record.summary.get("y").and_then(Value::as_u64).ok_or_else(|| {
                    WildDatumError::Invalid("spectral selection summary requires source pixel y".into())
                })?;
                let array_path = layer
                    .encoding
                    .get("cube_array")
                    .or_else(|| layer.encoding.get("hdf5_dataset"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| WildDatumError::Invalid("cube layer has no array mapping".into()))?;
                Ok((
                    layer.dataset_id.0.clone(),
                    DatasetQuery::Spectrum {
                        x,
                        y,
                        dataset_path: Some(array_path.into()),
                        wavelength_dataset: layer
                            .encoding
                            .get("wavelength_dataset")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        spectral_axis: layer
                            .encoding
                            .get("spectral_axis")
                            .and_then(Value::as_u64)
                            .unwrap_or(2) as u32,
                        wavelength_start_nm: Some(*wavelength_start_nm),
                        wavelength_end_nm: Some(*wavelength_end_nm),
                        scale_factor: layer.encoding.get("scale_factor").and_then(Value::as_f64),
                        add_offset: layer.encoding.get("add_offset").and_then(Value::as_f64),
                        no_data: layer.encoding.get("no_data").and_then(Value::as_f64),
                        bad_bands: layer
                            .encoding
                            .get("bad_bands")
                            .and_then(Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .filter_map(Value::as_u64)
                                    .map(|value| value as u32)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    },
                ))
            }
            SemanticSelection::CubePixel {
                dataset_id,
                array_path,
                x,
                y,
                x_axis,
                y_axis,
                spectral_axis,
                ..
            } => {
                ensure_dataset_in_view(view, &dataset_id.0)?;
                let manifest = self.get_manifest(&dataset_id.0)?;
                let descriptor = manifest
                    .cubes
                    .iter()
                    .find(|descriptor| descriptor.array_path == *array_path)
                    .ok_or_else(|| {
                        WildDatumError::NotFound(format!("cube array {array_path}"))
                    })?;
                let rank = descriptor.axes.len();
                let (x_axis, y_axis, spectral_axis) =
                    (*x_axis as usize, *y_axis as usize, *spectral_axis as usize);
                if rank != 3
                    || x_axis >= rank
                    || y_axis >= rank
                    || spectral_axis >= rank
                    || x_axis == y_axis
                    || x_axis == spectral_axis
                    || y_axis == spectral_axis
                    || *x >= descriptor.axes[x_axis].length
                    || *y >= descriptor.axes[y_axis].length
                {
                    return Err(WildDatumError::Invalid(
                        "cube pixel selection is incompatible with the mapped rank-3 array"
                            .into(),
                    ));
                }
                let ranges = descriptor
                    .axes
                    .iter()
                    .enumerate()
                    .map(|(axis, descriptor)| {
                        let (start, end) = if axis == x_axis {
                            (*x, *x + 1)
                        } else if axis == y_axis {
                            (*y, *y + 1)
                        } else {
                            (0, descriptor.length)
                        };
                        wilddatum_core::CubeRange {
                            start,
                            end,
                            step: 1,
                        }
                    })
                    .collect();
                Ok((
                    dataset_id.0.clone(),
                    DatasetQuery::CubeSlice {
                        array_path: array_path.clone(),
                        ranges,
                        cell_limit: 100_000,
                    },
                ))
            }
            SemanticSelection::PointSet {
                dataset_id,
                spatial_query,
                ..
            } => {
                ensure_dataset_in_view(view, &dataset_id.0)?;
                let mut geometry_value = spatial_query
                    .get("geometry")
                    .and_then(|geometry| geometry.get("geojson").or(Some(geometry)))
                    .cloned()
                    .or_else(|| spatial_query.get("geojson").cloned())
                    .unwrap_or_else(|| spatial_query.clone());
                let mut elevation_min =
                    spatial_query.get("elevation_min").and_then(Value::as_f64);
                let mut elevation_max =
                    spatial_query.get("elevation_max").and_then(Value::as_f64);
                let source_indices =
                    verified_point_source_indices(view, &dataset_id.0, spatial_query);
                if source_indices.is_empty()
                    && geometry_value.get("type").and_then(Value::as_str) == Some("Point")
                    && let Some(coordinates) = geometry_value
                        .get("coordinates")
                        .and_then(Value::as_array)
                    && coordinates.len() >= 2
                    && let (Some(x), Some(y)) =
                        (coordinates[0].as_f64(), coordinates[1].as_f64())
                {
                    let manifest = self.get_manifest(&dataset_id.0)?;
                    let tolerance = point_selection_tolerance(&manifest);
                    let z = coordinates.get(2).and_then(Value::as_f64);
                    geometry_value = json!({
                        "type": "Polygon",
                        "coordinates": [[
                            [x - tolerance[0], y - tolerance[1]],
                            [x + tolerance[0], y - tolerance[1]],
                            [x + tolerance[0], y + tolerance[1]],
                            [x - tolerance[0], y + tolerance[1]],
                            [x - tolerance[0], y - tolerance[1]]
                        ]]
                    });
                    if let Some(z) = z {
                        elevation_min.get_or_insert(z - tolerance[2]);
                        elevation_max.get_or_insert(z + tolerance[2]);
                    }
                }
                Ok((
                    dataset_id.0.clone(),
                    DatasetQuery::PointCloudRegion {
                        geometry: GeoGeometry {
                            geojson: geometry_value,
                        },
                        crs: spatial_query
                            .get("crs")
                            .and_then(Value::as_str)
                            .unwrap_or("source")
                            .into(),
                        source_indices,
                        classifications: spatial_query
                            .get("classifications")
                            .and_then(Value::as_array)
                            .map(|values| {
                                values
                                    .iter()
                                    .filter_map(Value::as_u64)
                                    .map(|value| value as u8)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        elevation_min,
                        elevation_max,
                        resolution: spatial_query.get("resolution").and_then(Value::as_f64),
                        level: spatial_query
                            .get("level")
                            .and_then(Value::as_i64)
                            .map(|value| value as i32),
                        point_limit,
                    },
                ))
            }
            SemanticSelection::Entities { .. } => Err(WildDatumError::Invalid(
                "entity selections identify rendered instances but do not contain a scientific source predicate; record rows, a map/raster region, spectral range, or point set"
                    .into(),
            )),
        }
    }
}

fn selection_link_source(selection: &SemanticSelection) -> (&'static str, Option<&str>) {
    match selection {
        SemanticSelection::CubePixel { dataset_id, .. } => {
            ("cube_pixel", Some(dataset_id.0.as_str()))
        }
        SemanticSelection::PointSet { dataset_id, .. } => {
            ("world_point", Some(dataset_id.0.as_str()))
        }
        SemanticSelection::Rows { dataset_id, .. } => ("rows", Some(dataset_id.0.as_str())),
        SemanticSelection::TimeInterval { .. } => ("time_interval", None),
        SemanticSelection::MapRegion { .. } => ("map_region", None),
        SemanticSelection::RasterRegion { .. } => ("raster_region", None),
        SemanticSelection::SpectralRange { .. } => ("spectral_range", None),
        SemanticSelection::Entities { .. } => ("entities", None),
    }
}

fn resolved_result_link(rule: &ViewLinkRule, result: ResultRecord) -> ResolvedViewLink {
    ResolvedViewLink {
        source_panel: rule.source_panel.clone(),
        target_panel: rule.target_panel.clone(),
        resolver: rule.resolver.clone(),
        exactness: rule.exactness.clone(),
        status: LinkResolutionStatus::Resolved,
        explanation: rule.explanation.clone(),
        derived_selection: None,
        result: Some(LinkedResultSummary {
            result_id: result.result_id,
            dataset_id: result.dataset_id,
            row_count: result.row_count,
            preview: result.preview,
        }),
    }
}

fn link_source_matches(
    view: &EcoViewSpec,
    rule: &ViewLinkRule,
    source_dataset: Option<&str>,
) -> Result<bool> {
    let panel = view
        .panels
        .iter()
        .find(|panel| panel.id == rule.source_panel)
        .ok_or_else(|| WildDatumError::NotFound(format!("panel {}", rule.source_panel)))?;
    let layer = view
        .layers
        .iter()
        .find(|layer| layer.id == panel.layer_id)
        .ok_or_else(|| WildDatumError::NotFound(format!("layer {}", panel.layer_id)))?;
    Ok(source_dataset.is_none_or(|dataset_id| layer.dataset_id.0 == dataset_id))
}

fn verified_source_row_query(
    view: &EcoViewSpec,
    dataset_id: &str,
    predicate: &Value,
    row_count: u64,
) -> Result<Option<DatasetQuery>> {
    let Some(object) = predicate.as_object() else {
        return Ok(None);
    };
    for forbidden in ["source_index", "source_indices", "source_index_verified"] {
        if object.contains_key(forbidden) {
            return Err(WildDatumError::Invalid(format!(
                "viewer row predicate must not supply trusted field {forbidden}"
            )));
        }
    }
    let mapping_fields = [
        "entity_path",
        "instance_id",
        "mapping_kind",
        "rerun_version",
    ];
    if !mapping_fields
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return Ok(None);
    }
    let unknown = object
        .keys()
        .filter(|key| !mapping_fields.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(WildDatumError::Invalid(format!(
            "viewer row predicate contains untrusted fields: {unknown:?}"
        )));
    }
    if row_count != 1 {
        return Err(WildDatumError::Invalid(
            "a Rerun instance selection must have row_count=1".into(),
        ));
    }
    let entity_path = object
        .get("entity_path")
        .and_then(Value::as_str)
        .ok_or_else(|| WildDatumError::Invalid("viewer row predicate needs entity_path".into()))?
        .trim_start_matches('/');
    let instance_id = object
        .get("instance_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            WildDatumError::Invalid("viewer row predicate needs an integer instance_id".into())
        })?;
    let supplied_kind = object
        .get("mapping_kind")
        .and_then(Value::as_str)
        .ok_or_else(|| WildDatumError::Invalid("viewer row predicate needs mapping_kind".into()))?;
    let supplied_version = object
        .get("rerun_version")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            WildDatumError::Invalid("viewer row predicate needs rerun_version".into())
        })?;

    let (layer, entity_suffix) = view
        .layers
        .iter()
        .find_map(|layer| {
            let prefix = format!(
                "datasets/{}/{}/",
                layer.dataset_id,
                safe_entity_component(&layer.id)
            );
            entity_path
                .strip_prefix(&prefix)
                .map(|suffix| (layer, suffix))
        })
        .ok_or_else(|| {
            WildDatumError::Invalid(format!(
                "viewer entity {entity_path} does not belong to a view layer"
            ))
        })?;
    if layer.dataset_id.0 != dataset_id {
        return Err(WildDatumError::Invalid(format!(
            "viewer entity belongs to dataset {}, not {dataset_id}",
            layer.dataset_id
        )));
    }
    if !matches!(
        layer.modality,
        Modality::Tabular | Modality::TimeSeries | Modality::Vector
    ) || layer.encoding.get("view_kind").and_then(Value::as_str)
        != Some(PROFILE_TRAJECTORY_VIEW_KIND)
    {
        return Err(WildDatumError::Invalid(
            "viewer row mapping requires a validated profile_trajectory_v1 layer".into(),
        ));
    }
    let mapping = layer
        .encoding
        .get("selection_mapping")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            WildDatumError::Invalid("profile/trajectory layer has no selection mapping".into())
        })?;
    let mapping_kind = mapping.get("kind").and_then(Value::as_str);
    if mapping_kind != Some("source_row_index") || Some(supplied_kind) != mapping_kind {
        return Err(WildDatumError::Invalid(
            "viewer row mapping kind does not match the service mapping".into(),
        ));
    }
    let mapping_version = mapping.get("rerun_version").and_then(Value::as_str);
    if mapping_version != Some(PINNED_RERUN_VERSION) || Some(supplied_version) != mapping_version {
        return Err(WildDatumError::Invalid(format!(
            "viewer row mapping requires Rerun {PINNED_RERUN_VERSION}"
        )));
    }
    let allowed_suffix = mapping
        .get("entity_suffixes")
        .and_then(Value::as_array)
        .is_some_and(|suffixes| suffixes.iter().any(|suffix| suffix == entity_suffix));
    if !allowed_suffix || entity_suffix.contains('/') {
        return Err(WildDatumError::Invalid(format!(
            "entity suffix {entity_suffix} is not an observation mapping"
        )));
    }
    let stride = mapping
        .get("stride")
        .and_then(Value::as_u64)
        .filter(|stride| *stride > 0)
        .ok_or_else(|| WildDatumError::Invalid("source row mapping has invalid stride".into()))?;
    let source_index = instance_id
        .checked_mul(stride)
        .ok_or_else(|| WildDatumError::Invalid("viewer instance source index overflowed".into()))?;
    Ok(Some(DatasetQuery::SourceRows {
        source_indices: vec![source_index],
        select: vec![],
    }))
}

fn safe_entity_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn verified_point_source_indices(
    view: &EcoViewSpec,
    dataset_id: &str,
    spatial_query: &Value,
) -> Vec<u64> {
    if spatial_query
        .get("source_index_verified")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return vec![];
    }
    let Some(instance_id) = spatial_query.get("instance_id").and_then(Value::as_u64) else {
        return vec![];
    };
    let Some(layer) = view
        .layers
        .iter()
        .find(|layer| layer.dataset_id.0 == dataset_id && layer.modality == Modality::PointCloud)
    else {
        return vec![];
    };
    let Some(mapping) = layer.encoding.get("instance_id_mapping") else {
        return vec![];
    };
    if mapping.get("kind").and_then(Value::as_str) != Some("source_stream_stride")
        || mapping.get("rerun_version").and_then(Value::as_str) != Some(PINNED_RERUN_VERSION)
    {
        return vec![];
    }
    let Some(stride) = mapping.get("stride").and_then(Value::as_u64) else {
        return vec![];
    };
    let Some(expected) = instance_id.checked_mul(stride.max(1)) else {
        return vec![];
    };
    let supplied = spatial_query
        .get("source_indices")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_u64).collect::<Vec<_>>())
        .unwrap_or_default();
    if supplied == [expected] {
        supplied
    } else {
        vec![]
    }
}

fn point_selection_tolerance(manifest: &DatasetManifest) -> [f64; 3] {
    let source = manifest.source_files.first();
    let scales = source
        .and_then(|source| source.metadata.get("coordinate_scale"))
        .and_then(Value::as_array);
    let bounds = source
        .and_then(|source| source.metadata.get("bounds"))
        .and_then(Value::as_object);
    let projected_like = bounds.is_some_and(|bounds| {
        (0..2).any(|axis| {
            ["min", "max"].into_iter().any(|bound| {
                bounds
                    .get(bound)
                    .and_then(|values| values.get(axis))
                    .and_then(Value::as_f64)
                    .is_some_and(|value| value.abs() > 1_000.0)
            })
        })
    });
    std::array::from_fn(|axis| {
        let scale = scales
            .and_then(|values| values.get(axis))
            .and_then(Value::as_f64)
            .unwrap_or_default()
            .abs();
        let extent = bounds
            .and_then(|bounds| {
                Some(
                    bounds.get("max")?.get(axis)?.as_f64()?
                        - bounds.get("min")?.get(axis)?.as_f64()?,
                )
            })
            .unwrap_or_default()
            .abs();
        (scale * 2.0)
            .max(extent * f64::from(f32::EPSILON) * 8.0)
            .max(if projected_like { 0.01 } else { 1e-7 })
    })
}

fn ensure_dataset_in_view(view: &EcoViewSpec, dataset_id: &str) -> Result<()> {
    if view
        .dataset_ids
        .iter()
        .any(|candidate| candidate.0 == dataset_id)
    {
        Ok(())
    } else {
        Err(WildDatumError::Invalid(format!(
            "dataset {dataset_id} is not part of view {}",
            view.view_id
        )))
    }
}

fn select_layer<'a>(
    view: &'a EcoViewSpec,
    requested_dataset_id: Option<&str>,
    predicate: impl Fn(&EcoLayer) -> bool,
) -> Result<&'a EcoLayer> {
    if let Some(dataset_id) = requested_dataset_id {
        return view
            .layers
            .iter()
            .find(|layer| layer.dataset_id.0 == dataset_id && predicate(layer))
            .ok_or_else(|| {
                WildDatumError::Invalid(format!(
                    "dataset {dataset_id} is not a compatible visible layer in view {}",
                    view.view_id
                ))
            });
    }
    let matches = view
        .layers
        .iter()
        .filter(|layer| predicate(layer))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [layer] => Ok(*layer),
        [] => Err(WildDatumError::Invalid(
            "the selection has no compatible dataset layer".into(),
        )),
        _ => Err(WildDatumError::Invalid(
            "the selection matches multiple dataset layers; supply dataset_id explicitly".into(),
        )),
    }
}

fn parse_selection_filters(predicate: &Value) -> Result<Vec<QueryFilter>> {
    let value = predicate.get("filters").unwrap_or(predicate);
    serde_json::from_value(value.clone()).map_err(|error| {
        WildDatumError::Invalid(format!(
            "row selection predicate must be an array of query filters: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::{fs::File, io::Write};

    use serde_json::json;
    use wilddatum_core::{
        DatasetId, GeoGeometry, ProfileTrajectoryRecipeV1, ProfileValueSpec, SemanticSelection,
        VerticalAxisSpec, VerticalDirection,
    };

    use super::*;
    use crate::ServicePaths;

    async fn profile_view(
        directory: &tempfile::TempDir,
        service: &WildDatumService,
    ) -> (DatasetManifest, EcoViewSpec) {
        let path = directory.path().join("profile.csv");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "platform,cycle,time,latitude,longitude,pressure,pres_qc,temperature,temp_qc"
        )
        .unwrap();
        for index in 0..8 {
            writeln!(
                file,
                "FLOAT_1,1,t{index},{},{},{},1,{},{}",
                34.8 + index as f64 / 100.0,
                -75.5 + index as f64 / 100.0,
                index * 10,
                18.0 - index as f64 / 10.0,
                if index == 7 { 4 } else { 1 }
            )
            .unwrap();
        }
        let mut manifest = service.import_local_file(&path).await.unwrap();
        manifest.modalities = vec![Modality::Vector, Modality::Tabular];
        service.save_manifest(&manifest).unwrap();
        let view = service
            .create_view("Profile".into(), vec![manifest.dataset_id.clone()])
            .unwrap();
        let view = service
            .configure_profile_trajectory_view(
                &view.view_id.0,
                1,
                "layer_1",
                ProfileTrajectoryRecipeV1 {
                    trajectory_id_field: "platform".into(),
                    profile_id_field: "cycle".into(),
                    time_field: Some("time".into()),
                    latitude_field: "latitude".into(),
                    longitude_field: "longitude".into(),
                    vertical: VerticalAxisSpec {
                        field: "pressure".into(),
                        direction: VerticalDirection::PositiveDown,
                        unit: Some("decibar".into()),
                        fill_values: vec![],
                    },
                    value: ProfileValueSpec {
                        field: "temperature".into(),
                        unit: Some("degree_Celsius".into()),
                        qc_field: Some("temp_qc".into()),
                        accepted_qc: vec!["1".into(), "2".into()],
                        fill_values: vec![],
                    },
                    additional_values: vec![],
                    vertical_range: None,
                    max_points_per_profile: None,
                },
            )
            .unwrap();
        (manifest, view)
    }

    fn viewer_row_selection(
        service: &WildDatumService,
        view: &EcoViewSpec,
        dataset_id: DatasetId,
        predicate: Value,
    ) -> SelectionRecord {
        service
            .save_selection(
                &view.view_id.0,
                SemanticSelection::Rows {
                    dataset_id,
                    predicate,
                    row_count: 1,
                },
                json!({"source": "rerun_web_viewer"}),
            )
            .unwrap()
    }

    fn valid_viewer_predicate(dataset_id: &DatasetId) -> Value {
        json!({
            "entity_path": format!("datasets/{dataset_id}/layer_1/map_observations"),
            "instance_id": 7,
            "mapping_kind": "source_row_index",
            "rerun_version": PINNED_RERUN_VERSION
        })
    }

    #[tokio::test]
    async fn regenerates_a_vector_query_from_exact_map_selection_state() {
        let directory = tempfile::tempdir().unwrap();
        let service = WildDatumService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        let path = directory.path().join("plots.geojson");
        serde_json::to_writer(
            File::create(&path).unwrap(),
            &json!({
                "type": "FeatureCollection",
                "features": [
                    {"type": "Feature", "properties": {"plot": "inside"}, "geometry": {"type": "Point", "coordinates": [1.0, 1.0]}},
                    {"type": "Feature", "properties": {"plot": "outside"}, "geometry": {"type": "Point", "coordinates": [10.0, 10.0]}}
                ]
            }),
        )
        .unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        let view = service
            .create_view(
                "Plots".into(),
                vec![DatasetId(manifest.dataset_id.0.clone())],
            )
            .unwrap();
        let selection = service
            .save_selection(
                &view.view_id.0,
                SemanticSelection::MapRegion {
                    geometry: GeoGeometry {
                        geojson: json!({
                            "type": "Polygon",
                            "coordinates": [[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0], [0.0, 0.0]]]
                        }),
                    },
                    crs: "source".into(),
                },
                json!({"gesture": "box_select"}),
            )
            .unwrap();
        let result = service
            .query_selection(&selection.selection_id.0, None, 100_000)
            .await
            .unwrap();
        assert_eq!(result.row_count, Some(1));
        assert_eq!(result.preview["rows"][0]["properties"]["plot"], "inside");
        assert_eq!(result.source_selection, Some(selection.selection_id));
        assert_eq!(result.transformations[0].name, "semantic_selection_query");
    }

    #[tokio::test]
    async fn regenerates_an_exact_rerun_point_instance_from_its_source_index() {
        let directory = tempfile::tempdir().unwrap();
        let service = WildDatumService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        let path = directory.path().join("points.las");
        let mut writer = las::Writer::from_path(&path, las::Header::default()).unwrap();
        for index in 0..10 {
            writer
                .write_point(las::Point {
                    x: index as f64,
                    y: index as f64,
                    z: index as f64 * 2.0,
                    ..Default::default()
                })
                .unwrap();
        }
        writer.close().unwrap();
        let manifest = service.import_local_file(&path).await.unwrap();
        let view = service
            .create_view(
                "Point selection".into(),
                vec![DatasetId(manifest.dataset_id.0.clone())],
            )
            .unwrap();
        assert_eq!(
            view.layers[0].encoding["instance_id_mapping"]["kind"],
            "source_stream_stride"
        );
        assert_eq!(view.layers[0].encoding["instance_id_mapping"]["stride"], 1);
        let selection = service
            .save_selection(
                &view.view_id.0,
                SemanticSelection::PointSet {
                    dataset_id: manifest.dataset_id.clone(),
                    spatial_query: json!({
                        "geometry": {"type": "Point", "coordinates": [6.9999, 6.9999, 13.9999]},
                        "instance_id": 7,
                        "source_indices": [7],
                        "source_index_verified": true
                    }),
                    estimated_points: 1,
                },
                json!({"source": "rerun_web_viewer"}),
            )
            .unwrap();
        let result = service
            .query_selection(&selection.selection_id.0, None, 10)
            .await
            .unwrap();
        assert_eq!(result.row_count, Some(1));
        assert_eq!(result.preview["rows"][0]["source_index"], 7);
        assert_eq!(result.preview["rows"][0]["z"], 14.0);
        assert_eq!(result.preview["selection_mode"], "source_indices");
        assert_eq!(result.source_selection, Some(selection.selection_id));
    }

    #[tokio::test]
    async fn regenerates_an_exact_profile_row_from_untrusted_rerun_instance_state() {
        let directory = tempfile::tempdir().unwrap();
        let service = WildDatumService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        let (manifest, view) = profile_view(&directory, &service).await;
        let selection = viewer_row_selection(
            &service,
            &view,
            manifest.dataset_id.clone(),
            valid_viewer_predicate(&manifest.dataset_id),
        );

        let result = service
            .query_selection(&selection.selection_id.0, None, 100_000)
            .await
            .unwrap();

        assert_eq!(result.row_count, Some(1));
        assert_eq!(result.preview["rows"][0]["source_index"], 7);
        assert_eq!(result.preview["rows"][0]["values"]["pressure"], "70");
        assert_eq!(result.preview["rows"][0]["values"]["temp_qc"], "4");
        assert_eq!(result.source_selection, Some(selection.selection_id));
    }

    #[tokio::test]
    async fn rejects_forged_or_incompatible_profile_instance_mappings() {
        let directory = tempfile::tempdir().unwrap();
        let service = WildDatumService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        let (manifest, view) = profile_view(&directory, &service).await;
        let base = valid_viewer_predicate(&manifest.dataset_id);

        let cases = [
            (
                "dataset absent",
                DatasetId("ds_absent".into()),
                base.clone(),
                "not part of view",
            ),
            (
                "another layer",
                manifest.dataset_id.clone(),
                json!({
                    "entity_path": format!("datasets/{}/layer_2/map_observations", manifest.dataset_id),
                    "instance_id": 7,
                    "mapping_kind": "source_row_index",
                    "rerun_version": PINNED_RERUN_VERSION
                }),
                "does not belong to a view layer",
            ),
            (
                "line entity",
                manifest.dataset_id.clone(),
                json!({
                    "entity_path": format!("datasets/{}/layer_1/profile_lines", manifest.dataset_id),
                    "instance_id": 7,
                    "mapping_kind": "source_row_index",
                    "rerun_version": PINNED_RERUN_VERSION
                }),
                "not an observation mapping",
            ),
            (
                "unknown observation suffix",
                manifest.dataset_id.clone(),
                json!({
                    "entity_path": format!("datasets/{}/layer_1/other_observations", manifest.dataset_id),
                    "instance_id": 7,
                    "mapping_kind": "source_row_index",
                    "rerun_version": PINNED_RERUN_VERSION
                }),
                "not an observation mapping",
            ),
            (
                "non-integer instance",
                manifest.dataset_id.clone(),
                json!({
                    "entity_path": format!("datasets/{}/layer_1/map_observations", manifest.dataset_id),
                    "instance_id": 7.5,
                    "mapping_kind": "source_row_index",
                    "rerun_version": PINNED_RERUN_VERSION
                }),
                "integer instance_id",
            ),
            (
                "wrong Rerun",
                manifest.dataset_id.clone(),
                json!({
                    "entity_path": format!("datasets/{}/layer_1/map_observations", manifest.dataset_id),
                    "instance_id": 7,
                    "mapping_kind": "source_row_index",
                    "rerun_version": "0.0.0"
                }),
                "requires Rerun",
            ),
            (
                "forged source index",
                manifest.dataset_id.clone(),
                json!({
                    "entity_path": format!("datasets/{}/layer_1/map_observations", manifest.dataset_id),
                    "instance_id": 7,
                    "mapping_kind": "source_row_index",
                    "rerun_version": PINNED_RERUN_VERSION,
                    "source_indices": [0],
                    "source_index_verified": true
                }),
                "must not supply trusted field",
            ),
        ];
        for (name, dataset_id, predicate, expected) in cases {
            let selection = viewer_row_selection(&service, &view, dataset_id, predicate);
            let error = service
                .query_selection(&selection.selection_id.0, None, 100_000)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{name}: {error}");
        }

        let mut without_recipe = view.clone();
        without_recipe.layers[0].encoding.remove("view_kind");
        let selection =
            viewer_row_selection(&service, &view, manifest.dataset_id.clone(), base.clone());
        let error = service
            .selection_to_query(&without_recipe, &selection, None, 100_000)
            .unwrap_err()
            .to_string();
        assert!(error.contains("validated profile_trajectory_v1"));

        let mut outside = base;
        outside["instance_id"] = json!(999);
        let selection = viewer_row_selection(&service, &view, manifest.dataset_id, outside);
        let error = service
            .query_selection(&selection.selection_id.0, None, 100_000)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside the source"));
    }

    #[tokio::test]
    async fn resolves_an_accepted_cube_pixel_link_into_an_exact_spectrum_result() {
        use hdf5_metno::types::FixedAscii;

        let directory = tempfile::tempdir().unwrap();
        let service = WildDatumService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        let path = directory.path().join("reflectance.h5");
        let file = hdf5_metno::File::create(&path).unwrap();
        let group = file.create_group("HARV/Reflectance").unwrap();
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
        let coordinate_system = group.create_group("Metadata/Coordinate_System").unwrap();
        coordinate_system
            .new_dataset::<FixedAscii<6>>()
            .shape([1])
            .create("EPSG Code")
            .unwrap()
            .write_raw(&[FixedAscii::<6>::from_ascii(b"32611").unwrap()])
            .unwrap();
        coordinate_system
            .new_dataset::<FixedAscii<128>>()
            .shape([1])
            .create("Map_Info")
            .unwrap()
            .write_raw(&[FixedAscii::<128>::from_ascii(
                b"UTM, 1.000, 1.000, 0.000, 4.000, 1.000, 1.000, 11, North, WGS-84, units=Meters, 0",
            )
            .unwrap()])
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
        let source_panel = view
            .panels
            .iter()
            .find(|panel| panel.representation == "rgb")
            .unwrap();
        let array_path = source_panel.encoding["cube_array"].as_str().unwrap();
        let selection = service
            .save_selection(
                &view.view_id.0,
                SemanticSelection::CubePixel {
                    dataset_id: manifest.dataset_id.clone(),
                    array_path: array_path.into(),
                    x: 1,
                    y: 1,
                    x_axis: 1,
                    y_axis: 0,
                    spectral_axis: 2,
                    displayed_bands: vec![2, 1, 0],
                },
                json!({"source": "test"}),
            )
            .unwrap();

        let resolution = service
            .resolve_selection_links(&selection.selection_id.0)
            .await
            .unwrap();
        assert_eq!(resolution.version, LINK_RESOLUTION_VERSION);
        assert_eq!(resolution.links.len(), 1);
        assert_eq!(resolution.links[0].status, LinkResolutionStatus::Resolved);
        assert_eq!(resolution.links[0].exactness, LinkExactness::Exact);
        let linked = resolution.links[0].result.as_ref().unwrap();
        assert_eq!(linked.row_count, Some(4));
        assert_eq!(linked.preview["rows"][0]["wavelength_nm"], 450.0);
        assert_eq!(linked.preview["rows"][2]["value"], 18.0);
        let persisted = service.get_result(&linked.result_id.0).unwrap();
        assert_eq!(
            persisted.source_selection.as_ref(),
            Some(&selection.selection_id)
        );
        assert!(persisted.transformations.iter().any(|transformation| {
            transformation.name == "view_link_resolution"
                && transformation.parameters["resolver"] == "cube_pixel_to_spectrum"
        }));

        let point_path = directory.path().join("points.las");
        let mut point_header = las::Builder::default();
        let geokeys = [1_u16, 1, 0, 1, 3072, 0, 1, 32611];
        point_header.vlrs.push(las::Vlr {
            user_id: "LASF_Projection".into(),
            record_id: 34735,
            description: "GeoKeyDirectoryTag".into(),
            data: geokeys.into_iter().flat_map(u16::to_le_bytes).collect(),
        });
        let mut point_writer =
            las::Writer::from_path(&point_path, point_header.into_header().unwrap()).unwrap();
        point_writer
            .write_point(las::Point {
                x: 1.2,
                y: 2.2,
                z: 3.0,
                ..Default::default()
            })
            .unwrap();
        point_writer.close().unwrap();
        let point_manifest = service.import_local_file(&point_path).await.unwrap();
        let combined_suggestions = service
            .suggest_views(&[
                point_manifest.dataset_id.0.clone(),
                manifest.dataset_id.0.clone(),
            ])
            .unwrap();
        let combined_suggestion = combined_suggestions
            .suggestions
            .iter()
            .find(|suggestion| suggestion.recipe == "point_cloud_spectral_cube_v1")
            .unwrap();
        assert!(combined_suggestion.links.iter().any(|link| {
            link.resolver == "world_to_raster_pixel" && link.exactness == LinkExactness::Exact
        }));
        let combined_view = service
            .create_view_from_suggestion(
                &combined_suggestion.suggestion_id,
                &[
                    point_manifest.dataset_id.0.clone(),
                    manifest.dataset_id.0.clone(),
                ],
                None,
            )
            .unwrap();
        let point_selection = service
            .save_selection(
                &combined_view.view_id.0,
                SemanticSelection::PointSet {
                    dataset_id: point_manifest.dataset_id,
                    spatial_query: json!({
                        "geometry": {"type": "Point", "coordinates": [1.2, 2.2, 3.0]}
                    }),
                    estimated_points: 1,
                },
                json!({"source": "test"}),
            )
            .unwrap();
        let chained = service
            .resolve_selection_links(&point_selection.selection_id.0)
            .await
            .unwrap();
        assert_eq!(chained.links.len(), 2);
        assert_eq!(chained.links[0].resolver, "world_to_raster_pixel");
        assert_eq!(chained.links[0].status, LinkResolutionStatus::Resolved);
        assert!(matches!(
            chained.links[0].derived_selection,
            Some(SemanticSelection::CubePixel { x: 1, y: 1, .. })
        ));
        assert_eq!(chained.links[1].resolver, "cube_pixel_to_spectrum");
        assert_eq!(chained.links[1].status, LinkResolutionStatus::Resolved);
        assert_eq!(chained.links[1].result.as_ref().unwrap().row_count, Some(4));

        service
            .patch_view(
                &view.view_id.0,
                view.revision,
                json!({"name": "new revision"}),
            )
            .unwrap();
        assert!(matches!(
            service
                .resolve_selection_links(&selection.selection_id.0)
                .await,
            Err(WildDatumError::Conflict(_))
        ));
    }
}
