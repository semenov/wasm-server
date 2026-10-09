#!/usr/bin/env bash
# Builds the Go notes app into notes.wasm (a wasm component) with TinyGo.
# Needs TinyGo 0.42+ (https://github.com/tinygo-org/tinygo/releases), Go
# 1.25–1.27, wasm-opt from Binaryen, wasm-tools, and wit-bindgen-go
# (`go install go.bytecodealliance.org/cmd/wit-bindgen-go@v0.7.0`).
#
# -gc=leaking: an instance lives for one request, so it never collects;
# the default GC halves throughput and doubles memory here (see
# BENCHMARKS.md). -scheduler=none: no goroutines needed.
set -euo pipefail
cd "$(dirname "$0")"
wit-bindgen-go generate --world demo:gonotes/app --out internal ./wit
tinygo build -target=wasip2 --wit-package ./wit --wit-world demo:gonotes/app \
    -opt=2 -no-debug -gc=leaking -scheduler=none -o notes.wasm .
ls -l notes.wasm
