// Open-addressing hash map, 64 MiB table: random memory access, cache misses.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char** argv) {
    int ops = argc > 1 ? atoi(argv[1]) : 20000000;
    const uint64_t cap = 1 << 22;  // 4M slots x 16 bytes
    uint64_t* keys = calloc(cap, 8);
    uint64_t* vals = calloc(cap, 8);
    uint64_t x = 88172645463325252ull, found = 0;
    for (int i = 0; i < ops; i++) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        uint64_t k = (x % (cap / 2)) + 1, h = (k * 0x9e3779b97f4a7c15ull) >> 42;
        while (keys[h] && keys[h] != k) h = (h + 1) & (cap - 1);
        if (keys[h] == k) { vals[h]++; found++; } else { keys[h] = k; vals[h] = 1; }
    }
    printf("%llu\n", (unsigned long long)found);
    return 0;
}
