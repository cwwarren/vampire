# Architecture

Vampire is a single-process async Rust HTTP proxy that caches package artifacts and metadata for three registries: PyPI, npm, and Cargo. A second listener on the same process proxies read-only GitHub smart-HTTP traffic for git-pinned package dependencies. Built on tokio + axum + reqwest.

## Module structure

```
main.rs           Entrypoint: load config, bind listeners, build App, serve
lib.rs            Module declarations and public re-exports (App, Config, StatsSnapshot)

app.rs            Serve entrypoint and top-level router composition
state.rs          App state, constructors, shared accessors
proxy.rs          Shared request plumbing, response helpers, artifact fetch orchestration
git.rs            Read-only GitHub smart-HTTP validation and forwarding
cargo.rs          Cargo routes and handlers
pypi.rs           PyPI routes and handlers
npm.rs            npm routes and handlers
cache.rs          Disk cache, inflight dedup, eviction
routes.rs         URL construction, origin validation, metadata rewriting (HTML/JSON)
config.rs         Env var parsing (bind, cache_dir, max_cache_size_mb, etc.)
stats.rs          In-memory fetch counters (artifact/metadata/join/git)
failure_log.rs    Structured JSON error logging to stderr
```

No module has circular dependencies. `routes.rs`, `stats.rs`, `config.rs`, and `failure_log.rs` have no crate-internal imports. `cache.rs` imports only `config`. `state.rs` owns shared state and constructors. `app.rs` depends on `state.rs` plus the registry and git modules to build the server router. `cargo.rs`, `pypi.rs`, `npm.rs`, and `git.rs` depend on `state.rs`; `cargo.rs`, `pypi.rs`, `npm.rs`, and `git.rs` also use shared response/failure behavior from `proxy.rs` where needed. `proxy.rs` owns the shared package fetch/cache behavior and depends on `state.rs`, `cache.rs`, `routes.rs`, and `failure_log.rs`.

## Concurrency model

`App` wraps all shared state in a single `Arc<AppInner>`:

```
App { inner: Arc<AppInner> }

AppInner {
    cache: CacheStore,          // disk cache + inflight map
    client: reqwest::Client,    // connection pool (internally Arc'd)
    stats: AppStats,            // Mutex<StatsInner> (std::sync, not tokio)
    upstreams: RegistryOrigins, // 6 upstream base URLs
}
```

Cloning `App` (which axum does per-request) is a single `Arc` refcount bump.

Synchronization primitives within `CacheStore`:

| Field | Type | Purpose |
|---|---|---|
| `inflight` | `tokio::sync::Mutex<HashMap<String, Arc<Inflight>>>` | Maps cache keys to in-progress artifact fetches |
| `temp_counter` | `AtomicU64` (Relaxed) | Generates unique temp file suffixes |
| `upstream_semaphore` | `Arc<Semaphore>` | Bounds concurrent upstream artifact fetches (default 32). Arc because `acquire_owned()` requires it |

`AppStats` uses `std::sync::Mutex` (not tokio) because the lock is held only for fast HashMap operations, never across await points.

## Request flow

### Routing

`App::serve(pkg_listener, git_listener)` runs two Axum routers over the same shared `App` state.

Package routes accept GET and HEAD on the package listener:

| Path | Type | Handler |
|---|---|---|
| `/cargo/index/config.json` | synthetic | Returns `{"dl": "{VAMPIRE_PUBLIC_BASE_URL}/cargo/api/v1/crates"}` |
| `/cargo/index/{*path}` | metadata | Cargo sparse index entries |
| `/cargo/api/v1/crates/{crate_name}/{version}/download` | artifact | Cargo crate tarballs |
| `/pypi/simple/` | metadata | PyPI simple index root |
| `/pypi/simple/{*project}` | metadata | PyPI simple project page |
| `/pypi/files/{*path}` | artifact | PyPI package files |
| `/npm/{*package}` | metadata | npm packument JSON |
| `/npm/tarballs/{*path}` | artifact | npm tarballs |

The git listener routes every request through the git handler. The git surface is path-based and GitHub-only:

| Path | Type | Handler |
|---|---|---|
| `/{owner}/{repo}.git/info/refs?service=git-upload-pack` | git discovery | Forward to GitHub |
| `/{owner}/{repo}.git/git-upload-pack` | git RPC | Forward to GitHub |

