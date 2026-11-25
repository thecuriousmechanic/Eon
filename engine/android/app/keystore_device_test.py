"""Build Android instrumentation and run explicitly selected disposable-target acceptance gates."""

import argparse
from contextlib import ExitStack
import hashlib
import os
from pathlib import Path
import re
import shlex
import subprocess
from zipfile import ZIP_STORED, ZipFile


APP = Path(__file__).resolve().parent
BUILD = APP / "build"
WORK = BUILD / "intermediates/keystore-test"
SOURCE = APP / "src/androidTest"
TARGET = "com.eon.assistant"
TEST = f"{TARGET}.keystoretest"
TARGET_APK = BUILD / "outputs/apk/debug/app-debug.apk"


def build(sdk):
    tools = sdk / "build-tools/37.0.0"
    platform = sdk / "platforms/android-35/android.jar"
    target_jar = BUILD / "libs/app.jar"
    key = BUILD / "debug.keystore"
    if not target_jar.is_file() or not key.is_file():
        raise SystemExit("Build :app:assembleDebug first.")
    classes = WORK / "classes"
    classes.mkdir(parents=True, exist_ok=True)
    for old in classes.rglob("*.class"):
        old.unlink()
    javac = ["javac", "--release", "8", "-classpath", os.pathsep.join(map(str, (platform, target_jar)))]
    subprocess.run([*javac, "-d", classes, *SOURCE.glob("java/**/*.java")], check=True)
    dex = WORK / "dex.zip"
    subprocess.run([tools / "d8", "--min-api", "28", "--lib", platform, "--classpath", target_jar,
                    "--output", dex, *classes.rglob("*.class")], check=True)
    raw = WORK / "test.apk"
    subprocess.run([tools / "aapt2", "link", "-I", platform, "--manifest", SOURCE / "AndroidManifest.xml",
                    "-o", raw], check=True)
    with ZipFile(raw, "a", compression=ZIP_STORED) as out, ZipFile(dex) as code:
        out.writestr("classes.dex", code.read("classes.dex"))
    aligned = WORK / "aligned.apk"
    signed = WORK / "keystore-test.apk"
    subprocess.run([tools / "zipalign", "-f", "4", raw, aligned], check=True)
    subprocess.run([tools / "apksigner", "sign", "--ks", key, "--ks-pass", "pass:android",
                    "--v4-signing-enabled", "false", "--out", signed, aligned], check=True)
    subprocess.run([tools / "apksigner", "verify", "--min-sdk-version", "28", signed], check=True)
    print(f"Built signed instrumentation: {signed}", flush=True)
    return signed


def check_result(output, key, expected):
    """am can exit zero on failure. Our raw Instrumentations return one result and one finish code."""
    lines = output.splitlines()
    if lines == [f"INSTRUMENTATION_RESULT: {key}={expected}", "INSTRUMENTATION_CODE: -1"]:
        return
    # Never echo the full device transcript (in particular, credential material or stack traces).
    for line in lines:
        failure = re.fullmatch(rf"INSTRUMENTATION_RESULT: {re.escape(key)}=FAIL-([a-z][a-z0-9-]{{0,47}})", line)
        if failure:
            raise SystemExit(f"{key}={expected} failed at {failure[1]}.")
    raise SystemExit(f"Missing or invalid instrumentation completion for {key}={expected}.")


def require_stock_emulator(adb, serial, minimum_api):
    if not re.fullmatch(r"emulator-[0-9]+", serial):
        raise SystemExit("This gate requires an explicitly selected disposable Android emulator.")
    avd = adb("emu", "avd", "name").splitlines()
    if len(avd) != 2 or not avd[0] or avd[1] != "OK":
        raise SystemExit("Could not verify the selected emulator's AVD.")
    api = adb("shell", "getprop", "ro.build.version.sdk").strip()
    if not re.fullmatch("[0-9]+", api) or int(api) < minimum_api:
        raise SystemExit(f"This gate requires API {minimum_api}+.")


def require_preconfigured_apk(adb):
    # An operator installed and configured the target, not the test APK. Don't replace, clear or
    # uninstall their target. A matching package name (or even debug signer) is not a build ID.
    paths = adb("shell", "pm", "path", "--user", "current", TARGET).splitlines()
    if len(paths) != 1 or not paths[0].startswith("package:/"):
        raise SystemExit("Expected the single preconfigured EON debug APK.")
    digest = hashlib.sha256()
    with TARGET_APK.open("rb") as apk:
        for chunk in iter(lambda: apk.read(1024 * 1024), b""):
            digest.update(chunk)
    remote = adb("shell", "sha256sum", shlex.quote(paths[0][len("package:"):])).split()
    if remote[:1] != [digest.hexdigest()]:
        raise SystemExit("Preconfigured target does not match the locally built APK.")


