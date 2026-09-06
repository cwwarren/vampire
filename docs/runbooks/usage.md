# Vampire Usage

## Start
Rustup selects Rust 1.98.1 from `rust-toolchain.toml`. CI uses the same version. The Docker builder copies this toolchain file into the Alpine Rust image, so Cargo installs and uses 1.98.1 even before a matching official image tag is published.

```bash
VAMPIRE_PUBLIC_BASE_URL=http://127.0.0.1:8080 \
VAMPIRE_MAX_CACHE_SIZE_MB=10000 \
cargo run
```

## Container
```bash
docker run --rm \
  -p 8080:8080 \
  -p 8081:8081 \
  -v vampire-cache:/var/cache/vampire \
  -e VAMPIRE_PUBLIC_BASE_URL=http://127.0.0.1:8080 \
  -e VAMPIRE_MAX_CACHE_SIZE_MB=10000 \
  ghcr.io/cwwarren/vampire:latest
```

Container defaults:
- `VAMPIRE_PKG_BIND=0.0.0.0:8080`
- `VAMPIRE_GIT_BIND=0.0.0.0:8081`
- `VAMPIRE_MANAGEMENT_BIND=127.0.0.1:8082`
- `VAMPIRE_CACHE_DIR=/var/cache/vampire`
- `VAMPIRE_PUBLIC_BASE_URL` has no default and must be set to the externally reachable package-listener origin
- Published tags are `latest` and `sha-<full git sha>`

The unauthenticated management listener is not published by default. To expose it only on a trusted host interface, opt in explicitly:

```bash
docker run --rm \
  -p 8080:8080 \
  -p 8081:8081 \
  -p 127.0.0.1:8082:8082 \
  -v vampire-cache:/var/cache/vampire \
  -e VAMPIRE_PUBLIC_BASE_URL=http://127.0.0.1:8080 \
  -e VAMPIRE_MAX_CACHE_SIZE_MB=10000 \
  -e VAMPIRE_MANAGEMENT_BIND=0.0.0.0:8082 \
  ghcr.io/cwwarren/vampire:latest
```

Listener configuration:
- Each listener uses a single `*_BIND` socket address.
- Supported listener prefixes are `PKG`, `GIT`, and `MANAGEMENT`.
- Resource limits and `VAMPIRE_UPSTREAM_TIMEOUT_MS` must be positive. The timeout is the package request/body deadline; the separate Git client uses the same value for connection setup and idle time between response chunks, with no total Git response deadline.

## Client Configuration
```bash
pip install --index-url http://127.0.0.1:8080/pypi/simple/ <package>
npm config set registry http://127.0.0.1:8080/npm/
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
export NPM_CONFIG_REGISTRY="$VAMPIRE/npm/"
export NPM_CONFIG_FUND=false
export NPM_CONFIG_UPDATE_NOTIFIER=false
```

With the registry configured, `npm search <term>` and `npm audit` work through Vampire without overriding npm audit defaults. Both are uncached. Search supports text, pagination, and quality/popularity/maintenance weights. Audits send dependency names and versions to the public npm registry. Bulk-advisories and quick-audit POSTs have an 8 MiB request limit; search and audit responses have a 128 MiB limit and use the package timeout. Both share upstream admission with package downloads and metadata; rejected audit body reads return 400 and saturation returns 503. Publishing, login, and `npm audit signatures` remain unsupported.

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
- Git traffic accepts both suffixless and `.git`-suffixed repository URLs, normalizing upstream paths to `.git`. It is GitHub-only, read-only, uncached, and path-validated before forwarding. Requests forward only `Git-Protocol`, plus `Content-Type` and `Content-Encoding` on `git-upload-pack`. Responses stream through directly; `git-upload-pack` request bodies use the 8 MiB preforwarding cap.
- If you run vampire over HTTPS with a trusted certificate, drop `PIP_TRUSTED_HOST` and `UV_INSECURE_HOST`.
- `npm` has other useful env-only toggles for sandboxes because every documented config key can be set through `NPM_CONFIG_*`.

