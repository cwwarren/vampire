use crate::routes::CacheClass;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct AppStats {
    inner: Arc<Mutex<StatsInner>>,
}

#[derive(Clone, Debug, Default)]
pub struct StatsSnapshot {
    pub artifact_fetches: HashMap<String, usize>,
    pub metadata_fetches: HashMap<String, usize>,
    pub artifact_joins: HashMap<String, usize>,
}

#[derive(Default)]
struct StatsInner {
    artifact_fetches: HashMap<String, usize>,
    metadata_fetches: HashMap<String, usize>,
    artifact_joins: HashMap<String, usize>,
}

impl AppStats {
    pub fn record_fetch(&self, cache_class: CacheClass, upstream: &str) {
        let mut inner = self.inner.lock().expect("stats mutex poisoned");
        let map = match cache_class {
            CacheClass::Artifact => &mut inner.artifact_fetches,
            CacheClass::Metadata => &mut inner.metadata_fetches,
        };
        *map.entry(upstream.to_owned()).or_insert(0) += 1;
    }

    pub fn record_artifact_join(&self, upstream: &str) {
        let mut inner = self.inner.lock().expect("stats mutex poisoned");
        *inner.artifact_joins.entry(upstream.to_owned()).or_insert(0) += 1;
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let inner = self.inner.lock().expect("stats mutex poisoned");
        StatsSnapshot {
            artifact_fetches: inner.artifact_fetches.clone(),
            metadata_fetches: inner.metadata_fetches.clone(),
            artifact_joins: inner.artifact_joins.clone(),
        }
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock().expect("stats mutex poisoned");
        *inner = StatsInner::default();
    }
}
