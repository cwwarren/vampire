use crate::proxy::{MetadataRewrite, request_failed_response, request_origin};
use crate::routes::{pypi_file_url, pypi_simple_url};
use crate::state::App;
use axum::Router;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;

pub(crate) fn router() -> Router<App> {
    Router::new()
        .route(
            "/pypi/simple/",
            get(pypi_simple_root_get).head(pypi_simple_root_head),
        )
        .route(
            "/pypi/simple/{*project}",
            get(pypi_simple_project_get).head(pypi_simple_project_head),
        )
        .route(
            "/pypi/files/{*path}",
            get(pypi_file_get).head(pypi_file_head),
        )
}

async fn pypi_simple_root_get(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&headers);
    let Some(upstream) = pypi_simple_url(app.upstreams(), None) else {
        return crate::proxy::not_found().await;
    };
    app.handle_metadata(upstream, MetadataRewrite::Pypi(origin))
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn pypi_simple_root_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(upstream) = pypi_simple_url(app.upstreams(), None) else {
        return crate::proxy::not_found().await;
    };
    app.handle_metadata_head(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

async fn pypi_simple_project_get(
    State(app): State<App>,
    Path(project): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&headers);
    let Some(upstream) = pypi_simple_url(app.upstreams(), Some(&project)) else {
        return crate::proxy::not_found().await;
    };
    app.handle_metadata(upstream, MetadataRewrite::Pypi(origin))
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn pypi_simple_project_head(
    State(app): State<App>,
    Path(project): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = pypi_simple_url(app.upstreams(), Some(&project)) else {
        return crate::proxy::not_found().await;
    };
    app.handle_metadata_head(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

async fn pypi_file_get(
    State(app): State<App>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = pypi_file_url(&path, app.upstreams()) else {
        return crate::proxy::not_found().await;
    };
    app.handle_artifact(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn pypi_file_head(
    State(app): State<App>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = pypi_file_url(&path, app.upstreams()) else {
        return crate::proxy::not_found().await;
    };
    app.handle_artifact_head(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}
