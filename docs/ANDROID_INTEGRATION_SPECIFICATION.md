# ANDROID INTEGRATION SPECIFICATION
### Mobile Sensory Presence & Accessibility Bridge

> **Subsystem:** EON Mobile Client  
> **Directory:** `clients/android/`  
> **Platform:** Android 14+ (API 34/35), Min SDK 28  
> **Authority:** CANONICAL BLUEPRINT FOR ANDROID IMPLEMENTATION

---

## 1. Overview & Architectural Role

The Android application (`com.eon.assistant`) serves as EON's mobile eyes, ears, and hands. It connects to the Native Rust Core (`libeon_uniffi.so`) via Mozilla UniFFI and native C/NDK hardware streams.

The Android subsystem provides:
1. **Accessibility Automation**: `EonAccessibilityService` traverses the user-authorized UI tree, finds clickable elements, dispatches gestures, and monitors window state transitions.
2. **Persistent HUD & Thought Visualizer**: `EonOverlayService` renders a non-intrusive floating pulse orb/card over other apps (`TYPE_APPLICATION_OVERLAY`).
3. **Zero-Copy Audio Capture Loop**: `EonCoreService` holds Android's foreground service microphone permission (`FOREGROUND_SERVICE_MICROPHONE`), while the native Rust `eon-voice` engine opens and reads directly from Android's native C/NDK `AAudio` / `Oboe` stream using zero-copy hardware ring buffers. No `sequence<i16>` arrays are copied across JNI during real-time speech.
4. **Multimodal Screen Perception**: Captures screen frames via `MediaProjection` and streams keyframes to `eon-vision` for OmniParser-v2 UI element detection.
5. **Quick Settings Tile**: `EonTileService` allows one-tap toggling of EON between Dormant, Silent Copilot, and Full-Duplex Voice modes.

### Shipped boundary: the private action demo (API 34+)

The inspection shortcut is read-only. Actions are available only in the foreground
`DemoActionActivity`, on its own counter and sample list. To try the fallback on stock
widgets, choose **Try samples without native actions**, then review and confirm a
counter click or list scroll. Changing sample mode clears approval; it does not grant
authority. The confirmation names the route. An advertised native action takes
precedence; once attempted, even a false return or a missing effect **never** causes
a fallback or retry.

The fallback is **activity-local hit testing and UI dispatch**, not an Android input
gesture. A display gesture cannot be bound to the reviewed `View` at delivery. Instead,
the activity derives the unclipped control's center in its own decor at review and
again at confirmation. That point must lie on the known button or a known list row,
with no overlapping sibling on its path. No coordinates are accepted, saved or
exported. The activity calls `performClick()` on that button or `scrollBy()` by at
most one viewport on that list; it never synthesizes touch events, flings, or uses
`AccessibilityService.dispatchGesture`. No manifest permission is needed for this route.

The 15-second, one-use review binds the route, control, starting value, window token,
and inspection-service epoch. Dispatch requires the unlocked, resumed, focused,
full-screen demo, the assistant role, and the enabled service and shortcut. The only
success signal is a counter increase of exactly one or observed forward scroll of
at most one viewport. Native scrolling may animate, so it gets one delayed **read**,
not another dispatch. A canceled or stale review cannot be replayed.

From `engine/android`, build `:app:assembleDebug` and use the existing device gates:

```sh
python3 app/keystore_device_test.py --serial "$CLEAN_AVD" --demo-denied
python3 app/keystore_device_test.py --preconfigured-allowed-serial "$OPERATOR_AVD"
```

Both require explicitly selected, disposable API 34+ emulators. For the allowed
gate, manually install the matching target APK, select EON as assistant, and enable
its accessibility service and shortcut. Leave the device unlocked and the test APK
uninstalled. Neither gate sets roles or enables services. They are separate tests:
passing the clean-device denial gate is not evidence that the allowed gate ran.

