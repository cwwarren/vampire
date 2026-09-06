use crate::cache::{
    ArtifactLeader, ArtifactLookup, CacheStore, Inflight, InflightOutcome, MetadataLeader,
    MetadataLookup, MetadataMemoryReservation, PublishedEntry, StoredEntry, StoredResponseMeta,
};
use crate::failure_log::log_failure;
use crate::routes::{MAX_METADATA_BODY_LEN, rewrite_npm_json, rewrite_pypi_html};
use crate::state::App;
use axum::body::Body;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use reqwest::header::HeaderMap as ReqwestHeaderMap;
use serde_json::json;
use std::io;
use std::sync::Arc;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

const MAX_ERROR_BODY_LEN: usize = 1024 * 1024;
const MIN_REWRITABLE_URL_LEN: usize = 20;
const REWRITE_PREFIX_LEN: usize = 16;

#[derive(Clone)]
pub(crate) enum MetadataRewrite {
    None,
    Npm(String),
    Pypi(String),
}

enum FetchOutcome {
    Cached(PublishedEntry),
    NonOk(StoredResponseMeta, Bytes),
}

struct BufferedMetadataBody {
    body: Bytes,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl AsRef<[u8]> for BufferedMetadataBody {
    fn as_ref(&self) -> &[u8] {
        self.body.as_ref()
    }
}

impl MetadataRewrite {
    fn cache_identity(&self) -> String {
        match self {
            Self::None => "raw:v1".to_owned(),
            Self::Npm(origin) => format!("npm:v2:{origin}"),
            Self::Pypi(origin) => format!("pypi:v2:{origin}"),
        }
    }

    fn output_bound(&self, input_len: usize) -> usize {
        let min_url_len = if matches!(self, Self::Pypi(_)) {
            "/simple/".len()
        } else {
            MIN_REWRITABLE_URL_LEN
        };
        match self {
            Self::None => input_len,
            Self::Npm(origin) | Self::Pypi(origin) => input_len
                .saturating_add(
                    input_len
                        .div_ceil(min_url_len)
                        .saturating_mul(origin.len().saturating_add(REWRITE_PREFIX_LEN)),
                )
                .min(MAX_METADATA_BODY_LEN),
        }
    }

    fn working_set(&self, input_len: usize, output_bound: usize) -> usize {
        match self {
            Self::None => input_len,
            Self::Pypi(_) => input_len.saturating_add(output_bound),
            Self::Npm(_) => input_len.saturating_mul(12).saturating_add(output_bound),
        }
    }
}

struct ArtifactFetchCleanup {
    app: App,
    inflight: Arc<Inflight>,
    key: String,
    temp_path: std::path::PathBuf,
    armed: bool,
}

impl ArtifactFetchCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ArtifactFetchCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let app = self.app.clone();
        let inflight = self.inflight.clone();
        let key = self.key.clone();
        let temp_path = self.temp_path.clone();
        tokio::spawn(async move {
            let _ = fs::remove_file(&temp_path).await;
            inflight
                .fail(
                    io::ErrorKind::Interrupted,
                    "artifact fetch cancelled".to_owned(),
                )
                .await;
            app.cache().finish_inflight(&key).await;
        });
    }
}

struct MetadataFetchCleanup {
    app: App,
    inflight: Arc<Inflight>,
    key: String,
    armed: bool,
}

impl MetadataFetchCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for MetadataFetchCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let app = self.app.clone();
        let inflight = self.inflight.clone();
        let key = self.key.clone();
        tokio::spawn(async move {
            inflight
                .fail(
                    io::ErrorKind::Interrupted,
                    "metadata fetch cancelled".to_owned(),
                )
                .await;
            app.cache().finish_inflight(&key).await;
        });
    }
}

impl App {
    pub(crate) async fn handle_npm_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> io::Result<Response> {
        let response = request.send().await.map_err(io::Error::other)?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut reservation = self.cache().metadata_memory_reservation();
        let body = read_metadata_body(response, &mut reservation).await?;
        let meta = meta_for_bytes(status, &headers, body.len(), true);
        let body = Bytes::from_owner(BufferedMetadataBody {
            body,
            _permit: reservation.into_permit(),
        });
        Ok(bytes_response(&meta, body))
    }

