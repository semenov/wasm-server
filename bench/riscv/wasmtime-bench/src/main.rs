//! wasmtime side of the sandbox comparison: the same tests as rv_bench.cpp,
//! with the engine configured like the server's (`server/src/engine.rs`):
//! Cranelift at Speed, epoch interruption, pooling allocator, copy-on-write
//! memory images, 2 MiB kept resident per reused slot.
//!
//! Usage: wasmtime-bench guest.wasm. Prints one JSON object per line.
use std::time::Instant;
use wasmtime::*;

fn emit(test: &str, value: f64, unit: &str, check: i64) {
    println!(r#"{{"engine": "wasmtime", "test": "{test}", "value": {value:.3}, "unit": "{unit}", "check": {check}}}"#);
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Median ns of one call to `f`, over `runs` runs after `warmup`.
fn time_median(warmup: usize, runs: usize, mut f: impl FnMut()) -> f64 {
    (0..warmup).for_each(|_| f());
    median((0..runs).map(|_| {
        let t = Instant::now();
        f();
        t.elapsed().as_nanos() as f64
    }).collect())
}

/// Average ns per iteration.
fn time_avg(n: u64, mut f: impl FnMut()) -> f64 {
    let t = Instant::now();
    (0..n).for_each(|_| f());
    t.elapsed().as_nanos() as f64 / n as f64
}

fn rss_kib() -> i64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:").map(|v| v.trim().trim_end_matches(" kB").parse().unwrap()))
        .unwrap_or(0)
}

fn main() -> Result<()> {
    let path = std::env::args().nth(1).expect("usage: wasmtime-bench guest.wasm");
    let mut config = Config::new();
    config.cranelift_opt_level(OptLevel::Speed);
    config.epoch_interruption(true);
    config.memory_init_cow(true);
    config.memory_reservation(4 << 30);
    config.memory_guard_size(32 << 20);
    let n = 2000;
    let mut pool = PoolingAllocationConfig::new();
    pool.total_core_instances(n)
        .total_memories(n)
        .total_tables(n)
        .max_memory_size(64 << 20)
        .max_unused_warm_slots(n)
        .linear_memory_keep_resident(2 << 20)
        .table_keep_resident(2 << 20)
        .pagemap_scan(Enabled::Auto);
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    let engine = Engine::new(&config)?;

    let t = Instant::now();
    let module = Module::from_file(&engine, &path)?;
    emit("compile module (Cranelift)", t.elapsed().as_nanos() as f64 / 1e3, "us", 0);
    let pre = Linker::<()>::new(&engine).instantiate_pre(&module)?;

    let new_store = || {
        let mut store = Store::new(&engine, ());
        // Production-like: a deadline is armed; the epoch never advances here.
        store.set_epoch_deadline(u64::MAX / 2);
        store
    };

    // Compute, in one long-lived instance.
    let mut store = new_store();
    let instance = pre.instantiate(&mut store)?;
    for (name, func, arg) in [("fib(27)", "fib", 27i64), ("sieve(1M)", "sieve", 1 << 20), ("mix(100)", "mix", 100)] {
        let f = instance.get_typed_func::<i64, i64>(&mut store, func)?;
        let mut r = 0;
        let t = time_median(3, 15, || r = f.call(&mut store, arg).unwrap());
        emit(name, t / 1e3, "us", r);
    }
    let noop = instance.get_typed_func::<i64, i64>(&mut store, "noop")?;
    let mut x = 0;
    emit("call noop", time_avg(2_000_000, || x = noop.call(&mut store, x).unwrap()), "ns", x);

    // A fresh instance per request: instantiate from the pool, one call,
    // drop (the slot returns to the pool).
    for (kib, label, iters) in [
        (0, "per request: new sandbox + noop", 200_000),
        (64, "per request: new sandbox + touch 64 KiB", 200_000),
        (1024, "per request: new sandbox + touch 1 MiB", 20_000),
    ] {
        let t = time_avg(iters, || {
            let mut store = new_store();
            let instance = pre.instantiate(&mut store).unwrap();
            if kib == 0 {
                instance.get_typed_func::<i64, i64>(&mut store, "noop").unwrap().call(&mut store, 1).unwrap();
            } else {
                instance.get_typed_func::<i64, i64>(&mut store, "touch").unwrap().call(&mut store, kib).unwrap();
            }
        });
        emit(label, t / 1e3, "us", 0);
    }

    // Memory of live sandboxes: 1,000 instances, each having written 64 KiB.
    let before = rss_kib();
    let mut live = Vec::new();
    for _ in 0..1000 {
        let mut store = new_store();
        let instance = pre.instantiate(&mut store)?;
        instance.get_typed_func::<i64, i64>(&mut store, "touch")?.call(&mut store, 64)?;
        live.push(store);
    }
    emit("memory per live sandbox (64 KiB written)", (rss_kib() - before) as f64 / 1000.0, "KiB", 0);
    Ok(())
}
