use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use bytes::Bytes;
use reqwest::Client;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::Duration;
use vampire::{App, Config};

pub(super) struct TestFixture {
    _temp_dir: TempDir,
    pub(super) git_bind: SocketAddr,
    pub(super) pkg_base_url: String,
    pub(super) git_base_url: String,
    pub(super) management_base_url: String,
    pub(super) public_base_url: String,
    pub(super) client: Client,
}

impl TestFixture {
    pub(super) async fn new() -> io::Result<Self> {
        Self::with_servers(Upstream::new().await?).await
    }

    pub(super) async fn with_servers(upstream: Upstream) -> io::Result<Self> {
        Self::with_servers_and_limits(upstream, 32 * 1024 * 1024, 8).await
    }

    pub(super) async fn with_servers_and_limits(
        upstream: Upstream,
        max_cache_size: u64,
        max_upstream_fetches: usize,
    ) -> io::Result<Self> {
        let pkg_listener = TcpListener::bind("127.0.0.1:0").await?;
        let public_base_url = format!("http://{}", pkg_listener.local_addr()?);
        Self::build(
            upstream,
            pkg_listener,
            public_base_url,
            max_cache_size,
            max_upstream_fetches,
        )
        .await
    }

    pub(super) async fn with_servers_and_public_base_url(
        upstream: Upstream,
        public_base_url: String,
    ) -> io::Result<Self> {
        let pkg_listener = TcpListener::bind("127.0.0.1:0").await?;
        Self::build(upstream, pkg_listener, public_base_url, 32 * 1024 * 1024, 8).await
    }

    async fn build(
        upstream: Upstream,
        pkg_listener: TcpListener,
        public_base_url: String,
        max_cache_size: u64,
        max_upstream_fetches: usize,
    ) -> io::Result<Self> {
        let temp_dir = tempfile::tempdir()?;
        let pkg_bind = pkg_listener.local_addr()?;
        let git_listener = TcpListener::bind("127.0.0.1:0").await?;
        let management_listener = TcpListener::bind("127.0.0.1:0").await?;
        let git_bind = git_listener.local_addr()?;
        let management_bind = management_listener.local_addr()?;
        let config = Config {
            pkg_bind,
            git_bind,
            management_bind,
            public_base_url: public_base_url.clone(),
            cache_dir: PathBuf::from(temp_dir.path()),
            max_cache_size,
            max_upstream_fetches,
            upstream_timeout: std::time::Duration::from_secs(5),
        };
        let app = App::new_with_loopback_upstream(config.clone(), upstream.addr).await?;
        let client = Client::new();
        tokio::spawn(async move {
            let _ = app
                .serve(pkg_listener, git_listener, management_listener)
                .await;
        });
        Ok(Self {
            _temp_dir: temp_dir,
            git_bind: config.git_bind,
            pkg_base_url: format!("http://{}", config.pkg_bind),
            git_base_url: format!("http://{}", config.git_bind),
            management_base_url: format!("http://{}", config.management_bind),
            public_base_url,
            client,
        })
    }
}

