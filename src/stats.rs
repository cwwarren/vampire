use std::collections::HashMap;
use std::fmt::Write;
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
    pub git_forwards: HashMap<String, usize>,
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

    pub fn record_git_forward(&self, upstream: &str) {
        let mut inner = self.inner.lock().expect("stats mutex poisoned");
        *inner.git_forwards.entry(upstream.to_owned()).or_insert(0) += 1;
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        self.inner.lock().expect("stats mutex poisoned").clone()
    }

    pub fn render_prometheus(&self) -> String {
        self.snapshot().render_prometheus()
    }

    pub fn reset(&self) {
        *self.inner.lock().expect("stats mutex poisoned") = StatsSnapshot::default();
    }
}

impl StatsSnapshot {
    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        write_counter_metric(
            &mut out,
            "vampire_artifact_fetches_total",
            "Number of upstream artifact GETs.",
            &self.artifact_fetches,
        );
        write_counter_metric(
            &mut out,
            "vampire_metadata_fetches_total",
            "Number of upstream metadata GETs.",
            &self.metadata_fetches,
        );
        write_counter_metric(
            &mut out,
            "vampire_artifact_joins_total",
            "Number of requests that joined an in-flight artifact fetch.",
            &self.artifact_joins,
        );
        write_counter_metric(
            &mut out,
            "vampire_git_forwards_total",
            "Number of git requests forwarded upstream.",
            &self.git_forwards,
        );
        out
    }
}

fn write_counter_metric(out: &mut String, name: &str, help: &str, values: &HashMap<String, usize>) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let mut entries: Vec<_> = values.iter().collect();
    entries.sort_by_key(|(upstream, _)| *upstream);
    for (upstream, count) in entries {
        let _ = writeln!(
            out,
            "{name}{{upstream=\"{}\"}} {count}",
            escape_label_value(upstream)
        );
    }
}

fn escape_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(ch),
        }
    }
    escaped
}
