#include <stdio.h>
#define MAX 10

/* A point */
struct point {
    int x;
    int y;
};

static int add(int a, int b) {
    return a + b;
}

int main(void) {
    struct point p = { 1, 2 };
    char *name = "world";
    printf("Hello %s %d\n", name, add(p.x, MAX));
    return 0;
}
