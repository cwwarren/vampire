use crate::cache::{
    ArtifactLeader, ArtifactLookup, CacheStore, InflightOutcome, StoredEntry, StoredResponseMeta,
};
use crate::config::Config;
use crate::failure_log::log_failure;
use crate::routes::{
    CacheClass, RegistryOrigins, cargo_config, cargo_download_url, cargo_index_url,
    npm_packument_url, npm_tarball_url, pypi_file_url, pypi_simple_url, rewrite_npm_json,
    rewrite_pypi_html,
};
use crate::stats::AppStats;
use axum::Router;
use axum::body::Body;
use axum::extract::{OriginalUri, Path, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use reqwest::Client;
use reqwest::header::HeaderMap as ReqwestHeaderMap;
use serde_json::json;
use std::io;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;

#[derive(Clone)]
pub struct App {
    cache: CacheStore,
    client: Client,
    stats: AppStats,
    upstreams: RegistryOrigins,
}

#[derive(Clone)]
enum MetadataRewrite {
    None,
    Npm(String),
    Pypi(String),
}

struct ArtifactFetchCleanup {
    cache: CacheStore,
    inflight: Arc<crate::cache::Inflight>,
    key: String,
    temp_path: std::path::PathBuf,
    armed: bool,
}

impl ArtifactFetchCleanup {
    fn new(
        cache: CacheStore,
        inflight: Arc<crate::cache::Inflight>,
        key: String,
        temp_path: std::path::PathBuf,
    ) -> Self {
        Self {
            cache,
            inflight,
            key,
            temp_path,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ArtifactFetchCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let cache = self.cache.clone();
        let inflight = self.inflight.clone();
        let key = self.key.clone();
        let temp_path = self.temp_path.clone();
        tokio::spawn(async move {
            let _ = fs::remove_file(&temp_path).await;
            inflight.fail("artifact fetch cancelled".to_owned()).await;
            cache.finish_inflight(&key).await;
        });
    }
}

impl App {
    pub async fn new(config: Config) -> io::Result<Self> {
        let client = Client::builder()
            .http2_adaptive_window(true)
            .tcp_nodelay(true)
            .timeout(config.upstream_timeout)
            .build()
            .map_err(io::Error::other)?;
        Self::new_with_upstreams(config, client, RegistryOrigins::default()).await
    }

    pub async fn new_with_client(config: Config, client: Client) -> io::Result<Self> {
        Self::new_with_upstreams(config, client, RegistryOrigins::default()).await
    }

    pub async fn new_with_upstreams(
        config: Config,
        client: Client,
        upstreams: RegistryOrigins,
    ) -> io::Result<Self> {
        let cache = CacheStore::new(&config).await?;
        Ok(Self {
            cache,
            client,
            stats: AppStats::default(),
            upstreams,
        })
    }

    pub fn stats(&self) -> AppStats {
        self.stats.clone()
    }

    pub async fn serve(self, listener: TcpListener) -> io::Result<()> {
        axum::serve(listener, self.router())
            .await
            .map_err(io::Error::other)
    }

    fn router(self) -> Router {
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
            .route(
                "/pypi/simple/",
                get(pypi_simple_root_get).head(pypi_simple_root_head),
            )
            .route(
                "/pypi/simple/{*project}",
                get(pypi_simple_project_get).head(pypi_simple_project_head),
            )
            .route(
                "/pypi/files/{filename}",
                get(pypi_file_get).head(pypi_file_head),
            )
            .route(
                "/npm/tarballs/{filename}",
                get(npm_tarball_get).head(npm_tarball_head),
            )
            .route(
                "/npm/{*package}",
                get(npm_packument_get).head(npm_packument_head),
            )
            .fallback(not_found)
            .with_state(self)
    }

    async fn handle_head(
        &self,
        cache_class: CacheClass,
        upstream: reqwest::Url,
    ) -> io::Result<Response> {
        let key = CacheStore::key_for(cache_class, upstream.as_str(), "");
        if let Some(entry) = self.cache.load(&key).await? {
            return Ok(empty_response_from_meta(entry.meta));
        }
        let response = self
            .client
            .head(upstream)
            .send()
            .await
            .map_err(io::Error::other)?;
        let meta = meta_from_upstream(cache_class, response.status(), response.headers(), 0);
        Ok(empty_response_from_meta(meta))
    }

    async fn handle_metadata(
        &self,
        upstream: reqwest::Url,
        rewrite: MetadataRewrite,
    ) -> io::Result<Response> {
        let key = CacheStore::key_for(CacheClass::Metadata, upstream.as_str(), "");
        if let Some(entry) = self.cache.load(&key).await? {
            if entry.meta.etag.is_some() || entry.meta.last_modified.is_some() {
                return self
                    .revalidate_metadata(upstream, rewrite, key, entry)
                    .await;
            }
            return file_response(entry).await;
        }
        self.fetch_metadata(upstream, rewrite, key).await
    }

    async fn revalidate_metadata(
        &self,
        upstream: reqwest::Url,
        rewrite: MetadataRewrite,
        key: String,
        entry: StoredEntry,
    ) -> io::Result<Response> {
        let mut request = self.client.get(upstream.clone());
        self.stats
            .record_fetch(CacheClass::Metadata, upstream.as_str());
        if let Some(etag) = &entry.meta.etag {
            request = request.header(IF_NONE_MATCH.as_str(), etag);
        }
        if let Some(last_modified) = &entry.meta.last_modified {
            request = request.header(IF_MODIFIED_SINCE.as_str(), last_modified);
        }
        let response = request.send().await.map_err(io::Error::other)?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return file_response(entry).await;
        }
        self.finish_metadata(rewrite, key, response).await
    }

    async fn fetch_metadata(
        &self,
        upstream: reqwest::Url,
        rewrite: MetadataRewrite,
        key: String,
    ) -> io::Result<Response> {
        self.stats
            .record_fetch(CacheClass::Metadata, upstream.as_str());
        let response = self
            .client
            .get(upstream)
            .send()
            .await
            .map_err(io::Error::other)?;
        self.finish_metadata(rewrite, key, response).await
    }

    async fn finish_metadata(
        &self,
        rewrite: MetadataRewrite,
        key: String,
        response: reqwest::Response,
    ) -> io::Result<Response> {
        let status = response.status();
        let upstream_headers = response.headers().clone();
        let body = response.bytes().await.map_err(io::Error::other)?;
        let rewritten = match rewrite {
            MetadataRewrite::None => body.to_vec(),
            MetadataRewrite::Npm(origin) => {
                rewrite_npm_json(&body, &self.upstreams, &origin).map_err(io::Error::other)?
            }
            MetadataRewrite::Pypi(origin) => {
                rewrite_pypi_html(&body, &self.upstreams, &origin).map_err(io::Error::other)?
            }
        };
        let meta = meta_for_bytes(
            CacheClass::Metadata,
            status,
            &upstream_headers,
            rewritten.len(),
        );
        if status == StatusCode::OK && (meta.etag.is_some() || meta.last_modified.is_some()) {
            let entry = self.cache.store_metadata(&key, &rewritten, &meta).await?;
            return file_response(entry).await;
        }
        Ok(bytes_response(meta, Bytes::from(rewritten)))
    }

    async fn handle_artifact(&self, upstream: reqwest::Url) -> io::Result<Response> {
        let key = CacheStore::key_for(CacheClass::Artifact, upstream.as_str(), "");
        match self.cache.lookup_or_start_artifact(key.clone()).await? {
            ArtifactLookup::Hit(entry) => file_response(entry).await,
            ArtifactLookup::Join(inflight) => {
                self.stats.record_artifact_join(upstream.as_str());
                self.serve_inflight(&key, inflight).await
            }
            ArtifactLookup::Leader(leader) => {
                let inflight = leader.inflight.clone();
                self.spawn_artifact_fetch(upstream, leader);
                self.serve_inflight(&key, inflight).await
            }
        }
    }

    fn spawn_artifact_fetch(&self, upstream: reqwest::Url, leader: ArtifactLeader) {
        let app = self.clone();
        tokio::spawn(async move {
            app.run_artifact_fetch(upstream, leader).await;
        });
    }

    async fn serve_inflight(
        &self,
        key: &str,
        inflight: Arc<crate::cache::Inflight>,
    ) -> io::Result<Response> {
        match inflight.wait_for_outcome().await? {
            InflightOutcome::Cached => {
                let entry = self.cache.load(key).await?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "artifact missing after inflight completion",
                    )
                })?;
                file_response(entry).await
            }
            InflightOutcome::Response(meta, body) => Ok(bytes_response(meta, body)),
        }
    }

    async fn run_artifact_fetch(&self, upstream: reqwest::Url, leader: ArtifactLeader) {
        let upstream_string = upstream.as_str().to_owned();
        let fail = |stage: &str, error: &str| {
            log_failure(
                "artifact_fetch_failed",
                json!({
                    "stage": stage,
                    "upstream": upstream_string,
                    "cache_key": leader.key,
                    "error": error,
                }),
            );
        };
        let mut cleanup = ArtifactFetchCleanup::new(
            self.cache.clone(),
            leader.inflight.clone(),
            leader.key.clone(),
            leader.paths.temp.clone(),
        );
        let _permit = match self.cache.acquire_upstream_permit().await {
            Ok(permit) => permit,
            Err(error) => {
                fail("acquire_upstream_permit", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                cleanup.disarm();
                return;
            }
        };
        self.stats
            .record_fetch(CacheClass::Artifact, upstream.as_str());
        let response = match self.client.get(upstream).send().await {
            Ok(response) => response,
            Err(error) => {
                let error = io::Error::other(error);
                fail("fetch_upstream", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                cleanup.disarm();
                return;
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        if status != StatusCode::OK {
            let body = match response.bytes().await {
                Ok(body) => body,
                Err(error) => {
                    let error = io::Error::other(error);
                    fail("read_error_response", &error.to_string());
                    leader.inflight.fail(error.to_string()).await;
                    self.cache.finish_inflight(&leader.key).await;
                    cleanup.disarm();
                    return;
                }
            };
            let meta = meta_for_bytes(CacheClass::Artifact, status, &headers, body.len());
            leader
                .inflight
                .finish_response(meta.clone(), body.clone())
                .await;
            self.cache.finish_inflight(&leader.key).await;
            cleanup.disarm();
            return;
        }
        if let Some(parent) = leader.paths.temp.parent() {
            if let Err(error) = fs::create_dir_all(parent).await {
                fail("create_temp_dir", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                cleanup.disarm();
                return;
            }
        }
        let mut file = match fs::File::create(&leader.paths.temp).await {
            Ok(file) => file,
            Err(error) => {
                fail("create_temp_file", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                cleanup.disarm();
                return;
            }
        };
        let mut response = response;
        let mut content_length = 0;
        loop {
            let chunk = match response.chunk().await {
                Ok(chunk) => chunk,
                Err(error) => {
                    let error = io::Error::other(error);
                    let _ = fs::remove_file(&leader.paths.temp).await;
                    fail("read_upstream_stream", &error.to_string());
                    leader.inflight.fail(error.to_string()).await;
                    self.cache.finish_inflight(&leader.key).await;
                    cleanup.disarm();
                    return;
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            if let Err(error) = file.write_all(&chunk).await {
                let _ = fs::remove_file(&leader.paths.temp).await;
                fail("write_temp_file", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                cleanup.disarm();
                return;
            }
            content_length += chunk.len();
        }
        if let Err(error) = file.flush().await {
            drop(file);
            let _ = fs::remove_file(&leader.paths.temp).await;
            fail("flush_temp_file", &error.to_string());
            leader.inflight.fail(error.to_string()).await;
            self.cache.finish_inflight(&leader.key).await;
            cleanup.disarm();
            return;
        }
        drop(file);
        let meta = meta_from_upstream(CacheClass::Artifact, status, &headers, content_length);
        let entry = match self
            .cache
            .commit_artifact(&leader.key, &meta, &leader.paths.temp)
            .await
        {
            Ok(entry) => entry,
            Err(error) => {
                let _ = fs::remove_file(&leader.paths.temp).await;
                fail("commit_cache_entry", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                cleanup.disarm();
                return;
            }
        };
        leader.inflight.finish_cached().await;
        self.cache.finish_inflight(&leader.key).await;
        cleanup.disarm();
        let _ = entry;
    }
}

async fn cargo_config_get(State(_app): State<App>, headers: HeaderMap) -> Response {
    let origin = request_origin(&headers);
    json_response(StatusCode::OK, cargo_config(&origin))
}

async fn cargo_config_head(State(_app): State<App>) -> Response {
    empty_response(StatusCode::OK)
}

async fn cargo_index_get(
    State(app): State<App>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let Some(upstream) = cargo_index_url(&app.upstreams, &path) else {
        return not_found().await;
    };
    let _origin = request_origin(&headers);
    app.handle_metadata(upstream, MetadataRewrite::None)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, error))
}

async fn cargo_index_head(
    State(app): State<App>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = cargo_index_url(&app.upstreams, &path) else {
        return not_found().await;
    };
    app.handle_head(CacheClass::Metadata, upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, error))
}

async fn cargo_download_get(
    State(app): State<App>,
    Path((crate_name, version)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = cargo_download_url(&app.upstreams, &crate_name, &version) else {
        return not_found().await;
    };
    app.handle_artifact(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, error))
}

async fn cargo_download_head(
    State(app): State<App>,
    Path((crate_name, version)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = cargo_download_url(&app.upstreams, &crate_name, &version) else {
        return not_found().await;
    };
    app.handle_head(CacheClass::Artifact, upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, error))
}

async fn pypi_simple_root_get(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&headers);
    let Some(upstream) = pypi_simple_url(&app.upstreams, None) else {
        return not_found().await;
    };
    app.handle_metadata(upstream, MetadataRewrite::Pypi(origin))
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, error))
}

async fn pypi_simple_root_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(upstream) = pypi_simple_url(&app.upstreams, None) else {
        return not_found().await;
    };
    app.handle_head(CacheClass::Metadata, upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, error))
}

async fn pypi_simple_project_get(
    State(app): State<App>,
    Path(project): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&headers);
    let Some(upstream) = pypi_simple_url(&app.upstreams, Some(&project)) else {
        return not_found().await;
    };
    app.handle_metadata(upstream, MetadataRewrite::Pypi(origin))
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, error))
}

