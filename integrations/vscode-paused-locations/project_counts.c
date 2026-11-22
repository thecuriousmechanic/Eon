/* Link into an operator-owned debug build with --wrap for the four functions
 * below. No preload, debugger API, allocation sizes or object data collection. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#if !defined(__linux__) || !defined(__x86_64__) || defined(__ILP32__)
#error "The allocation record requires little-endian Linux x86-64"
#endif

struct record {
    char magic[8], nonce[32];
    uint64_t dev, ino;
    _Atomic unsigned long long counts[4];
};
_Static_assert(ATOMIC_LLONG_LOCK_FREE == 2 && offsetof(struct record, counts) == 56 &&
               sizeof(struct record) == 88, "unexpected allocation record ABI");
static _Atomic(struct record *) sink;
static pid_t owner;

static void enable(void) {
    const char *nonce = secure_getenv("EON_PROJECT_ALLOC");
    if (!nonce || strnlen(nonce, 33) != 32 || strspn(nonce, "0123456789abcdef") != 32)
        return;
    char name[] = ".eon-alloc-00000000000000000000000000000000";
    memcpy(name + sizeof(".eon-alloc-") - 1, nonce, 32);
    struct stat dir, exe, file;
    if (stat(".", &dir) || dir.st_uid != geteuid() || (dir.st_mode & 022) ||
        stat("/proc/self/exe", &exe)) return;
    int fd = open(name, O_RDWR | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) return;
    struct record *r = MAP_FAILED;
    if (!fstat(fd, &file) && S_ISREG(file.st_mode) && file.st_uid == geteuid() &&
        (file.st_mode & 07777) == 0600 && file.st_nlink == 1 && file.st_size == sizeof(*r)) {
        r = mmap(NULL, sizeof(*r), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    }
    close(fd);
    if (r == MAP_FAILED) return;
    if (memcmp(r->magic, "EONPEND1", 8) || memcmp(r->nonce, nonce, 32) ||
        r->dev != exe.st_dev || r->ino != exe.st_ino || unlink(name)) {
        munmap(r, sizeof(*r));
        return;
    }
    // The editor retains its original handle. No name remains for a second exec.
    owner = getpid();
    memcpy(r->magic, "EONLIVE1", 8);
    atomic_store(&sink, r);
}

__attribute__((constructor)) static void initialize_counts(void) {
    int saved = errno;
    enable();
    errno = saved;
}

static void increment(unsigned index) {
    struct record *r = atomic_load_explicit(&sink, memory_order_acquire);
    if (!r || owner != getpid()) return; // fork children must not contribute.
    _Atomic unsigned long long *counter = &r->counts[index];
    unsigned long long n = atomic_load_explicit(counter, memory_order_relaxed);
    // 2^53 is the permanent "not representable" sentinel, never wrap to zero.
    while (n < (1ULL << 53) && !atomic_compare_exchange_weak_explicit(
           counter, &n, n + 1, memory_order_relaxed, memory_order_relaxed)) {}
}

extern void *__real_malloc(size_t);
extern void *__real_calloc(size_t, size_t);
extern void *__real_realloc(void *, size_t);
extern void __real_free(void *);

void *__wrap_malloc(size_t n) {
    void *p = __real_malloc(n);
    if (p) increment(0);
    return p;
}
void *__wrap_calloc(size_t n, size_t size) {
    void *p = __real_calloc(n, size);
    if (p) increment(1);
    return p;
}
void *__wrap_realloc(void *p, size_t n) {
    void *result = __real_realloc(p, n);
    if (result && n) increment(2);
    return result;
}
void __wrap_free(void *p) {
    int counted = p != NULL;
    __real_free(p);
    if (counted) increment(3);
}
