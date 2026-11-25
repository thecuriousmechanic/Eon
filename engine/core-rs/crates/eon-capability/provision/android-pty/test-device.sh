#!/usr/bin/env bash
# Run on a fresh disposable API 35 x86_64 AVD. TEST_BINARY is the executable
# from cargo test --locked --target=x86_64-linux-android -p eon-capability --lib --no-run
# (NDK r27d x86_64-linux-android35-clang as linker/CC, corresponding clang++ and llvm-ar).
set -euo pipefail
[[ $# == 3 && $1 =~ ^emulator-[0-9]+$ && $2 == /* && $3 == /* ]] || {
    echo "usage: $0 emulator-PORT ABSOLUTE_TEST_BINARY NEW_ABSOLUTE_BUILD_DIR" >&2; exit 2;
}
here=$(cd -- "$(dirname -- "$0")" && pwd)
p=$3
umask 077
mkdir -- "$p"  # Never adopt/delete a pre-existing build directory.
export TMPDIR=$p
adb=("$ANDROID_HOME/platform-tools/adb" -s "$1")
pkg=dev.eon.ptygate
[[ $("${adb[@]}" shell getprop ro.build.version.sdk) == 35 &&
   $("${adb[@]}" shell getprop ro.product.cpu.abi) == x86_64 &&
   $("${adb[@]}" shell getenforce) == Enforcing &&
   $("${adb[@]}" shell id -u) == 2000 ]]
[[ -z $("${adb[@]}" shell pm path "$pkg") ]] || { echo "fixture package already installed" >&2; exit 1; }
sdk="$ANDROID_HOME/build-tools/${ANDROID_BUILD_TOOLS:-37.0.0}"
jar="$ANDROID_HOME/platforms/android-35/android.jar"
mkdir -- "$p/classes" "$p/dex" "$p/lib" "$p/lib/x86_64"
bash "$here/build-worker.sh" "$p/lib/x86_64"
"$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-strip" --strip-all \
    "$2" -o "$p/lib/x86_64/libeon_pty_tests.so"
javac --release 8 -classpath "$jar" -d "$p/classes" "$here/device/PtyGate.java"
"$sdk/d8" --lib "$jar" --min-api 35 --output "$p/dex" "$p/classes/dev/eon/ptygate/PtyGate.class"
keytool -genkeypair -keystore "$p/disposable.p12" -storepass android -alias fixture \
    -dname CN=Disposable -keyalg RSA -validity 2 >/dev/null 2>&1
package() {
    "$sdk/aapt" package -f -M "$here/device/AndroidManifest.xml" -I "$jar" -F "$p/unsigned.apk"
    (cd -- "$p/dex" && zip -q "$p/unsigned.apk" classes.dex)
    (cd -- "$p" && zip -q "$p/unsigned.apk" lib/x86_64/*.so)
    "$sdk/zipalign" -f 4 "$p/unsigned.apk" "$p/aligned.apk"
    "$sdk/apksigner" sign --ks "$p/disposable.p12" --ks-pass pass:android --out "$p/gate.apk" "$p/aligned.apk"
}
run() {
    "${adb[@]}" shell am instrument -w -r "$@" "$pkg/.PtyGate" | tee "$p/result"
    grep -q '^INSTRUMENTATION_RESULT: pty=test result: ok\. [1-9]' "$p/result"
    grep -q '^INSTRUMENTATION_CODE: -1' "$p/result"
}
package
"${adb[@]}" install --streaming "$p/gate.apk"
trap '"${adb[@]}" uninstall "$pkg" >/dev/null' EXIT
run
# Same package and pinned location but altered bytes. Reject before any PTY
# or child is acquired; there is no lookup of an alternative program.
printf '.' >> "$p/lib/x86_64/libeon_pty_worker.so"
package
"${adb[@]}" install --streaming -r "$p/gate.apk"
run -e fixture wrong-image
