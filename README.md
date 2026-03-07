# Vampire

Vampire is a minimal Rust proxy for read-only PyPI, npm, and Cargo package installs with a bounded local disk cache.

## Scope
- explicit registry endpoints, not a general forward proxy
- artifact caching with miss dedupe
- metadata rewriting for PyPI and npm
- metadata revalidation with `ETag` and `Last-Modified`

## Run
```bash
VAMPIRE_MAX_CACHE_SIZE=10GiB cargo run
```

## Container
The official image is published to `ghcr.io/cwwarren/vampire`.

```bash
docker run --rm \
  -p 8080:8080 \
  -v vampire-cache:/var/lib/vampire \
  -e VAMPIRE_MAX_CACHE_SIZE=10GiB \
  ghcr.io/cwwarren/vampire:latest
```
