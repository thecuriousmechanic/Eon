#!/usr/bin/env bash
# Offline. Only the two pinned official archives, an operator JDK and our overlay.
set -euo pipefail
umask 077
[[ $# == 4 ]] || { echo "usage: $0 TEMURIN_17_JDK SERVER_ZIP SOURCE_ZIP NEW_INSTALL" >&2; exit 2; }
jdk=$(realpath -e -- "$1"); release=$(realpath -e -- "$2"); src=$(realpath -e -- "$3")
dest=$(realpath -m -- "$4"); here=$(dirname -- "$(realpath -e -- "$0")")
grep -Fxq 'IMPLEMENTOR_VERSION="Temurin-17.0.21+9"' "$jdk/release"
mkdir -m700 -- "$dest"
cd "$dest"
mkdir -m700 -p build/{classes,source}
cp -- "$release" build/server.zip
cp -- "$src" build/source.zip
printf '%s\n' '4fe7d71d087b307c7869036171bd9d8c6a4284cd7c25b89098b0a24eb2d9b6d2  build/server.zip' \
    '1ae777f2b53ab7b5e6eb2c1783fda2e052db836b3809987336a5053afdc08e0e  build/source.zip' | sha256sum --strict -c >&2
export TMPDIR="$dest/build" JAVA_TOOL_OPTIONS="-XX:-UsePerfData -Djava.io.tmpdir=$dest/build"
unzip -q build/server.zip -d build
python3 "$here/fwcd/prepare.py" build/source.zip build/source
cp=$(printf '%s:' "$dest"/build/server/lib/*.jar); cp=${cp%:}
"$jdk/bin/javac" --release 17 -proc:none -cp "$cp" -d build/classes "$here/kotlin/Protocol.java"
"$jdk/bin/java" -cp "$cp" org.jetbrains.kotlin.cli.jvm.K2JVMCompiler -no-stdlib -no-reflect \
    -jvm-target 17 -classpath "$cp:$dest/build/classes" -d build/classes "$here/fwcd/Worker.kt" build/source/*.kt
"$jdk/bin/jar" --create --file worker.jar --date 2025-01-18T00:00:00Z -C build/classes .
"$jdk/bin/jmod" extract --dir build/base "$jdk/jmods/java.base.jmod"
"$jdk/bin/jar" --create --file java-base.jar --date 2025-01-18T00:00:00Z -C build/base/classes .
install -m400 build/server/lib/kotlin-stdlib-2.1.0.jar kotlin-stdlib.jar
extra=(server-1.3.13 shared-1.3.13 kotlin-compiler-2.1.0 kotlin-reflect-2.1.0
    kotlinx-coroutines-core-jvm-1.6.4 trove4j-1.0.20200330 annotations-13.0
    org.eclipse.lsp4j-0.21.2 org.eclipse.lsp4j.jsonrpc-0.21.2 gson-2.10.1)
classpath=worker.jar:kotlin-stdlib.jar
files=(fwcd-kotlin-language-server worker.jar kotlin-stdlib.jar java-base.jar)
for name in "${extra[@]}"; do
    install -m400 -- "build/server/lib/$name.jar" "$name.jar"
    classpath+=":$name.jar"
    files+=("$name.jar")
done
"$jdk/bin/javac" --release 17 -proc:none -cp "$classpath" -d build/tests "$here/fwcd/ProviderLifetimeTest.java"
"$jdk/bin/java" -Xmx320m -Didea.home.path=/eon-no-idea -cp "build/tests:$classpath" ProviderLifetimeTest >&2
"$jdk/bin/jlink" --add-modules java.base,java.desktop,java.management,java.logging,jdk.unsupported \
    --strip-debug --no-header-files --no-man-pages --strip-native-commands --output build/jre
mkdir -p jre/lib/server
for f in modules server/libjvm.so libjava.so libjimage.so libzip.so libnet.so libnio.so libverify.so; do
    install -m400 -- "build/jre/lib/$f" "jre/lib/$f"
    files+=("jre/lib/$f")
done
mkdir -p jre/conf/security
install -m400 -- build/jre/conf/security/java.security jre/conf/security/java.security
files+=(jre/conf/security/java.security)
cc -std=c11 -O2 -Wall -Wextra -Werror -I"$jdk/include" -I"$jdk/include/linux" \
    "-DEON_JAVA_CLASSPATH=\"$classpath\"" "$here/kotlin/launcher.c" -ldl -pthread \
    -Wl,-z,relro,-z,now -o fwcd-kotlin-language-server
mkdir kotlin-license
unzip -p build/source.zip 'kotlin-language-server-1.3.13/LICENSE.txt' > kotlin-license/LICENSE.txt
cp build/server/licenseReport.html kotlin-license/
cp -R build/jre/legal jre/legal
rm -r -- build
chmod 500 fwcd-kotlin-language-server
chmod 400 worker.jar java-base.jar
sha256sum -- "${files[@]}" | sha256sum
