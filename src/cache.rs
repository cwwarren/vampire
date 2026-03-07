use crate::config::Config;
use crate::routes::CacheClass;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::fs;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

#[derive(Clone)]
pub struct CacheStore {
    root: Arc<PathBuf>,
    max_cache_size: u64,
    inflight: Arc<Mutex<HashMap<String, Arc<Inflight>>>>,
    temp_counter: Arc<AtomicU64>,
    upstream_semaphore: Arc<Semaphore>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredResponseMeta {
    pub cache_class: CacheClass,
    pub headers: Vec<(String, String)>,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
    pub status: u16,
}

#[derive(Clone, Debug)]
pub struct StoredArtifact {
    pub body_path: PathBuf,
    pub meta: StoredResponseMeta,
}

#[derive(Clone, Debug)]
pub struct StoredMetadata {
    pub body: Bytes,
    pub meta: StoredResponseMeta,
}

pub struct ArtifactLeader {
    pub inflight: Arc<Inflight>,
    pub key: String,
    pub paths: CachePaths,
}

pub enum ArtifactLookup {
    Hit(StoredArtifact),
    Join(Arc<Inflight>),
    Leader(ArtifactLeader),
}

pub struct CachePaths {
    pub body: PathBuf,
    pub meta: PathBuf,
    pub temp: PathBuf,
}

pub struct Inflight {
    state: Mutex<InflightState>,
    notify: Notify,
}

struct InflightState {
    outcome: Option<InflightOutcome>,
}

#[derive(Clone)]
pub enum InflightOutcome {
    Cached,
    Response(StoredResponseMeta, Bytes),
}

impl CacheStore {
    pub async fn new(config: &Config) -> io::Result<Self> {
        fs::create_dir_all(&config.cache_dir).await?;
        let store = Self {
            root: Arc::new(config.cache_dir.clone()),
            max_cache_size: config.max_cache_size,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            temp_counter: Arc::new(AtomicU64::new(0)),
            upstream_semaphore: Arc::new(Semaphore::new(config.max_upstream_fetches)),
        };
        store.cleanup_stale_temps(Duration::from_secs(300)).await?;
        store.evict_to_bound().await?;
        Ok(store)
    }

    pub fn key_for(cache_class: CacheClass, upstream: &str, accept_variant: &str) -> String {
        let mut hasher = Sha256::new();
        let class = match cache_class {
            CacheClass::Artifact => "artifact",
            CacheClass::Metadata => "metadata",
        };
        hasher.update(class.as_bytes());
        hasher.update([0]);
        hasher.update(upstream.as_bytes());
        hasher.update([0]);
        hasher.update(accept_variant.as_bytes());
        hex::encode(hasher.finalize())
    }

    pub async fn load_artifact(&self, key: &str) -> io::Result<Option<StoredArtifact>> {
        let paths = self.paths_for(key);
        let meta_bytes = match fs::read(&paths.meta).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let meta = serde_json::from_slice::<StoredResponseMeta>(&meta_bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if fs::metadata(&paths.body).await.is_err() {
            return Ok(None);
        }
        Ok(Some(StoredArtifact {
            body_path: paths.body,
            meta,
        }))
    }

    pub async fn load_metadata(&self, key: &str) -> io::Result<Option<StoredMetadata>> {
        let paths = self.paths_for(key);
        let bytes = match fs::read(&paths.body).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(unpack_metadata(&bytes)?))
    }

    pub async fn lookup_or_start_artifact(&self, key: String) -> io::Result<ArtifactLookup> {
        if let Some(entry) = self.load_artifact(&key).await? {
            return Ok(ArtifactLookup::Hit(entry));
        }
        let paths = self.paths_for(&key);
        if let Some(parent) = paths.body.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut inflight_map = self.inflight.lock().await;
        if let Some(existing) = inflight_map.get(&key) {
            return Ok(ArtifactLookup::Join(existing.clone()));
        }
        let inflight = Arc::new(Inflight::new());
        inflight_map.insert(key.clone(), inflight.clone());
        Ok(ArtifactLookup::Leader(ArtifactLeader {
            inflight,
            key,
            paths,
        }))
    }

