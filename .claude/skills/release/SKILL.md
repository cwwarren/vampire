---
name: release
description: Analyze changes since last release, draft release notes, bump version, tag, and publish a GitHub release
allowed-tools:
  - Bash(git log:*)
  - Bash(git diff:*)
  - Bash(git status:*)
  - Bash(git describe:*)
  - Bash(git tag:*)
  - Bash(git add:*)
  - Bash(git commit:*)
  - Bash(git push:*)
  - Bash(gh release:*)
  - Bash(cargo check:*)
  - Agent
  - AskUserQuestion
  - Read
  - Edit
  - Grep
  - Glob
---

## Context

- Working tree: !`git status --short`
- Branch: !`git branch --show-current`
- Latest tag: !`git describe --tags --abbrev=0 2>/dev/null || echo "none"`
- Cargo.toml version: !`grep '^version' Cargo.toml`
- Commits since last release: !`git log --oneline $(git describe --tags --abbrev=0 2>/dev/null)..HEAD 2>/dev/null || echo "no previous tag"`
- Diffstat since last release: !`git diff --stat $(git describe --tags --abbrev=0 2>/dev/null)..HEAD 2>/dev/null || echo "no previous tag"`

## Preflight checks

Stop immediately if any check fails:

1. Working tree must be clean (no output from working tree status above). If dirty: "Uncommitted changes detected. Commit or stash before releasing."
2. Branch must be `main`. If not: "Releases must be cut from main."
3. There must be at least one commit since the latest tag. If none: "No changes since the last release."

## Step 1: Analyze changes

Get the latest tag name and the full list of changed files since that tag.

Partition changed files into groups by concern area. Use your judgment, but typical groups are:
- Core proxy logic (`src/proxy.rs`, `src/cache.rs`, `src/state.rs`, `src/config.rs`, `src/routes.rs`)
- Registry modules (`src/npm.rs`, `src/pypi.rs`, `src/cargo.rs`, `src/git.rs`)
- Tests (`tests/`)
- Infra, CI, docs, config (`Dockerfile`, `.github/`, `docs/`, `Cargo.toml`, `Cargo.lock`)

Launch parallel Agent sub-agents (one per group that has changes). Each agent receives:
- The relevant diff: `git diff {tag}..HEAD -- {files}`
- Prompt: "Analyze these changes. For each meaningful change, classify it as one of: new feature, behavior change, bug fix, breaking change (new required config, removed/renamed routes, changed defaults, removed functionality). Return a structured list of bullet points. Be specific and concrete."

Collect all agent results before proceeding.

## Step 2: Determine version bump

From the agent summaries, classify the release. This project is pre-1.0, so semver rules are:

- **Minor bump** (0.x.y -> 0.{x+1}.0): any breaking change -- new required env vars, removed routes, changed default behavior, renamed config, removed public API
- **Patch bump** (0.x.y -> 0.x.{y+1}): non-breaking features, fixes, dependency updates, docs, CI changes

Parse the current version from the latest tag (strip the `v` prefix) and compute the new version string.

## Step 3: Draft release notes

Format release notes as markdown:

```
## What's new

### Breaking
- {description}

### Added
- {description}

### Changed
- {description}

### Fixed
- {description}
```

Rules:
- Omit empty sections entirely
- Each bullet: one sentence, concrete, user-facing language
- Write from the operator's perspective, not the developer's
- Reference env vars, routes, or config names where relevant
- Do not include test-only or formatting-only changes

## Step 4: Confirm with user

Use AskUserQuestion to present:

1. The proposed version bump: `{current} -> {new}`
2. The drafted release notes
3. Ask the user to confirm, adjust the version, or edit the notes

If the user requests changes, incorporate them and re-confirm. If the user declines, stop.

## Step 5: Execute the release

Only after explicit user confirmation:

1. Edit `Cargo.toml`: update the `version = "..."` line to the new version
2. Run `cargo check` to verify the project compiles and update `Cargo.lock`
3. Stage and commit: `git add Cargo.toml Cargo.lock && git commit -m "v{VERSION}"`
4. Tag: `git tag v{VERSION}`
5. Push commit and tag: `git push origin main --follow-tags`
6. Create GitHub release using a heredoc for the notes body:
   ```
   gh release create v{VERSION} --title "v{VERSION}" --notes "$(cat <<'EOF'
   {RELEASE_NOTES}
   EOF
   )"
   ```

Report the release URL when done.