    pub(crate) async fn handle_artifact_head(
        &self,
        upstream: reqwest::Url,
    ) -> io::Result<Response> {
        let key = CacheStore::artifact_key(upstream.as_str());
        if let Some(entry) = self.cache().load(&key).await? {
            return Ok(empty_response_from_meta(&entry.meta));
        }
        let _permit = self.cache().try_acquire_upstream_permit()?;
        let response = self
            .client()
            .head(upstream)
            .send()
            .await
            .map_err(io::Error::other)?;
        let meta = meta_from_upstream(response.status(), response.headers(), None);
        Ok(empty_response_from_meta(&meta))
    }

    pub(crate) async fn handle_metadata_head(
        &self,
        upstream: reqwest::Url,
        upstream_type: &'static str,
        rewrite: MetadataRewrite,
    ) -> io::Result<Response> {
        Ok(response_without_body(
            self.handle_metadata(upstream, upstream_type, rewrite)
                .await?,
        ))
    }

    pub(crate) async fn handle_metadata(
        &self,
        upstream: reqwest::Url,
        upstream_type: &'static str,
        rewrite: MetadataRewrite,
    ) -> io::Result<Response> {
        let key = CacheStore::metadata_key(upstream.as_str(), &rewrite.cache_identity());
        match self.cache().lookup_or_start_metadata(key).await? {
            MetadataLookup::Join(inflight) => self.serve_metadata_inflight(inflight).await,
            MetadataLookup::Leader(leader) => {
                self.run_metadata_fetch(upstream, upstream_type, rewrite, leader)
                    .await
            }
        }
    }

    async fn run_metadata_fetch(
        &self,
        upstream: reqwest::Url,
        upstream_type: &'static str,
        rewrite: MetadataRewrite,
        leader: MetadataLeader,
    ) -> io::Result<Response> {
        let mut cleanup = MetadataFetchCleanup {
            app: self.clone(),
            inflight: leader.inflight.clone(),
            key: leader.key.clone(),
            armed: true,
        };
        let result = match self.cache().load(&leader.key).await {
            Ok(entry) => {
                self.do_metadata_fetch(upstream, upstream_type, rewrite, entry, &leader.key)
                    .await
            }
            Err(error) => Err(error),
        };
        let response = match result {
            Ok((meta, body, reservation)) => {
                let body = Bytes::from_owner(BufferedMetadataBody {
                    body,
                    _permit: reservation.into_permit(),
                });
                leader
                    .inflight
                    .finish_response(meta.clone(), body.clone())
                    .await;
                Ok(bytes_response(&meta, body))
            }
            Err(error) => {
                leader.inflight.fail(error.kind(), error.to_string()).await;
                Err(error)
            }
        };
        self.cache().finish_inflight(&leader.key).await;
        cleanup.disarm();
        response
    }

