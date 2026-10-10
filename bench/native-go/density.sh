#!/bin/sh
cd /cmp; wasm-server compile rs.wasm -o /tmp/rs.cwasm >/dev/null
for n in 1 200; do
  args=""; for i in $(seq $n); do args="$args --app a$i=/tmp/rs.cwasm --grant a$i=notes"; done
  wasm-server serve --listen 127.0.0.1:8080 --workers 8 $args --db notes=sqlite:/tmp/d.db --db-init notes=/cmp/notes.sql --data-dir /tmp/dd$n >/tmp/d$n.log 2>&1 &
  pid=$!; sleep 3
  # touch every app once so each is really loaded and has served a request
  for i in $(seq $n); do oha --no-tui -n 1 -c 1 http://127.0.0.1:8080/a$i >/dev/null 2>&1; done
  sleep 1; printf '%4d apps: rss %6d KiB\n' $n $(awk '/VmRSS/{print $2}' /proc/$pid/status)
  kill $pid; wait $pid 2>/dev/null
done
GOMAXPROCS=8 /cmp/gonative /tmp/g.db 127.0.0.1:8080 >/dev/null 2>&1 & pid=$!; sleep 1; oha --no-tui -n 10 -c 1 http://127.0.0.1:8080/gonotes >/dev/null 2>&1
printf 'one native Go app after a few requests: rss %6d KiB\n' $(awk '/VmRSS/{print $2}' /proc/$pid/status); kill $pid
ls -l /tmp/rs.cwasm | awk '{print "rs.cwasm on disk:", $5, "bytes"}'
