#!/usr/bin/env bash
# Build the pinned stock API 35 x86_64 helper with Linux NDK r27d.
# Package the output UNCHANGED under lib/x86_64/ in the signed host APK;
# extractNativeLibs must be true. No writable helper installation or shell.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to NDK 27.3.13750724}"
[[ $# == 1 && $1 == /* ]] || { echo "usage: $0 ABSOLUTE_OUTPUT_DIRECTORY" >&2; exit 2; }
grep -qFx 'Pkg.Revision = 27.3.13750724' "$ANDROID_NDK_HOME/source.properties"
cc="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android35-clang"
mkdir -p -- "$1"
"$cc" -std=c11 -D_GNU_SOURCE -Os -Wall -Wextra -Werror -ffreestanding -fno-stack-protector \
    -nostdlib -static-pie -s -Wl,--build-id=none,-z,noexecstack,-z,defs \
    "$here/worker.c" -o "$1/libeon_pty_worker.so"
(cd -- "$1" && sha256sum -c "$here/worker.sha256")
