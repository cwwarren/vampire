use crate::failure_log::log_failure;
use crate::proxy::{MetadataRewrite, request_failed_response, simple_response};
use crate::routes::{join_url, npm_packument_url, npm_tarball_url};
use crate::state::App;
use crate::stats::UPSTREAM_NPM;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post};

const MAX_AUDIT_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn router() -> Router<App> {
    Router::new()
        .route("/npm/-/v1/search", get(npm_search))
        .route("/npm/-/npm/v1/security/advisories/bulk", post(npm_audit))
        .route("/npm/-/npm/v1/security/audits/quick", post(npm_audit))
        .route(
            "/npm/tarballs/{*path}",
            get(npm_tarball_get).head(npm_tarball_head),
        )
        .route(
            "/npm/{*package}",
            get(npm_packument_get).head(npm_packument_head),
        )
}

async fn npm_search(
    State(app): State<App>,
    method: Method,
    OriginalUri(uri): OriginalUri,
) -> Response {
    if uri.path() != "/npm/-/v1/search" {
        return crate::proxy::not_found();
    }
    let mut upstream = join_url(&app.upstreams().npm, "-/v1/search").expect("npm search URL");
    upstream.set_query(uri.query());
    if upstream.query_pairs().any(|(name, _)| {
        !matches!(
            name.as_ref(),
            "text" | "size" | "from" | "quality" | "popularity" | "maintenance"
        )
    }) {
        return crate::proxy::not_found();
    }
    app.stats().record_npm_search_request();
    let _permit = match app.cache().try_acquire_upstream_permit() {
        Ok(permit) => permit,
        Err(error) => return request_failed_response(method.as_str(), &uri, &error),
    };
    let response = app
        .handle_npm_request(app.client().get(upstream))
        .await
        .unwrap_or_else(|error| request_failed_response(method.as_str(), &uri, &error));
    if method == Method::HEAD {
        let (parts, _) = response.into_parts();
        Response::from_parts(parts, Body::empty())
    } else {
        response
    }
}

async fn npm_audit(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let path = match uri.path() {
        "/npm/-/npm/v1/security/advisories/bulk" => "-/npm/v1/security/advisories/bulk",
        "/npm/-/npm/v1/security/audits/quick" => "-/npm/v1/security/audits/quick",
        _ => return crate::proxy::not_found(),
    };
    if uri.query().is_some() {
        return crate::proxy::not_found();
    }
    app.stats().record_npm_audit_request();
    let _permit = match app.cache().try_acquire_upstream_permit() {
        Ok(permit) => permit,
        Err(error) => return request_failed_response("POST", &uri, &error),
    };
    let body = match to_bytes(body, MAX_AUDIT_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            log_failure(
                "npm_audit_body_read_failed",
                serde_json::json!({
                    "method": "POST", "path": uri.path(), "error": error.to_string(),
                }),
            );
            return simple_response(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                error.to_string(),
            );
        }
    };
    let upstream = join_url(&app.upstreams().npm, path).expect("npm audit URL");
    let mut request = app.client().post(upstream).body(body);
    for name in ["content-type", "content-encoding"] {
        if let Some(value) = headers.get(name) {
            request = request.header(name, value);
        }
    }
    app.handle_npm_request(request)
        .await
        .unwrap_or_else(|error| request_failed_response("POST", &uri, &error))
}

async fn npm_packument_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(package) = raw_path_tail(&uri, "/npm/") else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = npm_packument_url(app.upstreams(), package) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata(
        upstream,
        UPSTREAM_NPM,
        MetadataRewrite::Npm(app.public_base_url().to_owned()),
    )
    .await
    .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn npm_packument_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(package) = raw_path_tail(&uri, "/npm/") else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = npm_packument_url(app.upstreams(), package) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata_head(
        upstream,
        UPSTREAM_NPM,
        MetadataRewrite::Npm(app.public_base_url().to_owned()),
    )
    .await
    .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

async fn npm_tarball_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(path) = raw_path_tail(&uri, "/npm/tarballs/") else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = npm_tarball_url(path, app.upstreams()) else {
        return crate::proxy::not_found();
    };
    app.handle_artifact(upstream, UPSTREAM_NPM)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn npm_tarball_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(path) = raw_path_tail(&uri, "/npm/tarballs/") else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = npm_tarball_url(path, app.upstreams()) else {
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
    let tail = uri.path().strip_prefix(prefix)?;
    if tail.is_empty() {
        return None;
    }
    Some(tail)
}

#[cfg(test)]
mod tests {
    use super::raw_path_tail;
    use axum::http::Uri;

    #[test]
    fn preserves_raw_path_and_rejects_queries() {
        let uri: Uri = "/npm/@scope%2Fname".parse().unwrap();
        assert_eq!(raw_path_tail(&uri, "/npm/"), Some("@scope%2Fname"));
        let uri: Uri = "/npm/pkg?alias=true".parse().unwrap();
        assert_eq!(raw_path_tail(&uri, "/npm/"), None);
    }
}