    async fn do_metadata_fetch(
        &self,
        upstream: reqwest::Url,
        upstream_type: &'static str,
        rewrite: MetadataRewrite,
        mut entry: Option<StoredEntry>,
        key: &str,
    ) -> io::Result<(StoredResponseMeta, Bytes, MetadataMemoryReservation)> {
        if entry
            .as_ref()
            .is_some_and(|entry| entry.body_len > MAX_METADATA_BODY_LEN as u64)
        {
            entry = None;
        }
        let mut request = self.client().get(upstream);
        self.stats().record_metadata_fetch(upstream_type);
        if let Some(entry) = &entry {
            if let Some(etag) = &entry.meta.etag {
                request = request.header(IF_NONE_MATCH.as_str(), etag);
            }
            if let Some(last_modified) = &entry.meta.last_modified {
                request = request.header(IF_MODIFIED_SINCE.as_str(), last_modified);
            }
        }
        let response = request.send().await.map_err(io::Error::other)?;
        if response.status() == StatusCode::NOT_MODIFIED
            && let Some(entry) = entry.as_mut()
        {
            let meta = entry.meta.clone();
            let body_len = usize::try_from(entry.body_len)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "body length overflow"))?;
            let mut reservation = self.cache().metadata_memory_reservation();
            reservation.reserve(body_len)?;
            let body = entry.read_body().await?;
            return Ok((meta, body, reservation));
        }
        self.finish_metadata(rewrite, key, response).await
    }

    async fn serve_metadata_inflight(&self, inflight: Arc<Inflight>) -> io::Result<Response> {
        match inflight.wait_for_outcome().await? {
            InflightOutcome::Response(meta, body) => Ok(bytes_response(&meta, body)),
            InflightOutcome::Failed(kind, error) => Ok(simple_response(
                error_status(kind),
                "text/plain; charset=utf-8",
                error,
            )),
            InflightOutcome::Cached(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "metadata inflight produced artifact",
            )),
        }
    }

    async fn finish_metadata(
        &self,
        rewrite: MetadataRewrite,
        key: &str,
        response: reqwest::Response,
    ) -> io::Result<(StoredResponseMeta, Bytes, MetadataMemoryReservation)> {
        let status = response.status();
        let upstream_headers = response.headers().clone();
        let mut reservation = self.cache().metadata_memory_reservation();
        let body = read_metadata_body(response, &mut reservation).await?;
        let output_bound = rewrite.output_bound(body.len());
        reservation.reserve(rewrite.working_set(body.len(), output_bound))?;
        let expose_upstream_validators = matches!(rewrite, MetadataRewrite::None);
        let rewritten = match rewrite {
            MetadataRewrite::None => body,
            MetadataRewrite::Npm(origin) => Bytes::from(
                rewrite_npm_json(&body, self.upstreams(), &origin, MAX_METADATA_BODY_LEN)
                    .map_err(io::Error::other)?,
            ),
            MetadataRewrite::Pypi(origin) => Bytes::from(
                rewrite_pypi_html(&body, self.upstreams(), &origin, MAX_METADATA_BODY_LEN)
                    .map_err(io::Error::other)?,
            ),
        };
        if rewritten.len() > MAX_METADATA_BODY_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rewritten metadata body exceeds 128 MiB limit",
            ));
        }
        let meta = meta_for_bytes(
            status,
            &upstream_headers,
            rewritten.len(),
            expose_upstream_validators,
        );
        if status == StatusCode::OK && (meta.etag.is_some() || meta.last_modified.is_some()) {
            self.cache().store_metadata(key, &rewritten, &meta).await?;
        }
        Ok((meta, rewritten, reservation))
    }

    pub(crate) async fn handle_artifact(
        &self,
        upstream: reqwest::Url,
        upstream_type: &'static str,
    ) -> io::Result<Response> {
        let key = CacheStore::artifact_key(upstream.as_str());
        match self.cache().lookup_or_start_artifact(key.clone()).await? {
            ArtifactLookup::Hit(entry) => file_response(entry).await,
            ArtifactLookup::Join(inflight) => {
                self.stats().record_artifact_join(upstream_type);
                self.serve_inflight(&key, inflight).await
            }
            ArtifactLookup::Leader(leader) => {
                let inflight = leader.inflight.clone();
                self.spawn_artifact_fetch(upstream, upstream_type, leader);
                self.serve_inflight(&key, inflight).await
            }
        }
    }

    fn spawn_artifact_fetch(
        &self,
        upstream: reqwest::Url,
        upstream_type: &'static str,
        leader: ArtifactLeader,
    ) {
        let app = self.clone();
        tokio::spawn(async move {
            app.run_artifact_fetch(upstream, upstream_type, leader)
                .await;
        });
    }

    async fn serve_inflight(&self, key: &str, inflight: Arc<Inflight>) -> io::Result<Response> {
        match inflight.wait_for_outcome().await? {
            InflightOutcome::Cached(_pin) => {
                let entry = self.cache().load(key).await?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "artifact missing after inflight completion",
                    )
                })?;
                file_response(entry).await
            }
            InflightOutcome::Response(meta, body) => Ok(bytes_response(&meta, body)),
            InflightOutcome::Failed(kind, error) => Ok(simple_response(
                error_status(kind),
                "text/plain; charset=utf-8",
                error,
            )),
        }
    }

    pub(crate) async fn run_artifact_fetch(
        &self,
        upstream: reqwest::Url,
        upstream_type: &'static str,
        leader: ArtifactLeader,
    ) {
        let mut cleanup = ArtifactFetchCleanup {
            app: self.clone(),
            inflight: leader.inflight.clone(),
            key: leader.key.clone(),
            temp_path: leader.paths.temp.clone(),
            armed: true,
        };
        let result = self
            .do_artifact_fetch(&upstream, upstream_type, &leader)
            .await;
        if result.is_err() {
            let _ = fs::remove_file(&leader.paths.temp).await;
        }
        match result {
            Ok(FetchOutcome::Cached(entry)) => leader.inflight.finish_cached(entry).await,
            Ok(FetchOutcome::NonOk(meta, body)) => {
                leader.inflight.finish_response(meta, body).await;
            }
            Err((stage, error)) => {
                log_failure(
                    "artifact_fetch_failed",
                    json!({
                        "stage": stage,
                        "upstream": upstream.as_str(),
                        "cache_key": leader.key,
                        "error": error,
                    }),
                );
                leader.inflight.fail(io::ErrorKind::Other, error).await;
            }
        }
        self.cache().finish_inflight(&leader.key).await;
        cleanup.disarm();
    }

    async fn do_artifact_fetch(
        &self,
        upstream: &reqwest::Url,
        upstream_type: &'static str,
        leader: &ArtifactLeader,
    ) -> Result<FetchOutcome, (String, String)> {
        self.stats().record_artifact_fetch(upstream_type);
        let response = self
            .client()
            .get(upstream.clone())
            .send()
            .await
            .map_err(|e| ("fetch_upstream".into(), io::Error::other(e).to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        if status != StatusCode::OK {
            let body = read_limited_body(
                response,
                MAX_ERROR_BODY_LEN,
                "upstream error body exceeds 1 MiB limit",
            )
            .await
            .map_err(|error| ("read_error_response".into(), error.to_string()))?;
            let meta = meta_for_bytes(status, &headers, body.len(), true);
            return Ok(FetchOutcome::NonOk(meta, body));
        }
        let mut file = fs::File::create(&leader.paths.temp)
            .await
            .map_err(|e| ("create_temp_file".into(), e.to_string()))?;
        let mut response = response;
        let mut content_length = 0;
        loop {
            let chunk = response.chunk().await.map_err(|e| {
                (
                    "read_upstream_stream".into(),
                    io::Error::other(e).to_string(),
                )
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            file.write_all(&chunk)
                .await
                .map_err(|e| ("write_temp_file".into(), e.to_string()))?;
            content_length += chunk.len();
        }
        file.flush()
            .await
            .map_err(|e| ("flush_temp_file".into(), e.to_string()))?;
        drop(file);
        let meta = meta_from_upstream(status, &headers, Some(content_length));
        let published = self
            .cache()
            .commit_artifact(&leader.key, &meta, &leader.paths.temp)
            .await
            .map_err(|e| ("commit_cache_entry".into(), e.to_string()))?;
        Ok(FetchOutcome::Cached(published))
    }
}

async fn read_metadata_body(
    mut response: reqwest::Response,
    reservation: &mut MetadataMemoryReservation,
) -> io::Result<Bytes> {
    let limit_error = "metadata body exceeds 128 MiB limit";
    let content_length = response.content_length();
    let initial_capacity =
        limited_initial_capacity(content_length, MAX_METADATA_BODY_LEN, limit_error)?;
    let expected_len = content_length
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(initial_capacity);
    reservation.reserve(expected_len)?;
    let mut body = BytesMut::with_capacity(initial_capacity);
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        if chunk.len() > MAX_METADATA_BODY_LEN - body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, limit_error));
        }
        reservation.reserve(body.len() + chunk.len())?;
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

async fn read_limited_body(
    mut response: reqwest::Response,
    max_len: usize,
    limit_error: &'static str,
) -> io::Result<Bytes> {
    let initial_capacity =
        limited_initial_capacity(response.content_length(), max_len, limit_error)?;
    let mut body = BytesMut::with_capacity(initial_capacity);
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        if chunk.len() > max_len - body.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, limit_error));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