    pub async fn store_metadata(
        &self,
        key: &str,
        body: &[u8],
        meta: &StoredResponseMeta,
    ) -> io::Result<StoredMetadata> {
        let paths = self.paths_for(key);
        if let Some(parent) = paths.body.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temp_path = self.unique_temp_path(&paths.temp);
        let packed = pack_metadata(meta, body)?;
        fs::write(&temp_path, packed).await?;
        fs::rename(&temp_path, &paths.body).await?;
        self.evict_to_bound().await?;
        Ok(StoredMetadata {
            body: Bytes::copy_from_slice(body),
            meta: meta.clone(),
        })
    }

    pub async fn commit_artifact(
        &self,
        key: &str,
        meta: &StoredResponseMeta,
        temp_path: &Path,
    ) -> io::Result<StoredArtifact> {
        let paths = self.paths_for(key);
        let meta_bytes = serde_json::to_vec(meta)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&paths.meta, meta_bytes).await?;
        fs::rename(temp_path, &paths.body).await?;
        self.evict_to_bound().await?;
        Ok(StoredArtifact {
            body_path: paths.body,
            meta: meta.clone(),
        })
    }

    pub async fn acquire_upstream_permit(&self) -> io::Result<OwnedSemaphorePermit> {
        self.upstream_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))
    }

    pub async fn finish_inflight(&self, key: &str) {
        self.inflight.lock().await.remove(key);
    }

    pub fn paths_for(&self, key: &str) -> CachePaths {
        let shard = &key[..2];
        let dir = self.root.join(shard);
        CachePaths {
            body: dir.join(format!("{key}.body")),
            meta: dir.join(format!("{key}.json")),
            temp: dir.join(format!("{key}.part")),
        }
    }

    fn unique_temp_path(&self, path: &Path) -> PathBuf {
        let suffix = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("temp");
        path.with_file_name(format!("{name}.{suffix}.part"))
    }

    async fn cleanup_stale_temps(&self, max_age: Duration) -> io::Result<()> {
        let now = SystemTime::now();
        let mut stack = vec![self.root.as_ref().clone()];
        while let Some(dir) = stack.pop() {
            let mut entries = match fs::read_dir(&dir).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let metadata = entry.metadata().await?;
                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|extension| extension.to_str()) != Some("part") {
                    continue;
                }
                let age = now
                    .duration_since(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH))
                    .unwrap_or_default();
                if age >= max_age {
                    let _ = fs::remove_file(path).await;
                }
            }
        }
        Ok(())
    }

    async fn evict_to_bound(&self) -> io::Result<()> {
        let mut entries = self.completed_entries().await?;
        let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
        if total <= self.max_cache_size {
            return Ok(());
        }
        entries.sort_by_key(|entry| entry.modified);
        for entry in entries {
            if total <= self.max_cache_size {
                break;
            }
            let _ = fs::remove_file(&entry.body).await;
            let _ = fs::remove_file(&entry.meta).await;
            total = total.saturating_sub(entry.size);
        }
        Ok(())
    }

    async fn completed_entries(&self) -> io::Result<Vec<CompletedEntry>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.as_ref().clone()];
        while let Some(dir) = stack.pop() {
            let mut entries = match fs::read_dir(&dir).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let metadata = entry.metadata().await?;
                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|extension| extension.to_str()) != Some("body") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let meta = path.with_file_name(format!("{stem}.json"));
                let meta_size = fs::metadata(&meta)
                    .await
                    .map(|item| item.len())
                    .unwrap_or(0);
                out.push(CompletedEntry {
                    body: path,
                    meta,
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    size: metadata.len() + meta_size,
                });
            }
        }
        Ok(out)
    }
}

