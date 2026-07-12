use crate::config::Config;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::SystemTime;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc};

const FOOTER_LEN: usize = 4;
const MAX_META_LEN: usize = 1 << 20;
const CACHE_MARKER: &str = ".vampire-cache-v1";
const MIB: usize = 1024 * 1024;
const METADATA_MEMORY_BUDGET_MIB: usize = 1024;

pub struct CacheStore {
    root: PathBuf,
    max_cache_size: u64,
    inflight: Mutex<HashMap<String, Arc<Inflight>>>,
    pins: Arc<StdMutex<HashMap<String, Weak<()>>>>,
    eviction_lock: Arc<Mutex<()>>,
    eviction_tx: mpsc::Sender<Arc<std::fs::File>>,
    temp_counter: AtomicU64,
    upstream_semaphore: Arc<Semaphore>,
    metadata_memory_semaphore: Arc<Semaphore>,
    directory_lock: Arc<std::fs::File>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredResponseMeta {
    pub headers: Vec<(String, String)>,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
    pub status: u16,
}

pub struct StoredEntry {
    file: fs::File,
    pub body_len: u64,
    pub meta: StoredResponseMeta,
}

impl StoredEntry {
    pub async fn read_body(&mut self) -> io::Result<Bytes> {
        self.file.seek(io::SeekFrom::Start(0)).await?;
        let body_len = usize::try_from(self.body_len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "body length overflow"))?;
        let mut body = vec![0u8; body_len];
        self.file.read_exact(&mut body).await?;
        Ok(Bytes::from(body))
    }

    pub(crate) fn into_parts(self) -> (fs::File, u64, StoredResponseMeta) {
        (self.file, self.body_len, self.meta)
    }
}

pub struct ArtifactLeader {
    pub inflight: Arc<Inflight>,
    pub key: String,
    pub paths: CachePaths,
    _permit: OwnedSemaphorePermit,
}

pub enum ArtifactLookup {
    Hit(StoredEntry),
    Join(Arc<Inflight>),
    Leader(ArtifactLeader),
}

pub struct MetadataLeader {
    pub inflight: Arc<Inflight>,
    pub key: String,
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct MetadataMemoryReservation {
    semaphore: Arc<Semaphore>,
    permit: Option<OwnedSemaphorePermit>,
}

pub enum MetadataLookup {
    Join(Arc<Inflight>),
    Leader(MetadataLeader),
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
    Cached(PublishedEntry),
    Response(StoredResponseMeta, Bytes),
    Failed(io::ErrorKind, String),
}

#[derive(Clone)]
pub struct PublishedEntry {
    _inner: Arc<PublishedEntryInner>,
}

struct PublishedEntryInner {
    key: String,
    pins: Arc<StdMutex<HashMap<String, Weak<()>>>>,
    eviction_tx: mpsc::Sender<Arc<std::fs::File>>,
    directory_lock: Arc<std::fs::File>,
    pin: Arc<()>,
}

struct MetadataTempCleanup {
    path: PathBuf,
    directory_lock: Arc<std::fs::File>,
    armed: bool,
}

impl CacheStore {
    pub async fn new(config: &Config) -> io::Result<Self> {
        config
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        fs::create_dir_all(&config.cache_dir).await?;
        claim_cache_directory(&config.cache_dir).await?;
        let directory_lock = Arc::new(lock_cache_directory(&config.cache_dir).await?);
        let pins = Arc::new(StdMutex::new(HashMap::new()));
        let eviction_lock = Arc::new(Mutex::new(()));
        let (eviction_tx, mut eviction_rx) = mpsc::channel(1);
        let janitor_root = config.cache_dir.clone();
        let janitor_pins = pins.clone();
        let janitor_lock = eviction_lock.clone();
        let janitor_max_cache_size = config.max_cache_size;
        tokio::spawn(async move {
            while let Some(directory_lock) = eviction_rx.recv().await {
                let _directory_lock = directory_lock;
                let _guard = janitor_lock.lock().await;
                while eviction_rx.try_recv().is_ok() {}
                let _ = evict_files_to_bound(&janitor_root, janitor_max_cache_size, &janitor_pins)
                    .await;
            }
        });
        let store = Self {
            root: config.cache_dir.clone(),
            max_cache_size: config.max_cache_size,
            inflight: Mutex::new(HashMap::new()),
            pins,
            eviction_lock,
            eviction_tx,
            temp_counter: AtomicU64::new(0),
            upstream_semaphore: Arc::new(Semaphore::new(config.max_upstream_fetches)),
            metadata_memory_semaphore: Arc::new(Semaphore::new(METADATA_MEMORY_BUDGET_MIB)),
            directory_lock,
        };
        store.cleanup_stale_and_legacy().await?;
        store.evict_to_bound().await?;
        Ok(store)
    }

