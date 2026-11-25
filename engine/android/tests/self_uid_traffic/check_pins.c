// Host-operated provenance check for the disposable userdebug AVD, never part of either APK.
// Only ask for object metadata and cgroup attachment IDs. No map keys or values are accessed.
#include <fcntl.h>
#include <linux/bpf.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

#define NETD "/sys/fs/bpf/netd_shared/"

static int bpf(enum bpf_cmd cmd, union bpf_attr *attr) {
    int result = syscall(__NR_bpf, cmd, attr, sizeof(*attr));
    if (result < 0) perror("BPF metadata unavailable");
    return result;
}

static int pinned(const char *path, uint32_t flags) {
    union bpf_attr attr = {.pathname = (uintptr_t)path, .file_flags = flags};
    return bpf(BPF_OBJ_GET, &attr);
}

static bool info(int fd, void *data, uint32_t size) {
    union bpf_attr attr = {.info = {.bpf_fd = fd, .info = (uintptr_t)data, .info_len = size}};
    return fd >= 0 && bpf(BPF_OBJ_GET_INFO_BY_FD, &attr) == 0;
}

static bool contains(uint32_t needle, const uint32_t *ids, size_t count) {
    for (size_t i = 0; i < count; i++) if (ids[i] == needle) return true;
    return false;
}

static bool attached(int cgroup, enum bpf_attach_type direction, const char *path, uint32_t map_id) {
    uint32_t ids[64] = {};
    const uint32_t capacity = sizeof(ids) / sizeof(ids[0]);
    struct bpf_prog_info p = {.map_ids = (uintptr_t)ids, .nr_map_ids = capacity};
    int fd = pinned(path, 0);
    if (!info(fd, &p, sizeof(p)) || p.type != BPF_PROG_TYPE_CGROUP_SKB || p.nr_map_ids > capacity
            || !contains(map_id, ids, p.nr_map_ids)) return false;
    union bpf_attr attr = {.query = {.target_fd = cgroup, .attach_type = direction,
            .prog_ids = (uintptr_t)ids, .prog_cnt = capacity}};
    return bpf(BPF_PROG_QUERY, &attr) == 0 && attr.query.prog_cnt <= capacity
        && contains(p.id, ids, attr.query.prog_cnt);
}

int main(void) {
    int cg = open("/sys/fs/cgroup", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    int fd = pinned(NETD "map_netd_app_uid_stats_map", BPF_F_RDONLY);
    struct bpf_map_info map = {};
    if (cg < 0 || !info(fd, &map, sizeof(map)) || map.type != BPF_MAP_TYPE_HASH
            || map.key_size != 4 || map.value_size != 32
            || !attached(cg, BPF_CGROUP_INET_INGRESS, NETD "prog_netd_cgroupskb_ingress_stats", map.id)
            || !attached(cg, BPF_CGROUP_INET_EGRESS, NETD "prog_netd_cgroupskb_egress_stats", map.id)) {
        fputs("UID accounting map / attached programs not verified\n", stderr);
        return 1;
    }
    puts("PASS UID hash map referenced by both live cgroup ingress/egress programs");
    return 0;
}
