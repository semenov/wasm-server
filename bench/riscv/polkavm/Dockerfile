# PolkaVM's own benchmark tool (tools/benchtool): PolkaVM (recompiler and
# interpreter) vs wasmtime, wasmer, wasmi and native code on the same Rust
# programs. PolkaVM's recompiler exists only for x86-64 Linux; on an arm64
# Mac this image runs under emulation, so only ratios between engines in the
# same run mean anything.
#
#   docker build -t polkavm-bench bench/riscv/polkavm
#   docker run --rm --platform linux/amd64 polkavm-bench
ARG POLKAVM_COMMIT=73f898b

# Guest programs (PolkaVM blobs, wasm32, native x86-64), built on x86-64.
FROM --platform=linux/amd64 rust:1-bookworm AS guests
ARG POLKAVM_COMMIT
RUN git clone https://github.com/paritytech/polkavm /polkavm && git -C /polkavm checkout $POLKAVM_COMMIT
WORKDIR /polkavm/guest-programs
RUN rustup toolchain install nightly-2025-05-10 --profile minimal --component rust-src --target wasm32-unknown-unknown \
    && ./build-benchmarks.sh

# benchtool, cross-compiled for x86-64 on the build machine's own
# architecture: under emulation rustc runs out of memory in a 4 GiB VM.
# Thin LTO and 16 codegen units instead of the repo's full LTO and one unit:
# with generic-sandbox, compiling the polkavm crate as one unit needs more
# than the VM's 4 GiB even natively.
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS tool
ARG POLKAVM_COMMIT
RUN apt-get update && apt-get install -y --no-install-recommends gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
    && rm -rf /var/lib/apt/lists/*
RUN git clone https://github.com/paritytech/polkavm /polkavm && git -C /polkavm checkout $POLKAVM_COMMIT
WORKDIR /polkavm/tools/benchtool
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
# generic-sandbox: under emulation PolkaVM's Linux sandbox (a separate,
# seccomp-filtered process) fails to start; see below.
RUN rustup target add x86_64-unknown-linux-gnu \
    && CARGO_BUILD_JOBS=2 CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release --target x86_64-unknown-linux-gnu --no-default-features \
        --features native,wasmtime,wasmer,wasmi,polkavm/generic-sandbox \
    && cp target/x86_64-unknown-linux-gnu/release/benchtool /benchtool

FROM guests
COPY --from=tool /benchtool /polkavm/tools/benchtool/target/release/benchtool
WORKDIR /polkavm/tools/benchtool
# The in-process "generic" sandbox: the same recompiler and generated code,
# without the separate-process layer. On a real x86-64 Linux host, run with
# --privileged -e POLKAVM_SANDBOX=linux to use PolkaVM's own sandbox.
ENV POLKAVM_SANDBOX=generic POLKAVM_ALLOW_EXPERIMENTAL=1 POLKAVM_CORE_PINNING=disabled
CMD ["./target/release/benchtool", "benchmark", "-i", "20"]
