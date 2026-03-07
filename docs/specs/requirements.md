# Vampire Requirements

## Goals
- Allow read-only package installs for PyPI, npm, and Cargo through one internal service.
- Cache immutable artifacts on local disk.
- Revalidate cacheable metadata with upstream validators.
- Bound disk use with `VAMPIRE_MAX_CACHE_SIZE`.

## Non-Goals
- Generic HTTP proxying
- TLS MITM
- Publish, login, or npm audit APIs
- Shared-cache multi-process coordination

## Public Surface
- `GET|HEAD /pypi/simple/`
- `GET|HEAD /pypi/simple/{project}/`
- `GET|HEAD /pypi/files/{filename}?u=...`
- `GET|HEAD /npm/{package}`
- `GET|HEAD /npm/tarballs/{filename}?u=...`
- `GET|HEAD /cargo/index/config.json`
- `GET|HEAD /cargo/index/...`
- `GET|HEAD /cargo/api/v1/crates/{crate}/{version}/download`

## Config
- `VAMPIRE_MAX_CACHE_SIZE` required
- `VAMPIRE_BIND` default `127.0.0.1:8080`
- `VAMPIRE_CACHE_DIR` default `./.cache/vampire`
- `VAMPIRE_MAX_UPSTREAM_FETCHES` default `32`
- `VAMPIRE_UPSTREAM_TIMEOUT` default `30s`

## Cache Rules
- Artifacts are cached by canonical upstream URL until evicted.
- Metadata is cached only when upstream returns `ETag` or `Last-Modified`.
- Eviction is oldest-first by completed file mtime.
- Successful writes may overshoot temporarily; janitor eviction restores the bound after commit.

## Failure Logging
- Emit structured JSON lines to stderr for request failures, background artifact-fetch failures, and startup failures.
- Each line includes `ts_ms`, `level`, `event`, and a `data` object with failure-specific fields.
