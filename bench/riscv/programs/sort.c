// libc qsort of random integers: a callback per comparison, memory moves.
#include <stdio.h>
#include <stdlib.h>
static int cmp(const void* a, const void* b) { int x = *(const int*)a, y = *(const int*)b; return (x > y) - (x < y); }
int main(int argc, char** argv) {
    int n = argc > 1 ? atoi(argv[1]) : 5000000;
    int* v = malloc(n * sizeof(int));
    unsigned x = 42;
    for (int i = 0; i < n; i++) { x = x * 1664525 + 1013904223; v[i] = (int)(x >> 1); }
    qsort(v, n, sizeof(int), cmp);
    long sum = 0;
    for (int i = 0; i < n; i += 1000) sum += v[i] % 1000;
    printf("%d %d %ld\n", v[0], v[n - 1], sum);
    return 0;
}
