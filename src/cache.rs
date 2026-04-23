use crate::config::Config;
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
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

const FOOTER_LEN: usize = 4;
const MAX_META_LEN: u64 = 1 << 20;

pub struct CacheStore {
    root: PathBuf,
    max_cache_size: u64,
    inflight: Mutex<HashMap<String, Arc<Inflight>>>,
    temp_counter: AtomicU64,
    upstream_semaphore: Arc<Semaphore>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredResponseMeta {
    pub headers: Vec<(String, String)>,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
    pub status: u16,
}

#[derive(Clone, Debug)]
pub struct StoredEntry {
    pub body_path: PathBuf,
    pub body_len: u64,
    pub meta: StoredResponseMeta,
}

impl StoredEntry {
    pub async fn read_body(&self) -> io::Result<Bytes> {
        let mut file = fs::File::open(&self.body_path).await?;
        let mut body = vec![0u8; usize::try_from(self.body_len).unwrap()];
        file.read_exact(&mut body).await?;
        Ok(Bytes::from(body))
    }
}

pub struct ArtifactLeader {
    pub inflight: Arc<Inflight>,
    pub key: String,
    pub paths: CachePaths,
}

pub enum ArtifactLookup {
    Hit(StoredEntry),
    Join(Arc<Inflight>),
    Leader(ArtifactLeader),
}

pub struct CachePaths {
    pub body: PathBuf,
    pub temp: PathBuf,
}

pub struct Inflight {
    outcome: Mutex<Option<InflightOutcome>>,
    notify: Notify,
}

#[derive(Clone)]
pub enum InflightOutcome {
    Cached,
    Response(StoredResponseMeta, Bytes),
    Failed(String),
}

impl CacheStore {
    pub async fn new(config: &Config) -> io::Result<Self> {
        fs::create_dir_all(&config.cache_dir).await?;
        let store = Self {
            root: config.cache_dir.clone(),
            max_cache_size: config.max_cache_size,
            inflight: Mutex::new(HashMap::new()),
            temp_counter: AtomicU64::new(0),
            upstream_semaphore: Arc::new(Semaphore::new(config.max_upstream_fetches)),
        };
        store
            .cleanup_stale_and_legacy(Duration::from_mins(5))
            .await?;
        store.evict_to_bound().await?;
        Ok(store)
    }

    pub fn artifact_key(upstream: &str) -> String {
        hash_key("artifact", upstream)
    }

    pub fn metadata_key(upstream: &str) -> String {
        hash_key("metadata", upstream)
    }