async fn pypi_simple_project_head(
    State(app): State<App>,
    Path(project): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = pypi_simple_url(&app.upstreams, Some(&project)) else {
        return not_found().await;
    };
    app.handle_head(CacheClass::Metadata, upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, error))
}

async fn pypi_file_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(upstream) = pypi_file_url(uri.query(), &app.upstreams) else {
        return not_found().await;
    };
    app.handle_artifact(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, error))
}

async fn pypi_file_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(upstream) = pypi_file_url(uri.query(), &app.upstreams) else {
        return not_found().await;
    };
    app.handle_head(CacheClass::Artifact, upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, error))
}

async fn npm_packument_get(
    State(app): State<App>,
    Path(package): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&headers);
    let Some(upstream) = npm_packument_url(&app.upstreams, &package) else {
        return not_found().await;
    };
    app.handle_metadata(upstream, MetadataRewrite::Npm(origin))
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, error))
}

async fn npm_packument_head(
    State(app): State<App>,
    Path(package): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let Some(upstream) = npm_packument_url(&app.upstreams, &package) else {
        return not_found().await;
    };
    app.handle_head(CacheClass::Metadata, upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, error))
}

async fn npm_tarball_get(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(upstream) = npm_tarball_url(uri.query(), &app.upstreams) else {
        return not_found().await;
    };
    app.handle_artifact(upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("GET", &uri, error))
}

