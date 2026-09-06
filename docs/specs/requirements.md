# Vampire Requirements

## Goals
- Allow read-only package installs for PyPI, npm, and Cargo through one internal service.
- Allow git-pinned GitHub dependencies to resolve through that same service on a dedicated listener.
- Cache immutable artifacts on local disk.
- Revalidate cacheable metadata with upstream validators.
- Bound disk use with `VAMPIRE_MAX_CACHE_SIZE_MB`.

## Non-Goals
- Generic HTTP proxying
- TLS MITM
- Publish and login APIs
- Shared-cache multi-process coordination

## Public Surface
- Package listener on `VAMPIRE_PKG_BIND`:
- `GET|HEAD /pypi/simple/`
- `GET|HEAD /pypi/simple/{project}/`
- `GET|HEAD /pypi/files/{path...}`
- `GET|HEAD /npm/{package}`
- `GET|HEAD /npm/tarballs/{path...}`
- `GET|HEAD /npm/-/v1/search`
- `POST /npm/-/npm/v1/security/advisories/bulk`
- `POST /npm/-/npm/v1/security/audits/quick`
- `GET|HEAD /cargo/index/config.json`
- `GET|HEAD /cargo/index/...`
- `GET|HEAD /cargo/api/v1/crates/{crate}/{version}/download`
- Git listener on `VAMPIRE_GIT_BIND`:
- `GET /{owner}/{repo}.git/info/refs?service=git-upload-pack`
- `POST /{owner}/{repo}.git/git-upload-pack`
- Git repository paths accept both `{repo}` and `{repo}.git`; upstream paths use `{repo}.git`.
- Management listener on `VAMPIRE_MANAGEMENT_BIND`:
- `GET /stats`

## Container Image
- Publish an official image to `ghcr.io/<repo-owner>/vampire`.
- Container defaults set `VAMPIRE_PKG_BIND=0.0.0.0:8080`.
- Container defaults set `VAMPIRE_GIT_BIND=0.0.0.0:8081`.
- Container defaults set `VAMPIRE_MANAGEMENT_BIND=127.0.0.1:8082`.
- Container defaults set `VAMPIRE_CACHE_DIR=/var/cache/vampire`.
- `VAMPIRE_PUBLIC_BASE_URL` remains required at runtime and has no container default because it is deployment-specific.
- Published tags are `latest` and `sha-<full git sha>`.
- The default container invocation does not publish the unauthenticated management listener; exposing it requires an explicit bind to a trusted host interface.

## Config
- `VAMPIRE_PUBLIC_BASE_URL` required and must be the externally reachable package-listener origin
- `VAMPIRE_MAX_CACHE_SIZE_MB` required, positive, and rejected if byte conversion overflows
- `VAMPIRE_PKG_BIND` default `127.0.0.1:8080`
- `VAMPIRE_GIT_BIND` default `127.0.0.1:8081`
- `VAMPIRE_MANAGEMENT_BIND` default `127.0.0.1:8082`
- `VAMPIRE_CACHE_DIR` default `./.cache/vampire`
- `VAMPIRE_MAX_UPSTREAM_FETCHES` default `32`, must be positive, and bounds artifact GET leaders, metadata leaders, uncached artifact HEAD requests, and uncached npm search and audit requests together
- `VAMPIRE_UPSTREAM_TIMEOUT_MS` default `30000`, must be positive, and is the package total timeout and the Git connect and idle-read timeout
- The management listener is unauthenticated; deployments should bind it only to trusted interfaces.

## Cache Rules
- `/stats` is synthetic and never served from the disk cache.
- Artifacts are cached by canonical, query-free, fragment-free upstream URL until evicted.
- Metadata identity includes the canonical, query-free upstream URL and its served representation: raw v1, npm v2 plus rewrite origin, or PyPI v2 plus rewrite origin. PyPI v1 entries are no longer reused.
- Cache coordination is in-process only. One vampire process must exclusively own a dedicated cache directory.
- Vampire marks ownership with `.vampire-cache-v1` and holds an exclusive advisory lock on it; an already locked directory fails startup. An unmarked nonempty directory is accepted only when every entry has an exact current, legacy, or recognized temp cache filename, otherwise startup fails without deleting anything.
- Cleanup and eviction may remove only files matching vampire's exact cache layout; unrelated descendants must never be treated as cache entries.
- Git proxy traffic is never persisted in the disk cache; accepted git reads always forward directly to GitHub.
- Accepted git upload-pack responses stream through directly; vampire does not buffer the full upstream git response in memory before replying.
- On an artifact miss, vampire completes the upstream artifact fetch before it begins the client response.
- Duplicate artifact GET requests and metadata GET or HEAD requests join one leader per representation key and receive the same completed result. Cold artifact HEAD requests are admitted by the same capacity bound but are not single-flight.
- `VAMPIRE_MAX_UPSTREAM_FETCHES` bounds admitted unique leaders across artifact and metadata work plus search and audit requests; excess work returns HTTP 503 instead of waiting in an unbounded queue.
- Metadata is cached only when upstream returns `ETag` or `Last-Modified`.
- Upstream and rewritten metadata bodies are capped at 128 MiB; upstream size is checked from `Content-Length` when present and while streaming.
- Rewritten metadata output is rejected while it is generated, before it can exceed 128 MiB.
- Buffered metadata uses a separate 1 GiB byte-weighted reservation budget. Reservations cover reported or observed input size and bounded rewrite working space, then follow the shared response bytes until every clone drops.
- Non-200 artifact response bodies are capped at 1 MiB.
- Cold metadata fetches and validator revalidations are single-flight by representation cache key.
- All cache entries are published as a single atomic file and read through a stable open entry so replacement or eviction cannot mix headers and body bytes.
- Rewritten npm and PyPI metadata must not expose upstream `ETag` or `Last-Modified` to clients; those validators are only for vampire's own upstream revalidation.
- Metadata HEAD requests use the same cache lookup and conditional GET lifecycle as GET, then discard the body, so they emit the headers GET would return.
- Eviction is oldest-first by completed file mtime.
- Eviction scans are serialized within the owning process.
- A freshly published entry is pinned through response handoff and skipped by eviction until all waiters have opened it.
- Successful writes may remain above the bound while published; dropping the final publication pin requests bound enforcement through one capacity-one janitor queue, coalescing bursts into at most one pending follow-up scan.