Artifact routes for PyPI and npm encode the upstream path directly in the URL (e.g., `/pypi/files/packages/ab/cd/file.whl`). The proxy joins this path with the known upstream base URL. Path injection is prevented by `join_url`, which rejects absolute paths, `//` prefixes, and full URLs, then validates the resulting origin matches the base.

Git traffic stays uncached and fail-closed. The handler parses the raw request URI, rejects absolute-form targets, URL-userinfo, `git-receive-pack`, doubled slashes, dot segments, encoded repo segments, encoded separators, malformed escapes, and other non-canonical path forms locally, then forwards only accepted `git-upload-pack` requests to `https://github.com`. Upload-pack request bodies are buffered up to 8 MiB before forwarding, while accepted upstream git responses are streamed directly back to the client.

### Metadata path

```
handle_metadata(upstream, rewrite)
  key = SHA256("metadata\0" + upstream_url + "\0")  // hex-encoded, 64 chars
  if cached:
    if has etag or last-modified:
      conditional GET → 304 returns cached, else re-fetch
    else:
      return cached body
  else:
    fetch from upstream
  apply rewrite (None / PyPI HTML / npm JSON)
  if status 200 AND has etag or last-modified:
    store to disk (atomic write)
  return response
```

Metadata is only cached when the upstream provides a cache validator (etag or last-modified). Metadata fetches are NOT gated by the upstream semaphore.

### Artifact path

```
handle_artifact(upstream)
  key = SHA256("artifact\0" + upstream_url + "\0")  // hex-encoded, 64 chars
  lookup_or_start_artifact(key):
    Hit  → stream file from disk
    Join → wait on existing inflight, then serve result
    Leader → spawn background fetch, wait on inflight, then serve result
```

The requesting handler always goes through `serve_inflight` — even the Leader request waits on the `Inflight` outcome rather than getting special treatment.

The background fetch (`run_artifact_fetch`):
1. Acquire upstream semaphore permit
2. GET upstream URL
3. Stream response body to a `<key>.part` temp file
4. Atomic rename: write `<key>.json` meta, rename `.part` to `<key>.body`
5. Signal `Inflight` as `Cached`
6. Remove key from inflight map

On any error or task cancellation, the `ArtifactFetchCleanup` drop guard ensures the inflight is resolved (as a 502 error response) and the key is removed from the map, so joiners are never permanently blocked.

### Git path

Accepted git requests bypass the cache layer entirely.

```
git request
  reject absolute-form, userinfo, CONNECT, invalid path, write RPCs
  accept only GET info/refs?service=git-upload-pack
           and POST git-upload-pack
  forward only Git-Protocol (+ Content-Type on POST)
  stream upstream response back without writing cache entries
```

### HEAD path

Checks the cache (artifact or metadata as appropriate). On hit, returns headers with empty body. On miss, sends a real HEAD to upstream. No caching occurs on the HEAD path.

## Cache storage

### Key derivation

```
hex(SHA256(class + "\0" + upstream_url + "\0"))
```

Where `class` is the literal string `"artifact"` or `"metadata"`. First 2 hex characters are the shard directory name.

### Directory layout

```
<cache_dir>/
  <shard>/              # 2-char hex prefix (256 possible directories)
    <key>.body          # response body (artifact: raw file; metadata: packed format)
    <key>.json          # response metadata (artifact only)
    <key>.part          # temp file during artifact fetch
    <key>.part.N.part   # temp file during metadata write (N = monotonic counter)
```

### Artifact format (two files)

- `<key>.json` — `StoredResponseMeta` serialized as JSON: `{ headers, last_modified, etag, status }`
- `<key>.body` — raw upstream response body (e.g., `.crate`, `.whl`, `.tgz`)

Written in two steps: `fs::write` the `.json`, then `fs::rename` the `.part` to `.body`.

### Metadata format (one file, packed)

`<key>.body` contains:

```
[4 bytes: meta_len as big-endian u32]
[meta_len bytes: StoredResponseMeta as JSON]
[remaining bytes: response body]
```

Written atomically: pack into memory, write to a uniquely-suffixed `.part` temp file, then `fs::rename` to `.body`.

## Inflight dedup

Prevents duplicate upstream fetches when multiple concurrent requests hit the same uncached artifact.

### State machine

`lookup_or_start_artifact(key)` returns one of:

- **Hit** — artifact exists on disk. Serve immediately.
- **Join** — another request is already fetching this key. Wait on its `Inflight`.
- **Leader** — no one is fetching this key. Register in the inflight map, return a `Leader` token. The caller spawns the background fetch task.