    pub fn artifact_key(upstream: &str) -> String {
        hash_key("artifact", upstream, None)
    }

    pub fn metadata_key(upstream: &str, representation: &str) -> String {
        hash_key("metadata", upstream, Some(representation))
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
                file,
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
        let permit = self.try_acquire_upstream_permit()?;
        let inflight = Arc::new(Inflight::new());
        inflight_map.insert(key.clone(), inflight.clone());
        Ok(ArtifactLookup::Leader(ArtifactLeader {
            inflight,
            key,
            paths,
            _permit: permit,
        }))
    }

    pub async fn lookup_or_start_metadata(&self, key: String) -> io::Result<MetadataLookup> {
        let mut inflight_map = self.inflight.lock().await;
        if let Some(existing) = inflight_map.get(&key) {
            return Ok(MetadataLookup::Join(existing.clone()));
        }
        let permit = self.try_acquire_upstream_permit()?;
        let inflight = Arc::new(Inflight::new());
        inflight_map.insert(key.clone(), inflight.clone());
        Ok(MetadataLookup::Leader(MetadataLeader {
            inflight,
            key,
            _permit: permit,
        }))
    }

    pub(crate) fn metadata_memory_reservation(&self) -> MetadataMemoryReservation {
        MetadataMemoryReservation {
            semaphore: self.metadata_memory_semaphore.clone(),
            permit: None,
        }
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
        let meta_bytes = serialize_meta(meta)?;
        let meta_len = u32::try_from(meta_bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata too large"))?;
        let mut cleanup = MetadataTempCleanup {
            path: temp_path.clone(),
            directory_lock: self.directory_lock.clone(),
            armed: true,
        };
        let mut file = fs::File::create(&temp_path).await?;
        file.write_all(body).await?;
        file.write_all(&meta_bytes).await?;
        file.write_all(&meta_len.to_be_bytes()).await?;
        file.flush().await?;
        drop(file);
        let _guard = self.eviction_lock.lock().await;
        fs::rename(&temp_path, &paths.body).await?;
        cleanup.armed = false;
        evict_files_to_bound(&self.root, self.max_cache_size, &self.pins).await?;
        Ok(())
    }

    pub async fn commit_artifact(
        &self,
        key: &str,
        meta: &StoredResponseMeta,
        temp_path: &Path,
    ) -> io::Result<PublishedEntry> {
        let paths = self.paths_for(key);
        let meta_bytes = serialize_meta(meta)?;
        let meta_len = u32::try_from(meta_bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata too large"))?;
        let mut file = fs::OpenOptions::new().append(true).open(temp_path).await?;
        file.write_all(&meta_bytes).await?;
        file.write_all(&meta_len.to_be_bytes()).await?;
        drop(file);
        let published = self.pin(key);
        fs::rename(temp_path, &paths.body).await?;
        Ok(published)
    }

    pub fn try_acquire_upstream_permit(&self) -> io::Result<OwnedSemaphorePermit> {
        self.upstream_semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "upstream capacity exhausted"))
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

    fn pin(&self, key: &str) -> PublishedEntry {
        let pin = Arc::new(());
        self.pins
            .lock()
            .expect("cache pin lock")
            .insert(key.to_owned(), Arc::downgrade(&pin));
        PublishedEntry {
            _inner: Arc::new(PublishedEntryInner {
                key: key.to_owned(),
                pins: self.pins.clone(),
                eviction_tx: self.eviction_tx.clone(),
                directory_lock: self.directory_lock.clone(),
                pin,
            }),
        }
    }

    async fn cleanup_stale_and_legacy(&self) -> io::Result<()> {
        for (shard, dir) in shard_directories(&self.root).await? {
            let mut entries = fs::read_dir(dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                if !entry.file_type().await?.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if legacy_key(name, &shard).is_some() || temp_key(name, &shard).is_some() {
                    let _ = fs::remove_file(entry.path()).await;
                }
            }
        }
        Ok(())
    }

    async fn evict_to_bound(&self) -> io::Result<()> {
        let _guard = self.eviction_lock.lock().await;
        evict_files_to_bound(&self.root, self.max_cache_size, &self.pins).await
    }

    #[cfg(test)]
    async fn completed_entries(&self) -> io::Result<Vec<CompletedEntry>> {
        completed_entries_at(&self.root).await
    }
}

impl MetadataMemoryReservation {
    pub(crate) fn reserve(&mut self, bytes: usize) -> io::Result<()> {
        let required = bytes.div_ceil(MIB).max(1);
        let held = self
            .permit
            .as_ref()
            .map_or(0, OwnedSemaphorePermit::num_permits);
        if required <= held {
            return Ok(());
        }
        let additional = u32::try_from(required - held).map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "metadata memory capacity exhausted",
            )
        })?;
        let permit = self
            .semaphore
            .clone()
            .try_acquire_many_owned(additional)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "metadata memory capacity exhausted",
                )
            })?;
        if let Some(current) = self.permit.as_mut() {
            current.merge(permit);
        } else {
            self.permit = Some(permit);
        }
        Ok(())
    }

    pub(crate) fn into_permit(mut self) -> OwnedSemaphorePermit {
        self.permit.take().expect("metadata memory reservation")
    }
}