On the API 35 Google APIs image, Android reported that EON's assigned accessibility
button was not *shown*. That does not mean the user revoked the shortcut. For this
private demo only, check the exact component in the current user's
`accessibility_button_targets`, the enabled service's button flag and the existing
role, lock and window checks. If Android does not expose the setting, refuse the
action. A change to the assignment invalidates pending review. The shortcut inspector
itself remains read-only.

Lookup starts at the private sample group's `AccessibilityNodeInfo` and matches the
window, package, resource ID and class of each expected child, not its position in
the view list. It does not enumerate other windows. Two fresh API 35 AVDs passed
`demo=PASS-allowed` and `demo=PASS-denied` with the same signed APK: cancel left
the counter at 0, the native click produced 1, the local fallback produced 2, and
both scroll routes advanced the rendered list by at most one viewport.

**Locate sample field** reports only whether the demo's own unsaved `EditText` was
found. It does not expose a value, open an IME, accept edits or grant an action.
The API 35 gate changes the order of the field and its label, then checks that
sensitive, wrong-class and same-ID replacement controls cannot satisfy the lookup.

---

## 2. Android Manifest & Permissions (`AndroidManifest.xml`)

```xml
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="com.eon.assistant">

    <!-- Audio & Speech -->
    <uses-permission android:name="android.permission.RECORD_AUDIO" />
    <uses-permission android:name="android.permission.MODIFY_AUDIO_SETTINGS" />

    <!-- Overlay & System Interaction -->
    <uses-permission android:name="android.permission.SYSTEM_ALERT_WINDOW" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_MICROPHONE" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE" />
    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_MEDIA_PROJECTION" />
    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />

    <!-- Network & P2P Mesh -->
    <uses-permission android:name="android.permission.INTERNET" />
    <uses-permission android:name="android.permission.ACCESS_NETWORK_STATE" />
    <uses-permission android:name="android.permission.ACCESS_WIFI_STATE" />
    <uses-permission android:name="android.permission.CHANGE_WIFI_MULTICAST_STATE" />
    <uses-permission android:name="android.permission.BLUETOOTH" />
    <uses-permission android:name="android.permission.BLUETOOTH_CONNECT" />

    <!-- Hardware Sensory (Optional) -->
    <uses-permission android:name="android.permission.CAMERA" />
    <uses-permission android:name="android.permission.VIBRATE" />

    <application
        android:name=".EonApplication"
        android:allowBackup="false"
        android:icon="@mipmap/ic_launcher"
        android:label="EON"
        android:roundIcon="@mipmap/ic_launcher_round"
        android:supportsRtl="true"
        android:theme="@style/Theme.Eon">

        <!-- Persistent Core Foreground Service -->
        <service
            android:name=".service.EonCoreService"
            android:exported="false"
            android:foregroundServiceType="microphone|connectedDevice|mediaProjection" />

        <!-- Native Accessibility Controller -->
        <service
            android:name=".accessibility.EonAccessibilityService"
            android:permission="android.permission.BIND_ACCESSIBILITY_SERVICE"
            android:exported="true">
            <intent-filter>
                <action android:name="android.accessibilityservice.AccessibilityService" />
            </intent-filter>
            <meta-data
                android:name="android.accessibilityservice"
                android:resource="@xml/accessibility_service_config" />
        </service>

        <!-- Floating HUD Overlay Service -->
        <service
            android:name=".overlay.EonOverlayService"
            android:exported="false" />

        <!-- Quick Settings Tile -->
        <service
            android:name=".system.EonTileService"
            android:icon="@drawable/ic_eon_tile"
            android:label="EON"
            android:permission="android.permission.BIND_QUICK_SETTINGS_TILE"
            android:exported="true">
            <intent-filter>
                <action android:name="android.service.quicksettings.action.QS_TILE" />
            </intent-filter>
        </service>

        <!-- Native Android Digital Assistant (ROLE_ASSISTANT) -->
        <service
            android:name=".service.EonVoiceInteractionService"
            android:permission="android.permission.BIND_VOICE_INTERACTION"
            android:exported="true">
            <meta-data
                android:name="android.voice_interaction"
                android:resource="@xml/voice_interaction_service_config" />
            <intent-filter>
                <action android:name="android.service.voice.VoiceInteractionService" />
            </intent-filter>
        </service>

        <service
            android:name=".service.EonVoiceInteractionSessionService"
            android:permission="android.permission.BIND_VOICE_INTERACTION"
            android:exported="true" />

    </application>
</manifest>
```

