#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
    cargo_home="${CARGO_HOME:-}"
    if [[ -z "$cargo_home" && -n "${HOME:-}" ]]; then
        cargo_home="$HOME/.cargo"
    fi
    if [[ -n "$cargo_home" && -x "$cargo_home/bin/cargo" ]]; then
        export PATH="$cargo_home/bin:$PATH"
    else
        echo "cargo not found" >&2
        exit 1
    fi
fi

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo build --workspace --release --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
