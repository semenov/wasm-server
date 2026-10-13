#!/usr/bin/env python3
"""General sandbox benchmark: standard programs, each built natively, for
wasm and for RISC-V, run by the engines' command-line tools, pinned to one
CPU. Reports the best wall time of three runs, peak RSS, and whether the
output matches the native build.

Engines:
  native          the host build (gcc -O2)
  wasmtime        wasmtime run, precompiled with Cranelift (JIT/AOT)
  wasmtime-pulley wasmtime's portable interpreter (Pulley)
  libriscv        rvlinux, interpreter (libriscv's safe mode)
  libriscv-bintr  rvlinux, binary translation through gcc (the docs call it
                  unsafe for sandboxing)
"""
import json, os, re, subprocess, sys, time

PROGRAMS = {  # name: args (sized for roughly 0.5-2 s natively)
    "coremark": ["0x0", "0x0", "0x66", "100000"],
    "nbody": ["10000000"],
    "fannkuch": ["11"],
    "spectralnorm": ["2000"],
    "mandelbrot": ["4000"],
    "binarytrees": ["18"],
    "sha256": ["128"],
    "sort": ["5000000"],
    "hashmap": ["20000000"],
}
ENGINES = {
    "native": lambda p, a: [f"out/native/{p}", *a],
    "wasmtime": lambda p, a: ["wasmtime", "run", "--allow-precompiled", f"out/wasm/{p}.cwasm", *a],
    "wasmtime-pulley": lambda p, a: ["wasmtime", "run", "--target", "pulley64", "--allow-precompiled", f"out/wasm/{p}.pulley", *a],
    "libriscv": lambda p, a: ["rvlinux", "-s", "-n", f"out/rv/{p}", *a],
    "libriscv-bintr": lambda p, a: ["rvlinux", "-s", f"out/rv/{p}", *a],
}
CPU = "2"


def run(cmd, timeout=600):
    """Wall seconds, peak RSS in KiB and stdout of one pinned run."""
    full = ["taskset", "-c", CPU, "/usr/bin/time", "-f", "TIME %e %M", *cmd]
    t = time.monotonic()
    p = subprocess.run(full, capture_output=True, text=True, timeout=timeout, env={**os.environ, "CC": "gcc"})
    wall = time.monotonic() - t
    m = re.search(r"TIME ([\d.]+) (\d+)", p.stderr)
    if p.returncode != 0 or not m:
        raise RuntimeError(f"{cmd}: exit {p.returncode}: {p.stderr[-500:]}")
    return wall, int(m.group(2)), p.stdout


def normalize(out):
    # CoreMark prints timings; compare only its result lines.
    lines = [l for l in out.splitlines()
             if not re.search(r"(Total ticks|Total time|Iterations/Sec|CoreMark 1.0|Compiler version)", l)]
    return "\n".join(lines)


def main():
    """Usage: general.py [engines,...] [programs,...] [output.json]"""
    engines = sys.argv[1].split(",") if len(sys.argv) > 1 else list(ENGINES)
    programs = sys.argv[2].split(",") if len(sys.argv) > 2 and sys.argv[2] else list(PROGRAMS)
    out_file = sys.argv[3] if len(sys.argv) > 3 else "results/sandbox-general.json"
    results = []
    # libriscv-bintr: the first run translates the program (gcc) and caches
    # the result; time it separately.
    for prog in programs:
        expected = None
        for eng in engines:
            cmd = ENGINES[eng](prog, PROGRAMS[prog])
            row = {"program": prog, "engine": eng}
            try:
                if eng == "libriscv-bintr":
                    for f in os.listdir("/tmp"):
                        if f.startswith("rvbintr-"):
                            os.remove(os.path.join("/tmp", f))
                    t = time.monotonic()
                    subprocess.run(["taskset", "-c", CPU, "rvlinux", "-s", "-Q", f"out/rv/{prog}"],
                                   capture_output=True, env={**os.environ, "CC": "gcc"}, timeout=1800, check=True)
                    row["translate_s"] = time.monotonic() - t
                runs = [run(cmd) for _ in range(3)]
                row["wall_s"] = min(r[0] for r in runs)
                row["rss_kib"] = max(r[1] for r in runs)
                out = normalize(runs[0][2])
                if expected is None:
                    expected = out
                row["output_ok"] = out == expected
                m = re.search(r"Iterations/Sec\s*:\s*([\d.]+)", runs[0][2])
                if m:
                    row["coremark_score"] = float(m.group(1))
            except Exception as e:  # report and continue
                row["error"] = str(e)[:300]
            results.append(row)
            print(json.dumps(row), flush=True)
    # Start-up: an empty program, 20 runs.
    for eng in engines:
        cmd = ENGINES[eng]("hello", [])
        try:
            run(cmd)
            times = sorted(run(cmd)[0] for _ in range(20))
            row = {"program": "hello (start-up)", "engine": eng, "wall_s": times[10], "rss_kib": run(cmd)[1]}
        except Exception as e:
            row = {"program": "hello (start-up)", "engine": eng, "error": str(e)[:300]}
        results.append(row)
        print(json.dumps(row), flush=True)
    os.makedirs("results", exist_ok=True)
    with open(out_file, "w") as f:
        json.dump(results, f, indent=1)


if __name__ == "__main__":
    main()
