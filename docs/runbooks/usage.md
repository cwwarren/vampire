# Vampire Usage

## Start
```bash
VAMPIRE_MAX_CACHE_SIZE_MB=10000 cargo run
```

## Container
```bash
docker run --rm \
  -p 8080:8080 \
  -p 8081:8081 \
  -v vampire-cache:/var/cache/vampire \
  -e VAMPIRE_MAX_CACHE_SIZE_MB=10000 \
  ghcr.io/cwwarren/vampire:latest
```

Container defaults:
- `VAMPIRE_PKG_BIND=0.0.0.0:8080`
- `VAMPIRE_GIT_BIND=0.0.0.0:8081`
- `VAMPIRE_CACHE_DIR=/var/cache/vampire`
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

Git-pinned dependencies (`pip install git+https://github.com/...`, `cargo { git = "..." }`, npm `git+https://` deps) need the git listener. Persist the URL rewrite in a temporary git config and export the env vars alongside PM-specific config:

```bash
tmpdir=$(mktemp -d)
git config --file "$tmpdir/gitconfig" \
  url.http://127.0.0.1:8081/.insteadOf \
  https://github.com/
export GIT_CONFIG_GLOBAL="$tmpdir/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
export GIT_TERMINAL_PROMPT=0
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

[net]
git-fetch-with-cli = true
EOF
```

Notes:
- `pip` and `uv` need the `simple/` endpoint.
- `npm` and `bun` need the `/npm/` endpoint.
- Git-pinned dependencies use a separate listener on `VAMPIRE_GIT_BIND` (default `127.0.0.1:8081`). pip, uv, npm, and cargo all shell out to the system `git` binary, so `GIT_CONFIG_GLOBAL` with a `url.*.insteadOf` rewrite redirects their GitHub git traffic through vampire. Cargo requires `net.git-fetch-with-cli = true` in its config to use the system git (it defaults to its own git implementation which does not respect `GIT_CONFIG_GLOBAL`).
- Git traffic is GitHub-only, read-only, uncached, and path-validated before forwarding. Responses stream through directly; `git-upload-pack` request bodies use the 8 MiB preforwarding cap.
- If you run vampire over HTTPS with a trusted certificate, drop `PIP_TRUSTED_HOST` and `UV_INSECURE_HOST`.
- `npm` has other useful env-only toggles for sandboxes because every documented config key can be set through `NPM_CONFIG_*`.

## Operational Notes
- One vampire process must own a cache directory.
- `*.part` files are in-flight downloads and are cleaned on startup if stale.
- Artifact misses are single-flight per cache key. Vampire waits for the full upstream artifact before replying, then serves the committed file.
- Metadata cache fill is best-effort. Concurrent cold metadata requests can fetch upstream in parallel and race to populate cache.
- Cached metadata is committed as one file, so readers do not see mixed metadata headers and body bytes during revalidation.
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