    pub async fn load(&self, key: &str) -> io::Result<Option<StoredEntry>> {
        let paths = self.paths_for(key);
        let mut file = match fs::File::open(&paths.body).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        match read_footer(&mut file).await {
            Ok((meta, body_len)) => Ok(Some(StoredEntry {
                body_path: paths.body,
                body_len,
                meta,
            })),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn lookup_or_start_artifact(&self, key: String) -> io::Result<ArtifactLookup> {
        {
            let inflight_map = self.inflight.lock().await;
            if let Some(existing) = inflight_map.get(&key) {
                return Ok(ArtifactLookup::Join(existing.clone()));
            }
        }
        if let Some(entry) = self.load(&key).await? {
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
    ) -> io::Result<()> {
        let paths = self.paths_for(key);
        if let Some(parent) = paths.body.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temp_path = self.unique_temp_path(&paths.temp);
        let packed = pack_footer(body, meta)?;
        fs::write(&temp_path, packed).await?;
        fs::rename(&temp_path, &paths.body).await?;
        self.evict_to_bound().await?;
        Ok(())
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
        let meta_len = u32::try_from(meta_bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata too large"))?;
        let mut file = fs::OpenOptions::new().append(true).open(temp_path).await?;
        file.write_all(&meta_bytes).await?;
        file.write_all(&meta_len.to_be_bytes()).await?;
        drop(file);
        fs::rename(temp_path, &paths.body).await?;
        self.evict_to_bound().await?;
        Ok(())
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

    pub(crate) fn paths_for(&self, key: &str) -> CachePaths {
        let shard = &key[..2];
        let dir = self.root.join(shard);
        CachePaths {
            body: dir.join(key),
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

    async fn cleanup_stale_and_legacy(&self, max_age: Duration) -> io::Result<()> {
        let now = SystemTime::now();
        let mut stack = vec![self.root.clone()];
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
                match path.extension().and_then(|extension| extension.to_str()) {
                    Some("json" | "body") => {
                        let _ = fs::remove_file(path).await;
                    }
                    Some("part") => {
                        let age = now
                            .duration_since(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH))
                            .unwrap_or_default();
                        if age >= max_age {
                            let _ = fs::remove_file(path).await;
                        }
                    }
                    _ => {}
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
            total = total.saturating_sub(entry.size);
        }
        Ok(())
    }

    async fn completed_entries(&self) -> io::Result<Vec<CompletedEntry>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
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
                if path.extension().is_some() {
                    continue;
                }
                out.push(CompletedEntry {
                    body: path,
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    size: metadata.len(),
                });
            }
        }
        Ok(out)
    }
}

impl Inflight {
    pub(crate) fn new() -> Self {
        Self {
            outcome: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    pub async fn wait_for_outcome(&self) -> io::Result<InflightOutcome> {
        loop {
            let notified = self.notify.notified();
            let state = self.outcome.lock().await;
            if let Some(outcome) = state.as_ref() {
                return Ok(outcome.clone());
            }
            drop(state);
            notified.await;
        }
    }

    pub async fn finish_cached(&self) {
        *self.outcome.lock().await = Some(InflightOutcome::Cached);
        self.notify.notify_waiters();
    }

    pub async fn finish_response(&self, meta: StoredResponseMeta, body: Bytes) {
        *self.outcome.lock().await = Some(InflightOutcome::Response(meta, body));
        self.notify.notify_waiters();
    }

    pub async fn fail(&self, error: String) {
        *self.outcome.lock().await = Some(InflightOutcome::Failed(error));
        self.notify.notify_waiters();
    }
}

struct CompletedEntry {
    body: PathBuf,
    modified: SystemTime,
    size: u64,
}

fn hash_key(class: &str, upstream: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(class.as_bytes());
    hasher.update([0]);
    hasher.update(upstream.as_bytes());
    hasher.update([0]);
    hex::encode(hasher.finalize())
}

fn pack_footer(body: &[u8], meta: &StoredResponseMeta) -> io::Result<Vec<u8>> {
    let meta_bytes = serde_json::to_vec(meta)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let meta_len = u32::try_from(meta_bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata too large"))?;
    let mut packed = Vec::with_capacity(body.len() + meta_bytes.len() + FOOTER_LEN);
    packed.extend_from_slice(body);
    packed.extend_from_slice(&meta_bytes);
    packed.extend_from_slice(&meta_len.to_be_bytes());
    Ok(packed)
}

fn footer_geometry(total_size: u64, footer: [u8; FOOTER_LEN]) -> io::Result<(u64, usize)> {
    let meta_len = u64::from(u32::from_be_bytes(footer));
    if meta_len > MAX_META_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry metadata length exceeds sanity cap",
        ));
    }
    let available = total_size - FOOTER_LEN as u64;
    if meta_len > available {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry metadata length exceeds file size",
        ));
    }
    let meta_len_usize = usize::try_from(meta_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata length overflow"))?;
    Ok((available - meta_len, meta_len_usize))
}

fn parse_meta(bytes: &[u8]) -> io::Result<StoredResponseMeta> {
    serde_json::from_slice::<StoredResponseMeta>(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn short_footer_err() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "cache entry shorter than footer",
    )
}

async fn read_footer(file: &mut fs::File) -> io::Result<(StoredResponseMeta, u64)> {
    let size = file.metadata().await?.len();
    if size < FOOTER_LEN as u64 {
        return Err(short_footer_err());
    }
    file.seek(io::SeekFrom::Start(size - FOOTER_LEN as u64))
        .await?;
    let mut footer = [0u8; FOOTER_LEN];
    file.read_exact(&mut footer).await?;
    let (body_len, meta_len) = footer_geometry(size, footer)?;
    file.seek(io::SeekFrom::Start(body_len)).await?;
    let mut meta_bytes = vec![0u8; meta_len];
    file.read_exact(&mut meta_bytes).await?;
    Ok((parse_meta(&meta_bytes)?, body_len))
}

#[cfg(test)]
mod tests {
    use super::{CacheStore, MAX_META_LEN, StoredResponseMeta, pack_footer};
    use crate::config::Config;
    use bytes::Bytes;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::time::Duration;

    fn config_for(path: &Path) -> Config {
        Config {
            pkg_bind: "127.0.0.1:0".parse().unwrap(),
            git_bind: "127.0.0.1:0".parse().unwrap(),
            management_bind: "127.0.0.1:0".parse().unwrap(),
            public_base_url: "http://127.0.0.1:8080".to_owned(),
            cache_dir: PathBuf::from(path),
            max_cache_size: 1024 * 1024,
            max_upstream_fetches: 4,
            upstream_timeout: Duration::from_secs(5),
        }
    }

    fn sample_meta() -> StoredResponseMeta {
        StoredResponseMeta {
            headers: vec![("content-length".to_owned(), "5".to_owned())],
            last_modified: Some("yesterday".to_owned()),
            etag: Some("\"v1\"".to_owned()),
            status: 200,
        }
    }

    async fn write_raw_body(root: &Path, key: &str, bytes: &[u8]) {
        let shard = &key[..2];
        let dir = root.join(shard);
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(dir.join(key), bytes).await.unwrap();
    }

    #[tokio::test]
    async fn metadata_round_trip_uses_footer_format() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        let key = CacheStore::metadata_key("https://registry.npmjs.org/pkg");
        let meta = sample_meta();
        store.store_metadata(&key, b"hello", &meta).await.unwrap();
        let loaded = store.load(&key).await.unwrap().unwrap();
        assert_eq!(
            loaded.read_body().await.unwrap(),
            Bytes::from_static(b"hello")
        );
        assert_eq!(loaded.meta.headers, meta.headers);
        assert_eq!(loaded.meta.etag, meta.etag);
        let paths = store.paths_for(&key);
        let meta_sibling = paths.body.with_extension("json");
        assert!(fs::metadata(&meta_sibling).await.is_err());
        assert_eq!(
            fs::read(&paths.body).await.unwrap(),
            pack_footer(b"hello", &meta).unwrap()
        );
    }

    #[tokio::test]
    async fn artifact_round_trip_uses_footer_format() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        let key = CacheStore::artifact_key("https://example.com/pkg.tar.gz");
        let paths = store.paths_for(&key);
        fs::create_dir_all(paths.temp.parent().unwrap())
            .await
            .unwrap();
        let body = b"hello artifact world";
        fs::write(&paths.temp, body).await.unwrap();
        let meta = StoredResponseMeta {
            headers: vec![("content-length".to_owned(), body.len().to_string())],
            last_modified: None,
            etag: Some("\"art\"".to_owned()),
            status: 200,
        };
        store
            .commit_artifact(&key, &meta, &paths.temp)
            .await
            .unwrap();
        let loaded = store.load(&key).await.unwrap().unwrap();
        assert_eq!(loaded.body_len, body.len() as u64);
        assert_eq!(loaded.meta.etag, meta.etag);
        assert_eq!(loaded.meta.headers, meta.headers);
        let raw = fs::read(&paths.body).await.unwrap();
        assert_eq!(raw, pack_footer(body, &meta).unwrap());
    }

