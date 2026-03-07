use crate::config::Config;
use crate::routes::CacheClass;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::fs;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

#[derive(Clone)]
pub struct CacheStore {
    root: Arc<PathBuf>,
    max_cache_size: u64,
    inflight: Arc<Mutex<HashMap<String, Arc<Inflight>>>>,
    metadata_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
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
pub struct StoredEntry {
    pub body_path: PathBuf,
    pub meta: StoredResponseMeta,
}

pub struct ArtifactLeader {
    pub inflight: Arc<Inflight>,
    pub key: String,
    pub paths: CachePaths,
    pub lock_file: std::fs::File,
}

pub enum ArtifactLookup {
    Hit(StoredEntry),
    Join(Arc<Inflight>),
    Leader(ArtifactLeader),
}

pub struct CachePaths {
    pub body: PathBuf,
    pub meta: PathBuf,
    pub temp: PathBuf,
    pub lock: PathBuf,
}

pub struct Inflight {
    temp_path: PathBuf,
    state: Mutex<InflightState>,
    notify: Notify,
}

struct InflightState {
    complete: bool,
    error: Option<String>,
    meta: Option<StoredResponseMeta>,
    readers: usize,
    remove_when_idle: bool,
    bytes_written: u64,
}

pub struct ReaderGuard {
    inflight: Arc<Inflight>,
}

impl CacheStore {
    pub async fn new(config: &Config) -> io::Result<Self> {
        fs::create_dir_all(&config.cache_dir).await?;
        let store = Self {
            root: Arc::new(config.cache_dir.clone()),
            max_cache_size: config.max_cache_size,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            metadata_locks: Arc::new(Mutex::new(HashMap::new())),
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

    pub async fn load(&self, key: &str) -> io::Result<Option<StoredEntry>> {
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
        Ok(Some(StoredEntry {
            body_path: paths.body,
            meta,
        }))
    }

    pub async fn lookup_or_start_artifact(&self, key: String) -> io::Result<ArtifactLookup> {
        if let Some(entry) = self.load(&key).await? {
            return Ok(ArtifactLookup::Hit(entry));
        }
        {
            let inflight = self.inflight.lock().await;
            if let Some(existing) = inflight.get(&key) {
                return Ok(ArtifactLookup::Join(existing.clone()));
            }
        }
        let paths = self.paths_for(&key);
        if let Some(parent) = paths.body.parent() {
            fs::create_dir_all(parent).await?;
        }
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&paths.lock)?;
        if let Err(error) = lock_file.try_lock_exclusive() {
            if let Some(existing) = self.inflight.lock().await.get(&key).cloned() {
                return Ok(ArtifactLookup::Join(existing));
            }
            return Err(io::Error::new(io::ErrorKind::WouldBlock, error));
        }
        let inflight = Arc::new(Inflight::new(paths.temp.clone()));
        self.inflight
            .lock()
            .await
            .insert(key.clone(), inflight.clone());
        Ok(ArtifactLookup::Leader(ArtifactLeader {
            inflight,
            key,
            paths,
            lock_file,
        }))
    }

    pub async fn store_metadata(
        &self,
        key: &str,
        body: &[u8],
        meta: &StoredResponseMeta,
    ) -> io::Result<StoredEntry> {
        let paths = self.paths_for(key);
        if let Some(parent) = paths.body.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&paths.temp, body).await?;
        fs::rename(&paths.temp, &paths.body).await?;
        let meta_bytes = serde_json::to_vec(meta)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&paths.meta, meta_bytes).await?;
        self.evict_to_bound().await?;
        Ok(StoredEntry {
            body_path: paths.body,
            meta: meta.clone(),
        })
    }

    pub async fn commit_artifact(
        &self,
        key: &str,
        meta: &StoredResponseMeta,
        temp_path: &Path,
    ) -> io::Result<()> {
        let paths = self.paths_for(key);
        let meta_bytes = serde_json::to_vec(meta)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&paths.meta, meta_bytes).await?;
        fs::rename(temp_path, &paths.body).await?;
        self.evict_to_bound().await
    }

    pub async fn acquire_upstream_permit(&self) -> io::Result<OwnedSemaphorePermit> {
        self.upstream_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))
    }

    pub async fn lock_metadata(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.metadata_locks.lock().await;
            locks
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
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
            lock: dir.join(format!("{key}.lock")),
        }
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
    fn new(temp_path: PathBuf) -> Self {
        Self {
            temp_path,
            state: Mutex::new(InflightState {
                complete: false,
                error: None,
                meta: None,
                readers: 0,
                remove_when_idle: false,
                bytes_written: 0,
            }),
            notify: Notify::new(),
        }
    }

    pub fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    pub async fn wait_for_headers(&self) -> io::Result<StoredResponseMeta> {
        loop {
            let state = self.state.lock().await;
            if let Some(meta) = &state.meta {
                return Ok(meta.clone());
            }
            if let Some(error) = &state.error {
                return Err(io::Error::other(error.clone()));
            }
            drop(state);
            self.notify.notified().await;
        }
    }

    pub async fn snapshot(&self) -> InflightSnapshot {
        let state = self.state.lock().await;
        InflightSnapshot {
            bytes_written: state.bytes_written,
            complete: state.complete,
            error: state.error.clone(),
        }
    }

    pub async fn wait(&self) {
        self.notify.notified().await;
    }

    pub async fn set_headers(&self, meta: StoredResponseMeta) {
        let mut state = self.state.lock().await;
        state.meta = Some(meta);
        drop(state);
        self.notify.notify_waiters();
    }

    pub async fn advance(&self, bytes: usize) {
        let mut state = self.state.lock().await;
        state.bytes_written += bytes as u64;
        drop(state);
        self.notify.notify_waiters();
    }

    pub async fn finish(&self) {
        let mut state = self.state.lock().await;
        state.complete = true;
        drop(state);
        self.notify.notify_waiters();
    }

    pub async fn fail(&self, error: String, cleanup: bool) {
        let mut state = self.state.lock().await;
        state.complete = true;
        state.error = Some(error);
        state.remove_when_idle = cleanup;
        drop(state);
        self.notify.notify_waiters();
    }

    pub async fn mark_cleanup(&self) {
        let mut state = self.state.lock().await;
        state.remove_when_idle = true;
    }

    pub async fn reader_guard(self: &Arc<Self>) -> ReaderGuard {
        let mut state = self.state.lock().await;
        state.readers += 1;
        ReaderGuard {
            inflight: self.clone(),
        }
    }
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        let inflight = self.inflight.clone();
        tokio::spawn(async move {
            let mut state = inflight.state.lock().await;
            state.readers = state.readers.saturating_sub(1);
            let remove = state.readers == 0 && state.remove_when_idle;
            let temp = inflight.temp_path.clone();
            drop(state);
            if remove {
                let _ = fs::remove_file(temp).await;
            }
        });
    }
}

pub struct InflightSnapshot {
    pub bytes_written: u64,
    pub complete: bool,
    pub error: Option<String>,
}

struct CompletedEntry {
    body: PathBuf,
    meta: PathBuf,
    modified: SystemTime,
    size: u64,
}
