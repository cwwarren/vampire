use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct AppStats {
    inner: Mutex<StatsSnapshot>,
}

#[derive(Clone, Debug, Default)]
pub struct StatsSnapshot {
    pub artifact_fetches: HashMap<String, usize>,
    pub metadata_fetches: HashMap<String, usize>,
    pub artifact_joins: HashMap<String, usize>,
}

impl AppStats {
    pub fn record_artifact_fetch(&self, upstream: &str) {
        let mut inner = self.inner.lock().expect("stats mutex poisoned");
        *inner
            .artifact_fetches
            .entry(upstream.to_owned())
            .or_insert(0) += 1;
    }

    pub fn record_metadata_fetch(&self, upstream: &str) {
        let mut inner = self.inner.lock().expect("stats mutex poisoned");
        *inner
            .metadata_fetches
            .entry(upstream.to_owned())
            .or_insert(0) += 1;
    }

    pub fn record_artifact_join(&self, upstream: &str) {
        let mut inner = self.inner.lock().expect("stats mutex poisoned");
        *inner.artifact_joins.entry(upstream.to_owned()).or_insert(0) += 1;
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        self.inner.lock().expect("stats mutex poisoned").clone()
    }

    pub fn reset(&self) {
        *self.inner.lock().expect("stats mutex poisoned") = StatsSnapshot::default();
    }
}
