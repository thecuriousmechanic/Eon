# Android 15 platform UID byte counters

In EON’s local assistant activity, check **Compare EON’s platform UID traffic counters** and tap **Check model profile**. The opt-in is consumed once and cleared on loss of focus or pause. `SelfUidTraffic` reads only `TrafficStats.getUid{Rx,Tx}Bytes(Process.myUid())`. These are differences between Android’s reported UID totals across all interfaces, not traffic attributed to the action or a measure of CPU. Android may cache the totals. Missing/zero baselines, unsupported, decreasing, or stale samples produce **unavailable**, not a fabricated zero. The shipping app neither requests `INTERNET` nor creates traffic to establish a baseline.

## Device test

Use a fresh, disposable `eon-uid-counters` AVD: SDK `system-images;android-35;google_apis;x86_64` rev 9 with the unmodified factory image. On Linux start **both** the AVD and its own adb server inside a new network namespace with only `lo` up, no veth, uplink, or default route. Do not connect to a physical device or another adb server. Example using the runner’s SDK, NDK r27d, JDK 17 and build-tools 37 (inside that namespace as the ordinary KVM-capable host user):

```sh
repo=/home/runner/work/Eonba/Eonba
export ANDROID_HOME=/usr/local/lib/android/sdk ANDROID_ADB_SERVER_PORT=5094
export ANDROID_AVD_HOME="$repo/engine/android/build/uid-avd"
mkdir -p "$ANDROID_AVD_HOME"
printf 'no\n' | "$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager" create avd \
  -n eon-uid-counters -k 'system-images;android-35;google_apis;x86_64'
"$ANDROID_HOME/platform-tools/adb" start-server
"$ANDROID_HOME/emulator/emulator" -avd eon-uid-counters -port 5594 -no-snapshot -no-window -no-audio
# In another shell in the same namespace with the same environment, after boot:
bash "$repo/engine/android/tests/self_uid_traffic/run.sh" emulator-5594
```

The harness checks enforcement, the exact image and reviewed platform hashes, and uses userdebug `su` **only for a separate read-only metadata check** before installing the test app. `check_pins.c` checks the 4-byte-key/32-byte-value hash map and verifies that both pinned `CGROUP_SKB` programs reference **that map ID** and are **attached** to ingress/egress at `/sys/fs/cgroup`. It does not load a program, change a policy, enumerate keys or read map values. Neither APK contains this helper.

The self-targeted test-only app verifies its package, application UID, process name, `untrusted_app` domain, `Seccomp: 2`, `CapEff: 0`, and sole manifest permission `INTERNET` (for its fixture, not observation). Both connected UDP sockets bind literal IPv4 `127.0.0.1`. One warm-up exchange establishes a baseline; the opted-in action sends and receives 32 datagrams with 512-byte payloads. Only the fixture waits for the platform cache. On the verified image the expected and measured changes were **17,280 B RX and 17,280 B TX**: `32 × (512 + 20 + 8)` each. Missing loopback accounting or even an unexpected extra byte fails this test; there is no network fallback. The fixture and checker are removed on exit.

## Why this result is eBPF-backed on this AVD

Reference: AOSP `packages/modules/Connectivity`, tag `android-15.0.0_r1` (commit `355670c19612b43b73d496fee4b4fc7d4c13bf01`). A source tag alone does **not** identify an emulator’s updatable module. We checked the active factory `com.android.tethering` APEX version `350820300` on build `AE3A.240806.043/12960925`, kernel `6.6.50-android15-8-g8adecb593e9b-ab12525588`. `platform.sha256` pins the actual inspected APEX artifacts, not the reference source.

| Step | Android 15 source | Check on the installed binaries |
|---|---|---|
| Public getter → service | [TrafficStats.java](https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-15.0.0_r1/framework-t/src/android/net/TrafficStats.java#935), [NetworkStatsService.java](https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-15.0.0_r1/service-t/src/com/android/server/net/NetworkStatsService.java#2100) | `dexdump -d` on `classes.dex` in `framework-connectivity-t.jar` and `service-connectivity.jar`: RX/TX invoke `INetworkStatsService.getUidStats`; service checks Binder caller UID. Direct and cache-fill paths call `nativeGetUidStat`. |
| JNI → BPF map read | [service-t/jni/com_android_server_net_NetworkStatsService.cpp](https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-15.0.0_r1/service-t/jni/com_android_server_net_NetworkStatsService.cpp#101), [BpfNetworkStats.cpp](https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-15.0.0_r1/service-t/native/libs/libnetworkstats/BpfNetworkStats.cpp#72) | In `libservice-connectivity.so`, JNI method record `0x10088` binds `nativeGetUidStat` to `0x9de0`; `objdump -dC` shows its call to `bpfGetUidStats@plt` at `0x9e07`. `libnetworkstats.so` exports this function at `0x70650`; its implementation references `/sys/fs/bpf/netd_shared/map_netd_app_uid_stats_map` at `0x1d879`. AOSP reads one key and maps `ENOENT` to zeroes. |
| Traffic → same map | [netd.c](https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-15.0.0_r1/bpf_progs/netd.c#457), [netd.h](https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-15.0.0_r1/bpf_progs/netd.h#174) | `readelf -rW` on the factory `netd.o` shows `app_uid_stats_map` relocations in the ingress and egress accounting sections. The live-object check above verifies the actual loaded map/program relationship, not just object-file presence. |

Only the identified AVD has this provenance result. The shipping code uses public counters, never assumes all vendors implement them with BPF, and cannot distinguish all counter changes hidden between snapshots.
