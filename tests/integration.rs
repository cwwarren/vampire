use futures_util::future::join_all;
use http::StatusCode;
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use vampire::routes::RegistryOrigins;
use vampire::{App, Config};

type TestBody = UnsyncBoxBody<bytes::Bytes, io::Error>;

#[tokio::test]
async fn rejects_unknown_routes() {
    let fixture = TestFixture::new().await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/nope", fixture.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rewrites_pypi_links() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/simple/pkg/",
            UpstreamResponse::text(
                200,
                "text/html",
                r#"<a href="https://files.pythonhosted.org/packages/pkg.whl#sha256=abc">pkg</a>"#,
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/pypi/simple/pkg/", fixture.base_url))
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(body.contains(&format!("{}/pypi/files/pkg.whl?u=", fixture.base_url)));
}

#[tokio::test]
async fn caches_artifacts_and_dedupes_misses() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/crates/demo/demo-1.0.0.crate",
            UpstreamResponse::bytes(200, "application/octet-stream", vec![b'x'; 128 * 1024]),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let url = format!(
        "{}/cargo/api/v1/crates/demo/1.0.0/download",
        fixture.base_url
    );
    let responses = join_all((0..16).map(|_| fixture.client.get(&url).send())).await;
    for (index, response) in responses.into_iter().enumerate() {
        let response = response.unwrap();
        let status = response.status();
        let body = response.bytes().await.unwrap();
        assert_eq!(
            body.len(),
            128 * 1024,
            "response {index} status={status} body={}",
            String::from_utf8_lossy(&body)
        );
    }
    assert_eq!(
        upstream
            .request_count("/crates/demo/demo-1.0.0.crate")
            .await,
        1
    );
    let third = fixture.client.get(&url).send().await.unwrap();
    assert_eq!(third.bytes().await.unwrap().len(), 128 * 1024);
    assert_eq!(
        upstream
            .request_count("/crates/demo/demo-1.0.0.crate")
            .await,
        1
    );
}

#[tokio::test]
async fn cold_artifact_waits_for_complete_fetch() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/crates/slow/slow-1.0.0.crate",
            UpstreamResponse::slow_bytes(
                200,
                "application/octet-stream",
                vec![b'a'; 64 * 1024],
                vec![b'b'; 64 * 1024],
                Duration::from_millis(250),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let url = format!(
        "{}/cargo/api/v1/crates/slow/1.0.0/download",
        fixture.base_url
    );
    let start = Instant::now();
    let response = fixture.client.get(&url).send().await.unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(200),
        "artifact response started before upstream fetch completed: {:?}",
        start.elapsed()
    );
    let body = response.bytes().await.unwrap();
    assert_eq!(body.len(), 128 * 1024);
    assert_eq!(
        upstream
            .request_count("/crates/slow/slow-1.0.0.crate")
            .await,
        1
    );
}

#[tokio::test]
async fn revalidates_metadata() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/pkg",
            UpstreamResponse::json(
                200,
                json!({
                    "versions": {
                        "1.0.0": {
                            "dist": { "tarball": "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz" }
                        }
                    }
                }),
            )
            .with_header("etag", "\"v1\""),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let url = format!("{}/npm/pkg", fixture.base_url);
    let first = fixture.client.get(&url).send().await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    upstream
        .insert(
            "/pkg",
            UpstreamResponse::empty(304).with_if_none_match("\"v1\""),
        )
        .await;
    let second = fixture.client.get(&url).send().await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(upstream.request_count("/pkg").await, 2);
}

#[tokio::test]
async fn serves_cargo_config() {
    let fixture = TestFixture::new().await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/cargo/index/config.json", fixture.base_url))
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(body.contains("/cargo/api/v1/crates"));
}

struct TestFixture {
    _temp_dir: TempDir,
    base_url: String,
    client: Client,
}

impl TestFixture {
    async fn new() -> io::Result<Self> {
        Self::with_servers(Upstream::new().await?).await
    }

    async fn with_servers(upstream: Upstream) -> io::Result<Self> {
        let temp_dir = tempfile::tempdir()?;
        let bind = listen_addr().await?;
        let config = Config {
            bind,
            cache_dir: PathBuf::from(temp_dir.path()),
            max_cache_size: 32 * 1024 * 1024,
            max_upstream_fetches: 8,
            upstream_timeout: std::time::Duration::from_secs(5),
        };
        let client = Client::builder()
            .resolve("pypi.org", upstream.addr)
            .resolve("files.pythonhosted.org", upstream.addr)
            .resolve("registry.npmjs.org", upstream.addr)
            .resolve("index.crates.io", upstream.addr)
            .resolve("static.crates.io", upstream.addr)
            .build()
            .map_err(io::Error::other)?;
        let upstreams = RegistryOrigins {
            cargo_download: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            cargo_index: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            npm: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            pypi_files: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            pypi_simple: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
        };
        let app = App::new_with_upstreams(config.clone(), client.clone(), upstreams).await?;
        let listener = TcpListener::bind(bind).await?;
        tokio::spawn(async move {
            let _ = app.serve(listener).await;
        });
        Ok(Self {
            _temp_dir: temp_dir,
            base_url: format!("http://{}", config.bind),
            client,
        })
    }
}

