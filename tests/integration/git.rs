use crate::common::{TestFixture, Upstream, UpstreamResponse, raw_http_request};
use axum::http::StatusCode;
use bytes::Bytes;
use tokio::time::{Duration, Instant};

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
async fn git_accepts_suffixless_paths_and_canonicalizes_upstream() {
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
            "{}/octocat/Hello-World/info/refs?service=git-upload-pack",
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
            "{}/octocat/Hello-World/git-upload-pack",
            fixture.git_base_url
        ))
        .header("content-type", "application/x-git-upload-pack-request")
        .body(upload_pack_request.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);
    assert_eq!(upload.bytes().await.unwrap(), Bytes::from_static(b"PACK"));

    let missing_service = fixture
        .client
        .get(format!(
            "{}/octocat/Hello-World/info/refs",
            fixture.git_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_service.status(), StatusCode::BAD_REQUEST);

    for path in [
        "/octocat/Hello-World/git-receive-pack",
        "/octocat/Hello-World.git/git-receive-pack",
    ] {
        let receive_pack = fixture
            .client
            .post(format!("{}{path}", fixture.git_base_url))
            .body(Vec::from("PACK"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            receive_pack.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "receive-pack should be rejected for {path}"
        );
    }

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

    let upload_request = requests
        .iter()
        .find(|request| request.path == "/octocat/Hello-World.git/git-upload-pack")
        .unwrap();
    assert_eq!(upload_request.method, "POST");
    assert_eq!(upload_request.body, upload_pack_request);
    assert!(
        requests
            .iter()
            .all(|request| !request.path.contains("Hello-World/")),
        "upstream must only ever see .git-suffixed paths: {:?}",
        requests
            .iter()
            .map(|request| &request.path)
            .collect::<Vec<_>>()
    );
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
