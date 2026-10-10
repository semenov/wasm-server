#!/bin/sh
# Native Go vs wasm apps: throughput, memory and CPU per request.
cd /cmp
for w in c rs; do wasm-server compile $w.wasm -o /tmp/$w.cwasm >/dev/null; done
cpu() { awk '{print $14+$15}' /proc/$1/stat; }        # utime+stime in ticks
rss() { awk '/VmRSS/{print $2}' /proc/$1/status; }     # KiB
measure() { # label pid path
  oha --no-tui -n 20 -c 1 -m POST -d "a note with \"quotes\" and some text" http://127.0.0.1:8080/$3 >/dev/null
  idle=$(rss $2)
  for kind in GET POST; do
    if [ $kind = GET ]; then args="-z 6s -c 64"; else args="-z 6s -c 64 -m POST -d hello"; fi
    c0=$(cpu $2)
    out=$(oha --no-tui --output-format json $args http://127.0.0.1:8080/$3 2>/dev/null)
    c1=$(cpu $2); peak=$(awk '/VmHWM/{print $2}' /proc/$2/status)
    echo "$out" | python3 -c '
import json,sys
d=json.load(sys.stdin); s=d["summary"]; p=d["latencyPercentiles"]
n=sum(d["statusCodeDistribution"].values()); ticks=int(sys.argv[3])-int(sys.argv[2])
print("  %-18s %-4s %7.0f rps  p50 %.2f ms  p99 %.2f ms  cpu %5.1f ms/1k req  rss idle %5d KiB peak %6d KiB" % (sys.argv[1], sys.argv[4], s["requestsPerSec"], p["p50"]*1000, p["p99"]*1000, ticks*10/(n/1000), int(sys.argv[5]), int(sys.argv[6])))' "$1" $c0 $c1 $kind $idle $peak
  done
}
wasm_run() {
  wasm-server serve --listen 127.0.0.1:8080 --workers 8 --app $2=/tmp/$1.cwasm --db notes=sqlite:/tmp/w-$1.db --db-init notes=/cmp/notes.sql --grant $2=notes --data-dir /tmp/wd-$1 >/tmp/w-$1.log 2>&1 &
  pid=$!; sleep 2; measure "wasm $1" $pid $2; kill $pid; wait $pid 2>/dev/null
}
go_run() {
  GOMAXPROCS=8 /cmp/$1 /tmp/$1.db 127.0.0.1:8080 >/tmp/$1.log 2>&1 &
  pid=$!; sleep 1; measure "native $1" $pid gonotes; kill $pid; wait $pid 2>/dev/null
}
echo "== 8 cores, 64 connections, 6 s per test, oha in the same container"
for round in 1 2; do wasm_run c cnotes; wasm_run rs rnotes; go_run gonative-cgo; go_run gonative; done
