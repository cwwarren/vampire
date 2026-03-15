use crate::cache::{
    ArtifactLeader, ArtifactLookup, CacheStore, Inflight, InflightOutcome, StoredArtifact,
    StoredMetadata, StoredResponseMeta,
};
use crate::failure_log::log_failure;
use crate::routes::{rewrite_npm_json, rewrite_pypi_html};
use crate::state::App;
use axum::body::Body;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use bytes::Bytes;
use reqwest::header::HeaderMap as ReqwestHeaderMap;
use serde_json::json;
use std::io;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

#[derive(Clone)]
pub(crate) enum MetadataRewrite {
    None,
    Npm(String),
    Pypi(String),
}

enum FetchOutcome {
    Cached,
    NonOk(StoredResponseMeta, Bytes),
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
            inflight.fail("artifact fetch cancelled".to_owned()).await;
            app.cache().finish_inflight(&key).await;
        });
    }
}

impl App {
    pub(crate) async fn handle_artifact_head(
        &self,
        upstream: reqwest::Url,
    ) -> io::Result<Response> {
        let key = CacheStore::artifact_key(upstream.as_str());
        if let Some(entry) = self.cache().load_artifact(&key).await? {
            return Ok(empty_response_from_meta(&entry.meta));
        }
        let response = self
            .client()
            .head(upstream)
            .send()
            .await
            .map_err(io::Error::other)?;
        let meta = meta_from_upstream(response.status(), response.headers(), 0);
        Ok(empty_response_from_meta(&meta))
    }

    pub(crate) async fn handle_metadata_head(
        &self,
        upstream: reqwest::Url,
    ) -> io::Result<Response> {
        let key = CacheStore::metadata_key(upstream.as_str());
        if let Some(entry) = self.cache().load_metadata(&key).await? {
            return Ok(empty_response_from_meta(&entry.meta));
        }
        let response = self
            .client()
            .head(upstream)
            .send()
            .await
            .map_err(io::Error::other)?;
        let meta = meta_from_upstream(response.status(), response.headers(), 0);
        Ok(empty_response_from_meta(&meta))
    }

    pub(crate) async fn handle_metadata(
        &self,
        upstream: reqwest::Url,
        rewrite: MetadataRewrite,
    ) -> io::Result<Response> {
        let key = CacheStore::metadata_key(upstream.as_str());
        if let Some(entry) = self.cache().load_metadata(&key).await? {
            if entry.meta.etag.is_some() || entry.meta.last_modified.is_some() {
                return self
                    .revalidate_metadata(upstream, rewrite, key, entry)
                    .await;
            }
            return Ok(bytes_response(&entry.meta, entry.body));
        }
        self.fetch_metadata(upstream, rewrite, key).await
    }

