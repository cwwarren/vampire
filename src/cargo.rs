use crate::proxy::{MetadataRewrite, request_failed_response};
use crate::routes::{cargo_config, cargo_download_url, cargo_index_url};
use crate::state::App;
use crate::stats::{UPSTREAM_CARGO_DOWNLOAD, UPSTREAM_CARGO_INDEX};
use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderValue, Uri};
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

async fn cargo_config_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    if uri.query().is_some() {
        return crate::proxy::not_found();
    }
    cargo_config_response(app.public_base_url(), false)
}

async fn cargo_config_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    if uri.query().is_some() {
        return crate::proxy::not_found();
    }
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

async fn cargo_index_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(path) = raw_path_tail(&uri, "/cargo/index/") else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = cargo_index_url(app.upstreams(), path) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata(upstream, UPSTREAM_CARGO_INDEX, MetadataRewrite::None)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn cargo_index_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(path) = raw_path_tail(&uri, "/cargo/index/") else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = cargo_index_url(app.upstreams(), path) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata_head(upstream, UPSTREAM_CARGO_INDEX, MetadataRewrite::None)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

async fn cargo_download_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some((crate_name, version)) = raw_download(&uri) else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = cargo_download_url(app.upstreams(), crate_name, version) else {
        return crate::proxy::not_found();
    };
    app.handle_artifact(upstream, UPSTREAM_CARGO_DOWNLOAD)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn cargo_download_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some((crate_name, version)) = raw_download(&uri) else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = cargo_download_url(app.upstreams(), crate_name, version) else {
        return crate::proxy::not_found();
    };
    app.handle_artifact_head(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

fn raw_path_tail<'a>(uri: &'a Uri, prefix: &str) -> Option<&'a str> {
    if uri.query().is_some() {
        return None;
    }
    let path = uri.path().strip_prefix(prefix)?;
    (!path.is_empty()).then_some(path)
}

fn raw_download(uri: &Uri) -> Option<(&str, &str)> {
    if uri.query().is_some() {
        return None;
    }
    let path = uri
        .path()
        .strip_prefix("/cargo/api/v1/crates/")?
        .strip_suffix("/download")?;
    let (crate_name, version) = path.split_once('/')?;
    (!crate_name.is_empty() && !version.is_empty() && !version.contains('/'))
        .then_some((crate_name, version))
}

#[cfg(test)]
mod tests {
    use super::{raw_download, raw_path_tail};
    use axum::http::Uri;

    #[test]
    fn preserves_raw_cargo_paths() {
        let uri: Uri = "/cargo/index/se/rd/serde".parse().unwrap();
        assert_eq!(raw_path_tail(&uri, "/cargo/index/"), Some("se/rd/serde"));
        let uri: Uri = "/cargo/api/v1/crates/demo/1.0.0/download".parse().unwrap();
        assert_eq!(raw_download(&uri), Some(("demo", "1.0.0")));
    }

    #[test]
    fn rejects_cargo_query_aliases() {
        let uri: Uri = "/cargo/index/se/rd/serde?alias=true".parse().unwrap();
        assert_eq!(raw_path_tail(&uri, "/cargo/index/"), None);
        let uri: Uri = "/cargo/api/v1/crates/demo/1.0.0/download?alias=true"
            .parse()
            .unwrap();
        assert_eq!(raw_download(&uri), None);
    }
}
