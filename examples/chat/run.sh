#!/usr/bin/env bash
# Runs the chat showcase: a WebSocket function (no HTTP), a static page,
# SQLite history, cache-based rate limiting. Open http://chat.localhost:8080/
set -euo pipefail
cd "$(dirname "$0")/../.."
SERVER=${SERVER:-./target/release/wasm-server}

exec "$SERVER" serve \
  --app chat=apps/chat.cwasm \
  --static chat=sites/chat \
  --domain chat.localhost=chat \
  --websocket chat=/ws \
  --db chat=sqlite:data/chat/chat.db --db-init chat=examples/chat/chat.sql --grant chat=chat \
  --data-dir data/chat \
  "$@"