impl Drop for PublishedEntryInner {
    fn drop(&mut self) {
        let mut pins = self.pins.lock().expect("cache pin lock");
        let owns_pin = pins
            .get(&self.key)
            .and_then(Weak::upgrade)
            .is_some_and(|pin| Arc::ptr_eq(&pin, &self.pin));
        if owns_pin {
            pins.remove(&self.key);
        }
        drop(pins);
        if owns_pin {
            let _ = self.eviction_tx.try_send(self.directory_lock.clone());
        }
    }
}

impl Drop for MetadataTempCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let path = self.path.clone();
        let directory_lock = self.directory_lock.clone();
        runtime.spawn(async move {
            let _directory_lock = directory_lock;
            let _ = fs::remove_file(path).await;
        });
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

    pub async fn finish_cached(&self, entry: PublishedEntry) {
        self.complete(InflightOutcome::Cached(entry)).await;
    }

    pub async fn finish_response(&self, meta: StoredResponseMeta, body: Bytes) {
        self.complete(InflightOutcome::Response(meta, body)).await;
    }

    pub async fn fail(&self, kind: io::ErrorKind, error: String) {
        self.complete(InflightOutcome::Failed(kind, error)).await;
    }

    async fn complete(&self, outcome: InflightOutcome) {
        let mut current = self.outcome.lock().await;
        if current.is_some() {
            return;
        }
        *current = Some(outcome);
        drop(current);
        self.notify.notify_waiters();
    }
}

struct CompletedEntry {
    key: String,
    body: PathBuf,
    modified: SystemTime,
    size: u64,
}

