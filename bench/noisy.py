#!/usr/bin/env python3
"""Noisy neighbour: how one app's CPU-heavy requests affect another app's
latency on the same wasm-server.

The victim app (`hello`) gets a steady, modest request rate; the noisy app
(`compute?fib=N`, about 20 ms of CPU at N=33) is driven by a growing number
of connections. For each server size (workers pinned to as many CPUs) it
reports the victim's latency and the noisy app's throughput. Victim latency
is measured from each request's scheduled send time (oha's latency
correction), so time spent queued behind the noise counts.

--separate runs each app in its own wasm-server process on the same CPUs,
as a control: the kernel then shares the CPUs between the apps, as it would
with a process per app.

Runs in the benchmark image:
  docker run --rm -v "$PWD/results:/bench/results" --entrypoint python3 wasm-server-bench noisy.py
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request


def oha(url, conns, duration, rate=None):
    cmd = ["oha", "--no-tui", "--output-format", "json", "-z", duration, "-c", str(conns)]
    if rate:
        cmd += ["-q", str(rate), "--latency-correction"]
    return subprocess.Popen([*cmd, url], stdout=subprocess.PIPE)


def summary(proc):
    d = json.loads(proc.communicate()[0])
    lat = d["latencyPercentiles"]
    codes = d.get("statusCodeDistribution", {})
    n = sum(codes.values())
    return {
        "rps": d["summary"]["requestsPerSec"] or 0,
        "p50_ms": (lat["p50"] or 0) * 1e3,
        "p99_ms": (lat["p99"] or 0) * 1e3,
        "p999_ms": (lat["p99.9"] or 0) * 1e3,
        "max_ms": (d["summary"]["slowest"] or 0) * 1e3,
        "ok_ratio": codes.get("200", 0) / n if n else 0.0,
    }


def cpu_seconds(pid):
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def start(args, workers, port, apps):
    cpus = f"0-{workers - 1}" if workers > 1 else "0"
    cmd = ["taskset", "-c", cpus, args.server, "serve", "--listen", f"127.0.0.1:{port}", "--workers", str(workers)]
    for app in apps:
        cmd += ["--app", f"{app}={args.apps_dir}/{app}.cwasm"]
    server = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    for _ in range(100):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/{apps[0]}", timeout=1).read()
            return server
        except OSError:
            time.sleep(0.1)
    sys.exit(f"server did not start: {server.stderr.read()}")


def run_size(args, workers):
    if args.separate:
        servers = [start(args, workers, args.port, ["hello"]), start(args, workers, args.port + 1, ["compute"])]
        victim_base, noisy_base = f"http://127.0.0.1:{args.port}", f"http://127.0.0.1:{args.port + 1}"
    else:
        servers = [start(args, workers, args.port, ["hello", "compute"])]
        victim_base = noisy_base = f"http://127.0.0.1:{args.port}"
    cpu = lambda: sum(cpu_seconds(s.pid) for s in servers)
    rows = []
    try:
        # Noise levels: none, one connection, one per worker, four per worker.
        for noisy_conns in sorted({0, 1, workers, 4 * workers}):
            cpu0, t0 = cpu(), time.monotonic()
            victim = oha(victim_base + "/hello", args.victim_conns, args.duration, rate=args.victim_rate)
            noise = oha(f"{noisy_base}/compute?fib={args.fib}", noisy_conns, args.duration) if noisy_conns else None
            v = summary(victim)
            n = summary(noise) if noise else None
            cores = (cpu() - cpu0) / (time.monotonic() - t0)
            row = {"workers": workers, "noisy_conns": noisy_conns, "victim": v, "noisy": n, "server_cores": cores}
            rows.append(row)
            print(f"  workers={workers} noisy conns={noisy_conns}: victim {v['rps']:.0f} rps, p99 {v['p99_ms']:.1f} ms, "
                  f"noisy {n['rps'] if n else 0:.0f} rps", file=sys.stderr, flush=True)
            time.sleep(1)
    finally:
        for s in servers:
            s.terminate()
            s.wait()
    return rows


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--server", default="/usr/local/bin/wasm-server")
    ap.add_argument("--apps-dir", default="apps")
    ap.add_argument("--port", type=int, default=8080, help="--separate also uses the next one")
    ap.add_argument("--separate", action="store_true", help="each app in its own server process (control)")
    ap.add_argument("--duration", default="10s")
    ap.add_argument("--workers", default="8,1", help="server sizes to test, comma-separated")
    ap.add_argument("--fib", type=int, default=33, help="the noisy request: fib(N), 33 is about 20 ms")
    ap.add_argument("--victim-rate", type=int, default=1000, help="victim requests per second")
    ap.add_argument("--victim-conns", type=int, default=256,
                    help="enough that the connection count doesn't cap the victim's rate")
    ap.add_argument("--label", default="linux-docker-noisy")
    args = ap.parse_args()

    rows = [r for w in args.workers.split(",") for r in run_size(args, int(w))]
    mode = "one process per app" if args.separate else "both apps in one server"
    print(f"{mode}; victim: /hello at {args.victim_rate} rps; noisy: /compute?fib={args.fib}\n")
    print("| Workers (CPUs) | Noisy conns | Noisy RPS | Victim RPS | Victim p50 ms | Victim p99 ms | Victim p99.9 ms | Victim max ms | Victim OK % | Server cores |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    for r in rows:
        v, n = r["victim"], r["noisy"]
        print(f"| {r['workers']} | {r['noisy_conns']} | {n['rps'] if n else 0:,.0f} | {v['rps']:,.0f} | {v['p50_ms']:.2f} | {v['p99_ms']:.2f} "
              f"| {v['p999_ms']:.2f} | {v['max_ms']:.1f} | {v['ok_ratio']:.1%} | {r['server_cores']:.2f} |")
    os.makedirs("results", exist_ok=True)
    with open(f"results/{args.label}.json", "w") as f:
        json.dump({"label": args.label, "args": vars(args), "rows": rows}, f, indent=2)


if __name__ == "__main__":
    main()
