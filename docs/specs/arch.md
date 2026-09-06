# Architecture

Vampire is a single-process async Rust HTTP proxy that caches package artifacts and metadata for three registries: PyPI, npm, and Cargo. A second listener on the same process proxies read-only GitHub smart-HTTP traffic for git-pinned package dependencies, and a third management listener exposes Prometheus-formatted in-memory stats. Built on tokio + axum + reqwest.

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
    client: reqwest::Client,         // same-origin package redirects
    git_client: reqwest::Client,     // redirects disabled
    stats: AppStats,                 // mutex-protected counters
    upstreams: RegistryOrigins, // 6 upstream base URLs
    public_base_url: String,    // configured rewrite origin
}
```

Cloning `App` (which axum does per-request) is a single `Arc` refcount bump.

Synchronization primitives within `CacheStore`:

| Field | Type | Purpose |
|---|---|---|
| `inflight` | `tokio::sync::Mutex<HashMap<String, Arc<Inflight>>>` | Maps representation cache keys to in-progress artifact or metadata fetches |
| `pins` | `std::sync::Mutex<HashMap<String, Weak<()>>>` | Keeps newly published artifacts from eviction until waiters open them |
| `eviction_lock` | `Arc<tokio::sync::Mutex<()>>` | Serializes startup, metadata-write, and deferred artifact eviction scans |
| `eviction_tx` | `tokio::sync::mpsc::Sender<Arc<std::fs::File>>` | Coalesces deferred artifact eviction requests through a capacity-one janitor queue |
| `temp_counter` | `AtomicU64` (Relaxed) | Generates unique temp file suffixes |
| `upstream_semaphore` | `Arc<Semaphore>` | Bounds artifact GET leaders, metadata leaders, uncached artifact HEAD requests, and uncached npm search and audit requests together (default 32) |
| `metadata_memory_semaphore` | `Arc<Semaphore>` | Tracks buffered input and bounded rewrite working space in MiB against a separate 1 GiB budget |
| `directory_lock` | `Arc<std::fs::File>` | Holds the exclusive advisory lock on the ownership marker for the store lifetime |

`AppStats` uses `std::sync::Mutex` (not tokio) because the lock is held only for fast HashMap operations, never across await points.

The package client applies `VAMPIRE_UPSTREAM_TIMEOUT_MS` as its total request/body deadline. It permits at most 10 redirects that retain the original scheme, host, and effective port, rejecting credentials, cross-origin redirects, and HTTPS downgrades. The Git client disables redirects and applies the same timeout value to connection setup and each idle interval between response chunks, with no total response deadline.

## Request flow

### Routing

`App::serve(pkg_listener, git_listener, management_listener)` runs three Axum routers over the same shared `App` state.

Package routes accept GET and HEAD on the package listener:

| Path | Type | Handler |
|---|---|---|
| `/cargo/index/config.json` | synthetic | Returns `{"dl": "{VAMPIRE_PUBLIC_BASE_URL}/cargo/api/v1/crates"}` |
| `/cargo/index/{*path}` | metadata | Cargo sparse index entries |
| `/cargo/api/v1/crates/{crate_name}/{version}/download` | artifact | Cargo crate tarballs |
| `/pypi/simple/` | metadata | PyPI simple index root |
| `/pypi/simple/{project}/` | metadata | PyPI simple project page |
| `/pypi/files/{*path}` | artifact | PyPI package files |
| `/npm/{*package}` | metadata | npm packument JSON |
| `/npm/tarballs/{*path}` | artifact | npm tarballs |
| `/npm/-/v1/search` | uncached | npm search JSON with allowlisted query parameters |

Two additional package routes accept POST: `/npm/-/npm/v1/security/advisories/bulk` and `/npm/-/npm/v1/security/audits/quick`. Both forward uncached audit requests.

The git listener routes every request through the git handler. The git surface is path-based and GitHub-only:

| Path | Type | Handler |
|---|---|---|
| `/{owner}/{repo}.git/info/refs?service=git-upload-pack` | git discovery | Forward to GitHub |
| `/{owner}/{repo}.git/git-upload-pack` | git RPC | Forward to GitHub |

Both git routes also accept repository names without `.git`; forwarding canonicalizes them to `.git`. Receive-pack remains rejected in either form.

The management listener is stats-only:

| Path | Type | Handler |
|---|---|---|
| `/stats` | synthetic | Prometheus exposition for in-memory stats |

Package handlers preserve the raw URI path, reject client queries except on npm search, and pass only canonical relative paths to `join_url`. Validation rejects absolute paths, doubled separators, backslashes, dot segments, malformed or lowercase escapes, encoded separators, encoded unreserved aliases, query or fragment delimiters, and origin changes. Canonical safe escapes remain encoded; scoped npm packuments accept either hex case for `%2F` and normalize it to uppercase before identity and forwarding.

PyPI simple project routes accept exactly one canonical raw project segment. Literal and percent-encoded slashes are rejected locally before any upstream URL is built.

Git traffic stays uncached and fail-closed. The handler parses the raw request URI, rejects absolute-form targets, URL-userinfo, `git-receive-pack`, doubled slashes, dot segments, encoded repo segments, encoded separators, malformed escapes, and other non-canonical path forms locally, then forwards only accepted `git-upload-pack` requests to `https://github.com`. Upload-pack request bodies are buffered up to 8 MiB before forwarding, while accepted upstream git responses are streamed directly back to the client.

