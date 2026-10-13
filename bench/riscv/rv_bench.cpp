// libriscv side of the sandbox comparison, plus the native baseline.
//
// Built twice (see Dockerfile):
//   rv-bench-flat   flat memory arena (libriscv's default and fastest mode):
//                   compute, call latency, a new machine per request
//   rv-bench-paged  virtual paging: copy-on-write forks of a booted template,
//                   libriscv's mode for per-request isolation
//
// Usage: rv-bench guest.elf interp|bintr
// Prints one JSON object per line.
#include <libriscv/machine.hpp>
#include <algorithm>
#include <chrono>
#include <cstdio>
#include <fstream>
#include <functional>
#include <string>
#include <vector>
#include <cstdlib>

extern "C" {
int64_t native_fib(int64_t);
int64_t native_sieve(int64_t);
int64_t native_mix(int64_t);
int64_t native_noop(int64_t);
int64_t native_touch(int64_t);
}

using Machine = riscv::Machine<riscv::RISCV64>;
using Options = riscv::MachineOptions<riscv::RISCV64>;
// Production-like: every call has an instruction budget.
static constexpr uint64_t MAX_INSN = 50'000'000'000ull;

static double now_ns() {
    return std::chrono::duration<double, std::nano>(std::chrono::steady_clock::now().time_since_epoch()).count();
}

static double median(std::vector<double> v) {
    std::sort(v.begin(), v.end());
    return v[v.size() / 2];
}

/// Median time of one call to `f`, over `runs` runs after `warmup`.
static double time_median(int warmup, int runs, const std::function<void()>& f) {
    for (int i = 0; i < warmup; i++) f();
    std::vector<double> t;
    for (int i = 0; i < runs; i++) {
        double t0 = now_ns();
        f();
        t.push_back(now_ns() - t0);
    }
    return median(t);
}

/// Average time per iteration over `n` iterations.
static double time_avg(long n, const std::function<void()>& f) {
    double t0 = now_ns();
    for (long i = 0; i < n; i++) f();
    return (now_ns() - t0) / n;
}

static long rss_kib() {
    std::ifstream f("/proc/self/status");
    std::string line;
    while (std::getline(f, line))
        if (line.rfind("VmRSS:", 0) == 0) return std::stol(line.substr(6));
    return 0;
}

static void emit(const char* engine, const char* test, double value, const char* unit, int64_t check = 0) {
    printf("{\"engine\": \"%s\", \"test\": \"%s\", \"value\": %.3f, \"unit\": \"%s\", \"check\": %lld}\n",
           engine, test, value, unit, (long long)check);
    fflush(stdout);
}

struct Workload {
    const char* name;
    int64_t arg;
    int64_t (*native)(int64_t);
};
static const Workload WORKLOADS[] = {
    {"fib(27)", 27, native_fib},
    {"sieve(1M)", 1 << 20, native_sieve},
    {"mix(100)", 100, native_mix},
};

int main(int argc, char** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s guest.elf interp|bintr [native]\n", argv[0]);
        return 1;
    }
    std::ifstream file(argv[1], std::ios::binary);
    std::string binary((std::istreambuf_iterator<char>(file)), std::istreambuf_iterator<char>());
    const bool bintr = std::string(argv[2]) == "bintr";
#ifdef PAGED
    const std::string engine = std::string("libriscv-fork-") + argv[2];
#else
    const std::string engine = std::string("libriscv-") + argv[2];
#endif
    const char* e = engine.c_str();

    if (argc > 3) {
        // Native baseline: the same C, compiled for the host.
        for (auto& w : WORKLOADS) {
            int64_t r = 0;
            double t = time_median(3, 15, [&] { r = w.native(w.arg); });
            emit("native", w.name, t / 1e3, "us", r);
        }
        int64_t x = 0;
        emit("native", "call noop", time_avg(10'000'000, [&] { x = native_noop(x); }), "ns", x);
        return 0;
    }

    Options opts{
        .memory_max = 64ull << 20,
        .stack_size = 1u << 20,
        .default_exit_function = "fast_exit",
        .translate_enabled = bintr,
        .translate_invoke_compiler = bintr,
        .translation_cache = true,
    };
    auto boot = [&](Machine& m) {
        m.setup_linux_syscalls(false, false);
        m.setup_linux({"guest"}, {"LC_ALL=C"});
        m.simulate(100'000'000ull);  // main() stops at fast_exit
    };

    double t0 = now_ns();
