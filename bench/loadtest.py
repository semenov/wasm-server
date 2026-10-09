#!/usr/bin/env python3
"""Load test for wasm-server: throughput, latency, CPU and memory per scenario.

Starts the server, then for each scenario runs `oha` while sampling the
server process's CPU time and RSS, and reads the server's own per-app
counters (`/_stats`) for guest time, per-instance linear memory and database
calls.

Suites:
  basic    hello and compute apps (no database)
  db       catalog app; needs --db URL (database with bench/schema.sql)
  conduit  RealWorld "Conduit" API; needs --conduit-db URL (bench/conduit.sql).
           Endpoints one at a time, then a mixed workload.
  shortener  The functions showcase on SQLite: redirect (cache + queue),
           a typed call between functions, inserts. Needs --data-dir.

Works on Linux (/proc) and macOS (ps). Prints a Markdown table and writes the
raw numbers to results/<label>.json.
"""

import argparse
import json
import os
import platform
import re
import resource
import secrets
import subprocess
import sys
import threading
import time
import urllib.request


def scenario(name, path, conns, method="GET", auth=False, body=None, host=None):
    """One endpoint under load. A path containing "[" is a regex: oha
    generates a random matching URL for every request. `auth` sends a
    logged-in user's token; `host` routes to a site by domain."""
    return {"name": name, "method": method, "path": path, "conns": conns, "auth": auth, "body": body, "host": host}


SHORT = "short.localhost"


ARTICLE = "article-[1-4][0-9]{4}"  # seeded articles 10000..49999
NEW_ARTICLE = json.dumps({"article": {"title": "Load test article", "description": "Written under load",
                                      "body": "Lorem ipsum " * 50, "tagList": ["load", "test"]}})

SUITES = {
    "basic": [
        scenario("hello c=1", "/hello", 1),
        scenario("hello c=16", "/hello", 16),
        scenario("hello c=64", "/hello", 64),
        scenario("hello c=256", "/hello", 256),
        scenario("fib(20) c=64", "/compute?fib=20", 64),
        scenario("alloc 8MiB c=64", "/compute?alloc=8", 64),
    ],
    "db": [
        scenario("db: product by id c=1", "/catalog/products/[1-9][0-9]{0,3}", 1),
        scenario("db: product by id c=64", "/catalog/products/[1-9][0-9]{0,3}", 64),
        scenario("db: product by id c=256", "/catalog/products/[1-9][0-9]{0,3}", 256),
        scenario("db: list 20 c=64", "/catalog/products?limit=20&offset=[0-9]{1,3}", 64),
        scenario("db: order (tx, 4 round trips) c=64", "/catalog/orders?product=[2-9][0-9]{0,3}&qty=1", 64, "POST"),
    ],
    "shortener": [
        scenario("redirect (cache + queue send)", "/r/CODE", 64, host=SHORT),
        scenario("link info (typed call to stats)", "/api/links/CODE", 64, host=SHORT),
        scenario("top links (typed call)", "/api/top", 64, host=SHORT),
        scenario("create link (SQLite insert)", "/api/links", 64, "POST", body=json.dumps({"url": "https://example.com/some/long/path"}), host=SHORT),
    ],
    "conduit": [
        scenario("list articles (+count)", "/api/articles?limit=20", 64),
        scenario("list by tag", "/api/articles?limit=20&tag=tag[0-9]", 64),
        scenario("get article", f"/api/articles/{ARTICLE}", 64),
        scenario("article comments", f"/api/articles/{ARTICLE}/comments", 64),
        scenario("feed (auth)", "/api/articles/feed?limit=20", 64, auth=True),
        scenario("profile (auth)", "/api/profiles/user[1-9][0-9]{0,3}", 64, auth=True),
        scenario("popular tags", "/api/tags", 64),
        scenario("favorite (tx)", f"/api/articles/{ARTICLE}/favorite", 64, "POST", auth=True),
        scenario("login (argon2id)", "/api/users/login", 16, "POST", body="LOGIN"),
        {
            "name": "MIX: realistic traffic",
            "conns": 128,
            "mix": [
                (25, scenario("list", "/api/articles?limit=20", 0)),
                (10, scenario("list by tag", "/api/articles?limit=20&tag=tag[0-9]", 0)),
                (25, scenario("article", f"/api/articles/{ARTICLE}", 0)),
                (15, scenario("comments", f"/api/articles/{ARTICLE}/comments", 0)),
                (10, scenario("feed", "/api/articles/feed?limit=20", 0, auth=True)),
                (5, scenario("profile", "/api/profiles/user[1-9][0-9]{0,3}", 0, auth=True)),
                (5, scenario("favorite", f"/api/articles/{ARTICLE}/favorite", 0, "POST", auth=True)),
                (3, scenario("create", "/api/articles", 0, "POST", auth=True, body=NEW_ARTICLE)),
                (2, scenario("login", "/api/users/login", 0, "POST", body="LOGIN")),
            ],
        },
        # Last: it adds tens of thousands of articles, which would skew the
        # scenarios after it.
        scenario("create article (tx)", "/api/articles", 64, "POST", auth=True, body=NEW_ARTICLE),
    ],
}

