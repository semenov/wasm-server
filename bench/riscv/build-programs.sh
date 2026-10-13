#!/bin/sh
# Builds every program in programs/ (and CoreMark) three ways:
#   out/native/P   gcc -O2 for the host
#   out/rv/P       riscv64-linux-gnu-gcc -O2, static (libriscv runs Linux ELFs)
#   out/wasm/P.wasm      wasi-sdk clang -O2 (wasm32-wasip1)
#   out/wasm/P.cwasm     precompiled by wasmtime (Cranelift)
#   out/wasm/P.pulley    precompiled for Pulley, wasmtime's interpreter
set -e
mkdir -p out/native out/rv out/wasm
CM="/coremark/core_list_join.c /coremark/core_main.c /coremark/core_matrix.c /coremark/core_state.c /coremark/core_util.c /coremark/posix/core_portme.c"
CMFLAGS="-I/coremark -I/coremark/posix -DPERFORMANCE_RUN=1 -DITERATIONS=0 -DMULTITHREAD=1 -DUSE_CLOCK=1 -DHAS_TIME_H=1"
build() { # name sources extra-flags
    gcc -O2 $3 -DFLAGS_STR='"-O2"' $2 -lm -o out/native/$1
    riscv64-linux-gnu-gcc -O2 -static $3 -DFLAGS_STR='"-O2"' $2 -lm -o out/rv/$1
    /opt/wasi-sdk/bin/clang -O2 $3 -DFLAGS_STR='"-O2"' -D_WASI_EMULATED_PROCESS_CLOCKS $2 -lm -lwasi-emulated-process-clocks -o out/wasm/$1.wasm
    wasmtime compile out/wasm/$1.wasm -o out/wasm/$1.cwasm
    wasmtime compile --target pulley64 out/wasm/$1.wasm -o out/wasm/$1.pulley
}
for f in programs/*.c; do n=$(basename $f .c); build $n $f ""; done
build coremark "$CM" "$CMFLAGS"
ls -la out/*/ | head -60
