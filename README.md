# wasm-server

A server for WebAssembly functions. Every invocation gets a fresh, isolated
instance (about 6 µs to create on Linux). Functions are triggered by HTTP, by
WebSocket messages, by queue messages and cron, or by typed calls from other
functions. The host provides what instances can't keep themselves: databases
(SQLite or Postgres), a cache, durable queues, WebSocket connections and
static sites. Everything an app uses is metered and billed in credits.

## Layout

- `server/`: the host, built on Wasmtime 49 and hyper.
  - `engine.rs`: engine tuning (pooling allocator, CoW memory images, guard pages, epochs)
  - `app.rs`: functions and the per-instance store state, CPU and memory limits
  - `platform.rs`: running a function: HTTP request, event, or call from another function
  - `functions.rs`: typed calls between functions (linking imports to exports)
  - `server.rs`: thread-per-core HTTP serving
  - `deployment.rs`, `flags.rs`, `loader.rs`: what runs (from `--config` or flags), built and swapped on reload
  - `admin.rs`, `client.rs`: `/_platform/` endpoints (token, reload, upload) and the `deploy`/`reload` commands
  - `routes.rs`, `static_files.rs`: routing by domain or path; static sites
  - `db/`: `platform:sdk/sql`, with `sqlite.rs` and `postgres.rs` backends
  - `cache.rs`: `platform:sdk/cache`
  - `jobs.rs`: `platform:sdk/queue`, cron, and event delivery
  - `ws.rs`: `platform:sdk/websocket`, connections held by the host
  - `billing.rs`, `dashboard.html`: metering, credits, `/_platform/` dashboard
  - `stats.rs`: per-worker, per-app counters; `bench.rs`: in-process microbenchmark
- `wit/`: the `platform:sdk` package that apps import (`sql`, `cache`, `queue`, `events`)
- `guests/`: sample apps in Rust for `wasm32-wasip2`
  - `hello`, `compute`: minimal and CPU/memory-heavy handlers
  - `catalog`: product catalog (queries, a transaction, a cursor export); runs on SQLite or Postgres
  - `conduit`: the RealWorld "Conduit" API, a typical blog backend (JWT, argon2id, cache)
  - `shortener` + `stats`: the functions showcase (see below)
  - `chat`: chat rooms over WebSockets, with no HTTP handler at all
  - `notes-rs`: a small notes app, the Rust twin of the C and Go ones
- `guests-c/notes`, `guests-go/notes`: the same notes app in C and in Go (TinyGo)
- `sites/`: static frontends for `conduit` and `shortener`
- `examples/shortener/`, `examples/chat/`: schemas and run scripts for the showcases
- `bench/`: load test, seed data, `native-go/` (the notes app as a native Go
  server, for comparison), `native-conduit/` (the Conduit API as native Go
  and Rust servers, and `compare.sh`), `noisy.py` (one app's heavy requests
  vs another app's latency), `riscv/` (wasm vs RISC-V sandboxes: libriscv,
  PolkaVM); `docker-compose.yml`, `Dockerfile`: Postgres (with
  TLS) and the Linux benchmark image
- `docs/hosting.md`: multi-server hosting design; `docs/findings.md`: what the
  experiments taught us (wasm vs native, guest languages, outbound access,
  WASI status); `BENCHMARKS.md`: the numbers

## Quick start

```sh
rustup target add wasm32-wasip2
./build.sh                         # build host + guests, AOT-compile guests into apps/
./target/release/wasm-server serve --config examples/platform.toml --admin-token "$(openssl rand -hex 16)"
# http://short.localhost:8080/  http://chat.localhost:8080/  http://localhost:8080/hello
```

## Deploying without restarts

What runs is described in a TOML file (`examples/platform.toml`), or with
flags, which describe the same things:

```toml
[databases.links]
url = "sqlite:../data/links.db"
init = "shortener/links.sql"

[apps.shortener]
component = "../apps/shortener.cwasm"
static = "../sites/shortener"
domains = ["short.localhost"]
databases = ["links"]
env = { GREETING = "hi" }
credits = 1000

[apps.stats]
component = "../apps/stats.cwasm"
databases = ["stats"]
queues = ["clicks"]
cron = { rollup = "*/10 * * * * *" }
```

**Changing a running server:**

```sh
wasm-server deploy http://host:8080 --app shortener new.wasm   # upload new code
wasm-server reload http://host:8080                            # re-read the config (or: kill -HUP)
```

- **The switch is atomic.** The server builds the new version completely
  (loading only components whose files changed), then swaps it in.
- **Requests in flight finish on the version they started on;** new ones go
  to the new version.
- **Open WebSocket connections stay open,** and their next message is
  handled by the new version.
- **Queue deliveries, cron and typed calls** always use the current version.
- **What persists across deploys:** app caches, account ids and billing
  history.
- **Measured:** two deploys during a 10 s load test, 144,813 requests, every
  one `200`. A reload takes about 2 ms; uploading a `.wasm` adds its compile
  time.
- **Failures change nothing:**
  - A bad upload (not a component) is rejected before anything is written.
  - A config error is reported with its line and column (HTTP 422), and the
    previous version keeps running.
  - An upload that doesn't load puts the previous file back.
- **Uploads:** `deploy` accepts a `.wasm` (compiled on the server when the app
  runs from a `.cwasm`) or a `.cwasm`. The previous file is kept next to the
  new one as `.previous`.
- **Restart only for databases:** every worker holds connections to them, so
  database changes need a restart. Everything else (apps, sites, domains,
  grants, env, queues, cron, credits, links) applies on reload.

**Versions and rollback.**
- **History:** before a deploy or rollback replaces a component, the old file
  is saved to `<data-dir>/versions/APP/`. The last 10 distinct versions are
  kept, and identical files are stored once.
- **Rolling back:** `rollback` without `--to` switches to the newest version
  that differs from the running one. What was running is saved first, so a
  rollback can be undone the same way.

```sh
wasm-server versions http://host:8080 --app shortener          # ← marks the running one
wasm-server rollback http://host:8080 --app shortener          # previous version
wasm-server rollback http://host:8080 --app shortener --to 1791574461446-de04ffd4795e
```

**Tenants.** A tenant manages its own apps with its own token. The
deployment stores only the token's SHA-256:

```toml
[tenants.acme]
token_sha256 = "…"          # wasm-server hash-token TOKEN

[apps.shop]
tenant = "acme"
component = "apps/shop.cwasm"
```

| | admin | tenant | nobody (no tokens configured) |
|---|---|---|---|
| dashboard, billing, stats, jobs | everything, plus per-tenant totals | its own apps only | everything, read-only |
| deploy, versions, rollback | any app | its own apps | — |
| reload, backups | yes | — | — |

**The admin token** (`--admin-token`, or `WASM_SERVER_ADMIN_TOKEN`, at least 16
characters):
- **Required for writes:** reload and deploy are disabled without it.
- **Protects reads too** once set: stats, billing and the dashboard.
- **How to send it:** `Authorization: Bearer TOKEN`. For a browser, open
  `/_platform/?token=TOKEN` once; it sets an HttpOnly cookie.
- **Without a token,** `/_platform/` is read-only and open, which is fine for
  local development only.

## Functions

A function (app) is a component with any combination of entry points:

- **HTTP:** exports `wasi:http/incoming-handler`.
- **Events:** exports `platform:sdk/events` and receives queue messages and cron
  ticks through `handle(event)`.
- **Its own interfaces:** exports any WIT interface that other functions call.

**Typed calls.** If function A imports an interface that function B exports,
the server links them. A's call creates a B instance, runs the function with
the arguments copied over, and returns the results. There is no HTTP or JSON
in between, and the call is type-checked against the interface:

```wit
// guests/stats-api/wit/api.wit
interface api {
    record link-stats { code: string, clicks: u64, last-click-ms: u64 }
    summary: func(code: string) -> link-stats;
}
```

```rust
// in shortener, which imports demo:stats/api
let s = stats::summary(code);   // runs in a fresh `stats` instance, with stats' own grants
```

- **Callee runs as itself:** it uses its own databases and limits, and is
  counted in its own stats.
- **Call depth:** limited to 8.
- **Deadline:** the caller's wall-clock deadline covers the whole chain. Time
  spent in the callee doesn't count against the caller's CPU limit.
- **Data only:** interfaces with resources can't be linked, because resources
  belong to one instance.
- **Choosing a provider:** when several apps export the same interface, pick
  one with `--link APP:INTERFACE=TARGET`.

## Platform interfaces (`wit/`, package `platform:sdk`)

| Interface | What it gives a function |
|---|---|
| `sql` | Databases by name: queries, transactions, cursors. SQLite or Postgres behind the same API |
| `cache` | An in-memory cache per app: get/set with expiry, `add` (only if absent), atomic `incr` |
| `queue` | `send(queue, payload, delay-ms)`: durable, delivered at least once to the queue's consumer |
| `events` | (export) `handle(event)` for queue messages and cron ticks; an error means "retry later" |
| `websocket` | `send`, `close`, channels (`subscribe`, `publish`, `subscribers`), per-connection state |
| `websocket-handler` | (export) `on-connect` (may refuse), `on-message`, `on-close` |

Apps import the `platform:sdk/imports` world (or `events-handler` to also
receive events). A component only imports the interfaces it actually uses.

### Outgoing HTTP

Apps call other servers with standard `wasi:http/outgoing-handler`
(`server/src/outbound.rs`):

- **Where:** any public host, or only the hosts in the app's `outbound`
  list (`outbound = ["api.stripe.com", "*.openai.com"]`, `--outbound
  APP=HOST`; `[]` or `APP=none`: no requests). Loopback, private, link-local,
  cloud metadata and other non-public addresses are always refused, checked
  after DNS on every address of the name, and the connection goes to a
  checked address. A refusal is logged with the app's name.
- **Connections** are kept per worker and per host for 60 s and reused: to
  example.com a request takes 8 ms on a kept connection, 25–35 ms on a new
  one (TCP and TLS handshakes).
- **Timeouts:** connect 10 s, first byte 30 s, between bytes 30 s, or less
  through `request-options`; the request's wall-clock limit covers all.
- **Metered:** `outbound_requests` (billed per request), and bytes sent
  (billed as egress) and received.

### Databases

Apps name databases (`"main"`) and need a grant. URLs and credentials never
enter the sandbox.

```sh
--db main=sqlite:data/main.db --db-init main=schema.sql --grant app=main      # SQLite (default choice)
--db main='postgres://user:pass@host/db?sslmode=verify-full' --grant app=main  # Postgres
```

**SQLite.** One file per database, embedded in the server:
- **Fast:** queries are function calls (microseconds); there are no connections.
- **WAL mode:** readers run alongside the single writer.
- **Write lock:** a writer waiting for it doesn't block its thread, but retries
  after an async sleep.
- **Transactions** use `BEGIN IMMEDIATE`.
- **`--db-init`** runs an idempotent schema file at startup.
- **Parameters:** apps write `$1`, `$2` for both backends.

**Postgres:**
- **Pools per worker** (`--db-pool-size`).
- **TLS:** libpq's `sslmode` semantics (`disable` … `verify-full`), plus `--db-ca-file`.
- **Cancellation:** a query that times out is cancelled on the server.
- **Streaming cursors.**

**Both backends:**
- **Timeouts:** `--db-timeout-ms` (5000), and per app with `--app-db-timeout-ms APP=MS`.
- **Row cap:** `query` returns at most 10,000 rows; use a cursor for more.
- **Rollback:** a transaction that isn't committed is rolled back when it's
  dropped or the request ends.

