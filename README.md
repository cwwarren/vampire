# Vampire

Vampire is a minimal Rust proxy for read-only PyPI, npm, and Cargo package installs with a bounded local disk cache.

## Scope
- explicit registry endpoints, not a general forward proxy
- artifact caching with miss dedupe
- metadata rewriting for PyPI and npm
- metadata revalidation with `ETag` and `Last-Modified`

## Run
```bash
cd projects/vampire
VAMPIRE_MAX_CACHE_SIZE=10GiB cargo run
```
