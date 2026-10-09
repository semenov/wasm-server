# Benchmarks

Measured on 2026-10-09.

**Environment:** Apple M3 Max (10 performance + 4 efficiency cores), running a
Docker Desktop Linux VM (kernel 6.12.5-linuxkit, aarch64, 14 vCPUs, 4 GiB RAM).
The server and the load generator (oha 1.16) run in the same container, so
they share the 14 vCPUs.

**Method:** every request instantiates a fresh copy of the component, and HTTP/1.1
keep-alive is used. Each scenario gets a 2 s warmup followed by 8 s of
measurement. The server runs with 8 workers, leaving the rest of the CPU to oha.
Raw numbers are in `results/*.json`.

**Reproduce:**

```sh
docker build -t wasm-server-bench .
docker run --rm -v "$PWD/results:/bench/results" wasm-server-bench --label X -- --workers 8 --keep-resident-kib 2048
docker run --rm --entrypoint wasm-server wasm-server-bench bench apps/hello.cwasm --mode handle -t 4
```

## Runtime only, no HTTP (`wasm-server bench`, `hello`)

| | default slot reset | `keep_resident` 2 MiB |
|---|---|---|
| Instantiate, 1 thread, p50 | 6.8 µs | **5.7 µs** |
| Instantiate + handler, 1 thread, p50 | 18.9 µs | **13.8 µs** |
| Instantiate + handler, 1 thread | 52k/s | **71k/s** |
| Instantiate + handler, 8 threads | 163k/s | **367k/s** |
| Instantiate + handler, 14 threads | 188k/s | **476k/s** |

## HTTP load (`bench/loadtest.py`, 8 workers, `keep_resident` 2 MiB, the default)

| Scenario | RPS | p50 ms | p99 ms | p99.9 ms | Server CPU (cores) | Server CPU µs/req | RSS peak MiB | Guest mem/instance |
|---|---|---|---|---|---|---|---|---|
| hello, c=1 | 18,696 | 0.06 | 0.07 | 0.10 | 0.65 | 34.9 | 16 | 1.06 MiB |
| hello, c=16 | 117,731 | 0.12 | 0.31 | 1.34 | 5.59 | 47.9 | 36 | 1.06 MiB |
| hello, c=64 | 238,540 | 0.25 | 0.65 | 1.20 | 7.60 | 32.7 | 50 | 1.06 MiB |
| hello, c=256 | 246,299 | 0.98 | 2.16 | 3.92 | 7.67 | 31.9 | 116 | 1.06 MiB |
| fib(20), c=64 | 109,598 | 0.58 | 1.15 | 1.57 | 7.90 | 72.8 | 116 | 1.06 MiB |
| alloc 8 MiB, c=64 | 8,132 | 7.17 | 16.56 | 18.70 | 6.63 | 816.9 | 161 | 9.06 MiB |

Idle RSS: 10.7 MiB. All scenarios returned 100% `200 OK`. A rerun after the
database and CPU-accounting changes (`results/linux-docker-basic.json`) agrees
within ±5%.

With Wasmtime's default slot reset (`--keep-resident-kib 0`), `hello` reaches
126k RPS at c=64 and c=256 with 63 µs of CPU per request: half the throughput
at twice the CPU cost.

## With Postgres (`catalog` app, `docker compose run --rm bench`)

Postgres 17 runs in a second container on the same VM (`synchronous_commit=off`;
see `docker-compose.yml`). There are 10,000 products, and every request hits a
random product id. The server has 8 workers with 4 pooled connections each (32
connections in total). Raw numbers: `results/linux-docker-db.json`.