#[cfg(test)]
fn metadata_initial_capacity(content_length: Option<u64>) -> io::Result<usize> {
    limited_initial_capacity(
        content_length,
        MAX_METADATA_BODY_LEN,
        "metadata body exceeds 128 MiB limit",
    )
}

fn limited_initial_capacity(
    content_length: Option<u64>,
    max_len: usize,
    limit_error: &'static str,
) -> io::Result<usize> {
    if content_length.is_some_and(|length| length > max_len as u64) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, limit_error));
    }
    Ok(content_length
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(64 * 1024))
}

pub(crate) fn simple_response(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<String>,
) -> Response {
    let body = body.into();
    let len = body.len();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).expect("content length"),
    );
    response
}

pub(crate) fn is_hop_header(name: &str) -> bool {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

pub(crate) fn not_found() -> Response {
    simple_response(
        StatusCode::NOT_FOUND,
        "text/plain; charset=utf-8",
        "not found",
    )
}

pub(crate) fn request_failed_response(method: &str, uri: &Uri, error: &io::Error) -> Response {
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
        error_status(error.kind()),
        "text/plain; charset=utf-8",
        error.to_string(),
    )
}

fn error_status(kind: io::ErrorKind) -> StatusCode {
    if kind == io::ErrorKind::WouldBlock {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_GATEWAY
    }
}

