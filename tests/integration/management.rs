use crate::common::{TestFixture, Upstream, UpstreamResponse};
use axum::http::StatusCode;
use futures_util::future::join_all;
use serde_json::json;
use tokio::time::{Duration, Instant};

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
    assert!(
        body.contains("\nvampire_npm_search_requests_total 0\n"),
        "search counter must be exported even at zero: {body}"
    );
    assert!(
        body.contains("\nvampire_npm_audit_requests_total 0\n"),
        "audit counter must be exported even at zero: {body}"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn stats_count_npm_search_and_audit_requests_separately() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/-/v1/search",
            UpstreamResponse::json(200, &json!({"objects": [], "total": 0})),
        )
        .await;
    upstream
        .insert(
            "/-/npm/v1/security/advisories/bulk",
            UpstreamResponse::json(200, &json!({})),
        )
        .await;
    upstream
        .insert("/pkg", UpstreamResponse::json(200, &json!({"name": "pkg"})))
        .await;
    upstream
        .insert(
            "/crates/slow/slow-1.0.0.crate",
            UpstreamResponse::slow_bytes(
                200,
                "application/octet-stream",
                vec![b'a'; 1024],
                vec![b'b'; 1024],
                Duration::from_millis(400),
            ),
        )
        .await;
    let fixture = TestFixture::with_servers_and_limits(upstream.clone(), 1024 * 1024, 1)
        .await
        .unwrap();
    let search_url = format!("{}/npm/-/v1/search?text=demo", fixture.pkg_base_url);
    let audit_url = format!(
        "{}/npm/-/npm/v1/security/advisories/bulk",
        fixture.pkg_base_url
    );

    for status in [
        fixture
            .client
            .get(&search_url)
            .send()
            .await
            .unwrap()
            .status(),
        fixture
            .client
            .head(&search_url)
            .send()
            .await
            .unwrap()
            .status(),
        fixture
            .client
            .post(&audit_url)
            .header("content-type", "application/json")
            .body(r#"{"demo":["1.0.0"]}"#)
            .send()
            .await
            .unwrap()
            .status(),
        fixture
            .client
            .get(format!("{}/npm/pkg", fixture.pkg_base_url))
            .send()
            .await
            .unwrap()
            .status(),
    ] {
        assert_eq!(status, StatusCode::OK);
    }

    let oversized = fixture
        .client
        .post(&audit_url)
        .body(vec![b'x'; 8 * 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::BAD_REQUEST);

    let uncounted = fixture
        .client
        .get(format!(
            "{}/npm/-/v1/search?text=demo&registry=https://example.com",
            fixture.pkg_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(uncounted.status(), StatusCode::NOT_FOUND);
    let uncounted = fixture
        .client
        .post(format!("{audit_url}?alias=true"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(uncounted.status(), StatusCode::NOT_FOUND);
    let uncounted = fixture
        .client
        .post(&search_url)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(uncounted.status(), StatusCode::METHOD_NOT_ALLOWED);

    let client = fixture.client.clone();
    let slow_url = format!(
        "{}/cargo/api/v1/crates/slow/1.0.0/download",
        fixture.pkg_base_url
    );
    let slow = tokio::spawn(async move { client.get(slow_url).send().await.unwrap() });
    let deadline = Instant::now() + Duration::from_secs(2);
    while upstream
        .request_count("/crates/slow/slow-1.0.0.crate")
        .await
        == 0
    {
        assert!(
            Instant::now() < deadline,
            "slow artifact request did not start"
        );
        tokio::task::yield_now().await;
    }
    let rejected_search = fixture.client.get(&search_url).send().await.unwrap();
    assert_eq!(rejected_search.status(), StatusCode::SERVICE_UNAVAILABLE);
    let rejected_audit = fixture
        .client
        .post(&audit_url)
        .body(r#"{"demo":["1.0.0"]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected_audit.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(slow.await.unwrap().status(), StatusCode::OK);

    let stats = fixture
        .client
        .get(format!("{}/stats", fixture.management_base_url))
        .send()
        .await
        .unwrap();
    let body = stats.text().await.unwrap();
    assert!(
        body.contains("\nvampire_npm_search_requests_total 3\n"),
        "expected 3 search requests (GET, HEAD, admission rejection): {body}"
    );
    assert!(
        body.contains("\nvampire_npm_audit_requests_total 3\n"),
        "expected 3 audit requests (POST, oversized body, admission rejection): {body}"
    );
    assert!(
        body.contains("vampire_metadata_fetches_total{upstream=\"npm\"} 1"),
        "search and audit must not be counted as package metadata fetches: {body}"
    );
    assert!(!body.contains("vampire_npm_search_requests_total{"));
    assert!(!body.contains("vampire_npm_audit_requests_total{"));
    assert_eq!(upstream.request_count("/-/v1/search").await, 2);
    assert_eq!(
        upstream
            .request_count("/-/npm/v1/security/advisories/bulk")
            .await,
        1
    );
}