#ifdef PAGED
    // Variants for debugging the fork path.
    if (getenv("MASTER_ARENA")) opts.use_memory_arena = atoi(getenv("MASTER_ARENA"));
#endif
    Machine master{binary, opts};
    boot(master);
    emit(e, "load + boot (first, incl. translation)", (now_ns() - t0) / 1e3, "us");

    const auto a_noop = master.address_of("noop");
    const auto a_touch = master.address_of("touch");

    // Compute, in the booted machine.
    for (auto& w : WORKLOADS) {
        const auto addr = master.address_of(std::string_view(w.name, std::string_view(w.name).find('(')));
        int64_t r = 0;
        double t = time_median(3, 15, [&] { r = master.vmcall<MAX_INSN>(addr, w.arg); });
        emit(e, w.name, t / 1e3, "us", r);
    }
    int64_t x = 0;
    emit(e, "call noop", time_avg(2'000'000, [&] { x = master.vmcall<MAX_INSN>(a_noop, x); }), "ns", x);

#ifdef PAGED
    // A fresh sandbox per request: CoW fork of the booted template, one call,
    // destroy. This is what libriscv recommends for per-request isolation.
    // A full copy-on-write fork of the booted template. libriscv's C API
    // (libriscv_fast_fork) sets minimal_fork, which skips lending the
    // template's pages: the fork then runs without the program's initialized
    // memory, and in our tests the host process crashed with a segfault once
    // the fork wrote memory. MINIMAL_FORK=1 reproduces it.
    Options fork_opts{
        .memory_max = 64ull << 20,
        .minimal_fork = false,
        .use_memory_arena = false,
        .default_exit_function = "fast_exit",
        .translate_enabled = bintr,
        .translation_use_arena = false,
    };
    if (getenv("MINIMAL_FORK")) fork_opts.minimal_fork = atoi(getenv("MINIMAL_FORK"));
    for (int64_t kib : {0, 64, 1024}) {
        double t = time_avg(kib == 1024 ? 5'000 : 100'000, [&] {
            Machine fork{master, fork_opts};
            if (kib == 0) fork.vmcall<MAX_INSN>(a_noop, 1);
            else fork.vmcall<MAX_INSN>(a_touch, kib);
        });
        emit(e, kib == 0 ? "per request: new sandbox + noop" :
                (kib == 64 ? "per request: new sandbox + touch 64 KiB" : "per request: new sandbox + touch 1 MiB"),
             t / 1e3, "us");
    }
    // Isolation check: a fork's writes must not reach the template.
    {
        const auto a_peek = master.address_of("peek");
        const int64_t before = master.vmcall<MAX_INSN>(a_peek, 64);
        {
            Machine fork{master, fork_opts};
            fork.vmcall<MAX_INSN>(a_touch, 64);
            emit(e, "fork sees its own write (expect 64)", (double)fork.vmcall<MAX_INSN>(a_peek, 64), "byte");
        }
        emit(e, "template byte after fork wrote (expect unchanged)", (double)master.vmcall<MAX_INSN>(a_peek, 64), "byte", before);
    }
    // Memory of live sandboxes: 1,000 forks, each having written 64 KiB.
    {
        long before = rss_kib();
        std::vector<std::unique_ptr<Machine>> forks;
        for (int i = 0; i < 1000; i++) {
            forks.push_back(std::make_unique<Machine>(master, fork_opts));
            forks.back()->vmcall<MAX_INSN>(a_touch, 64);
        }
        emit(e, "memory per live sandbox (64 KiB written)", double(rss_kib() - before) / 1000, "KiB");
    }
#else
    // Without paging there is no fork: a new machine per request means
    // loading the ELF and running the C runtime's start-up.
    {
        Options per_req = opts;
        per_req.translate_invoke_compiler = false;  // reuse the cached translation
        double t = time_avg(2'000, [&] {
            Machine m{binary, per_req};
            boot(m);
            m.vmcall<MAX_INSN>(a_noop, 1);
        });
        emit(e, "per request: new sandbox + noop", t / 1e3, "us");
    }
#endif
    return 0;
}
