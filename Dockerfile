# Linux benchmark image: builds the server and guests, AOT-compiles the
# guests, and runs bench/loadtest.py (server and load generator in one
# container, so Docker's port forwarding is not part of the measurement).
#
#   docker build -t wasm-server-bench .
#   docker run --rm -v "$PWD/results:/bench/results" wasm-server-bench --label linux-docker
#   docker compose run --rm bench        # database suite, with Postgres

FROM rust:1-bookworm AS build
RUN rustup target add wasm32-wasip2
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY wit wit
COPY server server
COPY guests guests
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/guests/target \
    cargo build --release \
    && cp target/release/wasm-server /usr/local/bin/ \
    && (cd guests && cargo build --release) \
    && mkdir -p /wasm && cp guests/target/wasm32-wasip2/release/*.wasm /wasm/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install oha --version 1.16.0 --locked --root /usr/local

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/wasm-server /usr/local/bin/oha /usr/local/bin/
COPY --from=build /wasm /bench/wasm
COPY bench/loadtest.py bench/catalog-sqlite.sql /bench/
COPY examples /bench/examples
COPY sites /bench/sites
WORKDIR /bench
RUN mkdir apps && for a in hello compute catalog conduit stats shortener; do wasm-server compile wasm/$a.wasm -o apps/$a.cwasm; done
ENTRYPOINT ["python3", "loadtest.py", "--server", "/usr/local/bin/wasm-server"]
