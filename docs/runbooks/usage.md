# Vampire Usage

## Start
```bash
VAMPIRE_MAX_CACHE_SIZE=10GiB cargo run
```

## Client Configuration
```bash
pip install --index-url http://127.0.0.1:8080/pypi/simple/ <package>
npm config set registry http://127.0.0.1:8080/npm/
npm config set audit false
cargo add --registry crates-io <crate>
```

Cargo source replacement:

```toml
[source.crates-io]
replace-with = "vampire"

[source.vampire]
registry = "sparse+http://127.0.0.1:8080/cargo/index/"
```

## Operational Notes
- One vampire process should own a cache directory.
- `*.part` files are in-flight downloads and are cleaned on startup if stale.
- The cache bound is soft during a successful commit and enforced immediately after the write finishes.
- Failure logs are JSON lines on stderr with `event=request_failed`, `event=artifact_fetch_failed`, or `event=startup_failed`.

## Test
```bash
cargo test
cargo test --test real_e2e -- --ignored --test-threads=1 --nocapture
```
