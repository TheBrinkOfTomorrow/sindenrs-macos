#!/bin/sh
# Build a static (musl) sindenrs for SteamOS inside a throwaway Rust container.
#
# SteamOS ships no C toolchain and its root filesystem is read-only, so the build runs in
# podman (which SteamOS does ship). The result has no runtime dependencies and survives OS
# updates: target/x86_64-unknown-linux-musl/release/sindenrs.
set -eu
cd "$(dirname "$0")/../.."
mkdir -p "$HOME/.cache/sindenrs-build"
podman run --rm -e DEBIAN_FRONTEND=noninteractive \
    -v "$PWD":/src:Z -v "$HOME/.cache/sindenrs-build":/usr/local/cargo/registry:Z -w /src \
    docker.io/library/rust:1-slim sh -c '
        rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1
        apt-get update -qq >/dev/null && apt-get install -y -qq musl-tools >/dev/null 2>&1
        cargo build --release --target x86_64-unknown-linux-musl'
ls -la target/x86_64-unknown-linux-musl/release/sindenrs