### Backups (SQLite)

```sh
--backup-dir backups --backup-every-min 60 --backup-keep 24
```

- **Snapshots:** each SQLite database is snapshotted on a schedule with
  `VACUUM INTO`. That gives a consistent, compacted copy and doesn't block
  writers in WAL mode. Files are gzipped to
  `backups/<db>/<unix-ms>.db.gz`, and the newest N are kept.
- **On demand:** `POST /_platform/databases/NAME/backup` takes a snapshot now;
  `GET /_platform/backups` lists them (admin only).
- **Restore:** with the server stopped, run
  `wasm-server restore backups/main/1791574540572.db.gz --to data/catalog.db`.
  - The snapshot is checked with `PRAGMA integrity_check` before it replaces
    anything.
  - The old file is kept as `.before-restore`.
  - A file that isn't a SQLite database is refused.
- **Not yet:** continuous log shipping (point-in-time recovery) and read
  replicas on other servers. See `docs/hosting.md`.

### Cache

```rust
if let Some(bytes) = cache::get("popular-tags") { ... }
cache::set("popular-tags", &bytes, 30_000)?;          // TTL in ms
let hits = cache::incr(&format!("rate:{ip}:{minute}"), 1, 60_000);  // rate limiting
```

- **Shared:** all instances of an app on a server share its cache.
- **Bounded:** each app has `--cache-mb` (64), with least-recently-used
  eviction.
- **Not storage:** entries can disappear at any time, so never keep the only
  copy of data there.

### Queues and cron

```sh
--queue clicks=stats                       # messages to "clicks" go to app stats
--cron 'stats:rollup=*/10 * * * * *'       # 5 fields, or 6 with seconds
```

- **Durable:** `queue::send` returns once the message is stored, in
  `<--data-dir>/platform.db` (SQLite).
- **Delivery:** at least once, to the consumer's `events.handle`. A failure is
  retried after 2, 4, 8 and 16 s; after the 5th attempt the job is kept as
  dead. Use `event.id` to make handlers idempotent.
- **Cron** enqueues one job per tick, keyed by its scheduled time. A tick
  therefore runs once even across restarts, and gets the same retries as a
  queue message.
- **Monitoring:** `GET /_platform/jobs` shows pending and dead jobs per queue
  and per cron job.

### WebSockets

```sh
--websocket chat=/ws          # upgrades at /ws on chat's site go to app chat
```

- **The server holds the connections; functions run only on events.**
  - `on-connect` runs before the `101` response, so it can refuse the
    connection (the client gets a `403` with the reason).
  - `on-message` calls for one connection run one at a time, in order.
  - `on-close` can still read the connection's state.
- **Idle connections are cheap:** each one is a socket, a small buffer and a
  task, with no wasm instance.
- **Functions talk back through the host:**
  - `send` to a connection;
  - `publish` to a channel (a chat room, say) that connections `subscribe`
    to;
  - `set-state`/`get-state` up to 4 KiB per connection, since instances keep
    nothing.
- **Isolation:** connections and channels belong to the app that accepted
  them.
- **Limits:**
  - 64 KiB per message.
  - The server pings each connection every 30 s.
  - A client that stops reading (more than 256 queued messages) is
    disconnected rather than allowed to grow server memory.
- **Measured:** a chat message round trip (client, `on-message`, SQLite insert,
  channel publish, back to the client) takes about 2 ms on macOS.

```rust
impl Guest for Chat {
    fn on_connect(info: ConnectInfo) -> Result<(), String> {
        ws::set_state(&info.conn, b"alice")?;
        ws::subscribe(&info.conn, "room:general")?;
        Ok(())
    }
    fn on_message(conn: String, msg: Message) {
        let _ = ws::publish("room:general", &msg);
    }
    fn on_close(_conn: String, _code: u16) {}
}
```