async fn claim_cache_directory(root: &Path) -> io::Result<()> {
    let marker = root.join(CACHE_MARKER);
    match fs::read(&marker).await {
        Ok(contents) if contents == b"v1\n" => return Ok(()),
        Ok(_) => return Err(unowned_cache_directory()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut shards = fs::read_dir(root).await?;
    while let Some(shard) = shards.next_entry().await? {
        let shard_type = shard.file_type().await?;
        let shard_name = shard.file_name();
        let Some(shard_name) = shard_name
            .to_str()
            .filter(|name| shard_type.is_dir() && valid_hex(name, 2))
        else {
            return Err(unowned_cache_directory());
        };
        let mut entries = fs::read_dir(shard.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                return Err(unowned_cache_directory());
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(unowned_cache_directory());
            };
            if !valid_key(name, shard_name)
                && legacy_key(name, shard_name).is_none()
                && temp_key(name, shard_name).is_none()
            {
                return Err(unowned_cache_directory());
            }
        }
    }
    fs::write(marker, b"v1\n").await
}

async fn lock_cache_directory(root: &Path) -> io::Result<std::fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(CACHE_MARKER))
        .await?
        .into_std()
        .await;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cache directory is already in use",
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn unowned_cache_directory() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "cache directory is nonempty and not owned by vampire",
    )
}

async fn shard_directories(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut entries = match fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(error),
    };
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(shard) = name.to_str().filter(|name| valid_hex(name, 2)) else {
            continue;
        };
        out.push((shard.to_owned(), entry.path()));
    }
    Ok(out)
}

fn valid_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_key(key: &str, shard: &str) -> bool {
    valid_hex(key, 64) && key.starts_with(shard)
}

fn legacy_key<'a>(name: &'a str, shard: &str) -> Option<&'a str> {
    let key = name
        .strip_suffix(".json")
        .or_else(|| name.strip_suffix(".body"))?;
    valid_key(key, shard).then_some(key)
}

fn temp_key<'a>(name: &'a str, shard: &str) -> Option<&'a str> {
    let without_suffix = name.strip_suffix(".part")?;
    if valid_key(without_suffix, shard) {
        return Some(without_suffix);
    }
    let (key, counter) = without_suffix.rsplit_once(".part.")?;
    (valid_key(key, shard)
        && !counter.is_empty()
        && counter.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some(key)
}

async fn completed_entries_at(root: &Path) -> io::Result<Vec<CompletedEntry>> {
    let mut out = Vec::new();
    for (shard, dir) in shard_directories(root).await? {
        let mut entries = fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(key) = name.to_str().filter(|name| valid_key(name, &shard)) else {
                continue;
            };
            let path = entry.path();
            let metadata = match entry.metadata().await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            out.push(CompletedEntry {
                key: key.to_owned(),
                body: path,
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size: metadata.len(),
            });
        }
    }
    Ok(out)
}

fn is_pinned(pins: &StdMutex<HashMap<String, Weak<()>>>, key: &str) -> bool {
    let mut pins = pins.lock().expect("cache pin lock");
    pins.retain(|_, pin| pin.strong_count() > 0);
    pins.get(key).is_some_and(|pin| pin.strong_count() > 0)
}

async fn evict_files_to_bound(
    root: &Path,
    max_cache_size: u64,
    pins: &StdMutex<HashMap<String, Weak<()>>>,
) -> io::Result<()> {
    let mut entries = completed_entries_at(root).await?;
    let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
    if total <= max_cache_size {
        return Ok(());
    }
    entries.sort_by_key(|entry| entry.modified);
    for entry in entries {
        if total <= max_cache_size {
            break;
        }
        if is_pinned(pins, &entry.key) {
            continue;
        }
        match fs::remove_file(&entry.body).await {
            Ok(()) => total = total.saturating_sub(entry.size),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                total = total.saturating_sub(entry.size);
            }
            Err(_) => {}
        }
    }
    Ok(())
}

