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
- Publish, login, or npm audit APIs
- Shared-cache multi-process coordination

## Public Surface
- Package listener on `VAMPIRE_PKG_BIND`:
- `GET|HEAD /pypi/simple/`
- `GET|HEAD /pypi/simple/{project}/`
- `GET|HEAD /pypi/files/{path...}`
- `GET|HEAD /npm/{package}`
- `GET|HEAD /npm/tarballs/{path...}`
- `GET|HEAD /cargo/index/config.json`
- `GET|HEAD /cargo/index/...`
- `GET|HEAD /cargo/api/v1/crates/{crate}/{version}/download`
- Git listener on `VAMPIRE_GIT_BIND`:
- `GET /{owner}/{repo}.git/info/refs?service=git-upload-pack`
- `POST /{owner}/{repo}.git/git-upload-pack`

## Container Image
- Publish an official image to `ghcr.io/<repo-owner>/vampire`.
- Container defaults set `VAMPIRE_PKG_BIND=0.0.0.0:8080`.
- Container defaults set `VAMPIRE_GIT_BIND=0.0.0.0:8081`.
- Container defaults set `VAMPIRE_CACHE_DIR=/var/cache/vampire`.
- `VAMPIRE_PUBLIC_BASE_URL` remains required at runtime and has no container default because it is deployment-specific.
- Published tags are `latest` and `sha-<full git sha>`.

## Config
- `VAMPIRE_PUBLIC_BASE_URL` required and must be the externally reachable package-listener origin
- `VAMPIRE_MAX_CACHE_SIZE_MB` required
- `VAMPIRE_PKG_BIND` default `127.0.0.1:8080`
- `VAMPIRE_GIT_BIND` default `127.0.0.1:8081`
- `VAMPIRE_CACHE_DIR` default `./.cache/vampire`
- `VAMPIRE_MAX_UPSTREAM_FETCHES` default `32`
- `VAMPIRE_UPSTREAM_TIMEOUT_MS` default `30000`

## Cache Rules
- Artifacts are cached by canonical upstream URL until evicted.
- Cache coordination is in-process only. Sharing one cache directory across multiple vampire processes is unsupported.
- Git proxy traffic is never persisted in the disk cache; accepted git reads always forward directly to GitHub.
- Accepted git upload-pack responses stream through directly; vampire does not buffer the full upstream git response in memory before replying.
- On an artifact miss, vampire completes the upstream artifact fetch before it begins the client response.
- Followers wait for the same completed result and then serve the committed file or the completed upstream error response.
- Metadata is cached only when upstream returns `ETag` or `Last-Modified`.
- Metadata fetches are not deduped. Concurrent cold metadata requests may fetch upstream independently and race to populate cache.
- Cached metadata is published as a single atomic file so readers never observe mixed headers and body bytes.
- Rewritten npm and PyPI metadata must not expose upstream `ETag` or `Last-Modified` to clients; those validators are only for vampire's own upstream revalidation.
- HEAD responses must emit the same headers GET would emit for the same resource, including `Content-Length` on cold misses.
- Eviction is oldest-first by completed file mtime.
- Successful writes may overshoot temporarily; janitor eviction restores the bound after commit.

## Failure Logging
- Emit structured JSON lines to stderr for request failures, background artifact-fetch failures, and startup failures.
- Each line includes `ts_ms`, `level`, `event`, and a `data` object with failure-specific fields.

## Git Guardrails
- Git traffic is GitHub-only and read-only in v1.
- Only smart-HTTP `git-upload-pack` discovery and RPC are supported.
- Git routing is path-based, not header-based; `Git-Protocol` is forwarded when present but is not required for discovery.
- Non-canonical or unsafe git paths such as doubled slashes, dot segments, encoded repo segments, encoded separators, malformed escapes, proxy-style absolute targets, URL-userinfo, and `git-receive-pack` are rejected locally.
- For accepted git requests, vampire forwards only caller-supplied `Git-Protocol`, `Content-Type`, and `Content-Encoding` on `git-upload-pack`.
- `git-upload-pack` request bodies remain buffered and capped at 8 MiB before forwarding.
