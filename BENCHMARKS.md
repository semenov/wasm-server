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

## Guest language: C, Rust and Go (the same notes app)

`guests-c/notes` (C: wasi-sdk 34 + `wit-bindgen c`), `guests/notes-rs` (Rust,
128 KiB stack) and `guests-go/notes` (TinyGo 0.42 + `wit-bindgen-go`): the same
routes, SQLite through `platform:sdk/sql`, a counter in the cache. Linux, in
the Docker VM, 8 workers, 64 connections, all in one run.

| | C | Rust | Go | Go, hand-written JSON | Go, default GC |
|---|---|---|---|---|---|
| Lines of app code | 203 | 85 | 136 | 160 | 136 |
| `.wasm` | 55 KB | 147 KB | 578 KB | 67 KB (58 KB at `-opt=z`) | 1.1 MB |
| Memory per instance | 128 KiB | 192 KiB | 192 KiB | 128 KiB | 384 KiB |
| Instantiations/s, 8 threads | 1.56M | 1.76M | 1.36M | 1.40M | 0.24M |
| GET: 20 notes, cache, JSON | 86.3k–86.8k RPS | 87.5k RPS | 83.9k RPS | 84.5k–84.9k RPS | 43.6k RPS |
| POST: SQLite insert | 30.0k–30.1k RPS | 29.5k–30.1k RPS | 29.1k RPS | 28.9k–29.6k RPS | 22.1k RPS |

The Go builds use `-gc=leaking -scheduler=none` except the last column.

- **Speed hardly depends on the guest's language;** the server and SQLite
  dominate. Go is within about 3% of C and Rust.
- **Memory is a toolchain default, not the language.** Rust reserved a 1 MiB
  stack (1,088 KiB per instance); the guests workspace now uses 128 KiB.
- **TinyGo needs two settings.** With its default GC, instantiation is 6.5×
  slower and throughput halves, most likely from the runtime's start-up work.
  An instance lives for one request, so a GC that never collects
  (`-gc=leaking`) is the right fit.
- **`encoding/json` is about 500 KB of a Go app** (reflection). Hand-written
  JSON brings Go to C's size.
- **C costs the most to write:** manual buffers, escaping and memory
  management, and a verbose binding API.

### Plumb (2026-10-13)

