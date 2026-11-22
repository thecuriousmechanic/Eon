#!/usr/bin/env bash
# Run *inside* the disposable AVD's loopback-only Linux network namespace.
set -euo pipefail
umask 077
[[ $# == 1 && $1 =~ ^emulator-[0-9]+$ ]] || { echo 'Usage: bash run.sh emulator-PORT' >&2; exit 2; }
[[ $(ls /sys/class/net) == lo && -z $(ip route show default) && -z $(ip -6 route show default) ]] ||
    { echo 'Refusing a host namespace with non-loopback networking' >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd -P)
android=$(cd "$here/../.." && pwd -P)
sdk=${ANDROID_HOME:?set ANDROID_HOME}
adb=("$sdk/platform-tools/adb" -s "$1")
tools="$sdk/build-tools/37.0.0"
api="$sdk/platforms/android-35/android.jar"
package=org.eon.test.uidtraffic
[[ $("${adb[@]}" emu avd name | tr -d '\r') == $'eon-uid-counters\nOK' ]]
[[ $("${adb[@]}" shell getprop ro.kernel.qemu) == 1 ]]
[[ $("${adb[@]}" shell getprop ro.build.version.sdk) == 35 ]]
[[ $("${adb[@]}" shell getprop ro.build.fingerprint) == 'google/sdk_gphone64_x86_64/emu64xa:15/AE3A.240806.043/12960925:userdebug/dev-keys' ]]
[[ $("${adb[@]}" shell getenforce) == Enforcing ]]
[[ $("${adb[@]}" shell id -u) == 2000 ]] # No rooted adbd.
"${adb[@]}" shell -T sha256sum -c - < "$here/platform.sha256"
[[ $("${adb[@]}" shell pm list packages "$package") == '' ]] ||
    { echo 'Refusing to replace an installed fixture' >&2; exit 1; }

stage="$android/build/self-uid-traffic-$$"
mkdir -p "$android/build"; mkdir "$stage"
installed=false
remote=/data/local/tests/unrestricted/eon-uid-traffic-$$
created=false
cleanup() {
    local rc=$?
    if "$installed"; then "${adb[@]}" uninstall "$package" >/dev/null || rc=1; fi
    if "$created"; then "${adb[@]}" shell rm -r "$remote" >/dev/null || rc=1; fi
    rm -rf -- "$stage"
    exit "$rc"
}
trap cleanup EXIT
"$sdk/ndk/27.3.13750724/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android35-clang" \
    -std=gnu11 -Wall -Werror -O2 "$here/check_pins.c" -o "$stage/check_pins"
"${adb[@]}" shell mkdir "$remote"
created=true
"${adb[@]}" push "$stage/check_pins" "$remote/check_pins"
"${adb[@]}" shell su 0 "$remote/check_pins"
mkdir "$stage/classes"
javac --release 11 -classpath "$api" -d "$stage/classes" \
    "$android/app/src/main/java/com/eon/assistant/SelfUidTraffic.java" "$here/LoopbackTest.java"
jar cf "$stage/fixture.jar" -C "$stage/classes" .
"$tools/d8" --lib "$api" --min-api 35 --output "$stage" "$stage/fixture.jar"
"$tools/aapt2" link -o "$stage/unsigned.apk" -I "$api" --manifest "$here/AndroidManifest.xml"
jar uf "$stage/unsigned.apk" -C "$stage" classes.dex
"$tools/zipalign" -f 4 "$stage/unsigned.apk" "$stage/aligned.apk"
export UID_TRAFFIC_KEY_PASS
UID_TRAFFIC_KEY_PASS=$(openssl rand -hex 24)
keytool -genkeypair -keystore "$stage/key.p12" -storepass:env UID_TRAFFIC_KEY_PASS \
    -alias fixture -keyalg RSA -dname 'CN=Disposable UID Traffic Fixture' -validity 1
"$tools/apksigner" sign --ks "$stage/key.p12" --ks-pass env:UID_TRAFFIC_KEY_PASS \
    --out "$stage/test.apk" "$stage/aligned.apk"
"${adb[@]}" install --streaming -t "$stage/test.apk"
installed=true
result=$(timeout 30 "${adb[@]}" shell am instrument -w "$package/.LoopbackTest" | tr -d '\r')
printf '%s\n' "$result"
grep -Fxq 'INSTRUMENTATION_RESULT: uid_traffic=PASS untrusted_app; CapEff=0; seccomp=2; rx=17280; tx=17280' <<<"$result"
grep -Fxq 'INSTRUMENTATION_CODE: -1' <<<"$result"
