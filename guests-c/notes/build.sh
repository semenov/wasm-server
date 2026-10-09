#!/usr/bin/env bash
# Builds the C notes app into notes.wasm (a wasm component).
# Needs wasi-sdk (https://github.com/WebAssembly/wasi-sdk/releases) and
# wit-bindgen (`cargo install wit-bindgen-cli`).
#   WASI_SDK=/path/to/wasi-sdk ./build.sh
set -euo pipefail
cd "$(dirname "$0")"
: "${WASI_SDK:?set WASI_SDK to the wasi-sdk directory}"
wit-bindgen c wit --world demo:cnotes/app --out-dir gen
"$WASI_SDK/bin/clang" --target=wasm32-wasip2 -Oz -flto -s -mexec-model=reactor -Wall -Wextra \
    -o notes.wasm notes.c gen/app.c gen/app_component_type.o
ls -l notes.wasm
