use crate::cache::CacheStore;
use crate::config::Config;
use crate::routes::RegistryOrigins;
use crate::stats::AppStats;
use reqwest::Client;
use std::io;
use std::sync::Arc;

#[derive(Clone)]
pub struct App {
    inner: Arc<AppInner>,
}

pub(crate) struct AppInner {
    pub(crate) cache: CacheStore,
    pub(crate) client: Client,
    pub(crate) stats: AppStats,
    pub(crate) upstreams: RegistryOrigins,
    pub(crate) public_base_url: String,
}

impl App {
    pub async fn new(config: Config) -> io::Result<Self> {
        let client = Client::builder()
            .http2_adaptive_window(true)
            .tcp_nodelay(true)
            .timeout(config.upstream_timeout)
            .build()
            .map_err(io::Error::other)?;
        Self::new_with_upstreams(config, client, RegistryOrigins::default()).await
    }

    pub async fn new_with_upstreams(
        config: Config,
        client: Client,
        upstreams: RegistryOrigins,
    ) -> io::Result<Self> {
        let cache = CacheStore::new(&config).await?;
        Ok(Self {
            inner: Arc::new(AppInner {
                cache,
                client,
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

    pub(crate) fn upstreams(&self) -> &RegistryOrigins {
        &self.inner.upstreams
    }

    pub(crate) fn public_base_url(&self) -> &str {
        &self.inner.public_base_url
    }
}