#[derive(Clone)]
pub(super) struct Upstream {
    addr: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Vec<UpstreamResponse>>>>,
    counts: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Upstream {
    pub(super) async fn new() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let upstream = Self {
            addr,
            routes: Arc::new(Mutex::new(HashMap::new())),
            counts: Arc::new(Mutex::new(HashMap::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let router = Router::new()
            .fallback(any(upstream_handle))
            .with_state(upstream.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(upstream)
    }

    pub(super) async fn insert(&self, path: &str, response: UpstreamResponse) {
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

    pub(super) async fn request_count(&self, path: &str) -> usize {
        self.counts
            .lock()
            .await
            .get(path)
            .map_or(0, |value| value.load(Ordering::SeqCst))
    }

    pub(super) async fn recorded_requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().await.clone()
    }
}

async fn upstream_handle(
    State(upstream): State<Upstream>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = match to_bytes(body, 8 * 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => return text_response(500, &error.to_string()),
    };
    let path = uri.path().to_owned();
    if let Some(counter) = upstream.counts.lock().await.get(&path).cloned() {
        counter.fetch_add(1, Ordering::SeqCst);
    }
    upstream.requests.lock().await.push(RecordedRequest {
        method: method.to_string(),
        path: path.clone(),
        query: uri.query().map(str::to_owned),
        headers: headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect(),
        body: body.to_vec(),
    });
    let mut routes = upstream.routes.lock().await;
    let Some(queue) = routes.get_mut(&path) else {
        return text_response(404, "missing");
    };
    let mut response = queue
        .first()
        .cloned()
        .unwrap_or_else(|| UpstreamResponse::empty(404));
    if queue.len() > 1 {
        response = queue.remove(0);
    }
    if let Some(expected) = &response.if_none_match {
        let actual = headers
            .get("if-none-match")
            .and_then(|value| value.to_str().ok());
        if actual != Some(expected.as_str()) {
            response = UpstreamResponse::empty(412);
        }
    }
    response.into_response()
}

#[derive(Clone, Debug)]
pub(super) struct RecordedRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) query: Option<String>,
    headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
}

impl RecordedRequest {
    pub(super) fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }
}

#[derive(Clone)]
pub(super) struct UpstreamResponse {
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
    pub(super) fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
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

    pub(super) fn text(status: u16, content_type: &str, body: &str) -> Self {
        Self::bytes(status, content_type, body.as_bytes().to_vec())
    }

    pub(super) fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::bytes(
            status,
            "application/json",
            serde_json::to_vec(body).unwrap(),
        )
    }

    pub(super) fn empty(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: UpstreamBody::Full(Vec::new()),
            if_none_match: None,
        }
    }

    pub(super) fn slow_bytes(
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

    pub(super) fn slow_json(status: u16, body: &serde_json::Value, pause: Duration) -> Self {
        let bytes = serde_json::to_vec(body).unwrap();
        let midpoint = bytes.len() / 2;
        Self::slow_bytes(
            status,
            "application/json",
            bytes[..midpoint].to_vec(),
            bytes[midpoint..].to_vec(),
            pause,
        )
    }

    pub(super) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub(super) fn with_if_none_match(mut self, value: &str) -> Self {
        self.if_none_match = Some(value.to_owned());
        self
    }

    fn into_response(self) -> Response {
        let mut response = Response::new(match self.body {
            UpstreamBody::Full(body) => Body::from(body),
            UpstreamBody::Slow {
                first,
                second,
                pause,
            } => {
                let stream = async_stream::stream! {
                    yield Ok::<Bytes, io::Error>(Bytes::from(first));
                    tokio::time::sleep(pause).await;
                    yield Ok::<Bytes, io::Error>(Bytes::from(second));
                };
                Body::from_stream(stream)
            }
        });
        *response.status_mut() = StatusCode::from_u16(self.status).unwrap();
        let headers = response.headers_mut();
        for (name, value) in self.headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(&value).unwrap(),
            );
        }
        response
    }
}

#[derive(Debug)]
pub(super) struct RawHttpResponse {
    pub(super) status: StatusCode,
    pub(super) body: String,
}

pub(super) async fn raw_http_request(
    addr: SocketAddr,
    request: &str,
) -> io::Result<RawHttpResponse> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream.write_all(request.as_bytes()).await?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    parse_raw_http_response(&bytes)
}

fn parse_raw_http_response(bytes: &[u8]) -> io::Result<RawHttpResponse> {
    let text = String::from_utf8_lossy(bytes);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| io::Error::other(format!("invalid raw response: {text}")))?;
    let status_line = head
        .lines()
        .next()
        .ok_or_else(|| io::Error::other("missing status line"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::other(format!("missing status code in {status_line:?}")))?
        .parse::<u16>()
        .map_err(|error| io::Error::other(format!("invalid status code: {error}")))?;
    Ok(RawHttpResponse {
        status: StatusCode::from_u16(status).map_err(io::Error::other)?,
        body: body.to_owned(),
    })
}

pub(super) fn header_value(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
}

fn text_response(status: u16, body: &str) -> Response {
    let mut response = Response::new(Body::from(body.to_owned()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    response
}