async fn npm_tarball_head(State(app): State<App>, OriginalUri(uri): OriginalUri) -> Response {
    let Some(upstream) = npm_tarball_url(uri.query(), &app.upstreams) else {
        return not_found().await;
    };
    app.handle_head(CacheClass::Artifact, upstream)
        .await
        .unwrap_or_else(|error| request_failed_response("HEAD", &uri, error))
}

async fn not_found() -> Response {
    simple_response(
        StatusCode::NOT_FOUND,
        "text/plain; charset=utf-8",
        "not found",
    )
}

fn request_failed_response(method: &str, uri: &Uri, error: io::Error) -> Response {
    log_failure(
        "request_failed",
        json!({
            "method": method,
            "path": uri.path(),
            "query": uri.query(),
            "error": error.to_string(),
        }),
    );
    simple_response(
        StatusCode::BAD_GATEWAY,
        "text/plain; charset=utf-8",
        error.to_string(),
    )
}

fn request_origin(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

fn meta_from_upstream(
    cache_class: CacheClass,
    status: StatusCode,
    headers: &ReqwestHeaderMap,
    content_length: usize,
) -> StoredResponseMeta {
    let mut stored_headers = Vec::new();
    for (name, value) in headers {
        if is_hop_header(name.as_str()) {
            continue;
        }
        if name.as_str() == CONTENT_LENGTH.as_str() {
            continue;
        }
        if let Ok(value) = value.to_str() {
            stored_headers.push((name.as_str().to_owned(), value.to_owned()));
        }
    }
    if content_length > 0 {
        stored_headers.push((
            CONTENT_LENGTH.as_str().to_owned(),
            content_length.to_string(),
        ));
    }
    StoredResponseMeta {
        cache_class,
        headers: stored_headers,
        last_modified: headers
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        etag: headers
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        status: status.as_u16(),
    }
}

fn meta_for_bytes(
    cache_class: CacheClass,
    status: StatusCode,
    headers: &ReqwestHeaderMap,
    content_length: usize,
) -> StoredResponseMeta {
    let mut meta = meta_from_upstream(cache_class, status, headers, content_length);
    if !meta
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(CONTENT_TYPE.as_str()))
    {
        meta.headers.push((
            CONTENT_TYPE.as_str().to_owned(),
            "application/octet-stream".to_owned(),
        ));
    }
    meta
}

