use crate::proxy::{MetadataRewrite, request_failed_response, request_origin};
use crate::routes::{npm_packument_url, npm_tarball_url};
use crate::state::App;
use axum::Router;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use axum::routing::get;

pub(crate) fn router() -> Router<App> {
    Router::new()
        .route(
            "/npm/tarballs/{*path}",
            get(npm_tarball_get).head(npm_tarball_head),
        )
        .route(
            "/npm/{*package}",
            get(npm_packument_get).head(npm_packument_head),
        )
}

async fn npm_packument_get(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&headers);
    let Some(package) = raw_path_tail(&uri, "/npm/") else {
        return crate::proxy::not_found().await;
    };
    let Some(upstream) = npm_packument_url(app.upstreams(), package) else {
        return crate::proxy::not_found().await;
    };
    app.handle_metadata(upstream, MetadataRewrite::Npm(origin))
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn npm_packument_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(package) = raw_path_tail(&uri, "/npm/") else {
        return crate::proxy::not_found().await;
    };
    let Some(upstream) = npm_packument_url(app.upstreams(), package) else {
        return crate::proxy::not_found().await;
    };
    app.handle_metadata_head(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

async fn npm_tarball_get(
    State(app): State<App>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = npm_tarball_url(&path, app.upstreams()) else {
        return crate::proxy::not_found().await;
    };
    app.handle_artifact(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn npm_tarball_head(
    State(app): State<App>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = npm_tarball_url(&path, app.upstreams()) else {
        return crate::proxy::not_found().await;
    };
    app.handle_artifact_head(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

fn raw_path_tail<'a>(uri: &'a Uri, prefix: &str) -> Option<&'a str> {
    let tail = uri.path().strip_prefix(prefix)?;
    if tail.is_empty() {
        return None;
    }
    Some(tail)
}