---

## 3. Accessibility Service Configuration (`res/xml/accessibility_service_config.xml`)

```xml
<?xml version="1.0" encoding="utf-8"?>
<accessibility-service xmlns:android="http://schemas.android.com/apk/res/android"
    android:accessibilityEventTypes="typeWindowStateChanged|typeWindowContentChanged|typeViewClicked"
    android:accessibilityFeedbackType="feedbackGeneric"
    android:accessibilityFlags="flagDefault|flagRetrieveInteractiveWindows|flagReportViewIds|flagIncludeNotImportantViews"
    android:canRetrieveWindowContent="true"
    android:canPerformGestures="true"
    android:notificationTimeout="50"
    android:description="@string/accessibility_service_description" />
```

---

## 4. UniFFI Kotlin Integration Pattern & Native Audio

In `EonCoreService.kt`, the Android service initializes the native Rust engine and coordinates system services:

```kotlin
package com.eon.assistant.service

import android.app.Service
import android.content.Intent
import android.os.IBinder
import uniffi.eon_core.*

class EonCoreService : Service() {
    private lateinit var kernelBridge: EonKernelBridge
    private lateinit var voiceBridge: EonVoiceBridge

    override fun onCreate() {
        super.onCreate()
        val storagePath = applicationContext.filesDir.absolutePath
        val identityId = "id_01J9H2X7K9M4N3P2R1Q8S7T6V5"

        // Initialize Rust Native Core
        kernelBridge = EonKernelBridge(storagePath, identityId)
        voiceBridge = EonVoiceBridge()

        // In production, eon-voice attaches directly to native AAudio NDK stream.
        // startListening() commands Rust to open the hardware microphone stream.
        voiceBridge.startListening()

        // Register hardware barge-in callback
        voiceBridge.registerBargeInCallback(object : BargeInCallback {
            override fun onBargeInDetected(timestampMs: Long) {
                // Instantly mute speaker and show listening state on overlay
                EonOverlayService.showListeningState(applicationContext)
            }
        })
    }

    override fun onDestroy() {
        voiceBridge.stopListening()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null
}
```

---

## 5. Rich Visual Rendering & Live Artifact Canvas

For interactive code diffs, reverse engineering control-flow graphs, and multimodal bounding box overlays:
- **Android UI Layer**: Authored in **Jetpack Compose Multiplatform + Skia**.
- **Interactive Canvas**: Displays side-by-side AST patches, live video keyframe timelines with scrubbing, and OmniParser annotated element boundaries over the screen.

---

## 6. Android 14/15+ OS Survival & Digital Assistant Role Integration

Modern Android releases (API 34/35) enforce aggressive background restrictions:
1. Third-party apps attempting background microphone recording or screen capture are abruptly killed with `SecurityException` or targeted by OEM battery killers (e.g. Samsung Phantom Process Killer).
2. Background services attempting to launch activities or show overlays face strict foreground initiation barriers.

On stock Android the user may select EON as the **digital assistant (`ROLE_ASSISTANT`)**. The design must respect restrictions even when the role is active:

```mermaid
flowchart TD
    AndroidOS["Android 14/15+ Framework"] -->|Binds via android.service.voice| AssistantService["EonVoiceInteractionService (ROLE_ASSISTANT)"]

    Source["Private EON profile page"] -->|User approves one profile check| Local["Android local voice interaction"]
    Local -->|AssistState.ActivityId only| Session["Selected EON session"]
    AssistantService --> Session
    Session -->|Matching activity ID: request and perform direct action| Source
    Source -->|One profile result via Android| Session
```

