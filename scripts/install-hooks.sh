#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
hooks_dir=${HOOKS_DIR:-"$repo_root/.git/hooks"}

if [ ! -d "$repo_root/.git" ]; then
    echo "git repository not found at $repo_root" >&2
    exit 1
fi

mkdir -p "$hooks_dir"

install_hook() {
    name=$1
    src="$repo_root/.githooks/$name"
    dst="$hooks_dir/$name"

    if [ ! -f "$src" ]; then
        echo "missing hook template: $src" >&2
        exit 1
    fi

    cp "$src" "$dst"
    chmod +x "$dst"
}

install_hook pre-commit
install_hook pre-push

printf 'Installed git hooks in %s\n' "$hooks_dir"
