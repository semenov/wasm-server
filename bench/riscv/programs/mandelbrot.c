// Mandelbrot (Benchmarks Game, scalar): floating point in a tight loop.
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char** argv) {
    int n = argc > 1 ? atoi(argv[1]) : 4000;
    unsigned long sum = 0;
    for (int y = 0; y < n; y++) {
        double ci = 2.0 * y / n - 1.0;
        for (int x = 0; x < n; x++) {
            double cr = 2.0 * x / n - 1.5, zr = 0, zi = 0, tr = 0, ti = 0;
            int i = 0;
            for (; i < 50 && tr + ti <= 4.0; i++) { zi = 2 * zr * zi + ci; zr = tr - ti + cr; tr = zr * zr; ti = zi * zi; }
            sum += i;
        }
    }
    printf("%lu\n", sum);
    return 0;
}