### 6.1 `EonVoiceInteractionService` Implementation
Declaring a `VoiceInteractionService` makes the app eligible; it does not select the app for the user.
- **Hardware hotword: unsupported by this ordinary app.** The API 35 SDK's `data/api-versions.xml` marks `AlwaysOnHotwordDetector` and its public factory removed in API 31, the first API where EON enables its voice service. The API 35 public `android.jar` exposes neither a detector factory nor a module/enrollment query. In Android 15's [`VoiceInteractionService`](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/core/java/android/service/voice/VoiceInteractionService.java) the DSP factory is `@SystemApi`, `listModuleProperties()` is `@TestApi`, and the software detector factory is `@SystemApi` with `MANAGE_HOTWORD_DETECTION`. The assistant role does not change the SDK surface. Availability of a microphone or on-device `SpeechRecognizer` does not establish an enrolled wake word. EON uses the assistant gesture, not a fallback listener. Hardware availability on a particular phone is unknown.
- **Microphone**: Require the OS permissions and active capture conditions. Denial or role loss must stop capture, not trigger a fallback recorder.
- **Assist context**: `onReady()` sets and reads back the binding-wide structure and screenshot mask (`3`) before enabling EON's session controls. EON never clears either bit. The framework owns the state before that callback. On Android 15 even an activity-token-scoped show passes through the same mask. Filtering an incoming tree by package would be too late. EON does not select an Autofill provider, request an Autofill structure, or send a page to a provider for assistant use.
- **Process death**: The OS controls process priority. Restore durable work after restart instead of relying on an OOM score or immunity from process management.

#### Owner-only direct action (API 35 device check)

With EON selected, open **Review an Android assistant profile check**. Back leaves without checking. **Allow one profile check** starts `Activity.startLocalVoiceInteraction()` from that private page. Only the first showing of a fresh session with the live five-second Binder ticket can proceed. On API 34+ EON also matches Android’s `KEY_SHOW_SESSION_ID` between the selected service’s `onPrepareToShowSession` and the session’s `onPrepareShow`. A missing, late or mismatched preparation cannot authorize an action. The ID is correlation metadata, not a secret or consent; EON does not log it.

Android scopes the local invocation to its starting activity. The session compares the opaque `AssistState.activityId` with the ID from the source’s `VoiceInteractor.notifyDirectActionsChanged()` before requesting the fixed `CHECK_MODEL_PROFILE` action. The source checks that its original interactor is still active, as well as the selected binding, policy and foreground. It consumes a **separate** Binder permit before performing the check; the token passed as show options cannot be replayed as that permit. On API 34+, the next service preparation revokes that permit even if Android reuses the interactor and has not delivered the next session callback yet. Revocation drops the old card without finishing its replacement. The source disclosure stays visible independently of the previous check’s non-saved completion label.

A rejected local show has no card and offers no action. EON gives Android time to acknowledge local start; after five seconds it only finishes a rejected API 34+ showing if its OS ID still matches the binding. If consent ends before the source receives a start acknowledgement, the private activity finishes rather than retry on an `ActivityRecord` Android may still mark as pending. Reopen the page from controls for a fresh confirmation; EON does not retry the action for the user.

**The 9.5 OS-structure gate remains open.** This is OS-supplied assist identity and OS-routed execution, not `AssistStructure` extraction. On API 35, [`startLocalVoiceInteraction`](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/services/voiceinteraction/java/com/android/server/voiceinteraction/VoiceInteractionManagerService.java) passes `SHOW_SOURCE_ACTIVITY` without `SHOW_WITH_ASSIST`. [`Activity.showAssist`](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/services/core/java/com/android/server/wm/ActivityClientController.java) passes the caller's token, but the system adds both collection flags. [`showSessionLocked`](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/services/voiceinteraction/java/com/android/server/voiceinteraction/VoiceInteractionManagerServiceImpl.java) filters by token **and passes the same binding-wide mask**. [`showLocked`](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/services/voiceinteraction/java/com/android/server/voiceinteraction/VoiceInteractionSessionConnection.java) therefore forbids both fetches with mask `3`. The public [`AssistStructure()`](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/core/java/android/app/assist/AssistStructure.java) is empty; its activity-taking constructor is not public SDK. Neither temporarily relaxing the mask nor constructing a local stand-in meets this gate.