impl Inflight {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(InflightState { outcome: None }),
            notify: Notify::new(),
        }
    }

    pub async fn wait_for_outcome(&self) -> io::Result<InflightOutcome> {
        loop {
            let state = self.state.lock().await;
            if let Some(outcome) = &state.outcome {
                return Ok(outcome.clone());
            }
            drop(state);
            self.notify.notified().await;
        }
    }

    pub async fn finish_cached(&self) {
        let mut state = self.state.lock().await;
        state.outcome = Some(InflightOutcome::Cached);
        drop(state);
        self.notify.notify_waiters();
    }

    pub async fn finish_response(&self, meta: StoredResponseMeta, body: Bytes) {
        let mut state = self.state.lock().await;
        state.outcome = Some(InflightOutcome::Response(meta, body));
        drop(state);
        self.notify.notify_waiters();
    }

    pub async fn fail(&self, error: String) {
        let content_length = error.len().to_string();
        let mut state = self.state.lock().await;
        state.outcome = Some(InflightOutcome::Response(
            StoredResponseMeta {
                cache_class: CacheClass::Artifact,
                headers: vec![
                    ("content-length".to_owned(), content_length),
                    (
                        "content-type".to_owned(),
                        "text/plain; charset=utf-8".to_owned(),
                    ),
                ],
                last_modified: None,
                etag: None,
                status: 502,
            },
            Bytes::from(error),
        ));
        drop(state);
        self.notify.notify_waiters();
    }
}

struct CompletedEntry {
    body: PathBuf,
    meta: PathBuf,
    modified: SystemTime,
    size: u64,
}

fn pack_metadata(meta: &StoredResponseMeta, body: &[u8]) -> io::Result<Vec<u8>> {
    let meta_bytes = serde_json::to_vec(meta)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let meta_len = u32::try_from(meta_bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata too large"))?;
    let mut packed = Vec::with_capacity(4 + meta_bytes.len() + body.len());
    packed.extend_from_slice(&meta_len.to_be_bytes());
    packed.extend_from_slice(&meta_bytes);
    packed.extend_from_slice(body);
    Ok(packed)
}

fn unpack_metadata(bytes: &[u8]) -> io::Result<StoredMetadata> {
    if bytes.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata entry missing length prefix",
        ));
    }
    let meta_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if bytes.len() < 4 + meta_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata entry truncated",
        ));
    }
    let meta = serde_json::from_slice::<StoredResponseMeta>(&bytes[4..4 + meta_len])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(StoredMetadata {
        meta,
        body: Bytes::copy_from_slice(&bytes[4 + meta_len..]),
    })
}

#[cfg(test)]
mod tests {
    use super::{CacheStore, StoredResponseMeta, pack_metadata};
    use crate::config::Config;
    use crate::routes::CacheClass;
    use bytes::Bytes;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::time::Duration;

    #[tokio::test]
    async fn metadata_round_trip_uses_one_file() {
        let temp = tempdir().unwrap();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            cache_dir: PathBuf::from(temp.path()),
            max_cache_size: 1024 * 1024,
            max_upstream_fetches: 4,
            upstream_timeout: Duration::from_secs(5),
        };
        let store = CacheStore::new(&config).await.unwrap();
        let key = CacheStore::key_for(CacheClass::Metadata, "https://registry.npmjs.org/pkg", "");
        let meta = StoredResponseMeta {
            cache_class: CacheClass::Metadata,
            headers: vec![("content-length".to_owned(), "5".to_owned())],
            last_modified: Some("yesterday".to_owned()),
            etag: Some("\"v1\"".to_owned()),
            status: 200,
        };
        store.store_metadata(&key, b"hello", &meta).await.unwrap();
        let loaded = store.load_metadata(&key).await.unwrap().unwrap();
        assert_eq!(loaded.body, Bytes::from_static(b"hello"));
        assert_eq!(loaded.meta.headers, meta.headers);
        assert_eq!(loaded.meta.etag, meta.etag);
        let paths = store.paths_for(&key);
        assert!(fs::metadata(&paths.meta).await.is_err());
        assert_eq!(
            fs::read(&paths.body).await.unwrap(),
            pack_metadata(&meta, b"hello").unwrap()
        );
    }
}