The same notes app in [Plumb](https://github.com/semenov/plumb)
(`benchmarks/wasm-server` there: 34 lines of code), built with `plumb build
--target wasm-server`, against the C and Rust guests in one run: the server
pinned to CPUs 0-7 and oha to 8-13 (`taskset`), three rounds taking turns,
medians. Plumb's GET makes one host call fewer (it has no cache API yet).

| | C | Rust | Plumb |
|---|---|---|---|
| Lines of app code | 203 | 85 | 34 |
| `.wasm` | 55 KB | 147 KB | 161 KB |
| GET: 20 notes, JSON | 88.5k RPS | 88.9k RPS | 83.0k RPS (94%) |
| POST: SQLite insert | 31.2k RPS | 30.7k RPS | 29.2k RPS (93%) |
| Instantiations/s, 8 threads | 527k | 569k | 516k |

- **Plumb is about 6% behind on GET and 7% on POST:** its runtime starts
  the program (`main`, the router) on every request and decodes rows and
  encodes JSON generically.
- **Pinning matters on the Docker VM:** without it, runs on the same machine
  differed by up to 15%; with it by 1-4%.

## Native Go vs wasm (the same notes app)

`bench/native-go`: the notes app as an ordinary Go server (`net/http`,
`database/sql`), next to the wasm C and Rust apps. Linux, in the Docker VM,
8 CPUs shared with the load generator, 64 connections, 6 s per test, two
rounds. CPU is the server process's user + system time.

| | wasm C | wasm Rust | Go, pure-Go SQLite | Go, cgo SQLite |
|---|---|---|---|---|
| GET, RPS | **88k–89k** | **88k–89k** | 57k–60k | 54k–55k |
| GET, p99 | **1.5–1.7 ms** | **1.5 ms** | 4.2 ms | 8 ms |
| GET, CPU per 1,000 requests | 75 ms | 75 ms | **58 ms** | 92 ms |
| POST, RPS | 30k | 30k–31k | 26k–29k | **36k–38k** |
| POST, p99 | **3.4–4.0 ms** | 4.1 ms | 13–14 ms | 9–11 ms |
| POST, CPU per 1,000 requests | 76–77 ms | 75–83 ms | 40–42 ms | **35–37 ms** |
| Memory, one app (RSS) | 15 MB | 15 MB | 10–13 MB | 12–14 MB |

Cost of each additional app (`density.sh`):

| | 1 app | 200 apps | Per extra app |
|---|---|---|---|
| One `wasm-server` (Rust notes app, each served once) | 15 MB | 147 MB | **~660 KB** |
| One Go process per app | 10 MB | ~2 GB | ~10 MB |

- **Speed is comparable.** wasm reads faster with 3–5× better tail latency;
  the cgo Go server writes faster.
- **Go spends less CPU per request,** half on writes. That is our write path
  (async writer lock, a hop to a blocking thread, an instance per request),
  not wasm, and can be optimized.
- **Density is where wasm wins:** about 15× less memory per idle app, no CPU
  while idle, and 6 µs to start an instance instead of a process.
- Caveats: the Go server is untuned while ours is; CPU-heavy code wasn't
  measured (code inside wasm usually runs 1.2–2× slower than native).

## Typical backend: wasm vs native Go and Rust (Conduit)

The Conduit API three ways:
- **wasm:** the `conduit` app on `wasm-server`.
- **Go:** `net/http` + pgx.
- **Rust:** axum + tokio-postgres/deadpool.

All three share the routes, SQL, JWT and argon2id settings, and cache counts
and popular tags in process (`bench/native-conduit`). Measured on 2026-10-10.

**Setup:**
- **Same machine and database:** the same Docker VM and Postgres 17 over TLS
  (`verify-full`), with 32 connections.
- **Same CPUs:** every server is pinned to the same 8 CPUs.
- **Fresh data:** each run starts from a fresh copy of the seed data.
- **Rounds:** three runs per server; ranges cover all of them.
- **CPU:** the server's CPU is the server process; Postgres CPU is the
  database container's cgroup.

Run: `bench/native-conduit/compare.sh 2`. Raw numbers:
`results/linux-docker-compare-*.json`.

**Throughput and latency** (64 connections; login 16, the mix 128):

| Scenario | wasm RPS | Go RPS | Rust RPS | wasm p99 | Go p99 | Rust p99 |
|---|---|---|---|---|---|---|
| `hello`, no database | 151k–192k | **334k–338k** | 282k–293k | 1.0–1.2 ms | 2.2 ms | **0.5 ms** |
| Get article | 69k–88k | 66k–76k | 75k–77k | 1.7–2.2 | 3.0–3.4 | 1.6–1.8 |
| Profile (auth) | 86k–105k | 91k–97k | 101k–106k | 1.5–1.8 | 2.7–2.8 | 1.5–1.6 |
| Article comments | 48k–62k | 47k–54k | 50k–53k | 2.7–3.4 | 3.2–3.5 | 2.4–2.5 |
| List articles (+ cached count) | 20k–26k | 25k–29k | 23k–24k | 6.3–8.4 | 5.3–6.2 | 4.5–4.8 |
| Feed (auth) | 18k–21k | 21k–24k | 20k–21k | 7.0–9.4 | 6.1–7.0 | 5.1–5.4 |
| List by tag | 3.0k–3.2k | 2.7k–3.0k | 3.0k | 43–50 | 37–41 | 36 |
| Popular tags (cache hit) | 185k–204k | 303k–321k | 314k–325k | 0.8 | 2.1–2.2 | 0.5–0.6 |
| Favorite (transaction) | 29k–33k | 28k–30k | 27k–28k | 3.8–5.2 | 4.4–4.7 | 4.0–4.3 |
| Create article (transaction) | 23k–27k | 21k–23k | 20k | 5.4–6.0 | 5.4–5.7 | 5.9–7.0 |
| Login (argon2id) | 241–296 | 235–250 | **360–387** | 128–168 | 124–135 | **69–75** |
| Realistic mix | 10.4k–13.1k | 10.9k–11.5k | 10.6k–11.5k | 140–210 | 101–114 | 106–113 |

**CPU and memory:**

| Scenario | wasm CPU µs/req | Go | Rust | wasm RSS peak | Go | Rust | Postgres CPU µs/req |
|---|---|---|---|---|---|---|---|
| Idle | – | – | – | 20 MiB | 11–14 MiB | **4 MiB** | – |
| `hello` | 39–50 | 13–14 | **10** | 39–62 MiB | 20–22 | **8** | – |
| Get article | 56–73 | 52–60 | 48–50 | 59–87 | 31–33 | **18** | 54–74 |
| List articles | 158–220 | **98–119** | 168–173 | 56–82 | 34–36 | **13** | 232–331 |
| Feed | 184–226 | **111–126** | 177–186 | 61–90 | 34–35 | **18** | 330–417 |
| Favorite | 137–159 | 132–142 | **117–120** | 61–91 | 31–34 | **18** | 177–203 |
| Login | 26.9k–29.6k | 29.8k–32.4k | **20.4k–21.6k** | **325–356** | 629–670 | 2,889–3,195 | 162–297 |
| Realistic mix | 266–379 | 265–279 | 242–263 | 285–289 | 160–194 | 2,710–2,952 | 709–923 |

Every request succeeded in every run.

What this shows:

- **Throughput on database endpoints is the same** within run-to-run noise
  for all three, because Postgres is the limit.
  - It used 4–13 of the VM's 14 vCPUs.
  - On these endpoints Postgres spends as much CPU per request as the server,
    or more (list by tag: about 4.3 ms of Postgres CPU per request).
  - The wasm numbers vary most between runs (±15%).
- **wasm costs about 30–40 µs more server CPU per request.**
  - On `hello` that's 39–50 µs vs 10–14 µs: instantiation, the WASI context,
    copying the request and response through `wasi:http`. That gives native
    servers 1.8–2.2× the throughput when there is no database.
  - On database endpoints the server spends 1.0–1.5× native's CPU.
    Rust's list and feed are as expensive as wasm's (168–186 µs; `json!`
    building `serde_json::Value` trees). Go is the cheapest there.
- **A CPU-heavy request hurts the others on its worker in wasm.**
  - In the mix, wasm's p99 on cheap endpoints (article, profile, list) is
    50–66 ms vs 16–19 ms native.
  - **Measured cause:** with the 2% of logins removed from the mix, wasm's
    and Go's p99 were close (wasm 14–35 ms, Go 11–46 ms).
  - **Why:** an argon2 login takes about 25 ms of CPU. `wasm-server` is
    thread-per-core: a request stays on the worker that accepted it, and the
    guest yields only every 10 ms epoch tick. So requests queued on that
    worker wait while idle workers can't take them.
  - Go's scheduler and tokio steal work across cores, and the native Rust
    server runs argon2 on the blocking pool.
- **Memory: native is smaller in steady state.**
  - Rust 13–18 MiB, Go 30–36 MiB, wasm 56–91 MiB: pool slots, per-worker
    database pools and the engine.
- **Under memory-heavy requests, wasm is the most predictable.**
  - Instance memory goes back to the pool after each request, so 16
    concurrent logins peak at about 340 MiB (16 × 19 MiB plus the base),
    falling to about 110 MiB.
  - Go peaks at about 650 MiB and holds about 500 MiB afterwards (the GC
    frees it lazily; `GOMEMLIMIT` would bound it).
- **The native Rust server ran out of memory under login load.** It held
  2.9–3.2 GB and didn't release it. In the 4 GiB VM, standalone login tests
  were OOM-killed (exit 137), and one of four suite runs died before writing
  results.
  - **Not a leak:** it's glibc malloc. After the first 19 MiB argon2 buffer
    is freed, glibc's *dynamic mmap threshold* rises above 19 MiB, so later
    buffers come from per-thread arenas, which fragment and never shrink.
  - `MALLOC_ARENA_MAX=2` didn't help: still OOM.
  - `MALLOC_MMAP_THRESHOLD_=131072` (a fixed threshold) fixed it: peak
    321 MiB, 17 MiB after, with the same throughput
    (`results/linux-docker-compare-rust-malloc-fix.json`).
  - A typical axum service with any large per-request buffer can hit this.
    wasm guests don't: their allocator works inside linear memory, which the
    host discards.
- **Benchmark pitfall:** the first attempt at the malloc fix set the variable
  for the whole container, so oha (also Rust on glibc) got it too, and Rust
  "got" 25–40% faster. Applied to the server alone, the speedup disappeared.
  The load generator's own allocator affects these numbers.

## Noisy neighbour: one app's heavy requests vs another app (`bench/noisy.py`)

Two apps on one server:
- **Victim:** `hello`, at a steady 1,000 requests/s.
- **Noisy:** `compute?fib=33`, about 20 ms of CPU per request, like an
  argon2 login. More and more connections drive it.

Victim latency counts from each request's scheduled send time (oha's
latency correction), so waiting behind the noise counts. The server is
pinned to as many CPUs as it has workers.