#[derive(Clone)]
struct Upstream {
    addr: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Vec<UpstreamResponse>>>>,
    counts: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
}

impl Upstream {
    async fn new() -> io::Result<Self> {
        let listener = TcpListener::bind(listen_addr().await?).await?;
        let addr = listener.local_addr()?;
        let upstream = Self {
            addr,
            routes: Arc::new(Mutex::new(HashMap::new())),
            counts: Arc::new(Mutex::new(HashMap::new())),
        };
        let state = upstream.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(state.handle(request).await) }
                    });
                    let io = TokioIo::new(stream);
                    let _ = ServerBuilder::new(TokioExecutor::new())
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        Ok(upstream)
    }

    async fn insert(&self, path: &str, response: UpstreamResponse) {
        self.routes
            .lock()
            .await
            .entry(path.to_owned())
            .or_default()
            .push(response);
        self.counts
            .lock()
            .await
            .entry(path.to_owned())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)));
    }

    async fn request_count(&self, path: &str) -> usize {
        self.counts
            .lock()
            .await
            .get(path)
            .map(|value| value.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    async fn handle(&self, request: Request<Incoming>) -> Response<TestBody> {
        let path = request.uri().path().to_owned();
        if let Some(counter) = self.counts.lock().await.get(&path).cloned() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        let mut routes = self.routes.lock().await;
        let Some(queue) = routes.get_mut(&path) else {
            return Response::builder()
                .status(404)
                .body(
                    Full::new(bytes::Bytes::from_static(b"missing"))
                        .map_err(never)
                        .boxed_unsync(),
                )
                .unwrap();
        };
        let mut response = queue
            .first()
            .cloned()
            .unwrap_or_else(|| UpstreamResponse::empty(404));
        if queue.len() > 1 {
            response = queue.remove(0);
        }
        if let Some(expected) = &response.if_none_match {
            let actual = request
                .headers()
                .get("if-none-match")
                .and_then(|value| value.to_str().ok());
            if actual != Some(expected.as_str()) {
                response = UpstreamResponse::empty(412);
            }
        }
        response.into_response()
    }
}

#[derive(Clone)]
struct UpstreamResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: UpstreamBody,
    if_none_match: Option<String>,
}

#[derive(Clone)]
enum UpstreamBody {
    Full(Vec<u8>),
    Slow {
        first: Vec<u8>,
        second: Vec<u8>,
        pause: Duration,
    },
}

impl UpstreamResponse {
    fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![
                ("content-type".to_owned(), content_type.to_owned()),
                ("content-length".to_owned(), body.len().to_string()),
            ],
            body: UpstreamBody::Full(body),
            if_none_match: None,
        }
    }

    fn text(status: u16, content_type: &str, body: &str) -> Self {
        Self::bytes(status, content_type, body.as_bytes().to_vec())
    }

    fn json(status: u16, body: serde_json::Value) -> Self {
        Self::bytes(
            status,
            "application/json",
            serde_json::to_vec(&body).unwrap(),
        )
    }

    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: UpstreamBody::Full(Vec::new()),
            if_none_match: None,
        }
    }

    fn slow_bytes(
        status: u16,
        content_type: &str,
        first: Vec<u8>,
        second: Vec<u8>,
        pause: Duration,
    ) -> Self {
        Self {
            status,
            headers: vec![
                ("content-type".to_owned(), content_type.to_owned()),
                (
                    "content-length".to_owned(),
                    (first.len() + second.len()).to_string(),
                ),
            ],
            body: UpstreamBody::Slow {
                first,
                second,
                pause,
            },
            if_none_match: None,
        }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn with_if_none_match(mut self, value: &str) -> Self {
        self.if_none_match = Some(value.to_owned());
        self
    }

    fn into_response(self) -> Response<TestBody> {
        let mut response = Response::builder().status(self.status);
        let headers = response.headers_mut().unwrap();
        for (name, value) in self.headers {
            headers.insert(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::header::HeaderValue::from_str(&value).unwrap(),
            );
        }
        match self.body {
            UpstreamBody::Full(body) => response
                .body(
                    Full::new(bytes::Bytes::from(body))
                        .map_err(never)
                        .boxed_unsync(),
                )
                .unwrap(),
            UpstreamBody::Slow {
                first,
                second,
                pause,
            } => {
                let stream = async_stream::try_stream! {
                    yield Frame::data(bytes::Bytes::from(first));
                    tokio::time::sleep(pause).await;
                    yield Frame::data(bytes::Bytes::from(second));
                };
                response
                    .body(StreamBody::new(stream).boxed_unsync())
                    .unwrap()
            }
        }
    }
}

async fn listen_addr() -> io::Result<SocketAddr> {
    Ok(TcpListener::bind("127.0.0.1:0").await?.local_addr()?)
}

fn never(never: std::convert::Infallible) -> io::Error {
    match never {}
}
