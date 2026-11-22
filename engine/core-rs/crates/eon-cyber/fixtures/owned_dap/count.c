#include <stdlib.h>

static unsigned successful_allocations, completed_frees;

/* The only variables in this frame are three scalar integers. The block
 * itself stays in main; its contents are never inspected. */
static int count(int visit) {
    unsigned allocations = successful_allocations;
    unsigned frees = completed_frees;
    if (allocations != (unsigned)visit || frees + 1 != allocations)
        return 0;
    return visit + 1;
}

static int first(void) {
    return count(1);
}

static int second(void) {
    return count(2);
}

static int third(void) {
    return count(3);
}

static void *acquire(void) {
    void *block = malloc(8);
    if (!block)
        exit(1);
    ++successful_allocations;
    return block;
}

static void release(void *block) {
    free(block);
    ++completed_frees;
}

int main(void) {
    void *block = acquire();
    int a = first();
    release(block);
    block = acquire();
    int b = second();
    release(block);
    block = acquire();
    int c = third();
    release(block);
    return a + b + c == 9 && successful_allocations == 3 && completed_frees == 3 ? 0 : 1;
}