In that API 35 implementation, `doHandleAssistWithoutData` supplies the activity identity, index and count, but explicitly sends **null data, structure and content**. `showSessionLocked` adds the show ID independently of capture. Conversely, `KEY_FOREGROUND_ACTIVITIES` is populated only when assist data collection is allowed; it is not an alternative source under mask `3`. Neither EON’s own profile result nor an observer’s accessibility nodes are OS screen context.

**Owner-context policy.** `getUserDisabledShowContext()` reports the user's two Settings switches, not enterprise policy. Android's [`isAssistDataAllowed`](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/services/core/java/com/android/server/wm/ActivityTaskManagerService.java) enforces additional restrictions on OS capture. For its two *app-authored* context routes, EON checks the text switch, the calling user's `DevicePolicyManager.getScreenCaptureDisabled(null)` and, on API 35+, `UserManager.DISALLOW_ASSIST_CONTENT`. On 35+ it also declines non-managed profiles (which can include private space), without querying other users. Missing policy services or failed checks deny sharing. These checks apply before using the context and while showing the result; they are not a substitute for the OS's whole-display policy. The CPU-setting show bundle contains only the one-use Binder ticket, not the reviewed setting.

On a disposable, unlocked, setup-complete 3 GiB API 35 Google APIs x86_64 AVD with no personal data, install the matching signed debug **target** and select EON through Settings. Leave the observer uninstalled. From `engine/android`, choose the case matching the policy already on that AVD:

```sh
python3 app/keystore_device_test.py --preconfigured-profile-serial "$SERIAL" --profile-policy allowed
# On a separate AVD with Use text from screen already off:
python3 app/keystore_device_test.py --preconfigured-profile-serial "$SERIAL" --profile-policy denied
```

The driver checks setup, target APK identity and the closed collection mask, then installs and removes only its observer. Use `restricted` or `capture-disabled` only on a disposable AVD already under that policy. The observer checks the active service and actual policy before driving the UI. A different case's result does not count. This driver does not establish that an OS view tree was delivered.

On `AE3A.240806.043/12960925` (enforcing, loopback-only host network, no host audio or cameras), `PASS-os-action` observed the source's completed profile check, `p0_mobile_ultralight`, and a separately confirmed CPU setting. `PASS-policy-denied` observed both refusals and an unexecuted profile check. Both verified a fresh empty assistant card, `mDisabledShowContext=3`, and no microphone use. The observer also accepts `-e policy restricted` or `-e policy capture-disabled` on an already managed test device with just that restriction active. These enterprise denial cases were **not** verified on a managed device: the stock emulator denied the shell permission to impose `no_assist_content`, and the observer refused to count an unrestricted run as a pass. No admin permission was granted. The JVM suite covers denied and unavailable policy inputs.

#### Selected assistant crash, automatic rebind and fresh consent (9.5, software only)

**Not yet device-verified for the current implementation.** This checks whether the approved profile action or its result reappears after process death without another tap. It does not request assist contents, replay or extract a Binder token, or start a microphone. The production collection mask is unchanged. To compile without installing, selecting or running anything on a device, build `:app:assembleDebug`, then run `python3 "$A/app/keystore_device_test.py" --build-only` with `A` as below.

Use a fresh disposable `pixel_4` AVD named **`eon-profile-death`**, English UI, 3 GiB RAM, `system-images;android-35;google_apis;x86_64` rev 9 (`AE3A.240806.043/12960925`). Use only a local adb server in the same loopback-only host environment as the emulator. No accounts, personal data, host audio input or cameras. Example commands **after approval**, with the documented SDK/image and build toolchain installed:

```sh
A=/home/runner/work/Eonba/Eonba/engine/android
"$A/gradlew" -p "$A" :app:assembleDebug
python3 -m unittest discover -s "$A/app" -p 'test_profile_death_device_test.py'
export ANDROID_AVD_HOME="$A/build/profile-death-avd"
mkdir -m 700 "$ANDROID_AVD_HOME" || exit 1   # refuse to reuse an existing AVD directory
printf 'no\n' | "$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager" create avd -n eon-profile-death \
    -k 'system-images;android-35;google_apis;x86_64' --device pixel_4
"$ANDROID_HOME/emulator/emulator" -avd eon-profile-death -port 5588 -memory 3072 \
    -no-snapshot -no-audio -camera-front none -camera-back none
```

In a second terminal in that environment, finish setup and unlock owner user 0. Install **only** the local debug target on the empty AVD:

```sh
A=/home/runner/work/Eonba/Eonba/engine/android
ADB="$ANDROID_HOME/platform-tools/adb"
SERIAL=emulator-5588
test "$("$ADB" -H 127.0.0.1 -P 5037 -s "$SERIAL" shell getprop ro.boot.qemu.avd_name)" = eon-profile-death &&
    "$ADB" -H 127.0.0.1 -P 5037 -s "$SERIAL" install --user 0 "$A/app/build/outputs/apk/debug/app-debug.apk"
```

Select EON **through Android's picker** and return to EON controls with no assistant card showing. Leave the OS text-sharing switch enabled and device policy unrestricted. Do not configure accessibility, microphone access or other grants. No further input is needed:

```sh
ANDROID_ADB_SERVER_PORT=5037 python3 "$A/app/profile_death_device_test.py" \
    --disposable-serial "$SERIAL" --crash-selected-assistant
```

The driver pins the selected adb transport, checks the console and guest AVD identities, checks enforcing/non-root shell, setup, live role, service and mask, matches the installed APK's SHA-256 to the local build, and verifies `run-as` and an unshared target UID. It installs only the separate observer. The observer checks the actual sharing policy, opens the private EON source, confirms one action and leaves the expected result visible. **Only then**, after rechecking identity, the original service PID, mask `3` and Android's shown-session flag, the host issues **one** `am crash --user 0 com.eon.assistant`. The stock command selects the package's framework process record; it never signals a saved PID, force-stops the app or changes its selection.

