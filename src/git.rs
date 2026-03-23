use crate::failure_log::log_failure;
use crate::proxy::{is_hop_header, not_found, request_failed_response, simple_response};
use crate::routes::{RegistryOrigins, join_url};
use crate::state::App;
use axum::body::{Body, to_bytes};
use axum::extract::{OriginalUri, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use bytes::Bytes;
use std::io;
use url::Url;

const MAX_GIT_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GithubGitRpc {
    InfoRefs,
    UploadPack,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GithubGitPath<'a> {
    owner: &'a str,
    repo: &'a str,
    rpc: GithubGitRpc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitPathError {
    Invalid,
    ReceivePack,
}

struct GitUpstreamRequest {
    method: reqwest::Method,
    upstream: Url,
}

enum ParsedGitRequest {
    NotGit,
    Rejected(StatusCode, &'static str),
    Forward(GitUpstreamRequest),
}

impl App {
    async fn handle_git(
        &self,
        method: reqwest::Method,
        upstream: Url,
        headers: &HeaderMap,
        body: Option<Bytes>,
    ) -> io::Result<Response> {
        let mut forwarded_headers = HeaderMap::new();
        for (name, value) in headers {
            if is_allowlisted_git_request_header(&method, name.as_str()) {
                forwarded_headers.insert(name, value.clone());
            }
        }
        let upstream_str = upstream.as_str().to_owned();
        let mut request = self.client().request(method, upstream).headers(forwarded_headers);
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.map_err(io::Error::other)?;
        self.app_stats().record_git_forward(&upstream_str);
        let status = response.status();
        let upstream_headers = response.headers().clone();
        let mut output = Response::new(Body::from_stream(response.bytes_stream()));
        *output.status_mut() = status;
        for (name, value) in &upstream_headers {
            if !is_hop_header(name.as_str()) {
                output.headers_mut().insert(name, value.clone());
            }
        }
        Ok(output)
    }
}

impl<'a> GithubGitPath<'a> {
    fn new(owner: &'a str, repo: &'a str, rpc: GithubGitRpc) -> Option<Self> {
        if !valid_repo_segment(owner) || !valid_repo_segment(repo) {
            return None;
        }
        Some(Self { owner, repo, rpc })
    }

    fn upstream_url(&self, upstreams: &RegistryOrigins, query: Option<&str>) -> Option<Url> {
        match self.rpc {
            GithubGitRpc::InfoRefs if query == Some("service=git-upload-pack") => join_url(
                &upstreams.github,
                &format!(
                    "{}/{repo}.git/info/refs?service=git-upload-pack",
                    self.owner,
                    repo = self.repo
                ),
            ),
            GithubGitRpc::UploadPack if query.is_none() => join_url(
                &upstreams.github,
                &format!(
                    "{}/{repo}.git/git-upload-pack",
                    self.owner,
                    repo = self.repo
                ),
            ),
            _ => None,
        }
    }

    fn resolve(
        &self,
        method: &Method,
        query: Option<&str>,
        upstreams: &RegistryOrigins,
    ) -> ParsedGitRequest {
        if *method == Method::GET {
            return match self.rpc {
                GithubGitRpc::InfoRefs => match query {
                    Some("service=git-upload-pack") => {
                        let Some(upstream) = self.upstream_url(upstreams, query) else {
                            return ParsedGitRequest::NotGit;
                        };
                        ParsedGitRequest::Forward(GitUpstreamRequest {
                            method: reqwest::Method::GET,
                            upstream,
                        })
                    }
                    Some("service=git-receive-pack") => ParsedGitRequest::Rejected(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "git-receive-pack is not supported",
                    ),
                    _ => ParsedGitRequest::Rejected(
                        StatusCode::BAD_REQUEST,
                        "info/refs requires ?service=git-upload-pack",
                    ),
                },
                GithubGitRpc::UploadPack => ParsedGitRequest::Rejected(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "git-upload-pack requires POST",
                ),
            };
        }

        if *method == Method::POST {
            return match self.rpc {
                GithubGitRpc::UploadPack => {
                    if query.is_some() {
                        return ParsedGitRequest::Rejected(
                            StatusCode::BAD_REQUEST,
                            "git-upload-pack does not accept query parameters",
                        );
                    }
                    let Some(upstream) = self.upstream_url(upstreams, None) else {
                        return ParsedGitRequest::NotGit;
                    };
                    ParsedGitRequest::Forward(GitUpstreamRequest {
                        method: reqwest::Method::POST,
                        upstream,
                    })
                }
                GithubGitRpc::InfoRefs => ParsedGitRequest::Rejected(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "info/refs requires GET",
                ),
            };
        }

        ParsedGitRequest::Rejected(StatusCode::METHOD_NOT_ALLOWED, "unsupported git method")
    }
}

impl GitUpstreamRequest {
    fn reads_body(&self) -> bool {
        self.method == reqwest::Method::POST
    }
}

impl ParsedGitRequest {
    fn from_request(method: &Method, uri: &Uri, upstreams: &RegistryOrigins) -> Self {
        if uri_contains_userinfo(uri) {
            return Self::Rejected(
                StatusCode::FORBIDDEN,
                "credential-bearing git requests are not supported",
            );
        }
        if *method == Method::CONNECT {
            return Self::Rejected(StatusCode::METHOD_NOT_ALLOWED, "CONNECT is not supported");
        }
        if uri.scheme().is_some() || uri.authority().is_some() {
            return Self::Rejected(
                StatusCode::BAD_REQUEST,
                "absolute-form git requests are not supported",
            );
        }

        let path = match parse_github_git_request_path(uri.path()) {
            Ok(Some(path)) => path,
            Ok(None) => return Self::NotGit,
            Err(GitPathError::ReceivePack) => {
                return Self::Rejected(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "git-receive-pack is not supported",
                )
            }
            Err(GitPathError::Invalid) => {
                return Self::Rejected(StatusCode::BAD_REQUEST, "invalid git path")
            }
        };

        path.resolve(method, uri.query(), upstreams)
    }
}

pub(crate) async fn request(
    State(app): State<App>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let request = match ParsedGitRequest::from_request(&method, &uri, app.upstreams()) {
        ParsedGitRequest::Forward(request) => request,
        ParsedGitRequest::NotGit => return not_found(),
        ParsedGitRequest::Rejected(status, message) => {
            log_failure(
                "git_rejected",
                serde_json::json!({
                    "method": method.as_str(),
                    "path": uri.path(),
                    "query": uri.query(),
                    "status": status.as_u16(),
                    "message": message,
                }),
            );
            return rejection(status, message);
        }
    };

    let body = if request.reads_body() {
        match to_bytes(body, MAX_GIT_REQUEST_BODY_BYTES).await {
            Ok(body) => Some(body),
            Err(error) => {
                log_failure(
                    "git_body_read_failed",
                    serde_json::json!({
                        "method": method.as_str(),
                        "path": uri.path(),
                        "error": error.to_string(),
                    }),
                );
                return simple_response(
                    StatusCode::BAD_REQUEST,
                    "text/plain; charset=utf-8",
                    error.to_string(),
                );
            }
        }
    } else {
        None
    };

    app.handle_git(request.method, request.upstream, &headers, body)
        .await
        .unwrap_or_else(|error| request_failed_response(method.as_str(), &uri, &error))
}

fn rejection(status: StatusCode, message: &'static str) -> Response {
    simple_response(status, "text/plain; charset=utf-8", message)
}

fn uri_contains_userinfo(uri: &Uri) -> bool {
    uri.authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
}

fn is_allowlisted_git_request_header(method: &reqwest::Method, name: &str) -> bool {
    match *method {
        reqwest::Method::GET => name.eq_ignore_ascii_case("git-protocol"),
        reqwest::Method::POST => {
            name.eq_ignore_ascii_case(CONTENT_TYPE.as_str())
                || name.eq_ignore_ascii_case("content-encoding")
                || name.eq_ignore_ascii_case("git-protocol")
        }
        _ => false,
    }
}

fn parse_github_git_request_path(path: &str) -> Result<Option<GithubGitPath<'_>>, GitPathError> {
    if !path.starts_with('/') || path.contains('\\') {
        return Err(GitPathError::Invalid);
    }
    if path == "/" {
        return Ok(None);
    }
    let raw = &path[1..];
    if raw.is_empty() || raw.contains("//") {
        return Err(GitPathError::Invalid);
    }

    let segments: Vec<&str> = raw.split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return Err(GitPathError::Invalid);
    }
    for segment in &segments {
        if segment.contains('%') {
            ensure_well_formed_percent_encoding(segment)?;
            return Err(GitPathError::Invalid);
        }
    }

    match segments.as_slice() {
        [owner, repo_segment, "info", "refs"] => {
            let Some(repo) = repo_segment.strip_suffix(".git") else {
                return Ok(None);
            };
            Ok(GithubGitPath::new(owner, repo, GithubGitRpc::InfoRefs))
        }
        [owner, repo_segment, "git-upload-pack"] => {
            let Some(repo) = repo_segment.strip_suffix(".git") else {
                return Ok(None);
            };
            Ok(GithubGitPath::new(owner, repo, GithubGitRpc::UploadPack))
        }
        [_, repo_segment, "git-receive-pack"] if repo_segment.strip_suffix(".git").is_some() => {
            Err(GitPathError::ReceivePack)
        }
        _ => Ok(None),
    }
}

fn ensure_well_formed_percent_encoding(segment: &str) -> Result<(), GitPathError> {
    let bytes = segment.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len()
            || !bytes[index + 1].is_ascii_hexdigit()
            || !bytes[index + 2].is_ascii_hexdigit()
        {
            return Err(GitPathError::Invalid);
        }
        index += 3;
    }
    Ok(())
}