The control (`--separate`) runs each app in its own `wasm-server` process
on the same CPUs, so the kernel shares them out as it would with a process
per app. Measured on 2026-10-10; raw numbers:
`results/linux-docker-noisy*.json`.

| Workers (CPUs) | Noisy connections | Noisy RPS, one server / separate | Victim p50, one server | Victim p99, one server | Victim p99, separate processes |
|---|---|---|---|---|---|
| 8 | 0 | – | 0.5 ms | 2.8 ms | 1.9 ms |
| 8 | 1 | 47 / 48 | 0.1 ms | **24 ms** | 0.3 ms |
| 8 | 8 | 278 / 233 | 11 ms | **88 ms** | 0.2 ms |
| 8 | 32 | 374 / 374 | 99 ms | **249 ms** | 1.7 ms |
| 1 | 0 | – | 0.6 ms | 2.8 ms | 3.5 ms |
| 1 | 1 | 46 / 45 | 23 ms | **46 ms** | 1.1 ms |
| 1 | 4 | 46 / 47 | 130 ms | **162 ms** | 1.1 ms |

The victim got all of its 1,000 requests/s in every row, and all
responses were `200`.

- **One server: the victim's latency grows with the neighbour's load.** It
  climbs from 2.8 ms to 249 ms on 8 cores and to 162 ms on 1 core.
  - **With 7 cores idle:** a single noisy connection still raises the
    victim's p99 to 24 ms. The victim's connections that landed on the busy
    worker wait, and no other worker takes them.
  - **Why it adds up:** a request passes several await points (read,
    instantiate, handler, write). Each wake-up queues behind every running
    heavy guest, and each of those runs a full 10 ms epoch tick before
    yielding. So a 50 µs request waits several rounds of 10 ms × the number
    of heavy guests.
