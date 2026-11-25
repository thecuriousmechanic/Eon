// Only attached using a process-bound BPF_TRACE_UPROBE_MULTI link.
// No registers, arguments, return values, names or event stream.
#define SEC(name) __attribute__((section(name), used))
typedef unsigned int u32;
typedef unsigned long long u64;

struct {
    int (*type)[2];         // BPF_MAP_TYPE_ARRAY
    int (*max_entries)[1];
    u32 *key;
    u64 *value;
} FSYNC_CALLS SEC(".maps");

static void *(*const lookup)(void *, const void *) = (void *)1;

SEC("uprobe.multi")
int self_fsync(void *unused)
{
    u32 zero = 0;
    u64 *count = lookup(&FSYNC_CALLS, &zero);
    if (count)
        __sync_fetch_and_add(count, 1);
    return 0;
}

char LICENSE[] SEC("license") = "GPL";