## Operational Notes
- Scrape Prometheus metrics from `GET /stats` on the management listener. Package artifact/metadata fetches, artifact joins, and Git forwards are keyed by upstream type. `vampire_npm_search_requests_total` and `vampire_npm_audit_requests_total` separately count route-validated requests, including search HEAD, admission rejections, and audit body rejections. Both are unlabelled and start at zero; neither is included in package metadata counts.
- The management listener is unauthenticated. Leave it on loopback or another trusted internal interface unless you deliberately want to expose operational metadata.
- One vampire process must exclusively own a dedicated cache directory. Vampire writes and exclusively locks `.vampire-cache-v1`; another process using the same directory fails startup. An unmarked nonempty directory is refused unless every entry has an exact current, legacy, or recognized temp cache filename.
- Once the ownership lock is held, every recognized cache temp is orphaned and is removed on startup; unrelated names are left untouched.
- Artifact GET and package metadata GET or HEAD misses are single-flight per representation cache key. Duplicate requests join the leader; cold artifact HEAD, npm search, and npm audit requests use the same capacity bound without joining. Search and audit never read or write cached responses. `VAMPIRE_MAX_UPSTREAM_FETCHES` bounds all these classes, and excess work fails fast.
- Admission saturation returns HTTP 503 without starting or queueing new unique upstream work.
- Upstream and rewritten metadata bodies are capped at 128 MiB; upstream size uses both a `Content-Length` precheck and a streaming byte limit. Non-200 artifact bodies are capped at 1 MiB.
- Buffered metadata has a separate 1 GiB byte-weighted reservation budget. Reservations cover reported or observed input size and bounded rewrite working space, then remain attached to the shared response bytes through delivery.
- All cache entries are committed as one file and served through the opened entry, so replacement or eviction cannot mix headers and body bytes.
- Rewritten npm packuments and PyPI metadata omit upstream `ETag` and `Last-Modified` in client responses; vampire keeps those validators only for its own upstream revalidation. Search results are unmodified and retain validators. PyPI root and project links stay under `/pypi/simple/`; the updated representation refetches old cached metadata once, without invalidating cached artifacts.
- Cached artifact HEAD mirrors cached GET headers; cold artifact HEAD forwards an upstream HEAD. Package metadata HEAD uses the same cache lookup and conditional GET lifecycle as metadata GET, then discards the body. Search HEAD fetches an uncached upstream GET and discards its body.
- A completed entry stays pinned through response handoff, so a successful commit may remain above the cache bound until all waiters have opened it. Final pin drops request bound enforcement through one capacity-one janitor queue, coalescing bursts into at most one follow-up scan.
- Package redirects are followed for at most 10 hops and only when scheme, host, and effective port stay unchanged; credential-bearing and HTTPS-to-HTTP redirects are rejected. Git redirects are disabled.
- Failure logs are JSON lines on stderr with `event=request_failed`, `event=artifact_fetch_failed`, `event=startup_failed`, `event=git_rejected`, `event=git_body_read_failed`, `event=npm_audit_body_read_failed`, or `event=git_stream_failed`.

## Test
```bash
cargo test
cargo test --test integration npm::
cargo test --test real_e2e -- --ignored --test-threads=1 --nocapture
```

`tests/integration/main.rs` is the single integration test target. Feature modules cover cache behavior, Cargo, Git, management metrics, npm, PyPI, and routing; `common.rs` holds the shared HTTP fixtures. Filter by module to run one area without creating separate test binaries.

## CI
- GitHub Actions runs on `ubuntu-latest` hosted runners.
- A newer run cancels any older in-progress run for the same branch or pull-request ref.
- `pull_request` runs `cargo test` and the live suite in parallel for PR validation.
- `push` runs only on `main`, so PR branches do not get an extra duplicate push workflow.
- `push` to `main` also publishes `ghcr.io/<owner>/vampire` with `latest` and `sha-<full git sha>` tags.