- **Separate processes: the victim is unaffected,** at 0.2–1.7 ms p99 even on
  one core, and the noisy app keeps the same throughput.
  - The kernel's scheduler preempts a CPU-bound thread as soon as a thread
    that was sleeping wakes up, with sub-millisecond granularity.
  - Our scheduler is a cooperative round-robin with 10 ms slices and no
    notion of which app a task belongs to.
- **A first version of the test used 32 victim connections.** They could
  not sustain 1,000 requests/s when each took about 120 ms, and the backlog
  showed up as seconds of latency. That measured the client, not the
  server; 256 connections fixed it.

## Sandboxes: wasm vs RISC-V (libriscv)

Is a RISC-V emulator a faster sandbox than wasm? `bench/riscv`: wasmtime
49 vs libriscv (commit 2925e57) on the same code; Linux arm64 in the Docker
VM, one pinned CPU. Measured on 2026-10-10. Raw numbers:
`results/sandbox-*.json*`.

The engines:
- **wasmtime:** Cranelift, and Pulley, its interpreter.
- **libriscv:** its interpreter, and binary translation (RISC-V → C → gcc
  → native code). libriscv's own docs call translation "NOT safe for
  sandboxing".

PolkaVM is not measured: its recompiler exists only for x86-64, and under
emulation its sandbox doesn't start (see `bench/riscv/README.md`).

**Standard programs** (each engine's CLI, best of 3 runs; × = time
relative to native):

| Program | native (gcc) | wasmtime | wasmtime Pulley (interpreter) | libriscv interpreter | libriscv + translation (unsafe) |
|---|---|---|---|---|---|
| CoreMark | 2.53 s | 1.2× | 23× | 16× | 3.0× |
| nbody (floating point) | 0.37 s | 1.1× | 53× | 23× | 8.1× |
| fannkuch (integers) | 1.93 s | 1.0× | 15× | 9.0× | 3.8× |
| spectral-norm | 0.17 s | 0.7× | 32× | 17× | 2.7× |
| mandelbrot | 0.66 s | 1.1× | 23× | 6.4× | 4.3× |
| binary-trees (malloc) | 0.70 s | 1.6× | 29× | 26× | 6.0× |
| sha256 | 0.49 s | 0.9× | 30× | 19× | 3.1× |
| qsort | 0.42 s | 4.5× | 111× | 7.0× | 3.7× |
| hash map (cache misses) | 0.19 s | 1.2× | 15× | 11× | 2.0× |
| **Geometric mean** | | **1.26×** | 30× | 13× | 3.8× |
| Start-up of an empty program (CLI) | 0.5 ms | 22.5 ms | 1.6 ms | 3.9 ms | 3.3 ms |