### Metadata path

```
handle_metadata(upstream, rewrite)
  identity = raw:v1 | npm:v2:<origin> | pypi:v2:<origin>
  key = hash("metadata", canonical_upstream_url, identity)
  lookup_or_start(key):
    Join → wait for the existing cold fetch or revalidation
    Leader → return 503 if admission is full, otherwise continue
    Hit with validator → conditional GET; 304 returns the opened entry
    Miss → GET upstream
  reject upstream or rewritten bodies larger than 128 MiB
  apply rewrite (None / PyPI HTML / npm JSON)
  if status 200 AND has etag or last-modified:
    store to disk (atomic write)
  return response
```

Metadata is only cached when the upstream provides a cache validator (etag or last-modified). Cold fetches and validator revalidations are single-flight by representation cache key and share the upstream admission semaphore with artifact leaders. A separate 1 GiB memory budget reserves reported or observed input size in 1 MiB units, then reserves the bounded rewrite working set before allocation. The reservation is attached to the resulting shared `Bytes` and is released only after all response clones drop. Duplicate work joins; a unique request returns HTTP 503 when either admission bound cannot admit more work.
For rewritten npm and PyPI metadata, vampire still stores those upstream validators for its own conditional GETs, but strips `ETag` and `Last-Modified` from the client-facing response headers because the served bytes differ from the upstream representation.

### npm search and audit

Search and audit bypass cache lookup, validator revalidation, cache publication, and the inflight map. Both acquire the shared upstream admission permit and use `handle_npm_request` to read a bounded response through the package client. The existing 128 MiB response limit and 1 GiB byte reservation remain in force through delivery; uncached requests still need resource limits.

Search accepts only `text`, `size`, `from`, `quality`, `popularity`, and `maintenance` query keys. GET and HEAD both fetch an unconditional upstream GET; HEAD discards the body. The query is forwarded but never forms a cache key, and result bodies are not rewritten.

Audit accepts only the bulk-advisories and quick-audit POST paths, rejects queries, and acquires its permit before reading the request body. Bodies are buffered up to 8 MiB without decompression; invalid or oversized bodies return 400 and emit `npm_audit_body_read_failed`. Only `Content-Type` and `Content-Encoding` are forwarded, so gzip works without forwarding credentials.

### Artifact path

```
handle_artifact(upstream)
  key = SHA256("artifact\0" + upstream_url + "\0")  // hex-encoded, 64 chars
  lookup_or_start_artifact(key):
    Hit  → stream file from disk
    Join → wait on existing inflight, then serve result
    Leader → return 503 if admission is full; otherwise fetch and serve the result
```

Join and Leader requests go through `serve_inflight`; even the Leader request waits on the `Inflight` outcome rather than getting special treatment. Hits stream their already-open entry directly.

The background fetch (`run_artifact_fetch`):
1. Use the admission permit attached to the leader
2. GET upstream URL
3. Stream response body to a `<key>.part` temp file
4. Append footer (meta JSON + 4-byte length) to `.part`
5. Create a publication pin, atomically rename `.part` to `<key>`, and return the pin token
6. Signal `Inflight` with the token; each waiter opens its own stable entry before dropping it
7. Remove the key from the inflight map

The publication pin makes eviction skip the newly committed path until every waiter has opened its own stable entry. Dropping the final pin schedules another eviction pass, so temporary overshoot lasts through response handoff rather than ending immediately at commit.

On any error or task cancellation, the `ArtifactFetchCleanup` drop guard ensures the inflight is resolved (as a 502 error response) and the key is removed from the map, so joiners are never permanently blocked.

Non-200 artifact responses are returned without caching and buffered up to 1 MiB.

### Git path

Accepted git requests bypass the cache layer entirely.

```
git request
  reject absolute-form, userinfo, CONNECT, invalid path, write RPCs
  accept only GET info/refs?service=git-upload-pack
           and POST git-upload-pack
  forward only Git-Protocol (+ Content-Type and Content-Encoding on POST)
  stream upstream response back without writing cache entries
```

