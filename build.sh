#!/usr/bin/env bash
# Builds the plugin component into dist/plugin.wasm (then: Settings › Plugins › Reload in OpenIDEA).
set -euo pipefail
cd "$(dirname "$0")"
rustup target list --installed | grep -qx wasm32-wasip2 || rustup target add wasm32-wasip2
cargo test --quiet
cargo build --release --target wasm32-wasip2
mkdir -p dist
cp target/wasm32-wasip2/release/claudesurf.wasm dist/plugin.wasm
echo "dist/plugin.wasm ($(wc -c <dist/plugin.wasm | tr -d ' ') bytes)"
