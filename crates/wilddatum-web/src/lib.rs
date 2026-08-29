//! Loopback-only browser surface for the Rerun Web Viewer.

use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;
use wilddatum_core::{SemanticSelection, ViewId};
use wilddatum_service::WildDatumService;

#[derive(Clone)]
struct AppState {
    service: WildDatumService,
    view_id: ViewId,
    token: String,
    recording: Arc<RwLock<PathBuf>>,
}

#[derive(Debug, Deserialize)]
struct TokenQuery {
    token: String,
}

#[derive(Debug, Deserialize)]
struct SelectionEvent {
    selection: SemanticSelection,
    #[serde(default)]
    summary: Value,
}

#[derive(Debug, Deserialize)]
struct SelectionLinkEvent {
    selection_id: String,
}

pub struct ServeOptions {
    pub view_id: String,
    pub port: u16,
    pub open_browser: bool,
}

pub async fn serve(service: WildDatumService, options: ServeOptions) -> Result<()> {
    let view = service
        .get_view(&options.view_id)
        .context("loading the requested WildDatum view")?;
    let recording = service
        .paths()
        .views_dir
        .join(format!("{}.rrd", view.view_id));
    wilddatum_rerun::write_recording(&service, &view.view_id.0, &recording)
        .context("rendering the Rerun recording")?;
    let state = Arc::new(AppState {
        service,
        view_id: view.view_id,
        token: Uuid::new_v4().simple().to_string(),
        recording: Arc::new(RwLock::new(recording)),
    });

    let web_dist = find_web_dist();
    let fallback = ServeFile::new(web_dist.join("index.html"));
    let static_files = ServeDir::new(web_dist).not_found_service(fallback);
    let app = Router::new()
        .route("/api/view", get(get_view))
        .route("/api/selection", get(get_selection).post(post_selection))
        .route(
            "/api/selection-links",
            axum::routing::post(post_selection_links),
        )
        .route("/api/recording.rrd", get(get_recording))
        .fallback_service(static_files)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", options.port))
        .await
        .context("binding the loopback explorer server")?;
    let address = listener.local_addr()?;
    let url = format!("http://{address}/?token={}", state.token);
    println!("WildDatum explorer: {url}");
    if options.open_browser {
        webbrowser::open(&url).context("opening the browser")?;
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving the WildDatum explorer")
}

async fn get_view(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize(&state, &query, &headers, false) {
        return response.into_response();
    }
    match state.service.get_view(&state.view_id.0) {
        Ok(view) => Json(view).into_response(),
        Err(error) => error_response(error.to_string()),
    }
}

async fn get_selection(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize(&state, &query, &headers, false) {
        return response.into_response();
    }
    match state.service.latest_selection(&state.view_id.0) {
        Ok(selection) => Json(selection).into_response(),
        Err(wilddatum_core::WildDatumError::NotFound(_)) => {
            Json(json!({"selection": null})).into_response()
        }
        Err(error) => error_response(error.to_string()),
    }
}

async fn post_selection(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
    Json(event): Json<SelectionEvent>,
) -> Response {
    if let Err(response) = authorize(&state, &query, &headers, true) {
        return response.into_response();
    }
    match state
        .service
        .save_selection(&state.view_id.0, event.selection, event.summary)
    {
        Ok(selection) => (StatusCode::CREATED, Json(selection)).into_response(),
        Err(error) => error_response(error.to_string()),
    }
}

async fn post_selection_links(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
    Json(event): Json<SelectionLinkEvent>,
) -> Response {
    if let Err(response) = authorize(&state, &query, &headers, true) {
        return response.into_response();
    }
    match state.service.get_selection(&event.selection_id) {
        Ok(selection) if selection.view_id == state.view_id => {}
        Ok(_) | Err(wilddatum_core::WildDatumError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                "selection is not part of this explorer view",
            )
                .into_response();
        }
        Err(error) => return error_response(error.to_string()),
    }
    match state
        .service
        .resolve_selection_links(&event.selection_id)
        .await
    {
        Ok(resolution) => {
            let target = state
                .service
                .paths()
                .views_dir
                .join(format!("{}.linked.rrd", state.view_id));
            let temporary = state.service.paths().views_dir.join(format!(
                "{}.linked-{}.tmp.rrd",
                state.view_id,
                Uuid::new_v4().simple()
            ));
            let service = state.service.clone();
            let view_id = state.view_id.0.clone();
            let rendered = resolution.clone();
            let render_target = target.clone();
            let render_result = tokio::task::spawn_blocking(move || {
                wilddatum_rerun::write_recording_with_link_resolution(
                    &service,
                    &view_id,
                    &temporary,
                    Some(&rendered),
                )?;
                std::fs::rename(&temporary, &render_target)?;
                Ok::<_, wilddatum_core::WildDatumError>(())
            })
            .await;
            match render_result {
                Ok(Ok(())) => {
                    *state.recording.write().expect("recording lock poisoned") = target;
                    let mut response =
                        serde_json::to_value(&resolution).expect("link resolution is serializable");
                    response["recording_revision"] = json!(resolution.selection_id.0);
                    (StatusCode::CREATED, Json(response)).into_response()
                }
                Ok(Err(error)) => error_response(error.to_string()),
                Err(error) => error_response(format!("linked recording task failed: {error}")),
            }
        }
        Err(error) => error_response(error.to_string()),
    }
}