JWT_SECRET = "loadtest-secret-" + secrets.token_hex(16)


class Proc:
    """CPU seconds and RSS of a process, on Linux or macOS."""

    def __init__(self, pid):
        self.pid = pid
        self.linux = platform.system() == "Linux"
        self.tick = os.sysconf("SC_CLK_TCK") if self.linux else None

    def cpu_seconds(self):
        if self.linux:
            with open(f"/proc/{self.pid}/stat") as f:
                fields = f.read().rsplit(")", 1)[1].split()
            return (int(fields[11]) + int(fields[12])) / self.tick  # utime + stime
        out = subprocess.check_output(["ps", "-o", "cputime=", "-p", str(self.pid)], text=True).strip()
        secs = 0.0
        for part in out.split(":"):  # [[hh:]mm:]ss.cc
            secs = secs * 60 + float(part)
        return secs

    def rss_bytes(self):
        if self.linux:
            with open(f"/proc/{self.pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1]) * 1024
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(self.pid)], text=True)
        return int(out.strip()) * 1024


class RssSampler(threading.Thread):
    def __init__(self, proc, interval=0.1):
        super().__init__(daemon=True)
        self.proc, self.interval = proc, interval
        self.peak = 0
        self.stop = threading.Event()

    def run(self):
        while not self.stop.is_set():
            self.peak = max(self.peak, self.proc.rss_bytes())
            time.sleep(self.interval)


def api(base, method, path, body=None, token=None, host=None):
    req = urllib.request.Request(base + path, method=method, data=json.dumps(body).encode() if body else None)
    req.add_header("content-type", "application/json")
    if host:
        req.add_header("host", host)
    if token:
        req.add_header("authorization", f"Token {token}")
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def get_stats(base):
    return api(base, "GET", "/_platform/stats")["apps"]