### Billing in credits

Everything an app uses is metered, priced in credits and broken down by
what it was spent on:

| Category | Meters |
|---|---|
| Compute | invocations (HTTP, events, calls, WebSocket events), CPU-seconds, GB-seconds of memory |
| Network | egress GB (responses, static files, WebSocket messages sent), static requests |
| Database | calls, time in the database, GB-hours of SQLite storage |
| Cache | operations, GB-hours of memory held |
| Queues | messages sent |
| WebSockets | messages in and out, connection-minutes |

- **Prices:** `wasm-server pricing` lists the defaults (1 credit ≈ $0.001,
  near public cloud prices for the same resource). `--pricing FILE` overrides
  any of them with a JSON object such as `{"cpu": 0.1}`.
- **Collection:** counters are kept per worker without contention and turned
  into credits once a second.
- **Storage:** usage is written per minute to `<data-dir>/billing.db` every
  10 s and on SIGTERM/SIGINT, so totals survive restarts.
- **Where to look:**
  - `GET /_platform/billing` returns totals per app, by category and by meter,
    plus credits per minute for the last hour.
  - `GET /_platform/` is a dashboard of the same, phone-friendly.
- **Limits:** `--credits APP=N` grants an app (or static site) N credits.
  When they're used up:
  - HTTP and static files get `402`;
  - WebSocket upgrades are refused, and open connections are closed with code
    1008;
  - queue messages and cron ticks wait (without using up an attempt).
- **Without a grant**, an app is only metered.

### Sites and routing

```sh
--static shortener=sites/shortener --domain short.localhost=shortener
```

- **Static files are served by the host**, without running wasm. They're loaded
  into memory at startup, with gzip variants, ETags, `304`, and cache headers
  that depend on the file type. A content hash in a file name means
  `immutable`. Requests for anything else go to the site's app.
- **`--domain HOST=NAME`** gives a site every path on that domain. `*.localhost`
  needs no DNS setup in browsers.
- **Without a domain**, a request goes to the app named by the first path
  segment (`/NAME/...`).
- **Reserved paths:** `/_platform/*` belongs to the server (`stats`, `jobs`,
  `billing`, `reload`, `apps/NAME`, and the dashboard at `/_platform/`) and
  can't reach apps. See the admin token above.

## Showcase: link shortener

`examples/shortener/run.sh`: two functions, a site, two SQLite databases, a queue and cron.

```
browser ── short.localhost ──► static site (host)
        └─ /api/*, /r/CODE ──► shortener ──► links.db (SQLite), cache
                                   │  queue::send("clicks")      ┌──────────────┐
                                   ├────────────────────────────►│ stats        │
                                   │  stats::summary/top (typed) │ (no HTTP)    │──► stats.db
                                   └────────────────────────────►│ events + api │
                                        cron "rollup" every 10 s ─►└──────────────┘
```

- **Redirect:** `/r/CODE` looks the link up in the cache and queues a click,
  so it never waits on statistics.
- **Counting:** `stats` counts clicks idempotently (by event id) in its own
  database, which `shortener` can't reach except through the `demo:stats/api`
  interface.

## Showcase: chat

`examples/chat/run.sh`, then open http://chat.localhost:8080/.
- **No HTTP:** the `chat` function only exports `websocket-handler`, and the
  page is a static site served by the host.
- **Rooms are channels;** who is who lives in connection state.
- **History:** the last 50 messages come from SQLite when you join.
- **Rate limit:** at most 5 messages per second per connection, counted with
  the cache's `incr`.

## Deploying apps over the API (`plumb deploy`)

Besides the apps in the deployment file, the admin can create apps over
`/_platform/` (`server/src/managed.rs`), which is what `plumb deploy` does:

- **`POST /_platform/apps/NAME/deploy`** with JSON `{"settings": {"domains":
  [...], "databases": ["main"], "outbound": [...]}, "schema": {"main": "create
  table if not exists ..."}, "component": "<base64 .wasm>"}` creates or
  replaces the app without downtime. The schema runs first; if the new
  version doesn't load, the previous one stays.
- **Databases** are the app's own SQLite files, which its code opens by its
  own names (`db.open("main")`); they are backed up and billed like the
  others.
- **`PUT /_platform/apps/NAME/secrets`** with `{"KEY": "value", "OLD": null}`
  sets or removes environment variables, kept owner-only on disk; `GET`
  lists their names.
- Everything lives in `<data-dir>/apps/NAME/` (`app.toml`, `app.cwasm`,
  `db/`, `secrets.json`), so a restart brings the apps back. A server can
  start with no apps at all and wait for its first deploy.

## Logs and request metrics

- **Each app's log** (`server/src/logs.rs`): what its instances write to
  stdout and stderr, and a line from the host for every failed request
  (5xx, timeouts) and every one slower than a second, with where its time
  went: `GET /slow-api → 200 in 2.61 s (cpu 0.9 ms, http 1 request 2.61 s)`.
  Kept in `<data-dir>/logs/APP.log` (rotated at 10 MB) and echoed to the
  server's stderr as `APP: line`. `GET /_platform/apps/APP/logs?lines=N`
  (or `?since=OFFSET` to follow it; `plumb logs -f`).
- **Per-minute metrics** (`server/src/metrics.rs`), 30 days in
  `<data-dir>/metrics.db`: requests, failures, p50/p95/p99 from a
  histogram of request times, and the average request's time split into
  CPU, database, outgoing HTTP and the rest. `GET
  /_platform/metrics?app=APP&range=1h|6h|24h|7d|30d` (`plumb stats`), drawn
  at `/_platform/requests`.
- **Waiting isn't CPU:** time an instance waits on the database, other
  functions or outgoing HTTP doesn't count against `--cpu-limit-ms`.

## Several servers: a cluster and autoscaling

One server can grow into several without changing the apps
(`server/src/cluster.rs`, `server/src/autoscale.rs`):

```sh
# the primary: everything with state, and the workers' door (a private address)
wasm-server serve --config platform.toml --https --listen 0.0.0.0:443 \
  --cluster-listen 10.0.0.2:7070 --cluster-token "$CLUSTER_TOKEN"
# a worker: runs app code only
WASM_SERVER_CLUSTER_TOKEN=... wasm-server serve --join http://10.0.0.2:7070 \
  --listen 0.0.0.0:8080 --advertise http://10.0.0.3:8080
```

- **The primary keeps the state:** HTTPS, databases, queues and cron,
  WebSockets, static files, logs, metrics and billing. Workers take its
  apps (settings, secrets, components it compiled) and check for changes
  every 2 seconds; a deploy reaches them by itself.
- **Requests** go to the node with the fewest requests in flight per core,
  the primary included, and only to workers that run the current apps. A
  worker that can't be reached gets nothing; the request runs on the
  primary. Workers accept requests only from the primary.
- **A worker's database calls and queue messages** go to the primary over
  kept connections; its transactions are held there by id (rolled back
  after 30 s idle). Cursors aren't available on workers yet.
- **Logs and counters** come back with the heartbeat, so `/_platform/`
  (logs, metrics, charts, billing) covers the whole cluster;
  `GET /_platform/cluster` lists the nodes.
- **Same binary everywhere:** workers run the primary's compiled
  components, so they run the same wasm-server version on the same CPU
  architecture (a cloud worker downloads it from the primary).

Measured on one Mac (primary and two workers, 2 threads each, oha with 64
connections): 8.8k requests/s reading SQLite through the cluster, 3,000
inserts and 2,000 transactions from workers all in the database, no
errors.