| Scenario | RPS | p50 ms | p99 ms | p99.9 ms | Server CPU µs/req | DB calls/req | DB wait µs/req |
|---|---|---|---|---|---|---|---|
| Product by id, c=1 | 8,810 | 0.12 | 0.16 | 0.25 | 55.9 | 1 | 46 |
| Product by id, c=64 | **151,317** | 0.37 | 1.06 | 1.72 | 40.7 | 1 | 200 |
| Product by id, c=256 | 157,049 | 1.60 | 2.77 | 4.80 | 39.7 | 1 | 1,301 |
| List 20 products, c=64 | 78,366 | 0.76 | 1.94 | 2.84 | 57.4 | 1 | 519 |
| Order: transaction with BEGIN, UPDATE, INSERT, COMMIT, c=64 | 45,348 | 1.36 | 3.11 | 4.41 | 105.9 | 4 | 1,234 |

All scenarios returned 100% 2xx responses. Peak RSS was 160 MiB.

- **What a database call costs the server:** about 41 µs of server CPU for a
  request that makes one query, compared with 33 µs for `hello`. So a database
  call adds roughly 8 µs of host CPU: pool checkout, protocol encoding and
  decoding, and converting rows to WIT values.
- **Where the time goes:** at c=1 a request takes about 120 µs end to end, of
  which 46 µs is the database round trip and roughly 20 µs is the wasm instance
  and guest code.
- **What limits throughput:** at c=64, Postgres used about 3 cores and the
  server and oha together about 8.4, out of the VM's 14 vCPUs; the rest goes to
  container networking. The machine as a whole is saturated rather than any one
  component.
- **Waiting is free:** while a guest waits on the database, its worker serves
  other requests. At c=256 each request waits 1.3 ms on Postgres, yet server CPU
  per request stays at about 40 µs.

## Typical backend: RealWorld "Conduit" API (`conduit` app)

A Medium-like blog API: JWT auth, argon2id passwords, articles with tags,
favorites, comments, follows and a personal feed. Seed data: 10k users, 50k
articles, 100k tags, 200k favorites, 150k comments and 200k follows
(`bench/conduit.sql`). Postgres is reached over TLS with `sslmode=verify-full`.
The server has 8 workers. Raw numbers: `results/linux-docker-conduit*.json`.

**One endpoint at a time** (64 connections; login 16):

| Endpoint | RPS | p50 ms | p99 ms | Server CPU µs/req | DB calls | DB wait µs |
|---|---|---|---|---|---|---|
| Get article | 94,305 | 0.64 | 1.39 | 53 | 1 | 379 |
| Profile (auth) | 114,903 | 0.52 | 1.18 | 50 | 1 | 280 |
| Article comments | 62,852 | 0.94 | 2.71 | 62 | 1 | 790 |
| Favorite (transaction, 5 round trips) | 38,610 | 1.42 | 4.08 | 126 | 5 | 1,443 |
| Create article (transaction) | 26,916 | 2.19 | 5.26 | 164 | 5 | 2,158 |
| Feed (auth) | 20,365 | 2.95 | 7.02 | 202 | 2 | 2,688 |
| List articles + total count | 2,230 | 27.5 | 61.7 | 282 | 2 | 28,274 |
| List by tag + count | 1,104 | 50.2 | 140.0 | 318 | 2 | 57,327 |
| Popular tags | 1,070 | 57.9 | 114.2 | 171 | 1 | 59,237 |
| Login (argon2id, 19 MiB) | 243 | 61.8 | 126.2 | 24,805 | 1 | 16,319 |

**Realistic mix** (128 connections split by weight: 25% list, 25% article, 15%
comments, 10% list by tag, 10% feed, 5% profile, 5% favorite, 3% create, 2%
login; clean data):

| Endpoint | Conns | RPS | p50 ms | p99 ms |
|---|---|---|---|---|
| list | 32 | 674 | 38.6 | 186.1 |
| list by tag | 13 | 237 | 39.8 | 207.4 |
| article | 32 | 2,474 | 7.0 | 99.4 |
| comments | 19 | 1,593 | 7.5 | 77.5 |
| feed | 13 | 594 | 14.8 | 163.9 |
| profile | 6 | 520 | 9.4 | 38.4 |
| favorite | 6 | 253 | 12.7 | 139.8 |
| create | 4 | 104 | 21.4 | 142.5 |
| login | 3 | 33 | 95.4 | 161.3 |
| **Total** | 128 | **6,482** | | ≤ 207 |