def wait_ready(base, server, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if server.poll() is not None:
            sys.exit(f"server exited with code {server.returncode}: {server.stderr.read()}")
        try:
            get_stats(base)
            return
        except OSError:
            time.sleep(0.1)
    sys.exit("server did not become ready")


def conduit_setup(base):
    """Registers a load-test user who follows 20 seeded authors, so their feed
    has articles. Returns (token, login body)."""
    name = "load" + secrets.token_hex(4)
    password = "load-test-password"
    user = api(base, "POST", "/api/users", {"user": {"username": name, "email": f"{name}@example.com", "password": password}})
    token = user["user"]["token"]
    for i in range(20):
        api(base, "POST", f"/api/profiles/user{1 + i * 487}/follow", token=token)
    login = json.dumps({"user": {"email": f"{name}@example.com", "password": password}})
    return token, login


def shortener_setup(base):
    """Creates a link to load-test redirects with."""
    return api(base, "POST", "/api/links", {"url": "https://example.com/load-test"}, host=SHORT)["code"]


def oha_cmd(base, s, conns, duration, ctx):
    cmd = ["oha", "--no-tui", "--output-format", "json", "-z", duration, "-c", str(conns), "-m", s["method"]]
    if s["auth"]:
        cmd += ["-H", f"authorization: Token {ctx['token']}"]
    if s.get("host"):
        cmd += ["-H", f"host: {s['host']}"]
    if s["body"]:
        cmd += ["-T", "application/json", "-d", ctx["login"] if s["body"] == "LOGIN" else s["body"]]
    if "CODE" in s["path"]:
        s = {**s, "path": s["path"].replace("CODE", ctx["code"])}
    if "[" in s["path"]:
        # Keep the path's regex classes and repetitions; make everything else
        # literal. oha's regex parser mishandles backslash escapes (`\.`
        # matches any character), so literals use one-character classes.
        parts = re.split(r"(\[[^\]]*\](?:\{[0-9,]+\})?)", base + s["path"])
        literal = lambda text: "".join(c if c.isalnum() or c in "/:-_=&%" else f"[{c}]" for c in text)
        cmd += ["--rand-regex-url", "".join(p if p.startswith("[") else literal(p) for p in parts)]
    else:
        cmd.append(base + s["path"])
    return cmd


def run_oha(cmds):
    """Runs oha commands concurrently and returns their parsed results."""
    procs = [subprocess.Popen(cmd, stdout=subprocess.PIPE) for cmd in cmds]
    return [json.loads(p.communicate()[0]) for p in procs]


def children_cpu():
    r = resource.getrusage(resource.RUSAGE_CHILDREN)
    return r.ru_utime + r.ru_stime


def app_of(s):
    if s.get("host") == SHORT:
        return "shortener"
    return s["path"].strip("/").split("/")[0].split("?")[0]


def run_scenario(base, proc, s, duration, warmup, ctx):
    parts = s.get("mix") or [(1, s)]
    total_weight = sum(w for w, _ in parts)
    # Split connections by weight, at least one each.
    jobs = [(p, max(1, round(s["conns"] * w / total_weight))) for w, p in parts]
    app = app_of(parts[0][1])
    if warmup:
        run_oha([oha_cmd(base, p, c, warmup, ctx) for p, c in jobs])

    before = get_stats(base)[app]
    sampler = RssSampler(proc)
    sampler.start()
    cpu0, client0, t0 = proc.cpu_seconds(), children_cpu(), time.monotonic()
    results = run_oha([oha_cmd(base, p, c, duration, ctx) for p, c in jobs])
    wall = time.monotonic() - t0
    cpu1, client1 = proc.cpu_seconds(), children_cpu()
    sampler.stop.set()
    sampler.join()
    after = get_stats(base)[app]

    delta = {k: after[k] - before[k] for k in before if k != "mem_peak_max_bytes"}
    instances = max(delta["requests"] - delta["errors"] - delta["timeouts"], 1)
    total = ok = 0
    rps = 0.0
    endpoints = []
    for (p, conns), result in zip(jobs, results):
        codes = result.get("statusCodeDistribution", {})
        n = sum(codes.values())
        # 2xx, and 3xx for redirects, are successes.
        good = sum(c for code, c in codes.items() if code[:1] in "23")
        total += n
        ok += good
        rps += result["summary"]["requestsPerSec"] or 0
        lat = result["latencyPercentiles"]
        endpoints.append({
            "endpoint": p["name"], "connections": conns, "rps": result["summary"]["requestsPerSec"] or 0,
            "ok_ratio": good / n if n else 0.0,
            "p50_ms": (lat["p50"] or 0) * 1e3, "p99_ms": (lat["p99"] or 0) * 1e3,
            "status_codes": codes, "errors": result.get("errorDistribution", {}),
        })
    # For a single endpoint the latency is its own; for a mix, the worst p99.
    main = max(endpoints, key=lambda e: e["p99_ms"])
    server_cpu = cpu1 - cpu0
    return {
        "scenario": s["name"],
        "url": s.get("path", "mix"),
        "connections": sum(c for _, c in jobs),
        "requests": total,
        "rps": rps,
        "ok_ratio": ok / total if total else 0.0,
        "p50_ms": endpoints[0]["p50_ms"] if len(endpoints) == 1 else None,
        "p99_ms": main["p99_ms"],
        "p999_ms": (results[0]["latencyPercentiles"]["p99.9"] or 0) * 1e3 if len(endpoints) == 1 else None,
        "server_cpu_cores": server_cpu / wall,
        "server_cpu_us_per_req": server_cpu / max(total, 1) * 1e6,
        "client_cpu_cores": (client1 - client0) / wall,
        "rss_peak_mib": sampler.peak / 2**20,
        "rss_after_mib": proc.rss_bytes() / 2**20,
        "guest_us_avg": delta["guest_ns"] / instances / 1e3,
        "guest_mem_avg_kib": delta["mem_peak_sum_bytes"] / instances / 1024,
        "guest_mem_max_kib": after["mem_peak_max_bytes"] / 1024,
        "db_queries_per_req": delta["db_queries"] / instances,
        "db_wait_us_avg": delta["db_wait_ns"] / instances / 1e3,
        "endpoints": endpoints if len(endpoints) > 1 else None,
    }


def fmt(value, spec):
    return "–" if value is None else spec.format(value)


COLUMNS = [
    ("scenario", "Scenario", "{}"),
    ("rps", "RPS", "{:,.0f}"),
    ("p50_ms", "p50 ms", "{:.2f}"),
    ("p99_ms", "p99 ms", "{:.2f}"),
    ("p999_ms", "p99.9 ms", "{:.2f}"),
    ("ok_ratio", "OK %", "{:.1%}"),
    ("server_cpu_cores", "Srv CPU cores", "{:.2f}"),
    ("server_cpu_us_per_req", "Srv CPU µs/req", "{:.1f}"),
    ("client_cpu_cores", "oha CPU cores", "{:.2f}"),
    ("rss_peak_mib", "RSS peak MiB", "{:.1f}"),
    ("guest_us_avg", "Guest µs/req", "{:.1f}"),
    ("guest_mem_avg_kib", "Guest mem KiB/inst", "{:,.0f}"),
]
DB_COLUMNS = [
    ("db_queries_per_req", "DB calls/req", "{:.1f}"),
    ("db_wait_us_avg", "DB wait µs/req", "{:.1f}"),
]


def table(rows):
    columns = COLUMNS + (DB_COLUMNS if any(r["db_queries_per_req"] for r in rows) else [])
    lines = ["| " + " | ".join(c[1] for c in columns) + " |", "|" + "---|" * len(columns)]
    for r in rows:
        lines.append("| " + " | ".join(fmt(r[key], spec) for key, _, spec in columns) + " |")
    for r in rows:
        if r["endpoints"]:
            lines += ["", f"{r['scenario']}, per endpoint:", "",
                      "| Endpoint | Conns | RPS | p50 ms | p99 ms | OK % |", "|---|---|---|---|---|---|"]
            for e in r["endpoints"]:
                lines.append(f"| {e['endpoint']} | {e['connections']} | {e['rps']:,.0f} | {e['p50_ms']:.2f} "
                             f"| {e['p99_ms']:.2f} | {e['ok_ratio']:.1%} |")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--server", default="./target/release/wasm-server")
    ap.add_argument("--apps-dir", default="apps")
    ap.add_argument("--listen", default="127.0.0.1:8080")
    ap.add_argument("--duration", default="10s")
    ap.add_argument("--warmup", default="2s", help="per scenario; empty to skip")
    ap.add_argument("--label", default=f"{platform.system().lower()}-{platform.machine()}")
    ap.add_argument("--suite", choices=[*SUITES, "all"], default="basic")
    ap.add_argument("--db", help="Postgres URL for the catalog app (db suite)")
    ap.add_argument("--conduit-db", help="Postgres URL for the conduit app (conduit suite)")
    ap.add_argument("--db-ca-file", help="CA certificate for database TLS")
    ap.add_argument("--data-dir", help="directory for SQLite databases and the job store (shortener suite)")
    ap.add_argument("--only", help="run only scenarios whose name contains this text")
    ap.add_argument("server_args", nargs="*", help="extra args for `wasm-server serve` (after --)")
    args = ap.parse_args()

    needs = {"db": args.db, "conduit": args.conduit_db}
    for suite, url in needs.items():
        if args.suite in (suite, "all") and not url:
            sys.exit(f"the {suite} suite needs --{'db' if suite == 'db' else 'conduit-db'} postgres://...")

    cmd = [
        args.server, "serve", "--listen", args.listen,
        "--app", f"hello={args.apps_dir}/hello.cwasm",
        "--app", f"compute={args.apps_dir}/compute.cwasm",
        "--app", f"catalog={args.apps_dir}/catalog.cwasm",
        # Conduit's routes start with /api, so it is deployed as app "api".
        "--app", f"api={args.apps_dir}/conduit.cwasm",
        "--env", f"api:JWT_SECRET={JWT_SECRET}",
        *(["--db", f"main={args.db}", "--grant", "catalog=main"] if args.db else []),
        *(["--db", f"conduit={args.conduit_db}", "--grant", "api=conduit"] if args.conduit_db else []),
        *(["--db-ca-file", args.db_ca_file] if args.db_ca_file else []),
        *args.server_args,
    ]
    if args.suite in ("shortener", "all"):
        if not args.data_dir:
            sys.exit("the shortener suite needs --data-dir DIR")
        d = args.data_dir
        cmd += [
            "--app", f"shortener={args.apps_dir}/shortener.cwasm", "--app", f"stats={args.apps_dir}/stats.cwasm",
            "--db", f"links=sqlite:{d}/links.db", "--db-init", "links=examples/shortener/links.sql", "--grant", "shortener=links",
            "--db", f"stats=sqlite:{d}/stats.db", "--db-init", "stats=examples/shortener/stats.sql", "--grant", "stats=stats",
            "--queue", "clicks=stats", "--cron", "stats:rollup=*/10 * * * * *", "--data-dir", d,
            "--domain", f"{SHORT}=shortener",
        ]
    scenarios = [s for suite, items in SUITES.items() if args.suite in (suite, "all") for s in items]
    server = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    base = f"http://{args.listen}"
    try:
        wait_ready(base, server)
        proc = Proc(server.pid)
        time.sleep(0.5)
        idle_rss = proc.rss_bytes() / 2**20
        printable = [a.replace(JWT_SECRET, "***") for a in cmd[1:]]
        print(f"# {args.label}: {' '.join(printable)}")
        print(f"cpus={os.cpu_count()} idle RSS={idle_rss:.1f} MiB\n", flush=True)

        ctx = {}
        if any(app_of(s.get("mix", [(0, s)])[0][1]) == "api" for s in scenarios):
            ctx["token"], ctx["login"] = conduit_setup(base)
        if args.suite in ("shortener", "all"):
            ctx["code"] = shortener_setup(base)

        rows = []
        for s in scenarios:
            if args.only and args.only not in s["name"]:
                continue
            row = run_scenario(base, proc, s, args.duration, args.warmup, ctx)
            rows.append(row)
            print(f"  done: {s['name']}: {row['rps']:,.0f} rps", file=sys.stderr, flush=True)

        print(table(rows))
        os.makedirs("results", exist_ok=True)
        out = f"results/{args.label}.json"
        with open(out, "w") as f:
            json.dump({"label": args.label, "cpus": os.cpu_count(), "command": printable,
                       "idle_rss_mib": idle_rss, "scenarios": rows}, f, indent=2)
        print(f"\nraw results: {out}")
    finally:
        server.terminate()
        server.wait()


if __name__ == "__main__":
    main()
