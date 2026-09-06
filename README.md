# Vampire

Vampire is a Rust proxy for read-only PyPI, npm, and Cargo package installs, including git-pinned GitHub dependencies.

It is built for controlled environments such as agent sandboxes, where package installs need to work without opening general outbound internet access. Vampire is intentionally narrow: it serves package-manager traffic, keeps artifacts on local disk, and bounds cache growth with `VAMPIRE_MAX_CACHE_SIZE_MB`. It is not a general web proxy, and it does not implement publish or auth APIs.

## Run

Container-first:

```bash
docker run --rm \
  -p 8080:8080 \
  -p 8081:8081 \
  -v vampire-cache:/var/cache/vampire \
  -e VAMPIRE_PUBLIC_BASE_URL=http://127.0.0.1:8080 \
  -e VAMPIRE_MAX_CACHE_SIZE_MB=10000 \
  ghcr.io/cwwarren/vampire:latest
```

Local build:

```bash
VAMPIRE_PUBLIC_BASE_URL=http://127.0.0.1:8080 \
VAMPIRE_MAX_CACHE_SIZE_MB=10000 \
cargo run
```

Useful server env vars:
- `VAMPIRE_PUBLIC_BASE_URL` required; externally reachable package-listener origin
- `VAMPIRE_MAX_CACHE_SIZE_MB` required and positive
- `VAMPIRE_PKG_BIND` default `127.0.0.1:8080`
- `VAMPIRE_GIT_BIND` default `127.0.0.1:8081`
- `VAMPIRE_MANAGEMENT_BIND` default `127.0.0.1:8082`
- `VAMPIRE_CACHE_DIR` default `./.cache/vampire`
- `VAMPIRE_MAX_UPSTREAM_FETCHES` default `32` and positive; shared upstream admission bound
- `VAMPIRE_UPSTREAM_TIMEOUT_MS` default `30000` and positive; package total timeout, Git connect and idle-read timeout

## Sandbox Usage

For ephemeral sandboxes, prefer environment variables over persistent config files.

Git-pinned dependencies (`pip install git+https://github.com/...`, `cargo { git = "..." }`, npm `git+https://` deps) need the git listener. pip, uv, npm, and cargo all shell out to the system `git` binary, so a single URL rewrite redirects their GitHub git traffic through vampire:

```bash
export VAMPIRE_GIT=http://127.0.0.1:8081
tmpdir=$(mktemp -d)
git config --file "$tmpdir/gitconfig" \
  url.$VAMPIRE_GIT/.insteadOf https://github.com/
export GIT_CONFIG_GLOBAL="$tmpdir/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
export GIT_TERMINAL_PROMPT=0
```

Then configure each package manager's registry:

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
export NPM_CONFIG_REGISTRY="$VAMPIRE/npm/"
export NPM_CONFIG_FUND=false
export NPM_CONFIG_UPDATE_NOTIFIER=false
```

`npm search <term>` and `npm audit` use the same registry setting and remain uncached. Audits forward dependency names and versions to npm; the examples leave npm audit defaults unchanged. Both suffixless and `.git`-suffixed GitHub dependency URLs are supported by the Git rewrite above.

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

[net]
git-fetch-with-cli = true
EOF
```

## Behavior

Vampire exposes only the registry-specific paths it needs for PyPI, npm, and Cargo on its package listener, and a read-only GitHub smart-HTTP surface on its git listener for git-pinned package dependencies. The proxy keeps artifact downloads on its own URLs by rewriting PyPI and npm metadata before returning it to clients.

On a cache hit, vampire serves the artifact directly from disk. On a miss, one request fetches the artifact from upstream, commits it to the cache, and then serves it; concurrent followers wait for that completed result. Artifact and metadata leaders, cold artifact HEAD requests, and uncached npm search and audit requests share the `VAMPIRE_MAX_UPSTREAM_FETCHES` admission bound, and excess work fails fast instead of building an unbounded queue. Package metadata is cached only when the upstream response includes validators such as `ETag` or `Last-Modified`. Metadata, search, and audit response bodies are capped at 128 MiB; audit POST bodies are capped at 8 MiB. Every cache entry is committed as one atomically-renamed file and served through a stable open handle.

## More

Operational details and test commands live in [docs/runbooks/usage.md](docs/runbooks/usage.md).
