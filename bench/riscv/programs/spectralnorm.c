// Spectral norm (Benchmarks Game): floating point, nested loops, division.
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
static double A(int i, int j) { return 1.0 / ((i + j) * (i + j + 1) / 2 + i + 1); }
static void Av(int n, const double* v, double* out) {
    for (int i = 0; i < n; i++) { double s = 0; for (int j = 0; j < n; j++) s += A(i, j) * v[j]; out[i] = s; }
}
static void Atv(int n, const double* v, double* out) {
    for (int i = 0; i < n; i++) { double s = 0; for (int j = 0; j < n; j++) s += A(j, i) * v[j]; out[i] = s; }
}
static void AtAv(int n, const double* v, double* out, double* tmp) { Av(n, v, tmp); Atv(n, tmp, out); }
int main(int argc, char** argv) {
    int n = argc > 1 ? atoi(argv[1]) : 2000;
    double *u = malloc(n * sizeof(double)), *v = malloc(n * sizeof(double)), *tmp = malloc(n * sizeof(double));
    for (int i = 0; i < n; i++) u[i] = 1;
    for (int i = 0; i < 10; i++) { AtAv(n, u, v, tmp); AtAv(n, v, u, tmp); }
    double vBv = 0, vv = 0;
    for (int i = 0; i < n; i++) { vBv += u[i] * v[i]; vv += v[i] * v[i]; }
    printf("%0.9f\n", sqrt(vBv / vv));
    return 0;
}
