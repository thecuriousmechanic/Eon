#!/bin/sh
# Internal test stage only; entered by desktop.js after namespace creation.
set -eu
test "$$" = 1 && test "$(id -u)" = 0 || exit 1
case "$1" in "$PWD"/target/paused-project-*) ;; *) exit 1;; esac
test "$2" = "$PWD/target/paused-ui-tools"
mount --make-rprivate /
root="$1/root"
mkdir -p "$root"
mount -t tmpfs -o mode=755,size=128m tmpfs "$root"
cd "$root"
mkdir -p usr bin lib lib64 etc/fonts etc/alternatives dev/shm dev/pts proc home/ui opt/code opt/drivers old
ro() {
    mount --bind "$1" "$2"
    mount -o remount,bind,ro,nosuid,nodev "$2"
}
ro /usr usr
rmdir bin lib lib64
ln -s usr/bin bin; ln -s usr/lib lib; ln -s usr/lib64 lib64
ro /etc/fonts etc/fonts
ro /etc/alternatives etc/alternatives
for f in ld.so.cache passwd group os-release; do touch "etc/$f"; ro "/etc/$f" "etc/$f"; done
for f in null zero random urandom; do touch "dev/$f"; mount --bind "/dev/$f" "dev/$f"; done
mount -t devpts -o newinstance,ptmxmode=0666,mode=0620 devpts dev/pts
ln -s pts/ptmx dev/ptmx
ln -s /proc/self/fd dev/fd
mount -t tmpfs -o mode=1777,size=256m tmpfs dev/shm
mount --bind /proc proc
ro "$2/VSCode-linux-x64" opt/code
ro "$2/unpacked" opt/drivers
mount --bind "$1/home" home/ui
pivot_root . old
cd /
umount -l /old
rmdir /old
exec /usr/bin/unshare --user --map-user=1001 --map-group=1001 \
    /usr/bin/setpriv --no-new-privs --inh-caps=-all --ambient-caps=-all \
    /usr/bin/env -i HOME=/home/ui USER=runner LOGNAME=runner PATH=/opt/drivers/usr/bin:/usr/bin \
    XDG_RUNTIME_DIR=/home/ui/runtime TMPDIR=/home/ui/runtime XDG_SESSION_TYPE=wayland \
    XDG_CURRENT_DESKTOP=sway WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    LD_LIBRARY_PATH=/opt/drivers/usr/lib/x86_64-linux-gnu TESSDATA_PREFIX=/opt/drivers/usr/share/tesseract-ocr/5/tessdata \
    LLDB_DEBUGSERVER_PATH=/usr/lib/llvm-18/bin/lldb-server LC_ALL=C.UTF-8 \
    /usr/bin/python3 -IB /home/ui/desktop_ui.py
