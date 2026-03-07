use crate::cache::{
    ArtifactLeader, ArtifactLookup, CacheStore, InflightOutcome, StoredEntry, StoredResponseMeta,
};
use crate::config::Config;
use crate::failure_log::log_failure;
use crate::routes::{
    CacheClass, RegistryOrigins, Route, cargo_config, rewrite_metadata, route_request,
};
use crate::stats::AppStats;
use bytes::Bytes;
use futures_util::StreamExt;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use reqwest::Client;
use reqwest::header::HeaderMap as ReqwestHeaderMap;
use serde_json::json;
use std::convert::Infallible;
use std::io;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

type ResponseBody = UnsyncBoxBody<Bytes, io::Error>;

#[derive(Clone)]
pub struct App {
    cache: CacheStore,
    client: Client,
    stats: AppStats,
    upstreams: RegistryOrigins,
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
        loop {
            let (stream, _) = listener.accept().await?;
            let app = self.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let app = app.clone();
                    async move { Ok::<_, Infallible>(app.handle(request).await) }
                });
                let io = TokioIo::new(stream);
                let _ = ServerBuilder::new(TokioExecutor::new())
                    .serve_connection(io, service)
                    .await;
            });
        }
    }

    pub async fn handle(&self, request: Request<Incoming>) -> Response<ResponseBody> {
        let method = request.method().as_str().to_owned();
        let path = request.uri().path().to_owned();
        let query = request.uri().query().map(str::to_owned);
        match self.try_handle(request).await {
            Ok(response) => response,
            Err(error) => {
                log_failure(
                    "request_failed",
                    json!({
                        "method": method,
                        "path": path,
                        "query": query,
                        "error": error.to_string(),
                    }),
                );
                simple_response(
                    StatusCode::BAD_GATEWAY,
                    "text/plain; charset=utf-8",
                    error.to_string(),
                )
            }
        }
    }

    async fn try_handle(&self, request: Request<Incoming>) -> io::Result<Response<ResponseBody>> {
        match *request.method() {
            Method::GET | Method::HEAD => {}
            _ => {
                return Ok(simple_response(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "text/plain; charset=utf-8",
                    "method not allowed",
                ));
            }
        }
        let origin = request_origin(&request);
        let route = match route_request(
            request.uri().path(),
            request.uri().query(),
            origin,
            &self.upstreams,
        ) {
            Some(route) => route,
            None => {
                return Ok(simple_response(
                    StatusCode::NOT_FOUND,
                    "text/plain; charset=utf-8",
                    "not found",
                ));
            }
        };
        if request.method() == Method::HEAD {
            return self.handle_head(route).await;
        }
        match route.cache_class() {
            None => Ok(json_response(
                StatusCode::OK,
                cargo_config_from_route(&route),
            )),
            Some(CacheClass::Artifact) => self.handle_artifact(route).await,
            Some(CacheClass::Metadata) => self.handle_metadata(route).await,
        }
    }

    async fn handle_head(&self, route: Route) -> io::Result<Response<ResponseBody>> {
        if let Some(upstream) = route.upstream() {
            let key = CacheStore::key_for(route.cache_class().unwrap(), upstream.as_str(), "");
            if let Some(entry) = self.cache.load(&key).await? {
                return Ok(empty_response_from_meta(entry.meta));
            }
            let response = self
                .client
                .head(upstream.clone())
                .send()
                .await
                .map_err(io::Error::other)?;
            let meta = meta_from_upstream(
                route.cache_class().unwrap(),
                response.status(),
                response.headers(),
                0,
            );
            return Ok(empty_response_from_meta(meta));
        }
        Ok(empty_response(StatusCode::OK))
    }

    async fn handle_metadata(&self, route: Route) -> io::Result<Response<ResponseBody>> {
        let upstream = route
            .upstream()
            .expect("metadata route always has upstream");
        let key = CacheStore::key_for(CacheClass::Metadata, upstream.as_str(), "");
        let _guard = self.cache.lock_metadata(&key).await;
        if let Some(entry) = self.cache.load(&key).await? {
            if entry.meta.etag.is_some() || entry.meta.last_modified.is_some() {
                return self.revalidate_metadata(route, key, entry).await;
            }
            return file_response(entry, false).await;
        }
        self.fetch_metadata(route, key).await
    }

    async fn revalidate_metadata(
        &self,
        route: Route,
        key: String,
        entry: StoredEntry,
    ) -> io::Result<Response<ResponseBody>> {
        let upstream = route
            .upstream()
            .expect("metadata route always has upstream");
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
            return file_response(entry, false).await;
        }
        self.finish_metadata(route, key, response).await
    }

    async fn fetch_metadata(
        &self,
        route: Route,
        key: String,
    ) -> io::Result<Response<ResponseBody>> {
        let upstream = route
            .upstream()
            .expect("metadata route always has upstream");
        self.stats
            .record_fetch(CacheClass::Metadata, upstream.as_str());
        let response = self
            .client
            .get(upstream.clone())
            .send()
            .await
            .map_err(io::Error::other)?;
        self.finish_metadata(route, key, response).await
    }

    async fn finish_metadata(
        &self,
        route: Route,
        key: String,
        response: reqwest::Response,
    ) -> io::Result<Response<ResponseBody>> {
        let status = response.status();
        let upstream_headers = response.headers().clone();
        let body = response.bytes().await.map_err(io::Error::other)?;
        let rewritten =
            rewrite_metadata(&route, &body, &self.upstreams).map_err(io::Error::other)?;
        let meta = meta_for_bytes(
            CacheClass::Metadata,
            status,
            &upstream_headers,
            rewritten.len(),
        );
        if status == StatusCode::OK && (meta.etag.is_some() || meta.last_modified.is_some()) {
            let entry = self.cache.store_metadata(&key, &rewritten, &meta).await?;
            return file_response(entry, false).await;
        }
        Ok(bytes_response(meta, Bytes::from(rewritten)))
    }

    async fn handle_artifact(&self, route: Route) -> io::Result<Response<ResponseBody>> {
        let upstream = route
            .upstream()
            .expect("artifact route always has upstream");
        let key = CacheStore::key_for(CacheClass::Artifact, upstream.as_str(), "");
        match self.cache.lookup_or_start_artifact(key.clone()).await? {
            ArtifactLookup::Hit(entry) => file_response(entry, false).await,
            ArtifactLookup::Join(inflight) => {
                self.stats.record_artifact_join(upstream.as_str());
                self.serve_inflight(&key, inflight).await
            }
            ArtifactLookup::Leader(leader) => self.fetch_artifact(route, leader).await,
        }
    }

    async fn serve_inflight(
        &self,
        key: &str,
        inflight: Arc<crate::cache::Inflight>,
    ) -> io::Result<Response<ResponseBody>> {
        match inflight.wait_for_outcome().await? {
            InflightOutcome::Cached => {
                let entry = self.cache.load(key).await?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "artifact missing after inflight completion",
                    )
                })?;
                file_response(entry, false).await
            }
            InflightOutcome::Response(meta, body) => Ok(bytes_response(meta, body)),
        }
    }

    async fn fetch_artifact(
        &self,
        route: Route,
        leader: ArtifactLeader,
    ) -> io::Result<Response<ResponseBody>> {
        let upstream = route
            .upstream()
            .expect("artifact route always has upstream")
            .clone();
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
        let _permit = match self.cache.acquire_upstream_permit().await {
            Ok(permit) => permit,
            Err(error) => {
                fail("acquire_upstream_permit", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                return Err(error);
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
                return Err(error);
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
                    return Err(error);
                }
            };
            let meta = meta_for_bytes(CacheClass::Artifact, status, &headers, body.len());
            leader
                .inflight
                .finish_response(meta.clone(), body.clone())
                .await;
            self.cache.finish_inflight(&leader.key).await;
            return Ok(bytes_response(meta, body));
        }
        if let Some(parent) = leader.paths.temp.parent() {
            if let Err(error) = fs::create_dir_all(parent).await {
                fail("create_temp_dir", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                return Err(error);
            }
        }
        let mut file = match fs::File::create(&leader.paths.temp).await {
            Ok(file) => file,
            Err(error) => {
                fail("create_temp_file", &error.to_string());
                leader.inflight.fail(error.to_string()).await;
                self.cache.finish_inflight(&leader.key).await;
                return Err(error);
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
                    return Err(error);
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
                return Err(error);
            }
            content_length += chunk.len();
        }
        if let Err(error) = file.flush().await {
            drop(file);
            let _ = fs::remove_file(&leader.paths.temp).await;
            fail("flush_temp_file", &error.to_string());
            leader.inflight.fail(error.to_string()).await;
            self.cache.finish_inflight(&leader.key).await;
            return Err(error);
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
                return Err(error);
            }
        };
        leader.inflight.finish_cached().await;
        self.cache.finish_inflight(&leader.key).await;
        file_response(entry, false).await
    }
}

