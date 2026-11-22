/* Ordinary project code. No reporter API, counters, hard-coded reports, or
 * dependency on EON. The test build links the optional hook at the final link. */
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static void checkpoint(void) {
    volatile int stopped = 1; /* source breakpoint */
    (void)stopped;
}

int main(void) {
    void *a = malloc(17), *b = calloc(2, 13);
    if (!a || !b) return 1;
    checkpoint();
    void *c = realloc(a, 41);
    if (!c) return 2;
    free(c);
    free(b);
    free(NULL);
    // A separately running fork child must not contribute its own operations.
    pid_t child = fork();
    if (!child) {
        void *p = malloc(7);
        if (!p) _exit(3);
        free(p);
        _exit(0);
    }
    int status;
    if (child < 0 || waitpid(child, &status, 0) != child ||
        !WIFEXITED(status) || WEXITSTATUS(status)) return 4;
    checkpoint();
    return 0;
}
