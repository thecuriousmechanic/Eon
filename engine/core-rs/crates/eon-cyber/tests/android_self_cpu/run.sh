#!/usr/bin/env bash
# Stock-app denial gate. Requires an already-running *disposable* API 35 AVD
# named eon-self-cpu, the API 35 / build-tools 37 SDK, NDK r27d and Rust target.
set -euo pipefail
umask 077
[[ $# == 1 && $1 =~ ^emulator-[0-9]+$ ]] || { echo "Usage: bash run.sh emulator-PORT" >&2; exit 2; }
here=$(cd -- "$(dirname -- "$0")" && pwd -P)
workspace=$(cd "$here/../../../.." && pwd -P)
sdk=${ANDROID_HOME:?set ANDROID_HOME}
adb=("$sdk/platform-tools/adb" -s "$1")
[[ $("${adb[@]}" emu avd name | tr -d '\r') == $'eon-self-cpu\nOK' ]]
[[ $("${adb[@]}" shell getprop ro.kernel.qemu | tr -d '\r') == 1 ]]
[[ $("${adb[@]}" shell getprop ro.build.version.sdk | tr -d '\r') == 35 ]]
[[ $("${adb[@]}" shell getprop ro.product.cpu.abi | tr -d '\r') == x86_64 ]]
[[ $("${adb[@]}" shell getenforce | tr -d '\r') == Enforcing ]]
package=org.eon.test.selfcpu
[[ $("${adb[@]}" shell pm list packages "$package" | tr -d '\r') == "" ]] ||
    { echo "Refusing to replace an installed fixture" >&2; exit 1; }

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$workspace/target}"
mkdir -p "$CARGO_TARGET_DIR"
stage="$CARGO_TARGET_DIR/android-self-cpu-$$"
mkdir -m 700 "$stage"
installed=false
cleanup() {
    local rc=$?
    if "$installed"; then "${adb[@]}" uninstall "$package" >/dev/null || rc=1; fi
    rm -rf -- "$stage"
    exit "$rc"
}
trap cleanup EXIT
mkdir -p "$stage/classes" "$stage/lib/x86_64"
ndk="$sdk/ndk/27.3.13750724/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CC_x86_64_linux_android="$ndk/x86_64-linux-android26-clang" AR_x86_64_linux_android="$ndk/llvm-ar"
export CXX_x86_64_linux_android="$ndk/x86_64-linux-android26-clang++"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$CC_x86_64_linux_android"
cargo build --locked --manifest-path "$workspace/Cargo.toml" --target x86_64-linux-android \
    -p eon-cyber --no-default-features --features self-cpu-ebpf --bin eon-audit
"$ndk/llvm-strip" -o "$stage/lib/x86_64/libeon_audit.so" "$CARGO_TARGET_DIR/x86_64-linux-android/debug/eon-audit"

tools="$sdk/build-tools/37.0.0"
api="$sdk/platforms/android-35/android.jar"
javac --release 11 -classpath "$api" -d "$stage/classes" "$here/StockAppTest.java"
"$tools/d8" --lib "$api" --min-api 35 --output "$stage" "$stage/classes/org/eon/test/selfcpu/StockAppTest.class"
"$tools/aapt2" link -o "$stage/unsigned.apk" -I "$api" --manifest "$here/AndroidManifest.xml"
jar uf "$stage/unsigned.apk" -C "$stage" classes.dex -C "$stage" lib
"$tools/zipalign" -P 16 -f 4 "$stage/unsigned.apk" "$stage/aligned.apk"
export SELF_CPU_TEST_KEY_PASS
SELF_CPU_TEST_KEY_PASS=$(openssl rand -hex 24)
keytool -genkeypair -keystore "$stage/key.p12" -storepass:env SELF_CPU_TEST_KEY_PASS \
    -alias fixture -keyalg RSA -dname "CN=Disposable Self CPU Test" -validity 1
"$tools/apksigner" sign --ks "$stage/key.p12" --ks-pass env:SELF_CPU_TEST_KEY_PASS \
    --out "$stage/test.apk" "$stage/aligned.apk"
"${adb[@]}" install --streaming -t "$stage/test.apk"
installed=true
result=$(timeout 45 "${adb[@]}" shell am instrument -w "$package/.StockAppTest" | tr -d '\r')
printf '%s\n' "$result"
grep -Fxq 'INSTRUMENTATION_RESULT: self_cpu=PASS untrusted_app; seccomp=2; CapEff=0; opt-out absent; opt-in unavailable' <<<"$result"
grep -Fxq 'INSTRUMENTATION_CODE: -1' <<<"$result"