async fn file_response(entry: StoredEntry) -> io::Result<Response> {
    let file = fs::File::open(entry.body_path).await?;
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    *response.status_mut() = StatusCode::from_u16(entry.meta.status).unwrap_or(StatusCode::OK);
    apply_headers(response.headers_mut(), &entry.meta.headers);
    Ok(response)
}

fn bytes_response(meta: StoredResponseMeta, body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(meta.status).unwrap_or(StatusCode::OK);
    apply_headers(response.headers_mut(), &meta.headers);
    response
}

fn empty_response_from_meta(meta: StoredResponseMeta) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::from_u16(meta.status).unwrap_or(StatusCode::OK);
    apply_headers(response.headers_mut(), &meta.headers);
    response
}

fn apply_headers(headers: &mut HeaderMap, pairs: &[(String, String)]) {
    for (name, value) in pairs {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        headers.insert(name, value);
    }
}

fn is_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn simple_response(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<String>,
) -> Response {
    let body = body.into();
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).expect("content length"),
    );
    response
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).expect("content length"),
    );
    response
}

fn empty_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::{App, ArtifactLookup, InflightOutcome};
    use crate::config::Config;
    use crate::routes::RegistryOrigins;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use bytes::Bytes;
    use reqwest::Client;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn aborted_artifact_fetch_cleans_up_inflight() {
        let started = Arc::new(Notify::new());
        let upstream = slow_upstream(started.clone()).await.unwrap();
        let temp = tempdir().unwrap();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            cache_dir: PathBuf::from(temp.path()),
            max_cache_size: 16 * 1024 * 1024,
            max_upstream_fetches: 4,
            upstream_timeout: Duration::from_secs(30),
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let app = App::new_with_upstreams(config, client, RegistryOrigins::default())
            .await
            .unwrap();
        let upstream_url = reqwest::Url::parse(&format!("http://{}/artifact", upstream)).unwrap();
        let key = crate::cache::CacheStore::key_for(
            crate::routes::CacheClass::Artifact,
            upstream_url.as_str(),
            "",
        );
        let leader = match app
            .cache
            .lookup_or_start_artifact(key.clone())
            .await
            .unwrap()
        {
            ArtifactLookup::Leader(leader) => leader,
            other => panic!(
                "expected leader, got unexpected lookup state: {:?}",
                type_name(&other)
            ),
        };
        let inflight = leader.inflight.clone();
        let app_task = app.clone();
        let upstream_task = upstream_url.clone();
        let task = tokio::spawn(async move {
            app_task.run_artifact_fetch(upstream_task, leader).await;
        });
        started.notified().await;
        task.abort();
        let outcome = timeout(Duration::from_secs(2), inflight.wait_for_outcome())
            .await
            .expect("cleanup should resolve inflight")
            .expect("inflight wait should not error");
        match outcome {
            InflightOutcome::Response(meta, body) => {
                assert_eq!(meta.status, StatusCode::BAD_GATEWAY.as_u16());
                assert_eq!(body, Bytes::from_static(b"artifact fetch cancelled"));
            }
            InflightOutcome::Cached => panic!("unexpected cached outcome"),
        }
        match app.cache.lookup_or_start_artifact(key).await.unwrap() {
            ArtifactLookup::Leader(next) => {
                next.inflight.fail("test cleanup".to_owned()).await;
                app.cache.finish_inflight(&next.key).await;
            }
            ArtifactLookup::Join(_) => panic!("stale inflight entry remained after abort"),
            ArtifactLookup::Hit(_) => panic!("unexpected cached artifact after abort"),
        }
    }

    async fn slow_upstream(started: Arc<Notify>) -> io::Result<std::net::SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let router = Router::new().route(
            "/artifact",
            get(move || {
                let started = started.clone();
                async move {
                    started.notify_waiters();
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    (StatusCode::OK, Body::from("never reached"))
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(addr)
    }

    fn type_name(lookup: &ArtifactLookup) -> &'static str {
        match lookup {
            ArtifactLookup::Hit(_) => "hit",
            ArtifactLookup::Join(_) => "join",
            ArtifactLookup::Leader(_) => "leader",
        }
    }
}
