use crate::common::{TestFixture, Upstream, UpstreamResponse, header_value};
use axum::http::StatusCode;
use serde_json::json;

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
    let head_content_type = header_value(&head, "content-type").unwrap();
    let head_content_length = header_value(&head, "content-length").unwrap();
    assert!(head.bytes().await.unwrap().is_empty());

    let get = fixture.client.get(&url).send().await.unwrap();
    let get_content_type = header_value(&get, "content-type").unwrap();
    let get_content_length = header_value(&get, "content-length").unwrap();
    assert_eq!(head_content_type, get_content_type);
    assert_eq!(head_content_length, get_content_length);
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
    let head_content_type = header_value(&head, "content-type").unwrap();
    let head_content_length = header_value(&head, "content-length").unwrap();
    assert!(head.bytes().await.unwrap().is_empty());

    let get = fixture.client.get(&url).send().await.unwrap();
    let get_content_type = header_value(&get, "content-type").unwrap();
    let get_content_length = header_value(&get, "content-length").unwrap();
    assert_eq!(head_content_type, get_content_type);
    assert_eq!(head_content_length, get_content_length);
}

#[tokio::test]
async fn cargo_config_ignores_spoofed_origin_headers() {
    let fixture = TestFixture::with_servers_and_public_base_url(
        Upstream::new().await.unwrap(),
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