All requests succeeded. Server: 3.0 cores, 222 MiB peak RSS. **Postgres: about
10.6 cores**, measured with `docker stats` during the run.

What this shows:

- **The database is the bottleneck, not the wasm runtime.** Simple endpoints
  run at 60k–115k RPS with 50–60 µs of server CPU. Lists that compute a total
  count, filter by tag or aggregate tags cost Postgres about 3–5 ms of CPU each
  and saturate it at 1k–2k RPS. In the mix, the server uses 3 cores while
  Postgres uses 10.6.
- **The usual fixes are in the queries and caching, not the server:**
  - estimated or cached counts;
  - a `(tag, created_at)` index or a denormalized tag table;
  - caching popular tags.
  Instances keep nothing between requests, so caching needs a host capability
  (a key-value store), which this server doesn't have yet.
- **Login is deliberately expensive:** argon2id takes about 25 ms of CPU and
  20 MiB of guest memory per login, which is why peak RSS reaches 347 MiB in
  that scenario. In the mix, 2% of requests (logins) use about a quarter of the
  server's CPU.
- **A mistake worth knowing about:** one generic query with
  `($n IS NULL OR …)` for optional filters got poor plans; building the WHERE
  clause from only the filters given fixed it. And a benchmark that keeps
  inserting rows changes the data the following scenarios run on, so the
  create scenario runs last.

### With the cache (`platform:sdk/cache`)

Conduit caches its total counts (10 s) and popular tags (30 s); nothing else
changed. Same mix, clean data (`results/linux-docker-conduit-mix-cache.json`):

| | Without cache | With cache |
|---|---|---|
| Total RPS | 6,482 | **15,604** (2.4×) |
| Worst p99 | 207 ms | **81 ms** |
| List articles | 674 RPS | 3,477 RPS |
| List by tag | 237 RPS | 799 RPS |
| Article / comments | 2,474 / 1,593 RPS | 5,082 / 2,404 RPS |
| DB calls per request | 1.49 | 1.2 |

## SQLite vs Postgres (the `catalog` app, unchanged, on both)

8 workers. SQLite is a file in the server process; Postgres 17 runs in
another container and is reached over TLS (`verify-full`). Raw numbers:
`results/linux-docker-catalog-{sqlite,postgres}.json`.

| Scenario | SQLite RPS | Postgres RPS | SQLite p99 | Postgres p99 | DB time per call, SQLite vs Postgres |
|---|---|---|---|---|---|
| Product by id, c=1 | **15,140** | 8,419 | 0.09 ms | 0.16 ms | 4.8 µs vs 52 µs |
| Product by id, c=64 | **194,003** | 139,805 | 0.74 ms | 1.07 ms | |
| Product by id, c=256 | **202,922** | 140,034 | 2.20 ms | 3.10 ms | |
| List of 20 (OFFSET up to 999) | 52,531 | **74,459** | 3.08 ms | 2.02 ms | |
| Order: transaction, 4 statements | 20,204 | **40,185** | 6.90 ms | 3.52 ms | |

- **Reads favour SQLite:** a query is a function call (about 5 µs) instead of
  a network round trip. Latency halves, and throughput is up to 45% higher
  with no separate database CPU. Postgres used about 3 more cores of its own
  for the same load.
- **Heavier queries run on the server's own cores with SQLite,** while
  Postgres brings extra cores. In this VM that made Postgres faster on the
  list query.
- **Concurrent writes favour Postgres:** SQLite has one writer per database.
  Here that capped it at about 17k–20k write transactions per second per
  database, which is enough for most apps; write-heavy ones belong on Postgres.

