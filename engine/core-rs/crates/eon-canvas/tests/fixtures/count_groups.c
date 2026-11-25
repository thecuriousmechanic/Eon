unsigned count_groups(unsigned n) {
    unsigned groups = 0;
    while (n > 1) {
        do { --n; } while (n % 3);
        ++groups;
    }
    return groups;
}
