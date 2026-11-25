/* Fixed APK companion, not a command dispatcher. No input before confinement. */
#include <errno.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

/* No loader, constructors, allocator, or Bionic startup (which would read
   system properties and reserve several GiB before main). x86_64 Linux UAPI. */
static long call(long nr, long a, long b, long c) {
    register long d __asm__("r10") = 0;
    register long e __asm__("r8") = 0;
    long out;
    __asm__ volatile("syscall" : "=a"(out) : "a"(nr), "D"(a), "S"(b),
                     "d"(c), "r"(d), "r"(e) : "rcx", "r11", "memory");
    return out;
}

__attribute__((noreturn)) static void finish(int status) {
    call(__NR_exit_group, status, 0, 0);
    __builtin_trap();
}

static int equal(const char *a, const char *b) {
    while (*a && *a == *b) { ++a; ++b; }
    return *a == *b;
}

static int confine(void) {
    /* Only read(0), write(1), and exit. No ioctl (including TIOCSTI), open,
       exec, process creation, networking, or descriptor duplication. */
    static const struct sock_filter code[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_exit_group, 7, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_exit, 6, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_read, 0, 2),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, STDIN_FILENO, 3, 4),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_write, 0, 3),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, STDOUT_FILENO, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
    };
    /* Construct the pointer on the stack: this PIE has no relocation loader. */
    volatile struct sock_fprog filter;
    filter.len = sizeof(code) / sizeof(code[0]);
    filter.filter = (struct sock_filter *)code;
    return call(__NR_close_range, 3, UINT32_MAX, 0) == 0 &&
           call(__NR_prctl, PR_GET_NO_NEW_PRIVS, 0, 0) == 1 &&
           call(__NR_prctl, PR_SET_SECCOMP, SECCOMP_MODE_FILTER, (long)&filter) == 0;
}

/* The kernel supplies argc followed by argv pointers on the initial stack. */
__asm__(".text\n.global _start\n_start:\nmov %rsp, %rdi\nand $-16, %rsp\ncall stream\nud2");
__attribute__((noreturn,used)) static void stream(const uintptr_t *stack) {
    enum { COPY, UPPER, SQUEEZE } mode;
    if (!confine() || stack[0] != 2) finish(1);
    const char *arg = (const char *)stack[2];
    if (equal(arg, "copy")) mode = COPY;
    else if (equal(arg, "upper")) mode = UPPER;
    else if (equal(arg, "squeeze")) mode = SQUEEZE;
    else finish(1);

    uint8_t data[4096];
    int last_space = 0;
    for (;;) {
        long n = call(__NR_read, STDIN_FILENO, (long)data, sizeof(data));
        if (n == -EINTR) continue;
        if (n <= 0) finish(n == 0 ? 0 : 1);
        size_t len = 0;
        for (ssize_t i = 0; i < n; ++i) {
            uint8_t b = data[i];
            if (mode == SQUEEZE && last_space && b == ' ') continue;
            last_space = b == ' ';
            if (mode == UPPER && b >= 'a' && b <= 'z') b -= 'a' - 'A';
            data[len++] = b;
        }
        for (size_t sent = 0; sent < len;) {
            long w = call(__NR_write, STDOUT_FILENO, (long)(data + sent), (long)(len - sent));
            if (w == -EINTR) continue;
            if (w <= 0) finish(1);
            sent += (size_t)w;
        }
    }
}