def check_packages(inventory, preconfigured_allowed):
    packages = inventory.splitlines()
    if "package:android" not in packages or not all(line.startswith("package:") for line in packages):
        raise SystemExit("Could not verify installed packages.")
    if f"package:{TEST}" in packages or (not preconfigured_allowed and f"package:{TARGET}" in packages):
        raise SystemExit("Refusing to overwrite an existing EON installation.")
    if preconfigured_allowed and f"package:{TARGET}" not in packages:
        raise SystemExit("Install and configure the target APK before running the allowed gate.")


MIC_PHASES = {
    "denied": "Leave microphone access unreviewed. Tap Start and leave the denial visible.",
    "complete": "With access reviewed and a nonzero virtual input, tap Start and let all 12 levels finish.",
    "stop": "With access reviewed and a nonzero virtual input, tap Start. Let the first nonzero level remain "
            "visible for half a second, then tap Stop.",
    "hide": "Invoke over EON controls with the assistant role status visible underneath. With access reviewed "
            "and a nonzero virtual input, tap Start. Half a second after the first nonzero level, dismiss "
            "without collapsing or tapping Stop. Leave the "
            "controls visible for a second, invoke a fresh card, pause, then expand without starting.",
    "process-death": "On API 35+, with permission reviewed and a nonzero virtual input, tap Start. "
                     "After the first nonzero level, send SIGKILL only to EON's verified debug PID "
                     "using run-as. Do not dismiss or stop the take. After the old card disappears, "
                     "invoke a new card, pause, expand and wait at least one second before tapping Start. "
                     "Leave the refusal visible. Do not change the retained OS permission.",
}


def require_setup_complete(adb):
    for scope, key in (("global", "device_provisioned"), ("secure", "user_setup_complete")):
        if adb("shell", "settings", "--user", "current", "get", scope, key).strip() != "1":
            raise SystemExit("Finish device and current user setup in Android before testing a selected session.")


def require_closed_context(adb):
    # Stock AVD only. Filter at the device: never transfer show arguments or other assistant data.
    field = adb("shell", "dumpsys voiceinteraction | grep -E '^ *mDisabledShowContext=' || :").strip()
    if field != "mDisabledShowContext=3":
        raise SystemExit("Android did not confirm both assist structure and screenshot are disabled.")


PROFILE_RESULTS = {
    "allowed": "PASS-os-action",
    "denied": "PASS-policy-denied",
    "restricted": "PASS-policy-restricted",
    "capture-disabled": "PASS-policy-capture-disabled",
}


