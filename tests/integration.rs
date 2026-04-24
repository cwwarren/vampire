use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use bytes::Bytes;
use futures_util::future::join_all;
use reqwest::Client;
use serde_json::json;
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
use tokio::time::{Duration, Instant};
use vampire::routes::RegistryOrigins;
use vampire::{App, Config};

#[tokio::test]
async fn rejects_unknown_routes() {
    let fixture = TestFixture::new().await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/nope", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn serves_prometheus_stats_on_dedicated_management_listener() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/simple/pkg/",
            UpstreamResponse::text(200, "text/html", r#"<a href="pkg-1.0.0.tar.gz">pkg</a>"#),
        )
        .await;
    upstream
        .insert(
            "/crates/demo/demo-1.0.0.crate",
            UpstreamResponse::slow_bytes(
                200,
                "application/octet-stream",
                vec![b'a'; 16 * 1024],
                vec![b'b'; 16 * 1024],
                Duration::from_millis(200),
            ),
        )
        .await;
    upstream
        .insert(
            "/rust-lang/cargo.git/info/refs",
            UpstreamResponse::text(200, "application/x-git-upload-pack-advertisement", "ok"),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();

    let metadata = fixture
        .client
        .get(format!("{}/pypi/simple/pkg/", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(metadata.status(), StatusCode::OK);

    let artifact_url = format!(
        "{}/cargo/api/v1/crates/demo/1.0.0/download",
        fixture.pkg_base_url
    );
    let artifact_responses =
        join_all((0..2).map(|_| fixture.client.get(&artifact_url).send())).await;
    for response in artifact_responses {
        assert_eq!(response.unwrap().status(), StatusCode::OK);
    }

    let git = fixture
        .client
        .get(format!(
            "{}/rust-lang/cargo.git/info/refs?service=git-upload-pack",
            fixture.git_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(git.status(), StatusCode::OK);

    let stats_on_pkg_port = fixture
        .client
        .get(format!("{}/stats", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(stats_on_pkg_port.status(), StatusCode::NOT_FOUND);

    let stats_on_git_port = fixture
        .client
        .get(format!("{}/stats", fixture.git_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(stats_on_git_port.status(), StatusCode::NOT_FOUND);

    let response = fixture
        .client
        .get(format!("{}/stats", fixture.management_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("# HELP vampire_artifact_fetches_total Number of upstream artifact GETs.")
    );
    assert!(body.contains("# TYPE vampire_artifact_fetches_total counter"));
    assert!(body.contains("vampire_artifact_fetches_total{upstream=\"cargo_download\"} 1"));
    assert!(body.contains("vampire_artifact_joins_total{upstream=\"cargo_download\"} 1"));
    assert!(body.contains("vampire_metadata_fetches_total{upstream=\"pypi_simple\"} 1"));
    assert!(body.contains("vampire_git_forwards_total{upstream=\"github\"} 1"));
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
        .get(format!("{}/pypi/simple/pkg/", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(body.contains(&format!(
        "{}/pypi/files/packages/pkg.whl#sha256=abc",
        fixture.pkg_base_url
    )));
}

#[tokio::test]
async fn rejects_encoded_slashes_in_pypi_project_get() {
    let upstream = Upstream::new().await.unwrap();
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let response = fixture
        .client
        .get(format!(
            "{}/pypi/simple/..%2F..%2Fadmin/",
            fixture.pkg_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(upstream.recorded_requests().await.is_empty());
}

#[tokio::test]
async fn rejects_encoded_slashes_in_pypi_project_head() {
    let upstream = Upstream::new().await.unwrap();
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let response = fixture
        .client
        .head(format!(
            "{}/pypi/simple/..%2F..%2Fadmin/",
            fixture.pkg_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(upstream.recorded_requests().await.is_empty());
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
        fixture.pkg_base_url
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
        fixture.pkg_base_url
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
async fn cold_artifact_head_preserves_content_length() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/crates/demo/demo-1.0.0.crate",
            UpstreamResponse::bytes(200, "application/octet-stream", vec![b'x'; 128 * 1024]),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let response = fixture
        .client
        .head(format!(
            "{}/cargo/api/v1/crates/demo/1.0.0/download",
            fixture.pkg_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("content-length").unwrap(), "131072");
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    assert!(response.bytes().await.unwrap().is_empty());
    assert_eq!(
        upstream
            .request_count("/crates/demo/demo-1.0.0.crate")
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
                &json!({
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
    let url = format!("{}/npm/pkg", fixture.pkg_base_url);
    let first = fixture.client.get(&url).send().await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert!(first.headers().get("etag").is_none());
    assert!(first.headers().get("last-modified").is_none());
    upstream
        .insert(
            "/pkg",
            UpstreamResponse::empty(304).with_if_none_match("\"v1\""),
        )
        .await;
    let second = fixture.client.get(&url).send().await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert!(second.headers().get("etag").is_none());
    assert!(second.headers().get("last-modified").is_none());
    assert_eq!(upstream.request_count("/pkg").await, 2);
}

#[tokio::test]
async fn pypi_rewritten_metadata_hides_upstream_validators() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/simple/pkg/",
            UpstreamResponse::text(
                200,
                "text/html",
                r#"<a href="https://files.pythonhosted.org/packages/pkg.whl#sha256=abc">pkg</a>"#,
            )
            .with_header("etag", "\"v1\"")
            .with_header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/pypi/simple/pkg/", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("etag").is_none());
    assert!(response.headers().get("last-modified").is_none());
}

#[tokio::test]
async fn cargo_metadata_preserves_upstream_validators() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/se/rd/serde",
            UpstreamResponse::json(200, &json!({"name": "serde"}))
                .with_header("etag", "\"cargo-v1\"")
                .with_header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/cargo/index/se/rd/serde", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("etag").unwrap(), "\"cargo-v1\"");
    assert_eq!(
        response.headers().get("last-modified").unwrap(),
        "Wed, 21 Oct 2015 07:28:00 GMT"
    );
}

#[tokio::test]
async fn cold_cargo_index_head_matches_get_headers() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/se/rd/serde",
            UpstreamResponse::json(200, &json!({"name": "serde"})),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();
    let url = format!("{}/cargo/index/se/rd/serde", fixture.pkg_base_url);
    let head = fixture.client.head(&url).send().await.unwrap();
    let head_content_type = head
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let head_content_length = head
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(head.bytes().await.unwrap().is_empty());

    let get = fixture.client.get(&url).send().await.unwrap();
    let get_content_type = get
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let get_content_length = get
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(head_content_type, get_content_type);
    assert_eq!(head_content_length, get_content_length);
}

#[tokio::test]
async fn routes_scoped_npm_packuments_without_decoding_scope_separator() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/@scope%2Fname",
            UpstreamResponse::json(
                200,
                &json!({
                    "name": "@scope/name",
                    "dist": {
                        "tarball": "https://registry.npmjs.org/@scope/name/-/name-1.0.0.tgz"
                    }
                }),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/npm/@scope%2Fname", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body =
        serde_json::from_slice::<serde_json::Value>(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(upstream.request_count("/@scope%2Fname").await, 1);
    assert_eq!(
        body["dist"]["tarball"].as_str().unwrap(),
        format!(
            "{}/npm/tarballs/@scope/name/-/name-1.0.0.tgz",
            fixture.pkg_base_url
        )
    );
}

#[tokio::test]
async fn cold_metadata_requests_run_in_parallel() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/npm-a",
            UpstreamResponse::slow_json(
                200,
                &json!({
                    "versions": {
                        "1.0.0": {
                            "dist": { "tarball": "https://registry.npmjs.org/npm-a/-/npm-a-1.0.0.tgz" }
                        }
                    }
                }),
                Duration::from_millis(250),
            ),
        )
        .await;
    upstream
        .insert(
            "/npm-b",
            UpstreamResponse::slow_json(
                200,
                &json!({
                    "versions": {
                        "1.0.0": {
                            "dist": { "tarball": "https://registry.npmjs.org/npm-b/-/npm-b-1.0.0.tgz" }
                        }
                    }
                }),
                Duration::from_millis(250),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();
    let start = Instant::now();
    let first = fixture
        .client
        .get(format!("{}/npm/npm-a", fixture.pkg_base_url))
        .send();
    let second = fixture
        .client
        .get(format!("{}/npm/npm-b", fixture.pkg_base_url))
        .send();
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.unwrap().status(), StatusCode::OK);
    assert_eq!(second.unwrap().status(), StatusCode::OK);
    assert!(
        start.elapsed() < Duration::from_millis(450),
        "metadata requests serialized unexpectedly: {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn serves_cargo_config() {
    let fixture = TestFixture::new().await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/cargo/index/config.json", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(body.contains("/cargo/api/v1/crates"));
}

#[tokio::test]
async fn cargo_config_head_matches_get_headers() {
    let fixture = TestFixture::new().await.unwrap();
    let url = format!("{}/cargo/index/config.json", fixture.pkg_base_url);
    let head = fixture.client.head(&url).send().await.unwrap();
    let head_content_type = head
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let head_content_length = head
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(head.bytes().await.unwrap().is_empty());

    let get = fixture.client.get(&url).send().await.unwrap();
    let get_content_type = get
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let get_content_length = get
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(head_content_type, get_content_type);
    assert_eq!(head_content_length, get_content_length);
}

#[tokio::test]
async fn cold_npm_head_matches_get_headers() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/pkg",
            UpstreamResponse::json(
                200,
                &json!({
                    "dist": {
                        "tarball": "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz"
                    }
                }),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();
    let url = format!("{}/npm/pkg", fixture.pkg_base_url);
    let head = fixture.client.head(&url).send().await.unwrap();
    let head_content_type = head
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let head_content_length = head
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(head.bytes().await.unwrap().is_empty());

    let get = fixture.client.get(&url).send().await.unwrap();
    let get_content_type = get
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let get_content_length = get
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let get_body = get.bytes().await.unwrap();
    assert_eq!(head_content_type, get_content_type);
    assert_eq!(head_content_length, get_content_length);
    assert_eq!(head_content_length, get_body.len().to_string());
}

#[tokio::test]
async fn cold_pypi_head_matches_get_headers() {
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
    let url = format!("{}/pypi/simple/pkg/", fixture.pkg_base_url);
    let head = fixture.client.head(&url).send().await.unwrap();
    let head_content_type = head
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let head_content_length = head
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(head.bytes().await.unwrap().is_empty());

    let get = fixture.client.get(&url).send().await.unwrap();
    let get_content_type = get
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let get_content_length = get
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let get_body = get.bytes().await.unwrap();
    assert_eq!(head_content_type, get_content_type);
    assert_eq!(head_content_length, get_content_length);
    assert_eq!(head_content_length, get_body.len().to_string());
}

#[tokio::test]
async fn cargo_config_ignores_spoofed_origin_headers() {
    let fixture = TestFixture::with_servers_and_public_base_url(
        Upstream::new().await.unwrap(),
        listen_addr().await.unwrap(),
        "https://packages.example".to_owned(),
    )
    .await
    .unwrap();
    let response = fixture
        .client
        .get(format!("{}/cargo/index/config.json", fixture.pkg_base_url))
        .header("host", "evil.example")
        .header("x-forwarded-proto", "http")
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(body.contains(&format!("{}/cargo/api/v1/crates", fixture.public_base_url)));
    assert!(!body.contains("evil.example"));
}

#[tokio::test]
async fn npm_rewrite_ignores_spoofed_origin_headers() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/pkg",
            UpstreamResponse::json(
                200,
                &json!({
                    "dist": {
                        "tarball": "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz"
                    }
                }),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers_and_public_base_url(
        upstream,
        listen_addr().await.unwrap(),
        "https://packages.example".to_owned(),
    )
    .await
    .unwrap();
    let response = fixture
        .client
        .get(format!("{}/npm/pkg", fixture.pkg_base_url))
        .header("host", "evil.example")
        .header("x-forwarded-proto", "http")
        .send()
        .await
        .unwrap();
    let body =
        serde_json::from_slice::<serde_json::Value>(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(
        body["dist"]["tarball"].as_str().unwrap(),
        format!(
            "{}/npm/tarballs/pkg/-/pkg-1.0.0.tgz",
            fixture.public_base_url
        )
    );
}

#[tokio::test]
async fn pypi_rewrite_ignores_spoofed_origin_headers() {
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
    let fixture = TestFixture::with_servers_and_public_base_url(
        upstream,
        listen_addr().await.unwrap(),
        "https://packages.example".to_owned(),
    )
    .await
    .unwrap();
    let response = fixture
        .client
        .get(format!("{}/pypi/simple/pkg/", fixture.pkg_base_url))
        .header("host", "evil.example")
        .header("x-forwarded-proto", "http")
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(body.contains(&format!(
        "{}/pypi/files/packages/pkg.whl#sha256=abc",
        fixture.public_base_url
    )));
    assert!(!body.contains("evil.example"));
}

#[tokio::test]
async fn git_readonly_forwards_smart_http_requests() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/octocat/Hello-World.git/info/refs",
            UpstreamResponse::text(
                200,
                "application/x-git-upload-pack-advertisement",
                "001e# service=git-upload-pack\n0000",
            ),
        )
        .await;
    upstream
        .insert(
            "/octocat/Hello-World.git/git-upload-pack",
            UpstreamResponse::bytes(
                200,
                "application/x-git-upload-pack-result",
                b"PACK".to_vec(),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();

    let discovery = fixture
        .client
        .get(format!(
            "{}/octocat/Hello-World.git/info/refs?service=git-upload-pack",
            fixture.git_base_url
        ))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(discovery.status(), StatusCode::OK);

    let upload_pack_request = b"0014want deadbeef\n0000".to_vec();
    let upload = fixture
        .client
        .post(format!(
            "{}/octocat/Hello-World.git/git-upload-pack",
            fixture.git_base_url
        ))
        .header("content-type", "application/x-git-upload-pack-request")
        .body(upload_pack_request.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);
    assert_eq!(upload.bytes().await.unwrap(), Bytes::from_static(b"PACK"));

    let requests = upstream.recorded_requests().await;
    let discovery_request = requests
        .iter()
        .find(|request| request.path == "/octocat/Hello-World.git/info/refs")
        .unwrap();
    assert_eq!(discovery_request.method, "GET");
    assert_eq!(
        discovery_request.query.as_deref(),
        Some("service=git-upload-pack")
    );
    assert_eq!(
        discovery_request.header("git-protocol").as_deref(),
        Some("version=2")
    );

    let upload_request = requests
        .iter()
        .find(|request| request.path == "/octocat/Hello-World.git/git-upload-pack")
        .unwrap();
    assert_eq!(upload_request.method, "POST");
    assert_eq!(upload_request.query, None);
    assert_eq!(
        upload_request.header("content-type").as_deref(),
        Some("application/x-git-upload-pack-request")
    );
    assert_eq!(upload_request.body, upload_pack_request);
}

#[tokio::test]
async fn git_upload_pack_streams_upstream_response() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/octocat/Hello-World.git/git-upload-pack",
            UpstreamResponse::slow_bytes(
                200,
                "application/x-git-upload-pack-result",
                b"PACK".to_vec(),
                b"DATA".to_vec(),
                Duration::from_millis(250),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();

    let start = Instant::now();
    let mut response = fixture
        .client
        .post(format!(
            "{}/octocat/Hello-World.git/git-upload-pack",
            fixture.git_base_url
        ))
        .header("content-type", "application/x-git-upload-pack-request")
        .body(b"0014want deadbeef\n0000".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let first = response.chunk().await.unwrap().unwrap();
    assert_eq!(first, Bytes::from_static(b"PACK"));
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "proxy buffered the full upstream git response before yielding the first chunk: {:?}",
        start.elapsed()
    );

    let second = response.chunk().await.unwrap().unwrap();
    assert_eq!(second, Bytes::from_static(b"DATA"));
    assert!(
        start.elapsed() >= Duration::from_millis(200),
        "proxy returned the second chunk before the upstream pause elapsed: {:?}",
        start.elapsed()
    );
    assert!(response.chunk().await.unwrap().is_none());
}

#[tokio::test]
async fn git_repeated_reads_reforward_upstream() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/octocat/Hello-World.git/info/refs",
            UpstreamResponse::text(
                200,
                "application/x-git-upload-pack-advertisement",
                "001e# service=git-upload-pack\n0000",
            ),
        )
        .await;
    upstream
        .insert(
            "/octocat/Hello-World.git/info/refs",
            UpstreamResponse::text(
                200,
                "application/x-git-upload-pack-advertisement",
                "001e# service=git-upload-pack\n0000",
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();

    for _ in 0..2 {
        let response = fixture
            .client
            .get(format!(
                "{}/octocat/Hello-World.git/info/refs?service=git-upload-pack",
                fixture.git_base_url
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    assert_eq!(
        upstream
            .request_count("/octocat/Hello-World.git/info/refs")
            .await,
        2
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn git_guardrails_drop_headers_and_reject_invalid_paths() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/octocat/Hello-World.git/info/refs",
            UpstreamResponse::text(
                200,
                "application/x-git-upload-pack-advertisement",
                "001e# service=git-upload-pack\n0000",
            ),
        )
        .await;
    upstream
        .insert(
            "/octocat/Hello-World.git/git-upload-pack",
            UpstreamResponse::bytes(
                200,
                "application/x-git-upload-pack-result",
                b"PACK".to_vec(),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();

    let discovery = fixture
        .client
        .get(format!(
            "{}/octocat/Hello-World.git/info/refs?service=git-upload-pack",
            fixture.git_base_url
        ))
        .header("authorization", "Basic Zm9vOmJhcg==")
        .header("proxy-authorization", "Basic Zm9vOmJhcg==")
        .header("cookie", "session=abc")
        .header("forwarded", "host=evil.example;proto=https")
        .header("x-forwarded-host", "evil.example")
        .header("x-forwarded-proto", "https")
        .header("host", "github.com")
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(discovery.status(), StatusCode::OK);

    let upload = fixture
        .client
        .post(format!(
            "{}/octocat/Hello-World.git/git-upload-pack",
            fixture.git_base_url
        ))
        .header("authorization", "Basic Zm9vOmJhcg==")
        .header("forwarded", "host=evil.example;proto=https")
        .header("git-protocol", "version=2")
        .header("content-type", "application/x-git-upload-pack-request")
        .body(b"0014want deadbeef\n0000".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);

    let info_missing_query = fixture
        .client
        .get(format!(
            "{}/octocat/Hello-World.git/info/refs",
            fixture.git_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(info_missing_query.status(), StatusCode::BAD_REQUEST);

    let receive_pack = fixture
        .client
        .post(format!(
            "{}/octocat/Hello-World.git/git-receive-pack",
            fixture.git_base_url
        ))
        .body(Vec::from("PACK"))
        .send()
        .await
        .unwrap();
    assert_eq!(receive_pack.status(), StatusCode::METHOD_NOT_ALLOWED);

    let encoded_repo = raw_http_request(
        fixture.git_bind,
        &format!(
            "GET /octocat/%48ello-World.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            fixture.git_bind
        ),
    )
    .await
    .unwrap();
    assert_eq!(encoded_repo.status, StatusCode::BAD_REQUEST);
    assert!(encoded_repo.body.contains("invalid git path"));

    let absolute_form = raw_http_request(
        fixture.git_bind,
        &format!(
            "GET http://evil.example/octocat/Hello-World.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            fixture.git_bind
        ),
    )
    .await
    .unwrap();
    assert_eq!(absolute_form.status, StatusCode::BAD_REQUEST);
    assert!(absolute_form.body.contains("absolute-form"));

    let requests = upstream.recorded_requests().await;
    let discovery_request = requests
        .iter()
        .find(|request| request.path == "/octocat/Hello-World.git/info/refs")
        .unwrap();
    assert_eq!(
        discovery_request.header("git-protocol").as_deref(),
        Some("version=2")
    );
    for header in [
        "authorization",
        "proxy-authorization",
        "cookie",
        "forwarded",
        "x-forwarded-host",
        "x-forwarded-proto",
    ] {
        assert_eq!(
            discovery_request.header(header),
            None,
            "unexpected {header}"
        );
    }

    let upload_request = requests
        .iter()
        .find(|request| request.path == "/octocat/Hello-World.git/git-upload-pack")
        .unwrap();
    assert_eq!(
        upload_request.header("content-type").as_deref(),
        Some("application/x-git-upload-pack-request")
    );
    assert_eq!(
        upload_request.header("git-protocol").as_deref(),
        Some("version=2")
    );
    assert_eq!(upload_request.header("authorization"), None);
    assert_eq!(upload_request.header("forwarded"), None);
}

#[tokio::test]
async fn package_and_git_ports_are_isolated() {
    let fixture = TestFixture::new().await.unwrap();

    let git_on_package_port = fixture
        .client
        .get(format!(
            "{}/octocat/Hello-World.git/info/refs?service=git-upload-pack",
            fixture.pkg_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(git_on_package_port.status(), StatusCode::NOT_FOUND);

    let package_on_git_port = fixture
        .client
        .get(format!("{}/cargo/index/config.json", fixture.git_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(package_on_git_port.status(), StatusCode::NOT_FOUND);
}

struct TestFixture {
    _temp_dir: TempDir,
    git_bind: SocketAddr,
    pkg_base_url: String,
    git_base_url: String,
    management_base_url: String,
    public_base_url: String,
    client: Client,
}

impl TestFixture {
    async fn new() -> io::Result<Self> {
        Self::with_servers(Upstream::new().await?).await
    }

    async fn with_servers(upstream: Upstream) -> io::Result<Self> {
        let pkg_bind = listen_addr().await?;
        let public_base_url = format!("http://{pkg_bind}");
        Self::with_servers_and_public_base_url(upstream, pkg_bind, public_base_url).await
    }

    async fn with_servers_and_public_base_url(
        upstream: Upstream,
        pkg_bind: SocketAddr,
        public_base_url: String,
    ) -> io::Result<Self> {
        let temp_dir = tempfile::tempdir()?;
        let git_bind = listen_addr().await?;
        let management_bind = listen_addr().await?;
        let config = Config {
            pkg_bind,
            git_bind,
            management_bind,
            public_base_url: public_base_url.clone(),
            cache_dir: PathBuf::from(temp_dir.path()),
            max_cache_size: 32 * 1024 * 1024,
            max_upstream_fetches: 8,
            upstream_timeout: std::time::Duration::from_secs(5),
        };
        let client = Client::builder()
            .resolve("pypi.org", upstream.addr)
            .resolve("files.pythonhosted.org", upstream.addr)
            .resolve("github.com", upstream.addr)
            .resolve("registry.npmjs.org", upstream.addr)
            .resolve("index.crates.io", upstream.addr)
            .resolve("static.crates.io", upstream.addr)
            .build()
            .map_err(io::Error::other)?;
        let upstreams = RegistryOrigins {
            cargo_download: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            cargo_index: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            github: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            npm: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            pypi_files: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
            pypi_simple: reqwest::Url::parse(&format!("http://{}/", upstream.addr)).unwrap(),
        };
        let app = App::new_with_upstreams(config.clone(), client.clone(), upstreams).await?;
        let pkg_listener = TcpListener::bind(pkg_bind).await?;
        let git_listener = TcpListener::bind(git_bind).await?;
        let management_listener = TcpListener::bind(management_bind).await?;
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
struct Upstream {
    addr: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Vec<UpstreamResponse>>>>,
    counts: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Upstream {
    async fn new() -> io::Result<Self> {
        let listener = TcpListener::bind(listen_addr().await?).await?;
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
            .map_or(0, |value| value.load(Ordering::SeqCst))
    }

    async fn recorded_requests(&self) -> Vec<RecordedRequest> {
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
struct RecordedRequest {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
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

    fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::bytes(
            status,
            "application/json",
            serde_json::to_vec(body).unwrap(),
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

    fn slow_json(status: u16, body: &serde_json::Value, pause: Duration) -> Self {
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

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn with_if_none_match(mut self, value: &str) -> Self {
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
struct RawHttpResponse {
    status: StatusCode,
    body: String,
}

async fn raw_http_request(addr: SocketAddr, request: &str) -> io::Result<RawHttpResponse> {
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

async fn listen_addr() -> io::Result<SocketAddr> {
    TcpListener::bind("127.0.0.1:0").await?.local_addr()
}

fn text_response(status: u16, body: &str) -> Response {
    let mut response = Response::new(Body::from(body.to_owned()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    response
}
