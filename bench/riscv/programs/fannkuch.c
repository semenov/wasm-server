// Fannkuch-redux (Benchmarks Game): integer work on small arrays, branches.
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char** argv) {
    int n = argc > 1 ? atoi(argv[1]) : 11;
    int perm[16], perm1[16], count[16], maxflips = 0, checksum = 0, r = n, permcount = 0;
    for (int i = 0; i < n; i++) perm1[i] = i;
    for (;;) {
        for (; r != 1; r--) count[r - 1] = r;
        for (int i = 0; i < n; i++) perm[i] = perm1[i];
        int flips = 0, k;
        while ((k = perm[0]) != 0) {
            for (int i = 0, j = k; i < j; i++, j--) { int t = perm[i]; perm[i] = perm[j]; perm[j] = t; }
            flips++;
        }
        if (flips > maxflips) maxflips = flips;
        checksum += permcount % 2 == 0 ? flips : -flips;
        for (;;) {
            if (r == n) { printf("%d\nPfannkuchen(%d) = %d\n", checksum, n, maxflips); return 0; }
            int p0 = perm1[0];
            for (int i = 0; i < r; i++) perm1[i] = perm1[i + 1];
            perm1[r] = p0;
            if (--count[r] > 0) break;
            r++;
        }
        permcount++;
    }
}
