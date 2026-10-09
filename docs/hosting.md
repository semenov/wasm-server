# Multi-server hosting: design

How `wasm-server` could become a hosting platform for users' wasm apps across
many servers: routing, database connections and billing. This is a proposal,
not implemented. The numbers come from `BENCHMARKS.md`.

## Starting point

Three properties of the current server shape the design:

- **Instances are almost free:** 6 µs to create, about 1 MiB of memory, and they
  live for one request. An idle app costs only its `.cwasm` file on disk, so
  any node can serve any app, and keeping thousands of apps on a node is cheap.
- **Apps are stateless between requests.** Anything long-lived (database
  connections, caches, secrets) belongs to the host. That is what makes
  placement flexible, and it is also where the hard parts are.
- **The database, not the runtime, is the bottleneck for typical apps.** In the
  Conduit mix the server used 3 cores and Postgres 10.6. The design has to
  protect databases more than it has to squeeze the runtime.

## Components

```
                 users / custom domains
                          │  DNS / anycast
                 ┌────────▼────────┐
                 │  Routers (L7)   │  TLS termination, host → app,
                 │  stateless, N   │  app → node set, rate limits
                 └───┬─────────┬───┘
          app affinity│         │
        ┌─────────────▼──┐   ┌──▼─────────────┐
        │ Node (wasm-    │   │ Node            │  thread-per-core server,
        │ server) × many │   │                 │  artifact cache, usage counters
        └───────┬────────┘   └────────┬────────┘
                │   sql calls (RPC)   │
              ┌─▼─────────────────────▼─┐
              │ DB gateway (sharded by  │  pools, credentials, timeouts,
              │ database, per region)   │  DB metering
              └────────────┬────────────┘
                           │ few real connections
                      user databases

 Control plane: apps, versions, domains, secrets, grants, quotas, placement
 → streams config to routers, nodes and gateways.
 Usage pipeline: nodes, routers, gateways → usage stream → billing.
```

## Deploy

1. The user uploads a `.wasm` component, through the API or CLI.
2. The control plane validates it, checking the imports against what the
   platform offers (`wasi:http`, `platform:db/sql`, …).
3. It compiles the component ahead of time for each node type (CPU architecture
   plus Wasmtime version) and stores the `.cwasm` files content-addressed, by
   hash, in object storage.
4. A deployment is an immutable version, `app@v42`. Routes point at a version,
   so switching is atomic. Canary releases are percentage splits at the router,
   and a rollback is a route change.
5. Nodes fetch artifacts lazily on first request and keep an LRU cache on local
   disk. A cached `.cwasm` loads in 1–6 ms via mmap; the first request after a
   cache miss pays the download, tens of ms within a data centre. Popular
   artifacts can be pushed to nodes in advance.

## Routing

**Host name → app.** Each app gets `<app>.platform.app` plus custom domains
(certificates via ACME). Routers keep the whole table in memory, since millions
of entries is still small, and receive updates as a stream from the control
plane. The current path-prefix routing becomes Host-based routing.

**App → nodes: consistent hashing with bounded load.**

- Any node *could* run any app, but spreading an app across all nodes is
  wasteful: every node would need its artifact and, worse, its own database
  connections.
- So each app gets a *small set of nodes* from rendezvous hashing
  (`hash(app, node)`, top K). At least K = 2, for availability.
- The router sends each request to the least-loaded node in the set.
- When an app's nodes are above a load threshold, the router extends the set to
  the next nodes in the hash order (consistent hashing with bounded loads).
  Popular apps spread over many nodes; quiet apps stay on 2.
- Node membership and load come from heartbeats. When a node is added or
  removed, only about 1/N of apps move.
- **Failures:** routers health-check nodes. An idempotent request (GET, HEAD)
  is retried once on another node in the set. A node being taken out of
  service stops accepting new connections and finishes in-flight requests.

