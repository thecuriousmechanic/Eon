#!/usr/bin/env bash
# Offline operator provisioning; no project input, dependency resolver, shell
# at runtime, or automatic approval. Destination must be new in a trusted parent.
set -euo pipefail
umask 077
[[ $# == 3 ]] || { echo "usage: $0 TEMURIN_17_JDK KOTLIN_2.4.21_HOME NEW_INSTALL" >&2; exit 2; }
jdk=$(realpath -e -- "$1"); kotlin=$(realpath -e -- "$2"); dest=$(realpath -m -- "$3")
here=$(dirname -- "$(realpath -e -- "$0")")/kotlin
[[ $(cat "$kotlin/build.txt") == 2.4.21 ]]
grep -Fxq 'IMPLEMENTOR_VERSION="Temurin-17.0.21+9"' "$jdk/release"
mkdir -m700 -- "$dest"
cd "$dest"
mkdir -m700 build build/classes
export TMPDIR="$dest/build"
export JAVA_TOOL_OPTIONS="-XX:-UsePerfData -Djava.io.tmpdir=$TMPDIR"
while read -r _ name; do install -m400 -- "$kotlin/lib/$name" "$name"; done < "$here/inputs"
sha256sum --check --strict "$here/inputs" >&2
"$jdk/bin/javac" -proc:none --release 17 -classpath 'kotlin-compiler.jar:kotlin-stdlib.jar' \
    -d build/classes "$here/"*.java
"$jdk/bin/jar" --create --file worker.jar --date 2026-01-01T00:00:00Z -C build/classes .
"$jdk/bin/jlink" --add-modules java.base,java.desktop,java.management,java.logging,jdk.unsupported \
    --strip-debug --no-header-files --no-man-pages --strip-native-commands --output build/jre
mkdir -p jre/lib/server
files=(kotlin-language-server worker.jar kotlin-compiler.jar kotlin-stdlib.jar kotlin-reflect.jar
    kotlinx-coroutines-core-jvm.jar jre/lib/modules jre/lib/server/libjvm.so
    jre/lib/libjava.so jre/lib/libjimage.so jre/lib/libzip.so jre/lib/libnet.so jre/lib/libnio.so jre/lib/libverify.so)
for f in "${files[@]:6}"; do install -m400 -- "build/$f" "$f"; done
cc -std=c11 -O2 -Wall -Wextra -Werror -I"$jdk/include" -I"$jdk/include/linux" "$here/launcher.c" \
    -ldl -pthread -Wl,-z,relro,-z,now -o kotlin-language-server
cp -R -- "$kotlin/license" kotlin-license
cp -R -- build/jre/legal jre/legal
rm -r -- build
chmod 500 kotlin-language-server
chmod 400 worker.jar
# SHA-256 of the exact list of image digests, in profile order. No manifest
# obtained from the target installation is trusted at launch.
sha256sum -- "${files[@]}" | sha256sum
