use crate::cache::CacheStore;
use crate::config::Config;
use crate::routes::{RegistryOrigins, matches_origin};
use crate::stats::AppStats;
use reqwest::redirect::Policy;
use reqwest::{Client, Url};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

const MAX_PACKAGE_REDIRECTS: usize = 10;

#[derive(Clone)]
pub struct App {
    inner: Arc<AppInner>,
}

pub(crate) struct AppInner {
    pub(crate) cache: CacheStore,
    pub(crate) client: Client,
    pub(crate) git_client: Client,
    pub(crate) stats: AppStats,
    pub(crate) upstreams: RegistryOrigins,
    pub(crate) public_base_url: String,
}

impl App {
    pub async fn new(config: Config) -> io::Result<Self> {
        Self::new_with_origins(config, RegistryOrigins::default()).await
    }

    #[doc(hidden)]
    pub async fn new_with_loopback_upstream(
        config: Config,
        upstream: SocketAddr,
    ) -> io::Result<Self> {
        if !upstream.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "test upstream must use a loopback address",
            ));
        }
        Self::new_with_origins(config, RegistryOrigins::loopback(upstream)).await
    }

    async fn new_with_origins(config: Config, upstreams: RegistryOrigins) -> io::Result<Self> {
        config
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let client = build_package_client(config.upstream_timeout)?;
        let git_client = build_git_client(config.upstream_timeout)?;
        Self::new_with_clients(config, client, git_client, upstreams).await
    }

    async fn new_with_clients(
        config: Config,
        client: Client,
        git_client: Client,
        upstreams: RegistryOrigins,
    ) -> io::Result<Self> {
        let cache = CacheStore::new(&config).await?;
        Ok(Self {
            inner: Arc::new(AppInner {
                cache,
                client,
                git_client,
                stats: AppStats::default(),
                upstreams,
                public_base_url: config.public_base_url.clone(),
            }),
        })
    }

    pub fn stats(&self) -> &AppStats {
        &self.inner.stats
    }

    pub(crate) fn cache(&self) -> &CacheStore {
        &self.inner.cache
    }

    pub(crate) fn client(&self) -> &Client {
        &self.inner.client
    }

    pub(crate) fn git_client(&self) -> &Client {
        &self.inner.git_client
    }

    pub(crate) fn upstreams(&self) -> &RegistryOrigins {
        &self.inner.upstreams
    }

    pub(crate) fn public_base_url(&self) -> &str {
        &self.inner.public_base_url
    }
}

fn build_package_client(timeout: std::time::Duration) -> io::Result<Client> {
    Client::builder()
        .http2_adaptive_window(true)
        .tcp_nodelay(true)
        .redirect(package_redirect_policy())
        .timeout(timeout)
        .build()
        .map_err(io::Error::other)
}

fn package_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if package_redirect_allowed(attempt.url(), attempt.previous()) {
            attempt.follow()
        } else {
            attempt.error("unsafe package redirect")
        }
    })
}

fn build_git_client(timeout: std::time::Duration) -> io::Result<Client> {
    Client::builder()
        .http2_adaptive_window(true)
        .tcp_nodelay(true)
        .redirect(Policy::none())
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .build()
        .map_err(io::Error::other)
}

fn package_redirect_allowed(next: &Url, previous: &[Url]) -> bool {
    previous.len() <= MAX_PACKAGE_REDIRECTS
        && previous.first().is_some_and(|original| {
            matches_origin(next, original)
                && next.username().is_empty()
                && next.password().is_none()
        })
}

#[cfg(test)]
mod tests {
    use super::{App, MAX_PACKAGE_REDIRECTS, build_git_client, package_redirect_allowed};
    use crate::Config;
    use reqwest::Url;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn package_redirects_stay_on_the_original_origin() {
        let original = Url::parse("https://registry.example/pkg").unwrap();
        let previous = [original.clone()];
        assert!(package_redirect_allowed(
            &Url::parse("https://registry.example:443/other").unwrap(),
            &previous
        ));
        for target in [
            "http://registry.example/other",
            "https://other.example/other",
            "https://registry.example:444/other",
            "https://user@registry.example/other",
        ] {
            assert!(!package_redirect_allowed(
                &Url::parse(target).unwrap(),
                &previous
            ));
        }
    }

    #[test]
    fn package_redirects_have_a_hop_limit() {
        let original = Url::parse("https://registry.example/pkg").unwrap();
        let allowed = vec![original.clone(); MAX_PACKAGE_REDIRECTS];
        assert!(package_redirect_allowed(&original, &allowed));
        let rejected = vec![original.clone(); MAX_PACKAGE_REDIRECTS + 1];
        assert!(!package_redirect_allowed(&original, &rejected));
    }

    #[tokio::test]
    async fn custom_upstream_must_be_loopback() {
        let config = Config {
            pkg_bind: "127.0.0.1:0".parse().unwrap(),
            git_bind: "127.0.0.1:0".parse().unwrap(),
            management_bind: "127.0.0.1:0".parse().unwrap(),
            public_base_url: "http://127.0.0.1:8080".to_owned(),
            cache_dir: PathBuf::from("unused"),
            max_cache_size: 1,
            max_upstream_fetches: 1,
            upstream_timeout: Duration::from_secs(1),
        };
        let Err(error) =
            App::new_with_loopback_upstream(config, "192.0.2.1:80".parse().unwrap()).await
        else {
            panic!("non-loopback upstream was accepted");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn git_client_does_not_follow_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let bytes_read = stream.read(&mut request).await.unwrap();
            assert!(bytes_read > 0);
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let response = build_git_client(Duration::from_secs(1))
            .unwrap()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    }
}