def run(sdk, serial, test_apk, *, demo_denied=False, stock_hud=False, stock_assistant=False, preconfigured_allowed=False,
        preconfigured_hud=False, preconfigured_workflow=False, mic_phase=None, preconfigured_context=False,
        context_denied=False, profile_policy=None):
    def adb(*args, timeout=120):
        return subprocess.check_output([sdk / "platform-tools/adb", "-s", serial, *args],
                                       text=True, stderr=subprocess.STDOUT, timeout=timeout)

    def instrument(component, key, expected, *args):
        # The operator observer allows up to 30 seconds for each of eight visible checkpoints.
        timeout = 360 if component == "ControlsAssistAcceptance" else (
            270 if component in ("OperatorWindowAcceptance", "SessionMicAcceptance", "WorkflowNoticeAcceptance") else 120)
        output = adb("shell", "am", "instrument", "-w", "-r", *args, f"{TEST}/{TARGET}.{component}",
                     timeout=timeout)
        check_result(output, key, expected)
        if component in ("ControlsAssistAcceptance", "ProfileActionAcceptance"):
            require_closed_context(adb)
        name = expected.removeprefix("PASS-") if key == "keystore" else f"{key}={expected}"
        print(f"PASS: {name}", flush=True)

    if profile_policy is not None and profile_policy not in PROFILE_RESULTS:
        raise SystemExit("Unknown profile policy case.")
    selected_session = (preconfigured_hud or preconfigured_workflow or preconfigured_context
                        or mic_phase is not None or profile_policy is not None)
    preconfigured = preconfigured_allowed or selected_session
    minimum_api = 34 if demo_denied or stock_hud or stock_assistant or preconfigured else 28
    require_stock_emulator(adb, serial, 35 if mic_phase == "process-death" or profile_policy is not None else minimum_api)
    if selected_session:
        require_setup_complete(adb)
    if context_denied and (not preconfigured_context or
                           adb("shell", "settings", "--user", "current", "get", "secure", "assist_structure_enabled").strip() != "0"):
        raise SystemExit("Disable Use text from screen in the disposable AVD's digital-assistant Settings first.")
    check_packages(adb("shell", "pm", "list", "packages", "-u", "--user", "all"), preconfigured)
    with ExitStack() as cleanup:
        def install(package, apk):
            if "Success" not in adb("install", "-t", apk).splitlines():
                raise SystemExit(f"Installation failed for {package}.")
            cleanup.callback(adb, "uninstall", package)

        if preconfigured:
            require_preconfigured_apk(adb)
            if preconfigured_context or profile_policy is not None:
                require_closed_context(adb)
            install(TEST, test_apk)
            if profile_policy is not None:
                # The observer checks the actual OS policy before touching the UI.
                # Never configure the policy, grant the role, or replace the target.
                instrument("ProfileActionAcceptance", "profileAction", PROFILE_RESULTS[profile_policy],
                           "--user", "current", "-e", "policy", profile_policy)
            elif preconfigured_context:
                print("On EON controls with CPU timing OFF, review sharing the setting, pause, confirm and dismiss. "
                      + ("" if context_denied else "Select CPU timing ON and repeat the review, confirmation and dismissal. ")
                      + "Pause, then invoke normally. This tests app-volunteered metadata, NOT AssistStructure.",
                      flush=True)
                if context_denied:
                    instrument("ControlsAssistAcceptance", "controlsMetadata", "PASS-policy-denied", "-e", "policy", "denied")
                else:
                    instrument("ControlsAssistAcceptance", "controlsMetadata", "PASS-volunteered")
            elif preconfigured_workflow:
                print("Start on EON controls with selected role status visible. Pause briefly at each step. "
                      "Invoke the assistant, expand, tap Check model profile, and wait for the completed count "
                      "and profile. End session; leave the controls visible, invoke again, then expand without "
                      "checking.", flush=True)
                instrument("WorkflowNoticeAcceptance", "workflowNotice", "PASS-session")
            elif mic_phase is not None:
                print("Invoke the selected assistant on the unlocked AVD over EON controls. Expand its card "
                      "and pause with the meter off before tapping Start. "
                      + MIC_PHASES[mic_phase], flush=True)
                instrument("SessionMicAcceptance", "micHud", f"PASS-{mic_phase}", "-e", "mode", mic_phase)
            elif preconfigured_hud:
                print("Invoke EON with the unlocked assistant gesture. Expand and choose Analyze built-in "
                      "notification tone. After completion, tap Stop and clear; pause, close the card, "
                      "wait a second, then invoke again.",
                      flush=True)
                instrument("OperatorWindowAcceptance", "voiceHud", "PASS-session", "-e", "mode", "session")
            else:
                print("Open a fresh EON action demo. Review and cancel a counter click. Review and confirm "
                      "a click, then a scroll. Choose samples without native actions and repeat the "
                      "confirmed click and scroll. Pause briefly at each result.", flush=True)
                instrument("OperatorWindowAcceptance", "demo", "PASS-allowed", "-e", "mode", "demo")
            return

        install(TARGET, TARGET_APK)
        install(TEST, test_apk)
        for phase in ("issue", "restart"):
            adb("shell", "am", "force-stop", TARGET)
            instrument("KeystoreAcceptance", "keystore", f"PASS-{phase}", "-e", "phase", phase)
        if demo_denied:
            instrument("DemoActionAcceptance", "demo", "PASS-denied", "-e", "mode", "denied")
        if stock_hud:
            instrument("VoiceHudAcceptance", "voiceHud", "PASS-stock", "-e", "mode", "stock")
        if stock_assistant:
            instrument("AssistantActivityAcceptance", "assistantActivity", "PASS-picker-canceled")
        # Only after restart's single-alias and continuity checks; the native vault has its own key.
        instrument("NativeVaultAcceptance", "nativeVault", "PASS")


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--build-only", action="store_true")
    mode.add_argument("--serial", help="adb serial of a newly created disposable API 28+ emulator")
    mode.add_argument("--preconfigured-allowed-serial", metavar="SERIAL",
                      help="only run demo=allowed on a disposable unlocked API 34+ AVD: first install "
                           "app-debug.apk and manually select EON as assistant and enable its service and "
                           "shortcut. Leave the instrumentation APK uninstalled. No role or permission grants.")
    mode.add_argument("--preconfigured-hud-serial", metavar="SERIAL",
                      help="observe an operator's real assistant gesture, tone analysis, dismissal and fresh "
                           "invocation on an unlocked disposable API 34+ AVD already configured with EON "
                           "as assistant. Do not enable accessibility. Install only the matching target APK.")
    mode.add_argument("--preconfigured-mic-serial", metavar="SERIAL",
                      help="observe one manual microphone phase on a separate, setup-complete, unlocked API 34+ "
                           "AVD with the matching target already selected as assistant. Use only a private "
                           "virtual input, never a host microphone. The harness does not configure or start capture.")
    mode.add_argument("--preconfigured-workflow-serial", metavar="SERIAL",
                      help="only observe the user-started profile workflow notice on a setup-complete, unlocked "
                           "disposable API 34+ AVD with <8 decimal GB RAM, no personal data/accounts or host mic. "
                           "Install the matching debug target, choose EON via the Android assistant picker and "
                           "leave its controls visible. No accessibility or microphone permission is needed.")
    mode.add_argument("--preconfigured-context-serial", metavar="SERIAL",
                      help="observe review of app-volunteered settings from EON controls, then "
                           "dismissal and fresh private invocation. Use only a disposable setup-complete "
                           "API 34+ AVD with matching target already selected via Android's picker.")
    mode.add_argument("--preconfigured-profile-serial", metavar="SERIAL",
                      help="exercise both owner-context routes on an already configured disposable API 35 AVD. "
                           "The observer drives EON's private controls; no settings, grants or roles are changed.")
    parser.add_argument("--profile-policy", choices=PROFILE_RESULTS,
                        help="existing device policy to verify; requires --preconfigured-profile-serial")
    parser.add_argument("--context-denied", action="store_true", help="observe refusal after the operator disables "
                        "Use text from screen in Android Settings; requires --preconfigured-context-serial")
    parser.add_argument("--mic-phase", choices=MIC_PHASES, help="required only with --preconfigured-mic-serial")
    parser.add_argument("--stock-hud", action="store_true",
                        help="also measure and cancel an actual OS notification tone with the production "
                             "decoder on the unselected clean API 34+ emulator named by --serial")
    parser.add_argument("--demo-denied", action="store_true",
                        help="also require demo=denied on the clean, unlocked API 34+ emulator named by --serial")
    parser.add_argument("--stock-assistant", action="store_true",
                        help="on the clean unlocked API 34+ AVD, launch EON's ACTION_ASSIST activity and "
                             "cancel its real Android role picker without making a selection")
    args = parser.parse_args(argv)
    if bool(args.preconfigured_profile_serial) != (args.profile_policy is not None):
        parser.error("--preconfigured-profile-serial and --profile-policy must be supplied together")
    if args.context_denied and not args.preconfigured_context_serial:
        parser.error("--context-denied requires --preconfigured-context-serial")
    if bool(args.preconfigured_mic_serial) != (args.mic_phase is not None):
        parser.error("--preconfigured-mic-serial and --mic-phase must be supplied together")
    if args.demo_denied and not args.serial:
        parser.error("--demo-denied requires --serial")
    if args.stock_hud and not args.serial:
        parser.error("--stock-hud requires --serial")
    if args.stock_assistant and not args.serial:
        parser.error("--stock-assistant requires --serial")
    return args


if __name__ == "__main__":
    args = parse_args()
    sdk = Path(os.environ.get("ANDROID_HOME") or os.environ["ANDROID_SDK_ROOT"])
    apk = build(sdk)
    if not args.build_only:
        run(sdk, args.serial or args.preconfigured_allowed_serial or args.preconfigured_hud_serial
            or args.preconfigured_mic_serial or args.preconfigured_workflow_serial or args.preconfigured_context_serial
            or args.preconfigured_profile_serial, apk,
            demo_denied=args.demo_denied, stock_hud=args.stock_hud, stock_assistant=args.stock_assistant,
            preconfigured_allowed=args.preconfigured_allowed_serial is not None,
            preconfigured_hud=args.preconfigured_hud_serial is not None,
            preconfigured_context=args.preconfigured_context_serial is not None,
            context_denied=args.context_denied, profile_policy=args.profile_policy,
            preconfigured_workflow=args.preconfigured_workflow_serial is not None, mic_phase=args.mic_phase)
