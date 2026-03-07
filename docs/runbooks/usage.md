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

## Sandbox Overrides
For agent sandboxes, prefer environment variables over persistent dotfiles.

Python with `pip`:
```bash
export VAMPIRE=http://127.0.0.1:8080
export PIP_CONFIG_FILE=/dev/null
export PIP_INDEX_URL="$VAMPIRE/pypi/simple/"
export PIP_TRUSTED_HOST=127.0.0.1
```

Python with `uv`:
```bash
export VAMPIRE=http://127.0.0.1:8080
export UV_NO_CONFIG=1
export UV_DEFAULT_INDEX="$VAMPIRE/pypi/simple/"
export UV_INSECURE_HOST=127.0.0.1
```

Node with `npm`:
```bash
export VAMPIRE=http://127.0.0.1:8080
export NPM_CONFIG_USERCONFIG=/dev/null
export NPM_CONFIG_GLOBALCONFIG=/dev/null
export NPM_CONFIG_REGISTRY="$VAMPIRE/npm/"
export NPM_CONFIG_AUDIT=false
export NPM_CONFIG_FUND=false
export NPM_CONFIG_UPDATE_NOTIFIER=false
```

Node with `bun`:
```bash
export VAMPIRE=http://127.0.0.1:8080
export BUN_CONFIG_REGISTRY="$VAMPIRE/npm/"
```

Rust with `cargo`:
- Use `CARGO_HOME` to isolate cache and config from the host.
- There is no documented single env var that replaces crates.io for dependency resolution end-to-end.
- Generate a temporary `config.toml` for source replacement:

```bash
export VAMPIRE=http://127.0.0.1:8080
export CARGO_HOME="${TMPDIR:-/tmp}/vampire-cargo"
mkdir -p "$CARGO_HOME"
cat >"$CARGO_HOME/config.toml" <<EOF
[source.crates-io]
replace-with = "vampire"

[source.vampire]
registry = "sparse+$VAMPIRE/cargo/index/"
EOF
```

Notes:
- `pip` and `uv` need the `simple/` endpoint.
- `npm` and `bun` need the `/npm/` endpoint.
- If you run vampire over HTTPS with a trusted certificate, drop `PIP_TRUSTED_HOST` and `UV_INSECURE_HOST`.
- `npm` has other useful env-only toggles for sandboxes because every documented config key can be set through `NPM_CONFIG_*`.

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
- `push` to `main` also publishes `ghcr.io/<owner>/vampire` with `latest` and `sha-<full git sha>` tags from the ARC runner.