fn cargo_config_from_route(route: &Route) -> Vec<u8> {
    match route {
        Route::CargoConfig { origin } => cargo_config(origin),
        _ => Vec::new(),
    }
}

fn request_origin(request: &Request<Incoming>) -> String {
    let scheme = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    let host = request
        .headers()
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

async fn file_response(entry: StoredEntry, empty: bool) -> io::Result<Response<ResponseBody>> {
    let mut response = Response::builder()
        .status(StatusCode::from_u16(entry.meta.status).unwrap_or(StatusCode::OK));
    apply_headers(
        response.headers_mut().expect("response headers"),
        &entry.meta.headers,
    );
    if empty {
        return Ok(response.body(empty_body()).expect("empty response"));
    }
    let file = fs::File::open(entry.body_path).await?;
    let stream = tokio_util::io::ReaderStream::new(file).map(|chunk| match chunk {
        Ok(chunk) => Ok(Frame::data(chunk)),
        Err(error) => Err(error),
    });
    Ok(response
        .body(StreamBody::new(stream).boxed_unsync())
        .expect("file response"))
}

fn bytes_response(meta: StoredResponseMeta, body: Bytes) -> Response<ResponseBody> {
    let mut response =
        Response::builder().status(StatusCode::from_u16(meta.status).unwrap_or(StatusCode::OK));
    apply_headers(
        response.headers_mut().expect("response headers"),
        &meta.headers,
    );
    response
        .body(Full::new(body).map_err(never_to_io).boxed_unsync())
        .expect("bytes response")
}

fn empty_response_from_meta(meta: StoredResponseMeta) -> Response<ResponseBody> {
    let mut response =
        Response::builder().status(StatusCode::from_u16(meta.status).unwrap_or(StatusCode::OK));
    apply_headers(
        response.headers_mut().expect("response headers"),
        &meta.headers,
    );
    response.body(empty_body()).expect("head response")
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
) -> Response<ResponseBody> {
    let body = body.into();
    let mut response = Response::builder().status(status);
    let headers = response.headers_mut().expect("response headers");
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).expect("content length"),
    );
    response
        .body(
            Full::new(Bytes::from(body))
                .map_err(never_to_io)
                .boxed_unsync(),
        )
        .expect("simple response")
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response<ResponseBody> {
    let mut response = Response::builder().status(status);
    let headers = response.headers_mut().expect("response headers");
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).expect("content length"),
    );
    response
        .body(
            Full::new(Bytes::from(body))
                .map_err(never_to_io)
                .boxed_unsync(),
        )
        .expect("json response")
}

fn empty_response(status: StatusCode) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .body(empty_body())
        .expect("empty response")
}

fn empty_body() -> ResponseBody {
    Full::new(Bytes::new()).map_err(never_to_io).boxed_unsync()
}

fn never_to_io(never: Infallible) -> io::Error {
    match never {}
}
