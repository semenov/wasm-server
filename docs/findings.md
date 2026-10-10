# Findings

What the experiments in this repo taught us, beyond the numbers in
`BENCHMARKS.md`. Dated where it can go stale.

## Where wasm fits (and where it doesn't)

Measured against the same app as a native Go server (`bench/native-go`):

- **Per-request speed is comparable.** wasm reads faster with better tail
  latency; native Go writes faster and spends less CPU per request.
- **Density is the real difference.** An extra app costs about 660 KB in one
  `wasm-server` versus about 10 MB as its own Go process, and nothing while
  idle; an instance starts in 6 µs.

So wasm is not a faster way to host *a* web app. For one app with one owner,
an ordinary server is simpler: the full library ecosystem, state in memory,
no WIT or guest toolchains. wasm is the way to host *many* small apps from
different, untrusted authors (our case: apps written by AI agents): sandbox
per request, no network or files unless granted, precise metering, instant
deploys. The alternative for untrusted code, a container or microVM per app,
costs 10–100× more memory and seconds to start.

The price of the model: there's no state in memory between requests, so every
stateful feature (cache, queues, WebSockets, cron) has to live in the host.

### A typical backend: Conduit as wasm, Go and Rust (2026-10-10)

The same Conduit API as a wasm app, a `net/http` + pgx server and an axum +
tokio-postgres server, all on the same Postgres and the same 8 CPUs
(`bench/native-conduit`, numbers in `BENCHMARKS.md`):

- **With a database, throughput is the same.** Postgres is the limit long
  before any of the servers. wasm's overhead is about 30–40 µs of server CPU
  per request (instance, WASI context, `wasi:http` copies). It matters only
  for endpoints that don't touch the database: `hello` and cache hits run
  at about 55–60% of native throughput.
- **Steady-state memory is higher for wasm:** 60–90 MiB vs 30–35 (Go) and
  13–18 (Rust) for one app. The per-app cost that matters for density
  (about 660 KB per extra app vs a process per app) is measured separately
  above.
- **Peak memory is most predictable with wasm.** An instance's memory goes
  back to the pool when the request ends. Memory-heavy requests (argon2,
  19 MiB) left Go holding about 500 MiB after the load, and left Rust with
  glibc malloc holding about 3 GB until it was OOM-killed.
  - The Rust problem is glibc's dynamic mmap threshold, not a leak. A fixed
    `MALLOC_MMAP_THRESHOLD_` fixes it; `MALLOC_ARENA_MAX` does not.
  - Untrusted apps can't be relied on to tune their allocator, so this is a
    real argument for per-request linear memory.
