#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

cd "$repo_root"
"$repo_root/scripts/check-fast.sh"
cargo clippy --workspace --all-targets --locked -- -D warnings

if ! cargo +nightly --version >/dev/null 2>&1; then
    echo "nightly toolchain is required: rustup toolchain install nightly --profile minimal" >&2
    exit 1
fi

if ! cargo +nightly udeps --version >/dev/null 2>&1; then
    echo "cargo-udeps is required: cargo install --locked cargo-udeps" >&2
    exit 1
fi

if ! cargo llvm-cov --version >/dev/null 2>&1; then
    echo "cargo-llvm-cov is required: cargo install --locked cargo-llvm-cov" >&2
    exit 1
fi

if ! rustup component list --toolchain nightly --installed | grep -q '^rust-src'; then
    echo "nightly rust-src is required: rustup component add rust-src --toolchain nightly" >&2
    exit 1
fi

if ! rustup component list --installed | grep -q '^llvm-tools'; then
    echo "llvm-tools-preview is required: rustup component add llvm-tools-preview" >&2
    exit 1
fi

cargo test --locked
cargo +nightly udeps --workspace --all-targets --locked
cargo llvm-cov --workspace --locked --summary-only --fail-under-lines 80.5