- **CoreMark scores:**

  | | Iterations/s |
  |---|---|
  | native | 38,870 |
  | wasmtime | 33,081 |
  | libriscv + translation | 13,053 |
  | libriscv interpreter | 2,460 |
  | Pulley | 1,717 |

- **Translation is slow to produce:** about 75 s per program, because gcc
  compiles the whole static glibc. The result is cached.
- **Outputs:** all matched native.
  - CoreMark differs only in its timing lines and the "run at least 10 s"
    warning.
  - libriscv's `qsort` output adds an "Unhandled system call: 179"
    (`sysinfo`) line.
- **wasm runs below 1.0×:** clang 20 (wasm) beats gcc 12 (native) on
  those programs.
- **qsort on wasm:** 4.5× comes from wasi-libc's `qsort`, not from
  wasmtime.

**Inside a host process** (`rv_bench.cpp`, `wasmtime-bench`; the same C
functions; wasmtime configured like the server, with an epoch deadline;
libriscv with an instruction limit):

| | wasmtime | libriscv interpreter | libriscv + translation |
|---|---|---|---|
| fib(27) (native 0.48 ms) | 1.18 ms | 5.3 ms | 2.2 ms |
| sieve 1M (native 2.3 ms) | 2.4 ms | 17–19 ms | 5.7–6.1 ms |
| Call into a live sandbox | 11.0 ns | **4.4 ns** | 5.7–5.9 ns |
| Fresh sandbox + call | **1.3 µs** | 9.1 µs (fork) | 9.8 µs (fork) |
| Fresh sandbox, writes 64 KiB | **2.9 µs** | 17.5 µs | 14.6 µs |
| Fresh sandbox, writes 1 MiB | **31 µs** | 120 µs | 76 µs |
| Memory per live sandbox (64 KiB written) | **72 KiB** | 107 KiB | 105 KiB |
| Load and start a program (ELF, glibc start-up) | 1.4–9.3 ms (compile) | 3.6 ms | 47–87 s (first translation) |

- **libriscv's "fast fork"** (`minimal_fork`, about 80 ns, used by its C
  API and its per-request isolation example) doesn't lend the template's
  pages to the fork. The fork runs without the program's initialized
  memory. In our tests the host process crashed (segfault) as soon as such
  a fork wrote memory. The numbers above use the full copy-on-write fork,
  and an isolation check confirms the template stays unchanged.
- **Without forks** (the flat-arena build) a fresh libriscv sandbox means
  loading the ELF and running the C runtime's start-up: about 0.7 ms.
- **The first run of this test was wrong.** Nothing read the buffer the
  guest wrote, so both compilers removed the writes, and "writes 1 MiB"
  measured an empty loop. A function that reads it back (`peek`) fixed
  that.

## Rust guest size: what's inside

`notes-rs` (147 KB, `opt-level=3`, stripped): code 93 KB, data 33 KB, the
component wrapper (WIT types and imports) 16 KB. The code by purpose:

| | Size | Share |
|---|---|---|
| Debug formatting for `unwrap`/`expect` messages | 27 KB | 29% |
| Allocator (dlmalloc from wasi-libc) | 15 KB | 16% |
| serde_json + BTreeMap (`json!` builds a map) | 13 KB | 14% |
| `fmt` machinery | 10 KB | 10% |
| **The app itself** | 8.5 KB | 9% |
| Panics | 6 KB | 7% |
| WIT bindings | 3 KB | 3% |

The largest single function (6.6 KB) is `Debug` for `wasi:http`'s
`error-code` (about 40 variants). `hello` is 93 KB, 37% of it Debug
formatting. Measured options:

| | hello | notes-rs | conduit |
|---|---|---|---|
| Current (`opt-level=3`) | 93 KB | 147 KB | 354 KB |
| `opt-level="s"` | 87 KB | 121 KB | 287 KB |
| + `-Zbuild-std` (nightly) | 84 KB | 114 KB | 275 KB |
| + `-Cpanic=immediate-abort` (nightly) | 33 KB | 61 KB | 224 KB |

None is adopted: `immediate-abort` drops panic messages, which an agent
debugging its app needs, and the size isn't a bottleneck.

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
