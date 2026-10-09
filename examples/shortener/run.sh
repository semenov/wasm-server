#!/usr/bin/env bash
# Runs the link shortener showcase: two functions, a static site, two SQLite
# databases, a queue and a cron job. Open http://short.localhost:8080/
set -euo pipefail
cd "$(dirname "$0")/../.."
SERVER=${SERVER:-./target/release/wasm-server}

exec "$SERVER" serve \
  --app shortener=apps/shortener.cwasm \
  --app stats=apps/stats.cwasm \
  --static shortener=sites/shortener \
  --domain short.localhost=shortener \
  --db links=sqlite:data/links.db --db-init links=examples/shortener/links.sql --grant shortener=links \
  --db stats=sqlite:data/stats.db --db-init stats=examples/shortener/stats.sql --grant stats=stats \
  --queue clicks=stats \
  --cron 'stats:rollup=*/10 * * * * *' \
  "$@"