## Outbound Requests
- Package and Git traffic use separate upstream clients.
- Except for npm search, package routes reject client queries. Package routes reject noncanonical raw paths, including dot segments, malformed or lowercase escapes, encoded separators, encoded unreserved aliases, and query or fragment delimiters; scoped npm `%2F` is accepted in either hex case and normalized to uppercase.
- Package redirects are limited to 10 hops and may retain only the exact scheme, host, and effective port of the original registry origin.
- Credential-bearing redirects, cross-origin redirects, and HTTPS-to-HTTP downgrades are rejected.
- Git redirects are disabled.
- The package client applies `VAMPIRE_UPSTREAM_TIMEOUT_MS` as its total request/body deadline.
- The Git client applies the same value to connection setup and each idle interval between response chunks, with no total response deadline.

## Failure Logging
- Emit structured JSON lines to stderr for request failures, background artifact-fetch failures, startup failures, rejected Git requests, rejected Git and npm audit request bodies, and Git response stream failures.
- Each line includes `ts_ms`, `level`, `event`, and a `data` object with failure-specific fields.

## Registry Metadata and npm APIs
- PyPI root-relative `/simple/...` links and absolute PyPI Simple API links are rewritten to `{VAMPIRE_PUBLIC_BASE_URL}/pypi/simple/...`; artifact links retain their hash fragments.
- Search forwards only to the npm registry `/-/v1/search`, allowing `text`, `size`, `from`, `quality`, `popularity`, and `maintenance` query parameters. Unknown query parameters are rejected locally. Search GET and HEAD both fetch an unconditional upstream GET; HEAD discards the body and retains the response headers. Result bodies are not rewritten.
- Audit accepts only the two listed POST endpoints, without queries. Search and audit requests are uncached and are not single-flight, even when upstream returns validators. Each request acquires shared upstream capacity.
- Audit acquires shared upstream capacity before buffering at most 8 MiB of request bytes. Failed or oversized body reads return HTTP 400 without forwarding. Only caller `Content-Type` and `Content-Encoding` are forwarded, including gzip; credentials and cookies are not forwarded. Vampire does not decompress request bodies.
- Search and audit responses preserve upstream status and body, are capped at 128 MiB, and use the shared 1 GiB metadata memory reservation budget through delivery. They use the package client timeout and redirect policy.
- Audits disclose dependency names and versions to the public npm registry. Publish, login, and signature/attestation APIs remain unsupported.
- `vampire_npm_search_requests_total` and `vampire_npm_audit_requests_total` are separate unlabelled counters, emitted from zero. They count requests after method, path, and query validation, including search HEAD, admission rejections, and audit body rejections. Package `metadata_fetches` excludes both.

## Git Guardrails
- Git traffic is GitHub-only and read-only in v1.
- Only smart-HTTP `git-upload-pack` discovery and RPC are supported.
- Git routing is path-based, not header-based; `Git-Protocol` is forwarded when present but is not required for discovery.
- Non-canonical or unsafe git paths such as doubled slashes, dot segments, encoded repo segments, encoded separators, malformed escapes, proxy-style absolute targets, URL-userinfo, and `git-receive-pack` are rejected locally.
- For accepted git requests, vampire forwards only caller-supplied `Git-Protocol`, `Content-Type`, and `Content-Encoding` on `git-upload-pack`.
- `git-upload-pack` request bodies remain buffered and capped at 8 MiB before forwarding.

## CI and Release
- GitHub Actions jobs run on `ubuntu-latest` hosted runners.
- Starting a newer workflow run cancels any older in-progress run for the same branch or pull-request ref.
- Pull requests run the heavy checks and live end-to-end suite.
- Pushes to `main` run the same checks and publish `ghcr.io/<repo-owner>/vampire` as `latest` and `sha-<full git sha>`.
- Release version tags do not select container image tags.
