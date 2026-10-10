# Native Go vs wasm: the notes app

The same notes app as `guests-c/notes` and `guests/notes-rs`, written as an
ordinary Go server, to compare with the wasm apps. Results are in
`BENCHMARKS.md` ("Native Go vs wasm").

- `pure/`: SQLite through `modernc.org/sqlite` (pure Go, no cgo).
- `cgo/`: SQLite through `mattn/go-sqlite3` (cgo, the C SQLite).
- `native.sh`: throughput, CPU per 1,000 requests and memory for wasm C, wasm
  Rust and both Go servers.
- `density.sh`: memory of one wasm server with 1 vs 200 apps, and of one Go
  process.

Both scripts run inside the `wasm-server-bench` image with a directory mounted
at `/cmp` that holds `c.wasm`, `rs.wasm`, `notes.sql`, `gonative` and
`gonative-cgo` (Linux builds):

```sh
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go -C pure build -ldflags='-s -w' -o "$DIR/gonative" .
docker run --rm -v "$PWD/cgo:/src" -v "$DIR:/out" -w /src golang:1.27-bookworm \
    go build -ldflags='-s -w' -o /out/gonative-cgo .
cp native.sh density.sh notes.sql "$DIR/"
docker run --rm --cpus 8 -v "$DIR:/cmp" --entrypoint sh wasm-server-bench /cmp/native.sh
```
