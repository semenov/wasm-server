// Guest code for comparing sandboxes: the same functions built natively,
// for wasm32 (wasmtime) and for RISC-V rv64gc (libriscv).
//
//   fib(n)        recursive calls
//   sieve(n)      primes below n: memory reads and writes (1 MiB buffer)
//   mix(rounds)   integer arithmetic over a 64 KiB buffer
//   noop(x)       call overhead
//   touch(kib)    writes KiB of memory, like a request that builds a response
//   peek(i)       reads back a byte written by touch (isolation checks)
#include <stdint.h>

#ifdef __wasm__
#define EXPORT(name) __attribute__((export_name(#name)))
#else
#define EXPORT(name) __attribute__((used, noinline))
#endif

#ifdef NATIVE
#define G(name) native_##name
#else
#define G(name) name
#endif

static uint8_t sieve_buf[1 << 20];
static uint64_t mix_buf[8192];
static uint8_t touch_buf[1 << 20];

EXPORT(fib) int64_t G(fib)(int64_t n) {
    return n < 2 ? n : G(fib)(n - 1) + G(fib)(n - 2);
}

EXPORT(sieve) int64_t G(sieve)(int64_t n) {
    if (n > (int64_t)sizeof(sieve_buf)) n = sizeof(sieve_buf);
    for (int64_t i = 0; i < n; i++) sieve_buf[i] = 1;
    int64_t count = 0;
    for (int64_t i = 2; i < n; i++) {
        if (!sieve_buf[i]) continue;
        count++;
        for (int64_t j = i * i; j < n; j += i) sieve_buf[j] = 0;
    }
    return count;
}

EXPORT(mix) int64_t G(mix)(int64_t rounds) {
    uint64_t x = 0x9e3779b97f4a7c15ull;
    for (int64_t r = 0; r < rounds; r++) {
        for (int i = 0; i < 8192; i++) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            mix_buf[i] = mix_buf[i] * 0x100000001b3ull + x;
        }
    }
    uint64_t sum = 0;
    for (int i = 0; i < 8192; i++) sum += mix_buf[i];
    return (int64_t)sum;
}

EXPORT(noop) int64_t G(noop)(int64_t x) {
    return x + 1;
}

EXPORT(touch) int64_t G(touch)(int64_t kib) {
    if (kib > (int64_t)(sizeof(touch_buf) >> 10)) kib = sizeof(touch_buf) >> 10;
    int64_t sum = 0;
    for (int64_t i = 0; i < (kib << 10); i += 64) {
        touch_buf[i] = (uint8_t)i;
        sum += touch_buf[i];
    }
    return sum;
}

EXPORT(peek) int64_t G(peek)(int64_t i) {
    return touch_buf[i];
}

#if defined(__riscv)
// libriscv: main() runs once (in the template machine) and stops at
// fast_exit, which is also where every vmcall returns to.
__attribute__((noreturn, used)) void fast_exit(int code) {
    register long a0 asm("a0") = code;
    asm volatile("r%=: wfi \nj r%=\n" :: "r"(a0));
    __builtin_unreachable();
}

int main(void) {
    fast_exit(0);
}
#endif