fn valid_repo_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains('/')
        && !segment.contains('\\')
        && !segment.contains('%')
}

#[cfg(test)]
mod tests {
    use super::{GithubGitPath, GithubGitRpc, RegistryOrigins, parse_github_git_request_path};

    #[test]
    fn builds_git_upstream_urls() {
        let upstreams = RegistryOrigins::default();
        assert!(
            GithubGitPath::new("rust-lang", "cargo", GithubGitRpc::InfoRefs)
                .and_then(|path| path.upstream_url(&upstreams, Some("service=git-upload-pack")))
                .is_some()
        );
        assert!(
            GithubGitPath::new("rust-lang", "cargo", GithubGitRpc::UploadPack)
                .and_then(|path| path.upstream_url(&upstreams, None))
                .is_some()
        );
    }

    #[test]
    fn rejects_invalid_github_git_shapes() {
        let upstreams = RegistryOrigins::default();
        assert!(
            GithubGitPath::new("rust-lang", "cargo", GithubGitRpc::InfoRefs)
                .and_then(|path| path.upstream_url(&upstreams, None))
                .is_none()
        );
        assert!(
            GithubGitPath::new("rust-lang", "cargo", GithubGitRpc::InfoRefs)
                .and_then(|path| path.upstream_url(&upstreams, Some("service=git-receive-pack")))
                .is_none()
        );
        assert!(GithubGitPath::new("..", "cargo", GithubGitRpc::UploadPack).is_none());
        assert!(GithubGitPath::new("rust-lang", "a/b", GithubGitRpc::UploadPack).is_none());
    }

    #[test]
    fn rejects_noncanonical_git_paths() {
        for path in [
            "//rust-lang/cargo.git/info/refs",
            "/rust-lang/./cargo.git/info/refs",
            "/rust-lang/cargo.git/./git-upload-pack",
            "/rust-lang/%63argo.git/info/refs",
            "/rust-lang/cargo%2fgit/info/refs",
            "/rust-lang/cargo.git/info/%72efs",
            "/rust-lang/cargo.git/info/%zz",
        ] {
            assert!(
                parse_github_git_request_path(path).is_err(),
                "path should be rejected: {path}"
            );
        }
    }

    #[test]
    fn leaves_plain_non_git_paths_unmatched() {
        assert_eq!(parse_github_git_request_path("/login").unwrap(), None);
        assert_eq!(
            parse_github_git_request_path("/rust-lang/cargo").unwrap(),
            None
        );
    }
}