**Regions:** an app runs in the region of its database. A wasm instance is
cheap anywhere, but each database round trip from far away costs tens of ms. A
global edge (anycast plus routers in every region) forwards requests to the
app's home region. Running read-only, database-free apps at the edge is a later
optimisation.

## Postgres: not every node pooling to every database

The problem: N nodes × M apps × pool size. With 100 nodes, 1,000 apps and 4
connections each, that would be 400k connections. Postgres handles a few
hundred, at roughly 5–10 MB each. There are three levers, which combine.

**1. Affinity (from routing).** An app's database is only used from the
app's K nodes. Connections per database become K × (per-node pool), for
example 2 × 4 = 8, instead of N × pool. This is the biggest lever, and it comes
from the routing design for free.

**2. Elastic pools on nodes.**

- A pool is created on first use and shrinks to 0 after it has been idle for
  30–60 s, so quiet apps hold no connections.
- Each node has a cap: per app, and in total.
- Requests wait for a free connection, bounded by the existing database
  timeout.
- This replaces today's fixed `--db-pool-size` per worker. Pools still belong to
  each worker core; only their size adapts to load.

**3. A DB gateway as a separate tier.** When apps and nodes number in the
thousands, nodes stop talking to databases directly:

- **What it is:** a gateway service, sharded by database, that owns the pools.
  Nodes send `query(db, sql, params)` to it over an RPC protocol: the same
  `platform:db/sql` interface, sent over the network.
- **Connections:** each database gets pools in exactly one place (plus a
  standby).
- **Credentials** live only in the gateway. Nodes never see them, which is a
  security gain.
- **Control in one place:** per-tenant concurrency limits, query timeouts,
  metering of database time, read-replica routing, and later result caching.
- **Cost:** one extra network hop, about 0.1–0.3 ms inside a data centre. That
  matters little next to a 0.4–3 ms query, and the gateway can live next to the
  database.
- **Prior art:** this is the model of Cloudflare Hyperdrive, and of
  PgBouncer/PgCat/Supavisor run as a service.

**Why the interface allows this:** `platform:db/sql` has no session state.
There are only single calls and explicit transactions: no `SET`, no
`LISTEN`, no advisory locks. That makes *transaction-level pooling* safe. Many
apps' calls can share a few server connections, and a connection belongs to a
request only for the duration of a call or transaction. This is exactly why
PgBouncer's transaction mode breaks ordinary drivers but would work here.

**Hosted databases:** the platform can also run Postgres for users (shared
clusters with a database per app and a role per app, plus quotas), or connect
to external ones: Neon, Supabase, RDS, or a user's own server. External
databases go through the same gateway; providers' serverless HTTP drivers are
an alternative.

**Recommended path:** start with 1 + 2 (affinity and elastic pools). Add the
gateway once the number of apps or nodes makes direct connections the limit,
or as soon as the platform hosts databases itself.

## Update: SQLite per app, functions, queues across servers

The decisions since this was written change parts of the design.

**SQLite per app (the default database).**

- **Primary:** an app's database file lives on one node of its set (the
  first by rendezvous hash), which takes all writes. Reads from the app's
  other nodes go to local replicas, kept current by shipping the write-ahead
  log, as in LiteFS and libSQL. Reads are then local function calls; writes
  pay one hop to the primary when the app runs elsewhere.
- **Backup:** the log is copied continuously to object storage (Litestream
  style). A replacement primary restores from the latest backup plus replicas.
- **Failover:** when the primary node fails, the next node in the hash order
  becomes primary. Its replica is promoted, and writes pause for a few seconds.
- **Scaling:** this covers almost all apps. An app that outgrows one writer
  moves to Postgres behind the same `platform:sdk/sql` interface. The
  connection problem described above applies only to those apps.

**Typed calls between functions across servers.**

- **Same node:** if the target app has an instance pool on the caller's node
  (for example both apps are in the same node set), the call stays local,
  which is what the server does today.
