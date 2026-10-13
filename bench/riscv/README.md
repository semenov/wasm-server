# wasm vs RISC-V sandboxes

Is a RISC-V emulator a faster sandbox than wasm? This directory compares
wasmtime with libriscv (and, where possible, PolkaVM) on the same code.
Results are in `BENCHMARKS.md` ("Sandboxes: wasm vs RISC-V") and
`docs/findings.md`.

**Engines:**

| Engine | What it is | Safe for untrusted code? |
|---|---|---|
| wasmtime (Cranelift) | wasm, compiled to native code; the server's engine | yes (its design goal) |
| wasmtime (Pulley) | wasmtime's portable interpreter | yes |
| libriscv | RISC-V emulator, interpreter | yes |
| libriscv + binary translation | RISC-V translated to C, compiled by gcc, loaded as native code | **no**, per libriscv's own docs |
| PolkaVM | RISC-V (rv32e/rv64e) single-pass recompiler, guest in a separate seccomp process | its design goal; recompiler only on x86-64 |

**Two test sets:**

- **`rv_bench.cpp`, `wasmtime-bench/`:** the same C functions
  (`guest.c`), measured inside the host process.
  - compute: fib, sieve, xorshift mix;
  - the cost of a call into a live sandbox;
  - a fresh sandbox per request (wasmtime: pooled instance; libriscv:
    copy-on-write fork of a booted template), with 0, 64 KiB or 1 MiB
    written;
  - memory per live sandbox;
  - an isolation check (a fork's writes don't reach the template).
- **`general.py`:** standard programs (CoreMark, nbody, fannkuch,
  spectral-norm, mandelbrot, binary-trees, sha256, qsort, a hash map),
  built natively, for wasm32-wasip1 (wasi-sdk 27) and for rv64gc (static
  glibc).
  - run by the engines' own CLIs (`wasmtime run`, `rvlinux`), pinned to
    one CPU;
  - best of three runs, peak RSS, output checked against native.

**Run:**

```sh
docker build -t sandbox-bench bench/riscv
docker run --rm sandbox-bench                     # rv_bench + wasmtime-bench
docker run --rm -v "$PWD/results:/bench/results" sandbox-bench python3 general.py
```

**PolkaVM:** `polkavm/Dockerfile` builds PolkaVM's own `benchtool`
(PolkaVM vs wasmtime, wasmer, wasmi, native). It needs an x86-64 Linux
host. On an arm64 Mac it runs under emulation, and there:
- PolkaVM's Linux sandbox fails to start;
- the in-process fallback (`generic-sandbox`) didn't compile within
  Docker's 4 GiB.

So PolkaVM is not measured yet.

**Caveats:**

- **Platform:** arm64 (Apple M3 Max, Docker Linux VM). libriscv's author
  reports x86-64 numbers, and its binary translation may do better there.
- **Compilers:** native and RISC-V builds use gcc 12; wasm uses clang 20.
  The libcs differ too (glibc vs wasi-libc), which is why `sort` (qsort)
  is slow on wasm.
- **No preemption in the CLI runs:** neither engine is set up for it
  (wasmtime without epochs or fuel, rvlinux without an instruction limit).
  `rv_bench`/`wasmtime-bench` do include them (epoch deadline, instruction
  limit).