fn hash_key(class: &str, upstream: &str, representation: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(class.as_bytes());
    hasher.update([0]);
    hasher.update(upstream.as_bytes());
    hasher.update([0]);
    if let Some(representation) = representation {
        hasher.update(representation.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn serialize_meta(meta: &StoredResponseMeta) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(meta)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() > MAX_META_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata too large",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
fn pack_footer(body: &[u8], meta: &StoredResponseMeta) -> io::Result<Vec<u8>> {
    let meta_bytes = serialize_meta(meta)?;
    let meta_len = u32::try_from(meta_bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata too large"))?;
    let mut packed = Vec::with_capacity(body.len() + meta_bytes.len() + FOOTER_LEN);
    packed.extend_from_slice(body);
    packed.extend_from_slice(&meta_bytes);
    packed.extend_from_slice(&meta_len.to_be_bytes());
    Ok(packed)
}

fn footer_geometry(total_size: u64, footer: [u8; FOOTER_LEN]) -> io::Result<(u64, usize)> {
    let meta_len = usize::try_from(u32::from_be_bytes(footer))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata length overflow"))?;
    if meta_len > MAX_META_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry metadata length exceeds sanity cap",
        ));
    }
    let available = total_size - FOOTER_LEN as u64;
    let meta_len_u64 = u64::try_from(meta_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata length overflow"))?;
    if meta_len_u64 > available {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry metadata length exceeds file size",
        ));
    }
    Ok((available - meta_len_u64, meta_len))
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
    use super::{
        ArtifactLookup, CacheStore, Inflight, InflightOutcome, MAX_META_LEN, MIB,
        StoredResponseMeta, pack_footer,
    };
    use crate::config::Config;
    use bytes::Bytes;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::time::{Duration, timeout};

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
        let key = CacheStore::metadata_key("https://registry.npmjs.org/pkg", "raw:v1");
        let meta = sample_meta();
        store.store_metadata(&key, b"hello", &meta).await.unwrap();
        let mut loaded = store.load(&key).await.unwrap().unwrap();
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
    async fn legacy_and_temp_files_are_cleaned_on_startup() {
        let temp = tempdir().unwrap();
        let key = CacheStore::artifact_key("https://example.com/legacy");
        let shard_dir = temp.path().join(&key[..2]);
        fs::create_dir_all(&shard_dir).await.unwrap();
        let legacy_json = shard_dir.join(format!("{key}.json"));
        let legacy_body = shard_dir.join(format!("{key}.body"));
        let artifact_temp = shard_dir.join(format!("{key}.part"));
        let metadata_temp = shard_dir.join(format!("{key}.part.0.part"));
        fs::write(&legacy_json, b"legacy meta").await.unwrap();
        fs::write(&legacy_body, b"legacy body").await.unwrap();
        fs::write(&artifact_temp, b"partial artifact")
            .await
            .unwrap();
        fs::write(&metadata_temp, b"partial metadata")
            .await
            .unwrap();
        let _store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        assert!(fs::metadata(&legacy_json).await.is_err());
        assert!(fs::metadata(&legacy_body).await.is_err());
        assert!(fs::metadata(&artifact_temp).await.is_err());
        assert!(fs::metadata(&metadata_temp).await.is_err());
    }

    #[tokio::test]
    async fn cleanup_and_eviction_ignore_unrelated_files() {
        let temp = tempdir().unwrap();
        let mut config = config_for(temp.path());
        config.max_cache_size = 1;
        drop(CacheStore::new(&config).await.unwrap());
        let shard_dir = temp.path().join("ab");
        fs::create_dir_all(&shard_dir).await.unwrap();
        let json = shard_dir.join("notes.json");
        let extensionless = shard_dir.join("important");
        let fake_entry = shard_dir.join(format!("{}x", "a".repeat(63)));
        fs::write(&json, b"notes").await.unwrap();
        fs::write(&extensionless, b"important").await.unwrap();
        fs::write(&fake_entry, b"not a cache key").await.unwrap();
        let store = CacheStore::new(&config).await.unwrap();
        let key = CacheStore::metadata_key("https://example.com/a", "raw:v1");
        store
            .store_metadata(&key, b"metadata", &sample_meta())
            .await
            .unwrap();
        assert_eq!(fs::read(json).await.unwrap(), b"notes");
        assert_eq!(fs::read(extensionless).await.unwrap(), b"important");
        assert_eq!(fs::read(fake_entry).await.unwrap(), b"not a cache key");
    }

    #[tokio::test]
    async fn rejects_unowned_nonempty_cache_directory() {
        let temp = tempdir().unwrap();
        let unrelated = temp.path().join("notes.json");
        fs::write(&unrelated, b"notes").await.unwrap();
        let Err(error) = CacheStore::new(&config_for(temp.path())).await else {
            panic!("unowned cache directory was accepted");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(unrelated).await.unwrap(), b"notes");
    }

    #[tokio::test]
    async fn cache_directory_has_one_owner() {
        let temp = tempdir().unwrap();
        let config = config_for(temp.path());
        let first = CacheStore::new(&config).await.unwrap();
        let Err(error) = CacheStore::new(&config).await else {
            panic!("locked cache directory was accepted");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        drop(first);
        CacheStore::new(&config).await.unwrap();
    }

    #[tokio::test]
    async fn writers_reject_unreadable_metadata_footer() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        let mut meta = sample_meta();
        meta.headers = vec![("x-large".to_owned(), "a".repeat(MAX_META_LEN))];
        let metadata_key = CacheStore::metadata_key("https://example.com/meta", "raw:v1");
        let error = store
            .store_metadata(&metadata_key, b"body", &meta)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let artifact_key = CacheStore::artifact_key("https://example.com/artifact");
        let paths = store.paths_for(&artifact_key);
        fs::create_dir_all(paths.temp.parent().unwrap())
            .await
            .unwrap();
        fs::write(&paths.temp, b"body").await.unwrap();
        let Err(error) = store
            .commit_artifact(&artifact_key, &meta, &paths.temp)
            .await
        else {
            panic!("oversized artifact metadata was accepted");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn failed_metadata_write_removes_temp_file() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        let key = CacheStore::metadata_key("https://example.com/meta", "raw:v1");
        let paths = store.paths_for(&key);
        fs::create_dir_all(&paths.body).await.unwrap();
        assert!(
            store
                .store_metadata(&key, b"body", &sample_meta())
                .await
                .is_err()
        );
        timeout(Duration::from_secs(1), async {
            loop {
                let mut entries = fs::read_dir(paths.body.parent().unwrap()).await.unwrap();
                let mut found_temp = false;
                while let Some(entry) = entries.next_entry().await.unwrap() {
                    let name = entry.file_name();
                    found_temp |= name.to_string_lossy().contains(".part");
                }
                if !found_temp {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("metadata temp file was not removed");
    }

    #[tokio::test]
    async fn loaded_entry_keeps_original_inode_after_replacement() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        let key = CacheStore::metadata_key("https://example.com/a", "raw:v1");
        let meta = sample_meta();
        store.store_metadata(&key, b"first", &meta).await.unwrap();
        let mut first = store.load(&key).await.unwrap().unwrap();
        store.store_metadata(&key, b"second", &meta).await.unwrap();
        assert_eq!(
            first.read_body().await.unwrap(),
            Bytes::from_static(b"first")
        );
        let mut second = store.load(&key).await.unwrap().unwrap();
        assert_eq!(
            second.read_body().await.unwrap(),
            Bytes::from_static(b"second")
        );
    }

    #[tokio::test]
    async fn metadata_publication_waits_for_eviction_lock() {
        let temp = tempdir().unwrap();
        let store = Arc::new(CacheStore::new(&config_for(temp.path())).await.unwrap());
        let key = CacheStore::metadata_key("https://example.com/a", "raw:v1");
        let meta = sample_meta();
        store.store_metadata(&key, b"first", &meta).await.unwrap();
        let guard = store.eviction_lock.lock().await;
        let task_store = store.clone();
        let task_key = key.clone();
        let task_meta = meta.clone();
        let task = tokio::spawn(async move {
            task_store
                .store_metadata(&task_key, b"second", &task_meta)
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut entry = store.load(&key).await.unwrap().unwrap();
        assert_eq!(
            entry.read_body().await.unwrap(),
            Bytes::from_static(b"first")
        );
        drop(guard);
        task.await.unwrap().unwrap();
        let mut entry = store.load(&key).await.unwrap().unwrap();
        assert_eq!(
            entry.read_body().await.unwrap(),
            Bytes::from_static(b"second")
        );
    }

    #[tokio::test]
    async fn deferred_eviction_burst_coalesces() {
        let temp = tempdir().unwrap();
        let mut config = config_for(temp.path());
        config.max_cache_size = 1;
        let store = CacheStore::new(&config).await.unwrap();
        let mut publications = Vec::new();
        for index in 0..32 {
            let key = CacheStore::artifact_key(&format!("https://example.com/{index}"));
            let paths = store.paths_for(&key);
            fs::create_dir_all(paths.temp.parent().unwrap())
                .await
                .unwrap();
            fs::write(&paths.temp, b"artifact").await.unwrap();
            publications.push(
                store
                    .commit_artifact(&key, &sample_meta(), &paths.temp)
                    .await
                    .unwrap(),
            );
        }
        let guard = store.eviction_lock.lock().await;
        drop(publications);
        assert_eq!(store.eviction_tx.capacity(), 0);
        drop(guard);
        timeout(Duration::from_secs(2), async {
            loop {
                if store.completed_entries().await.unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("deferred eviction did not reach the cache bound");
    }

    #[tokio::test]
    async fn unique_work_is_admitted_before_leader_creation() {
        let temp = tempdir().unwrap();
        let mut config = config_for(temp.path());
        config.max_upstream_fetches = 1;
        let store = CacheStore::new(&config).await.unwrap();
        let first_key = CacheStore::artifact_key("https://example.com/a");
        let first = match store
            .lookup_or_start_artifact(first_key.clone())
            .await
            .unwrap()
        {
            ArtifactLookup::Leader(leader) => leader,
            ArtifactLookup::Hit(_) | ArtifactLookup::Join(_) => panic!("expected leader"),
        };
        assert!(matches!(
            store
                .lookup_or_start_artifact(first_key.clone())
                .await
                .unwrap(),
            ArtifactLookup::Join(_)
        ));
        let metadata_key = CacheStore::metadata_key("https://example.com/b", "raw:v1");
        let Err(error) = store.lookup_or_start_metadata(metadata_key).await else {
            panic!("expected admission failure");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        first
            .inflight
            .fail(io::ErrorKind::Other, "test cleanup".to_owned())
            .await;
        store.finish_inflight(&first.key).await;
    }

    #[tokio::test]
    async fn buffered_metadata_has_a_separate_memory_budget() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        let mut first = store.metadata_memory_reservation();
        first.reserve(512 * MIB).unwrap();
        let mut second = store.metadata_memory_reservation();
        second.reserve(512 * MIB).unwrap();
        let mut third = store.metadata_memory_reservation();
        let error = third.reserve(1).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("memory"));
        drop(first);
        third.reserve(1).unwrap();
    }

    #[tokio::test]
    async fn inflight_completion_is_first_writer_wins() {
        let inflight = Inflight::new();
        inflight
            .finish_response(sample_meta(), Bytes::from_static(b"success"))
            .await;
        inflight
            .fail(io::ErrorKind::Other, "late failure".to_owned())
            .await;
        match inflight.wait_for_outcome().await.unwrap() {
            InflightOutcome::Response(_, body) => {
                assert_eq!(body, Bytes::from_static(b"success"));
            }
            InflightOutcome::Cached(_) | InflightOutcome::Failed(_, _) => {
                panic!("completed inflight was overwritten")
            }
        }
    }

    #[test]
    fn metadata_key_includes_representation() {
        let upstream = "https://example.com/a";
        let raw = CacheStore::metadata_key(upstream, "raw:v1");
        let npm_a = CacheStore::metadata_key(upstream, "npm:v2:https://a.example");
        let npm_b = CacheStore::metadata_key(upstream, "npm:v2:https://b.example");
        assert_ne!(raw, npm_a);
        assert_ne!(npm_a, npm_b);
    }

    #[tokio::test]
    async fn evict_counts_single_file_size() {
        let temp = tempdir().unwrap();
        let store = CacheStore::new(&config_for(temp.path())).await.unwrap();
        let meta = sample_meta();
        let key1 = CacheStore::metadata_key("https://example.com/a", "raw:v1");
        let key2 = CacheStore::metadata_key("https://example.com/b", "raw:v1");
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