### HEAD path

Artifact HEAD checks the artifact cache and otherwise sends an upstream HEAD. Metadata HEAD uses the same single-flight cache lookup and conditional GET lifecycle as metadata GET, then discards the body. Search HEAD instead runs an uncached upstream GET and discards the body.

On miss:
- artifact paths send a real upstream HEAD and preserve the upstream `Content-Length`
- metadata paths run the normal GET, validation, and rewrite flow so headers match the corresponding GET
- `/cargo/index/config.json` synthesizes the same `Content-Type` and `Content-Length` as GET, but with no body

## Cache storage

### Key derivation

```
artifact: hash("artifact", canonical_upstream_url)
metadata: hash("metadata", canonical_upstream_url, representation_identity)
```

The canonical upstream URL used for cache identity excludes queries and fragments. Metadata representation identities are `raw:v1`, `npm:v2:<origin>`, and `pypi:v2:<origin>`. Including the rewrite origin prevents a persistent cache from serving URLs generated for an older `VAMPIRE_PUBLIC_BASE_URL`. PyPI v2 invalidates pages cached before the Simple API link fix; old entries expire through normal eviction. Search and audit do not create cache keys; previously cached search entries are no longer read and remain subject to normal eviction. Keys are SHA-256 hex strings; the first 2 hex characters are the shard directory name.

### Directory layout

```
<cache_dir>/
  .vampire-cache-v1     # ownership marker
  <shard>/              # 2-char hex prefix (256 possible directories)
    <key>               # committed cache entry (packed: body + meta footer)
    <key>.part          # temp file during artifact fetch
    <key>.part.N.part   # temp file during metadata write (N = monotonic counter)
```

### Packed entry format

Artifacts and metadata share a single on-disk layout. `<key>` contains:

```
[body bytes:       offset 0 .. N]
[meta JSON:        offset N .. N + M]
[meta_len (u32 BE): offset N + M .. N + M + 4]
```

Total file size is `N + M + 4`. `StoredResponseMeta` (`{ headers, last_modified, etag, status }`) carries both the headers returned to clients and the upstream validator fields vampire uses for conditional revalidation.

Read: open the packed entry, seek 4 bytes from end → `meta_len`, seek `4 + meta_len` from end, read `meta_len` bytes → meta JSON, body is `0 .. file_size - 4 - meta_len`. Readers and writers reject metadata over 1 MiB. `StoredEntry` retains that open file, so a later rename or eviction cannot change the bytes paired with its parsed metadata.

Write for artifacts: the upstream body is streamed straight to `<key>.part`, then the meta JSON and 4-byte length are appended to the same `.part` and it is atomically renamed to `<key>`. Write for metadata: body bytes, meta JSON, and the length footer are written sequentially to a uniquely-suffixed `.part` temp file, then atomically renamed. A drop guard removes the unique metadata temp after write failure or cancellation.

## Inflight dedup

Prevents duplicate upstream work when concurrent artifact GET or metadata GET or HEAD requests use the same representation cache key. Cold artifact HEAD requests use the shared admission semaphore without joining inflight work.

### State machine

`lookup_or_start_artifact(key)` returns one of:

- **Hit** — a completed entry exists on disk. Serve its stable open handle immediately.
- **Join** — another request is fetching or revalidating this key. Wait on its `Inflight`.
- **Leader** — no work exists for this key and admission capacity is available. Register it and start upstream work.

When admission capacity is exhausted, lookup returns `WouldBlock` without registering work; handlers map it to HTTP 503.

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
- `Cached` — file committed to disk and pinned long enough for waiters to open and stream it
- `Response(meta, body)` — non-200 upstream response returned directly as bytes
- `Failed(kind, error)` — leader failure with its I/O kind preserved; `WouldBlock` is returned to every waiter as HTTP 503 and other failures as HTTP 502

### Cancellation safety

`ArtifactFetchCleanup` is a RAII guard created at the start of `run_artifact_fetch`. If the tokio task is aborted, `Drop` spawns a detached cleanup task that deletes the temp file, signals the inflight as failed (502), and removes the key from the inflight map. On normal completion (success or handled error), the guard is disarmed.

## Eviction

At startup, vampire requires `.vampire-cache-v1`. It creates the marker for an empty directory or an unmarked directory containing only exact current, legacy, or recognized temp cache filenames; any other nonempty unmarked directory fails startup unchanged. The process takes an exclusive advisory lock on the marker and fails startup if another vampire holds it. Because that lock proves no writer is live, `cleanup_stale_and_legacy` deletes every recognized temp and legacy file from exact two-lowercase-hex shard directories immediately; unrelated files and directories are untouched.

