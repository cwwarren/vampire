use crate::common::{TestFixture, Upstream, UpstreamResponse};
use axum::http::{Method, StatusCode};
use futures_util::future::join_all;
use serde_json::json;
use tokio::time::{Duration, Instant};

#[tokio::test]
async fn rejects_encoded_fragment_cache_aliases() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/pkg/-/pkg.tgz",
            UpstreamResponse::bytes(200, "application/octet-stream", b"package".to_vec()),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    for fragment in ["one", "two"] {
        let response = fixture
            .client
            .get(format!(
                "{}/npm/tarballs/pkg/-/pkg.tgz%23{fragment}",
                fixture.pkg_base_url
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(upstream.request_count("/pkg/-/pkg.tgz").await, 0);
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
async fn serves_artifact_larger_than_cache_bound() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/crates/demo/demo-1.0.0.crate",
            UpstreamResponse::bytes(200, "application/octet-stream", vec![b'x'; 128 * 1024]),
        )
        .await;
    let fixture = TestFixture::with_servers_and_limits(upstream.clone(), 1, 8)
        .await
        .unwrap();
    let response = fixture
        .client
        .get(format!(
            "{}/cargo/api/v1/crates/demo/1.0.0/download",
            fixture.pkg_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().len(), 128 * 1024);
    assert_eq!(
        upstream
            .request_count("/crates/demo/demo-1.0.0.crate")
            .await,
        1
    );
}

#[tokio::test]
async fn rejects_unadmitted_unique_work_with_service_unavailable() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/crates/slow/slow-1.0.0.crate",
            UpstreamResponse::slow_bytes(
                200,
                "application/octet-stream",
                vec![b'a'; 1024],
                vec![b'b'; 1024],
                Duration::from_millis(250),
            ),
        )
        .await;
    upstream
        .insert(
            "/crates/other/other-1.0.0.crate",
            UpstreamResponse::bytes(200, "application/octet-stream", b"other".to_vec()),
        )
        .await;
    let fixture = TestFixture::with_servers_and_limits(upstream.clone(), 1024 * 1024, 1)
        .await
        .unwrap();
    let client = fixture.client.clone();
    let slow_url = format!(
        "{}/cargo/api/v1/crates/slow/1.0.0/download",
        fixture.pkg_base_url
    );
    let slow = tokio::spawn(async move { client.get(slow_url).send().await.unwrap() });
    let deadline = Instant::now() + Duration::from_secs(1);
    while upstream
        .request_count("/crates/slow/slow-1.0.0.crate")
        .await
        == 0
    {
        assert!(Instant::now() < deadline, "slow request did not start");
        tokio::task::yield_now().await;
    }
    let rejected = fixture
        .client
        .get(format!(
            "{}/cargo/api/v1/crates/other/1.0.0/download",
            fixture.pkg_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        upstream
            .request_count("/crates/other/other-1.0.0.crate")
            .await,
        0
    );
    for (method, path) in [
        (Method::POST, "/npm/-/npm/v1/security/advisories/bulk"),
        (Method::GET, "/npm/-/v1/search?text=demo"),
    ] {
        let response = fixture
            .client
            .request(method, format!("{}{path}", fixture.pkg_base_url))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(upstream.recorded_requests().await.len(), 1);
    assert_eq!(slow.await.unwrap().status(), StatusCode::OK);
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
async fn cached_metadata_head_revalidates() {
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
    let second = fixture.client.head(&url).send().await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert!(second.headers().get("etag").is_none());
    assert!(second.headers().get("last-modified").is_none());
    assert!(second.bytes().await.unwrap().is_empty());
    assert_eq!(upstream.request_count("/pkg").await, 2);
    let requests = upstream.recorded_requests().await;
    assert_eq!(requests[1].method, "GET");
    assert_eq!(
        requests[1].header("if-none-match").as_deref(),
        Some("\"v1\"")
    );
}

#[tokio::test]
async fn distinct_cold_metadata_requests_run_in_parallel_within_admission_limit() {
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
async fn concurrent_cold_metadata_requests_singleflight() {
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
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let url = format!("{}/npm/npm-a", fixture.pkg_base_url);
    let responses = join_all((0..8).map(|_| fixture.client.get(&url).send())).await;
    for response in responses {
        assert_eq!(response.unwrap().status(), StatusCode::OK);
    }
    assert_eq!(upstream.request_count("/npm-a").await, 1);
}

#[tokio::test]
async fn rejects_metadata_content_length_over_limit() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/npm-a",
            UpstreamResponse::slow_bytes(
                200,
                "application/json",
                Vec::new(),
                Vec::new(),
                Duration::ZERO,
            )
            .with_header("content-length", &(128_u64 * 1024 * 1024 + 1).to_string()),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();
    let response = fixture
        .client
        .get(format!("{}/npm/npm-a", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.text().await.unwrap().contains("128 MiB limit"));
}
