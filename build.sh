#!/usr/bin/env bash
# Builds the server and the guest apps, then AOT-compiles the guests into apps/.
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release
(cd guests && cargo build --release)

mkdir -p apps
for app in hello compute catalog conduit; do
  ./target/release/wasm-server compile "guests/target/wasm32-wasip2/release/$app.wasm" -o "apps/$app.cwasm"
done