Oldest-first-by-mtime eviction runs inline after metadata writes and once at startup. Artifact publication defers eviction until the final publication pin drops. One janitor task consumes a capacity-one queue and shares the async eviction lock with inline scans. After acquiring the lock it drains requests that accumulated while waiting, then scans once. A drop during the scan queues one follow-up pass; further drops coalesce into that pending request, so deferred work is bounded to one worker and one queued scan.

Algorithm:
1. Inspect exact shard directories and collect only extensionless 64-lowercase-hex `<key>` files
2. Sum all sizes (each entry is a single file). If under `max_cache_size`, return
3. Sort by mtime ascending (oldest first)
4. Delete oldest entries until total is under the limit

Metadata and artifact entries compete equally for space. There is no separate quota.
Eviction skips a freshly published target while its publication pin is live. The final pin drop schedules bound enforcement.

## Metadata rewriting

The proxy rewrites upstream metadata responses to redirect artifact downloads through itself. The rewrite origin is the configured `VAMPIRE_PUBLIC_BASE_URL`. Client request headers do not influence emitted artifact URLs.

### PyPI (HTML)

Regex-matches all `href="..."` and `href='...'` attributes. For each:
- URLs matching the configured `pypi_files` origin or hostname `files.pythonhosted.org` → `{VAMPIRE_PUBLIC_BASE_URL}/pypi/files/{relative_path}` (preserving `#hash` fragments)
- URLs matching the configured `pypi_simple` origin or hostname `pypi.org`, with path starting `/simple/`, and root-relative `/simple/...` links → `{VAMPIRE_PUBLIC_BASE_URL}/pypi{path}` after canonical path validation
- Other URLs → unchanged

Rewritten PyPI responses do not forward upstream `ETag` or `Last-Modified` headers to clients.

### npm (JSON)

Keeps unrelated packument values as borrowed raw JSON and materializes only the root, `versions`, package, and `dist` maps needed to rewrite `dist.tarball` on the root object and every entry in `versions.*`:
- URLs matching the configured `npm` origin or hostname `registry.npmjs.org` → `{VAMPIRE_PUBLIC_BASE_URL}/npm/tarballs/{relative_path}`
- Other URLs → unchanged

Rewritten npm packuments do not forward upstream `ETag` or `Last-Modified` headers to clients. Search results are raw metadata and retain upstream validators.

### Cargo

No rewriting. Cargo discovers the download URL from `/cargo/index/config.json`, which returns `{"dl": "{VAMPIRE_PUBLIC_BASE_URL}/cargo/api/v1/crates"}` — a synthetic response pointing back to the proxy. Because sparse index responses are forwarded byte-for-byte, upstream validators remain client-visible on Cargo metadata.

## Failure logging

`log_failure(event, data)` writes a JSON line to stderr:

```json
{"ts_ms": 1710000000000, "level": "error", "event": "...", "data": {...}}
```

Events:
- `startup_failed` — config, bind, or app initialization error (with `stage` field; listener-bind stages include the `bind` address)
- `request_failed` — any handler-level I/O error propagated to the route (with `method`, `path`, `query`, `error`)
- `artifact_fetch_failed` — background fetch task error (with `stage`, `upstream`, `cache_key`, `error`)
- `git_rejected` — rejected Git request (with `method`, `path`, `query`, `status`, `message`)
- `git_body_read_failed` — rejected Git request body (with `method`, `path`, `error`)
- `npm_audit_body_read_failed` — rejected npm audit request body (with `method`, `path`, `error`)
- `git_stream_failed` — Git response stream failure (with `method`, `upstream`, `error`)

## Stats

`AppStats` tracks four counters keyed by upstream type (six fixed values):
- `artifact_fetches` — incremented per upstream artifact GET
- `metadata_fetches` — incremented per upstream package metadata GET, including revalidation but excluding search and audit
- `artifact_joins` — incremented when a request deduplicates against an in-progress fetch
- `git_forwards` — incremented per forwarded git request to GitHub

Two scalar counters, `npm_search_requests` and `npm_audit_requests`, count requests after method, path, and query validation, before admission or body processing. They include search HEAD, saturation responses, and rejected audit bodies, but exclude invalid routes or queries. `/stats` always emits `vampire_npm_search_requests_total` and `vampire_npm_audit_requests_total`, initially zero, without labels.

Upstream types: `pypi_files`, `pypi_simple`, `npm`, `cargo_download`, `cargo_index`, `github`.

Exposed via `App::stats() -> &AppStats` with `snapshot()`, `reset()`, and `render_prometheus()` methods. `/stats` renders one sample per `(metric, upstream type)` pair plus the two scalar npm counters, bounding cardinality to 26 time series max (4 metrics × 6 upstreams + 2).
