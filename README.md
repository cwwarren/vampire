# Vampire

Vampire is a minimal Rust proxy for read-only PyPI, npm, and Cargo package installs.

It is built for controlled environments such as agent sandboxes, where package installs need to work without opening general outbound internet access. Vampire is intentionally narrow: it serves package-manager traffic, keeps artifacts on local disk, and bounds cache growth with `VAMPIRE_MAX_CACHE_SIZE`. It is not a general web proxy, and it does not implement publish or auth APIs.

## Run

Container-first:

```bash
docker run --rm \
  -p 8080:8080 \
  -v vampire-cache:/var/lib/vampire \
  -e VAMPIRE_MAX_CACHE_SIZE=10GiB \
  ghcr.io/cwwarren/vampire:latest
```

Local build:

```bash
VAMPIRE_MAX_CACHE_SIZE=10GiB cargo run
```

Useful server env vars:
- `VAMPIRE_MAX_CACHE_SIZE` required
- `VAMPIRE_BIND` default `127.0.0.1:8080`
- `VAMPIRE_CACHE_DIR` default `./.cache/vampire`
- `VAMPIRE_MAX_UPSTREAM_FETCHES` default `32`
- `VAMPIRE_UPSTREAM_TIMEOUT` default `30s`

## Sandbox Usage

For ephemeral sandboxes, prefer environment variables over persistent config files.

`pip`:

```bash
export VAMPIRE=http://127.0.0.1:8080
export PIP_CONFIG_FILE=/dev/null
export PIP_INDEX_URL="$VAMPIRE/pypi/simple/"
export PIP_TRUSTED_HOST=127.0.0.1
```

`uv`:

```bash
export VAMPIRE=http://127.0.0.1:8080
export UV_NO_CONFIG=1
export UV_DEFAULT_INDEX="$VAMPIRE/pypi/simple/"
export UV_INSECURE_HOST=127.0.0.1
```

`npm`:

```bash
export VAMPIRE=http://127.0.0.1:8080
export NPM_CONFIG_USERCONFIG=/dev/null
export NPM_CONFIG_GLOBALCONFIG=/dev/null
export NPM_CONFIG_REGISTRY="$VAMPIRE/npm/"
export NPM_CONFIG_AUDIT=false
export NPM_CONFIG_FUND=false
export NPM_CONFIG_UPDATE_NOTIFIER=false
```

`bun`:

```bash
export VAMPIRE=http://127.0.0.1:8080
export BUN_CONFIG_REGISTRY="$VAMPIRE/npm/"
```

`cargo`:

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

## Behavior

Vampire exposes only the registry-specific paths it needs for PyPI, npm, and Cargo, including PyPI simple pages and file downloads, npm package metadata and tarballs, and Cargo sparse index and crate download endpoints. The proxy keeps artifact downloads on its own URLs by rewriting PyPI and npm metadata before returning it to clients.

On a cache hit, vampire serves the artifact directly from disk. On a miss, one request fetches the artifact from upstream, commits it to the cache, and then serves it; any concurrent followers wait for that completed result instead of triggering another fetch. Only completed artifacts are ever served to clients. Metadata is cached more conservatively and only when the upstream response includes validators such as `ETag` or `Last-Modified`. Those metadata cache entries are committed as single files, while concurrent cold metadata requests are still allowed to race and populate cache on a best-effort basis.

## More

Operational details and test commands live in [docs/runbooks/usage.md](docs/runbooks/usage.md).