    async fn revalidate_metadata(
        &self,
        upstream: reqwest::Url,
        rewrite: MetadataRewrite,
        key: String,
        entry: StoredMetadata,
    ) -> io::Result<Response> {
        let mut request = self.client().get(upstream.clone());
        self.app_stats().record_metadata_fetch(upstream.as_str());
        if let Some(etag) = &entry.meta.etag {
            request = request.header(IF_NONE_MATCH.as_str(), etag);
        }
        if let Some(last_modified) = &entry.meta.last_modified {
            request = request.header(IF_MODIFIED_SINCE.as_str(), last_modified);
        }
        let response = request.send().await.map_err(io::Error::other)?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(bytes_response(&entry.meta, entry.body));
        }
        self.finish_metadata(rewrite, key, response).await
    }

    async fn fetch_metadata(
        &self,
        upstream: reqwest::Url,
        rewrite: MetadataRewrite,
        key: String,
    ) -> io::Result<Response> {
        self.app_stats().record_metadata_fetch(upstream.as_str());
        let response = self
            .client()
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
                rewrite_npm_json(&body, self.upstreams(), &origin).map_err(io::Error::other)?
            }
            MetadataRewrite::Pypi(origin) => {
                rewrite_pypi_html(&body, self.upstreams(), &origin).map_err(io::Error::other)?
            }
        };
        let meta = meta_for_bytes(status, &upstream_headers, rewritten.len());
        if status == StatusCode::OK && (meta.etag.is_some() || meta.last_modified.is_some()) {
            let entry = self.cache().store_metadata(&key, &rewritten, &meta).await?;
            return Ok(bytes_response(&entry.meta, entry.body));
        }
        Ok(bytes_response(&meta, Bytes::from(rewritten)))
    }

    pub(crate) async fn handle_artifact(&self, upstream: reqwest::Url) -> io::Result<Response> {
        let key = CacheStore::artifact_key(upstream.as_str());
        match self.cache().lookup_or_start_artifact(key.clone()).await? {
            ArtifactLookup::Hit(entry) => file_response(entry).await,
            ArtifactLookup::Join(inflight) => {
                self.app_stats().record_artifact_join(upstream.as_str());
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

    async fn serve_inflight(&self, key: &str, inflight: Arc<Inflight>) -> io::Result<Response> {
        match inflight.wait_for_outcome().await? {
            InflightOutcome::Cached => {
                let entry = self.cache().load_artifact(key).await?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "artifact missing after inflight completion",
                    )
                })?;
                file_response(entry).await
            }
            InflightOutcome::Response(meta, body) => Ok(bytes_response(&meta, body)),
            InflightOutcome::Failed(error) => Ok(simple_response(
                StatusCode::BAD_GATEWAY,
                "text/plain; charset=utf-8",
                error,
            )),
        }
    }

    pub(crate) async fn run_artifact_fetch(&self, upstream: reqwest::Url, leader: ArtifactLeader) {
        let mut cleanup = ArtifactFetchCleanup {
            app: self.clone(),
            inflight: leader.inflight.clone(),
            key: leader.key.clone(),
            temp_path: leader.paths.temp.clone(),
            armed: true,
        };
        let result = self.do_artifact_fetch(&upstream, &leader).await;
        if result.is_err() {
            let _ = fs::remove_file(&leader.paths.temp).await;
        }
        match result {
            Ok(FetchOutcome::Cached) => leader.inflight.finish_cached().await,
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
                leader.inflight.fail(error).await;
            }
        }
        self.cache().finish_inflight(&leader.key).await;
        cleanup.disarm();
    }

    async fn do_artifact_fetch(
        &self,
        upstream: &reqwest::Url,
        leader: &ArtifactLeader,
    ) -> Result<FetchOutcome, (String, String)> {
        let _permit = self
            .cache()
            .acquire_upstream_permit()
            .await
            .map_err(|e| ("acquire_upstream_permit".into(), e.to_string()))?;
        self.app_stats().record_artifact_fetch(upstream.as_str());
        let response = self
            .client()
            .get(upstream.clone())
            .send()
            .await
            .map_err(|e| ("fetch_upstream".into(), io::Error::other(e).to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        if status != StatusCode::OK {
            let body = response.bytes().await.map_err(|e| {
                (
                    "read_error_response".into(),
                    io::Error::other(e).to_string(),
                )
            })?;
            let meta = meta_for_bytes(status, &headers, body.len());
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
        let meta = meta_from_upstream(status, &headers, content_length);
        self.cache()
            .commit_artifact(&leader.key, &meta, &leader.paths.temp)
            .await
            .map_err(|e| ("commit_cache_entry".into(), e.to_string()))?;
        Ok(FetchOutcome::Cached)
    }
}

pub(crate) async fn not_found() -> Response {
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
        StatusCode::BAD_GATEWAY,
        "text/plain; charset=utf-8",
        error.to_string(),
    )
}

pub(crate) fn request_origin(headers: &HeaderMap) -> String {
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
) -> StoredResponseMeta {
    let mut meta = meta_from_upstream(status, headers, content_length);
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

async fn file_response(entry: StoredArtifact) -> io::Result<Response> {
    let file = fs::File::open(entry.body_path).await?;
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    *response.status_mut() = StatusCode::from_u16(entry.meta.status).unwrap_or(StatusCode::OK);
    apply_headers(response.headers_mut(), &entry.meta.headers);
    Ok(response)
}

fn bytes_response(meta: &StoredResponseMeta, body: Bytes) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(meta.status).unwrap_or(StatusCode::OK);
    apply_headers(response.headers_mut(), &meta.headers);
    response
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

fn is_hop_header(name: &str) -> bool {
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

fn simple_response(
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

#[cfg(test)]
mod tests {
    use super::App;
    use crate::cache::{ArtifactLookup, InflightOutcome};
    use crate::config::Config;
    use crate::routes::RegistryOrigins;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
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
            app_task.run_artifact_fetch(upstream_task, leader).await;
        });
        started.notified().await;
        task.abort();
        let outcome = timeout(Duration::from_secs(2), inflight.wait_for_outcome())
            .await
            .expect("cleanup should resolve inflight")
            .expect("inflight wait should not error");
        match outcome {
            InflightOutcome::Failed(error) => {
                assert_eq!(error, "artifact fetch cancelled");
            }
            InflightOutcome::Cached | InflightOutcome::Response(_, _) => {
                panic!("expected Failed outcome")
            }
        }
        match app.cache().lookup_or_start_artifact(key).await.unwrap() {
            ArtifactLookup::Leader(next) => {
                next.inflight.fail("test cleanup".to_owned()).await;
                app.cache().finish_inflight(&next.key).await;
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
