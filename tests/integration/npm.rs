use crate::common::{TestFixture, Upstream, UpstreamResponse, header_value};
use axum::http::{Method, StatusCode};
use serde_json::json;

#[tokio::test]
async fn npm_audit_reports_are_uncached_and_do_not_forward_credentials() {
    let upstream = Upstream::new().await.unwrap();
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    for path in [
        "/-/npm/v1/security/advisories/bulk",
        "/-/npm/v1/security/audits/quick",
    ] {
        let vulnerable = json!({"demo": [{"id": 123, "vulnerable_versions": "<2.0.0"}]});
        upstream
            .insert(
                path,
                UpstreamResponse::json(200, &vulnerable).with_header("etag", "v1"),
            )
            .await;
        upstream
            .insert(path, UpstreamResponse::json(200, &json!({})))
            .await;
        for (body, expected) in [
            (r#"{"demo":["1.0.0"]}"#, vulnerable),
            (r#"{"demo":["2.0.0"]}"#, json!({})),
        ] {
            let response = fixture
                .client
                .post(format!("{}/npm{path}", fixture.pkg_base_url))
                .header("content-type", "application/json")
                .header("authorization", "Bearer client-secret")
                .header("cookie", "session=client-secret")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&response.text().await.unwrap()).unwrap(),
                expected
            );
        }
        assert_eq!(upstream.request_count(path).await, 2);
    }
    for request in upstream.recorded_requests().await {
        assert_eq!(request.method, "POST");
        assert_eq!(request.header("authorization"), None);
        assert_eq!(request.header("cookie"), None);
        assert_eq!(request.header("if-none-match"), None);
    }
}

#[tokio::test]
async fn npm_search_forwards_every_request_fresh_and_unconditionally() {
    let upstream = Upstream::new().await.unwrap();
    let path = "/-/v1/search";
    let first = json!({"objects": [{"package": {"name": "@scope/demo"}}], "total": 1});
    let second = json!({"objects": [{"package": {"name": "other"}}], "total": 1});
    upstream
        .insert(
            path,
            UpstreamResponse::json(200, &first).with_header("etag", "\"first\""),
        )
        .await;
    upstream
        .insert(
            path,
            UpstreamResponse::json(200, &second).with_header("etag", "\"second\""),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let query = "text=%40scope%2Fdemo&size=1&from=0&quality=0.65&popularity=0.98&maintenance=0.5";
    let url = format!("{}/npm{path}?{query}", fixture.pkg_base_url);

    let (left, right) = tokio::join!(
        fixture.client.get(&url).send(),
        fixture.client.get(&url).send()
    );
    let mut bodies = Vec::new();
    for response in [left.unwrap(), right.unwrap()] {
        assert_eq!(response.status(), StatusCode::OK);
        bodies.push(
            serde_json::from_str::<serde_json::Value>(&response.text().await.unwrap()).unwrap(),
        );
    }
    assert!(
        bodies.contains(&first) && bodies.contains(&second),
        "identical concurrent searches were coalesced or cached: {bodies:?}"
    );
    assert_eq!(upstream.request_count(path).await, 2);

    let other_query = "text=other&size=1";
    let other = fixture
        .client
        .get(format!("{}/npm{path}?{other_query}", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::OK);

    let head = fixture.client.head(&url).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    let head_headers = [
        header_value(&head, "content-type"),
        header_value(&head, "content-length"),
        header_value(&head, "etag"),
    ];
    assert!(head.bytes().await.unwrap().is_empty());

    let get = fixture.client.get(&url).send().await.unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    let get_headers = [
        header_value(&get, "content-type"),
        header_value(&get, "content-length"),
        header_value(&get, "etag"),
    ];
    let get_body = get.bytes().await.unwrap();
    assert_eq!(head_headers, get_headers);
    assert_eq!(
        head_headers[2].as_deref(),
        Some("\"second\""),
        "upstream validators should reach the client untouched"
    );
    assert_eq!(
        head_headers[1].as_deref(),
        Some(&*get_body.len().to_string())
    );

    let requests = upstream.recorded_requests().await;
    assert_eq!(requests.len(), 5);
    for request in &requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.header("if-none-match"), None);
        assert_eq!(request.header("if-modified-since"), None);
    }
    let queries: Vec<Option<&str>> = requests
        .iter()
        .map(|request| request.query.as_deref())
        .collect();
    assert_eq!(
        queries,
        vec![
            Some(query),
            Some(query),
            Some(other_query),
            Some(query),
            Some(query),
        ]
    );
}

#[tokio::test]
async fn npm_utility_routes_reject_unapproved_requests_locally() {
    let upstream = Upstream::new().await.unwrap();
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    for (method, path, expected) in [
        (
            Method::GET,
            "/npm/-/v1/search?text=demo&registry=https://example.com",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::POST,
            "/npm/-/npm/v1/security/advisories/bulk?alias=true",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::POST,
            "/npm/-/npm/v1/security/audits/quick?alias=true",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::POST,
            "/npm/-/v1/search?text=demo",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (Method::PUT, "/npm/demo", StatusCode::METHOD_NOT_ALLOWED),
        (
            Method::POST,
            "/npm/-/npm/v1/security/unknown",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
    ] {
        let response = fixture
            .client
            .request(method, format!("{}{path}", fixture.pkg_base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
    }
    assert!(upstream.recorded_requests().await.is_empty());
}

#[tokio::test]
async fn npm_audit_rejects_oversized_bodies_before_forwarding() {
    let upstream = Upstream::new().await.unwrap();
    let fixture = TestFixture::with_servers(upstream.clone()).await.unwrap();
    let response = fixture
        .client
        .post(format!(
            "{}/npm/-/npm/v1/security/advisories/bulk",
            fixture.pkg_base_url
        ))
        .body(vec![b'x'; 8 * 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(upstream.recorded_requests().await.is_empty());
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
        .get(format!("{}/npm/@scope%2fname", fixture.pkg_base_url))
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
    let head_content_type = header_value(&head, "content-type").unwrap();
    let head_content_length = header_value(&head, "content-length").unwrap();
    assert!(head.bytes().await.unwrap().is_empty());

    let get = fixture.client.get(&url).send().await.unwrap();
    let get_content_type = header_value(&get, "content-type").unwrap();
    let get_content_length = header_value(&get, "content-length").unwrap();
    let get_body = get.bytes().await.unwrap();
    assert_eq!(head_content_type, get_content_type);
    assert_eq!(head_content_length, get_content_length);
    assert_eq!(head_content_length, get_body.len().to_string());
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
