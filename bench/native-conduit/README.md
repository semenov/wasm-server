# Native Go and Rust vs wasm: the Conduit API

The RealWorld "Conduit" API (`guests/conduit`) as ordinary servers, to
compare a typical backend running as a wasm app with the same backend built
the usual way. Results are in `BENCHMARKS.md` ("Typical backend: wasm vs
native Go and Rust").

- `go/`: `net/http` (Go 1.22 routing), pgx v5 (`pgxpool`, cached prepared
  statements), `golang.org/x/crypto/argon2`, `encoding/json`.
- `rust/`: axum 0.8 on tokio, tokio-postgres with deadpool (cached prepared
  statements), rustls, serde_json; argon2 on tokio's blocking pool.

Same as the wasm app:
- routes, SQL statements, JSON;
- JWT (HS256) and argon2id parameters (19 MiB, 2 passes);
- cached total counts (10 s) and popular tags (30 s), in-process and shared
  by all threads, like the wasm host's per-app cache;
- Postgres over TLS with certificate and host name checks (`verify-full`);
- 32 database connections (the wasm server: 8 workers × 4).

`compare.sh` runs the `compare` suite of `bench/loadtest.py` (`hello`, then
the conduit scenarios) for each target in the Linux benchmark image:

```sh
docker build -t wasm-server-bench .
docker compose up -d postgres
bench/native-conduit/compare.sh 2          # 2 rounds of wasm, go, rust
```

- **Clean data for every target:** each run starts from a copy of the seed
  data (`conduit_seed`, made once from `bench/conduit.sql`).
- **The same 8 CPUs:** every server is pinned to CPUs 0–7 (`taskset`);
  `GOMAXPROCS=8`, 8 tokio workers, `--workers 8`.
- **Postgres CPU** comes from its container's cgroup.

Without Docker, against any Postgres with the Conduit schema:

```sh
JWT_SECRET=... DATABASE_URL='postgres://...?sslmode=verify-full&sslrootcert=ca.pem' go -C go run .
JWT_SECRET=... DATABASE_URL='postgres://...' DB_CA_FILE=ca.pem cargo run --release --manifest-path rust/Cargo.toml
```
