//! Engine configuration shared by `compile` and `serve`.
//!
//! Precompiled `.cwasm` artifacts are only loadable by an engine with a
//! compatible configuration, so both commands must build the engine here.

use wasmtime::Result;
use wasmtime::{Config, Enabled, Engine, InstanceAllocationStrategy, OptLevel, PoolingAllocationConfig};

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Allocator {
    /// Pre-reserved slots, reused across requests.
    Pooling,
    /// A fresh `mmap` for every instance.
    OnDemand,
}

pub struct EngineOpts {
    pub allocator: Allocator,
    /// Maximum number of concurrently live instances (pool slots).
    pub pool_size: u32,
    /// Maximum linear memory per instance, in bytes.
    pub max_memory: usize,
    /// Linux only: bytes of each linear memory to keep resident and reset
    /// with `memset` (guided by `PAGEMAP_SCAN`) instead of `madvise`. Zero
    /// keeps Wasmtime's default of returning pages to the kernel.
    pub keep_resident: usize,
}

impl Default for EngineOpts {
    fn default() -> Self {
        Self { allocator: Allocator::Pooling, pool_size: 1000, max_memory: 64 << 20, keep_resident: 2 << 20 }
    }
}

pub fn build(opts: &EngineOpts) -> Result<Engine> {
    let mut config = Config::new();
    config.cranelift_opt_level(OptLevel::Speed);

    // Cheap preemption: guest code only checks an epoch counter at function
    // entries and loop back-edges; a background thread bumps the counter.
    config.epoch_interruption(true);

    // Map the module's initial memory image copy-on-write instead of copying
    // data segments into every new instance.
    config.memory_init_cow(true);

    // Reserve a full 4 GiB for each 32-bit memory plus a guard region, so the
    // generated code needs no explicit bounds checks: out-of-bounds accesses
    // fault in hardware and become traps.
    config.memory_reservation(4 << 30);
    config.memory_guard_size(32 << 20);

    // Pooling allocator: all memories, tables and stacks are reserved once at
    // startup, so instantiation is "pop a slot off a free list".
    let n = opts.pool_size;
    let mut pool = PoolingAllocationConfig::new();
    pool.total_component_instances(n)
        // A Rust wasm32-wasip2 component contains the main module plus a few
        // small adapter/shim modules.
        .total_core_instances(n * 4)
        .total_memories(n * 2)
        .total_tables(n * 4)
        .total_stacks(n)
        .max_memory_size(opts.max_memory)
        .max_component_instance_size(1 << 20)
        .max_core_instance_size(1 << 20)
        // Keep released slots warm so the next instance of the same component
        // reuses a slot that already has its memory image mapped.
        .max_unused_warm_slots(n);
    if opts.keep_resident > 0 {
        // Reused slots keep their pages: no page faults on the next request,
        // and only pages the kernel reports as touched get reset.
        pool.linear_memory_keep_resident(opts.keep_resident)
            .table_keep_resident(opts.keep_resident)
            .pagemap_scan(Enabled::Auto);
    }
    if opts.allocator == Allocator::Pooling {
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    }

    Engine::new(&config)
}