**Autoscaling** (`--autoscale local|digitalocean` on the primary): the
cluster's CPU use (every process, as a share of all cores) is sampled every
15 s. At 70% (`--autoscale-high`) for 2 minutes a worker is added, up to
`--autoscale-max` (2); under 30% (`--autoscale-low`) for 10 minutes the
newest is drained (no new requests, the running ones finish) and removed,
down to `--autoscale-min` (0); 3 minutes between decisions.

- `local` starts worker processes on the same machine: for trying it out
  (tested: two workers added under load, both removed after it, no failed
  requests).
- `digitalocean` creates droplets (`--autoscale-size s-2vcpu-4gb`,
  `--autoscale-region fra1`, `DIGITALOCEAN_TOKEN`) that download the
  binary from the primary and join it as a service, and deletes them. At
  start it prints the price per droplet and the most a month could cost.
  The droplets must reach the cluster port, and the primary their port
  8080, on a private network. Not tried against the real API yet.

## HTTPS

`--https` serves `--listen` over TLS with certificates from Let's Encrypt for
every domain in the deployment, the usual setup on one server:

```sh
wasm-server serve --config platform.toml --https --listen 0.0.0.0:443 \
  --acme-email you@example.com --admin-token "$TOKEN"
```

- **Certificates** come through ACME's HTTP-01 challenge, which port 80
  (`--http-listen`, `0.0.0.0:80` by default) answers; everything else there
  is redirected to https. The domain's DNS must point at the server.
- **New domains** (added by a reload or deploy) get a certificate within a
  minute; certificates are renewed 30 days before they expire, and kept in
  `<data-dir>/certs` with the ACME account, so a restart reuses them.
- **Failures** (DNS not pointing here yet, port 80 closed) are logged and
  retried after 5 minutes, then less often, up to every 6 hours, well
  inside Let's Encrypt's rate limits.