    #[tokio::test]
    async fn load_returns_none_on_corrupt_footer() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();

        let key_random = CacheStore::artifact_key("https://example.com/1");
        write_raw_body(temp.path(), &key_random, &[0x55u8; 128]).await;
        assert!(store.load(&key_random).await.unwrap().is_none());

        let key_short = CacheStore::artifact_key("https://example.com/2");
        write_raw_body(temp.path(), &key_short, b"hi").await;
        assert!(store.load(&key_short).await.unwrap().is_none());

        let key_overflow = CacheStore::artifact_key("https://example.com/3");
        let mut overflow_bytes = vec![0u8; 20];
        overflow_bytes[16..20].copy_from_slice(&100u32.to_be_bytes());
        write_raw_body(temp.path(), &key_overflow, &overflow_bytes).await;
        assert!(store.load(&key_overflow).await.unwrap().is_none());

        let key_cap = CacheStore::artifact_key("https://example.com/4");
        let oversize = u32::try_from(MAX_META_LEN).unwrap() + 1;
        let mut cap_bytes = vec![0u8; 16];
        cap_bytes[12..16].copy_from_slice(&oversize.to_be_bytes());
        write_raw_body(temp.path(), &key_cap, &cap_bytes).await;
        assert!(store.load(&key_cap).await.unwrap().is_none());

        let key_bad_json = CacheStore::artifact_key("https://example.com/5");
        let fake_meta = b"not json";
        let meta_len = u32::try_from(fake_meta.len()).unwrap();
        let mut bad_bytes = Vec::new();
        bad_bytes.extend_from_slice(b"body");
        bad_bytes.extend_from_slice(fake_meta);
        bad_bytes.extend_from_slice(&meta_len.to_be_bytes());
        write_raw_body(temp.path(), &key_bad_json, &bad_bytes).await;
        assert!(store.load(&key_bad_json).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn legacy_split_format_files_cleaned_on_startup() {
        let temp = tempdir().unwrap();
        let shard_dir = temp.path().join("ab");
        fs::create_dir_all(&shard_dir).await.unwrap();
        let legacy_json = shard_dir.join("ab01abcd.json");
        let legacy_body = shard_dir.join("ab01abcd.body");
        fs::write(&legacy_json, b"legacy meta").await.unwrap();
        fs::write(&legacy_body, b"legacy body").await.unwrap();
        let _store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        assert!(fs::metadata(&legacy_json).await.is_err());
        assert!(fs::metadata(&legacy_body).await.is_err());
    }

    #[tokio::test]
    async fn evict_counts_single_file_size() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        let meta = sample_meta();
        let key1 = CacheStore::metadata_key("https://example.com/a");
        let key2 = CacheStore::metadata_key("https://example.com/b");
        store.store_metadata(&key1, b"aaa", &meta).await.unwrap();
        store
            .store_metadata(&key2, b"bbbbbbbb", &meta)
            .await
            .unwrap();

        let paths1 = store.paths_for(&key1);
        let paths2 = store.paths_for(&key2);
        let file1 = fs::metadata(&paths1.body).await.unwrap().len();
        let file2 = fs::metadata(&paths2.body).await.unwrap().len();

        let entries = store.completed_entries().await.unwrap();
        assert_eq!(entries.len(), 2);
        let sizes: std::collections::HashMap<_, _> =
            entries.into_iter().map(|e| (e.body, e.size)).collect();
        assert_eq!(sizes.get(&paths1.body), Some(&file1));
        assert_eq!(sizes.get(&paths2.body), Some(&file2));
    }
}
