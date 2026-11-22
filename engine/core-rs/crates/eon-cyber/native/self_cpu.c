// One private map, one fixed program, one non-inherited event. No paths or pins.
#define _GNU_SOURCE
#include <linux/bpf.h>
#include <linux/capability.h>
#include <linux/perf_event.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

static int allowed(void) {
    // Stock app seccomp may kill the caller rather than return EPERM. Never
    // attempt bpf/perf under a filter, nor ask the OS for additional privileges.
    if (prctl(PR_GET_SECCOMP, 0L, 0L, 0L, 0L) != 0)
        return 0;
    struct __user_cap_header_struct self = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = 0 };
    struct __user_cap_data_struct caps[2] = {0};
    if (syscall(__NR_capget, &self, caps) != 0)
        return 0;
    return (caps[CAP_BPF / 32].effective & (1u << (CAP_BPF % 32))) &&
           (caps[CAP_PERFMON / 32].effective & (1u << (CAP_PERFMON % 32)));
}

#define INSN(op, dst, src, off_, imm_) \
    { .code = (op), .dst_reg = (dst), .src_reg = (src), .off = (off_), .imm = (imm_) }

static int counter_program(int map) {
    // Discard the incoming perf context without reading it. key=0; if (slot)
    // atomic_add(slot, 1); return 0 (do not emit any perf sample records).
    const struct bpf_insn insns[] = {
        INSN(BPF_LD | BPF_DW | BPF_IMM, BPF_REG_1, BPF_PSEUDO_MAP_FD, 0, map),
        INSN(0, 0, 0, 0, 0),
        INSN(BPF_ST | BPF_W | BPF_MEM, BPF_REG_10, 0, -4, 0),
        INSN(BPF_ALU64 | BPF_MOV | BPF_X, BPF_REG_2, BPF_REG_10, 0, 0),
        INSN(BPF_ALU64 | BPF_ADD | BPF_K, BPF_REG_2, 0, 0, -4),
        INSN(BPF_JMP | BPF_CALL, 0, 0, 0, BPF_FUNC_map_lookup_elem),
        INSN(BPF_JMP | BPF_JEQ | BPF_K, BPF_REG_0, 0, 2, 0),
        INSN(BPF_ALU64 | BPF_MOV | BPF_K, BPF_REG_1, 0, 0, 1),
        INSN(BPF_STX | BPF_DW | BPF_XADD, BPF_REG_0, BPF_REG_1, 0, 0),
        INSN(BPF_ALU64 | BPF_MOV | BPF_K, BPF_REG_0, 0, 0, 0),
        INSN(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
    };
    const union bpf_attr attr = {
        .prog_type = BPF_PROG_TYPE_PERF_EVENT,
        .insn_cnt = sizeof(insns) / sizeof(insns[0]),
        .insns = (uintptr_t)insns,
        .license = (uintptr_t)"GPL",
    };
    return syscall(__NR_bpf, BPF_PROG_LOAD, &attr, sizeof(attr));
}

int eon_self_cpu_begin(int fds[2]) {
    if (!allowed())
        return -1;
    const struct perf_event_attr attr = {
        .type = PERF_TYPE_SOFTWARE, .size = sizeof(attr), .config = PERF_COUNT_SW_CPU_CLOCK,
        .sample_period = 1000000, // target 1 ms; not a duration measurement
        .disabled = 1, .inherit = 0, .exclude_kernel = 1, .exclude_hv = 1,
        .exclude_guest = 1, .remove_on_exec = 1,
    };
    // pid=0 is *this thread*, cpu=-1 follows it. There is no system-wide fallback.
    int event = syscall(__NR_perf_event_open, &attr, 0, -1, -1, PERF_FLAG_FD_CLOEXEC);
    if (event < 0)
        return -1;
    const union bpf_attr create = {
        .map_type = BPF_MAP_TYPE_ARRAY, .key_size = sizeof(uint32_t),
        .value_size = sizeof(uint64_t), .max_entries = 1, .map_flags = BPF_F_RDONLY,
    };
    int map = syscall(__NR_bpf, BPF_MAP_CREATE, &create, sizeof(create));
    if (map >= 0) {
        int prog = counter_program(map);
        if (prog >= 0) {
            int attached = ioctl(event, PERF_EVENT_IOC_SET_BPF, prog);
            close(prog); // On success the private perf event holds the reference.
            if (attached == 0 && ioctl(event, PERF_EVENT_IOC_ENABLE, 0) == 0) {
                fds[0] = event;
                fds[1] = map;
                return 0;
            }
        }
        close(map);
    }
    close(event);
    return -1;
}

int eon_self_cpu_disable(int event) {
    return ioctl(event, PERF_EVENT_IOC_DISABLE, 0);
}

int eon_self_cpu_read(int map, uint64_t *samples) {
    const uint32_t key = 0;
    const union bpf_attr attr = { .map_fd = map, .key = (uintptr_t)&key, .value = (uintptr_t)samples };
    return syscall(__NR_bpf, BPF_MAP_LOOKUP_ELEM, &attr, sizeof(attr));
}