- **Other node:** otherwise the host sends it to a node in the target's set
  over a persistent connection, with arguments serialized in the component
  model's canonical encoding. A typed call is then a small binary RPC, about
  0.1–0.3 ms inside a data centre.
- **Placement:** call pairs that are used heavily get placed on overlapping
  node sets, so most calls stay local.

**Queues and cron across servers.**

- **Messages:** the SQLite job store becomes NATS JetStream, which gives
  durable streams per queue, explicit acknowledgements, redelivery with
  backoff, and dead letters.
- **Consumers:** each consumer app gets a JetStream consumer, and its nodes pull
  messages, so delivery follows the app's placement.
- **Cron:** the dedup key (`app:name:scheduled-time`) works unchanged as
  JetStream message deduplication, so several schedulers can run and each tick
  still fires once.

## Billing

**What costs us money:**

- CPU while guest code runs.
- Memory while an instance is alive, including while it waits on I/O.
- Egress traffic.
- Database work.

Waiting on the database costs us almost nothing beyond the instance's memory.

**What to meter, per app.** The server already counts most of this per worker
without contention (`/_stats`); billing just needs it exported.

| Meter | Source | Notes |
|---|---|---|
| Requests | node | Main unit |
| CPU-ms | node (`CpuMeter`) | Only time guest code runs; I/O waits excluded |
| Memory GB-s | node | Peak linear memory × instance lifetime |
| Egress GB | router | Response bytes plus outbound HTTP |
| DB calls, DB-ms | node or gateway | Time spent in the database |
| DB storage GB-month | DB cluster | Only for databases the platform hosts |

**Pricing shape:**

- **Free tier:** for example 100k requests per day, 10 ms CPU per request and
  64 MiB memory per instance.
- **Paid:** per million requests plus per million CPU-ms, with higher limits per
  request.
- Billing CPU rather than wall time is fair to apps that mostly wait on
  databases or APIs, and it matches our real costs. Cloudflare Workers bill the
  same way.
- Memory can be included up to the plan's limit per instance, and billed as
  GB-s above it.

**Pipeline:**

1. Nodes and routers aggregate per-app counters every 10–60 s.
2. They emit usage records `(app, node, window, requests, cpu_us, mem_gb_s,
   egress, db_us)` to a stream (Kafka/NATS). Each record has an idempotency key
   `(node, app, window_seq)`, so a retry never bills twice.
3. Rollups land in an analytics store (ClickHouse) for dashboards, and hourly
   totals go to the billing database.
4. Invoices are generated from the hourly totals, for example with Stripe
   metered billing.

**Enforcement:**

- **Per request:** the CPU, memory and wall-clock limits that already exist.
- **Per app:**
  - Rate and concurrency limits live at the router, with each router taking a
    share of the limit.
  - A per-node concurrency cap also falls out of affinity.
  - Database concurrency is capped at the gateway.
- **Spending caps:** the usage stream feeds near-real-time budgets, with about a
  minute of delay. An app over budget gets `429` or `402` from the router
  until the next period or until the user raises the cap.

## Isolation between tenants

- **Sandbox:** wasm isolation per request, plus the per-request limits.
- **Defence in depth:**
  - One process per trust tier (free, paid, enterprise) on separate nodes.
  - seccomp on node processes.
  - Wasmtime's Spectre mitigations kept on.
- **Secrets:** encrypted at rest, sent to nodes only for apps placed there, and
  exposed only as that app's environment.
- **Network:** apps have no raw sockets. Outbound HTTP goes through a host
  policy (allow-lists, egress metering, blocking internal addresses).

## Changes to wasm-server for this

1. Host-based routing, and apps identified by version.
2. A control-plane client: stream in apps, routes, secrets and grants; fetch
   artifacts into a local LRU cache; unload idle apps.
3. Elastic per-worker database pools (min 0, idle timeout, per-app caps), later
   a gateway client implementing `platform:db/sql`.
4. Usage export from the existing per-worker counters, plus egress counting.
5. Graceful drain and health/readiness endpoints for routers.
