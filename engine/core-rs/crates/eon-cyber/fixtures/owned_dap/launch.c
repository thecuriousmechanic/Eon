/* Fixed local toolchain only. This guard takes no arguments and cannot be
 * used as a general command launcher. The filter is inherited across exec. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#if !defined(__x86_64__) || defined(__ILP32__)
#error "Only the native Linux x86-64 ABI is supported"
#endif

#define REJECT (SECCOMP_RET_ERRNO | EPERM)
#define DENY(n)                                                                \
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (n), 0, 1),                            \
        BPF_STMT(BPF_RET | BPF_K, REJECT)

static int guard(void) {
    struct sock_filter rules[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JGE | BPF_K, 0x40000000, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, REJECT),
        DENY(SYS_socket),
        DENY(SYS_bind),
        DENY(SYS_connect),
        DENY(SYS_listen),
        DENY(SYS_accept),
        DENY(SYS_accept4),
        DENY(SYS_io_uring_setup),
        /* LLDB may use anonymous local IPC between its own processes. */
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_socketpair, 0, 3),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS,
                 offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AF_UNIX, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, REJECT),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog filter = {
        .len = sizeof rules / sizeof rules[0],
        .filter = rules,
    };
    return prctl(PR_SET_NO_NEW_PRIVS, 1L, 0L, 0L, 0L) == 0 &&
           prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &filter) == 0;
}

static volatile sig_atomic_t cancelled;
static void cancel(int sig) {
    (void)sig;
    cancelled = 1;
}

/* lldb-server and its inferior may create separate process groups. Once the
 * adapter exits, a subreaper can clean up its own orphaned children without
 * searching host processes or signaling a recycled PID. Only this thread
 * reaps; a child read here retains its PID until our next waitpid. */
static int reap_descendants(void) {
    int status;
    for (;;) {
        pid_t child = waitpid(-1, &status, WNOHANG);
        if (child < 0 && errno != EINTR)
            return errno == ECHILD;
        if (child != 0)
            continue;
        FILE *children = fopen("/proc/thread-self/children", "re");
        if (!children)
            return 0;
        int pid;
        while (fscanf(children, "%d", &pid) == 1 && pid > 0)
            kill(pid, SIGKILL);
        fclose(children);
        usleep(10000);
    }
}

int main(int argc, char **argv) {
    (void)argv;
    struct rlimit zero = {0, 0}, cpu = {10, 10};
    sigset_t mask;
    sigemptyset(&mask);
    if (argc != 1 || close_range(3, ~0U, 0) != 0 ||
        setrlimit(RLIMIT_CORE, &zero) != 0 ||
        setrlimit(RLIMIT_CPU, &cpu) != 0 ||
        signal(SIGCHLD, SIG_DFL) == SIG_ERR ||
        signal(SIGTERM, cancel) == SIG_ERR ||
        signal(SIGALRM, cancel) == SIG_ERR ||
        sigprocmask(SIG_SETMASK, &mask, NULL) != 0 ||
        prctl(PR_SET_CHILD_SUBREAPER, 1L, 0L, 0L, 0L) != 0 ||
        prctl(PR_SET_PDEATHSIG, SIGTERM, 0L, 0L, 0L) != 0 ||
        access("/proc/thread-self/children", R_OK) != 0 || !guard())
        return 1;
    if (socket(AF_INET, SOCK_STREAM, 0) != -1 || errno != EPERM)
        return 1;
    alarm(15);
    pid_t worker = fork();
    if (worker < 0)
        return 1;
    if (!worker) {
#ifdef OWNED_CODELLDB
        /* Only the digest-checked executable in the private fixture directory. */
        execl("./codelldb", "codelldb", "--liblldb",
              "/usr/lib/llvm-18/lib/liblldb.so.1", (char *)0);
#else
        execl("/usr/lib/llvm-18/bin/lldb-dap", "lldb-dap", (char *)0);
#endif
        _exit(1);
    }
    close(STDIN_FILENO);
    close(STDOUT_FILENO);
    int status;
    for (;;) {
        pid_t result = waitpid(worker, &status, WNOHANG);
        if (result == worker)
            break;
        if (result == -1 && errno != EINTR)
            return 1;
        if (cancelled)
            kill(worker, SIGKILL);
        usleep(10000);
    }
    if (!reap_descendants())
        return 1;
    if (WIFSIGNALED(status))
        return cancelled && WTERMSIG(status) == SIGKILL ? 0 : 1;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
