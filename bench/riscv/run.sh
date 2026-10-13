#!/bin/sh
# Runs every engine, one JSON line per measurement. Pinned to one CPU.
set -e
run() { taskset -c 2 "$@"; }
run ./rv-bench-flat guest.elf interp native
run ./wt-bench guest.wasm
for mode in interp bintr; do
    rm -f /tmp/rvbintr-*
    CC=gcc run ./rv-bench-flat guest.elf $mode
    rm -f /tmp/rvbintr-*
    CC=gcc run ./rv-bench-paged guest.elf $mode
done
