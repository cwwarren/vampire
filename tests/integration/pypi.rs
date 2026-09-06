use crate::common::{TestFixture, Upstream, UpstreamResponse, header_value};
use axum::http::StatusCode;

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
async fn follows_rewritten_pypi_root_index_links_to_project_routes() {
    let upstream = Upstream::new().await.unwrap();
    upstream
        .insert(
            "/simple/",
            UpstreamResponse::text(
                200,
                "text/html",
                concat!(
                    r#"<a href="/simple/rooted/">rooted</a>"#,
                    r#"<a href="https://pypi.org/simple/absolute/">absolute</a>"#
                ),
            ),
        )
        .await;
    upstream
        .insert(
            "/simple/rooted/",
            UpstreamResponse::text(200, "text/html", r#"<a href="rooted-1.0.0.tar.gz">r</a>"#),
        )
        .await;
    upstream
        .insert(
            "/simple/absolute/",
            UpstreamResponse::text(200, "text/html", r#"<a href="absolute-2.0.0.tar.gz">a</a>"#),
        )
        .await;
    let fixture = TestFixture::with_servers(upstream).await.unwrap();
    let index = fixture
        .client
        .get(format!("{}/pypi/simple/", fixture.pkg_base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    let body = index.text().await.unwrap();
    for (project, artifact) in [
        ("rooted", "rooted-1.0.0.tar.gz"),
        ("absolute", "absolute-2.0.0.tar.gz"),
    ] {
        let link = format!("{}/pypi/simple/{project}/", fixture.public_base_url);
        assert!(
            body.contains(&format!("href=\"{link}\"")),
            "root index missing {link}: {body}"
        );
        let response = fixture.client.get(&link).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.text().await.unwrap().contains(artifact));
    }
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
