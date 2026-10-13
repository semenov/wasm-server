# wasm (wasmtime) vs RISC-V (libriscv) sandboxes, on the same guest code.
#
#   docker build -t sandbox-bench bench/riscv
#   docker run --rm sandbox-bench
FROM rust:1-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake g++ gcc-riscv64-linux-gnu libc6-dev-riscv64-cross clang lld \
    && rm -rf /var/lib/apt/lists/*
# libriscv at a fixed commit (2026-09-30).
RUN git clone https://github.com/libriscv/libriscv /libriscv && git -C /libriscv checkout 2925e57
WORKDIR /bench
COPY guest.c rv_bench.cpp CMakeLists.txt run.sh ./
COPY wasmtime-bench wasmtime-bench
# The guest, three ways.
RUN riscv64-linux-gnu-gcc -O2 -static -march=rv64gc -mabi=lp64d guest.c -o guest.elf \
    && clang --target=wasm32 -O2 -mbulk-memory -nostdlib -Wl,--no-entry -o guest.wasm guest.c
RUN for v in flat paged; do \
        cmake -S . -B build-$v -DCMAKE_BUILD_TYPE=Release -DLIBRISCV_DIR=/libriscv \
            $([ $v = paged ] && echo -DPAGED=ON) >/dev/null \
        && cmake --build build-$v -j"$(nproc)" >/dev/null && cp build-$v/rv-bench rv-bench-$v || exit 1; \
    done
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --manifest-path wasmtime-bench/Cargo.toml --target-dir /tmp/wt \
    && cp /tmp/wt/release/wasmtime-bench wt-bench

# ---- General benchmark: standard programs as native, wasm and RISC-V
# executables, run by the engines' own command-line tools.
RUN apt-get update && apt-get install -y --no-install-recommends time python3 xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sL https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-27/wasi-sdk-27.0-arm64-linux.tar.gz \
        | tar xz -C /opt && mv /opt/wasi-sdk-27.0-arm64-linux /opt/wasi-sdk \
    && curl -sL https://github.com/bytecodealliance/wasmtime/releases/download/v49.0.2/wasmtime-v49.0.2-aarch64-linux.tar.xz \
        | tar xJ -C /opt && cp /opt/wasmtime-v49.0.2-aarch64-linux/wasmtime /usr/local/bin/ \
    && git clone https://github.com/eembc/coremark /coremark && git -C /coremark checkout 1f483d5
# libriscv's CLI, with binary translation through the system compiler.
RUN cmake -S /libriscv/emulator -B /rvlinux -DCMAKE_BUILD_TYPE=Release -DRISCV_BINARY_TRANSLATION=ON \
        -DRISCV_LIBTCC=OFF -DRISCV_ASMJIT=OFF >/dev/null \
    && cmake --build /rvlinux -j"$(nproc)" >/dev/null && cp /rvlinux/rvlinux /usr/local/bin/
COPY programs programs
COPY general.py build-programs.sh ./
RUN sh build-programs.sh
CMD ["sh", "run.sh"]
