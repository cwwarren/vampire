# Repository Guidelines

## Project Structure & Module Organization
`src/` contains the proxy implementation. `main.rs` starts the server, `lib.rs` exposes shared code, and the other modules hold the HTTP app, routing, cache, config, stats, and failure logging. `tests/` contains integration coverage, including `real_e2e.rs` for live registry tests. `docs/specs/requirements.md` defines behavior, and `docs/runbooks/usage.md` explains operation. Keep those two documents in sync with code changes.

## Build, Test, and Development Commands
Use Cargo directly:

- `cargo run` starts the proxy locally. Set `VAMPIRE_MAX_CACHE_SIZE`, for example: `VAMPIRE_MAX_CACHE_SIZE=10GiB cargo run`.
- `cargo test` runs the standard test suite.
- `cargo test --test real_e2e -- --ignored --test-threads=1 --nocapture` runs the live end-to-end suite against real PyPI, npm, and Cargo backends.
- `cargo fmt` formats the codebase.
- `cargo check` is the fastest sanity pass while iterating.

## Coding Style & Naming Conventions
Keep the code small, direct, and explicit. Work from first principles, not from existing abstractions. Avoid adding layers until there is a demonstrated third use case. Delete code when possible. Use Rust defaults: 4-space indentation, `snake_case` for functions and modules, `UpperCamelCase` for types, and `SCREAMING_SNAKE_CASE` for constants and env vars. Do not add comments unless they are necessary and requested.

## Testing Guidelines
Add or update tests for every behavior change. Prefer integration-style tests that exercise the public HTTP surface over narrow implementation tests. Keep test names descriptive, for example `pypi_rewrites_artifact_links` or `concurrent_artifact_miss_dedupes_upstream_fetch`. When changing documented behavior, update tests, `docs/specs/requirements.md`, and `docs/runbooks/usage.md` together.

## Commit & Pull Request Guidelines
The current history uses short imperative commit messages, for example `Initial commit`. Continue that style: `Rewrite npm tarball URLs` is good; `fixed stuff` is not. Pull requests should explain the behavior change, call out config or operational impact, list the commands run, and note any live-test coverage or gaps.

## Security & Configuration Tips
This service is intentionally read-only and registry-specific. Do not expand it into a general forward proxy. Treat outbound access, cache persistence, and environment variables such as `VAMPIRE_MAX_CACHE_SIZE` as part of the security boundary.