Before launching **any** recovery UI, the driver requires a different PID hosting the same bound voice service, the old session no longer shown, and a new matching exit record for the old PID and UID. On [API 35](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/services/core/java/com/android/server/am/ActivityManagerShellCommand.java), `am crash` schedules `CrashedByAdbException`; the [Java crash path](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/services/core/java/com/android/server/am/AppErrors.java) records [`reason=4 (APP CRASH(EXCEPTION))`](https://android.googlesource.com/platform/frameworks/base/+/android-15.0.0_r1/core/java/android/app/ApplicationExitInfo.java). A shell return code, a different PID alone, a signal, or a native crash is not sufficient.

In that replacement process the observer brings the launcher task forward **without `CLEAR_TOP`**. If Android restored the private source it checks that page, rather than discarding it and manufacturing an empty one. If Android returned to EON’s controls it opens the private page. An already-foreground task need not emit an event: in that case the observer checks the stock default-display focus on-device and sends one Tab only if it names the EON controls or private source. Tab does not confirm a check. The observer still requires a node from an EON event; it never falls back to querying a foreign UI root. It invokes a normal card **without confirming**, then samples the same live card for longer than the five-second ticket lifetime. There must be no visible authorized result, and on dismissal the same source window must still show the disclosure without any outcome. A new confirmation must then produce both the expected OS-routed result and the source's completion label. Expected final line: `profileDeath=PASS-crash-rebind-fresh-consent`. This is an observable non-replay check, not proof of every intermediate callback, crash cause on arbitrary builds, microphone release, hotword support or owner-only `AssistStructure` delivery.

The armed checkpoint is **not** a pass. A crash dialog, delayed/missing exit record, failed rebind, timeout or interruption is inconclusive; do not dismiss dialogs for the harness, retry the crash, or relaunch to manufacture progress. The driver normally removes only its observer. A host/transport interruption can leave it installed or the crash already delivered; discard the AVD rather than resume a partial run.

### 6.2 `MediaProjection` Token Lifecycle
**Implemented on API 35+: grant lifecycle only.** Select EON as the digital assistant in Android Settings, open EON controls, choose **Review a no-output projection grant**, then explicitly open Android's picker. Choose **one app → EON**. The OS owns the scope choice; EON neither queries that scope nor reads anything from it. Assistant selection alone never starts the picker or a grant.

The private singleton controls require EON's live voice binding, the assistant role and an unlocked, interactive device. On return from the picker, the secure controls or the exact EON launcher instance which opened them must have resumed and gained focus. A result expires after 1.5 seconds; both dispatch and consumption check the monotonic deadline. Only an opaque, process-local request ID goes in the service command. The service promotes itself with type `mediaProjection` **before** exchanging the result for a `MediaProjection`, registers its stop callback and retains only the grant. There is **no virtual display, surface, image reader, frame, storage, audio, native bridge, or network output** in this path.

Stop from the grant controls, or use Android's sharing controls if the OS provides them. The diagnostic lease also expires after one minute. Role/binding loss, screen off, task/service teardown and process death invalidate ownership. The service is not sticky. Another session always needs a new foreground tap and picker result. The voice service keeps assist structure and screenshot disabled (`mDisabledShowContext=3`); this is not continuous perception or hotword detection.

#### Reproduce the device checks

Use an empty, offline, setup-complete API 35 `ranchu` AVD, no host audio or cameras, and an explicitly selected `adb -s emulator-N` throughout. In `engine/android`, run `./gradlew :app:test :app:assembleDebug` and `python3 app/keystore_device_test.py --build-only`. Install `app/build/outputs/apk/debug/app-debug.apk` and `app/build/intermediates/keystore-test/keystore-test.apk` with `install -t`. Choose the role through Android Settings, not a shell role or app-op grant. Start EON by its normal launcher entry. Leave accessibility and microphone setup untouched.

1. Run `adb -s emulator-N shell am instrument -w -r com.eon.assistant.keystoretest/com.eon.assistant.ProjectionGrantAcceptance`. The instrumentation targets its **own** package, not the selected voice service. Open the grant controls and obtain the one-app EON grant. If Android foregrounds the selected EON launcher task, reopen the grant controls to expose the `active` checkpoint.
2. Try the system chip's **Stop** if the device exposes it. Alternatively, on the disposable AVD, start `adb -s emulator-N shell am start -n com.eon.assistant.keystoretest/com.eon.assistant.ProjectionReplacementFixture`. Tap the fixture button and approve the **second real Android dialog**. Its typed foreground service holds only the replacement token for three seconds. The observer must see **Android stopped the grant (onStop callback)**, not an app-initiated stop or a timer. Open the picker again and cancel with Back. Expect `PASS-external-stop-denied`.
3. Return to the EON launcher controls. Repeat the instrumentation with `-e phase restart` before the component name. After the real `active` checkpoint, run `adb -s emulator-N shell am force-stop com.eon.assistant`. Relaunch EON, reopen controls and cancel a fresh picker. Expect `PASS-restart-denied`. This checks the previous PID is gone, the new one differs, and both the projection and typed service are gone.

On the tested `system-images;android-35-ext15;google_apis;x86_64` image (`AE3A.240806.046.B2`), the real one-app chooser foregrounded the owned EON launcher. `dumpsys media_projection` reported `(com.eon.assistant, uid=10210): TYPE_SCREEN_CAPTURE` and the service reported `isForeground=true`, type `0x00000020`. External replacement changed the platform owner to the fixture UID, triggered EON's callback, and left `Media Projection: null` and neither service running after release. Both acceptance modes passed. **A tappable status-bar Stop chip was not verified on this image**, including in a discarded null-surface experiment. No flag was changed to enable one; the shipped implementation needs no display.