- **Our weak spot is head-of-line blocking.**
  - **The problem:** a CPU-heavy request (25 ms argon2) delays the other
    requests on its worker. In a mix with 2% logins, cheap endpoints had a
    p99 of 50–66 ms on wasm vs 16–19 ms native; without the logins the
    difference disappears.
  - **Why:** thread-per-core keeps a request on the worker that accepted it,
    and guests yield only every 10 ms epoch tick.
  - **Possible fixes:**
    - work stealing (tokio's multi-thread runtime) at some cost per request;
    - shorter ticks for guests that have run a long time;
    - moving CPU-heavy instances off the accepting worker.
- **Benchmark hygiene:** environment variables given to the whole benchmark
  container also reach the load generator. A glibc setting meant for the
  server made oha faster and the server look 25–40% faster. Set server-only
  settings on the server alone.

## Guest languages

The same notes app runs in C, Rust and Go (TinyGo) with no server changes;
`platform:sdk` is language-neutral.

- **Throughput doesn't depend on the guest's language:** within about 3%.
  The server and SQLite dominate.
- **Memory per instance is a toolchain default.** Rust reserved a 1 MiB stack
  (1,088 KiB per instance); `guests/.cargo/config.toml` now sets 128 KiB
  (192 KiB per instance). Overflow traps cleanly (`out of bounds memory
  access`, see `compute?stack=N`), because the stack sits below the data.
- **Size:** C 55 KB, Rust 147 KB, Go 578 KB (58–67 KB without
  `encoding/json`). In Rust, Debug formatting for `unwrap`/`expect` is the
  largest part; our code is about 9% (see `BENCHMARKS.md`).
- **TinyGo needs `-gc=leaking`.** An instance lives for one request, so a GC
  that never collects is right; the default GC makes instantiation 6.5×
  slower and halves throughput.
- **C costs the most to write:** manual buffers, escaping and memory, and a
  verbose binding API.

Toolchain notes (October 2026):

- C: wasi-sdk 34 + `wit-bindgen c`; the WASI WIT files have to be copied in.
- TinyGo 0.42 needs `wit-bindgen-go` from `go.bytecodealliance.org` (v0.7.0,
  with `cm` v0.3.0), not `wit-bindgen go`, whose output needs
  `runtime.Pinner`, which TinyGo lacks. The world must
  `include wasi:cli/imports`, and `wasm-opt` (Binaryen) must be installed.
  The newer official Go route is `componentize-go`.
- Rust `-Cpanic=immediate-abort` (nightly, with `-Zbuild-std`) cuts sizes by
  half or more but drops panic messages; not adopted.

## Outbound access from apps

Checked with a probe app (`?url=` through `wasi:http`, `?tcp=` through
`std::net`):

| Target | Result |
|---|---|
| External HTTPS | Works (wasmtime's default client, TLS on the host) |
| **`127.0.0.1`: our admin endpoints, other local services** | **Reachable** |
| Cloud metadata `169.254.169.254` | Refused on a Mac; would be reachable on a cloud VM |
| Raw TCP (`wasi:sockets`) anywhere | Denied (the default address check refuses all) |
| A hanging upstream | Cut by the request's wall-clock limit (504) |

**This is an open SSRF hole.** `WasiHttpView::http` passes default hooks, so
`send_request` has no policy. Planned fix, all in the `send_request` hook
(shared by WASI 0.2 and 0.3 in wasmtime 49), no app changes:

1. Deny loopback, private, link-local and metadata addresses by default;
   check after DNS resolution and connect to the checked IP.
2. A per-app allow-list in the TOML (`outbound = ["api.example.com"]`).
3. Count outbound requests and bytes in stats and billing.
4. A connection pool per worker (now every call does a new TCP and TLS
   handshake, about 190 ms to GitHub) and connect and first-byte timeouts.

## WASI status (checked 2026-10-10)

- **WASI 0.3.0 was ratified on 2026-06-11**; 0.3.1 came on 2026-08-11. A
  point release follows every two months (0.3.2 on 2026-10-13, 0.3.3 on
  2026-12-08). Async is native to the component model: `stream<T>`,
  `future<T>` and `async func` replace `wasi:io`'s pollables and streams, and
  `wasi:io` is gone. `wasi:http`'s `proxy` world became `service` and
  `middleware`, so components can be chained in-process. 0.3.1 adds `map<K, V>`
  and `implements` (several named instances of one interface) to WIT.
- **Runtimes and toolchains:** wasmtime has had 0.3 on by default since 46
  (we use 49, so the server can already host 0.3 components next to 0.2
  ones; the wasip3 code is new, and 49.0.2 fixed security bugs in it). The
  Rust `wasm32-wasip3` target is on its way to Tier 2 (nightly now, stable
  planned for 1.100). jco supports 0.3; Go has `componentize-go`; Python,
  C# and C bindings are in progress.
- **Phase 3** (the standard set): clocks, random, filesystem, sockets, cli,
  http. Nothing is at phase 4 or 5 yet.
- **Phase 2:** keyvalue, messaging and config are near-dormant (still 0.2
  drafts, about one commit a year); wasi-nn hasn't changed since 2024-10;
  WebGPU is active (0.3.0-rc.2, 2026-07). Also timezone, informational
  responses in http, I2C.
- **Phase 1:** blob store, SQL, logging, OTel, TLS, crypto, threads and more.
  Only crypto and TLS show recent activity.

Conclusions for us:

- Don't implement the stalled `wasi:keyvalue`/`wasi:config` for
  portability; `platform:sdk` loses nothing by comparison.
- WASI 0.3 is the meaningful next step: concurrent SQL and HTTP calls from one
  request as plain `async` code, natural streaming (SSE, LLM tokens), and the
  `middleware` world fits typed calls between functions. Try one 0.3 app on
  nightly Rust next to the 0.2 ones; move fully once `wasm32-wasip3` is
  stable. Streaming will also need a longer request limit than 10 s.

Sources: [WASI proposals](https://github.com/WebAssembly/WASI/blob/main/docs/Proposals.md),
[release schedule](https://github.com/WebAssembly/WASI/blob/main/docs/Release.md),
[WASI 0.3.0 notes](https://github.com/WebAssembly/WASI/releases/tag/v0.3.0),
[WASI 0.3 announcement](https://bytecodealliance.org/articles/WASI-0.3),
[`wasm32-wasip3`](https://doc.rust-lang.org/nightly/rustc/platform-support/wasm32-wasip3.html).

## What `wasi:http` 0.2 offers

Incoming (`incoming-handler`) and outgoing (`outgoing-handler`) HTTP:

- **Requests:** any method, headers with host-enforced forbidden ones.
- **Bodies:** streaming request and response bodies with trailers.
- **Outgoing calls:** they return futures, so one instance can run several
  calls in parallel via `wasi:io/poll`; per-request connect, first-byte and
  between-bytes timeouts.
- **Errors:** a detailed `error-code`.

The host owns TLS, DNS, HTTP versions, pooling and redirects, which is why
outbound policy belongs in one host hook.

Not in the spec:

- WebSockets or Upgrade (hence `platform:sdk/websocket`);
- the client's IP;
- a cookie jar.
