use crate::proxy::{MetadataRewrite, request_failed_response};
use crate::routes::{pypi_file_url, pypi_simple_url};
use crate::state::App;
use crate::stats::{UPSTREAM_PYPI_FILES, UPSTREAM_PYPI_SIMPLE};
use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;

pub(crate) fn router() -> Router<App> {
    Router::new()
        .route(
            "/pypi/simple/",
            get(pypi_simple_root_get).head(pypi_simple_root_head),
        )
        .route(
            "/pypi/simple/{project}/",
            get(pypi_simple_project_get).head(pypi_simple_project_head),
        )
        .route(
            "/pypi/files/{*path}",
            get(pypi_file_get).head(pypi_file_head),
        )
}

async fn pypi_simple_root_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    if uri.query().is_some() {
        return crate::proxy::not_found();
    }
    let Some(upstream) = pypi_simple_url(app.upstreams(), None) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata(
        upstream,
        UPSTREAM_PYPI_SIMPLE,
        MetadataRewrite::Pypi(app.public_base_url().to_owned()),
    )
    .await
    .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn pypi_simple_root_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    if uri.query().is_some() {
        return crate::proxy::not_found();
    }
    let Some(upstream) = pypi_simple_url(app.upstreams(), None) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata_head(
        upstream,
        UPSTREAM_PYPI_SIMPLE,
        MetadataRewrite::Pypi(app.public_base_url().to_owned()),
    )
    .await
    .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

async fn pypi_simple_project_get(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(project) = raw_project(&uri) else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = pypi_simple_url(app.upstreams(), Some(project)) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata(
        upstream,
        UPSTREAM_PYPI_SIMPLE,
        MetadataRewrite::Pypi(app.public_base_url().to_owned()),
    )
    .await
    .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn pypi_simple_project_head(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(project) = raw_project(&uri) else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = pypi_simple_url(app.upstreams(), Some(project)) else {
        return crate::proxy::not_found();
    };
    app.handle_metadata_head(
        upstream,
        UPSTREAM_PYPI_SIMPLE,
        MetadataRewrite::Pypi(app.public_base_url().to_owned()),
    )
    .await
    .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

async fn pypi_file_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(path) = raw_path_tail(&uri, "/pypi/files/") else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = pypi_file_url(path, app.upstreams()) else {
        return crate::proxy::not_found();
    };
    app.handle_artifact(upstream, UPSTREAM_PYPI_FILES)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, &error))
}

async fn pypi_file_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(path) = raw_path_tail(&uri, "/pypi/files/") else {
        return crate::proxy::not_found();
    };
    let Some(upstream) = pypi_file_url(path, app.upstreams()) else {
        return crate::proxy::not_found();
    };
    app.handle_artifact_head(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, &error))
}

fn raw_project(uri: &Uri) -> Option<&str> {
    if uri.query().is_some() {
        return None;
    }
    let project = uri
        .path()
        .strip_prefix("/pypi/simple/")?
        .strip_suffix('/')?;
    (!project.is_empty() && !project.contains('/')).then_some(project)
}

fn raw_path_tail<'a>(uri: &'a Uri, prefix: &str) -> Option<&'a str> {
    if uri.query().is_some() {
        return None;
    }
    let path = uri.path().strip_prefix(prefix)?;
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::{raw_path_tail, raw_project};
    use axum::http::Uri;

    #[test]
    fn preserves_raw_project_and_file_paths() {
        let uri: Uri = "/pypi/simple/pkg%2Falias/".parse().unwrap();
        assert_eq!(raw_project(&uri), Some("pkg%2Falias"));
        let uri: Uri = "/pypi/files/pkg%20name.whl".parse().unwrap();
        assert_eq!(raw_path_tail(&uri, "/pypi/files/"), Some("pkg%20name.whl"));
    }

    #[test]
    fn rejects_query_aliases() {
        let uri: Uri = "/pypi/simple/pkg/?alias=true".parse().unwrap();
        assert_eq!(raw_project(&uri), None);
        let uri: Uri = "/pypi/files/pkg.whl?alias=true".parse().unwrap();
        assert_eq!(raw_path_tail(&uri, "/pypi/files/"), None);
    }
}