The implementation uses double-checked locking:
1. Lock inflight map, check for existing entry → **Join** (skip disk I/O)
2. Unlock, check disk → **Hit**
3. Lock inflight map again, check again (race guard) → **Join** or insert new entry → **Leader**

### Inflight resolution

`Inflight` contains a `Mutex<Option<InflightOutcome>>` and a `Notify`. Waiters call `wait_for_outcome()`:

```rust
loop {
    let notified = self.notify.notified();  // register BEFORE checking
    if let Some(outcome) = self.outcome.lock().await.as_ref() {
        return outcome.clone();
    }
    notified.await;
}
```

The `notified()` future is created before locking to prevent lost wakeups. Outcomes:
- `Cached` — file committed to disk, waiter loads and streams it
- `Response(meta, body)` — non-200 upstream response or error, returned directly as bytes

### Cancellation safety

`ArtifactFetchCleanup` is a RAII guard created at the start of `run_artifact_fetch`. If the tokio task is aborted, `Drop` spawns a detached cleanup task that deletes the temp file, signals the inflight as failed (502), and removes the key from the inflight map. On normal completion (success or handled error), the guard is disarmed.

## Eviction

At startup, `cleanup_stale_temps` walks the cache tree and deletes any `.part` files older than 5 minutes (remnants of interrupted fetches).

LRU-by-mtime eviction runs inline after every successful cache write (`store_metadata`, `commit_artifact`) and once at startup.

Algorithm:
1. Walk the entire cache directory tree, collecting all `.body` files
2. For each, stat the sibling `.json` file to include its size
3. Sum all sizes. If under `max_cache_size`, return
4. Sort by mtime ascending (oldest first)
5. Delete oldest entries (both `.body` and `.json`) until total is under the limit

Metadata and artifact entries compete equally for space. There is no separate quota.

## Metadata rewriting

The proxy rewrites upstream metadata responses to redirect artifact downloads through itself. The rewrite origin is the configured `VAMPIRE_PUBLIC_BASE_URL`. Client request headers do not influence emitted artifact URLs.

### PyPI (HTML)

Regex-matches all `href="..."` and `href='...'` attributes. For each:
- URLs matching the configured `pypi_files` origin or hostname `files.pythonhosted.org` → `{VAMPIRE_PUBLIC_BASE_URL}/pypi/files/{relative_path}` (preserving `#hash` fragments)
- URLs matching the configured `pypi_simple` origin or hostname `pypi.org`, with path starting `/simple/` → `{VAMPIRE_PUBLIC_BASE_URL}{path}` (strips host, keeps path)
- Other URLs → unchanged

### npm (JSON)

Parses the full packument as `serde_json::Value`. Rewrites `dist.tarball` on the root object and on every entry in `versions.*`:
- URLs matching the configured `npm` origin or hostname `registry.npmjs.org` → `{VAMPIRE_PUBLIC_BASE_URL}/npm/tarballs/{relative_path}`
- Other URLs → unchanged

### Cargo

No rewriting. Cargo discovers the download URL from `/cargo/index/config.json`, which returns `{"dl": "{VAMPIRE_PUBLIC_BASE_URL}/cargo/api/v1/crates"}` — a synthetic response pointing back to the proxy.

## Failure logging

`log_failure(event, data)` writes a JSON line to stderr:

```json
{"ts_ms": 1710000000000, "level": "error", "event": "...", "data": {...}}
```

Events:
- `startup_failed` — config, bind, or app initialization error (with `stage` field; `bind_pkg_listener` and `bind_git_listener` stages include the `bind` address)
- `request_failed` — any handler-level I/O error propagated to the route (with `method`, `path`, `query`, `error`)
- `artifact_fetch_failed` — background fetch task error (with `stage`, `upstream`, `cache_key`, `error`)

## Stats

`AppStats` tracks four counters, all keyed by upstream URL string:
- `artifact_fetches` — incremented per upstream artifact GET
- `metadata_fetches` — incremented per upstream metadata GET (including revalidation)
- `artifact_joins` — incremented when a request deduplicates against an in-progress fetch
- `git_forwards` — incremented per forwarded git request to GitHub

Exposed via `App::stats() -> &AppStats` with `snapshot()` and `reset()` methods. No HTTP endpoint — used by integration tests to verify dedup behavior.