### Tuning SQLite writes

Two changes, measured on the shortener suite:

| | Redirect + queue send | Create link (INSERT) |
|---|---|---|
| Checkpoints inside commits (SQLite default) | 23,543 RPS, p99 22.7 ms | 24,767 RPS, p99 23.5 ms |
| Background checkpoints | 67,872 RPS, p99 1.7 ms | 20,552 RPS, p99 27.6 ms |
| + in-process write queue | 57,378 RPS, p99 1.7 ms | 16,985 RPS, p99 **7.3 ms** |

- **Background checkpoints:** by default SQLite copies its log into the main
  file inside whichever commit fills the log, which stalls that request for
  about 20 ms. A background thread now runs non-blocking checkpoints instead.
  That tripled queue throughput, since the job store has a single writer.
- **Write queue:** with many connections writing to one file, racing for the
  lock and retrying on a timer gave long tails. An async write lock per file
  hands the lock over in order, which cut p99 from 27.6 to 7.3 ms for about
  17% less peak throughput.
- **Noise:** run-to-run variation in this VM is about ±15% (the redirect column
  doesn't take the write lock).

## Functions showcase (shortener + stats, SQLite)

`results/linux-docker-shortener.json`, 8 workers, 64 connections:

| Scenario | RPS | p50 ms | p99 ms | Server CPU µs/req |
|---|---|---|---|---|
| Redirect (cache hit + durable queue send) | 57,378 | 0.96 | 1.67 | 76 |
| Link info (typed call to `stats` + its SQLite query) | 94,375 | 0.60 | 1.80 | 81 |
| Top links (typed call returning 10 records + 10 cache reads) | 38,869 | 1.56 | 3.88 | 167 |
| Create link (SQLite insert) | 16,985 | 3.54 | 7.30 | 196 |

- **A typed call is cheap:** a request that crosses two functions (a new
  `stats` instance, arguments and results copied) costs 81 µs of server CPU,
  close to a plain request with one query (38–42 µs) plus a second instance.
- **Queue sends are fast:** each redirect commits a message to the durable
  queue before answering, at 57k RPS. Group commit amortises the writes.

## TLS to Postgres

Same scenario (catalog, product by id, c=64), run back to back:

| sslmode | RPS | p99 ms | Server CPU µs/req |
|---|---|---|---|
| disable (plaintext) | 114,997 | 1.28 | 53.9 |
| verify-full (TLS 1.3, certificate and host name checked) | 113,748 | 1.21 | 52.3 |

The difference is about 1%, within noise: pooled connections do the TLS
handshake once. (Both runs are below the 151k measured earlier because the
database has changed since: a second database and dead rows from about half a
million stock updates. Only the side-by-side comparison is meaningful.)

## What the numbers say

- **Cost per request:** about 33 µs of server CPU for a full HTTP request, of which
  about 14 µs is Wasmtime (instantiate plus handler). The remaining ~19 µs is hyper,
  socket syscalls and the per-request WASI context.
- **Memory:** a Rust `hello` instance touches 1.06 MiB of linear memory (17 wasm
  pages). The process stays small (50 MiB at 240k RPS) because pool slots are
  reused and only touched pages are resident.
- **`alloc 8 MiB`** is bound by the guest's own work: zeroing and touching 8 MiB costs
  about 0.8 ms of CPU per request.
- **"Guest µs/req" in the raw results** is wall time from instantiation until the
  handler returns, including time waiting for the worker's core. That is why it
  grows with concurrency while CPU per request stays flat.

## Caveats

- This is a VM on a laptop, and the load generator shares the CPUs. A dedicated Linux
  machine with the client on a separate host will show different absolute numbers.
- macOS hosts are 15–20× slower at instantiation, because Wasmtime's fast slot
  reuse (memfd CoW, `keep_resident`, `PAGEMAP_SCAN`) is Linux-only.
