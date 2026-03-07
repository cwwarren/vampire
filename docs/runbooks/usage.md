# Vampire Usage

## Start
```bash
VAMPIRE_MAX_CACHE_SIZE=10GiB cargo run
```

## Container
```bash
docker run --rm \
  -p 8080:8080 \
  -v vampire-cache:/var/lib/vampire \
  -e VAMPIRE_MAX_CACHE_SIZE=10GiB \
  ghcr.io/cwwarren/vampire:latest
```

Container defaults:
- `VAMPIRE_BIND=0.0.0.0:8080`
- `VAMPIRE_CACHE_DIR=/var/lib/vampire`
- Published tags are `latest` and `sha-<full git sha>`

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
- Artifact misses are single-flight per cache key. Vampire waits for the full upstream artifact before replying, then serves the committed file.
- The cache bound is soft during a successful commit and enforced immediately after the write finishes.
- Failure logs are JSON lines on stderr with `event=request_failed`, `event=artifact_fetch_failed`, or `event=startup_failed`.

## Test
```bash
cargo test
cargo test --test real_e2e -- --ignored --test-threads=1 --nocapture
```

## CI
- GitHub Actions runs on the ARC scale-set label `procyon-vampire`.
- Do not combine `self-hosted` with the ARC scale-set name in `runs-on`.
- `pull_request` runs `cargo test` and the live suite in parallel for PR validation.
- `push` runs only on `main`, so PR branches do not get an extra duplicate push workflow.
- `push` to `main` runs the same two jobs and then uploads `target/release/vampire` as a workflow artifact.
- `push` to `main` also publishes `ghcr.io/<owner>/vampire` with `latest` and `sha-<full git sha>` tags from the ARC runner.
