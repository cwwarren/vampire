use crate::common::TestFixture;
use axum::http::StatusCode;

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
    let response = fixture
        .client
        .get(format!(
            "{}/cargo/index/config.json?alias=true",
            fixture.pkg_base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
