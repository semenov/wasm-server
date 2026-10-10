#!/bin/sh
# Conduit as a wasm app vs native Go and Rust servers: the `compare` suite of
# bench/loadtest.py for each target, in the Linux benchmark image, against
# the compose Postgres over TLS. Every target starts from the same clean
# data (a copy of the seed database), is pinned to the same 8 CPUs, and gets
# 32 database connections. Postgres CPU is read from its container's cgroup.
#
#   docker build -t wasm-server-bench .
#   docker compose up -d postgres
#   bench/native-conduit/compare.sh [ROUNDS] [TARGETS...]     # default: 1 wasm go rust
set -e
cd "$(dirname "$0")/../.."
rounds=${1:-1}
[ $# -gt 0 ] && shift
targets=${*:-wasm go rust}
pg() { docker compose exec -T postgres psql -U postgres -v ON_ERROR_STOP=1 -q "$@"; }

if [ "$(pg -tAc "SELECT 1 FROM pg_database WHERE datname = 'conduit_seed'")" != 1 ]; then
    echo "creating conduit_seed (a clean copy of the seed data)..."
    sed -e 's/^CREATE DATABASE conduit;/CREATE DATABASE conduit_seed;/' -e 's/^\\connect conduit$/\\connect conduit_seed/' \
        bench/conduit.sql | pg
fi
cgroup=/hostcg/docker/$(docker compose ps -q postgres)

for round in $(seq "$rounds"); do
    for target in $targets; do
        echo "== round $round: $target"
        pg -c "DROP DATABASE IF EXISTS conduit WITH (FORCE)" -c "CREATE DATABASE conduit TEMPLATE conduit_seed"
        docker compose run --rm -e DB_CGROUP="$cgroup" bench --suite compare --target "$target" --cpu-list 0-7 \
            --conduit-db "postgres://postgres:bench@postgres/conduit?sslmode=verify-full" --db-ca-file /certs/ca.pem \
            --label "linux-docker-compare-$target-r$round" -- --workers 8
    done
done
