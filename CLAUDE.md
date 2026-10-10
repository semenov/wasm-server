# Notes for agents working on this repo

## What this project is

A research and learning project: a server that runs apps as WebAssembly
components, aimed at hosting many small, untrusted apps **written by AI
agents**. It's not a product. The goal is learning what works for agent-built
apps.

Decisions already made:

- **Agents use a CLI, not MCP.** Tooling for agents goes into the
  `wasm-server` CLI (machine-readable output where useful).
- **Rust is the main language** for the server, guests and tooling. No JS/TS.
- **C and Go (TinyGo) guests** (`guests-c/`, `guests-go/`) are
  language-neutrality examples, not main targets.
- **Guest size options rejected:** no `-Cpanic=immediate-abort` profile, and
  the workspace stays at `opt-level=3`. Size isn't a bottleneck, and panic
  messages help agents debug their apps. The 128 KiB stack in
  `guests/.cargo/config.toml` stays.
- **Frame trade-offs around many untrusted apps.** For a single app with one
  owner, a native server wins; see `docs/findings.md`.

## Benchmarks

- **Only Linux numbers count.** Run benchmarks in the `wasm-server-bench`
  Docker image (the `Dockerfile`), with `oha` inside the same container.
  `BENCHMARKS.md` records the environment.
- **Don't trust benchmark numbers taken on macOS from an agent's shell.**
  Processes started there can run at background QoS, and Wasmtime's
  instantiation is slow on macOS regardless. Use macOS to check that things
  work.
- Postgres for tests: `docker compose` (port 5433, TLS certs in
  `bench/certs`). On macOS the Docker CLI may not be on `PATH`:
  `/Applications/Docker.app/Contents/Resources/bin`.
- The benchmark image has no curl or wget; seed data with
  `oha -n N -m POST -d ...`.
- Restarting a server on the same port right after killing it can fail with
  "Address already in use"; wait a moment or use another port.

## Working rules

- Ask before pushing to GitHub; the repository is public. Never commit
  secrets (JWT secrets, database passwords, admin tokens).
- Record new measurements in `BENCHMARKS.md`, conclusions in
  `docs/findings.md`, and planned work in the README's "Next steps".