fn meta_from_upstream(
    status: StatusCode,
    headers: &ReqwestHeaderMap,
    content_length: Option<usize>,
) -> StoredResponseMeta {
    let mut stored_headers = Vec::new();
    for (name, value) in headers {
        if is_hop_header(name.as_str()) {
            continue;
        }
        if content_length.is_some() && name.as_str() == CONTENT_LENGTH.as_str() {
            continue;
        }
        if let Ok(value) = value.to_str() {
            stored_headers.push((name.as_str().to_owned(), value.to_owned()));
        }
    }
    if let Some(content_length) = content_length {
        stored_headers.push((
            CONTENT_LENGTH.as_str().to_owned(),
            content_length.to_string(),
        ));
    }
    StoredResponseMeta {
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
    status: StatusCode,
    headers: &ReqwestHeaderMap,
    content_length: usize,
    expose_upstream_validators: bool,
) -> StoredResponseMeta {
    let mut meta = meta_from_upstream(status, headers, Some(content_length));
    if !expose_upstream_validators {
        strip_header(&mut meta.headers, reqwest::header::ETAG.as_str());
        strip_header(&mut meta.headers, reqwest::header::LAST_MODIFIED.as_str());
    }
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

fn strip_header(headers: &mut Vec<(String, String)>, name: &str) {
    headers.retain(|(header_name, _)| !header_name.eq_ignore_ascii_case(name));
}

async fn file_response(entry: StoredEntry) -> io::Result<Response> {
    let (mut file, body_len, meta) = entry.into_parts();
    file.seek(io::SeekFrom::Start(0)).await?;
    let reader = file.take(body_len);
    let mut response = Response::new(Body::from_stream(ReaderStream::new(reader)));
    *response.status_mut() = StatusCode::from_u16(meta.status).unwrap_or(StatusCode::OK);
    apply_headers(response.headers_mut(), &meta.headers);
    Ok(response)
}

fn bytes_response(meta: &StoredResponseMeta, body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(meta.status).unwrap_or(StatusCode::OK);
    apply_headers(response.headers_mut(), &meta.headers);
    response
}

fn response_without_body(response: Response) -> Response {
    let (parts, _) = response.into_parts();
    Response::from_parts(parts, Body::empty())
}

fn empty_response_from_meta(meta: &StoredResponseMeta) -> Response {
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

#[cfg(test)]
mod tests {
    use super::{
        App, MAX_ERROR_BODY_LEN, MAX_METADATA_BODY_LEN, MetadataRewrite, limited_initial_capacity,
        metadata_initial_capacity,
    };
    use crate::cache::{ArtifactLookup, CacheStore, Inflight, InflightOutcome};
    use crate::config::Config;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    fn config_for(path: &Path, max_cache_size: u64, max_upstream_fetches: usize) -> Config {
        Config {
            pkg_bind: "127.0.0.1:0".parse().unwrap(),
            git_bind: "127.0.0.1:0".parse().unwrap(),
            management_bind: "127.0.0.1:0".parse().unwrap(),
            public_base_url: "http://127.0.0.1:8080".to_owned(),
            cache_dir: PathBuf::from(path),
            max_cache_size,
            max_upstream_fetches,
            upstream_timeout: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn aborted_artifact_fetch_cleans_up_inflight() {
        let started = Arc::new(Notify::new());
        let upstream = slow_upstream(started.clone()).await.unwrap();
        let temp = tempdir().unwrap();
        let config = config_for(temp.path(), 16 * 1024 * 1024, 4);
        let app = App::new(config).await.unwrap();
        let upstream_url = reqwest::Url::parse(&format!("http://{upstream}/artifact")).unwrap();
        let key = crate::cache::CacheStore::artifact_key(upstream_url.as_str());
        let leader = match app
            .cache()
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
            app_task
                .run_artifact_fetch(upstream_task, "test_upstream", leader)
                .await;
        });
        started.notified().await;
        task.abort();
        let outcome = timeout(Duration::from_secs(2), inflight.wait_for_outcome())
            .await
            .expect("cleanup should resolve inflight")
            .expect("inflight wait should not error");
        match outcome {
            InflightOutcome::Failed(kind, error) => {
                assert_eq!(kind, io::ErrorKind::Interrupted);
                assert_eq!(error, "artifact fetch cancelled");
            }
            InflightOutcome::Cached(_) | InflightOutcome::Response(_, _) => {
                panic!("expected Failed outcome")
            }
        }
        match app.cache().lookup_or_start_artifact(key).await.unwrap() {
            ArtifactLookup::Leader(next) => {
                next.inflight
                    .fail(io::ErrorKind::Other, "test cleanup".to_owned())
                    .await;
                app.cache().finish_inflight(&next.key).await;
            }
            ArtifactLookup::Join(_) => panic!("stale inflight entry remained after abort"),
            ArtifactLookup::Hit(_) => panic!("unexpected cached artifact after abort"),
        }
    }

    #[tokio::test]
    async fn file_response_stops_at_footer() {
        use crate::cache::{CacheStore, StoredResponseMeta};
        let temp = tempdir().unwrap();
        let config = config_for(temp.path(), 16 * 1024 * 1024, 4);
        let store = CacheStore::new(&config).await.unwrap();
        let key = CacheStore::artifact_key("https://example.com/pkg.tar.gz");
        let paths = store.paths_for(&key);
        tokio::fs::create_dir_all(paths.temp.parent().unwrap())
            .await
            .unwrap();
        let body_bytes = b"important body bytes";
        tokio::fs::write(&paths.temp, body_bytes).await.unwrap();
        let meta = StoredResponseMeta {
            headers: vec![(
                "content-type".to_owned(),
                "application/octet-stream".to_owned(),
            )],
            last_modified: None,
            etag: None,
            status: 200,
        };
        store
            .commit_artifact(&key, &meta, &paths.temp)
            .await
            .unwrap();
        let entry = store.load(&key).await.unwrap().unwrap();
        assert_eq!(entry.body_len, body_bytes.len() as u64);
        let response = super::file_response(entry).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), body_bytes);
    }

    #[tokio::test]
    async fn oversized_artifact_is_served_before_eviction() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/artifact",
            get(|| async { (StatusCode::OK, Body::from("oversized artifact")) }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let temp = tempdir().unwrap();
        let app = App::new(config_for(temp.path(), 1, 1)).await.unwrap();
        let upstream = reqwest::Url::parse(&format!("http://{addr}/artifact")).unwrap();
        let key = CacheStore::artifact_key(upstream.as_str());
        let response = app.handle_artifact(upstream, "test").await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            "oversized artifact"
        );
        timeout(Duration::from_secs(1), async {
            while app.cache().load(&key).await.unwrap().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("oversized artifact should be evicted after handoff");
    }

    #[tokio::test]
    async fn concurrent_metadata_miss_has_one_leader() {
        let count = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/metadata",
            get({
                let count = count.clone();
                move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        (StatusCode::OK, [("content-type", "application/json")], "{}")
                    }
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let temp = tempdir().unwrap();
        let app = App::new(config_for(temp.path(), 1024 * 1024, 1))
            .await
            .unwrap();
        let upstream = reqwest::Url::parse(&format!("http://{addr}/metadata")).unwrap();
        let first = app.handle_metadata(upstream.clone(), "test", MetadataRewrite::None);
        let second = app.handle_metadata(upstream, "test", MetadataRewrite::None);
        let (first, second) = tokio::join!(first, second);
        for response in [first.unwrap(), second.unwrap()] {
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(to_bytes(response.into_body(), 1024).await.unwrap(), "{}");
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cached_metadata_head_conditionally_revalidates() {
        let count = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/metadata",
            get({
                let count = count.clone();
                move |headers: HeaderMap| {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        if headers.get("if-none-match").is_some() {
                            (StatusCode::NOT_MODIFIED, [("etag", "\"v1\"")], "").into_response()
                        } else {
                            (
                                StatusCode::OK,
                                [("content-type", "application/json"), ("etag", "\"v1\"")],
                                "{}",
                            )
                                .into_response()
                        }
                    }
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let temp = tempdir().unwrap();
        let app = App::new(config_for(temp.path(), 1024 * 1024, 1))
            .await
            .unwrap();
        let upstream = reqwest::Url::parse(&format!("http://{addr}/metadata")).unwrap();
        let first = app
            .handle_metadata(upstream.clone(), "test", MetadataRewrite::None)
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let head = app
            .handle_metadata_head(upstream, "test", MetadataRewrite::None)
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert!(to_bytes(head.into_body(), 1024).await.unwrap().is_empty());
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn metadata_content_length_over_limit_is_rejected_before_allocation() {
        let error = metadata_initial_capacity(Some(MAX_METADATA_BODY_LEN as u64 + 1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn artifact_error_content_length_is_bounded() {
        let error = limited_initial_capacity(
            Some(MAX_ERROR_BODY_LEN as u64 + 1),
            MAX_ERROR_BODY_LEN,
            "limit",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn admission_failure_maps_to_service_unavailable() {
        let error = io::Error::new(io::ErrorKind::WouldBlock, "upstream capacity exhausted");
        let response = super::request_failed_response("GET", &Uri::from_static("/a"), &error);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn joined_admission_failure_maps_to_service_unavailable() {
        let temp = tempdir().unwrap();
        let app = App::new(config_for(temp.path(), 1024 * 1024, 1))
            .await
            .unwrap();
        let inflight = Arc::new(Inflight::new());
        inflight
            .fail(
                io::ErrorKind::WouldBlock,
                "metadata memory capacity exhausted".to_owned(),
            )
            .await;
        let response = app.serve_metadata_inflight(inflight).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn rewrite_bound_covers_short_pypi_index_links() {
        let origin = format!("https://{}.example", "a".repeat(200));
        let body = "href='/simple/' ".repeat(100);
        let rewrite = MetadataRewrite::Pypi(origin.clone());
        let output = crate::routes::rewrite_pypi_html(
            body.as_bytes(),
            &crate::routes::RegistryOrigins::default(),
            &origin,
            MAX_METADATA_BODY_LEN,
        )
        .unwrap();
        assert!(output.len() <= rewrite.output_bound(body.len()));
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
