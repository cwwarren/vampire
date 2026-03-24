use crate::proxy::{MetadataRewrite, request_failed_response};
use crate::routes::{cargo_config, cargo_download_url, cargo_index_url};
use crate::state::App;
use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderValue;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::Response;
use axum::routing::get;

pub(crate) fn router() -> Router<App> {
    Router::new()
        .route(
            "/cargo/index/config.json",
            get(cargo_config_get).head(cargo_config_head),
        )
        .route(
            "/cargo/index/{*path}",
            get(cargo_index_get).head(cargo_index_head),
        )
        .route(
            "/cargo/api/v1/crates/{crate_name}/{version}/download",
            get(cargo_download_get).head(cargo_download_head),
        )
}

async fn cargo_config_get(State(app): State<App>) -> Response {
    cargo_config_response(app.public_base_url(), false)
}

async fn cargo_config_head(State(app): State<App>) -> Response {
    cargo_config_response(app.public_base_url(), true)
}

fn cargo_config_response(origin: &str, head_only: bool) -> Response {
    let body = cargo_config(origin);
    let len = body.len();
    let mut response = Response::new(if head_only {
        Body::empty()
    } else {
        Body::from(body)
    });
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).expect("content length"),
    );
    response
}

async fn cargo_index_get(
    State(app): State<App>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = cargo_index_url(app.upstreams(), &path) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata(upstream, MetadataRewrite::None)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn cargo_index_head(
    State(app): State<App>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = cargo_index_url(app.upstreams(), &path) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata_head(upstream, MetadataRewrite::None)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

async fn cargo_download_get(
    State(app): State<App>,
    Path((crate_name, version)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = cargo_download_url(app.upstreams(), &crate_name, &version) else {
        return crate::proxy::not_found();
    };
    app.handle_artifact(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn cargo_download_head(
    State(app): State<App>,
    Path((crate_name, version)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = cargo_download_url(app.upstreams(), &crate_name, &version) else {
        return crate::proxy::not_found();
    };
    app.handle_artifact_head(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}