- **Trying it out:** `--acme-directory staging` uses Let's Encrypt's test CA;
  `--acme-directory URL --acme-ca-file PEM` any ACME server (tested with
  [Pebble](https://github.com/letsencrypt/pebble)).

## Other `serve` options

- **`--workers`:** HTTP worker threads (default: number of CPUs).
- **`--job-workers`:** threads delivering queue messages and cron ticks (2).
- **`--timeout-ms`:** wall-clock limit per request, including nested calls (10000).
- **`--cpu-limit-ms`:** CPU limit per request (1000).
- **`--max-memory-mb`:** memory limit per instance (64).
- **`--pool-size`:** maximum number of concurrent instances (1000).
- **`--env APP:KEY=VALUE`:** an environment variable for an app, e.g. a secret.
- **`--outbound APP=HOST`:** a host the app may call (see Outgoing HTTP).
- **`--keep-resident-kib`:** Linux only; how much memory each pool slot keeps
  between requests (2048).

## Measuring

```sh
./target/release/wasm-server bench apps/hello.cwasm --mode handle --threads 4   # runtime only
python3 bench/loadtest.py --suite basic          # RPS, latency, CPU, RSS, guest memory
python3 bench/loadtest.py --suite shortener --data-dir /tmp/short
python3 bench/loadtest.py --suite db --db sqlite:/tmp/catalog.db -- --db-init main=bench/catalog-sqlite.sql
python3 bench/loadtest.py --suite conduit --conduit-db postgres://postgres:bench@127.0.0.1:5433/conduit
docker compose run --rm bench --suite ...        # the same on Linux
```

- **Results:** [BENCHMARKS.md](BENCHMARKS.md).
- **Hosting across many servers** (routing, databases, billing):
  [docs/hosting.md](docs/hosting.md).
- **macOS caveat:** Wasmtime's fast slot reuse is Linux-only. Processes under
  a background QoS clamp (`ps -o pri` shows 4) run on efficiency cores and give
  meaningless numbers.

## Writing an app

- **Build:** an app is a Rust `cdylib` for `wasm32-wasip2`. Or any language
  that compiles to a WASI component: the same notes app is in C
  (`guests-c/notes`, wasi-sdk plus `wit-bindgen c`) and in Go
  (`guests-go/notes`, TinyGo plus `wit-bindgen-go`, built with
  `-gc=leaking`). See BENCHMARKS.md for how they compare.
  - **HTTP:** use `wasip2::http::proxy::export!` (see `guests/hello`).
  - **Platform interfaces:** generate bindings with
    `wit_bindgen::generate!({ path: ..., world: "platform:sdk/imports" })`.
  - **Your own interfaces:** add them to your own world, with `wit/` as a
    dependency (see `guests/stats` and `guests/shortener`).
- **Stack size:** Rust reserves a 1 MiB stack by default, most of a small
  app's memory. `guests/.cargo/config.toml` sets 128 KiB, which brings typical
  apps from 1,088 KiB to 192 KiB per instance. An app that overflows its stack
  traps cleanly (`out of bounds memory access`): the stack sits below the data,
  so it can't corrupt memory. Raise it per app with `-zstack-size` if needed.
- **Compile ahead of time:** `wasm-server compile app.wasm -o app.cwasm`.
  `.cwasm` files contain native code; they must come from the same
  `wasm-server` build on the same CPU architecture.

## Why it is fast

| Technique | Effect |
|---|---|
| Ahead-of-time compilation, mmapped `.cwasm` | No compilation when an app loads |
| `InstancePre` | Imports are resolved and type-checked once per app |
| Pooling allocator, copy-on-write memory images | Instantiation takes a pre-reserved slot instead of calling `mmap` |
| 4 GiB reservation plus guard pages | No bounds checks in the generated code |
| `keep_resident` + `PAGEMAP_SCAN` (Linux) | Reused slots skip `madvise` and page faults; about 2× throughput |
| Epoch interruption | Near-zero-cost CPU limit and cooperative yielding |
| Thread per core, per-worker database connections | No work stealing or cross-core locking on the hot path |
| SQLite in-process | A query is a function call, not a network round trip |

## Limits and behaviour

- **CPU:** a guest that uses more than `--cpu-limit-ms` of CPU is stopped and
  the client gets `504`. Time spent waiting on databases, queues or other
  functions doesn't count. Within its budget, a long-running guest yields every
  10 ms so the other requests on its core keep moving.
- **Memory:** a guest that grows past `--max-memory-mb` fails its allocation,
  and the client gets `500`.
- **Wall clock:** a request still unanswered after `--timeout-ms`, including
  everything it calls, gets `504`, and its guest is cancelled.

## Next steps

- Continuous WAL shipping for point-in-time recovery; read replicas across servers.
- Per-tenant credit pools and quotas (today credits are per app).
- SQLite replication (read replicas, continuous backup to object storage).
- Queues across servers (NATS JetStream), as described in `docs/hosting.md`.
- Outgoing HTTP: HTTP/2 to hosts that offer it, and a private-address
  allow-list for apps that need a service on the same network.
- WASI 0.3 (native async): wasmtime 49 already runs it, so try one Rust app on
  nightly (`wasm32-wasip3`) next to the 0.2 apps; move fully once the target
  is stable. Streaming responses will need a longer request limit.
- **Isolation between apps on one server (noisy neighbours):** one app's
  CPU-heavy requests (20 ms each) raise other apps' p99 from 3 ms to
  160–250 ms. As separate processes the kernel keeps them at about 1 ms
  (`bench/noisy.py`, see `docs/findings.md`).
  - **Plan:** move requests that pass a few ms of CPU off the per-core
    workers to a lower-priority thread pool, with per-app limits on
    concurrent heavy requests.
  - **Then measure:** the cost per request, and the noisy and Conduit
    tests again.
- Per-app logs.
- CLI commands for agents: `new`, `build`, `call`, with `--json` output.
- Benchmark tasks for agent experiments: give agents app specs and measure the
  results.
- A license for the repository.
