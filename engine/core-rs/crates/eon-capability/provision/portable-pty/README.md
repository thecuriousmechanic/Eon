# Private upstream PTY allocator (x86_64 glibc Linux)

`portable-pty-allocator` selects crates.io `portable-pty = 0.9.0` for PTY
allocation. Its Unix `openpty` unlocks before returning and sets close-on-exec
after allocation. Never call it in the multithreaded daemon or on the ordinary
host devpts. Without the feature, the existing secured devpts backend remains
the default. With the feature, a bad grant is an error, not a fallback.

**Operator contract.** Start a new, recursively private mount namespace. Inside
it, mount a **new instance** of devpts at `/dev/pts`, with `mode=0600`, no `uid=`
or `gid=` override, and `/dev/ptmx` referring to *that instance's* `ptmx`. Do
not change the mounts or grant for its lifetime. Use a dedicated non-root UID
and GID, without supplementary groups or capabilities. The daemon needs no
namespace or mount privileges. Linux must support statx mount IDs, close_range,
TIOCGPTPEER, Landlock ABI v3 and seccomp filtering.

Package the fixed `eon-pty-allocator` ELF and its **trusted build digest** in a
namespace-local, root-owned installation. The grant directory is
`/usr/local/libexec/eon-pty/<mount-namespace-inode>-<devpts-st_dev>-<uid>-<gid>`.
All components must be nonsymlinks and not group/other writable. Files:

* `allocator`: the trusted, non-setid executable.
* `allocator.sha256`: 64 lowercase hex digits and LF; obtain from trusted
  build provenance, not by hashing an unverified installation at startup.

The daemon pins the verified ELF. A fresh exec gets null input/error, its one
private socket as stdout, an empty environment, and no other host descriptors.
The helper rechecks the grant, its own executable inode, and its single thread.
Before `native_pty_system().openpty(...)`, it restricts file access to the
verified instance and installs a syscall allowlist without fork, clone, exec or
socket creation. glibc's pathname-based slave fallback is denied. The helper
relocks the master and transfers only that descriptor. The daemon verifies the
fixed packet and kernel credentials before reaping the helper, obtains the
exact peer with `TIOCGPTPEER`, and uses the existing confined worker launch.

### Reproduce the Linux tests

From `engine/core-rs`, build as the ordinary user, **not** with sudo:

```sh
cargo build --locked -p eon-capability --features portable-pty-allocator --bin eon-pty-allocator
cargo test --locked -p eon-capability --features portable-pty-allocator --lib --no-run --message-format=json >target/pty-test.json
tests=$(jq -r 'select(.executable != null) | .executable' target/pty-test.json | tail -1)
helper=$PWD/target/debug/eon-pty-allocator
digest=$(sha256sum "$helper" | cut -d ' ' -f1)
install -d -m 0700 target/pty-test-home
```

The following is a **test provisioner only**, using system utilities. It
installs the locally built ELF solely in a namespace-private tmpfs, never on
the host. Neither the workspace ELF nor test code executes as root. Run this
only on a machine where ordinary sudo/mount namespace provisioning is allowed:

```sh
sudo unshare --mount --propagation private --fork /bin/sh -eu -c '
  PATH=/usr/sbin:/usr/bin:/sbin:/bin
  old=$(stat -Lc %d /dev/pts/ptmx)
  mount -t devpts -o newinstance,nosuid,noexec,mode=0600,ptmxmode=0666 devpts /dev/pts
  dev=$(stat -Lc %d /dev/pts/ptmx)
  [ "$dev" != "$old" ]
  if [ ! -L /dev/ptmx ]; then mount --bind /dev/pts/ptmx /dev/ptmx; fi
  mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /usr/local
  cd /usr/local
  grant=libexec/eon-pty/$(stat -Lc %i /proc/self/ns/mnt)-$dev-$1-$2
  install -D -m 0555 "$3" "$grant/allocator"
  printf "%s\n" "$4" >"$grant/allocator.sha256"
  cd "$6"
  exec setpriv --reuid="$1" --regid="$2" --clear-groups --inh-caps=-all \
    --ambient-caps=-all --bounding-set=-all --no-new-privs \
    /usr/bin/env -i HOME="$6/target/pty-test-home" "$5" pty:: workspace_tests process_tests
' provision "$(id -u)" "$(id -g)" "$helper" "$digest" "$tests" "$PWD"
```

For the separate negative fixture, use either `mode=0620` with the correct
digest, or `mode=0600` with a different 64-digit digest. Replace the last test
selectors with `--ignored --exact pty::native::portable::tests::bad_provisioning_is_refused_before_launch`.
The unprovisioned default regression suite is
`cargo test --locked -p eon-capability --lib -- pty:: workspace_tests process_tests`.
