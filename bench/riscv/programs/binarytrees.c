// Binary trees (Benchmarks Game): malloc/free of many small objects.
#include <stdio.h>
#include <stdlib.h>
typedef struct Node { struct Node *l, *r; } Node;
static Node* make(int d) {
    Node* n = malloc(sizeof(Node));
    if (d > 0) { n->l = make(d - 1); n->r = make(d - 1); } else n->l = n->r = NULL;
    return n;
}
static int check(Node* n) { return 1 + (n->l ? check(n->l) + check(n->r) : 0); }
static void freet(Node* n) { if (n->l) { freet(n->l); freet(n->r); } free(n); }
int main(int argc, char** argv) {
    int maxd = argc > 1 ? atoi(argv[1]) : 18, mind = 4;
    if (maxd < mind + 2) maxd = mind + 2;
    Node* s = make(maxd + 1);
    printf("stretch tree of depth %d\t check: %d\n", maxd + 1, check(s));
    freet(s);
    Node* longlived = make(maxd);
    for (int d = mind; d <= maxd; d += 2) {
        int it = 1 << (maxd - d + mind), c = 0;
        for (int i = 0; i < it; i++) { Node* t = make(d); c += check(t); freet(t); }
        printf("%d\t trees of depth %d\t check: %d\n", it, d, c);
    }
    printf("long lived tree of depth %d\t check: %d\n", maxd, check(longlived));
    return 0;
}