async fn get_recording(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize(&state, &query, &headers, false) {
        return response.into_response();
    }
    let recording = state
        .recording
        .read()
        .expect("recording lock poisoned")
        .clone();
    match tokio::fs::read(recording).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(bytes))
            .expect("static response"),
        Err(error) => error_response(error.to_string()),
    }
}

fn authorize(
    state: &AppState,
    query: &TokenQuery,
    headers: &HeaderMap,
    require_same_origin: bool,
) -> std::result::Result<(), (StatusCode, &'static str)> {
    if query.token != state.token {
        return Err((StatusCode::UNAUTHORIZED, "invalid explorer launch token"));
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !(host.starts_with("127.0.0.1:") || host.starts_with("localhost:")) {
        return Err((StatusCode::FORBIDDEN, "non-loopback Host rejected"));
    }
    if require_same_origin {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !(origin.starts_with("http://127.0.0.1:") || origin.starts_with("http://localhost:")) {
            return Err((StatusCode::FORBIDDEN, "cross-origin mutation rejected"));
        }
    }
    Ok(())
}

fn find_web_dist() -> PathBuf {
    if let Some(path) =
        std::env::var_os("WILDDATUM_WEB_DIST").or_else(|| std::env::var_os("ECOSCOPE_WEB_DIST"))
    {
        return PathBuf::from(path);
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(bin_dir) = executable.parent()
    {
        let installed = bin_dir.join("../share/wilddatum/web");
        if installed.join("index.html").is_file() {
            return installed;
        }
        let legacy = bin_dir.join("../share/ecoscope/web");
        if legacy.join("index.html").is_file() {
            return legacy;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../viewer/web-bootstrap/dist")
}

fn error_response(message: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": message})),
    )
        .into_response()
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use wilddatum_service::ServicePaths;

    use super::*;

    #[test]
    fn explorer_authorization_requires_token_loopback_and_same_origin() {
        let directory = tempfile::tempdir().unwrap();
        let service = WildDatumService::open(ServicePaths::under(
            directory.path().join("data"),
            directory.path().join("cache"),
        ))
        .unwrap();
        let state = AppState {
            service,
            view_id: ViewId::new(),
            token: "secret".into(),
            recording: Arc::new(RwLock::new(directory.path().join("view.rrd"))),
        };
        let query = TokenQuery {
            token: "secret".into(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4123"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:4123"),
        );
        assert!(authorize(&state, &query, &headers, true).is_ok());

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.test"),
        );
        let response = authorize(&state, &query, &headers, true).unwrap_err();
        assert_eq!(response.0, StatusCode::FORBIDDEN);

        let wrong_token = TokenQuery {
            token: "wrong".into(),
        };
        let response = authorize(&state, &wrong_token, &headers, false).unwrap_err();
        assert_eq!(response.0, StatusCode::UNAUTHORIZED);
    }
}
